//! M6-B1 translation execution tests via the public pipeline surface, driven by the
//! scripted `Fake` transport (`docs/TESTING.md` §3/§4). No network; `tempfile`
//! stores for the crash-resume case.
//!
//! Covers: M6-AC1 (fake end-to-end translation; all batches `done`, every
//! translatable cue translated, non-translatable cues skipped), M6-AC2 (kill between
//! batches, no batch sent twice), input_hash reuse, batch planning / context /
//! speaker-prefix rules, quality flagging, the tier-1 whole-batch content retry, and
//! `--dry-run`.

use std::time::Duration;

use myansub::domain::{Cue, QualityFlag};
use myansub::llm::client::{CommitToken, LlmResponse, ServiceTier, ThinkingSetting, Usage};
use myansub::llm::dry_run::Estimator;
use myansub::llm::fake::Fake;
use myansub::llm::pricing::{Cost, ModelPrice, PriceTable};
use myansub::llm::retry::RetryConfig;
use myansub::pipeline::scheduler::SchedulerConfig;
use myansub::pipeline::translation::{
    TranslationFlags, TranslationOutcome, TranslationParams, dry_run_translation, run_translation,
    translation_exit_code,
};
use myansub::quality::checks::Limits;
use myansub::store::repo_analysis::AnalysisOrigin;
use myansub::store::repo_analysis::{approve_current_analysis, insert_analysis_version};
use myansub::store::repo_cues::insert_cues;
use myansub::store::repo_project::set_stage;
use myansub::store::repo_translations::{
    NewTranslation, TranslationOrigin, cue_id_by_idx, get_translation, list_translations,
    upsert_translation,
};
use myansub::store::repo_units::{
    NewTranslationBatch, claim_translation_batch, complete_translation_batch,
    count_translation_batches_by_status, create_translation_batch, get_translation_batch,
};
use myansub::store::{Stage, StageStatus, Store, UnitStatus};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

const MODEL: &str = "gemini-flash-latest";
const MM: &str = "မင်္ဂလာပါ"; // Burmese, so `untranslated` never fires

/// An approved analysis document whose single scene covers the whole cue range and
/// names one character (`c1` → Aoi).
const ANALYSIS: &str = r#"{"schema_version":1,"premise":"p","tone":"t",
"characters":[{"id":"c1","name":"Aoi","gender":"f","age_group":"young"}],
"address_matrix":[],"glossary":[],
"scenes":[{"cue_range":[1,9],"summary":"a quiet morning","speakers":["c1"]}],
"style_rules":[],"unresolved_ambiguities":[]}"#;

fn cue_full(idx: u32, masked: &str, speaker: Option<&str>, translatable: bool) -> Cue {
    Cue {
        idx,
        start_ms: u64::from(idx) * 1000,
        end_ms: u64::from(idx) * 1000 + 900,
        text_raw: masked.to_string(),
        text_masked: masked.to_string(),
        lead_tags: None,
        inline_tags: vec![],
        style: None,
        speaker: speaker.map(str::to_string),
        translatable,
    }
}

/// Cues 1,2 translatable; 3 non-translatable; 4,5 translatable. Cue 1 has a known
/// speaker (c1), cue 2 does not.
fn mixed_cues() -> Vec<Cue> {
    vec![
        cue_full(1, "Good morning.", Some("c1"), true),
        cue_full(2, "Where are you going?", None, true),
        cue_full(3, "...", None, false),
        cue_full(4, "To the market.", None, true),
        cue_full(5, "Be careful.", None, true),
    ]
}

fn estimator() -> Estimator {
    Estimator {
        bytes_per_token_latin: 4.0,
        bytes_per_token_myanmar: 3.0,
        bytes_per_token_cjk: 3.0,
    }
}

fn retry() -> RetryConfig {
    RetryConfig {
        max_retries: 2,
        backoff_base_ms: 1,
        backoff_max_ms: 2,
        max_sheds_before_fallback: 1,
        service_tier_fallback: true,
    }
}

fn scheduler() -> SchedulerConfig {
    // Deterministic FIFO alignment with the scripted fake: one unit at a time, no
    // warm-up reordering.
    SchedulerConfig {
        concurrency: 1,
        warm_cache_first: false,
        grace: Duration::from_secs(5),
    }
}

fn limits() -> Limits {
    Limits {
        max_graphemes_per_line: 42,
        max_lines: 2,
        max_gps: 20.0,
        drift_ratio: 3.0,
    }
}

/// Params with `batch_size` cues per batch and no context lines (clean call log).
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
        estimator: estimator(),
        limits: limits(),
        scheduler: scheduler(),
        seed: 42,
        retry: retry(),
    }
}

/// Builds a `{"lines":[{"id","t"}]}` response.
fn lines_response(pairs: &[(i64, &str)]) -> LlmResponse {
    let items: Vec<serde_json::Value> = pairs
        .iter()
        .map(|(id, t)| serde_json::json!({ "id": id, "t": t }))
        .collect();
    let text = serde_json::json!({ "lines": items }).to_string();
    text_response(&text)
}

fn text_response(text: &str) -> LlmResponse {
    LlmResponse {
        text: text.to_string(),
        usage: Usage {
            prompt_tokens: 50,
            output_tokens: 20,
            ..Usage::default()
        },
        commit: CommitToken::noop(),
    }
}

/// Seeds an in-memory store with `cues` and an approved analysis version.
async fn seed(cues: &[Cue]) -> Store {
    let store = Store::open_in_memory().expect("open");
    let cues = cues.to_vec();
    store
        .call(move |conn| insert_cues(conn, &cues))
        .await
        .expect("cues");
    store
        .call(|conn| {
            insert_analysis_version(
                conn,
                ANALYSIS,
                AnalysisOrigin::Llm,
                "sha256:proj",
                "prompt_v1|m",
            )
            .map(|_| ())
        })
        .await
        .expect("analysis");
    store
        .call(|conn| approve_current_analysis(conn).map(|_| ()))
        .await
        .expect("approve");
    store
}

/// The concatenated content text of each recorded request.
fn call_texts(fake: &Fake) -> Vec<String> {
    fake.calls()
        .into_iter()
        .map(|r| {
            r.contents
                .iter()
                .map(|b| b.text.clone())
                .collect::<String>()
        })
        .collect()
}

// ---------------------------------------------------------------------------
// M6-AC1: fake end-to-end translation; all batches done
// ---------------------------------------------------------------------------

// AC: M6-AC1 — a full translation run marks every batch `done`, stores one
// `origin=llm` translation per translatable cue, leaves non-translatable cues
// untranslated, and sends each batch exactly once.
#[tokio::test]
async fn end_to_end_translates_every_batch_once() {
    let cues = mixed_cues();
    let store = seed(&cues).await;
    // Every call returns all four translatable lines; each batch persists only its
    // own cues, so the test is independent of dispatch order.
    let fake = Fake::new().always(|| Ok(lines_response(&[(1, MM), (2, MM), (4, MM), (5, MM)])));
    let token = CancellationToken::new();

    let outcome = run_translation(
        fake.clone(),
        &store,
        &cues,
        &params(2),
        TranslationFlags::default(),
        &token,
    )
    .await
    .expect("run");

    // translatable = [1,2,4,5]; batch_size 2 → 2 batches.
    assert_eq!(outcome, TranslationOutcome::Completed { batches: 2 });
    assert_eq!(fake.call_count(), 2, "one call per batch");

    let done = store
        .call(|c| count_translation_batches_by_status(c, UnitStatus::Done))
        .await
        .expect("count");
    assert_eq!(done, 2, "all batches done");

    let translations = store.call(|c| list_translations(c)).await.expect("list");
    assert_eq!(translations.len(), 4, "one per translatable cue");
    assert!(
        translations
            .iter()
            .all(|t| t.origin == myansub::store::repo_translations::TranslationOrigin::Llm),
        "all origin=llm"
    );

    // The non-translatable cue (idx 3) has no translation.
    let id3 = store
        .call(|c| Ok(cue_id_by_idx(c)?.get(&3).copied()))
        .await
        .expect("map")
        .expect("cue 3 present");
    let none = store
        .call(move |c| get_translation(c, id3))
        .await
        .expect("get");
    assert!(none.is_none(), "non-translatable cue is not translated");

    // Each batch's TRANSLATE lines appear exactly once (no double send).
    assert!(fake.sent_exactly_once(|r| joined(r).contains("[1]")));
    assert!(fake.sent_exactly_once(|r| joined(r).contains("[4]")));
}

