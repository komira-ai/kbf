//! How many actions a Linux virtual worker takes: as many as the job service's hosts can run,
//! never fewer than the flags declare.
//!
//! The declared shape (`--executor-hosts` workers of `--executor-host-slots` slots) is a
//! constant, and the farm places no more than it at once however many hosts the job service
//! has: with more hosts than it fills, the farm's queue is the wait, and the hosts idle. So every
//! [`SIZE_EVERY`] the executor reads the service's hosts (`ListHosts`) and counts the whole cores
//! of each one that takes ordinary jobs of its own: heard (live), with no mark, no cordon and no
//! taint, of `os` `linux` and the executor's architecture where the host names them. A job of an
//! action holds at least one core of its host, so a host's cores bound how many of them it runs
//! at once. The cores are spread over the Linux workers, each taking at least the declared slots
//! and at most [`MAX_SLOTS`], and each worker's cores and memory grow with its slots in the
//! declared proportion ([`Config::hello_sized`](super::Config::hello_sized)). A listing that
//! fails leaves the size as it was; one that finds no such host is the declared size.
//!
//! With shared ownership every process of the group sizes its own workers from the whole fleet,
//! so the group may place more than the hosts run at once: the job service queues the rest, as
//! it queues anything past its hosts.

use std::time::Duration;

use js_proto::v1::{Host, HostMark, HostState};
use tokio::time::Instant;

use crate::executor::Ctx;

/// How often the hosts are read.
pub(super) const SIZE_EVERY: Duration = Duration::from_secs(30);

/// The most slots a worker takes ([`Config::validate`](super::Config::validate)).
pub(super) const MAX_SLOTS: u64 = 256;

/// The whole cores of the `hosts` that take ordinary Linux jobs of `arch`.
pub(super) fn fleet_cores(hosts: &[Host], arch: &str) -> u64 {
    let names = |host: &Host, key: &str, value: &str| {
        host.capabilities
            .get(key)
            .is_none_or(|named| named == value)
    };
    hosts
        .iter()
        .filter(|host| {
            [
                host.state() == HostState::Live,
                host.mark() == HostMark::Unspecified,
                host.cordon_expires_at_ms.is_none(),
                host.taint_reason.is_empty(),
                names(host, "os", "linux"),
                names(host, "arch", arch),
            ]
            .into_iter()
            .all(|ok| ok)
        })
        .map(|host| {
            host.shape
                .as_ref()
                .map_or(0, |shape| shape.cpu_millis / 1_000)
        })
        .fold(0, u64::saturating_add)
}

/// The slots of each of `workers` workers for `cores` cores: their share, rounded up, at least
/// `declared` and at most [`MAX_SLOTS`].
pub(super) fn per_worker(cores: u64, workers: usize, declared: u64) -> u64 {
    let workers = u64::try_from(workers.max(1)).unwrap_or(u64::MAX);
    cores
        .div_ceil(workers)
        .clamp(declared, MAX_SLOTS.max(declared))
}

/// The size of the Linux workers, and when the hosts are read again.
pub(super) struct Sizing {
    /// The slots each Linux worker is to have.
    pub(super) slots: u64,
    /// When the hosts are read next; `None` before the first read.
    next: Option<Instant>,
    /// The last read failed: a failure is logged once, until a read succeeds again.
    failing: bool,
}

impl Sizing {
    /// The declared size, with the hosts not read yet.
    pub(super) fn new(declared: u64) -> Self {
        Self {
            slots: declared,
            next: None,
            failing: false,
        }
    }

    /// Reads the job service's hosts if it is time to, and sizes the workers from them.
    pub(super) async fn check(&mut self, ctx: &Ctx<'_>, now: Instant) {
        if self.next.is_some_and(|next| now < next) {
            return;
        }
        self.next = Some(now + SIZE_EVERY);
        match ctx.api().list_hosts().await {
            Ok(hosts) => {
                let cores = fleet_cores(&hosts, &ctx.cfg.arch);
                let slots = per_worker(cores, ctx.cfg.hosts, ctx.cfg.slots);
                if slots != self.slots {
                    tracing::info!(
                        cores,
                        workers = ctx.cfg.hosts,
                        from = self.slots,
                        to = slots,
                        "the job service's hosts size the virtual workers"
                    );
                }
                self.slots = slots;
                self.failing = false;
            }
            Err(error) => {
                if !std::mem::replace(&mut self.failing, true) {
                    tracing::warn!(%error, slots = self.slots, "the job service's hosts could not be read: the virtual workers keep their size");
                }
            }
        }
    }
}

#[cfg(test)]
#[path = "capacity_tests.rs"]
mod tests;
