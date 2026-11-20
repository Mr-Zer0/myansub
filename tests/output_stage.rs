//! Output-stage and `export` tests (milestone M7-B, `docs/REQUIREMENTS.md` FR-7,
//! `docs/CLI.md` §3/§6). All tests are offline and use `tempfile` directories; the
//! subtitler re-parse is local (`docs/TESTING.md` §2/§5).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use myansub::domain::{Cue, SubtitleFormat};
use myansub::error::ExitCode;
use myansub::formats::codec::parse_document;
use myansub::pipeline::PipelineError;
use myansub::pipeline::input::{InitOutcome, run_init, run_input, run_input_with_fault};
use myansub::pipeline::output::{
    CueFinalStop, OutputWarning, ResolvedOutput, build_output_document, default_output_path,
    resolve_output_format_from_flags, resolve_output_target, run_output, store_creation_prefs,
    stored_output_prefs,
};
use myansub::store::repo_project::{get_output_prefs, get_project, set_output_prefs, set_stage};
use myansub::store::repo_translations::{
    NewTranslation, TranslationOrigin, cue_id_by_idx, delete_unlocked_translation,
};
use myansub::store::{Stage, StageStatus, Store, StoreError, load_cues};
use tempfile::tempdir;

const SRT: &str = include_str!("fixtures/formats/sample.srt");
const VTT: &str = include_str!("fixtures/formats/sample.vtt");
const ASS: &str = include_str!("fixtures/formats/sample.ass");
const SSA: &str = include_str!("fixtures/formats/sample.ssa");
const SBV: &str = include_str!("fixtures/formats/sample.sbv");

fn fixture(format: SubtitleFormat) -> &'static str {
    match format {
        SubtitleFormat::Srt => SRT,
        SubtitleFormat::Vtt => VTT,
        SubtitleFormat::Ass => ASS,
        SubtitleFormat::Ssa => SSA,
        SubtitleFormat::Sbv => SBV,
    }
}

/// A masked target "translation" for every translatable cue, keyed by `idx`. Using
/// the source masked text means `unmask` restores the exact source tags, which lets
/// a round-trip comparison focus on timings/structure (`docs/TESTING.md` §5).
fn translations_from_cues(cues: &[Cue]) -> BTreeMap<u32, String> {
    cues.iter()
        .filter(|c| c.translatable)
        .map(|c| (c.idx, c.text_masked.clone()))
        .collect()
}

/// Seeds a project on disk from `format`'s fixture with an approved translation
/// review and a translation for every translatable cue. Returns the store and the
/// source path. Writing the source to disk lets the source-hash check pass.
async fn seed_approved_project(
    dir: &Path,
    format: SubtitleFormat,
    approve_review: bool,
) -> (Store, PathBuf) {
    let source = dir.join(format!("movie.en.{}", format.extension()));
    std::fs::write(&source, fixture(format)).expect("write source");

    let store = Store::open(&dir.join("project.db")).expect("open store");
    run_input(
        &store,
        &source,
        "en",
        None,
        Some(format.extension()),
        None,
        None,
    )
    .await
    .expect("run input");

    let cues = store.call(|conn| load_cues(conn)).await.expect("load cues");
    let ids = store
        .call(|conn| cue_id_by_idx(conn))
        .await
        .expect("cue ids");

    for cue in cues.iter().filter(|c| c.translatable) {
        let id = *ids.get(&cue.idx).expect("cue id");
        let t = NewTranslation {
            cue_id: id,
            text: cue.text_masked.clone(),
            origin: TranslationOrigin::Llm,
            locked: false,
            stale: false,
            flags: vec![],
            analysis_version: 1,
            model: Some("gemini-flash-latest".to_string()),
        };
        store
            .call(move |conn| {
                myansub::store::repo_translations::upsert_translation(conn, &t).map(|_| ())
            })
            .await
            .expect("insert translation");
    }

    store
        .call(move |conn| {
            set_stage(conn, Stage::Translation, StageStatus::Completed)?;
            if approve_review {
                set_stage(conn, Stage::TranslationReview, StageStatus::Approved)?;
            }
            Ok(())
        })
        .await
        .expect("set stages");

    (store, source)
}

fn timings(cues: &[Cue]) -> Vec<(u64, u64)> {
    cues.iter().map(|c| (c.start_ms, c.end_ms)).collect()
}

