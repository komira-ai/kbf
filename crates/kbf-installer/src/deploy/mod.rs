//! `kbf-installer deploy --commit <sha>`: the gamma trigger (docs/design/kbf-cicd.md,
//! "The gamma stage"). It runs on the frontend host, as root, and drives the local
//! operator API through one release: stage the bytes, check the release is complete for
//! every platform the farm runs, set the desired release, wait for each node to confirm,
//! update the frontend, then check the farm.
//!
//! Everything outside this process is behind a trait ([`api::Operator`], [`Clock`],
//! [`Smoke`], [`Check`]) so a test plays the farm. The result is a [`Summary`]: one JSON
//! line, with a step per stage and the state of each node. It holds no token, no path of
//! one and no host name.
//!
//! The deploy never orders a node and never rolls back. The frontend offers the desired
//! release and the drain system rolls the nodes, one storage node at a time; the first node
//! that does not confirm stops the deploy and leaves the farm as the drain system leaves it
//! (mixed, the failed host held until a person acts).

pub mod api;
pub mod bundle;
pub mod cli;
#[cfg(test)]
mod cli_flag_tests;
#[cfg(test)]
mod cli_observe_tests;
mod frontend_addr;
#[cfg(test)]
mod frontend_addr_tests;
mod gates;
mod interrupt;
mod nodes;
pub mod observe;
mod post;
mod record;
pub mod smoke;
#[cfg(test)]
pub(super) mod smoke_tests;
mod summary;
#[cfg(test)]
mod tests;

use std::collections::BTreeSet;
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use crate::CommitId;
use api::{Operator, Reply, Request};
use gates::Gates;
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

/// Which side updates first.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Order {
    NodesFirst,
    FrontendFirst,
}

#[derive(Clone, Debug)]
pub struct Options {
    pub commit: CommitId,
    /// Without it: read-only steps, then print the plan.
    pub apply: bool,
    pub node_timeout: Duration,
    pub handover_timeout: Duration,
    pub smoke_timeout: Duration,
    pub allow_offline: Vec<String>,
    pub order: Order,
    /// The frontend host's own platform (`linux-x86_64`, ...).
    pub host_platform: String,
    /// How long to wait between polls.
    pub poll: Duration,
    /// How long members have to become active after the roll.
    pub members_timeout: Duration,
    /// Gate E: how long after a node step the farm has to be fully replicated again.
    pub settle: Duration,
    /// Where the result document is written; with `None` none is (a unit test of the logic).
    pub result: Option<PathBuf>,
    /// The hash of the release set (`SHA256SUMS`) when the caller has it; else the document
    /// carries one derived from what the store holds.
    pub set_hash: Option<kbf_gate::SetHash>,
}

impl Options {
    pub fn new(commit: CommitId, host_platform: &str) -> Options {
        Options {
            commit,
            apply: false,
            node_timeout: Duration::from_secs(1800),
            handover_timeout: Duration::from_secs(900),
            smoke_timeout: Duration::from_secs(60),
            allow_offline: Vec::new(),
            order: Order::NodesFirst,
            host_platform: host_platform.to_owned(),
            poll: Duration::from_secs(5),
            members_timeout: Duration::from_secs(120),
            settle: Duration::from_secs(600),
            result: None,
            set_hash: None,
        }
    }
}

