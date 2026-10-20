//! The release poller (#947, supply side, phase 1): on the active frontend, every
//! `--release-poll-every-secs`, ask the GitHub Releases API of `--release-poll-repo` for the
//! releases of `v*` tags, and **propose** the newest one the farm could run.
//!
//! - **Only proposes.** A proposal is the row `update/proposed/<commit>`
//!   ([`crate::update::Proposal`]), shown in `GET /v1/update`. The poller never writes
//!   `update/desired`, `update/policy` or a host row, downloads nothing and trusts nothing:
//!   the release store is fed, and a run is started, by an operator. What it reads is
//!   names (see [`feed`]); no attestation is verified here.
//! - **Active child only.** A farm that is handed over ([`Handed::handed`]) makes no request,
//!   and a poll that was under way when the farm was handed over is dropped before it
//!   writes. The proposal rows are a fenced family (`write_rules`), so a store handle a
//!   newer writer superseded is refused too, and the poller ends ([`Cycle::Superseded`]).
//! - **Cheap when nothing changed.** The request is conditional on the last `ETag`; a `304`
//!   is a quiet cycle. The feed is read again only after a cycle finished with it, so a
//!   cycle that failed half way is repeated whole.
//! - **Backs off.** After a failure the wait doubles each time up to an hour, and a rate
//!   limit's `Retry-After` (or reset time) is honoured if longer. No credential is ever
//!   sent (phase 2, which reads the newest successful artifacts run, needs a read token
//!   and is a held item).
//! - **Metrics** ([`Stats::render`]): `kbf_release_poll_polls_total`,
//!   `_not_modified_total`, `_proposals_total`, `_errors_total` and
//!   `_ignored_total{reason}` for the entries that were not candidates.

pub mod feed;
pub mod github;

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kbf_front::Handed;
use kbf_state::{Store, StoreError};
use kbf_types::FarmTime;
use tokio::task::AbortHandle;

use self::feed::{Candidate, Ignore};
use crate::farm::Farm;
use crate::update::{Proposal, RowError, Rows};

/// The longest wait the doubling after failures reaches (a configured interval above it
/// is kept).
const MAX_BACKOFF: Duration = Duration::from_secs(3600);

/// The longest a rate limit's request to wait is honoured for.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(2 * 3600);

/// The GitHub API.
pub const GITHUB_API: &str = "https://api.github.com";

/// What the poller needs to know.
#[derive(Clone, Debug)]
pub struct Config {
    /// The time between polls.
    pub every: Duration,
    /// The repository whose releases are read, `owner/name`.
    pub repo: String,
    /// The refs a release may have been built from.
    pub refs: Vec<String>,
    /// The API's base URL.
    pub api_url: String,
    /// How long one request may take.
    pub timeout: Duration,
    /// The commit this build was made from (12 hex digits), whose release is not proposed.
    pub running: String,
}

/// The flags of the poller.
#[derive(Clone, Debug, clap::Args)]
pub struct PollFlags {
    /// How often, in seconds, the active frontend asks GitHub for new releases of `--release-poll-repo` and records the newest one the farm could run as a proposal (`proposals` of `GET /v1/update`). A proposal starts nothing: only an operator's `POST /v1/update/desired` does. The question is one conditional request with no credential, so an unchanged feed costs nothing. 0 turns the poller off. It needs `--state-store`, which holds the proposals
    #[arg(long, value_name = "SECONDS", default_value_t = 300)]
    pub release_poll_every_secs: u64,
    /// The GitHub repository, `owner/name`, whose releases of `v*` tags are polled
    #[arg(
        long,
        value_name = "OWNER/NAME",
        default_value = "komira-ai/kbf",
        value_parser = parse_repo
    )]
    pub release_poll_repo: String,
    /// The refs a polled release may have been built from, as its notes say, separated by commas; a release built from another is ignored and counted
    #[arg(
        long,
        value_name = "REF",
        value_delimiter = ',',
        default_value = "main"
    )]
    pub release_poll_refs: Vec<String>,
}

