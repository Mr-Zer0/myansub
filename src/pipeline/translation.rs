//! The translation stage: batching, cache-friendly prompt assembly, response
//! validation with a tier-1 content retry, deterministic quality flagging, and the
//! `--dry-run` estimate (`docs/LLM.md` §2/§3/§6/§8/§9, `docs/ARCHITECTURE.md`
//! §4/§6, `docs/DATA_MODEL.md` §2/§4/§5).
//!
//! The project's translatable cues are split into contiguous batches of
//! `translation.batch_size` cues (`docs/LLM.md` §3); non-translatable cues are
//! never sent. Each batch is a scheduler [`WorkUnit`]: it claims the batch, sends
//! the batch's variable block behind a byte-stable cached prefix (the translation
//! system instruction plus the canonical analysis sheet, ADR-006), validates the
//! `{"lines":[{"id","t"}]}` response, runs the per-cue quality checks, and writes
//! all of the batch's translations **and** the batch's `done` status in one
//! transaction (`docs/ARCHITECTURE.md` §6 rule 1).
//!
//! Resume (`docs/ARCHITECTURE.md` §6, `docs/TESTING.md` §4): batch rows are created
//! once and carry an `input_hash` (sha256 of the variable block, analysis version,
//! model, and `PROMPT_VERSION`; `docs/LLM.md` §3). A rerun rebuilds the units from
//! the persisted rows; a `done` batch is skipped by the scheduler, so a completed
//! batch's request is never sent twice.
//!
//! Content retries (`docs/LLM.md` §6) are counted separately from the transport
//! retries inside `Retrying`. This milestone (M6-B1) implements only tier 1:
//! an unparseable / schema-mismatched response retries the whole batch once with a
//! strict reminder, up to `max_content_attempts`, after which the batch is failed.
//! The id-set and per-line tiers are M6-B2. Quality flags are **stored**, not
//! retried on (M6-B2).

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::domain::{Cue, QualityFlag};
use crate::llm::assemble_stack;
use crate::llm::cache::Cached;
use crate::llm::canonical::canonical_json;
use crate::llm::client::{
    CommitToken, ContentBlock, GenerationConfig, LlmClient, LlmError, LlmRequest, ServiceTier,
    ThinkingSetting,
};
use crate::llm::dry_run::{DryRun, Estimator};
use crate::llm::metered::CallContext;
use crate::llm::pricing::{self, Cost, PriceTable};
use crate::llm::prompts::{
    PROMPT_VERSION, TranslationBlock, TranslationLine, translation_retry_reminder,
    translation_system_instruction,
};
use crate::llm::retry::RetryConfig;
use crate::llm::schemas::translation_response_schema;
use crate::quality::checks::{self, AnalysisContext, GlossaryTerm, Limits};
use crate::store::repo_analysis::{current_analysis, get_analysis_version};
use crate::store::repo_cues::load_cues;
use crate::store::repo_llm::put_cache_in_tx;
use crate::store::repo_project::set_stage;
use crate::store::repo_translations::{
    NewTranslation, TranslationOrigin, cue_id_by_idx, upsert_translation_in_tx,
};
use crate::store::repo_units::{
    NewTranslationBatch, claim_translation_batch, complete_translation_batch_in_tx,
    count_translation_batches_by_status, create_translation_batch, fail_translation_batch,
    get_translation_batch, list_translation_batches_by_status, reset_translation_batch,
};
use crate::store::{Stage, StageStatus, Store, UnitStatus};

use super::error::PipelineError;
use super::input::check_source_unchanged;
use super::orchestrator::{can_run, load_state};
use super::scheduler::{self, SchedulerConfig, UnitOutcome, UnitState, WorkUnit};

/// Per-line JSON overhead (ids, keys, quotes, separators) added to the output
/// budget so a batch's structured `{"id","t"}` array is never truncated.
const OUTPUT_OVERHEAD_PER_LINE: u64 = 32;

/// A fixed base allowance for the surrounding JSON (`{"lines":[…]}`) in the output
/// budget.
const OUTPUT_OVERHEAD_BASE: u64 = 256;

/// The largest `maxOutputTokens` the translation stage will request, within current
/// Gemini output limits so a request is never rejected for an out-of-range budget.
const OUTPUT_CEILING: u64 = 65_536;

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------

/// Everything the translation executor needs that is not the transport, the store,
/// or the cues (`docs/CONFIG.md` `[translation]`). Built from config + CLI
/// overrides by the command handler; held as plain data so the executor and its
/// tests do not depend on the `config` type (mirrors [`AnalysisParams`]).
///
/// [`AnalysisParams`]: super::analysis::AnalysisParams
#[derive(Debug, Clone)]
pub struct TranslationParams {
    /// The translation model (`[llm.models].translation`, overridable by `--model`).
    pub model: String,
    /// The service tier (`[llm].service_tier`, overridable by `--tier`).
    pub tier: ServiceTier,
    /// The translation thinking setting (`[llm.thinking].translation`, default off).
    pub thinking: ThinkingSetting,
    /// Translatable cues per batch (`[translation].batch_size`).
    pub batch_size: u32,
    /// Context source lines before each batch (`[translation].context_before`).
    pub context_before: u32,
    /// Context source lines after each batch (`[translation].context_after`).
    pub context_after: u32,
    /// Content-retry budget (`[translation].max_content_attempts`).
    pub max_content_attempts: u32,
    /// Output-token ratio for the output budget and dry-run estimate
    /// (`[translation].output_ratio`).
    pub output_ratio: f64,
    /// Per-script token estimator (`[estimate]`).
    pub estimator: Estimator,
    /// Deterministic quality-check thresholds (`[quality]`).
    pub limits: Limits,
    /// Scheduler tuning (`[translation].concurrency`/`warm_cache_first`,
    /// `[output].grace_secs`).
    pub scheduler: SchedulerConfig,
    /// Jitter seed for the transport retry backoff.
    pub seed: u64,
    /// Transport retry policy (`[llm]`).
    pub retry: RetryConfig,
}

