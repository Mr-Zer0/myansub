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
//! retries inside `Retrying`, and the whole tiered sequence is bounded by one
//! shared `max_content_attempts` budget so a pathological response can never loop
//! forever:
//! - **Tier 1** — an unparseable / schema-mismatched response retries the whole
//!   batch once with a strict reminder.
//! - **Tier 2** — a parseable response that omits some batch ids keeps the valid
//!   lines already received and re-requests *only the missing ids* as a new, smaller
//!   request, merging until coverage is reached (or the budget runs out, failing the
//!   batch — never a silent skip).
//! - **Tier 3** — a covered batch whose cues carry a *hard* quality flag
//!   (`placeholder_mismatch`, `untranslated`, `zawgyi_suspect`) retries just those
//!   lines once, isolated; if still bad the best result is stored with the flag and
//!   the batch is still `done`. Soft flags are stored, never retried.
//! - **Tier 4** — after the budget is exhausted a batch that still lacks coverage is
//!   marked `failed` and the run continues (a failed batch never blocks the others).
//!
//! A retry that covers no new id makes no progress, so the loop stops early rather
//! than burning the remaining budget on an unresponsive model.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use sha2::{Digest, Sha256};
use tokio_util::sync::CancellationToken;

use crate::domain::{Cue, DigitStyle, QualityFlag};
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
    PROMPT_VERSION, TranslationBlock, TranslationLine, translation_missing_ids_reminder,
    translation_per_line_reminder, translation_retry_reminder, translation_system_instruction,
};
use crate::llm::retry::RetryConfig;
use crate::llm::schemas::translation_response_schema;
use crate::quality::checks::{self, AnalysisContext, GlossaryTerm, Limits};
use crate::store::repo_analysis::{current_analysis, get_analysis_version};
use crate::store::repo_cues::load_cues;
use crate::store::repo_llm::put_cache_in_tx;
use crate::store::repo_project::set_stage;
use crate::store::repo_translations::{
    NewTranslation, TranslationOrigin, cue_id_by_idx, delete_unlocked_translation,
    list_translations, mark_unlocked_stale_in_tx, upsert_translation_in_tx,
};
use crate::store::repo_units::{
    NewTranslationBatch, claim_translation_batch, complete_translation_batch_in_tx,
    count_translation_batches_by_status, create_translation_batch, fail_translation_batch,
    get_translation_batch, list_translation_batches_by_status, requeue_failed_translation_batches,
    requeue_translation_batch, reset_translation_batch, set_translation_batch_input_hash_in_tx,
};
use crate::store::{Stage, StageStatus, Store, StoreError, UnitStatus};

use super::confirm::{self, ConfirmSettings, ConfirmedRun, CostEstimate};
use super::error::PipelineError;
use super::input::check_source_unchanged;
use super::orchestrator::{can_run, load_state};
use super::progress::{Progress, ProgressStage};
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
    /// The digit style requested in the translation prompt (`[burmese].digits`,
    /// ADR-049(e)). Part of the prompt and folded into the `input_hash`, so a change
    /// marks unlocked translations stale (`docs/LLM.md` §7).
    pub digits: DigitStyle,
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

/// Run shaping flags from the `translate` command (`docs/CLI.md` §6).
///
/// - `retry_failed` requeues `failed` batches and reruns them.
/// - `redo` re-translates every **unlocked** cue (locked cues are preserved and
///   never re-sent), minting fresh translations.
/// - `stale` re-translates only **stale, unlocked** cues (the `stale` flag is set by
///   M8 selective invalidation).
///
/// `retry_failed` runs **independently** of `redo`/`stale`: a failed batch has no
/// translation rows, so it never joins the stale set and must be requeued on its own.
/// Within the unlocked-cue re-request, `redo` takes precedence over `stale` (the
/// broader selection wins); the two together still requeue failed batches when
/// `retry_failed` is also set.
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
    /// Some batches ended not-done after the run: `failed` is the count of true
    /// content failures (exhausted their retry budget), `incomplete` is the count
    /// still `pending`/`in_flight` (e.g. never dispatched). Maps to exit code 4 when
    /// either is non-zero (`docs/CLI.md` §6/§7).
    CompletedWithFailures { failed: usize, incomplete: usize },
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
#[allow(clippy::too_many_arguments)]
pub async fn run_translation<T>(
    transport: T,
    store: &Store,
    cues: &[Cue],
    params: &TranslationParams,
    flags: TranslationFlags,
    token: &CancellationToken,
    progress: Arc<dyn Progress>,
) -> Result<TranslationOutcome, PipelineError>
where
    T: LlmClient + Clone + Send + Sync + 'static,
{
    let (analysis_version, sheet) = load_sheet(store).await?;
    let mut assembled = assemble_all_batches(cues, &sheet, params, analysis_version);
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

    let idx_to_id = store.call(|conn| cue_id_by_idx(conn)).await?;

    // Done-batch reconciliation for a config-driven prompt change (ADR-050, ADR-049(e)).
    // A `done` batch is skipped by the scheduler on resume, so a pure `[burmese].digits`
    // change — which moves each batch's recomputed `input_hash` while the source,
    // analysis version, model, and PROMPT_VERSION are unchanged — would otherwise be
    // silently ignored. For each `done` batch whose freshly-assembled hash differs from
    // its stored one, in ONE transaction mark its UNLOCKED cues stale (locked cues are
    // never touched, FR-6) and rewrite the stored hash so the drift is recorded and the
    // next plain run does not re-mark (idempotent). The subsequent `translate --stale`
    // then re-translates exactly those unlocked cues; this run sends nothing new.
    reconcile_done_batch_hashes(store, &assembled, &idx_to_id).await?;

    // Apply the run-shaping flags (`docs/CLI.md` §6): requeue failed batches
    // (`--retry-failed`) or re-run unlocked / stale-unlocked cues (`--redo`/`--stale`),
    // rebuilding any batch that must drop a locked cue so it is never re-sent. This
    // resets the affected batch rows before the status snapshot is taken.
    apply_run_flags(
        store,
        &mut assembled,
        flags,
        &idx_to_id,
        params,
        analysis_version,
        &sheet.formal_segments,
    )
    .await?;

    // The authoritative status for each batch comes from the DB rows.
    let statuses: BTreeMap<i64, UnitStatus> = status_by_batch(store).await?;

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
        system_instruction: translation_system_instruction(params.digits),
        analysis_sheet: sheet.canonical.clone(),
        analysis_version,
        estimator: params.estimator,
        output_ratio: params.output_ratio,
        limits: params.limits,
        glossary: sheet.glossary.clone(),
        names: sheet.names.clone(),
        name_targets: sheet.name_targets.clone(),
        digits: params.digits,
        formal_segments: sheet.formal_segments.clone(),
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

    progress.stage_start(ProgressStage::Translation);
    let summary = scheduler::run(units, params.scheduler, token, progress.clone()).await;
    progress.stage_finish(summary);

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
    // The translation stage completes once no unit is still pending/in_flight, even
    // when some batches FAILED: a failed batch does not block the translation-review
    // gate (`docs/ARCHITECTURE.md` §2, ADR-030). The gate lists the failed cues; the
    // run's exit code still reflects the failures (4). `translate --retry-failed`
    // later resets the stage to `running` when it requeues a failed batch.
    if incomplete == 0 {
        store
            .call(|conn| set_stage(conn, Stage::Translation, StageStatus::Completed))
            .await?;
    }

    if failed > 0 || incomplete > 0 {
        return Ok(TranslationOutcome::CompletedWithFailures { failed, incomplete });
    }
    Ok(TranslationOutcome::Completed { batches: total })
}

