use std::io;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::SeqCst};
use std::time::Duration;

use serde_json::{Value, json};

use super::api::{Operator, Reply, Request};
use super::*;
use crate::proxy::api::model::{RollbackBlocked, StatusBody, UpdateId, UpdateRequest};
use crate::proxy::model::{Failure, Phase, Step as FailedStep};
use crate::testkit::{A, B, c};

const TARGET: &str = A;
const OLD: &str = B;

/// The farm's time in seconds, which moves only when the deploy sleeps.
#[derive(Clone, Default)]
struct Time(Arc<AtomicU64>);

impl Time {
    fn get(&self) -> u64 {
        self.0.load(SeqCst)
    }

    fn set(&self, to: u64) {
        self.0.store(to, SeqCst);
    }
}

/// The request id the options of these tests carry.
const REQUEST_ID: &str = "deploy-test";

/// Seconds after the POST that the update API shows each phase (`Farm::phase_of`).
const FRONTEND_TIMELINE: [(u64, Phase); 8] = [
    (0, Phase::Accepted),
    (5, Phase::Staged),
    (10, Phase::Starting),
    (20, Phase::Switching),
    (25, Phase::Baking),
    (35, Phase::Nodes),
    (45, Phase::Draining),
    (55, Phase::Done),
];
/// Seconds after the POST that the new frontend offers the commit (the `nodes` phase).
const OFFER_AT: u64 = 35;
/// Seconds after the POST that the update is `done`.
const DONE_AT: u64 = 55;

