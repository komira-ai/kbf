//! Shared ownership (`--executor-ownership shared`, `docs/design/n-frontends.md` section 4):
//! the `exec/` rows a process writes so that any number of frontends of one group serve beside
//! each other over one state store.
//!
//! What a process of the group does that one of [`Ownership::Single`] does not:
//!
//! - It renews its liveness row, `exec/fe/<instance>`, at most every heartbeat interval, with a
//!   check of every worker row it holds ([`Share::beat`]). The row lives
//!   [`Config::live`] on the store's clock. The workers' fence counts the store's silence too
//!   ([`Share::renewed`]): a process that has not renewed for the fence time stops acting
//!   before its row can expire.
//! - Its workers are its own (`js:<group>.<instance>-<n>`, and `js-mac:<group>.<instance>-<n>`
//!   for the Macs, [`Config::worker_of`]), each with a row written before the farm hears of
//!   it ([`Share::open`]).
//! - A job's row is written before its submit ([`HostWorker::write_row`]) and deleted once the
//!   farm acknowledged its result, or once nothing is owed for the lease any more
//!   ([`HostWorker::flush_rows`]).
//! - It cancels a job only on the farm's `Cancel` or a withdrawal, the farm ending a worker's
//!   stream included (and then deletes the row only of a job whose cancel was answered): not
//!   at start (no rival check, no cancel of an earlier process's jobs), not when a worker
//!   fences, not when the process ends. At its end each worker row is deleted if it holds no
//!   job row, else handed back with no owner, and the liveness row is deleted
//!   ([`HostWorker::hand_back`], [`Share::leave`]).
//!
//! Every call here is one transaction of [`ExecRows`]. A row call that fails is tried again
//! on a later pass; none of them waits.
//!
//! [`Ownership::Single`]: super::config::Ownership::Single
//! [`Config::live`]: super::Config::live
//! [`Config::worker_of`]: super::Config::worker_of

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use kbf_proto::reapi::Digest;
use kbf_state::Store;
use kbf_types::LeaseId;
use tokio::time::Instant;

use crate::executor::host::HostWorker;
use crate::executor::mac::{MAC_ARCH, WorkerOs};
use crate::executor::rows::{
    ExecRows, Held, JobRow, Live, Liveness, RowsError, Shape, WorkerEntry, WorkerRow,
};
use crate::executor::{Config, Ctx};

/// The release a liveness row names.
const RELEASE: &str = env!("CARGO_PKG_VERSION");

/// A worker row this process holds, and what it last wrote there.
#[derive(Clone, Debug)]
pub(super) struct Own {
    pub(super) held: Held,
    pub(super) row: WorkerRow,
}

/// What came of writing a job's row before its submit.
pub(super) enum Rowed {
    /// The row is there: the job may be submitted.
    Written,
    /// The store could not be asked: try again on a later pass.
    Retry,
    /// Another process holds the worker now: the task is dropped, and nothing is submitted.
    Lost,
}

/// The process's side of the rows: its liveness and the handle on the store.
pub(crate) struct Share {
    rows: ExecRows,
    instance: String,
    group: String,
    started_ms: u64,
    /// The newest renewal the store took, if any.
    live: Option<Live>,
    /// When the newest renewal the store took was sent, or when the process started.
    renewed: Instant,
    next_beat: Instant,
}

/// Milliseconds since the Unix epoch, now.
fn epoch_ms() -> u64 {
    let since = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
}

impl Share {
    pub(super) fn new(cfg: &Config, store: Arc<dyn Store>, now: Instant) -> Self {
        Self {
            rows: ExecRows::new(store),
            instance: cfg.instance.clone(),
            group: cfg.frontend.clone(),
            started_ms: epoch_ms(),
            live: None,
            renewed: now,
            next_beat: now,
        }
    }

    /// When the newest renewal the store took was sent: a worker's fence counts from it as
    /// it counts from the farm's acknowledgement.
    pub(super) fn renewed(&self) -> Instant {
        self.renewed
    }

