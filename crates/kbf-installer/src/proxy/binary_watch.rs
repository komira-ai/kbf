//! The watch on the kbf a package installs (`proxy --kbf-binary PATH`): when a package manager
//! puts another kbf at `PATH`, the proxy copies it, stages its facts and starts the frontend
//! swap to it. Nothing else notices: a running process keeps the file it was started from, and
//! nothing restarts it. Without the flag nothing is watched and no thread is started
//! ([`spawn`]).
//!
//! # Polling
//!
//! Every [`POLL`] the watch reads the identity of the file at `PATH` (device, inode, size and
//! modification time to the nanosecond; links followed). It acts on an identity only when two
//! polls in a row read the same one, so a file that is still being unpacked, and so changing
//! from one poll to the next, is skipped until it holds still. A file that cannot be read
//! counts as no identity and starts over.
//!
//! # What acting on an identity does
//!
//! 1. If the swap cannot be started now ([`Swapper::busy`]: one is running or finishing, or the
//!    proxy is not bound or is holding for want of a serving child), nothing is copied, staged
//!    or started (the kbf is run, once for each identity, to name its commit for the status)
//!    and the identity is left unconsumed: the next poll asks again, and the swap starts when
//!    it can.
//! 2. The kbf at `PATH` is run for `--version --json`; its `commit_full` names the release. If
//!    that is the release serving ([`RootOrders::installed`] of `releases.json`) nothing is
//!    copied, staged or started; the copies are pruned as in step 4 (which is what removes the
//!    copy of the previous release once a swap is over) and the identity is consumed.
//! 3. Otherwise [`Copies::ensure`](super::releases::Copies::ensure) copies it into the proxy's
//!    state directory (and probes the copy), the facts of the probe are staged in
//!    `releases.json` ([`LocalOrders::stage`](super::releases::LocalOrders::stage)) and the
//!    swap is started with `nodes` off, the source `staged` and the update id
//!    `pkg-<first 12 digits of the commit>-<unix seconds>`. A start with an id the swap
//!    knows is a no-op. The identity is consumed.
//! 4. The copies the proxy no longer needs are removed
//!    ([`Copies::prune`](super::releases::Copies::prune)): all but the serving release, the
//!    release the swap goes to and the release of each slot's recorded process.
//!
//! A step that fails (the kbf does not run, prints no `commit_full`, cannot be copied, or the
//! swap refuses to start) is logged and tried again for the same identity after a wait that
//! doubles with each failure, from 10 s to [`RETRY_CAP`] (a clock set back does not stretch
//! it); the staged release a refused start left is dropped. A different identity starts over
//! without waiting. An identity a swap was started for is not acted on again, whatever becomes
//! of that swap: a new file, or a new proxy, is what asks again.
//!
//! # What it publishes
//!
//! What the watch knows is shared with the swap-state endpoint through a [`WatchStatus`]
//! ([`crate::proxy::swap_status`]):
//!
//! - **`pending`**, the commit of a changed kbf that holds still and for which no swap has been
//!   started: while the swap is busy (the kbf is then run, once for each identity, only to name
//!   its commit), and from the moment it is known not to be the release that serves until a
//!   swap is started, which includes a copy or a start that failed and waits out its backoff.
//!   The kbf that is the release that serves is not pending, busy or not. A kbf that cannot say
//!   its commit is pending as nothing, and a pass that cannot read `releases.json` fails before
//!   the kbf is known to be a change, so it is only the refusal. A kbf that waits for a swap
//!   that is already running, one the proxy resumed to that very release, is pending until that
//!   swap is over.
//! - **the refusal**, the failure to copy, stage or start the swap for that kbf, with the
//!   reason of the newest failed pass (a failed probe names no commit, so it is only logged).
//!   The retries of one kbf are one refusal: it keeps the id, the number and the time of the
//!   first failure.
//!
//! Both are dropped when the file changes or goes (they were about another file), and when the
//! swap is started or the kbf is the release that serves.
//!
//! Time comes from the [`Clock`] it is given (the wait between polls, the update id) and the
//! swap from a [`Swapper`], so a test drives [`Watch::pass`] one poll at a time.

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::io::Write as _;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::thread::{Builder, JoinHandle};
use std::time::Duration;

use super::handover_swap::Handover;
use super::kinds::{RunningUpdate, Supervisor, SupervisorError, UpdateId, UpdateSpec};
use super::migrator::probe_version;
use super::model::{Locator, Source, StagedFacts, Verb};
use super::ports::{Clock, RootOrders};
use super::releases::Local;
use crate::CommitId;
use crate::binary::Version;
use crate::run::{Runner, SystemRunner};

