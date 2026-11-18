//! M5-A analysis execution tests via the public pipeline surface, driven by the
//! scripted `Fake` transport (`docs/TESTING.md` §3/§4). No network, `tempfile`
//! stores for the crash-resume case.
//!
//! Covers: M5-AC1 (analyze stores a versioned analysis with provenance; rerun is a
//! no-op; `--redo` mints a new version), M5-AC2 (map-reduce resumes after a crash
//! between chunks with no duplicate map calls), M5-AC5 (`--dry-run` writes nothing
//! and reports an estimate), and the content-retry behaviour of `docs/LLM.md` §6.

use std::time::Duration;

use myansub::domain::Cue;
use myansub::error::ExitCode;
use myansub::llm::PROMPT_VERSION;
use myansub::llm::client::{CommitToken, LlmResponse, ServiceTier, ThinkingSetting, Usage};
use myansub::llm::dry_run::Estimator;
use myansub::llm::fake::Fake;
use myansub::llm::pricing::{Cost, ModelPrice, PriceTable};
use myansub::llm::retry::RetryConfig;
use myansub::pipeline::PipelineError;
use myansub::pipeline::analysis::{
    AnalysisFlags, AnalysisModeKind, AnalysisOutcome, AnalysisParams, analysis_exit_code,
    analysis_output_budget, dry_run_analysis, parse_tier, prepare_analysis, run_analysis,
    worst_case_doc_tokens,
};
use myansub::pipeline::input::run_input;
use myansub::pipeline::scheduler::SchedulerConfig;
use myansub::store::repo_analysis::get_analysis_version;
use myansub::store::repo_project::set_stage;
use myansub::store::repo_units::{
    claim_analysis_chunk, complete_analysis_chunk, count_analysis_chunks_by_status,
    create_analysis_chunk,
};
use myansub::store::{Stage, StageStatus, Store, UnitStatus};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

const MODEL: &str = "gemini-flash-latest";

/// A full, schema-valid analysis document (passes `validate_full`).
const VALID: &str = r#"{"schema_version":1,"premise":"A traveller arrives at dawn.","tone":"calm",
"characters":[{"id":"c1","name":"Aoi","gender":"f","age_group":"young"}],
"address_matrix":[],"glossary":[],
"scenes":[{"cue_range":[1,5],"summary":"intro"}],
"style_rules":[],"unresolved_ambiguities":[]}"#;

/// A second valid document, to distinguish a `--redo` version.
const VALID_V2: &str = r#"{"schema_version":1,"premise":"A revised premise.","tone":"tense",
"characters":[],"scenes":[{"cue_range":[1,5],"summary":"intro"}]}"#;

/// A valid map-phase partial (passes `validate_partial`: any JSON object).
const PARTIAL: &str = r#"{"premise":"partial","characters":[]}"#;

/// Unparseable text (fails validation → triggers a content retry).
const INVALID: &str = "this is not json {";

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
        cue(2, "Where are you going?"),
        cue(3, "To the market."),
        cue(4, "Be careful out there."),
        cue(5, "I will, thank you."),
    ]
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

