//! Input-stage tests (`docs/REQUIREMENTS.md` FR-1, `docs/DATA_MODEL.md` §2).
//! All tests use `tempfile` directories and never touch the network
//! (`docs/TESTING.md` §1).

use std::fs;
use std::path::Path;

use myansub::pipeline::PipelineError;
use myansub::pipeline::input::{check_source_unchanged, run_input, run_input_with_fault};
use myansub::store::{Stage, StageStatus, Store, StoreError};
use tempfile::tempdir;

const SRT: &str = "1\n00:00:01,000 --> 00:00:03,000\nGood morning, traveller.\n\n\
                   2\n00:00:04,500 --> 00:00:06,750\nWhere does this road lead?\n";

async fn open_store(dir: &Path) -> Store {
    Store::open(&dir.join("project.db")).expect("open store")
}

#[tokio::test]
async fn input_stage_stores_project_row_fields() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT).expect("write source");
    let store = open_store(tmp.path()).await;

    run_input(&store, &source, "en", None, Some("srt"), None, None)
        .await
        .expect("input stage runs");

    let project = store
        .call(|conn| myansub::store::repo_project::get_project(conn))
        .await
        .expect("query project")
        .expect("project row present");

    assert_eq!(project.source_format, "srt");
    assert_eq!(project.source_lang, "en");
    assert_eq!(project.target_lang, "my");
    assert!(project.source_hash.starts_with("sha256:"), "hash prefix");
    assert!(!project.source_encoding.is_empty());
    assert_eq!(project.source_path, source.to_string_lossy());
    // A clean UTF-8 SRT needs no statistical detector, so no confidence is
    // recorded (docs: `encoding_rs`/`chardetng` deterministic path).
    assert_eq!(project.encoding_confidence, None);
    // SRT carries no preserved headers/styles.
    assert_eq!(project.format_meta, None);
}

// Strengthens coverage for the ASS path: an ASS source records format_meta
// (script info + styles) so same-format output can reproduce them later.
#[tokio::test]
async fn input_stage_records_format_meta_for_ass_source() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.ass");
    fs::write(&source, include_bytes!("fixtures/formats/sample.ass")).expect("write ass source");
    let store = open_store(tmp.path()).await;

    run_input(&store, &source, "en", None, Some("ass"), None, None)
        .await
        .expect("input stage runs on ass");

    let project = store
        .call(|conn| myansub::store::repo_project::get_project(conn))
        .await
        .expect("query project")
        .expect("project row present");

    assert_eq!(project.source_format, "ass");
    let meta_json = project.format_meta.expect("ass records format_meta");
    let meta: myansub::domain::FormatMeta =
        serde_json::from_str(&meta_json).expect("format_meta is valid JSON");
    assert!(
        meta.script_info.is_some() || meta.styles.is_some(),
        "ass format_meta should carry script info or styles"
    );
}

#[tokio::test]
async fn input_stage_round_trips_cues_and_marks_stage_completed() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT).expect("write source");
    let store = open_store(tmp.path()).await;

    run_input(&store, &source, "en", None, Some("srt"), None, None)
        .await
        .expect("input stage runs");

    let cues = store
        .call(|conn| myansub::store::load_cues(conn))
        .await
        .expect("load cues");
    assert_eq!(cues.len(), 2);
    assert_eq!(cues[0].idx, 1);
    assert_eq!(cues[0].text_raw, "Good morning, traveller.");
    assert_eq!(cues[1].start_ms, 4500);

    let stage = store
        .call(|conn| myansub::store::repo_project::get_stage(conn, Stage::Input))
        .await
        .expect("get stage");
    assert_eq!(stage, Some(StageStatus::Completed));
}

// AC: FR-1 — decoding with replacement characters warns and lists the affected
// cues, but the input stage still succeeds (it is a warning, not a failure).
#[tokio::test]
async fn input_stage_lists_cues_with_replacement_chars_but_succeeds() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    // 0xFC is a Shift_JIS lead byte; followed by a space (not a valid trail) it
    // decodes to U+FFFD while the surrounding ASCII survives. Forcing the wrong
    // encoding reproduces the mis-decode FR-1 describes.
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"1\n00:00:01,000 --> 00:00:03,000\ncaf\xFC scene\n");
    fs::write(&source, &bytes).expect("write source");
    let store = open_store(tmp.path()).await;

    let outcome = run_input(
        &store,
        &source,
        "en",
        Some("shift_jis"),
        Some("srt"),
        None,
        None,
    )
    .await
    .expect("input stage still succeeds on replacement chars");

    assert!(
        !outcome.replacement_cues.is_empty(),
        "affected cue list should be non-empty"
    );

    // The project was still initialized.
    let stage = store
        .call(|conn| myansub::store::repo_project::get_stage(conn, Stage::Input))
        .await
        .expect("get stage");
    assert_eq!(stage, Some(StageStatus::Completed));
}

