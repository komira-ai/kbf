//! `kbf-installer deploy --commit <sha>`: the gamma trigger (docs/design/kbf-cicd.md,
//! "The gamma stage"). It runs on the frontend host, as root, and drives the frontend's
//! public port through one release, frontend first (docs/design/frontend-proxy.md, section
//! 9): stage the bytes, check the release is complete for every platform the farm runs,
//! ask the update API to swap the frontend and wait for its update to be `done`, then wait
//! for the nodes the new frontend rolls, and check the farm.
//!
//! Everything outside this process is behind a trait ([`api::Operator`], [`Clock`],
//! [`Smoke`], [`Check`]) so a test plays the farm. The result is a [`Summary`]: one JSON
//! line, with a step per stage and the state of each node. It holds no token, no path of
//! one and no host name.
//!
//! The deploy never orders a node, never sets the desired release and never rolls back. The
//! update of the frontend ends with the new frontend offering the release (`nodes: true`),
//! and its controller rolls the nodes one storage node at a time; the deploy only watches
//! (`GET /v1/nodes`). The first node that does not confirm stops the deploy and leaves the
//! farm as the controller leaves it (mixed, the failed host held until a person acts). A
//! frontend update that fails leaves the old frontend serving.

pub mod api;
pub mod bundle;
pub mod cli;
#[cfg(test)]
mod cli_flag_tests;
mod frontend;
mod frontend_addr;
#[cfg(test)]
mod frontend_addr_tests;
mod nodes;
mod post;
pub mod smoke;
#[cfg(test)]
pub(super) mod smoke_tests;
mod summary;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::CommitId;
use api::{Operator, Reply, Request};
pub use summary::{Exit, NodeResult, PostResult, Previous, Step, Summary};

/// Where the installer's lock is.
pub const LOCK: &str = "/run/kbf-installer/lock";

/// Time, behind a trait so a test does not wait.
pub trait Clock {
    /// Seconds since the Unix epoch.
    fn now(&self) -> u64;
    fn sleep(&self, d: Duration);
}

pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// The CAS conformance smoke on the new frontend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SmokeResult {
    Pass,
    Fail(String),
    /// Not run (the host names no REAPI listener, `--cas`). Never a pass.
    Skipped,
}

pub trait Smoke {
    fn run(&self) -> SmokeResult;
}

/// The smoke where the host names no listener: always skipped.
pub struct NoSmoke;

impl Smoke for NoSmoke {
    fn run(&self) -> SmokeResult {
        SmokeResult::Skipped
    }
}

/// What the host's own checks say before a deploy (the installer's `check`, with the
/// Envoy range). `Ok(false)` means the check is not available here.
pub trait Check {
    fn run(&self) -> Result<bool, String>;
}

/// The check where `check` is not built: reports itself skipped.
pub struct NoCheck;

impl Check for NoCheck {
    fn run(&self) -> Result<bool, String> {
        Ok(false)
    }
}

#[derive(Clone, Debug)]
pub struct Options {
    pub commit: CommitId,
    /// Names this run's frontend update to the update API: every retry of the request
    /// carries it and so starts no second update. A new run needs a new one (the API
    /// answers a request id it has seen with the update it started then).
    pub request_id: String,
    /// Without it: read-only steps, then print the plan.
    pub apply: bool,
    pub node_timeout: Duration,
    /// The whole frontend step: the POST (and its retries) and the wait for `done`. It
    /// includes the new frontend's bake.
    pub handover_timeout: Duration,
    pub smoke_timeout: Duration,
    pub allow_offline: Vec<String>,
    /// The frontend host's own platform (`linux-x86_64`, ...).
    pub host_platform: String,
    /// How long to wait between polls.
    pub poll: Duration,
    /// How long members have to become active after the roll.
    pub members_timeout: Duration,
}

impl Options {
    pub fn new(commit: CommitId, host_platform: &str, request_id: String) -> Options {
        Options {
            commit,
            request_id,
            apply: false,
            node_timeout: Duration::from_secs(1800),
            handover_timeout: Duration::from_secs(3600),
            smoke_timeout: Duration::from_secs(60),
            allow_offline: Vec::new(),
            host_platform: host_platform.to_owned(),
            poll: Duration::from_secs(5),
            members_timeout: Duration::from_secs(120),
        }
    }
}

