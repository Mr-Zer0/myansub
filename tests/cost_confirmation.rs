//! M8-C cost-confirmation gate (`docs/CLI.md` §5, `docs/LLM.md` §7/§9) and
//! `cache prune` (`docs/CLI.md` §6, `docs/LLM.md` §4).
//!
//! Offline, in-memory stores and the scripted `Fake` transport (`docs/TESTING.md`
//! §1/§3). The pure [`confirm_decision`] is unit-tested here through the public
//! surface; the wiring tests prove that a refusal spends nothing (the `Fake` is never
//! called) and names `--yes`, while `--yes` / a below-threshold estimate proceeds.
//! Real stdin is never read — the interactive `Prompt` branch is driven only by the
//! pure decision, exactly as `decide_reinit` is tested.

use std::collections::BTreeSet;
use std::time::Duration;

use myansub::domain::{Cue, DigitStyle};
use myansub::error::ExitCode;
use myansub::error::exit;
use myansub::llm::client::{CommitToken, LlmResponse, ServiceTier, ThinkingSetting, Usage};
use myansub::llm::dry_run::Estimator;
use myansub::llm::fake::Fake;
use myansub::llm::pricing::{Cost, ModelPrice, PriceTable};
use myansub::llm::retry::RetryConfig;
use myansub::pipeline::PipelineError;
use myansub::pipeline::analysis::{AnalysisFlags, AnalysisOutcome, AnalysisParams, run_analysis};
use myansub::pipeline::confirm::{ConfirmAction, ConfirmSettings, ConfirmedRun, confirm_decision};
use myansub::pipeline::scheduler::SchedulerConfig;
use myansub::pipeline::translation::{
    TranslationFlags, TranslationOutcome, TranslationParams, run_translation,
};
use myansub::pipeline::{analysis, approve_and_invalidate, translation};
use myansub::quality::checks::Limits;
use myansub::store::repo_analysis::{
    AnalysisOrigin, approve_current_analysis, insert_analysis_version,
};
use myansub::store::repo_cues::insert_cues;
use myansub::store::{Store, repo_llm};
use tokio_util::sync::CancellationToken;

const MODEL: &str = "gemini-flash-latest";
const MM: &str = "မင်္ဂလာပါ"; // Burmese, so the `untranslated` check never fires.

// ---------------------------------------------------------------------------
// Pure decision (`confirm_decision`)
// ---------------------------------------------------------------------------

// AC: CLI.md §5 — `--yes` always proceeds, even above the threshold.
#[test]
fn yes_always_proceeds() {
    assert_eq!(
        confirm_decision(Cost::Known(100.0), 1.0, false, true),
        ConfirmAction::Proceed
    );
    assert_eq!(
        confirm_decision(Cost::Unknown, 0.0, false, true),
        ConfirmAction::Proceed
    );
}

// AC: CLI.md §5 — an estimate above the threshold on a TTY prompts.
#[test]
fn above_threshold_interactive_prompts() {
    assert_eq!(
        confirm_decision(Cost::Known(2.0), 1.0, true, false),
        ConfirmAction::Prompt
    );
}

// AC: CLI.md §5 — an estimate above the threshold with no TTY and no `--yes` refuses,
// naming `--yes` (never blocks on input).
#[test]
fn above_threshold_non_interactive_refuses() {
    assert_eq!(
        confirm_decision(Cost::Known(2.0), 1.0, false, false),
        ConfirmAction::RefuseNeedsYes
    );
}

// AC: CLI.md §5 — an estimate at or below the threshold proceeds without prompting.
#[test]
fn at_or_below_threshold_proceeds() {
    assert_eq!(
        confirm_decision(Cost::Known(0.5), 1.0, false, false),
        ConfirmAction::Proceed
    );
    // Exactly equal does not "exceed" the threshold.
    assert_eq!(
        confirm_decision(Cost::Known(1.0), 1.0, true, false),
        ConfirmAction::Proceed
    );
}

// AC: CLI.md §5 / CONFIG.md — a threshold of 0 always asks.
#[test]
fn zero_threshold_always_asks() {
    assert_eq!(
        confirm_decision(Cost::Known(0.0), 0.0, true, false),
        ConfirmAction::Prompt
    );
    assert_eq!(
        confirm_decision(Cost::Known(0.0), 0.0, false, false),
        ConfirmAction::RefuseNeedsYes
    );
}