fn joined(req: &myansub::llm::client::LlmRequest) -> String {
    req.contents.iter().map(|b| b.text.clone()).collect()
}

// ---------------------------------------------------------------------------
// M6-AC2: kill between batches; no batch sent twice
// ---------------------------------------------------------------------------

// AC: M6-AC2 — a multi-batch run is interrupted after two batches complete; a rerun
// on the reopened store finishes the rest. Across both runs every batch's request is
// sent exactly once, and all batches end `done`.
#[tokio::test]
async fn kill_between_batches_sends_no_batch_twice() {
    let tmp = tempdir().expect("tempdir");
    let db = tmp.path().join("project.db");
    let cues = mixed_cues(); // translatable [1,2,4,5]; batch_size 1 → 4 batches

    // Seed cues + approved analysis once on a file-backed store.
    {
        let store = Store::open(&db).expect("open seed");
        let c = cues.clone();
        store
            .call(move |conn| insert_cues(conn, &c))
            .await
            .expect("cues");
        store
            .call(|conn| {
                insert_analysis_version(
                    conn,
                    ANALYSIS,
                    AnalysisOrigin::Llm,
                    "sha256:proj",
                    "prompt_v1|m",
                )
                .map(|_| ())
            })
            .await
            .expect("analysis");
        store
            .call(|conn| approve_current_analysis(conn).map(|_| ()))
            .await
            .expect("approve");
    }

    // --- Run 1: cancel after the 2nd batch completes. ---
    let token1 = CancellationToken::new();
    let fake1 = Fake::new()
        .always(|| Ok(lines_response(&[(1, MM), (2, MM), (4, MM), (5, MM)])))
        .cancel_after(token1.clone(), 2);
    {
        let store = Store::open(&db).expect("open 1");
        let outcome = run_translation(
            fake1.clone(),
            &store,
            &cues,
            &params(1),
            TranslationFlags::default(),
            &token1,
        )
        .await
        .expect("run 1");
        assert_eq!(outcome, TranslationOutcome::Interrupted);
    }
    assert_eq!(fake1.call_count(), 2, "run 1 sent exactly two batches");

    // --- Run 2: reopen and finish. ---
    let token2 = CancellationToken::new();
    let fake2 = Fake::new().always(|| Ok(lines_response(&[(1, MM), (2, MM), (4, MM), (5, MM)])));
    {
        let store = Store::open(&db).expect("open 2");
        let outcome = run_translation(
            fake2.clone(),
            &store,
            &cues,
            &params(1),
            TranslationFlags::default(),
            &token2,
        )
        .await
        .expect("run 2");
        assert_eq!(outcome, TranslationOutcome::Completed { batches: 4 });

        let done = store
            .call(|c| count_translation_batches_by_status(c, UnitStatus::Done))
            .await
            .expect("count");
        assert_eq!(done, 4, "all four batches done");
    }
    assert_eq!(
        fake2.call_count(),
        2,
        "run 2 sent the two remaining batches"
    );

    // Each batch's single TRANSLATE line appears exactly once across both runs.
    let all: Vec<String> = call_texts(&fake1)
        .into_iter()
        .chain(call_texts(&fake2))
        .collect();
    for marker in ["[1]", "[2]", "[4]", "[5]"] {
        let hits = all.iter().filter(|t| t.contains(marker)).count();
        assert_eq!(hits, 1, "batch for {marker} sent exactly once");
    }
}

// ---------------------------------------------------------------------------
// input_hash reuse: a done batch with unchanged input is not resent
// ---------------------------------------------------------------------------

// AC: M6-B1 — rerunning a fully-translated project sends nothing (every `done` batch
// with a matching input_hash is skipped), and the batches stay done.
#[tokio::test]
async fn done_batches_are_not_resent_on_rerun() {
    let cues = mixed_cues();
    let store = seed(&cues).await;
    let token = CancellationToken::new();

    let fake1 = Fake::new().always(|| Ok(lines_response(&[(1, MM), (2, MM), (4, MM), (5, MM)])));
    run_translation(
        fake1.clone(),
        &store,
        &cues,
        &params(2),
        TranslationFlags::default(),
        &token,
    )
    .await
    .expect("run 1");
    assert_eq!(fake1.call_count(), 2);

    // Each batch stored a non-null input_hash.
    let hash1 = store
        .call(|c| get_translation_batch(c, 1))
        .await
        .expect("get")
        .expect("batch 1");
    assert!(hash1.input_hash.is_some(), "input_hash persisted");

    // Rerun: nothing to send.
    let fake2 = Fake::new().always(|| Ok(lines_response(&[(1, MM), (2, MM), (4, MM), (5, MM)])));
    let outcome = run_translation(
        fake2.clone(),
        &store,
        &cues,
        &params(2),
        TranslationFlags::default(),
        &token,
    )
    .await
    .expect("run 2");
    assert_eq!(outcome, TranslationOutcome::Completed { batches: 2 });
    assert_eq!(fake2.call_count(), 0, "done batches are not resent");
}

// ---------------------------------------------------------------------------
// Batch planning: boundaries, translatable-only, context, speaker prefix
// ---------------------------------------------------------------------------

// AC: M6-B1 — batches cover only translatable cues in contiguous groups of
// batch_size; context lines are translatable-only; the speaker prefix appears only
// for a cue with a known speaker.
#[tokio::test]
async fn batch_planning_context_and_speaker_prefix() {
    let cues = mixed_cues(); // translatable [1,2,4,5]; non-translatable 3
    let store = seed(&cues).await;
    let token = CancellationToken::new();
    let fake = Fake::new().always(|| Ok(lines_response(&[(1, MM), (2, MM), (4, MM), (5, MM)])));

    // batch_size 2 with one context line on each side.
    let mut p = params(2);
    p.context_before = 1;
    p.context_after = 1;

    run_translation(
        fake.clone(),
        &store,
        &cues,
        &p,
        TranslationFlags::default(),
        &token,
    )
    .await
    .expect("run");

    let texts = call_texts(&fake);
    assert_eq!(texts.len(), 2, "two batches");

    // Batch 1 translates [1,2]; its CONTEXT AFTER is the next *translatable* cue [4],
    // never the non-translatable [3].
    let b1 = texts
        .iter()
        .find(|t| t.contains("TRANSLATE:") && t.contains("[1]"))
        .expect("batch 1");
    assert!(b1.contains("[2]"), "batch 1 translates cue 2");
    assert!(
        b1.contains("CONTEXT AFTER"),
        "batch 1 has a trailing context"
    );
    assert!(
        b1.contains("[4]"),
        "context after skips non-translatable 3 to 4"
    );
    assert!(!b1.contains("[3]"), "non-translatable cue 3 is never sent");
    // Speaker prefix only for the cue with a known speaker.
    assert!(
        b1.contains("[1] c1: Good morning."),
        "known speaker prefixed"
    );
    assert!(
        b1.contains("[2] Where are you going?"),
        "unknown speaker unprefixed"
    );

    // Batch 2 translates [4,5]; its CONTEXT BEFORE is the previous translatable cue
    // [2], not the non-translatable [3].
    let b2 = texts
        .iter()
        .find(|t| t.contains("TRANSLATE:") && t.contains("[5]"))
        .expect("batch 2");
    assert!(b2.contains("[4]"), "batch 2 translates cue 4");
    assert!(
        b2.contains("CONTEXT BEFORE"),
        "batch 2 has a leading context"
    );
    assert!(
        b2.contains("[2]"),
        "context before is the translatable cue 2"
    );
    assert!(!b2.contains("[3]"), "non-translatable cue 3 is never sent");
}