// AC: ARCHITECTURE §6 rule 5 — the shared source-hash check passes when the
// source is unchanged.
#[tokio::test]
async fn check_source_unchanged_passes_when_source_is_unchanged() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT).expect("write source");
    let store = open_store(tmp.path()).await;
    run_input(&store, &source, "en", None, Some("srt"), None, None)
        .await
        .expect("input stage runs");

    check_source_unchanged(&store, &source)
        .await
        .expect("unchanged source passes");
}

// AC: ARCHITECTURE §6 rule 5 — the source-hash check fails (exit 3) when the
// source changed since init.
#[tokio::test]
async fn check_source_unchanged_errors_when_source_changed() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT).expect("write source");
    let store = open_store(tmp.path()).await;
    run_input(&store, &source, "en", None, Some("srt"), None, None)
        .await
        .expect("input stage runs");

    fs::write(
        &source,
        format!("{SRT}\n3\n00:00:08,000 --> 00:00:09,000\nNew line.\n"),
    )
    .expect("modify source");

    let err = check_source_unchanged(&store, &source)
        .await
        .expect_err("changed source is refused");
    assert!(matches!(err, PipelineError::SourceChanged { .. }));
}

// Contract pinned for M4-C callers: with no project row there is no recorded hash
// to compare against, so the check passes vacuously. Callers verify a project
// exists before trusting this (ARCHITECTURE §6 rule 5).
#[tokio::test]
async fn check_source_unchanged_is_ok_when_no_project_row_exists() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT).expect("write source");
    // Store opened but input stage never run → no project row.
    let store = open_store(tmp.path()).await;

    check_source_unchanged(&store, &source)
        .await
        .expect("vacuously unchanged with no project row");
}

// AC: ARCHITECTURE §6 rule 1 — the destructive re-init is atomic. A failure
// between clear_all_data and commit must roll the clear back, leaving the
// ORIGINAL project row and cues fully intact (never empty or partial).
#[tokio::test]
async fn failed_reinit_rolls_back_and_keeps_original_data() {
    let tmp = tempdir().expect("tempdir");
    let source = tmp.path().join("movie.srt");
    fs::write(&source, SRT).expect("write source");
    let store = open_store(tmp.path()).await;

    // Fresh init: 2 cues stored.
    run_input(&store, &source, "en", None, Some("srt"), None, None)
        .await
        .expect("fresh init");
    let original = store
        .call(|conn| myansub::store::repo_project::get_project(conn))
        .await
        .expect("query project")
        .expect("project row present");

    // Change the source so a successful re-init would store different content.
    fs::write(
        &source,
        format!("{SRT}\n3\n00:00:08,000 --> 00:00:09,000\nThird line.\n"),
    )
    .expect("modify source");

    // Attempt a re-init whose transaction fails right after the clear.
    let err = run_input_with_fault(
        &store,
        &source,
        "en",
        None,
        Some("srt"),
        None,
        None,
        |_conn| {
            Err(StoreError::Unexpected(
                "injected fault after clear".to_string(),
            ))
        },
    )
    .await
    .expect_err("fault aborts the re-init");
    assert!(matches!(err, PipelineError::Store(_)));

    // The original data survived: same cue count, same first cue, same hash.
    let cues = store
        .call(|conn| myansub::store::load_cues(conn))
        .await
        .expect("load cues");
    assert_eq!(cues.len(), 2, "clear must have rolled back");
    assert_eq!(cues[0].text_raw, "Good morning, traveller.");

    let after = store
        .call(|conn| myansub::store::repo_project::get_project(conn))
        .await
        .expect("query project")
        .expect("project row still present");
    assert_eq!(after.source_hash, original.source_hash);
}