// AC: M7-AC3 — a same-format export re-parses with subtitler and matches the
// source timings (and cue count) for every format.
#[tokio::test]
async fn same_format_export_preserves_timings_for_every_format() {
    for format in [
        SubtitleFormat::Srt,
        SubtitleFormat::Vtt,
        SubtitleFormat::Ass,
        SubtitleFormat::Ssa,
        SubtitleFormat::Sbv,
    ] {
        let dir = tempdir().expect("tempdir");
        let (store, source) = seed_approved_project(dir.path(), format, true).await;
        let out = dir.path().join(format!("out.{}", format.extension()));

        let report = run_output(
            &store,
            &source,
            &ResolvedOutput {
                format,
                path: out.clone(),
            },
            "",
            false,
            false,
            CueFinalStop::Keep,
        )
        .await
        .unwrap_or_else(|e| panic!("export {format} failed: {e}"));

        let source_cues = store.call(|conn| load_cues(conn)).await.expect("load cues");
        let exported = std::fs::read_to_string(&out).expect("read exported");
        let reparsed = parse_document(&exported, format)
            .unwrap_or_else(|e| panic!("reparse {format} failed: {e}"));

        assert_eq!(
            reparsed.cues.len(),
            source_cues.len(),
            "{format}: cue count changed"
        );
        assert_eq!(
            timings(&reparsed.cues),
            timings(&source_cues),
            "{format}: timings changed"
        );
        assert_eq!(report.cue_count, source_cues.len());
    }
}

// AC: M7-AC4 — a cross-format export (ASS → SRT) succeeds and surfaces a
// lost-styling warning.
#[test]
fn cross_format_export_warns_about_lost_styling() {
    let doc = parse_document(ASS, SubtitleFormat::Ass).expect("parse ass");
    let translations = translations_from_cues(&doc.cues);

    let built = build_output_document(
        SubtitleFormat::Ass,
        doc.format_meta.as_ref(),
        &doc.cues,
        &translations,
        SubtitleFormat::Srt,
        "",
        false,
        CueFinalStop::Keep,
    )
    .expect("build cross-format output");

    assert!(
        built.warnings.iter().any(|w| matches!(
            w,
            OutputWarning::LostStylingCrossFormat {
                from: SubtitleFormat::Ass,
                to: SubtitleFormat::Srt
            }
        )),
        "expected a cross-format lost-styling warning, got {:?}",
        built.warnings
    );
}

// AC: M7-AC4 — a same-format ASS export (with format_meta) produces no
// cross-format warning.
#[test]
fn same_format_ass_export_has_no_cross_format_warning() {
    let doc = parse_document(ASS, SubtitleFormat::Ass).expect("parse ass");
    let translations = translations_from_cues(&doc.cues);

    let built = build_output_document(
        SubtitleFormat::Ass,
        doc.format_meta.as_ref(),
        &doc.cues,
        &translations,
        SubtitleFormat::Ass,
        "",
        false,
        CueFinalStop::Keep,
    )
    .expect("build same-format output");

    assert!(
        !built
            .warnings
            .iter()
            .any(|w| matches!(w, OutputWarning::LostStylingCrossFormat { .. })),
        "same-format export must not warn about cross-format styling: {:?}",
        built.warnings
    );
}

// ---------------------------------------------------------------------------
// Cue-final ။ handling at export (M12-AC13, ADR-049(e))
// ---------------------------------------------------------------------------

/// A minimal translatable cue carrying `masked` as its text, no tags.
fn plain_cue(idx: u32, masked: &str) -> Cue {
    Cue {
        idx,
        start_ms: u64::from(idx) * 1000,
        end_ms: u64::from(idx) * 1000 + 900,
        text_raw: masked.to_string(),
        text_masked: masked.to_string(),
        lead_tags: None,
        inline_tags: Vec::new(),
        style: None,
        speaker: None,
        translatable: true,
    }
}

/// The restored output body of cue `idx` after building with `policy`.
fn exported_text(cue: &Cue, target: &str, policy: CueFinalStop) -> String {
    let mut translations = BTreeMap::new();
    translations.insert(cue.idx, target.to_string());
    let built = build_output_document(
        SubtitleFormat::Srt,
        None,
        std::slice::from_ref(cue),
        &translations,
        SubtitleFormat::Srt,
        "",
        false,
        policy,
    )
    .expect("build");
    built.document.cues[0].text_raw.clone()
}

