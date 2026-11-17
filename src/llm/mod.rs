//! The LLM client layer and its pure support modules (`docs/ARCHITECTURE.md` §5).
//!
//! M3-A provides the logical surface and the pure cores: the [`client`] trait and
//! types, [`canonical`] JSON (the cache-key basis), the [`retry`] policy, [`pricing`],
//! [`prompts`], and [`schemas`]. M3-B adds the [`gemini`] transport, the
//! [`retrying`] layer, the [`dry_run`] transport, and the scripted [`fake`]
//! transport. The remaining `Cached`/`Metered` layers arrive in later M3 slices.
//!
//! `llm` depends only on `domain`/`config` here and must never depend on
//! `pipeline` (`docs/ARCHITECTURE.md` §3).

pub mod cache;
pub mod canonical;
pub mod client;
pub mod dry_run;
pub mod gemini;
pub mod metered;
pub mod pricing;
pub mod prompts;
pub mod retry;
pub mod retrying;
pub mod schemas;

/// Scripted transport for tests (`docs/TESTING.md` §3). Compiled only for this
/// crate's unit tests or when the `testing` feature is enabled (ADR-027).
#[cfg(any(test, feature = "testing"))]
pub mod fake;

pub use cache::{CacheError, CacheStore, Cached, CachedEntry, cache_key};
pub use canonical::canonical_json;
pub use client::{
    CachePayload, CommitToken, ContentBlock, ErrorClass, GenerationConfig, LlmClient, LlmError,
    LlmRequest, LlmResponse, Role, Secret, ServiceTier, ThinkingSetting, Usage,
};
pub use dry_run::{DryRun, Estimator};
pub use gemini::Gemini;
pub use metered::{CallContext, CallOutcome, LlmCallRecord, Metered, SinkError, UsageSink};
pub use pricing::{Cost, ModelPrice, PriceTable, cost, storage_cost};
pub use prompts::{PROMPT_VERSION, TranslationBlock, TranslationLine};
pub use retry::{Decision, RetryConfig, Xorshift, decide};
pub use retrying::Retrying;

/// The full client layer stack `Metered<Cached<Retrying<Transport>>>`
/// (`docs/ARCHITECTURE.md` §5). A type alias so the pipeline (M5/M6) can name the
/// assembled stack without spelling out the nesting.
pub type Stack<Transport, Store, Sink> = Metered<Cached<Retrying<Transport>, Store>, Sink>;

/// Assembles the full layer stack over `transport` (`docs/ARCHITECTURE.md` §5):
/// `Metered<Cached<Retrying<Transport>>>`.
pub fn assemble_stack<Transport, Store, Sink>(
    transport: Transport,
    cfg: RetryConfig,
    token: tokio_util::sync::CancellationToken,
    seed: u64,
    store: Store,
    sink: Sink,
    context: CallContext,
) -> Stack<Transport, Store, Sink> {
    let retrying = Retrying::new(transport, cfg, token, seed);
    let cached = Cached::new(retrying, store);
    Metered::new(cached, sink, context)
}