// AC: LLM.md §9 — an unknown (unpriced) estimate is treated as needing confirmation
// (the safe choice): never spend an unknown amount silently.
#[test]
fn unknown_cost_needs_confirmation() {
    assert_eq!(
        confirm_decision(Cost::Unknown, 1000.0, true, false),
        ConfirmAction::Prompt
    );
    assert_eq!(
        confirm_decision(Cost::Unknown, 1000.0, false, false),
        ConfirmAction::RefuseNeedsYes
    );
}

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// A full, schema-valid analysis document whose single scene covers cues 1..=3.
const ANALYSIS: &str = r#"{"schema_version":1,"premise":"p","tone":"t",
"characters":[],"address_matrix":[],"glossary":[],
"scenes":[{"cue_range":[1,3],"summary":"s","speakers":[]}],
"style_rules":[],"unresolved_ambiguities":[]}"#;

fn cue(idx: u32, text: &str) -> Cue {
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
        translatable: true,
    }
}

fn cues() -> Vec<Cue> {
    vec![
        cue(1, "Good morning."),
        cue(2, "How are you?"),
        cue(3, "Goodbye."),
    ]
}

fn analysis_response() -> LlmResponse {
    response(ANALYSIS)
}

fn response(text: &str) -> LlmResponse {
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
    SchedulerConfig {
        concurrency: 1,
        warm_cache_first: false,
        grace: Duration::from_secs(5),
    }
}

fn analysis_params() -> AnalysisParams {
    AnalysisParams {
        model: MODEL.to_string(),
        tier: ServiceTier::Flex,
        thinking: ThinkingSetting::Low,
        source_digest: "sha256:proj".to_string(),
        max_content_attempts: 3,
        single_pass_max_tokens: 1_000_000,
        chunk_cues: 400,
        estimator: estimator(),
        output_ratio: 4.0,
        scheduler: scheduler(),
        seed: 42,
        retry: retry(),
    }
}

fn translation_params() -> TranslationParams {
    TranslationParams {
        model: MODEL.to_string(),
        tier: ServiceTier::Flex,
        thinking: ThinkingSetting::Off,
        batch_size: 10,
        context_before: 0,
        context_after: 0,
        max_content_attempts: 3,
        output_ratio: 4.0,
        digits: DigitStyle::Myanmar,
        estimator: estimator(),
        limits: Limits {
            max_graphemes_per_line: 42,
            max_lines: 2,
            max_gps: 20.0,
            drift_ratio: 3.0,
        },
        scheduler: scheduler(),
        seed: 42,
        retry: retry(),
    }
}

/// A price table that prices [`MODEL`] cheaply, so an estimate is [`Cost::Known`].
fn priced() -> PriceTable {
    let mut table = PriceTable::new();
    table.insert(
        MODEL,
        ModelPrice {
            input: 0.1,
            cached_input: 0.025,
            output: 0.4,
            storage_per_hour: 1.0,
            flex_multiplier: 0.5,
            long_prompt_threshold: None,
            long_input: None,
            long_output: None,
        },
    );
    table
}

fn settings(confirm_above_usd: f64, interactive: bool, yes: bool) -> ConfirmSettings {
    ConfirmSettings {
        confirm_above_usd,
        interactive,
        yes,
    }
}

fn batch2_params() -> TranslationParams {
    TranslationParams {
        batch_size: 2,
        ..translation_params()
    }
}

fn known(cost: Cost) -> f64 {
    match cost {
        Cost::Known(v) => v,
        Cost::Unknown => panic!("expected a known cost"),
    }
}

