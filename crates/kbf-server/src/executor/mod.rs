//! The host executor: running a placed action as a job of the job service instead of on a
//! `kbf-daemon`.
//!
//! # How it sits in the server
//!
//! The scheduler core knows nothing of this. The executor registers virtual workers, `js:<frontend>-<n>`,
//! each a node of the declared shape whose drivers are `container` and `native-whole-machine`,
//! and takes the messages the farm sends a daemon on the channel it owns the receiving end of.
//! A `Start` is submitted to the job service as a job of the kbf image, whose entrypoint is the
//! task tool (a `whole_machine` lease's as a job that needs the whole host), under an id
//! made from the farm lease, so a submit sent again makes no second job; the job is read back
//! until it ends, its ending is mapped to the farm's failure classes ([`outcome`]), and the
//! result is reported the way a daemon reports one. A `Cancel` is a cancel of the job. The
//! executor holds no capacity, renews nothing and never sees where a job runs. `worker.rs`, the
//! scheduler core and the sched crates are untouched, and a server that never builds an
//! [`Executor`] never opens a connection to a job service: [`ExecutorKind::KbfDaemon`] is the
//! path every server runs by default.
//!
//! The executor runs on exactly one frontend: it labels its jobs with its frontend id and an id
//! of the process that made them, and on start cancels every live job an earlier process left
//! under that frontend id, which nothing of the farm maps to any more. Those are taken only
//! once no new job of that process is seen over [`Config::adopt_watch`]: a process that makes
//! new jobs is alive under the same id (a copied config), and the executor stops instead of
//! cancelling its jobs. [`Executor::rival`] finds another frontend's live jobs on the job
//! service, and the task that steps the executor ([`Running`]) stops when it does. The id must
//! be different for each frontend.
//!
//! The frontend builds one only for `--executor js-agent` (`frontend/executor_flags.rs`), over
//! [`Front`], which implements the three seams ([`Ingest`], [`Presign`], [`Inputs`]) with the
//! frontend's own packs, CAS, key mint and run preparation, and steps it from
//! [`FrontendHandle::serve`](crate::frontend::FrontendHandle::serve) until the frontend stops.

mod api;
mod config;
mod driver;
mod front;
mod host;
mod metrics;
mod out;
mod outcome;
mod output;
mod phases;
mod pool;
mod runs;
mod submit;
mod task;
mod traits;

#[cfg(test)]
#[path = "support_tests.rs"]
mod test_support;

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use futures::future::join_all;
use js_proto::v1::Job;
use kbf_job_client::Client;
use kbf_types::WorkerId;
use tokio::time::Instant;

use crate::Farm;
use host::HostWorker;
use pool::{Adopted, Adoption};
use submit::{FRONTEND_LABEL, INSTANCE_LABEL};

pub use config::{Config, ConfigError, Network};
pub use driver::Running;
pub use front::{
    Front, FrontParts, PrepareError, PreparedPutDir, PreparedRun, PutDirParts, RunPrepare,
    RunSource,
};
pub use metrics::Metrics;
pub use out::{MAX_ROOT_BYTES, OUT_TARGET, PutDirRoots, RootError, TASK_IN_TREE, TaskOut};
pub use submit::job_id;
pub use task::{Delivery, PackKey, Secret, TaskArgs, build_id};
pub use traits::{
    ActionFacts, Ingest, Ingested, Inputs, InputsError, MintError, Presign, PresignError,
    ReadGrant, RejectReason, UntrustedLimits,
};

/// Which executor a server runs placed actions on.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExecutorKind {
    /// The `kbf-daemon` of each node, over `kbf.worker.v1`: the default path, and the
    /// rollback.
    #[default]
    KbfDaemon,
    /// Jobs of the job service, run by [`Executor`].
    JsAgent,
}