/// Base params. `single_pass_max_tokens` is huge, so the default plan is single-pass.
fn params() -> AnalysisParams {
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

/// Params that force map-reduce (tiny threshold) with two cues per chunk.
fn map_reduce_params() -> AnalysisParams {
    AnalysisParams {
        single_pass_max_tokens: 1,
        chunk_cues: 2,
        ..params()
    }
}

// ---------------------------------------------------------------------------
// M5-AC1: analyze stores a versioned analysis with provenance
// ---------------------------------------------------------------------------

// AC: M5-AC1 — single-pass analyze stores version 1 with correct provenance
// (source_digest == project hash; signature == "prompt_v{N}|{model}"), unapproved,
// doc parsing to the analysis schema.
#[tokio::test]
async fn single_pass_stores_version_one_with_provenance() {
    let store = Store::open_in_memory().expect("open");
    let fake = Fake::new().push_response(response(VALID));
    let token = CancellationToken::new();

    let outcome = run_analysis(
        fake.clone(),
        &store,
        &cues(),
        &params(),
        AnalysisFlags::default(),
        &token,
    )
    .await
    .expect("run");

    assert_eq!(outcome, AnalysisOutcome::Completed { version: 1 });
    assert_eq!(fake.call_count(), 1, "single-pass makes exactly one call");

    let v = store
        .call(|c| get_analysis_version(c, 1))
        .await
        .expect("get")
        .expect("version 1 present");
    assert_eq!(v.source_digest, "sha256:proj");
    assert_eq!(v.signature, format!("prompt_v{PROMPT_VERSION}|{MODEL}"));
    assert!(!v.approved, "a fresh version is unapproved");
    // The stored doc parses as the analysis document.
    let parsed: serde_json::Value = serde_json::from_str(&v.doc).expect("doc is valid JSON");
    assert_eq!(parsed["schema_version"], 1);
}

// AC: M5-AC1 — a second analyze with a current version and no --redo does nothing
// (no new version, no LLM call); --redo mints version 2.
#[tokio::test]
async fn rerun_is_noop_without_redo_and_redo_mints_a_new_version() {
    let store = Store::open_in_memory().expect("open");
    let token = CancellationToken::new();

    run_analysis(
        Fake::new().push_response(response(VALID)),
        &store,
        &cues(),
        &params(),
        AnalysisFlags::default(),
        &token,
    )
    .await
    .expect("first run");

    // Second run, no --redo: nothing to do, transport untouched.
    let fake2 = Fake::new().push_response(response(VALID));
    let outcome = run_analysis(
        fake2.clone(),
        &store,
        &cues(),
        &params(),
        AnalysisFlags::default(),
        &token,
    )
    .await
    .expect("second run");
    assert_eq!(outcome, AnalysisOutcome::NothingToDo);
    assert_eq!(fake2.call_count(), 0, "nothing-to-do makes no LLM call");

    // --redo: a new version even though a current one exists. The input is
    // identical, so the response is served from the local cache (no transport call),
    // but a new, unapproved version is still minted (`docs/LLM.md` §4, `docs/CLI.md`
    // §6).
    let fake3 = Fake::new().push_response(response(VALID_V2));
    let outcome = run_analysis(
        fake3.clone(),
        &store,
        &cues(),
        &params(),
        AnalysisFlags {
            redo: true,
            retry_failed: false,
        },
        &token,
    )
    .await
    .expect("redo run");
    assert_eq!(outcome, AnalysisOutcome::Completed { version: 2 });
    assert_eq!(
        fake3.call_count(),
        0,
        "identical input is served from cache"
    );
    let v2 = store
        .call(|c| get_analysis_version(c, 2))
        .await
        .expect("get")
        .expect("version 2 present");
    assert_eq!(
        v2.doc, VALID,
        "redo reuses the cached response for a new version"
    );
    assert!(!v2.approved);
}

// AC: M5-AC1 — a --redo with a *different* model is a fresh cache key, so it does
// call the transport and can produce a different analysis.
#[tokio::test]
async fn redo_with_different_model_calls_the_transport() {
    let store = Store::open_in_memory().expect("open");
    let token = CancellationToken::new();

    run_analysis(
        Fake::new().push_response(response(VALID)),
        &store,
        &cues(),
        &params(),
        AnalysisFlags::default(),
        &token,
    )
    .await
    .expect("first run");

    let fake2 = Fake::new().push_response(response(VALID_V2));
    let mut p = params();
    p.model = "gemini-pro-latest".to_string();
    let outcome = run_analysis(
        fake2.clone(),
        &store,
        &cues(),
        &p,
        AnalysisFlags {
            redo: true,
            retry_failed: false,
        },
        &token,
    )
    .await
    .expect("redo run");

    assert_eq!(outcome, AnalysisOutcome::Completed { version: 2 });
    assert_eq!(fake2.call_count(), 1, "a new model is a cache miss");
    let v2 = store
        .call(|c| get_analysis_version(c, 2))
        .await
        .expect("get")
        .expect("v2");
    assert_eq!(
        v2.signature,
        format!("prompt_v{PROMPT_VERSION}|gemini-pro-latest")
    );
}

// ---------------------------------------------------------------------------
// Content retries (docs/LLM.md §6)
// ---------------------------------------------------------------------------

// AC: content retry — an invalid response then a valid one retries the whole call
// once and succeeds; the content attempts are the two calls (separate from the
// transport retries, which never fire here because each call is a transport success).
#[tokio::test]
async fn content_retry_succeeds_after_one_invalid_response() {
    let store = Store::open_in_memory().expect("open");
    let fake = Fake::new()
        .push_response(response(INVALID))
        .push_response(response(VALID));
    let token = CancellationToken::new();

    let outcome = run_analysis(
        fake.clone(),
        &store,
        &cues(),
        &params(),
        AnalysisFlags::default(),
        &token,
    )
    .await
    .expect("run");

    assert_eq!(outcome, AnalysisOutcome::Completed { version: 1 });
    assert_eq!(fake.call_count(), 2, "one retry: invalid then valid");
}

// AC: content retry — invalid beyond max_content_attempts fails the unit (exit 4),
// and the content-attempt counter is bounded by max_content_attempts (3 calls for
// one chunk), not by the transport retry budget.
#[tokio::test]
async fn content_retry_exhaustion_fails_the_unit_exit_4() {
    let store = Store::open_in_memory().expect("open");
    // Always invalid → every attempt fails validation.
    let fake = Fake::new().always(|| Ok(response(INVALID)));
    let token = CancellationToken::new();

    // One chunk (3 cues, chunk_cues from map_reduce_params is 2 → 2 chunks).
    let mut p = map_reduce_params();
    p.chunk_cues = 10; // all five cues in a single chunk
    let outcome = run_analysis(
        fake.clone(),
        &store,
        &cues(),
        &p,
        AnalysisFlags::default(),
        &token,
    )
    .await
    .expect("run");

    assert_eq!(
        outcome,
        AnalysisOutcome::CompletedWithFailures { failed: 1 }
    );
    assert_eq!(analysis_exit_code(false, &outcome), 4);
    assert_eq!(
        fake.call_count(),
        3,
        "exactly max_content_attempts calls for the one chunk"
    );
    let failed = store
        .call(|c| count_analysis_chunks_by_status(c, UnitStatus::Failed))
        .await
        .expect("count");
    assert_eq!(failed, 1);
}

// ---------------------------------------------------------------------------
// M5-AC2: map-reduce resumes after a simulated crash between chunks
// ---------------------------------------------------------------------------

// AC: M5-AC2 — a map-reduce over three chunks is interrupted after the first chunk
// completes; a rerun on the reopened store finishes it. Every chunk's map call is
// sent exactly once across the two runs, and the merged version is produced.
#[tokio::test]
async fn map_reduce_resumes_after_crash_with_no_duplicate_map_calls() {
    let tmp = tempdir().expect("tempdir");
    let db = tmp.path().join("project.db");
    let p = map_reduce_params(); // single_pass_max_tokens 1, chunk_cues 2 → 3 chunks

    // --- Run 1: crash after the first chunk completes. ---
    let token1 = CancellationToken::new();
    let fake1 = Fake::new()
        .push_response(response(PARTIAL))
        .push_response(response(PARTIAL))
        .push_response(response(PARTIAL))
        .cancel_after(token1.clone(), 1);
    {
        let store = Store::open(&db).expect("open 1");
        let outcome = run_analysis(
            fake1.clone(),
            &store,
            &cues(),
            &p,
            AnalysisFlags::default(),
            &token1,
        )
        .await
        .expect("run 1");
        assert_eq!(outcome, AnalysisOutcome::Interrupted);
    }
    assert_eq!(fake1.call_count(), 1, "run 1 sent exactly one map call");

    // --- Run 2: reopen the store and finish. ---
    let token2 = CancellationToken::new();
    let fake2 = Fake::new()
        .push_response(response(PARTIAL)) // chunk 2 map
        .push_response(response(PARTIAL)) // chunk 3 map
        .push_response(response(VALID)); // reduce
    let final_version;
    {
        let store = Store::open(&db).expect("open 2");
        let outcome = run_analysis(
            fake2.clone(),
            &store,
            &cues(),
            &p,
            AnalysisFlags::default(),
            &token2,
        )
        .await
        .expect("run 2");
        assert_eq!(outcome, AnalysisOutcome::Completed { version: 1 });

        let v = store
            .call(|c| get_analysis_version(c, 1))
            .await
            .expect("get")
            .expect("version present");
        final_version = v.doc;
        let done = store
            .call(|c| count_analysis_chunks_by_status(c, UnitStatus::Done))
            .await
            .expect("count");
        assert_eq!(done, 3, "all three chunks are done");
    }

    assert_eq!(fake2.call_count(), 3, "two remaining maps + one reduce");
    assert_eq!(
        final_version, VALID,
        "the reduce output is the stored version"
    );

    // Each chunk's map call appears exactly once across the two runs. Chunk 1 covers
    // cues 1-2, chunk 2 covers 3-4, chunk 3 covers 5.
    let all: Vec<String> = fake1
        .calls()
        .into_iter()
        .chain(fake2.calls())
        .map(|r| {
            r.contents
                .iter()
                .map(|b| b.text.clone())
                .collect::<String>()
        })
        .collect();
    for marker in ["[1]", "[3]", "[5]"] {
        let hits = all.iter().filter(|c| c.contains(marker)).count();
        assert_eq!(hits, 1, "map call for {marker} sent exactly once");
    }
}

// ---------------------------------------------------------------------------
// M5-AC5: --dry-run writes nothing and prints a report
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
                count("SELECT COUNT(*) FROM analysis")?,
                count("SELECT COUNT(*) FROM analysis_chunks")?,
                count("SELECT COUNT(*) FROM llm_calls")?,
                count("SELECT COUNT(*) FROM llm_cache")?,
            ))
        })
        .await
        .expect("counts")
}

