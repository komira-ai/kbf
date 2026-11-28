//! How the executor learns what became of the jobs it runs: one listing for all of them each
//! poll, not one read of each.
//!
//! The job service has no call that waits on a job: no watch, no stream, no long poll. Without
//! this module each operation would read its own job every poll. Here, a job's own first reads
//! still go out at once and at the steps of a job that has not started ([`super::pace`]), so a
//! job that starts or ends at once is seen as soon as before. When its next wait would be a
//! whole poll, the job is handed to the executor's [`Follower`] and is no longer read on its
//! own. The follower sends one `ListJobs` each poll, for every job of this process that has not
//! ended: the four live states, and the `kbf-instance` label with this process's id
//! ([`Config::instance`](super::Config::instance)), a page of [`PAGE`] jobs at a time. Each job
//! it follows gets one of three answers, [`News`]:
//!
//! - **Listed**: the job is in the listing, and the listed job is taken as a read of it.
//! - **Missing**: the job is not in a listing that started after the job was handed over. Its
//!   operation reads it once on its own: it has ended, or the service no longer knows it, or (a
//!   state this server does not know) it may still be live, and then it is followed again.
//! - **Unlisted**: the listing failed with an error that is not the service's load (a page token
//!   refused or given twice, more than [`Client::MAX_LISTED`](kbf_job_client::Client::MAX_LISTED)
//!   jobs, an answer that cannot be decoded). Each operation reads its job on its own at once,
//!   and from then on at its own waits, until it ends: a listing that cannot be had is not sent
//!   again each poll on top of the reads, and the jobs' reads are not bunched at its answer.
//!
//! A listing that fails because the service is busy or cannot be reached tells no job anything:
//! the jobs wait for the next listing, which waits as long as any call on a busy service
//! ([`Busy::pause_or`](super::busy::Busy::pause_or)). A lost connection is so retried and never
//! ends a run. Each page of a listing has the limit of one call
//! ([`Config::call_timeout`](super::Config::call_timeout)), so a listing of many pages is not cut
//! for its length. The follower waits a poll after each listing's answer, as an operation waited a
//! poll after each read, so no job waits longer between two looks at it than before. A cancel
//! is heard while the job waits for the listing, as it was while it waited for its own read.
//!
//! A job another process submitted and this one took up carries the other process's label and
//! is not in this process's listings. It is read on its own each poll, as before, until it ends.
//! A job whose read names no `kbf-instance` label of this process is never handed over.
//!
//! The follower runs a task only while it follows a job. Its first listing goes out a poll after
//! the first job is handed over, and it ends when no job is left.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use js_proto::v1::{Job, JobState, ListJobsRequest};
use kbf_job_client::Error;
use tokio::sync::watch;
use tokio::time::sleep;

use super::Inner;
use super::submit::INSTANCE_LABEL;

/// The most jobs one page of a listing asks for. A job's record (its spec and its newest
/// events) is a few KiB, so a page stays well under the client's 4 MiB cap.
pub(crate) const PAGE: u32 = 250;

/// The states of a job that has not ended: what a listing asks for.
const LIVE: [JobState; 4] = [
    JobState::Queued,
    JobState::Placed,
    JobState::Starting,
    JobState::Running,
];

/// The listing of the live jobs of the process `instance`.
pub(crate) fn live_jobs(instance: &str) -> ListJobsRequest {
    ListJobsRequest {
        states: LIVE.into_iter().map(i32::from).collect(),
        labels: std::iter::once((INSTANCE_LABEL.to_owned(), instance.to_owned())).collect(),
        page_size: PAGE,
        ..ListJobsRequest::default()
    }
}

/// What a listing told a job it follows.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum News {
    /// The job as the listing has it.
    Listed(Box<Job>),
    /// The job is not in a listing that started after it was handed over.
    Missing,
    /// The listing failed with an error that is not the service's load.
    Unlisted,
}

/// The jobs the executor follows by listing, shared by every operation.
#[derive(Default)]
pub(crate) struct Follower {
    registry: Mutex<Registry>,
}

impl Follower {
    fn registry(&self) -> MutexGuard<'_, Registry> {
        self.registry.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// How many jobs it follows.
    #[cfg(test)]
    pub(crate) fn following(&self) -> usize {
        self.registry().jobs.len()
    }
}

/// The followed jobs, and where the listings are.
#[derive(Default)]
struct Registry {
    jobs: BTreeMap<String, Entry>,
    /// The mark of the next job handed over: a listing that starts at mark `m` says something
    /// of a missing job only if the job's mark is below `m`.
    next: u64,
    /// Whether the task that lists runs.
    listing: bool,
}

/// One followed job.
struct Entry {
    /// [`Registry::next`] when it was handed over.
    since: u64,
    /// The last news of it.
    news: watch::Sender<Option<News>>,
}

impl Registry {
    /// Follows `job`, and whether the task that lists is to be started.
    fn join(&mut self, job: &str) -> (watch::Receiver<Option<News>>, bool) {
        let (news, heard) = watch::channel(None);
        let since = self.next;
        self.next += 1;
        self.jobs.insert(job.to_owned(), Entry { since, news });
        let start = !std::mem::replace(&mut self.listing, true);
        (heard, start)
    }

