//! The analysis stage: single-pass and map-reduce execution, content retries,
//! versioning with provenance, and the `--dry-run` estimate
//! (`docs/LLM.md` §2/§5/§6/§9, `docs/ARCHITECTURE.md` §4/§6, `docs/DATA_MODEL.md`
//! §2/§3).
//!
//! The transcript is estimated (`ceil(utf8_bytes / bytes_per_token)` per script,
//! `docs/LLM.md` §9): at or below `analysis.single_pass_max_tokens` it is analyzed
//! in one call; above it, in a map-reduce where each chunk (≈ `analysis.chunk_cues`
//! cues) is a scheduler unit producing a partial document, and one reduce call
//! merges the partials in chunk order (`docs/LLM.md` §2).
//!
//! Crash-safety (`docs/ARCHITECTURE.md` §6): every chunk writes its partial result
//! and its `done` status in one transaction, and `Store::open` recovers `in_flight`
//! chunks to `pending`, so a rerun skips `done` chunks and reruns the rest without
//! re-sending a completed chunk's map call.
//!
//! Content retries (`docs/LLM.md` §6) live here, counted separately from the
//! transport retries inside `Retrying`: a transport-successful response that is not
//! valid JSON matching the analysis schema is retried — the whole unit — with a
//! strict reminder, up to `max_content_attempts`, after which the unit is failed.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

use tokio_util::sync::CancellationToken;

use crate::domain::Cue;
use crate::llm::assemble_stack;
use crate::llm::cache::Cached;
use crate::llm::client::{
    CommitToken, ContentBlock, GenerationConfig, LlmClient, LlmError, LlmRequest, ServiceTier,
    ThinkingSetting,
};
use crate::llm::dry_run::{DryRun, Estimator};
use crate::llm::metered::CallContext;
use crate::llm::pricing::{self, Cost, PriceTable};
use crate::llm::prompts::{
    CapKind, PROMPT_VERSION, analysis_cap_retry_reminder, analysis_input_line,
    analysis_reduce_instruction, analysis_system_instruction, strict_retry_reminder,
};
use crate::llm::retry::RetryConfig;
use crate::llm::schemas::analysis_response_schema;
use crate::store::repo_analysis::{
    AnalysisOrigin, current_analysis, insert_analysis_version_in_tx,
};
use crate::store::repo_cues::load_cues;
use crate::store::repo_llm::put_cache_in_tx;
use crate::store::repo_project::set_stage;
use crate::store::repo_units::{
    claim_analysis_chunk, clear_analysis_chunks, complete_analysis_chunk_in_tx,
    count_analysis_chunks_by_status, create_analysis_chunk, fail_analysis_chunk,
    list_all_analysis_chunks, list_analysis_chunks_by_status, requeue_failed_analysis_chunks,
    reset_analysis_chunk,
};
use crate::store::{Stage, StageStatus, Store, UnitStatus};

use super::confirm::{self, ConfirmSettings, ConfirmedRun, CostEstimate};
use super::error::PipelineError;
use super::input::check_source_unchanged;
use super::orchestrator::{can_run, load_state};
use super::progress::{Progress, ProgressStage};
use super::scheduler::{self, SchedulerConfig, Summary, UnitOutcome, UnitState, WorkUnit};

/// Characters per output token for the analysis-document estimate. The document is
/// predominantly English JSON, for which ~4 chars/token is standard; the few Burmese
/// fields (`self_ref`, `addressee_ref`, `particle`, glossary `target`) are short.
const OUTPUT_CHARS_PER_TOKEN: u64 = 4;

/// Headroom (in tokens) for thinking, which `maxOutputTokens` **includes** (verified
/// against the live Gemini `generateContent` docs, 2026-11); analysis runs with
/// `thinking = low`. Added on top of the worst-case document so a thinking budget
/// never eats into the document's room.
const ANALYSIS_THINKING_HEADROOM: u64 = 8_192;

/// The largest `maxOutputTokens` we will ever request for analysis — a value within
/// current Gemini Flash/Pro output-token limits, so the request itself is never
/// rejected for an out-of-range budget. The absolute schema-maximum document can
/// exceed this; such a pathological input may still truncate, but every realistic
/// large-cast analysis fits well under it (see `analysis_output_budget`).
const ANALYSIS_OUTPUT_CEILING: u32 = 65_536;

// ---------------------------------------------------------------------------
// Public surface
// ---------------------------------------------------------------------------

/// Everything the analysis executor needs that is not the transport, the store, or
/// the cues. Built from config + CLI overrides by the command handler; held as
/// plain data so the executor and its tests do not depend on the `config` type.
#[derive(Debug, Clone)]
pub struct AnalysisParams {
    /// The analysis model (`[llm.models].analysis`, overridable by `--model`).
    pub model: String,
    /// The service tier (`[llm].service_tier`, overridable by `--tier`).
    pub tier: ServiceTier,
    /// The analysis thinking setting (`[llm.thinking].analysis`).
    pub thinking: ThinkingSetting,
    /// The project source hash, stored as the version's `source_digest`
    /// (`docs/DATA_MODEL.md` §2).
    pub source_digest: String,
    /// Content-retry budget (`[translation].max_content_attempts`, reused for
    /// analysis — see the module/ADR notes).
    pub max_content_attempts: u32,
    /// Single-pass threshold in estimated tokens (`[analysis].single_pass_max_tokens`).
    pub single_pass_max_tokens: u64,
    /// Chunk size in cues for the map phase (`[analysis].chunk_cues`).
    pub chunk_cues: u32,
    /// Per-script token estimator (`[estimate]`).
    pub estimator: Estimator,
    /// Output-token ratio for the dry-run estimate (`[translation].output_ratio`).
    pub output_ratio: f64,
    /// Scheduler tuning for the map phase (`[translation].concurrency`/`warm_cache_first`,
    /// `[output].grace_secs`).
    pub scheduler: SchedulerConfig,
    /// Jitter seed for the transport retry backoff.
    pub seed: u64,
    /// Transport retry policy (`[llm]`).
    pub retry: RetryConfig,
}

/// Run shaping flags from the `analyze` command (`docs/CLI.md` §6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AnalysisFlags {
    /// Produce a new analysis version even if a current one exists.
    pub redo: bool,
    /// Rerun only failed chunks.
    pub retry_failed: bool,
}

/// Where the analysis executor stopped (`docs/CLI.md` §6/§7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AnalysisOutcome {
    /// A current analysis version already exists and `--redo` was not given.
    NothingToDo,
    /// A new analysis version was stored (single-pass, or a completed map-reduce).
    Completed { version: i64 },
    /// Some units failed after their content-retry budget; no version was stored.
    /// Maps to exit code 4 (`docs/CLI.md` §7).
    CompletedWithFailures { failed: usize },
    /// Interrupted by cancellation (Ctrl-C).
    Interrupted,
}

/// Whether a plan is single-pass or map-reduce (`docs/LLM.md` §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnalysisModeKind {
    SinglePass,
    MapReduce,
}