/// What a deploy talks to.
pub struct Env<'a> {
    pub api: &'a dyn Operator,
    pub clock: &'a dyn Clock,
    pub smoke: &'a dyn Smoke,
    pub check: &'a dyn Check,
}

/// The installer lock, held while a deploy runs. Dropping it releases it.
#[derive(Debug)]
pub struct Lock(#[allow(dead_code)] std::fs::File);

/// Take the lock without waiting; `None` when another run holds it.
pub fn lock(path: &Path) -> std::io::Result<Option<Lock>> {
    // `/run` is a tmpfs: the directory is gone after a reboot.
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .open(path)?;
    take(file, std::fs::File::try_lock)
}

/// `file` locked by `try_lock`: the lock, `None` when another holds it, else the error.
fn take(
    file: std::fs::File,
    try_lock: fn(&std::fs::File) -> Result<(), std::fs::TryLockError>,
) -> std::io::Result<Option<Lock>> {
    match try_lock(&file) {
        Ok(()) => Ok(Some(Lock(file))),
        Err(std::fs::TryLockError::WouldBlock) => Ok(None),
        Err(std::fs::TryLockError::Error(e)) => Err(e),
    }
}

/// The error `what: cause`, for `map_err`.
pub(crate) fn why<E: std::fmt::Display>(what: &'static str) -> impl FnOnce(E) -> String {
    move |cause| format!("{what}: {cause}")
}

/// A failure that ends the deploy.
pub(crate) struct Fail {
    pub exit: Exit,
    pub reason: &'static str,
    pub detail: String,
}

pub(crate) fn fail(exit: Exit, reason: &'static str, detail: impl Into<String>) -> Fail {
    Fail {
        exit,
        reason,
        detail: detail.into(),
    }
}

/// Everything one run builds up.
pub(crate) struct Run<'a> {
    pub env: &'a Env<'a>,
    pub opts: &'a Options,
    pub sum: Summary,
    /// Every node the preflight saw (id to platform): the nodes the roll is waited for on.
    /// The new frontend lists only the nodes that have reconnected to it, so a list that
    /// lacks one of these is not a roll that finished.
    pub expected: BTreeMap<String, String>,
}

impl Run<'_> {
    pub fn send(&self, request: &Request) -> Result<Reply, String> {
        self.env
            .api
            .send(request)
            .map_err(|e| format!("{} {}: {e}", request.method, request.path))
    }

    pub fn get_json(&self, path: &str) -> Result<Value, String> {
        let reply = self.send(&Request::get(path))?;
        if reply.status != 200 {
            return Err(format!("GET {path}: status {}", reply.status));
        }
        reply
            .json()
            .ok_or_else(|| format!("GET {path}: the body is not JSON"))
    }

    /// Run `body` as the step `name` and record it. A skipped step is `Ok(false)`.
    fn step<T>(
        &mut self,
        name: &str,
        body: impl FnOnce(&mut Run) -> Result<T, Fail>,
    ) -> Result<T, Fail> {
        let start = self.env.clock.now();
        let result = body(self);
        let ms = self.env.clock.now().saturating_sub(start) * 1000;
        match &result {
            Ok(_) => self.sum.steps.push(Step::ok(name, ms)),
            Err(f) => self.sum.steps.push(Step::failed(name, ms, f.reason)),
        }
        result
    }
}

/// Run a deploy to its end and return the summary. `bundle` is the tar on standard input,
/// if there is one.
pub fn deploy(env: &Env, opts: &Options, bundle: Option<&mut dyn Read>) -> Summary {
    let mut run = Run {
        env,
        opts,
        sum: Summary::new(&opts.commit, env.clock.now()),
        expected: BTreeMap::new(),
    };
    let outcome = run.go(bundle);
    run.sum.ended = summary::utc(env.clock.now());
    match outcome {
        Ok(()) => {}
        Err(f) => run.sum.end(f.exit, f.reason, &f.detail),
    }
    run.sum
}