// AC: M6-B1 — the cached prefix (system instruction + canonical analysis sheet) is
// byte-identical across batches (ADR-006), so implicit caching applies.
#[tokio::test]
async fn cached_prefix_is_stable_across_batches() {
    let cues = mixed_cues();
    let store = seed(&cues).await;
    let token = CancellationToken::new();
    let fake = Fake::new().always(|| Ok(lines_response(&[(1, MM), (2, MM), (4, MM), (5, MM)])));

    run_translation(
        fake.clone(),
        &store,
        &cues,
        &params(2),
        TranslationFlags::default(),
        &token,
    )
    .await
    .expect("run");

    let calls = fake.calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(
        calls[0].system_instruction, calls[1].system_instruction,
        "system instruction is stable"
    );
    assert_eq!(
        calls[0].contents[0], calls[1].contents[0],
        "the analysis-sheet prefix block is byte-identical"
    );
    assert_ne!(
        calls[0].contents[1], calls[1].contents[1],
        "only the variable block differs per batch"
    );
}

// ---------------------------------------------------------------------------
// Quality flagging (store flags, no retry — retry is M6-B2)
// ---------------------------------------------------------------------------

// AC: M6-B2 — a response that drops a cue's placeholder (a hard flag) drives one
// isolated tier-3 per-line retry; when the retry does not fix it the best result is
// stored WITH the `placeholder_mismatch` flag and the batch still completes `done`.
// (This replaces the M6-B1 "no per-flag retry" assertion, which tier 3 supersedes.)
#[tokio::test]
async fn hard_flag_drives_tier3_retry_then_stored_flagged() {
    let cues = vec![cue_full(1, "Hello ⟦0⟧", None, true)];
    let store = seed(&cues).await;
    let token = CancellationToken::new();
    // Target drops the ⟦0⟧ placeholder → placeholder_mismatch, every time.
    let fake = Fake::new().always(|| Ok(lines_response(&[(1, MM)])));

    let outcome = run_translation(
        fake.clone(),
        &store,
        &cues,
        &params(80),
        TranslationFlags::default(),
        &token,
    )
    .await
    .expect("run");
    assert_eq!(outcome, TranslationOutcome::Completed { batches: 1 });
    assert_eq!(
        fake.call_count(),
        2,
        "original call + exactly one isolated tier-3 per-line retry"
    );

    let id1 = store
        .call(|c| Ok(cue_id_by_idx(c)?.get(&1).copied()))
        .await
        .expect("map")
        .expect("cue 1");
    let row = store
        .call(move |c| get_translation(c, id1))
        .await
        .expect("get")
        .expect("translated");
    assert!(
        row.flags.contains(&QualityFlag::PlaceholderMismatch),
        "placeholder_mismatch stored after the retry failed to fix it, got {:?}",
        row.flags
    );
}

// ---------------------------------------------------------------------------
// Whole-batch content retry (tier 1)
// ---------------------------------------------------------------------------

// AC: M6-B1 — an unparseable response then a valid one retries the whole batch once
// and succeeds; the two content attempts are distinct from transport retries (which
// never fire, as each response is a transport success).
#[tokio::test]
async fn whole_batch_content_retry_then_success() {
    let cues = mixed_cues();
    let store = seed(&cues).await;
    let token = CancellationToken::new();
    let fake = Fake::new()
        .push_response(text_response("this is not json {"))
        .push_response(lines_response(&[(1, MM), (2, MM), (4, MM), (5, MM)]))
        .always(|| Ok(lines_response(&[(1, MM), (2, MM), (4, MM), (5, MM)])));

    // One batch holds all four translatable cues.
    let outcome = run_translation(
        fake.clone(),
        &store,
        &cues,
        &params(80),
        TranslationFlags::default(),
        &token,
    )
    .await
    .expect("run");

    assert_eq!(outcome, TranslationOutcome::Completed { batches: 1 });
    assert_eq!(
        fake.call_count(),
        2,
        "invalid then valid: one content retry"
    );
    let translations = store.call(|c| list_translations(c)).await.expect("list");
    assert_eq!(translations.len(), 4, "the retried batch stored its cues");
}

// AC: M6-B1 — a batch that never returns valid JSON is failed after the tier-1
// whole-batch retry (`docs/LLM.md` §6.1: retry the whole batch *once*), so the call
// budget here is original + 1 retry, not the full max_content_attempts. Maps to exit 4.
#[tokio::test]
async fn content_retry_exhaustion_fails_the_batch_exit_4() {
    let cues = vec![cue_full(1, "Hello.", None, true)];
    let store = seed(&cues).await;
    let token = CancellationToken::new();
    let fake = Fake::new().always(|| Ok(text_response("not json {")));

    let outcome = run_translation(
        fake.clone(),
        &store,
        &cues,
        &params(80),
        TranslationFlags::default(),
        &token,
    )
    .await
    .expect("run");

    assert_eq!(
        outcome,
        TranslationOutcome::CompletedWithFailures {
            failed: 1,
            incomplete: 0,
        }
    );
    assert_eq!(translation_exit_code(false, &outcome), 4);
    assert_eq!(
        fake.call_count(),
        2,
        "original call + exactly one whole-batch retry (§6.1)"
    );
    let failed = store
        .call(|c| count_translation_batches_by_status(c, UnitStatus::Failed))
        .await
        .expect("count");
    assert_eq!(failed, 1);
}

// ---------------------------------------------------------------------------
// Coverage: a response that omits batch cue ids is a content failure, never a
// silent skip-and-done (review #1, BLOCKER).
// ---------------------------------------------------------------------------

// AC: M6-B1 — a response that persistently omits one of the batch's cue ids is a
// tier-1 content failure driving the whole-batch retry; on exhaustion the batch is
// FAILED (never marked done), so the omitted cue is never silently dropped.
#[tokio::test]
async fn omitted_batch_cue_id_fails_the_batch() {
    // Two translatable cues in one batch; the response only ever returns id 1.
    let cues = vec![
        cue_full(1, "Good morning.", None, true),
        cue_full(2, "Where are you going?", None, true),
    ];
    let store = seed(&cues).await;
    let token = CancellationToken::new();
    let fake = Fake::new().always(|| Ok(lines_response(&[(1, MM)])));

    let outcome = run_translation(
        fake.clone(),
        &store,
        &cues,
        &params(2),
        TranslationFlags::default(),
        &token,
    )
    .await
    .expect("run");

    assert_eq!(
        outcome,
        TranslationOutcome::CompletedWithFailures {
            failed: 1,
            incomplete: 0,
        }
    );
    assert_eq!(
        fake.call_count(),
        2,
        "original call + one whole-batch retry"
    );

    let failed = store
        .call(|c| count_translation_batches_by_status(c, UnitStatus::Failed))
        .await
        .expect("count");
    let done = store
        .call(|c| count_translation_batches_by_status(c, UnitStatus::Done))
        .await
        .expect("count");
    assert_eq!(failed, 1, "the incomplete batch is failed");
    assert_eq!(done, 0, "the incomplete batch is never marked done");

    // Nothing is committed for a batch that never covered its cues (no silent skip).
    let translations = store.call(|c| list_translations(c)).await.expect("list");
    assert!(
        translations.is_empty(),
        "no partial translations committed, got {translations:?}"
    );
}