/// The time between polls of the binary's identity. Fixed: it is not a setting.
pub const POLL: Duration = Duration::from_secs(5);

/// The longest wait before an identity that failed is tried again.
pub const RETRY_CAP: Duration = Duration::from_secs(300);

/// The commits a process the proxy started may still be running, whose copies are kept.
pub type InUse = Box<dyn Fn() -> Vec<CommitId> + Send>;

/// A file as a poll read it: two polls with the same five values saw the same file, whole.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Identity {
    dev: u64,
    ino: u64,
    size: u64,
    mtime: i64,
    mtime_nsec: i64,
}

impl Identity {
    /// The identity of the file at `path` (links followed); `None` when it cannot be read.
    fn of(path: &Path) -> Option<Identity> {
        let meta = fs::metadata(path).ok()?;
        Some(Identity {
            dev: meta.dev(),
            ino: meta.ino(),
            size: meta.size(),
            mtime: meta.mtime(),
            mtime_nsec: meta.mtime_nsec(),
        })
    }
}

/// What the watch starts a swap through: the proxy's [`Handover`], or a fake.
pub trait Swapper: Send {
    /// Why no swap can be started now: the proxy cannot take one, or one is running or
    /// finishing. `None` when a start would be taken.
    fn busy(&self) -> Option<String>;

    /// Start the swap `spec` describes.
    ///
    /// # Errors
    /// [`SupervisorError`], as [`Supervisor::start`] gives it.
    fn start(&mut self, spec: &UpdateSpec) -> Result<(), SupervisorError>;
}

/// The words for an update that is going on.
fn describe(update: &RunningUpdate) -> String {
    format!("update {} is {:?}", update.update_id.0, update.phase)
}

impl Swapper for Handover {
    /// The supervisor's own refusal ([`Supervisor::start_blocked`]) or, since it does not
    /// count a swap that is going on, an update not yet over.
    fn busy(&self) -> Option<String> {
        self.start_blocked().or_else(|| {
            let update = self.status().update?;
            let going = !update.phase.is_terminal();
            going.then(|| describe(&update))
        })
    }

    fn start(&mut self, spec: &UpdateSpec) -> Result<(), SupervisorError> {
        Supervisor::start(self, spec)
    }
}

/// What the watch needs of the proxy: the copies and orders of `--kbf-binary` (whose `binary`
/// is the file watched), the swap and the status it publishes ([`super::assemble::local`]
/// builds all three, and gives the same status to the endpoint that reads it).
#[derive(Clone)]
pub struct Wiring {
    pub local: Local,
    pub swap: Handover,
    pub status: WatchStatus,
}

/// A swap the watch could not start: the newest one it was refused. A kbf that is refused again
/// on a retry is the same refusal, with the newest `from` and `reason`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Refusal {
    /// The id the update would have had, at the first failure.
    pub id: String,
    /// Its number ([`WatchStatus::seq_of`]).
    pub seq: u64,
    /// The release that served, when the record could be read.
    pub from: Option<CommitId>,
    /// The release the kbf at the path is.
    pub to: CommitId,
    /// Why, in the words of the newest failed pass.
    pub reason: String,
    /// When the first failure was, in seconds of the watch's clock.
    pub at: u64,
}

#[derive(Debug, Default)]
struct Seen {
    pending: Option<CommitId>,
    refusal: Option<Refusal>,
    /// The last number given to an update or a refusal.
    issued: u64,
    /// The number of each update the swap has run, by id.
    numbers: BTreeMap<String, u64>,
}

/// What the watch publishes for the swap-state endpoint, and the numbers that endpoint gives
/// to updates. A clone is the same status.
#[derive(Clone, Debug, Default)]
pub struct WatchStatus(Arc<Mutex<Seen>>);

impl WatchStatus {
    fn held(&self) -> MutexGuard<'_, Seen> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The commit of a changed kbf that has no swap started for it, if one waits.
    #[must_use]
    pub fn pending(&self) -> Option<CommitId> {
        self.held().pending.clone()
    }

    /// The newest refusal, while the kbf it was for is still the one at the path.
    #[must_use]
    pub fn refusal(&self) -> Option<Refusal> {
        self.held().refusal.clone()
    }

