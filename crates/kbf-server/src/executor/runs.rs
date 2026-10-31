//! The runs of a worker: what a `Start` becomes, how it is posted, read back and settled.
//!
//! A task is one farm lease. It is `Waiting` from its `Start` until the lease server accepts
//! it, `Running` while the host has it, `Ingesting` once the task tool exited 0 and the
//! frontend reads the pack, and `Reporting` from the moment its `Result` is formed until
//! the farm acknowledges it.

use std::collections::BTreeMap;
use std::ops::ControlFlow;
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt as _;
use kbf_lease_client::{Error, ErrorCode, Run, RunId, RunLimits, RunSpec, RunState};
use kbf_proto::reapi::Digest;
use kbf_proto::worker::{Lost, ResultAck, Start};
use kbf_sched::fence::START_VALIDITY;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use crate::executor::config::Config;
use crate::executor::host::{Drain, HostWorker, Retire};
use crate::executor::metrics::{IngestEnd, Metrics, RunEnd};
use crate::executor::outcome::{Settled, Verdict, judge, ran, refused_for_quota, result_of};
use crate::executor::phases::{Mark, Timeline};
use crate::executor::task::{Nonce, PackKey, TaskArgs};
use crate::executor::traits::{Ingested, InputsError, MintError, PresignError, UntrustedLimits};
use crate::executor::{Ctx, note};

/// The label that names a run by the nonce drawn for its `POST`, so a run whose `202` was
/// lost can be found again.
pub(super) const RUN_LABEL: &str = "kbf-run";

/// One farm lease the worker was told to run.
pub(super) struct Task {
    pub(super) action: Digest,
    pub(super) stage: Stage,
    /// When the run passed each of its moments, on the executor's clock.
    pub(super) timeline: Timeline,
}

pub(super) enum Stage {
    /// Told to start, not accepted by the lease server yet.
    Waiting(Waiting),
    /// Queued or running on the host.
    Running(Running),
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
    millicpus: u64,
    memory_bytes: u64,
    /// What the run is made of, kept while the lease server cannot take it yet: a read of the
    /// CAS, a url and a grant are not made again on each pass of the up to 14 s it may wait.
    prepared: Option<Arc<Prepared>>,
}

/// A run ready to post.
pub(super) struct Prepared {
    spec: RunSpec,
    key: PackKey,
    nonce: Nonce,
    wall: Duration,
}

pub(super) struct Running {
    run: RunId,
    key: PackKey,
    /// The lease cut the run's wall time short of what was asked.
    truncated: bool,
    /// The farm cancelled the lease: the run is stopped and forgotten.
    cancelled: bool,
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
}

/// What came of trying to start a task.
enum Queued {
    /// Not now: try again while the `Start` is valid, with the run as it was prepared.
    Wait(Arc<Prepared>),
    Posted(Running),
    Settled(Settled),
    Retire(Retire),
    /// The `Start`'s window ended while the task was being prepared: dropped, as one that
    /// arrived late is.
    Late,
}

impl From<InputsError> for Settled {
    fn from(error: InputsError) -> Self {
        match error {
            InputsError::Invalid(why) => Self::Invalid(why),
            InputsError::Unavailable(why) => Self::Infra(why),
        }
    }
}

impl From<PresignError> for Settled {
    fn from(error: PresignError) -> Self {
        let (PresignError::Refused(why) | PresignError::Unavailable(why)) = error;
        Self::Infra(format!("no url for the pack: {why}"))
    }
}

/// `image` as the lease server takes it, if it is pinned by digest: a name, then
/// `@sha256:` and 64 hex digits. The `docker://` scheme the platform property carries is
/// dropped.
fn pinned(image: &str) -> Result<String, Settled> {
    let image = image.strip_prefix("docker://").unwrap_or(image);
    let digest = image.rsplit_once("@sha256:").map(|(_, digest)| digest);
    let pinned = digest.is_some_and(|d| d.len() == 64 && d.bytes().all(|b| b.is_ascii_hexdigit()));
    match pinned {
        true => Ok(image.to_owned()),
        false => Err(Settled::Invalid(format!(
            "the action's image {image:?} is not pinned by an @sha256 digest"
        ))),
    }
}

