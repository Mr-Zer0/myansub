//! Selective invalidation after an analysis edit (`docs/LLM.md` §7,
//! `docs/REQUIREMENTS.md` FR-5/FR-6).
//!
//! When a new analysis version is approved after translations already exist, the
//! old and new analysis documents are diffed and the affected, **unlocked**
//! translations are marked `stale = 1` so `translate --stale` re-translates exactly
//! them (`docs/LLM.md` §7). `locked` cues are never set stale (`docs/REQUIREMENTS.md`
//! FR-6, `docs/DATA_MODEL.md` §2).
//!
//! The diff is a **pure** function ([`affected_cues`]) over the two parsed analysis
//! documents and the project cues, so each row of the `docs/LLM.md` §7 table is
//! unit-testable in isolation. Approval and the stale marking it drives run in **one**
//! transaction ([`approve_and_invalidate_in_tx`], `docs/ARCHITECTURE.md` §6): a crash
//! or error can never leave an approved-but-not-invalidated state (which the `run`
//! loop would never heal). The two approve sites — the orchestrator's auto-approve
//! path and the `analysis approve` command — call [`approve_and_invalidate`].
//! ([`invalidate_after_approval`] is the stand-alone stale-marking trigger, kept for
//! callers that approved separately.)
//!
//! Placement note (`docs/ARCHITECTURE.md` §3): the diff and the atomic approve path
//! live in `pipeline` because nothing may depend on `pipeline` or `review`; the two
//! approve call sites that trigger it (`pipeline::orchestrator` and `main.rs`) both
//! sit at or above `pipeline` and orchestrate the atomic approve themselves rather
//! than calling into `review`, so neither forces a `review → pipeline` edge. See
//! `docs/DECISIONS.md` ADR-041.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Transaction, params};
use serde::Deserialize;

use crate::domain::Cue;
use crate::store::db::StoreError;
use crate::store::repo_analysis::{approve_current_analysis_in_tx, get_analysis_version};
use crate::store::repo_cues::load_cues;
use crate::store::repo_project::set_stage_with_origin;
use crate::store::repo_translations::{
    count_stale_unlocked, count_translations, cue_id_by_idx, mark_unlocked_stale_in_tx,
};
use crate::store::{Stage, StageStatus, Store};

use super::error::PipelineError;

// ---------------------------------------------------------------------------
// Parsed analysis document (only the §7-relevant fields)
// ---------------------------------------------------------------------------

/// The subset of an analysis document (`docs/DATA_MODEL.md` §3) that selective
/// invalidation reasons about (`docs/LLM.md` §7). Parsing is lenient: unknown and
/// missing fields are ignored (`#[serde(default)]`), so it reads any stored doc.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct AnalysisDoc {
    #[serde(default)]
    pub premise: String,
    #[serde(default)]
    pub tone: String,
    #[serde(default)]
    pub style_rules: Vec<String>,
    #[serde(default)]
    pub glossary: Vec<GlossaryEntry>,
    #[serde(default)]
    pub characters: Vec<Character>,
    #[serde(default)]
    pub address_matrix: Vec<AddressPair>,
    #[serde(default)]
    pub scenes: Vec<Scene>,
}

/// A glossary entry (`docs/DATA_MODEL.md` §3). Equality drives the "changed"
/// detection: a term is affected when its whole entry differs, or it is present in
/// only one document (`docs/LLM.md` §7).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct GlossaryEntry {
    #[serde(default)]
    pub term: String,
    #[serde(default)]
    pub target: String,
    #[serde(default)]
    pub keep: bool,
    #[serde(default)]
    pub note: String,
}

/// A character (`docs/DATA_MODEL.md` §3). Only the five fields `docs/LLM.md` §7
/// lists as triggering invalidation are read — `role` is intentionally absent, so a
/// role-only edit never counts as a character change.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Character {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub gender: String,
    #[serde(default)]
    pub age_group: String,
    #[serde(default)]
    pub speech_style: String,
}