// AC: M12-AC13 — `strip` removes one trailing ။ (U+104B) from a single-line cue.
#[test]
fn cue_final_stop_strip_removes_trailing_stop_single_line() {
    let cue = plain_cue(1, "x");
    assert_eq!(exported_text(&cue, "ကက။", CueFinalStop::Strip), "ကက");
}

// AC: M12-AC13 — `strip` also removes trailing whitespace around the stop.
#[test]
fn cue_final_stop_strip_removes_trailing_whitespace() {
    let cue = plain_cue(1, "x");
    assert_eq!(exported_text(&cue, "ကက။  ", CueFinalStop::Strip), "ကက");
    assert_eq!(exported_text(&cue, "ကက။\n", CueFinalStop::Strip), "ကက");
}

// AC: M12-AC13 — for a multi-line cue, `strip` touches only the final line's
// trailing ။; a ။ in the middle of the cue is preserved.
#[test]
fn cue_final_stop_strip_multiline_touches_only_final_stop() {
    let cue = plain_cue(1, "x");
    assert_eq!(
        exported_text(&cue, "ကက။\nခခ။", CueFinalStop::Strip),
        "ကက။\nခခ"
    );
}

// AC: M12-AC13 — `keep` leaves the cue text unchanged, including a trailing ။.
#[test]
fn cue_final_stop_keep_leaves_text_unchanged() {
    let cue = plain_cue(1, "x");
    assert_eq!(exported_text(&cue, "ကက။", CueFinalStop::Keep), "ကက။");
    assert_eq!(
        exported_text(&cue, "ကက။\nခခ။", CueFinalStop::Keep),
        "ကက။\nခခ။"
    );
}

// AC: M12-AC13 — a cue with no trailing ။ is unchanged under `strip`.
#[test]
fn cue_final_stop_strip_no_trailing_stop_is_unchanged() {
    let cue = plain_cue(1, "x");
    assert_eq!(exported_text(&cue, "ကက", CueFinalStop::Strip), "ကက");
}

// ADR-024 carry-over: a same-format ASS export whose source has no format_meta
// warns that styling may be lost.
#[test]
fn same_format_ass_without_format_meta_warns() {
    let doc = parse_document(ASS, SubtitleFormat::Ass).expect("parse ass");
    let translations = translations_from_cues(&doc.cues);

    let built = build_output_document(
        SubtitleFormat::Ass,
        None, // fallback-parsed source carries no format_meta
        &doc.cues,
        &translations,
        SubtitleFormat::Ass,
        "",
        false,
        CueFinalStop::Keep,
    )
    .expect("build");

    assert!(
        built.warnings.iter().any(|w| matches!(
            w,
            OutputWarning::LostStylingNoFormatMeta {
                format: SubtitleFormat::Ass
            }
        )),
        "expected a no-format-meta styling warning, got {:?}",
        built.warnings
    );
}

// Tag restoration: a cue with inline placeholders and a lead override block gets
// its real tags restored (unmask + lead prepend).
#[test]
fn export_restores_inline_and_lead_tags() {
    let cue = Cue {
        idx: 1,
        start_ms: 1000,
        end_ms: 3000,
        text_raw: "{\\an8}A sign reads {\\i1}Old Harbour{\\i0}.".to_string(),
        text_masked: "A sign reads \u{27E6}0\u{27E7}Old Harbour\u{27E6}1\u{27E7}.".to_string(),
        lead_tags: Some("{\\an8}".to_string()),
        inline_tags: vec!["{\\i1}".to_string(), "{\\i0}".to_string()],
        style: Some("Sign".to_string()),
        speaker: None,
        translatable: true,
    };
    // Translation references the same per-cue placeholders ⟦0⟧ and ⟦1⟧.
    let mut translations = BTreeMap::new();
    translations.insert(
        1,
        "ဆိုင်းဘုတ် \u{27E6}0\u{27E7}ဟောင်း\u{27E6}1\u{27E7}".to_string(),
    );

    let built = build_output_document(
        SubtitleFormat::Ass,
        None,
        std::slice::from_ref(&cue),
        &translations,
        SubtitleFormat::Ass,
        "",
        false,
        CueFinalStop::Keep,
    )
    .expect("build");

    assert_eq!(
        built.document.cues[0].text_raw,
        "{\\an8}ဆိုင်းဘုတ် {\\i1}ဟောင်း{\\i0}"
    );
}