/// A store with cues 1..=3 and an approved analysis version 1 (so translation can run).
async fn translation_store() -> Store {
    let store = Store::open_in_memory().expect("open");
    let cues = cues();
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

// ---------------------------------------------------------------------------
// analyze wiring
// ---------------------------------------------------------------------------

// AC: CLI.md §5 — `analyze` above the threshold with no TTY and no `--yes` exits 3
// (naming `--yes`) and spends nothing: the transport factory is never invoked.
#[tokio::test]
async fn analyze_refuses_without_tty_or_yes() {
    let store = Store::open_in_memory().expect("open");
    let fake = Fake::new().push_response(analysis_response());
    let token = CancellationToken::new();

    let err = analysis::run_analysis_confirmed(
        || Ok::<_, PipelineError>(fake.clone()),
        &store,
        &cues(),
        &analysis_params(),
        AnalysisFlags::default(),
        &token,
        &PriceTable::new(),
        settings(0.0, false, false),
        std::sync::Arc::new(myansub::pipeline::progress::NoopProgress),
    )
    .await
    .expect_err("refusal");

    // CLI.md §5: the refusal must name the flag the user needs.
    match &err {
        PipelineError::ConfirmationNeeded { flag } => assert_eq!(flag, "--yes"),
        other => panic!("expected ConfirmationNeeded, got {other:?}"),
    }
    assert_eq!(err.exit_code(), exit::PRECONDITION);
    assert_eq!(fake.call_count(), 0, "a refusal spends nothing");
}

// AC: CLI.md §5 — `analyze --yes` clears the gate and runs (the transport is called).
#[tokio::test]
async fn analyze_with_yes_proceeds() {
    let store = Store::open_in_memory().expect("open");
    let fake = Fake::new().push_response(analysis_response());
    let token = CancellationToken::new();

    let confirmed = analysis::run_analysis_confirmed(
        || Ok::<_, PipelineError>(fake.clone()),
        &store,
        &cues(),
        &analysis_params(),
        AnalysisFlags::default(),
        &token,
        &PriceTable::new(),
        settings(0.0, false, true),
        std::sync::Arc::new(myansub::pipeline::progress::NoopProgress),
    )
    .await
    .expect("run");

    assert_eq!(
        confirmed,
        ConfirmedRun::Ran(AnalysisOutcome::Completed { version: 1 })
    );
    assert_eq!(fake.call_count(), 1, "the analysis call was sent");
}

// AC: CLI.md §5 — a priced estimate at or below the threshold proceeds without a
// prompt or `--yes`.
#[tokio::test]
async fn analyze_below_threshold_proceeds_silently() {
    let store = Store::open_in_memory().expect("open");
    let fake = Fake::new().push_response(analysis_response());
    let token = CancellationToken::new();

    let confirmed = analysis::run_analysis_confirmed(
        || Ok::<_, PipelineError>(fake.clone()),
        &store,
        &cues(),
        &analysis_params(),
        AnalysisFlags::default(),
        &token,
        &priced(),
        settings(1000.0, false, false),
        std::sync::Arc::new(myansub::pipeline::progress::NoopProgress),
    )
    .await
    .expect("run");

    assert!(matches!(
        confirmed,
        ConfirmedRun::Ran(AnalysisOutcome::Completed { .. })
    ));
    assert_eq!(fake.call_count(), 1);
}

// ---------------------------------------------------------------------------
// translate wiring
// ---------------------------------------------------------------------------

// AC: CLI.md §5 — `translate` above the threshold with no TTY and no `--yes` exits 3
// and spends nothing.
#[tokio::test]
async fn translate_refuses_without_tty_or_yes() {
    let store = translation_store().await;
    let fake = Fake::new().always(|| Ok(lines_response(&[(1, MM), (2, MM), (3, MM)])));
    let token = CancellationToken::new();

    let err = translation::run_translation_confirmed(
        || Ok::<_, PipelineError>(fake.clone()),
        &store,
        &cues(),
        &translation_params(),
        TranslationFlags::default(),
        &token,
        &PriceTable::new(),
        settings(0.0, false, false),
        std::sync::Arc::new(myansub::pipeline::progress::NoopProgress),
    )
    .await
    .expect_err("refusal");

    assert!(matches!(err, PipelineError::ConfirmationNeeded { .. }));
    assert_eq!(err.exit_code(), exit::PRECONDITION);
    assert_eq!(fake.call_count(), 0, "a refusal spends nothing");
}

// AC: CLI.md §5 — `translate --yes` clears the gate and runs.
#[tokio::test]
async fn translate_with_yes_proceeds() {
    let store = translation_store().await;
    let fake = Fake::new().always(|| Ok(lines_response(&[(1, MM), (2, MM), (3, MM)])));
    let token = CancellationToken::new();

    let confirmed = translation::run_translation_confirmed(
        || Ok::<_, PipelineError>(fake.clone()),
        &store,
        &cues(),
        &translation_params(),
        TranslationFlags::default(),
        &token,
        &PriceTable::new(),
        settings(0.0, false, true),
        std::sync::Arc::new(myansub::pipeline::progress::NoopProgress),
    )
    .await
    .expect("run");

    assert_eq!(
        confirmed,
        ConfirmedRun::Ran(TranslationOutcome::Completed { batches: 1 })
    );
    assert!(fake.call_count() >= 1, "the translation call was sent");
}

// ---------------------------------------------------------------------------
// translate --stale wiring (`docs/LLM.md` §7)
// ---------------------------------------------------------------------------

/// Full first pass + a premise edit that marks every unlocked cue stale.
async fn store_with_stale_cues() -> Store {
    let store = translation_store().await;
    let cues = cues();
    let token = CancellationToken::new();

    // First pass at v1: every cue gets an origin=llm translation.
    run_translation(
        Fake::new().always(|| Ok(lines_response(&[(1, MM), (2, MM), (3, MM)]))),
        &store,
        &cues,
        &translation_params(),
        TranslationFlags::default(),
        &token,
        std::sync::Arc::new(myansub::pipeline::progress::NoopProgress),
    )
    .await
    .expect("first pass");

    // Edit the analysis (premise change → affects all) and approve atomically, which
    // marks every unlocked translation stale (`docs/LLM.md` §7).
    const V2: &str = r#"{"schema_version":1,"premise":"edited","tone":"t",
    "characters":[],"address_matrix":[],"glossary":[],
    "scenes":[{"cue_range":[1,3],"summary":"s","speakers":[]}],
    "style_rules":[],"unresolved_ambiguities":[]}"#;
    store
        .call(|conn| {
            insert_analysis_version(conn, V2, AnalysisOrigin::User, "sha256:proj", "prompt_v1|m")
                .map(|_| ())
        })
        .await
        .expect("v2");
    approve_and_invalidate(&store, "user")
        .await
        .expect("approve + invalidate");
    store
}

