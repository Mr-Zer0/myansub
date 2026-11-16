//! Error types and exit-code mapping.
//!
//! One `thiserror` enum per module (see `docs/ARCHITECTURE.md` §8). For M0 only
//! [`ConfigError`] is defined; later milestones add the remaining module enums.
//! This module also owns the exit-code surface consumed by `main.rs`
//! (see `docs/CLI.md` §7).

pub use crate::config::ConfigError;

/// Process exit codes, per `docs/CLI.md` §7.
///
/// `Usage` (2) is normally produced by clap itself on a parse failure; it is
/// listed here so the mapping is documented in one place.
pub mod exit {
    /// Success, including "nothing to do".
    pub const SUCCESS: i32 = 0;
    /// Generic error.
    pub const ERROR: i32 = 1;
    /// Usage error (clap, conflicting flags).
    pub const USAGE: i32 = 2;
    /// Precondition not met (gate not approved, source changed, ambiguous
    /// project, confirmation needed without a TTY).
    pub const PRECONDITION: i32 = 3;
    /// Completed with failed units.
    pub const FAILED_UNITS: i32 = 4;
    /// Interrupted (Ctrl-C).
    pub const INTERRUPTED: i32 = 130;
}

/// Maps an error to the process exit code it should produce.
///
/// Implemented by each module error enum so `main.rs` can translate a typed
/// error into the codes in `docs/CLI.md` §7 without matching on variants.
pub trait ExitCode {
    fn exit_code(&self) -> i32;
}

impl ExitCode for ConfigError {
    fn exit_code(&self) -> i32 {
        exit::ERROR
    }
}
