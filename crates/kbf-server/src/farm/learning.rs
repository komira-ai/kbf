//! The farm's side of learned sizes in shadow ([`crate::learned`]): what it hands the
//! estimator from the places a lease starts, a result is reported, a lease is given up
//! and an operation is submitted. Every method returns at once while the estimator is
//! off.

use kbf_estimator::Bounds;
use kbf_invocation::Evidence;
use kbf_sched::{Request, Requeue, RequeueReason};
use std::sync::atomic::Ordering;

use kbf_types::{Failure, LeaseId, LeaseKind, OperationId, Outcome, WorkerId};

use super::State;
use crate::action_usage::Measured;
use crate::learned::{Attempt, BATCH, Row, Skip, basis_of_os, hint_of, key_of, kill_run, run_of};
use crate::run_record;

impl State {
    /// The bounds an attempt on `worker` is judged against: the node's memory and CPU.
    fn bounds_on(&self, worker: &WorkerId) -> Bounds {
        let capacity = self.sched.capacity(worker);
        Bounds {
            default_memory: self.learned.default_memory(),
            host_memory: capacity.map_or(u64::MAX, |c| c.memory_bytes),
            node_cpu_millis: capacity.map_or(u32::MAX, |c| {
                u32::try_from(c.cpu_millis).unwrap_or(u32::MAX)
            }),
        }
    }

    /// The bounds a new operation for `os` is asked about: the largest registered node of
    /// that OS (the scheduler's own cap also looks at the rest of the request's `needs`).
    fn bounds_for_os(&self, os: &str) -> Bounds {
        let mut bounds = Bounds {
            default_memory: self.learned.default_memory(),
            host_memory: 0,
            node_cpu_millis: 0,
        };
        for worker in self.machine.nodes() {
            let same_os = self.links.get(worker).is_some_and(|l| l.os == os);
            if let Some(capacity) = self.sched.capacity(worker).filter(|_| same_os) {
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
        let Some((_, waiter)) = self.first_waiter(operation) else {
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
            Ok(key) => self.learned.stage(lease, Some(Attempt { key, run })),
            Err(why) => self.learned.skip(why),
        }
    }

    /// The scheduler has answered the report of `lease`, run by `worker`; `taken`: it
    /// took it.
    pub(super) fn finish_learning(&mut self, lease: LeaseId, worker: &WorkerId, taken: bool) {
        if self.learned.off() {
            return;
        }
        let bounds = self.bounds_on(worker);
        let rows = self.learned.finish(lease, taken, &bounds);
        self.write_estimates(&rows);
    }

    /// The scheduler gave up `requeue`'s lease: the rows to write in the transaction of
    /// the requeue (the summaries an own-limit kill changed).
    pub(super) fn learned_requeue(&mut self, requeue: &Requeue) -> Vec<Row> {
        if self.learned.off() {
            return Vec::new();
        }
        if let RequeueReason::OutOfMemory { raised, .. } = requeue.reason {
            let bounds = self.bounds_on(&requeue.worker);
            return self.learned.requeued(requeue.lease, raised, &bounds);
        }
        self.learned.forget(requeue.lease);
        Vec::new()
    }

    /// Logs what the estimator would book for `request`, the new operation `name`. A
    /// caller that joined a twin (`joined`) is not a new operation.
    pub(super) fn learn_submit(
        &mut self,
        name: &str,
        request: &Request,
        evidence: &Evidence,
        joined: bool,
    ) {
        if self.learned.off() || joined {
            return;
        }
        if request.kind != LeaseKind::Action || request.resources.gpus != 0 {
            self.learned.skip(Skip::Kind);
            return;
        }
        let Some((_, waiter)) = self.machine.named(name) else {
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
                self.learned.skip(why);
                return;
            }
        };
        let bounds = self.bounds_for_os(platform.os_family);
        let hint = hint_of(
            evidence,
            request.resources.memory_bytes,
            request.resources.cpu_millis,
        );
        let text = self.learned.estimate_text(&key, &hint, &bounds);
        tracing::info!(operation = %name, "size estimate: {text}");
    }

    /// Writes `rows`, size summaries, on a best-effort basis: a store that refuses them is
    /// logged and their keys are written again from their newest summary at a later tick.
    /// It never marks the store unreachable and never changes what the scheduler wrote.
    pub(super) fn write_estimates(&mut self, rows: &[Row]) {
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
                tracing::warn!(%error, "size summaries were not written");
                refused.extend_from_slice(chunk);
            }
        }
        self.learned.unwritten(&refused);
    }

    /// Writes behind the summaries changed since they were last written, and sweeps the
    /// stale ones from the store once an hour, a bounded number of rows at a tick.
    pub(super) fn flush_learned(&mut self) {
        if self.learned.off() || self.fenced {
            return;
        }
        let rows = self.learned.due();
        self.write_estimates(&rows);
        let Some(today) = self.learned.sweep_due() else {
            return;
        };
        let more = match self.durable.as_ref().map(|d| d.sweep_estimates(today)) {
            Some(Ok((_, more))) => more,
            Some(Err(error)) => {
                tracing::warn!(%error, "the size summaries could not be swept");
                false
            }
            None => false,
        };
        self.learned.swept(more);
    }
}