// AC: M6-B1 — an empty `{"lines":[]}` response covers none of the batch's cues, so
// it is a content failure (not a success), and the batch ends failed.
#[tokio::test]
async fn empty_lines_response_fails_the_batch() {
    let cues = vec![
        cue_full(1, "Good morning.", None, true),
        cue_full(2, "Where are you going?", None, true),
    ];
    let store = seed(&cues).await;
    let token = CancellationToken::new();
    let fake = Fake::new().always(|| Ok(lines_response(&[])));

    let outcome = run_translation(
        fake.clone(),
        &store,
        &cues,
        &params(2),
        TranslationFlags::default(),
        &token,
    )
    .await
    .expect("run");

    assert_eq!(
        outcome,
        TranslationOutcome::CompletedWithFailures {
            failed: 1,
            incomplete: 0,
        }
    );
    assert_eq!(fake.call_count(), 2, "empty lines → one whole-batch retry");
    let done = store
        .call(|c| count_translation_batches_by_status(c, UnitStatus::Done))
        .await
        .expect("count");
    assert_eq!(done, 0, "an empty response is not a success");
}

// ---------------------------------------------------------------------------
// Crash-resume: a batch left in_flight across a Store drop/reopen is recovered to
// pending and re-sent (review #3), mirroring the analysis stage's template.
// ---------------------------------------------------------------------------

// AC: M6-AC2 — a batch claimed but not completed (left in_flight) across a Store
// drop/reopen is recovered to pending and re-sent exactly once on rerun, while an
// already-done batch is not re-sent.
#[tokio::test]
async fn interrupted_in_flight_batch_is_recovered_and_resent() {
    let tmp = tempdir().expect("tempdir");
    let db = tmp.path().join("project.db");
    // Two translatable cues; batch_size 1 → batch 1 = [1], batch 2 = [2].
    let cues = vec![
        cue_full(1, "Good morning.", Some("c1"), true),
        cue_full(2, "Where are you going?", None, true),
    ];

    // --- Simulate a crash: batch 2 done, batch 1 claimed (in_flight) but not done. ---
    {
        let store = Store::open(&db).expect("open 1");
        let c = cues.clone();
        store
            .call(move |conn| insert_cues(conn, &c))
            .await
            .expect("cues");
        store
            .call(|conn| {
                insert_analysis_version(
                    conn,
                    ANALYSIS,
                    AnalysisOrigin::Llm,
                    "sha256:proj",
                    "prompt_v1|m",
                )
                .map(|_| ())
            })
            .await
            .expect("analysis");
        store
            .call(|conn| approve_current_analysis(conn).map(|_| ()))
            .await
            .expect("approve");
        store
            .call(|conn| {
                let tx = conn.transaction()?;
                create_translation_batch(
                    &tx,
                    &NewTranslationBatch {
                        batch_no: 1,
                        first_cue: 1,
                        last_cue: 1,
                        input_hash: None,
                        model: Some(MODEL.to_string()),
                        analysis_version: Some(1),
                    },
                )?;
                create_translation_batch(
                    &tx,
                    &NewTranslationBatch {
                        batch_no: 2,
                        first_cue: 2,
                        last_cue: 2,
                        input_hash: None,
                        model: Some(MODEL.to_string()),
                        analysis_version: Some(1),
                    },
                )?;
                set_stage(&tx, Stage::Translation, StageStatus::Running)?;
                tx.commit()?;
                Ok(())
            })
            .await
            .expect("seed batches");
        // batch 2: claim then complete → done.
        store
            .call(|c| claim_translation_batch(c, 2).map(|_| ()))
            .await
            .expect("claim 2");
        store
            .call(|c| complete_translation_batch(c, 2).map(|_| ()))
            .await
            .expect("complete 2");
        // batch 1: claim only → left in_flight (the crash point).
        store
            .call(|c| claim_translation_batch(c, 1).map(|_| ()))
            .await
            .expect("claim 1");
        let in_flight = store
            .call(|c| count_translation_batches_by_status(c, UnitStatus::InFlight))
            .await
            .expect("count");
        assert_eq!(in_flight, 1, "batch 1 left in_flight");
    }

    // --- Reopen: Store::open recovers in_flight → pending. ---
    let store = Store::open(&db).expect("open 2");
    let pending = store
        .call(|c| count_translation_batches_by_status(c, UnitStatus::Pending))
        .await
        .expect("count");
    assert_eq!(pending, 1, "the interrupted batch was recovered to pending");

    // --- Rerun: the recovered batch is re-sent; the done batch is not. ---
    let token = CancellationToken::new();
    let fake = Fake::new().always(|| Ok(lines_response(&[(1, MM), (2, MM)])));
    let outcome = run_translation(
        fake.clone(),
        &store,
        &cues,
        &params(1),
        TranslationFlags::default(),
        &token,
    )
    .await
    .expect("rerun");
    assert_eq!(outcome, TranslationOutcome::Completed { batches: 2 });
    assert_eq!(fake.call_count(), 1, "only the recovered batch is re-sent");

    let texts = call_texts(&fake);
    assert!(
        texts.iter().any(|t| t.contains("[1]")),
        "recovered batch 1 re-sent"
    );
    assert!(
        !texts.iter().any(|t| t.contains("[2]")),
        "already-done batch 2 not re-sent"
    );

    let done = store
        .call(|c| count_translation_batches_by_status(c, UnitStatus::Done))
        .await
        .expect("count");
    assert_eq!(done, 2, "both batches end done");
}

// ---------------------------------------------------------------------------
// Nothing to do, deferred-flag inertness, and uneven batching
// ---------------------------------------------------------------------------

// AC: M6-B1 — a project with no translatable cues translates nothing: the outcome is
// NothingToDo (exit 0), no LLM call is made, and no rows are written.
#[tokio::test]
async fn no_translatable_cues_is_nothing_to_do() {
    let cues = vec![
        cue_full(1, "...", None, false),
        cue_full(2, "♪♪", None, false),
    ];
    let store = seed(&cues).await;
    let before = table_counts(&store).await;
    let token = CancellationToken::new();
    let fake = Fake::new().always(|| Ok(lines_response(&[(1, MM)])));

    let outcome = run_translation(
        fake.clone(),
        &store,
        &cues,
        &params(2),
        TranslationFlags::default(),
        &token,
    )
    .await
    .expect("run");

    assert_eq!(outcome, TranslationOutcome::NothingToDo);
    assert_eq!(translation_exit_code(false, &outcome), 0);
    assert_eq!(fake.call_count(), 0, "no LLM call for an empty plan");

    let after = table_counts(&store).await;
    assert_eq!(before, after, "nothing is written");
    assert_eq!(after.0, 0, "no translations");
    assert_eq!(after.1, 0, "no batch rows");
}

