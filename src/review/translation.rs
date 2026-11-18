//! Translation-specific review: list, show, set, unlock, edit, export, import, and
//! approve (`docs/CLI.md` §6, `docs/REQUIREMENTS.md` FR-6, `docs/DATA_MODEL.md`
//! §2/§5/§6). Mirrors the structure of [`super::analysis`] (M5-B) and reuses the
//! generic [`super::editor`] flow for the atomic export, the `$EDITOR` round-trip,
//! and the import file read.
//!
//! The translation review file (`docs/DATA_MODEL.md` §6) is pretty-printed JSON. On
//! import only each cue's `target` is read back; every other field (`time`,
//! `speaker`, `source`, `flags`, `locked`, `analysis_version`) is informational and
//! ignored. The `target` is stored and edited in **masked** form — placeholders
//! shown as `⟦n⟧` so the reviewer preserves them (export, M7-B, restores the real
//! tags). The `source` field is likewise shown masked, so its `⟦n⟧` placeholders
//! line up with the target the reviewer must keep intact.
//!
//! Import is strict and all-or-nothing (`docs/ARCHITECTURE.md` §6 rule 7): the file
//! is parsed, the `project_source_hash` is checked, every cue id must exist, and
//! every changed cue's `⟦n⟧` placeholder multiset is validated against its source
//! cue — a mismatch is a **hard reject** naming the cue id (M7-AC2), because a
//! broken placeholder set would corrupt tag restoration on export. Only when the
//! whole file validates are the accepted changes applied in one transaction, each
//! becoming `origin = user, locked = 1` with the previous text written to
//! `translation_history` and the quality flags re-run.

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::domain::{Cue, QualityFlag};
use crate::quality::checks::{self, AnalysisContext, GlossaryTerm, Limits};
use crate::store::repo_analysis::{current_analysis, get_analysis_version};
use crate::store::repo_cues::load_cues;
use crate::store::repo_project::{get_project, get_stage, set_stage_with_origin};
use crate::store::repo_translations::{
    TranslationRow, apply_user_edit_in_tx, cue_id_by_idx, list_translations, set_locked_in_tx,
};
use crate::store::repo_units::count_translation_batches_by_status;
use crate::store::{Stage, StageStatus, Store, UnitStatus};

use super::editor::{EditorLauncher, capture_edit, export_to_path};
use super::error::ReviewError;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Filters for `review list` (`docs/CLI.md` §6). All set filters are ANDed; an
/// all-default value lists every reviewable cue.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewFilters {
    /// Only suspect cues (carrying at least one quality flag).
    pub suspect: bool,
    /// Only failed cues (a translatable cue left untranslated by a failed batch).
    pub failed: bool,
    /// Only stale cues.
    pub stale: bool,
    /// Only cues carrying the given quality flag.
    pub flag: Option<QualityFlag>,
    /// Only cues whose id is in the inclusive range `[a, b]`.
    pub range: Option<(u32, u32)>,
}

/// The `--suspect | --range` selection for `review edit`/`review export`
/// (`docs/CLI.md` §6). The two are mutually exclusive at the CLI layer.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReviewSelection {
    pub suspect: bool,
    pub range: Option<(u32, u32)>,
}

/// One cue's line in a `review list` result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewEntry {
    /// The cue `idx` (1-based, stable), the user-facing id.
    pub id: u32,
    /// `HH:MM:SS.mmm --> HH:MM:SS.mmm`.
    pub time: String,
    pub speaker: Option<String>,
    /// The masked source text (`⟦n⟧` placeholders visible).
    pub source: String,
    /// The stored (masked) translation, or `None` for a failed cue.
    pub target: Option<String>,
    pub flags: Vec<QualityFlag>,
    pub locked: bool,
    pub stale: bool,
    /// True when the cue is translatable but has no translation (a failed batch).
    pub failed: bool,
}

/// The FR-6 counts shown alongside a `review list` (`docs/REQUIREMENTS.md` FR-6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReviewCounts {
    /// Cues carrying at least one quality flag.
    pub suspect: usize,
    /// Cues the reviewer has edited (locked).
    pub fixed: usize,
    /// Cues with no flags and no reviewer edit.
    pub untouched: usize,
}

