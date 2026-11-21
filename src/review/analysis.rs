//! Analysis-specific review: show, export, import, edit, and approve
//! (`docs/CLI.md` §6, `docs/REQUIREMENTS.md` FR-4, `docs/DATA_MODEL.md` §3/§6).
//!
//! The analysis review file is the analysis document (`docs/DATA_MODEL.md` §3),
//! pretty-printed JSON; the whole document is editable, and provenance
//! (`source_digest`, `signature`) stays in table columns, never in the JSON
//! (`docs/DATA_MODEL.md` §6).
//!
//! Import is strict and all-or-nothing (`docs/ARCHITECTURE.md` §6 rule 7): the
//! file is parsed, then validated against the `docs/DATA_MODEL.md` §3 rules,
//! *collecting* every violation with its field location (M5-AC3). Only if there
//! are no violations is a new, unapproved `origin = user` version inserted — which
//! returns the analysis-review stage to `completed` and re-blocks translation until
//! it is approved (`docs/ARCHITECTURE.md` §2). Unknown fields are rejected on this
//! user-import path (strict); the LLM-output path stays lenient (M5-A), unchanged.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;

use crate::store::repo_analysis::{
    AnalysisOrigin, approve_current_analysis_in_tx, current_analysis, get_analysis_version,
    insert_analysis_version_in_tx,
};
use crate::store::repo_cues::load_cues;
use crate::store::repo_project::{set_stage, set_stage_with_origin};
use crate::store::{Stage, StageStatus, Store};

use super::editor::{EditorLauncher, capture_edit, export_to_path};
use super::error::{ReviewError, Violation};

/// The top-level fields allowed in an analysis document (`docs/DATA_MODEL.md` §3).
/// Any other top-level key is rejected on user import (strict).
const TOP_FIELDS: &[&str] = &[
    "schema_version",
    "premise",
    "tone",
    "narration_register",
    "characters",
    "address_matrix",
    "glossary",
    "scenes",
    "formal_segments",
    "style_rules",
    "unresolved_ambiguities",
];

// Array caps (`docs/DATA_MODEL.md` §3).
const CAP_CHARACTERS: usize = 40;
const CAP_ADDRESS_MATRIX: usize = 80;
const CAP_GLOSSARY: usize = 120;
const CAP_SCENES: usize = 120;
const CAP_FORMAL_SEGMENTS: usize = 60;
const CAP_ALIASES: usize = 8;
const CAP_STYLE_RULES: usize = 12;
const CAP_AMBIGUITIES: usize = 12;

// ---------------------------------------------------------------------------
// show
// ---------------------------------------------------------------------------

/// Renders the current analysis for the user (`docs/CLI.md` §6). With `json`, the
/// raw document is emitted as pretty JSON; otherwise a human view is produced with
/// `unresolved_ambiguities` **first** (M5-AC4), then the remaining sections.
///
/// Returns [`ReviewError::NoCurrentAnalysis`] (exit 3) when the project has no
/// analysis version yet.
pub async fn show(store: &Store, json: bool) -> Result<String, ReviewError> {
    let value = load_current_value(store).await?;
    if json {
        render_json(&value)
    } else {
        // The human view shows each formal segment's first source line (M12-AC7), so
        // load the cue idx → source-text map alongside the document.
        let cue_texts = store
            .call(|conn| Ok(cue_source_texts(&load_cues(conn)?)))
            .await?;
        Ok(render_human(&value, &cue_texts))
    }
}

/// The cue `idx` → source-text (`text_raw`) map, used by [`render_human`] to show the
/// first source line of each formal segment (M12-AC7).
fn cue_source_texts(cues: &[crate::domain::Cue]) -> std::collections::BTreeMap<u32, String> {
    cues.iter().map(|c| (c.idx, c.text_raw.clone())).collect()
}

/// Renders the document as pretty JSON (`docs/DATA_MODEL.md` §6).
pub fn render_json(value: &Value) -> Result<String, ReviewError> {
    serde_json::to_string_pretty(value).map_err(|e| ReviewError::Malformed(e.to_string()))
}

