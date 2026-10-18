use std::cell::{Cell, RefCell};
use std::io;
use std::rc::Rc;
use std::time::Duration;

use serde_json::{Value, json};

use super::api::{Http, Operator, Reply, Request, parse_reply};
use super::*;
use crate::testkit::{A, B, c};

const TARGET: &str = A;
const OLD: &str = B;

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
}

fn node(id: &'static str, platform: &'static str, fate: Fate) -> NodeScript {
    NodeScript {
        id,
        platform,
        connected: true,
        fate,
        stale: None,
    }
}

#[derive(Clone, Copy, PartialEq)]
enum FrontendFate {
    Confirm,
    Revert,
    Hang,
    Busy,
    /// Shows the previous order, confirmed, until the new one reverts.
    StaleThenRevert,
}

/// A farm the operator API plays, driven by a clock that only moves when the deploy sleeps.
struct Farm {
    now: Rc<Cell<u64>>,
    log: RefCell<Vec<String>>,
    nodes: Vec<NodeScript>,
    complete: Vec<&'static str>,
    desired_at: Cell<Option<u64>>,
    generation: Cell<u64>,
    frontend_fate: FrontendFate,
    frontend_at: Cell<Option<u64>>,
    in_flight_at_start: bool,
    upload_status: u16,
    desired_status: u16,
    ready: bool,
    members_active_after: u64,
    unattested: bool,
    server_commit: &'static str,
    cordoned: Option<&'static str>,
}

impl Farm {
    fn new(now: Rc<Cell<u64>>, nodes: Vec<NodeScript>) -> Farm {
        Farm {
            now,
            log: RefCell::new(Vec::new()),
            nodes,
            complete: vec!["linux-x86_64", "darwin-arm64"],
            desired_at: Cell::new(None),
            generation: Cell::new(7),
            frontend_fate: FrontendFate::Confirm,
            frontend_at: Cell::new(None),
            in_flight_at_start: false,
            upload_status: 201,
            desired_status: 200,
            ready: true,
            members_active_after: 0,
            unattested: false,
            server_commit: &TARGET[..12],
            cordoned: None,
        }
    }

    fn requests(&self, prefix: &str) -> usize {
        self.log
            .borrow()
            .iter()
            .filter(|l| l.starts_with(prefix))
            .count()
    }

    fn index_of(&self, line: &str) -> Option<usize> {
        self.log.borrow().iter().position(|l| l == line)
    }

