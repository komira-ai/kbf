//! The pool of host leases: the slots the executor fills with leases of the lease server,
//! one worker each.
//!
//! A slot is vacant until the lease server grants a lease (immediately, or `202 pending` until
//! the host is ready); the lease is then held by a worker until it is retired. The lease server
//! cannot be asked for a named host, so each lease is of whichever free host fits the shape,
//! and the worker is named by the host it was given.

use kbf_lease_client::{AcquireRequest, Acquired, Lease, LeaseId, LeaseState, Resources};
use tokio::time::Instant;

use crate::executor::host::{Drain, HostWorker, Retire};
use crate::executor::{Ctx, HostInfo, Listing, View, note, release_lease};

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

    /// The lease the slot holds or has booked.
    fn lease(&self) -> Option<LeaseId> {
        match self {
            Self::Active(worker) => Some(worker.lease),
            Self::Pending { lease } => Some(*lease),
            Self::Vacant { .. } => None,
        }
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
            // A lease that could not be looked up is not a lease that is gone: only a list
            // that was read and lacks it frees the slot to book another.
            Self::Pending { lease } => match view.lease(lease) {
                Listing::Found(held) if held.state == LeaseState::Held => activate(ctx, held, now),
                Listing::Absent => Self::Vacant { retry_at: now },
                Listing::Found(_) | Listing::Unread => Self::Pending { lease },
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
            Self::Active(mut worker) => {
                // The farm is told the worker takes no more work, so it does not place
                // actions on a receiver that is gone and requeue them a grace later.
                worker.begin_drain(ctx, Drain::Rotation);
                worker.retire(ctx, Retire::Fenced).await;
            }
            Self::Pending { lease } => release_lease(ctx, lease).await,
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
    let mut request = AcquireRequest::new(&ctx.cfg.class, resources, ctx.cfg.max_wall)
        .label(FRONTEND_LABEL, &ctx.cfg.frontend)
        .ttl(ctx.cfg.lease_ttl);
    // Exactly what the node report claims, and what the task tool and its static binary
    // need: a host of another system or architecture is not one this executor can use.
    request.capabilities = ctx.cfg.host_capabilities();
    match ctx.api().acquire(&request).await {
        Ok(Acquired::Granted(granted)) => {
            let ends = (
                ctx.clock.local(granted.expires_at_ms, now),
                now + ctx.cfg.max_wall,
            );
            let host = HostWorker::start(ctx, granted.id, granted.host, ends, now);
            Slot::Active(Box::new(host))
        }
        Ok(Acquired::Pending(pending)) => Slot::Pending { lease: pending.id },
        Err(e) => {
            tracing::warn!(error = %e, "no host leased");
            // The server may have booked a host whose answer was lost: nothing holds it.
            if e.may_have_applied() {
                ctx.strays.suspect();
            }
            Slot::Vacant {
                retry_at: now + ctx.cfg.acquire_retry,
            }
        }
    }
}

/// A worker for the held lease `held`, which ends when the lease server says.
fn activate(ctx: &Ctx<'_>, held: &Lease, now: Instant) -> Slot {
    let ends = (
        ctx.clock.local(held.expires_at_ms, now),
        ctx.clock.local(held.ends_at_ms, now),
    );
    let host = held.host.clone().unwrap_or_default();
    Slot::Active(Box::new(HostWorker::start(ctx, held.id, host, ends, now)))
}

/// The leases this frontend made before it started: held ones get a worker (after their runs
/// are stopped: the lease server forgets none of them, and nothing of the farm maps to them),
/// booked ones wait, and any beyond the pool's size are released. `None` if the lease server
/// could not be asked.
pub(super) async fn adopt(ctx: &Ctx<'_>, now: Instant) -> Option<Vec<Slot>> {
    let own = ctx
        .api()
        .leases_labelled(FRONTEND_LABEL, &ctx.cfg.frontend)
        .await
        .ok()?;
    let mut slots = Vec::new();
    for lease in own {
        let room = slots.len() < ctx.cfg.hosts;
        match (lease.state, room) {
            (LeaseState::Held, true) => match stop_runs(ctx, lease.id).await {
                true => slots.push(activate(ctx, &lease, now)),
                // Runs that could not be stopped would hold the host's budget until their
                // wall time ends and every `Start` would be refused for room: the lease is
                // given up, which marks its runs to stop, and the slot asks for another.
                false => {
                    release_lease(ctx, lease.id).await;
                    slots.push(Slot::Vacant {
                        retry_at: now + ctx.cfg.acquire_retry,
                    });
                }
            },
            (LeaseState::Pending, true) => slots.push(Slot::Pending { lease: lease.id }),
            (LeaseState::Held | LeaseState::Pending, false) => {
                release_lease(ctx, lease.id).await;
            }
            _ => {}
        }
    }
    slots.resize_with(ctx.cfg.hosts, || Slot::Vacant { retry_at: now });
    Some(slots)
}

/// Stops every run of `lease`: they belong to the frontend that made it, which is gone.
/// False if the runs could not be listed, or a stop got no answer: some may still run.
async fn stop_runs(ctx: &Ctx<'_>, lease: LeaseId) -> bool {
    let listed = ctx.api().runs(lease).await;
    note("listing the runs of an earlier frontend", &listed);
    let Ok(runs) = listed else {
        return false;
    };
    let mut stopped = true;
    for run in runs {
        let answer = ctx.api().stop_run(lease, &run.run_id).await;
        note("stopping a run of an earlier frontend", &answer);
        stopped &= answer.as_ref().err().is_none_or(|e| e.api().is_some());
    }
    stopped
}

/// Releases every lease under this frontend's label that no slot holds: the lease server
/// books on a request whose answer may be lost, and a release can fail, and either leaves a
/// lease that nothing renews or releases and that keeps a host busy until its ttl ends. False
/// if the lease server could not be asked.
pub(super) async fn reconcile(ctx: &Ctx<'_>, slots: &[Slot]) -> bool {
    let listed = ctx
        .api()
        .leases_labelled(FRONTEND_LABEL, &ctx.cfg.frontend)
        .await;
    note("listing the leases to look for strays", &listed);
    let Ok(own) = listed else {
        return false;
    };
    let held: Vec<LeaseId> = slots.iter().filter_map(Slot::lease).collect();
    for lease in own {
        let live = matches!(lease.state, LeaseState::Held | LeaseState::Pending);
        if live && !held.contains(&lease.id) {
            tracing::warn!(lease = %lease.id, "releasing a lease no slot holds");
            release_lease(ctx, lease.id).await;
        }
    }
    true
}