/// What the executor is built over.
#[derive(Clone)]
pub struct Deps {
    /// The farm the workers register with.
    pub farm: Arc<Farm>,
    /// The job service.
    pub client: Client,
    /// The frontend's read of a task's pack.
    pub ingest: Arc<dyn Ingest>,
    /// The store's presigned `PUT`.
    pub presign: Arc<dyn Presign>,
    /// The CAS read of an action, and the read grant.
    pub inputs: Arc<dyn Inputs>,
    /// What the executor counts.
    pub metrics: Arc<Metrics>,
}

/// Why the executor must stop, as a worker found it: another process holds this frontend's id.
#[derive(Default)]
pub(crate) struct Twin(Mutex<Option<String>>);

impl Twin {
    pub(crate) fn flag(&self, why: String) {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(why);
    }

    fn take(&self) -> Option<String> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).take()
    }
}

/// The message the executor stops with when another process runs under its frontend id.
pub(crate) fn twin_message(frontend: &str) -> String {
    format!(
        "another frontend runs under the same id {frontend:?}: --executor-frontend-id must be different for each frontend"
    )
}

/// What one pass reads once for every worker.
pub(crate) struct Ctx<'a> {
    pub(crate) cfg: &'a Config,
    pub(crate) deps: &'a Deps,
    pub(crate) twin: &'a Twin,
}

impl Ctx<'_> {
    /// The job service, each call limited to [`Config::call_timeout`].
    pub(crate) fn api(&self) -> api::Api<'_> {
        api::Api {
            client: &self.deps.client,
            limit: self.cfg.call_timeout,
        }
    }

    /// The executor's clock: tokio's, so a test that pauses it moves it with `advance`.
    pub(crate) fn now(&self) -> Instant {
        Instant::now()
    }
}

/// A virtual worker, as an operator or a test looks at it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WorkerInfo {
    /// The worker the farm knows.
    pub worker: WorkerId,
    /// The farm leases it holds: submitted, ingesting, being withdrawn or waiting for an
    /// acknowledgement.
    pub runs: usize,
    /// Whether it has stopped taking work.
    pub draining: bool,
    /// Whether it has fenced itself and is silent.
    pub silent: bool,
}

/// The virtual workers, and what runs as their jobs.
pub struct Executor {
    cfg: Config,
    deps: Deps,
    workers: Vec<HostWorker>,
    /// Whether the jobs an earlier process of this frontend left have been taken up.
    adoption: Adoption,
    /// Why the executor stopped itself: another process runs under this frontend's id.
    twin: Option<String>,
    flagged: Twin,
}

impl Executor {
    /// An executor of `cfg` over `deps`. Nothing is asked of the job service until the first
    /// [`step`](Self::step).
    ///
    /// # Errors
    /// `cfg` breaks a rule of [`Config::validate`].
    pub fn new(cfg: Config, deps: Deps) -> Result<Self, ConfigError> {
        cfg.validate()?;
        Ok(Self {
            cfg,
            deps,
            workers: Vec::new(),
            adoption: Adoption::Unread,
            twin: None,
            flagged: Twin::default(),
        })
    }

    /// The workers registered now.
    #[must_use]
    pub fn workers(&self) -> Vec<WorkerInfo> {
        self.workers.iter().map(HostWorker::info).collect()
    }

    /// How often the caller steps the executor.
    #[must_use]
    pub fn poll(&self) -> Duration {
        self.cfg.poll
    }

    /// How often the caller checks that no other frontend has jobs: every
    /// [`Config::adopt_watch`].
    #[must_use]
    pub fn rival_check(&self) -> Duration {
        self.cfg.adopt_watch()
    }

