//! M7-C: `run` orchestrates the whole pipeline (analysis + translation + export) via
//! an injected transport, with `--auto-approve`, approval-origin tracking, the
//! failed-batch guardrail, the zero-translatable edge, resume, and `--dry-run`
//! (`docs/CLI.md` §2/§4/§6/§7, `docs/ARCHITECTURE.md` §2/§4, ADR-019/031/034).
//!
//! Every test is offline: the LLM is the scripted `Fake` transport wrapped by `run`
//! in the real `Metered<Cached<Retrying<_>>>` stack, and every project lives in a
//! `tempfile` directory (`docs/TESTING.md` §1/§3/§4).

use std::fs;
use std::path::Path;

use myansub::config::{Config, PriceEntry};
use myansub::error::ExitCode;
use myansub::formats::codec::parse_document;
use myansub::llm::client::{CommitToken, LlmResponse, Usage};
use myansub::llm::fake::Fake;
use myansub::llm::pricing::Cost;
use myansub::pipeline::PipelineError;
use myansub::pipeline::confirm::{ConfirmSettings, ConfirmedRun};
use myansub::pipeline::orchestrator::{
    ApprovalOrigin, AutoApproveGate, RunOptions, RunOutcome, RunReport, RunRequest, run,
    run_confirmed, run_exit_code, status,
};
use myansub::project::{PROJECT_DB_FILE, project_dir_for_source};
use myansub::store::repo_analysis::current_analysis;
use myansub::store::repo_translations::{cue_id_by_idx, get_translation, set_locked_in_tx};
use myansub::store::{Stage, StageStatus, Store};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

/// Three translatable cues with distinct timings, for an end-to-end export check.
const SRT3: &str = "1\n00:00:01,000 --> 00:00:03,000\nGood morning.\n\n\
2\n00:00:04,000 --> 00:00:06,000\nHow are you?\n\n\
3\n00:00:07,000 --> 00:00:09,000\nGoodbye.\n";

/// One translatable cue.
const SRT1: &str = "1\n00:00:01,000 --> 00:00:03,000\nHello.\n";

/// Two cues with no alphabetic characters, so both parse as non-translatable
/// (`docs/REQUIREMENTS.md`; `markup` marks a cue translatable only when it has a
/// letter).
const SRT_NONTRANSLATABLE: &str = "1\n00:00:01,000 --> 00:00:03,000\n123\n\n\
2\n00:00:04,000 --> 00:00:06,000\n456\n";

/// A valid analysis document whose single scene covers every small fixture.
const ANALYSIS_DOC: &str = r#"{"schema_version":1,"premise":"p","tone":"t",
"characters":[],"address_matrix":[],"glossary":[],
"scenes":[{"cue_range":[1,9],"summary":"s","speakers":[]}],
"style_rules":[],"unresolved_ambiguities":[]}"#;

const MM: &str = "မင်္ဂလာပါ"; // Burmese, so the `untranslated` check never fires.

fn resp(text: &str) -> LlmResponse {
    LlmResponse {
        text: text.to_string(),
        usage: Usage {
            prompt_tokens: 10,
            output_tokens: 5,
            ..Usage::default()
        },
        commit: CommitToken::noop(),
    }
}

/// A `{"lines":[…]}` response translating every id in `ids` to Burmese.
fn lines(ids: &[i64]) -> String {
    let items: Vec<serde_json::Value> = ids
        .iter()
        .map(|id| serde_json::json!({ "id": id, "t": MM }))
        .collect();
    serde_json::json!({ "lines": items }).to_string()
}

/// A Fake that answers the single-pass analysis call with [`ANALYSIS_DOC`] and every
/// later translation call with a Burmese line for each of `ids`.
fn fake_full(ids: &'static [i64]) -> Fake {
    Fake::new()
        .push_response(resp(ANALYSIS_DOC))
        .always(move || Ok(resp(&lines(ids))))
}

