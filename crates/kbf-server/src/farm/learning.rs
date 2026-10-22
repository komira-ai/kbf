//! The farm's side of learned sizes in shadow ([`crate::learned`]): what it hands the
//! estimator from the places a lease starts, a result is reported, a lease is given up
//! and an operation is submitted. Every method returns at once while the estimator is
//! off.

use kbf_estimator::Bounds;
use kbf_invocation::Evidence;
use kbf_sched::{Request, Requeue, RequeueReason};
use kbf_types::{LeaseId, LeaseKind, OperationId, Outcome, WorkerId};

use super::State;
use crate::action_usage::Measured;
use crate::learned::{Attempt, Row, Skip, basis_of_os, hint_of, key_of, run_of};
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
        let Some((basis, run)) = run_of(outcome, usage.as_ref(), booked) else {
            self.learned.stage(lease, None);
            return;
        };
        match key_of(&instance, &platform, basis, &subject) {
            Some(key) => self.learned.stage(lease, Some(Attempt { key, run })),
            None => self.learned.skip(Skip::NoOs),
        }
    }

    /// The scheduler has answered the report of `lease`, run by `worker`; `taken`: it
    /// took it.
    pub(super) fn finish_learning(&mut self, lease: LeaseId, worker: &WorkerId, taken: bool) {
        if self.learned.off() {
            return;
        }
        let bounds = self.bounds_on(worker);
        self.learned.finish(lease, taken, &bounds);
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
            .and_then(|basis| key_of(&instance, &platform, basis, &subject));
        let Some(key) = key else {
            self.learned.skip(Skip::NoOs);
            return;
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

    /// Writes behind the summaries changed since they were last written, and sweeps the
    /// stale ones from the store once an hour.
    pub(super) fn flush_learned(&mut self) {
        if self.learned.off() || self.fenced {
            return;
        }
        let rows = self.learned.due();
        if !rows.is_empty() && !self.durably("size summaries", |d| d.put_estimates(&rows)) {
            self.learned.unwritten(&rows);
        }
        if let Some(today) = self.learned.sweep_due() {
            self.durably("a size sweep", |d| d.sweep_estimates(today).map(drop));
        }
    }
}