impl Run<'_> {
    fn go(&mut self, bundle: Option<&mut dyn Read>) -> Result<(), Fail> {
        self.step("bundle", |r| r.upload(bundle))?;
        self.step("preflight", |r| r.preflight())?;
        self.step("baseline", |r| r.baseline())?;
        if !self.opts.apply {
            self.sum.end(Exit::Plan, "", "");
            return Ok(());
        }
        self.step("frontend", |r| r.update_frontend())?;
        self.step("nodes", |r| r.wait_nodes())?;
        let smoke = self.post_checks()?;
        if smoke == SmokeResult::Skipped {
            self.sum.end(
                Exit::NeedsOperator,
                "smoke_skipped",
                "the CAS smoke did not run",
            );
        } else {
            self.sum.end(Exit::Ok, "", "");
        }
        Ok(())
    }

    /// Step 1: put the bundle's files in the store.
    fn upload(&mut self, bundle: Option<&mut dyn Read>) -> Result<(), Fail> {
        let Some(input) = bundle else { return Ok(()) };
        let entries = bundle::read(input, &self.opts.commit)
            .map_err(|e| fail(Exit::Usage, "bad_bundle", e))?;
        if !self.opts.apply {
            return Ok(());
        }
        for entry in &entries {
            let path = entry.path(&self.opts.commit);
            let reply = self
                .send(&Request::put_raw(&path, entry.bytes.clone()))
                .map_err(|e| fail(Exit::Failed, "upload_failed", e))?;
            match reply.status {
                200 | 201 => {}
                409 => {
                    return Err(fail(
                        Exit::Refused,
                        "upload_conflict",
                        format!(
                            "{} {}: the store holds other bytes",
                            entry.platform, entry.name
                        ),
                    ));
                }
                s => {
                    return Err(fail(
                        Exit::Failed,
                        "upload_failed",
                        format!("PUT {path}: status {s}"),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Step 2: the release is complete for every platform the farm runs, nothing is
    /// offline, no frontend update is in flight.
    fn preflight(&mut self) -> Result<(), Fail> {
        let io = |e: String| fail(Exit::Failed, "api_error", e);
        let nodes = self.get_json("/v1/nodes").map_err(io)?;
        // A reply with no `nodes` array, or an empty one, is a farm nobody confirmed anything
        // on: refuse it rather than let an empty list read as a pass.
        if nodes::list(&nodes).is_empty() {
            return Err(fail(Exit::Refused, "no_nodes", "/v1/nodes lists no node"));
        }
        let mut wanted = BTreeSet::from([self.opts.host_platform.clone()]);
        let mut unknown = Vec::new();
        let mut offline = Vec::new();
        for node in nodes::list(&nodes) {
            match node.platform {
                Some(p) => {
                    self.expected.insert(node.id.clone(), p.clone());
                    wanted.insert(p);
                }
                None => unknown.push(node.id.clone()),
            }
            if !node.connected && !self.opts.allow_offline.contains(&node.id) {
                offline.push(node.id);
            }
        }
        if !unknown.is_empty() {
            return Err(fail(
                Exit::Refused,
                "node_platform_unknown",
                format!("no platform for {}", unknown.join(", ")),
            ));
        }
        let releases = self.get_json("/v1/releases").map_err(io)?;
        let mut missing = Vec::new();
        for platform in &wanted {
            let state = nodes::platform_state(&releases, self.opts.commit.as_str(), platform);
            if state != "complete" {
                missing.push(platform.clone());
            }
            self.sum
                .platforms
                .insert(platform.clone(), state.to_owned());
        }
        if !missing.is_empty() {
            return Err(fail(
                Exit::Refused,
                "release_incomplete",
                format!("not complete: {}", missing.join(", ")),
            ));
        }
        if !offline.is_empty() {
            self.sum.nodes = offline
                .iter()
                .map(|id| NodeResult::new(id, "offline"))
                .collect();
            return Err(fail(
                Exit::Failed,
                "node_offline",
                format!("not connected: {}", offline.join(", ")),
            ));
        }
        match self.env.check.run() {
            Ok(true) => {}
            Ok(false) => self.sum.check_skipped = true,
            Err(e) => return Err(fail(Exit::Refused, "check_failed", e)),
        }
        let status = self
            .frontend_status()
            .map_err(|unseen| io(unseen.into_text()))?;
        if status.phase.is_some_and(|phase| !phase.is_terminal()) {
            return Err(fail(
                Exit::Busy,
                "frontend_busy",
                "a frontend update is in flight",
            ));
        }
        Ok(())
    }

    /// Step 3: what the farm was.
    fn baseline(&mut self) -> Result<(), Fail> {
        let io = |e: String| fail(Exit::Failed, "api_error", e);
        let update = self.get_json("/v1/update").map_err(io)?;
        self.sum.previous_desired = nodes::desired(&update);
        self.get_json("/v1/members").map_err(io)?;
        Ok(())
    }
}