fn request<'a>(project_dir: &'a Path, source: &'a Path, options: RunOptions) -> RunRequest<'a> {
    RunRequest {
        project_dir,
        source_path: Some(source),
        from_lang: Some("en"),
        encoding: None,
        output_format: None,
        output_path: None,
        options,
    }
}

fn auto_all() -> RunOptions {
    RunOptions {
        auto_approve: Some(AutoApproveGate::All),
        ..RunOptions::default()
    }
}

async fn open(project_dir: &Path) -> Store {
    Store::open(&project_dir.join(PROJECT_DB_FILE)).expect("open store")
}

async fn stage(project_dir: &Path, stage: Stage) -> StageStatus {
    open(project_dir)
        .await
        .call(move |conn| myansub::store::repo_project::get_stage(conn, stage))
        .await
        .expect("get stage")
        .unwrap_or(StageStatus::NotStarted)
}

fn origin_of(
    report: &myansub::pipeline::orchestrator::StatusReport,
    s: Stage,
) -> Option<ApprovalOrigin> {
    report
        .stages
        .iter()
        .find(|line| line.stage == s)
        .and_then(|line| line.approval_origin)
}

// ---------------------------------------------------------------------------
// Capstone: unattended full run
// ---------------------------------------------------------------------------

// AC: CLI.md §6 — a fresh FILE with `--auto-approve all` runs the WHOLE pipeline via
// the injected Fake stack: analysis created + approved (auto), translations done,
// translation review approved (auto), and the output file is written (exists,
// re-parses, timings match). exit 0.
#[tokio::test]
async fn unattended_full_run_writes_output() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT3).expect("write source");
    let project_dir = project_dir_for_source(&source);

    let fake = fake_full(&[1, 2, 3]);
    let token = CancellationToken::new();
    let outcome = run(
        fake,
        request(&project_dir, &source, auto_all()),
        &Config::default(),
        &token,
        &mut RunReport::default(),
    )
    .await
    .expect("full run");

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(run_exit_code(false, &outcome), 0);

    // Every gate was auto-approved and every stage completed.
    assert_eq!(
        stage(&project_dir, Stage::Analysis).await,
        StageStatus::Completed
    );
    assert_eq!(
        stage(&project_dir, Stage::AnalysisReview).await,
        StageStatus::Approved
    );
    assert_eq!(
        stage(&project_dir, Stage::TranslationReview).await,
        StageStatus::Approved
    );
    assert_eq!(
        stage(&project_dir, Stage::Output).await,
        StageStatus::Completed
    );

    // The output file exists, re-parses, and preserves the source timings.
    let output = tmp.path().join("movie.my.srt");
    assert!(output.is_file(), "output file was written");
    let text = fs::read_to_string(&output).expect("read output");
    let doc = parse_document(&text, myansub::domain::SubtitleFormat::Srt).expect("reparse");
    assert_eq!(doc.cues.len(), 3);
    assert_eq!(doc.cues[0].start_ms, 1000);
    assert_eq!(doc.cues[0].end_ms, 3000);
    assert_eq!(doc.cues[2].start_ms, 7000);
    assert_eq!(doc.cues[2].end_ms, 9000);
    assert!(text.contains(MM), "output carries the Burmese translation");
}

// AC: CLI.md §4 — after an `--auto-approve all` run, `status` shows both review gates
// with origin `auto`.
#[tokio::test]
async fn auto_approve_records_origin_auto() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT3).expect("write source");
    let project_dir = project_dir_for_source(&source);

    run(
        fake_full(&[1, 2, 3]),
        request(&project_dir, &source, auto_all()),
        &Config::default(),
        &CancellationToken::new(),
        &mut RunReport::default(),
    )
    .await
    .expect("full run");

    let report = status(&project_dir, &Config::default())
        .await
        .expect("status");
    assert_eq!(
        origin_of(&report, Stage::AnalysisReview),
        Some(ApprovalOrigin::Auto)
    );
    assert_eq!(
        origin_of(&report, Stage::TranslationReview),
        Some(ApprovalOrigin::Auto)
    );
}

