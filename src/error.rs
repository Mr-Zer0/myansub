//! Error types and exit-code mapping.
//!
//! One `thiserror` enum per module (see `docs/ARCHITECTURE.md` §8). For M0 only
//! [`ConfigError`] is defined; later milestones add the remaining module enums.
//! This module also owns the exit-code surface consumed by `main.rs`
//! (see `docs/CLI.md` §7).

pub use crate::config::ConfigError;
pub use crate::formats::codec::FormatError;
pub use crate::formats::encoding::EncodingError;
pub use crate::llm::client::LlmError;
pub use crate::store::StoreError;

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

impl ExitCode for EncodingError {
    fn exit_code(&self) -> i32 {
        exit::ERROR
    }
}

impl ExitCode for FormatError {
    fn exit_code(&self) -> i32 {
        exit::ERROR
    }
}

impl ExitCode for StoreError {
    fn exit_code(&self) -> i32 {
        exit::ERROR
    }
}

impl ExitCode for LlmError {
    /// A cancelled operation maps to the interrupted code (130); every other
    /// `LlmError` maps to the generic error code. The pipeline translates failed
    /// *units* to exit 4 later (see the task brief and `docs/CLI.md` §7).
    fn exit_code(&self) -> i32 {
        match self {
            LlmError::Cancelled => exit::INTERRUPTED,
            _ => exit::ERROR,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // AC: CLI.md §7 — a cancelled LLM op exits 130; other LlmErrors exit 1.
    #[test]
    fn cancelled_llm_error_maps_to_interrupted_exit_code() {
        assert_eq!(LlmError::Cancelled.exit_code(), 130);
        assert_eq!(exit::INTERRUPTED, 130);
    }

    #[test]
    fn other_llm_errors_map_to_generic_error_code() {
        assert_eq!(LlmError::Timeout.exit_code(), 1);
        assert_eq!(
            LlmError::Fatal {
                status: 400,
                message: "bad".to_string(),
            }
            .exit_code(),
            exit::ERROR
        );
    }
}