// A non-translatable cue keeps its source text in the output.
#[test]
fn non_translatable_cue_keeps_source_text() {
    let cue = Cue {
        idx: 1,
        start_ms: 0,
        end_ms: 1000,
        text_raw: "♪♪♪".to_string(),
        text_masked: "♪♪♪".to_string(),
        lead_tags: None,
        inline_tags: vec![],
        style: None,
        speaker: None,
        translatable: false,
    };

    let built = build_output_document(
        SubtitleFormat::Srt,
        None,
        std::slice::from_ref(&cue),
        &BTreeMap::new(),
        SubtitleFormat::Srt,
        "",
        false,
        CueFinalStop::Keep,
    )
    .expect("build");

    assert_eq!(built.document.cues[0].text_raw, "♪♪♪");
    assert!(built.warnings.is_empty());
}

// ASS font override rewrites every style's font; an empty override keeps the
// original fonts.
#[test]
fn ass_font_override_applies_to_all_styles() {
    let doc = parse_document(ASS, SubtitleFormat::Ass).expect("parse ass");
    let translations = translations_from_cues(&doc.cues);

    let overridden = build_output_document(
        SubtitleFormat::Ass,
        doc.format_meta.as_ref(),
        &doc.cues,
        &translations,
        SubtitleFormat::Ass,
        "Noto Sans Myanmar",
        false,
        CueFinalStop::Keep,
    )
    .expect("build with override");
    let written =
        myansub::formats::codec::write_document(&overridden.document, SubtitleFormat::Ass)
            .expect("write ass");
    assert!(
        written.contains("Noto Sans Myanmar"),
        "override font missing: {written}"
    );
    assert!(
        !written.contains("Arial") && !written.contains("Verdana"),
        "original fonts must be replaced: {written}"
    );

    let kept = build_output_document(
        SubtitleFormat::Ass,
        doc.format_meta.as_ref(),
        &doc.cues,
        &translations,
        SubtitleFormat::Ass,
        "",
        false,
        CueFinalStop::Keep,
    )
    .expect("build without override");
    let written_kept = myansub::formats::codec::write_document(&kept.document, SubtitleFormat::Ass)
        .expect("write ass");
    assert!(
        written_kept.contains("Arial"),
        "empty override must keep original fonts: {written_kept}"
    );
}

// Completeness: a translatable cue with no translation refuses export (exit 3)
// without --allow-incomplete.
#[test]
fn missing_translation_refuses_without_allow_incomplete() {
    let cue = Cue {
        idx: 7,
        start_ms: 0,
        end_ms: 1000,
        text_raw: "Hello.".to_string(),
        text_masked: "Hello.".to_string(),
        lead_tags: None,
        inline_tags: vec![],
        style: None,
        speaker: None,
        translatable: true,
    };

    let err = build_output_document(
        SubtitleFormat::Srt,
        None,
        std::slice::from_ref(&cue),
        &BTreeMap::new(),
        SubtitleFormat::Srt,
        "",
        false,
        CueFinalStop::Keep,
    )
    .expect_err("must refuse");

    match &err {
        PipelineError::IncompleteTranslation { cue_ids } => assert_eq!(cue_ids, &vec![7]),
        other => panic!("expected IncompleteTranslation, got {other:?}"),
    }
    assert_eq!(err.exit_code(), 3);
}

// Completeness: with --allow-incomplete the source text is used and a warning
// lists the affected cue ids.
#[test]
fn missing_translation_allowed_uses_source_and_warns() {
    let cue = Cue {
        idx: 7,
        start_ms: 0,
        end_ms: 1000,
        text_raw: "Hello.".to_string(),
        text_masked: "Hello.".to_string(),
        lead_tags: None,
        inline_tags: vec![],
        style: None,
        speaker: None,
        translatable: true,
    };

    let built = build_output_document(
        SubtitleFormat::Srt,
        None,
        std::slice::from_ref(&cue),
        &BTreeMap::new(),
        SubtitleFormat::Srt,
        "",
        true,
        CueFinalStop::Keep,
    )
    .expect("allow incomplete");

    assert_eq!(built.document.cues[0].text_raw, "Hello.");
    assert!(
        built.warnings.iter().any(|w| matches!(
            w,
            OutputWarning::IncompleteCuesUsedSource { cue_ids } if cue_ids == &vec![7]
        )),
        "expected an incomplete-cues warning, got {:?}",
        built.warnings
    );
}

