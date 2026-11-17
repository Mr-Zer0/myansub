//! Deterministic quality checks over translated cues (`docs/LLM.md` §8).
//!
//! This module is **pure**: every function here is a deterministic function over
//! domain types and plain limits. It must not depend on `llm` or `store`
//! (`docs/ARCHITECTURE.md` §3, ADR-010). The checks are shared by the
//! translation pipeline and the review stage.

pub mod checks;