/// The `analyze --dry-run` report (`docs/LLM.md` §9, `docs/CLI.md` §6). Estimated,
/// never persisted.
#[derive(Debug, Clone, PartialEq)]
pub struct DryRunReport {
    pub mode: AnalysisModeKind,
    /// Number of map chunks (0 for single-pass).
    pub chunk_count: usize,
    /// Total logical calls the real run would make (1 for single-pass; chunks + 1
    /// reduce for map-reduce).
    pub calls: usize,
    pub estimated_prompt_tokens: u64,
    pub estimated_output_tokens: u64,
    pub estimated_cost: Cost,
}

/// The `"prompt_v{N}|{model}"` signature stored on an analysis version
/// (`docs/DATA_MODEL.md` §2). `N` is the current [`PROMPT_VERSION`].
pub fn analysis_signature(model: &str) -> String {
    format!("prompt_v{PROMPT_VERSION}|{model}")
}

/// The worst-case analysis-document size in output tokens, derived from the schema
/// caps (`docs/DATA_MODEL.md` §3) at [`OUTPUT_CHARS_PER_TOKEN`], including per-element
/// key/structure overhead. Used to size [`analysis_output_budget`].
pub fn worst_case_doc_tokens() -> u64 {
    // maxLength budgets × caps, plus a per-element allowance for ids, enums, keys,
    // quotes, and separators. (`docs/DATA_MODEL.md` §3.)
    let characters = 40 * (120 + 8 * 80 + 260 + 180 + 120); // name, aliases, role, speech_style, +ids/enums/keys
    let address_matrix = 80 * (160 + 120 + 120 + 60 + 120); // relation, self_ref, addressee_ref, particle, +from/to/register/keys
    let glossary = 120 * (120 + 120 + 180 + 60); // term, target, note, +keep/keys
    let scenes = 120 * (300 + 80 + 60 + 60); // summary, mood, +cue_range/speakers/keys
    let lists = 12 * 180 + 12 * 220; // style_rules + unresolved_ambiguities
    let top = 500 + 160 + 256; // premise + tone + schema_version/outer braces
    let chars: u64 = characters + address_matrix + glossary + scenes + lists + top;
    chars / OUTPUT_CHARS_PER_TOKEN
}

/// The analysis `maxOutputTokens` budget (`docs/LLM.md` §5: "Always set
/// `maxOutputTokens`"). The worst-case document plus thinking headroom (since
/// `maxOutputTokens` includes thinking tokens), bounded by [`ANALYSIS_OUTPUT_CEILING`]
/// so the request stays within the model's output limit. Comfortably above the old
/// fixed 16 384, so a large-cast single-pass or a full merged reduce is not truncated
/// into a content-retry failure for an input the schema permits.
pub fn analysis_output_budget() -> u32 {
    let want = worst_case_doc_tokens().saturating_add(ANALYSIS_THINKING_HEADROOM);
    want.min(u64::from(ANALYSIS_OUTPUT_CEILING)) as u32
}

/// Parses an `[llm.thinking]` label into a [`ThinkingSetting`]. A bare integer is a
/// token budget; an unrecognized label defaults to `low` (the analysis default,
/// `docs/LLM.md` §5.7).
pub fn parse_thinking(label: &str) -> ThinkingSetting {
    match label.trim().to_ascii_lowercase().as_str() {
        "off" => ThinkingSetting::Off,
        "minimal" => ThinkingSetting::Minimal,
        "low" => ThinkingSetting::Low,
        "medium" => ThinkingSetting::Medium,
        "high" => ThinkingSetting::High,
        other => other
            .parse::<u32>()
            .map(ThinkingSetting::Budget)
            .unwrap_or(ThinkingSetting::Low),
    }
}

/// Parses a service-tier label, defaulting to `flex` (the cost default,
/// `docs/LLM.md` §5).
pub fn parse_tier(label: &str) -> ServiceTier {
    match label.trim().to_ascii_lowercase().as_str() {
        "standard" => ServiceTier::Standard,
        _ => ServiceTier::Flex,
    }
}

/// Maps an [`AnalysisOutcome`] to its process exit code (`docs/CLI.md` §7).
/// Cancellation wins (130), then failed units (4), otherwise success (0).
pub fn analysis_exit_code(cancelled: bool, outcome: &AnalysisOutcome) -> i32 {
    use crate::error::exit;
    if cancelled {
        return exit::INTERRUPTED;
    }
    match outcome {
        AnalysisOutcome::Interrupted => exit::INTERRUPTED,
        AnalysisOutcome::CompletedWithFailures { .. } => exit::FAILED_UNITS,
        AnalysisOutcome::Completed { .. } | AnalysisOutcome::NothingToDo => exit::SUCCESS,
    }
}

// ---------------------------------------------------------------------------
// run_analysis
// ---------------------------------------------------------------------------

/// Runs the analysis stage over `cues` using `transport` for LLM calls
/// (`docs/ARCHITECTURE.md` §4). Decides single-pass vs map-reduce from the token
/// estimate, honours `--redo`/`--retry-failed`, and stores a new, unapproved
/// analysis version on success. The caller supplies a [`CancellationToken`] that is
/// threaded to the scheduler and every LLM call.
#[allow(clippy::too_many_arguments)]
pub async fn run_analysis<T>(
    transport: T,
    store: &Store,
    cues: &[Cue],
    params: &AnalysisParams,
    flags: AnalysisFlags,
    token: &CancellationToken,
    progress: Arc<dyn Progress>,
) -> Result<AnalysisOutcome, PipelineError>
where
    T: LlmClient + Clone + Send + Sync + 'static,
{
    // A current analysis version plus no `--redo` is nothing to do: editing uses
    // `--redo` to mint a new version (`docs/CLI.md` §6). A crashed map-reduce has no
    // version yet, so it is *not* nothing-to-do and resumes below.
    let current = store.call(|conn| current_analysis(conn)).await?;
    if current.is_some() && !flags.redo {
        return Ok(AnalysisOutcome::NothingToDo);
    }

    if flags.redo {
        // A fresh version starts from a clean map phase.
        store
            .call(|conn| clear_analysis_chunks(conn).map(|_| ()))
            .await?;
    }
    if flags.retry_failed {
        store
            .call(|conn| requeue_failed_analysis_chunks(conn).map(|_| ()))
            .await?;
    }

    let plan = plan_mode(
        cues,
        params.chunk_cues,
        params.single_pass_max_tokens,
        &params.estimator,
    );
    match plan.kind {
        AnalysisModeKind::SinglePass => {
            run_single_pass(&transport, store, cues, params, token, progress).await
        }
        AnalysisModeKind::MapReduce => {
            run_map_reduce(transport, store, cues, plan.chunks, params, token, progress).await
        }
    }
}