// AC: LLM.md §7 — `translate --stale` above the threshold with no TTY and no `--yes`
// exits 3 and re-translates nothing.
#[tokio::test]
async fn translate_stale_refuses_without_tty_or_yes() {
    let store = store_with_stale_cues().await;
    let fake = Fake::new().always(|| Ok(lines_response(&[(1, MM), (2, MM), (3, MM)])));
    let token = CancellationToken::new();

    let err = translation::run_translation_confirmed(
        || Ok::<_, PipelineError>(fake.clone()),
        &store,
        &cues(),
        &translation_params(),
        TranslationFlags {
            stale: true,
            ..TranslationFlags::default()
        },
        &token,
        &PriceTable::new(),
        settings(0.0, false, false),
        std::sync::Arc::new(myansub::pipeline::progress::NoopProgress),
    )
    .await
    .expect_err("refusal");

    assert!(matches!(err, PipelineError::ConfirmationNeeded { .. }));
    assert_eq!(
        fake.call_count(),
        0,
        "no stale cue re-translated on refusal"
    );
}

// AC: LLM.md §7 — `translate --stale --yes` re-translates exactly the stale cues.
#[tokio::test]
async fn translate_stale_with_yes_retranslates_stale_cues() {
    let store = store_with_stale_cues().await;
    let fake = Fake::new().always(|| Ok(lines_response(&[(1, MM), (2, MM), (3, MM)])));
    let token = CancellationToken::new();

    let confirmed = translation::run_translation_confirmed(
        || Ok::<_, PipelineError>(fake.clone()),
        &store,
        &cues(),
        &translation_params(),
        TranslationFlags {
            stale: true,
            ..TranslationFlags::default()
        },
        &token,
        &PriceTable::new(),
        settings(0.0, false, true),
        std::sync::Arc::new(myansub::pipeline::progress::NoopProgress),
    )
    .await
    .expect("stale run");

    assert!(matches!(confirmed, ConfirmedRun::Ran(_)));
    let sent: String = fake
        .calls()
        .into_iter()
        .flat_map(|r| r.contents.into_iter().map(|b| b.text))
        .collect();
    assert!(sent.contains("[1]"), "cue 1 re-sent");
    assert!(sent.contains("[2]"), "cue 2 re-sent");
    assert!(sent.contains("[3]"), "cue 3 re-sent");
}