/// Renders the human `show` view with `unresolved_ambiguities` first (M5-AC4),
/// then premise, tone, characters, address matrix, glossary, scenes, style rules.
pub fn render_human(value: &Value, cue_texts: &std::collections::BTreeMap<u32, String>) -> String {
    let mut out = String::new();

    // M5-AC4: unresolved ambiguities are shown first.
    out.push_str("Unresolved ambiguities:\n");
    let ambiguities = string_array(value, "unresolved_ambiguities");
    if ambiguities.is_empty() {
        out.push_str("  (none)\n");
    } else {
        for item in ambiguities {
            out.push_str(&format!("  - {item}\n"));
        }
    }

    out.push_str(&section(
        "Premise",
        str_field(value, "premise").unwrap_or(""),
    ));
    out.push_str(&section("Tone", str_field(value, "tone").unwrap_or("")));

    out.push_str("Characters:\n");
    for ch in array(value, "characters") {
        let id = str_field(ch, "id").unwrap_or("?");
        let name = str_field(ch, "name").unwrap_or("");
        let gender = str_field(ch, "gender").unwrap_or("");
        let age = str_field(ch, "age_group").unwrap_or("");
        out.push_str(&format!("  [{id}] {name} ({gender}, {age})\n"));
    }

    out.push_str("Address matrix:\n");
    for entry in array(value, "address_matrix") {
        let from = str_field(entry, "from").unwrap_or("?");
        let to = str_field(entry, "to").unwrap_or("?");
        let register = str_field(entry, "register").unwrap_or("");
        out.push_str(&format!("  {from} -> {to} ({register})\n"));
    }

    out.push_str("Glossary:\n");
    for entry in array(value, "glossary") {
        let term = str_field(entry, "term").unwrap_or("");
        let target = str_field(entry, "target").unwrap_or("");
        out.push_str(&format!("  {term} => {target}\n"));
    }

    out.push_str("Scenes:\n");
    for scene in array(value, "scenes") {
        let summary = str_field(scene, "summary").unwrap_or("");
        let range = scene
            .get("cue_range")
            .and_then(|r| r.as_array())
            .map(|r| {
                let a = r.first().and_then(Value::as_i64).unwrap_or_default();
                let b = r.get(1).and_then(Value::as_i64).unwrap_or_default();
                format!("[{a},{b}]")
            })
            .unwrap_or_default();
        out.push_str(&format!("  {range} {summary}\n"));
    }

    // M12-AC7: formal segments with their cue ranges, kind, and the first source line
    // of each range, so the reviewer can confirm or correct them (ADR-049(b)).
    out.push_str("Formal segments:\n");
    for seg in array(value, "formal_segments") {
        let kind = str_field(seg, "kind").unwrap_or("");
        let (a, b) = cue_range_bounds(seg);
        let first_line = first_source_line(cue_texts, a, b).unwrap_or_default();
        out.push_str(&format!("  [{a},{b}] ({kind}) {first_line}\n"));
    }

    out.push_str("Style rules:\n");
    for item in string_array(value, "style_rules") {
        out.push_str(&format!("  - {item}\n"));
    }

    out
}

/// The `(a, b)` of a `cue_range` array field, defaulting each missing element to 0.
fn cue_range_bounds(value: &Value) -> (i64, i64) {
    value
        .get("cue_range")
        .and_then(|r| r.as_array())
        .map(|r| {
            let a = r.first().and_then(Value::as_i64).unwrap_or_default();
            let b = r.get(1).and_then(Value::as_i64).unwrap_or_default();
            (a, b)
        })
        .unwrap_or_default()
}

/// The first source line in the inclusive cue range `[a, b]` — the lowest cue idx in
/// range present in `cue_texts` (M12-AC7).
fn first_source_line(
    cue_texts: &std::collections::BTreeMap<u32, String>,
    a: i64,
    b: i64,
) -> Option<String> {
    cue_texts
        .range(u32::try_from(a.max(0)).ok()?..)
        .find(|(idx, _)| i64::from(**idx) <= b)
        .map(|(_, text)| text.clone())
}

fn section(title: &str, body: &str) -> String {
    format!("{title}:\n  {body}\n")
}

// ---------------------------------------------------------------------------
// export / import / edit / approve
// ---------------------------------------------------------------------------

/// Exports the current analysis as pretty JSON to `path` via the crash-safe
/// atomic writer (`docs/ARCHITECTURE.md` §6 rule 4, `docs/DATA_MODEL.md` §6). The
/// filesystem write runs on the blocking pool so it never stalls an async runtime
/// worker (`docs/ARCHITECTURE.md` §7).
pub async fn export(store: &Store, path: &Path) -> Result<(), ReviewError> {
    let value = load_current_value(store).await?;
    let bytes = render_json(&value)?.into_bytes();
    let path = path.to_path_buf();
    on_blocking(move || export_to_path(&path, &bytes)).await
}