impl Character {
    /// Whether the five invalidation-relevant fields (`docs/LLM.md` §7) are equal.
    fn attrs_eq(&self, other: &Character) -> bool {
        self.name == other.name
            && self.aliases == other.aliases
            && self.gender == other.gender
            && self.age_group == other.age_group
            && self.speech_style == other.speech_style
    }
}

/// An address-matrix entry (`docs/DATA_MODEL.md` §3). A pair is identified by
/// `(from, to)`; equality on the whole entry drives "changed" detection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct AddressPair {
    #[serde(default)]
    pub from: String,
    #[serde(default)]
    pub to: String,
    #[serde(default)]
    pub relation: String,
    #[serde(default)]
    pub self_ref: String,
    #[serde(default)]
    pub addressee_ref: String,
    #[serde(default)]
    pub register: String,
    #[serde(default)]
    pub particle: String,
}

/// A scene (`docs/DATA_MODEL.md` §3). Equality on the whole scene drives the
/// scene-change rule (`docs/LLM.md` §7: a changed scene invalidates its old or new
/// cue range).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct Scene {
    #[serde(default)]
    pub cue_range: Vec<i64>,
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub speakers: Vec<String>,
    #[serde(default)]
    pub mood: String,
}

impl Scene {
    /// The ascending inclusive `(a, b)` cue range, when well formed.
    fn range(&self) -> Option<(i64, i64)> {
        match self.cue_range.as_slice() {
            [a, b] => Some((*a.min(b), *a.max(b))),
            _ => None,
        }
    }
}

/// Parses an analysis document JSON string into the §7-relevant [`AnalysisDoc`].
pub fn parse_analysis_doc(json: &str) -> Result<AnalysisDoc, serde_json::Error> {
    serde_json::from_str(json)
}

// ---------------------------------------------------------------------------
// Pure diff (`docs/LLM.md` §7)
// ---------------------------------------------------------------------------

/// Diffs `old` against `new` and returns the set of cue `idx` values whose
/// translations are affected per the `docs/LLM.md` §7 table. Only translatable cues
/// can be affected (non-translatable cues have no translation). The result ignores
/// `locked`/`stale` state — that is a property of the stored translation, enforced
/// when the set is applied ([`mark_unlocked_stale_in_tx`]).
pub fn affected_cues(old: &AnalysisDoc, new: &AnalysisDoc, cues: &[Cue]) -> BTreeSet<u32> {
    // Normalize set-semantic lists before diffing so a pure reorder (no semantic
    // change) never invalidates — `docs/LLM.md` §7 is about real changes, and
    // re-translating on a reorder is wasted cost. Address pairs are already compared
    // set-wise (keyed by `(from, to)`), and glossary entries by `term`.
    let old = &normalized(old);
    let new = &normalized(new);

    let mut affected = BTreeSet::new();
    let translatable: Vec<&Cue> = cues.iter().filter(|c| c.translatable).collect();

    // Row 1: premise / tone / style_rules → every translatable cue. This is the
    // whole set, so later rows can only re-add the same cues; return early.
    if old.premise != new.premise || old.tone != new.tone || old.style_rules != new.style_rules {
        for c in &translatable {
            affected.insert(c.idx);
        }
        return affected;
    }

    // Scenes consulted by the character and address-matrix rules: a scene listed in
    // either document maps an id to a cue range (`docs/LLM.md` §7 "overlapping scene").
    let all_scenes: Vec<&Scene> = old.scenes.iter().chain(new.scenes.iter()).collect();

    // Row 2: glossary term added / changed / removed → cues whose source contains it.
    for term in changed_glossary_terms(&old.glossary, &new.glossary) {
        add_cues_containing(&translatable, &term, &mut affected);
    }

    // Row 3: character changed → cues mentioning the name/alias, plus the cues of any
    // scene that lists the character id.
    for id in changed_character_ids(&old.characters, &new.characters) {
        for needle in character_needles(&old.characters, &new.characters, &id) {
            add_cues_containing(&translatable, &needle, &mut affected);
        }
        for scene in &all_scenes {
            if scene.speakers.contains(&id) {
                add_cues_in_range(&translatable, scene, &mut affected);
            }
        }
    }

    // Row 4: address_matrix pair changed → cues of any scene listing BOTH characters.
    for (from, to) in changed_address_pairs(&old.address_matrix, &new.address_matrix) {
        for scene in &all_scenes {
            if scene.speakers.contains(&from) && scene.speakers.contains(&to) {
                add_cues_in_range(&translatable, scene, &mut affected);
            }
        }
    }

    // Row 5: scene changed → cues of the OLD or NEW cue range. A scene is "changed"
    // when it appears (as a whole) in only one document.
    for scene in symmetric_difference(&old.scenes, &new.scenes) {
        add_cues_in_range(&translatable, scene, &mut affected);
    }

    affected
}