    /// The number of the update `id`: one more than the last number given, the first time the
    /// id is asked about, and the same number every time after. The watch asks for the
    /// updates it starts, the endpoint for the one it reports, so a number counts the updates
    /// this process has started or resumed, in the order they were first seen.
    pub fn seq_of(&self, id: &str) -> u64 {
        let mut seen = self.held();
        if let Some(number) = seen.numbers.get(id) {
            return *number;
        }
        seen.issued += 1;
        let number = seen.issued;
        seen.numbers.insert(id.to_owned(), number);
        number
    }

    /// A changed kbf waits for the swap, as `commit` (`None`: it did not name one).
    pub(super) fn pend(&self, commit: Option<CommitId>) {
        self.held().pending = commit;
    }

    /// The swap to `to` was refused for `reason` at `at` seconds. The first refusal of a `to`
    /// is given the next number; a retry of the same `to` that is refused again keeps the
    /// `id`, the number and the time of the first, and gives only its `from` and `reason`.
    pub(super) fn refuse(
        &self,
        id: String,
        from: Option<CommitId>,
        to: CommitId,
        reason: String,
        at: u64,
    ) {
        let mut seen = self.held();
        let first = seen.refusal.take().filter(|kept| kept.to == to);
        let (id, seq, at) = match first {
            Some(kept) => (kept.id, kept.seq, kept.at),
            None => {
                seen.issued += 1;
                (id, seen.issued, at)
            }
        };
        seen.refusal = Some(Refusal {
            id,
            seq,
            from,
            to,
            reason,
            at,
        });
    }

    /// Nothing waits any more: the kbf changed, or a swap was started for it.
    pub(super) fn clear(&self) {
        let mut seen = self.held();
        seen.pending = None;
        seen.refusal = None;
    }
}

/// The id of the update to `to` that is started at `secs` of the clock.
fn update_id(to: &CommitId, secs: u64) -> String {
    format!("pkg-{}-{secs}", &to.as_str()[..12])
}

/// What one poll came to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Pass {
    /// Nothing to do: the file is not read or not yet still, it was acted on already, or a
    /// failed attempt is waiting out its backoff.
    Quiet,
    /// A new kbf is waiting for the swap to be startable, for this reason.
    Busy(String),
    /// The kbf at the path is the release that serves: nothing was started, and the copies it
    /// does not need were pruned.
    Same(CommitId),
    /// The swap with this update id was started.
    Started(String),
    /// Acting on the file failed, for this reason; it is tried again later.
    Failed(String),
}

impl fmt::Display for Pass {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Pass::Quiet => write!(f, "nothing to do"),
            Pass::Busy(why) => write!(f, "a new kbf waits for the swap: {why}"),
            Pass::Same(commit) => write!(f, "{commit} is the release that serves"),
            Pass::Started(id) => write!(f, "swap {id} started"),
            Pass::Failed(why) => write!(f, "{why}; trying again later"),
        }
    }
}

/// How an identity that failed is waited on.
#[derive(Clone, Copy, Debug, Default)]
struct Retry {
    failures: u32,
    /// Not before this time of the clock.
    after: Duration,
}

impl Retry {
    /// The state after one more failure at `now`: the wait is [`POLL`] doubled once for each
    /// failure so far (10 s, 20 s, ...), at most [`RETRY_CAP`].
    fn failed(self, now: Duration) -> Retry {
        let failures = self.failures.saturating_add(1);
        let wait = POLL.saturating_mul(1 << failures.min(16));
        Retry {
            failures,
            after: now + wait.min(RETRY_CAP),
        }
    }
}

/// `what` failed with `error`: the text of a [`Pass::Failed`].
fn fail<E: fmt::Display>(what: &'static str) -> impl Fn(E) -> String {
    move |error| format!("{what}: {error}")
}

/// A line of the log, on the standard error as the rest of the verb's.
fn say(line: &str) {
    let _ = writeln!(
        std::io::stderr(),
        "kbf-installer: proxy: binary watch: {line}"
    );
}

/// The facts the swap plans by, as the build at `commit` printed them. A number that does not
/// fit is the largest one, which the swap's checks refuse as past what they understand.
fn facts_of(version: &Version, commit: &CommitId) -> StagedFacts {
    let small = |n: u64| u32::try_from(n).unwrap_or(u32::MAX);
    StagedFacts {
        commit: commit.clone(),
        protocol: small(version.protocol),
        min_protocol: small(version.min_protocol),
        state_schema: small(version.state_schema),
        profile: Some(version.profile.clone()),
    }
}