/// A `review list` result: the filtered entries plus the project-wide counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewListing {
    pub cues: Vec<ReviewEntry>,
    pub counts: ReviewCounts,
}

/// What an import (or edit) applied (`docs/DATA_MODEL.md` §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportSummary {
    /// Cues whose target changed (now `origin = user, locked = 1`).
    pub changed: usize,
    /// Cues whose target matched the stored text and were left as-is.
    pub unchanged: usize,
}

/// Parses a quality-flag name (snake_case, `docs/DATA_MODEL.md` §5) for
/// `review list --flag F`. Returns `None` on an unknown name so the caller can
/// raise a usage error.
pub fn parse_quality_flag(name: &str) -> Option<QualityFlag> {
    serde_json::from_value(serde_json::Value::String(name.to_string())).ok()
}

/// Parses a `--flag F` name, mapping an unknown flag to [`ReviewError::UnknownFlag`]
/// (a usage error, exit 2 — `docs/CLI.md` §7).
pub fn parse_flag(name: &str) -> Result<QualityFlag, ReviewError> {
    parse_quality_flag(name).ok_or_else(|| ReviewError::UnknownFlag(name.to_string()))
}

/// Parses a cue id argument (`review show|set|unlock <ID>`, `docs/CLI.md` §6), a
/// positive integer. A non-numeric value is [`ReviewError::InvalidArgument`] (a
/// usage error, exit 2).
pub fn parse_cue_id(id: &str) -> Result<u32, ReviewError> {
    id.trim().parse::<u32>().map_err(|_| {
        ReviewError::InvalidArgument(format!("cue id must be a positive integer, got '{id}'"))
    })
}

/// Parses an `A-B` inclusive id range (`review list/edit/export --range`,
/// `docs/CLI.md` §6). A malformed or descending range is
/// [`ReviewError::InvalidArgument`] (a usage error, exit 2).
pub fn parse_id_range(spec: &str) -> Result<(u32, u32), ReviewError> {
    let invalid = || {
        ReviewError::InvalidArgument(format!(
            "range must be two ascending integers 'A-B', got '{spec}'"
        ))
    };
    let (a, b) = spec.split_once('-').ok_or_else(invalid)?;
    let a = a.trim().parse::<u32>().map_err(|_| invalid())?;
    let b = b.trim().parse::<u32>().map_err(|_| invalid())?;
    if a <= b { Ok((a, b)) } else { Err(invalid()) }
}

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

/// Lists the reviewable cues matching `filters`, together with the project-wide
/// suspect/fixed/untouched counts (`docs/CLI.md` §6, `docs/REQUIREMENTS.md` FR-6).
/// The counts are over every reviewable cue, not just the filtered subset.
pub async fn list(store: &Store, filters: &ReviewFilters) -> Result<ReviewListing, ReviewError> {
    require_translation_ready(store).await?;
    let snap = load_snapshot(store).await?;
    let all = build_entries(&snap);
    let counts = counts_of(&all);
    let cues = all
        .into_iter()
        .filter(|e| matches_filters(e, filters))
        .collect();
    Ok(ReviewListing { cues, counts })
}

/// Renders one cue's source/target/flags/timing/speaker/locked for `review show`
/// (`docs/CLI.md` §6). An unknown id is [`ReviewError::UnknownCue`].
pub async fn show(store: &Store, id: u32) -> Result<String, ReviewError> {
    require_translation_ready(store).await?;
    let snap = load_snapshot(store).await?;
    let cue = snap
        .cues
        .iter()
        .find(|c| c.idx == id)
        .ok_or(ReviewError::UnknownCue { id })?;
    let entry = entry_for(&snap, cue);
    Ok(render_entry(&entry))
}