// Gate: export before the translation review is approved is a precondition (exit 3).
#[tokio::test]
async fn export_before_review_approved_fails_with_precondition() {
    let dir = tempdir().expect("tempdir");
    let (store, source) = seed_approved_project(dir.path(), SubtitleFormat::Srt, false).await;
    let out = dir.path().join("out.srt");

    let err = run_output(
        &store,
        &source,
        &ResolvedOutput {
            format: SubtitleFormat::Srt,
            path: out.clone(),
        },
        "",
        false,
        false,
        CueFinalStop::Keep,
    )
    .await
    .expect_err("gate must block");

    assert!(matches!(err, PipelineError::GateViolation { .. }));
    assert_eq!(err.exit_code(), 3);
    assert!(
        !out.exists(),
        "no file must be written when the gate blocks"
    );
}

// A UTF-8 BOM is prepended when requested.
#[tokio::test]
async fn export_prepends_utf8_bom_when_configured() {
    let dir = tempdir().expect("tempdir");
    let (store, source) = seed_approved_project(dir.path(), SubtitleFormat::Srt, true).await;
    let out = dir.path().join("out.srt");

    run_output(
        &store,
        &source,
        &ResolvedOutput {
            format: SubtitleFormat::Srt,
            path: out.clone(),
        },
        "",
        true,
        false,
        CueFinalStop::Keep,
    )
    .await
    .expect("export");

    let bytes = std::fs::read(&out).expect("read");
    assert_eq!(&bytes[..3], &[0xEF, 0xBB, 0xBF], "missing UTF-8 BOM");
}

// Atomic write: a failure (output into a non-existent directory) leaves no file.
#[tokio::test]
async fn export_leaves_no_partial_file_on_write_failure() {
    let dir = tempdir().expect("tempdir");
    let (store, source) = seed_approved_project(dir.path(), SubtitleFormat::Srt, true).await;
    let out = dir.path().join("missing_dir").join("out.srt");

    let err = run_output(
        &store,
        &source,
        &ResolvedOutput {
            format: SubtitleFormat::Srt,
            path: out.clone(),
        },
        "",
        false,
        false,
        CueFinalStop::Keep,
    )
    .await
    .expect_err("write into a missing directory must fail");

    assert!(matches!(err, PipelineError::Store(_)));
    assert!(!out.exists(), "no target file must remain");
}

// --- Output format / path resolution (docs/CLI.md §3) -----------------------

#[test]
fn flag_format_wins_over_stored_and_default() {
    let resolved = resolve_output_target(
        Some(SubtitleFormat::Vtt),
        None,
        Some(SubtitleFormat::Ass),
        None,
        Path::new("/x/movie.ja.srt"),
        SubtitleFormat::Srt,
    )
    .expect("resolve");
    assert_eq!(resolved.format, SubtitleFormat::Vtt);
}

#[test]
fn stored_format_wins_over_default_when_no_flag() {
    let resolved = resolve_output_target(
        None,
        None,
        Some(SubtitleFormat::Ass),
        None,
        Path::new("/x/movie.ja.srt"),
        SubtitleFormat::Srt,
    )
    .expect("resolve");
    assert_eq!(resolved.format, SubtitleFormat::Ass);
}

#[test]
fn default_format_is_source_format() {
    let resolved = resolve_output_target(
        None,
        None,
        None,
        None,
        Path::new("/x/movie.ja.srt"),
        SubtitleFormat::Srt,
    )
    .expect("resolve");
    assert_eq!(resolved.format, SubtitleFormat::Srt);
    assert_eq!(resolved.path, PathBuf::from("/x/movie.ja.my.srt"));
}

#[test]
fn output_extension_sets_format_when_format_flag_absent() {
    let fmt = resolve_output_format_from_flags(None, Some(Path::new("out.vtt"))).expect("resolve");
    assert_eq!(fmt, Some(SubtitleFormat::Vtt));
}