fn parse_repo(text: &str) -> Result<String, String> {
    let name = |s: &str| {
        !s.is_empty()
            && !s.starts_with('.')
            && s.bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    };
    match text.split_once('/') {
        Some((owner, repo)) if name(owner) && name(repo) => Ok(text.to_owned()),
        _ => Err("expected OWNER/NAME of letters, digits, '-', '_' and '.'".to_owned()),
    }
}

impl PollFlags {
    /// The poller these flags ask for; `None` when it is off.
    #[must_use]
    pub fn config(&self) -> Option<Config> {
        (self.release_poll_every_secs > 0).then(|| Config {
            every: Duration::from_secs(self.release_poll_every_secs),
            repo: self.release_poll_repo.clone(),
            refs: self.release_poll_refs.clone(),
            api_url: GITHUB_API.to_owned(),
            timeout: Duration::from_secs(60),
            running: crate::BUILD_COMMIT.to_owned(),
        })
    }
}

/// What a request for the feed gave.
#[derive(Debug, PartialEq, Eq)]
pub enum Fetched {
    /// `304`: nothing changed since the `ETag` sent.
    NotModified,
    /// `200`: the feed, and its `ETag`.
    Feed {
        /// The entity tag to send next time.
        etag: Option<String>,
        /// The JSON body.
        body: Vec<u8>,
    },
}

/// Why a request for the feed gave none.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum FetchError {
    /// The connection, the TLS handshake or the HTTP exchange failed.
    #[error("{0}")]
    Transport(String),
    /// The answer did not come in time.
    #[error("no answer in time")]
    Timeout,
    /// Rate limited, asking the caller to wait this long if it said.
    #[error("rate limited")]
    RateLimited(Option<Duration>),
    /// Another status.
    #[error("status {0}")]
    Status(u16),
    /// The body is larger than is read.
    #[error("the feed is too large")]
    TooLarge,
}

/// Where the feed comes from: GitHub ([`github::GithubSource`]), or a fake in a test.
pub trait Source: Send + Sync + 'static {
    /// The feed, unless `etag` (the last answer's) still names it.
    fn fetch<'a>(
        &'a self,
        etag: Option<&'a str>,
    ) -> impl Future<Output = Result<Fetched, FetchError>> + Send + 'a;
}

/// What the poller counted. Shared with the metrics route.
#[derive(Debug, Default)]
pub struct Stats {
    polls: AtomicU64,
    not_modified: AtomicU64,
    proposals: AtomicU64,
    errors: AtomicU64,
    ignored: [AtomicU64; Ignore::ALL.len()],
}

impl Stats {
    /// Polls begun.
    pub fn polls(&self) -> u64 {
        self.polls.load(Ordering::Relaxed)
    }

    /// Polls the source answered with "not modified".
    pub fn not_modified(&self) -> u64 {
        self.not_modified.load(Ordering::Relaxed)
    }

    /// Proposals written.
    pub fn proposals(&self) -> u64 {
        self.proposals.load(Ordering::Relaxed)
    }

    /// Polls that failed: the request, the feed or the rows.
    pub fn errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }

    /// Feed entries ignored for `reason`.
    pub fn ignored(&self, reason: Ignore) -> u64 {
        self.ignored[reason.index()].load(Ordering::Relaxed)
    }

    /// Appends the counters in the Prometheus text format.
    pub fn render(&self, out: &mut String) {
        use std::fmt::Write as _;
        let counters = [
            (
                "kbf_release_poll_polls_total",
                "Polls of the release feed begun",
                self.polls(),
            ),
            (
                "kbf_release_poll_not_modified_total",
                "Polls the release feed answered with not modified",
                self.not_modified(),
            ),
            (
                "kbf_release_poll_proposals_total",
                "Releases proposed to the operator",
                self.proposals(),
            ),
            (
                "kbf_release_poll_errors_total",
                "Polls that failed in the request, the feed or the update rows",
                self.errors(),
            ),
        ];
        for (name, help, value) in counters {
            let _ = writeln!(
                out,
                "# HELP {name} {help}\n# TYPE {name} counter\n{name} {value}"
            );
        }
        let name = "kbf_release_poll_ignored_total";
        let _ = writeln!(
            out,
            "# HELP {name} Feed entries that were not candidates, by the first reason met\n# TYPE {name} counter"
        );
        for reason in Ignore::ALL {
            let _ = writeln!(
                out,
                "{name}{{reason=\"{}\"}} {}",
                reason.label(),
                self.ignored(reason)
            );
        }
    }
}