/// Runs the analysis stage behind the cost-confirmation gate (`docs/CLI.md` §5,
/// `docs/LLM.md` §9). Computes the pre-flight estimate with the dry-run estimator
/// (no LLM call), then [`confirm::gate`]s the real run: `make_transport` — and thus
/// the API key it needs — is only built once the gate is cleared, so a refused run
/// (non-TTY, no `--yes`) returns [`PipelineError::ConfirmationNeeded`] (exit 3)
/// without spending or requiring a key. `--yes` or an estimate at/under the
/// threshold proceeds silently.
#[allow(clippy::too_many_arguments)]
pub async fn run_analysis_confirmed<T, MK>(
    make_transport: MK,
    store: &Store,
    cues: &[Cue],
    params: &AnalysisParams,
    flags: AnalysisFlags,
    token: &CancellationToken,
    prices: &PriceTable,
    settings: ConfirmSettings,
    progress: Arc<dyn Progress>,
) -> Result<ConfirmedRun<AnalysisOutcome>, PipelineError>
where
    T: LlmClient + Clone + Send + Sync + 'static,
    MK: FnOnce() -> Result<T, PipelineError>,
{
    let report = dry_run_analysis(store, cues, params, prices).await?;
    let estimate = CostEstimate {
        prompt_tokens: report.estimated_prompt_tokens,
        output_tokens: report.estimated_output_tokens,
        cost: report.estimated_cost,
    };
    confirm::gate(estimate, settings, make_transport, |transport| {
        run_analysis(transport, store, cues, params, flags, token, progress)
    })
    .await
}

/// Enforces the analysis stage's preconditions and loads the cues it will analyze
/// (`docs/ARCHITECTURE.md` §2/§6). This is the command handler's shared front door:
/// it verifies the source has not changed since init (§6 rule 5), checks the
/// analysis gate (input completed, [`can_run`]), and returns the cues. A failed gate
/// is a [`PipelineError::GateViolation`] (exit 3); a changed source is
/// [`PipelineError::SourceChanged`] (exit 3).
pub async fn prepare_analysis(
    store: &Store,
    source_path: &Path,
) -> Result<Vec<Cue>, PipelineError> {
    check_source_unchanged(store, source_path).await?;
    let state = load_state(store).await?;
    can_run(Stage::Analysis, &state)?;
    let cues = store.call(|conn| load_cues(conn)).await?;
    Ok(cues)
}

async fn run_single_pass<T>(
    transport: &T,
    store: &Store,
    cues: &[Cue],
    params: &AnalysisParams,
    token: &CancellationToken,
    progress: Arc<dyn Progress>,
) -> Result<AnalysisOutcome, PipelineError>
where
    T: LlmClient + Clone + Send + Sync + 'static,
{
    // Single-pass is one logical unit of work; report it as a one-unit stage so the
    // operator still sees live progress on the common small-input path (§11).
    progress.stage_start(ProgressStage::Analysis);
    progress.stage_units(1);
    let req = transcript_request(full_transcript(cues), params);
    let env = CallEnv::new(transport, store, params, token);
    let result = call_validated(&env, None, req, validate_full).await;

    // Resolve the call (including the `store_version` DB write, which can itself fail)
    // into a single `(optional tick, summary, return value)` triple, then emit the
    // tick + `stage_finish` exactly once below. This guarantees a terminating
    // `stage_finish` on EVERY path — including the `store_version` error — so the
    // renderer's in-place line is always closed with a newline before any error text
    // reaches stderr and can never collide with it (§11, ADR-046(d)). A ticked unit is
    // only one that actually finished its work; a cancelled call or an error that
    // committed nothing finishes the stage without a tick (consistent with a dropped
    // post-grace tick).
    let (tick, summary, ret): (
        Option<UnitOutcome>,
        Summary,
        Result<AnalysisOutcome, PipelineError>,
    ) = match result {
        CallResult::Valid { text, commit } => {
            match store_version(store, text, commit, params).await {
                Ok(version) => (
                    Some(UnitOutcome::Done),
                    Summary {
                        done: 1,
                        ..Summary::default()
                    },
                    Ok(AnalysisOutcome::Completed { version }),
                ),
                Err(err) => (
                    None,
                    Summary {
                        failed: 1,
                        ..Summary::default()
                    },
                    Err(err),
                ),
            }
        }
        CallResult::Exhausted => (
            Some(UnitOutcome::Failed),
            Summary {
                failed: 1,
                ..Summary::default()
            },
            Ok(AnalysisOutcome::CompletedWithFailures { failed: 1 }),
        ),
        CallResult::Cancelled => (
            None,
            Summary {
                cancelled: 1,
                ..Summary::default()
            },
            Ok(AnalysisOutcome::Interrupted),
        ),
        CallResult::Transport(err) => (
            None,
            Summary {
                failed: 1,
                ..Summary::default()
            },
            Err(PipelineError::Llm(err)),
        ),
    };
    if let Some(outcome) = tick {
        progress.unit_finished(outcome);
    }
    progress.stage_finish(summary);
    ret
}

#[allow(clippy::too_many_arguments)]
async fn run_map_reduce<T>(
    transport: T,
    store: &Store,
    cues: &[Cue],
    chunks: Vec<ChunkBounds>,
    params: &AnalysisParams,
    token: &CancellationToken,
    progress: Arc<dyn Progress>,
) -> Result<AnalysisOutcome, PipelineError>
where
    T: LlmClient + Clone + Send + Sync + 'static,
{
    progress.stage_start(ProgressStage::Analysis);
    // On a fresh run, create the chunk rows and mark the stage running, all in one
    // transaction. On a resume the rows already exist and this is skipped, so a
    // `done` chunk keeps its committed result (`docs/ARCHITECTURE.md` §6).
    store
        .call(move |conn| {
            if list_all_analysis_chunks(conn)?.is_empty() {
                let tx = conn.transaction()?;
                for cb in &chunks {
                    create_analysis_chunk(&tx, cb.chunk_no, cb.first_cue, cb.last_cue)?;
                }
                set_stage(&tx, Stage::Analysis, StageStatus::Running)?;
                tx.commit()?;
            }
            Ok(())
        })
        .await?;

    // Build one unit per chunk from the authoritative DB rows (so resumes honour the
    // persisted bounds and statuses), with its transcript sliced from the cues.
    let rows = store.call(|conn| list_all_analysis_chunks(conn)).await?;
    let cues_arc = Arc::new(cues.to_vec());
    let ctx = Arc::new(ChunkExecCtx {
        transport,
        store: store.clone(),
        retry: params.retry.clone(),
        token: token.clone(),
        seed: params.seed,
        model: params.model.clone(),
        tier: params.tier,
        thinking: params.thinking,
        max_content_attempts: params.max_content_attempts.max(1),
    });
    let units: Vec<Arc<AnalysisChunkUnit<T>>> = rows
        .iter()
        .map(|row| {
            let transcript = range_transcript(&cues_arc, row.first_cue, row.last_cue);
            Arc::new(AnalysisChunkUnit::new(
                row.chunk_no,
                transcript,
                row.status,
                ctx.clone(),
            ))
        })
        .collect();

    let summary = scheduler::run(units, params.scheduler, token, progress.clone()).await;
    progress.stage_finish(summary);

    if token.is_cancelled() {
        return Ok(AnalysisOutcome::Interrupted);
    }

    // The DB is authoritative for the final decision, not the scheduler summary.
    let (failed, incomplete) = store
        .call(|conn| {
            let failed = count_analysis_chunks_by_status(conn, UnitStatus::Failed)?;
            let pending = count_analysis_chunks_by_status(conn, UnitStatus::Pending)?;
            let in_flight = count_analysis_chunks_by_status(conn, UnitStatus::InFlight)?;
            Ok((failed, pending + in_flight))
        })
        .await?;
    if failed > 0 || incomplete > 0 {
        // Never fabricate a reduce from incomplete partials (`docs/LLM.md` §2).
        return Ok(AnalysisOutcome::CompletedWithFailures {
            failed: failed + incomplete,
        });
    }

    // All chunks done → reduce their partials in chunk order.
    let partials: Vec<String> = store
        .call(|conn| list_analysis_chunks_by_status(conn, UnitStatus::Done))
        .await?
        .into_iter()
        .filter_map(|c| c.result)
        .collect();

    let req = reduce_request(&partials, params);
    let env = CallEnv::new(&ctx.transport, store, params, token);
    match call_validated(&env, None, req, validate_full).await {
        CallResult::Valid { text, commit } => {
            let version = store_version(store, text, commit, params).await?;
            Ok(AnalysisOutcome::Completed { version })
        }
        CallResult::Exhausted => Ok(AnalysisOutcome::CompletedWithFailures { failed: 1 }),
        CallResult::Cancelled => Ok(AnalysisOutcome::Interrupted),
        CallResult::Transport(err) => Err(PipelineError::Llm(err)),
    }
}

