//! The farm's side of learned sizes in shadow ([`crate::learned`]): what it hands the
//! estimator from the places a lease starts, a result is reported, a lease is given up
//! and an operation is submitted. Every method returns at once while the estimator is
//! off.

use kbf_estimator::Bounds;
use kbf_invocation::Evidence;
use kbf_sched::{Prediction, Request, Scheduler};
use std::sync::atomic::Ordering;

use kbf_types::{Failure, LeaseId, LeaseKind, OperationId, Outcome, WaiterId, WorkerId};

use super::State;
use super::engine::{Adapter, Engine};
use super::side::Side;
use crate::action_usage::Measured;
use crate::learned::{
    Attempt, BATCH, Row, Skip, Unset, basis_of_os, hint_of, key_of, kill_run, run_of,
};
use crate::machine::Platform;
use crate::run_record;

impl Side {
    /// The bounds an attempt on `worker` is judged against: the node's memory and CPU.
    fn bounds_on(&self, sched: &Scheduler, worker: &WorkerId) -> Bounds {
        let capacity = sched.capacity(worker);
        Bounds {
            default_memory: self.learned.default_memory(),
            host_memory: capacity.map_or(u64::MAX, |c| c.memory_bytes),
            node_cpu_millis: capacity.map_or(u32::MAX, |c| {
                u32::try_from(c.cpu_millis).unwrap_or(u32::MAX)
            }),
        }
    }

    /// The bounds an attempt of `operation` on `worker` is judged against. With `on`, an
    /// operation that has a size key is bounded as the scheduler bounds its key floor (the
    /// largest node that could run the request, not the node that ran this attempt), so
    /// that the estimator's floor and the scheduler's agree on what a kill raised.
    pub(super) fn bounds_of(
        &self,
        sched: &Scheduler,
        operation: OperationId,
        worker: &WorkerId,
    ) -> Bounds {
        let mut bounds = self.bounds_on(sched, worker);
        if self.learned.keys() && sched.size_key(operation).is_some() {
            // Never below the worker's own memory (the bound is the largest worker, and
            // the one that reported is among those that could run it).
            bounds.host_memory = bounds.host_memory.max(sched.key_floor_bound_of(operation));
        }
        bounds
    }

