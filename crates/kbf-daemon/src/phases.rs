//! Where a lease's time went, logged in one INFO line when it ends (`lease phases:`)
//! and sent to an observer as [`crate::Event::LeaseEnded`]: how long the lease ran,
//! how long its driver spent fetching inputs, running the action and uploading
//! outputs, and what the daemon's CAS client moved for it.
//!
//! The phase times are the driver's own stamps in the result's
//! `ExecutedActionMetadata` (input fetch, execution, output upload), so they are known
//! only for a lease that produced a result; the line says `-` otherwise. What moved
//! is counted by [`crate::CasClient`] into the [`Transfers`] of the lease whose task
//! made the call: the lease manager runs each lease inside [`scoped`], and a call made
//! outside any lease (a test's own upload) counts nowhere.

use std::fmt;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use kbf_proto::reapi::ActionResult;
use kbf_types::LeaseId;
use prost_types::Timestamp;

tokio::task_local! {
    /// The transfers of the lease this task runs.
    static LEASE: Arc<Transfers>;
}

/// Runs `lease`, counting the CAS calls it makes into `transfers`.
pub fn scoped<F: Future>(transfers: Arc<Transfers>, lease: F) -> impl Future<Output = F::Output> {
    LEASE.scope(transfers, lease)
}

/// What one lease moved through the daemon's CAS client, counted as it goes.
#[derive(Debug, Default)]
pub struct Transfers {
    blobs_read: AtomicU64,
    bytes_read: AtomicU64,
    blobs_uploaded: AtomicU64,
    bytes_uploaded: AtomicU64,
    uploads_in_flight: AtomicU64,
    most_uploads_in_flight: AtomicU64,
}

impl Transfers {
    /// The counts so far.
    #[must_use]
    pub fn counts(&self) -> TransferCounts {
        let load = |n: &AtomicU64| n.load(Ordering::Relaxed);
        TransferCounts {
            blobs_read: load(&self.blobs_read),
            bytes_read: load(&self.bytes_read),
            blobs_uploaded: load(&self.blobs_uploaded),
            bytes_uploaded: load(&self.bytes_uploaded),
            most_uploads_in_flight: load(&self.most_uploads_in_flight),
        }
    }
}

/// Counts a blob of `bytes` read for the lease this task runs, if any.
pub fn read(bytes: u64) {
    let _ = LEASE.try_with(|t| {
        t.blobs_read.fetch_add(1, Ordering::Relaxed);
        t.bytes_read.fetch_add(bytes, Ordering::Relaxed);
    });
}

/// Starts counting one upload for the lease this task runs, if any: it is in flight
/// until the returned guard is dropped, and counted as uploaded only through
/// [`Upload::done`].
pub fn upload() -> Upload {
    let transfers = LEASE.try_with(Arc::clone).ok();
    if let Some(t) = &transfers {
        let now = t.uploads_in_flight.fetch_add(1, Ordering::Relaxed) + 1;
        t.most_uploads_in_flight.fetch_max(now, Ordering::Relaxed);
    }
    Upload(transfers)
}

/// One upload in flight ([`upload`]).
#[derive(Debug)]
pub struct Upload(Option<Arc<Transfers>>);

impl Upload {
    /// The blob, `bytes` long, is stored.
    pub fn done(self, bytes: u64) {
        if let Some(t) = &self.0 {
            t.blobs_uploaded.fetch_add(1, Ordering::Relaxed);
            t.bytes_uploaded.fetch_add(bytes, Ordering::Relaxed);
        }
    }
}

impl Drop for Upload {
    fn drop(&mut self) {
        if let Some(t) = &self.0 {
            t.uploads_in_flight.fetch_sub(1, Ordering::Relaxed);
        }
    }
}

/// [`Transfers`] at one moment.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TransferCounts {
    /// Blobs read (the Action, the Command, input Directories and files).
    pub blobs_read: u64,
    /// Their bytes.
    pub bytes_read: u64,
    /// Blobs stored (output files, Directories, Trees, stdout and stderr), each one
    /// ByteStream Write.
    pub blobs_uploaded: u64,
    /// Their bytes.
    pub bytes_uploaded: u64,
    /// The most uploads in flight at once.
    pub most_uploads_in_flight: u64,
}

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
    /// Output files the result names (files inside an output directory are counted
    /// in `transfers.blobs_uploaded`, not here).
    pub output_files: u64,
    /// Output directories the result names.
    pub output_dirs: u64,
    /// What the daemon's CAS client moved for the lease.
    pub transfers: TransferCounts,
}

impl LeasePhases {
    /// The phases of lease `lease`, which ran on `driver` for `elapsed` and ended with
    /// `result` (`None` for a run that produced none), having moved `transfers`.
    #[must_use]
    pub fn new(
        lease: LeaseId,
        driver: &'static str,
        elapsed: Duration,
        result: Option<&ActionResult>,
        transfers: TransferCounts,
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
            transfers,
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
        let t = &self.transfers;
        write!(
            f,
            "lease={} driver={} ok={} lease_ms={} inputs_ms={} run_ms={} outputs_ms={} \
             output_files={} output_dirs={} blobs_read={} bytes_read={} blobs_uploaded={} \
             bytes_uploaded={} most_uploads_in_flight={}",
            self.lease,
            self.driver,
            self.ok,
            self.lease_ms,
            phase(self.inputs_ms),
            phase(self.run_ms),
            phase(self.outputs_ms),
            self.output_files,
            self.output_dirs,
            t.blobs_read,
            t.bytes_read,
            t.blobs_uploaded,
            t.bytes_uploaded,
            t.most_uploads_in_flight,
        )
    }
}

#[cfg(test)]
mod tests;