/// How a node ends after the desired release is set.
#[derive(Clone)]
enum Fate {
    /// Confirmed this many seconds after the desired release is set.
    Confirm(u64),
    /// Reverted by its own timer after this many seconds.
    Revert(u64),
    Refuse(&'static str, u64),
    Unsupported,
    /// Reports `confirmed`, but on the old commit.
    ConfirmedOnOld,
    Never,
}

#[derive(Clone)]
struct NodeScript {
    id: &'static str,
    platform: &'static str,
    connected: bool,
    fate: Fate,
    /// A state the node reports from an older offer (its generation) until this many seconds
    /// after the desired release is set.
    stale: Option<(&'static str, u64, u64)>,
    /// The new frontend lists the node only this many seconds after it offered the commit:
    /// the node has to reconnect to it first.
    relisted_after: Option<u64>,
}

fn node(id: &'static str, platform: &'static str, fate: Fate) -> NodeScript {
    NodeScript {
        id,
        platform,
        connected: true,
        fate,
        stale: None,
        relisted_after: None,
    }
}

/// How the update API treats the frontend update the deploy asks for.
#[derive(Clone, Copy, PartialEq)]
enum FrontendFate {
    /// Through the phases of [`FRONTEND_TIMELINE`] to `done`, offering the commit to the
    /// nodes on the way.
    Done,
    /// `failed` ten seconds after the POST, at step `start`.
    Failed,
    /// `rolled_back` twenty seconds after the POST, with no failure.
    RolledBack,
    /// Stays in `baking` for ever.
    Hang,
    /// Answers the POST with 409 `update_open`.
    Busy,
    /// Answers the POST with 409 and this code.
    Refuses(&'static str),
    /// Answers the POST with 422.
    Incomplete,
    /// `done`, but the new frontend never offers the commit to the nodes.
    NoOffer,
    /// Shows the update before ours, `done`, for this many seconds after the POST.
    Lagging(u64),
    /// Shows our update in `nodes` and never goes on.
    StuckInNodes,
}

/// How one call of the API is made to go wrong.
#[derive(Clone, Copy)]
enum Breaks {
    /// The request cannot be sent.
    Io,
    /// Answered with this status and an empty object.
    Status(u16),
    /// Answered 200 with this body.
    Body(&'static str),
    /// Carried out, and the answer lost: the connection closes without one.
    Lost,
    /// Answered with this status and this body.
    Raw(u16, &'static str),
}

/// Calls (`"METHOD path"`, as a prefix) that go wrong, from the start or only once the
/// new frontend has offered the commit, and for how many calls.
struct Broken {
    call: &'static str,
    how: Breaks,
    after_desired: bool,
    /// Only once the frontend update has been accepted.
    after_post: bool,
    /// How many calls go wrong; the rest are answered.
    times: usize,
    hits: AtomicUsize,
}

/// A farm the operator API plays, driven by a clock that only moves when the deploy sleeps.
/// It is the same farm whether the deploy calls it directly (the [`Operator`] it is) or over
/// TLS (`tls::with_tls` serves [`Farm::answer`]).
struct Farm {
    now: Time,
    log: Mutex<Vec<String>>,
    /// The farm's time of each logged call.
    times: Mutex<Vec<u64>>,
    nodes: Vec<NodeScript>,
    complete: Vec<&'static str>,
    frontend_fate: FrontendFate,
    /// Every body POSTed to the frontend update route that parsed as an update request.
    posted: Mutex<Vec<UpdateRequest>>,
    /// The farm's time of the first accepted POST: the start of update `u-1`.
    frontend_at: Mutex<Option<u64>>,
    /// The phase of the update before ours, which the status shows until ours is accepted;
    /// none when the API has run no update.
    previous: Option<Phase>,
    /// What the API answers an accepted POST with.
    accepted_status: u16,
    /// The commit has been the desired release since before the deploy started.
    offered_at_start: bool,
    /// Every body POSTed to the frontend update route, as it was sent.
    raw: Mutex<Vec<Vec<u8>>>,
    upload_status: u16,
    ready: bool,
    members_active_after: u64,
    /// `/v1/members` lists no member at all.
    no_members: bool,
    unattested: bool,
    server_commit: &'static str,
    cordoned: Option<&'static str>,
    /// `/v1/nodes` has no `nodes` key at all.
    nodes_key_missing: bool,
    /// `/v1/nodes` lists no node once the commit is offered.
    empty_after_desired: bool,
    broken: Vec<Broken>,
}

impl Broken {
    fn new(call: &'static str, how: Breaks) -> Broken {
        Broken {
            call,
            how,
            after_desired: false,
            after_post: false,
            times: usize::MAX,
            hits: AtomicUsize::new(0),
        }
    }

    /// Only once the new frontend has offered the commit.
    fn after_desired(mut self) -> Broken {
        self.after_desired = true;
        self
    }

    /// Only once the frontend update has been accepted.
    fn after_post(mut self) -> Broken {
        self.after_post = true;
        self
    }

    /// Only for the first `n` matching calls.
    fn times(mut self, n: usize) -> Broken {
        self.times = n;
        self
    }
}

impl Farm {
    fn new(now: Time, nodes: Vec<NodeScript>) -> Farm {
        Farm {
            now,
            log: Mutex::new(Vec::new()),
            times: Mutex::new(Vec::new()),
            nodes,
            complete: vec!["linux-x86_64", "darwin-arm64"],
            frontend_fate: FrontendFate::Done,
            posted: Mutex::new(Vec::new()),
            frontend_at: Mutex::new(None),
            previous: Some(Phase::Done),
            accepted_status: 202,
            offered_at_start: false,
            raw: Mutex::new(Vec::new()),
            upload_status: 201,
            ready: true,
            members_active_after: 0,
            no_members: false,
            unattested: false,
            server_commit: &TARGET[..12],
            cordoned: None,
            nodes_key_missing: false,
            empty_after_desired: false,
            broken: Vec::new(),
        }
    }

    fn requests(&self, prefix: &str) -> usize {
        self.log
            .lock()
            .unwrap()
            .iter()
            .filter(|l| l.starts_with(prefix))
            .count()
    }

    fn index_of(&self, line: &str) -> Option<usize> {
        self.log.lock().unwrap().iter().position(|l| l == line)
    }

    /// The farm's time of the first `line` after the first `after`.
    fn first_at(&self, line: &str, after: &str) -> Option<u64> {
        let start = self.index_of(after)?;
        let log = self.log.lock().unwrap();
        let at = log.iter().skip(start).position(|l| l == line)? + start;
        Some(self.times.lock().unwrap()[at])
    }

    /// Seconds since update `u-1` was accepted.
    fn since_post(&self) -> Option<u64> {
        let at = (*self.frontend_at.lock().unwrap())?;
        Some(self.now.get().saturating_sub(at))
    }

    /// The farm's time at which the new frontend offered the commit, if it has.
    fn desired_at(&self) -> Option<u64> {
        if self.offered_at_start {
            return Some(0);
        }
        let offers = matches!(
            self.frontend_fate,
            FrontendFate::Done | FrontendFate::Lagging(_) | FrontendFate::StuckInNodes
        );
        let at = (*self.frontend_at.lock().unwrap())? + OFFER_AT;
        (offers && self.now.get() >= at).then_some(at)
    }

    /// The phase update `u-1` is in `e` seconds after it was accepted.
    fn phase_at(&self, e: u64) -> (Phase, Option<Failure>) {
        let by_time = FRONTEND_TIMELINE
            .iter()
            .rev()
            .find(|(at, _)| e >= *at)
            .map_or(Phase::Accepted, |(_, phase)| *phase);
        match self.frontend_fate {
            FrontendFate::Failed if e >= 10 => (
                Phase::Failed,
                Some(Failure {
                    step: FailedStep::Start,
                    reason: "the candidate exited".to_owned(),
                }),
            ),
            FrontendFate::RolledBack if e >= 20 => (Phase::RolledBack, None),
            FrontendFate::Hang => (by_time.min(Phase::Baking), None),
            FrontendFate::StuckInNodes => (by_time.min(Phase::Nodes), None),
            _ => (by_time, None),
        }
    }

    /// `GET /v1/update/frontend`.
    fn frontend_status(&self) -> StatusBody {
        let (id, phase, failure) = match self.since_post() {
            Some(e) if !matches!(self.frontend_fate, FrontendFate::Lagging(n) if e < n) => {
                let (phase, failure) = self.phase_at(e);
                (Some("u-1"), Some(phase), failure)
            }
            _ => (self.previous.map(|_| "u-0"), self.previous, None),
        };
        StatusBody {
            update_id: id.map(|id| UpdateId(id.to_owned())),
            from: id.map(|_| c(OLD)),
            to: id.map(|_| c(TARGET)),
            phase,
            since: id.map(|_| 0),
            active_slot: None,
            restart_pending: false,
            rollback_allowed: false,
            rollback_blocked_reason: Some(RollbackBlocked::NotSwitched),
            old: None,
            new: None,
            failure,
        }
    }

    /// `POST /v1/update/frontend`, as the API answers it: a request id it has seen gets the
    /// update it started then; another while that update exists is 409.
    fn accept(&self, body: &[u8]) -> io::Result<Reply> {
        self.raw.lock().unwrap().push(body.to_vec());
        let request: UpdateRequest = match serde_json::from_slice(body) {
            Ok(request) => request,
            Err(e) => {
                return Farm::reply(
                    400,
                    json!({"error": "bad_request", "message": e.to_string()}),
                );
            }
        };
        let mut posted = self.posted.lock().unwrap();
        let known = posted.iter().any(|p| p.request_id == request.request_id);
        posted.push(request.clone());
        let refusal = match self.frontend_fate {
            FrontendFate::Busy => Some((409, "update_open")),
            FrontendFate::Refuses(code) => Some((409, code)),
            FrontendFate::Incomplete => Some((422, "incomplete_release")),
            _ if !known && self.frontend_at.lock().unwrap().is_some() => Some((409, "update_open")),
            _ => None,
        };
        if let Some((status, code)) = refusal {
            return Farm::reply(status, json!({"error": code, "message": "not now"}));
        }
        self.frontend_at
            .lock()
            .unwrap()
            .get_or_insert(self.now.get());
        Farm::reply(
            self.accepted_status,
            json!({"update_id": "u-1", "phase": "accepted"}),
        )
    }

    fn node_json(&self, n: &NodeScript) -> Value {
        let elapsed = self.desired_at().map(|t| self.now.get() - t);
        let stale = n
            .stale
            .filter(|(_, _, until)| elapsed.is_none_or(|e| e < *until));
        let (state, running, refusal, offered) = match (elapsed, &n.fate) {
            _ if stale.is_some() => {
                let (s, g, _) = stale.unwrap();
                (s, &OLD[..12], "", g)
            }
            (None, _) => ("confirmed", &OLD[..12], "", 7),
            (Some(_), Fate::ConfirmedOnOld) => ("confirmed", &OLD[..12], "", 8),
            (Some(e), Fate::Confirm(at)) if e >= *at => ("confirmed", &TARGET[..12], "", 8),
            (Some(e), Fate::Revert(at)) if e >= *at => ("reverted", &OLD[..12], "", 8),
            (Some(e), Fate::Refuse(why, at)) if e >= *at => ("refused", &OLD[..12], *why, 8),
            (Some(_), _) => ("staged", &OLD[..12], "", 8),
        };
        let availability = if matches!(n.fate, Fate::Unsupported) && elapsed.is_some() {
            "unsupported"
        } else {
            "ok"
        };
        let confirmed = state == "confirmed" && running == &TARGET[..12];
        let placement = if self.cordoned == Some(n.id) {
            "cordoned"
        } else {
            "serving"
        };
        json!({
            "node_id": n.id,
            "connected": n.connected,
            "placement": {"state": placement},
            "update": {
                "state": state, "running_commit": running, "refusal": refusal,
                "offered_generation": offered, "up_to_date": confirmed,
                "availability": availability, "platform": n.platform,
            },
        })
    }

    fn reply(status: u16, body: Value) -> io::Result<Reply> {
        Ok(Reply {
            status,
            body: body.to_string().into_bytes(),
        })
    }
}

impl Farm {
    /// The farm's answer to `r`: the effect of the call, unless the call is broken.
    fn answer(&self, r: &Request) -> io::Result<Reply> {
        self.log
            .lock()
            .unwrap()
            .push(format!("{} {}", r.method, r.path));
        self.times.lock().unwrap().push(self.now.get());
        let call = format!("{} {}", r.method, r.path);
        let broken = self.broken.iter().find(|b| {
            // A call is named whole, or by a prefix that ends in `*`.
            let named = match b.call.strip_suffix('*') {
                Some(prefix) => call.starts_with(prefix),
                None => call == b.call,
            };
            named
                && (!b.after_desired || self.desired_at().is_some())
                && (!b.after_post || self.frontend_at.lock().unwrap().is_some())
                && b.hits.fetch_add(1, SeqCst) < b.times
        });
        match broken.map(|b| b.how) {
            None => self.serve(r),
            Some(Breaks::Io) => Err(io::Error::other("the API is down")),
            Some(Breaks::Status(s)) => Farm::reply(s, json!({})),
            Some(Breaks::Raw(status, body)) => Ok(Reply {
                status,
                body: body.as_bytes().to_vec(),
            }),
            Some(Breaks::Body(text)) => Ok(Reply {
                status: 200,
                body: text.as_bytes().to_vec(),
            }),
            Some(Breaks::Lost) => {
                let _ = self.serve(r);
                Err(io::Error::other("the answer was lost"))
            }
        }
    }

    fn serve(&self, r: &Request) -> io::Result<Reply> {
        let now = self.now.get();
        match (r.method, r.path.as_str()) {
            ("PUT", _) => Farm::reply(self.upload_status, json!({"sha256": "x", "size": 1})),
            ("GET", "/v1/nodes") => {
                if self.nodes_key_missing {
                    return Farm::reply(200, json!({"server": {"commit": self.server_commit}}));
                }
                let vanished = self.empty_after_desired && self.desired_at().is_some();
                let nodes: Vec<Value> = if vanished {
                    Vec::new()
                } else {
                    self.nodes
                        .iter()
                        .filter(|n| {
                            let since = self.desired_at().map(|t| now - t);
                            n.relisted_after
                                .is_none_or(|after| since.is_none_or(|s| s >= after))
                        })
                        .map(|n| self.node_json(n))
                        .collect()
                };
                Farm::reply(
                    200,
                    json!({"server": {"commit": self.server_commit}, "nodes": nodes}),
                )
            }
            ("GET", "/v1/releases") => {
                let platforms: Vec<Value> = self
                    .complete
                    .iter()
                    .map(|p| {
                        json!({"platform": p, "complete": true, "components": [
                            {"name": "kbf", "sha256": "x", "size": 1, "attested": !self.unattested}]})
                    })
                    .collect();
                Farm::reply(
                    200,
                    json!({"releases": [{"commit": TARGET, "platforms": platforms}]}),
                )
            }
            ("GET", "/v1/members") => {
                let state = if now >= self.members_active_after {
                    "active"
                } else {
                    "catching_up"
                };
                let members = if self.no_members {
                    json!([])
                } else {
                    json!([{"id": 1, "state": state}])
                };
                Farm::reply(200, json!({"generation": 3, "members": members}))
            }
            ("GET", "/v1/update") => {
                let desired = if self.desired_at().is_some() {
                    json!({"commit": TARGET, "generation": 8})
                } else {
                    json!({"commit": OLD, "generation": 7})
                };
                Farm::reply(200, json!({"desired": desired}))
            }
            ("GET", "/v1/update/frontend") => {
                // A newer API may say more than this client reads.
                let mut status = serde_json::to_value(self.frontend_status()).unwrap();
                status["a_later_field"] = json!({"x": 1});
                Farm::reply(200, status)
            }
            ("POST", "/v1/update/frontend") => self.accept(&r.body),
            // The deploy never does this: the new frontend offers the commit. The farm
            // takes it, so a deploy that does is seen doing it.
            ("POST", "/v1/update/desired") => Farm::reply(200, json!({})),
            ("GET", "/readyz") => Farm::reply(if self.ready { 200 } else { 503 }, json!({})),
            _ => Farm::reply(404, json!({})),
        }
    }
}

impl Operator for Farm {
    fn send(&self, r: &Request) -> io::Result<Reply> {
        self.answer(r)
    }
}

struct Clk(Time);

impl Clock for Clk {
    fn now(&self) -> u64 {
        1_790_000_000 + self.0.get()
    }

    fn sleep(&self, d: Duration) {
        self.0.set(self.0.get() + d.as_secs());
    }
}

struct FixedSmoke(SmokeResult);

impl Smoke for FixedSmoke {
    fn run(&self) -> SmokeResult {
        self.0.clone()
    }
}

struct FixedCheck(Result<bool, String>);

impl Check for FixedCheck {
    fn run(&self) -> Result<bool, String> {
        self.0.clone()
    }
}

fn opts(apply: bool) -> Options {
    let mut o = Options::new(c(TARGET), "linux-x86_64", REQUEST_ID.to_owned());
    o.apply = apply;
    o.node_timeout = Duration::from_secs(1800);
    o
}

fn two_nodes() -> Vec<NodeScript> {
    vec![
        node("a-store", "linux-x86_64", Fate::Confirm(100)),
        node("b-mac", "darwin-arm64", Fate::Confirm(200)),
    ]
}

fn run_with(
    farm: &Farm,
    o: &Options,
    smoke: SmokeResult,
    bundle: Option<&mut dyn io::Read>,
) -> Summary {
    let clock = Clk(farm.now.clone());
    let smoke = FixedSmoke(smoke);
    let check = FixedCheck(Ok(true));
    let env = Env {
        api: farm,
        clock: &clock,
        smoke: &smoke,
        check: &check,
    };
    deploy(&env, o, bundle)
}

fn go(farm: &Farm) -> Summary {
    run_with(farm, &opts(true), SmokeResult::Pass, None)
}

fn farm(nodes: Vec<NodeScript>) -> Farm {
    Farm::new(Time::default(), nodes)
}

mod gaps;
mod tls;
mod update_api;

use tls::run_tls;

// ---- the whole deploy --------------------------------------------------------------

/// Proves the happy path ends 0 with every step, node and check recorded, the frontend
/// updated first and the nodes only watched. Catches a deploy that skips a step, reports a
/// node it never saw confirm, orders a node, or sets the desired release itself.
#[test]
fn a_clean_deploy_updates_the_frontend_first_then_watches_the_nodes_and_checks_the_farm() {
    let f = farm(two_nodes());
    let s = go(&f);
    assert_eq!(
        (s.exit, s.result.as_str(), s.reason.as_str()),
        (0, "ok", ""),
        "{}",
        s.line()
    );
    let names: Vec<_> = s.steps.iter().map(|x| x.name.as_str()).collect();
    assert_eq!(
        names,
        ["bundle", "preflight", "baseline", "frontend", "nodes"]
    );
    assert_eq!(
        s.previous_desired,
        Some(Previous {
            commit: OLD.into(),
            generation: 7
        })
    );
    assert_eq!(s.generation, Some(8));
    assert!(s.nodes.iter().all(|n| n.state == "confirmed"));
    assert_eq!(s.nodes.len(), 2);
    assert_eq!(
        s.frontend,
        Some(summary::Frontend {
            update_id: Some("u-1".to_owned()),
            phase: Some(Phase::Done),
            failure: None,
            already_running: false,
        })
    );
    assert_eq!(s.post.cas_smoke.as_deref(), Some("pass"));
    assert_eq!(s.post.members_active, Some(true));
    assert_eq!(s.post.nodes_on_commit, Some(true));
    assert_eq!(s.post.readyz, Some(true));
    // One request, the one the design names, and nothing that sets the desired release or
    // orders a node.
    assert_eq!(
        *f.posted.lock().unwrap(),
        [UpdateRequest {
            commit: c(TARGET),
            request_id: REQUEST_ID.to_owned(),
            nodes: true,
            rollback: false,
        }]
    );
    assert_eq!(f.requests("POST"), 1);
    assert_eq!(f.requests("POST /v1/update/desired"), 0);
    assert_eq!(f.requests("POST /v1/nodes"), 0);
    // The frontend is updated, and `done`, before the nodes are looked at again.
    assert!(
        f.first_at("GET /v1/nodes", "POST /v1/update/frontend") >= Some(DONE_AT),
        "{:?}",
        f.log.lock().unwrap()
    );
    let json: Value = serde_json::from_str(&s.line()).unwrap();
    assert_eq!(json["schema"], 2);
    assert_eq!(json["started"], "2026-09-21T14:13:20Z");
    assert_eq!(json["frontend"]["update_id"], "u-1");
    assert_eq!(json["frontend"]["phase"], "done");
    assert!(json["frontend"].get("failure").is_none());
    assert!(json["frontend"].get("already_running").is_none());
}

/// Proves that without `--apply` nothing is written. Catches a plan that mutates.
#[test]
fn without_apply_nothing_is_written_and_the_result_is_a_plan() {
    let f = farm(two_nodes());
    let bundle = tarball(&[(&format!("kbf-{TARGET}-linux-x86_64.tar.gz"), b"x")]);
    let mut input: &[u8] = &bundle;
    let s = run_with(&f, &opts(false), SmokeResult::Pass, Some(&mut input));
    assert_eq!((s.exit, s.result.as_str()), (0, "plan"), "{}", s.line());
    assert_eq!(f.requests("POST"), 0);
    assert_eq!(f.requests("PUT"), 0);
    assert!(s.nodes.is_empty());
}

// ---- step 1: the bundle --------------------------------------------------------------

fn tarball(files: &[(&str, &[u8])]) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for (name, bytes) in files {
        let mut h = tar::Header::new_gnu();
        h.set_size(bytes.len() as u64);
        h.set_mode(0o644);
        h.set_cksum();
        b.append_data(&mut h, name, *bytes).unwrap();
    }
    b.into_inner().unwrap()
}

/// Proves each file goes to the store path its name gives, before anything else is asked.
#[test]
fn the_bundle_is_uploaded_to_the_store_paths() {
    let f = farm(two_nodes());
    let bundle = tarball(&[
        (&format!("kbf-{TARGET}-linux-x86_64.tar.gz"), b"t"),
        (
            &format!("kbf-{TARGET}-linux-x86_64.tar.gz.attestation"),
            b"a",
        ),
        (&format!("kbf-daemon-{TARGET}-darwin-arm64.tar.gz"), b"t"),
        (
            &format!("kbf-daemon-{TARGET}-darwin-arm64.tar.gz.attestation"),
            b"a",
        ),
    ]);
    let mut input: &[u8] = &bundle;
    let s = run_with(&f, &opts(true), SmokeResult::Pass, Some(&mut input));
    assert_eq!(s.exit, 0, "{}", s.line());
    let log = f.log.lock().unwrap();
    for want in [
        format!("PUT /v1/releases/{TARGET}/linux-x86_64/kbf"),
        format!("PUT /v1/releases/{TARGET}/linux-x86_64/kbf.attestation"),
        format!("PUT /v1/releases/{TARGET}/darwin-arm64/kbf-daemon"),
        format!("PUT /v1/releases/{TARGET}/darwin-arm64/kbf-daemon.attestation"),
    ] {
        assert!(log.contains(&want), "{want} in {log:?}");
    }
    assert!(log[..4].iter().all(|l| l.starts_with("PUT")));
}

/// Proves a store that holds other bytes for the same file refuses the deploy (a release
/// is immutable) and nothing is set.
#[test]
fn an_upload_the_store_answers_409_to_is_refused() {
    let mut f = farm(two_nodes());
    f.upload_status = 409;
    let bundle = tarball(&[(&format!("kbf-{TARGET}-linux-x86_64.tar.gz"), b"t")]);
    let mut input: &[u8] = &bundle;
    let s = run_with(&f, &opts(true), SmokeResult::Pass, Some(&mut input));
    assert_eq!((s.exit, s.reason.as_str()), (3, "upload_conflict"));
    assert_eq!(f.requests("POST"), 0);
}

#[test]
fn an_upload_that_fails_is_a_failure() {
    let mut f = farm(two_nodes());
    f.upload_status = 503;
    let bundle = tarball(&[(&format!("kbf-{TARGET}-linux-x86_64.tar.gz"), b"t")]);
    let mut input: &[u8] = &bundle;
    let s = run_with(&f, &opts(true), SmokeResult::Pass, Some(&mut input));
    assert_eq!((s.exit, s.reason.as_str()), (1, "upload_failed"));
}

/// Proves a mistake in the bundle is a usage error before any byte is uploaded.
#[test]
fn a_bundle_with_a_file_the_store_would_not_take_uploads_nothing() {
    let f = farm(two_nodes());
    let bundle = tarball(&[
        (&format!("kbf-{TARGET}-linux-x86_64.tar.gz"), b"t"),
        (&format!("kbf-server-{TARGET}-linux-x86_64.tar.gz"), b"t"),
    ]);
    let mut input: &[u8] = &bundle;
    let s = run_with(&f, &opts(true), SmokeResult::Pass, Some(&mut input));
    assert_eq!((s.exit, s.reason.as_str()), (2, "bad_bundle"));
    assert_eq!(f.requests("PUT"), 0);
}

#[test]
fn the_bundle_reader_refuses_what_is_not_this_commit_or_not_a_file_once() {
    let commit = c(TARGET);
    let name = |n: &str| format!("{n}-{TARGET}-linux-arm64.tar.gz");
    let read = |files: &[(&str, &[u8])]| bundle::read(&tarball(files)[..], &commit);
    assert!(read(&[(&name("kbf"), b"x")]).is_ok());
    assert!(
        read(&[(
            "kbf-0123456789abcdef0123456789abcdef01234567-linux-arm64.tar.gz",
            b"x"
        )])
        .is_err()
    );
    assert!(read(&[(&format!("kbf-{TARGET}-windows.tar.gz"), b"x")]).is_err());
    assert!(read(&[(&name("kbf"), b"x"), (&name("kbf"), b"y")]).is_err());
    assert!(read(&[(&name("kbf"), b"")]).is_err());
    assert!(read(&[("notes.txt", b"x")]).is_err());
    assert!(read(&[]).is_err());
    assert!(bundle::read(&b"not a tar at all"[..], &commit).is_err());
    let big = vec![0u8; (bundle::MAX_ATTESTATION + 1) as usize];
    assert!(read(&[(&format!("{}.attestation", name("kbf")), &big)]).is_err());
}

// ---- step 2: preflight ---------------------------------------------------------------

/// Proves a platform some node runs, without a complete release, refuses the deploy before
/// the desired release is set. Catches a preflight that only checks the host's platform.
#[test]
fn preflight_refuses_a_platform_the_release_is_not_complete_for() {
    let mut f = farm(two_nodes());
    f.complete = vec!["linux-x86_64"];
    let s = go(&f);
    assert_eq!(
        (s.exit, s.reason.as_str()),
        (3, "release_incomplete"),
        "{}",
        s.line()
    );
    assert!(s.detail.contains("darwin-arm64"));
    assert_eq!(s.platforms["darwin-arm64"], "missing");
    assert_eq!(f.requests("POST"), 0);
}

/// Proves the frontend host's own platform is required even with no node on it.
#[test]
fn the_frontend_hosts_own_platform_must_be_complete_too() {
    let mut f = farm(vec![node("b-mac", "darwin-arm64", Fate::Confirm(1))]);
    f.complete = vec!["darwin-arm64"];
    let s = go(&f);
    assert_eq!((s.exit, s.reason.as_str()), (3, "release_incomplete"));
    assert!(s.detail.contains("linux-x86_64"));
}

/// Proves a component without its attestation does not count as complete.
#[test]
fn a_component_without_an_attestation_is_not_complete() {
    let mut f = farm(two_nodes());
    f.unattested = true;
    let s = go(&f);
    assert_eq!((s.exit, s.reason.as_str()), (3, "release_incomplete"));
    assert_eq!(s.platforms["linux-x86_64"], "incomplete");
}

#[test]
fn a_node_with_no_known_platform_is_refused() {
    let mut f = farm(two_nodes());
    f.nodes.push(node("c-odd", "plan9-mips", Fate::Never));
    let s = go(&f);
    assert_eq!((s.exit, s.reason.as_str()), (3, "node_platform_unknown"));
}

/// Proves a farm that lists no node, or whose `/v1/nodes` has no `nodes` array, is refused
/// before anything is set: an empty list must not read as "every node is fine". Catches a
/// renamed or broken field giving a green deploy that never confirmed a node.
#[test]
fn a_farm_that_reports_no_nodes_is_refused_before_anything_is_set() {
    let f = farm(Vec::new());
    let s = go(&f);
    assert_eq!((s.exit, s.reason.as_str()), (3, "no_nodes"));
    assert_eq!(f.requests("POST"), 0);
    let mut f = farm(two_nodes());
    f.nodes_key_missing = true;
    let s = go(&f);
    assert_eq!((s.exit, s.reason.as_str()), (3, "no_nodes"));
    assert_eq!(f.requests("POST"), 0);
}

/// Proves the wait does not finish on an empty node list (the list read after the commit is
/// offered), and times out instead. Catches `all()` on an empty list passing.
#[test]
fn an_empty_node_list_while_waiting_is_not_every_node_confirmed() {
    let mut f = farm(two_nodes());
    f.empty_after_desired = true;
    let s = go(&f);
    assert_eq!((s.exit, s.reason.as_str()), (1, "node_not_confirmed"));
    assert_eq!(f.requests("POST /v1/update/frontend"), 1);
}

/// Proves the post check needs at least one node on the commit.
#[test]
fn the_post_check_needs_a_node() {
    let v = json!({"server": {"commit": &TARGET[..12]}, "nodes": []});
    assert!(!super::post::nodes_on_commit(&v, TARGET));
    let v = json!({"server": {"commit": &TARGET[..12]}});
    assert!(!super::post::nodes_on_commit(&v, TARGET));
}

/// Proves an offline node fails the deploy before anything is set, and that listing it in
/// `--allow-offline` lets the rest go on.
#[test]
fn an_offline_node_fails_the_deploy_unless_it_is_allowed() {
    let mut off = node("c-off", "linux-x86_64", Fate::Never);
    off.connected = false;
    let mut nodes = two_nodes();
    nodes.push(off);
    let f = farm(nodes.clone());
    let s = go(&f);
    assert_eq!((s.exit, s.reason.as_str()), (1, "node_offline"));
    assert_eq!(s.nodes[0].state, "offline");
    assert_eq!(f.requests("POST"), 0);

    let f = farm(nodes);
    let mut o = opts(true);
    o.allow_offline = vec!["c-off".to_owned()];
    let s = run_with(&f, &o, SmokeResult::Pass, None);
    // The allowed node is listed offline; the deploy's post check still demands every
    // node connected, so the allowance does not hide it.
    assert_eq!(
        s.nodes.iter().find(|n| n.id == "c-off").unwrap().state,
        "offline"
    );
    assert_eq!(s.post.nodes_on_commit, Some(false));
    assert_eq!((s.exit, s.reason.as_str()), (1, "postcheck_failed"));
}

#[test]
fn a_failed_installer_check_refuses_and_an_unavailable_one_is_recorded() {
    let f = farm(two_nodes());
    let clock = Clk(f.now.clone());
    let smoke = FixedSmoke(SmokeResult::Pass);
    let bad = FixedCheck(Err("envoy is out of range".into()));
    let env = Env {
        api: &f,
        clock: &clock,
        smoke: &smoke,
        check: &bad,
    };
    let s = deploy(&env, &opts(true), None);
    assert_eq!((s.exit, s.reason.as_str()), (3, "check_failed"));
    assert_eq!(f.requests("POST"), 0);

    let f = farm(two_nodes());
    let clock = Clk(f.now.clone());
    let none = NoCheck;
    let env = Env {
        api: &f,
        clock: &clock,
        smoke: &smoke,
        check: &none,
    };
    let s = deploy(&env, &opts(true), None);
    assert!(s.check_skipped && s.exit == 0);
}

/// Proves a frontend update already in flight is exit 4 and nothing is set.
#[test]
fn a_frontend_update_in_flight_is_busy() {
    let mut f = farm(two_nodes());
    f.previous = Some(Phase::Baking);
    let s = go(&f);
    assert_eq!((s.exit, s.reason.as_str()), (4, "frontend_busy"));
    assert_eq!(f.requests("POST"), 0);
}

// ---- step 5: nodes -------------------------------------------------------------------

/// Proves a node that never confirms times out and the deploy stops there, after the frontend
/// update (which the nodes follow) and before any post check.
#[test]
fn a_node_that_never_confirms_times_out_after_the_frontend_was_updated() {
    let f = farm(vec![
        node("a-store", "linux-x86_64", Fate::Confirm(100)),
        node("b-store", "linux-x86_64", Fate::Never),
    ]);
    let mut o = opts(true);
    o.node_timeout = Duration::from_secs(600);
    let s = run_with(&f, &o, SmokeResult::Pass, None);
    assert_eq!(
        (s.exit, s.reason.as_str()),
        (1, "node_not_confirmed"),
        "{}",
        s.line()
    );
    let by: Vec<_> = s
        .nodes
        .iter()
        .map(|n| (n.id.as_str(), n.state.as_str()))
        .collect();
    assert_eq!(by, [("a-store", "confirmed"), ("b-store", "timed_out")]);
    assert_eq!(f.requests("POST /v1/update/frontend"), 1);
    assert_eq!(s.frontend.unwrap().phase, Some(Phase::Done));
    assert!(s.post.readyz.is_none());
}

/// Proves the first node that does not confirm stops the deploy: the node after it is not
/// waited for and shows `pending`, even though it would have confirmed. Catches a loop
/// that carries on to the next node.
#[test]
fn the_first_unconfirmed_node_stops_the_deploy_and_later_nodes_are_not_waited_for() {
    let f = farm(vec![
        node("a-store", "linux-x86_64", Fate::Revert(50)),
        node("b-store", "linux-x86_64", Fate::Confirm(500)),
        node("c-mac", "darwin-arm64", Fate::Confirm(900)),
    ]);
    let s = go(&f);
    assert_eq!((s.exit, s.reason.as_str()), (1, "node_not_confirmed"));
    let by: Vec<_> = s
        .nodes
        .iter()
        .map(|n| (n.id.as_str(), n.state.as_str()))
        .collect();
    assert_eq!(
        by,
        [
            ("a-store", "reverted"),
            ("b-store", "pending"),
            ("c-mac", "pending")
        ]
    );
    // Stopped when the first failed, not after the rest would have confirmed.
    assert!(f.now.get() < DONE_AT + 100, "waited {} s", f.now.get());
    assert!(s.detail.contains("a-store"));
}

#[test]
fn a_refusal_and_an_unsupported_node_are_named() {
    let f = farm(vec![node(
        "a-store",
        "linux-x86_64",
        Fate::Refuse("floor", 20),
    )]);
    let s = go(&f);
    assert_eq!(s.nodes[0].state, "refused");
    assert_eq!(s.nodes[0].refusal, "floor");
    let f = farm(vec![node("a-store", "linux-x86_64", Fate::Unsupported)]);
    let s = go(&f);
    assert_eq!(s.nodes[0].state, "unsupported");
    assert_eq!(s.exit, 1);
}

/// Proves each node gets its own budget, started when the one before it confirms: two
/// nodes that confirm 980 s and then 1000 s into the wait pass a 1500 s budget. Catches one
/// shared clock for all nodes.
#[test]
fn each_node_has_its_own_budget() {
    let f = farm(vec![
        node("a-store", "linux-x86_64", Fate::Confirm(1000)),
        node("b-store", "linux-x86_64", Fate::Confirm(2000)),
    ]);
    let mut o = opts(true);
    o.node_timeout = Duration::from_secs(1500);
    let s = run_with(&f, &o, SmokeResult::Pass, None);
    assert_eq!(s.exit, 0, "{}", s.line());
    // The wait starts when the update is `done`, 20 s after the commit was offered.
    let secs: Vec<_> = s.nodes.iter().map(|n| n.secs).collect();
    assert!((975..=990).contains(&secs[0]), "{secs:?}");
    assert!((995..=1010).contains(&secs[1]), "{secs:?}");
}

/// Proves a `reverted` left by an older offer is not this deploy's verdict.
#[test]
fn a_state_left_from_an_older_offer_is_not_this_deploys() {
    let mut n = node("a-store", "linux-x86_64", Fate::Confirm(100));
    n.stale = Some(("reverted", 7, 40));
    let f = farm(vec![n]);
    // For 40 s after the commit is offered the node still reports the state of the offer
    // before it (generation 7), 20 s of them while the deploy waits; that is not this
    // deploy's verdict.
    let s = go(&f);
    assert_eq!(s.exit, 0, "{}", s.line());
}

/// Proves `confirmed` on another commit is not a confirmation of this one.
#[test]
fn a_node_confirmed_on_another_commit_is_not_confirmed() {
    let f = farm(vec![node("a-store", "linux-x86_64", Fate::ConfirmedOnOld)]);
    let mut o = opts(true);
    o.node_timeout = Duration::from_secs(60);
    let s = run_with(&f, &o, SmokeResult::Pass, None);
    assert_eq!((s.exit, s.nodes[0].state.as_str()), (1, "timed_out"));
}

// ---- step 7: post checks ---------------------------------------------------------------

/// Proves every post check runs after one fails, and any failure is exit 1.
#[test]
fn a_failed_post_check_is_exit_1_and_every_check_is_recorded() {
    let mut f = farm(two_nodes());
    f.cordoned = Some("a-store");
    f.ready = false;
    let s = go(&f);
    assert_eq!((s.exit, s.reason.as_str()), (1, "postcheck_failed"));
    assert_eq!(s.post.nodes_on_commit, Some(false));
    assert_eq!(s.post.readyz, Some(false));
    assert_eq!(s.post.members_active, Some(true));
    assert_eq!(s.post.cas_smoke.as_deref(), Some("pass"));
}

#[test]
fn a_server_not_on_the_commit_fails_the_post_check() {
    let mut f = farm(two_nodes());
    f.server_commit = &OLD[..12];
    let s = go(&f);
    assert_eq!(s.post.nodes_on_commit, Some(false));
    assert_eq!(s.exit, 1);
}

/// Proves a members list with no member fails the post check: nothing is active, so
/// nothing was checked. Catches `all_active` reading an empty list as "all active".
#[test]
fn an_empty_members_list_fails_the_post_check() {
    let mut f = farm(two_nodes());
    f.no_members = true;
    let s = go(&f);
    assert_eq!(
        (s.exit, s.reason.as_str(), s.post.members_active),
        (1, "postcheck_failed", Some(false))
    );
}

/// A clock that steps back once, 1000 s, the first time a sleep brings the farm's time to
/// `at` seconds (a step of the clock, as an NTP correction makes).
struct StepBack {
    farm: Time,
    at: u64,
    skew: Time,
}

impl Clock for StepBack {
    fn now(&self) -> u64 {
        1_790_000_000 + self.farm.get() - self.skew.get()
    }