/// Whether the translation for cue `idx` is stale, read from `project_dir`.
async fn stale_at(project_dir: &Path, idx: u32) -> bool {
    let store = open(project_dir).await;
    let id = store
        .call(move |c| Ok(cue_id_by_idx(c)?.get(&idx).copied()))
        .await
        .expect("idx map")
        .expect("cue present");
    store
        .call(move |c| get_translation(c, id))
        .await
        .expect("get translation")
        .expect("translation row")
        .stale
}

// AC: M8-AC3 — the `run --auto-approve` trigger site: approving an edited analysis
// version atomically marks the affected unlocked translations stale, while the locked
// cue is never set stale (FR-6). A changed premise affects every translatable cue.
#[tokio::test]
async fn run_auto_approve_marks_affected_unlocked_stale() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT3).expect("write source");
    let project_dir = project_dir_for_source(&source);
    let config = Config::default();

    // Full run 1: analysis v1 approved (auto), three cues translated at v1, output
    // written.
    run(
        fake_full(&[1, 2, 3]),
        request(&project_dir, &source, auto_all()),
        &config,
        &CancellationToken::new(),
        &mut RunReport::default(),
    )
    .await
    .expect("run 1");

    // Lock cue 2: it must never be set stale.
    let store = open(&project_dir).await;
    let locked_id = store
        .call(|c| Ok(cue_id_by_idx(c)?.get(&2).copied()))
        .await
        .expect("idx map")
        .expect("cue 2");
    store
        .call(move |c| {
            let tx = c.transaction()?;
            set_locked_in_tx(&tx, locked_id, true)?;
            tx.commit()?;
            Ok(())
        })
        .await
        .expect("lock cue 2");

    // Edit the analysis: a new unapproved v2 with a changed premise, re-blocking
    // translation (the scene range is kept within the three cues so import validates).
    let v2 = r#"{"schema_version":1,"premise":"p2","tone":"t","characters":[],
    "address_matrix":[],"glossary":[],
    "scenes":[{"cue_range":[1,3],"summary":"s","speakers":[]}],
    "style_rules":[],"unresolved_ambiguities":[]}"#;
    myansub::review::analysis::import_doc(&store, v2)
        .await
        .expect("import v2");

    // Run 2 with --auto-approve: the auto-approve path approves v2 and invalidates in
    // one transaction. Nothing else executes (translation/output already complete), so
    // the stale flags persist to observe.
    run(
        fake_full(&[1, 2, 3]),
        request(&project_dir, &source, auto_all()),
        &config,
        &CancellationToken::new(),
        &mut RunReport::default(),
    )
    .await
    .expect("run 2");

    assert!(stale_at(&project_dir, 1).await, "cue 1 (unlocked) is stale");
    assert!(stale_at(&project_dir, 3).await, "cue 3 (unlocked) is stale");
    assert!(
        !stale_at(&project_dir, 2).await,
        "locked cue 2 is never set stale"
    );
}

// ---------------------------------------------------------------------------
// Gated flow without --auto-approve; origin = user
// ---------------------------------------------------------------------------