/// Stores a new analysis version plus its cache payload and the completed stage
/// state, all in one transaction (`docs/ARCHITECTURE.md` §6 rule 1).
async fn store_version(
    store: &Store,
    doc: String,
    commit: CommitToken,
    params: &AnalysisParams,
) -> Result<i64, PipelineError> {
    let digest = params.source_digest.clone();
    let signature = analysis_signature(&params.model);
    let version = store
        .call(move |conn| {
            let tx = conn.transaction()?;
            let version =
                insert_analysis_version_in_tx(&tx, &doc, AnalysisOrigin::Llm, &digest, &signature)?;
            if let Some(payload) = commit.payload() {
                put_cache_in_tx(&tx, payload)?;
            }
            set_stage(&tx, Stage::Analysis, StageStatus::Completed)?;
            tx.commit()?;
            Ok(version)
        })
        .await?;
    Ok(version)
}

// ---------------------------------------------------------------------------
// Dry run
// ---------------------------------------------------------------------------

/// Produces the `analyze --dry-run` report without persisting anything
/// (`docs/LLM.md` §9). The real plan is computed and the real prompts are built;
/// each would-be call is estimated through the `DryRun` transport behind the local
/// cache (a hit is free), and nothing is written to the database.
pub async fn dry_run_analysis(
    store: &Store,
    cues: &[Cue],
    params: &AnalysisParams,
    prices: &PriceTable,
) -> Result<DryRunReport, PipelineError> {
    let plan = plan_mode(
        cues,
        params.chunk_cues,
        params.single_pass_max_tokens,
        &params.estimator,
    );

    // DryRun behind Cached: real local-cache lookups, estimated misses, no writes
    // (nothing is ever committed, so the cache is only read).
    let client = Cached::new(
        DryRun::new(params.estimator, params.output_ratio),
        store.clone(),
    );

    let requests: Vec<LlmRequest> = match plan.kind {
        AnalysisModeKind::SinglePass => vec![transcript_request(full_transcript(cues), params)],
        AnalysisModeKind::MapReduce => {
            let mut reqs: Vec<LlmRequest> = plan
                .chunks
                .iter()
                .map(|cb| {
                    transcript_request(range_transcript(cues, cb.first_cue, cb.last_cue), params)
                })
                .collect();
            // Approximate the reduce input by the chunk transcripts (we have no real
            // partials in a dry run); this tends to overestimate, which is the safe
            // direction for a cost estimate (ADR-029).
            let proxy: Vec<String> = plan
                .chunks
                .iter()
                .map(|cb| range_transcript(cues, cb.first_cue, cb.last_cue))
                .collect();
            reqs.push(reduce_request(&proxy, params));
            reqs
        }
    };

    let mut prompt_tokens = 0u64;
    let mut output_tokens = 0u64;
    let mut cost = Cost::Known(0.0);
    let mut calls = 0usize;
    for req in requests {
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
        mode: plan.kind,
        chunk_count: plan.chunks.len(),
        calls,
        estimated_prompt_tokens: prompt_tokens,
        estimated_output_tokens: output_tokens,
        estimated_cost: cost,
    })
}

/// Sums two [`Cost`]s; an unknown component makes the total unknown
/// (`docs/LLM.md` §9).
fn add_cost(acc: Cost, next: Cost) -> Cost {
    match (acc, next) {
        (Cost::Known(a), Cost::Known(b)) => Cost::Known(a + b),
        _ => Cost::Unknown,
    }
}

// ---------------------------------------------------------------------------
// Planning + transcript
// ---------------------------------------------------------------------------

/// A map-phase chunk's cue-index bounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ChunkBounds {
    chunk_no: i64,
    first_cue: i64,
    last_cue: i64,
}

/// A chosen analysis plan.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Plan {
    kind: AnalysisModeKind,
    /// Empty for single-pass.
    chunks: Vec<ChunkBounds>,
}

/// Chooses single-pass vs map-reduce from the full-transcript token estimate
/// (`docs/LLM.md` §2/§9).
fn plan_mode(
    cues: &[Cue],
    chunk_cues: u32,
    single_pass_max_tokens: u64,
    estimator: &Estimator,
) -> Plan {
    let transcript = full_transcript(cues);
    let tokens = estimator.estimate_tokens(&transcript);
    if tokens <= single_pass_max_tokens {
        return Plan {
            kind: AnalysisModeKind::SinglePass,
            chunks: Vec::new(),
        };
    }

    let included: Vec<&Cue> = included_cues(cues);
    let size = (chunk_cues.max(1)) as usize;
    let mut chunks = Vec::new();
    for (i, group) in included.chunks(size).enumerate() {
        if let (Some(first), Some(last)) = (group.first(), group.last()) {
            chunks.push(ChunkBounds {
                chunk_no: i as i64 + 1,
                first_cue: i64::from(first.idx),
                last_cue: i64::from(last.idx),
            });
        }
    }
    Plan {
        kind: AnalysisModeKind::MapReduce,
        chunks,
    }
}

/// The cues that contribute transcript lines: translatable cues with non-blank
/// masked text (`docs/LLM.md` §2 "tags removed").
fn included_cues(cues: &[Cue]) -> Vec<&Cue> {
    cues.iter()
        .filter(|c| c.translatable && !c.text_masked.trim().is_empty())
        .collect()
}