    fn sleep(&self, d: Duration) {
        self.farm.set(self.farm.get() + d.as_secs());
        if self.farm.get() >= self.at {
            self.skew.set(1000);
        }
    }
}

fn with_step_back(farm: &Farm, at: u64) -> Summary {
    let clock = StepBack {
        farm: farm.now.clone(),
        at,
        skew: Time::default(),
    };
    let smoke = FixedSmoke(SmokeResult::Pass);
    let check = FixedCheck(Ok(true));
    let env = Env {
        api: farm,
        clock: &clock,
        smoke: &smoke,
        check: &check,
    };
    deploy(&env, &opts(true), None)
}

/// Proves a clock that steps back while the frontend update is followed, or while nodes are
/// awaited, does not panic the deploy (the elapsed time reads 0). Catches `now - start` on
/// u64 in the frontend step, and `now - last` in `wait_nodes`.
#[test]
fn a_clock_that_steps_back_while_the_frontend_or_the_nodes_are_awaited_does_not_panic() {
    for at in [10, DONE_AT + 5] {
        let f = farm(two_nodes());
        let s = with_step_back(&f, at);
        assert_eq!(s.exit, 0, "{at}: {}", s.line());
    }
}

/// The same while the members are awaited. Catches `now - start` on u64 in `post_checks`.
#[test]
fn a_clock_that_steps_back_while_members_are_awaited_does_not_panic() {
    let mut f = farm(two_nodes());
    f.members_active_after = 100_000;
    let s = with_step_back(&f, 250);
    assert_eq!(
        (s.exit, s.post.members_active),
        (1, Some(false)),
        "{}",
        s.line()
    );
}

/// Proves members get a window to catch up, and fail after it.
#[test]
fn members_that_are_catching_up_are_waited_for_and_then_fail() {
    let mut f = farm(two_nodes());
    f.members_active_after = 300; // the roll ends around 235 s
    let s = go(&f);
    assert_eq!(s.exit, 0, "{}", s.line());
    let mut f = farm(two_nodes());
    f.members_active_after = 100_000;
    let s = go(&f);
    assert_eq!((s.exit, s.post.members_active), (1, Some(false)));
}

/// Proves a smoke that did not run is exit 5, never 0, and a smoke that failed is exit 1.
#[test]
fn a_skipped_smoke_needs_an_operator_and_a_failed_one_fails() {
    let f = farm(two_nodes());
    let s = run_with(&f, &opts(true), SmokeResult::Skipped, None);
    assert_eq!(
        (s.exit, s.result.as_str(), s.reason.as_str()),
        (5, "needs_operator", "smoke_skipped")
    );
    assert_eq!(s.post.cas_smoke.as_deref(), Some("skipped"));
    let f = farm(two_nodes());
    let s = run_with(&f, &opts(true), SmokeResult::Fail("blob lost".into()), None);
    assert_eq!((s.exit, s.post.cas_smoke.as_deref()), (1, Some("fail")));
    assert!(s.detail.contains("blob lost"));
    assert_eq!(NoSmoke.run(), SmokeResult::Skipped);
}

// ---- the lock --------------------------------------------------------------------------

/// Proves a second deploy while one holds the lock is refused with "held", and gets in once
/// the first is dropped.
#[test]
fn a_second_deploy_cannot_take_the_lock() {
    let (_, dir) = crate::testkit::host_root("deploy-lock");
    let path = dir.join("run/lock");
    let first = lock(&path).unwrap();
    assert!(first.is_some());
    assert!(lock(&path).unwrap().is_none());
    drop(first);
    assert!(lock(&path).unwrap().is_some());
    // The lock's directory is made when missing (`/run` is a tmpfs), but not under a file.
    assert!(lock(&dir.join("new/dir/lock")).unwrap().is_some());
    std::fs::write(dir.join("plain"), "x").unwrap();
    assert!(lock(&dir.join("plain/lock")).is_err());
}

// ---- the client ------------------------------------------------------------------------

mod client;

#[test]
fn utc_names_the_date() {
    assert_eq!(summary::utc(0), "1970-01-01T00:00:00Z");
    assert_eq!(summary::utc(951_782_400 + 86_399), "2000-02-29T23:59:59Z");
}

// ---- the command line ------------------------------------------------------------------

mod cli_tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    use super::super::cli;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    fn run(list: &[&str]) -> (i32, String, String) {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = cli::main_as(&args(list), &mut &b""[..], &mut out, &mut err, false);
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    fn token(name: &str, mode: u32, body: &str) -> std::path::PathBuf {
        let (_, dir) = crate::testkit::host_root(name);
        let path = dir.join("token");
        fs::write(&path, body).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(mode)).unwrap();
        path
    }