/// Edits the current analysis in the editor and imports the result (`docs/CLI.md`
/// §6). The current document is written to a scratch file in `work_dir`, opened in
/// the editor, and the edited contents run through [`import_doc`] — so a valid edit
/// creates a new version and an invalid edit is rejected with field locations,
/// leaving the stored analysis untouched (M5-AC3).
///
/// The editor round-trip (scratch write + `$EDITOR` spawn + read-back) runs on the
/// blocking pool via [`tokio::task::spawn_blocking`]: it does blocking filesystem
/// I/O and waits on a human-duration editor process, so it must never run on an
/// async runtime worker (`docs/ARCHITECTURE.md` §7). The launcher is owned (`Send +
/// 'static`) so it can move onto the blocking task.
pub async fn edit<L>(store: &Store, launcher: L, work_dir: &Path) -> Result<i64, ReviewError>
where
    L: EditorLauncher + Send + 'static,
{
    let value = load_current_value(store).await?;
    let bytes = render_json(&value)?.into_bytes();
    let work_dir = work_dir.to_path_buf();
    let edited =
        on_blocking(move || capture_edit(&launcher, &work_dir, "analysis", &bytes)).await?;
    import_doc(store, &edited).await
}

/// Runs a blocking closure on the Tokio blocking pool, mapping a join failure
/// (only a task panic or runtime shutdown) to an I/O error — mirroring the
/// convention in `pipeline::input` (`docs/ARCHITECTURE.md` §7).
async fn on_blocking<F, T>(f: F) -> Result<T, ReviewError>
where
    F: FnOnce() -> Result<T, ReviewError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(join) => Err(ReviewError::Io {
            path: std::path::PathBuf::new(),
            source: std::io::Error::other(join),
        }),
    }
}

/// Validates `content` fully and, only if there are no violations, applies it as a
/// new `origin = user`, unapproved analysis version in one transaction
/// (`docs/ARCHITECTURE.md` §6 rule 7). Returns the new version number.
///
/// Validation collects **every** violation with its field location (M5-AC3); on
/// any violation the whole import is rejected and nothing is written. References
/// (`from`/`to`/`speakers`) are checked against the character ids in the imported
/// document, and scene cue ranges against the project's existing cue indices
/// (`docs/DATA_MODEL.md` §3). Provenance (`source_digest`, `signature`) is carried
/// forward from the analysis being edited and kept in columns, not the JSON.
pub async fn import_doc(store: &Store, content: &str) -> Result<i64, ReviewError> {
    let value: Value =
        serde_json::from_str(content).map_err(|e| ReviewError::Parse(e.to_string()))?;

    // One short read: the cue-index bounds (for scene ranges) and the current
    // version's provenance (to carry forward). A missing current version means
    // there is nothing to edit.
    let (bounds, provenance) = store
        .call(|conn| {
            let bounds = cue_index_bounds(&load_cues(conn)?);
            let provenance = match current_analysis(conn)? {
                Some(current) => get_analysis_version(conn, current.version)?
                    .map(|v| (v.source_digest, v.signature)),
                None => None,
            };
            Ok((bounds, provenance))
        })
        .await?;
    let (source_digest, signature) = provenance.ok_or(ReviewError::NoCurrentAnalysis)?;

    let violations = validate(&value, bounds);
    if !violations.is_empty() {
        return Err(ReviewError::Invalid(violations));
    }

    // Normalise the document (strip incidental whitespace) before storing.
    let doc = serde_json::to_string(&value).map_err(|e| ReviewError::Malformed(e.to_string()))?;

    let version = store
        .call(move |conn| {
            let tx = conn.transaction()?;
            let version = insert_analysis_version_in_tx(
                &tx,
                &doc,
                AnalysisOrigin::User,
                &source_digest,
                &signature,
            )?;
            // The new unapproved version returns the review stage to `completed`
            // and re-blocks translation (`docs/ARCHITECTURE.md` §2).
            set_stage(&tx, Stage::AnalysisReview, StageStatus::Completed)?;
            tx.commit()?;
            Ok(version)
        })
        .await?;
    Ok(version)
}

/// Approves the current analysis version (`docs/ARCHITECTURE.md` §2: "Approve =
/// mark current analysis version approved"). Also moves the analysis-review stage
/// to `approved`, both in one transaction. Returns the approved version number, or
/// [`ReviewError::NoCurrentAnalysis`] (exit 3) when there is nothing to approve.
///
/// **Not for production gate approval.** This approves WITHOUT running selective
/// invalidation (`docs/LLM.md` §7), so using it to clear the gate could reintroduce
/// the approved-but-not-invalidated bug. The `analysis approve` command and `run`'s
/// auto-approve path both clear the gate via `pipeline::approve_and_invalidate`, which
/// approves and marks the affected translations stale **atomically** (ADR-041). This
/// function is retained only to exercise the review-stage approval mechanics
/// (versioning, gate status, origin) in tests.
pub async fn approve(store: &Store) -> Result<i64, ReviewError> {
    let version = store
        .call(|conn| {
            let Some(current) = current_analysis(conn)? else {
                return Ok(None);
            };
            let tx = conn.transaction()?;
            approve_current_analysis_in_tx(&tx)?;
            // A human ran `analysis approve`, so the gate's origin is `user`
            // (`docs/CLI.md` §4, ADR-019).
            set_stage_with_origin(
                &tx,
                Stage::AnalysisReview,
                StageStatus::Approved,
                Some("user"),
            )?;
            tx.commit()?;
            Ok(Some(current.version))
        })
        .await?;
    version.ok_or(ReviewError::NoCurrentAnalysis)
}

