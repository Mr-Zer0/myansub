//! The LLM client layer and its pure support modules (`docs/ARCHITECTURE.md` §5).
//!
//! M3-A provides the logical surface and the pure cores: the [`client`] trait and
//! types, [`canonical`] JSON (the cache-key basis), the [`retry`] policy, [`pricing`],
//! [`prompts`], and [`schemas`]. The transport (`gemini.rs`), the layer stack
//! (`Retrying`, `Cached`, `Metered`), the dry-run transport, and the fake
//! transport arrive in later M3 slices.
//!
//! `llm` depends only on `domain`/`config` here and must never depend on
//! `pipeline` (`docs/ARCHITECTURE.md` §3).

pub mod canonical;
pub mod client;
pub mod pricing;
pub mod prompts;
pub mod retry;
pub mod schemas;

pub use canonical::canonical_json;
pub use client::{
    CommitToken, ContentBlock, ErrorClass, GenerationConfig, LlmClient, LlmError, LlmRequest,
    LlmResponse, Role, ServiceTier, ThinkingSetting, Usage,
};
pub use pricing::{Cost, ModelPrice, PriceTable, cost, storage_cost};
pub use prompts::{PROMPT_VERSION, TranslationBlock, TranslationLine};
pub use retry::{Decision, RetryConfig, Xorshift, decide};
