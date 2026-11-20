//! M8-A selective-invalidation integration tests (`docs/LLM.md` §7,
//! `docs/REQUIREMENTS.md` FR-5/FR-6). In-memory stores, no network
//! (`docs/TESTING.md` §1); the `--stale` tie-in uses the scripted `Fake` transport
//! (`docs/TESTING.md` §3).
//!
//! The pure §7-row diff is unit-tested inside `pipeline::invalidation`; these tests
//! cover the trigger end to end: approving a new analysis version marks the affected
//! **unlocked** translations stale, never touches `locked` cues, and drives
//! `translate --stale` to re-translate exactly the stale cues.

use std::time::Duration;

use myansub::domain::{Cue, DigitStyle};
use myansub::llm::client::{CommitToken, LlmResponse, ServiceTier, ThinkingSetting, Usage};
use myansub::llm::dry_run::Estimator;
use myansub::llm::fake::Fake;
use myansub::llm::retry::RetryConfig;
use myansub::pipeline::scheduler::SchedulerConfig;
use myansub::pipeline::translation::{TranslationFlags, TranslationParams, run_translation};
use myansub::pipeline::{
    approve_and_invalidate, approve_and_invalidate_in_tx, invalidate_after_approval,
};
use myansub::quality::checks::Limits;
use myansub::store::repo_analysis::{
    AnalysisOrigin, approve_current_analysis, current_analysis, insert_analysis_version,
};
use myansub::store::repo_cues::insert_cues;
use myansub::store::repo_project::{get_stage, get_stage_origin};
use myansub::store::repo_translations::{
    NewTranslation, TranslationOrigin, cue_id_by_idx, get_translation, list_translations,
    set_locked_in_tx, upsert_translation,
};
use myansub::store::{Stage, StageStatus, Store};
use tokio_util::sync::CancellationToken;

const MODEL: &str = "gemini-flash-latest";
const MM: &str = "မင်္ဂလာပါ"; // Burmese, so `untranslated` never fires

fn cue(idx: u32, text: &str, translatable: bool) -> Cue {
    Cue {
        idx,
        start_ms: u64::from(idx) * 1000,
        end_ms: u64::from(idx) * 1000 + 900,
        text_raw: text.to_string(),
        text_masked: text.to_string(),
        lead_tags: None,
        inline_tags: vec![],
        style: None,
        speaker: None,
        translatable,
    }
}

/// Cues 1,2,4,5 translatable; 3 non-translatable. Cue 2 mentions "kingdom".
fn cues() -> Vec<Cue> {
    vec![
        cue(1, "Hello, Aoi!", true),
        cue(2, "The kingdom is far.", true),
        cue(3, "...", false),
        cue(4, "Goodbye.", true),
        cue(5, "Be careful.", true),
    ]
}

/// An analysis doc whose single scene covers the cue range and names one character.
fn doc(premise: &str, glossary: &str) -> String {
    format!(
        r#"{{"schema_version":1,"premise":"{premise}","tone":"t",
        "characters":[{{"id":"c1","name":"Aoi","aliases":[],"gender":"f",
        "age_group":"young","role":"lead","speech_style":"plain"}}],
        "address_matrix":[],"glossary":[{glossary}],
        "scenes":[{{"cue_range":[1,5],"summary":"s","speakers":["c1"],"mood":"calm"}}],
        "style_rules":[],"unresolved_ambiguities":[]}}"#
    )
}

/// Seeds an in-memory store with the cues and an approved analysis version 1.
async fn seed(doc_v1: &str) -> Store {
    let store = Store::open_in_memory().expect("open");
    let cues = cues();
    store
        .call(move |conn| insert_cues(conn, &cues))
        .await
        .expect("cues");
    let doc_v1 = doc_v1.to_string();
    store
        .call(move |conn| {
            insert_analysis_version(
                conn,
                &doc_v1,
                AnalysisOrigin::Llm,
                "sha256:p",
                "prompt_v1|m",
            )
            .map(|_| ())
        })
        .await
        .expect("analysis v1");
    store
        .call(|conn| approve_current_analysis(conn).map(|_| ()))
        .await
        .expect("approve v1");
    store
}