/// Loads the current analysis document as a [`Value`], or
/// [`ReviewError::NoCurrentAnalysis`] when none exists.
async fn load_current_value(store: &Store) -> Result<Value, ReviewError> {
    let doc = store
        .call(|conn| {
            let Some(current) = current_analysis(conn)? else {
                return Ok(None);
            };
            Ok(get_analysis_version(conn, current.version)?.map(|v| v.doc))
        })
        .await?
        .ok_or(ReviewError::NoCurrentAnalysis)?;
    serde_json::from_str(&doc).map_err(|e| ReviewError::Malformed(e.to_string()))
}

// ---------------------------------------------------------------------------
// Validation (`docs/DATA_MODEL.md` §3)
// ---------------------------------------------------------------------------

/// Inclusive `(min, max)` of the existing cue indices, or `None` when there are no
/// cues. Used to bound scene cue ranges (`docs/DATA_MODEL.md` §3).
fn cue_index_bounds(cues: &[crate::domain::Cue]) -> Option<(u32, u32)> {
    let min = cues.iter().map(|c| c.idx).min()?;
    let max = cues.iter().map(|c| c.idx).max()?;
    Some((min, max))
}

/// Validates an imported analysis document against the `docs/DATA_MODEL.md` §3
/// rules, collecting every violation with its field location (M5-AC3). An empty
/// result means the document may be applied.
fn validate(value: &Value, bounds: Option<(u32, u32)>) -> Vec<Violation> {
    let mut violations = Vec::new();

    let Some(obj) = value.as_object() else {
        violations.push(Violation::new(
            "<root>",
            "the analysis document must be a JSON object",
        ));
        return violations;
    };

    reject_unknown(obj.keys(), TOP_FIELDS, "<root>", &mut violations);

    check_schema_version(obj, &mut violations);
    check_string(obj, "premise", 500, true, "<root>", &mut violations);
    check_string(obj, "tone", 160, false, "<root>", &mut violations);
    // `narration_register`, when present, must be "spoken" or "literary" (ADR-049(b)).
    if obj.contains_key("narration_register") {
        check_enum(
            obj,
            "narration_register",
            &["spoken", "literary"],
            "<root>",
            &mut violations,
        );
    }

    let char_ids = validate_characters(obj, &mut violations);
    validate_address_matrix(obj, &char_ids, &mut violations);
    validate_glossary(obj, &mut violations);
    validate_scenes(obj, &char_ids, bounds, &mut violations);
    validate_formal_segments(obj, bounds, &mut violations);
    validate_string_list(
        obj,
        "unresolved_ambiguities",
        CAP_AMBIGUITIES,
        220,
        &mut violations,
    );
    validate_string_list(obj, "style_rules", CAP_STYLE_RULES, 180, &mut violations);

    violations
}

fn check_schema_version(obj: &serde_json::Map<String, Value>, violations: &mut Vec<Violation>) {
    match obj.get("schema_version") {
        Some(Value::Number(n)) if n.as_i64() == Some(1) => {}
        Some(Value::Number(_)) => violations.push(Violation::new(
            "schema_version",
            "unsupported schema_version (expected 1)",
        )),
        Some(_) => violations.push(Violation::new("schema_version", "must be the integer 1")),
        None => violations.push(Violation::new("schema_version", "is required")),
    }
}

fn validate_characters(
    obj: &serde_json::Map<String, Value>,
    violations: &mut Vec<Violation>,
) -> BTreeSet<String> {
    const KNOWN: &[&str] = &[
        "id",
        "name",
        "name_target",
        "aliases",
        "aliases_target",
        "gender",
        "age_group",
        "role",
        "speech_style",
    ];
    const GENDERS: &[&str] = &["m", "f", "unknown"];
    const AGES: &[&str] = &["child", "young", "adult", "elder", "unknown"];

    let mut ids = BTreeSet::new();
    let Some(items) = require_array(obj, "characters", CAP_CHARACTERS, true, violations) else {
        return ids;
    };

    for (i, item) in items.iter().enumerate() {
        let loc = format!("characters[{i}]");
        let Some(ch) = object_at(item, &loc, violations) else {
            continue;
        };
        reject_unknown(ch.keys(), KNOWN, &loc, violations);

        if let Some(id) = check_required_string(ch, "id", 120, &loc, violations) {
            ids.insert(id);
        }
        check_required_string(ch, "name", 120, &loc, violations);
        // `name_target` is required, ≤ 120, and must contain Myanmar code points
        // (`docs/DATA_MODEL.md` §3, ADR-049(d)).
        if let Some(target) = check_required_string(ch, "name_target", 120, &loc, violations)
            && !has_myanmar(&target)
        {
            violations.push(Violation::new(
                field_loc(&loc, "name_target"),
                "must contain Myanmar (Burmese) characters",
            ));
        }
        check_enum(ch, "gender", GENDERS, &loc, violations);
        check_enum(ch, "age_group", AGES, &loc, violations);
        check_string(ch, "role", 260, false, &loc, violations);
        check_string(ch, "speech_style", 180, false, &loc, violations);
        check_optional_string_list(ch, "aliases", CAP_ALIASES, 80, &loc, violations);
        validate_aliases_target(ch, &loc, violations);
    }
    ids
}