/// Returns a copy of `doc` with every set-semantic list sorted, so order carries no
/// meaning when diffing (`style_rules`, each character's `aliases`, each scene's
/// `speakers`). Fields whose order is significant (e.g. `cue_range`) are untouched.
fn normalized(doc: &AnalysisDoc) -> AnalysisDoc {
    let mut doc = doc.clone();
    doc.style_rules.sort();
    for ch in &mut doc.characters {
        ch.aliases.sort();
    }
    for scene in &mut doc.scenes {
        scene.speakers.sort();
    }
    doc
}

/// The glossary terms that were added, removed, or whose entry changed, keyed by the
/// term string (`docs/LLM.md` §7).
fn changed_glossary_terms(old: &[GlossaryEntry], new: &[GlossaryEntry]) -> BTreeSet<String> {
    let old_by: BTreeMap<&str, &GlossaryEntry> = old.iter().map(|e| (e.term.as_str(), e)).collect();
    let new_by: BTreeMap<&str, &GlossaryEntry> = new.iter().map(|e| (e.term.as_str(), e)).collect();
    let mut changed = BTreeSet::new();
    for term in old_by.keys().chain(new_by.keys()) {
        if old_by.get(term) != new_by.get(term) {
            changed.insert((*term).to_string());
        }
    }
    changed
}

/// The character ids whose §7 fields changed, or that were added / removed.
fn changed_character_ids(old: &[Character], new: &[Character]) -> BTreeSet<String> {
    let old_by: BTreeMap<&str, &Character> = old.iter().map(|c| (c.id.as_str(), c)).collect();
    let new_by: BTreeMap<&str, &Character> = new.iter().map(|c| (c.id.as_str(), c)).collect();
    let mut changed = BTreeSet::new();
    for id in old_by.keys().chain(new_by.keys()) {
        let differs = match (old_by.get(id), new_by.get(id)) {
            (Some(o), Some(n)) => !o.attrs_eq(n),
            _ => true, // added or removed
        };
        if differs {
            changed.insert((*id).to_string());
        }
    }
    changed
}

/// The names and aliases to search cue sources for, for one changed character id —
/// the union across both documents so a renamed character matches its old and new
/// names (`docs/LLM.md` §7).
fn character_needles(old: &[Character], new: &[Character], id: &str) -> BTreeSet<String> {
    let mut needles = BTreeSet::new();
    for ch in old.iter().chain(new.iter()).filter(|c| c.id == id) {
        if !ch.name.is_empty() {
            needles.insert(ch.name.clone());
        }
        for alias in &ch.aliases {
            if !alias.is_empty() {
                needles.insert(alias.clone());
            }
        }
    }
    needles
}

/// The `(from, to)` pairs that were added, removed, or whose entry changed.
fn changed_address_pairs(old: &[AddressPair], new: &[AddressPair]) -> BTreeSet<(String, String)> {
    let key = |p: &AddressPair| (p.from.clone(), p.to.clone());
    let old_by: BTreeMap<(String, String), &AddressPair> =
        old.iter().map(|p| (key(p), p)).collect();
    let new_by: BTreeMap<(String, String), &AddressPair> =
        new.iter().map(|p| (key(p), p)).collect();
    let mut changed = BTreeSet::new();
    for k in old_by.keys().chain(new_by.keys()) {
        if old_by.get(k) != new_by.get(k) {
            changed.insert(k.clone());
        }
    }
    changed
}