/// Runs the translation stage behind the cost-confirmation gate (`docs/CLI.md` §5,
/// `docs/LLM.md` §7/§9). Computes the pre-flight estimate with the dry-run estimator
/// (no LLM call) and [`confirm::gate`]s the real run; `make_transport` is only built
/// once the gate is cleared, so a refused run (non-TTY, no `--yes`) returns
/// [`PipelineError::ConfirmationNeeded`] (exit 3) without spending or needing a key.
///
/// For `--stale` (`docs/LLM.md` §7) the estimate covers **only** the stale, unlocked
/// cues that will be re-translated, not the whole project. `--redo` and the default
/// estimate the full batch plan (already-done batches hit the local cache and are
/// free, so a resume's estimate reflects only the remaining work).
#[allow(clippy::too_many_arguments)]
pub async fn run_translation_confirmed<T, MK>(
    make_transport: MK,
    store: &Store,
    cues: &[Cue],
    params: &TranslationParams,
    flags: TranslationFlags,
    token: &CancellationToken,
    prices: &PriceTable,
    settings: ConfirmSettings,
    progress: Arc<dyn Progress>,
) -> Result<ConfirmedRun<TranslationOutcome>, PipelineError>
where
    T: LlmClient + Clone + Send + Sync + 'static,
    MK: FnOnce() -> Result<T, PipelineError>,
{
    // `--stale` (without the broader `--redo`) re-translates only the stale, unlocked
    // cues, so estimate over the real affected-batch structure — one prefix-bearing
    // request per original batch holding a stale cue (`docs/LLM.md` §7), not a re-chunk
    // of the stale cues (which would collapse scattered batches and under-count).
    let report = if flags.stale && !flags.redo {
        dry_run_translation_stale(store, cues, params, prices).await?
    } else {
        dry_run_translation(store, cues, params, prices).await?
    };
    let estimate = CostEstimate {
        prompt_tokens: report.estimated_prompt_tokens,
        output_tokens: report.estimated_output_tokens,
        cost: report.estimated_cost,
    };
    confirm::gate(estimate, settings, make_transport, |transport| {
        run_translation(transport, store, cues, params, flags, token, progress)
    })
    .await
}