    /// The bounds a new operation for `os` is asked about: the largest registered node of
    /// that OS (the scheduler's own cap also looks at the rest of the request's `needs`).
    pub(super) fn bounds_for_os(&self, sched: &Scheduler, os: &str) -> Bounds {
        let mut bounds = Bounds {
            default_memory: self.learned.default_memory(),
            host_memory: 0,
            node_cpu_millis: 0,
        };
        for worker in self.machine.nodes() {
            let same_os = self.links.get(worker).is_some_and(|l| l.os == os);
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
        if self.learned.off() {
            return;
        }
        let Ok(booked) = self.learned.booking(lease) else {
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
        let os_basis = self.links.get(worker).and_then(|l| basis_of_os(&l.os));
        let found = run_of(outcome, usage.as_ref(), booked).or_else(|| {
            os_basis
                .filter(|_| killed)
                .map(|basis| (basis, kill_run(booked)))
        });
        let Some((basis, run)) = found else {
            self.learned.stage(lease, None);
            return;
        };
        match key_of(&instance, &platform, basis, &subject) {
            Ok(key) => {
                self.learned.stage(lease, Some(Attempt { key, run }));
                self.learned.stage_prediction(lease, operation);
            }
            Err(why) => self.learned.skip(why),
        }
    }

    /// Names the estimator key of a keyed operation read back from the store, so that the
    /// decay of its summary reaches the operation's key floor again after a restart.
    pub(super) fn register_stored(&mut self, op: &super::durable::StoredOperation, size_key: u64) {
        let platform = Platform::of(&op.request.needs);
        let Some(subject) = &op.subject else {
            return;
        };
        let Some(basis) = basis_of_os(platform.os_family) else {
            return;
        };
        if let Ok(key) = key_of(&op.action.instance, &platform, basis, subject) {
            self.learned.register(&key, size_key);
        }
    }
}

impl Engine {
    /// `floors` with its key-floor part as the scheduler holds it now, for a requeue
    /// written (or written again) later than it was taken: the floor of the key as it
    /// stands, and a key's row deleted only if it is still forgotten. A requeue retried
    /// after a newer one therefore never puts an older floor over a higher row, nor deletes
    /// the row of a key raised again since.
    pub(super) fn current_key_floors(
        &self,
        floors: &kbf_sched::FloorChange,
    ) -> kbf_sched::FloorChange {
        let mut now = floors.clone();
        now.key_raised = floors
            .key_raised
            .as_ref()
            .and_then(|f| self.sched.key_floor_row(f.key));
        now.key_evicted
            .retain(|k| self.sched.key_floor(*k).is_none());
        now
    }

    /// Takes the key floors the estimator's decay lowered (`lowered`, as `(key, floor
    /// before the decay, floor asked)`) to the scheduler, and writes them down on a
    /// best-effort basis, as the size summaries are: a row the store refuses is kept and
    /// written again at a later tick (until then a restart books the higher floor). A floor
    /// the scheduler holds above what the estimator saw before the decay is left alone: a
    /// kill the estimator never saw raised it since.
    pub(super) fn lower_key_floors(&mut self, a: &dyn Adapter, lowered: Vec<(u64, u64, u64)>) {
        for (size_key, before, ask) in lowered {
            if self.sched.key_floor(size_key).is_some_and(|f| f > before) {
                continue;
            }
            let Some(row) = self.sched.lower_key_floor(size_key, ask) else {
                continue;
            };
            self.unwritten_floors.retain(|f| f.key != size_key);
            self.unwritten_floors.push(row);
        }
        self.write_key_floors(a);
    }

    /// Writes the lowered key floors waiting for the store; those it refuses stay.
    pub(super) fn write_key_floors(&mut self, a: &dyn Adapter) {
        let Some(durable) = self
            .durable
            .as_ref()
            .filter(|d| !self.fenced && !d.fenced.load(Ordering::SeqCst))
        else {
            // No store to write to, or a newer writer holds it: nothing is kept.
            self.unwritten_floors.clear();
            return;
        };
        let sched = &self.sched;
        // A pending row is written only while the scheduler still holds the floor it
        // lowered to: a kill that raised it since has written the higher row (a restart
        // loses what is pending, and the higher row it leaves is the safe side).
        self.unwritten_floors
            .retain(|row| sched.key_floor(row.key).unwrap_or(0) == row.ask);
        self.unwritten_floors.retain(|row| {
            let refused = durable
                .put_key_floor(row, a.summary_key(row.key).as_ref())
                .err();
            if let Some(error) = &refused {
                tracing::warn!(%error, "a lowered key floor was not written");
            }
            refused.is_some()
        });
    }

    /// Writes `rows`, size summaries, on a best-effort basis: a store that refuses them is
    /// logged and their keys are written again from their newest summary at a later tick.
    /// It never marks the store unreachable and never changes what the scheduler wrote.
    pub(super) fn write_estimates(&mut self, a: &mut dyn Adapter, rows: &[Row]) {
        let Some(durable) = self
            .durable
            .as_ref()
            .filter(|_| !self.fenced && !rows.is_empty())
        else {
            return;
        };
        if durable.fenced.load(Ordering::SeqCst) {
            return;
        }
        let mut refused = Vec::new();
        for chunk in rows.chunks(BATCH) {
            if let Err(error) = durable.put_estimates(chunk) {
                // A newer writer holds the store: nothing more is written, nor kept.
                if durable.superseded_by(&error) {
                    return;
                }
                tracing::warn!(%error, "size summaries were not written");
                refused.extend_from_slice(chunk);
            }
        }
        a.estimates_refused(&refused);
    }
}

impl State {
    /// The bounds an attempt of `operation` on `worker` is judged against
    /// ([`Side::bounds_of`]).
    #[cfg(test)]
    pub(super) fn bounds_of(&self, operation: OperationId, worker: &WorkerId) -> Bounds {
        self.side.bounds_of(&self.engine.sched, operation, worker)
    }

    /// Logs what the estimator would book for `request`, the new operation `name` that
    /// `waiter` waits on, `operation`. A caller that joined a twin (`joined`) is not a new
    /// operation. Returns the prediction the submit gives the scheduler for `operation`
    /// (`--placement-predictions on`; `shadow` remembers it to compare with the run).
    #[allow(clippy::too_many_arguments)]
    pub(super) fn learn_submit(
        &mut self,
        waiter: WaiterId,
        operation: OperationId,
        name: &str,
        request: &Request,
        evidence: &Evidence,
        joined: bool,
        size_key: Option<u64>,
    ) -> Option<Prediction> {
        let side = &mut self.side;
        if side.learned.off() || joined {
            return None;
        }
        if request.kind != LeaseKind::Action || request.resources.gpus != 0 {
            side.learned.skip(Skip::Kind);
            side.learned.unpredicted(Unset::Kind);
            return None;
        }
        let waiter = side.machine.waiter(waiter)?;
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
                side.learned.skip(why);
                side.learned.unpredicted(Unset::of_key_error(why));
                return None;
            }
        };
        if let Some(size_key) = size_key {
            side.learned.register(&key, size_key);
        }
        let bounds = side.bounds_for_os(&self.engine.sched, platform.os_family);
        let hint = hint_of(
            evidence,
            request.resources.memory_bytes,
            request.resources.cpu_millis,
        );
        let text = side.learned.estimate_text(&key, &hint, &bounds);
        tracing::info!(operation = %name, "size estimate: {text}");
        let predicted = side.learned.predict(&key, &hint, &bounds);
        side.learned.to_set(operation, predicted)
    }

