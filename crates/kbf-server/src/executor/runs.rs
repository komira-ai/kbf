//! The tasks of a worker: what a `Start` becomes, how its job is submitted, read back,
//! cancelled and settled.
//!
//! A task is one farm lease. It is `Waiting` from its `Start` until the job service has
//! answered its submit with the job, `Running` while the job is queued, placed, starting or
//! running, `Withdrawing` once the farm cancelled it (or its window ended after a submit whose
//! answer was lost) and until the service answers the cancel, `Ingesting` once the task tool
//! exited 0 and the frontend reads the pack, and `Reporting` from the moment its `Result` is
//! formed until the farm acknowledges it.
//!
//! A `Start` of a `whole_machine` lease takes the same path: it is submitted as a job that
//! needs the whole host ([`Booking::WholeHost`]), and its end is read as any job's is, by the
//! table of [`outcome`](super::outcome) (a host the service could not ready for it is
//! `exclusive-not-ready`, an infrastructure failure the farm reruns).
//!
//! Two guards keep one farm lease to one job. The in-memory task is the first: a `Start` sent
//! again finds its task and changes nothing. The job id ([`job_id`](super::job_id)) is the
//! second: a submit whose answer was lost is sent again with the same id and the same job,
//! which the task keeps, and the service answers from the job the first made. A process that
//! restarts loses its tasks with its farm; the jobs it left are cancelled by the next one
//! (`pool::adopt_jobs`), and a job built again under the same id would differ (a new key and
//! url) and be refused as a conflict, never run twice.

use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt as _;
use js_proto::v1::JobState;
use kbf_proto::reapi::Digest;
use kbf_proto::worker::{ResultAck, Start};
use kbf_sched::fence::START_VALIDITY;
use kbf_types::LeaseId;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::executor::api::{answered, not_found, note};
use crate::executor::host::HostWorker;
use crate::executor::metrics::{IngestEnd, Metrics, RunEnd};
use crate::executor::outcome::{Settled, Verdict, forgotten, judge, ran, refused, result_of};
use crate::executor::output::{Grants, Output, Tail};
use crate::executor::phases::{Mark, Timeline};
use crate::executor::submit::{Booking, Prepared, prepare};
use crate::executor::task::PackKey;
use crate::executor::traits::{Ingested, UntrustedLimits};
use crate::executor::{Ctx, twin_message};

/// One farm lease the worker was told to run.
pub(super) struct Task {
    pub(super) action: Digest,
    pub(super) stage: Stage,
    /// When the job passed each of its moments, on the executor's clock.
    pub(super) timeline: Timeline,
    /// The silence of the host holding its job was seen and counted.
    silence_seen: bool,
}

pub(super) enum Stage {
    /// Told to start, its submit not answered with the job yet.
    Waiting(Waiting),
    /// The job is live on the service.
    Running(Running),
    /// The job of a task nothing is owed for any more: cancelled until the service answers.
    Withdrawing(String),
    /// Exited 0; the frontend is reading its pack.
    Ingesting(JoinHandle<Ingested>),
    /// Has a result the farm has not acknowledged.
    Reporting(Box<Reporting>),
}

#[derive(Clone)]
pub(super) struct Waiting {
    /// Until when the `Start` may be acted on; `None` for one that names a heartbeat the
    /// worker did not send.
    deadline: Option<Instant>,
    /// A share of a host, or the whole host for a `whole_machine` lease.
    booking: Booking,
    /// The job, kept from the first try: a read of the CAS, a url and a grant are not made
    /// again on each pass, and a submit sent again sends the same job.
    prepared: Option<Arc<Prepared>>,
    /// A submit was sent whose answer was lost: the job may exist.
    sent: bool,
}

impl Waiting {
    /// The id of the job that may exist.
    fn sent_job(&self) -> Option<String> {
        let prepared = self.prepared.as_ref().filter(|_| self.sent)?;
        Some(prepared.spec.job_id.clone())
    }
}

