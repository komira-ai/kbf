//! Rule `restart` of [`super::modes`]: a node unit (`storage`, `daemon`,
//! `storage-and-daemon`) keeps retrying a failure that repeats quickly.
//!
//! systemd's defaults are `RestartSec=100ms` and a start limit of 5 starts in 10 s. A
//! failure that repeats at once (the cold store or a disk not ready yet) uses up the 5
//! starts in about half a second, the unit enters `start-limit-hit`, and it stays down
//! until someone runs `systemctl reset-failed`. So the unit must have:
//!
//! - `RestartSec=5` in `[Service]` (`5s` is the same), the last of them as systemd
//!   reads it;
//! - `StartLimitIntervalSec=0` in `[Unit]`, which turns the start limit off.
//!
//! Not for `frontend` and `all-in-one`: after a handover the old frontend drains and
//! exits 0 and must stay down, and the frontend keeps its own restart policy.
//!
//! What it cannot see: a drop-in that sets either key (rule `stop` reads drop-ins on the
//! node; this rule reads the unit file alone), and a value systemd ignores because it
//! cannot parse it.

use crate::unit::{UnitFile, WHITESPACE};

/// The `RestartSec=` values that are 5 seconds.
const FIVE_SECONDS: [&str; 2] = ["5", "5s"];

/// Rule `restart` on `unit`: one line per way a quickly repeating failure ends in
/// `start-limit-hit`.
pub(super) fn check(unit: &UnitFile) -> Vec<String> {
    let mut out = Vec::new();
    match unit
        .get_in("Service", "RestartSec")
        .map(|s| s.trim_matches(WHITESPACE))
    {
        Some(v) if FIVE_SECONDS.contains(&v) => {}
        Some(v) => out.push(format!(
            "has RestartSec={v}, not RestartSec=5: a failure that repeats quickly must not \
             use up the start limit"
        )),
        None => out.push(
            "gives no RestartSec=5: systemd restarts after 100 ms, and a failure that \
             repeats quickly ends in start-limit-hit with the unit down"
                .to_owned(),
        ),
    }
    match unit
        .get_in("Unit", "StartLimitIntervalSec")
        .map(|s| s.trim_matches(WHITESPACE))
    {
        Some("0") => {}
        Some(v) => out.push(format!(
            "has StartLimitIntervalSec={v}, not StartLimitIntervalSec=0: five starts in \
             that interval end in start-limit-hit with the unit down"
        )),
        None => out.push(
            "gives no StartLimitIntervalSec=0 in [Unit]: systemd's default of 5 starts in \
             10 s ends a repeating failure in start-limit-hit with the unit down"
                .to_owned(),
        ),
    }
    out
}

#[cfg(test)]
#[path = "restart_tests.rs"]
mod tests;