    fn node_json(&self, n: &NodeScript) -> Value {
        let elapsed = self.desired_at.get().map(|t| self.now.get() - t);
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

impl Operator for Farm {
    fn send(&self, r: &Request) -> io::Result<Reply> {
        self.log
            .borrow_mut()
            .push(format!("{} {}", r.method, r.path));
        let now = self.now.get();
        match (r.method, r.path.as_str()) {
            ("PUT", _) => Farm::reply(self.upload_status, json!({"sha256": "x", "size": 1})),
            ("GET", "/v1/nodes") => {
                let nodes: Vec<Value> = self.nodes.iter().map(|n| self.node_json(n)).collect();
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
                Farm::reply(
                    200,
                    json!({"generation": 3, "members": [{"id": 1, "state": state}]}),
                )
            }
            ("GET", "/v1/update") => {
                let desired = if self.desired_at.get().is_some() {
                    json!({"commit": TARGET, "generation": self.generation.get()})
                } else {
                    json!({"commit": OLD, "generation": self.generation.get()})
                };
                let frontend = match (self.frontend_at.get(), self.in_flight_at_start) {
                    (None, true) => json!({"order_id": 1, "state": "pending"}),
                    (None, false) => json!({"order_id": 1, "state": "confirmed"}),
                    (Some(t), _) => {
                        let state = match self.frontend_fate {
                            FrontendFate::Confirm if now >= t + 10 => "confirmed",
                            FrontendFate::Revert | FrontendFate::StaleThenRevert
                                if now >= t + 10 =>
                            {
                                "reverted"
                            }
                            _ => "activating",
                        };
                        if self.frontend_fate == FrontendFate::StaleThenRevert && now < t + 10 {
                            json!({"order_id": 1, "state": "confirmed"})
                        } else {
                            json!({"order_id": 2, "state": state})
                        }
                    }
                };
                Farm::reply(200, json!({"desired": desired, "frontend": frontend}))
            }
            ("POST", "/v1/update/desired") => {
                if self.desired_status == 200 {
                    self.desired_at.set(Some(now));
                    self.generation.set(8);
                }
                Farm::reply(self.desired_status, json!({}))
            }
            ("POST", "/v1/update/frontend") => {
                if self.frontend_fate == FrontendFate::Busy {
                    return Farm::reply(409, json!({"error": "busy"}));
                }
                self.frontend_at.set(Some(now));
                Farm::reply(202, json!({"order_id": 2}))
            }
            ("GET", "/readyz") => Farm::reply(if self.ready { 200 } else { 503 }, json!({})),
            _ => Farm::reply(404, json!({})),
        }
    }
}

struct Clk(Rc<Cell<u64>>);

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
    let mut o = Options::new(c(TARGET), "linux-x86_64");
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
    Farm::new(Rc::new(Cell::new(0)), nodes)
}

// ---- the whole deploy --------------------------------------------------------------

/// Proves the happy path ends 0 with every step, node and check recorded. Catches a deploy
/// that skips a step or reports a node it never saw confirm.
#[test]
fn a_clean_deploy_confirms_every_node_updates_the_frontend_and_checks_the_farm() {
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
        [
            "bundle",
            "preflight",
            "baseline",
            "desired",
            "nodes",
            "frontend"
        ]
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
        s.frontend.as_ref().map(|x| x.state.as_str()),
        Some("confirmed")
    );
    assert_eq!(s.post.cas_smoke.as_deref(), Some("pass"));
    assert_eq!(s.post.members_active, Some(true));
    assert_eq!(s.post.nodes_on_commit, Some(true));
    assert_eq!(s.post.readyz, Some(true));
    // The nodes are rolled before the frontend is updated.
    let desired = f.index_of("POST /v1/update/desired").unwrap();
    let frontend = f.index_of("POST /v1/update/frontend").unwrap();
    assert!(desired < frontend);
    // Never orders a node.
    assert_eq!(f.requests("POST /v1/nodes"), 0);
    let json: Value = serde_json::from_str(&s.line()).unwrap();
    assert_eq!(json["schema"], 1);
    assert_eq!(json["started"], "2026-09-21T14:13:20Z");
}

/// Proves `--frontend-first` flips the order, and only the order.
#[test]
fn frontend_first_updates_the_frontend_before_setting_the_desired_release() {
    let f = farm(two_nodes());
    let mut o = opts(true);
    o.order = Order::FrontendFirst;
    let s = run_with(&f, &o, SmokeResult::Pass, None);
    assert_eq!(s.exit, 0, "{}", s.line());
    assert!(f.index_of("POST /v1/update/frontend") < f.index_of("POST /v1/update/desired"));
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
    let log = f.log.borrow();
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
    f.in_flight_at_start = true;
    let s = go(&f);
    assert_eq!((s.exit, s.reason.as_str()), (4, "frontend_busy"));
    assert_eq!(f.requests("POST"), 0);
}

// ---- step 4 --------------------------------------------------------------------------

#[test]
fn a_409_on_the_desired_release_is_a_refusal() {
    let mut f = farm(two_nodes());
    f.desired_status = 409;
    let s = go(&f);
    assert_eq!((s.exit, s.reason.as_str()), (3, "release_incomplete"));
    assert_eq!(f.requests("POST /v1/update/frontend"), 0);
    f.desired_status = 500;
    assert_eq!(go(&f).exit, 1);
}