/// Sets the translation for one cue and locks it (`review set`, `docs/CLI.md` §6).
/// Same rules as a changed import cue (`docs/DATA_MODEL.md` §6): a placeholder
/// mismatch against the source is a hard reject naming the id (M7-AC2); otherwise
/// the cue becomes `origin = user, locked = 1`, the previous text is written to
/// `translation_history`, and the quality flags are re-run.
pub async fn set_target(
    store: &Store,
    id: u32,
    text: &str,
    limits: &Limits,
) -> Result<(), ReviewError> {
    require_translation_ready(store).await?;
    let snap = load_snapshot(store).await?;
    let cue = snap
        .cues
        .iter()
        .find(|c| c.idx == id)
        .ok_or(ReviewError::UnknownCue { id })?
        .clone();
    let cue_id = *snap
        .idx_to_id
        .get(&id)
        .ok_or(ReviewError::UnknownCue { id })?;

    if checks::placeholder_mismatch(&cue, text) {
        return Err(ReviewError::PlaceholderMismatch { id });
    }
    let ctx = AnalysisSheet::parse(&snap.analysis_doc);
    let flags = ctx.check(&cue, text, limits);
    let fallback_version = snap.fallback_version();
    let text = text.to_string();

    store
        .call(move |conn| {
            let tx = conn.transaction()?;
            apply_user_edit_in_tx(&tx, cue_id, &text, &flags, fallback_version)?;
            tx.commit()?;
            Ok(())
        })
        .await?;
    Ok(())
}

/// Unlocks one cue (`review unlock`, `docs/CLI.md` §6): `locked = 0`. An unknown id
/// is [`ReviewError::UnknownCue`]; a cue that exists but has no translation is a
/// no-op.
pub async fn unlock(store: &Store, id: u32) -> Result<(), ReviewError> {
    require_translation_ready(store).await?;
    let cue_id = store
        .call(move |conn| Ok(cue_id_by_idx(conn)?.get(&id).copied()))
        .await?
        .ok_or(ReviewError::UnknownCue { id })?;
    store
        .call(move |conn| {
            let tx = conn.transaction()?;
            set_locked_in_tx(&tx, cue_id, false)?;
            tx.commit()?;
            Ok(())
        })
        .await?;
    Ok(())
}

/// Exports the (optionally selected) translation review file to `path` via the
/// crash-safe atomic writer (`docs/ARCHITECTURE.md` §6 rule 4, `docs/DATA_MODEL.md`
/// §6). The filesystem write runs on the blocking pool (`docs/ARCHITECTURE.md` §7).
pub async fn export(
    store: &Store,
    path: &Path,
    selection: &ReviewSelection,
) -> Result<(), ReviewError> {
    require_translation_ready(store).await?;
    let snap = load_snapshot(store).await?;
    let bytes = render_review_file(&snap, selection)?.into_bytes();
    let path = path.to_path_buf();
    on_blocking(move || export_to_path(&path, &bytes)).await
}

/// Edits the (optionally selected) translation review file in the editor and
/// imports the result (`review edit`, `docs/CLI.md` §6). The current file is written
/// to a scratch file in `work_dir`, opened in the editor, and the edited contents
/// run through [`import`] — so a valid edit is applied (locked) and an invalid edit
/// is rejected, leaving the stored translations untouched.
///
/// The editor round-trip runs on the blocking pool (`docs/ARCHITECTURE.md` §7); the
/// launcher is owned (`Send + 'static`) so it can move onto the blocking task.
pub async fn edit<L>(
    store: &Store,
    launcher: L,
    work_dir: &Path,
    selection: &ReviewSelection,
    limits: &Limits,
) -> Result<ImportSummary, ReviewError>
where
    L: EditorLauncher + Send + 'static,
{
    require_translation_ready(store).await?;
    let snap = load_snapshot(store).await?;
    let bytes = render_review_file(&snap, selection)?.into_bytes();
    let work_dir = work_dir.to_path_buf();
    let edited =
        on_blocking(move || capture_edit(&launcher, &work_dir, "translation", &bytes)).await?;
    import(store, &edited, limits).await
}

