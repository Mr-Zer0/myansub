//! Orchestrator tests: the stage gate table (M4-AC2), the `run` advance loop up
//! to the analysis boundary, resume guards, `status`, and the cancellation exit
//! mapping (`docs/ARCHITECTURE.md` §2/§4, `docs/CLI.md` §2/§6/§7). Every
//! filesystem case uses a `tempfile` directory and never touches the network
//! (`docs/TESTING.md` §1).

use std::fs;

use myansub::config::{Config, PriceEntry};
use myansub::error::ExitCode;
use myansub::llm::metered::{CallOutcome, LlmCallRecord};
use myansub::llm::pricing::Cost;
use myansub::pipeline::PipelineError;
use myansub::pipeline::orchestrator::{
    NextStep, ProjectState, RunOptions, RunOutcome, RunRequest, UsageTotals, can_run, next_step,
    run, run_exit_code, status,
};
use myansub::project::{PROJECT_DB_FILE, project_dir_for_source};
use myansub::store::repo_llm::insert_call;
use myansub::store::{Stage, StageStatus, Store};
use tempfile::tempdir;
use tokio_util::sync::CancellationToken;

const SRT: &str = "1\n00:00:01,000 --> 00:00:03,000\nGood morning, traveller.\n";

/// A state with the input stage completed and nothing else done.
fn input_done() -> ProjectState {
    ProjectState {
        input: StageStatus::Completed,
        ..ProjectState::empty()
    }
}

// ---------------------------------------------------------------------------
// M4-AC2: gate violations return exit code 3 with a helpful message
// ---------------------------------------------------------------------------

// AC: M4-AC2 — analysis before input is completed is refused (exit 3), naming the
// init command.
#[test]
fn gate_analysis_requires_input_completed() {
    let err = can_run(Stage::Analysis, &ProjectState::empty())
        .expect_err("analysis before input must be refused");
    assert_eq!(err.exit_code(), 3);
    assert!(
        matches!(err, PipelineError::GateViolation { .. }),
        "expected GateViolation, got {err:?}"
    );
    assert!(
        err.to_string().contains("input"),
        "message names the missing precondition, got: {err}"
    );
}

// AC: M4-AC2 — analysis review before analysis is completed is refused (exit 3),
// naming the analyze step (the §2 analysis_review row).
#[test]
fn gate_analysis_review_requires_analysis_completed() {
    // Only input is done, so analysis is NOT completed.
    let err = can_run(Stage::AnalysisReview, &input_done())
        .expect_err("analysis review before analysis completed must be refused");
    assert_eq!(err.exit_code(), 3);
    assert!(
        matches!(err, PipelineError::GateViolation { .. }),
        "expected GateViolation, got {err:?}"
    );
    assert!(
        err.to_string().contains("analyze"),
        "message names the analyze step, got: {err}"
    );
}

// AC: M4-AC2 — translation without an approved analysis version is refused
// (exit 3), naming `analysis approve`.
#[test]
fn gate_translation_requires_approved_analysis() {
    let state = ProjectState {
        analysis: StageStatus::Completed,
        analysis_approved: false,
        ..input_done()
    };
    let err = can_run(Stage::Translation, &state).expect_err("translation needs approved analysis");
    assert_eq!(err.exit_code(), 3);
    assert!(
        err.to_string().contains("analysis approve"),
        "message names `analysis approve`, got: {err}"
    );
}

// AC: M4-AC2 — translation review before translation is completed is refused
// (exit 3), naming `translate`.
#[test]
fn gate_translation_review_requires_translation_completed() {
    let state = ProjectState {
        analysis: StageStatus::Completed,
        analysis_approved: true,
        translation: StageStatus::NotStarted,
        ..input_done()
    };
    let err =
        can_run(Stage::TranslationReview, &state).expect_err("review needs translation completed");
    assert_eq!(err.exit_code(), 3);
    assert!(
        err.to_string().contains("translate"),
        "message names `translate`, got: {err}"
    );
}

// AC: M4-AC2 — translation review with a still-pending batch is refused (exit 3).
#[test]
fn gate_translation_review_refused_with_pending_batches() {
    let state = ProjectState {
        analysis: StageStatus::Completed,
        analysis_approved: true,
        translation: StageStatus::Completed,
        translation_pending: 1,
        ..input_done()
    };
    let err = can_run(Stage::TranslationReview, &state).expect_err("pending batch blocks review");
    assert_eq!(err.exit_code(), 3);
}

