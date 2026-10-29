//! Learned sizes in shadow (`--learned-sizes=shadow`, `crate::learned`) on the js core: what
//! the adapter hands the estimator from the places a lease starts, a result is reported, a
//! lease is given up and an operation is submitted, and the size summaries (`est/` rows)
//! it keeps in the state store. The farm's own driver does the same from `farm/learning.rs`.
//!
//! Shadow never changes a booking: the estimator is shown what happened and says what it
//! would have booked, in a log line and a tally. The `on` mode (size keys on submit, key
//! floors on requeue) is not supported here; a farm asked for it learns as `shadow` does.
//! Every method returns at once while the estimator is off.

// Only the farm of the js core (and the tests) write the summaries; a build without it keeps
// the pieces for them compiled.
#![cfg_attr(
    not(any(feature = "farm-core-js", test)),
    allow(dead_code, unused_imports)
)]

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use js_server_core::{Core, View};
use kbf_estimator::{Bounds, Day, Key, Summaries};
use kbf_invocation::Evidence;
use kbf_sched::{Requeue, RequeueReason, Scheduler};
use kbf_state::{Cond, Store, StoreError, Txn};
use kbf_types::{
    Failure, FarmTime, LeaseId, LeaseKind, OperationId, Outcome, StartLease, WorkerId,
};

use super::adapter::KbfAdapter;
use super::ext::KbfExt;
use crate::action_usage::Measured;
use crate::farm::durable::{ESTIMATE, estimate};
use crate::learned::{
    Attempt, BATCH, Learned, LearnedSizes, Row, Skip, basis_of_os, hint_of, key_of, kill_run,
    run_of,
};
use crate::run_record;

/// Transactions of at most [`SWEEP_ROWS`] rows a tick's sweep commits.
const SWEEP_TXNS: usize = 4;
/// Rows in one transaction of the sweep.
const SWEEP_ROWS: usize = 256;

/// What the adapter keeps for the estimator.
pub(super) struct Learning {
    learned: Learned,
    /// Whether the core has a state store to write the summaries to.
    stored: bool,
    /// The size a lease given up after an own-limit kill will be booked at next, by lease,
    /// from the core's `requeued` hook until the report that caused it is answered.
    raised: BTreeMap<LeaseId, u64>,
    /// The summaries an own-limit kill changed, which [`write_learned`] writes at once.
    urgent: BTreeSet<Key>,
}

impl Learning {
    /// A farm that learns nothing.
    pub(super) fn off() -> Self {
        Self::of(Learned::new(LearnedSizes::Off, 0, 0), false)
    }

    /// The state of `learned`, writing its summaries to a store if `stored`. A mode of `on`
    /// learns as `shadow`.
    pub(super) fn of(learned: Learned, stored: bool) -> Self {
        Self {
            learned,
            stored,
            raised: BTreeMap::new(),
            urgent: BTreeSet::new(),
        }
    }
}

/// What a submit asked for, as the estimate is made from it.
pub(crate) struct Sized {
    /// The lease kind.
    pub(crate) kind: LeaseKind,
    /// The GPUs asked for.
    pub(crate) gpus: u64,
    /// The memory asked for, in bytes.
    pub(crate) memory_bytes: u64,
    /// The cores asked for, in milli-cores.
    pub(crate) cpu_millis: u64,
    /// What the client named of the size.
    pub(crate) evidence: Evidence,
}

impl KbfAdapter {
    /// The bounds an attempt on `worker` is judged against: the node's memory and CPU.
    fn bounds_on(&self, sched: &Scheduler, worker: &WorkerId) -> Bounds {
        let capacity = sched.capacity(worker);
        Bounds {
            default_memory: self.learning.learned.default_memory(),
            host_memory: capacity.map_or(u64::MAX, |c| c.memory_bytes),
            node_cpu_millis: capacity.map_or(u32::MAX, |c| {
                u32::try_from(c.cpu_millis).unwrap_or(u32::MAX)
            }),
        }
    }

    /// The bounds a new operation for `os` is asked about: the largest registered node of
    /// that OS.
    fn bounds_for_os(&self, sched: &Scheduler, os: &str) -> Bounds {
        let mut bounds = Bounds {
            default_memory: self.learning.learned.default_memory(),
            host_memory: 0,
            node_cpu_millis: 0,
        };
        for worker in self.machine.nodes() {
            let same_os = self.worker_facts(worker).is_some_and(|f| f.os == os);
            if let Some(capacity) = sched.capacity(worker).filter(|_| same_os) {
                bounds.host_memory = bounds.host_memory.max(capacity.memory_bytes);
                let cpu = u32::try_from(capacity.cpu_millis).unwrap_or(u32::MAX);
                bounds.node_cpu_millis = bounds.node_cpu_millis.max(cpu);
            }
        }
        if bounds.host_memory == 0 {
            bounds.host_memory = u64::MAX;
            bounds.node_cpu_millis = u32::MAX;
        }
        bounds
    }