pub(super) struct Running {
    pub(super) job: String,
    key: PackKey,
    /// What the job was handed, redacted from its output if it does not end well.
    grants: Arc<Grants>,
}

pub(super) struct Reporting {
    result: kbf_proto::worker::Result,
    in_flight: Option<JoinHandle<Option<ResultAck>>>,
    retry_at: Instant,
}

impl Task {
    pub(super) fn abort(&self) {
        match &self.stage {
            Stage::Ingesting(handle) => handle.abort(),
            Stage::Reporting(report) => report.in_flight.iter().for_each(JoinHandle::abort),
            _ => {}
        }
    }

    /// The id of the job this task may have on the service.
    pub(super) fn job(&self) -> Option<String> {
        match &self.stage {
            Stage::Running(running) => Some(running.job.clone()),
            Stage::Withdrawing(job) => Some(job.clone()),
            Stage::Waiting(waiting) => waiting.sent_job(),
            _ => None,
        }
    }
}

/// What came of trying to submit a task.
enum Submitted {
    /// Not now: try again while the `Start` is valid.
    Wait(Waiting),
    Running(Running),
    Settled(Settled),
    /// The `Start`'s window ended while the task was being prepared or its submit was not
    /// answered: dropped, as one that arrived late is, after the job that may exist is
    /// cancelled.
    Late(Option<String>),
}

impl HostWorker {
    /// A `Start` from the farm. It is acted on only inside the window it names, counted from
    /// the heartbeat it names, as a daemon does: a `Start` that is late is dropped without a
    /// result, and the farm, which no longer lists it, runs the lease again. A window of
    /// zero (a server that names none) is no time at all, the safe way.
    pub(super) fn accept(&mut self, start: &Start) {
        let lease = start.lease_id.unwrap_or_default();
        let window = Duration::from_millis(start.valid_for_ms).min(START_VALIDITY);
        let waiting = Waiting {
            deadline: self.sent_at(start.heartbeat_seq).map(|sent| sent + window),
            booking: Booking::of(start),
            prepared: None,
            sent: false,
        };
        let action = start.action_digest.clone().unwrap_or_default();
        // A `Start` sent again, for a lease that is already known, changes nothing: it must
        // never submit a second job.
        self.tasks
            .entry(LeaseId::new(lease.term, lease.seq))
            .or_insert(Task {
                action,
                stage: Stage::Waiting(waiting),
                timeline: Timeline::default(),
                silence_seen: false,
            });
    }

    /// The farm no longer holds the lease: its job is cancelled, and nothing is owed. A task
    /// that never submitted is dropped without a call.
    pub(super) fn cancel(&mut self, lease: LeaseId) {
        let Some(task) = self.tasks.get(&lease) else {
            return;
        };
        let done = matches!(task.stage, Stage::Ingesting(_) | Stage::Reporting(_));
        match task.job().filter(|_| !done) {
            Some(job) => self.set_stage(lease, Stage::Withdrawing(job)),
            None => self.drop_task(lease),
        }
    }

    /// The farm acknowledged the result of `lease`: it is forgotten.
    pub(super) fn acked(&mut self, lease: LeaseId) {
        self.drop_task(lease);
    }

    fn drop_task(&mut self, lease: LeaseId) {
        self.tasks.remove(&lease).iter().for_each(Task::abort);
    }

    /// The leases a heartbeat lists: those running, and those whose result the farm has
    /// not acknowledged. A `Start` not yet submitted is not listed, as a daemon lists only what
    /// it started, and nor is a job being withdrawn, which the farm no longer runs here.
    pub(super) fn listed(&self) -> Vec<LeaseId> {
        self.tasks
            .iter()
            .filter(|(_, task)| !matches!(task.stage, Stage::Waiting(_) | Stage::Withdrawing(_)))
            .map(|(lease, _)| *lease)
            .collect()
    }

    /// Whether a job of this worker may be live on the service: the calls about it are what
    /// the self-fence counts the service's silence by.
    pub(super) fn holds_jobs(&self) -> bool {
        self.tasks.values().any(|task| task.job().is_some())
    }

