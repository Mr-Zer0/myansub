//! M5-B analysis-review tests via the public `review` surface (`docs/CLI.md` §6,
//! `docs/REQUIREMENTS.md` FR-4, `docs/DATA_MODEL.md` §3/§6). Offline, in-memory
//! stores and `tempfile` scratch dirs (`docs/TESTING.md` §1/§2); the editor is the
//! injectable [`EditorLauncher`], never a real `$EDITOR`.

use std::path::Path;

use myansub::domain::Cue;
use myansub::error::ExitCode;
use myansub::error::exit;
use myansub::pipeline::can_run;
use myansub::pipeline::orchestrator::load_state;
use myansub::review::ReviewError;
use myansub::review::analysis;
use myansub::review::editor::{self, EditorLauncher, resolve_editor};
use myansub::store::repo_analysis::{
    AnalysisOrigin, current_analysis, insert_analysis_version, list_analysis_versions,
};
use myansub::store::repo_cues::insert_cues;
use myansub::store::repo_project::get_stage;
use myansub::store::{Stage, StageStatus, Store};

const MODEL_SIG: &str = "prompt_v1|gemini-flash-latest";
const DIGEST: &str = "sha256:abc";

/// A complete, valid analysis document (1 character, 2 non-overlapping scenes).
const VALID: &str = r#"{
  "schema_version": 1,
  "premise": "A traveller arrives at dawn.",
  "tone": "calm",
  "characters": [{"id":"c1","name":"Aoi","gender":"f","age_group":"young"}],
  "address_matrix": [{"from":"c1","to":"c1","relation":"self","register":"polite"}],
  "glossary": [{"term":"sakura","target":"cherry blossom","keep":false}],
  "scenes": [
    {"cue_range":[1,50],"summary":"intro","speakers":["c1"]},
    {"cue_range":[51,100],"summary":"outro"}
  ],
  "style_rules": ["keep it terse"],
  "unresolved_ambiguities": ["who is the narrator?"]
}"#;

fn cue(idx: u32) -> Cue {
    Cue {
        idx,
        start_ms: u64::from(idx) * 1000,
        end_ms: u64::from(idx) * 1000 + 800,
        text_raw: format!("line {idx}"),
        text_masked: format!("line {idx}"),
        lead_tags: None,
        inline_tags: Vec::new(),
        style: None,
        speaker: None,
        translatable: true,
    }
}

/// A store with 100 cues and one unapproved LLM analysis version (= the base a user
/// edits). Returns the opened store.
async fn seeded_store(doc: &str) -> Store {
    let store = Store::open_in_memory().expect("open");
    let doc = doc.to_string();
    store
        .call(move |conn| {
            let cues: Vec<Cue> = (1..=100).map(cue).collect();
            insert_cues(conn, &cues)?;
            insert_analysis_version(conn, &doc, AnalysisOrigin::Llm, DIGEST, MODEL_SIG)?;
            Ok(())
        })
        .await
        .expect("seed");
    store
}

async fn version_count(store: &Store) -> usize {
    store
        .call(|conn| Ok(list_analysis_versions(conn)?.len()))
        .await
        .expect("list")
}

// ---------------------------------------------------------------------------
// M5-AC4 — unresolved_ambiguities shown first
// ---------------------------------------------------------------------------

// AC: M5-AC4 — `show` renders unresolved_ambiguities before every other section.
#[tokio::test]
async fn show_renders_ambiguities_first() {
    let store = seeded_store(VALID).await;
    let rendered = analysis::show(&store, false).await.expect("show");

    let amb = rendered
        .find("Unresolved ambiguities")
        .expect("ambiguities");
    let premise = rendered.find("Premise").expect("premise");
    let characters = rendered.find("Characters").expect("characters");
    assert!(amb < premise, "ambiguities must precede premise");
    assert!(premise < characters, "premise must precede characters");
    assert!(rendered.contains("who is the narrator?"));
}