/// The scenes present in exactly one of the two documents (the "changed" scenes).
fn symmetric_difference<'a>(old: &'a [Scene], new: &'a [Scene]) -> Vec<&'a Scene> {
    let mut out: Vec<&Scene> = old.iter().filter(|s| !new.contains(s)).collect();
    out.extend(new.iter().filter(|s| !old.contains(s)));
    out
}

/// Inserts every translatable cue whose source (`text_raw`, the unmasked source)
/// contains `needle`, case-insensitively.
fn add_cues_containing(translatable: &[&Cue], needle: &str, affected: &mut BTreeSet<u32>) {
    if needle.is_empty() {
        return;
    }
    let needle = needle.to_lowercase();
    for c in translatable {
        if c.text_raw.to_lowercase().contains(&needle) {
            affected.insert(c.idx);
        }
    }
}

/// Inserts every translatable cue whose `idx` falls within the scene's cue range.
fn add_cues_in_range(translatable: &[&Cue], scene: &Scene, affected: &mut BTreeSet<u32>) {
    let Some((a, b)) = scene.range() else {
        return;
    };
    for c in translatable {
        let idx = i64::from(c.idx);
        if a <= idx && idx <= b {
            affected.insert(c.idx);
        }
    }
}

// ---------------------------------------------------------------------------
// Trigger: mark affected unlocked translations stale
// ---------------------------------------------------------------------------

/// The outcome of an atomic approve + invalidate (`docs/LLM.md` §7, ADR-041).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApprovalInvalidation {
    /// The analysis version that was approved.
    pub approved_version: i64,
    /// Translations newly marked stale by this approval.
    pub newly_stale: usize,
    /// Total currently-stale, unlocked translations after this approval — the newly
    /// marked plus any left stale by an earlier interrupted run — so a recovery
    /// re-run still signals how many translations are pending re-translation.
    pub stale_unlocked: usize,
}

/// Approves the current (highest-version) analysis and marks the affected unlocked
/// translations stale, **all within the caller's single transaction** (ADR-041).
///
/// Approval and stale-marking are all-or-nothing: a crash or error can never leave an
/// approved-but-not-invalidated state, which the `run` loop would never heal (it would
/// export ignoring the approved edit). `origin` is the review-gate approval origin
/// (`user`/`auto`, `docs/CLI.md` §4). Returns [`None`] when there is no analysis
/// version to approve (so the caller can surface a precondition error).
pub fn approve_and_invalidate_in_tx(
    tx: &Transaction,
    origin: &str,
) -> Result<Option<ApprovalInvalidation>, StoreError> {
    if !approve_current_analysis_in_tx(tx)? {
        return Ok(None);
    }
    // A human (`user`) or `run --auto-approve` (`auto`) cleared the gate (ADR-019).
    set_stage_with_origin(
        tx,
        Stage::AnalysisReview,
        StageStatus::Approved,
        Some(origin),
    )?;

    // The current version is the just-approved one (highest version).
    let approved_version: i64 =
        tx.query_row("SELECT MAX(version) FROM analysis", [], |r| r.get(0))?;
    let newly_stale = invalidate_in_tx(tx, approved_version)?;
    let stale_unlocked = count_stale_unlocked(tx)?;
    Ok(Some(ApprovalInvalidation {
        approved_version,
        newly_stale,
        stale_unlocked,
    }))
}