    /// Submits every waiting task the job service will take.
    pub(super) async fn launch(&mut self, ctx: &Ctx<'_>, now: Instant) {
        let waiting: Vec<_> = self
            .tasks
            .iter()
            .filter_map(|(lease, task)| match &task.stage {
                Stage::Waiting(w) => Some((*lease, task.action.clone(), w.clone())),
                _ => None,
            })
            .collect();
        for (lease, action, w) in waiting {
            // Read again for each task: the ones before it took time.
            let at = ctx.now();
            // The heartbeats and the fence come after this, in the same pass: it starts no more
            // once its budget is spent, and the tasks left are taken up by the next.
            if at.saturating_duration_since(now) >= ctx.cfg.pass_budget {
                break;
            }
            let Some(deadline) = w.deadline.filter(|deadline| at < *deadline) else {
                tracing::warn!(worker = %self.id, %lease, "a Start outside its window is dropped");
                self.late(ctx, lease, w.sent_job());
                continue;
            };
            self.stamp(lease, Mark::Seen, at, ctx);
            match self.submit(ctx, lease, &action, w, deadline).await {
                Submitted::Wait(w) => self.set_stage(lease, Stage::Waiting(w)),
                Submitted::Running(running) => {
                    self.stamp(lease, Mark::Posted, ctx.now(), ctx);
                    self.set_stage(lease, Stage::Running(running));
                }
                Submitted::Settled(settled) => {
                    self.settle_with(lease, &settled, now, &ctx.deps.metrics);
                }
                Submitted::Late(job) => {
                    tracing::warn!(worker = %self.id, %lease, "a Start ran out of its window before its job was submitted and is dropped");
                    self.late(ctx, lease, job);
                }
            }
        }
    }

    /// Drops a `Start` whose window ended: at once if no job of it may exist, else once the
    /// job is cancelled.
    fn late(&mut self, ctx: &Ctx<'_>, lease: LeaseId, job: Option<String>) {
        ctx.deps.metrics.late_start();
        match job {
            Some(job) => self.set_stage(lease, Stage::Withdrawing(job)),
            None => self.drop_task(lease),
        }
    }

    /// Stamps `mark` of `lease`'s job at `at`: the first stamp of each is kept.
    fn stamp(&mut self, lease: LeaseId, mark: Mark, at: Instant, ctx: &Ctx<'_>) {
        let metrics = &ctx.deps.metrics;
        self.tasks
            .entry(lease)
            .and_modify(|task| task.timeline.stamp(mark, at, metrics));
    }

    fn set_stage(&mut self, lease: LeaseId, stage: Stage) {
        self.tasks
            .entry(lease)
            .and_modify(|task| task.stage = stage);
    }

    /// Forms the result of `lease` from `settled`, to be reported.
    fn settle_with(&mut self, lease: LeaseId, settled: &Settled, now: Instant, metrics: &Metrics) {
        self.tasks.entry(lease).and_modify(|task| {
            let result = result_of(lease, &task.action, settled);
            metrics.run_ended(RunEnd::of(&result));
            task.stage = reporting(result, now);
        });
    }