// AC: M6-B2 — `--redo` re-translates every unlocked cue (re-sending its batch) while a
// `locked = 1` cue is preserved and excluded from the request, so it is never re-sent
// or overwritten. (This replaces the M6-B1 "--redo is inert" assertion.) Dropping the
// locked cue rebuilds the batch's variable block, so the re-sent request is a genuine
// cache miss and appears in the Fake call log.
#[tokio::test]
async fn redo_retranslates_unlocked_and_preserves_locked() {
    // One batch with two cues; cue 2 will be reviewer-locked.
    let cues = vec![
        cue_full(1, "Good morning.", None, true),
        cue_full(2, "Where are you going?", None, true),
    ];
    let store = seed(&cues).await;
    let token = CancellationToken::new();

    let fake1 = Fake::new().always(|| Ok(lines_response(&[(1, MM), (2, MM)])));
    run_translation(
        fake1.clone(),
        &store,
        &cues,
        &params(2),
        TranslationFlags::default(),
        &token,
    )
    .await
    .expect("run 1");
    assert_eq!(fake1.call_count(), 1, "one batch, one call");

    // Lock cue 2 with reviewer text (simulating an M7 review set).
    let id2 = store
        .call(|c| Ok(cue_id_by_idx(c)?.get(&2).copied()))
        .await
        .expect("map")
        .expect("cue 2");
    store
        .call(move |c| {
            upsert_translation(
                c,
                &NewTranslation {
                    cue_id: id2,
                    text: "ကိုယ်တိုင်ပြင်".to_string(),
                    origin: TranslationOrigin::User,
                    locked: true,
                    stale: false,
                    flags: vec![],
                    analysis_version: 1,
                    model: None,
                },
            )
            .map(|_| ())
        })
        .await
        .expect("lock cue 2");

    // --redo: cue 1 (unlocked) is re-translated with a new target; cue 2 (locked) is
    // excluded from the request and left untouched.
    let mm2 = "နေကောင်းလား";
    let fake2 = Fake::new().always(move || Ok(lines_response(&[(1, mm2)])));
    let outcome = run_translation(
        fake2.clone(),
        &store,
        &cues,
        &params(2),
        TranslationFlags {
            redo: true,
            ..TranslationFlags::default()
        },
        &token,
    )
    .await
    .expect("redo rerun");

    assert_eq!(outcome, TranslationOutcome::Completed { batches: 1 });
    assert_eq!(fake2.call_count(), 1, "--redo re-sends the unlocked batch");

    // The re-sent request carries only the unlocked cue id.
    let texts = call_texts(&fake2);
    assert!(texts.iter().any(|t| t.contains("[1]")), "cue 1 re-sent");
    assert!(
        !texts.iter().any(|t| t.contains("[2]")),
        "locked cue 2 is never re-sent"
    );

    // cue 1 now holds the fresh target; cue 2 keeps its locked reviewer text.
    let id1 = store
        .call(|c| Ok(cue_id_by_idx(c)?.get(&1).copied()))
        .await
        .expect("map")
        .expect("cue 1");
    let row1 = store
        .call(move |c| get_translation(c, id1))
        .await
        .expect("get")
        .expect("cue 1 translated");
    assert_eq!(row1.text, mm2, "unlocked cue re-translated");
    let row2 = store
        .call(move |c| get_translation(c, id2))
        .await
        .expect("get")
        .expect("cue 2 present");
    assert_eq!(row2.text, "ကိုယ်တိုင်ပြင်", "locked cue text preserved");
    assert!(row2.locked, "cue 2 stays locked");
}

// AC: M6-B1 — an uneven final batch: 3 translatable cues with batch_size 2 plans
// batches [2,1] (cues [1,2] then [3]).
#[tokio::test]
async fn uneven_final_batch_is_planned() {
    let cues = vec![
        cue_full(1, "Good morning.", None, true),
        cue_full(2, "Where are you going?", None, true),
        cue_full(3, "To the market.", None, true),
    ];
    let store = seed(&cues).await;
    let token = CancellationToken::new();
    let fake = Fake::new().always(|| Ok(lines_response(&[(1, MM), (2, MM), (3, MM)])));

    let outcome = run_translation(
        fake.clone(),
        &store,
        &cues,
        &params(2),
        TranslationFlags::default(),
        &token,
    )
    .await
    .expect("run");

    assert_eq!(outcome, TranslationOutcome::Completed { batches: 2 });

    let b1 = store
        .call(|c| get_translation_batch(c, 1))
        .await
        .expect("get")
        .expect("batch 1");
    assert_eq!(
        (b1.first_cue, b1.last_cue),
        (1, 2),
        "first batch has 2 cues"
    );
    let b2 = store
        .call(|c| get_translation_batch(c, 2))
        .await
        .expect("get")
        .expect("batch 2");
    assert_eq!(
        (b2.first_cue, b2.last_cue),
        (3, 3),
        "uneven final batch has 1 cue"
    );

    let translations = store.call(|c| list_translations(c)).await.expect("list");
    assert_eq!(
        translations.len(),
        3,
        "one translation per translatable cue"
    );
}

// ---------------------------------------------------------------------------
// Dry run: writes nothing, reports an estimate
// ---------------------------------------------------------------------------

fn priced_table() -> PriceTable {
    let mut t = PriceTable::new();
    t.insert(
        MODEL,
        ModelPrice {
            input: 1.0,
            cached_input: 0.5,
            output: 2.0,
            storage_per_hour: 0.0,
            flex_multiplier: 0.5,
            long_prompt_threshold: None,
            long_input: None,
            long_output: None,
        },
    );
    t
}

async fn table_counts(store: &Store) -> (usize, usize, usize, usize) {
    store
        .call(|c| {
            let count = |sql: &str| -> Result<usize, myansub::store::StoreError> {
                let n: i64 = c.query_row(sql, [], |r| r.get(0))?;
                Ok(n.max(0) as usize)
            };
            Ok((
                count("SELECT COUNT(*) FROM translations")?,
                count("SELECT COUNT(*) FROM translation_batches")?,
                count("SELECT COUNT(*) FROM llm_calls")?,
                count("SELECT COUNT(*) FROM llm_cache")?,
            ))
        })
        .await
        .expect("counts")
}

// AC: M6-B1 — a dry run reports a per-batch estimate and leaves the database
// unchanged (no translations, no batch rows, no llm_calls, no cache rows).
#[tokio::test]
async fn dry_run_reports_estimate_and_writes_nothing() {
    let cues = mixed_cues();
    let store = seed(&cues).await;
    let before = table_counts(&store).await;

    let report = dry_run_translation(&store, &cues, &params(2), &priced_table())
        .await
        .expect("dry run");

    assert_eq!(report.batches, 2);
    assert_eq!(report.calls, 2);
    assert!(
        report.estimated_prompt_tokens > 0,
        "prompt tokens estimated"
    );
    match report.estimated_cost {
        Cost::Known(usd) => assert!(usd > 0.0, "priced model yields a positive cost"),
        Cost::Unknown => panic!("priced model should be known"),
    }

    let after = table_counts(&store).await;
    assert_eq!(before, after, "dry run must not change the database");
    assert_eq!(after.0, 0, "no translations written");
    assert_eq!(after.1, 0, "no batch rows written");
    assert_eq!(after.2, 0, "no llm_calls written");
    assert_eq!(after.3, 0, "no cache rows written");
}

// ---------------------------------------------------------------------------
// M6-AC3: partial id set → only the MISSING ids are re-requested (tier 2)
// ---------------------------------------------------------------------------

// AC: M6-AC3 — a first response omits one of the batch's cue ids; the pipeline keeps
// the valid line it received and re-requests ONLY the missing id as a new, smaller
// request (not the whole batch). The merged batch then completes `done` with every cue
// translated. The key proof is the Fake call log: the retry request contains only the
// missing id.
#[tokio::test]
async fn partial_ids_re_request_only_missing_ids() {
    // Two translatable cues in one batch.
    let cues = vec![
        cue_full(1, "Good morning.", None, true),
        cue_full(2, "Where are you going?", None, true),
    ];
    let store = seed(&cues).await;
    let token = CancellationToken::new();
    // First call returns only id 1 (id 2 missing); the second call returns id 2.
    let fake = Fake::new()
        .push_response(lines_response(&[(1, MM)]))
        .push_response(lines_response(&[(2, MM)]))
        .always(|| Ok(lines_response(&[(1, MM), (2, MM)])));

    let outcome = run_translation(
        fake.clone(),
        &store,
        &cues,
        &params(2),
        TranslationFlags::default(),
        &token,
    )
    .await
    .expect("run");

    assert_eq!(outcome, TranslationOutcome::Completed { batches: 1 });
    assert_eq!(
        fake.call_count(),
        2,
        "original call + one partial re-request of the missing id"
    );

    // The KEY assertion: the second (retry) request carries ONLY the missing id 2,
    // never the whole batch (no [1]).
    let texts = call_texts(&fake);
    assert!(
        texts[1].contains("[2]"),
        "the re-request translates the missing cue 2"
    );
    assert!(
        !texts[1].contains("[1]"),
        "the re-request does not re-send the already-received cue 1"
    );

    // Both cues end translated and the batch is done.
    let translations = store.call(|c| list_translations(c)).await.expect("list");
    assert_eq!(
        translations.len(),
        2,
        "every cue translated after the merge"
    );
    let done = store
        .call(|c| count_translation_batches_by_status(c, UnitStatus::Done))
        .await
        .expect("count");
    assert_eq!(done, 1, "the merged batch is done");
}

