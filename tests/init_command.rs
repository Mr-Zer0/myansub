//! `init` command tests (`docs/CLI.md` §5/§6, M4-AC3). All tests use `tempfile`
//! directories and never touch the network (`docs/TESTING.md` §1).

use std::fs;
use std::path::Path;

use myansub::pipeline::PipelineError;
use myansub::pipeline::input::{InitOutcome, ReinitAction, decide_reinit, run_init};
use myansub::project::project_dir_for_source;
use myansub::store::Store;
use tempfile::tempdir;

const SRT: &str = "1\n00:00:01,000 --> 00:00:03,000\nGood morning, traveller.\n";

async fn cue_count(project_dir: &Path) -> usize {
    let store = Store::open(&project_dir.join("project.db")).expect("open store");
    store
        .call(|conn| myansub::store::load_cues(conn))
        .await
        .expect("load cues")
        .len()
}

#[test]
fn decide_reinit_covers_every_branch() {
    // Unchanged source → nothing to do, regardless of flags.
    assert_eq!(
        decide_reinit(false, true, false, true),
        ReinitAction::AlreadyInitialized
    );
    // Changed source, no --force → refuse (exit 3).
    assert_eq!(
        decide_reinit(true, false, true, true),
        ReinitAction::RefuseNeedsForce
    );
    // Changed source, --force, --yes → proceed.
    assert_eq!(
        decide_reinit(true, true, false, true),
        ReinitAction::Proceed
    );
    // Changed source, --force, interactive TTY, no --yes → prompt.
    assert_eq!(
        decide_reinit(true, true, true, false),
        ReinitAction::Confirm
    );
}

// AC: CLI.md §5 — a destructive re-init on a non-TTY without --yes refuses,
// naming the flag needed (exit 3).
#[test]
fn decide_reinit_refuses_without_tty_or_yes() {
    assert_eq!(
        decide_reinit(true, true, false, false),
        ReinitAction::ConfirmationUnavailable
    );
}

#[tokio::test]
async fn fresh_init_creates_project_and_stores_cues() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT).expect("write source");
    let project_dir = project_dir_for_source(&source);

    let outcome = run_init(
        &project_dir,
        &source,
        "en",
        None,
        false,
        false,
        false,
        None,
        None,
    )
    .await
    .expect("fresh init");

    assert!(matches!(outcome, InitOutcome::Initialized(_)));
    assert!(project_dir.join("project.db").is_file());
    assert_eq!(cue_count(&project_dir).await, 1);
}

#[tokio::test]
async fn second_init_on_unchanged_source_is_nothing_to_do() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT).expect("write source");
    let project_dir = project_dir_for_source(&source);

    run_init(
        &project_dir,
        &source,
        "en",
        None,
        false,
        false,
        false,
        None,
        None,
    )
    .await
    .expect("first init");
    let outcome = run_init(
        &project_dir,
        &source,
        "en",
        None,
        false,
        false,
        false,
        None,
        None,
    )
    .await
    .expect("second init");

    assert_eq!(outcome, InitOutcome::AlreadyInitialized);
}

// AC: M4-AC3 — init on a CHANGED source refuses without --force (exit 3,
// pointing to `init --force`).
#[tokio::test]
async fn init_on_changed_source_without_force_refuses() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT).expect("write source");
    let project_dir = project_dir_for_source(&source);

    run_init(
        &project_dir,
        &source,
        "en",
        None,
        false,
        false,
        false,
        None,
        None,
    )
    .await
    .expect("first init");
    fs::write(
        &source,
        format!("{SRT}\n2\n00:00:04,000 --> 00:00:05,000\nNew line.\n"),
    )
    .expect("modify source");

    let err = run_init(
        &project_dir,
        &source,
        "en",
        None,
        false,
        false,
        false,
        None,
        None,
    )
    .await
    .expect_err("changed source refused without --force");

    match err {
        PipelineError::SourceChanged { .. } => {
            assert!(
                err.to_string().contains("init --force"),
                "message points to --force, got: {err}"
            );
        }
        other => panic!("expected SourceChanged, got {other:?}"),
    }
}

// AC: CLI.md §5 / M4-AC3 — a destructive re-init (changed source + --force) on a
// non-TTY without --yes surfaces ConfirmationNeeded naming `--yes` (NOT
// `--force`), mapping to exit 3. Guards against mis-naming the flag.
#[tokio::test]
async fn init_force_changed_source_non_tty_without_yes_needs_confirmation() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT).expect("write source");
    let project_dir = project_dir_for_source(&source);

    run_init(
        &project_dir,
        &source,
        "en",
        None,
        false,
        false,
        false,
        None,
        None,
    )
    .await
    .expect("first init");
    fs::write(
        &source,
        format!("{SRT}\n2\n00:00:04,000 --> 00:00:05,000\nNew line.\n"),
    )
    .expect("modify source");

    // force=true, interactive=false (non-TTY), yes=false.
    let err = run_init(
        &project_dir,
        &source,
        "en",
        None,
        true,
        false,
        false,
        None,
        None,
    )
    .await
    .expect_err("confirmation cannot be obtained");

    match err {
        PipelineError::ConfirmationNeeded { flag } => {
            assert_eq!(flag, "--yes", "must name --yes, not --force");
        }
        other => panic!("expected ConfirmationNeeded, got {other:?}"),
    }
    // The error message names the flag the user needs (CLI.md §5).
    let rendered = run_init(
        &project_dir,
        &source,
        "en",
        None,
        true,
        false,
        false,
        None,
        None,
    )
    .await
    .expect_err("confirmation cannot be obtained")
    .to_string();
    assert!(
        rendered.contains("--yes"),
        "message names --yes, got: {rendered}"
    );
}

// AC: M4-AC3 — with --force (and --yes to satisfy the confirmation) the project
// re-initializes and the new content is stored.
#[tokio::test]
async fn init_force_yes_reinitializes_with_new_content() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT).expect("write source");
    let project_dir = project_dir_for_source(&source);

    run_init(
        &project_dir,
        &source,
        "en",
        None,
        false,
        false,
        false,
        None,
        None,
    )
    .await
    .expect("first init");
    assert_eq!(cue_count(&project_dir).await, 1);

    fs::write(
        &source,
        format!("{SRT}\n2\n00:00:04,000 --> 00:00:05,000\nNew line.\n"),
    )
    .expect("modify source");

    let outcome = run_init(
        &project_dir,
        &source,
        "en",
        None,
        true,
        false,
        true,
        None,
        None,
    )
    .await
    .expect("re-init with --force --yes");

    assert!(matches!(outcome, InitOutcome::ReInitialized(_)));
    assert_eq!(cue_count(&project_dir).await, 2, "new content stored");
}