#[test]
fn unknown_output_extension_is_ignored() {
    let fmt = resolve_output_format_from_flags(None, Some(Path::new("out.txt"))).expect("resolve");
    assert_eq!(fmt, None);
}

#[test]
fn format_and_extension_disagreement_is_usage_error() {
    let err =
        resolve_output_format_from_flags(Some(SubtitleFormat::Srt), Some(Path::new("out.vtt")))
            .expect_err("disagreement");
    assert!(matches!(
        err,
        PipelineError::OutputFormatExtensionMismatch {
            format: SubtitleFormat::Srt,
            extension: SubtitleFormat::Vtt
        }
    ));
    assert_eq!(err.exit_code(), 2);
}

#[test]
fn default_output_path_uses_source_stem_and_format_extension() {
    assert_eq!(
        default_output_path(Path::new("/a/b/movie.ja.srt"), SubtitleFormat::Ass),
        PathBuf::from("/a/b/movie.ja.my.ass")
    );
}

#[test]
fn flag_path_wins_over_stored_and_default() {
    let resolved = resolve_output_target(
        None,
        Some(Path::new("flag.srt")),
        None,
        Some(Path::new("stored.srt")),
        Path::new("/x/movie.ja.srt"),
        SubtitleFormat::Srt,
    )
    .expect("resolve");
    assert_eq!(resolved.path, PathBuf::from("flag.srt"));
}

#[test]
fn stored_path_wins_over_default() {
    let resolved = resolve_output_target(
        None,
        None,
        None,
        Some(Path::new("stored.srt")),
        Path::new("/x/movie.ja.srt"),
        SubtitleFormat::Srt,
    )
    .expect("resolve");
    assert_eq!(resolved.path, PathBuf::from("stored.srt"));
}

// --- #1 (MAJOR): the format/extension disagreement is a usage error BEFORE any
// work, via the real command handlers (binary-level). ------------------------

fn bin() -> std::process::Command {
    std::process::Command::new(env!("CARGO_BIN_EXE_myansub"))
}

// AC: docs/CLI.md §3 — `init` with --output-format vs -o extension disagreement
// exits 2 and creates NO project.
#[test]
fn init_with_format_extension_disagreement_exits_2_and_creates_nothing() {
    let dir = tempdir().expect("tempdir");
    let source = dir.path().join("movie.en.srt");
    std::fs::write(&source, SRT).expect("write source");

    let status = bin()
        .current_dir(dir.path())
        .args([
            "init",
            source.to_str().expect("utf8 path"),
            "--from",
            "en",
            "--output-format",
            "srt",
            "-o",
            "out.vtt",
        ])
        .status()
        .expect("spawn init");

    assert_eq!(status.code(), Some(2), "disagreement must be a usage error");
    let project_db = dir.path().join("movie.en.srt.myansub").join("project.db");
    assert!(
        !project_db.exists(),
        "no project must be created on a usage error"
    );
}

// AC: docs/CLI.md §3 — `run` on an EXISTING project with a disagreement exits 2
// (it must not be ignored).
#[test]
fn run_on_existing_project_with_disagreement_exits_2() {
    let dir = tempdir().expect("tempdir");
    let source = dir.path().join("movie.en.srt");
    std::fs::write(&source, SRT).expect("write source");

    let created = bin()
        .current_dir(dir.path())
        .args(["init", source.to_str().expect("utf8 path"), "--from", "en"])
        .status()
        .expect("spawn init");
    assert!(created.success(), "init should succeed");

    let status = bin()
        .current_dir(dir.path())
        .args([
            "run",
            source.to_str().expect("utf8 path"),
            "--output-format",
            "srt",
            "-o",
            "out.vtt",
        ])
        .status()
        .expect("spawn run");
    assert_eq!(
        status.code(),
        Some(2),
        "run must reject a disagreement even on an existing project"
    );
}

// --- #2 (MAJOR): output prefs are persisted atomically with project creation. -