/// What one cycle did.
#[derive(Debug, PartialEq, Eq)]
pub enum Cycle {
    /// The source said nothing changed.
    NotModified,
    /// The feed was read and the newest candidate recorded.
    Proposed(Proposal),
    /// The feed was read and holds nothing to propose.
    Nothing,
    /// The poll failed; it is made again after a backoff.
    Failed,
    /// The farm was handed over while the request was out: nothing was written.
    Dropped,
    /// The store handle was superseded by a newer writer: this poller is done.
    Superseded,
}

/// The poller's state between cycles.
pub struct Poller<S> {
    source: S,
    rows: Rows,
    config: Config,
    stats: Arc<Stats>,
    etag: Option<String>,
    failures: u32,
    asked_to_wait: Option<Duration>,
}

impl<S: Source> Poller<S> {
    /// A poller over `rows` reading `source`, counting into `stats`.
    pub fn new(source: S, rows: Rows, config: Config, stats: Arc<Stats>) -> Self {
        Self {
            source,
            rows,
            config,
            stats,
            etag: None,
            failures: 0,
            asked_to_wait: None,
        }
    }

    /// How long to wait before the next cycle: the interval, doubled for each failure in a
    /// row up to an hour, and no less than a rate limit asked for.
    #[must_use]
    pub fn wait(&self) -> Duration {
        let every = self.config.every;
        let backed = every
            .saturating_mul(1 << self.failures.min(6))
            .min(MAX_BACKOFF.max(every));
        backed.max(self.asked_to_wait.unwrap_or_default().min(MAX_RETRY_AFTER))
    }

    fn fail(&mut self, asked: Option<Duration>) -> Cycle {
        self.stats.errors.fetch_add(1, Ordering::Relaxed);
        self.failures = self.failures.saturating_add(1);
        self.asked_to_wait = asked;
        Cycle::Failed
    }

    /// One poll. `active` says whether this process is still the farm's active child; it is
    /// asked again after the request, before anything is written.
    pub async fn cycle(&mut self, now: FarmTime, active: impl Fn() -> bool) -> Cycle {
        self.stats.polls.fetch_add(1, Ordering::Relaxed);
        let fetched = self.source.fetch(self.etag.as_deref()).await;
        let (etag, body) = match fetched {
            Err(e) => {
                tracing::warn!("release poll: {e}");
                let asked = match e {
                    FetchError::RateLimited(asked) => asked,
                    _ => None,
                };
                return self.fail(asked);
            }
            Ok(Fetched::NotModified) => {
                self.failures = 0;
                self.asked_to_wait = None;
                self.stats.not_modified.fetch_add(1, Ordering::Relaxed);
                return Cycle::NotModified;
            }
            Ok(Fetched::Feed { etag, body }) => (etag, body),
        };
        let entries = match feed::entries(&body) {
            Ok(entries) => entries,
            Err(why) => {
                tracing::warn!("release poll: {why}");
                return self.fail(None);
            }
        };
        if !active() {
            return Cycle::Dropped;
        }
        let mut candidates = Vec::new();
        for entry in &entries {
            match feed::judge(entry, &self.config.refs) {
                Ok(candidate) => candidates.push(candidate),
                Err(reason) => {
                    self.stats.ignored[reason.index()].fetch_add(1, Ordering::Relaxed);
                }
            }
        }
        let rows = self.rows.clone();
        let running = self.config.running.clone();
        let done = blocking(move || {
            let proposals = rows.proposals()?;
            let desired = rows.desired()?.value.map(|d| d.commit);
            let Some(next) = decide(
                candidates,
                &proposals,
                desired.as_ref().map(|d| d.as_str()),
                &running,
            ) else {
                return Ok(None);
            };
            let proposal = Proposal {
                commit: next.commit,
                tag: next.tag,
                published_at_secs: next.published_at_secs,
                proposed_at_ms: now.as_millis(),
            };
            rows.propose(&proposal).map(|new| new.then_some(proposal))
        })
        .await;
        match done {
            Err(RowError::Store(StoreError::Fenced)) => Cycle::Superseded,
            Err(e) => {
                tracing::warn!("release poll: {e}");
                self.fail(None)
            }
            Ok(proposed) => {
                self.failures = 0;
                self.asked_to_wait = None;
                self.etag = etag;
                match proposed {
                    Some(proposal) => {
                        self.stats.proposals.fetch_add(1, Ordering::Relaxed);
                        tracing::info!(commit = %proposal.commit, tag = %proposal.tag, "release poll: proposed");
                        Cycle::Proposed(proposal)
                    }
                    None => Cycle::Nothing,
                }
            }
        }
    }
}