    async fn submit(
        &mut self,
        ctx: &Ctx<'_>,
        lease: LeaseId,
        action: &Digest,
        w: Waiting,
        deadline: Instant,
    ) -> Submitted {
        // What preparing reads (the CAS, the store, the grant service) is not the job
        // service's and is not bounded by it: it is bounded by the window, which it cannot
        // outlast and have the submit still be good.
        let prepared = match &w.prepared {
            Some(prepared) => Arc::clone(prepared),
            None => {
                let preparing = prepare(ctx, lease, action, w.booking);
                match tokio::time::timeout_at(deadline, preparing).await {
                    Ok(Ok(prepared)) => {
                        self.stamp(lease, Mark::Prepared, ctx.now(), ctx);
                        prepared
                    }
                    Ok(Err(settled)) => return Submitted::Settled(settled),
                    Err(_) => return Submitted::Late(None),
                }
            }
        };
        // The CAS read, the url and the grant each took as long as the store took: the window
        // is judged by the clock now, the last moment before the submit.
        let now = ctx.now();
        if now >= deadline {
            return Submitted::Late(w.sent_job());
        }
        if w.sent {
            ctx.deps.metrics.job_resubmitted();
        }
        let answer = ctx.api().submit_job(prepared.spec.clone()).await;
        self.heard(ctx, &answer);
        match answer {
            Ok(job) => {
                ctx.deps.metrics.job_submitted();
                Submitted::Running(Running {
                    job: job.id,
                    key: prepared.key.clone(),
                    grants: Arc::new(prepared.grants.clone()),
                })
            }
            Err(error) => match refused(&error) {
                None => {
                    tracing::warn!(worker = %self.id, %lease, %error, "the submit was not answered; it is sent again");
                    Submitted::Wait(Waiting {
                        sent: w.sent || error.may_have_applied(),
                        prepared: Some(prepared),
                        ..w
                    })
                }
                Some(refusal) => {
                    if refusal.twin {
                        ctx.twin.flag(twin_message(&ctx.cfg.frontend));
                    }
                    Submitted::Settled(refusal.settled)
                }
            },
        }
    }

    /// Reads the job of each running task and moves on the ones that ended, and cancels the
    /// jobs of the tasks being withdrawn. Each call is bounded, and the reads stop once the
    /// pass budget is spent from the first of them, so a service that never answers holds the
    /// pass for a budget and a call, not a call per job; the next pass begins after the last
    /// task read, so every task is read in turn.
    pub(super) async fn poll(&mut self, ctx: &Ctx<'_>, now: Instant) {
        // The action and the key of a running task, for the ingest if its job ended well, and
        // its grants, for its output if it did not.
        type Read = Option<(Digest, PackKey, Arc<Grants>)>;
        let mut jobs: Vec<(LeaseId, String, Read)> = self
            .tasks
            .iter()
            .filter_map(|(lease, task)| match &task.stage {
                Stage::Running(r) => {
                    let read = Some((task.action.clone(), r.key.clone(), Arc::clone(&r.grants)));
                    Some((*lease, r.job.clone(), read))
                }
                Stage::Withdrawing(job) => Some((*lease, job.clone(), None)),
                _ => None,
            })
            .collect();
        let after = self.read_after;
        jobs.sort_by_key(|(lease, _, _)| (after.is_some_and(|last| *lease <= last), *lease));
        let began = ctx.now();
        for (lease, job, read) in jobs {
            if ctx.now().saturating_duration_since(began) >= ctx.cfg.pass_budget {
                break;
            }
            self.read_after = Some(lease);
            match read {
                Some(ingest) => self.read(ctx, now, lease, &job, ingest).await,
                None => self.withdraw(ctx, lease, &job).await,
            }
        }
    }

    /// Cancels the job of a task nothing is owed for, and forgets the task once the service
    /// has answered: an ended job and one the service does not know are answers too.
    async fn withdraw(&mut self, ctx: &Ctx<'_>, lease: LeaseId, job: &str) {
        let answer = ctx.api().cancel_job(job).await;
        self.heard(ctx, &answer);
        note("cancelling a job", &answer);
        if answer.is_ok() {
            ctx.deps.metrics.job_cancelled();
        }
        if answered(&answer) {
            self.drop_task(lease);
        }
    }

