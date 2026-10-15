//! The memory backstop: while the node's memory stall time is high and it holds leases,
//! the daemon says [`kbf_types::NodePhase::MemoryPressure`], and the server offers it no
//! new lease (`docs/design/worker-protocol.md#node-phases`).
//!
//! The signal is the kernel's pressure stall information for memory
//! (`/proc/pressure/memory`): the `some` line's `avg10`, the percentage of the last
//! 10 seconds in which at least one task waited for memory. [`watch`] samples it.
//! [`paused`] decides.
//!
//! The backstop never deadlocks a node. It pauses only while a lease runs, and every
//! lease ends (its action's timeout, the fence, or a kill). With no lease the node
//! serves whatever the pressure, so pressure the daemon's own leases do not cause
//! still lets one lease at a time through, and the node is never left idle for good.

use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::MissedTickBehavior;

/// Where the kernel reports memory pressure stall information.
pub const MEMORY_PSI: &str = "/proc/pressure/memory";

/// How often [`watch`] samples the file.
pub const SAMPLE_EVERY: Duration = Duration::from_secs(1);

/// The `avg10` of the `some` line of a pressure file (`some avg10=1.23 avg60=0.40
/// avg300=0.10 total=123456`), as a percentage. `None` if the text has no such line or
/// the value is not a number.
#[must_use]
pub fn some_avg10(text: &str) -> Option<f64> {
    let line = text.lines().find(|line| line.starts_with("some "))?;
    line.split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))?
        .parse()
        .ok()
        .filter(|avg: &f64| avg.is_finite())
}

/// Whether the node is under pressure it should stop taking leases for: `avg10` is
/// above `above` percent and `leases` run. No reading (`None`: the file is absent or
/// unreadable, as on a kernel without pressure stall information) never pauses.
#[must_use]
pub fn paused(avg10: Option<f64>, above: f64, leases: usize) -> bool {
    leases > 0 && avg10.is_some_and(|avg| avg > above)
}

/// Samples the `some avg10` of the pressure file at `path` every `every` for as long
/// as the returned receiver (or a clone) lives; `None` while the file cannot be read.
/// Must run inside a tokio runtime.
#[must_use]
pub fn watch(path: PathBuf, every: Duration) -> watch::Receiver<Option<f64>> {
    let (tx, rx) = watch::channel(sample(&path));
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(every);
        tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
        loop {
            tokio::select! {
                () = tx.closed() => return,
                _ = tick.tick() => {
                    let now = sample(&path);
                    tx.send_if_modified(|seen| std::mem::replace(seen, now) != now);
                }
            }
        }
    });
    rx
}

/// The `some avg10` in the file at `path`, if it can be read.
fn sample(path: &Path) -> Option<f64> {
    some_avg10(&std::fs::read_to_string(path).ok()?)
}