/// Imports a translation review file (`review import`, `docs/DATA_MODEL.md` §6),
/// all-or-nothing (`docs/ARCHITECTURE.md` §6 rule 7). The file is parsed, the
/// `project_source_hash` is checked, every cue id must exist, and every cue whose
/// `target` differs from the stored text has its `⟦n⟧` placeholder multiset
/// validated against its source cue — a mismatch rejects the whole import naming the
/// cue id (M7-AC2). Only when the whole file validates are the changes applied in
/// one transaction: each becomes `origin = user, locked = 1`, the previous text is
/// written to `translation_history`, and the quality flags are re-run.
pub async fn import(
    store: &Store,
    content: &str,
    limits: &Limits,
) -> Result<ImportSummary, ReviewError> {
    require_translation_ready(store).await?;
    let file: ImportFile =
        serde_json::from_str(content).map_err(|e| ReviewError::Parse(e.to_string()))?;
    let snap = load_snapshot(store).await?;

    if file.project_source_hash != snap.source_hash {
        return Err(ReviewError::HashMismatch {
            expected: snap.source_hash.clone(),
            found: file.project_source_hash,
        });
    }

    let ctx = AnalysisSheet::parse(&snap.analysis_doc);
    let fallback_version = snap.fallback_version();

    // Validate the whole file first, collecting the accepted changes. Any unknown id
    // or placeholder mismatch returns early — before any write — so a rejected
    // import applies nothing (all-or-nothing).
    let mut changes: Vec<(i64, String, Vec<QualityFlag>)> = Vec::new();
    let mut unchanged = 0usize;
    for ic in &file.cues {
        let cue = snap
            .cues
            .iter()
            .find(|c| c.idx == ic.id)
            .ok_or(ReviewError::UnknownCue { id: ic.id })?;
        let cue_id = *snap
            .idx_to_id
            .get(&ic.id)
            .ok_or(ReviewError::UnknownCue { id: ic.id })?;
        let stored = snap
            .translations
            .get(&cue_id)
            .map(|t| t.text.as_str())
            .unwrap_or("");
        if ic.target == stored {
            unchanged += 1;
            continue;
        }
        if checks::placeholder_mismatch(cue, &ic.target) {
            return Err(ReviewError::PlaceholderMismatch { id: ic.id });
        }
        let flags = ctx.check(cue, &ic.target, limits);
        changes.push((cue_id, ic.target.clone(), flags));
    }

    let changed = changes.len();
    store
        .call(move |conn| {
            let tx = conn.transaction()?;
            for (cue_id, text, flags) in &changes {
                apply_user_edit_in_tx(&tx, *cue_id, text, flags, fallback_version)?;
            }
            tx.commit()?;
            Ok(())
        })
        .await?;
    Ok(ImportSummary { changed, unchanged })
}