/// Approves the current analysis version and invalidates the affected unlocked
/// translations in one transaction (ADR-041). The two approve trigger sites (the
/// orchestrator's `auto_approve_analysis` and `main.rs`' `analysis approve` handler)
/// call this so approval and stale-marking are atomic. Returns
/// [`PipelineError::NoAnalysisToApprove`] when there is nothing to approve.
pub async fn approve_and_invalidate(
    store: &Store,
    origin: &str,
) -> Result<ApprovalInvalidation, PipelineError> {
    let origin = origin.to_string();
    let outcome = store
        .call(move |conn| {
            let tx = conn.transaction()?;
            let outcome = approve_and_invalidate_in_tx(&tx, &origin)?;
            tx.commit()?;
            Ok(outcome)
        })
        .await?;
    outcome.ok_or(PipelineError::NoAnalysisToApprove)
}

/// After `approved_version` becomes the approved analysis version, invalidates the
/// translations affected by the change from the version the existing translations
/// were built against (`docs/LLM.md` §7). Returns the number of translations newly
/// marked stale. A no-op (returns 0) when there are no translations, no earlier
/// version to diff against, the two documents are identical, or nothing is affected.
///
/// This is the stand-alone trigger (one transaction of its own). The atomic approve
/// path uses [`approve_and_invalidate_in_tx`] instead, which shares the approval's
/// transaction.
pub async fn invalidate_after_approval(
    store: &Store,
    approved_version: i64,
) -> Result<usize, PipelineError> {
    let marked = store
        .call(move |conn| {
            let tx = conn.transaction()?;
            let marked = invalidate_in_tx(&tx, approved_version)?;
            tx.commit()?;
            Ok(marked)
        })
        .await?;
    Ok(marked)
}

/// The stale-marking half of an approval, within the caller's transaction (no
/// commit). Diffs the just-approved `approved_version` against the version the
/// existing translations were built against and marks the affected unlocked
/// translations stale (`docs/LLM.md` §7), returning how many were newly marked. A
/// no-op (returns 0) when there are no translations, no earlier version to diff
/// against, the two documents are identical, or nothing is affected.
fn invalidate_in_tx(tx: &Transaction, approved_version: i64) -> Result<usize, StoreError> {
    // Nothing to invalidate when no translations exist yet (the first-ever approval
    // before translation).
    if count_translations(tx)? == 0 {
        return Ok(0);
    }

    // Diff against the version the existing translations were built against: the
    // highest `analysis_version` recorded on any translation that is older than the
    // newly approved version. `MAX` over no matching rows is SQL NULL → `None`, so a
    // re-approval of the same version is a no-op.
    let prior: Option<i64> = tx.query_row(
        "SELECT MAX(analysis_version) FROM translations WHERE analysis_version < ?1",
        params![approved_version],
        |row| row.get(0),
    )?;
    let Some(prior_version) = prior else {
        return Ok(0);
    };

    let (Some(new_row), Some(old_row)) = (
        get_analysis_version(tx, approved_version)?,
        get_analysis_version(tx, prior_version)?,
    ) else {
        return Ok(0);
    };
    // Identical documents cannot invalidate anything (cheap fast path).
    if new_row.doc == old_row.doc {
        return Ok(0);
    }

    let old = parse_analysis_doc(&old_row.doc).map_err(parse_err)?;
    let new = parse_analysis_doc(&new_row.doc).map_err(parse_err)?;
    let cues = load_cues(tx)?;
    let affected = affected_cues(&old, &new, &cues);
    if affected.is_empty() {
        return Ok(0);
    }

    // Map affected cue `idx` values to the `cues.id` the translations key on.
    let by_idx = cue_id_by_idx(tx)?;
    let cue_ids: Vec<i64> = affected
        .iter()
        .filter_map(|idx| by_idx.get(idx).copied())
        .collect();

    mark_unlocked_stale_in_tx(tx, &cue_ids)
}