/// Validates an optional `aliases_target` (`docs/DATA_MODEL.md` §3, ADR-049(d)): when
/// present it must be an array whose length equals `aliases` (so the two run parallel),
/// each entry ≤ 120 chars and containing Myanmar code points.
fn validate_aliases_target(
    ch: &serde_json::Map<String, Value>,
    parent: &str,
    violations: &mut Vec<Violation>,
) {
    let loc = field_loc(parent, "aliases_target");
    let Some(value) = ch.get("aliases_target") else {
        return;
    };
    let Some(items) = value.as_array() else {
        violations.push(Violation::new(loc, "must be an array"));
        return;
    };
    let aliases_len = ch
        .get("aliases")
        .and_then(Value::as_array)
        .map(Vec::len)
        .unwrap_or(0);
    if items.len() != aliases_len {
        violations.push(Violation::new(
            &loc,
            format!(
                "has {} entries but aliases has {aliases_len} (they must run parallel)",
                items.len()
            ),
        ));
    }
    for (i, item) in items.iter().enumerate() {
        let iloc = format!("{loc}[{i}]");
        match item.as_str() {
            Some(s) if s.chars().count() > 120 => {
                violations.push(Violation::new(iloc, "exceeds 120 characters"));
            }
            Some(s) if !has_myanmar(s) => {
                violations.push(Violation::new(
                    iloc,
                    "must contain Myanmar (Burmese) characters",
                ));
            }
            Some(_) => {}
            None => violations.push(Violation::new(iloc, "must be a string")),
        }
    }
}

/// True when `s` contains at least one Myanmar code point in any of the three ranges
/// used for Burmese: main (U+1000–U+109F), Extended-A (U+AA60–U+AA7F), Extended-B
/// (U+A9E0–U+A9FF). Kept local so the review layer carries no dependency on `quality`
/// (`docs/ARCHITECTURE.md` §3, ADR-049(d)).
fn has_myanmar(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(c as u32,
            0x1000..=0x109F | 0xAA60..=0xAA7F | 0xA9E0..=0xA9FF)
    })
}

fn validate_address_matrix(
    obj: &serde_json::Map<String, Value>,
    char_ids: &BTreeSet<String>,
    violations: &mut Vec<Violation>,
) {
    const KNOWN: &[&str] = &[
        "from",
        "to",
        "relation",
        "self_ref",
        "addressee_ref",
        "honorific",
        "register",
        "particle",
    ];
    const REGISTERS: &[&str] = &["colloquial", "polite", "formal"];

    let Some(items) = optional_array(obj, "address_matrix", CAP_ADDRESS_MATRIX, violations) else {
        return;
    };
    for (i, item) in items.iter().enumerate() {
        let loc = format!("address_matrix[{i}]");
        let Some(entry) = object_at(item, &loc, violations) else {
            continue;
        };
        reject_unknown(entry.keys(), KNOWN, &loc, violations);

        check_reference(entry, "from", char_ids, &loc, violations);
        check_reference(entry, "to", char_ids, &loc, violations);
        check_enum(entry, "register", REGISTERS, &loc, violations);
        check_string(entry, "relation", 160, false, &loc, violations);
        check_string(entry, "honorific", 20, false, &loc, violations);
    }
}

fn validate_glossary(obj: &serde_json::Map<String, Value>, violations: &mut Vec<Violation>) {
    const KNOWN: &[&str] = &["term", "target", "keep", "note"];

    let Some(items) = optional_array(obj, "glossary", CAP_GLOSSARY, violations) else {
        return;
    };
    for (i, item) in items.iter().enumerate() {
        let loc = format!("glossary[{i}]");
        let Some(entry) = object_at(item, &loc, violations) else {
            continue;
        };
        reject_unknown(entry.keys(), KNOWN, &loc, violations);

        let term = check_required_string(entry, "term", 120, &loc, violations);
        let target = check_required_string(entry, "target", 120, &loc, violations);
        check_string(entry, "note", 180, false, &loc, violations);

        // `keep: true` implies `target == term` (`docs/DATA_MODEL.md` §3).
        match entry.get("keep") {
            Some(Value::Bool(true)) => {
                if term != target {
                    violations.push(Violation::new(
                        &loc,
                        "keep=true requires target to equal term",
                    ));
                }
            }
            Some(Value::Bool(false)) | None => {}
            Some(_) => violations.push(Violation::new(format!("{loc}.keep"), "must be a boolean")),
        }
    }
}