// AC: M4-AC2 — output before the translation review is approved is refused
// (exit 3), naming `review approve`.
#[test]
fn gate_output_requires_translation_review_approved() {
    let state = ProjectState {
        analysis: StageStatus::Completed,
        analysis_approved: true,
        translation: StageStatus::Completed,
        translation_review: StageStatus::Completed,
        ..input_done()
    };
    let err = can_run(Stage::Output, &state).expect_err("output needs review approved");
    assert_eq!(err.exit_code(), 3);
    assert!(
        err.to_string().contains("review approve"),
        "message names `review approve`, got: {err}"
    );
}

// Positive cases: input always runs; analysis runs once input is completed.
#[test]
fn gate_input_always_runnable_and_analysis_after_input() {
    assert!(can_run(Stage::Input, &ProjectState::empty()).is_ok());
    assert!(can_run(Stage::Analysis, &ProjectState::empty()).is_err());
    assert!(can_run(Stage::Analysis, &input_done()).is_ok());
}

// ---------------------------------------------------------------------------
// next_step planner
// ---------------------------------------------------------------------------

#[test]
fn next_step_walks_the_stage_order() {
    // Fresh project: input not completed yet.
    assert_eq!(
        next_step(&ProjectState::empty()),
        NextStep::Execute(Stage::Input)
    );

    // Input done → analysis is next.
    assert_eq!(next_step(&input_done()), NextStep::Execute(Stage::Analysis));

    // Analysis completed but unapproved → analysis-review gate.
    let awaiting_approval = ProjectState {
        analysis: StageStatus::Completed,
        ..input_done()
    };
    assert!(matches!(
        next_step(&awaiting_approval),
        NextStep::Approve(Stage::AnalysisReview, _)
    ));

    // Approved analysis → translation is next.
    let approved = ProjectState {
        analysis: StageStatus::Completed,
        analysis_approved: true,
        ..input_done()
    };
    assert_eq!(next_step(&approved), NextStep::Execute(Stage::Translation));

    // Translation completed, review not approved → translation-review gate.
    let translated = ProjectState {
        translation: StageStatus::Completed,
        ..approved
    };
    assert!(matches!(
        next_step(&translated),
        NextStep::Approve(Stage::TranslationReview, _)
    ));

    // Review approved, output pending → output is next.
    let reviewed = ProjectState {
        translation_review: StageStatus::Approved,
        ..translated
    };
    assert_eq!(next_step(&reviewed), NextStep::Execute(Stage::Output));

    // Everything done.
    let done = ProjectState {
        output: StageStatus::Completed,
        ..reviewed
    };
    assert_eq!(next_step(&done), NextStep::Done);
}

// ---------------------------------------------------------------------------
// run: up to the analysis boundary
// ---------------------------------------------------------------------------

async fn stage_status(project_dir: &std::path::Path, stage: Stage) -> StageStatus {
    let store = Store::open(&project_dir.join(PROJECT_DB_FILE)).expect("open store");
    store
        .call(move |conn| myansub::store::repo_project::get_stage(conn, stage))
        .await
        .expect("get stage")
        .expect("stage row present")
}

// run on a fresh FILE creates the project, completes input, and stops at the
// analysis boundary (exit 0), naming `analyze`.
#[tokio::test]
async fn run_fresh_file_creates_project_and_stops_at_analysis() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT).expect("write source");
    let project_dir = project_dir_for_source(&source);

    let token = CancellationToken::new();
    let outcome = run(
        RunRequest {
            project_dir: &project_dir,
            source_path: Some(&source),
            from_lang: Some("en"),
            encoding: None,
            output_format: None,
            output_path: None,
            options: RunOptions::default(),
        },
        &Config::default(),
        &token,
    )
    .await
    .expect("run fresh");

    match &outcome {
        RunOutcome::StoppedAtStage { stage, next_action } => {
            assert_eq!(*stage, Stage::Analysis);
            assert!(next_action.contains("analyze"), "got: {next_action}");
        }
        other => panic!("expected StoppedAtStage(Analysis), got {other:?}"),
    }
    assert_eq!(run_exit_code(token.is_cancelled(), &outcome), 0);
    assert!(project_dir.join(PROJECT_DB_FILE).is_file());
    assert_eq!(
        stage_status(&project_dir, Stage::Input).await,
        StageStatus::Completed
    );
}