    /// Renews the liveness row when a renewal is due, with a check of each worker row of
    /// `held`. A renewal the store refuses or cannot take is logged and changes nothing: the
    /// fence counts from the last one it took.
    pub(super) fn beat(&mut self, cfg: &Config, held: &[Held], draining: bool, now: Instant) {
        if now < self.next_beat {
            return;
        }
        self.next_beat = now + cfg.heartbeat;
        let row = Liveness {
            group: self.group.clone(),
            started_ms: self.started_ms,
            release: RELEASE.to_owned(),
            draining,
        };
        match self.rows.beat(&self.instance, &row, cfg.live, held) {
            Ok(live) => {
                self.live = Some(live);
                self.renewed = now;
            }
            Err(error) => {
                tracing::warn!(instance = %self.instance, %error, "the liveness row was not renewed");
            }
        }
    }

    /// Opens the virtual workers this process does not have yet, in the order the farm sees
    /// them without shared ownership (the Linux ones, then the Macs'): each one's row is
    /// written before the farm hears of it. Nothing is opened before the liveness row is
    /// written, and a row the store does not take leaves the rest for a later pass.
    pub(super) fn open(&self, ctx: &Ctx<'_>, workers: &mut Vec<HostWorker>, now: Instant) {
        if self.live.is_none() {
            return;
        }
        let linux = ctx.cfg.hosts;
        let macs = ctx.cfg.mac.as_ref().map_or(0, |mac| mac.hosts);
        while workers.len() < linux + macs {
            let (os, n) = match workers.len() {
                k if k < linux => (WorkerOs::Linux, k),
                k => (WorkerOs::Mac, k - linux),
            };
            let name = ctx.cfg.worker_of(os, n);
            let row = WorkerRow {
                owner_instance: self.instance.clone(),
                farm_instance: HostWorker::farm_instance(ctx, &name, 0),
                shape: shape_of(ctx.cfg, os),
            };
            match self.rows.create_worker(&name, &row) {
                Ok(held) => {
                    let mut worker = HostWorker::start_of(ctx, os, n, now);
                    worker.own = Some(Own { held, row });
                    workers.push(worker);
                }
                Err(error) => {
                    tracing::warn!(worker = %name, %error, "the row of a worker was not written: it is not registered yet");
                    return;
                }
            }
        }
    }

    /// Deletes the liveness row: the process ends.
    pub(super) fn leave(&self) {
        if let Err(error) = self.rows.leave(&self.instance) {
            tracing::warn!(instance = %self.instance, %error, "the liveness row was not deleted: it expires on its own");
        }
    }

    /// Writes again the row of a worker this process holds, naming `farm_instance`: the worker
    /// registered with the farm again. `None` when the store did not take it; the row then keeps
    /// what it had, and the holder still holds it.
    fn rename(&self, own: &Own, farm_instance: &str) -> Option<Own> {
        let me = self.live.as_ref()?;
        let entry = WorkerEntry {
            worker: own.held.worker.clone(),
            row: own.row.clone(),
            version: own.held.version,
        };
        match self.rows.claim(me, &entry, farm_instance) {
            Ok(held) => Some(Own {
                held,
                row: WorkerRow {
                    farm_instance: farm_instance.to_owned(),
                    ..own.row.clone()
                },
            }),
            Err(error) => {
                tracing::warn!(worker = %own.held.worker, %error, "the row of a worker that registered again was not rewritten");
                None
            }
        }
    }
}

/// The shape a worker of `cfg` for `os` is registered with: kept in its row. A Mac has the
/// shape of a Linux worker and the Macs' one architecture.
fn shape_of(cfg: &Config, os: WorkerOs) -> Shape {
    let arch = match os {
        WorkerOs::Linux => cfg.arch.clone(),
        WorkerOs::Mac => MAC_ARCH.to_owned(),
    };
    Shape {
        cpu_millis: cfg.cpu_millis,
        memory_bytes: cfg.memory_bytes,
        slots: cfg.slots,
        arch,
    }
}

/// The text of a digest in a job row.
fn digest_text(digest: &Digest) -> String {
    format!("{}/{}", digest.hash, digest.size_bytes)
}