    /// Moves the estimator's clock on to the farm time of the feed about to start.
    pub(super) fn learning_clock(&mut self, now: FarmTime) {
        self.learning.learned.clock(now);
    }

    /// Remembers what a lease that starts books.
    pub(super) fn learning_started(&mut self, start: &StartLease) {
        self.learning.learned.on_start(start);
    }

    /// A lease given up: after an own-limit kill the estimator is told what the next run
    /// books once the report that caused it is answered; for any other reason the lease is
    /// forgotten.
    pub(super) fn learning_requeued(&mut self, requeue: &Requeue) {
        if self.learning.learned.off() {
            return;
        }
        if let RequeueReason::OutOfMemory { raised, .. } = requeue.reason {
            self.learning.raised.insert(requeue.lease, raised);
        } else {
            self.learning.learned.forget(requeue.lease);
        }
    }

    /// The report of `lease` of `operation`, about to be fed to the scheduler: what the
    /// estimator will be shown of it if the scheduler takes it. Nothing is learned yet.
    pub(super) fn stage_learning(
        &mut self,
        sched: &Scheduler,
        operation: OperationId,
        lease: LeaseId,
        worker: &WorkerId,
        outcome: Outcome,
        measured: Option<&Measured>,
    ) {
        let learned = &mut self.learning.learned;
        if learned.off() {
            return;
        }
        let Ok(booked) = learned.booking(lease) else {
            return;
        };
        let Some((_, waiter)) = self.first_waiter(sched, operation) else {
            return;
        };
        let (instance, platform, subject) = (
            waiter.key.instance.clone(),
            waiter.platform.clone(),
            waiter.usage.clone(),
        );
        let usage = measured.map(|m| run_record::run_usage(m.usage()));
        // An own-limit kill that carried no usage still happened: it is measured by its
        // node's OS and died at its limit.
        let killed = outcome == Outcome::Failed(Failure::OutOfMemory);
        let os_basis = self
            .facts
            .get(worker)
            .and_then(|facts| basis_of_os(&facts.os));
        let found = run_of(outcome, usage.as_ref(), booked).or_else(|| {
            os_basis
                .filter(|_| killed)
                .map(|basis| (basis, kill_run(booked)))
        });
        let learned = &mut self.learning.learned;
        let Some((basis, run)) = found else {
            learned.stage(lease, None);
            return;
        };
        match key_of(&instance, &platform, basis, &subject) {
            Ok(key) => learned.stage(lease, Some(Attempt { key, run })),
            Err(why) => learned.skip(why),
        }
    }

    /// The scheduler has answered the report of `lease`, run by `worker`; `taken`: it took
    /// it (or holds its finish for the store). Shows the estimator the attempt and writes
    /// the summaries an own-limit kill changed.
    pub(super) fn finish_learning(
        &mut self,
        view: &View<'_>,
        lease: LeaseId,
        worker: &WorkerId,
        taken: bool,
    ) {
        if self.learning.learned.off() {
            return;
        }
        let bounds = self.bounds_on(view.sched(), worker);
        let rows = match self.learning.raised.remove(&lease) {
            Some(raised) => self.learning.learned.requeued(lease, raised, &bounds),
            None => self.learning.learned.finish(lease, taken, &bounds),
        };
        self.learning
            .urgent
            .extend(rows.into_iter().map(|(key, _)| key));
    }

    /// Logs what the estimator would book for the new operation `admitted`, that `sized`
    /// describes. A caller that joined a twin is not a new operation.
    pub(crate) fn learn_submit(
        &mut self,
        view: &View<'_>,
        admitted: &js_server_core::Admitted,
        sized: &Sized,
    ) {
        if self.learning.learned.off() || admitted.joined {
            return;
        }
        if sized.kind != LeaseKind::Action || sized.gpus != 0 {
            self.learning.learned.skip(Skip::Kind);
            return;
        }
        let Some(waiter) = self.machine.waiter(admitted.waiter) else {
            return;
        };
        let (instance, platform, subject) = (
            waiter.key.instance.clone(),
            waiter.platform.clone(),
            waiter.usage.clone(),
        );
        let key = basis_of_os(platform.os_family)
            .ok_or(Skip::NoOs)
            .and_then(|basis| key_of(&instance, &platform, basis, &subject));
        let key = match key {
            Ok(key) => key,
            Err(why) => {
                self.learning.learned.skip(why);
                return;
            }
        };
        let bounds = self.bounds_for_os(view.sched(), platform.os_family);
        let hint = hint_of(&sized.evidence, sized.memory_bytes, sized.cpu_millis);
        let text = self.learning.learned.estimate_text(&key, &hint, &bounds);
        let name = crate::machine::stored_name(admitted.operation);
        tracing::info!(operation = %name, "size estimate: {text}");
    }