/// Renders one transcript line as `[idx] Speaker: text` / `[idx] text`
/// (`docs/LLM.md` §2).
fn render_line(cue: &Cue) -> String {
    analysis_input_line(cue.idx, cue.speaker.as_deref(), &cue.text_masked)
}

/// The full transcript of all included cues.
fn full_transcript(cues: &[Cue]) -> String {
    included_cues(cues)
        .into_iter()
        .map(render_line)
        .collect::<Vec<_>>()
        .join("\n")
}

/// The transcript of the included cues whose index lies in `[first, last]`.
fn range_transcript(cues: &[Cue], first: i64, last: i64) -> String {
    included_cues(cues)
        .into_iter()
        .filter(|c| {
            let idx = i64::from(c.idx);
            idx >= first && idx <= last
        })
        .map(render_line)
        .collect::<Vec<_>>()
        .join("\n")
}

// ---------------------------------------------------------------------------
// Request builders
// ---------------------------------------------------------------------------

/// Builds an analysis request carrying `transcript` (single-pass, or one map
/// chunk). The system instruction and schema are fixed per [`PROMPT_VERSION`].
fn transcript_request(transcript: String, params: &AnalysisParams) -> LlmRequest {
    LlmRequest {
        model: params.model.clone(),
        service_tier: params.tier,
        system_instruction: analysis_system_instruction(),
        contents: vec![ContentBlock::user(transcript)],
        generation_config: generation_config(params.thinking),
    }
}

/// Builds the reduce request: the partial documents, in chunk order, under the
/// reduce instruction (`docs/LLM.md` §2).
fn reduce_request(partials: &[String], params: &AnalysisParams) -> LlmRequest {
    let joined = partials
        .iter()
        .enumerate()
        .map(|(i, p)| format!("PARTIAL {}:\n{p}", i + 1))
        .collect::<Vec<_>>()
        .join("\n\n");
    let text = format!("{}\n\n{joined}", analysis_reduce_instruction());
    LlmRequest {
        model: params.model.clone(),
        service_tier: params.tier,
        system_instruction: analysis_system_instruction(),
        contents: vec![ContentBlock::user(text)],
        generation_config: generation_config(params.thinking),
    }
}

fn generation_config(thinking: ThinkingSetting) -> GenerationConfig {
    GenerationConfig {
        max_output_tokens: analysis_output_budget(),
        response_mime_type: "application/json".to_string(),
        response_schema: Some(analysis_response_schema()),
        thinking,
    }
}

/// Appends the content-retry reminder as a trailing user block, selecting the
/// cap-aware reminder when the triggering failure was a cap violation and the
/// generic JSON-shape reminder otherwise (`docs/LLM.md` §6).
fn with_reminder(mut req: LlmRequest, last_error: Option<&ValidationError>) -> LlmRequest {
    req.contents
        .push(ContentBlock::user(retry_reminder_for(last_error)));
    req
}

/// Chooses the content-retry reminder text for the failure that triggered the retry
/// (`docs/LLM.md` §6). A cap violation — valid JSON over a per-field limit — gets the
/// corrective, cap-aware reminder naming the field and its limit; every genuine
/// JSON/shape failure (parse error, missing field, wrong type) keeps the existing
/// strict reminder. A missing error (defensive) also uses the strict reminder.
fn retry_reminder_for(last_error: Option<&ValidationError>) -> String {
    match last_error {
        Some(ValidationError::CapExceeded { field, cap, actual }) => {
            analysis_cap_retry_reminder(field, CapKind::Entries, *cap, *actual)
        }
        Some(ValidationError::StringTooLong { field, cap, actual }) => {
            analysis_cap_retry_reminder(field, CapKind::Characters, *cap, *actual)
        }
        _ => strict_retry_reminder(),
    }
}

// ---------------------------------------------------------------------------
// LLM call + content retry
// ---------------------------------------------------------------------------

/// The shared inputs for a validated LLM call (avoids threading five arguments).
struct CallEnv<'a, T> {
    transport: &'a T,
    store: &'a Store,
    retry: &'a RetryConfig,
    seed: u64,
    token: &'a CancellationToken,
    max_content_attempts: u32,
}

impl<'a, T> CallEnv<'a, T> {
    fn new(
        transport: &'a T,
        store: &'a Store,
        params: &'a AnalysisParams,
        token: &'a CancellationToken,
    ) -> Self {
        Self {
            transport,
            store,
            retry: &params.retry,
            seed: params.seed,
            token,
            max_content_attempts: params.max_content_attempts.max(1),
        }
    }
}

/// The outcome of a validated call (`docs/LLM.md` §6).
enum CallResult {
    /// A transport-successful, schema-valid response and its cache-commit token.
    Valid { text: String, commit: CommitToken },
    /// The content-retry budget was exhausted on invalid responses.
    Exhausted,
    /// The operation was cancelled.
    Cancelled,
    /// A terminal transport error (after transport retries).
    Transport(LlmError),
}

/// Calls the LLM stack and validates the response, retrying the whole call with a
/// strict reminder on a content failure, up to `max_content_attempts`
/// (`docs/LLM.md` §6). The content-attempt counter here is separate from the
/// transport retries inside `Retrying`.
async fn call_validated<T, V>(
    env: &CallEnv<'_, T>,
    unit: Option<i64>,
    base_req: LlmRequest,
    validate: V,
) -> CallResult
where
    T: LlmClient + Clone + Send + Sync + 'static,
    V: Fn(&str) -> Result<(), ValidationError>,
{
    let mut attempt = 0u32;
    let mut last_error: Option<ValidationError> = None;
    loop {
        if env.token.is_cancelled() {
            return CallResult::Cancelled;
        }
        let req = if attempt == 0 {
            base_req.clone()
        } else {
            with_reminder(base_req.clone(), last_error.as_ref())
        };

        let ctx = CallContext {
            stage: Stage::Analysis.as_str().to_string(),
            unit,
            dry_run: false,
        };
        let stack = assemble_stack(
            env.transport.clone(),
            env.retry.clone(),
            env.token.clone(),
            env.seed,
            env.store.clone(),
            env.store.clone(),
            ctx,
        );

        match stack.generate(req).await {
            Ok(resp) => match validate(&resp.text) {
                Ok(()) => {
                    return CallResult::Valid {
                        text: resp.text,
                        commit: resp.commit,
                    };
                }
                Err(err) => {
                    tracing::debug!(
                        attempt,
                        "analysis response failed validation ({err}); content retry"
                    );
                    last_error = Some(err);
                    attempt += 1;
                    if attempt >= env.max_content_attempts {
                        return CallResult::Exhausted;
                    }
                }
            },
            Err(LlmError::Cancelled) => return CallResult::Cancelled,
            Err(err) => return CallResult::Transport(err),
        }
    }
}

// ---------------------------------------------------------------------------
// Validation
// ---------------------------------------------------------------------------