// ---------------------------------------------------------------------------
// run_analysis is unaffected when the gate is cleared (sanity: the wrapper delegates)
// ---------------------------------------------------------------------------

// AC: CLI.md §5 — with `--yes`, the confirmed wrapper runs exactly what `run_analysis`
// would (same stored version), proving the gate only decides whether to run.
#[tokio::test]
async fn confirmed_run_matches_direct_run() {
    let direct_store = Store::open_in_memory().expect("open");
    run_analysis(
        Fake::new().push_response(analysis_response()),
        &direct_store,
        &cues(),
        &analysis_params(),
        AnalysisFlags::default(),
        &CancellationToken::new(),
        std::sync::Arc::new(myansub::pipeline::progress::NoopProgress),
    )
    .await
    .expect("direct");

    let gated_store = Store::open_in_memory().expect("open");
    let confirmed = analysis::run_analysis_confirmed(
        || Ok::<_, PipelineError>(Fake::new().push_response(analysis_response())),
        &gated_store,
        &cues(),
        &analysis_params(),
        AnalysisFlags::default(),
        &CancellationToken::new(),
        &PriceTable::new(),
        settings(0.0, false, true),
        std::sync::Arc::new(myansub::pipeline::progress::NoopProgress),
    )
    .await
    .expect("gated");

    assert_eq!(
        confirmed,
        ConfirmedRun::Ran(AnalysisOutcome::Completed { version: 1 })
    );
}

// ---------------------------------------------------------------------------
// --stale estimate scoping (`docs/LLM.md` §7) — scattered stale cues
// ---------------------------------------------------------------------------

/// Analysis naming character c1 "Aoi"; scene speakers empty so only the name-mention
/// path fires (not the scene-member path). Covers cues 1..=6.
const ANALYSIS_AOI: &str = r#"{"schema_version":1,"premise":"p","tone":"t",
"characters":[{"id":"c1","name":"Aoi","aliases":[],"gender":"f","age_group":"young","role":"lead","speech_style":"plain"}],
"address_matrix":[],"glossary":[],
"scenes":[{"cue_range":[1,6],"summary":"s","speakers":[],"mood":"calm"}],
"style_rules":[],"unresolved_ambiguities":[]}"#;

/// The same analysis with c1 renamed Aoi → Yuki (affects only cues mentioning "Aoi").
const ANALYSIS_AOI_RENAMED: &str = r#"{"schema_version":1,"premise":"p","tone":"t",
"characters":[{"id":"c1","name":"Yuki","aliases":[],"gender":"f","age_group":"young","role":"lead","speech_style":"plain"}],
"address_matrix":[],"glossary":[],
"scenes":[{"cue_range":[1,6],"summary":"s","speakers":[],"mood":"calm"}],
"style_rules":[],"unresolved_ambiguities":[]}"#;

/// Six cues where only cues 1 and 3 mention "Aoi" — so a rename marks exactly {1,3}
/// stale, scattered across two different `batch_size = 2` batches ([1,2] and [3,4]).
fn scattered_cues() -> Vec<Cue> {
    vec![
        cue(1, "Good morning Aoi."),
        cue(2, "How are you?"),
        cue(3, "Aoi, be careful."),
        cue(4, "I will."),
        cue(5, "See you."),
        cue(6, "Bye."),
    ]
}