impl HostWorker {
    /// The worker rows this process holds, for the check of a renewal.
    pub(super) fn held(&self) -> Option<Held> {
        self.own.as_ref().map(|own| own.held.clone())
    }

    /// Writes the row of `lease`'s job `job`, keyed `key`, before its submit. Without shared
    /// ownership there is no row, and the job may be submitted.
    pub(super) fn write_row(
        &mut self,
        ctx: &Ctx<'_>,
        lease: LeaseId,
        action: &Digest,
        job: &str,
        key: &str,
    ) -> Rowed {
        let (Some(share), Some(own)) = (ctx.share, &self.own) else {
            return Rowed::Written;
        };
        if self
            .tasks
            .get(&lease)
            .is_some_and(|task| task.row.is_some())
        {
            return Rowed::Written;
        }
        let row = JobRow {
            lease_term: lease.term,
            lease_seq: lease.seq,
            action: digest_text(action),
            pack_key: key.to_owned(),
            submitted_ms: epoch_ms(),
        };
        match share.rows.insert_job(&own.held, job, &row) {
            Ok(_) => {
                self.tasks
                    .entry(lease)
                    .and_modify(|task| task.row = Some(job.to_owned()));
                Rowed::Written
            }
            Err(RowsError::Conflict { .. }) => {
                tracing::warn!(worker = %self.id, %lease, "another process holds the worker: the task is dropped and nothing is submitted");
                Rowed::Lost
            }
            Err(error) => {
                tracing::warn!(worker = %self.id, %lease, %error, "the row of a job was not written: its submit waits");
                Rowed::Retry
            }
        }
    }

    /// Deletes the rows of the jobs nothing is owed for any more. A delete the store cannot
    /// take is tried again on the next pass; one refused because another process holds the
    /// worker now is that process's to settle.
    pub(super) fn flush_rows(&mut self, ctx: &Ctx<'_>) {
        let (Some(share), Some(own)) = (ctx.share, &self.own) else {
            self.unrow.clear();
            return;
        };
        let jobs = std::mem::take(&mut self.unrow);
        for job in jobs {
            match share.rows.delete_job(&own.held, &job) {
                Ok(()) => {}
                Err(RowsError::Conflict { .. }) => {
                    tracing::warn!(worker = %self.id, %job, "another process holds the worker: the row of the job is its own");
                }
                Err(error) => {
                    tracing::warn!(worker = %self.id, %job, %error, "the row of a job was not deleted: tried again");
                    self.unrow.push(job);
                }
            }
        }
    }

    /// The worker registered with the farm again under `farm_instance`: its row says so.
    pub(super) fn renamed(&mut self, ctx: &Ctx<'_>, farm_instance: &str) {
        let (Some(share), Some(own)) = (ctx.share, &self.own) else {
            return;
        };
        if let Some(own) = share.rename(own, farm_instance) {
            self.own = Some(own);
        }
    }

    /// The process ends: the farm is told the worker takes no more work, its tasks are dropped
    /// without a cancel, and its row is deleted if no job row is left under it, else handed
    /// back with no owner, for another process of the group to take up.
    pub(super) fn hand_back(mut self, ctx: &Ctx<'_>) {
        tracing::info!(worker = %self.id, runs = self.tasks.len(), "the process ends: the worker is handed back, and its jobs go on");
        self.begin_drain(ctx);
        self.forget();
        self.flush_rows(ctx);
        let (Some(share), Some(own)) = (ctx.share, self.own.take()) else {
            return;
        };
        let left = share.rows.jobs(&own.held.worker);
        let done = match &left {
            Ok(left) if left.is_empty() => share.rows.retire(&own.held),
            Ok(_) => share.rows.release(&own.held, &own.row),
            Err(error) => Err(error.clone()),
        };
        if let Err(error) = done {
            tracing::warn!(worker = %self.id, %error, "the row of the worker was not deleted or handed back: it is taken up once the liveness row expires");
        }
    }
}

#[cfg(test)]
#[path = "share_tests.rs"]
mod tests;
