//! The exit codes of a step.

use std::process::ExitCode;

/// Success.
pub const EXIT_OK: i32 = 0;
/// Failure.
pub const EXIT_FAILURE: i32 = 1;
/// Deferred: the host is busy or not ready, nothing irreversible happened, try later.
pub const EXIT_DEFERRED: i32 = 75;
/// Halt: no version can repair this; acting further would make it worse.
pub const EXIT_HALT: i32 = 76;

/// How a step ended, as the updater reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Outcome {
    /// Exit `0`.
    Success,
    /// Exit `1`, any other code, a timeout or a signal.
    Failure,
    /// Exit `75`.
    Deferred,
    /// Exit `76`.
    Halt,
}

impl Outcome {
    /// The code a hook exits with for this outcome.
    pub const fn code(self) -> i32 {
        match self {
            Outcome::Success => EXIT_OK,
            Outcome::Failure => EXIT_FAILURE,
            Outcome::Deferred => EXIT_DEFERRED,
            Outcome::Halt => EXIT_HALT,
        }
    }

    /// The outcome of one of the four defined codes, `None` for any other.
    pub const fn from_code(code: i32) -> Option<Outcome> {
        match code {
            EXIT_OK => Some(Outcome::Success),
            EXIT_FAILURE => Some(Outcome::Failure),
            EXIT_DEFERRED => Some(Outcome::Deferred),
            EXIT_HALT => Some(Outcome::Halt),
            _ => None,
        }
    }

    /// How the updater reads a step's end: the four codes by their meaning; any other code, and
    /// a step with no code (killed by its timeout or by a signal), is a failure.
    pub fn from_exit(code: Option<i32>) -> Outcome {
        code.and_then(Outcome::from_code)
            .unwrap_or(Outcome::Failure)
    }
}

impl From<Outcome> for ExitCode {
    /// For a hook's `main`: `Outcome::Deferred.into()` exits `75`.
    fn from(outcome: Outcome) -> ExitCode {
        // The codes are 0, 1, 75 and 76: all fit a byte.
        ExitCode::from(u8::try_from(outcome.code()).unwrap_or(1))
    }
}