/// Run shaping flags from the `translate` command (`docs/CLI.md` §6). For M6-B1
/// these are accepted but their behaviours (`--stale`/`--redo`/`--retry-failed`)
/// are deferred to M6-B2.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TranslationFlags {
    pub redo: bool,
    pub retry_failed: bool,
    pub stale: bool,
}

/// Where the translation executor stopped (`docs/CLI.md` §6/§7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TranslationOutcome {
    /// The project has no translatable cues, so there is nothing to translate.
    NothingToDo,
    /// Every batch is `done`.
    Completed { batches: usize },
    /// Some batches failed after their content-retry budget. Maps to exit code 4.
    CompletedWithFailures { failed: usize },
    /// Interrupted by cancellation (Ctrl-C).
    Interrupted,
}

/// The `translate --dry-run` report (`docs/LLM.md` §9, `docs/CLI.md` §6).
/// Estimated, never persisted.
#[derive(Debug, Clone, PartialEq)]
pub struct DryRunReport {
    /// Number of batches the real run would send.
    pub batches: usize,
    /// Total logical calls (one per batch).
    pub calls: usize,
    pub estimated_prompt_tokens: u64,
    pub estimated_output_tokens: u64,
    pub estimated_cost: Cost,
}

/// Maps a [`TranslationOutcome`] to its process exit code (`docs/CLI.md` §7).
/// Cancellation wins (130), then failed units (4), otherwise success (0).
pub fn translation_exit_code(cancelled: bool, outcome: &TranslationOutcome) -> i32 {
    use crate::error::exit;
    if cancelled {
        return exit::INTERRUPTED;
    }
    match outcome {
        TranslationOutcome::Interrupted => exit::INTERRUPTED,
        TranslationOutcome::CompletedWithFailures { .. } => exit::FAILED_UNITS,
        TranslationOutcome::Completed { .. } | TranslationOutcome::NothingToDo => exit::SUCCESS,
    }
}

/// Enforces the translation stage's preconditions and loads the cues it will
/// translate (`docs/ARCHITECTURE.md` §2/§6). This is the command handler's shared
/// front door: it verifies the source has not changed since init (§6 rule 5),
/// checks the translation gate (current analysis approved, [`can_run`]), and
/// returns the cues. A failed gate is a [`PipelineError::GateViolation`] (exit 3);
/// a changed source is [`PipelineError::SourceChanged`] (exit 3).
pub async fn prepare_translation(
    store: &Store,
    source_path: &std::path::Path,
) -> Result<Vec<Cue>, PipelineError> {
    check_source_unchanged(store, source_path).await?;
    let state = load_state(store).await?;
    can_run(Stage::Translation, &state)?;
    let cues = store.call(|conn| load_cues(conn)).await?;
    Ok(cues)
}

// ---------------------------------------------------------------------------
// run_translation
// ---------------------------------------------------------------------------