// AC: M5-AC5 — a single-pass dry run reports an estimate and leaves the DB
// unchanged (no analysis version, no chunk rows, no llm_calls, no cache rows).
#[tokio::test]
async fn dry_run_single_pass_reports_estimate_and_writes_nothing() {
    let store = Store::open_in_memory().expect("open");
    let before = table_counts(&store).await;

    let report = dry_run_analysis(&store, &cues(), &params(), &priced_table())
        .await
        .expect("dry run");

    assert_eq!(report.mode, AnalysisModeKind::SinglePass);
    assert_eq!(report.chunk_count, 0);
    assert_eq!(report.calls, 1);
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
    assert_eq!(after, (0, 0, 0, 0), "nothing was written");
}

// AC: M5-AC5 — a map-reduce dry run reports chunk count and the reduce call, and
// still writes nothing.
#[tokio::test]
async fn dry_run_map_reduce_reports_chunks_and_writes_nothing() {
    let store = Store::open_in_memory().expect("open");
    let before = table_counts(&store).await;

    let report = dry_run_analysis(&store, &cues(), &map_reduce_params(), &priced_table())
        .await
        .expect("dry run");

    assert_eq!(report.mode, AnalysisModeKind::MapReduce);
    assert_eq!(report.chunk_count, 3, "5 cues / 2 per chunk → 3 chunks");
    assert_eq!(report.calls, 4, "3 map calls + 1 reduce call");

    let after = table_counts(&store).await;
    assert_eq!(before, after);
    assert_eq!(after, (0, 0, 0, 0));
}