// ---------------------------------------------------------------------------
// M6-AC5: a failed batch does not block other batches; the run exits 4
// ---------------------------------------------------------------------------

// AC: M6-AC5 — in a multi-batch run, one batch fails irrecoverably (a persistent
// coverage miss) while the others succeed. The failure does NOT prevent the others
// from completing: they end `done`, the failing batch ends `failed` with a recorded
// error, the outcome is CompletedWithFailures and the exit code is 4.
#[tokio::test]
async fn one_failed_batch_does_not_block_others_exit_4() {
    // Three cues, batch_size 1 → batches [1], [2], [3].
    let cues = vec![
        cue_full(1, "Good morning.", None, true),
        cue_full(2, "Where are you going?", None, true),
        cue_full(3, "To the market.", None, true),
    ];
    let store = seed(&cues).await;
    let token = CancellationToken::new();
    // Every call returns ids 1 and 3 only: batches 1 and 3 are covered, batch 2 never
    // is, so it fails after its retry — independent of dispatch order.
    let fake = Fake::new().always(|| Ok(lines_response(&[(1, MM), (3, MM)])));

    let outcome = run_translation(
        fake.clone(),
        &store,
        &cues,
        &params(1),
        TranslationFlags::default(),
        &token,
    )
    .await
    .expect("run");

    assert_eq!(
        outcome,
        TranslationOutcome::CompletedWithFailures {
            failed: 1,
            incomplete: 0,
        }
    );
    assert_eq!(translation_exit_code(false, &outcome), 4, "exit 4");

    let done = store
        .call(|c| count_translation_batches_by_status(c, UnitStatus::Done))
        .await
        .expect("count");
    let failed = store
        .call(|c| count_translation_batches_by_status(c, UnitStatus::Failed))
        .await
        .expect("count");
    assert_eq!(
        done, 2,
        "the other two batches completed despite the failure"
    );
    assert_eq!(failed, 1, "exactly the uncoverable batch failed");

    // The failing batch (cue 2) recorded an error so `status`/the report can list it.
    let b2 = store
        .call(|c| get_translation_batch(c, 2))
        .await
        .expect("get")
        .expect("batch 2");
    assert_eq!(b2.status, UnitStatus::Failed);
    assert!(b2.last_error.is_some(), "failure recorded for the report");

    // The non-blocked batches' cues are translated; the failed batch's is not.
    let id2 = store
        .call(|c| Ok(cue_id_by_idx(c)?.get(&2).copied()))
        .await
        .expect("map")
        .expect("cue 2");
    assert!(
        store
            .call(move |c| get_translation(c, id2))
            .await
            .expect("get")
            .is_none(),
        "the failed batch committed nothing"
    );
    let translations = store.call(|c| list_translations(c)).await.expect("list");
    assert_eq!(translations.len(), 2, "cues 1 and 3 translated");
}

// ---------------------------------------------------------------------------
// Tier 3: per-line hard-flag retries; soft flags are not retried
// ---------------------------------------------------------------------------

// AC: M6-B2 — a cue returned with a hard flag (placeholder_mismatch) drives one
// isolated per-line retry; when the retry fixes the flag the clean text is stored.
#[tokio::test]
async fn tier3_retry_fixes_hard_flag_and_stores_clean() {
    let cues = vec![cue_full(1, "Hello ⟦0⟧", None, true)];
    let store = seed(&cues).await;
    let token = CancellationToken::new();
    let fixed = "⟦0⟧ မင်္ဂလာပါ"; // preserves the placeholder → no mismatch
    // First response drops ⟦0⟧; the isolated retry restores it.
    let fake = Fake::new()
        .push_response(lines_response(&[(1, MM)]))
        .push_response(lines_response(&[(1, fixed)]))
        .always(move || Ok(lines_response(&[(1, fixed)])));

    let outcome = run_translation(
        fake.clone(),
        &store,
        &cues,
        &params(80),
        TranslationFlags::default(),
        &token,
    )
    .await
    .expect("run");

    assert_eq!(outcome, TranslationOutcome::Completed { batches: 1 });
    assert_eq!(fake.call_count(), 2, "one isolated tier-3 retry");

    // The retry re-requests just cue 1.
    let texts = call_texts(&fake);
    assert!(
        texts[1].contains("[1]"),
        "the retry re-requests the flagged line"
    );

    let id1 = store
        .call(|c| Ok(cue_id_by_idx(c)?.get(&1).copied()))
        .await
        .expect("map")
        .expect("cue 1");
    let row = store
        .call(move |c| get_translation(c, id1))
        .await
        .expect("get")
        .expect("translated");
    assert_eq!(row.text, fixed, "the fixed target replaced the flagged one");
    assert!(
        !row.flags.contains(&QualityFlag::PlaceholderMismatch),
        "placeholder_mismatch cleared, got {:?}",
        row.flags
    );
}

// AC: M6-B2 — a cue carrying only a soft flag (too_long) is stored as-is and is NOT
// retried: the batch sends exactly one call.
#[tokio::test]
async fn soft_flag_only_cue_is_not_retried() {
    let cues = vec![cue_full(1, "Hello.", None, true)];
    let store = seed(&cues).await;
    let token = CancellationToken::new();
    // Three Burmese lines (> max_lines = 2) → too_long (soft); has Myanmar, no
    // placeholder → no hard flag.
    let long = "မ\nမ\nမ";
    let fake = Fake::new().always(move || Ok(lines_response(&[(1, long)])));

    let outcome = run_translation(
        fake.clone(),
        &store,
        &cues,
        &params(80),
        TranslationFlags::default(),
        &token,
    )
    .await
    .expect("run");

    assert_eq!(outcome, TranslationOutcome::Completed { batches: 1 });
    assert_eq!(fake.call_count(), 1, "a soft flag triggers no retry");

    let id1 = store
        .call(|c| Ok(cue_id_by_idx(c)?.get(&1).copied()))
        .await
        .expect("map")
        .expect("cue 1");
    let row = store
        .call(move |c| get_translation(c, id1))
        .await
        .expect("get")
        .expect("translated");
    assert!(row.flags.contains(&QualityFlag::TooLong), "too_long stored");
    assert!(
        !row.flags.iter().any(|f| matches!(
            f,
            QualityFlag::PlaceholderMismatch
                | QualityFlag::Untranslated
                | QualityFlag::ZawgyiSuspect
        )),
        "no hard flag present, got {:?}",
        row.flags
    );
}

// ---------------------------------------------------------------------------
// --retry-failed: requeue failed batches and rerun; done batches are not resent
// ---------------------------------------------------------------------------