// AC: CLI.md §6/§4 — without `--auto-approve`, `run` executes analysis and stops at
// the analysis-review gate (exit 3); a manual `analysis approve` (origin user) then a
// second `run` executes translation and stops at the translation-review gate (exit 3);
// a manual `review approve` (origin user) then a third `run` exports (exit 0). status
// shows origin `user` for both gates.
#[tokio::test]
async fn gated_flow_resumes_and_records_origin_user() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT3).expect("write source");
    let project_dir = project_dir_for_source(&source);
    let config = Config::default();

    // 1) run → stops at the analysis-review gate (exit 3).
    let o1 = run(
        fake_full(&[1, 2, 3]),
        request(&project_dir, &source, RunOptions::default()),
        &config,
        &CancellationToken::new(),
        &mut RunReport::default(),
    )
    .await
    .expect("run 1");
    assert!(
        matches!(
            o1,
            RunOutcome::StoppedAtGate {
                stage: Stage::AnalysisReview,
                ..
            }
        ),
        "got {o1:?}"
    );
    assert_eq!(run_exit_code(false, &o1), 3);

    // 2) analysis approve (origin user).
    myansub::review::analysis::approve(&open(&project_dir).await)
        .await
        .expect("analysis approve");

    // 3) run → executes translation, stops at the translation-review gate (exit 3).
    let o2 = run(
        fake_full(&[1, 2, 3]),
        request(&project_dir, &source, RunOptions::default()),
        &config,
        &CancellationToken::new(),
        &mut RunReport::default(),
    )
    .await
    .expect("run 2");
    assert!(
        matches!(
            o2,
            RunOutcome::StoppedAtGate {
                stage: Stage::TranslationReview,
                ..
            }
        ),
        "got {o2:?}"
    );
    assert_eq!(run_exit_code(false, &o2), 3);

    // 4) review approve (origin user).
    myansub::review::translation::approve(&open(&project_dir).await)
        .await
        .expect("review approve");

    // 5) run → exports (exit 0).
    let o3 = run(
        fake_full(&[1, 2, 3]),
        request(&project_dir, &source, RunOptions::default()),
        &config,
        &CancellationToken::new(),
        &mut RunReport::default(),
    )
    .await
    .expect("run 3");
    assert_eq!(o3, RunOutcome::Completed);
    assert!(tmp.path().join("movie.my.srt").is_file());

    // Origins are both `user`.
    let report = status(&project_dir, &config).await.expect("status");
    assert_eq!(
        origin_of(&report, Stage::AnalysisReview),
        Some(ApprovalOrigin::User)
    );
    assert_eq!(
        origin_of(&report, Stage::TranslationReview),
        Some(ApprovalOrigin::User)
    );
}

// ---------------------------------------------------------------------------
// Failed-batch guardrail
// ---------------------------------------------------------------------------

// AC: CLI.md §4 — `--auto-approve all` is refused (exit 3) at the translation gate
// when a batch failed and `--allow-incomplete` is absent; re-running WITH
// `--allow-incomplete` auto-approves, exports using the source for the failed cue,
// and exits 4 (completed with failed units).
#[tokio::test]
async fn auto_approve_translation_refused_on_failed_batch() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT1).expect("write source");
    let project_dir = project_dir_for_source(&source);
    let config = Config::default();

    // Analysis valid; every translation response is unparseable → the batch fails.
    let failing = || {
        Fake::new()
            .push_response(resp(ANALYSIS_DOC))
            .always(|| Ok(resp("not json at all")))
    };

    // Without --allow-incomplete: refused at the translation gate (exit 3).
    let refused = run(
        failing(),
        request(&project_dir, &source, auto_all()),
        &config,
        &CancellationToken::new(),
        &mut RunReport::default(),
    )
    .await
    .expect("run refused");
    match &refused {
        RunOutcome::StoppedAtGate { stage, next_action } => {
            assert_eq!(*stage, Stage::TranslationReview);
            assert!(
                next_action.contains("allow-incomplete"),
                "message names --allow-incomplete, got: {next_action}"
            );
        }
        other => panic!("expected StoppedAtGate(TranslationReview), got {other:?}"),
    }
    assert_eq!(run_exit_code(false, &refused), 3);
    assert!(!tmp.path().join("movie.my.srt").is_file(), "no export yet");

    // With --allow-incomplete: auto-approves and exports with the source text (exit 4).
    let options = RunOptions {
        auto_approve: Some(AutoApproveGate::All),
        allow_incomplete: true,
        dry_run: false,
        ..RunOptions::default()
    };
    let done = run(
        failing(),
        request(&project_dir, &source, options),
        &config,
        &CancellationToken::new(),
        &mut RunReport::default(),
    )
    .await
    .expect("run allow-incomplete");
    assert!(
        matches!(done, RunOutcome::CompletedWithFailures { failed } if failed == 1),
        "got {done:?}"
    );
    assert_eq!(run_exit_code(false, &done), 4);
    assert!(tmp.path().join("movie.my.srt").is_file(), "exported anyway");
}