fn validate_scenes(
    obj: &serde_json::Map<String, Value>,
    char_ids: &BTreeSet<String>,
    bounds: Option<(u32, u32)>,
    violations: &mut Vec<Violation>,
) {
    const KNOWN: &[&str] = &["cue_range", "summary", "speakers", "mood"];

    let Some(items) = require_array(obj, "scenes", CAP_SCENES, true, violations) else {
        return;
    };

    // Collected ascending, in-bounds ranges for the overlap check.
    let mut ranges: Vec<(usize, i64, i64)> = Vec::new();

    for (i, item) in items.iter().enumerate() {
        let loc = format!("scenes[{i}]");
        let Some(scene) = object_at(item, &loc, violations) else {
            continue;
        };
        reject_unknown(scene.keys(), KNOWN, &loc, violations);
        check_required_string(scene, "summary", 300, &loc, violations);
        check_string(scene, "mood", 80, false, &loc, violations);

        // speakers reference existing character ids.
        if let Some(speakers) = scene.get("speakers") {
            match speakers.as_array() {
                Some(list) => {
                    for (s, sp) in list.iter().enumerate() {
                        let sloc = format!("{loc}.speakers[{s}]");
                        match sp.as_str() {
                            Some(id) if char_ids.contains(id) => {}
                            Some(id) => violations.push(Violation::new(
                                &sloc,
                                format!("references unknown character '{id}'"),
                            )),
                            None => violations
                                .push(Violation::new(&sloc, "must be a character id string")),
                        }
                    }
                }
                None => violations.push(Violation::new(
                    format!("{loc}.speakers"),
                    "must be an array",
                )),
            }
        }

        if let Some((a, b)) = validate_cue_range(scene, &loc, bounds, violations) {
            ranges.push((i, a, b));
        }
    }

    // Non-overlapping (`docs/DATA_MODEL.md` §3). Compare every valid pair and
    // report the later scene as overlapping the earlier one.
    for x in 0..ranges.len() {
        for y in (x + 1)..ranges.len() {
            let (ix, ax, bx) = ranges[x];
            let (iy, ay, by) = ranges[y];
            if ax <= by && ay <= bx {
                let (first, second) = if ix <= iy { (ix, iy) } else { (iy, ix) };
                violations.push(Violation::new(
                    format!("scenes[{second}].cue_range"),
                    format!("overlaps scenes[{first}]"),
                ));
            }
        }
    }
}

/// Validates `formal_segments` on user import (`docs/DATA_MODEL.md` §3, ADR-049(b)):
/// an optional array (cap 60); each item has only the known fields `{cue_range,
/// register, kind, note}`, `register` must equal `"literary"`, `kind` is one of the
/// six enum values (unknown rejected), `note` ≤ 120; each `cue_range` lies within the
/// existing cue indices and is ascending; and across entries the ranges must be
/// ascending and non-overlapping. Mirrors `validate_scenes`, reusing
/// [`validate_cue_range`] for the per-entry range check.
fn validate_formal_segments(
    obj: &serde_json::Map<String, Value>,
    bounds: Option<(u32, u32)>,
    violations: &mut Vec<Violation>,
) {
    const KNOWN: &[&str] = &["cue_range", "register", "kind", "note"];
    const KINDS: &[&str] = &[
        "reading",
        "broadcast",
        "speech",
        "recitation",
        "onscreen_text",
        "other",
    ];

    let Some(items) = optional_array(obj, "formal_segments", CAP_FORMAL_SEGMENTS, violations)
    else {
        return;
    };

    // Collected ascending, in-bounds ranges (in document order) for the ascending /
    // non-overlapping cross-entry check.
    let mut ranges: Vec<(usize, i64, i64)> = Vec::new();

    for (i, item) in items.iter().enumerate() {
        let loc = format!("formal_segments[{i}]");
        let Some(seg) = object_at(item, &loc, violations) else {
            continue;
        };
        reject_unknown(seg.keys(), KNOWN, &loc, violations);

        // `register` must be exactly "literary" (the only allowed value).
        check_enum(seg, "register", &["literary"], &loc, violations);
        check_enum(seg, "kind", KINDS, &loc, violations);
        check_string(seg, "note", 120, false, &loc, violations);

        if let Some((a, b)) = validate_cue_range(seg, &loc, bounds, violations) {
            ranges.push((i, a, b));
        }
    }

    // Across entries: ascending and non-overlapping. Compare consecutive valid entries
    // in document order; each must start strictly after the previous one ends.
    for w in ranges.windows(2) {
        let (_, _, prev_b) = w[0];
        let (iy, next_a, _) = w[1];
        if next_a <= prev_b {
            violations.push(Violation::new(
                format!("formal_segments[{iy}].cue_range"),
                "must be ascending and non-overlapping across entries",
            ));
        }
    }
}