/// What a deploy talks to.
pub struct Env<'a> {
    /// The update API: every call that reads or changes the release (`--api`).
    pub api: &'a dyn Operator,
    /// Finds the frontend the gates observe (`--observe`, `--observe-base-port`):
    /// `/metrics` and `/v1/invocations`. A different listener from `api` on a real host (the
    /// public proxy answers its own `/metrics`, and the update routes are not the child's), so
    /// a gate never reads `api`. Asked again after every frontend step.
    pub observe: &'a dyn observe::Observe,
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
    pub gates: Gates,
    /// The frontend the gates read now; set by [`Run::resolve_observed`].
    pub observed: Option<observe::Observed<'a>>,
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

    /// GET `path` from the observed frontend and parse the JSON body.
    pub fn observe_json(&self, path: &str) -> Result<Value, String> {
        let reply = self
            .observe_send(&Request::get(path))
            .map_err(|e| format!("GET {path}: {e}"))?;
        if reply.status != 200 {
            return Err(format!("GET {path}: status {}", reply.status));
        }
        reply
            .json()
            .ok_or_else(|| format!("GET {path}: the body is not JSON"))
    }

    /// Send `request` to the observed frontend (never to the update API).
    pub fn observe_send(&self, request: &Request) -> Result<Reply, String> {
        let observed = self
            .observed
            .as_ref()
            .ok_or("no frontend to observe was resolved")?;
        observed.api.send(request).map_err(|e| e.to_string())
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
///
/// An applied deploy also writes its result document (`opts.result`): `RUNNING` before the
/// first request and `FINISHED` before this function returns, whatever the end, so the caller
/// may `exit` at once. The one-line summary stays beside it until the exit codes 0 to 5 are
/// mapped to the document's statuses by the callers; it is removed on 2026-12-01 (the
/// document is the only format after that).
pub fn deploy(env: &Env, opts: &Options, bundle: Option<&mut dyn Read>) -> Summary {
    let began = env.clock.now();
    let mut run = Run {
        env,
        opts,
        sum: Summary::new(&opts.commit, began),
        gates: Gates::new(opts.apply),
        observed: None,
    };
    let record = match record::open(&run) {
        Ok(record) => record,
        Err(f) => {
            run.sum.ended = summary::utc(env.clock.now());
            run.sum.end(f.exit, f.reason, &f.detail);
            return run.sum;
        }
    };
    let mut outcome = run.go(bundle);
    // The end of the gates runs after a failed deploy too, so the document is whole.
    let ended = run.gate_finish();
    if outcome.is_ok() {
        outcome = ended;
    }
    run.sum.ended = summary::utc(env.clock.now());
    match outcome {
        Ok(()) => {}
        Err(f) => run.sum.end(f.exit, f.reason, &f.detail),
    }
    if let Some(record) = record {
        record::seal(&mut run, record, began);
    }
    run.sum
}

impl Run<'_> {
    fn go(&mut self, bundle: Option<&mut dyn Read>) -> Result<(), Fail> {
        let commit = self.opts.commit.clone();
        self.step("bundle", |r| r.upload(bundle))?;
        let platforms = self.step("preflight", |r| r.preflight())?;
        self.step("baseline", |r| r.baseline())?;
        if !self.opts.apply {
            self.sum.end(Exit::Plan, "", "");
            return Ok(());
        }
        let _ = platforms;
        if self.opts.order == Order::FrontendFirst {
            self.step("frontend", |r| r.update_frontend())?;
        }
        self.step("desired", |r| {
            r.set_desired()?;
            r.gate_checkpoint("desired")
        })?;
        self.step("nodes", |r| r.wait_nodes())?;
        if self.opts.order == Order::NodesFirst {
            self.step("frontend", |r| r.update_frontend())?;
        }
        let smoke = self.post_checks()?;
        let _ = commit;
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
    fn preflight(&mut self) -> Result<BTreeSet<String>, Fail> {
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
        let update = self.get_json("/v1/update").map_err(io)?;
        if nodes::frontend_in_flight(&update) {
            return Err(fail(
                Exit::Busy,
                "frontend_busy",
                "a frontend update is in flight",
            ));
        }
        self.gates.derived_set_hash = nodes::set_hash(&releases, self.opts.commit.as_str());
        // The gates start from a farm that is healthy now: storage fully replicated, no lost
        // blob, nothing untraced. Nothing has been changed yet, so a red is a refusal.
        self.gate_step("preflight");
        self.resolve_observed("preflight", Exit::Refused)?;
        let under = self.gate_scrape(Exit::Refused)?;
        self.gate_preflight_storage(under)?;
        self.gate_snapshot(Exit::Refused)?;
        Ok(wanted)
    }

    /// Step 3: what the farm was.
    fn baseline(&mut self) -> Result<(), Fail> {
        let io = |e: String| fail(Exit::Failed, "api_error", e);
        let update = self.get_json("/v1/update").map_err(io)?;
        self.sum.previous_desired = nodes::desired(&update);
        self.get_json("/v1/members").map_err(io)?;
        Ok(())
    }

    /// Step 4.
    fn set_desired(&mut self) -> Result<(), Fail> {
        let body = json!({ "commit": self.opts.commit.as_str() });
        let reply = self
            .send(&Request::post_json("/v1/update/desired", &body))
            .map_err(|e| fail(Exit::Failed, "api_error", e))?;
        match reply.status {
            200..=202 => {}
            409 => {
                return Err(fail(
                    Exit::Refused,
                    "release_incomplete",
                    "the frontend holds no complete platform for the commit",
                ));
            }
            s => {
                return Err(fail(
                    Exit::Failed,
                    "api_error",
                    format!("POST /v1/update/desired: status {s}"),
                ));
            }
        }
        let update = self
            .get_json("/v1/update")
            .map_err(|e| fail(Exit::Failed, "api_error", e))?;
        self.sum.generation = nodes::desired(&update).map(|d| d.generation);
        Ok(())
    }

    /// Step 6.
    fn update_frontend(&mut self) -> Result<(), Fail> {
        let body = json!({ "commit": self.opts.commit.as_str() });
        let reply = self
            .send(&Request::post_json("/v1/update/frontend", &body))
            .map_err(|e| fail(Exit::Failed, "api_error", e))?;
        match reply.status {
            200..=202 => {}
            409 | 423 => return Err(fail(Exit::Busy, "frontend_busy", "an update is in flight")),
            s => {
                return Err(fail(
                    Exit::Failed,
                    "api_error",
                    format!("POST /v1/update/frontend: status {s}"),
                ));
            }
        }
        let order_id = reply
            .json()
            .and_then(|v| v.get("order_id").and_then(Value::as_u64))
            .filter(|&id| id >= 1)
            .ok_or_else(|| {
                fail(
                    Exit::Failed,
                    "api_error",
                    "POST /v1/update/frontend: the reply names no order_id",
                )
            })?;
        self.sum.frontend = Some(summary::Frontend {
            order_id,
            state: "pending".to_owned(),
        });
        let start = self.env.clock.now();
        loop {
            // The instance behind the proxy is swapped during the handover; a request
            // that fails then is not yet an answer.
            let state = self
                .get_json("/v1/update")
                .ok()
                .and_then(|u| nodes::frontend_state(&u, order_id));
            if let Some(state) = state {
                self.sum.frontend = Some(summary::Frontend {
                    order_id,
                    state: state.clone(),
                });
                match state.as_str() {
                    "confirmed" => {
                        self.gate_step("frontend");
                        self.gate_revert(None);
                        // The old child is retired: read the one that serves now.
                        self.resolve_observed("frontend", Exit::Failed)?;
                        return self.gate_checkpoint("frontend");
                    }
                    "reverted" => {
                        self.gate_step("frontend");
                        self.gate_revert(Some("frontend"));
                        // The old child serves again; the end of the gates reads it.
                        let _ = self.resolve_observed("frontend", Exit::Failed);
                        return Err(fail(
                            Exit::Failed,
                            "frontend_reverted",
                            "the new instance never became ready; the old one is serving",
                        ));
                    }
                    _ => {}
                }
            }
            if self.env.clock.now().saturating_sub(start) >= self.opts.handover_timeout.as_secs() {
                return Err(fail(
                    Exit::Failed,
                    "frontend_timeout",
                    "the handover did not finish in time",
                ));
            }
            self.env.clock.sleep(self.opts.poll);
        }
    }
}
