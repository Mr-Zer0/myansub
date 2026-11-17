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

pub mod canonical;
pub mod client;
pub mod dry_run;
pub mod gemini;
pub mod pricing;
pub mod prompts;
pub mod retry;
pub mod retrying;
pub mod schemas;

/// Scripted transport for tests (`docs/TESTING.md` §3). Compiled only for this
/// crate's unit tests or when the `testing` feature is enabled (ADR-027).
#[cfg(any(test, feature = "testing"))]
pub mod fake;

pub use canonical::canonical_json;
pub use client::{
    CommitToken, ContentBlock, ErrorClass, GenerationConfig, LlmClient, LlmError, LlmRequest,
    LlmResponse, Role, Secret, ServiceTier, ThinkingSetting, Usage,
};
pub use dry_run::{DryRun, Estimator};
pub use gemini::Gemini;
pub use pricing::{Cost, ModelPrice, PriceTable, cost, storage_cost};
pub use prompts::{PROMPT_VERSION, TranslationBlock, TranslationLine};
pub use retry::{Decision, RetryConfig, Xorshift, decide};
pub use retrying::Retrying;