async fn cue_id(store: &Store, idx: u32) -> i64 {
    store
        .call(move |c| Ok(cue_id_by_idx(c)?.get(&idx).copied()))
        .await
        .expect("map")
        .expect("cue present")
}

/// Inserts an `origin=llm` translation at `analysis_version` for one cue idx.
async fn translate(store: &Store, idx: u32, version: i64) {
    let id = cue_id(store, idx).await;
    let t = NewTranslation {
        cue_id: id,
        text: MM.to_string(),
        origin: TranslationOrigin::Llm,
        locked: false,
        stale: false,
        flags: vec![],
        analysis_version: version,
        model: Some(MODEL.to_string()),
    };
    store
        .call(move |c| upsert_translation(c, &t).map(|_| ()))
        .await
        .expect("translate");
}

/// Inserts a new unapproved analysis version and approves it, returning its number.
async fn add_and_approve(store: &Store, new_doc: &str) -> i64 {
    let new_doc = new_doc.to_string();
    let version = store
        .call(move |conn| {
            insert_analysis_version(
                conn,
                &new_doc,
                AnalysisOrigin::User,
                "sha256:p",
                "prompt_v1|m",
            )
        })
        .await
        .expect("insert v2");
    store
        .call(|conn| approve_current_analysis(conn).map(|_| ()))
        .await
        .expect("approve v2");
    version
}

/// Inserts a new, **unapproved** analysis version, returning its number. (Unlike
/// [`add_and_approve`], it leaves approval to the atomic approve path under test.)
async fn add_version(store: &Store, new_doc: &str) -> i64 {
    let new_doc = new_doc.to_string();
    store
        .call(move |conn| {
            insert_analysis_version(
                conn,
                &new_doc,
                AnalysisOrigin::User,
                "sha256:p",
                "prompt_v1|m",
            )
        })
        .await
        .expect("insert version")
}

async fn is_stale(store: &Store, idx: u32) -> bool {
    let id = cue_id(store, idx).await;
    store
        .call(move |c| get_translation(c, id))
        .await
        .expect("get")
        .expect("row")
        .stale
}

// AC: M8-AC3 — approving a new version whose premise changed marks every unlocked
// translation stale, but never the locked one (FR-6).
#[tokio::test]
async fn premise_change_marks_unlocked_stale_but_not_locked() {
    let store = seed(&doc("p1", "")).await;
    for idx in [1, 2, 4, 5] {
        translate(&store, idx, 1).await;
    }
    // Lock cue 4: a locked cue is never set stale.
    let locked_id = cue_id(&store, 4).await;
    store
        .call(move |c| {
            let tx = c.transaction()?;
            set_locked_in_tx(&tx, locked_id, true)?;
            tx.commit()?;
            Ok(())
        })
        .await
        .expect("lock");

    let v2 = add_and_approve(&store, &doc("p2", "")).await;
    let marked = invalidate_after_approval(&store, v2)
        .await
        .expect("invalidate");

    assert_eq!(
        marked, 3,
        "the three unlocked translations were marked stale"
    );
    assert!(is_stale(&store, 1).await);
    assert!(is_stale(&store, 2).await);
    assert!(is_stale(&store, 5).await);
    assert!(!is_stale(&store, 4).await, "locked cue is never set stale");
}

