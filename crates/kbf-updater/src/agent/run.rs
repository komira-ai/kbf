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
//! A host that runs more services than `kbf` (`--service`) has one more slot of state for each: its
//! version, health and last update go in the heartbeat's `services`, an order may name one of
//! them, and it is run with that service's own ports. The update thread is the only one that
//! updates, so two A/Bs never overlap on the host, whatever the services.
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
use crate::wire::{self, KBF_SERVICE, Outcome, ServiceReply};

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

/// The ports of one service besides `kbf`: which helper (and channel) installs it, how its node
/// is drained, and how its health is judged. They are data: the A/B is the same for every
/// service.
pub struct ServicePorts {
    pub name: String,
    pub channel: String,
    pub local: Arc<dyn Local>,
    pub drain: Arc<dyn Drain>,
    pub health: Arc<dyn HealthCheck>,
}

/// An order: the service it names (`None`: `kbf`) and the version.
type Order = (Option<String>, Version);

/// What the agent keeps for one service: its ports and its state.
struct Slot {
    /// `None` for `kbf`.
    service: Option<String>,
    channel: String,
    local: Arc<dyn Local>,
    drain: Arc<dyn Drain>,
    health: Arc<dyn HealthCheck>,
    status: Shared,
    /// The target that failed last, and when.
    failed: Mutex<Option<(Version, Duration)>>,
    /// Why the service was last reported unhealthy, so a reason is logged once and not on every
    /// heartbeat.
    unhealthy: Mutex<Option<String>>,
}

impl Slot {
    fn new(
        service: Option<String>,
        channel: String,
        ports: (Arc<dyn Local>, Arc<dyn Drain>, Arc<dyn HealthCheck>),
        status: Status,
    ) -> Self {
        Self {
            service,
            channel,
            local: ports.0,
            drain: ports.1,
            health: ports.2,
            status: Shared::new(status),
            failed: Mutex::new(None),
            unhealthy: Mutex::new(None),
        }
    }

    /// `line`, led by the service's name when it is not `kbf`.
    fn say(&self, line: &str) -> String {
        match &self.service {
            Some(name) => format!("{name}: {line}"),
            None => line.to_owned(),
        }
    }
}

/// The agent of one host.
pub struct Agent {
    host: Host,
    timing: Timing,
    world: World,
    /// `kbf` first, then the services in the order they were given.
    slots: Vec<Slot>,
}

impl Agent {
    pub fn new(host: Host, channel: String, timing: Timing, world: World, status: Status) -> Self {
        let kbf = Slot::new(
            None,
            channel,
            (
                world.local.clone(),
                world.drain.clone(),
                world.health.clone(),
            ),
            status,
        );
        Self {
            host,
            timing,
            world,
            slots: vec![kbf],
        }
    }

    /// The agent also runs `services`: its heartbeat lists them and it reads orders that name
    /// one. Without any, the agent is a `kbf`-only agent and speaks as one.
    #[must_use]
    pub fn with_services(mut self, services: Vec<ServicePorts>) -> Self {
        let kbf = self.slots[0].status.get();
        for service in services {
            self.slots.push(Slot::new(
                Some(service.name),
                service.channel,
                (service.local, service.drain, service.health),
                Status::unknown(kbf.platform, kbf.version.clone()),
            ));
        }
        self
    }

    /// The status of `kbf` the heartbeats say.
    pub fn status(&self) -> Status {
        self.slots[0].status.get()
    }

    /// The status of the service `name`, if the agent runs it.
    pub fn service_status(&self, name: &str) -> Option<Status> {
        self.slots
            .iter()
            .find(|s| s.service.as_deref() == Some(name))
            .map(|s| s.status.get())
    }

    fn log(&self, line: &str) {
        (self.world.log)(line);
    }

    /// Log why the service is not healthy when the reason is not the one logged last, and log
    /// once when it is healthy again (`None`).
    fn log_health(&self, slot: &Slot, reason: Option<String>) {
        let mut last = slot.unhealthy.lock().unwrap_or_else(|e| e.into_inner());
        if *last == reason {
            return;
        }
        match &reason {
            Some(reason) => self.log(&slot.say(&format!("the host is not healthy: {reason}"))),
            None => self.log(&slot.say("the host is healthy again")),
        }
        *last = reason;
    }

    /// Run until `stop`: finish an update the helper is in the middle of, heartbeat, and update
    /// when told to.
    pub fn run(&self, stop: &Stop) {
        self.refresh();
        // The updater was restarted in the middle of an A/B: the helper has a release on trial.
        // The first heartbeat says so, and the trial is finished before any other order.
        let mut resumed = Vec::new();
        for (i, slot) in self.slots.iter().enumerate() {
            if let Some(target) = self.trial_in_progress(slot) {
                slot.status.update(|s| s.updating = Some(target.clone()));
                resumed.push((i, target));
            }
        }
        let (orders, inbox) = channel();
        std::thread::scope(|scope| {
            scope.spawn(|| self.heartbeats(stop, &orders));
            if let Some(renewer) = &self.world.renewer {
                scope.spawn(|| self.renewals(renewer.as_ref(), stop));
            }
            for (i, target) in &resumed {
                self.update(&self.slots[*i], target);
            }
            self.updates(stop, &inbox);
        });
    }

