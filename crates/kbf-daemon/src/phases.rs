//! Where a lease's time went, logged in one INFO line when it ends (`lease phases:`)
//! and sent to an observer as [`crate::Event::LeaseEnded`]: how long the lease ran,
//! how long its driver spent fetching inputs, running the action and uploading
//! outputs, and the output files and directories its result names.
//!
//! The phase times are the driver's own stamps in the result's
//! `ExecutedActionMetadata` (input fetch, execution, output upload), so they are known
//! only for a lease that produced a result; the line says `-` otherwise. The upload
//! phase includes waiting for the transfer engine's shared request limit.
//!
//! What the lease moved is not on this line: the transfer engine counts it per lease
//! ([`kbf_transfer::TransferStats`]) and logs it in its own INFO line, `transfers:`,
//! with the same `lease=` field, when the lease's last handle on the engine goes (after
//! the lease ends, if a read it started is still finishing for another lease). Join the
//! two lines on `lease=`.

use std::fmt;
use std::time::{Duration, SystemTime};

use kbf_proto::reapi::ActionResult;
use kbf_types::LeaseId;
use prost_types::Timestamp;

/// Where one lease's time went ([`crate::Event::LeaseEnded`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LeasePhases {
    /// The lease.
    pub lease: LeaseId,
    /// The driver it ran on.
    pub driver: &'static str,
    /// Whether the run produced a result (an action that exits non-zero does).
    pub ok: bool,
    /// From the lease's start here to its run's end, in milliseconds.
    pub lease_ms: u64,
    /// The driver's input fetch, if the result says.
    pub inputs_ms: Option<u64>,
    /// The action's execution, if the result says.
    pub run_ms: Option<u64>,
    /// The driver's output upload, if the result says.
    pub outputs_ms: Option<u64>,
    /// Output files the result names (not the files inside an output directory).
    pub output_files: u64,
    /// Output directories the result names.
    pub output_dirs: u64,
}

impl LeasePhases {
    /// The phases of lease `lease`, which ran on `driver` for `elapsed` and ended with
    /// `result` (`None` for a run that produced none).
    #[must_use]
    pub fn new(
        lease: LeaseId,
        driver: &'static str,
        elapsed: Duration,
        result: Option<&ActionResult>,
    ) -> Self {
        let metadata = result.and_then(|r| r.execution_metadata.as_ref());
        let [inputs_ms, run_ms, outputs_ms] = metadata.map_or([None; 3], |m| {
            [
                ms(
                    &m.input_fetch_start_timestamp,
                    &m.input_fetch_completed_timestamp,
                ),
                ms(
                    &m.execution_start_timestamp,
                    &m.execution_completed_timestamp,
                ),
                ms(
                    &m.output_upload_start_timestamp,
                    &m.output_upload_completed_timestamp,
                ),
            ]
        });
        Self {
            lease,
            driver,
            ok: result.is_some(),
            lease_ms: millis(elapsed),
            inputs_ms,
            run_ms,
            outputs_ms,
            output_files: result.map_or(0, |r| r.output_files.len() as u64),
            output_dirs: result.map_or(0, |r| r.output_directories.len() as u64),
        }
    }
}

/// The milliseconds from `start` to `end`, if both are stamped and in order.
fn ms(start: &Option<Timestamp>, end: &Option<Timestamp>) -> Option<u64> {
    let time = |t: &Option<Timestamp>| SystemTime::try_from((*t)?).ok();
    let elapsed = time(end)?.duration_since(time(start)?).ok()?;
    Some(millis(elapsed))
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// The `key=value` fields of the `lease phases:` line, `-` for a phase not known.
impl fmt::Display for LeasePhases {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let phase = |p: Option<u64>| p.map_or_else(|| "-".to_owned(), |ms| ms.to_string());
        write!(
            f,
            "lease={} driver={} ok={} lease_ms={} inputs_ms={} run_ms={} outputs_ms={} \
             output_files={} output_dirs={}",
            self.lease,
            self.driver,
            self.ok,
            self.lease_ms,
            phase(self.inputs_ms),
            phase(self.run_ms),
            phase(self.outputs_ms),
            self.output_files,
            self.output_dirs,
        )
    }
}

#[cfg(test)]
mod tests;