    /// Takes up the size summaries the state store holds, when the core starts.
    pub(crate) fn restore_learned(&mut self) {
        if self.learning.learned.off() || !self.learning.stored {
            return;
        }
        match KbfExt.estimates(&*self.store) {
            Ok(rows) => self.learning.learned.restore(&rows),
            Err(error) => {
                tracing::warn!(%error, "the size summaries could not be read: every action starts cold");
            }
        }
    }

    /// Copies the estimator's tally into the scrape.
    #[cfg_attr(not(feature = "farm-core-js"), allow(dead_code))]
    pub(crate) fn publish_learned(&self) {
        let metrics = self.cache.metrics();
        self.learning.learned.publish(&metrics.estimator_shadow);
    }

    /// Whether summaries a kill changed wait to be written ([`write_learned`]).
    pub(crate) fn learning_pending(&self) -> bool {
        !self.learning.urgent.is_empty()
    }

    /// Takes what [`write_learned`] writes, under the core's lock and from memory only: the
    /// summaries a kill changed, as they are now; with `tick`, also the dirty summaries that
    /// are due and the day to sweep the store for, if a sweep is due. Takes nothing while the
    /// estimator is off.
    pub(crate) fn take_learned_work(&mut self, take: Take) -> Work {
        let learning = &mut self.learning;
        if learning.learned.off() {
            return Work::default();
        }
        let urgent = std::mem::take(&mut learning.urgent);
        let mut rows = learning.learned.take_rows(urgent);
        let mut sweep = None;
        if matches!(take, Take::Tick) {
            rows.extend(learning.learned.due());
            sweep = learning.learned.sweep_due();
        } else if matches!(take, Take::Now) {
            rows.extend(learning.learned.due_now());
        }
        let store = learning.stored.then(|| Arc::clone(&self.store));
        Work { rows, sweep, store }
    }

    /// Takes what writing [`Work`] left: the rows the store refused are written again from
    /// their newest summary at a later tick, and a sweep says whether it has more to do.
    pub(crate) fn finish_learned_work(&mut self, refused: &[Row], sweep_more: Option<bool>) {
        self.learning.learned.unwritten(refused);
        if let Some(more) = sweep_more {
            self.learning.learned.swept(more);
        }
    }
}

/// What the estimator has for the state store, taken under the core's lock.
#[derive(Default)]
pub(crate) struct Work {
    rows: Vec<Row>,
    sweep: Option<Day>,
    store: Option<Arc<dyn Store>>,
}

/// What a writer of size summaries takes under the core's lock.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Take {
    /// The summaries a kill changed.
    Urgent,
    /// Those, the dirty summaries that are due, and the sweep if one is due.
    Tick,
    /// Those, and every dirty summary whatever the time since the last write.
    Now,
}

/// Lets one writer of size summaries at a time take its rows and write them, so that a
/// write of an older summary never lands after a write of a newer one.
#[derive(Default)]
pub(crate) struct Gate(pub(super) tokio::sync::Mutex<()>);

/// Writes the size summaries the estimator holds for the state store, with no hold of the
/// core's lock across a store call: the writer takes the gate, takes its rows from the
/// core in one short hold (each key's newest summary), writes them, and tells the core what
/// the store refused. `tick`: the dirty summaries that are due and the hourly sweep go too;
/// otherwise only the summaries a kill changed, which is written at once, after the report
/// that changed them. A newer writer's fence stops the writing and keeps nothing.
pub(crate) async fn write_learned(core: &Core<KbfAdapter>, gate: &Gate, tick: bool) {
    if !tick && !core.with(|_, adapter| adapter.learning_pending()) {
        return;
    }
    let _writer = gate.0.lock().await;
    write_taken(core, if tick { Take::Tick } else { Take::Urgent });
}

/// Writes every changed summary now, whatever the time since the last write, and returns when
/// it is written: for `Farm::write_behind_now`, which is not async, as the farm's own driver's
/// is not. No awaiting happens while a writer holds the gate (the store's calls are blocking),
/// so on a runtime of one thread the gate is free here and on a runtime of many a writer that
/// holds it is a few store calls from letting go: waiting for it needs no runtime.
pub(crate) fn write_behind_now(core: &Core<KbfAdapter>, gate: &Gate) {
    let _writer = hold(gate, std::thread::yield_now);
    write_taken(core, Take::Now);
}