    /// Proves a caller that came through sudo cannot aim the deploy: each of --api,
    /// --token-file, --config, --lock, --platform and --cas is a usage error (exit 2) before the
    /// token is read or anything is sent, and the flags the workflow sends are not. Catches
    /// the deploy account sending a root-readable file as a bearer token to its own listener.
    #[test]
    fn through_sudo_the_host_flags_are_refused() {
        let commit = "a".repeat(40);
        for (flag, value) in [
            ("--api", "127.0.0.1:1"),
            ("--api-ca", "/etc/shadow"),
            ("--api-server-name", "farm.example"),
            ("--token-file", "/etc/shadow"),
            ("--config", "/var/tmp/x/deploy.json"),
            ("--lock", "/var/tmp/x/lock"),
            ("--platform", "linux-x86_64"),
            ("--cas", "http://127.0.0.1:1"),
        ] {
            let list = args(&["--commit", &commit, flag, value]);
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let code = cli::main_as(&list, &mut &b""[..], &mut out, &mut err, true);
            let err = String::from_utf8(err).unwrap();
            assert_eq!(code, 2, "{flag}");
            assert!(out.is_empty(), "{flag}");
            assert!(err.contains("not accepted through sudo"), "{flag}: {err}");
        }
        // The argv the workflow sends gets past the flag check (it stops later, for want of
        // a config on this machine, with a different message).
        let list = args(&["--apply", "--json", "--commit", &commit, "--bundle", "-"]);
        let (mut out, mut err) = (Vec::new(), Vec::new());
        cli::main_as(&list, &mut &b""[..], &mut out, &mut err, true);
        assert!(!String::from_utf8(err).unwrap().contains("through sudo"));
    }