/// Runs a blocking call on the store's rows; a call that panics fails like a store error.
async fn blocking<T: Send + 'static>(
    call: impl FnOnce() -> Result<T, RowError> + Send + 'static,
) -> Result<T, RowError> {
    crate::release::desired::blocking(call).await
}

/// The candidate to propose, if any: the newest, unless it is the release already desired
/// or running, or no newer than the newest proposal held (older and same releases are never
/// proposed, and a release is proposed once).
#[must_use]
pub fn decide(
    candidates: Vec<Candidate>,
    held: &[Proposal],
    desired: Option<&str>,
    running: &str,
) -> Option<Candidate> {
    let newest = candidates
        .into_iter()
        .max_by(|a, b| (a.published_at_secs, &a.commit).cmp(&(b.published_at_secs, &b.commit)))?;
    let is_running = running.len() >= 7 && newest.commit.starts_with(running);
    let is_desired = desired == Some(newest.commit.as_str());
    let already = held
        .iter()
        .any(|p| p.published_at_secs >= newest.published_at_secs || p.commit == newest.commit);
    (!is_running && !is_desired && !already).then_some(newest)
}

/// Runs the poller until its store handle is superseded or the task is aborted. `active` is
/// asked before every request and again before every write.
pub async fn run<S: Source>(
    mut poller: Poller<S>,
    now: impl Fn() -> FarmTime,
    active: impl Fn() -> bool,
) {
    let mut waiting = false;
    loop {
        tokio::time::sleep(poller.wait()).await;
        if !active() {
            if !std::mem::replace(&mut waiting, true) {
                tracing::info!("the release poller makes no request: this farm is handed over");
            }
            continue;
        }
        waiting = false;
        if poller.cycle(now(), &active).await == Cycle::Superseded {
            tracing::warn!("the release poller stopped: a newer writer holds the state store");
            return;
        }
    }
}

/// Starts the poller of `config` over `store` for `farm`, on the current runtime; the
/// handle stops it. `None` without a state store (the proposals have nowhere to live) or
/// when the source cannot be made.
#[must_use]
pub fn spawn(
    farm: Arc<Farm>,
    store: Option<Arc<dyn Store>>,
    config: Option<Config>,
    stats: Arc<Stats>,
) -> Option<AbortHandle> {
    let (store, config) = store.zip(config)?;
    let source = match github::GithubSource::new(
        &config.api_url,
        &config.repo,
        config.timeout,
        github::Roots::System,
    ) {
        Ok(source) => source,
        Err(why) => {
            tracing::error!("the release poller is off: {why}");
            return None;
        }
    };
    let poller = Poller::new(source, Rows::new(store), config, stats);
    let for_now = Arc::clone(&farm);
    Some(
        tokio::spawn(run(
            poller,
            move || for_now.wall_now(),
            move || !farm.handed(),
        ))
        .abort_handle(),
    )
}

#[cfg(test)]
mod tests;