// ---------------------------------------------------------------------------
// Zero-translatable edge
// ---------------------------------------------------------------------------

// AC: CLI.md §6 — a project whose cues are all non-translatable completes under
// `--auto-approve all` (translation trivially completes, its review is auto-approved)
// and writes the source cues without hanging at a gate. exit 0.
#[tokio::test]
async fn zero_translatable_project_completes() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT_NONTRANSLATABLE).expect("write source");
    let project_dir = project_dir_for_source(&source);

    let outcome = run(
        // Analysis still runs (over an empty transcript); no translation calls.
        fake_full(&[]),
        request(&project_dir, &source, auto_all()),
        &Config::default(),
        &CancellationToken::new(),
        &mut RunReport::default(),
    )
    .await
    .expect("zero-translatable run");

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(
        stage(&project_dir, Stage::Output).await,
        StageStatus::Completed
    );

    let output = tmp.path().join("movie.my.srt");
    assert!(output.is_file());
    let text = fs::read_to_string(&output).expect("read output");
    let doc = parse_document(&text, myansub::domain::SubtitleFormat::Srt).expect("reparse");
    assert_eq!(doc.cues.len(), 2, "both source cues are written");
    assert!(text.contains("123") && text.contains("456"));
}

// ---------------------------------------------------------------------------
// Resume (crash mid-run → re-run, no duplicate LLM calls)
// ---------------------------------------------------------------------------

// AC: CLI.md §6 — cancelling mid-translation yields exit 130; re-running the identical
// command resumes and sends no call for the already-done batch (the second Fake's call
// log shows only the remaining batches). exit 0.
#[tokio::test]
async fn resume_after_cancel_sends_no_duplicate_calls() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT3).expect("write source");
    let project_dir = project_dir_for_source(&source);

    // One cue per batch so the run has three distinct translation calls.
    let mut config = Config::default();
    config.translation.batch_size = 1;
    config.translation.concurrency = 1;
    config.translation.warm_cache_first = false;

    // First run: analysis (call 1) + batch 1 (call 2), then the token is cancelled.
    let token = CancellationToken::new();
    let fake1 = fake_full(&[1, 2, 3]).cancel_after(token.clone(), 2);
    let interrupted = run(
        fake1.clone(),
        request(&project_dir, &source, auto_all()),
        &config,
        &token,
        &mut RunReport::default(),
    )
    .await
    .expect("interrupted run");
    assert_eq!(interrupted, RunOutcome::Interrupted);
    assert_eq!(run_exit_code(token.is_cancelled(), &interrupted), 130);
    assert_eq!(fake1.call_count(), 2, "analysis + one batch before cancel");

    // Re-run with a fresh token and a fresh Fake (so its call log is only this run).
    // Analysis is NothingToDo on resume, so this Fake only scripts translation lines.
    let token2 = CancellationToken::new();
    let fake2 = Fake::new().always(|| Ok(resp(&lines(&[1, 2, 3]))));
    let outcome = run(
        fake2.clone(),
        request(&project_dir, &source, auto_all()),
        &config,
        &token2,
        &mut RunReport::default(),
    )
    .await
    .expect("resume run");

    assert_eq!(outcome, RunOutcome::Completed);
    // Analysis is NothingToDo (0 calls); batch 1 is already done (skipped); only the
    // two remaining batches are sent — no duplicate for the completed batch.
    assert_eq!(
        fake2.call_count(),
        2,
        "resume sends only the two remaining batches"
    );
    assert!(tmp.path().join("movie.my.srt").is_file());
}