/// Validates one scene's `cue_range`, returning the ascending `(a, b)` when it is
/// well formed and in bounds (so the caller can overlap-check it).
fn validate_cue_range(
    scene: &serde_json::Map<String, Value>,
    loc: &str,
    bounds: Option<(u32, u32)>,
    violations: &mut Vec<Violation>,
) -> Option<(i64, i64)> {
    let field = format!("{loc}.cue_range");
    let Some(arr) = scene.get("cue_range") else {
        violations.push(Violation::new(field, "is required"));
        return None;
    };
    let Some(arr) = arr.as_array() else {
        violations.push(Violation::new(field, "must be a two-element array"));
        return None;
    };
    if arr.len() != 2 {
        violations.push(Violation::new(field, "must have exactly two elements"));
        return None;
    }
    let (Some(a), Some(b)) = (arr[0].as_i64(), arr[1].as_i64()) else {
        violations.push(Violation::new(field, "must be two integers"));
        return None;
    };

    if a > b {
        violations.push(Violation::new(field, format!("[{a},{b}] is not ascending")));
        return None;
    }
    match bounds {
        Some((min, max)) => {
            if a < i64::from(min) || b > i64::from(max) {
                violations.push(Violation::new(
                    field,
                    format!("[{a},{b}] is outside existing cue indices [{min},{max}]"),
                ));
                return None;
            }
        }
        None => {
            violations.push(Violation::new(
                field,
                format!("[{a},{b}] is outside existing cue indices (there are no cues)"),
            ));
            return None;
        }
    }
    Some((a, b))
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

/// Pushes an "unknown field" violation for each key not in `allowed`.
fn reject_unknown<'a, I>(keys: I, allowed: &[&str], loc: &str, violations: &mut Vec<Violation>)
where
    I: IntoIterator<Item = &'a String>,
{
    for key in keys {
        if !allowed.contains(&key.as_str()) {
            violations.push(Violation::new(loc, format!("unknown field '{key}'")));
        }
    }
}

/// Returns the object at `value`, or records a violation and returns `None`.
fn object_at<'a>(
    value: &'a Value,
    loc: &str,
    violations: &mut Vec<Violation>,
) -> Option<&'a serde_json::Map<String, Value>> {
    match value.as_object() {
        Some(obj) => Some(obj),
        None => {
            violations.push(Violation::new(loc, "must be an object"));
            None
        }
    }
}

/// Validates a required array field within its cap, returning the items the caller
/// should walk. An over-cap array records one violation and then only its first
/// `cap` elements are returned, so validation work stays bounded on a pathological
/// (huge) array (the whole import is rejected regardless).
fn require_array<'a>(
    obj: &'a serde_json::Map<String, Value>,
    field: &str,
    cap: usize,
    required: bool,
    violations: &mut Vec<Violation>,
) -> Option<&'a [Value]> {
    match obj.get(field) {
        Some(Value::Array(a)) => {
            if a.len() > cap {
                violations.push(Violation::new(
                    field,
                    format!("{} entries exceeds the cap of {cap}", a.len()),
                ));
            }
            Some(&a[..cap.min(a.len())])
        }
        Some(_) => {
            violations.push(Violation::new(field, "must be an array"));
            None
        }
        None => {
            if required {
                violations.push(Violation::new(field, "is required"));
            }
            None
        }
    }
}

/// Validates an optional array field within its cap, returning its items when
/// present.
fn optional_array<'a>(
    obj: &'a serde_json::Map<String, Value>,
    field: &str,
    cap: usize,
    violations: &mut Vec<Violation>,
) -> Option<&'a [Value]> {
    require_array(obj, field, cap, false, violations)
}

/// Validates an optional string field (length limit, type), present-or-absent.
fn check_string(
    obj: &serde_json::Map<String, Value>,
    field: &str,
    max_len: usize,
    required: bool,
    parent: &str,
    violations: &mut Vec<Violation>,
) {
    let loc = field_loc(parent, field);
    match obj.get(field) {
        Some(Value::String(s)) => {
            if s.chars().count() > max_len {
                violations.push(Violation::new(loc, format!("exceeds {max_len} characters")));
            }
        }
        Some(_) => violations.push(Violation::new(loc, "must be a string")),
        None => {
            if required {
                violations.push(Violation::new(loc, "is required"));
            }
        }
    }
}