// run on an existing input-completed project resumes (no FILE/--from) and stops at
// analysis again (nothing to do beyond input), exit 0.
#[tokio::test]
async fn run_resumes_existing_project_and_stops_at_analysis() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT).expect("write source");
    let project_dir = project_dir_for_source(&source);
    let token = CancellationToken::new();

    run(
        RunRequest {
            project_dir: &project_dir,
            source_path: Some(&source),
            from_lang: Some("en"),
            encoding: None,
            output_format: None,
            output_path: None,
            options: RunOptions::default(),
        },
        &Config::default(),
        &token,
    )
    .await
    .expect("initial run");

    // Resume with just --project (no FILE, no --from): must hash-check the stored
    // source and stop at analysis.
    let outcome = run(
        RunRequest {
            project_dir: &project_dir,
            source_path: None,
            from_lang: None,
            encoding: None,
            output_format: None,
            output_path: None,
            options: RunOptions::default(),
        },
        &Config::default(),
        &token,
    )
    .await
    .expect("resume run");

    assert!(matches!(
        outcome,
        RunOutcome::StoppedAtStage {
            stage: Stage::Analysis,
            ..
        }
    ));
    assert_eq!(run_exit_code(token.is_cancelled(), &outcome), 0);
}

// run with a --from that disagrees with the stored source language → exit 3.
#[tokio::test]
async fn run_from_disagreement_is_exit_3() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT).expect("write source");
    let project_dir = project_dir_for_source(&source);
    let token = CancellationToken::new();

    run(
        RunRequest {
            project_dir: &project_dir,
            source_path: Some(&source),
            from_lang: Some("ja"),
            encoding: None,
            output_format: None,
            output_path: None,
            options: RunOptions::default(),
        },
        &Config::default(),
        &token,
    )
    .await
    .expect("initial run with --from ja");

    let err = run(
        RunRequest {
            project_dir: &project_dir,
            source_path: Some(&source),
            from_lang: Some("en"),
            encoding: None,
            output_format: None,
            output_path: None,
            options: RunOptions::default(),
        },
        &Config::default(),
        &token,
    )
    .await
    .expect_err("disagreeing --from must be refused");

    assert_eq!(err.exit_code(), 3);
    assert!(
        matches!(err, PipelineError::FromDisagreement { .. }),
        "got {err:?}"
    );
}

// run on a changed source → exit 3 pointing to `init --force`.
#[tokio::test]
async fn run_changed_source_is_exit_3_pointing_to_force() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT).expect("write source");
    let project_dir = project_dir_for_source(&source);
    let token = CancellationToken::new();

    run(
        RunRequest {
            project_dir: &project_dir,
            source_path: Some(&source),
            from_lang: Some("en"),
            encoding: None,
            output_format: None,
            output_path: None,
            options: RunOptions::default(),
        },
        &Config::default(),
        &token,
    )
    .await
    .expect("initial run");

    fs::write(
        &source,
        format!("{SRT}2\n00:00:04,000 --> 00:00:05,000\nNew line.\n"),
    )
    .expect("modify source");

    let err = run(
        RunRequest {
            project_dir: &project_dir,
            source_path: Some(&source),
            from_lang: None,
            encoding: None,
            output_format: None,
            output_path: None,
            options: RunOptions::default(),
        },
        &Config::default(),
        &token,
    )
    .await
    .expect_err("changed source must be refused");

    assert_eq!(err.exit_code(), 3);
    assert!(
        matches!(err, PipelineError::SourceChanged { .. }),
        "got {err:?}"
    );
    assert!(err.to_string().contains("init --force"), "got: {err}");
}