// ---------------------------------------------------------------------------
// Review #1: generous output budget (truncation risk)
// ---------------------------------------------------------------------------

// AC (review #1) — the analysis maxOutputTokens budget exceeds the old fixed 16384
// and comfortably covers the worst-case document (from the schema caps), while
// staying within the model output ceiling.
#[test]
fn analysis_output_budget_is_generous_and_bounded() {
    let budget = analysis_output_budget();
    assert!(
        budget > 16_384,
        "budget must exceed the old fixed 16384, got {budget}"
    );
    assert!(
        u64::from(budget) >= worst_case_doc_tokens(),
        "budget {budget} must cover the worst-case doc {}",
        worst_case_doc_tokens()
    );
    assert!(budget <= 65_536, "budget stays within the model ceiling");
}

// AC (review #1) — the actual single-pass request carries the generous budget, so a
// large merged/large-cast document is not truncated into a validation failure.
#[tokio::test]
async fn single_pass_request_carries_the_generous_output_budget() {
    let store = Store::open_in_memory().expect("open");
    let fake = Fake::new().push_response(response(VALID));
    let token = CancellationToken::new();

    run_analysis(
        fake.clone(),
        &store,
        &cues(),
        &params(),
        AnalysisFlags::default(),
        &token,
    )
    .await
    .expect("run");

    let req = &fake.calls()[0];
    assert_eq!(
        req.generation_config.max_output_tokens,
        analysis_output_budget()
    );
    assert!(req.generation_config.max_output_tokens > 16_384);
}