// AC: M8-AC3 — adding a glossary term marks stale only the cues whose source contains
// the term.
#[tokio::test]
async fn glossary_add_marks_only_cues_containing_the_term() {
    let store = seed(&doc("p1", "")).await;
    for idx in [1, 2, 4, 5] {
        translate(&store, idx, 1).await;
    }

    let v2 = add_and_approve(
        &store,
        &doc("p1", r#"{"term":"kingdom","target":"X","keep":false}"#),
    )
    .await;
    let marked = invalidate_after_approval(&store, v2)
        .await
        .expect("invalidate");

    assert_eq!(marked, 1);
    assert!(is_stale(&store, 2).await, "cue 2 mentions 'kingdom'");
    for idx in [1, 4, 5] {
        assert!(!is_stale(&store, idx).await, "cue {idx} is unaffected");
    }
}

// AC: M8-AC3 — approving an identical document invalidates nothing.
#[tokio::test]
async fn identical_document_marks_nothing_stale() {
    let store = seed(&doc("p1", "")).await;
    for idx in [1, 2, 4, 5] {
        translate(&store, idx, 1).await;
    }

    let v2 = add_and_approve(&store, &doc("p1", "")).await;
    let marked = invalidate_after_approval(&store, v2)
        .await
        .expect("invalidate");

    assert_eq!(marked, 0);
    for idx in [1, 2, 4, 5] {
        assert!(!is_stale(&store, idx).await);
    }
}

// AC: M8-AC3 — the first-ever approval, before any translations exist, invalidates
// nothing.
#[tokio::test]
async fn first_approval_without_translations_is_a_noop() {
    let store = seed(&doc("p1", "")).await;
    let marked = invalidate_after_approval(&store, 1)
        .await
        .expect("invalidate");
    assert_eq!(marked, 0);
    assert!(list_translations_empty(&store).await);
}

async fn list_translations_empty(store: &Store) -> bool {
    store
        .call(|c| list_translations(c))
        .await
        .expect("list")
        .is_empty()
}

// AC: M8-AC3 — after invalidation, `translate --stale` re-translates exactly the
// stale, unlocked cues (ties to M6-B2): cue 5 is locked and is neither marked stale
// nor re-sent, while cues 1,2,4 are.
#[tokio::test]
async fn translate_stale_retranslates_exactly_the_stale_cues() {
    let store = seed(&doc("p1", "")).await;
    let cues = cues();

    // Full first pass at version 1: every translatable cue gets an origin=llm row.
    let fake = Fake::new().always(|| Ok(lines_response(&[(1, MM), (2, MM), (4, MM), (5, MM)])));
    let token = CancellationToken::new();
    run_translation(
        fake,
        &store,
        &cues,
        &params(10),
        TranslationFlags::default(),
        &token,
        std::sync::Arc::new(myansub::pipeline::progress::NoopProgress),
    )
    .await
    .expect("first pass");

    // Lock cue 5 so it is excluded from the stale set.
    let locked_id = cue_id(&store, 5).await;
    store
        .call(move |c| {
            let tx = c.transaction()?;
            set_locked_in_tx(&tx, locked_id, true)?;
            tx.commit()?;
            Ok(())
        })
        .await
        .expect("lock");

    // Approve a new version with a changed premise and invalidate.
    let v2 = add_and_approve(&store, &doc("p2", "")).await;
    let marked = invalidate_after_approval(&store, v2)
        .await
        .expect("invalidate");
    assert_eq!(marked, 3, "cues 1,2,4 stale; cue 5 locked");

    // `translate --stale` re-requests only the stale, unlocked cues.
    let stale_fake = Fake::new().always(|| Ok(lines_response(&[(1, MM), (2, MM), (4, MM)])));
    run_translation(
        stale_fake.clone(),
        &store,
        &cues,
        &params(10),
        TranslationFlags {
            stale: true,
            ..TranslationFlags::default()
        },
        &token,
        std::sync::Arc::new(myansub::pipeline::progress::NoopProgress),
    )
    .await
    .expect("stale pass");

    let sent: String = stale_fake
        .calls()
        .into_iter()
        .flat_map(|r| r.contents.into_iter().map(|b| b.text))
        .collect();
    assert!(sent.contains("[1]"), "cue 1 re-sent");
    assert!(sent.contains("[2]"), "cue 2 re-sent");
    assert!(sent.contains("[4]"), "cue 4 re-sent");
    assert!(!sent.contains("[5]"), "locked cue 5 is never re-sent");
}

// AC: M8-AC3 — the `analysis approve` command trigger site (`approve_and_invalidate`,
// the exact path `main.rs` drives): approving an edited version marks the affected
// unlocked translations stale, never the locked one, and records the gate approved
// with origin `user` — all atomically.
#[tokio::test]
async fn analysis_approve_command_marks_affected_unlocked_stale() {
    let store = seed(&doc("p1", "")).await;
    for idx in [1, 2, 4, 5] {
        translate(&store, idx, 1).await;
    }
    // Lock cue 4: a locked cue is never set stale (FR-6).
    let locked_id = cue_id(&store, 4).await;
    store
        .call(move |c| {
            let tx = c.transaction()?;
            set_locked_in_tx(&tx, locked_id, true)?;
            tx.commit()?;
            Ok(())
        })
        .await
        .expect("lock");
    // Edit the analysis: a new unapproved v2 with a changed premise (affects all).
    add_version(&store, &doc("p2", "")).await;

    let outcome = approve_and_invalidate(&store, "user")
        .await
        .expect("approve");

    assert_eq!(outcome.approved_version, 2);
    assert_eq!(
        outcome.newly_stale, 3,
        "three unlocked translations marked stale"
    );
    assert_eq!(outcome.stale_unlocked, 3);
    assert!(is_stale(&store, 1).await);
    assert!(is_stale(&store, 2).await);
    assert!(is_stale(&store, 5).await);
    assert!(!is_stale(&store, 4).await, "locked cue is never set stale");

    // The version is approved and the gate records origin `user`.
    let current = store
        .call(|c| current_analysis(c))
        .await
        .expect("current")
        .expect("row");
    assert!(current.approved, "the new version is approved");
    let origin = store
        .call(|c| get_stage_origin(c, Stage::AnalysisReview))
        .await
        .expect("origin");
    assert_eq!(origin.as_deref(), Some("user"));
    let stage = store
        .call(|c| get_stage(c, Stage::AnalysisReview))
        .await
        .expect("stage");
    assert_eq!(stage, Some(StageStatus::Approved));
}

// AC: M8-AC3 — the combined approve+invalidate is ATOMIC: a fault injected before the
// commit rolls back BOTH the approval and the stale-marking (all-or-nothing), so a
// crash can never leave an approved-but-not-invalidated state (`docs/TESTING.md` §4,
// ADR-041).
#[tokio::test]
async fn approve_and_invalidate_is_atomic_on_fault() {
    let store = seed(&doc("p1", "")).await;
    for idx in [1, 2, 4, 5] {
        translate(&store, idx, 1).await;
    }
    // A new unapproved v2 with a changed premise (would mark every unlocked cue stale).
    add_version(&store, &doc("p2", "")).await;

    // Approve + invalidate in one transaction, then inject a constraint violation
    // (duplicate analysis version PK) before commit — mirroring the store
    // rollback-seam pattern (`docs/TESTING.md` §4).
    let result = store
        .call(|conn| {
            let tx = conn.transaction()?;
            approve_and_invalidate_in_tx(&tx, "user")?;
            tx.execute(
                "INSERT INTO analysis (version, doc, origin, source_digest, signature, approved) \
                 VALUES (1, 'x', 'llm', 'd', 's', 0)",
                [],
            )?;
            tx.commit()?;
            Ok(())
        })
        .await;
    assert!(result.is_err(), "the injected fault must abort the commit");

    // Neither the approval nor the stale-marking persisted.
    let current = store
        .call(|c| current_analysis(c))
        .await
        .expect("current")
        .expect("row");
    assert!(
        !current.approved,
        "approval rolled back with the transaction"
    );
    for idx in [1, 2, 4, 5] {
        assert!(
            !is_stale(&store, idx).await,
            "no stale-marking persisted (cue {idx})"
        );
    }
}

// AC: M8-AC3 — with multiple intervening edits, the diff is taken against the version
// the existing translations were built against (v1), not the latest unapproved one:
// translating at v1, inserting v2 (adds "kingdom") and v3 (adds "kingdom" + "careful"),
// then approving ONLY v3 must still catch the v2-origin change (cue 2).
#[tokio::test]
async fn multi_edit_diffs_against_translated_version_not_latest() {
    let store = seed(&doc("p1", "")).await;
    for idx in [1, 2, 4, 5] {
        translate(&store, idx, 1).await;
    }
    // v2 adds the "kingdom" glossary term (cue 2), left unapproved.
    add_version(
        &store,
        &doc("p1", r#"{"term":"kingdom","target":"X","keep":false}"#),
    )
    .await;
    // v3 adds "kingdom" AND "careful" (cues 2 and 5); approve only v3.
    let v3 = add_and_approve(
        &store,
        &doc(
            "p1",
            r#"{"term":"kingdom","target":"X","keep":false},{"term":"careful","target":"Y","keep":false}"#,
        ),
    )
    .await;
    let marked = invalidate_after_approval(&store, v3)
        .await
        .expect("invalidate");

    assert_eq!(marked, 2, "diff is v3-vs-v1: both glossary terms count");
    assert!(
        is_stale(&store, 2).await,
        "cue 2 (term added in v2) is stale: diffed against v1, not v2"
    );
    assert!(
        is_stale(&store, 5).await,
        "cue 5 (term added in v3) is stale"
    );
    for idx in [1, 4] {
        assert!(!is_stale(&store, idx).await, "cue {idx} is unaffected");
    }
}

// AC: M8-AC3 — a recovery re-run (re-approving an already-approved version after an
// interrupted run left cues stale) marks nothing new but still reports the count of
// currently-stale unlocked translations, so it keeps signalling pending re-translation
// (`docs/LLM.md` §7).
#[tokio::test]
async fn reapproval_still_reports_currently_stale_unlocked() {
    let store = seed(&doc("p1", "")).await;
    for idx in [1, 2, 4, 5] {
        translate(&store, idx, 1).await;
    }
    add_version(&store, &doc("p2", "")).await;

    let first = approve_and_invalidate(&store, "user")
        .await
        .expect("approve");
    assert_eq!(first.newly_stale, 4);
    assert_eq!(first.stale_unlocked, 4);

    // Re-approve the same (already-approved) current version.
    let again = approve_and_invalidate(&store, "user")
        .await
        .expect("reapprove");
    assert_eq!(again.newly_stale, 0, "nothing newly marked on re-approval");
    assert_eq!(
        again.stale_unlocked, 4,
        "still signals the four pending re-translations"
    );
}

fn lines_response(pairs: &[(i64, &str)]) -> LlmResponse {
    let items: Vec<serde_json::Value> = pairs
        .iter()
        .map(|(id, t)| serde_json::json!({ "id": id, "t": t }))
        .collect();
    LlmResponse {
        text: serde_json::json!({ "lines": items }).to_string(),
        usage: Usage {
            prompt_tokens: 50,
            output_tokens: 20,
            ..Usage::default()
        },
        commit: CommitToken::noop(),
    }
}

fn params(batch_size: u32) -> TranslationParams {
    TranslationParams {
        model: MODEL.to_string(),
        tier: ServiceTier::Flex,
        thinking: ThinkingSetting::Off,
        batch_size,
        context_before: 0,
        context_after: 0,
        max_content_attempts: 3,
        output_ratio: 4.0,
        digits: DigitStyle::Myanmar,
        estimator: Estimator {
            bytes_per_token_latin: 4.0,
            bytes_per_token_myanmar: 3.0,
            bytes_per_token_cjk: 3.0,
        },
        limits: Limits {
            max_graphemes_per_line: 42,
            max_lines: 2,
            max_gps: 20.0,
            drift_ratio: 3.0,
        },
        scheduler: SchedulerConfig {
            concurrency: 1,
            warm_cache_first: false,
            grace: Duration::from_secs(5),
        },
        seed: 42,
        retry: RetryConfig {
            max_retries: 2,
            backoff_base_ms: 1,
            backoff_max_ms: 2,
            max_sheds_before_fallback: 1,
            service_tier_fallback: true,
        },
    }
}