    const GOOD: &str = "0123456789abcdef0123456789abcdef0123456789abcdef\n";

    #[test]
    fn a_bad_command_line_is_a_usage_error_with_nothing_on_stdout() {
        for bad in [
            &["--commit", "abc"][..],
            &["--api", "127.0.0.1:1"],
            &[
                "--commit",
                super::TARGET,
                "--api",
                "nonsense",
                "--token-file",
                "/x",
            ],
            &["--commit", super::TARGET, "--api", "127.0.0.1:1"],
            &[
                "--commit",
                super::TARGET,
                "--api",
                "127.0.0.1:1",
                "--token-file",
                "/x",
                // The order of the update is the API's: nodes first is gone.
                "--order",
                "nodes-first",
            ],
            &[
                "--commit",
                super::TARGET,
                "--api",
                "127.0.0.1:1",
                "--token-file",
                "/x",
                "--node-timeout-s",
                "0",
            ],
            &[
                "--commit",
                super::TARGET,
                "--api",
                "127.0.0.1:1",
                "--token-file",
                "/x",
                "--platform",
                "beos",
            ],
            &[
                "--commit",
                super::TARGET,
                "--api",
                "127.0.0.1:1",
                "--token-file",
                "/x",
                "--frobnicate",
            ],
            &["--commit", super::TARGET, "--api"],
        ] {
            let (code, out, err) = run(bad);
            assert_eq!((code, out.as_str()), (2, ""), "{bad:?}");
            assert!(err.contains("usage:"), "{err}");
        }
    }