// ---------------------------------------------------------------------------
// Review #2: --retry-failed reruns only the failed chunk
// ---------------------------------------------------------------------------

// AC (review #2) — with one chunk failed and the rest done, `--retry-failed` re-sends
// only the failed chunk's map call (done chunks untouched) and produces the merged
// version.
#[tokio::test]
async fn retry_failed_reruns_only_the_failed_chunk() {
    let store = Store::open_in_memory().expect("open");
    let token = CancellationToken::new();
    let two = vec![cue(1, "Good morning."), cue(2, "Where are you going?")];
    let mut p = map_reduce_params();
    p.chunk_cues = 1; // one cue per chunk → chunk 1 = [1], chunk 2 = [2]

    // Run 1: chunk 1 exhausts its content retries (3 invalid); chunk 2 succeeds.
    let fake1 = Fake::new()
        .push_response(response(INVALID))
        .push_response(response(INVALID))
        .push_response(response(INVALID))
        .push_response(response(PARTIAL));
    let o1 = run_analysis(
        fake1.clone(),
        &store,
        &two,
        &p,
        AnalysisFlags::default(),
        &token,
    )
    .await
    .expect("run 1");
    assert_eq!(o1, AnalysisOutcome::CompletedWithFailures { failed: 1 });
    assert_eq!(fake1.call_count(), 4, "chunk 1 x3 (exhausted) + chunk 2 x1");

    // Run 2: --retry-failed reruns only chunk 1, then the reduce.
    let fake2 = Fake::new()
        .push_response(response(PARTIAL)) // chunk 1 retry
        .push_response(response(VALID)); // reduce
    let o2 = run_analysis(
        fake2.clone(),
        &store,
        &two,
        &p,
        AnalysisFlags {
            redo: false,
            retry_failed: true,
        },
        &token,
    )
    .await
    .expect("run 2");
    assert_eq!(o2, AnalysisOutcome::Completed { version: 1 });
    assert_eq!(
        fake2.call_count(),
        2,
        "only the failed chunk's map + the reduce"
    );

    let run2: Vec<String> = fake2
        .calls()
        .into_iter()
        .map(|r| {
            r.contents
                .iter()
                .map(|b| b.text.clone())
                .collect::<String>()
        })
        .collect();
    assert!(
        run2.iter().any(|t| t.contains("[1]")),
        "failed chunk 1 re-sent"
    );
    assert!(
        !run2.iter().any(|t| t.contains("[2]")),
        "done chunk 2 not re-sent"
    );

    let done = store
        .call(|c| count_analysis_chunks_by_status(c, UnitStatus::Done))
        .await
        .expect("count");
    assert_eq!(done, 2, "both chunks done after the retry");
}