/// Runs the translation stage over `cues` using `transport` for LLM calls
/// (`docs/ARCHITECTURE.md` §4). Loads the current approved analysis for the cached
/// prefix and scene/speaker context, plans the batches (creating them once),
/// schedules them, and reports where it stopped. The caller supplies a
/// [`CancellationToken`] threaded to the scheduler and every LLM call.
pub async fn run_translation<T>(
    transport: T,
    store: &Store,
    cues: &[Cue],
    params: &TranslationParams,
    flags: TranslationFlags,
    token: &CancellationToken,
) -> Result<TranslationOutcome, PipelineError>
where
    T: LlmClient + Clone + Send + Sync + 'static,
{
    if flags.redo || flags.retry_failed || flags.stale {
        tracing::info!(
            redo = flags.redo,
            retry_failed = flags.retry_failed,
            stale = flags.stale,
            "translate: --redo/--retry-failed/--stale are deferred to M6-B2 and have no effect yet"
        );
    }

    let (analysis_version, sheet) = load_sheet(store).await?;
    let assembled = assemble_all_batches(cues, &sheet, params, analysis_version);
    if assembled.is_empty() {
        return Ok(TranslationOutcome::NothingToDo);
    }

    // Fresh run: create the batch rows (with their input_hash) and mark the stage
    // running, all in one transaction. On a resume the rows already exist and this
    // is skipped, so a `done` batch keeps its status (`docs/ARCHITECTURE.md` §6).
    let model = params.model.clone();
    let rows_for_create: Vec<NewTranslationBatch> = assembled
        .iter()
        .map(|b| NewTranslationBatch {
            batch_no: b.batch_no,
            first_cue: b.first_cue,
            last_cue: b.last_cue,
            input_hash: Some(b.input_hash.clone()),
            model: Some(model.clone()),
            analysis_version: Some(analysis_version),
        })
        .collect();
    store
        .call(move |conn| {
            if list_translation_batches_by_status(conn, UnitStatus::Pending)?.is_empty()
                && count_translation_batches_by_status(conn, UnitStatus::InFlight)? == 0
                && count_translation_batches_by_status(conn, UnitStatus::Done)? == 0
                && count_translation_batches_by_status(conn, UnitStatus::Failed)? == 0
            {
                let tx = conn.transaction()?;
                for batch in &rows_for_create {
                    create_translation_batch(&tx, batch)?;
                }
                set_stage(&tx, Stage::Translation, StageStatus::Running)?;
                tx.commit()?;
            }
            Ok(())
        })
        .await?;

    // The authoritative status for each batch comes from the DB rows.
    let statuses: BTreeMap<i64, UnitStatus> = status_by_batch(store).await?;
    let idx_to_id = store.call(|conn| cue_id_by_idx(conn)).await?;

    let ctx = Arc::new(BatchExecCtx {
        transport,
        store: store.clone(),
        retry: params.retry.clone(),
        token: token.clone(),
        seed: params.seed,
        model: params.model.clone(),
        tier: params.tier,
        thinking: params.thinking,
        max_content_attempts: params.max_content_attempts.max(1),
        system_instruction: translation_system_instruction(),
        analysis_sheet: sheet.canonical.clone(),
        analysis_version,
        estimator: params.estimator,
        output_ratio: params.output_ratio,
        limits: params.limits,
        glossary: sheet.glossary.clone(),
        names: sheet.names.clone(),
        idx_to_id,
    });

    let units: Vec<Arc<TranslationBatchUnit<T>>> = assembled
        .into_iter()
        .map(|b| {
            let status = statuses
                .get(&b.batch_no)
                .copied()
                .unwrap_or(UnitStatus::Pending);
            Arc::new(TranslationBatchUnit::new(b, status, ctx.clone()))
        })
        .collect();
    let total = units.len();

    let _summary = scheduler::run(units, params.scheduler, token).await;

    if token.is_cancelled() {
        return Ok(TranslationOutcome::Interrupted);
    }

    // The DB is authoritative for the final decision, not the scheduler summary.
    let (failed, incomplete) = store
        .call(|conn| {
            let failed = count_translation_batches_by_status(conn, UnitStatus::Failed)?;
            let pending = count_translation_batches_by_status(conn, UnitStatus::Pending)?;
            let in_flight = count_translation_batches_by_status(conn, UnitStatus::InFlight)?;
            Ok((failed, pending + in_flight))
        })
        .await?;
    if failed > 0 || incomplete > 0 {
        return Ok(TranslationOutcome::CompletedWithFailures {
            failed: failed + incomplete,
        });
    }

    // Every batch is done → the translation stage is complete, opening the
    // translation-review gate (`docs/ARCHITECTURE.md` §2).
    store
        .call(|conn| set_stage(conn, Stage::Translation, StageStatus::Completed))
        .await?;
    Ok(TranslationOutcome::Completed { batches: total })
}

/// Reads the current (highest-version) analysis status for each batch.
async fn status_by_batch(store: &Store) -> Result<BTreeMap<i64, UnitStatus>, PipelineError> {
    let map = store
        .call(|conn| {
            let mut out = BTreeMap::new();
            for status in [
                UnitStatus::Pending,
                UnitStatus::InFlight,
                UnitStatus::Done,
                UnitStatus::Failed,
            ] {
                for batch in list_translation_batches_by_status(conn, status)? {
                    out.insert(batch.batch_no, status);
                }
            }
            Ok(out)
        })
        .await?;
    Ok(map)
}

/// Loads the current approved analysis as `(version, Sheet)` for the cached prefix
/// and scene/speaker/glossary context. The translation gate already guarantees an
/// approved analysis; this re-checks defensively and errors with a
/// [`PipelineError::GateViolation`] otherwise.
async fn load_sheet(store: &Store) -> Result<(i64, Sheet), PipelineError> {
    let current = store.call(|conn| current_analysis(conn)).await?;
    let current = current
        .filter(|c| c.approved)
        .ok_or_else(|| PipelineError::GateViolation {
            message: "translation requires an approved analysis version; run \
                      `myansub analysis approve`"
                .to_string(),
        })?;
    let version = current.version;
    let doc = store
        .call(move |conn| get_analysis_version(conn, version))
        .await?
        .ok_or_else(|| PipelineError::GateViolation {
            message: "the approved analysis version could not be loaded".to_string(),
        })?;
    Ok((version, Sheet::parse(&doc.doc)))
}

// ---------------------------------------------------------------------------
// Dry run
// ---------------------------------------------------------------------------