// ---- step 5: nodes -------------------------------------------------------------------

/// Proves a node that never confirms times out, the deploy stops, and the frontend is not
/// touched.
#[test]
fn a_node_that_never_confirms_times_out_and_the_frontend_is_left_alone() {
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
    assert_eq!(f.requests("POST /v1/update/frontend"), 0);
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
    assert!(f.now.get() < 100, "waited {} s", f.now.get());
    assert_eq!(f.requests("POST /v1/update/frontend"), 0);
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

/// Proves each node gets its own budget, started when the one before it confirms: two nodes
/// that take 1000 s each pass a 1500 s budget. Catches one shared clock for all nodes.
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
    assert!(
        s.nodes.iter().all(|n| n.secs >= 995 && n.secs <= 1010),
        "{:?}",
        s.nodes
    );
}

/// Proves a `reverted` left by an older offer is not this deploy's verdict.
#[test]
fn a_state_left_from_an_older_offer_is_not_this_deploys() {
    let mut n = node("a-store", "linux-x86_64", Fate::Confirm(100));
    n.stale = Some(("reverted", 7, 30));
    let f = farm(vec![n]);
    // For 30 s after the desired release is set the node still reports the state of the
    // offer before it (generation 7); that is not this deploy's verdict.
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

/// Proves another order's `confirmed` is not ours: the new instance's record is waited for.
#[test]
fn a_confirmed_record_of_an_earlier_order_is_not_ours() {
    let mut f = farm(two_nodes());
    f.frontend_fate = FrontendFate::StaleThenRevert;
    let s = go(&f);
    assert_eq!(
        (s.exit, s.reason.as_str()),
        (1, "frontend_reverted"),
        "{}",
        s.line()
    );
}

// ---- step 6: the frontend ------------------------------------------------------------

/// Proves a reverted frontend is a failure that says the old instance serves, and that no
/// post check is run on a farm in that state.
#[test]
fn a_reverted_frontend_fails_and_leaves_the_old_instance_serving() {
    let mut f = farm(two_nodes());
    f.frontend_fate = FrontendFate::Revert;
    let s = go(&f);
    assert_eq!((s.exit, s.reason.as_str()), (1, "frontend_reverted"));
    assert!(s.detail.contains("old one is serving"));
    assert_eq!(s.frontend.as_ref().unwrap().state, "reverted");
    assert_eq!(s.post, PostResult::default());
}

#[test]
fn a_frontend_that_never_finishes_the_handover_times_out() {
    let mut f = farm(two_nodes());
    f.frontend_fate = FrontendFate::Hang;
    let mut o = opts(true);
    o.handover_timeout = Duration::from_secs(60);
    let s = run_with(&f, &o, SmokeResult::Pass, None);
    assert_eq!((s.exit, s.reason.as_str()), (1, "frontend_timeout"));
}

#[test]
fn a_409_on_the_frontend_update_is_busy() {
    let mut f = farm(two_nodes());
    f.frontend_fate = FrontendFate::Busy;
    let s = go(&f);
    assert_eq!((s.exit, s.reason.as_str()), (4, "frontend_busy"));
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

/// Proves members get a window to catch up, and fail after it.
#[test]
fn members_that_are_catching_up_are_waited_for_and_then_fail() {
    let mut f = farm(two_nodes());
    f.members_active_after = 300; // the roll ends around 200 s
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

#[test]
fn replies_are_parsed_plain_and_chunked() {
    let plain = parse_reply(b"HTTP/1.1 201 Created\r\nContent-Length: 2\r\n\r\n{}").unwrap();
    assert_eq!((plain.status, plain.body.as_slice()), (201, &b"{}"[..]));
    let chunked = parse_reply(
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n",
    )
    .unwrap();
    assert_eq!(chunked.body, b"abcde");
    assert!(parse_reply(b"HTTP/1.1 200 OK").is_err());
    assert!(parse_reply(b"nonsense\r\n\r\n").is_err());
    assert!(parse_reply(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n").is_err());
    assert_eq!(plain.json(), Some(json!({})));
}

/// Proves the client sends the bearer token and the right content type, and refuses an
/// address that is not loopback (the token goes in clear).
#[test]
fn the_http_client_sends_the_token_and_only_to_loopback() {
    use std::io::{Read, Write};
    use std::net::TcpListener;
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        let mut seen = Vec::new();
        for _ in 0..2 {
            let (mut s, _) = listener.accept().unwrap();
            // The whole request: its head, then as many body bytes as it announces.
            let mut got = Vec::new();
            loop {
                let mut buf = [0u8; 1024];
                let n = s.read(&mut buf).unwrap();
                got.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&got).into_owned();
                if let Some(end) = text.find("\r\n\r\n") {
                    let want = text
                        .lines()
                        .find_map(|l| l.strip_prefix("Content-Length: "))
                        .map_or(0, |v| v.trim().parse::<usize>().unwrap());
                    if got.len() >= end + 4 + want {
                        break;
                    }
                }
            }
            seen.push(String::from_utf8_lossy(&got).into_owned());
            s.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\n{}")
                .unwrap();
        }
        seen
    });
    let http = Http::new(addr, "tok".to_owned(), Duration::from_secs(5)).unwrap();
    assert_eq!(http.send(&Request::get("/v1/nodes")).unwrap().status, 200);
    http.send(&Request::post_json(
        "/v1/update/desired",
        &json!({"commit": "x"}),
    ))
    .unwrap();
    let seen = server.join().unwrap();
    assert!(seen[0].starts_with("GET /v1/nodes HTTP/1.1"));
    assert!(seen[0].contains("Authorization: Bearer tok"));
    assert!(!seen[0].contains("Content-Type"));
    assert!(seen[1].contains("Content-Type: application/json"));
    assert!(seen[1].ends_with("{\"commit\":\"x\"}"));
    let remote = "192.0.2.1:80".parse().unwrap();
    assert!(Http::new(remote, "t".into(), Duration::from_secs(1)).is_err());
    let _ = Request::put_raw("/x", vec![1]);
}

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
        let code = cli::main(&args(list), &mut &b""[..], &mut out, &mut err);
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
                "--order",
                "x",
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
        let conf = dir.join("deploy.json");
        let write = |mode: u32, body: &str| {
            fs::write(&conf, body).unwrap();
            fs::set_permissions(&conf, fs::Permissions::from_mode(mode)).unwrap();
        };
        let body = format!(
            "{{\"api\": \"127.0.0.1:1\", \"token_file\": {:?}}}",
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
            let code = cli::main(&a, &mut &b""[..], &mut out, &mut err);
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
        // Both flags given: the config is not read at all.
        write(0o666, &body);
        assert_eq!(
            go(argv(&[
                "--api",
                "127.0.0.1:1",
                "--token-file",
                token.to_str().unwrap()
            ]))
            .0,
            1
        );
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
        let common = |api: &str| {
            [
                "--commit",
                super::TARGET,
                "--api",
                api,
                "--token-file",
                token.to_str().unwrap(),
                "--lock",
                lock_path.to_str().unwrap(),
                "--json",
            ]
            .map(str::to_owned)
        };
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = cli::main(&common("127.0.0.1:1"), &mut &b""[..], &mut out, &mut err);
        assert_eq!((code, out.len()), (4, 0));
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
        let code = cli::main(&common(&addr), &mut &b""[..], &mut out, &mut err);
        let out = String::from_utf8(out).unwrap();
        assert_eq!(code, 1);
        assert_eq!(out.lines().count(), 1);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["reason"], "api_error");
        assert!(!out.contains("0123456789abcdef0123456789abcdef0123456789abcdef"));
    }
}
