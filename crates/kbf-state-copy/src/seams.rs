//! The two places a test reaches into a run: this host's clock, and SQL run on the target
//! between the write and the check that reads it back.
//!
//! The tool passes the defaults, which read the real clock and run nothing. A test passes
//! its own to show that a clock that is off is refused and that a copy which changes
//! after it was written is caught, and the changes are rolled back or reported.

use std::time::{SystemTime, UNIX_EPOCH};

/// Where in a copy a test can act on the target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stage {
    /// Inside the copy's transaction, after the rows are written and before they are read
    /// back to be compared.
    InTransaction,
    /// On the new connection that reads the committed copy back, before it reads.
    AfterCommit,
}

/// What a run takes from outside itself.
#[derive(Clone, Copy)]
pub struct Seams {
    /// This host's clock, in microseconds on the Unix clock.
    pub local_now_us: fn() -> i64,
    /// SQL to run on the target at a stage, if any.
    pub tamper: fn(Stage) -> Option<&'static str>,
}

impl Default for Seams {
    fn default() -> Seams {
        Seams {
            local_now_us: system_now_us,
            tamper: leave_alone,
        }
    }
}

fn system_now_us() -> i64 {
    let since_epoch = SystemTime::now().duration_since(UNIX_EPOCH);
    let micros = since_epoch.unwrap_or_default().as_micros();
    i64::try_from(micros).unwrap_or(i64::MAX)
}

fn leave_alone(_stage: Stage) -> Option<&'static str> {
    None
}