/// A store whose cues 1 and 3 are stale (scattered), via a first full pass at v1 then a
/// character rename approved atomically (which marks the affected unlocked cues stale).
async fn scattered_stale_store() -> Store {
    let store = Store::open_in_memory().expect("open");
    let cues = scattered_cues();
    store
        .call(move |conn| insert_cues(conn, &cues))
        .await
        .expect("cues");
    store
        .call(|conn| {
            insert_analysis_version(
                conn,
                ANALYSIS_AOI,
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

    // First full pass at v1 (batch_size 2 → 3 batches), so every cue has a translation.
    run_translation(
        Fake::new().always(|| {
            Ok(lines_response(&[
                (1, MM),
                (2, MM),
                (3, MM),
                (4, MM),
                (5, MM),
                (6, MM),
            ]))
        }),
        &store,
        &scattered_cues(),
        &batch2_params(),
        TranslationFlags::default(),
        &CancellationToken::new(),
        std::sync::Arc::new(myansub::pipeline::progress::NoopProgress),
    )
    .await
    .expect("first pass");

    // Rename c1 and approve atomically → marks cues 1 and 3 stale (name-mention path).
    store
        .call(|conn| {
            insert_analysis_version(
                conn,
                ANALYSIS_AOI_RENAMED,
                AnalysisOrigin::User,
                "sha256:p",
                "prompt_v1|m",
            )
            .map(|_| ())
        })
        .await
        .expect("analysis v2");
    approve_and_invalidate(&store, "user")
        .await
        .expect("approve + invalidate");
    store
}

// AC: LLM.md §7 — the stale set is exactly the affected, unlocked cues {1,3}
// (a rename invalidates only the cues mentioning the name, not the whole project).
#[tokio::test]
async fn stale_unlocked_idxs_returns_only_the_stale_set() {
    let store = scattered_stale_store().await;
    let stale = translation::stale_unlocked_idxs(&store)
        .await
        .expect("stale");
    assert_eq!(stale, BTreeSet::from([1u32, 3u32]));
}

// AC: LLM.md §7 — the --stale estimate keeps ONE prefix-bearing request per ORIGINAL
// batch holding a stale cue (cues 1 and 3 live in two different batches → 2 requests),
// never collapsing scattered stale cues into a single re-chunked request.
#[tokio::test]
async fn stale_estimate_keeps_one_request_per_affected_batch() {
    let store = scattered_stale_store().await;
    let report = translation::dry_run_translation_stale(
        &store,
        &scattered_cues(),
        &batch2_params(),
        &priced(),
    )
    .await
    .expect("stale estimate");
    assert_eq!(report.calls, 2, "one request per affected original batch");
    assert_eq!(report.batches, 2);
}

// AC: LLM.md §7 — the --stale estimate is strictly SMALLER than a full-project estimate
// (proving the scoping is real, not indistinguishable from estimating everything).
#[tokio::test]
async fn stale_estimate_is_under_full_project_estimate() {
    let store = scattered_stale_store().await;
    let stale = translation::dry_run_translation_stale(
        &store,
        &scattered_cues(),
        &batch2_params(),
        &priced(),
    )
    .await
    .expect("stale");
    let full =
        translation::dry_run_translation(&store, &scattered_cues(), &batch2_params(), &priced())
            .await
            .expect("full");
    assert!(stale.calls < full.calls, "{} < {}", stale.calls, full.calls);
    assert!(stale.estimated_prompt_tokens < full.estimated_prompt_tokens);
    assert!(known(stale.estimated_cost) < known(full.estimated_cost));
}

// AC: LLM.md §7 — the --stale WIRING uses the scoped estimate: with a threshold set
// between the stale-scoped cost and the full-project cost, a correctly-scoped run
// PROCEEDS (a regression estimating the whole project would exceed it and refuse).
#[tokio::test]
async fn stale_wiring_uses_scoped_estimate_not_full_project() {
    let store = scattered_stale_store().await;
    let stale_cost = known(
        translation::dry_run_translation_stale(
            &store,
            &scattered_cues(),
            &batch2_params(),
            &priced(),
        )
        .await
        .expect("stale")
        .estimated_cost,
    );
    let full_cost = known(
        translation::dry_run_translation(&store, &scattered_cues(), &batch2_params(), &priced())
            .await
            .expect("full")
            .estimated_cost,
    );
    assert!(stale_cost < full_cost, "{stale_cost} < {full_cost}");
    let threshold = (stale_cost + full_cost) / 2.0;

    let fake = Fake::new().always(|| Ok(lines_response(&[(1, MM), (3, MM)])));
    let confirmed = translation::run_translation_confirmed(
        || Ok::<_, PipelineError>(fake.clone()),
        &store,
        &scattered_cues(),
        &batch2_params(),
        TranslationFlags {
            stale: true,
            ..TranslationFlags::default()
        },
        &CancellationToken::new(),
        &priced(),
        settings(threshold, false, false),
        std::sync::Arc::new(myansub::pipeline::progress::NoopProgress),
    )
    .await
    .expect("scoped estimate under threshold proceeds");

    assert!(
        matches!(confirmed, ConfirmedRun::Ran(_)),
        "correct scoping proceeds; a full-project estimate would exceed the threshold"
    );
}

// ---------------------------------------------------------------------------
// cache prune (`docs/CLI.md` §6, `docs/LLM.md` §4)
// ---------------------------------------------------------------------------

/// Inserts a `llm_cache` row with a specific `created_at` modifier (e.g. `-100 days`).
async fn insert_cache_row(store: &Store, key: &'static str, created_at_modifier: &'static str) {
    store
        .call(move |conn| {
            conn.execute(
                "INSERT INTO llm_cache (key, model, prompt_version, response, usage, created_at) \
                 VALUES (?1, 'm', 1, 'r', '{}', datetime('now', ?2))",
                rusqlite::params![key, created_at_modifier],
            )
            .map_err(myansub::store::StoreError::from)?;
            Ok(())
        })
        .await
        .expect("insert cache row");
}

async fn cache_count(store: &Store) -> i64 {
    store
        .call(|conn| {
            conn.query_row("SELECT COUNT(*) FROM llm_cache", [], |r| r.get::<_, i64>(0))
                .map_err(myansub::store::StoreError::from)
        })
        .await
        .expect("count")
}

// AC: CLI.md §6 — `cache prune --older-than N` removes only entries older than N days
// (aged by first-seen `created_at`, M3-C), keeping recent ones.
#[tokio::test]
async fn prune_older_than_removes_only_old_entries() {
    let store = Store::open_in_memory().expect("open");
    insert_cache_row(&store, "old", "-100 days").await;
    insert_cache_row(&store, "recent", "-1 days").await;

    let removed = store
        .call(|conn| repo_llm::prune_cache(conn, 30))
        .await
        .expect("prune");

    assert_eq!(removed, 1, "only the 100-day-old entry is removed");
    assert_eq!(cache_count(&store).await, 1);
    assert!(
        store
            .call(|conn| repo_llm::get_cache_row(conn, "old"))
            .await
            .expect("get old")
            .is_none(),
        "the old entry was pruned; the recent one survives"
    );
}

// AC: CLI.md §6 — `cache prune` with no flag removes every entry and reports the count.
#[tokio::test]
async fn prune_all_removes_every_entry() {
    let store = Store::open_in_memory().expect("open");
    insert_cache_row(&store, "old", "-100 days").await;
    insert_cache_row(&store, "recent", "-1 days").await;
    insert_cache_row(&store, "fresh", "-0 days").await;
    assert_eq!(cache_count(&store).await, 3);

    let removed = store
        .call(repo_llm::prune_all_cache)
        .await
        .expect("prune all");

    assert_eq!(removed, 3, "every entry removed; count reported");
    assert_eq!(cache_count(&store).await, 0);
}

// AC: CLI.md §6 — the `cache prune` router (what the command drives): `None` removes
// every entry and reports the count (guards the None → prune_all routing).
#[tokio::test]
async fn prune_router_none_removes_all() {
    let store = Store::open_in_memory().expect("open");
    insert_cache_row(&store, "old", "-100 days").await;
    insert_cache_row(&store, "recent", "-1 days").await;

    let removed = store
        .call(|conn| repo_llm::prune(conn, None))
        .await
        .expect("prune None");

    assert_eq!(removed, 2);
    assert_eq!(cache_count(&store).await, 0);
}

// AC: CLI.md §6 — the router with `Some(days)` prunes only old entries, and a huge day
// count is clamped (no overflow/panic), removing nothing recent.
#[tokio::test]
async fn prune_router_some_prunes_by_age_and_clamps() {
    let store = Store::open_in_memory().expect("open");
    insert_cache_row(&store, "old", "-100 days").await;
    insert_cache_row(&store, "recent", "-1 days").await;

    let removed = store
        .call(|conn| repo_llm::prune(conn, Some(30)))
        .await
        .expect("prune Some(30)");
    assert_eq!(removed, 1, "only the old entry is pruned");

    // A day count beyond u32 is clamped rather than overflowing; it prunes nothing new.
    let removed = store
        .call(|conn| repo_llm::prune(conn, Some(u64::MAX)))
        .await
        .expect("prune clamp");
    assert_eq!(removed, 0);
    assert_eq!(cache_count(&store).await, 1);
}