/// The cue `idx` values with a stale, unlocked translation — the exact set
/// `translate --stale` re-translates (`docs/LLM.md` §7). Used to scope the `--stale`
/// pre-flight estimate; `pub` so the scoping is directly testable.
pub async fn stale_unlocked_idxs(store: &Store) -> Result<BTreeSet<u32>, PipelineError> {
    let (rows, idx_to_id) = store
        .call(|conn| {
            let rows = list_translations(conn)?;
            let idx_to_id = cue_id_by_idx(conn)?;
            Ok((rows, idx_to_id))
        })
        .await?;
    let id_to_idx: BTreeMap<i64, u32> = idx_to_id.iter().map(|(idx, id)| (*id, *idx)).collect();
    let mut out = BTreeSet::new();
    for row in rows {
        if row.stale
            && !row.locked
            && let Some(idx) = id_to_idx.get(&row.cue_id)
        {
            out.insert(*idx);
        }
    }
    Ok(out)
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

/// Applies the `translate` run-shaping flags (`docs/CLI.md` §6) to the planned
/// batches and the stored batch rows, before the status snapshot and scheduling:
///
/// - `--redo`: re-run every batch holding at least one unlocked cue, clearing those
///   cues' translations first and excluding any locked cue so it is never re-sent.
/// - `--stale`: re-request **only** the stale, unlocked cues (`docs/LLM.md` §7,
///   `docs/CLI.md` §6). Non-stale cues in an affected batch are preserved — never
///   re-sent, re-billed, or overwritten — and locked cues are never touched.
/// - `--retry-failed`: requeue every `failed` batch back to `pending`.
///
/// `--retry-failed` runs **independently** of `--redo`/`--stale` (ADR): a failed
/// batch has no translation rows, so it never enters the stale set and must be
/// requeued on its own regardless of the other flags. `--redo` takes precedence over
/// `--stale` within the unlocked-cue re-request (the broader selection wins).
#[allow(clippy::too_many_arguments)]
async fn apply_run_flags(
    store: &Store,
    assembled: &mut [AssembledBatch],
    flags: TranslationFlags,
    idx_to_id: &BTreeMap<u32, i64>,
    params: &TranslationParams,
    analysis_version: i64,
    segments: &[FormalRange],
) -> Result<(), PipelineError> {
    if flags.redo || flags.stale {
        // Load locked/stale state per cue idx from the stored translations.
        let id_to_idx: BTreeMap<i64, u32> = idx_to_id.iter().map(|(idx, id)| (*id, *idx)).collect();
        let rows = store.call(|conn| list_translations(conn)).await?;
        let mut locked: BTreeSet<u32> = BTreeSet::new();
        let mut stale: BTreeSet<u32> = BTreeSet::new();
        for row in rows {
            let Some(idx) = id_to_idx.get(&row.cue_id).copied() else {
                continue;
            };
            if row.locked {
                locked.insert(idx);
            }
            if row.stale {
                stale.insert(idx);
            }
        }

        let mut reset_nos: Vec<i64> = Vec::new();
        let mut clear_ids: Vec<i64> = Vec::new();
        for batch in assembled.iter_mut() {
            let unlocked: Vec<Cue> = batch
                .translate_cues
                .iter()
                .filter(|c| !locked.contains(&c.idx))
                .cloned()
                .collect();
            // A batch whose cues are all locked is never re-run or re-sent.
            if unlocked.is_empty() {
                continue;
            }
            // `--redo` re-requests every unlocked cue; `--stale` re-requests only the
            // stale, unlocked cues (and preserves the rest). `--redo` wins when both
            // are set.
            let subset: Vec<Cue> = if flags.redo {
                unlocked
            } else {
                unlocked
                    .into_iter()
                    .filter(|c| stale.contains(&c.idx))
                    .collect()
            };
            if subset.is_empty() {
                continue;
            }
            reset_nos.push(batch.batch_no);
            // `--redo` mints fresh translations, so clear the unlocked cues first.
            // `--stale` leaves the prior rows in place; the upsert overwrites only the
            // re-requested stale cues (appending the superseded text to history).
            if flags.redo {
                for c in &subset {
                    if let Some(id) = idx_to_id.get(&c.idx) {
                        clear_ids.push(*id);
                    }
                }
            }
            // Reduce the request to the re-requested subset so locked cues (and, under
            // `--stale`, non-stale cues) are never re-sent.
            if subset.len() != batch.translate_cues.len() {
                rebuild_batch_subset(batch, subset, params, analysis_version, segments);
            }
        }

        store
            .call(move |conn| {
                let tx = conn.transaction()?;
                for id in &clear_ids {
                    delete_unlocked_translation(&tx, *id)?;
                }
                let mut requeued = false;
                for no in &reset_nos {
                    requeued |= requeue_translation_batch(&tx, *no)?;
                }
                // Requeuing re-opens pending work, so the stage returns to `running`
                // (it may have been `completed` from a prior failed/finished run); this
                // keeps the stage consistent with the pending batches (ADR-030).
                if requeued {
                    set_stage(&tx, Stage::Translation, StageStatus::Running)?;
                }
                tx.commit()?;
                Ok(())
            })
            .await?;
    }

    // `--retry-failed` is independent of `--redo`/`--stale`: always requeue failed
    // batches so a failure (which has no translation rows and so never joins the stale
    // set) is retried regardless of the other flags.
    if flags.retry_failed {
        store
            .call(|conn| {
                let tx = conn.transaction()?;
                let requeued = requeue_failed_translation_batches(&tx)?;
                if requeued > 0 {
                    set_stage(&tx, Stage::Translation, StageStatus::Running)?;
                }
                tx.commit()?;
                Ok(())
            })
            .await?;
    }

    Ok(())
}

/// Reconciles `done` batches whose stored `input_hash` no longer matches the
/// freshly-assembled one (ADR-050). Isolates a pure config-driven prompt change
/// (`[burmese].digits`): same source, analysis version, model, and PROMPT_VERSION
/// mean only the digit style can have moved the hash. For each drifted `done` batch,
/// in one transaction, marks its UNLOCKED cues stale and rewrites the stored hash so
/// the drift is recorded and re-marking is idempotent. Locked cues are never marked
/// (FR-6). A plain run does no re-sending here; `translate --stale` does that next.
async fn reconcile_done_batch_hashes(
    store: &Store,
    assembled: &[AssembledBatch],
    idx_to_id: &BTreeMap<u32, i64>,
) -> Result<(), PipelineError> {
    let stored_done: BTreeMap<i64, Option<String>> = store
        .call(|conn| list_translation_batches_by_status(conn, UnitStatus::Done))
        .await?
        .into_iter()
        .map(|b| (b.batch_no, b.input_hash))
        .collect();

    // Each drifted done batch: (batch_no, new_hash, cue_ids of its cues).
    let mut updates: Vec<(i64, String, Vec<i64>)> = Vec::new();
    for batch in assembled {
        let Some(stored) = stored_done.get(&batch.batch_no) else {
            continue; // not a done batch
        };
        if stored.as_deref() == Some(batch.input_hash.as_str()) {
            continue; // unchanged
        }
        let cue_ids: Vec<i64> = batch
            .translate_cues
            .iter()
            .filter_map(|c| idx_to_id.get(&c.idx).copied())
            .collect();
        updates.push((batch.batch_no, batch.input_hash.clone(), cue_ids));
    }

    if updates.is_empty() {
        return Ok(());
    }

    store
        .call(move |conn| {
            let tx = conn.transaction()?;
            for (batch_no, new_hash, cue_ids) in &updates {
                mark_unlocked_stale_in_tx(&tx, cue_ids)?;
                set_translation_batch_input_hash_in_tx(&tx, *batch_no, new_hash)?;
            }
            tx.commit()?;
            Ok(())
        })
        .await?;
    Ok(())
}

/// Rebuilds a batch's variable block, `input_hash`, bounds, and cue list from a
/// subset of its cues (used when a locked cue must be dropped from a `--redo`/`--stale`
/// re-run so it is never re-sent). The scene header is preserved; context lines are
/// omitted from the reduced request.
fn rebuild_batch_subset(
    batch: &mut AssembledBatch,
    cues: Vec<Cue>,
    params: &TranslationParams,
    analysis_version: i64,
    segments: &[FormalRange],
) {
    let refs: Vec<&Cue> = cues.iter().collect();
    let lines = to_lines(&refs, segments);
    let block = TranslationBlock {
        scene: &batch.scene,
        speakers: &batch.speakers,
        context_before: &[],
        translate: &lines,
        context_after: &[],
    };
    let variable_block = block.render();
    batch.input_hash = compute_input_hash(
        &variable_block,
        analysis_version,
        params.model.as_str(),
        params.digits,
    );
    if let Some(first) = cues.first() {
        batch.first_cue = i64::from(first.idx);
    }
    if let Some(last) = cues.last() {
        batch.last_cue = i64::from(last.idx);
    }
    batch.lines = cues.len() as u64;
    batch.variable_block = variable_block;
    batch.translate_cues = cues;
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
    estimate_assembled(store, params, prices, &sheet, &assembled).await
}

/// Estimates the `translate --stale` re-run (`docs/LLM.md` §7), mirroring the real
/// run's batch structure in [`apply_run_flags`]: one request per **original** batch
/// that holds at least one stale, unlocked cue, each reduced to that stale subset but
/// still carrying the full analysis-sheet prefix. This is why it cannot re-chunk the
/// stale cues into fresh contiguous groups — scattered stale cues keep their separate
/// prefix-bearing requests, so the estimate counts N prefixes, not `ceil(stale / batch)`.
pub async fn dry_run_translation_stale(
    store: &Store,
    cues: &[Cue],
    params: &TranslationParams,
    prices: &PriceTable,
) -> Result<DryRunReport, PipelineError> {
    let (analysis_version, sheet) = load_sheet(store).await?;
    let stale = stale_unlocked_idxs(store).await?;

    // Build every batch, then keep only those holding a stale-unlocked cue, reducing
    // each to that subset exactly as `apply_run_flags`/`rebuild_batch_subset` do.
    let mut affected = Vec::new();
    for mut batch in assemble_all_batches(cues, &sheet, params, analysis_version) {
        let subset: Vec<Cue> = batch
            .translate_cues
            .iter()
            .filter(|c| stale.contains(&c.idx))
            .cloned()
            .collect();
        if subset.is_empty() {
            continue;
        }
        if subset.len() != batch.translate_cues.len() {
            rebuild_batch_subset(
                &mut batch,
                subset,
                params,
                analysis_version,
                &sheet.formal_segments,
            );
        }
        affected.push(batch);
    }
    estimate_assembled(store, params, prices, &sheet, &affected).await
}

/// Estimates a set of already-assembled batches through the `DryRun` transport behind
/// the local cache (a hit is free), writing nothing. Shared by [`dry_run_translation`]
/// and [`dry_run_translation_stale`] so both price the same request shape.
async fn estimate_assembled(
    store: &Store,
    params: &TranslationParams,
    prices: &PriceTable,
    sheet: &Sheet,
    assembled: &[AssembledBatch],
) -> Result<DryRunReport, PipelineError> {
    let client = Cached::new(
        DryRun::new(params.estimator, params.output_ratio),
        store.clone(),
    );

    let mut prompt_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut cost = Cost::Known(0.0);
    let mut calls = 0usize;
    for batch in assembled {
        let req = build_request(
            &params.model,
            params.tier,
            params.thinking,
            &translation_system_instruction(params.digits),
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

/// One `formal_segments` range (`docs/DATA_MODEL.md` §3, ADR-049(b)): the inclusive
/// cue-idx bounds and the segment's `kind`. A TRANSLATE line whose cue idx falls in
/// this range carries the `(literary: <kind>)` marker (`docs/LLM.md` §2).
#[derive(Debug, Clone)]
struct FormalRange {
    first: i64,
    last: i64,
    kind: String,
}

/// The formal-segment `kind` for cue `idx`, when it falls inside a formal segment —
/// the marker text for its TRANSLATE line (`docs/LLM.md` §2, ADR-049(b)).
fn marker_for(segments: &[FormalRange], idx: u32) -> Option<String> {
    let idx = i64::from(idx);
    segments
        .iter()
        .find(|s| s.first <= idx && idx <= s.last)
        .map(|s| s.kind.clone())
}

/// The slice of the approved analysis the translation stage consults: the canonical
/// JSON for the cached prefix (ADR-006), the scenes, the id→name map, and the
/// glossary/name data the quality checks need (`docs/LLM.md` §8).
#[derive(Debug, Clone)]
struct Sheet {
    /// Canonical JSON of the whole document — byte-stable across batches.
    canonical: String,
    scenes: Vec<SceneInfo>,
    /// Formal-segment ranges and kinds, for the TRANSLATE-line `(literary: <kind>)`
    /// marker (`docs/LLM.md` §2, ADR-049(b)).
    formal_segments: Vec<FormalRange>,
    char_names: BTreeMap<String, String>,
    /// Glossary entries as `(term, target, keep)`.
    glossary: Vec<(String, String, bool)>,
    /// Character names and aliases, for the `untranslated` check.
    names: Vec<String>,
    /// Source name/alias → Burmese target-spelling pairs: `(name, name_target)` and,
    /// positionally, `(aliases[i], aliases_target[i])`, for the name coverage of the
    /// `glossary_miss` check (`docs/LLM.md` §8, ADR-049(d)).
    name_targets: Vec<(String, String)>,
}

impl Sheet {
    /// Parses the analysis document leniently (missing fields default to empty), so
    /// a sparse LLM document never aborts translation.
    fn parse(doc: &str) -> Sheet {
        let value: serde_json::Value = serde_json::from_str(doc).unwrap_or(serde_json::Value::Null);
        let canonical = canonical_json(&value);

        let mut char_names = BTreeMap::new();
        let mut names = Vec::new();
        let mut name_targets = Vec::new();
        if let Some(chars) = value.get("characters").and_then(|v| v.as_array()) {
            for ch in chars {
                let id = ch.get("id").and_then(|v| v.as_str()).unwrap_or_default();
                let name = ch.get("name").and_then(|v| v.as_str()).unwrap_or_default();
                let name_target = ch
                    .get("name_target")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default();
                if !id.is_empty() {
                    char_names.insert(id.to_string(), name.to_string());
                }
                if !name.is_empty() {
                    names.push(name.to_string());
                    if !name_target.is_empty() {
                        name_targets.push((name.to_string(), name_target.to_string()));
                    }
                }
                let aliases: Vec<&str> = ch
                    .get("aliases")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
                    .unwrap_or_default();
                let aliases_target: Vec<&str> = ch
                    .get("aliases_target")
                    .and_then(|v| v.as_array())
                    .map(|a| a.iter().filter_map(|x| x.as_str()).collect())
                    .unwrap_or_default();
                for (i, alias) in aliases.iter().enumerate() {
                    if alias.is_empty() {
                        continue;
                    }
                    names.push((*alias).to_string());
                    // Pair an alias with its target spelling only when `aliases_target`
                    // runs parallel (same length): a positional pair is meaningless
                    // otherwise (`docs/DATA_MODEL.md` §3).
                    if aliases_target.len() == aliases.len() && !aliases_target[i].is_empty() {
                        name_targets.push(((*alias).to_string(), aliases_target[i].to_string()));
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

        let mut formal_segments = Vec::new();
        if let Some(arr) = value.get("formal_segments").and_then(|v| v.as_array()) {
            for fs in arr {
                let range = fs.get("cue_range").and_then(|v| v.as_array());
                let (Some(first), Some(last)) = (
                    range.and_then(|r| r.first()).and_then(|v| v.as_i64()),
                    range.and_then(|r| r.get(1)).and_then(|v| v.as_i64()),
                ) else {
                    continue;
                };
                let kind = fs
                    .get("kind")
                    .and_then(|v| v.as_str())
                    .unwrap_or_default()
                    .to_string();
                if kind.is_empty() {
                    continue;
                }
                formal_segments.push(FormalRange {
                    first: first.min(last),
                    last: first.max(last),
                    kind,
                });
            }
        }

        Sheet {
            canonical,
            scenes,
            formal_segments,
            char_names,
            glossary,
            names,
            name_targets,
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
/// `input_hash`, the batch's cues (for mapping + quality), the line count, and the
/// scene header (summary + speakers) needed to re-render a smaller variable block
/// for a tier-2/tier-3 partial re-request (`docs/LLM.md` §6).
#[derive(Debug, Clone)]
struct AssembledBatch {
    batch_no: i64,
    first_cue: i64,
    last_cue: i64,
    variable_block: String,
    input_hash: String,
    translate_cues: Vec<Cue>,
    lines: u64,
    scene: String,
    speakers: Vec<(String, String)>,
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
        let before_lines = to_lines(ctx_before, &[]);
        let translate_lines = to_lines(group, &sheet.formal_segments);
        let after_lines = to_lines(ctx_after, &[]);
        let block = TranslationBlock {
            scene: &summary,
            speakers: &speakers,
            context_before: &before_lines,
            translate: &translate_lines,
            context_after: &after_lines,
        };
        let variable_block = block.render();
        let input_hash = compute_input_hash(
            &variable_block,
            analysis_version,
            params.model.as_str(),
            params.digits,
        );

        out.push(AssembledBatch {
            batch_no: i as i64 + 1,
            first_cue: first_idx,
            last_cue: last_idx,
            variable_block,
            input_hash,
            translate_cues: group.iter().map(|c| (*c).clone()).collect(),
            lines: group.len() as u64,
            scene: summary,
            speakers,
        });
    }
    out
}

/// Renders a slice of cues as prompt [`TranslationLine`]s; the speaker prefix is
/// carried only when the cue has a known speaker (`docs/LLM.md` §2). Each line is
/// marked with its formal-segment `kind` when its cue idx falls inside a
/// `formal_segments` range (`docs/LLM.md` §2, ADR-049(b)); pass `&[]` for context
/// lines, which are never marked.
fn to_lines(cues: &[&Cue], segments: &[FormalRange]) -> Vec<TranslationLine> {
    cues.iter()
        .map(|c| TranslationLine {
            idx: c.idx,
            speaker: c.speaker.clone().filter(|s| !s.is_empty()),
            text: c.text_masked.clone(),
            marker: marker_for(segments, c.idx),
        })
        .collect()
}

/// The `input_hash` of a batch (`docs/LLM.md` §3): sha256 of the variable block +
/// the analysis version + the model + [`PROMPT_VERSION`] + the digit style. A `done`
/// batch whose `input_hash` is unchanged is never resent, and a re-approved analysis
/// version changes the hash (the basis for M6-B2 selective invalidation). The digit
/// style is folded in so a pure `[burmese].digits` change moves the hash — the basis
/// for the done-batch reconciliation that marks unlocked cues stale (ADR-049(e),
/// ADR-050).
fn compute_input_hash(
    variable_block: &str,
    analysis_version: i64,
    model: &str,
    digits: DigitStyle,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(variable_block.as_bytes());
    hasher.update([0u8]);
    hasher.update(analysis_version.to_le_bytes());
    hasher.update([0u8]);
    hasher.update(model.as_bytes());
    hasher.update([0u8]);
    hasher.update(PROMPT_VERSION.to_le_bytes());
    hasher.update([0u8]);
    let digit_tag: u8 = match digits {
        DigitStyle::Myanmar => 0,
        DigitStyle::Western => 1,
    };
    hasher.update([digit_tag]);
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

/// Why a translation response is structurally unusable (`docs/DATA_MODEL.md` §4).
/// A structural failure is the tier-1 "unparseable / schema mismatch" case; the
/// message is logged, not surfaced to the user. A *parseable* response that merely
/// omits some ids is **not** a structural error — coverage is handled by the
/// content-retry loop (tier 2), which keeps the valid lines and re-requests the
/// missing ones.
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
}

/// Extracts a line's integer `id`, accepting both signed and unsigned JSON integers
/// (Gemini may emit either). Shared by [`parse_response`] so structural validation
/// and the parsed `(id, text)` pairs agree on what counts as an id (review).
fn extract_id(value: &serde_json::Value) -> Option<i64> {
    if value.is_i64() {
        value.as_i64()
    } else {
        value.as_u64().map(|u| u as i64)
    }
}

/// Parses a translation response against `{"lines":[{"id":int,"t":string}]}`
/// (`docs/DATA_MODEL.md` §4), returning the `(id, text)` pairs in order. A structural
/// problem (bad JSON, missing `lines`, a malformed entry) is a [`ValidationError`]
/// and drives the tier-1 whole-batch retry. Coverage of the expected ids is *not*
/// checked here; the caller compares the returned ids against what it still needs.
fn parse_response(text: &str) -> Result<Vec<(i64, String)>, ValidationError> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| ValidationError::Parse(e.to_string()))?;
    let obj = value.as_object().ok_or(ValidationError::NotObject)?;
    let lines = obj
        .get("lines")
        .and_then(|v| v.as_array())
        .ok_or(ValidationError::Lines)?;
    let mut out = Vec::with_capacity(lines.len());
    for entry in lines {
        let entry = entry.as_object().ok_or(ValidationError::Entry)?;
        let id = entry.get("id").and_then(extract_id);
        let t = entry.get("t").and_then(|v| v.as_str());
        match (id, t) {
            (Some(id), Some(t)) => out.push((id, t.to_string())),
            _ => return Err(ValidationError::Entry),
        }
    }
    Ok(out)
}

/// The "hard" quality flags that drive the tier-3 isolated per-line retry
/// (`docs/LLM.md` §6.3): a mismatched placeholder set, an untranslated target, or a
/// Zawgyi-suspect target. Every other flag is "soft" — stored, never retried.
const HARD_FLAGS: [QualityFlag; 3] = [
    QualityFlag::PlaceholderMismatch,
    QualityFlag::Untranslated,
    QualityFlag::ZawgyiSuspect,
];

/// Whether any flag in `flags` is a tier-3 hard flag.
fn has_hard_flag(flags: &[QualityFlag]) -> bool {
    flags.iter().any(|f| HARD_FLAGS.contains(f))
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
    name_targets: Vec<(String, String)>,
    /// The digit style for the `digit_style` quality check (ADR-049(e)).
    digits: DigitStyle,
    /// Formal-segment ranges, so a tier-2/tier-3 partial re-request keeps the
    /// `(literary: <kind>)` register markers on its TRANSLATE lines (ADR-049(b)).
    formal_segments: Vec<FormalRange>,
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
    scene: String,
    speakers: Vec<(String, String)>,
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
            scene: batch.scene,
            speakers: batch.speakers,
            state: AtomicU8::new(status_to_atom(status)),
            ctx,
        }
    }

    /// This run's glossary terms as borrowed [`GlossaryTerm`]s for the quality checks.
    fn glossary_terms(&self) -> Vec<GlossaryTerm<'_>> {
        self.ctx
            .glossary
            .iter()
            .map(|(term, target, keep)| GlossaryTerm {
                term,
                target,
                keep: *keep,
            })
            .collect()
    }

    /// Runs the per-cue quality checks on one `(cue, text)` pair (`docs/LLM.md` §8).
    fn cue_flags(&self, cue: &Cue, text: &str) -> Vec<QualityFlag> {
        let glossary = self.glossary_terms();
        let names: Vec<&str> = self.ctx.names.iter().map(String::as_str).collect();
        let ctx = AnalysisContext {
            glossary: &glossary,
            names: &names,
            name_targets: &self.ctx.name_targets,
            digits: self.ctx.digits,
        };
        checks::check_cue(cue, text, &ctx, &self.ctx.limits)
    }

    /// Runs the per-cue and batch-level quality checks and builds the rows to
    /// persist, mapping each returned line id (a cue `idx`) to its cue and
    /// `cues.id` (`docs/LLM.md` §8, `docs/DATA_MODEL.md` §4/§5).
    ///
    /// A returned id with no `cues.id` mapping is an invariant violation (the
    /// coverage gate has already guaranteed every batch cue is present), so this
    /// fails the batch rather than silently dropping the pair — a silent skip can
    /// never re-emerge after the coverage gate (review).
    fn build_translations(
        &self,
        lines: &[(i64, String)],
    ) -> Result<Vec<NewTranslation>, StoreError> {
        let glossary = self.glossary_terms();
        let names: Vec<&str> = self.ctx.names.iter().map(String::as_str).collect();
        let analysis_ctx = AnalysisContext {
            glossary: &glossary,
            names: &names,
            name_targets: &self.ctx.name_targets,
            digits: self.ctx.digits,
        };

        // Pair each returned line with its batch cue (ignore ids not in this batch).
        // Duplicate ids in one response are deduped first-wins so a cue is never
        // upserted twice with a spurious history row (review).
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

        let mut out = Vec::with_capacity(paired.len());
        for ((cue, text), flags) in paired.into_iter().zip(flags) {
            let cue_id = self.ctx.idx_to_id.get(&cue.idx).ok_or_else(|| {
                StoreError::Unexpected(format!(
                    "cue idx {} has no cues.id mapping in batch {}",
                    cue.idx, self.batch_no
                ))
            })?;
            out.push(NewTranslation {
                cue_id: *cue_id,
                text,
                origin: TranslationOrigin::Llm,
                locked: false,
                stale: false,
                flags,
                analysis_version: self.ctx.analysis_version,
                model: Some(self.ctx.model.clone()),
            });
        }
        Ok(out)
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
            CallResult::Valid { lines, commits } => {
                let translations = match self.build_translations(&lines) {
                    Ok(translations) => translations,
                    Err(err) => {
                        // A missing cue→id mapping is an invariant violation; fail the
                        // batch rather than committing a partial result (review).
                        let message = err.to_string();
                        self.mark_failed(&message).await;
                        return UnitOutcome::Failed;
                    }
                };
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
                        // Commit every contributing call's cache payload in the same
                        // transaction (tiers 2/3 may have made more than one call).
                        for commit in &commits {
                            if let Some(payload) = commit.payload() {
                                put_cache_in_tx(&tx, payload)?;
                            }
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

/// The outcome of the tiered content-retry loop (`docs/LLM.md` §6). `Valid` carries
/// the merged `(id, text)` pairs covering every batch cue and the cache-commit tokens
/// of every contributing call.
enum CallResult {
    Valid {
        lines: Vec<(i64, String)>,
        commits: Vec<CommitToken>,
    },
    Exhausted,
    Cancelled,
    Transport(LlmError),
}

/// What one generate call produced inside the content loop.
enum CallStep {
    /// A structurally valid response, parsed into `(id, text)` pairs.
    Parsed(Vec<(i64, String)>, CommitToken),
    /// A transport-successful but structurally invalid response (tier-1 case).
    Structural,
    Cancelled,
    Transport(LlmError),
}

impl<T> TranslationBatchUnit<T>
where
    T: LlmClient + Clone + Send + Sync + 'static,
{
    /// Resolves the batch through the tiered content-retry sequence (`docs/LLM.md`
    /// §6.2-§6.4), all bounded by the single `max_content_attempts` budget:
    ///
    /// 1. Reach id coverage: the first call is the whole batch; a structurally invalid
    ///    response retries the whole batch (tier 1), a partial response keeps the valid
    ///    lines and re-requests *only the missing ids* (tier 2). A retry that covers no
    ///    new id makes no progress, so the loop stops rather than looping forever.
    /// 2. Repair hard-flagged lines: once covered, cues carrying a hard quality flag
    ///    get one isolated re-request (tier 3); a result that clears the flag replaces
    ///    the original, otherwise the original (best result) is kept and stored flagged.
    ///
    /// The content-attempt counter is separate from the transport retries inside
    /// `Retrying`.
    async fn call_validated(&self) -> CallResult {
        let expected: BTreeSet<i64> = self
            .translate_cues
            .iter()
            .map(|c| i64::from(c.idx))
            .collect();
        if expected.is_empty() {
            return CallResult::Valid {
                lines: Vec::new(),
                commits: Vec::new(),
            };
        }

        let max = self.ctx.max_content_attempts.max(1);
        let mut accepted: BTreeMap<i64, String> = BTreeMap::new();
        let mut commits: Vec<CommitToken> = Vec::new();
        let mut attempts = 0u32;

        // --- Tiers 1 + 2: reach coverage of every expected id. ---
        loop {
            let missing: BTreeSet<i64> = expected
                .difference(&accepted_ids(&accepted))
                .copied()
                .collect();
            if missing.is_empty() {
                break;
            }
            if attempts >= max {
                return CallResult::Exhausted;
            }

            // First call: whole batch, no reminder. A retry with nothing accepted yet
            // re-sends the whole batch (tier 1); with partial coverage it re-requests
            // only the missing ids (tier 2). Retries always carry the strict reminder.
            let req = if attempts == 0 {
                self.base_request()
            } else if accepted.is_empty() {
                // Tier 1: the whole batch failed structural validation.
                with_reminder(self.base_request(), translation_retry_reminder())
            } else {
                // Tier 2: parseable, but some requested ids were missing.
                with_reminder(
                    self.subset_request(&missing),
                    translation_missing_ids_reminder(),
                )
            };
            attempts += 1;

            let before = accepted.len();
            match self.one_call(req).await {
                CallStep::Parsed(lines, commit) => {
                    commits.push(commit);
                    for (id, text) in lines {
                        if expected.contains(&id) {
                            accepted.entry(id).or_insert(text);
                        }
                    }
                }
                CallStep::Structural => {
                    tracing::debug!(
                        attempt = attempts,
                        "translation response failed structural validation; content retry"
                    );
                }
                CallStep::Cancelled => return CallResult::Cancelled,
                CallStep::Transport(err) => return CallResult::Transport(err),
            }

            // A retry that covered no new id is making no progress; stop before burning
            // the rest of the budget on an unresponsive model (`docs/LLM.md` §6).
            if accepted.len() == before && attempts >= 2 {
                return CallResult::Exhausted;
            }
        }

        // --- Tier 3: one isolated retry of hard-flagged lines. ---
        let hard: BTreeSet<i64> = self
            .translate_cues
            .iter()
            .filter(|c| {
                accepted
                    .get(&i64::from(c.idx))
                    .is_some_and(|text| has_hard_flag(&self.cue_flags(c, text)))
            })
            .map(|c| i64::from(c.idx))
            .collect();
        // Tier 3 is the terminal content attempt: it draws the last slot of the budget
        // (`attempts < max`) but nothing reads the counter after it.
        if !hard.is_empty()
            && attempts < max
            && let CallStep::Parsed(lines, commit) = self
                .one_call(with_reminder(
                    self.subset_request(&hard),
                    translation_per_line_reminder(),
                ))
                .await
        {
            commits.push(commit);
            for (id, text) in lines {
                if !hard.contains(&id) {
                    continue;
                }
                // Keep the retried text only if it clears the hard flag; otherwise
                // the original stays as the best result (stored with its flag).
                if let Some(cue) = self.translate_cues.iter().find(|c| i64::from(c.idx) == id)
                    && !has_hard_flag(&self.cue_flags(cue, &text))
                {
                    accepted.insert(id, text);
                }
            }
        }

        CallResult::Valid {
            lines: accepted.into_iter().collect(),
            commits,
        }
    }

    /// Makes one LLM call through the stack and classifies its result for the content
    /// loop.
    async fn one_call(&self, req: LlmRequest) -> CallStep {
        if self.ctx.token.is_cancelled() {
            return CallStep::Cancelled;
        }
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
            Ok(resp) => match parse_response(&resp.text) {
                Ok(lines) => CallStep::Parsed(lines, resp.commit),
                Err(_) => CallStep::Structural,
            },
            Err(LlmError::Cancelled) => CallStep::Cancelled,
            Err(err) => CallStep::Transport(err),
        }
    }

    /// Builds a request whose variable block translates only the cues in `ids` (a
    /// tier-2/tier-3 partial re-request), keeping the batch's scene header so the
    /// cached prefix is unchanged (`docs/LLM.md` §6). Context lines are omitted so the
    /// smaller request carries *only* the requested ids.
    fn subset_request(&self, ids: &BTreeSet<i64>) -> LlmRequest {
        let cues: Vec<&Cue> = self
            .translate_cues
            .iter()
            .filter(|c| ids.contains(&i64::from(c.idx)))
            .collect();
        let lines = to_lines(&cues, &self.ctx.formal_segments);
        let block = TranslationBlock {
            scene: &self.scene,
            speakers: &self.speakers,
            context_before: &[],
            translate: &lines,
            context_after: &[],
        };
        let variable_block = block.render();
        let budget = output_budget(
            &variable_block,
            &self.ctx.estimator,
            self.ctx.output_ratio,
            lines.len() as u64,
        );
        build_request(
            &self.ctx.model,
            self.ctx.tier,
            self.ctx.thinking,
            &self.ctx.system_instruction,
            &self.ctx.analysis_sheet,
            &variable_block,
            budget,
        )
    }
}

/// The ids currently accepted, as a set (for difference against `expected`).
fn accepted_ids(accepted: &BTreeMap<i64, String>) -> BTreeSet<i64> {
    accepted.keys().copied().collect()
}

/// Appends the tier-appropriate reminder as a trailing user block for a content retry
/// (`docs/LLM.md` §6). The caller passes the reminder matching the failure tier
/// (structural, missing-ids, or per-line).
fn with_reminder(mut req: LlmRequest, reminder: String) -> LlmRequest {
    req.contents.push(ContentBlock::user(reminder));
    req
}

#[cfg(test)]
mod tests {
    use super::super::scheduler::SchedulerConfig;
    use super::*;
    use crate::llm::retry::RetryConfig;
    use crate::quality::checks::Limits;
    use std::time::Duration;

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

    fn params(batch_size: u32, context: u32) -> TranslationParams {
        TranslationParams {
            model: "m".to_string(),
            tier: ServiceTier::Flex,
            thinking: ThinkingSetting::Off,
            batch_size,
            context_before: context,
            context_after: context,
            max_content_attempts: 3,
            output_ratio: 4.0,
            digits: DigitStyle::Myanmar,
            estimator: Estimator {
                bytes_per_token_latin: 4.0,
                bytes_per_token_myanmar: 3.0,
                bytes_per_token_cjk: 3.0,
            },
            limits: Limits {
                max_graphemes_per_line: 42,
                max_lines: 2,
                max_gps: 20.0,
                drift_ratio: 3.0,
            },
            scheduler: SchedulerConfig {
                concurrency: 1,
                warm_cache_first: false,
                grace: Duration::from_secs(5),
            },
            seed: 42,
            retry: RetryConfig {
                max_retries: 2,
                backoff_base_ms: 1,
                backoff_max_ms: 2,
                max_sheds_before_fallback: 1,
                service_tier_fallback: true,
            },
        }
    }

    // AC: M12-AC5 — a batch whose cue range spans a formal-segment boundary marks
    // exactly the in-range TRANSLATE lines; out-of-range TRANSLATE lines and ALL
    // context lines (even in-range ones) carry no marker (ADR-049(b), LLM.md §2).
    #[test]
    fn assembled_batch_marks_only_in_range_translate_lines() {
        // Formal segment covers cues 2..=5. Cues 1..=6 are all translatable.
        let doc = r#"{
            "schema_version":1,"premise":"p","characters":[],
            "scenes":[{"cue_range":[1,6],"summary":"s","speakers":[]}],
            "formal_segments":[{"cue_range":[2,5],"register":"literary","kind":"reading"}]
        }"#;
        let sheet = Sheet::parse(doc);
        let cues: Vec<Cue> = (1..=6).map(cue).collect();

        // batch_size 2 with 2 context lines each side → the middle batch is cues [3,4]
        // with context_before [1,2] and context_after [5,6], spanning the segment edges.
        let batches = assemble_all_batches(&cues, &sheet, &params(2, 2), 1);
        let middle = &batches[1];
        let block = &middle.variable_block;

        // In-range TRANSLATE lines are marked.
        assert!(
            block.contains("[3] (literary: reading) line 3"),
            "in-range translate line 3 must be marked: {block}"
        );
        assert!(
            block.contains("[4] (literary: reading) line 4"),
            "in-range translate line 4 must be marked: {block}"
        );
        // In-range CONTEXT lines (2 and 5) are NOT marked.
        assert!(
            !block.contains("[2] (literary"),
            "in-range context line 2 must be unmarked: {block}"
        );
        assert!(
            !block.contains("[5] (literary"),
            "in-range context line 5 must be unmarked: {block}"
        );
        // Out-of-range context lines are plain.
        assert!(block.contains("[1] line 1"), "{block}");
        assert!(block.contains("[6] line 6"), "{block}");
    }
}