// A project.db with no project row + no --from is treated as uninitialized → a
// helpful exit 3, never a panic (M4-B carry-over).
#[tokio::test]
async fn run_empty_db_without_from_is_helpful_exit_3() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT).expect("write source");
    let project_dir = project_dir_for_source(&source);
    fs::create_dir_all(&project_dir).expect("mk project dir");
    // An empty file: a valid empty SQLite db with no project row.
    fs::write(project_dir.join(PROJECT_DB_FILE), b"").expect("touch project.db");

    let token = CancellationToken::new();
    let err = run(
        RunRequest {
            project_dir: &project_dir,
            source_path: Some(&source),
            from_lang: None,
            encoding: None,
            output_format: None,
            output_path: None,
            options: RunOptions::default(),
        },
        &Config::default(),
        &token,
    )
    .await
    .expect_err("uninitialized db without --from must be refused");

    assert_eq!(err.exit_code(), 3);
    assert!(
        matches!(err, PipelineError::MissingFromLanguage),
        "got {err:?}"
    );
}

// A pre-cancelled token yields RunOutcome::Interrupted and exit 130.
#[tokio::test]
async fn run_with_cancelled_token_is_interrupted() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT).expect("write source");
    let project_dir = project_dir_for_source(&source);

    let token = CancellationToken::new();
    token.cancel();

    let outcome = run(
        RunRequest {
            project_dir: &project_dir,
            source_path: Some(&source),
            from_lang: Some("en"),
            encoding: None,
            output_format: None,
            output_path: None,
            options: RunOptions::default(),
        },
        &Config::default(),
        &token,
    )
    .await
    .expect("run with cancelled token");

    assert_eq!(outcome, RunOutcome::Interrupted);
    assert_eq!(run_exit_code(token.is_cancelled(), &outcome), 130);
}

// ---------------------------------------------------------------------------
// cancellation exit mapping
// ---------------------------------------------------------------------------

// AC: CLI.md §7 — a cancelled run maps to 130 regardless of the outcome; an
// uninterrupted stop-at-boundary (M4 transitional analysis stage) maps to 0, and a
// completed pipeline maps to 0.
#[test]
fn run_exit_code_prefers_interrupt_on_cancellation() {
    let at_boundary = RunOutcome::StoppedAtStage {
        stage: Stage::Analysis,
        next_action: "run `myansub analyze`".to_string(),
    };
    assert_eq!(run_exit_code(true, &at_boundary), 130);
    assert_eq!(run_exit_code(false, &at_boundary), 0);
    assert_eq!(run_exit_code(false, &RunOutcome::Interrupted), 130);
    assert_eq!(run_exit_code(false, &RunOutcome::Completed), 0);
}

// CLI.md §7 (ADR-xx) — `run` stopping at an unapproved review gate is a
// precondition/action-needed state → exit 3; cancellation still wins (130).
#[test]
fn run_exit_code_gate_stop_is_precondition() {
    let gate = RunOutcome::StoppedAtGate {
        stage: Stage::AnalysisReview,
        next_action: "run `myansub analysis approve`".to_string(),
    };
    assert_eq!(run_exit_code(false, &gate), 3);
    assert_eq!(run_exit_code(true, &gate), 130);
}

// ---------------------------------------------------------------------------
// status
// ---------------------------------------------------------------------------

// status on an input-completed project shows input=completed, others not_started,
// zeroed usage, and a known $0 cost — without error.
#[tokio::test]
async fn status_on_input_completed_project() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT).expect("write source");
    let project_dir = project_dir_for_source(&source);
    let token = CancellationToken::new();

    run(
        RunRequest {
            project_dir: &project_dir,
            source_path: Some(&source),
            from_lang: Some("en"),
            encoding: None,
            output_format: None,
            output_path: None,
            options: RunOptions::default(),
        },
        &Config::default(),
        &token,
    )
    .await
    .expect("run to initialize");

    let report = status(&project_dir, &Config::default())
        .await
        .expect("status succeeds");

    let input = report
        .stages
        .iter()
        .find(|s| s.stage == Stage::Input)
        .expect("input stage line");
    assert_eq!(input.status, StageStatus::Completed);

    let analysis = report
        .stages
        .iter()
        .find(|s| s.stage == Stage::Analysis)
        .expect("analysis stage line");
    assert_eq!(analysis.status, StageStatus::NotStarted);

    assert_eq!(report.usage, Default::default());
    assert_eq!(report.analysis_chunks, Default::default());
    assert_eq!(report.translation_batches, Default::default());
    assert!(report.flag_counts.is_empty());
    assert!(report.unknown_cost_models.is_empty());
    assert!(!report.source_changed);
}

