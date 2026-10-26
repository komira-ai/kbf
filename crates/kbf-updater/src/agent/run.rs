//! The agent: a heartbeat to the coordinator at start and every interval, an update when the
//! reply says "update to X", and the heartbeat going on through the whole of it.
//!
//! Two threads, because an update blocks for minutes (a fetch, a drain, a restart) and a host
//! that goes quiet for 45 seconds counts as down (`docs/design/updates.md`, section 3):
//!
//! - the **heartbeat thread** reads the shared [`Status`](super::status::Status), posts it,
//!   and hands a reply of `update to X` to the other thread;
//! - the **update thread** (the caller's) runs the updates one at a time, and between them
//!   asks the host what it runs and whether it is healthy, so the status is fresh.
//!
//! The reply's version is the only thing taken from it. A target that failed is not tried
//! again before the retry time is out, so a bad version does not make a host drain itself
//! over and over; the failure is in the next heartbeat, and the coordinator keeps the host's
//! slot while it is not healthy.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::coordinator::Coordinator;
use super::flags::Timing;
use super::local::{HostState, Local};
use super::renew::Renewer;
use super::status::{Shared, Status, version_of_commit};
use super::update::{Ports, Update};
use crate::inventory::Host;
use crate::ports::{Clock, Drain, Health, HealthCheck};
use crate::version::Version;
use crate::wire::{self, Outcome, Reply};

/// How long the loops sleep between looks at whether they are stopped.
const SLICE: Duration = Duration::from_millis(50);

/// Asks a running agent to stop.
#[derive(Clone, Debug, Default)]
pub struct Stop(Arc<AtomicBool>);