/// Maps a failure to parse a *stored* analysis document (which this crate wrote and
/// validated) to a store-level "unexpected value" error — it indicates corruption,
/// not user input.
fn parse_err(err: serde_json::Error) -> StoreError {
    StoreError::Unexpected(format!("parse stored analysis doc: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cue(idx: u32, text: &str, translatable: bool) -> Cue {
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
            translatable,
        }
    }

    /// Cue 3 is non-translatable; all others are translatable.
    fn base_cues() -> Vec<Cue> {
        vec![
            cue(1, "Hello, Aoi!", true),
            cue(2, "The kingdom is far.", true),
            cue(3, "...", false),
            cue(4, "Goodbye Zed.", true),
            cue(5, "Cara and X-man wave.", true),
        ]
    }

    fn character(id: &str, name: &str, aliases: &[&str]) -> Character {
        Character {
            id: id.to_string(),
            name: name.to_string(),
            aliases: aliases.iter().map(|s| s.to_string()).collect(),
            gender: "unknown".to_string(),
            age_group: "adult".to_string(),
            speech_style: "plain".to_string(),
        }
    }

    fn pair(from: &str, to: &str, register: &str) -> AddressPair {
        AddressPair {
            from: from.to_string(),
            to: to.to_string(),
            register: register.to_string(),
            ..AddressPair::default()
        }
    }

    fn scene(range: [i64; 2], summary: &str, speakers: &[&str]) -> Scene {
        Scene {
            cue_range: range.to_vec(),
            summary: summary.to_string(),
            speakers: speakers.iter().map(|s| s.to_string()).collect(),
            mood: "calm".to_string(),
        }
    }

    fn base_doc() -> AnalysisDoc {
        AnalysisDoc {
            premise: "a tale".to_string(),
            tone: "wistful".to_string(),
            style_rules: vec!["short lines".to_string()],
            glossary: vec![],
            characters: vec![
                character("c1", "Aoi", &[]),
                character("c2", "Ben", &[]),
                character("c3", "Cara", &[]),
                character("c4", "Zed", &[]),
                character("c5", "Xavier", &["X-man"]),
            ],
            address_matrix: vec![pair("c1", "c2", "polite")],
            scenes: vec![
                scene([1, 2], "morning", &["c1", "c2"]),
                scene([4, 5], "evening", &["c3"]),
            ],
        }
    }

    fn idxs(items: &[u32]) -> BTreeSet<u32> {
        items.iter().copied().collect()
    }

    // AC: M8-AC3 (row 1) — a changed premise invalidates ALL translatable cues; the
    // non-translatable cue (idx 3) is never affected.
    #[test]
    fn premise_change_affects_all_translatable_cues() {
        let old = base_doc();
        let mut new = base_doc();
        new.premise = "a different tale".to_string();
        assert_eq!(affected_cues(&old, &new, &base_cues()), idxs(&[1, 2, 4, 5]));
    }

    // AC: M8-AC3 (row 1) — a changed tone invalidates all translatable cues.
    #[test]
    fn tone_change_affects_all_translatable_cues() {
        let old = base_doc();
        let mut new = base_doc();
        new.tone = "cheerful".to_string();
        assert_eq!(affected_cues(&old, &new, &base_cues()), idxs(&[1, 2, 4, 5]));
    }

    // AC: M8-AC3 (row 1) — a changed style rule invalidates all translatable cues.
    #[test]
    fn style_rules_change_affects_all_translatable_cues() {
        let old = base_doc();
        let mut new = base_doc();
        new.style_rules = vec!["long lines".to_string()];
        assert_eq!(affected_cues(&old, &new, &base_cues()), idxs(&[1, 2, 4, 5]));
    }

    // AC: M8-AC3 (row 2) — adding a glossary term invalidates only cues whose source
    // contains it (case-insensitive: "Kingdom" vs "kingdom").
    #[test]
    fn glossary_add_affects_cues_containing_the_term() {
        let old = base_doc();
        let mut new = base_doc();
        new.glossary = vec![GlossaryEntry {
            term: "Kingdom".to_string(),
            target: "နိုင်ငံတော်".to_string(),
            ..GlossaryEntry::default()
        }];
        assert_eq!(affected_cues(&old, &new, &base_cues()), idxs(&[2]));
    }

    // AC: M8-AC3 (row 2) — changing a glossary term's target invalidates cues
    // containing the term.
    #[test]
    fn glossary_change_affects_cues_containing_the_term() {
        let mut old = base_doc();
        old.glossary = vec![GlossaryEntry {
            term: "kingdom".to_string(),
            target: "A".to_string(),
            ..GlossaryEntry::default()
        }];
        let mut new = base_doc();
        new.glossary = vec![GlossaryEntry {
            term: "kingdom".to_string(),
            target: "B".to_string(),
            ..GlossaryEntry::default()
        }];
        assert_eq!(affected_cues(&old, &new, &base_cues()), idxs(&[2]));
    }

    // AC: M8-AC3 (row 2) — removing a glossary term invalidates cues containing it.
    #[test]
    fn glossary_remove_affects_cues_containing_the_term() {
        let mut old = base_doc();
        old.glossary = vec![GlossaryEntry {
            term: "kingdom".to_string(),
            target: "A".to_string(),
            ..GlossaryEntry::default()
        }];
        let new = base_doc();
        assert_eq!(affected_cues(&old, &new, &base_cues()), idxs(&[2]));
    }

    // AC: M8-AC3 (row 3) — a character NAME change invalidates cues mentioning the
    // (old) name; c4 "Zed" is in no scene, isolating the name-mention path.
    #[test]
    fn character_name_change_affects_name_mentions() {
        let old = base_doc();
        let mut new = base_doc();
        new.characters[3] = character("c4", "Yon", &[]);
        assert_eq!(affected_cues(&old, &new, &base_cues()), idxs(&[4]));
    }

    // AC: M8-AC3 (row 3) — a character ALIAS change invalidates cues mentioning an
    // alias; c5 "Xavier"/"X-man" is in no scene, isolating the alias-mention path.
    #[test]
    fn character_alias_change_affects_alias_mentions() {
        let old = base_doc();
        let mut new = base_doc();
        new.characters[4] = character("c5", "Xavier", &["X-dude"]);
        assert_eq!(affected_cues(&old, &new, &base_cues()), idxs(&[5]));
    }

    // AC: M8-AC3 (row 3) — a character gender/age/speech_style change invalidates the
    // cues of every scene that lists the character id (c1 is in scene [1,2]).
    #[test]
    fn character_attr_change_affects_scene_members() {
        let old = base_doc();
        let mut new = base_doc();
        new.characters[0].gender = "f".to_string();
        assert_eq!(affected_cues(&old, &new, &base_cues()), idxs(&[1, 2]));
    }

    // AC: M8-AC3 (row 3) — a role-only edit is NOT a character change (`role` is not
    // among the §7 fields), so it invalidates nothing.
    #[test]
    fn character_role_only_change_is_noop() {
        let with_role = |role: &str| {
            format!(
                r#"{{"schema_version":1,"premise":"p","tone":"t","style_rules":[],
                "glossary":[],"address_matrix":[],"scenes":[],
                "characters":[{{"id":"c1","name":"Aoi","aliases":[],"gender":"f",
                "age_group":"adult","speech_style":"plain","role":"{role}"}}]}}"#
            )
        };
        let old = parse_analysis_doc(&with_role("servant")).expect("old");
        let new = parse_analysis_doc(&with_role("queen")).expect("new");
        assert!(affected_cues(&old, &new, &base_cues()).is_empty());
    }

    // AC: M8-AC3 (row 4) — an address-matrix pair change invalidates the cues of a
    // scene that lists BOTH characters (scene [1,2] lists c1 and c2).
    #[test]
    fn address_pair_change_affects_scene_listing_both() {
        let old = base_doc();
        let mut new = base_doc();
        new.address_matrix = vec![pair("c1", "c2", "formal")];
        assert_eq!(affected_cues(&old, &new, &base_cues()), idxs(&[1, 2]));
    }

    // AC: M8-AC3 (row 4) — a pair change invalidates nothing when no scene lists both
    // characters (no scene lists c1 and c3 together).
    #[test]
    fn address_pair_change_without_shared_scene_is_noop() {
        let old = base_doc();
        let mut new = base_doc();
        new.address_matrix = vec![pair("c1", "c2", "polite"), pair("c1", "c3", "formal")];
        assert!(affected_cues(&old, &new, &base_cues()).is_empty());
    }

    // AC: M8-AC3 (row 5) — a scene summary change invalidates the cues in its range.
    #[test]
    fn scene_summary_change_affects_its_range() {
        let old = base_doc();
        let mut new = base_doc();
        new.scenes[0].summary = "a new summary".to_string();
        assert_eq!(affected_cues(&old, &new, &base_cues()), idxs(&[1, 2]));
    }

    // AC: M8-AC3 (row 5) — a scene range change invalidates the OLD and NEW ranges.
    #[test]
    fn scene_range_change_affects_old_and_new_ranges() {
        let old = base_doc();
        let mut new = base_doc();
        new.scenes[1].cue_range = vec![5, 5];
        assert_eq!(affected_cues(&old, &new, &base_cues()), idxs(&[4, 5]));
    }

    // AC: M8-AC3 (row 5) — a scene speakers change invalidates the cues in its range.
    #[test]
    fn scene_speakers_change_affects_its_range() {
        let old = base_doc();
        let mut new = base_doc();
        new.scenes[0].speakers = vec!["c1".to_string()];
        assert_eq!(affected_cues(&old, &new, &base_cues()), idxs(&[1, 2]));
    }

    // AC: M8-AC3 (no-op) — identical documents invalidate nothing.
    #[test]
    fn identical_documents_affect_nothing() {
        let old = base_doc();
        let new = base_doc();
        assert!(affected_cues(&old, &new, &base_cues()).is_empty());
    }

    // AC: M8-AC3 (row 3) — an age_group-only character change invalidates the cues of
    // every scene that lists the character id (character-attr → scene-member path);
    // c1 is in scene [1,2].
    #[test]
    fn character_age_group_change_affects_scene_members() {
        let old = base_doc();
        let mut new = base_doc();
        new.characters[0].age_group = "elder".to_string();
        assert_eq!(affected_cues(&old, &new, &base_cues()), idxs(&[1, 2]));
    }

    // AC: M8-AC3 (row 3) — a speech_style-only character change invalidates the cues
    // of every scene that lists the character id (character-attr → scene-member path).
    #[test]
    fn character_speech_style_change_affects_scene_members() {
        let old = base_doc();
        let mut new = base_doc();
        new.characters[0].speech_style = "formal".to_string();
        assert_eq!(affected_cues(&old, &new, &base_cues()), idxs(&[1, 2]));
    }

    // AC: M8-AC3 — a pure REORDER of `style_rules` is not a semantic change and
    // invalidates nothing (set-semantics, `docs/LLM.md` §7).
    #[test]
    fn style_rules_reorder_invalidates_nothing() {
        let mut old = base_doc();
        old.style_rules = vec!["alpha".to_string(), "beta".to_string()];
        let mut new = base_doc();
        new.style_rules = vec!["beta".to_string(), "alpha".to_string()];
        assert!(affected_cues(&old, &new, &base_cues()).is_empty());
    }

    // AC: M8-AC3 — a pure REORDER of a scene's `speakers` invalidates nothing.
    #[test]
    fn scene_speakers_reorder_invalidates_nothing() {
        let old = base_doc();
        let mut new = base_doc();
        // base scene [1,2] lists ["c1","c2"]; reverse the order only.
        new.scenes[0].speakers = vec!["c2".to_string(), "c1".to_string()];
        assert!(affected_cues(&old, &new, &base_cues()).is_empty());
    }

    // AC: M8-AC3 — a pure REORDER of a character's `aliases` invalidates nothing.
    #[test]
    fn character_aliases_reorder_invalidates_nothing() {
        let mut old = base_doc();
        old.characters[4] = character("c5", "Xavier", &["X-man", "X"]);
        let mut new = base_doc();
        new.characters[4] = character("c5", "Xavier", &["X", "X-man"]);
        assert!(affected_cues(&old, &new, &base_cues()).is_empty());
    }
}