// AC: M5-AC4 — show still works when unresolved_ambiguities is empty/absent.
#[tokio::test]
async fn show_with_empty_ambiguities_still_works() {
    const NO_AMB: &str = r#"{"schema_version":1,"premise":"p","characters":[],
    "scenes":[{"cue_range":[1,5],"summary":"s"}]}"#;
    let store = seeded_store(NO_AMB).await;
    let rendered = analysis::show(&store, false).await.expect("show");
    assert!(rendered.contains("Unresolved ambiguities"));
    assert!(rendered.contains("(none)"));
}

// AC: M5-AC4 — `show --json` emits the raw document as JSON.
#[tokio::test]
async fn show_json_emits_document() {
    let store = seeded_store(VALID).await;
    let json = analysis::show(&store, true).await.expect("show json");
    let parsed: serde_json::Value = serde_json::from_str(&json).expect("valid json");
    assert_eq!(parsed["premise"], "A traveller arrives at dawn.");
}

// ---------------------------------------------------------------------------
// Preconditions (exit 3)
// ---------------------------------------------------------------------------

// AC: CLI.md §7 — show with no current analysis is a precondition failure (exit 3).
#[tokio::test]
async fn show_without_current_analysis_is_precondition() {
    let store = Store::open_in_memory().expect("open");
    let err = analysis::show(&store, false)
        .await
        .expect_err("no analysis");
    assert!(matches!(err, ReviewError::NoCurrentAnalysis));
    assert_eq!(err.exit_code(), exit::PRECONDITION);
}

// AC: CLI.md §7 — approve with no current analysis is a precondition failure.
#[tokio::test]
async fn approve_without_current_analysis_is_precondition() {
    let store = Store::open_in_memory().expect("open");
    let err = analysis::approve(&store).await.expect_err("no analysis");
    assert!(matches!(err, ReviewError::NoCurrentAnalysis));
    assert_eq!(err.exit_code(), 3);
}

// ---------------------------------------------------------------------------
// M5-AC1 — close the loop: analyze → show → import → approve
// ---------------------------------------------------------------------------

// AC: M5-AC1 — export the current analysis, import a valid edit (new user version),
// approve it, and confirm the translation gate now passes.
#[tokio::test]
async fn close_the_loop_show_export_import_approve() {
    let store = seeded_store(VALID).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("analysis.json");

    // show works.
    analysis::show(&store, false).await.expect("show");

    // export writes pretty JSON that parses back.
    analysis::export(&store, &path).await.expect("export");
    let exported = std::fs::read_to_string(&path).expect("read export");
    let mut doc: serde_json::Value = serde_json::from_str(&exported).expect("parse export");

    // edit a field (valid) and import → a new user version.
    doc["premise"] = serde_json::Value::String("An edited premise.".into());
    let edited = serde_json::to_string_pretty(&doc).expect("serialize");
    let new_version = analysis::import_doc(&store, &edited).await.expect("import");
    assert_eq!(new_version, 2);

    let current = store
        .call(|conn| current_analysis(conn))
        .await
        .expect("current")
        .expect("present");
    assert_eq!(current.version, 2);
    assert!(!current.approved, "a fresh user version is unapproved");

    let v2 = store
        .call(|conn| myansub::store::repo_analysis::get_analysis_version(conn, 2))
        .await
        .expect("get")
        .expect("v2");
    assert_eq!(v2.origin, AnalysisOrigin::User);
    assert_eq!(v2.source_digest, DIGEST, "source_digest carried forward");
    assert_eq!(v2.signature, MODEL_SIG, "signature carried forward");

    // translation gate is blocked before approval.
    let state = load_state(&store).await.expect("state");
    assert!(can_run(Stage::Translation, &state).is_err());

    // approve → gate passes.
    let approved = analysis::approve(&store).await.expect("approve");
    assert_eq!(approved, 2);
    let state = load_state(&store).await.expect("state");
    assert!(state.analysis_approved);
    assert!(can_run(Stage::Translation, &state).is_ok());
}