/// Why a response is not a usable analysis document (`docs/DATA_MODEL.md` §3). Used
/// only to drive the content retry; the message is logged, not surfaced to the user.
#[derive(Debug, thiserror::Error)]
enum ValidationError {
    #[error("response is not valid JSON: {0}")]
    Parse(String),
    #[error("response is not a JSON object")]
    NotObject,
    #[error("missing required field `{0}`")]
    Missing(&'static str),
    #[error("field `{0}` has the wrong type")]
    WrongType(&'static str),
    #[error("array `{field}` has {actual} entries, over its cap of {cap}")]
    CapExceeded {
        field: &'static str,
        cap: usize,
        actual: usize,
    },
    #[error("string `{field}` is {actual} chars, over its length cap of {cap}")]
    StringTooLong {
        field: &'static str,
        cap: usize,
        actual: usize,
    },
}

/// Validates a full analysis document leniently (`docs/DATA_MODEL.md` §3): it must
/// parse, be an object, carry the required fields with the right JSON types, and
/// respect both the array caps and the per-field string-length caps. Unknown
/// fields are ignored (lenient on LLM output).
///
/// These caps used to be `maxItems`/`maxLength` in the Gemini wire schema but were
/// removed from it: the large `maxItems` array bounds overflowed Gemini's
/// constrained-decoding serving ceiling (HTTP 400 "too many states for serving",
/// ADR-044). The `maxLength` caps were dropped too, but as cleanup — Gemini ignores
/// `maxLength` during constrained decoding, so enforcing string lengths here also
/// closed a latent gap where over-length strings on the LLM output path were
/// previously validated by nothing. They are enforced here as a hard validation
/// error — over-cap output drives a content retry (`docs/LLM.md` §6) and, if never
/// satisfied, fails the unit loudly rather than silently truncating. String lengths
/// are measured in `chars().count()` (not graphemes), consistent with the
/// user-import validator (ADR-036).
fn validate_full(text: &str) -> Result<(), ValidationError> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| ValidationError::Parse(e.to_string()))?;
    let obj = value.as_object().ok_or(ValidationError::NotObject)?;

    if !obj.get("schema_version").is_some_and(|v| v.is_number()) {
        return Err(if obj.contains_key("schema_version") {
            ValidationError::WrongType("schema_version")
        } else {
            ValidationError::Missing("schema_version")
        });
    }
    if !obj.get("premise").is_some_and(|v| v.is_string()) {
        return Err(if obj.contains_key("premise") {
            ValidationError::WrongType("premise")
        } else {
            ValidationError::Missing("premise")
        });
    }
    string_cap(obj.get("premise"), "premise", 500)?;
    string_cap(obj.get("tone"), "tone", 160)?;

    require_array_within(obj, "characters", 40)?;
    for ch in object_items(obj, "characters") {
        string_cap(ch.get("name"), "characters[].name", 120)?;
        string_cap(ch.get("role"), "characters[].role", 260)?;
        string_cap(ch.get("speech_style"), "characters[].speech_style", 180)?;
        array_cap(ch.get("aliases"), "characters[].aliases", 8)?;
        for alias in string_items(ch.get("aliases")) {
            string_cap(Some(alias), "characters[].aliases[]", 80)?;
        }
    }

    require_array_within(obj, "scenes", 120)?;
    for scene in object_items(obj, "scenes") {
        string_cap(scene.get("summary"), "scenes[].summary", 300)?;
        string_cap(scene.get("mood"), "scenes[].mood", 80)?;
    }

    // These top-level arrays are optional, but when present must respect their caps.
    optional_array_within(obj, "address_matrix", 80)?;
    for am in object_items(obj, "address_matrix") {
        string_cap(am.get("relation"), "address_matrix[].relation", 160)?;
    }

    optional_array_within(obj, "glossary", 120)?;
    for g in object_items(obj, "glossary") {
        string_cap(g.get("term"), "glossary[].term", 120)?;
        string_cap(g.get("target"), "glossary[].target", 120)?;
        string_cap(g.get("note"), "glossary[].note", 180)?;
    }

    array_cap(obj.get("style_rules"), "style_rules", 12)?;
    for rule in string_items(obj.get("style_rules")) {
        string_cap(Some(rule), "style_rules[]", 180)?;
    }
    array_cap(
        obj.get("unresolved_ambiguities"),
        "unresolved_ambiguities",
        12,
    )?;
    for amb in string_items(obj.get("unresolved_ambiguities")) {
        string_cap(Some(amb), "unresolved_ambiguities[]", 220)?;
    }
    Ok(())
}

/// Enforces a string field's length cap in `chars` (`docs/DATA_MODEL.md` §3).
/// Absent or non-string values are skipped: presence/type of required fields is
/// checked elsewhere, and the wire schema still constrains field types.
fn string_cap(
    value: Option<&serde_json::Value>,
    field: &'static str,
    cap: usize,
) -> Result<(), ValidationError> {
    if let Some(serde_json::Value::String(s)) = value {
        let actual = s.chars().count();
        if actual > cap {
            return Err(ValidationError::StringTooLong { field, cap, actual });
        }
    }
    Ok(())
}

/// Enforces an array field's element-count cap. Absent or non-array values are
/// skipped (the wire schema still constrains the field type).
fn array_cap(
    value: Option<&serde_json::Value>,
    field: &'static str,
    cap: usize,
) -> Result<(), ValidationError> {
    if let Some(serde_json::Value::Array(a)) = value
        && a.len() > cap
    {
        return Err(ValidationError::CapExceeded {
            field,
            cap,
            actual: a.len(),
        });
    }
    Ok(())
}

/// Iterates the object elements of a top-level array field (non-objects skipped).
fn object_items<'a>(
    obj: &'a serde_json::Map<String, serde_json::Value>,
    field: &str,
) -> impl Iterator<Item = &'a serde_json::Map<String, serde_json::Value>> {
    obj.get(field)
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_object())
}

/// Iterates the elements of an array value (any element type).
fn string_items(value: Option<&serde_json::Value>) -> impl Iterator<Item = &serde_json::Value> {
    value.and_then(|v| v.as_array()).into_iter().flatten()
}

/// Validates a map-phase partial leniently: it must parse and be a JSON object
/// (`docs/LLM.md` §2). Partials are merged by the reduce call, so field-level
/// completeness is not required here.
fn validate_partial(text: &str) -> Result<(), ValidationError> {
    let value: serde_json::Value =
        serde_json::from_str(text).map_err(|e| ValidationError::Parse(e.to_string()))?;
    if value.is_object() {
        Ok(())
    } else {
        Err(ValidationError::NotObject)
    }
}

fn require_array_within(
    obj: &serde_json::Map<String, serde_json::Value>,
    field: &'static str,
    cap: usize,
) -> Result<(), ValidationError> {
    match obj.get(field) {
        Some(serde_json::Value::Array(a)) if a.len() <= cap => Ok(()),
        Some(serde_json::Value::Array(a)) => Err(ValidationError::CapExceeded {
            field,
            cap,
            actual: a.len(),
        }),
        Some(_) => Err(ValidationError::WrongType(field)),
        None => Err(ValidationError::Missing(field)),
    }
}