    /// Ask the host what each of its services runs and whether it is healthy, and say so in the
    /// status.
    pub fn refresh(&self) {
        for slot in &self.slots {
            match slot.local.query() {
                Ok(state) => self.note(slot, &state),
                Err(e) => {
                    self.log_health(slot, Some(format!("the host helper does not answer: {e}")));
                    slot.status.update(|s| s.health = wire::Health::Unhealthy);
                }
            }
            // A cordon an operator gave back by hand: the drain forgets its mark.
            if let Err(e) = slot.drain.observe(&self.host) {
                self.log(&slot.say(&format!("the node's cordon could not be looked at: {e}")));
            }
        }
    }

    fn note(&self, slot: &Slot, state: &HostState) {
        let running = state
            .version
            .clone()
            .or_else(|| state.commit.as_deref().and_then(version_of_commit));
        let Some(running) = running else {
            self.log_health(
                slot,
                Some("the host helper reports neither a version nor a known commit".to_owned()),
            );
            slot.status.update(|s| s.health = wire::Health::Unhealthy);
            return;
        };
        let reason = match slot.health.check(&self.host, &running) {
            Ok(Health::Healthy) => None,
            Ok(Health::Waiting(why)) => Some(format!("not yet healthy on {running}: {why}")),
            Ok(Health::Abandoned(why)) => {
                Some(format!("will not become healthy on {running}: {why}"))
            }
            Err(e) => Some(format!("the health check of {running} failed: {e}")),
        };
        let healthy = reason.is_none();
        self.log_health(slot, reason);
        slot.status.update(|s| {
            s.version = running;
            s.health = if healthy {
                wire::Health::Healthy
            } else {
                wire::Health::Unhealthy
            };
        });
    }

    /// The version of the release the helper has on trial, if it has one it can name.
    fn trial_in_progress(&self, slot: &Slot) -> Option<Version> {
        slot.local.query().ok()?.pending?.version
    }

    /// The heartbeat as it goes out: a `kbf`-only agent sends today's, and reads today's reply;
    /// one with services lists them, and reads a reply that may name one.
    fn heartbeats(&self, stop: &Stop, orders: &std::sync::mpsc::Sender<Order>) {
        while !stop.stopped() {
            let mut beat = self.slots[0].status.get().heartbeat();
            let reply = if self.slots.len() == 1 {
                self.world
                    .coordinator
                    .heartbeat(&beat)
                    .map(ServiceReply::from)
            } else {
                beat.services = Some(
                    self.slots[1..]
                        .iter()
                        .filter_map(|slot| {
                            Some(slot.status.get().service_beat(slot.service.as_deref()?))
                        })
                        .collect(),
                );
                self.world.coordinator.heartbeat_services(&beat)
            };
            match reply {
                Ok(ServiceReply::Update { service, version }) => {
                    let service = service.filter(|name| name != KBF_SERVICE);
                    let _ = orders.send((service, version));
                }
                Ok(ServiceReply::Nothing) => {}
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

    fn updates(&self, stop: &Stop, inbox: &Receiver<Order>) {
        let mut idle = Duration::ZERO;
        while !stop.stopped() {
            if let Ok(first) = inbox.recv_timeout(SLICE) {
                // The newest order for each service wins; the ones queued behind it were the
                // same. They run one after the other: the host updates one service at a time.
                let mut orders = vec![first];
                for (service, target) in inbox.try_iter() {
                    match orders.iter_mut().find(|(s, _)| *s == service) {
                        Some(order) => order.1 = target,
                        None => orders.push((service, target)),
                    }
                }
                for (service, target) in &orders {
                    self.order(service.as_deref(), target);
                }
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

    /// An order for `target` of `service` (`None`: `kbf`): run it, unless the same target
    /// failed a moment ago.
    fn order(&self, service: Option<&str>, target: &Version) {
        let Some(slot) = self.slots.iter().find(|s| s.service.as_deref() == service) else {
            self.log(&format!(
                "an order for {}, which this agent does not run",
                service.unwrap_or(KBF_SERVICE)
            ));
            return;
        };
        let now = self.world.clock.now();
        let backing_off = slot
            .failed
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .is_some_and(|(failed, at)| {
                failed == target && now.saturating_sub(*at) < self.timing.retry
            });
        if backing_off {
            self.log(&slot.say(&format!(
                "update to {target}: failed a moment ago; not again yet"
            )));
            return;
        }
        self.update(slot, target);
    }

    fn update(&self, slot: &Slot, target: &Version) {
        let log = |line: &str| (self.world.log)(&slot.say(line));
        let update = Update {
            ports: Ports {
                local: &*slot.local,
                drain: &*slot.drain,
                health: &*slot.health,
                clock: &*self.world.clock,
                unix_now: &*self.world.unix_now,
                log: &log,
            },
            host: &self.host,
            channel: &slot.channel,
            timing: self.timing,
            status: &slot.status,
        };
        let done = update.run(target);
        {
            let mut failed = slot
                .failed
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            *failed = (done.outcome == Outcome::Failed)
                .then(|| (done.target.clone(), self.world.clock.now()));
        }
        self.refresh();
    }
}