// AC: docs/CLI.md §3 — `run_init` persists the output prefs as part of creation;
// they are present immediately in the just-created project (one transaction).
#[tokio::test]
async fn run_init_persists_output_prefs_with_creation() {
    let dir = tempdir().expect("tempdir");
    let source = dir.path().join("movie.en.srt");
    std::fs::write(&source, SRT).expect("write source");

    let outcome = run_init(
        dir.path(),
        &source,
        "en",
        None,
        false,
        false,
        false,
        Some("vtt"),
        Some("out.vtt"),
    )
    .await
    .expect("init");
    assert!(matches!(outcome, InitOutcome::Initialized(_)));

    let store = Store::open(&dir.path().join("project.db")).expect("open");
    let prefs = store
        .call(|conn| get_output_prefs(conn))
        .await
        .expect("prefs");
    assert_eq!(
        prefs,
        (Some("vtt".to_string()), Some("out.vtt".to_string()))
    );
}

// AC: docs/ARCHITECTURE.md §6 rule 1 — the output prefs live in the SAME
// transaction as creation: a fault rolls them back together with the project row.
#[tokio::test]
async fn output_prefs_roll_back_with_creation_on_fault() {
    let dir = tempdir().expect("tempdir");
    let source = dir.path().join("movie.en.srt");
    std::fs::write(&source, SRT).expect("write source");

    let store = Store::open(&dir.path().join("project.db")).expect("open");
    let err = run_input_with_fault(
        &store,
        &source,
        "en",
        None,
        Some("srt"),
        Some("vtt"),
        Some("out.vtt"),
        |_conn| Err(StoreError::Unexpected("injected fault".to_string())),
    )
    .await
    .expect_err("fault must abort");
    assert!(matches!(err, PipelineError::Store(_)));

    let project = store.call(|conn| get_project(conn)).await.expect("get");
    assert!(project.is_none(), "project row must have rolled back");
    let prefs = store
        .call(|conn| get_output_prefs(conn))
        .await
        .expect("prefs");
    assert_eq!(prefs, (None, None), "prefs must have rolled back too");
}

// --- #3: the pref chain (store_creation_prefs → stored → resolve). -----------

#[tokio::test]
async fn pref_chain_stores_resolves_round_trip() {
    let dir = tempdir().expect("tempdir");
    let source = dir.path().join("movie.en.srt");
    std::fs::write(&source, SRT).expect("write source");
    let store = Store::open(&dir.path().join("project.db")).expect("open");
    run_input(&store, &source, "en", None, Some("srt"), None, None)
        .await
        .expect("input");

    store_creation_prefs(&store, Some(SubtitleFormat::Vtt), Some(Path::new("x.vtt")))
        .await
        .expect("store prefs");

    let (stored_format, stored_path) = stored_output_prefs(&store).await.expect("read prefs");
    assert_eq!(stored_format, Some(SubtitleFormat::Vtt));
    assert_eq!(stored_path.as_deref(), Some(Path::new("x.vtt")));

    let resolved = resolve_output_target(
        None,
        None,
        stored_format,
        stored_path.as_deref(),
        &source,
        SubtitleFormat::Srt,
    )
    .expect("resolve");
    assert_eq!(resolved.format, SubtitleFormat::Vtt);
    assert_eq!(resolved.path, PathBuf::from("x.vtt"));
}

#[tokio::test]
async fn unparseable_stored_format_falls_back_to_none() {
    let dir = tempdir().expect("tempdir");
    let source = dir.path().join("movie.en.srt");
    std::fs::write(&source, SRT).expect("write source");
    let store = Store::open(&dir.path().join("project.db")).expect("open");
    run_input(&store, &source, "en", None, Some("srt"), None, None)
        .await
        .expect("input");

    store
        .call(|conn| set_output_prefs(conn, Some("bogus"), None))
        .await
        .expect("set bogus");

    let (stored_format, _) = stored_output_prefs(&store).await.expect("read prefs");
    assert_eq!(stored_format, None);
}

#[tokio::test]
async fn store_creation_prefs_rejects_disagreement_with_usage_error() {
    let dir = tempdir().expect("tempdir");
    let source = dir.path().join("movie.en.srt");
    std::fs::write(&source, SRT).expect("write source");
    let store = Store::open(&dir.path().join("project.db")).expect("open");

    let err = store_creation_prefs(&store, Some(SubtitleFormat::Srt), Some(Path::new("x.vtt")))
        .await
        .expect_err("disagreement");
    assert!(matches!(
        err,
        PipelineError::OutputFormatExtensionMismatch { .. }
    ));
    assert_eq!(err.exit_code(), 2);
}

// --- #4: BOM-off and run_output incomplete refusal leaves no file. -----------