fn require(ok: bool, why: impl FnOnce() -> String) -> Result<(), Settled> {
    match ok {
        true => Ok(()),
        false => Err(Settled::Invalid(why())),
    }
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
            millicpus: start.millicpus,
            memory_bytes: start.memory_bytes,
            prepared: None,
        };
        let action = start.action_digest.clone().unwrap_or_default();
        // A `Start` sent again, for a lease that is already known, changes nothing: it must
        // never post a second run.
        self.tasks
            .entry(kbf_types::LeaseId::new(lease.term, lease.seq))
            .or_insert(Task {
                action,
                stage: Stage::Waiting(waiting),
                timeline: Timeline::default(),
            });
    }

    /// The farm no longer holds the lease: whatever is stopped, and nothing is owed.
    pub(super) fn cancel(&mut self, lease: kbf_types::LeaseId) {
        match self.tasks.get_mut(&lease).map(|task| &mut task.stage) {
            Some(Stage::Running(running)) => running.cancelled = true,
            _ => self.drop_task(lease),
        }
    }

    /// The farm acknowledged the result of `lease`: it is forgotten.
    pub(super) fn acked(&mut self, lease: kbf_types::LeaseId) {
        self.drop_task(lease);
    }

    fn drop_task(&mut self, lease: kbf_types::LeaseId) {
        self.tasks.remove(&lease).iter().for_each(Task::abort);
    }

    /// The leases a heartbeat lists: those running, and those whose result the farm has
    /// not acknowledged. A `Start` not yet accepted is not listed, as a daemon lists only what
    /// it started.
    pub(super) fn listed(&self) -> Vec<kbf_types::LeaseId> {
        self.tasks
            .iter()
            .filter(|(_, task)| !matches!(task.stage, Stage::Waiting(_)))
            .map(|(lease, _)| *lease)
            .collect()
    }

    /// Posts every waiting task the lease server will take.
    pub(super) async fn launch(&mut self, ctx: &Ctx<'_>, now: Instant) -> ControlFlow<Retire> {
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
            let at = ctx.clock.now();
            // The heartbeats and the fence come after this, in the same pass: it starts no more
            // once its budget is spent, and the tasks left are taken up by the next.
            if at.saturating_duration_since(now) >= ctx.cfg.pass_budget {
                break;
            }
            let Some(deadline) = w.deadline.filter(|deadline| at < *deadline) else {
                tracing::warn!(worker = %self.id, %lease, "a Start outside its window is dropped");
                ctx.deps.metrics.late_start();
                self.drop_task(lease);
                continue;
            };
            self.stamp(lease, Mark::Seen, at, ctx);
            match self.queue(ctx, lease, &action, &w, deadline).await {
                Queued::Wait(prepared) => {
                    let w = Waiting {
                        prepared: Some(prepared),
                        ..w
                    };
                    self.set_stage(lease, Stage::Waiting(w));
                }
                Queued::Posted(running) => {
                    self.stamp(lease, Mark::Posted, ctx.clock.now(), ctx);
                    self.set_stage(lease, Stage::Running(running));
                }
                Queued::Settled(settled) => {
                    self.settle_with(lease, &settled, now, &ctx.deps.metrics);
                }
                Queued::Retire(why) => return ControlFlow::Break(why),
                Queued::Late => {
                    tracing::warn!(worker = %self.id, %lease, "a Start ran out of its window while it was prepared and is dropped");
                    ctx.deps.metrics.late_start();
                    self.drop_task(lease);
                }
            }
        }
        ControlFlow::Continue(())
    }

    /// Stamps `mark` of `lease`'s run at `at`: the first stamp of each is kept.
    fn stamp(&mut self, lease: kbf_types::LeaseId, mark: Mark, at: Instant, ctx: &Ctx<'_>) {
        if let Some(task) = self.tasks.get_mut(&lease) {
            task.timeline.stamp(mark, at, &ctx.deps.metrics);
        }
    }

    fn set_stage(&mut self, lease: kbf_types::LeaseId, stage: Stage) {
        self.tasks
            .entry(lease)
            .and_modify(|task| task.stage = stage);
    }

    /// Forms the result of `lease` from `settled`, to be reported.
    fn settle_with(
        &mut self,
        lease: kbf_types::LeaseId,
        settled: &Settled,
        now: Instant,
        metrics: &Metrics,
    ) {
        let action = self.tasks[&lease].action.clone();
        let result = result_of(lease, &action, settled);
        metrics.run_ended(RunEnd::of(&result));
        self.set_stage(lease, reporting(result, now));
    }

    async fn queue(
        &mut self,
        ctx: &Ctx<'_>,
        lease: kbf_types::LeaseId,
        action: &Digest,
        w: &Waiting,
        deadline: Instant,
    ) -> Queued {
        // What preparing reads (the CAS, the store, the grant service) is not the lease
        // server's and is not bounded by it: it is bounded by the window, which it cannot
        // outlast and have the post still be good.
        let prepared = match &w.prepared {
            Some(prepared) => Arc::clone(prepared),
            None => {
                let preparing =
                    tokio::time::timeout_at(deadline, self.prepare(ctx, lease, action, w));
                match preparing.await {
                    Ok(Ok(prepared)) => {
                        self.stamp(lease, Mark::Prepared, ctx.clock.now(), ctx);
                        Arc::new(prepared)
                    }
                    Ok(Err(settled)) => return Queued::Settled(settled),
                    Err(_) => return Queued::Late,
                }
            }
        };
        let wall = prepared.wall;
        // The lease server cuts the run's wall time to what the lease has left when it is
        // posted, and a renewal later does not stretch it: renew first when the lease would
        // end before the run does.
        let now = ctx.clock.now();
        let renewed = self.renew(ctx, now, self.left(now) < wall).await;
        if let ControlFlow::Break(why) = renewed {
            return Queued::Retire(why);
        }
        // The CAS read, the url, the grant and the renewal each took as long as the lease
        // server or the store took: the window is judged by the clock now, the last moment
        // before the post, not by the one read when the pass began.
        let now = ctx.clock.now();
        if now >= deadline {
            return Queued::Late;
        }
        let truncated = wall > self.left(now);
        let posted = ctx.api().queue_run(self.lease, &prepared.spec).await;
        self.heard(now, &posted);
        let label = prepared.nonce.to_string();
        let running = |run: RunId| Running {
            run,
            key: prepared.key.clone(),
            truncated,
            cancelled: false,
        };
        match posted {
            Ok(accepted) => Queued::Posted(running(accepted.run_id)),
            Err(e) if e.may_have_applied() => self.recover(ctx, &label, running).await,
            Err(e) => self.refused(ctx, &e, prepared),
        }
    }

    /// How long the lease lasts from `now`: until it expires unless renewed, or ends.
    fn left(&self, now: Instant) -> Duration {
        self.expires_at
            .min(self.ends_at)
            .saturating_duration_since(now)
    }

    /// The run, its key, and the wall time it asks for.
    async fn prepare(
        &self,
        ctx: &Ctx<'_>,
        lease: kbf_types::LeaseId,
        action: &Digest,
        w: &Waiting,
    ) -> Result<Prepared, Settled> {
        let facts = ctx.deps.inputs.action(action).await?;
        let timeout = facts.timeout.unwrap_or(ctx.cfg.default_timeout);
        require(timeout <= ctx.cfg.max_run, || {
            format!("the action's timeout {timeout:?} is over the executor's limit")
        })?;
        let image = pinned(&facts.image)?;
        let wall = ctx.cfg.wall(timeout);
        let nonce = Nonce::random();
        let key = ctx
            .deps
            .ingest
            .mint(lease, action, wall)
            .await
            .map_err(|e| match e {
                MintError::Refused(why) => Settled::Invalid(format!("no key for the pack: {why}")),
                MintError::Unavailable(why) => {
                    Settled::Infra(format!("no key for the pack: {why}"))
                }
            })?;
        let pack = ctx
            .deps
            .presign
            .put_url(&self.host, &key, wall, ctx.cfg.max_pack_bytes)
            .await?;
        // The signed url is the task's write permission: sent in clear only if the operator
        // said the store does not serve TLS yet.
        if !ctx.cfg.allow_plain_http_put && !pack.expose().starts_with("https://") {
            return Err(Settled::Infra("the pack url is not https".to_owned()));
        }
        let grant = ctx.deps.inputs.read_grant(lease, action, wall).await?;
        // The bearer token goes into the arguments: not over a scheme that sends it in clear,
        // unless the operator said the job network is trusted for it.
        if !ctx.cfg.allow_plain_http_grant && !grant.url.starts_with("https://") {
            return Err(Settled::Infra(
                "the read grant is not served over https".to_owned(),
            ));
        }
        let args = TaskArgs {
            action,
            lease,
            grant_url: &grant.url,
            grant_token: &grant.token,
            pack_url: &pack,
            deadline: timeout,
            work_dir: ctx.cfg.work_dir.as_deref(),
            allow_plain_http_put: ctx.cfg.allow_plain_http_put,
            allow_plain_http_grant: ctx.cfg.allow_plain_http_grant,
            locate: ctx.cfg.locate,
        };
        let limits = RunLimits {
            cpu_millis: w.millicpus.max(1),
            memory_bytes: Config::hard_memory(w.memory_bytes),
            pids: None,
            wall_ms: Some(u64::try_from(wall.as_millis()).unwrap_or(u64::MAX)),
            disk_bytes: Some(ctx.cfg.disk_bytes(grant.input_bytes)),
        };
        if grant.input_bytes.is_none() {
            tracing::warn!(
                worker = %self.id,
                %lease,
                "the size of the inputs is not known; the run books the allowance and the margin only"
            );
        }
        let mut spec = RunSpec::new(image, limits);
        spec.args = args.argv(ctx.cfg.delivery);
        // The labels are listed back with every run and are not secret-safe: the nonce only.
        spec.labels = BTreeMap::from([(RUN_LABEL.to_owned(), nonce.to_string())]);
        Ok(Prepared {
            spec,
            key,
            nonce,
            wall,
        })
    }

    /// The post's answer was lost: the run may be queued. Found by its label, it is the
    /// run; not found, the worker cannot know, and is taken down with its lease.
    async fn recover(
        &self,
        ctx: &Ctx<'_>,
        label: &str,
        running: impl FnOnce(RunId) -> Running,
    ) -> Queued {
        let found = ctx.api().runs(self.lease).await.ok().and_then(|runs| {
            runs.into_iter()
                .find(|run| run.labels.get(RUN_LABEL).is_some_and(|v| v == label))
        });
        match found {
            Some(run) => Queued::Posted(running(run.run_id)),
            None => Queued::Retire(Retire::Ambiguous),
        }
    }

    /// The lease server refused the run: nothing was queued.
    fn refused(&mut self, ctx: &Ctx<'_>, e: &Error, prepared: Arc<Prepared>) -> Queued {
        match e.api().map(|api| &api.code) {
            None | Some(ErrorCode::NoCapacity | ErrorCode::NotReady) => Queued::Wait(prepared),
            Some(ErrorCode::HostTainted) => {
                self.begin_drain(ctx, Drain::Tainted);
                Queued::Settled(Settled::Infra("the host is tainted".to_owned()))
            }
            Some(ErrorCode::QuotaUnavailable) => {
                self.quota_unavailable(ctx, ctx.clock.now());
                Queued::Settled(Settled::Lost(Lost::Fenced))
            }
            Some(ErrorCode::HostLost | ErrorCode::HostNotRegistered) => {
                Queued::Retire(Retire::Lost)
            }
            Some(ErrorCode::NotHeld | ErrorCode::UnknownLease | ErrorCode::Expired) => {
                Queued::Retire(Retire::Gone)
            }
            // Every host holds the pool's shape, so a run over it fits no host and rerunning
            // the action never helps: the action's limits are the caller's to fix.
            Some(ErrorCode::LimitsExceedShape) => {
                let limits = &prepared.spec.limits;
                let disk = limits
                    .disk_bytes
                    .map_or_else(|| "none".to_owned(), |bytes| format!("{bytes} bytes"));
                Queued::Settled(Settled::Invalid(format!(
                    "the run's limits (cpu {} millis, memory {} bytes, disk {disk}) exceed the \
                     pool's shape (cpu {} millis, memory {} bytes), which every host shares",
                    limits.cpu_millis,
                    limits.memory_bytes,
                    ctx.cfg.cpu_millis,
                    ctx.cfg.memory_bytes,
                )))
            }
            Some(other) => Queued::Settled(Settled::Infra(format!(
                "the lease server refused the run: {other}"
            ))),
        }
    }

    /// Reads the host's runs and acts on each running task: stop the ones the farm cancelled,
    /// and take the verdict on those that ended.
    pub(super) async fn poll(&mut self, ctx: &Ctx<'_>, now: Instant) -> ControlFlow<Retire> {
        let listed = ctx.api().runs(self.lease).await;
        self.heard(now, &listed);
        let runs = match listed {
            Ok(runs) => runs,
            Err(e) if crate::executor::lease_gone(&e) => return ControlFlow::Break(Retire::Gone),
            Err(e) => {
                tracing::warn!(worker = %self.id, error = %e, "reading the runs failed");
                return ControlFlow::Continue(());
            }
        };
        let running: Vec<_> = self
            .tasks
            .iter()
            .filter_map(|(lease, task)| match &task.stage {
                Stage::Running(r) => Some((
                    *lease,
                    r.run.clone(),
                    r.cancelled,
                    r.truncated,
                    r.key.clone(),
                )),
                _ => None,
            })
            .collect();
        for (lease, run_id, cancelled, truncated, key) in running {
            match cancelled {
                true => self.stop(ctx, lease, &run_id).await,
                false => {
                    let seen = runs.iter().find(|run| run.run_id == run_id).cloned();
                    let run = seen.unwrap_or_else(|| absent(run_id));
                    self.take_verdict(ctx, now, lease, &run, truncated, key);
                }
            }
        }
        ControlFlow::Continue(())
    }

    /// Asks the lease server to stop a run the farm cancelled, and forgets it once the server
    /// has answered: a server that knows no such run has nothing to stop.
    async fn stop(&mut self, ctx: &Ctx<'_>, lease: kbf_types::LeaseId, run: &RunId) {
        let stopped = ctx.api().stop_run(self.lease, run).await;
        note("stopping a run", &stopped);
        let answered = stopped.as_ref().err().is_none_or(|e| e.api().is_some());
        answered
            .then_some(lease)
            .iter()
            .for_each(|l| self.drop_task(*l));
    }

    /// Moves a running task on if its run ended.
    fn take_verdict(
        &mut self,
        ctx: &Ctx<'_>,
        now: Instant,
        lease: kbf_types::LeaseId,
        run: &Run,
        truncated: bool,
        key: PackKey,
    ) {
        if run.state == RunState::Running {
            self.stamp(lease, Mark::Running, now, ctx);
        }
        let exited = matches!(run.state, RunState::Exited | RunState::Ended);
        if exited {
            self.stamp(lease, Mark::Ended, now, ctx);
        }
        let ended = exited || run.state == RunState::NeverStarted;
        if !ended {
            return;
        }
        if refused_for_quota(run) {
            self.quota_unavailable(ctx, now);
        }
        match judge(run, truncated) {
            Verdict::Settled(settled) => {
                self.settle_with(lease, &settled, now, &ctx.deps.metrics);
            }
            Verdict::Ingest => {
                let action = self.tasks[&lease].action.clone();
                let ingest = std::sync::Arc::clone(&ctx.deps.ingest);
                let limits = UntrustedLimits {
                    max_pack_bytes: ctx.cfg.max_pack_bytes,
                    max_blob_bytes: ctx.cfg.max_blob_bytes,
                };
                let handle =
                    tokio::spawn(async move { ingest.ingest(lease, &key, &action, &limits).await });
                self.set_stage(lease, Stage::Ingesting(handle));
            }
        }
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

    /// Asks the lease server to stop every run the worker has, as far as it can be reached.
    pub(super) async fn stop_all(&self, ctx: &Ctx<'_>) {
        let runs: Vec<RunId> = self
            .tasks
            .values()
            .filter_map(|task| match &task.stage {
                Stage::Running(r) => Some(r.run.clone()),
                _ => None,
            })
            .collect();
        for run in runs {
            note(
                "stopping a run",
                &ctx.api().stop_run(self.lease, &run).await,
            );
        }
    }

    /// Tells the farm that every run of a lease the lease server no longer has is lost, so it
    /// runs them again at once. A result already formed is sent as it is.
    pub(super) async fn report_lost(&self, ctx: &Ctx<'_>) {
        for (lease, task) in &self.tasks {
            let result = match &task.stage {
                Stage::Reporting(report) => report.result.clone(),
                _ => {
                    ctx.deps.metrics.run_ended(RunEnd::Lost);
                    result_of(*lease, &task.action, &Settled::Lost(Lost::DaemonRestarted))
                }
            };
            ctx.deps.farm.report(&self.id, result).await;
        }
    }
}

impl Reporting {
    /// Collects the answer of the report in flight, and sends the result again when none is
    /// in flight and the last send was long enough ago. True once the farm acknowledged.
    fn drive(
        &mut self,
        farm: &std::sync::Arc<crate::Farm>,
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
            let (farm, worker, result) = (
                std::sync::Arc::clone(farm),
                worker.clone(),
                self.result.clone(),
            );
            self.in_flight = Some(tokio::spawn(
                async move { farm.report(&worker, result).await },
            ));
        }
        acked
    }
}

fn absent(run: RunId) -> Run {
    Run {
        run_id: run,
        state: RunState::NeverStarted,
        outcome: None,
        exit_code: None,
        detail: None,
        labels: BTreeMap::new(),
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
    lease: kbf_types::LeaseId,
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