/// Produces the `translate --dry-run` report without persisting anything
/// (`docs/LLM.md` §9). The real plan is computed and the real prompts are built;
/// each would-be call is estimated through the `DryRun` transport behind the local
/// cache (a hit is free), and nothing is written to the database.
pub async fn dry_run_translation(
    store: &Store,
    cues: &[Cue],
    params: &TranslationParams,
    prices: &PriceTable,
) -> Result<DryRunReport, PipelineError> {
    let (analysis_version, sheet) = load_sheet(store).await?;
    let assembled = assemble_all_batches(cues, &sheet, params, analysis_version);

    let client = Cached::new(
        DryRun::new(params.estimator, params.output_ratio),
        store.clone(),
    );

    let mut prompt_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut cost = Cost::Known(0.0);
    let mut calls = 0usize;
    for batch in &assembled {
        let req = build_request(
            &params.model,
            params.tier,
            params.thinking,
            &translation_system_instruction(),
            &sheet.canonical,
            &batch.variable_block,
            output_budget(
                &batch.variable_block,
                &params.estimator,
                params.output_ratio,
                batch.lines,
            ),
        );
        let resp = client.generate(req).await.map_err(PipelineError::Llm)?;
        prompt_tokens += resp.usage.prompt_tokens;
        output_tokens += resp.usage.output_tokens;
        cost = add_cost(
            cost,
            pricing::cost(&params.model, params.tier, &resp.usage, prices),
        );
        calls += 1;
    }

    Ok(DryRunReport {
        batches: assembled.len(),
        calls,
        estimated_prompt_tokens: prompt_tokens,
        estimated_output_tokens: output_tokens,
        estimated_cost: cost,
    })
}

/// Sums two [`Cost`]s; an unknown component makes the total unknown.
fn add_cost(acc: Cost, next: Cost) -> Cost {
    match (acc, next) {
        (Cost::Known(a), Cost::Known(b)) => Cost::Known(a + b),
        _ => Cost::Unknown,
    }
}

// ---------------------------------------------------------------------------
// Analysis sheet (parsed once per run)
// ---------------------------------------------------------------------------

/// One scene's cue range, summary, and speaker ids (`docs/DATA_MODEL.md` §3).
#[derive(Debug, Clone)]
struct SceneInfo {
    first: i64,
    last: i64,
    summary: String,
    speaker_ids: Vec<String>,
}

/// The slice of the approved analysis the translation stage consults: the canonical
/// JSON for the cached prefix (ADR-006), the scenes, the id→name map, and the
/// glossary/name data the quality checks need (`docs/LLM.md` §8).
#[derive(Debug, Clone)]
struct Sheet {
    /// Canonical JSON of the whole document — byte-stable across batches.
    canonical: String,
    scenes: Vec<SceneInfo>,
    char_names: BTreeMap<String, String>,
    /// Glossary entries as `(term, target, keep)`.
    glossary: Vec<(String, String, bool)>,
    /// Character names and aliases, for the `untranslated` check.
    names: Vec<String>,
}

