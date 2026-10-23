//! The places a test reaches into a run: this host's clock, SQL run on the target between
//! the write and the check that reads it back, and the question put to systemd about a unit.
//!
//! The tool passes the defaults, which read the real clock, run nothing and ask the real
//! `systemctl`. A test passes its own to show that a clock that is off is refused, that a
//! copy which changes after it was written is caught, and the changes are rolled back or
//! reported, and that a unit which is running is refused.

use std::io;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::unit;

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
    /// What systemd says of a unit: its active state, or `not-found` (or another load state)
    /// when it is not a unit that is loaded; see [`unit::state_of`].
    pub unit_state: fn(&str) -> io::Result<String>,
}

impl Default for Seams {
    fn default() -> Seams {
        Seams {
            local_now_us: system_now_us,
            tamper: leave_alone,
            unit_state: systemctl_state,
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

fn systemctl_state(name: &str) -> io::Result<String> {
    unit::query("systemctl", name)
}
