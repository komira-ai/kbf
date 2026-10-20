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
#[cfg(test)]
mod cli_observe_tests;
mod frontend;
mod frontend_addr;
#[cfg(test)]
mod frontend_addr_tests;
mod gates;
mod interrupt;
mod nodes;
pub mod observe;
pub mod offer;
mod post;
mod record;
pub mod smoke;
#[cfg(test)]
pub(super) mod smoke_tests;
mod summary;
#[cfg(test)]
mod tests;
pub mod update;
#[cfg(test)]
mod update_tests;

use std::collections::{BTreeMap, BTreeSet};
use std::fs::OpenOptions;
use std::io::Read;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::Value;

use crate::CommitId;
use api::{Operator, Reply, Request};
use gates::Gates;
pub use offer::{Attest, GhAttest, HostProbe, NoProbe, OfferPolicy, Probe, ProbeFault, Stage};
pub use summary::{Exit, NodeResult, OfferRecord, PostResult, Previous, Step, Summary};

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
    /// Ask the new frontend to update the nodes once it has baked (the update API's `nodes`).
    /// `deploy` does; `update` leaves it to the caller.
    pub nodes: bool,
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
    /// Gate E: how long after a node step the farm has to be fully replicated again.
    pub settle: Duration,
    /// Where the result document is written; with `None` none is (a unit test of the logic).
    pub result: Option<PathBuf>,
    /// The hash of the release set (`SHA256SUMS`) when the caller has it; else the document
    /// carries one derived from what the store holds.
    pub set_hash: Option<kbf_gate::SetHash>,
    /// What the deploy requires of an offer (docs/design/gamma-deploy.md, "The offer").
    pub offer: OfferPolicy,
}

impl Options {
    pub fn new(commit: CommitId, host_platform: &str, request_id: String) -> Options {
        Options {
            commit,
            request_id,
            apply: false,
            nodes: true,
            node_timeout: Duration::from_secs(1800),
            handover_timeout: Duration::from_secs(3600),
            smoke_timeout: Duration::from_secs(60),
            allow_offline: Vec::new(),
            host_platform: host_platform.to_owned(),
            poll: Duration::from_secs(5),
            members_timeout: Duration::from_secs(120),
            settle: Duration::from_secs(600),
            result: None,
            set_hash: None,
            offer: OfferPolicy::Unchecked,
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
    /// Reads the `meta_log` of the staged `kbf` (only an offer asks).
    pub probe: &'a dyn Probe,
}

/// The installer lock, held while a deploy runs. Dropping it releases it.
///
/// An `flock` belongs to the open file description, not to the descriptor, and a child
/// forked while the lock is held carries a copy until it execs, so closing the file alone
/// would leave the lock held as long as that takes. Dropping unlocks first, which no copy
/// outlives.
#[derive(Debug)]
pub struct Lock(std::fs::File);

impl Drop for Lock {
    fn drop(&mut self) {
        // An error leaves the close that follows to release the lock, as before.
        let _ = self.0.unlock();
    }
}

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
        expected: BTreeMap::new(),
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

/// The frontend step of a deploy, alone: ask the update API to swap the frontend and wait for
/// the update to end. `kbf-installer update --frontend` is this. No bundle, no preflight, no
/// node watch and no smoke: the summary holds the one step. The exit is the summary's own
/// (`Ok` for an update that is `done`, or already runs the commit).
pub fn update_frontend(env: &Env, opts: &Options) -> (Summary, Exit) {
    let mut run = Run {
        env,
        opts,
        sum: Summary::new(&opts.commit, env.clock.now()),
        expected: BTreeMap::new(),
    };
    let outcome = run.step("frontend", |r| r.update_frontend());
    run.sum.ended = summary::utc(env.clock.now());
    let (exit, reason, detail) = match outcome {
        Ok(()) => (Exit::Ok, "", String::new()),
        Err(f) => (f.exit, f.reason, f.detail),
    };
    run.sum.end(exit, reason, &detail);
    (run.sum, exit)
}

impl Run<'_> {
    fn go(&mut self, bundle: Option<&mut dyn Read>) -> Result<(), Fail> {
        if !matches!(self.opts.offer, OfferPolicy::Unchecked) {
            self.step("offer", |r| r.check_offer())?;
        }
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

    /// Step 0, when a stage is named: the offer (or the `--no-offer`) the deploy runs under.
    /// What needs the bundle is checked in step 1, before a byte is uploaded.
    fn check_offer(&mut self) -> Result<(), Fail> {
        let refused = |(reason, detail): offer::Refused| fail(Exit::Refused, reason, detail);
        self.sum.offer = self.opts.offer.record();
        match &self.opts.offer {
            OfferPolicy::Missing(stage) => Err(fail(
                Exit::Refused,
                "offer_required",
                format!(
                    "a {} deploy needs --offer offer.json (--no-offer is the deliberate exception)",
                    stage.word()
                ),
            )),
            OfferPolicy::Unchecked | OfferPolicy::Waived(_) => Ok(()),
            OfferPolicy::Offer {
                stage,
                doc,
                max_age,
            } => offer::check_header(
                *stage,
                doc,
                &self.opts.commit,
                self.env.clock.now(),
                *max_age,
            )
            .map_err(refused),
        }
    }

    /// Step 1: put the bundle's files in the store.
    fn upload(&mut self, bundle: Option<&mut dyn Read>) -> Result<(), Fail> {
        if matches!(self.opts.offer, OfferPolicy::Offer { .. }) && bundle.is_none() {
            return Err(fail(
                Exit::Refused,
                "offer_set_unverified",
                "with --offer the bundle (--bundle -) must carry the release and its SHA256SUMS",
            ));
        }
        let Some(input) = bundle else { return Ok(()) };
        let bundle = bundle::read_bundle(input, &self.opts.commit)
            .map_err(|e| fail(Exit::Usage, "bad_bundle", e))?;
        if let OfferPolicy::Offer { doc, .. } = &self.opts.offer {
            offer::check_bundle(
                doc,
                &bundle,
                &self.opts.commit,
                &self.opts.host_platform,
                self.env.probe,
            )
            .map_err(|(reason, detail)| fail(Exit::Refused, reason, detail))?;
        }
        let entries = bundle.entries;
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
        if let OfferPolicy::Offer { doc, .. } = &self.opts.offer {
            offer::check_platforms(doc, &wanted)
                .map_err(|(reason, detail)| fail(Exit::Refused, reason, detail))?;
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
        self.gates.derived_set_hash = nodes::set_hash(&releases, self.opts.commit.as_str());
        // The gates start from a farm that is healthy now: storage fully replicated, no lost
        // blob, nothing untraced. Nothing has been changed yet, so a red is a refusal.
        self.gate_step("preflight");
        self.resolve_observed("preflight", Exit::Refused)?;
        let under = self.gate_scrape(Exit::Refused)?;
        self.gate_preflight_storage(under)?;
        self.gate_snapshot(Exit::Refused)?;
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
