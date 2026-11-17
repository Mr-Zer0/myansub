//! `myansub` library crate root.
//!
//! See `docs/ARCHITECTURE.md` §3 for the full module layout. M0 provides the
//! CLI surface, configuration, and error/exit-code plumbing; later milestones
//! add the domain, formats, store, llm, quality, pipeline, and review modules.

pub mod cli;
pub mod config;
pub mod domain;
pub mod error;
pub mod formats;
pub mod llm;
pub mod pipeline;
pub mod store;