impl Sheet {
    /// Parses the analysis document leniently (missing fields default to empty), so
    /// a sparse LLM document never aborts translation.
    fn parse(doc: &str) -> Sheet {
        let value: serde_json::Value = serde_json::from_str(doc).unwrap_or(serde_json::Value::Null);
        let canonical = canonical_json(&value);

        let mut char_names = BTreeMap::new();
        let mut names = Vec::new();
        if let Some(chars) = value.get("characters").and_then(|v| v.as_array()) {
            for ch in chars {
                let id = ch.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                let name = ch.get("name").and_then(|v| v.as_str()).unwrap_or_default();
                if !id.is_empty() {
                    char_names.insert(id.to_string(), name.to_string());
                }
                if !name.is_empty() {
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

        let mut scenes = Vec::new();
        if let Some(arr) = value.get("scenes").and_then(|v| v.as_array()) {
            for s in arr {
                let range = s.get("cue_range").and_then(|v| v.as_array());
                let (Some(first), Some(last)) = (
                    range.and_then(|r| r.first()).and_then(|v| v.as_i64()),
                    range.and_then(|r| r.get(1)).and_then(|v| v.as_i64()),
                ) else {
                    continue;
                };
                let summary = s
                    .get("summary")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                let speaker_ids = s
                    .get("speakers")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|x| x.as_str())
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                scenes.push(SceneInfo {
                    first,
                    last,
                    summary,
                    speaker_ids,
                });
            }
        }

        Sheet {
            canonical,
            scenes,
            char_names,
            glossary,
            names,
        }
    }

    /// The scene overlapping `[first, last]`: its summary and its speakers mapped to
    /// `(id, name)` pairs. Falls back to empty values when no scene overlaps.
    fn scene_for(&self, first: i64, last: i64) -> (String, Vec<(String, String)>) {
        let scene = self
            .scenes
            .iter()
            .find(|s| s.first <= last && s.last >= first);
        match scene {
            Some(s) => {
                let speakers = s
                    .speaker_ids
                    .iter()
                    .map(|id| {
                        let name = self.char_names.get(id).cloned().unwrap_or_default();
                        (id.clone(), name)
                    })
                    .collect();
                (s.summary.clone(), speakers)
            }
            None => (String::new(), Vec::new()),
        }
    }
}

// ---------------------------------------------------------------------------
// Batch planning + variable-block assembly
// ---------------------------------------------------------------------------

/// A planned batch: its number, cue-idx bounds, assembled variable block, the
/// `input_hash`, the batch's cues (for mapping + quality), and the line count.
#[derive(Debug, Clone)]
struct AssembledBatch {
    batch_no: i64,
    first_cue: i64,
    last_cue: i64,
    variable_block: String,
    input_hash: String,
    translate_cues: Vec<Cue>,
    lines: u64,
}

/// The cues eligible for translation: translatable cues with non-blank masked text
/// (`docs/LLM.md` §3 "translatable cues"; non-translatable cues are never sent).
fn translatable_cues(cues: &[Cue]) -> Vec<&Cue> {
    cues.iter()
        .filter(|c| c.translatable && !c.text_masked.trim().is_empty())
        .collect()
}

/// Plans and assembles every batch: contiguous groups of `batch_size` translatable
/// cues, each with its variable block, `input_hash`, and cues (`docs/LLM.md` §3).
fn assemble_all_batches(
    cues: &[Cue],
    sheet: &Sheet,
    params: &TranslationParams,
    analysis_version: i64,
) -> Vec<AssembledBatch> {
    let translatable = translatable_cues(cues);
    let size = params.batch_size.max(1) as usize;
    let before = params.context_before as usize;
    let after = params.context_after as usize;

    let mut out = Vec::new();
    for (i, group) in translatable.chunks(size).enumerate() {
        let (Some(first), Some(last)) = (group.first(), group.last()) else {
            continue;
        };
        let first_idx = i64::from(first.idx);
        let last_idx = i64::from(last.idx);

        // Index of this group within the translatable list, for context slicing.
        let start = i * size;
        let end = start + group.len(); // exclusive
        let ctx_before = &translatable[start.saturating_sub(before)..start];
        let ctx_after = &translatable[end..(end + after).min(translatable.len())];

        let (summary, speakers) = sheet.scene_for(first_idx, last_idx);
        let before_lines = to_lines(ctx_before);
        let translate_lines = to_lines(group);
        let after_lines = to_lines(ctx_after);
        let block = TranslationBlock {
            scene: &summary,
            speakers: &speakers,
            context_before: &before_lines,
            translate: &translate_lines,
            context_after: &after_lines,
        };
        let variable_block = block.render();
        let input_hash =
            compute_input_hash(&variable_block, analysis_version, params.model.as_str());

        out.push(AssembledBatch {
            batch_no: i as i64 + 1,
            first_cue: first_idx,
            last_cue: last_idx,
            variable_block,
            input_hash,
            translate_cues: group.iter().map(|c| (*c).clone()).collect(),
            lines: group.len() as u64,
        });
    }
    out
}

/// Renders a slice of cues as prompt [`TranslationLine`]s; the speaker prefix is
/// carried only when the cue has a known speaker (`docs/LLM.md` §2).
fn to_lines(cues: &[&Cue]) -> Vec<TranslationLine> {
    cues.iter()
        .map(|c| TranslationLine {
            idx: c.idx,
            speaker: c.speaker.clone().filter(|s| !s.is_empty()),
            text: c.text_masked.clone(),
        })
        .collect()
}

/// The `input_hash` of a batch (`docs/LLM.md` §3): sha256 of the variable block +
/// the analysis version + the model + [`PROMPT_VERSION`]. A `done` batch whose
/// `input_hash` is unchanged is never resent, and a re-approved analysis version
/// changes the hash (the basis for M6-B2 selective invalidation).
fn compute_input_hash(variable_block: &str, analysis_version: i64, model: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(variable_block.as_bytes());
    hasher.update([0u8]);
    hasher.update(analysis_version.to_le_bytes());
    hasher.update([0u8]);
    hasher.update(model.as_bytes());
    hasher.update([0u8]);
    hasher.update(PROMPT_VERSION.to_le_bytes());
    hex_encode(&hasher.finalize())
}

/// Lowercase hex encoding of a byte slice (no external hex dependency).
fn hex_encode(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// The `maxOutputTokens` budget for a batch (`docs/LLM.md` §5.8): source tokens ×
/// `output_ratio` plus per-line and base JSON overhead, bounded by [`OUTPUT_CEILING`].
fn output_budget(
    variable_block: &str,
    estimator: &Estimator,
    output_ratio: f64,
    lines: u64,
) -> u32 {
    let source = estimator.estimate_tokens(variable_block);
    let scaled = (source as f64 * output_ratio).ceil() as u64;
    let want = scaled + lines * OUTPUT_OVERHEAD_PER_LINE + OUTPUT_OVERHEAD_BASE;
    want.clamp(OUTPUT_OVERHEAD_BASE, OUTPUT_CEILING) as u32
}

/// Builds a translation request with the fixed cache-friendly layout
/// (`docs/LLM.md` §2): system instruction + canonical analysis sheet (the cached
/// prefix), then the batch's variable block.
fn build_request(
    model: &str,
    tier: ServiceTier,
    thinking: ThinkingSetting,
    system_instruction: &str,
    analysis_sheet: &str,
    variable_block: &str,
    max_output_tokens: u32,
) -> LlmRequest {
    LlmRequest {
        model: model.to_string(),
        service_tier: tier,
        system_instruction: system_instruction.to_string(),
        contents: vec![
            ContentBlock::user(analysis_sheet.to_string()),
            ContentBlock::user(variable_block.to_string()),
        ],
        generation_config: GenerationConfig {
            max_output_tokens,
            response_mime_type: "application/json".to_string(),
            response_schema: Some(translation_response_schema()),
            thinking,
        },
    }
}

// ---------------------------------------------------------------------------
// Response validation + parsing
// ---------------------------------------------------------------------------

/// Why a translation response is not usable (`docs/DATA_MODEL.md` §4). Drives the
/// tier-1 content retry only; the message is logged, not surfaced to the user.
#[derive(Debug, thiserror::Error)]
enum ValidationError {
    #[error("response is not valid JSON: {0}")]
    Parse(String),
    #[error("response is not a JSON object")]
    NotObject,
    #[error("missing or non-array `lines`")]
    Lines,
    #[error("a line entry is malformed (needs integer `id` and string `t`)")]
    Entry,
    #[error("response does not cover every batch cue id (missing {missing:?})")]
    Coverage { missing: Vec<i64> },
}

/// Validates a translation response against `{"lines":[{"id":int,"t":string}]}`
/// (`docs/DATA_MODEL.md` §4) **and** that the returned id set covers every cue id in
/// `expected` (review #1). A response that omits a batch cue id — or returns an empty
/// `lines` array — is a tier-1 content failure, not a silent skip: build_translations
/// pairs only returned ids, so an uncovered cue would otherwise get no translation,
/// no flag, and no retry while the batch is marked done. Extra/unknown returned ids
/// are ignored for B1 (fine-grained re-request is M6-B2).
fn validate_translation(text: &str, expected: &BTreeSet<i64>) -> Result<(), ValidationError> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| ValidationError::Parse(e.to_string()))?;
    let obj = value.as_object().ok_or(ValidationError::NotObject)?;
    let lines = obj
        .get("lines")
        .and_then(|v| v.as_array())
        .ok_or(ValidationError::Lines)?;
    let mut returned: BTreeSet<i64> = BTreeSet::new();
    for entry in lines {
        let entry = entry.as_object().ok_or(ValidationError::Entry)?;
        let id = entry.get("id").and_then(|v| {
            if v.is_i64() {
                v.as_i64()
            } else {
                v.as_u64().map(|u| u as i64)
            }
        });
        let t_ok = entry.get("t").is_some_and(|v| v.is_string());
        match id {
            Some(id) if t_ok => {
                returned.insert(id);
            }
            _ => return Err(ValidationError::Entry),
        }
    }
    let missing: Vec<i64> = expected.difference(&returned).copied().collect();
    if !missing.is_empty() {
        return Err(ValidationError::Coverage { missing });
    }
    Ok(())
}

/// Parses a validated response into `(id, text)` pairs in order.
fn parse_lines(text: &str) -> Vec<(i64, String)> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return Vec::new();
    };
    let Some(lines) = value.get("lines").and_then(|v| v.as_array()) else {
        return Vec::new();
    };
    lines
        .iter()
        .filter_map(|entry| {
            let id = entry.get("id").and_then(|v| v.as_i64())?;
            let t = entry.get("t").and_then(|v| v.as_str())?.to_string();
            Some((id, t))
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Batch work unit
// ---------------------------------------------------------------------------

const ST_PENDING: u8 = 0;
const ST_IN_FLIGHT: u8 = 1;
const ST_DONE: u8 = 2;
const ST_FAILED: u8 = 3;

fn status_to_atom(status: UnitStatus) -> u8 {
    match status {
        UnitStatus::Pending => ST_PENDING,
        UnitStatus::InFlight => ST_IN_FLIGHT,
        UnitStatus::Done => ST_DONE,
        UnitStatus::Failed => ST_FAILED,
    }
}

fn atom_to_state(atom: u8) -> UnitState {
    match atom {
        ST_IN_FLIGHT => UnitState::InFlight,
        ST_DONE => UnitState::Done,
        ST_FAILED => UnitState::Failed,
        _ => UnitState::Pending,
    }
}

/// Shared execution context for every batch of one run. The cached prefix
/// (`system_instruction` + `analysis_sheet`) is built once and reused, so it stays
/// byte-stable across batches (ADR-006).
struct BatchExecCtx<T> {
    transport: T,
    store: Store,
    retry: RetryConfig,
    token: CancellationToken,
    seed: u64,
    model: String,
    tier: ServiceTier,
    thinking: ThinkingSetting,
    max_content_attempts: u32,
    system_instruction: String,
    analysis_sheet: String,
    analysis_version: i64,
    estimator: Estimator,
    output_ratio: f64,
    limits: Limits,
    glossary: Vec<(String, String, bool)>,
    names: Vec<String>,
    idx_to_id: BTreeMap<u32, i64>,
}

/// One translation batch as a [`WorkUnit`] (`docs/ARCHITECTURE.md` §4/§6). Its
/// `run` claims the batch, sends the variable block behind the cached prefix,
/// validates the response (with the tier-1 content retry), runs the quality checks,
/// and writes the batch's translations and `done` status in one transaction.
struct TranslationBatchUnit<T> {
    batch_no: i64,
    variable_block: String,
    input_hash: String,
    translate_cues: Vec<Cue>,
    lines: u64,
    state: AtomicU8,
    ctx: Arc<BatchExecCtx<T>>,
}

impl<T> TranslationBatchUnit<T> {
    fn new(batch: AssembledBatch, status: UnitStatus, ctx: Arc<BatchExecCtx<T>>) -> Self {
        Self {
            batch_no: batch.batch_no,
            variable_block: batch.variable_block,
            input_hash: batch.input_hash,
            translate_cues: batch.translate_cues,
            lines: batch.lines,
            state: AtomicU8::new(status_to_atom(status)),
            ctx,
        }
    }

    /// Runs the per-cue and batch-level quality checks and builds the rows to
    /// persist, mapping each returned line id (a cue `idx`) to its cue and
    /// `cues.id` (`docs/LLM.md` §8, `docs/DATA_MODEL.md` §4/§5).
    fn build_translations(&self, lines: &[(i64, String)]) -> Vec<NewTranslation> {
        let glossary: Vec<GlossaryTerm> = self
            .ctx
            .glossary
            .iter()
            .map(|(term, target, keep)| GlossaryTerm {
                term,
                target,
                keep: *keep,
            })
            .collect();
        let names: Vec<&str> = self.ctx.names.iter().map(String::as_str).collect();
        let analysis_ctx = AnalysisContext {
            glossary: &glossary,
            names: &names,
        };

        // Pair each returned line with its batch cue (ignore ids not in this batch;
        // the id-set tier is M6-B2). Duplicate ids in one response are deduped
        // first-wins so a cue is never upserted twice with a spurious history row
        // (review #6).
        let mut paired: Vec<(&Cue, String)> = Vec::new();
        let mut seen: BTreeSet<i64> = BTreeSet::new();
        for (id, text) in lines {
            if !seen.insert(*id) {
                continue;
            }
            if let Some(cue) = self.translate_cues.iter().find(|c| i64::from(c.idx) == *id) {
                paired.push((cue, text.clone()));
            }
        }

        let mut flags: Vec<Vec<QualityFlag>> = paired
            .iter()
            .map(|(cue, text)| checks::check_cue(cue, text, &analysis_ctx, &self.ctx.limits))
            .collect();

        // Batch-level boundary drift over the returned pairs.
        let drift_input: Vec<(&Cue, &str)> = paired
            .iter()
            .map(|(cue, text)| (*cue, text.as_str()))
            .collect();
        let drift = checks::check_batch_drift(&drift_input, &self.ctx.limits);
        for (i, fired) in drift.iter().enumerate() {
            if *fired && !flags[i].contains(&QualityFlag::BoundaryDrift) {
                flags[i].push(QualityFlag::BoundaryDrift);
            }
        }

        paired
            .into_iter()
            .zip(flags)
            .filter_map(|((cue, text), flags)| {
                self.ctx
                    .idx_to_id
                    .get(&cue.idx)
                    .map(|cue_id| NewTranslation {
                        cue_id: *cue_id,
                        text,
                        origin: TranslationOrigin::Llm,
                        locked: false,
                        stale: false,
                        flags,
                        analysis_version: self.ctx.analysis_version,
                        model: Some(self.ctx.model.clone()),
                    })
            })
            .collect()
    }

    fn base_request(&self) -> LlmRequest {
        let budget = output_budget(
            &self.variable_block,
            &self.ctx.estimator,
            self.ctx.output_ratio,
            self.lines,
        );
        build_request(
            &self.ctx.model,
            self.ctx.tier,
            self.ctx.thinking,
            &self.ctx.system_instruction,
            &self.ctx.analysis_sheet,
            &self.variable_block,
            budget,
        )
    }

    async fn mark_failed(&self, message: &str) {
        let batch_no = self.batch_no;
        let store = self.ctx.store.clone();
        let message = message.to_string();
        let _ = store
            .call(move |conn| fail_translation_batch(conn, batch_no, &message).map(|_| ()))
            .await;
        self.state.store(ST_FAILED, Ordering::Relaxed);
    }
}

impl<T> WorkUnit for TranslationBatchUnit<T>
where
    T: LlmClient + Clone + Send + Sync + 'static,
{
    fn id(&self) -> u64 {
        self.batch_no.max(0) as u64
    }

    fn kind(&self) -> &str {
        "translation_batch"
    }

    fn state(&self) -> UnitState {
        atom_to_state(self.state.load(Ordering::Relaxed))
    }

    async fn reset(&self) {
        let batch_no = self.batch_no;
        let store = self.ctx.store.clone();
        let _ = store
            .call(move |conn| reset_translation_batch(conn, batch_no).map(|_| ()))
            .await;
        self.state.store(ST_PENDING, Ordering::Relaxed);
    }

    async fn run(&self) -> UnitOutcome {
        let batch_no = self.batch_no;

        // A `done` batch whose `input_hash` is unchanged is never resent
        // (`docs/LLM.md` §3): skip it before claiming. (The scheduler already skips
        // `done` units, so this is the belt-and-braces guard the resume contract
        // names.)
        let store = self.ctx.store.clone();
        let expected_hash = self.input_hash.clone();
        if let Ok(Some(row)) = store
            .call(move |conn| get_translation_batch(conn, batch_no))
            .await
            && row.status == UnitStatus::Done
            && row.input_hash.as_deref() == Some(expected_hash.as_str())
        {
            self.state.store(ST_DONE, Ordering::Relaxed);
            return UnitOutcome::Done;
        }

        // Claim: pending → in_flight. A lost claim means the batch is no longer
        // pending (already done/claimed); skip it as done so a `done` batch whose
        // input is unchanged is never resent (`docs/LLM.md` §3).
        let store = self.ctx.store.clone();
        match store
            .call(move |conn| claim_translation_batch(conn, batch_no))
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                self.state.store(ST_DONE, Ordering::Relaxed);
                return UnitOutcome::Done;
            }
            Err(_) => {
                self.state.store(ST_FAILED, Ordering::Relaxed);
                return UnitOutcome::Failed;
            }
        }
        self.state.store(ST_IN_FLIGHT, Ordering::Relaxed);

        match self.call_validated().await {
            CallResult::Valid { text, commit } => {
                let lines = parse_lines(&text);
                let translations = self.build_translations(&lines);
                let store = self.ctx.store.clone();
                let committed = store
                    .call(move |conn| {
                        let tx = conn.transaction()?;
                        // Flip the status first: if the batch is no longer `in_flight`
                        // this returns false and we commit nothing, so a result is
                        // never written against a non-in_flight batch (review #4).
                        let ok = complete_translation_batch_in_tx(&tx, batch_no)?;
                        if !ok {
                            return Ok(false);
                        }
                        for t in &translations {
                            upsert_translation_in_tx(&tx, t)?;
                        }
                        if let Some(payload) = commit.payload() {
                            put_cache_in_tx(&tx, payload)?;
                        }
                        tx.commit()?;
                        Ok(true)
                    })
                    .await;
                match committed {
                    Ok(true) => {
                        self.state.store(ST_DONE, Ordering::Relaxed);
                        UnitOutcome::Done
                    }
                    _ => {
                        self.state.store(ST_FAILED, Ordering::Relaxed);
                        UnitOutcome::Failed
                    }
                }
            }
            CallResult::Exhausted => {
                self.mark_failed("content validation failed after retries")
                    .await;
                UnitOutcome::Failed
            }
            CallResult::Transport(err) => {
                let message = err.to_string();
                self.mark_failed(&message).await;
                UnitOutcome::Failed
            }
            CallResult::Cancelled => {
                // Leave the batch `in_flight`; `Store::open` recovers it to `pending`
                // so the rerun resumes it without re-sending a completed batch.
                UnitOutcome::Failed
            }
        }
    }
}