    /// Reads one running task's job, and its result once it has ended; the output of one that
    /// did not end well goes with its `Result` ([`output`](super::output)).
    async fn read(
        &mut self,
        ctx: &Ctx<'_>,
        now: Instant,
        lease: LeaseId,
        id: &str,
        (action, key, grants): (Digest, PackKey, Arc<Grants>),
    ) {
        let answer = ctx.api().get_job(id).await;
        self.heard(ctx, &answer);
        let job = match answer {
            Ok(job) => job,
            Err(error) if not_found(&error) => {
                tracing::warn!(worker = %self.id, %lease, job = %id, "the job service no longer knows the job");
                self.settle_with(lease, &forgotten(), now, &ctx.deps.metrics);
                return;
            }
            Err(error) => {
                tracing::warn!(worker = %self.id, %lease, %error, "reading the job failed");
                return;
            }
        };
        if job.host_silent_since_ms.is_some() {
            self.saw_silence(ctx, lease, &job.host_id);
        }
        let live = match job.state() {
            JobState::Queued | JobState::Placed | JobState::Starting => true,
            JobState::Running => {
                self.stamp(lease, Mark::Running, now, ctx);
                true
            }
            // A state this server does not know may be live or ended: the result says which.
            _ => false,
        };
        if live {
            return;
        }
        let result = ctx.api().get_result(id).await;
        self.heard(ctx, &result);
        let result = match result {
            Ok(result) => result,
            Err(error) if not_found(&error) => {
                let output = self.output(ctx, lease, id, None, &grants).await;
                self.settle_failed(lease, &forgotten(), &output, now, &ctx.deps.metrics);
                return;
            }
            Err(error) => {
                tracing::warn!(worker = %self.id, %lease, %error, "reading the result of a job failed");
                return;
            }
        };
        self.stamp(lease, Mark::Ended, now, ctx);
        match judge(&job, &result) {
            Verdict::Settled(settled) => {
                let output = self.output(ctx, lease, id, Some(&result), &grants).await;
                self.settle_failed(lease, &settled, &output, now, &ctx.deps.metrics);
            }
            Verdict::Ingest => self.ingest(ctx, lease, action, key),
        }
    }

    /// The output of job `id`, which did not end well: the tails of its `result` if they hold
    /// any, else one `GetLogs`; redacted of `grants` and cut to the cap. Only the lengths are
    /// logged.
    async fn output(
        &mut self,
        ctx: &Ctx<'_>,
        lease: LeaseId,
        id: &str,
        result: Option<&js_proto::v1::JobResult>,
        grants: &Grants,
    ) -> Output {
        let tail = match result.and_then(Tail::of_result) {
            Some(tail) => Ok(tail),
            None => {
                let logs = ctx.api().logs_tail(id).await;
                self.heard(ctx, &logs);
                logs
            }
        };
        let output = match tail {
            Ok(tail) => Output::of(&tail, grants),
            Err(error) => Output::unread(&error),
        };
        tracing::info!(
            worker = %self.id,
            %lease,
            stdout_bytes = output.stdout.len(),
            stderr_bytes = output.stderr.len(),
            truncated = output.truncated,
            unread = output.note.is_some(),
            "the output of a job that did not end well goes with its result"
        );
        output
    }

    /// Forms the result of `lease` from `settled`, with the output of its job.
    fn settle_failed(
        &mut self,
        lease: LeaseId,
        settled: &Settled,
        output: &Output,
        now: Instant,
        metrics: &Metrics,
    ) {
        self.tasks.entry(lease).and_modify(|task| {
            let mut result = result_of(lease, &task.action, settled);
            output.attach(&mut result);
            metrics.run_ended(RunEnd::of(&result));
            task.stage = reporting(result, now);
        });
    }

    /// Logs and counts, once a task, that the host holding its job has missed its grace.
    /// The service ends the job as lost if the host does not come back; nothing changes here.
    fn saw_silence(&mut self, ctx: &Ctx<'_>, lease: LeaseId, host: &str) {
        let worker = &self.id;
        self.tasks.entry(lease).and_modify(|task| {
            if !std::mem::replace(&mut task.silence_seen, true) {
                tracing::warn!(%worker, %lease, %host, "the host of a job has missed its grace");
                ctx.deps.metrics.host_silence();
            }
        });
    }

    /// Starts the frontend's read of the pack of a job that ended well.
    fn ingest(&mut self, ctx: &Ctx<'_>, lease: LeaseId, action: Digest, key: PackKey) {
        let ingest = Arc::clone(&ctx.deps.ingest);
        let limits = UntrustedLimits {
            max_pack_bytes: ctx.cfg.max_pack_bytes,
            max_blob_bytes: ctx.cfg.max_blob_bytes,
        };
        let handle =
            tokio::spawn(async move { ingest.ingest(lease, &key, &action, &limits).await });
        self.set_stage(lease, Stage::Ingesting(handle));
    }