/// Takes the gate without awaiting, calling `waiting` each time another writer holds it.
pub(super) fn hold<'g>(
    gate: &'g Gate,
    mut waiting: impl FnMut(),
) -> tokio::sync::MutexGuard<'g, ()> {
    loop {
        if let Ok(held) = gate.0.try_lock() {
            return held;
        }
        waiting();
    }
}
/// Takes what `take` says under the core's lock and writes it, with no hold of the core's lock
/// across a store call. The caller holds the gate.
fn write_taken(core: &Core<KbfAdapter>, take: Take) {
    let work = core.with(|_, adapter| adapter.take_learned_work(take));
    let Work { rows, sweep, store } = work;
    let Some(store) = store else {
        // Nothing to write to: a sweep that is due ends at once.
        core.with(|_, adapter| adapter.finish_learned_work(&[], sweep.map(|_| false)));
        return;
    };
    let (refused, fenced) = write_estimates(&*store, &rows);
    // A sweep the fence stopped, or the store refused, is over until the next hour.
    let more = sweep.map(|today| {
        if fenced {
            return false;
        }
        sweep_estimates(&*store, today).unwrap_or_else(|error| {
            tracing::warn!(%error, "the size summaries could not be swept");
            false
        })
    });
    core.with(|_, adapter| adapter.finish_learned_work(&refused, more));
}

/// Writes `rows` on a best-effort basis: the rows a store refuses are returned, to be
/// written again from their newest summary at a later tick. Whether a newer writer holds the
/// store, which ends the writing with nothing kept.
fn write_estimates(store: &dyn Store, rows: &[Row]) -> (Vec<Row>, bool) {
    let mut refused = Vec::new();
    for chunk in rows.chunks(BATCH) {
        if let Err(error) = commit(store, KbfExt.put_estimates(Txn::new(), chunk)) {
            if matches!(error, StoreError::Fenced) {
                return (Vec::new(), true);
            }
            tracing::warn!(%error, "size summaries were not written");
            refused.extend_from_slice(chunk);
        }
    }
    (refused, false)
}

/// Commits `txn` to the state store.
fn commit(store: &dyn Store, txn: Txn) -> Result<(), StoreError> {
    let mut results = store.apply_batch(vec![txn])?;
    match results.pop() {
        Some(Ok(_)) => Ok(()),
        _ => Err(StoreError::Failed(
            "the size summaries were not committed".to_owned(),
        )),
    }
}

/// Deletes the size summaries idle past the estimator's staleness on `today`, and strips the
/// samples from those that hold a floor, as the estimator itself does in memory. At most
/// [`SWEEP_TXNS`] transactions of [`SWEEP_ROWS`] rows are committed per call. Returns
/// whether more are left for another call.
fn sweep_estimates(store: &dyn Store, today: Day) -> Result<bool, StoreError> {
    let mut changes = Vec::new();
    for (name, row) in store.scan_prefix(ESTIMATE)? {
        let Some((key, summary)) = estimate::decode(&name, &row.value) else {
            continue;
        };
        let mut held = Summaries::default();
        if held.insert_encoded(&key, &summary).is_err() {
            continue;
        }
        held.expire(today);
        match held.get(&key) {
            None => changes.push((name, None, row.version)),
            Some(kept) if kept.encode() != summary => {
                changes.push((name, Some(estimate::encode(&kept.encode())), row.version));
            }
            Some(_) => {}
        }
    }
    let left = changes.len() > SWEEP_TXNS * SWEEP_ROWS;
    let mut changed = 0;
    for chunk in changes.chunks(SWEEP_ROWS).take(SWEEP_TXNS) {
        let mut txn = Txn::new();
        for (name, value, version) in chunk {
            txn = match value {
                None => txn.delete(name.clone(), Cond::Version(*version)),
                Some(value) => txn.put(name.clone(), value.clone(), Cond::Version(*version)),
            };
        }
        match commit(store, txn) {
            Ok(()) => changed += chunk.len(),
            Err(error) => {
                tracing::warn!(%error, "a batch of size summaries was not swept");
                if matches!(error, StoreError::Fenced) {
                    break;
                }
            }
        }
    }
    // A sweep that committed nothing does not ask for another tick.
    Ok(left && changed > 0)
}

impl KbfAdapter {
    /// This adapter, showing the estimator of `learned` the attempts that finish. `stored`:
    /// the core has a state store to write the summaries to.
    #[must_use]
    #[cfg_attr(not(feature = "farm-core-js"), allow(dead_code))]
    pub(crate) fn learning_with(mut self, learned: Learned, stored: bool) -> Self {
        self.learning = Learning::of(learned, stored);
        self
    }
}

#[cfg(test)]
impl KbfAdapter {
    /// The row of the summary of `key` as the estimator holds it now.
    pub(super) fn summary_now(&self, key: &kbf_estimator::Key) -> Option<Vec<u8>> {
        self.learning
            .learned
            .summaries()
            .get(key)
            .map(|s| s.encode())
    }
}