// ---------------------------------------------------------------------------
// --dry-run
// ---------------------------------------------------------------------------

// AC: FR-9 / CLI.md §6 — `run --dry-run` estimates the stages, writes nothing (no
// analysis version, no output file), and returns a DryRun outcome (exit 0).
#[tokio::test]
async fn dry_run_writes_nothing() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT3).expect("write source");
    let project_dir = project_dir_for_source(&source);

    let options = RunOptions {
        auto_approve: Some(AutoApproveGate::All),
        allow_incomplete: false,
        dry_run: true,
        ..RunOptions::default()
    };
    // The dry-run path builds its own `Cached<DryRun>` and never calls this Fake.
    let outcome = run(
        fake_full(&[1, 2, 3]),
        request(&project_dir, &source, options),
        &Config::default(),
        &CancellationToken::new(),
        &mut RunReport::default(),
    )
    .await
    .expect("dry run");

    assert_eq!(outcome, RunOutcome::DryRun);
    assert_eq!(run_exit_code(false, &outcome), 0);

    // Nothing was written: no analysis version and no output file.
    let current = open(&project_dir)
        .await
        .call(|conn| current_analysis(conn))
        .await
        .expect("current analysis");
    assert!(current.is_none(), "dry run stored no analysis version");
    assert!(!tmp.path().join("movie.my.srt").is_file(), "no output file");
}

// AC: FR-9 / CLI.md §6 — `run --dry-run` surfaces a token + cost estimate (the cost
// preview) through the report out-param, with a priced model yielding a known, non-zero
// cost. The estimate is the whole purpose of a dry run before an unattended run.
#[tokio::test]
async fn dry_run_reports_estimate() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT3).expect("write source");
    let project_dir = project_dir_for_source(&source);

    // Price the default analysis model so the estimate is a known, non-zero cost.
    let mut config = Config::default();
    config.pricing.insert(
        config.llm.models.analysis.clone(),
        PriceEntry {
            input: 2.0,
            cached_input: 1.0,
            output: 4.0,
            storage_per_hour: 0.0,
            flex_multiplier: 0.5,
            long_prompt_threshold: None,
            long_input: None,
            long_output: None,
        },
    );

    let options = RunOptions {
        auto_approve: Some(AutoApproveGate::All),
        allow_incomplete: false,
        dry_run: true,
        ..RunOptions::default()
    };
    let mut report = RunReport::default();
    let outcome = run(
        fake_full(&[1, 2, 3]),
        request(&project_dir, &source, options),
        &config,
        &CancellationToken::new(),
        &mut report,
    )
    .await
    .expect("dry run");

    assert_eq!(outcome, RunOutcome::DryRun);
    let estimate = report.dry_run.expect("dry-run estimate is produced");
    assert!(estimate.prompt_tokens > 0, "input tokens estimated");
    assert!(estimate.output_tokens > 0, "output tokens estimated");
    match estimate.cost {
        Cost::Known(usd) => assert!(usd > 0.0, "priced model yields a non-zero cost, got {usd}"),
        Cost::Unknown => panic!("priced model should yield a known cost"),
    }
}