// AC: M6-B2 — after a run leaves one batch failed, a `--retry-failed` rerun with a
// valid script reruns only that batch (now succeeding, exit 0); the already-done
// batches are not resent.
#[tokio::test]
async fn retry_failed_reruns_only_the_failed_batch() {
    let cues = vec![
        cue_full(1, "Good morning.", None, true),
        cue_full(2, "Where are you going?", None, true),
        cue_full(3, "To the market.", None, true),
    ];
    let store = seed(&cues).await;
    let token = CancellationToken::new();

    // Run 1: batch 2 never gets covered → fails; 1 and 3 succeed.
    let fake1 = Fake::new().always(|| Ok(lines_response(&[(1, MM), (3, MM)])));
    let outcome = run_translation(
        fake1.clone(),
        &store,
        &cues,
        &params(1),
        TranslationFlags::default(),
        &token,
    )
    .await
    .expect("run 1");
    assert_eq!(
        outcome,
        TranslationOutcome::CompletedWithFailures {
            failed: 1,
            incomplete: 0,
        }
    );

    // Run 2 --retry-failed: a valid script; only batch 2 is requeued and rerun.
    let fake2 = Fake::new().always(|| Ok(lines_response(&[(1, MM), (2, MM), (3, MM)])));
    let outcome = run_translation(
        fake2.clone(),
        &store,
        &cues,
        &params(1),
        TranslationFlags {
            retry_failed: true,
            ..TranslationFlags::default()
        },
        &token,
    )
    .await
    .expect("run 2");

    assert_eq!(
        outcome,
        TranslationOutcome::Completed { batches: 3 },
        "exit 0"
    );
    assert_eq!(translation_exit_code(false, &outcome), 0);
    assert_eq!(
        fake2.call_count(),
        1,
        "only the previously-failed batch is resent"
    );

    // The resent request is the failed batch's cue 2 only.
    let texts = call_texts(&fake2);
    assert!(texts.iter().any(|t| t.contains("[2]")), "batch 2 rerun");
    assert!(
        !texts.iter().any(|t| t.contains("[1]") || t.contains("[3]")),
        "done batches 1 and 3 are not resent"
    );

    let done = store
        .call(|c| count_translation_batches_by_status(c, UnitStatus::Done))
        .await
        .expect("count");
    assert_eq!(done, 3, "all batches done after retry-failed");
}

// ---------------------------------------------------------------------------
// --stale: rerun only batches overlapping a stale, unlocked cue
// ---------------------------------------------------------------------------

/// Deletes every local response-cache row (so a re-run actually re-calls the fake).
async fn clear_llm_cache(store: &Store) {
    store
        .call(|c| {
            c.execute("DELETE FROM llm_cache", [])?;
            Ok(())
        })
        .await
        .expect("clear cache");
}

// AC: M6-B2 — with `--stale`, only the batch overlapping a stale, unlocked cue is
// re-run; a non-stale batch is untouched, and a stale+locked cue is never re-run.
#[tokio::test]
async fn stale_reruns_only_stale_unlocked_batch() {
    let cues = vec![
        cue_full(1, "Good morning.", None, true),
        cue_full(2, "Where are you going?", None, true),
        cue_full(3, "To the market.", None, true),
    ];
    let store = seed(&cues).await;
    let token = CancellationToken::new();

    // Run 1: everything translated, batch_size 1 → batches [1], [2], [3].
    let fake1 = Fake::new().always(|| Ok(lines_response(&[(1, MM), (2, MM), (3, MM)])));
    run_translation(
        fake1.clone(),
        &store,
        &cues,
        &params(1),
        TranslationFlags::default(),
        &token,
    )
    .await
    .expect("run 1");

    // Mark cue 1 stale+unlocked; cue 3 stale+locked (must NOT re-run); cue 2 clean.
    let ids = store
        .call(|c| {
            let m = cue_id_by_idx(c)?;
            Ok((m[&1], m[&3]))
        })
        .await
        .expect("ids");
    store
        .call(move |c| {
            upsert_translation(
                c,
                &NewTranslation {
                    cue_id: ids.0,
                    text: MM.to_string(),
                    origin: TranslationOrigin::Llm,
                    locked: false,
                    stale: true,
                    flags: vec![],
                    analysis_version: 1,
                    model: None,
                },
            )?;
            upsert_translation(
                c,
                &NewTranslation {
                    cue_id: ids.1,
                    text: MM.to_string(),
                    origin: TranslationOrigin::User,
                    locked: true,
                    stale: true,
                    flags: vec![],
                    analysis_version: 1,
                    model: None,
                },
            )?;
            Ok(())
        })
        .await
        .expect("set stale/locked");

    // Clear the local cache so a genuine re-run calls the fake (an identical request
    // would otherwise be a free cache hit).
    clear_llm_cache(&store).await;

    // Run 2 --stale: a fresh target proves cue 1 was re-translated.
    let mm2 = "နေကောင်းလား";
    let fake2 = Fake::new().always(move || Ok(lines_response(&[(1, mm2), (2, mm2), (3, mm2)])));
    let outcome = run_translation(
        fake2.clone(),
        &store,
        &cues,
        &params(1),
        TranslationFlags {
            stale: true,
            ..TranslationFlags::default()
        },
        &token,
    )
    .await
    .expect("run 2");

    assert_eq!(outcome, TranslationOutcome::Completed { batches: 3 });
    assert_eq!(
        fake2.call_count(),
        1,
        "only the stale, unlocked batch is re-run"
    );
    let texts = call_texts(&fake2);
    assert!(
        texts.iter().any(|t| t.contains("[1]")),
        "stale cue 1 re-run"
    );
    assert!(
        !texts.iter().any(|t| t.contains("[2]") || t.contains("[3]")),
        "non-stale cue 2 and stale+locked cue 3 are not re-run"
    );

    // cue 1 re-translated; cue 3 (stale+locked) preserved.
    let row1 = store
        .call(move |c| get_translation(c, ids.0))
        .await
        .expect("get")
        .expect("cue 1");
    assert_eq!(row1.text, mm2, "stale unlocked cue re-translated");
    let row3 = store
        .call(move |c| get_translation(c, ids.1))
        .await
        .expect("get")
        .expect("cue 3");
    assert_eq!(row3.text, MM, "stale+locked cue preserved");
}

// AC: M6-B2 — `--stale` re-requests ONLY the stale, unlocked cues in an affected
// batch: a mixed batch (batch_size ≥ 2) holding one stale-unlocked cue and one
// non-stale unlocked cue re-sends only the stale cue's id, and the non-stale cue's
// prior translation is preserved (not re-sent, not re-billed, not overwritten).
#[tokio::test]
async fn stale_re_requests_only_stale_cues_in_a_mixed_batch() {
    // One batch of two cues (batch_size 2): cue 1 will be stale, cue 2 stays clean.
    let cues = vec![
        cue_full(1, "Good morning.", None, true),
        cue_full(2, "Where are you going?", None, true),
    ];
    let store = seed(&cues).await;
    let token = CancellationToken::new();

    // Run 1: both cues translated to MM in one batch.
    let fake1 = Fake::new().always(|| Ok(lines_response(&[(1, MM), (2, MM)])));
    run_translation(
        fake1.clone(),
        &store,
        &cues,
        &params(2),
        TranslationFlags::default(),
        &token,
    )
    .await
    .expect("run 1");
    assert_eq!(fake1.call_count(), 1, "one batch, one call");

    // Mark cue 1 stale+unlocked; cue 2 stays clean (not stale).
    let ids = store
        .call(|c| {
            let m = cue_id_by_idx(c)?;
            Ok((m[&1], m[&2]))
        })
        .await
        .expect("ids");
    store
        .call(move |c| {
            upsert_translation(
                c,
                &NewTranslation {
                    cue_id: ids.0,
                    text: MM.to_string(),
                    origin: TranslationOrigin::Llm,
                    locked: false,
                    stale: true,
                    flags: vec![],
                    analysis_version: 1,
                    model: None,
                },
            )
            .map(|_| ())
        })
        .await
        .expect("mark cue 1 stale");

    clear_llm_cache(&store).await;

    // Run 2 --stale: a fresh target; only cue 1 (stale) should be re-requested.
    let mm2 = "နေကောင်းလား";
    let fake2 = Fake::new().always(move || Ok(lines_response(&[(1, mm2), (2, mm2)])));
    let outcome = run_translation(
        fake2.clone(),
        &store,
        &cues,
        &params(2),
        TranslationFlags {
            stale: true,
            ..TranslationFlags::default()
        },
        &token,
    )
    .await
    .expect("run 2");

    assert_eq!(outcome, TranslationOutcome::Completed { batches: 1 });
    assert_eq!(fake2.call_count(), 1, "the stale batch is re-run once");

    // The re-request carries ONLY the stale cue id, never the non-stale cue 2.
    let texts = call_texts(&fake2);
    assert!(
        texts.iter().any(|t| t.contains("[1]")),
        "stale cue 1 re-sent"
    );
    assert!(
        !texts.iter().any(|t| t.contains("[2]")),
        "non-stale cue 2 is never re-sent under --stale"
    );

    // cue 1 re-translated; cue 2's prior translation is unchanged.
    let row1 = store
        .call(move |c| get_translation(c, ids.0))
        .await
        .expect("get")
        .expect("cue 1");
    assert_eq!(row1.text, mm2, "stale cue re-translated");
    let row2 = store
        .call(move |c| get_translation(c, ids.1))
        .await
        .expect("get")
        .expect("cue 2");
    assert_eq!(row2.text, MM, "non-stale cue preserved unchanged");
}