/// The watch on one file. See the module documentation.
pub struct Watch {
    local: Local,
    swap: Box<dyn Swapper>,
    clock: Arc<dyn Clock + Send + Sync>,
    in_use: InUse,
    status: WatchStatus,
    /// The identity the last poll read.
    seen: Option<Identity>,
    /// The identity that was acted on to the end.
    done: Option<Identity>,
    /// The commit the kbf at an identity named when it was run while the swap was busy; `None`
    /// for a kbf that did not say.
    named: Option<(Identity, Option<CommitId>)>,
    retry: Retry,
}

impl Watch {
    /// The watch on `local.binary`, starting swaps through `swap`. `in_use` names the commits a
    /// started process may still run, whose copies a prune keeps.
    #[must_use]
    pub fn new(
        local: Local,
        swap: Box<dyn Swapper>,
        clock: Arc<dyn Clock + Send + Sync>,
        in_use: InUse,
    ) -> Watch {
        Watch {
            local,
            swap,
            clock,
            in_use,
            status: WatchStatus::default(),
            seen: None,
            done: None,
            named: None,
            retry: Retry::default(),
        }
    }

    /// The same watch, publishing to `status` (the one the swap-state endpoint reads).
    #[must_use]
    pub fn publishing(mut self, status: WatchStatus) -> Watch {
        self.status = status;
        self
    }

    /// One poll: read the file's identity and, when it is the second equal one of an identity
    /// not acted on, act on it. `runner` runs the kbf for its version.
    pub fn pass(&mut self, runner: &dyn Runner) -> Pass {
        let now = Identity::of(&self.local.binary);
        let still = now.filter(|id| self.seen == Some(*id));
        if self.seen != now {
            self.retry = Retry::default();
            // What was published was about the file that is gone.
            self.status.clear();
        }
        self.seen = now;
        let Some(id) = still else {
            return Pass::Quiet;
        };
        let time = self.clock.now();
        // The clock is the wall clock: one set back must not stretch a wait past the cap.
        self.retry.after = self.retry.after.min(time + RETRY_CAP);
        if self.done == Some(id) || time < self.retry.after {
            return Pass::Quiet;
        }
        if let Some(why) = self.swap.busy() {
            let commit = self.name(runner, id);
            // The kbf that is the release that serves is not a change waiting.
            let serving = self.local.orders.installed().ok().flatten();
            let waiting = commit.filter(|named| serving.as_ref() != Some(named));
            self.status.pend(waiting);
            return Pass::Busy(why);
        }
        match self.act(runner) {
            Ok(pass) => {
                self.done = Some(id);
                self.retry = Retry::default();
                // Nothing waits: a swap was started, or the kbf is the release that serves.
                self.status.clear();
                pass
            }
            Err(why) => {
                self.retry = self.retry.failed(self.clock.now());
                Pass::Failed(why)
            }
        }
    }

    /// Poll every [`POLL`], logging what comes of it, until the clock is stopped (the proxy is
    /// going away). A new kbf that keeps waiting for the swap is logged once.
    pub fn run(&mut self, runner: &dyn Runner) {
        let mut last = Pass::Quiet;
        loop {
            let pass = self.pass(runner);
            match &pass {
                Pass::Quiet => {}
                Pass::Busy(_) if pass == last => {}
                other => say(&other.to_string()),
            }
            last = pass;
            if self.clock.sleep(POLL).is_err() {
                return;
            }
        }
    }

    /// The build the kbf at the path says it is: the facts it printed and the commit it names.
    fn probe(&self, runner: &dyn Runner) -> Result<(Version, CommitId), String> {
        let binary = self.local.binary.as_path();
        let version = probe_version(runner, binary).map_err(fail("probe"))?;
        let named = version.commit_full.as_deref();
        let full = named.ok_or_else(|| "the kbf prints no commit_full".to_owned())?;
        let to = CommitId::new(full).map_err(fail("commit_full"))?;
        Ok((version, to))
    }

    /// The commit the kbf with identity `id` names, run once for each identity: the swap is
    /// busy, so nothing else is done with it, and the status says which release waits.
    fn name(&mut self, runner: &dyn Runner, id: Identity) -> Option<CommitId> {
        let cached = self.named.as_ref().filter(|(seen, _)| *seen == id);
        if let Some((_, commit)) = cached {
            return commit.clone();
        }
        let commit = self.probe(runner).ok().map(|(_, commit)| commit);
        self.named = Some((id, commit.clone()));
        commit
    }