    /// Proves flags can be left to the host's config, a flag wins over it, and a config
    /// another user could write is refused (the caller could then pick the endpoint).
    #[test]
    fn the_host_config_fills_the_api_and_the_token_file() {
        let token = token("cfg-tok", 0o600, GOOD);
        let (_, dir) = crate::testkit::host_root("cfg");
        let ca = dir.join("ca.pem");
        fs::write(&ca, crate::deploy::smoke_tests::pki("farm.example").0).unwrap();
        let conf = dir.join("deploy.json");
        let write = |mode: u32, body: &str| {
            fs::write(&conf, body).unwrap();
            fs::set_permissions(&conf, fs::Permissions::from_mode(mode)).unwrap();
        };
        let body = format!(
            "{{\"api\": \"127.0.0.1:1\", \"api_ca\": {:?}, \"api_server_name\": \"farm.example\", \
             \"token_file\": {:?}}}",
            ca.to_str().unwrap(),
            token.to_str().unwrap()
        );
        let lock = dir.join("run/lock");
        let argv = |extra: &[&str]| {
            let mut v = vec![
                "--commit",
                super::TARGET,
                "--config",
                conf.to_str().unwrap(),
                "--lock",
                lock.to_str().unwrap(),
            ];
            v.extend_from_slice(extra);
            v.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>()
        };
        let go = |a: Vec<String>| {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let code = cli::main_as(&a, &mut &b""[..], &mut out, &mut err, false);
            (code, String::from_utf8(err).unwrap())
        };
        write(0o644, &body);
        // Nothing listens on port 1: the run gets as far as calling the API (exit 1).
        assert_eq!(go(argv(&[])).0, 1);
        write(0o666, &body);
        let (code, err) = go(argv(&[]));
        assert_eq!(code, 2);
        assert!(err.contains("no one else may write"), "{err}");
        write(0o644, "not json");
        assert_eq!(go(argv(&[])).0, 2);
        write(0o644, "{}");
        let (code, err) = go(argv(&[]));
        assert_eq!(code, 2);
        assert!(err.contains("--api is required"), "{err}");
        // Every setting the config can give is given by a flag: the config is not read at all.
        write(0o666, &body);
        let all = [
            ("--api", "127.0.0.1:1"),
            ("--api-ca", ca.to_str().unwrap()),
            ("--api-server-name", "farm.example"),
            ("--token-file", token.to_str().unwrap()),
            ("--cas", "https://127.0.0.1:1"),
            ("--cas-ca", ca.to_str().unwrap()),
            ("--cas-server-name", "farm.example"),
        ];
        let without = |left_out: Option<&str>| {
            let flags: Vec<&str> = all
                .iter()
                .filter(|(flag, _)| Some(*flag) != left_out)
                .flat_map(|(flag, value)| [*flag, *value])
                .collect();
            go(argv(&flags))
        };
        assert_eq!(without(None).0, 1);
        // Leave out any one and the config is read for it, so a config others may write is
        // refused.
        for (flag, _) in all {
            let (code, err) = without(Some(flag));
            assert_eq!(code, 2, "{flag}");
            assert!(err.contains("no one else may write"), "{flag}: {err}");
        }
        // No config and no flags.
        let none = dir.join("absent.json");
        let (code, _) = go(vec![
            "--commit".into(),
            super::TARGET.into(),
            "--config".into(),
            none.to_str().unwrap().into(),
        ]);
        assert_eq!(code, 2);
    }

