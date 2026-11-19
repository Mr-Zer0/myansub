//! M7-A translation-review tests via the public `review::translation` surface
//! (`docs/CLI.md` §6, `docs/REQUIREMENTS.md` FR-6, `docs/DATA_MODEL.md` §6,
//! `docs/ARCHITECTURE.md` §2). Offline, in-memory stores; the editor is the
//! injectable [`EditorLauncher`], never a real `$EDITOR`. The `--redo` interaction
//! is driven by the scripted `Fake` transport (`docs/TESTING.md` §3).

use std::path::Path;
use std::time::Duration;

use myansub::domain::{Cue, QualityFlag};
use myansub::error::ExitCode;
use myansub::error::exit;
use myansub::llm::client::{CommitToken, LlmResponse, ServiceTier, ThinkingSetting, Usage};
use myansub::llm::dry_run::Estimator;
use myansub::llm::fake::Fake;
use myansub::llm::retry::RetryConfig;
use myansub::pipeline::can_run;
use myansub::pipeline::orchestrator::load_state;
use myansub::pipeline::scheduler::SchedulerConfig;
use myansub::pipeline::translation::{
    TranslationFlags, TranslationOutcome, TranslationParams, run_translation,
};
use myansub::quality::checks::Limits;
use myansub::review::ReviewError;
use myansub::review::editor::EditorLauncher;
use myansub::review::translation::{self, ReviewFilters, ReviewSelection};
use myansub::store::repo_analysis::{
    AnalysisOrigin, approve_current_analysis, insert_analysis_version,
};
use myansub::store::repo_cues::insert_cues;
use myansub::store::repo_project::{Project, insert_project, set_stage};
use myansub::store::repo_translations::{
    NewTranslation, TranslationOrigin, cue_id_by_idx, get_translation, list_translation_history,
};
use myansub::store::repo_units::{
    NewTranslationBatch, claim_translation_batch, count_translation_batches_by_status,
    create_translation_batch, fail_translation_batch,
};
use myansub::store::{Stage, StageStatus, Store, UnitStatus};
use tokio_util::sync::CancellationToken;

const HASH: &str = "sha256:proj";
const MODEL: &str = "gemini-flash-latest";
const MM: &str = "မင်္ဂလာပါ"; // Burmese, so `untranslated` never fires

/// An approved analysis whose single scene names one character (`c1` → Aoi).
const ANALYSIS: &str = r#"{"schema_version":1,"premise":"p","tone":"t",
"characters":[{"id":"c1","name":"Aoi","gender":"f","age_group":"young"}],
"address_matrix":[],"glossary":[],
"scenes":[{"cue_range":[1,9],"summary":"a quiet morning","speakers":["c1"]}],
"style_rules":[],"unresolved_ambiguities":[]}"#;

fn limits() -> Limits {
    Limits {
        max_graphemes_per_line: 42,
        max_lines: 2,
        max_gps: 20.0,
        drift_ratio: 3.0,
    }
}

fn cue(idx: u32, masked: &str, translatable: bool) -> Cue {
    Cue {
        idx,
        start_ms: u64::from(idx) * 1000,
        end_ms: u64::from(idx) * 1000 + 900,
        text_raw: masked.to_string(),
        text_masked: masked.to_string(),
        lead_tags: None,
        inline_tags: vec![],
        style: None,
        speaker: None,
        translatable,
    }
}

/// One stored translation to seed.
struct Seed {
    idx: u32,
    text: &'static str,
    flags: Vec<QualityFlag>,
    locked: bool,
    stale: bool,
}

fn project() -> Project {
    Project {
        source_path: "movie.srt".to_string(),
        source_hash: HASH.to_string(),
        source_format: "srt".to_string(),
        source_encoding: "utf-8".to_string(),
        encoding_confidence: None,
        source_lang: "en".to_string(),
        target_lang: "my".to_string(),
        format_meta: None,
    }
}

/// Seeds a project row, cues, and an approved analysis — the reachable state just
/// before translation runs. No translation rows, no translation stage row.
async fn seed_base(cues: &[Cue]) -> Store {
    let store = Store::open_in_memory().expect("open");
    let cues = cues.to_vec();
    store
        .call(move |conn| {
            insert_project(conn, &project())?;
            insert_cues(conn, &cues)?;
            insert_analysis_version(conn, ANALYSIS, AnalysisOrigin::Llm, HASH, "prompt_v1|m")?;
            Ok(())
        })
        .await
        .expect("seed base");
    store
        .call(|conn| approve_current_analysis(conn).map(|_| ()))
        .await
        .expect("approve analysis");
    store
}