    /// Why this executor must stop, if another frontend (or another process under this
    /// frontend's id) has live jobs on the job service and is older: the executor runs on
    /// exactly one frontend, and two would each register workers the farm knows one set of,
    /// and a restart under one id cancels the other's jobs. Of two that have jobs the one whose
    /// oldest live job is younger stops (ties go to the lower frontend id, then instance), so
    /// two that started together and both submitted are not both stopped, and a frontend that
    /// finds another's jobs before it has any stops at once. A job whose cancel was asked for is
    /// on its way out and is no one's. Nothing is concluded from a list that could not be read
    /// (one of more jobs than the client lists at once included).
    ///
    /// Jobs of another process under this frontend's own id are not judged here until the jobs
    /// of an earlier process have been taken up: that is [`Adoption`]'s question.
    pub async fn rival(&self) -> Option<String> {
        let ctx = Ctx {
            cfg: &self.cfg,
            deps: &self.deps,
            twin: &self.flagged,
        };
        let jobs = ctx.api().list_all_jobs(pool::live_jobs(&[])).await.ok()?;
        let me = (self.cfg.frontend.as_str(), self.cfg.instance.as_str());
        let mut mine = u64::MAX;
        let mut others = Vec::new();
        for job in jobs.iter().filter(|job| !job.cancel_requested) {
            let Some((frontend, instance)) = owner(job) else {
                continue;
            };
            let queued = job.times.as_ref().map_or(0, |times| times.queued_at_ms);
            if (frontend, instance) == me {
                mine = mine.min(queued);
            } else if frontend != self.cfg.frontend || self.adoption.is_done() {
                others.push((queued, frontend.to_owned(), instance.to_owned()));
            }
        }
        let older = others
            .into_iter()
            .filter(|(since, frontend, instance)| {
                (*since, frontend.as_str(), instance.as_str()) < (mine, me.0, me.1)
            })
            .min()?;
        Some(match older.1 == self.cfg.frontend {
            true => twin_message(&older.1),
            false => format!(
                "the frontend {:?} has jobs on the job service too: the executor runs on exactly one frontend",
                older.1
            ),
        })
    }

    /// Why the executor stopped itself without being asked, if it did: another process runs
    /// under this frontend's id ([`Executor::new`]'s `Config::frontend`).
    #[must_use]
    pub fn twin(&self) -> Option<&str> {
        self.twin.as_deref()
    }

    /// One pass over every worker: the farm's messages taken, jobs submitted, read and
    /// cancelled, results reported, heartbeats sent. A caller steps it every [`Config::poll`];
    /// a pass never waits on a job. Before the first pass that registers the workers, the jobs
    /// an earlier process left are taken up.
    pub async fn step(&mut self) {
        if self.twin.is_some() {
            return;
        }
        let now = Instant::now();
        let ctx = Ctx {
            cfg: &self.cfg,
            deps: &self.deps,
            twin: &self.flagged,
        };
        if !self.adoption.is_done() {
            match pool::adopt_jobs(&ctx, now, &mut self.adoption).await {
                Adopted::Done => {
                    self.adoption = Adoption::Done;
                    self.workers = pool::workers(&ctx, now);
                }
                // The jobs of an earlier process are not yet known to be nobody's: nothing is
                // submitted while a live twin might own them.
                Adopted::Watching | Adopted::Unread => return,
                Adopted::Twin(why) => {
                    self.twin = Some(why);
                    return;
                }
            }
        }
        join_all(self.workers.iter_mut().map(|worker| worker.step(&ctx, now))).await;
        self.twin = self.flagged.take();
    }

    /// Cancels every job and tells the farm the workers take no more work. The executor holds
    /// nothing afterwards, and asks for nothing more unless it is stepped.
    pub async fn shutdown(&mut self) {
        let ctx = Ctx {
            cfg: &self.cfg,
            deps: &self.deps,
            twin: &self.flagged,
        };
        let workers = std::mem::take(&mut self.workers);
        join_all(workers.into_iter().map(|worker| worker.retire(&ctx))).await;
        self.adoption = Adoption::Unread;
        self.twin = None;
        self.flagged.take();
    }
}

/// The frontend and the instance labels of `job`, if it is one of a kbf executor's.
fn owner(job: &Job) -> Option<(&str, &str)> {
    let labels = &job.spec.as_ref()?.labels;
    let frontend = labels.get(FRONTEND_LABEL)?;
    let instance = labels.get(INSTANCE_LABEL).map_or("", String::as_str);
    Some((frontend.as_str(), instance))
}