// AC: CLI.md §4 — on `--auto-approve translation|all`, suspect cues do NOT block the
// gate but their count is surfaced. A cue with a SOFT flag (too_long, via a 3-line
// target) is produced; the run auto-approves the translation gate, reports a suspect
// count of 1, and still exports (exit 0).
#[tokio::test]
async fn auto_approve_reports_suspect_count() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT1).expect("write source");
    let project_dir = project_dir_for_source(&source);

    // A 3-line Burmese target trips the soft `too_long` flag (max_lines = 2) without
    // tripping any hard flag (it is valid Unicode Burmese, no placeholders).
    let three_lines = serde_json::json!({"lines":[{"id":1,"t":"မ\nမ\nမ"}]}).to_string();
    let fake = Fake::new()
        .push_response(resp(ANALYSIS_DOC))
        .always(move || Ok(resp(&three_lines)));

    let mut report = RunReport::default();
    let outcome = run(
        fake,
        request(&project_dir, &source, auto_all()),
        &Config::default(),
        &CancellationToken::new(),
        &mut report,
    )
    .await
    .expect("suspect run");

    assert_eq!(outcome, RunOutcome::Completed);
    assert_eq!(run_exit_code(false, &outcome), 0);
    assert_eq!(
        report.suspect_auto_approved,
        Some(1),
        "the one flagged cue is reported as suspect"
    );
    assert!(
        tmp.path().join("movie.my.srt").is_file(),
        "suspect cues do not block the export"
    );
}

// AC: ADR-031 / CLI.md §7 — a pre-cancelled token short-circuits `run` to Interrupted
// (exit 130) even with a scripted Fake available. Guards the interrupt-wins rule
// (ARCHITECTURE §6 rule 6).
#[tokio::test]
async fn precancelled_token_is_interrupted() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT3).expect("write source");
    let project_dir = project_dir_for_source(&source);

    let token = CancellationToken::new();
    token.cancel();
    let outcome = run(
        fake_full(&[1, 2, 3]),
        request(&project_dir, &source, auto_all()),
        &Config::default(),
        &token,
        &mut RunReport::default(),
    )
    .await
    .expect("precancelled run");
    assert_eq!(outcome, RunOutcome::Interrupted);
    assert_eq!(run_exit_code(token.is_cancelled(), &outcome), 130);

    // A fatal guard: the capstone's PipelineError type is in scope (exit mapping).
    let _ = PipelineError::MissingApiKey.exit_code();
}

// ---------------------------------------------------------------------------
// M8-C: `run` cost-confirmation gate (docs/CLI.md §4/§5)
// ---------------------------------------------------------------------------

fn confirm_settings(confirm_above_usd: f64, interactive: bool, yes: bool) -> ConfirmSettings {
    ConfirmSettings {
        confirm_above_usd,
        interactive,
        yes,
    }
}

// AC: CLI.md §5 — `run` with an estimate above the threshold, no TTY, and no `--yes`
// exits 3 (naming `--yes`) and spends nothing: the transport factory is never invoked.
#[tokio::test]
async fn run_refuses_cost_without_tty_or_yes() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT3).expect("write source");
    let project_dir = project_dir_for_source(&source);

    let fake = fake_full(&[1, 2, 3]);
    let token = CancellationToken::new();
    let err = run_confirmed(
        || Ok::<_, PipelineError>(fake.clone()),
        request(&project_dir, &source, auto_all()),
        &Config::default(),
        &token,
        &mut RunReport::default(),
        confirm_settings(0.0, false, false),
    )
    .await
    .expect_err("refusal");

    assert!(matches!(err, PipelineError::ConfirmationNeeded { .. }));
    assert_eq!(err.exit_code(), 3);
    assert_eq!(fake.call_count(), 0, "a refused run spends nothing");
    // The pipeline never advanced past input: no analysis was produced.
    assert_eq!(
        stage(&project_dir, Stage::Analysis).await,
        StageStatus::NotStarted
    );
}