/// Validates a required string field and returns its value (owned) when valid.
fn check_required_string(
    obj: &serde_json::Map<String, Value>,
    field: &str,
    max_len: usize,
    parent: &str,
    violations: &mut Vec<Violation>,
) -> Option<String> {
    let loc = field_loc(parent, field);
    match obj.get(field) {
        Some(Value::String(s)) => {
            if s.chars().count() > max_len {
                violations.push(Violation::new(loc, format!("exceeds {max_len} characters")));
            }
            Some(s.clone())
        }
        Some(_) => {
            violations.push(Violation::new(loc, "must be a string"));
            None
        }
        None => {
            violations.push(Violation::new(loc, "is required"));
            None
        }
    }
}

/// Validates a required enum field against `allowed`.
fn check_enum(
    obj: &serde_json::Map<String, Value>,
    field: &str,
    allowed: &[&str],
    parent: &str,
    violations: &mut Vec<Violation>,
) {
    let loc = field_loc(parent, field);
    match obj.get(field) {
        Some(Value::String(s)) if allowed.contains(&s.as_str()) => {}
        Some(Value::String(s)) => violations.push(Violation::new(
            loc,
            format!("invalid value '{s}' (expected {})", allowed.join("|")),
        )),
        Some(_) => violations.push(Violation::new(loc, "must be a string")),
        None => violations.push(Violation::new(loc, "is required")),
    }
}

/// Validates that `field` references an existing character id.
fn check_reference(
    obj: &serde_json::Map<String, Value>,
    field: &str,
    char_ids: &BTreeSet<String>,
    parent: &str,
    violations: &mut Vec<Violation>,
) {
    let loc = field_loc(parent, field);
    match obj.get(field) {
        Some(Value::String(id)) if char_ids.contains(id) => {}
        Some(Value::String(id)) => violations.push(Violation::new(
            loc,
            format!("references unknown character '{id}'"),
        )),
        Some(_) => violations.push(Violation::new(loc, "must be a character id string")),
        None => violations.push(Violation::new(loc, "is required")),
    }
}

/// Validates a top-level optional string array (cap + per-item length).
fn validate_string_list(
    obj: &serde_json::Map<String, Value>,
    field: &str,
    cap: usize,
    max_len: usize,
    violations: &mut Vec<Violation>,
) {
    let Some(items) = optional_array(obj, field, cap, violations) else {
        return;
    };
    for (i, item) in items.iter().enumerate() {
        let loc = format!("{field}[{i}]");
        match item.as_str() {
            Some(s) if s.chars().count() <= max_len => {}
            Some(_) => {
                violations.push(Violation::new(loc, format!("exceeds {max_len} characters")))
            }
            None => violations.push(Violation::new(loc, "must be a string")),
        }
    }
}

/// Validates a nested optional string array inside an object element.
fn check_optional_string_list(
    obj: &serde_json::Map<String, Value>,
    field: &str,
    cap: usize,
    max_len: usize,
    parent: &str,
    violations: &mut Vec<Violation>,
) {
    let loc = field_loc(parent, field);
    let Some(value) = obj.get(field) else {
        return;
    };
    let Some(items) = value.as_array() else {
        violations.push(Violation::new(loc, "must be an array"));
        return;
    };
    if items.len() > cap {
        violations.push(Violation::new(
            &loc,
            format!("{} entries exceeds the cap of {cap}", items.len()),
        ));
    }
    // Bound the per-element work on an over-cap array (the import is rejected
    // regardless); only the first `cap` elements are length-checked.
    let bounded = &items[..cap.min(items.len())];
    for (i, item) in bounded.iter().enumerate() {
        let iloc = format!("{loc}[{i}]");
        match item.as_str() {
            Some(s) if s.chars().count() <= max_len => {}
            Some(_) => violations.push(Violation::new(
                iloc,
                format!("exceeds {max_len} characters"),
            )),
            None => violations.push(Violation::new(iloc, "must be a string")),
        }
    }
}

fn field_loc(parent: &str, field: &str) -> String {
    if parent == "<root>" {
        field.to_string()
    } else {
        format!("{parent}.{field}")
    }
}

// ---------------------------------------------------------------------------
// Render helpers
// ---------------------------------------------------------------------------

fn str_field<'a>(value: &'a Value, field: &str) -> Option<&'a str> {
    value.get(field).and_then(Value::as_str)
}

fn array<'a>(value: &'a Value, field: &str) -> &'a [Value] {
    value
        .get(field)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
}

fn string_array<'a>(value: &'a Value, field: &str) -> Vec<&'a str> {
    array(value, field)
        .iter()
        .filter_map(Value::as_str)
        .collect()
}