// status aggregates llm_calls usage across models, prices the model that has a
// price entry, and lists the unpriced model as unknown (FR-9/FR-10, LLM.md §9).
#[tokio::test]
async fn status_aggregates_usage_and_prices_known_models() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT).expect("write source");
    let project_dir = project_dir_for_source(&source);
    let token = CancellationToken::new();

    run(
        RunRequest {
            project_dir: &project_dir,
            source_path: Some(&source),
            from_lang: Some("en"),
            encoding: None,
            output_format: None,
            output_path: None,
            options: RunOptions::default(),
        },
        &Config::default(),
        &token,
    )
    .await
    .expect("run to initialize");

    // Insert two llm_calls rows: one for a priced model, one for an unpriced one.
    let store = Store::open(&project_dir.join(PROJECT_DB_FILE)).expect("open store");
    store
        .call(|conn| {
            insert_call(
                conn,
                &LlmCallRecord {
                    stage: "analysis".to_string(),
                    unit: None,
                    model: "priced-model".to_string(),
                    service_tier: Some("standard".to_string()),
                    prompt_tokens: Some(1000),
                    cached_tokens: Some(200),
                    output_tokens: Some(500),
                    thinking_tokens: Some(100),
                    latency_ms: Some(10),
                    cache_hit_local: false,
                    dry_run: false,
                    outcome: CallOutcome::Ok,
                },
            )?;
            insert_call(
                conn,
                &LlmCallRecord {
                    stage: "translation".to_string(),
                    unit: Some(1),
                    model: "mystery-model".to_string(),
                    service_tier: Some("standard".to_string()),
                    prompt_tokens: Some(300),
                    cached_tokens: Some(0),
                    output_tokens: Some(100),
                    thinking_tokens: Some(0),
                    latency_ms: Some(10),
                    cache_hit_local: false,
                    dry_run: false,
                    outcome: CallOutcome::Ok,
                },
            )?;
            Ok(())
        })
        .await
        .expect("insert llm_calls");

    // Price only `priced-model`.
    let mut config = Config::default();
    config.pricing.insert(
        "priced-model".to_string(),
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

    let report = status(&project_dir, &config)
        .await
        .expect("status succeeds");

    // Token sums across both models.
    assert_eq!(
        report.usage,
        UsageTotals {
            prompt_tokens: 1300,
            cached_tokens: 200,
            output_tokens: 600,
            thinking_tokens: 100,
        }
    );

    // Cost = priced model only. fresh=800*2 + cached=200*1 + output=(500+100)*4
    //       = 1600 + 200 + 2400 = 4200 per 1M → 0.0042 (standard tier, x1).
    match report.cost {
        Cost::Known(usd) => assert!((usd - 0.0042).abs() < 1e-9, "expected 0.0042, got {usd}"),
        Cost::Unknown => panic!("priced model should yield a known cost"),
    }

    assert_eq!(
        report.unknown_cost_models,
        vec!["mystery-model".to_string()]
    );
}

// status on a project.db with no project row is a helpful error, not a panic.
#[tokio::test]
async fn status_on_empty_db_is_uninitialized_error() {
    let tmp = tempdir().expect("tempdir");
    let project_dir = tmp.path().join("movie.srt.myansub");
    fs::create_dir_all(&project_dir).expect("mk project dir");
    fs::write(project_dir.join(PROJECT_DB_FILE), b"").expect("touch project.db");

    let err = status(&project_dir, &Config::default())
        .await
        .expect_err("empty db is uninitialized");

    assert_eq!(err.exit_code(), 3);
    assert!(
        matches!(err, PipelineError::Uninitialized { .. }),
        "got {err:?}"
    );
}

// status on a directory with no project.db at all is also a clean error.
#[tokio::test]
async fn status_on_missing_db_is_uninitialized_error() {
    let tmp = tempdir().expect("tempdir");
    let err = status(tmp.path(), &Config::default())
        .await
        .expect_err("missing db is uninitialized");
    assert!(
        matches!(err, PipelineError::Uninitialized { .. }),
        "got {err:?}"
    );
}
