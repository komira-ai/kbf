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

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, channel};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use super::coordinator::{Coordinator, CoordinatorError};
use super::flags::Timing;
use super::hold::{ANNOUNCED, Deferral, Features, Fit, Halt, Hold, MAX_RETRY_SECS, Marks, fit};
use super::local::{HostState, Local};
use super::renew::Renewer;
use super::status::{Shared, Status, version_of_commit};
use super::update::{Ports, Update};
use crate::inventory::Host;
use crate::ports::{Clock, Drain, Health, HealthCheck};
use crate::version::Version;
use crate::wire::{
    self, DeferredReport, Heartbeat, KBF_SERVICE, MAX_HEARTBEAT_BYTES, Outcome, ServiceReply, Token,
};

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
    /// The update the host declined, until its retry time (kept on disk by [`Marks`]).
    deferral: Mutex<Option<Deferral>>,
    /// Why the service is held after a halt: no order for it runs while the coordinator
    /// advertises `halt-v1`.
    halted: Mutex<Option<Halt>>,
}

fn held<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
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
            deferral: Mutex::new(None),
            halted: Mutex::new(None),
        }
    }

    /// The name the unit goes by on disk and in the logs.
    fn unit(&self) -> &str {
        self.service.as_deref().unwrap_or(KBF_SERVICE)
    }

    /// The deferral the heartbeat reports at `now`, when the coordinator takes one.
    fn deferred_report(&self, features: Features, now: u64) -> Option<DeferredReport> {
        features
            .deferred
            .then(|| held(&self.deferral).as_ref().and_then(|d| d.report(now)))
            .flatten()
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
    /// The features announced in every heartbeat; none for an agent that speaks as it always
    /// has.
    announced: Vec<String>,
    /// Which of them the coordinator's last reply advertised.
    advertised: Mutex<Features>,
    /// The clear generations of the last reply, by unit.
    generations: Mutex<BTreeMap<String, u64>>,
    /// Where deferrals are kept, for an agent that announces.
    marks: Option<Marks>,
    /// The most bytes of a heartbeat that are sent.
    max_beat: usize,
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
            announced: Vec::new(),
            advertised: Mutex::new(Features::default()),
            generations: Mutex::new(BTreeMap::new()),
            marks: None,
            max_beat: MAX_HEARTBEAT_BYTES,
        }
    }

    /// The agent also does `Deferred` and `halt` (`docs/design/host-updates.md`, 3.8): it
    /// announces `deferred-v1` and `halt-v1`, and uses each only after a reply advertises it.
    /// A deferral is kept in `dir` (`defer-<unit>`), so a restart keeps its `retry_after`; one
    /// already there is read now. Call it after [`Agent::with_services`], so every unit is
    /// read.
    #[must_use]
    pub fn with_host_updates(mut self, dir: PathBuf) -> Self {
        let marks = Marks::new(dir);
        for slot in &self.slots {
            match marks.load(slot.unit()) {
                Ok(deferral) => {
                    let now = (self.world.unix_now)();
                    *held(&slot.deferral) = deferral.map(|d| d.read_at(now));
                }
                Err(e) => self.log(&slot.say(&format!("a deferral on disk is ignored: {e}"))),
            }
            match marks.load_halt(slot.unit()) {
                Ok(halt) => *held(&slot.halted) = halt,
                Err(e) => self.log(&slot.say(&format!("a halt on disk is ignored: {e}"))),
            }
        }
        self.announced = ANNOUNCED.iter().map(|name| (*name).to_owned()).collect();
        self.marks = Some(marks);
        self
    }

    /// Send heartbeats of at most `max` bytes.
    #[cfg(test)]
    #[must_use]
    pub(super) fn with_beat_limit(mut self, max: usize) -> Self {
        self.max_beat = max;
        self
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

    /// The heartbeats, one every interval.
    fn heartbeats(&self, stop: &Stop, orders: &std::sync::mpsc::Sender<Order>) {
        while !stop.stopped() {
            if let Some(order) = self.beat_once() {
                let _ = orders.send(order);
            }
            sleep(self.timing.heartbeat, stop);
        }
    }

    /// One heartbeat, and the order its reply holds, if it holds one. A `kbf`-only agent that
    /// announces nothing sends today's and reads today's reply; one with services lists them
    /// and reads a reply that may name one; one that announces features reads the features
    /// the coordinator advertises as well.
    pub(super) fn beat_once(&self) -> Option<Order> {
        let beat = match fit(self.build_beat(), self.max_beat) {
            Fit::Whole(beat) => beat,
            Fit::Trimmed(beat) => {
                self.log(&format!(
                    "heartbeat: over {} bytes with the optional facts; sent without them",
                    self.max_beat
                ));
                beat
            }
            Fit::Oversized => {
                self.log(&format!(
                    "heartbeat: over {} bytes even without the optional facts; not sent",
                    self.max_beat
                ));
                return None;
            }
        };
        match self.send(&beat) {
            Ok(ServiceReply::Update { service, version }) => {
                Some((service.filter(|name| name != KBF_SERVICE), version))
            }
            Ok(ServiceReply::Nothing) => None,
            Err(e) => {
                self.log(&format!("heartbeat: {e}"));
                None
            }
        }
    }

    /// The heartbeat the status says: `kbf`'s facts, the services', the features announced and
    /// the deferrals the coordinator takes.
    fn build_beat(&self) -> Heartbeat {
        let now = (self.world.unix_now)();
        let features = self.features();
        let mut beat = self.slots[0].status.get().heartbeat();
        beat.agent_features.clone_from(&self.announced);
        beat.deferred = self.slots[0].deferred_report(features, now);
        if self.slots.len() > 1 {
            beat.services = Some(
                self.slots[1..]
                    .iter()
                    .filter_map(|slot| {
                        let mut beat = slot.status.get().service_beat(slot.service.as_deref()?);
                        beat.deferred = slot.deferred_report(features, now);
                        Some(beat)
                    })
                    .collect(),
            );
        }
        beat
    }

    fn send(&self, beat: &Heartbeat) -> Result<ServiceReply, CoordinatorError> {
        let coordinator = &self.world.coordinator;
        if !self.announced.is_empty() {
            let answer = coordinator.heartbeat_answer(beat)?;
            self.learn(answer.features.as_deref().unwrap_or_default());
            self.clear_halts(&answer.halts);
            Ok(answer.reply)
        } else if self.slots.len() == 1 {
            coordinator.heartbeat(beat).map(ServiceReply::from)
        } else {
            coordinator.heartbeat_services(beat)
        }
    }

    /// What the coordinator's last reply lets this agent use.
    fn features(&self) -> Features {
        *held(&self.advertised)
    }

    /// Take the features a reply advertises, and log a change.
    fn learn(&self, advertised: &[String]) {
        let now = Features::of(advertised);
        let before = std::mem::replace(&mut *held(&self.advertised), now);
        if before != now {
            self.log(&format!(
                "the coordinator advertises deferred-v1: {}, halt-v1: {}",
                now.deferred, now.halt
            ));
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

    /// An order for `target` of `service` (`None`: `kbf`): run it, unless the service is held
    /// after a halt, or the same target was deferred and its retry time has not come (both only
    /// while the coordinator advertises the feature), or the same target failed a moment ago.
    pub(super) fn order(&self, service: Option<&str>, target: &Version) {
        let Some(slot) = self.slots.iter().find(|s| s.service.as_deref() == service) else {
            self.log(&format!(
                "an order for {}, which this agent does not run",
                service.unwrap_or(KBF_SERVICE)
            ));
            return;
        };
        let features = self.features();
        let unix = (self.world.unix_now)();
        if features.halt
            && let Some(Halt { reason, .. }) = held(&slot.halted).as_ref()
        {
            self.log(&slot.say(&format!(
                "update to {target}: not run, the service is held after a halt ({})",
                reason.as_str()
            )));
            return;
        }
        if features.deferred
            && let Some(left) = held(&slot.deferral)
                .as_ref()
                .filter(|d| d.declines(target, unix))
                .map(|d| d.retry_after_unix - unix)
        {
            self.log(&slot.say(&format!(
                "update to {target}: declined, deferred for {left} s more"
            )));
            return;
        }
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
        // An update that runs ends any deferral of the unit: the host took the order.
        if held(&slot.deferral).take().is_some()
            && let Err(e) = self.marks.as_ref().map_or(Ok(()), |m| m.clear(slot.unit()))
        {
            self.log(&slot.say(&format!("the deferral on disk could not be removed: {e}")));
        }
        let done = update.run_for(target, self.features());
        match &done.hold {
            Some(Hold::Deferred { reason }) => self.defer(slot, target, reason),
            Some(Hold::Halt { reason }) => self.halt(slot, reason),
            None => {}
        }
        let deferred = matches!(done.hold, Some(Hold::Deferred { .. }));
        *held(&slot.failed) = (done.result.outcome == Outcome::Failed && !deferred)
            .then(|| (done.result.target.clone(), self.world.clock.now()));
        self.refresh();
    }

    /// The unit cannot be repaired by a retry: hold it, under the clear generation of the last
    /// reply, in memory and on disk.
    fn halt(&self, slot: &Slot, reason: &Token) {
        let generation = held(&self.generations)
            .get(slot.unit())
            .copied()
            .unwrap_or(0);
        let halt = Halt {
            reason: reason.clone(),
            generation,
        };
        if let Err(e) = self
            .marks
            .as_ref()
            .map_or(Ok(()), |m| m.save_halt(slot.unit(), &halt))
        {
            self.log(&slot.say(&format!("the halt could not be recorded on disk: {e}")));
        }
        *held(&slot.halted) = Some(halt);
    }

    /// Take the clear generations a reply carries: a held unit whose generation is newer than
    /// the one it halted under is cleared, and its hold is lifted.
    fn clear_halts(&self, halts: &BTreeMap<String, u64>) {
        *held(&self.generations) = halts.clone();
        if !self.features().halt {
            return;
        }
        for slot in &self.slots {
            let generation = halts.get(slot.unit()).copied().unwrap_or(0);
            let mut halted = held(&slot.halted);
            if halted.as_ref().is_some_and(|h| generation > h.generation) {
                *halted = None;
                drop(halted);
                self.log(&slot.say(&format!(
                    "the halt is lifted: cleared (generation {generation})"
                )));
                if let Err(e) = self
                    .marks
                    .as_ref()
                    .map_or(Ok(()), |m| m.clear_halt(slot.unit()))
                {
                    self.log(&slot.say(&format!("the halt on disk could not be removed: {e}")));
                }
            }
        }
    }

    /// The host declined `target`: remember until when, in memory and on disk.
    fn defer(&self, slot: &Slot, target: &Version, reason: &Token) {
        let retry = self.timing.retry.as_secs().min(MAX_RETRY_SECS);
        let deferral = Deferral {
            target: target.clone(),
            reason: reason.clone(),
            retry_after_unix: (self.world.unix_now)().saturating_add(retry),
        };
        self.log(&slot.say(&format!(
            "update to {target}: deferred ({}), not ordered again for {} s",
            reason.as_str(),
            retry
        )));
        if let Err(e) = self
            .marks
            .as_ref()
            .map_or(Ok(()), |m| m.save(slot.unit(), &deferral))
        {
            self.log(&slot.say(&format!("the deferral could not be recorded on disk: {e}")));
        }
        *held(&slot.deferral) = Some(deferral);
    }
}