// AC: CLI.md §4/§5 — `--auto-approve` does not imply `--yes`, but `--yes` clears the
// cost gate and the whole pipeline runs (the transport is called).
#[tokio::test]
async fn run_with_yes_proceeds_through_pipeline() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT3).expect("write source");
    let project_dir = project_dir_for_source(&source);

    let fake = fake_full(&[1, 2, 3]);
    let token = CancellationToken::new();
    let confirmed = run_confirmed(
        || Ok::<_, PipelineError>(fake.clone()),
        request(&project_dir, &source, auto_all()),
        &Config::default(),
        &token,
        &mut RunReport::default(),
        confirm_settings(0.0, false, true),
    )
    .await
    .expect("confirmed run");

    assert_eq!(confirmed, ConfirmedRun::Ran(RunOutcome::Completed));
    assert!(fake.call_count() >= 1, "the pipeline sent LLM calls");
    assert_eq!(
        stage(&project_dir, Stage::Output).await,
        StageStatus::Completed
    );
}

// AC: CLI.md §4/§5 — the SECOND (translation) gate: a fresh `run --auto-approve all`
// whose analysis estimate is UNDER the threshold but whose translation is NOT (the
// translation model is unpriced → unknown → needs confirmation), non-TTY, no `--yes`,
// refuses at the translation gate (exit 3) AFTER analysis ran — translation unspent.
#[tokio::test]
async fn run_confirms_translation_gate_after_analysis() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT3).expect("write source");
    let project_dir = project_dir_for_source(&source);

    // Distinct models: price ONLY the analysis model, so the analysis pre-flight is a
    // known, tiny cost (under the high threshold) while the translation estimate is
    // unknown → the translation gate asks.
    let mut config = Config::default();
    config.llm.models.analysis = "model-a".to_string();
    config.llm.models.translation = "model-b".to_string();
    config.pricing.insert(
        "model-a".to_string(),
        PriceEntry {
            input: 0.1,
            cached_input: 0.025,
            output: 0.4,
            storage_per_hour: 0.0,
            flex_multiplier: 0.5,
            long_prompt_threshold: None,
            long_input: None,
            long_output: None,
        },
    );

    let fake = fake_full(&[1, 2, 3]);
    let token = CancellationToken::new();
    let err = run_confirmed(
        || Ok::<_, PipelineError>(fake.clone()),
        request(&project_dir, &source, auto_all()),
        &config,
        &token,
        &mut RunReport::default(),
        confirm_settings(1000.0, false, false),
    )
    .await
    .expect_err("refused at the translation gate");

    match &err {
        PipelineError::ConfirmationNeeded { flag } => assert_eq!(flag, "--yes"),
        other => panic!("expected ConfirmationNeeded, got {other:?}"),
    }
    assert_eq!(err.exit_code(), 3);
    assert_eq!(
        fake.call_count(),
        1,
        "analysis ran (1 call); translation was never sent"
    );
    // Analysis was created and auto-approved, but translation never started.
    assert_eq!(
        stage(&project_dir, Stage::AnalysisReview).await,
        StageStatus::Approved
    );
    assert_eq!(
        stage(&project_dir, Stage::Translation).await,
        StageStatus::NotStarted
    );
}

// AC: CLI.md §5 — a `--dry-run` run bypasses the gate entirely (dry-run never prompts):
// even with threshold 0, non-TTY, no `--yes`, it returns Ran(DryRun), not a refusal.
#[tokio::test]
async fn run_confirmed_dry_run_bypasses_gate() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT3).expect("write source");
    let project_dir = project_dir_for_source(&source);

    let options = RunOptions {
        auto_approve: Some(AutoApproveGate::All),
        dry_run: true,
        ..RunOptions::default()
    };
    let fake = fake_full(&[1, 2, 3]);
    let confirmed = run_confirmed(
        || Ok::<_, PipelineError>(fake.clone()),
        request(&project_dir, &source, options),
        &Config::default(),
        &CancellationToken::new(),
        &mut RunReport::default(),
        confirm_settings(0.0, false, false),
    )
    .await
    .expect("dry run bypasses the gate");

    assert_eq!(confirmed, ConfirmedRun::Ran(RunOutcome::DryRun));
    assert_eq!(
        fake.call_count(),
        0,
        "dry run builds its own DryRun transport"
    );
}