    /// No longer follows `job`.
    fn leave(&mut self, job: &str) {
        self.jobs.remove(job);
    }

    /// The mark of a listing that starts now, or `None` when no job is followed, and the task
    /// that lists ends.
    fn round(&mut self) -> Option<u64> {
        if self.jobs.is_empty() {
            self.listing = false;
            return None;
        }
        Some(self.next)
    }

    /// Tells each followed job what the listing that started at `mark` found: `listed`, the
    /// jobs it answered, or `None` when it failed with an error that is not the service's load.
    fn tell(&self, mark: u64, listed: Option<Vec<Job>>) {
        let Some(listed) = listed else {
            for entry in self.jobs.values() {
                entry.news.send_replace(Some(News::Unlisted));
            }
            return;
        };
        let mut by_id: BTreeMap<String, Job> = listed
            .into_iter()
            .map(|job| (job.id.clone(), job))
            .collect();
        for (id, entry) in &self.jobs {
            let news = match by_id.remove(id) {
                Some(job) => News::Listed(Box::new(job)),
                None if entry.since < mark => News::Missing,
                // Handed over while the listing was out: the next one says.
                None => continue,
            };
            entry.news.send_replace(Some(news));
        }
    }
}

/// Lists the followed jobs every poll, until none is left.
async fn rounds(inner: Arc<Inner>) {
    loop {
        sleep(inner.busy.pause_or(inner.cfg.poll)).await;
        let Some(mark) = inner.follower.registry().round() else {
            return;
        };
        let answer = inner.api().list_live(&inner.cfg.instance).await;
        let seen = inner.busy.saw("listing the jobs it follows", &answer);
        let listed = match answer {
            Ok(jobs) => Some(jobs),
            // The jobs wait for the next listing; a busy service is logged once, not each time.
            Err(_) if seen.busy() => continue,
            Err(error) => {
                tracing::warn!(%error, "listing the jobs it follows failed: each is read on its own");
                None
            }
        };
        inner.follower.registry().tell(mark, listed);
    }
}

/// The reads of one job, by the operation that waits on it: its own `GetJob` until its wait is
/// a whole poll, then the follower's listings. Dropped, it stops following the job.
pub(crate) struct Follow {
    inner: Arc<Inner>,
    job: String,
    /// The follower's news of the job, once it follows it.
    heard: Option<watch::Receiver<Option<News>>>,
    /// Whether the last read of the job names this process in its `kbf-instance` label.
    ours: bool,
    /// Whether a listing could not be had, and the job is read on its own until it ends.
    alone: bool,
}

impl Follow {
    /// The reads of `job`, which no read has looked at yet.
    pub(crate) fn new(inner: &Arc<Inner>, job: &str) -> Self {
        Self {
            inner: Arc::clone(inner),
            job: job.to_owned(),
            heard: None,
            ours: false,
            alone: false,
        }
    }

    /// The next read of the job, after `wait` when it is the job's own: none for the first
    /// read; the step or the poll after it ([`super::pace`]). A wait of a whole poll hands a job
    /// of this process to the follower, whose listing is then the read, unless it tells the job
    /// to be read on its own. Safe to drop at any await: a cancel that comes while it waits
    /// loses nothing.
    pub(crate) async fn read(&mut self, wait: Option<Duration>) -> Result<Job, Error> {
        let poll = self.inner.cfg.poll;
        let due = wait.is_some_and(|wait| wait >= poll);
        if self.heard.is_none() && self.ours && !self.alone && due {
            let (heard, start) = self.inner.follower.registry().join(&self.job);
            if start {
                tokio::spawn(rounds(Arc::clone(&self.inner)));
            }
            self.heard = Some(heard);
        }
        match self.heard.as_mut() {
            Some(heard) => match news(heard).await {
                Some(News::Listed(job)) => return Ok(*job),
                Some(News::Missing) => {}
                // No listing can be had: read on its own, now and until it ends.
                Some(News::Unlisted) | None => {
                    self.inner.follower.registry().leave(&self.job);
                    (self.heard, self.alone) = (None, true);
                }
            },
            None => waited(wait.map(|quiet| self.inner.busy.pause_or(quiet))).await,
        }
        let read = self.inner.api().get_job(&self.job).await;
        if let Ok(job) = &read {
            self.ours = names(job, &self.inner.cfg.instance);
        }
        read
    }
}

impl Drop for Follow {
    fn drop(&mut self) {
        if self.heard.is_some() {
            self.inner.follower.registry().leave(&self.job);
        }
    }
}

/// The next news the follower sends; `None` if it can send none.
async fn news(heard: &mut watch::Receiver<Option<News>>) -> Option<News> {
    heard.changed().await.ok()?;
    heard.borrow_and_update().clone()
}

/// Whether `job` carries the `kbf-instance` label of `instance`.
fn names(job: &Job, instance: &str) -> bool {
    job.spec
        .as_ref()
        .and_then(|spec| spec.labels.get(INSTANCE_LABEL))
        .is_some_and(|label| label == instance)
}

/// Completes after `pause`, or at once when there is none.
async fn waited(pause: Option<Duration>) {
    if let Some(pause) = pause {
        sleep(pause).await;
    }
}

#[cfg(test)]
#[path = "follow_tests.rs"]
mod tests;