// ---------------------------------------------------------------------------
// M5-AC3 — import validation with field locations, all-or-nothing
// ---------------------------------------------------------------------------

/// Imports `doc` into a freshly seeded store and asserts it is rejected, that the
/// rendered error names `needle`, and that no new version was created.
async fn assert_rejected_with(doc_mutate: impl FnOnce(&mut serde_json::Value), needle: &str) {
    let store = seeded_store(VALID).await;
    let before = version_count(&store).await;

    let mut doc: serde_json::Value = serde_json::from_str(VALID).expect("parse valid");
    doc_mutate(&mut doc);
    let body = serde_json::to_string(&doc).expect("serialize");

    let err = analysis::import_doc(&store, &body)
        .await
        .expect_err("should be rejected");
    let message = err.to_string();
    assert!(
        message.contains(needle),
        "error {message:?} should name location {needle:?}"
    );
    assert!(matches!(err, ReviewError::Invalid(_)));

    let after = version_count(&store).await;
    assert_eq!(
        before, after,
        "a rejected import must create no new version"
    );
}

// AC: M5-AC3 — an unknown field is rejected (strict) with its location.
#[tokio::test]
async fn import_rejects_unknown_field_with_location() {
    assert_rejected_with(
        |doc| {
            doc["characters"][0]["bogus"] = serde_json::Value::Bool(true);
        },
        "characters[0]",
    )
    .await;
}

// AC: M5-AC3 — an address_matrix reference to a non-existent character is rejected
// naming the field and the bad id.
#[tokio::test]
async fn import_rejects_bad_reference_with_location() {
    let store = seeded_store(VALID).await;
    let mut doc: serde_json::Value = serde_json::from_str(VALID).expect("parse");
    doc["address_matrix"][0]["from"] = serde_json::Value::String("c9".into());
    let body = serde_json::to_string(&doc).expect("serialize");

    let err = analysis::import_doc(&store, &body)
        .await
        .expect_err("rejected");
    let message = err.to_string();
    assert!(message.contains("address_matrix[0].from"), "{message}");
    assert!(message.contains("c9"), "{message}");
    assert_eq!(version_count(&store).await, 1);
}

// AC: M5-AC3 — a scene cue_range outside the existing cue indices is rejected.
#[tokio::test]
async fn import_rejects_scene_range_outside_cues() {
    assert_rejected_with(
        |doc| {
            doc["scenes"][0]["cue_range"] = serde_json::json!([1, 200]);
        },
        "scenes[0].cue_range",
    )
    .await;
}

// AC: M5-AC3 — a non-ascending cue_range is rejected naming the scene.
#[tokio::test]
async fn import_rejects_non_ascending_cue_range() {
    assert_rejected_with(
        |doc| {
            doc["scenes"][0]["cue_range"] = serde_json::json!([90, 80]);
        },
        "scenes[0].cue_range",
    )
    .await;
}

// AC: M5-AC3 — overlapping cue ranges are rejected naming both scenes.
#[tokio::test]
async fn import_rejects_overlapping_cue_ranges() {
    let store = seeded_store(VALID).await;
    let mut doc: serde_json::Value = serde_json::from_str(VALID).expect("parse");
    doc["scenes"][1]["cue_range"] = serde_json::json!([40, 90]);
    let body = serde_json::to_string(&doc).expect("serialize");

    let err = analysis::import_doc(&store, &body)
        .await
        .expect_err("rejected");
    let message = err.to_string();
    assert!(message.contains("scenes[1]"), "{message}");
    assert!(message.contains("scenes[0]"), "{message}");
    assert_eq!(version_count(&store).await, 1);
}

// AC: M5-AC3 — glossary keep=true with target != term is rejected with location.
#[tokio::test]
async fn import_rejects_glossary_keep_mismatch() {
    assert_rejected_with(
        |doc| {
            doc["glossary"][0]["keep"] = serde_json::Value::Bool(true);
            // term="sakura", target="cherry blossom" → mismatch
        },
        "glossary[0]",
    )
    .await;
}