    /// Moves ingested tasks to their result, and sends or resends the results the farm has
    /// not acknowledged.
    pub(super) fn settle(&mut self, ctx: &Ctx<'_>, now: Instant) {
        let worker = self.id.clone();
        let mut acked = Vec::new();
        for (lease, task) in &mut self.tasks {
            match &mut task.stage {
                Stage::Ingesting(handle) => {
                    let next = handle.now_or_never().map(|done| {
                        task.timeline.stamp(Mark::Done, now, &ctx.deps.metrics);
                        let (result, end) = ingested(*lease, &task.action, done);
                        ctx.deps.metrics.ingested(end);
                        ctx.deps.metrics.run_ended(RunEnd::of(&result));
                        reporting(result, now)
                    });
                    next.into_iter().for_each(|stage| task.stage = stage);
                }
                Stage::Reporting(report) => {
                    let done = report.drive(&ctx.deps.farm, &worker, now, ctx.cfg.heartbeat);
                    done.then_some(*lease).iter().for_each(|l| acked.push(*l));
                }
                _ => {}
            }
        }
        acked.into_iter().for_each(|lease| self.drop_task(lease));
    }

    /// Cancels every job the worker may have, once each, as far as the service can be
    /// reached: a cancel that is not answered is not sent again, and its job runs to its
    /// wall limit.
    pub(super) async fn cancel_all(&self, ctx: &Ctx<'_>) {
        let jobs: Vec<String> = self.tasks.values().filter_map(Task::job).collect();
        for job in jobs {
            let answer = ctx.api().cancel_job(&job).await;
            note("cancelling a job", &answer);
            if answer.is_ok() {
                ctx.deps.metrics.job_cancelled();
            }
        }
    }
}

impl Reporting {
    /// Collects the answer of the report in flight, and sends the result again when none is
    /// in flight and the last send was long enough ago. True once the farm acknowledged.
    fn drive(
        &mut self,
        farm: &Arc<crate::Farm>,
        worker: &kbf_types::WorkerId,
        now: Instant,
        resend: Duration,
    ) -> bool {
        let done = self
            .in_flight
            .as_mut()
            .and_then(|handle| handle.now_or_never());
        self.in_flight = self.in_flight.take().filter(|_| done.is_none());
        let acked = matches!(done, Some(Ok(Some(_))));
        let due = [self.in_flight.is_none(), now >= self.retry_at, !acked]
            .into_iter()
            .all(|b| b);
        if due {
            self.retry_at = now + resend;
            let (farm, worker, result) = (Arc::clone(farm), worker.clone(), self.result.clone());
            self.in_flight = Some(tokio::spawn(
                async move { farm.report(&worker, result).await },
            ));
        }
        acked
    }
}

fn reporting(result: kbf_proto::worker::Result, now: Instant) -> Stage {
    Stage::Reporting(Box::new(Reporting {
        result,
        in_flight: None,
        retry_at: now,
    }))
}

fn ingested(
    lease: LeaseId,
    action: &Digest,
    done: Result<Ingested, tokio::task::JoinError>,
) -> (kbf_proto::worker::Result, IngestEnd) {
    let infra = |why: String| result_of(lease, action, &Settled::Infra(why));
    match done {
        Ok(Ingested::Ran { result, .. }) => (ran(lease, action, *result), IngestEnd::Ok),
        Ok(Ingested::Reject(why)) => (
            infra(format!("the pack was refused: {why}")),
            IngestEnd::Rejected(why),
        ),
        Ok(Ingested::Unavailable(why)) => (
            infra(format!("the pack could not be read: {why}")),
            IngestEnd::Unavailable,
        ),
        Err(e) => (
            infra(format!("the ingest stopped: {e}")),
            IngestEnd::Stopped,
        ),
    }
}

#[cfg(test)]
#[path = "runs_tests.rs"]
mod tests;