/// Seeds [`seed_base`] plus the given translations and the translation stage marked
/// `completed`.
async fn seed(cues: &[Cue], translations: &[Seed]) -> Store {
    let store = seed_base(cues).await;

    for s in translations {
        let (idx, text, flags, locked, stale) = (
            s.idx,
            s.text.to_string(),
            s.flags.clone(),
            s.locked,
            s.stale,
        );
        store
            .call(move |conn| {
                let cue_id = cue_id_by_idx(conn)?[&idx];
                let tx = conn.transaction()?;
                myansub::store::repo_translations::upsert_translation_in_tx(
                    &tx,
                    &NewTranslation {
                        cue_id,
                        text,
                        origin: TranslationOrigin::Llm,
                        locked,
                        stale,
                        flags,
                        analysis_version: 1,
                        model: Some(MODEL.to_string()),
                    },
                )?;
                tx.commit()?;
                Ok(())
            })
            .await
            .expect("seed translation");
    }

    store
        .call(|conn| set_stage(conn, Stage::Translation, StageStatus::Completed))
        .await
        .expect("stage completed");
    store
}

/// The standard fixture: translatable cues 1,2,4,5 (3 is non-translatable). Cue 1 is
/// suspect (`too_fast`), cue 2 carries a placeholder, cue 4 is stale, cue 5 is a
/// failed cue (a genuinely-failed batch left it with no translation row). This is a
/// reachable completed-with-failures state (ADR-038): the stage is `completed`, no
/// batch is pending, and one batch is `failed`.
async fn standard_store() -> Store {
    let cues = vec![
        cue(1, "Good morning.", true),
        cue(2, "Hello ⟦0⟧", true),
        cue(3, "...", false),
        cue(4, "Be careful.", true),
        cue(5, "Where are you going?", true),
    ];
    let translations = vec![
        Seed {
            idx: 1,
            text: MM,
            flags: vec![QualityFlag::TooFast],
            locked: false,
            stale: false,
        },
        Seed {
            idx: 2,
            text: "⟦0⟧ မင်္ဂလာ",
            flags: vec![],
            locked: false,
            stale: false,
        },
        Seed {
            idx: 4,
            text: MM,
            flags: vec![],
            locked: false,
            stale: true,
        },
        // cue 5 has no translation → its batch genuinely failed (created below).
    ];
    let store = seed(&cues, &translations).await;
    // A genuinely-failed batch covering cue 5, so the missing row is a reachable state.
    store
        .call(|conn| {
            let tx = conn.transaction()?;
            create_translation_batch(
                &tx,
                &NewTranslationBatch {
                    batch_no: 5,
                    first_cue: 5,
                    last_cue: 5,
                    input_hash: None,
                    model: Some(MODEL.to_string()),
                    analysis_version: Some(1),
                },
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
        .expect("create batch 5");
    store
        .call(|c| claim_translation_batch(c, 5).map(|_| ()))
        .await
        .expect("claim 5");
    store
        .call(|c| fail_translation_batch(c, 5, "coverage miss").map(|_| ()))
        .await
        .expect("fail 5");
    store
}

async fn cue_id(store: &Store, idx: u32) -> i64 {
    store
        .call(move |c| Ok(cue_id_by_idx(c)?.get(&idx).copied()))
        .await
        .expect("map")
        .expect("cue present")
}

async fn translation_text(store: &Store, idx: u32) -> Option<String> {
    let id = cue_id(store, idx).await;
    store
        .call(move |c| get_translation(c, id))
        .await
        .expect("get")
        .map(|t| t.text)
}

async fn is_locked(store: &Store, idx: u32) -> bool {
    let id = cue_id(store, idx).await;
    store
        .call(move |c| get_translation(c, id))
        .await
        .expect("get")
        .map(|t| t.locked)
        .unwrap_or(false)
}

async fn history_texts(store: &Store, idx: u32) -> Vec<String> {
    let id = cue_id(store, idx).await;
    store
        .call(move |c| list_translation_history(c, id))
        .await
        .expect("history")
        .into_iter()
        .map(|h| h.text)
        .collect()
}

/// Builds a translation review file with the given `project_source_hash` and the
/// listed `(id, target)` cues (`docs/DATA_MODEL.md` §6). Other fields are
/// informational and are ignored on import.
fn review_file(hash: &str, cues: &[(u32, &str)]) -> String {
    let items: Vec<serde_json::Value> = cues
        .iter()
        .map(|(id, target)| {
            serde_json::json!({
                "id": id,
                "time": "00:00:01.000 --> 00:00:01.900",
                "speaker": serde_json::Value::Null,
                "source": "source",
                "target": target,
                "flags": [],
                "locked": false,
            })
        })
        .collect();
    serde_json::json!({
        "project_source_hash": hash,
        "analysis_version": 1,
        "cues": items,
    })
    .to_string()
}

// ---------------------------------------------------------------------------
// M7-AC2 — placeholder mismatch in an edited target is a hard reject (all-or-nothing)
// ---------------------------------------------------------------------------

// AC: M7-AC2 — an import whose edited target drops a ⟦n⟧ placeholder relative to its
// source is REJECTED naming the cue id, and nothing is applied (the other, valid
// change in the same file is not applied either — all-or-nothing).
#[tokio::test]
async fn import_rejects_placeholder_mismatch_naming_cue_and_applies_nothing() {
    let store = standard_store().await;

    // Cue 2's source is "Hello ⟦0⟧"; dropping the ⟦0⟧ is a placeholder mismatch. Cue 1
    // carries a valid change in the same file to prove all-or-nothing.
    let file = review_file(HASH, &[(1, "နေကောင်းလား"), (2, "no placeholder here")]);
    let err = translation::import(&store, &file, &limits())
        .await
        .expect_err("rejected");

    assert!(
        matches!(err, ReviewError::PlaceholderMismatch { id: 2 }),
        "expected placeholder mismatch for cue 2, got {err:?}"
    );
    assert!(
        err.to_string().contains('2'),
        "error names the cue id: {err}"
    );
    assert_eq!(err.exit_code(), exit::ERROR);

    // Nothing applied: cue 1 keeps its original text, unlocked, no history.
    assert_eq!(translation_text(&store, 1).await.as_deref(), Some(MM));
    assert!(!is_locked(&store, 1).await, "cue 1 must stay unlocked");
    assert!(
        history_texts(&store, 1).await.is_empty(),
        "no history written"
    );
}

// AC: M7-AC2 — a valid import (placeholders preserved) is accepted: the changed cue
// becomes locked, its previous text is written to history, and the quality flags are
// re-run (the artificial `too_fast` flag is recomputed away).
#[tokio::test]
async fn import_valid_edit_locks_records_history_and_reruns_flags() {
    let store = standard_store().await;

    let summary = translation::import(&store, &review_file(HASH, &[(1, "နေကောင်းလား")]), &limits())
        .await
        .expect("accepted");
    assert_eq!(summary.changed, 1);
    assert_eq!(summary.unchanged, 0);

    assert_eq!(
        translation_text(&store, 1).await.as_deref(),
        Some("နေကောင်းလား")
    );
    assert!(is_locked(&store, 1).await, "a changed cue is locked");
    assert_eq!(
        history_texts(&store, 1).await,
        vec![MM.to_string()],
        "previous text written to history"
    );

    // The seeded `too_fast` flag is re-run away for the short edited target.
    let id1 = cue_id(&store, 1).await;
    let flags = store
        .call(move |c| get_translation(c, id1))
        .await
        .expect("get")
        .expect("row")
        .flags;
    assert!(
        !flags.contains(&QualityFlag::TooFast),
        "flags re-run on the changed cue, got {flags:?}"
    );
}

// ---------------------------------------------------------------------------
// M7-AC1 — `review set` locks a cue; `translate --redo` preserves it
// ---------------------------------------------------------------------------

// AC: M7-AC1 — `review set <id> <text>` locks the cue (origin=user, locked=1) with the
// previous text in history; a subsequent `translate --redo` leaves the locked cue's
// text unchanged and still locked, while an unlocked cue is re-translated.
#[tokio::test]
async fn set_locks_cue_and_redo_preserves_it() {
    let cues = vec![cue(1, "Good morning.", true), cue(2, "Be careful.", true)];
    let translations = vec![
        Seed {
            idx: 1,
            text: MM,
            flags: vec![],
            locked: false,
            stale: false,
        },
        Seed {
            idx: 2,
            text: MM,
            flags: vec![],
            locked: false,
            stale: false,
        },
    ];
    let store = seed(&cues, &translations).await;

    // Reviewer sets cue 1.
    let reviewer = "ကိုယ်တိုင်ပြင်ထားသည်";
    translation::set_target(&store, 1, reviewer, &limits())
        .await
        .expect("set");
    assert!(is_locked(&store, 1).await, "set locks the cue");
    assert_eq!(
        history_texts(&store, 1).await,
        vec![MM.to_string()],
        "previous text written to history"
    );

    // translate --redo: cue 2 (unlocked) is re-translated; cue 1 (locked) is preserved.
    let fresh = "နေကောင်းလား";
    let fake = Fake::new().always(move || Ok(lines_response(&[(2, fresh)])));
    let token = CancellationToken::new();
    let outcome = run_translation(
        fake.clone(),
        &store,
        &cues,
        &params(80),
        TranslationFlags {
            redo: true,
            ..TranslationFlags::default()
        },
        &token,
        std::sync::Arc::new(myansub::pipeline::progress::NoopProgress),
    )
    .await
    .expect("redo");
    assert_eq!(outcome, TranslationOutcome::Completed { batches: 1 });

    assert_eq!(
        translation_text(&store, 1).await.as_deref(),
        Some(reviewer),
        "locked cue text unchanged by --redo"
    );
    assert!(is_locked(&store, 1).await, "locked cue stays locked");
    assert_eq!(
        translation_text(&store, 2).await.as_deref(),
        Some(fresh),
        "unlocked cue is re-translated"
    );

    // The locked cue is never re-sent.
    let texts: Vec<String> = fake
        .calls()
        .into_iter()
        .map(|r| {
            r.contents
                .iter()
                .map(|b| b.text.clone())
                .collect::<String>()
        })
        .collect();
    assert!(texts.iter().any(|t| t.contains("[2]")), "cue 2 re-sent");
    assert!(
        !texts.iter().any(|t| t.contains("[1]")),
        "locked cue 1 never re-sent"
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
        estimator: Estimator {
            bytes_per_token_latin: 4.0,
            bytes_per_token_myanmar: 3.0,
            bytes_per_token_cjk: 3.0,
        },
        limits: limits(),
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

// ---------------------------------------------------------------------------
// Import rules (`docs/DATA_MODEL.md` §6)
// ---------------------------------------------------------------------------

// AC: DATA_MODEL §6 — a hash mismatch rejects the whole import; nothing applied.
#[tokio::test]
async fn import_rejects_hash_mismatch() {
    let store = standard_store().await;
    let err = translation::import(
        &store,
        &review_file("sha256:other", &[(1, "နေကောင်းလား")]),
        &limits(),
    )
    .await
    .expect_err("rejected");
    assert!(matches!(err, ReviewError::HashMismatch { .. }), "{err:?}");
    assert_eq!(translation_text(&store, 1).await.as_deref(), Some(MM));
    assert!(!is_locked(&store, 1).await);
}

// AC: DATA_MODEL §6 — an unknown cue id rejects the whole import; nothing applied.
#[tokio::test]
async fn import_rejects_unknown_cue_id() {
    let store = standard_store().await;
    let err = translation::import(
        &store,
        &review_file(HASH, &[(1, "နေကောင်းလား"), (999, "x")]),
        &limits(),
    )
    .await
    .expect_err("rejected");
    assert!(
        matches!(err, ReviewError::UnknownCue { id: 999 }),
        "{err:?}"
    );
    // all-or-nothing: the valid cue 1 change is not applied.
    assert_eq!(translation_text(&store, 1).await.as_deref(), Some(MM));
    assert!(!is_locked(&store, 1).await);
}

// AC: DATA_MODEL §6 — an unchanged cue is left untouched: not re-locked, no history.
#[tokio::test]
async fn import_unchanged_cue_is_untouched() {
    let store = standard_store().await;
    // Cue 1's target equals the stored text → unchanged.
    let summary = translation::import(&store, &review_file(HASH, &[(1, MM)]), &limits())
        .await
        .expect("accepted");
    assert_eq!(summary.changed, 0);
    assert_eq!(summary.unchanged, 1);
    assert!(
        !is_locked(&store, 1).await,
        "unchanged cue is not re-locked"
    );
    assert!(
        history_texts(&store, 1).await.is_empty(),
        "no spurious history for an unchanged cue"
    );
}

// AC: DATA_MODEL §6 — a changed cue becomes origin=user, locked=1, with history and
// re-run flags.
#[tokio::test]
async fn import_changed_cue_becomes_user_locked_with_history() {
    let store = standard_store().await;
    let summary = translation::import(&store, &review_file(HASH, &[(1, "နေကောင်းလား")]), &limits())
        .await
        .expect("accepted");
    assert_eq!((summary.changed, summary.unchanged), (1, 0));

    let id1 = cue_id(&store, 1).await;
    let row = store
        .call(move |c| get_translation(c, id1))
        .await
        .expect("get")
        .expect("row");
    assert_eq!(row.origin, TranslationOrigin::User, "origin=user");
    assert!(row.locked, "locked=1");
    assert_eq!(history_texts(&store, 1).await, vec![MM.to_string()]);
}

// ---------------------------------------------------------------------------
// list filters and counts (`docs/REQUIREMENTS.md` FR-6, `docs/CLI.md` §6)
// ---------------------------------------------------------------------------

async fn list_ids(store: &Store, filters: ReviewFilters) -> Vec<u32> {
    translation::list(store, &filters)
        .await
        .expect("list")
        .cues
        .iter()
        .map(|c| c.id)
        .collect()
}

// AC: FR-6 — `list` with no filter returns every translatable cue (incl. failed).
#[tokio::test]
async fn list_returns_all_translatable_cues() {
    let store = standard_store().await;
    assert_eq!(
        list_ids(&store, ReviewFilters::default()).await,
        vec![1, 2, 4, 5]
    );
}

// AC: FR-6 — `--suspect` returns flagged cues (seeded too_fast + the failed cue 5).
#[tokio::test]
async fn list_suspect_filter() {
    let store = standard_store().await;
    let filters = ReviewFilters {
        suspect: true,
        ..ReviewFilters::default()
    };
    assert_eq!(list_ids(&store, filters).await, vec![1, 5]);
}

// AC: FR-6 — `--failed` returns the translatable cue with no translation row.
#[tokio::test]
async fn list_failed_filter() {
    let store = standard_store().await;
    let filters = ReviewFilters {
        failed: true,
        ..ReviewFilters::default()
    };
    assert_eq!(list_ids(&store, filters).await, vec![5]);
}

// AC: FR-6 — `--stale` returns only the stale cue.
#[tokio::test]
async fn list_stale_filter() {
    let store = standard_store().await;
    let filters = ReviewFilters {
        stale: true,
        ..ReviewFilters::default()
    };
    assert_eq!(list_ids(&store, filters).await, vec![4]);
}

// AC: FR-6 — `--flag F` returns only cues carrying flag F.
#[tokio::test]
async fn list_flag_filter() {
    let store = standard_store().await;
    let too_fast = ReviewFilters {
        flag: Some(QualityFlag::TooFast),
        ..ReviewFilters::default()
    };
    assert_eq!(list_ids(&store, too_fast).await, vec![1]);
    let failed = ReviewFilters {
        flag: Some(QualityFlag::Failed),
        ..ReviewFilters::default()
    };
    assert_eq!(list_ids(&store, failed).await, vec![5]);
}

// AC: FR-6 — `--range A-B` returns only cues in the inclusive range.
#[tokio::test]
async fn list_range_filter() {
    let store = standard_store().await;
    let filters = ReviewFilters {
        range: Some((2, 4)),
        ..ReviewFilters::default()
    };
    // cue 3 is non-translatable (never listed); 1 and 5 are out of range.
    assert_eq!(list_ids(&store, filters).await, vec![2, 4]);
}

// AC: FR-6 — the counts (suspect/fixed/untouched) are over every reviewable cue.
#[tokio::test]
async fn list_counts_are_correct() {
    let store = standard_store().await;
    let counts = |s: &Store| {
        let s = s.clone();
        async move {
            let c = translation::list(&s, &ReviewFilters::default())
                .await
                .expect("list")
                .counts;
            (c.suspect, c.fixed, c.untouched)
        }
    };

    // suspect: cues 1 (too_fast) and 5 (failed) = 2; fixed: none locked; untouched:
    // cues 2 and 4 (no flags, not locked) = 2.
    assert_eq!(counts(&store).await, (2, 0, 2));

    // A clean edit of cue 1 re-runs its flags away and locks it: it moves out of
    // suspect and into fixed. Now suspect = {5}, fixed = {1}, untouched = {2,4}.
    translation::set_target(&store, 1, "နေကောင်းလား", &limits())
        .await
        .expect("set");
    assert_eq!(counts(&store).await, (1, 1, 2));

    // ADR-038 (d): the counts are NOT a partition. Editing cue 2 with a target that
    // still trips a soft flag (`too_long`, 3 lines) but preserves ⟦0⟧ locks it AND
    // leaves it flagged, so it is counted in BOTH suspect and fixed.
    translation::set_target(&store, 2, "⟦0⟧\nမ\nမ", &limits())
        .await
        .expect("set cue 2");
    // suspect = {2 (too_long), 5 (failed)} = 2; fixed = {1, 2} = 2; untouched = {4} = 1.
    // The triple sums to 5 over 4 reviewable cues — cue 2 is double-counted (overlap).
    assert_eq!(counts(&store).await, (2, 2, 1));
}

// ---------------------------------------------------------------------------
// Preconditions: the translation stage must be completed (exit 3)
// ---------------------------------------------------------------------------

/// A store with cues and an approved analysis but the translation stage NOT completed.
async fn store_without_translation() -> Store {
    let store = Store::open_in_memory().expect("open");
    let cues = vec![cue(1, "Good morning.", true)];
    store
        .call(move |conn| {
            insert_project(conn, &project())?;
            insert_cues(conn, &cues)?;
            insert_analysis_version(conn, ANALYSIS, AnalysisOrigin::Llm, HASH, "prompt_v1|m")?;
            Ok(())
        })
        .await
        .expect("seed");
    store
}

// AC: ARCH §2 / CLI.md §7 — `show` before translation completes is a precondition
// failure (exit 3).
#[tokio::test]
async fn show_before_translation_is_precondition() {
    let store = store_without_translation().await;
    let err = translation::show(&store, 1).await.expect_err("not ready");
    assert!(matches!(err, ReviewError::TranslationNotReady), "{err:?}");
    assert_eq!(err.exit_code(), exit::PRECONDITION);
}

// AC: ARCH §2 / CLI.md §7 — `approve` before translation completes is a precondition
// failure (exit 3).
#[tokio::test]
async fn approve_before_translation_is_precondition() {
    let store = store_without_translation().await;
    let err = translation::approve(&store).await.expect_err("not ready");
    assert!(matches!(err, ReviewError::TranslationNotReady), "{err:?}");
    assert_eq!(err.exit_code(), exit::PRECONDITION);
}

// AC: ARCH §2 — approving a completed translation review opens the output gate.
#[tokio::test]
async fn approve_opens_output_gate() {
    let store = standard_store().await;

    // Output is blocked before approval.
    let state = load_state(&store).await.expect("state");
    assert!(can_run(Stage::Output, &state).is_err());

    translation::approve(&store).await.expect("approve");

    let state = load_state(&store).await.expect("state");
    assert_eq!(state.translation_review, StageStatus::Approved);
    assert!(
        can_run(Stage::Output, &state).is_ok(),
        "approved translation review opens output"
    );
}

// ---------------------------------------------------------------------------
// edit via the injectable launcher
// ---------------------------------------------------------------------------

/// A fake editor that overwrites the scratch file with fixed content.
struct FakeEditor {
    content: String,
}

impl EditorLauncher for FakeEditor {
    fn launch(&self, path: &Path) -> Result<(), ReviewError> {
        std::fs::write(path, &self.content).expect("write scratch");
        Ok(())
    }
}

// AC: CLI.md §6 — editing with a valid edit applies and locks the changed cue.
#[tokio::test]
async fn edit_valid_applies_and_locks() {
    let store = standard_store().await;
    let dir = tempfile::tempdir().expect("tempdir");

    let launcher = FakeEditor {
        content: review_file(HASH, &[(1, "နေကောင်းလား")]),
    };
    let summary = translation::edit(
        &store,
        launcher,
        dir.path(),
        &ReviewSelection::default(),
        &limits(),
    )
    .await
    .expect("edit");

    assert_eq!(summary.changed, 1);
    assert_eq!(
        translation_text(&store, 1).await.as_deref(),
        Some("နေကောင်းလား")
    );
    assert!(is_locked(&store, 1).await);
}

// AC: M7-AC2 — editing with a placeholder-breaking target is rejected naming the cue,
// and the stored translations are unchanged.
#[tokio::test]
async fn edit_invalid_placeholder_is_rejected_and_unchanged() {
    let store = standard_store().await;
    let dir = tempfile::tempdir().expect("tempdir");

    let launcher = FakeEditor {
        content: review_file(HASH, &[(2, "dropped placeholder")]),
    };
    let err = translation::edit(
        &store,
        launcher,
        dir.path(),
        &ReviewSelection::default(),
        &limits(),
    )
    .await
    .expect_err("rejected");

    assert!(
        matches!(err, ReviewError::PlaceholderMismatch { id: 2 }),
        "{err:?}"
    );
    assert_eq!(
        translation_text(&store, 2).await.as_deref(),
        Some("⟦0⟧ မင်္ဂလာ"),
        "cue 2 unchanged after a rejected edit"
    );
    assert!(!is_locked(&store, 2).await);
}

// ---------------------------------------------------------------------------
// set — placeholder mismatch is a hard reject (M7-AC2)
// ---------------------------------------------------------------------------

// AC: M7-AC2 — `review set` with a placeholder-breaking target is rejected naming the
// cue id and nothing is applied.
#[tokio::test]
async fn set_rejects_placeholder_mismatch() {
    let store = standard_store().await;
    let err = translation::set_target(&store, 2, "no placeholder", &limits())
        .await
        .expect_err("rejected");
    assert!(
        matches!(err, ReviewError::PlaceholderMismatch { id: 2 }),
        "{err:?}"
    );
    assert_eq!(
        translation_text(&store, 2).await.as_deref(),
        Some("⟦0⟧ မင်္ဂလာ")
    );
    assert!(!is_locked(&store, 2).await);
}

// ---------------------------------------------------------------------------
// export — atomic review file (`docs/ARCHITECTURE.md` §6 rule 4)
// ---------------------------------------------------------------------------

// AC: ARCH §6 rule 4 — export writes a complete review file (no leftover .tmp).
#[tokio::test]
async fn export_writes_complete_review_file() {
    let store = standard_store().await;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("review.json");
    translation::export(&store, &path, &ReviewSelection::default())
        .await
        .expect("export");
    let bytes = std::fs::read(&path).expect("read");
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("valid json");
    assert_eq!(parsed["project_source_hash"], HASH);
    assert!(parsed["cues"].as_array().expect("cues").len() >= 4);
    assert!(!dir.path().join("review.json.tmp").exists());
}

// AC: CLI.md §6 — `show` renders a known cue's source, target, and flags.
#[tokio::test]
async fn show_renders_cue() {
    let store = standard_store().await;
    let rendered = translation::show(&store, 1).await.expect("show");
    assert!(rendered.contains("Cue 1"));
    assert!(rendered.contains(MM), "target shown");
}

// AC: CLI.md §6 — `unlock` clears the locked flag on a previously locked cue.
#[tokio::test]
async fn unlock_clears_locked_flag() {
    let store = standard_store().await;
    translation::set_target(&store, 1, "နေကောင်းလား", &limits())
        .await
        .expect("set");
    assert!(is_locked(&store, 1).await);
    translation::unlock(&store, 1).await.expect("unlock");
    assert!(!is_locked(&store, 1).await, "cue 1 unlocked");
}

// AC: CLI.md §7 — `unlock` on an unknown cue id is an error naming the id.
#[tokio::test]
async fn unlock_unknown_cue_is_error() {
    let store = standard_store().await;
    let err = translation::unlock(&store, 999).await.expect_err("unknown");
    assert!(
        matches!(err, ReviewError::UnknownCue { id: 999 }),
        "{err:?}"
    );
}

// ---------------------------------------------------------------------------
// BLOCKER (ADR-038): a genuinely-failed batch opens the review and is listed
// ---------------------------------------------------------------------------

// AC: ARCH §2 / ADR-038 — a run where one batch genuinely FAILS still completes the
// translation stage (exit 4), so `review list` OPENS (not TranslationNotReady) and the
// failed cue is listed by `--failed` and counted as suspect. Uses the real pipeline,
// not a forced stage + missing row.
#[tokio::test]
async fn genuinely_failed_batch_opens_review_and_lists_failed_cue() {
    // Two translatable cues, batch_size 1 → batch 1 = [1], batch 2 = [2]. The Fake only
    // ever returns id 1, so batch 2 (cue 2) never gets covered and fails after its retry.
    let cues = vec![cue(1, "Good morning.", true), cue(2, "Be careful.", true)];
    let store = seed_base(&cues).await;
    let token = CancellationToken::new();
    let fake = Fake::new().always(|| Ok(lines_response(&[(1, MM)])));

    let outcome = run_translation(
        fake,
        &store,
        &cues,
        &params(1),
        TranslationFlags::default(),
        &token,
        std::sync::Arc::new(myansub::pipeline::progress::NoopProgress),
    )
    .await
    .expect("run");
    assert_eq!(
        outcome,
        TranslationOutcome::CompletedWithFailures {
            failed: 1,
            incomplete: 0,
        },
        "one batch failed, none incomplete"
    );

    // The stage is `completed` (failed units allowed) and no batch is pending, so the
    // gate the review and orchestrator share is open.
    let state = load_state(&store).await.expect("state");
    assert_eq!(state.translation, StageStatus::Completed, "stage completed");
    assert!(
        can_run(Stage::TranslationReview, &state).is_ok(),
        "the orchestrator gate opens with a failed batch"
    );

    // `review list` opens (no TranslationNotReady) and lists cue 2 as failed/suspect.
    let listing = translation::list(&store, &ReviewFilters::default())
        .await
        .expect("review list opens with a failed batch");
    assert_eq!(
        listing.cues.iter().map(|c| c.id).collect::<Vec<_>>(),
        vec![1, 2]
    );
    let failed = ReviewFilters {
        failed: true,
        ..ReviewFilters::default()
    };
    assert_eq!(
        list_ids(&store, failed).await,
        vec![2],
        "--failed lists cue 2"
    );
    // cue 1 translated (no flags), cue 2 failed (synthetic Failed flag) → suspect 1.
    assert_eq!(listing.counts.suspect, 1);
    assert_eq!(listing.counts.untouched, 1);
}

// AC: ADR-038 — the gate requires NO pending units: a `completed` stage that still has
// a pending batch (e.g. after `--retry-failed` requeued one) keeps the review closed.
#[tokio::test]
async fn pending_batch_keeps_review_closed() {
    let store = standard_store().await;
    // standard_store is open; add a pending batch to simulate requeued work.
    store
        .call(|conn| {
            let tx = conn.transaction()?;
            create_translation_batch(
                &tx,
                &NewTranslationBatch {
                    batch_no: 9,
                    first_cue: 1,
                    last_cue: 1,
                    input_hash: None,
                    model: Some(MODEL.to_string()),
                    analysis_version: Some(1),
                },
            )?;
            tx.commit()?;
            Ok(())
        })
        .await
        .expect("create pending batch");
    let pending = store
        .call(|c| count_translation_batches_by_status(c, UnitStatus::Pending))
        .await
        .expect("count");
    assert_eq!(pending, 1, "one pending batch");

    let err = translation::list(&store, &ReviewFilters::default())
        .await
        .expect_err("pending blocks review");
    assert!(matches!(err, ReviewError::TranslationNotReady), "{err:?}");
    assert_eq!(err.exit_code(), exit::PRECONDITION);
}

// ---------------------------------------------------------------------------
// Import reject causes (`docs/DATA_MODEL.md` §6, ADR-038): parse error, missing id,
// and partial-file acceptance
// ---------------------------------------------------------------------------

// AC: DATA_MODEL §6 — malformed JSON is a parse error; nothing is applied.
#[tokio::test]
async fn import_rejects_malformed_json() {
    let store = standard_store().await;
    let err = translation::import(&store, "not json {", &limits())
        .await
        .expect_err("parse");
    assert!(matches!(err, ReviewError::Parse(_)), "{err:?}");
    assert_eq!(err.exit_code(), exit::ERROR);
    assert_eq!(translation_text(&store, 1).await.as_deref(), Some(MM));
    assert!(!is_locked(&store, 1).await);
}

// AC: DATA_MODEL §6 — a cue entry lacking its `id` field is a shape/parse error that
// rejects the whole import; nothing is applied.
#[tokio::test]
async fn import_rejects_cue_missing_id_field() {
    let store = standard_store().await;
    let body = serde_json::json!({
        "project_source_hash": HASH,
        "analysis_version": 1,
        "cues": [ { "target": "နေကောင်းလား" } ],
    })
    .to_string();
    let err = translation::import(&store, &body, &limits())
        .await
        .expect_err("missing id");
    assert!(matches!(err, ReviewError::Parse(_)), "{err:?}");
    assert_eq!(translation_text(&store, 1).await.as_deref(), Some(MM));
}

// AC: ADR-038 (c) — a PARTIAL file (only some project cues listed) is valid: the listed
// cue is applied and cues absent from the file are left untouched.
#[tokio::test]
async fn import_partial_file_is_valid() {
    let store = standard_store().await;
    // Only cue 1 is listed; cues 2 and 4 are absent from the file.
    let summary = translation::import(&store, &review_file(HASH, &[(1, "နေကောင်းလား")]), &limits())
        .await
        .expect("partial file accepted");
    assert_eq!((summary.changed, summary.unchanged), (1, 0));
    assert_eq!(
        translation_text(&store, 1).await.as_deref(),
        Some("နေကောင်းလား"),
        "listed cue applied"
    );
    assert_eq!(
        translation_text(&store, 2).await.as_deref(),
        Some("⟦0⟧ မင်္ဂလာ"),
        "absent cue untouched"
    );
    assert!(!is_locked(&store, 2).await, "absent cue not locked");
}

// ---------------------------------------------------------------------------
// Usage-error (exit 2) argument parsing helpers (`docs/CLI.md` §6/§7)
// ---------------------------------------------------------------------------

// AC: CLI.md §7 — a non-numeric cue id is an InvalidArgument usage error (exit 2).
#[test]
fn parse_cue_id_rejects_non_numeric() {
    let err = translation::parse_cue_id("abc").expect_err("bad id");
    assert!(matches!(err, ReviewError::InvalidArgument(_)), "{err:?}");
    assert_eq!(err.exit_code(), exit::USAGE);
    assert_eq!(translation::parse_cue_id(" 42 ").expect("ok"), 42);
}

// AC: CLI.md §7 — a malformed or descending range is a usage error (exit 2).
#[test]
fn parse_id_range_rejects_bad_range() {
    for bad in ["1", "a-b", "5-2", "1-"] {
        let err = translation::parse_id_range(bad).expect_err("bad range");
        assert!(
            matches!(err, ReviewError::InvalidArgument(_)),
            "{bad}: {err:?}"
        );
        assert_eq!(err.exit_code(), exit::USAGE);
    }
    assert_eq!(translation::parse_id_range("2-4").expect("ok"), (2, 4));
}

// AC: CLI.md §7 — an unknown `--flag` is routed through ReviewError::UnknownFlag (exit 2).
#[test]
fn parse_flag_rejects_unknown() {
    let err = translation::parse_flag("bogus").expect_err("unknown flag");
    assert!(matches!(err, ReviewError::UnknownFlag(_)), "{err:?}");
    assert_eq!(err.exit_code(), exit::USAGE);
    assert_eq!(
        translation::parse_flag("too_fast").expect("ok"),
        QualityFlag::TooFast
    );
}