    /// Proves --cas is checked before anything runs: a value that is not an http:// URL is a
    /// usage error (exit 2) that says so. Catches a bad address found only at the end of a
    /// deploy.
    #[test]
    fn a_cas_address_that_is_not_an_http_url_is_a_usage_error() {
        let commit = "a".repeat(40);
        for bad in ["127.0.0.1:8980", "https://127.0.0.1:8980", "http://"] {
            let (code, out, err) = run(&["--commit", &commit, "--cas", bad]);
            assert_eq!((code, out.as_str()), (2, ""), "{bad}");
            assert!(err.contains("--cas"), "{bad}: {err}");
        }
    }

    /// Proves the production CLI's smoke is the real one when the flags, the host config or
    /// the frontend's own config name a listener, and none when nothing does. Catches
    /// `NoSmoke` left in the path (every real deploy then ends exit 5).
    #[test]
    fn the_cli_smoke_is_the_cas_smoke_when_a_listener_is_named() {
        use super::super::SmokeResult;
        use super::super::cli::{CasFlags, FrontendConfig};
        use std::time::Duration;
        let t = Duration::from_secs(5);
        let (_, dir) = crate::testkit::host_root("cli-smoke");
        let none = FrontendConfig {
            toml: dir.join("absent.toml"),
            units: vec![dir.join("absent.service")],
        };
        let flags = |url: Option<&str>| CasFlags {
            url: url.map(str::to_owned),
            ..CasFlags::default()
        };
        // Nothing named anywhere: skipped.
        assert_eq!(
            cli::smoke_for(&flags(None), t, &none).unwrap().run(),
            SmokeResult::Skipped
        );
        // Nothing listens at port 1: the real smoke fails; the stand-in would skip.
        let real = cli::smoke_for(&flags(Some("http://127.0.0.1:1")), t, &none).unwrap();
        assert!(matches!(real.run(), SmokeResult::Fail(_)));
        assert!(cli::smoke_for(&flags(Some("127.0.0.1:1")), t, &none).is_err());
        // The frontend's config names it (same: nothing listens there).
        let conf = FrontendConfig {
            toml: dir.join("kbf.toml"),
            units: Vec::new(),
        };
        fs::write(&conf.toml, "[listener]\nlisten = \"127.0.0.1:1\"\n").unwrap();
        let derived = cli::smoke_for(&flags(None), t, &conf).unwrap();
        assert!(matches!(derived.run(), SmokeResult::Fail(_)));
        // An unreadable CA file is an error before the deploy starts.
        let bad_ca = CasFlags {
            ca: Some(dir.join("absent-ca.pem")),
            ..flags(Some("https://127.0.0.1:1"))
        };
        assert!(cli::smoke_for(&bad_ca, t, &none).is_err());
        // The host config's `cas` flows through the flags.
        let (_, d2) = crate::testkit::host_root("cfg-cas");
        let conf = d2.join("deploy.json");
        fs::write(&conf, "{\"cas\": \"http://127.0.0.1:1\"}").unwrap();
        fs::set_permissions(&conf, fs::Permissions::from_mode(0o644)).unwrap();
        let commit = "a".repeat(40);
        let tok = token("cfg-cas-tok", 0o600, GOOD);
        let ca = d2.join("ca.pem");
        fs::write(&ca, crate::deploy::smoke_tests::pki("farm.example").0).unwrap();
        let (code, _, err) = run(&[
            "--commit",
            &commit,
            "--config",
            conf.to_str().unwrap(),
            "--api",
            "127.0.0.1:1",
            "--api-ca",
            ca.to_str().unwrap(),
            "--api-server-name",
            "farm.example",
            "--token-file",
            tok.to_str().unwrap(),
            "--lock",
            d2.join("run/lock").to_str().unwrap(),
        ]);
        // Without --apply the run stops at the plan (exit 1: nothing listens at the API).
        assert_eq!(code, 1, "{err}");
    }