impl Stop {
    pub fn stop(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    fn stopped(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

/// Sleeps for `total`, or until `stop`.
fn sleep(total: Duration, stop: &Stop) {
    let mut slept = Duration::ZERO;
    while slept < total && !stop.stopped() {
        let step = SLICE.min(total - slept);
        std::thread::sleep(step);
        slept += step;
    }
}

/// Everything an agent talks to.
pub struct World {
    pub coordinator: Arc<dyn Coordinator>,
    pub local: Arc<dyn Local>,
    pub drain: Arc<dyn Drain>,
    pub health: Arc<dyn HealthCheck>,
    pub clock: Arc<dyn Clock>,
    pub unix_now: Arc<dyn Fn() -> u64 + Send + Sync>,
    /// Keeps the certificate fresh, for a host the agent enrolled (`--state-dir`); none for
    /// files an operator placed.
    pub renewer: Option<Arc<dyn Renewer>>,
    pub log: Arc<dyn Fn(&str) + Send + Sync>,
}

/// The agent of one host.
pub struct Agent {
    host: Host,
    channel: String,
    timing: Timing,
    world: World,
    status: Shared,
    /// The target that failed last, and when.
    failed: Mutex<Option<(Version, Duration)>>,
    /// Why the host was last reported unhealthy, so a reason is logged once and not on every
    /// heartbeat.
    unhealthy: Mutex<Option<String>>,
}

impl Agent {
    pub fn new(host: Host, channel: String, timing: Timing, world: World, status: Status) -> Self {
        Self {
            host,
            channel,
            timing,
            world,
            status: Shared::new(status),
            failed: Mutex::new(None),
            unhealthy: Mutex::new(None),
        }
    }

    /// The status the heartbeats say.
    pub fn status(&self) -> Status {
        self.status.get()
    }

    fn log(&self, line: &str) {
        (self.world.log)(line);
    }

    /// Log why the host is not healthy when the reason is not the one logged last, and log once
    /// when it is healthy again (`None`).
    fn log_health(&self, reason: Option<String>) {
        let mut last = self.unhealthy.lock().unwrap_or_else(|e| e.into_inner());
        if *last == reason {
            return;
        }
        match &reason {
            Some(reason) => self.log(&format!("the host is not healthy: {reason}")),
            None => self.log("the host is healthy again"),
        }
        *last = reason;
    }

    /// Run until `stop`: finish an update the helper is in the middle of, heartbeat, and update
    /// when told to.
    pub fn run(&self, stop: &Stop) {
        self.refresh();
        // The updater was restarted in the middle of an A/B: the helper has a release on trial.
        // The first heartbeat says so, and the trial is finished before any other order.
        let resumed = self.trial_in_progress();
        if let Some(target) = &resumed {
            self.status.update(|s| s.updating = Some(target.clone()));
        }
        let (orders, inbox) = channel();
        std::thread::scope(|scope| {
            scope.spawn(|| self.heartbeats(stop, &orders));
            if let Some(renewer) = &self.world.renewer {
                scope.spawn(|| self.renewals(renewer.as_ref(), stop));
            }
            if let Some(target) = &resumed {
                self.update(target);
            }
            self.updates(stop, &inbox);
        });
    }

    /// Ask the host what it runs and whether it is healthy, and say so in the status.
    pub fn refresh(&self) {
        match self.world.local.query() {
            Ok(state) => self.note(&state),
            Err(e) => {
                self.log_health(Some(format!("the host helper does not answer: {e}")));
                self.status.update(|s| s.health = wire::Health::Unhealthy);
            }
        }
    }

    fn note(&self, state: &HostState) {
        let running = state
            .version
            .clone()
            .or_else(|| state.commit.as_deref().and_then(version_of_commit));
        let Some(running) = running else {
            self.log_health(Some(
                "the host helper reports neither a version nor a known commit".to_owned(),
            ));
            self.status.update(|s| s.health = wire::Health::Unhealthy);
            return;
        };
        let reason = match self.world.health.check(&self.host, &running) {
            Ok(Health::Healthy) => None,
            Ok(Health::Waiting(why)) => Some(format!("not yet healthy on {running}: {why}")),
            Ok(Health::Abandoned(why)) => {
                Some(format!("will not become healthy on {running}: {why}"))
            }
            Err(e) => Some(format!("the health check of {running} failed: {e}")),
        };
        let healthy = reason.is_none();
        self.log_health(reason);
        self.status.update(|s| {
            s.version = running;
            s.health = if healthy {
                wire::Health::Healthy
            } else {
                wire::Health::Unhealthy
            };
        });
    }

    /// The version of the release the helper has on trial, if it has one it can name.
    fn trial_in_progress(&self) -> Option<Version> {
        self.world.local.query().ok()?.pending?.version
    }

    fn heartbeats(&self, stop: &Stop, orders: &std::sync::mpsc::Sender<Version>) {
        while !stop.stopped() {
            let beat = self.status.get().heartbeat();
            match self.world.coordinator.heartbeat(&beat) {
                Ok(Reply::Update { version }) => {
                    let _ = orders.send(version);
                }
                Ok(Reply::Nothing) => {}
                Err(e) => self.log(&format!("heartbeat: {e}")),
            }
            sleep(self.timing.heartbeat, stop);
        }
    }

    /// The certificate's renewals, on a thread of their own: a renewal calls the frontend and
    /// may wait for it, and neither a heartbeat nor an update waits for that.
    fn renewals(&self, renewer: &dyn Renewer, stop: &Stop) {
        while !stop.stopped() {
            sleep(renewer.round(), stop);
        }
    }

    fn updates(&self, stop: &Stop, inbox: &Receiver<Version>) {
        let mut idle = Duration::ZERO;
        while !stop.stopped() {
            if let Ok(target) = inbox.recv_timeout(SLICE) {
                // The newest order wins; the ones queued behind it were the same.
                let target = inbox.try_iter().last().unwrap_or(target);
                self.order(&target);
                idle = Duration::ZERO;
                continue;
            }
            idle += SLICE;
            if idle >= self.timing.heartbeat {
                self.refresh();
                idle = Duration::ZERO;
            }
        }
    }

    /// An order for `target`: run it, unless the same target failed a moment ago.
    fn order(&self, target: &Version) {
        let now = self.world.clock.now();
        let backing_off = self
            .failed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|(failed, at)| {
                failed == target && now.saturating_sub(*at) < self.timing.retry
            });
        if backing_off {
            self.log(&format!(
                "update to {target}: failed a moment ago; not again yet"
            ));
            return;
        }
        self.update(target);
    }

    fn update(&self, target: &Version) {
        let update = Update {
            ports: Ports {
                local: &*self.world.local,
                drain: &*self.world.drain,
                health: &*self.world.health,
                clock: &*self.world.clock,
                unix_now: &*self.world.unix_now,
                log: &*self.world.log,
            },
            host: &self.host,
            channel: &self.channel,
            timing: self.timing,
            status: &self.status,
        };
        let done = update.run(target);
        {
            let mut failed = self
                .failed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *failed = (done.outcome == Outcome::Failed)
                .then(|| (done.target.clone(), self.world.clock.now()));
        }
        self.refresh();
    }
}