/// The outcome of a validated call (`docs/LLM.md` §6).
enum CallResult {
    Valid { text: String, commit: CommitToken },
    Exhausted,
    Cancelled,
    Transport(LlmError),
}

impl<T> TranslationBatchUnit<T>
where
    T: LlmClient + Clone + Send + Sync + 'static,
{
    /// Calls the LLM stack and validates the response, retrying the whole batch with a
    /// strict reminder on a content failure (`docs/LLM.md` §6.1). Tier 1 retries the
    /// whole batch **once** ([`TIER1_WHOLE_BATCH_RETRIES`]), bounded by the overall
    /// `max_content_attempts` budget that the later tiers (M6-B2) will also draw on;
    /// after that the batch is exhausted. The content-attempt counter here is separate
    /// from the transport retries inside `Retrying`.
    async fn call_validated(&self) -> CallResult {
        let base_req = self.base_request();
        let expected: BTreeSet<i64> = self
            .translate_cues
            .iter()
            .map(|c| i64::from(c.idx))
            .collect();
        // Total whole-batch calls = original + at most one retry, never more than the
        // overall content-attempt budget.
        let cap = self
            .ctx
            .max_content_attempts
            .min(TIER1_WHOLE_BATCH_RETRIES + 1);
        let mut attempt = 0u32;
        loop {
            if self.ctx.token.is_cancelled() {
                return CallResult::Cancelled;
            }
            let req = if attempt == 0 {
                base_req.clone()
            } else {
                with_reminder(base_req.clone())
            };

            let call_ctx = CallContext {
                stage: Stage::Translation.as_str().to_string(),
                unit: Some(self.batch_no),
                dry_run: false,
            };
            let stack = assemble_stack(
                self.ctx.transport.clone(),
                self.ctx.retry.clone(),
                self.ctx.token.clone(),
                self.ctx.seed,
                self.ctx.store.clone(),
                self.ctx.store.clone(),
                call_ctx,
            );

            match stack.generate(req).await {
                Ok(resp) => match validate_translation(&resp.text, &expected) {
                    Ok(()) => {
                        return CallResult::Valid {
                            text: resp.text,
                            commit: resp.commit,
                        };
                    }
                    Err(err) => {
                        tracing::debug!(
                            attempt,
                            "translation response failed validation ({err}); content retry"
                        );
                        attempt += 1;
                        if attempt >= cap {
                            return CallResult::Exhausted;
                        }
                    }
                },
                Err(LlmError::Cancelled) => return CallResult::Cancelled,
                Err(err) => return CallResult::Transport(err),
            }
        }
    }
}

/// Tier-1 whole-batch content retries (`docs/LLM.md` §6.1: retry the whole batch
/// *once*). Later tiers (id-set, per-line) are M6-B2 and consume the remaining
/// `max_content_attempts` budget.
const TIER1_WHOLE_BATCH_RETRIES: u32 = 1;

/// Appends the strict reminder as a trailing user block for a content retry
/// (`docs/LLM.md` §6).
fn with_reminder(mut req: LlmRequest) -> LlmRequest {
    req.contents
        .push(ContentBlock::user(translation_retry_reminder()));
    req
}