/// Approves the translation-review stage (`review approve`, `docs/ARCHITECTURE.md`
/// §2: approve = `translation_review` approved → output allowed). Requires the
/// translation stage completed ([`ReviewError::TranslationNotReady`] otherwise,
/// exit 3).
pub async fn approve(store: &Store) -> Result<(), ReviewError> {
    require_translation_ready(store).await?;
    store
        .call(|conn| {
            let tx = conn.transaction()?;
            // A human ran `review approve`, so the gate's origin is `user`
            // (`docs/CLI.md` §4, ADR-019).
            set_stage_with_origin(
                &tx,
                Stage::TranslationReview,
                StageStatus::Approved,
                Some("user"),
            )?;
            tx.commit()?;
            Ok(())
        })
        .await?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Precondition
// ---------------------------------------------------------------------------

/// Enforces the translation-review precondition (`docs/ARCHITECTURE.md` §2): the
/// translation stage must be `completed` **and** have no pending batches. Failed
/// batches are allowed — the review lists them (ADR-038). This is the identical
/// predicate the orchestrator's `can_run(Stage::TranslationReview)` evaluates;
/// it is re-expressed here over the store so `review` does not depend on `pipeline`
/// (`docs/ARCHITECTURE.md` §3 forbids `review → pipeline`). Otherwise
/// [`ReviewError::TranslationNotReady`] (exit 3).
async fn require_translation_ready(store: &Store) -> Result<(), ReviewError> {
    let ready = store
        .call(|conn| {
            let completed = matches!(
                get_stage(conn, Stage::Translation)?,
                Some(StageStatus::Completed)
            );
            let pending = count_translation_batches_by_status(conn, UnitStatus::Pending)?;
            Ok(completed && pending == 0)
        })
        .await?;
    if ready {
        Ok(())
    } else {
        Err(ReviewError::TranslationNotReady)
    }
}

// ---------------------------------------------------------------------------
// Snapshot loading
// ---------------------------------------------------------------------------

/// Everything a review command reads in one short DB closure: the cues, the
/// idx→cues.id map, the translations keyed by `cues.id`, the project source hash
/// (for the import hash check), and the current analysis version + document (for
/// re-running the quality checks).
struct Snapshot {
    cues: Vec<Cue>,
    idx_to_id: BTreeMap<u32, i64>,
    translations: BTreeMap<i64, TranslationRow>,
    source_hash: String,
    analysis_version: i64,
    analysis_doc: String,
}

impl Snapshot {
    /// The analysis version to stamp onto a brand-new translation row (a failed
    /// cue the reviewer fills in), falling back to 1 when no analysis exists.
    fn fallback_version(&self) -> i64 {
        if self.analysis_version > 0 {
            self.analysis_version
        } else {
            1
        }
    }
}

async fn load_snapshot(store: &Store) -> Result<Snapshot, ReviewError> {
    let snap = store
        .call(|conn| {
            let cues = load_cues(conn)?;
            let idx_to_id = cue_id_by_idx(conn)?;
            let translations = list_translations(conn)?
                .into_iter()
                .map(|t| (t.cue_id, t))
                .collect();
            let source_hash = get_project(conn)?
                .map(|p| p.source_hash)
                .unwrap_or_default();
            let (analysis_version, analysis_doc) = match current_analysis(conn)? {
                Some(cur) => match get_analysis_version(conn, cur.version)? {
                    Some(v) => (v.version, v.doc),
                    None => (0, String::new()),
                },
                None => (0, String::new()),
            };
            Ok(Snapshot {
                cues,
                idx_to_id,
                translations,
                source_hash,
                analysis_version,
                analysis_doc,
            })
        })
        .await?;
    Ok(snap)
}

// ---------------------------------------------------------------------------
// Entry building, filtering, counts
// ---------------------------------------------------------------------------

/// Builds the review entries for every translatable cue. A translatable cue with no
/// translation row is surfaced as a failed entry (the translation stage is complete,
/// so a missing row means the cue's batch failed), carrying a synthetic
/// [`QualityFlag::Failed`].
fn build_entries(snap: &Snapshot) -> Vec<ReviewEntry> {
    snap.cues
        .iter()
        .filter(|c| c.translatable)
        .map(|c| entry_for(snap, c))
        .collect()
}

/// Builds the review entry for one cue (translatable or not).
fn entry_for(snap: &Snapshot, cue: &Cue) -> ReviewEntry {
    let time = format_time_range(cue.start_ms, cue.end_ms);
    let tr = snap
        .idx_to_id
        .get(&cue.idx)
        .and_then(|cid| snap.translations.get(cid));
    match tr {
        Some(t) => ReviewEntry {
            id: cue.idx,
            time,
            speaker: cue.speaker.clone(),
            source: cue.text_masked.clone(),
            target: Some(t.text.clone()),
            flags: t.flags.clone(),
            locked: t.locked,
            stale: t.stale,
            failed: false,
        },
        None => {
            // A translatable cue with no row is a failed cue; a non-translatable
            // cue is simply untranslated (no synthetic flag).
            let flags = if cue.translatable {
                vec![QualityFlag::Failed]
            } else {
                Vec::new()
            };
            ReviewEntry {
                id: cue.idx,
                time,
                speaker: cue.speaker.clone(),
                source: cue.text_masked.clone(),
                target: None,
                flags,
                locked: false,
                stale: false,
                failed: cue.translatable,
            }
        }
    }
}

fn matches_filters(e: &ReviewEntry, f: &ReviewFilters) -> bool {
    if f.suspect && e.flags.is_empty() {
        return false;
    }
    if f.failed && !e.failed {
        return false;
    }
    if f.stale && !e.stale {
        return false;
    }
    if let Some(flag) = f.flag
        && !e.flags.contains(&flag)
    {
        return false;
    }
    if let Some((a, b)) = f.range
        && (e.id < a || e.id > b)
    {
        return false;
    }
    true
}

fn counts_of(entries: &[ReviewEntry]) -> ReviewCounts {
    let suspect = entries.iter().filter(|e| !e.flags.is_empty()).count();
    let fixed = entries.iter().filter(|e| e.locked).count();
    let untouched = entries
        .iter()
        .filter(|e| e.flags.is_empty() && !e.locked)
        .count();
    ReviewCounts {
        suspect,
        fixed,
        untouched,
    }
}

fn selection_matches(e: &ReviewEntry, sel: &ReviewSelection) -> bool {
    if sel.suspect && e.flags.is_empty() {
        return false;
    }
    if let Some((a, b)) = sel.range
        && (e.id < a || e.id > b)
    {
        return false;
    }
    true
}

// ---------------------------------------------------------------------------
// Review file (de)serialization (`docs/DATA_MODEL.md` §6)
// ---------------------------------------------------------------------------

/// The translation review file written on export (`docs/DATA_MODEL.md` §6).
#[derive(Debug, Serialize)]
struct ReviewFileOut {
    project_source_hash: String,
    analysis_version: i64,
    cues: Vec<ReviewFileCueOut>,
}

#[derive(Debug, Serialize)]
struct ReviewFileCueOut {
    id: u32,
    time: String,
    speaker: Option<String>,
    source: String,
    target: String,
    flags: Vec<QualityFlag>,
    locked: bool,
}

/// The import view of the file: only `project_source_hash` and each cue's `id` and
/// `target` are read (`docs/DATA_MODEL.md` §6); all other fields are ignored (serde
/// drops unknown keys by default), so the reviewer's informational columns do not
/// need to round-trip.
#[derive(Debug, Deserialize)]
struct ImportFile {
    project_source_hash: String,
    #[serde(default)]
    cues: Vec<ImportCue>,
}

#[derive(Debug, Deserialize)]
struct ImportCue {
    id: u32,
    target: String,
}

/// Renders the selected cues as the pretty-printed review file.
fn render_review_file(snap: &Snapshot, selection: &ReviewSelection) -> Result<String, ReviewError> {
    let cues = build_entries(snap)
        .into_iter()
        .filter(|e| selection_matches(e, selection))
        .map(|e| ReviewFileCueOut {
            id: e.id,
            time: e.time,
            speaker: e.speaker,
            source: e.source,
            target: e.target.unwrap_or_default(),
            flags: e.flags,
            locked: e.locked,
        })
        .collect();
    let out = ReviewFileOut {
        project_source_hash: snap.source_hash.clone(),
        analysis_version: snap.analysis_version,
        cues,
    };
    serde_json::to_string_pretty(&out).map_err(|e| ReviewError::Malformed(e.to_string()))
}

/// Renders one cue for `review show` (`docs/CLI.md` §6).
fn render_entry(e: &ReviewEntry) -> String {
    let mut out = String::new();
    out.push_str(&format!("Cue {}\n", e.id));
    out.push_str(&format!("  time:    {}\n", e.time));
    out.push_str(&format!(
        "  speaker: {}\n",
        e.speaker.as_deref().unwrap_or("(none)")
    ));
    out.push_str(&format!("  source:  {}\n", e.source));
    match &e.target {
        Some(t) => out.push_str(&format!("  target:  {t}\n")),
        None => out.push_str("  target:  (not translated)\n"),
    }
    let flags = if e.flags.is_empty() {
        "(none)".to_string()
    } else {
        e.flags
            .iter()
            .map(flag_label)
            .collect::<Vec<_>>()
            .join(", ")
    };
    out.push_str(&format!("  flags:   {flags}\n"));
    out.push_str(&format!("  locked:  {}\n", e.locked));
    out.push_str(&format!("  stale:   {}\n", e.stale));
    out
}

/// The snake_case label of a quality flag (`docs/DATA_MODEL.md` §5).
fn flag_label(flag: &QualityFlag) -> String {
    serde_json::to_value(flag)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_else(|| format!("{flag:?}"))
}

// ---------------------------------------------------------------------------
// Analysis context for re-running quality checks
// ---------------------------------------------------------------------------

/// The slice of the analysis the quality re-check needs: the glossary and the
/// character names/aliases (`docs/LLM.md` §8). Parsed leniently so a sparse analysis
/// never aborts a review edit.
struct AnalysisSheet {
    glossary: Vec<(String, String, bool)>,
    names: Vec<String>,
}

impl AnalysisSheet {
    fn parse(doc: &str) -> AnalysisSheet {
        let value: serde_json::Value = serde_json::from_str(doc).unwrap_or(serde_json::Value::Null);

        let mut names = Vec::new();
        if let Some(chars) = value.get("characters").and_then(|v| v.as_array()) {
            for ch in chars {
                if let Some(name) = ch.get("name").and_then(|v| v.as_str())
                    && !name.is_empty()
                {
                    names.push(name.to_string());
                }
                if let Some(aliases) = ch.get("aliases").and_then(|v| v.as_array()) {
                    for alias in aliases.iter().filter_map(|a| a.as_str()) {
                        if !alias.is_empty() {
                            names.push(alias.to_string());
                        }
                    }
                }
            }
        }

        let mut glossary = Vec::new();
        if let Some(terms) = value.get("glossary").and_then(|v| v.as_array()) {
            for g in terms {
                let term = g.get("term").and_then(|v| v.as_str()).unwrap_or_default();
                let target = g.get("target").and_then(|v| v.as_str()).unwrap_or_default();
                let keep = g.get("keep").and_then(|v| v.as_bool()).unwrap_or(false);
                if !term.is_empty() {
                    glossary.push((term.to_string(), target.to_string(), keep));
                }
            }
        }

        AnalysisSheet { glossary, names }
    }

    /// Re-runs the deterministic quality checks for one edited cue (`docs/LLM.md`
    /// §8). Soft flags (e.g. `too_long`) only update the stored flags; they never
    /// reject — the hard placeholder check is done by the caller before this.
    fn check(&self, source: &Cue, target: &str, limits: &Limits) -> Vec<QualityFlag> {
        let glossary: Vec<GlossaryTerm> = self
            .glossary
            .iter()
            .map(|(term, target, keep)| GlossaryTerm {
                term,
                target,
                keep: *keep,
            })
            .collect();
        let names: Vec<&str> = self.names.iter().map(String::as_str).collect();
        let ctx = AnalysisContext {
            glossary: &glossary,
            names: &names,
        };
        checks::check_cue(source, target, &ctx, limits)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Formats a `start --> end` timecode range as `HH:MM:SS.mmm --> HH:MM:SS.mmm`
/// (`docs/DATA_MODEL.md` §6).
fn format_time_range(start_ms: u64, end_ms: u64) -> String {
    format!(
        "{} --> {}",
        format_timecode(start_ms),
        format_timecode(end_ms)
    )
}

fn format_timecode(ms: u64) -> String {
    let hours = ms / 3_600_000;
    let minutes = (ms % 3_600_000) / 60_000;
    let seconds = (ms % 60_000) / 1000;
    let millis = ms % 1000;
    format!("{hours:02}:{minutes:02}:{seconds:02}.{millis:03}")
}

/// Runs a blocking closure on the Tokio blocking pool, mapping a join failure to an
/// I/O error — mirroring [`super::analysis`] (`docs/ARCHITECTURE.md` §7).
async fn on_blocking<F, T>(f: F) -> Result<T, ReviewError>
where
    F: FnOnce() -> Result<T, ReviewError> + Send + 'static,
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(f).await {
        Ok(result) => result,
        Err(join) => Err(ReviewError::BackgroundTask(join.to_string())),
    }
}