// ---------------------------------------------------------------------------
// Review #4: --tier override
// ---------------------------------------------------------------------------

// AC (review #4) — parse_tier maps the config/CLI labels; unknown defaults to flex.
#[test]
fn parse_tier_maps_labels() {
    assert_eq!(parse_tier("standard"), ServiceTier::Standard);
    assert_eq!(parse_tier("STANDARD"), ServiceTier::Standard);
    assert_eq!(parse_tier("flex"), ServiceTier::Flex);
    assert_eq!(parse_tier("anything-else"), ServiceTier::Flex);
}

// AC (review #4) — the tier in params reaches the actual LLM request.
#[tokio::test]
async fn tier_override_reaches_the_request() {
    let store = Store::open_in_memory().expect("open");
    let fake = Fake::new().push_response(response(VALID));
    let token = CancellationToken::new();
    let mut p = params();
    p.tier = ServiceTier::Standard;

    run_analysis(
        fake.clone(),
        &store,
        &cues(),
        &p,
        AnalysisFlags::default(),
        &token,
    )
    .await
    .expect("run");

    assert_eq!(fake.calls()[0].service_tier, ServiceTier::Standard);
}

// ---------------------------------------------------------------------------
// Review #5: analyze command preconditions (gate + source-hash)
// ---------------------------------------------------------------------------

const SRT: &str = "1\n00:00:01,000 --> 00:00:03,000\nGood morning.\n\n\
2\n00:00:04,000 --> 00:00:06,000\nWhere are you going?\n";

/// Initializes a file-backed project with the input stage completed.
async fn init_project(db: &std::path::Path, source: &std::path::Path) -> Store {
    std::fs::write(source, SRT).expect("write source");
    let store = Store::open(db).expect("open");
    run_input(&store, source, "en", None, Some("srt"), None, None)
        .await
        .expect("input stage");
    store
}

// AC (review #5) — prepare_analysis refuses (exit 3) when the input stage is not
// completed (the analysis gate).
#[tokio::test]
async fn analyze_refused_when_input_not_completed_exit_3() {
    let tmp = tempdir().expect("tempdir");
    let db = tmp.path().join("project.db");
    let source = tmp.path().join("movie.srt");
    let store = init_project(&db, &source).await;

    // Downgrade the input stage so the gate fails.
    store
        .call(|c| set_stage(c, Stage::Input, StageStatus::Running))
        .await
        .expect("downgrade");

    let err = prepare_analysis(&store, &source)
        .await
        .expect_err("analysis gate must refuse");
    assert_eq!(err.exit_code(), 3);
    assert!(
        matches!(err, PipelineError::GateViolation { .. }),
        "got {err:?}"
    );
}

// AC (review #5) — prepare_analysis refuses (exit 3) when the source changed.
#[tokio::test]
async fn analyze_refused_when_source_changed_exit_3() {
    let tmp = tempdir().expect("tempdir");
    let db = tmp.path().join("project.db");
    let source = tmp.path().join("movie.srt");
    let store = init_project(&db, &source).await;

    std::fs::write(
        &source,
        format!("{SRT}\n3\n00:00:07,000 --> 00:00:08,000\nNew.\n"),
    )
    .expect("modify source");

    let err = prepare_analysis(&store, &source)
        .await
        .expect_err("changed source must refuse");
    assert_eq!(err.exit_code(), 3);
    assert!(
        matches!(err, PipelineError::SourceChanged { .. }),
        "got {err:?}"
    );
}

// AC (review #5) — on a well-formed, input-completed project, the dry-run path
// succeeds (exit 0) and writes nothing.
#[tokio::test]
async fn analyze_dry_run_path_succeeds_and_writes_nothing() {
    let tmp = tempdir().expect("tempdir");
    let db = tmp.path().join("project.db");
    let source = tmp.path().join("movie.srt");
    let store = init_project(&db, &source).await;

    // The gate passes and the cues load.
    let cues = prepare_analysis(&store, &source).await.expect("prepared");
    assert!(!cues.is_empty());

    let before = table_counts(&store).await;
    let report = dry_run_analysis(&store, &cues, &params(), &priced_table())
        .await
        .expect("dry run");
    assert_eq!(report.mode, AnalysisModeKind::SinglePass);
    // The dry run leaves analysis/chunks/calls/cache unchanged.
    let after = table_counts(&store).await;
    assert_eq!(before, after);
}