    /// Proves a host whose frontend serves TLS ends a smoke with no flag at all but the CA:
    /// the address and the certificate's name come from the frontend's config, the
    /// connection goes to loopback, and the certificate is verified under its own name.
    /// Catches an https frontend dialled as plain text, a name that is not passed on, and a
    /// CA file that is not read.
    #[test]
    fn a_tls_frontend_is_smoked_from_its_own_config_and_a_ca_file() {
        use super::super::SmokeResult;
        use super::super::cli::{CasFlags, FrontendConfig};
        use super::super::smoke_tests::tls_farm;
        use std::time::Duration;
        let farm = tls_farm("farm.example");
        let (_, dir) = crate::testkit::host_root("cli-tls");
        let cert = dir.join("reapi.pem");
        let ca = dir.join("ca.pem");
        fs::write(&cert, &farm.cert).unwrap();
        fs::write(&ca, &farm.ca).unwrap();
        let conf = FrontendConfig {
            toml: dir.join("kbf.toml"),
            units: Vec::new(),
        };
        fs::write(
            &conf.toml,
            format!(
                "[listener]\nlisten = \"127.0.0.1:{}\"\n[tls]\nreapi-tls-cert = {:?}\n",
                farm.port,
                cert.to_str().unwrap()
            ),
        )
        .unwrap();
        let t = Duration::from_secs(30);
        let with_ca = CasFlags {
            ca: Some(ca),
            ..CasFlags::default()
        };
        let smoke = cli::smoke_for(&with_ca, t, &conf).unwrap();
        assert_eq!(smoke.run(), SmokeResult::Pass);
        // The certificate is for farm.example; asking for another name fails.
        let wrong = CasFlags {
            server_name: Some("other.example".into()),
            ..with_ca
        };
        let smoke = cli::smoke_for(&wrong, t, &conf).unwrap();
        assert!(matches!(smoke.run(), SmokeResult::Fail(_)));
    }

    /// Proves --cas-ca and --cas-server-name, like --cas, cannot be set through sudo.
    #[test]
    fn through_sudo_the_cas_tls_flags_are_refused() {
        let commit = "a".repeat(40);
        for flag in ["--cas-ca", "--cas-server-name"] {
            let list = args(&["--commit", &commit, flag, "x"]);
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let code = cli::main_as(&list, &mut &b""[..], &mut out, &mut err, true);
            assert_eq!(code, 2, "{flag}");
            assert!(String::from_utf8(err).unwrap().contains("through sudo"));
        }
    }

    #[test]
    fn a_token_file_must_be_private_and_hold_a_token() {
        assert!(cli::read_token(&token("tok-ok", 0o600, GOOD)).is_ok());
        assert!(cli::read_token(&token("tok-ro", 0o400, GOOD)).is_ok());
        assert!(cli::read_token(&token("tok-open", 0o644, GOOD)).is_err());
        assert!(cli::read_token(&token("tok-short", 0o600, "short\n")).is_err());
        assert!(cli::read_token(&token("tok-space", 0o600, &"a b ".repeat(20))).is_err());
        let (_, dir) = crate::testkit::host_root("tok-dir");
        assert!(cli::read_token(&dir).is_err());
        assert!(cli::read_token(&dir.join("absent")).is_err());
    }

    #[test]
    fn a_plan_prints_one_json_line_and_a_held_lock_is_busy() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let token = token("tok-plan", 0o600, GOOD);
        let (_, dir) = crate::testkit::host_root("cli-lock");
        let lock_path = dir.join("run/lock");
        let held = super::lock(&lock_path).unwrap().unwrap();
        let ca = dir.join("ca.pem");
        fs::write(&ca, crate::deploy::smoke_tests::pki("farm.example").0).unwrap();
        let ca = ca.to_str().unwrap().to_owned();
        let common = |api: &str| {
            [
                "--commit",
                super::TARGET,
                "--api",
                api,
                "--api-ca",
                ca.as_str(),
                "--api-server-name",
                "farm.example",
                "--token-file",
                token.to_str().unwrap(),
                "--lock",
                lock_path.to_str().unwrap(),
                "--json",
            ]
            .map(str::to_owned)
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = cli::main_as(
            &common("127.0.0.1:1"),
            &mut &b""[..],
            &mut out,
            &mut err,
            false,
        );
        // Exit 4 prints the one-line summary too, as every other end does.
        let out = String::from_utf8(out).unwrap();
        assert_eq!((code, out.lines().count()), (4, 1), "{out}");
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(
            (
                v["exit"].as_i64(),
                v["result"].as_str(),
                v["reason"].as_str()
            ),
            (Some(4), Some("busy"), Some("lock_held"))
        );
        assert_eq!(v["commit"], super::TARGET);
        drop(held);

        // A server that answers nothing useful: the plan stops at the first read.
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        std::thread::spawn(move || {
            for stream in listener.incoming().take(1) {
                let mut s = stream.unwrap();
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let _ =
                    s.write_all(b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\n\r\n");
            }
        });
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = cli::main_as(&common(&addr), &mut &b""[..], &mut out, &mut err, false);
        let out = String::from_utf8(out).unwrap();
        assert_eq!(code, 1);
        assert_eq!(out.lines().count(), 1);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["reason"], "api_error");
        assert!(!out.contains("0123456789abcdef0123456789abcdef0123456789abcdef"));
    }
}