#[tokio::test]
async fn export_writes_no_bom_when_disabled() {
    let dir = tempdir().expect("tempdir");
    let (store, source) = seed_approved_project(dir.path(), SubtitleFormat::Srt, true).await;
    let out = dir.path().join("out.srt");

    run_output(
        &store,
        &source,
        &ResolvedOutput {
            format: SubtitleFormat::Srt,
            path: out.clone(),
        },
        "",
        false,
        false,
        CueFinalStop::Keep,
    )
    .await
    .expect("export");

    let bytes = std::fs::read(&out).expect("read");
    assert_ne!(&bytes[..3], &[0xEF, 0xBB, 0xBF], "no BOM must be written");
}

#[tokio::test]
async fn run_output_incomplete_refuses_and_writes_no_file() {
    let dir = tempdir().expect("tempdir");
    let (store, source) = seed_approved_project(dir.path(), SubtitleFormat::Srt, true).await;
    // Remove one translatable cue's translation to make the project incomplete.
    let ids = store
        .call(|conn| cue_id_by_idx(conn))
        .await
        .expect("cue ids");
    let victim = *ids.get(&1).expect("cue 1");
    store
        .call(move |conn| delete_unlocked_translation(conn, victim).map(|_| ()))
        .await
        .expect("delete translation");

    let out = dir.path().join("out.srt");
    let err = run_output(
        &store,
        &source,
        &ResolvedOutput {
            format: SubtitleFormat::Srt,
            path: out.clone(),
        },
        "",
        false,
        false,
        CueFinalStop::Keep,
    )
    .await
    .expect_err("must refuse incomplete export");

    assert!(matches!(err, PipelineError::IncompleteTranslation { .. }));
    assert_eq!(err.exit_code(), 3);
    assert!(!out.exists(), "no file must be written on refusal");
}

// AC: M12-AC14 — changing `cue_final_stop` needs only a re-export: a second
// `run_output` with the other policy produces a different file, and the export path
// makes zero LLM/transport calls (ADR-049(e), ADR-050). `run_output` takes no
// transport, so an `llm_calls` count of 0 after two exports proves no LLM traffic.
#[tokio::test]
async fn changing_cue_final_stop_reexports_differently_with_no_llm_calls() {
    let dir = tempdir().expect("tempdir");
    let (store, source) = seed_approved_project(dir.path(), SubtitleFormat::Srt, true).await;

    // Give a translatable cue a Burmese target ending in the sentence stop ။.
    let ids = store
        .call(|conn| cue_id_by_idx(conn))
        .await
        .expect("cue ids");
    let cue1 = *ids.get(&1).expect("cue 1");
    store
        .call(move |conn| {
            myansub::store::repo_translations::upsert_translation(
                conn,
                &NewTranslation {
                    cue_id: cue1,
                    text: "ကက။".to_string(),
                    origin: TranslationOrigin::Llm,
                    locked: false,
                    stale: false,
                    flags: vec![],
                    analysis_version: 1,
                    model: Some("gemini-flash-latest".to_string()),
                },
            )
            .map(|_| ())
        })
        .await
        .expect("seed burmese translation");

    let strip_path = dir.path().join("strip.srt");
    run_output(
        &store,
        &source,
        &ResolvedOutput {
            format: SubtitleFormat::Srt,
            path: strip_path.clone(),
        },
        "",
        false,
        false,
        CueFinalStop::Strip,
    )
    .await
    .expect("strip export");

    let keep_path = dir.path().join("keep.srt");
    run_output(
        &store,
        &source,
        &ResolvedOutput {
            format: SubtitleFormat::Srt,
            path: keep_path.clone(),
        },
        "",
        false,
        false,
        CueFinalStop::Keep,
    )
    .await
    .expect("keep export");

    let stripped = std::fs::read_to_string(&strip_path).expect("read strip");
    let kept = std::fs::read_to_string(&keep_path).expect("read keep");
    assert_ne!(
        stripped, kept,
        "the two policies must produce different output"
    );
    assert!(stripped.contains("ကက") && !stripped.contains("ကက။"));
    assert!(kept.contains("ကက။"));

    // The export path makes no LLM calls.
    let calls = store
        .call(|conn| myansub::store::repo_llm::count_calls(conn))
        .await
        .expect("count calls");
    assert_eq!(calls, 0, "export must make no LLM calls");
}
