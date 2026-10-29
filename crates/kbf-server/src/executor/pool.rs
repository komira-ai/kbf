//! The pool of host leases: the slots the executor fills with leases of the lease server,
//! one worker each.
//!
//! A slot is vacant until the lease server grants a lease (immediately, or `202 pending` until
//! the host is ready); the lease is then held by a worker until it is retired. The lease server
//! cannot be asked for a named host, so each lease is of whichever free host fits the shape,
//! and the worker is named by the host it was given.

use kbf_lease_client::{AcquireRequest, Acquired, Lease, LeaseId, LeaseState, Resources};
use tokio::time::Instant;

use crate::executor::host::{HostWorker, Retire};
use crate::executor::{Ctx, HostInfo, View};

/// The label every lease of this executor carries, valued with the frontend's id: how a
/// restarted frontend finds the leases it made.
pub(super) const FRONTEND_LABEL: &str = "kbf-frontend";

pub(super) enum Slot {
    /// No lease; asking for one is due at `retry_at`.
    Vacant { retry_at: Instant },
    /// A lease booked on a host that is not ready.
    Pending { lease: LeaseId },
    /// A lease held, and the worker that stands for its host.
    Active(Box<HostWorker>),
}

impl Slot {
    pub(super) fn is_pending(&self) -> bool {
        matches!(self, Self::Pending { .. })
    }

    pub(super) fn is_active(&self) -> bool {
        matches!(self, Self::Active(_))
    }

    pub(super) fn info(&self) -> Option<HostInfo> {
        match self {
            Self::Active(worker) => Some(worker.info()),
            _ => None,
        }
    }

    /// One pass over the slot.
    pub(super) async fn drive(&mut self, ctx: &Ctx<'_>, view: &View, now: Instant) {
        let slot = std::mem::replace(self, Self::Vacant { retry_at: now });
        *self = slot.advance(ctx, view, now).await;
    }

    async fn advance(self, ctx: &Ctx<'_>, view: &View, now: Instant) -> Self {
        match self {
            Self::Vacant { retry_at } if now < retry_at => Self::Vacant { retry_at },
            Self::Vacant { .. } => acquire(ctx, now).await,
            Self::Pending { lease } => match view.lease(lease) {
                Some(held) if held.state == LeaseState::Held => activate(ctx, held, now),
                Some(_) => Self::Pending { lease },
                None => Self::Vacant { retry_at: now },
            },
            Self::Active(mut worker) => match worker.step(ctx, view, now).await {
                None => Self::Active(worker),
                Some(why) => {
                    let retry_at = match why {
                        Retire::Drained => now,
                        _ => now + ctx.cfg.acquire_retry,
                    };
                    worker.retire(ctx, why).await;
                    Self::Vacant { retry_at }
                }
            },
        }
    }

    /// Releases the lease the slot holds or has booked.
    pub(super) async fn release(self, ctx: &Ctx<'_>) {
        match self {
            Self::Active(worker) => worker.retire(ctx, Retire::Fenced).await,
            Self::Pending { lease } => {
                if let Err(e) = ctx.deps.client.release(lease).await {
                    tracing::warn!(%lease, error = %e, "releasing a booked lease failed");
                }
            }
            Self::Vacant { .. } => {}
        }
    }
}

/// Asks the lease server for a host.
async fn acquire(ctx: &Ctx<'_>, now: Instant) -> Slot {
    let resources = Resources {
        cpu_millis: ctx.cfg.cpu_millis,
        memory_bytes: ctx.cfg.memory_bytes,
        gpus: 0,
        gpu_class: None,
    };
    let request = AcquireRequest::new(&ctx.cfg.class, resources, ctx.cfg.max_wall)
        .label(FRONTEND_LABEL, &ctx.cfg.frontend)
        .ttl(ctx.cfg.lease_ttl);
    match ctx.deps.client.acquire(&request).await {
        Ok(Acquired::Granted(granted)) => {
            let ends_at = now + ctx.cfg.max_wall;
            let host = HostWorker::start(ctx, granted.id, granted.host, ends_at, now);
            Slot::Active(Box::new(host))
        }
        Ok(Acquired::Pending(pending)) => Slot::Pending { lease: pending.id },
        Err(e) => {
            tracing::warn!(error = %e, "no host leased");
            Slot::Vacant {
                retry_at: now + ctx.cfg.acquire_retry,
            }
        }
    }
}

/// A worker for the held lease `held`, which ends when the lease server says.
fn activate(ctx: &Ctx<'_>, held: &Lease, now: Instant) -> Slot {
    let ends_at = ctx.clock.local(held.ends_at_ms, now);
    let host = held.host.clone().unwrap_or_default();
    Slot::Active(Box::new(HostWorker::start(ctx, held.id, host, ends_at, now)))
}

/// The leases this frontend made before it started: held ones get a worker (after their runs
/// are stopped: the lease server forgets none of them, and nothing of the farm maps to them),
/// booked ones wait, and any beyond the pool's size are released. `None` if the lease server
/// could not be asked.
pub(super) async fn adopt(ctx: &Ctx<'_>, now: Instant) -> Option<Vec<Slot>> {
    let own = ctx
        .deps
        .client
        .leases_labelled(FRONTEND_LABEL, &ctx.cfg.frontend)
        .await
        .ok()?;
    let mut slots = Vec::new();
    for lease in own {
        let room = slots.len() < ctx.cfg.hosts;
        match (lease.state, room) {
            (LeaseState::Held, true) => {
                stop_runs(ctx, lease.id).await;
                slots.push(activate(ctx, &lease, now));
            }
            (LeaseState::Pending, true) => slots.push(Slot::Pending { lease: lease.id }),
            (LeaseState::Held | LeaseState::Pending, false) => {
                if let Err(e) = ctx.deps.client.release(lease.id).await {
                    tracing::warn!(lease = %lease.id, error = %e, "releasing a spare lease failed");
                }
            }
            _ => {}
        }
    }
    slots.resize_with(ctx.cfg.hosts, || Slot::Vacant { retry_at: now });
    Some(slots)
}

/// Stops every run of `lease`: they belong to the frontend that made it, which is gone.
async fn stop_runs(ctx: &Ctx<'_>, lease: LeaseId) {
    let runs = ctx.deps.client.runs(lease).await.unwrap_or_default();
    for run in runs {
        if let Err(e) = ctx.deps.client.stop_run(lease, &run.run_id).await {
            tracing::warn!(%lease, error = %e, "stopping a run of an earlier frontend failed");
        }
    }
}