    /// Copy the kbf at the path and start the swap to it, unless it is the release that serves.
    /// Once the kbf is known not to be that release its commit is what waits, and a failure from
    /// then on is published as the swap's refusal.
    fn act(&mut self, runner: &dyn Runner) -> Result<Pass, String> {
        let (version, to) = self.probe(runner)?;
        let started = self.switch(runner, &version, &to);
        started.inspect_err(|why| self.refuse(&to, why))
    }

    /// Start the swap to `to`, the release the kbf at the path is, which `version` printed.
    fn switch(
        &mut self,
        runner: &dyn Runner,
        version: &Version,
        to: &CommitId,
    ) -> Result<Pass, String> {
        let binary = self.local.binary.as_path();
        let serving = self.local.orders.installed().map_err(fail("status"))?;
        if serving.as_ref() == Some(to) {
            self.prune(to, to);
            return Ok(Pass::Same(to.clone()));
        }
        self.status.pend(Some(to.clone()));
        let from = serving.ok_or_else(|| "no installed release is recorded".to_owned())?;
        let copied = self.local.copies.ensure(runner, binary, to);
        copied.map_err(fail("copy"))?;
        let facts = facts_of(version, to);
        self.local.orders.stage(&facts).map_err(fail("stage"))?;
        let spec = self.spec(&from, facts);
        if let Err(refused) = self.swap.start(&spec) {
            let _ = self.local.orders.revert(to);
            return Err(fail("swap")(refused));
        }
        // The update is the proxy's to number from here on, and nothing waits or stands refused
        // while the copies are pruned.
        self.status.clear();
        self.status.seq_of(&spec.update_id.0);
        self.prune(&from, to);
        Ok(Pass::Started(spec.update_id.0))
    }

    /// Publish that the swap to `to` was refused, for `why`.
    fn refuse(&self, to: &CommitId, why: &str) {
        let from = self.local.orders.installed().ok().flatten();
        let at = self.clock.now().as_secs();
        let id = update_id(to, at);
        self.status.refuse(id, from, to.clone(), why.to_owned(), at);
    }

    /// The update from `from` to the staged release `facts` describe. The locator is the
    /// order of the API's path, which the swap does not read; nothing is ordered.
    fn spec(&self, from: &CommitId, facts: StagedFacts) -> UpdateSpec {
        let to = facts.commit.clone();
        let secs = self.clock.now().as_secs();
        let id = update_id(&to, secs);
        UpdateSpec {
            update_id: UpdateId(id.clone()),
            request_id: id,
            from: Some(from.clone()),
            to: to.clone(),
            nodes: false,
            rollback: false,
            source: Source::Staged,
            order: Locator {
                order_id: 0,
                epoch: 0,
                generation: 0,
                verb: Verb::FrontendUpdate,
                commit: to,
                window_s: 0,
                source: Source::Staged,
            },
            facts,
        }
    }

    /// Remove the copies of every release but the serving one, the one the swap goes to and
    /// those of the slots' recorded processes. A failure is logged and does not fail the pass:
    /// the copies left are only space.
    fn prune(&self, serving: &CommitId, to: &CommitId) {
        let mut keep = (self.in_use)();
        keep.extend([serving.clone(), to.clone()]);
        if let Err(error) = self.local.copies.prune(&keep) {
            say(&format!("the copies of kbf could not be pruned: {error}"));
        }
    }
}

/// Start the watch on a thread of its own, named `kbf-binary-watch`, polling until `clock` is
/// stopped; `None` (nothing is spawned) without `wiring`, which is a proxy without
/// `--kbf-binary`, or when the thread cannot be started. The thread is not joined: a pass it
/// is in the middle of at the proxy's end leaves at most a `.partial-*` copy, which the next
/// start sweeps.
pub fn spawn(
    wiring: Option<Wiring>,
    clock: Arc<dyn Clock + Send + Sync>,
    in_use: InUse,
) -> Option<JoinHandle<()>> {
    let wiring = wiring?;
    let watch = Watch::new(wiring.local, Box::new(wiring.swap), clock, in_use);
    let mut watch = watch.publishing(wiring.status);
    let thread = Builder::new().name("kbf-binary-watch".into());
    thread.spawn(move || watch.run(&SystemRunner)).ok()
}

#[cfg(test)]
#[path = "binary_watch_tests.rs"]
mod tests;