// ---------------------------------------------------------------------------
// Review #6: a chunk interrupted BETWEEN claim and complete is recovered on reopen
// ---------------------------------------------------------------------------

// AC (review #6) — a chunk left in_flight across a Store drop/reopen is recovered to
// pending and its map call is re-sent on rerun, while an already-done chunk is not.
#[tokio::test]
async fn interrupted_in_flight_chunk_is_recovered_and_resent() {
    let tmp = tempdir().expect("tempdir");
    let db = tmp.path().join("project.db");
    let two = vec![cue(1, "Good morning."), cue(2, "Where are you going?")];
    let mut p = map_reduce_params();
    p.chunk_cues = 1; // chunk 1 = [1], chunk 2 = [2]

    // --- Simulate a crash: chunk 2 done, chunk 1 claimed (in_flight) but not done. ---
    {
        let store = Store::open(&db).expect("open 1");
        store
            .call(|conn| {
                let tx = conn.transaction()?;
                create_analysis_chunk(&tx, 1, 1, 1)?;
                create_analysis_chunk(&tx, 2, 2, 2)?;
                set_stage(&tx, Stage::Analysis, StageStatus::Running)?;
                tx.commit()?;
                Ok(())
            })
            .await
            .expect("seed chunks");
        // chunk 2: claim then complete → done.
        store
            .call(|c| claim_analysis_chunk(c, 2).map(|_| ()))
            .await
            .expect("claim 2");
        store
            .call(|c| complete_analysis_chunk(c, 2, PARTIAL).map(|_| ()))
            .await
            .expect("complete 2");
        // chunk 1: claim only → left in_flight (the crash point, between claim/complete).
        store
            .call(|c| claim_analysis_chunk(c, 1).map(|_| ()))
            .await
            .expect("claim 1");
        let in_flight = store
            .call(|c| count_analysis_chunks_by_status(c, UnitStatus::InFlight))
            .await
            .expect("count");
        assert_eq!(in_flight, 1, "chunk 1 left in_flight");
    }

    // --- Reopen: Store::open recovers in_flight → pending. ---
    let store = Store::open(&db).expect("open 2");
    let pending = store
        .call(|c| count_analysis_chunks_by_status(c, UnitStatus::Pending))
        .await
        .expect("count");
    assert_eq!(pending, 1, "the interrupted chunk was recovered to pending");

    // --- Rerun: the recovered chunk's map is re-sent; the done chunk is not. ---
    let token = CancellationToken::new();
    let fake = Fake::new()
        .push_response(response(PARTIAL)) // chunk 1 map (re-sent)
        .push_response(response(VALID)); // reduce
    let outcome = run_analysis(
        fake.clone(),
        &store,
        &two,
        &p,
        AnalysisFlags::default(),
        &token,
    )
    .await
    .expect("rerun");
    assert_eq!(outcome, AnalysisOutcome::Completed { version: 1 });
    assert_eq!(fake.call_count(), 2, "recovered chunk map + reduce");

    let calls: Vec<String> = fake
        .calls()
        .into_iter()
        .map(|r| {
            r.contents
                .iter()
                .map(|b| b.text.clone())
                .collect::<String>()
        })
        .collect();
    assert!(
        calls.iter().any(|t| t.contains("[1]")),
        "recovered chunk 1 re-sent"
    );
    assert!(
        !calls.iter().any(|t| t.contains("[2]")),
        "already-done chunk 2 not re-sent"
    );

    let done = store
        .call(|c| count_analysis_chunks_by_status(c, UnitStatus::Done))
        .await
        .expect("count");
    assert_eq!(done, 2);
}