// AC: M6-B2 — `--stale --retry-failed` runs both selectors independently: a project
// with a stale-unlocked cue in one batch AND a separately-failed batch re-runs BOTH
// (retry-failed is not a subset of stale/redo, so a failed batch — which has no
// translation rows and so never enters the stale set — is still requeued).
#[tokio::test]
async fn stale_and_retry_failed_rerun_both_batches() {
    // Three cues, batch_size 1 → batches [1], [2], [3].
    let cues = vec![
        cue_full(1, "Good morning.", None, true),
        cue_full(2, "Where are you going?", None, true),
        cue_full(3, "To the market.", None, true),
    ];
    let store = seed(&cues).await;
    let token = CancellationToken::new();

    // Run 1: batch 2 never covered → fails; batches 1 and 3 succeed.
    let fake1 = Fake::new().always(|| Ok(lines_response(&[(1, MM), (3, MM)])));
    let outcome = run_translation(
        fake1.clone(),
        &store,
        &cues,
        &params(1),
        TranslationFlags::default(),
        &token,
    )
    .await
    .expect("run 1");
    assert_eq!(
        outcome,
        TranslationOutcome::CompletedWithFailures {
            failed: 1,
            incomplete: 0,
        }
    );

    // Mark cue 1 stale+unlocked (batch 1); batch 2 is still failed from run 1.
    let id1 = store
        .call(|c| Ok(cue_id_by_idx(c)?[&1]))
        .await
        .expect("id1");
    store
        .call(move |c| {
            upsert_translation(
                c,
                &NewTranslation {
                    cue_id: id1,
                    text: MM.to_string(),
                    origin: TranslationOrigin::Llm,
                    locked: false,
                    stale: true,
                    flags: vec![],
                    analysis_version: 1,
                    model: None,
                },
            )
            .map(|_| ())
        })
        .await
        .expect("mark cue 1 stale");

    clear_llm_cache(&store).await;

    // Run 2 --stale --retry-failed: batch 1 (stale) AND batch 2 (failed) both re-run.
    let mm2 = "နေကောင်းလား";
    let fake2 = Fake::new().always(move || Ok(lines_response(&[(1, mm2), (2, mm2), (3, mm2)])));
    let outcome = run_translation(
        fake2.clone(),
        &store,
        &cues,
        &params(1),
        TranslationFlags {
            stale: true,
            retry_failed: true,
            ..TranslationFlags::default()
        },
        &token,
    )
    .await
    .expect("run 2");

    assert_eq!(
        outcome,
        TranslationOutcome::Completed { batches: 3 },
        "exit 0"
    );
    assert_eq!(translation_exit_code(false, &outcome), 0);
    assert_eq!(
        fake2.call_count(),
        2,
        "both the stale batch and the failed batch re-run"
    );
    let texts = call_texts(&fake2);
    assert!(
        texts.iter().any(|t| t.contains("[1]")),
        "stale batch 1 re-run"
    );
    assert!(
        texts.iter().any(|t| t.contains("[2]")),
        "failed batch 2 re-run"
    );
    assert!(
        !texts.iter().any(|t| t.contains("[3]")),
        "clean, non-stale batch 3 is not re-run"
    );

    let done = store
        .call(|c| count_translation_batches_by_status(c, UnitStatus::Done))
        .await
        .expect("count");
    assert_eq!(done, 3, "all batches done after stale + retry-failed");
}

// AC: M6-B2 — an interruption DURING a multi-call (tier-2) batch leaves the batch
// in_flight with nothing committed; a Store reopen recovers it to pending. Mirrors
// the analysis in-flight-recovery template but for the multi-call content path.
#[tokio::test]
async fn interrupt_during_multi_call_batch_commits_nothing_and_recovers() {
    let tmp = tempdir().expect("tempdir");
    let db = tmp.path().join("project.db");
    // One batch of two cues (batch_size 2) forces a tier-2 second sub-request.
    let cues = vec![
        cue_full(1, "Good morning.", None, true),
        cue_full(2, "Where are you going?", None, true),
    ];
    {
        let store = Store::open(&db).expect("open seed");
        let c = cues.clone();
        store
            .call(move |conn| insert_cues(conn, &c))
            .await
            .expect("cues");
        store
            .call(|conn| {
                insert_analysis_version(
                    conn,
                    ANALYSIS,
                    AnalysisOrigin::Llm,
                    "sha256:proj",
                    "prompt_v1|m",
                )
                .map(|_| ())
            })
            .await
            .expect("analysis");
        store
            .call(|conn| approve_current_analysis(conn).map(|_| ()))
            .await
            .expect("approve");
    }

    let store = Store::open(&db).expect("open run");
    let token = CancellationToken::new();
    // The first call returns only id 1 (id 2 missing) AND cancels the token, so the
    // tier-2 re-request for the missing id 2 is short-circuited by cancellation before
    // a second call is ever sent.
    let fake = Fake::new()
        .push_response(lines_response(&[(1, MM)]))
        .cancel_after(token.clone(), 1);

    let outcome = run_translation(
        fake.clone(),
        &store,
        &cues,
        &params(2),
        TranslationFlags::default(),
        &token,
    )
    .await
    .expect("run");
    assert_eq!(outcome, TranslationOutcome::Interrupted);
    assert_eq!(translation_exit_code(true, &outcome), 130);
    assert_eq!(
        fake.call_count(),
        1,
        "the second sub-request is never sent after cancellation"
    );

    // Nothing committed: no translations, and the batch is not done.
    let translations = store.call(|c| list_translations(c)).await.expect("list");
    assert!(
        translations.is_empty(),
        "no partial translations committed, got {translations:?}"
    );
    let done = store
        .call(|c| count_translation_batches_by_status(c, UnitStatus::Done))
        .await
        .expect("count");
    assert_eq!(done, 0, "the interrupted batch is not marked done");

    // The batch was left in_flight; a Store reopen recovers it to pending.
    drop(store);
    let store = Store::open(&db).expect("reopen");
    let pending = store
        .call(|c| count_translation_batches_by_status(c, UnitStatus::Pending))
        .await
        .expect("count");
    assert_eq!(pending, 1, "the interrupted batch recovered to pending");
    let in_flight = store
        .call(|c| count_translation_batches_by_status(c, UnitStatus::InFlight))
        .await
        .expect("count");
    assert_eq!(in_flight, 0, "no batch left in_flight after recovery");
}