fn optional_array_within(
    obj: &serde_json::Map<String, serde_json::Value>,
    field: &'static str,
    cap: usize,
) -> Result<(), ValidationError> {
    match obj.get(field) {
        Some(serde_json::Value::Array(a)) if a.len() <= cap => Ok(()),
        Some(serde_json::Value::Array(a)) => Err(ValidationError::CapExceeded {
            field,
            cap,
            actual: a.len(),
        }),
        Some(_) => Err(ValidationError::WrongType(field)),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// Map-phase work unit
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

/// Shared execution context for every map chunk of one run.
struct ChunkExecCtx<T> {
    transport: T,
    store: Store,
    retry: RetryConfig,
    token: CancellationToken,
    seed: u64,
    model: String,
    tier: ServiceTier,
    thinking: ThinkingSetting,
    max_content_attempts: u32,
}

/// One map-phase chunk as a [`WorkUnit`] (`docs/ARCHITECTURE.md` §4/§6). Its `run`
/// claims the chunk, calls the LLM with the chunk transcript, validates the partial
/// (with content retries), and writes the partial result and `done` status in one
/// transaction.
struct AnalysisChunkUnit<T> {
    chunk_no: i64,
    transcript: String,
    /// The scheduler reads [`WorkUnit::state`] synchronously, so the DB status is
    /// cached here and updated as the unit progresses.
    state: AtomicU8,
    ctx: Arc<ChunkExecCtx<T>>,
}

impl<T> AnalysisChunkUnit<T> {
    fn new(
        chunk_no: i64,
        transcript: String,
        status: UnitStatus,
        ctx: Arc<ChunkExecCtx<T>>,
    ) -> Self {
        Self {
            chunk_no,
            transcript,
            state: AtomicU8::new(status_to_atom(status)),
            ctx,
        }
    }

    fn call_env(&self) -> CallEnv<'_, T> {
        CallEnv {
            transport: &self.ctx.transport,
            store: &self.ctx.store,
            retry: &self.ctx.retry,
            seed: self.ctx.seed,
            token: &self.ctx.token,
            max_content_attempts: self.ctx.max_content_attempts,
        }
    }
}

impl<T> WorkUnit for AnalysisChunkUnit<T>
where
    T: LlmClient + Clone + Send + Sync + 'static,
{
    fn id(&self) -> u64 {
        self.chunk_no.max(0) as u64
    }

    fn kind(&self) -> &str {
        "analysis_chunk"
    }

    fn state(&self) -> UnitState {
        atom_to_state(self.state.load(Ordering::Relaxed))
    }

    async fn reset(&self) {
        let chunk_no = self.chunk_no;
        let store = self.ctx.store.clone();
        let _ = store
            .call(move |conn| reset_analysis_chunk(conn, chunk_no).map(|_| ()))
            .await;
        self.state.store(ST_PENDING, Ordering::Relaxed);
    }

    async fn run(&self) -> UnitOutcome {
        let chunk_no = self.chunk_no;

        // Claim: pending → in_flight. A lost claim means the chunk is no longer
        // pending (already done/claimed); skip it as done.
        let store = self.ctx.store.clone();
        match store
            .call(move |conn| claim_analysis_chunk(conn, chunk_no))
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

        let params_thinking = self.ctx.thinking;
        let req = LlmRequest {
            model: self.ctx.model.clone(),
            service_tier: self.ctx.tier,
            system_instruction: analysis_system_instruction(),
            contents: vec![ContentBlock::user(self.transcript.clone())],
            generation_config: generation_config(params_thinking),
        };

        let env = self.call_env();
        match call_validated(&env, Some(chunk_no), req, validate_partial).await {
            CallResult::Valid { text, commit } => {
                let store = self.ctx.store.clone();
                let committed = store
                    .call(move |conn| {
                        let tx = conn.transaction()?;
                        let ok = complete_analysis_chunk_in_tx(&tx, chunk_no, &text)?;
                        if let Some(payload) = commit.payload() {
                            put_cache_in_tx(&tx, payload)?;
                        }
                        tx.commit()?;
                        Ok(ok)
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
                self.mark_failed(chunk_no, "content validation failed after retries")
                    .await;
                UnitOutcome::Failed
            }
            CallResult::Transport(err) => {
                let message = err.to_string();
                self.mark_failed(chunk_no, &message).await;
                UnitOutcome::Failed
            }
            CallResult::Cancelled => {
                // Leave the chunk `in_flight`; `Store::open` recovers it to `pending`
                // so the rerun resumes it without re-sending a completed map call.
                UnitOutcome::Failed
            }
        }
    }
}

impl<T> AnalysisChunkUnit<T> {
    async fn mark_failed(&self, chunk_no: i64, message: &str) {
        let store = self.ctx.store.clone();
        let message = message.to_string();
        let _ = store
            .call(move |conn| fail_analysis_chunk(conn, chunk_no, &message).map(|_| ()))
            .await;
        self.state.store(ST_FAILED, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::{ValidationError, retry_reminder_for, validate_full};
    use serde_json::json;

    // QA item D (ADR-045): a retry triggered by an over-length string selects the
    // corrective, cap-aware reminder — it names the field and its limit and tells the
    // model to shorten, and does NOT falsely claim the JSON was invalid.
    #[test]
    fn retry_reminder_for_string_cap_is_cap_aware() {
        let err = ValidationError::StringTooLong {
            field: "premise",
            cap: 500,
            actual: 612,
        };
        let text = retry_reminder_for(Some(&err));
        assert!(text.contains("premise"), "names the field: {text}");
        assert!(text.contains("500"), "states the limit: {text}");
        assert!(text.contains("612"), "states the offending value: {text}");
        assert!(
            text.to_lowercase().contains("shorten"),
            "tells to shorten: {text}"
        );
        assert!(
            text.contains("ONLY a single JSON object"),
            "stays parseable: {text}"
        );
        assert!(
            !text.to_lowercase().contains("not valid json"),
            "must not claim invalid JSON: {text}"
        );
    }

    // QA item D (ADR-045): a retry triggered by an over-cap array selects the cap-aware
    // reminder naming the entry limit.
    #[test]
    fn retry_reminder_for_array_cap_is_cap_aware() {
        let err = ValidationError::CapExceeded {
            field: "unresolved_ambiguities",
            cap: 12,
            actual: 15,
        };
        let text = retry_reminder_for(Some(&err));
        assert!(
            text.contains("unresolved_ambiguities"),
            "names the field: {text}"
        );
        assert!(text.contains("12"), "states the limit: {text}");
        assert!(text.contains("entries"), "describes an entry cap: {text}");
        assert!(
            !text.to_lowercase().contains("not valid json"),
            "must not claim invalid JSON: {text}"
        );
    }

    // QA item D (ADR-045): a non-cap validation failure (missing field, parse error,
    // wrong type) keeps the existing JSON-shape reminder, unchanged.
    #[test]
    fn retry_reminder_for_non_cap_failure_uses_strict_reminder() {
        for err in [
            ValidationError::Missing("premise"),
            ValidationError::Parse("expected value".to_string()),
            ValidationError::NotObject,
            ValidationError::WrongType("schema_version"),
        ] {
            let text = retry_reminder_for(Some(&err));
            assert_eq!(
                text,
                super::strict_retry_reminder(),
                "non-cap keeps strict text"
            );
            assert!(
                text.contains("not valid JSON"),
                "strict reminder text: {text}"
            );
        }
        // A missing error (defensive) also falls back to the strict reminder.
        assert_eq!(retry_reminder_for(None), super::strict_retry_reminder());
    }

    /// A minimal analysis document that satisfies every §3 cap.
    fn base_doc() -> serde_json::Value {
        json!({
            "schema_version": 1,
            "premise": "A short premise.",
            "tone": "wistful",
            "characters": [
                {
                    "id": "c1",
                    "name": "Aoi",
                    "aliases": ["A"],
                    "gender": "f",
                    "age_group": "young",
                    "role": "the lead",
                    "speech_style": "clipped"
                }
            ],
            "address_matrix": [
                { "from": "c1", "to": "c1", "relation": "self", "register": "polite" }
            ],
            "glossary": [
                { "term": "Tokyo", "target": "တိုကျို", "keep": false, "note": "city" }
            ],
            "scenes": [
                { "cue_range": [1, 5], "summary": "opening", "speakers": ["c1"], "mood": "calm" }
            ],
            "style_rules": ["keep it short"],
            "unresolved_ambiguities": ["who is narrating"]
        })
    }

    fn validate_doc(doc: &serde_json::Value) -> Result<(), ValidationError> {
        validate_full(&doc.to_string())
    }

    #[test]
    fn validate_full_accepts_doc_within_caps() {
        assert!(validate_doc(&base_doc()).is_ok());
    }

    #[test]
    fn validate_full_rejects_too_many_characters() {
        let mut doc = base_doc();
        let one = doc["characters"][0].clone();
        doc["characters"] = serde_json::Value::Array(vec![one; 41]);
        assert!(matches!(
            validate_doc(&doc),
            Err(ValidationError::CapExceeded {
                field: "characters",
                ..
            })
        ));
    }

    #[test]
    fn validate_full_rejects_over_length_premise() {
        let mut doc = base_doc();
        doc["premise"] = json!("x".repeat(501));
        assert!(matches!(
            validate_doc(&doc),
            Err(ValidationError::StringTooLong {
                field: "premise",
                ..
            })
        ));
        // Exactly at the cap is fine.
        doc["premise"] = json!("x".repeat(500));
        assert!(validate_doc(&doc).is_ok());
    }

    #[test]
    fn validate_full_rejects_over_length_character_name() {
        let mut doc = base_doc();
        doc["characters"][0]["name"] = json!("x".repeat(121));
        assert!(matches!(
            validate_doc(&doc),
            Err(ValidationError::StringTooLong {
                field: "characters[].name",
                ..
            })
        ));
    }

    #[test]
    fn validate_full_rejects_over_length_scene_summary() {
        let mut doc = base_doc();
        doc["scenes"][0]["summary"] = json!("x".repeat(301));
        assert!(matches!(
            validate_doc(&doc),
            Err(ValidationError::StringTooLong {
                field: "scenes[].summary",
                ..
            })
        ));
    }

    #[test]
    fn validate_full_rejects_too_many_aliases() {
        let mut doc = base_doc();
        doc["characters"][0]["aliases"] = serde_json::Value::Array(vec![json!("a"); 9]);
        assert!(matches!(
            validate_doc(&doc),
            Err(ValidationError::CapExceeded {
                field: "characters[].aliases",
                ..
            })
        ));
    }

    #[test]
    fn validate_full_rejects_too_many_style_rules() {
        let mut doc = base_doc();
        doc["style_rules"] = serde_json::Value::Array(vec![json!("r"); 13]);
        assert!(matches!(
            validate_doc(&doc),
            Err(ValidationError::CapExceeded {
                field: "style_rules",
                ..
            })
        ));
    }

    #[test]
    fn validate_full_accepts_array_exactly_at_cap() {
        // Arrays exactly at their cap pass (guards a `>`→`>=` off-by-one that the
        // over-limit-only tests cannot catch).
        let mut doc = base_doc();
        let one = doc["characters"][0].clone();
        doc["characters"] = serde_json::Value::Array(vec![one; 40]);
        doc["characters"][0]["aliases"] = serde_json::Value::Array(vec![json!("a"); 8]);
        doc["style_rules"] = serde_json::Value::Array(vec![json!("r"); 12]);
        assert!(validate_doc(&doc).is_ok());
    }

    #[test]
    fn validate_full_rejects_too_many_unresolved_ambiguities() {
        let mut doc = base_doc();
        doc["unresolved_ambiguities"] = serde_json::Value::Array(vec![json!("a"); 13]);
        assert!(matches!(
            validate_doc(&doc),
            Err(ValidationError::CapExceeded {
                field: "unresolved_ambiguities",
                ..
            })
        ));
        // Exactly at the cap is fine.
        doc["unresolved_ambiguities"] = serde_json::Value::Array(vec![json!("a"); 12]);
        assert!(validate_doc(&doc).is_ok());
    }

    #[test]
    fn validate_full_rejects_over_length_unresolved_ambiguity() {
        let mut doc = base_doc();
        doc["unresolved_ambiguities"] = json!(["x".repeat(221)]);
        assert!(matches!(
            validate_doc(&doc),
            Err(ValidationError::StringTooLong {
                field: "unresolved_ambiguities[]",
                ..
            })
        ));
        // Exactly at the cap is fine.
        doc["unresolved_ambiguities"] = json!(["x".repeat(220)]);
        assert!(validate_doc(&doc).is_ok());
    }

    #[test]
    fn validate_full_rejects_over_length_address_matrix_relation() {
        let mut doc = base_doc();
        doc["address_matrix"][0]["relation"] = json!("x".repeat(161));
        assert!(matches!(
            validate_doc(&doc),
            Err(ValidationError::StringTooLong {
                field: "address_matrix[].relation",
                ..
            })
        ));
        // Exactly at the cap is fine.
        doc["address_matrix"][0]["relation"] = json!("x".repeat(160));
        assert!(validate_doc(&doc).is_ok());
    }

    #[test]
    fn validate_full_rejects_over_length_glossary_note() {
        let mut doc = base_doc();
        doc["glossary"][0]["note"] = json!("x".repeat(181));
        assert!(matches!(
            validate_doc(&doc),
            Err(ValidationError::StringTooLong {
                field: "glossary[].note",
                ..
            })
        ));
        // Exactly at the cap is fine.
        doc["glossary"][0]["note"] = json!("x".repeat(180));
        assert!(validate_doc(&doc).is_ok());
    }
}