// AC: M5-AC3 — an array over its cap (characters > 40) is rejected.
#[tokio::test]
async fn import_rejects_characters_over_cap() {
    assert_rejected_with(
        |doc| {
            let chars: Vec<serde_json::Value> = (0..41)
                .map(|i| {
                    serde_json::json!({
                        "id": format!("c{i}"),
                        "name": "n",
                        "gender": "unknown",
                        "age_group": "unknown"
                    })
                })
                .collect();
            doc["characters"] = serde_json::Value::Array(chars);
        },
        "characters",
    )
    .await;
}

// AC: M5-AC3 — a scene speaker referencing a non-existent character is rejected
// naming the field (scenes[i].speakers[j]) and the bad id.
#[tokio::test]
async fn import_rejects_bad_speaker_reference_with_location() {
    let store = seeded_store(VALID).await;
    let mut doc: serde_json::Value = serde_json::from_str(VALID).expect("parse");
    doc["scenes"][0]["speakers"] = serde_json::json!(["c9"]);
    let body = serde_json::to_string(&doc).expect("serialize");

    let err = analysis::import_doc(&store, &body)
        .await
        .expect_err("rejected");
    let message = err.to_string();
    assert!(message.contains("scenes[0].speakers[0]"), "{message}");
    assert!(message.contains("c9"), "{message}");
    assert!(matches!(err, ReviewError::Invalid(_)));
    assert_eq!(version_count(&store).await, 1);
}

// AC: M5-AC3 — an invalid enum value (gender not m|f|unknown) is rejected with its
// location.
#[tokio::test]
async fn import_rejects_invalid_gender_with_location() {
    assert_rejected_with(
        |doc| {
            doc["characters"][0]["gender"] = serde_json::Value::String("x".into());
        },
        "characters[0].gender",
    )
    .await;
}

// AC: M5-AC3 — a string over its length limit (premise > 500) is rejected with its
// location.
#[tokio::test]
async fn import_rejects_over_length_string_with_location() {
    assert_rejected_with(
        |doc| {
            doc["premise"] = serde_json::Value::String("a".repeat(501));
        },
        "premise",
    )
    .await;
}

// AC: M5-AC3 — a character whose aliases exceed the per-element cap (8) is rejected
// with its location.
#[tokio::test]
async fn import_rejects_aliases_over_cap_with_location() {
    assert_rejected_with(
        |doc| {
            let aliases: Vec<serde_json::Value> = (0..9)
                .map(|i| serde_json::Value::String(format!("a{i}")))
                .collect();
            doc["characters"][0]["aliases"] = serde_json::Value::Array(aliases);
        },
        "characters[0].aliases",
    )
    .await;
}

// AC: ARCH §2 — importing a valid edit after approval re-blocks translation and
// returns the analysis-review stage to `completed`.
#[tokio::test]
async fn importing_after_approval_reblocks_translation() {
    let store = seeded_store(VALID).await;

    // Approve v1 → translation gate opens.
    analysis::approve(&store).await.expect("approve v1");
    let state = load_state(&store).await.expect("state");
    assert!(can_run(Stage::Translation, &state).is_ok());

    // Import a valid edit → v2 (unapproved).
    let v2 = analysis::import_doc(&store, VALID).await.expect("import");
    assert_eq!(v2, 2);

    // Translation is re-blocked and the review stage is back to `completed`.
    let state = load_state(&store).await.expect("state");
    assert!(
        can_run(Stage::Translation, &state).is_err(),
        "a fresh unapproved version re-blocks translation"
    );
    let stage = store
        .call(|conn| get_stage(conn, Stage::AnalysisReview))
        .await
        .expect("get_stage");
    assert_eq!(stage, Some(StageStatus::Completed));
}

// AC: M5-AC3 — a fully valid import is accepted (the validator is not over-strict).
#[tokio::test]
async fn import_accepts_valid_document() {
    let store = seeded_store(VALID).await;
    let version = analysis::import_doc(&store, VALID).await.expect("import");
    assert_eq!(version, 2);
    assert_eq!(version_count(&store).await, 2);
}

