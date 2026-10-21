//! The exit codes of `kbf-installer` (docs/design/kbf-installer.md section 2). Scripts and
//! units branch on them, so they are one enum and a golden-tested table, not literals.

use std::process::ExitCode;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Exit {
    /// Done, or a dry run that found nothing wrong.
    Ok,
    /// Tried and failed.
    Failed,
    /// Usage, or a prerequisite is missing (not root, no such file).
    Usage,
    /// Verification or policy refused the order (`status.json` says which word).
    Refused,
    /// Another run holds the lock. Not a refusal: ask again.
    Busy,
    /// Only a person can go on (a swap is pending, a trust file expired).
    NeedsOperator,
}

impl Exit {
    pub const ALL: [Exit; 6] = [
        Exit::Ok,
        Exit::Failed,
        Exit::Usage,
        Exit::Refused,
        Exit::Busy,
        Exit::NeedsOperator,
    ];

    pub fn code(self) -> u8 {
        match self {
            Exit::Ok => 0,
            Exit::Failed => 1,
            Exit::Usage => 2,
            Exit::Refused => 3,
            Exit::Busy => 4,
            Exit::NeedsOperator => 5,
        }
    }
}

impl From<Exit> for ExitCode {
    fn from(exit: Exit) -> ExitCode {
        ExitCode::from(exit.code())
    }
}