    /// The scheduler has answered the report of `lease`, run by `worker`; `taken`: it
    /// took it.
    pub(super) fn finish_learning(
        &mut self,
        operation: OperationId,
        lease: LeaseId,
        worker: &WorkerId,
        taken: bool,
    ) {
        if self.side.learned.off() {
            return;
        }
        let bounds = self.side.bounds_of(&self.engine.sched, operation, worker);
        let rows = self.side.learned.finish(lease, taken, &bounds);
        self.engine.write_estimates(&mut self.side, &rows);
        self.lower_key_floors();
    }

    /// Takes the key floors the estimator's decay lowered to the scheduler, and writes
    /// them down ([`Engine::lower_key_floors`]).
    fn lower_key_floors(&mut self) {
        let lowered = self.side.learned.take_lowered();
        self.engine.lower_key_floors(&self.side, lowered);
    }

    /// Writes the changed summaries and the floors not yet written; `now` skips the wait
    /// for the write-behind interval.
    pub(super) fn write_behind(&mut self, now: bool) {
        let rows = if now {
            self.side.learned.due_now()
        } else {
            self.side.learned.due()
        };
        self.engine.write_estimates(&mut self.side, &rows);
        if !self.engine.unwritten_floors.is_empty() {
            self.engine.write_key_floors(&self.side);
        }
    }

    /// Writes behind the summaries changed since they were last written, and sweeps the
    /// stale ones from the store once an hour, a bounded number of rows at a tick.
    pub(super) fn flush_learned(&mut self) {
        if self.side.learned.off() || self.engine.fenced {
            return;
        }
        self.write_behind(false);
        let Some(today) = self.side.learned.sweep_due() else {
            return;
        };
        let more = match self
            .engine
            .durable
            .as_ref()
            .map(|d| d.sweep_estimates(today))
        {
            Some(Ok((_, more))) => more,
            Some(Err(error)) => {
                tracing::warn!(%error, "the size summaries could not be swept");
                false
            }
            None => false,
        };
        self.side.learned.swept(more);
    }
}