// AC: M5-AC3 — invalid JSON is a parse error, not a validation error.
#[tokio::test]
async fn import_rejects_unparseable_json() {
    let store = seeded_store(VALID).await;
    let err = analysis::import_doc(&store, "not json {")
        .await
        .expect_err("parse");
    assert!(matches!(err, ReviewError::Parse(_)));
    assert_eq!(version_count(&store).await, 1);
}

// ---------------------------------------------------------------------------
// Edit flow via the injectable launcher
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

// AC: M5-AC1 — editing with a valid edit creates a new user version.
#[tokio::test]
async fn edit_with_valid_edit_creates_new_version() {
    let store = seeded_store(VALID).await;
    let dir = tempfile::tempdir().expect("tempdir");

    let mut doc: serde_json::Value = serde_json::from_str(VALID).expect("parse");
    doc["tone"] = serde_json::Value::String("tense".into());
    let edited = serde_json::to_string_pretty(&doc).expect("serialize");

    let launcher = FakeEditor { content: edited };
    let version = analysis::edit(&store, launcher, dir.path())
        .await
        .expect("edit");
    assert_eq!(version, 2);
    assert_eq!(version_count(&store).await, 2);
}

// AC: M5-AC3 — an invalid edit is rejected with a location and creates no version.
#[tokio::test]
async fn edit_with_invalid_edit_is_rejected_and_unchanged() {
    let store = seeded_store(VALID).await;
    let dir = tempfile::tempdir().expect("tempdir");

    let mut doc: serde_json::Value = serde_json::from_str(VALID).expect("parse");
    doc["scenes"][0]["cue_range"] = serde_json::json!([90, 80]);
    let edited = serde_json::to_string_pretty(&doc).expect("serialize");

    let launcher = FakeEditor { content: edited };
    let err = analysis::edit(&store, launcher, dir.path())
        .await
        .expect_err("rejected");
    assert!(matches!(err, ReviewError::Invalid(_)));
    assert!(err.to_string().contains("scenes[0].cue_range"));
    assert_eq!(version_count(&store).await, 1, "no new version on reject");
}

// ---------------------------------------------------------------------------
// Editor resolution (no real $EDITOR spawned)
// ---------------------------------------------------------------------------

// AC: CLI.md §6 — with neither $EDITOR nor $VISUAL set, the launcher errors clearly.
#[test]
fn resolve_editor_errors_when_unset() {
    let err = resolve_editor(|_| None).expect_err("unset");
    assert!(matches!(err, ReviewError::EditorNotConfigured));
}

// AC: CLI.md §6 — $EDITOR wins and is returned; $VISUAL is the fallback.
#[test]
fn resolve_editor_prefers_editor_then_visual() {
    let editor = resolve_editor(|k| (k == "EDITOR").then(|| "vi".to_string())).expect("editor");
    assert_eq!(editor, "vi");
    let visual = resolve_editor(|k| (k == "VISUAL").then(|| "nano".to_string())).expect("visual");
    assert_eq!(visual, "nano");
    // A blank $EDITOR is ignored in favour of $VISUAL.
    let fallback = resolve_editor(|k| match k {
        "EDITOR" => Some("   ".to_string()),
        "VISUAL" => Some("nano".to_string()),
        _ => None,
    })
    .expect("fallback");
    assert_eq!(fallback, "nano");
}

// AC: ARCH §6 rule 4 — export writes atomically (file present and complete).
#[tokio::test]
async fn export_writes_complete_file() {
    let store = seeded_store(VALID).await;
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("out.json");
    analysis::export(&store, &path).await.expect("export");
    let bytes = std::fs::read(&path).expect("read");
    let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("valid json");
    assert_eq!(parsed["schema_version"], 1);
    // no leftover .tmp
    assert!(!dir.path().join("out.json.tmp").exists());
    // keep editor module referenced for the unused-import lint
    let _ = editor::read_file(&path).expect("read via editor");
}
