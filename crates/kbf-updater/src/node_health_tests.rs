//! The health of storage and daemon hosts against a fake operator API, route by route.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::frontend_health::FrontendHealth;
use crate::health::{HostHealth, InstallTimes, commit_of, names};
use crate::https::{ProxyApi, ProxyTls};
use crate::inventory::{Host, Role};
use crate::node_health::NodeHealth;
use crate::operator::OperatorApi;
use crate::ports::{Health, HealthCheck, PortError, PortErrorKind};
use crate::testutil::{
    FakeApi, KEY, TOKEN, closed_port, fake_api_by, fake_proxy, file, http, scratch,
};
use crate::version::Version;

const TARGET: &str = "0.1.7+g0123456789ab-3";
const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";
const OTHER: &str = "fedcba9876543210fedcba9876543210fedcba98";
const ADDR: &str = "192.0.2.11:8982";

/// The install call in milliseconds.
const INSTALL_MS: u64 = 1_792_658_476_500;

fn host(alias: &str, role: Role) -> Host {
    Host {
        alias: alias.to_owned(),
        address: "secret-host.farm.example".to_owned(),
        role,
        host_key: KEY.to_owned(),
    }
}

fn target() -> Version {
    Version::parse(TARGET).expect("version")
}

type Routes = Vec<(&'static str, Vec<u8>)>;

/// The routes of a fake operator API: the answer of each path (a 404 for any other).
fn routes(table: Routes) -> FakeApi {
    fake_api_by(move |_, head| {
        let path = head.split(' ').nth(1).unwrap_or_default();
        Some(
            table
                .iter()
                .find(|(p, _)| *p == path)
                .map_or_else(|| http(404, "{\"error\":\"no\"}"), |(_, a)| a.clone()),
        )
    })
}

fn api(server: &FakeApi, name: &str) -> OperatorApi {
    let token = file(&scratch(name), "token", TOKEN, 0o600);
    OperatorApi::new(server.addr, token, None)
        .expect("api")
        .with_timeout(Duration::from_millis(500))
}

fn adapter(server: &FakeApi, name: &str) -> NodeHealth {
    NodeHealth::with_api(api(server, name), Arc::new(|| INSTALL_MS))
}

fn enrolled(id: &str, advertise: &str) -> Vec<u8> {
    http(
        200,
        &json!({"nodes": [{"id": id, "roles": ["storage"], "state": "active", "advertise": advertise}]})
            .to_string(),
    )
}

fn member(addr: &str, state: &str, commit: Option<&str>) -> Value {
    json!({"id": 1, "addr": addr, "state": state, "weight": 1, "protocol": 1,
           "runs_log": null, "log_version": null, "build_commit": commit})
}

fn members(list: Vec<Value>) -> Vec<u8> {
    http(200, &json!({"generation": 7, "members": list}).to_string())
}

fn storage(rows: Vec<Value>) -> FakeApi {
    routes(vec![
        ("/v1/enroll/nodes", enrolled("st1", ADDR)),
        ("/v1/members", members(rows)),
    ])
}

fn storage_check(server: &FakeApi, name: &str) -> Result<Health, PortError> {
    adapter(server, name).check(&host("st1", Role::Storage), &target())
}

fn waiting(health: &Result<Health, PortError>) -> String {
    match health {
        Ok(Health::Waiting(why)) => why.clone(),
        other => panic!("expected waiting, got {other:?}"),
    }
}

/// Catches a storage node not found, or a wrong member taken: the node's advertised address
/// (from the enrollment registry) picks the member (the first of two), the request is what the
/// operator gates want (bearer, JSON content type), and a member that is active and built from
/// the target's commit is healthy, with a 40-digit or a 12-digit report.
#[test]
fn an_active_member_built_from_the_target_is_healthy() {
    for built in [COMMIT, "0123456789ab"] {
        let rows = vec![
            json!({"id": 2, "addr": "192.0.2.12:8982", "state": "active", "build_commit": OTHER}),
            member(ADDR, "active", Some(built)),
        ];
        let server = storage(rows);
        assert_eq!(
            storage_check(&server, "nh-ok"),
            Ok(Health::Healthy),
            "{built}"
        );
        let seen = server.requests();
        assert!(
            seen[0].starts_with("GET /v1/enroll/nodes HTTP/1.1\r\n"),
            "{}",
            seen[0]
        );
        assert!(
            seen[1].starts_with("GET /v1/members HTTP/1.1\r\n"),
            "{}",
            seen[1]
        );
        for head in &seen {
            assert!(
                head.contains(&format!("Authorization: Bearer {TOKEN}\r\n")),
                "{head}"
            );
            assert!(
                head.contains("Content-Type: application/json\r\n"),
                "{head}"
            );
        }
    }
    // The address is compared as the registry lower-cases it.
    let server = routes(vec![
        ("/v1/enroll/nodes", enrolled("st1", "st1.farm.example:8982")),
        (
            "/v1/members",
            members(vec![member(
                "ST1.farm.example:8982",
                "active",
                Some(COMMIT),
            )]),
        ),
    ]);
    assert_eq!(storage_check(&server, "nh-case"), Ok(Health::Healthy));
}

/// Catches a member called healthy that is not: not active (each other state), no build
/// reported, another build, a build of `unknown`, a build that is a shorter prefix of the
/// target's, or no member at the node's address (a lapsed member leaves the map for a while).
/// All wait, and the text names the member and what is wrong.
#[test]
fn a_member_that_is_not_active_or_not_the_target_waits() {
    for state in ["joining", "catching_up", "draining", "down"] {
        let server = storage(vec![member(ADDR, state, Some(COMMIT))]);
        let why = waiting(&storage_check(&server, "nh-state"));
        assert!(why.contains(&format!("m1 is {state}, not active")), "{why}");
    }
    for (commit, text) in [
        (None, "has not reported its build commit"),
        (Some(OTHER), "runs build fedcba"),
        (Some("unknown"), "runs build unknown"),
        (Some("0123456789a"), "runs build 0123456789a"),
        (Some(""), "runs build "),
    ] {
        let server = storage(vec![member(ADDR, "active", commit)]);
        let why = waiting(&storage_check(&server, "nh-commit"));
        assert!(why.contains(text), "{commit:?}: {why}");
    }
    let server = storage(vec![member("192.0.2.99:8982", "active", Some(COMMIT))]);
    let why = waiting(&storage_check(&server, "nh-absent"));
    assert!(why.contains("no storage member is registered"), "{why}");
}

/// Catches a node the cluster does not know being waited on: an id the registry lacks, or a
/// node with no storage address, can never become a storage member, so the host fails at once
/// with the reason.
#[test]
fn an_unknown_node_or_one_without_storage_fails_at_once() {
    let server = routes(vec![("/v1/enroll/nodes", enrolled("other", ADDR))]);
    let Ok(Health::Abandoned(why)) = storage_check(&server, "nh-unknown") else {
        panic!("expected a failure");
    };
    assert!(why.contains("st1 is not an enrolled node"), "{why}");
    let server = routes(vec![("/v1/enroll/nodes", enrolled("st1", ""))]);
    let Ok(Health::Abandoned(why)) = storage_check(&server, "nh-nostorage") else {
        panic!("expected a failure");
    };
    assert!(why.contains("advertises no storage address"), "{why}");
    assert_eq!(server.requests().len(), 1, "the members are not read");
}

/// Catches an answer that is not understood being taken for health or for "keep waiting":
/// a frontend without enrollment or without a state store (404), a gate's refusal, a surprise,
/// a body that is not the document and a 5xx are each an error of the right kind, none holds
/// the token or an address.
#[test]
fn errors_are_of_the_right_kind_and_leak_nothing() {
    let cases: Vec<(Routes, PortErrorKind, &str)> = vec![
        (vec![], PortErrorKind::Other, "no enrollment registry"),
        (
            vec![("/v1/enroll/nodes", enrolled("st1", ADDR))],
            PortErrorKind::Other,
            "keeps no storage members",
        ),
        (
            vec![("/v1/enroll/nodes", http(401, "{}"))],
            PortErrorKind::Other,
            "refused the call (401)",
        ),
        (
            vec![("/v1/enroll/nodes", http(403, "{}"))],
            PortErrorKind::Other,
            "(403)",
        ),
        (
            vec![("/v1/enroll/nodes", http(503, "{}"))],
            PortErrorKind::Unreachable,
            "answered 503",
        ),
        (
            vec![("/v1/enroll/nodes", http(302, "{}"))],
            PortErrorKind::Other,
            "answered 302",
        ),
        (
            vec![("/v1/enroll/nodes", http(200, "<html>"))],
            PortErrorKind::Other,
            "is not what it should be",
        ),
        (
            vec![
                ("/v1/enroll/nodes", enrolled("st1", ADDR)),
                ("/v1/members", http(500, "{}")),
            ],
            PortErrorKind::Unreachable,
            "answered 500",
        ),
    ];
    for (table, kind, text) in cases {
        let server = routes(table);
        let e = storage_check(&server, "nh-errors").expect_err(text);
        assert_eq!(
            (e.kind(), e.to_string().contains(text)),
            (kind, true),
            "{text}: {e}"
        );
        for secret in [TOKEN, "secret-host", &server.addr.to_string()] {
            assert!(!e.to_string().contains(secret), "{secret} in {e}");
        }
    }
    let dead = NodeHealth::with_api(
        OperatorApi::new(
            closed_port(),
            file(&scratch("nh-dead"), "token", TOKEN, 0o600),
            None,
        )
        .expect("api"),
        Arc::new(|| INSTALL_MS),
    );
    let e = dead
        .check(&host("st1", Role::Storage), &target())
        .expect_err("refused");
    assert!(e.is_unreachable(), "{e}");
}

/// Catches a version without a commit being judged: no storage node can be shown to run it,
/// so the host fails at once and nothing is asked. A daemon needs no commit, and is judged.
#[test]
fn a_version_without_a_commit_fails_a_storage_host_only() {
    let server = storage(vec![member(ADDR, "active", Some(COMMIT))]);
    let v = Version::parse("2.0-1").expect("version");
    let Ok(Health::Abandoned(why)) =
        adapter(&server, "nh-nocommit").check(&host("st1", Role::Storage), &v)
    else {
        panic!("expected a failure");
    };
    assert!(why.contains("names no commit"), "{why}");
    assert!(server.requests().is_empty());
    let nodes = routes(vec![("/v1/nodes/dm", node(true, "serving", None))]);
    assert_eq!(
        adapter(&nodes, "nh-dm-nocommit").check(&host("dm", Role::Daemon), &v),
        Ok(Health::Healthy)
    );
}

fn node(connected: bool, placement: &str, received_at: Option<u64>) -> Vec<u8> {
    http(
        200,
        &json!({
            "node_id": "dm", "connected": connected, "protocol": 1,
            "software": received_at.map(|ms| json!({"os_name": "Linux", "received_at_unix_ms": ms})),
            "needs_attention": [], "placement": {"state": placement},
        })
        .to_string(),
    )
}

fn daemon_check(adapter: &NodeHealth) -> Result<Health, PortError> {
    adapter.check(&host("dm", Role::Daemon), &target())
}

/// Catches a daemon taken for healthy when it is not (the version is not verified for a
/// daemon, so these are all that is checked): not registered with this frontend (404), not
/// connected, cordoned or draining (not serving). Each waits, with the reason.
#[test]
fn a_daemon_waits_until_it_is_connected_and_serving() {
    let missing = routes(vec![]);
    let why = waiting(&daemon_check(&adapter(&missing, "nh-dm-404")));
    assert!(
        why.contains("has not registered with the frontend"),
        "{why}"
    );
    let down = routes(vec![("/v1/nodes/dm", node(false, "serving", None))]);
    assert!(waiting(&daemon_check(&adapter(&down, "nh-dm-down"))).contains("dm is not connected"));
    for placement in ["cordoned", "draining", "drained", "drain_paused"] {
        let server = routes(vec![("/v1/nodes/dm", node(true, placement, None))]);
        let why = waiting(&daemon_check(&adapter(&server, "nh-dm-placed")));
        assert!(
            why.contains(&format!("dm is {placement}, not serving")),
            "{why}"
        );
    }
}

/// Catches a daemon that has not restarted being called healthy: when this process announced
/// the install, the daemon's newest status must have arrived at or after it (a daemon sends
/// one on every stream, so a newer one is a new process); an older one, or none, waits. With
/// no announced install (the updater restarted) connected and serving is all that can be asked.
#[test]
fn a_daemon_must_have_reported_since_the_install() {
    let dm = host("dm", Role::Daemon);
    for (received, healthy) in [
        (Some(INSTALL_MS - 1), false),
        (Some(INSTALL_MS), true),
        (Some(INSTALL_MS + 5_000), true),
        (None, false),
    ] {
        let server = routes(vec![("/v1/nodes/dm", node(true, "serving", received))]);
        let health = adapter(&server, "nh-dm-fresh");
        health.installing(&dm);
        let got = daemon_check(&health);
        if healthy {
            assert_eq!(got, Ok(Health::Healthy), "{received:?}");
        } else {
            let why = waiting(&got);
            assert!(
                why.contains("has not restarted") || why.contains("has sent no status"),
                "{received:?}: {why}"
            );
        }
        // Without the announcement, the same answer is healthy.
        let fresh = adapter(&server, "nh-dm-nocall");
        assert_eq!(daemon_check(&fresh), Ok(Health::Healthy), "{received:?}");
    }
    let server = routes(vec![("/v1/nodes/dm", http(503, "{}"))]);
    assert!(
        daemon_check(&adapter(&server, "nh-dm-5xx"))
            .expect_err("5xx")
            .is_unreachable()
    );
}

/// Catches the node API's path built from anything but the alias, and the id being sent
/// where a body should: the daemon is asked as `GET /v1/nodes/{alias}`.
#[test]
fn the_daemon_is_asked_by_its_alias() {
    let server = routes(vec![("/v1/nodes/dm", node(true, "serving", None))]);
    assert_eq!(
        daemon_check(&adapter(&server, "nh-dm-path")),
        Ok(Health::Healthy)
    );
    assert!(server.requests()[0].starts_with("GET /v1/nodes/dm HTTP/1.1\r\n"));
}

/// Catches a version tied to the wrong commit: the 12 digits after `+g` of a package version,
/// bare after `+` for the older form, up to a `-`; none for a version without them, with fewer
/// or more digits than a commit has, or with other text after them. A reported commit matches
/// by prefix, in either case, and a shorter report or a non-ASCII one does not.
#[test]
fn a_version_names_its_commit_and_a_report_matches_by_prefix() {
    for (version, commit) in [
        ("0.1.7+g0123456789ab-3", Some("0123456789ab")),
        ("0.1.7~feature+g0123456789ab-3", Some("0123456789ab")),
        ("0.1.0+0123456789ab", Some("0123456789ab")),
        (
            "0.1.0+g0123456789abcdef0123456789abcdef01234567",
            Some(COMMIT),
        ),
        ("0.1.7+gABCDEF012345-1", Some("ABCDEF012345")),
        ("0.1.7+g0123456789ab", Some("0123456789ab")),
        ("2.0-1", None),
        ("0.1.7+g0123456789a-3", None),
        ("0.1.7+g0123456789abc0123456789abc0123456789abc0123-3", None),
        ("0.1.7+g0123456789abz-3", None),
        ("0.1.7+g0123456789ab.1", None),
        ("0.1.7+git9ba38c2-2", None),
        ("0.1.7+", None),
        ("0.1.7+g", None),
    ] {
        let v = Version::parse(version).expect(version);
        assert_eq!(v.commit(), commit, "{version}");
    }
    assert!(names(COMMIT, "0123456789ab"));
    assert!(names("0123456789AB", "0123456789ab"));
    assert!(names("0123456789ab", "0123456789ab"));
    assert!(!names("0123456789a", "0123456789ab"));
    assert!(!names("unknown", "0123456789ab"));
    assert!(!names("", "0123456789ab"));
    assert!(!names("0123456789ac", "0123456789ab"));
    assert!(!names("\u{e9}123456789abcdef", "0123456789ab"));
    assert!(commit_of(&target()).is_ok());
    assert!(matches!(
        commit_of(&Version::parse("1").expect("v")),
        Err(Health::Abandoned(_))
    ));
}

/// Catches the install time being lost or crossed between hosts: the last call of a host wins,
/// another host's is its own, and a host never announced has none.
#[test]
fn install_times_are_kept_per_host_and_the_last_call_wins() {
    let now = Arc::new(Mutex::new(10u64));
    let clock = Arc::clone(&now);
    let times = InstallTimes::new(Arc::new(move || *clock.lock().expect("lock")));
    assert_eq!(times.at("a"), None);
    times.note("a");
    *now.lock().expect("lock") = 20;
    times.note("b");
    assert_eq!((times.at("a"), times.at("b")), (Some(10), Some(20)));
    *now.lock().expect("lock") = 30;
    times.note("a");
    assert_eq!((times.at("a"), times.at("b")), (Some(30), Some(20)));
    assert!(crate::health::system_wall_clock()() > 1_700_000_000_000);
}

/// Catches a host judged by the wrong adapter, and an announcement that goes to the wrong one:
/// the frontend is asked of kbf-proxy only, a storage and a daemon host of the operator API
/// only, and the install announced for a daemon is the time its freshness is held to.
#[test]
fn each_host_is_judged_by_the_adapter_of_its_role() {
    let status = json!({
        "proxy_boot": "b", "installed_commit": COMMIT, "active_slot": "a",
        "active_commit": COMMIT, "pending_binary": null,
        "update": {"to": COMMIT, "phase": "done", "state": "done", "reason": null,
                   "started_at": "2026-10-22T08:41:20Z"},
    })
    .to_string();
    let proxy = fake_proxy("proxy.example", true, move |_, _| Some(http(200, &status)));
    let dir = scratch("nh-router");
    let tls = ProxyTls {
        addr: proxy.addr,
        ca_file: file(&dir, "ca.pem", &proxy.ca, 0o644),
        server_name: Some("proxy.example".to_owned()),
    };
    let token = file(&dir, "token", TOKEN, 0o600);
    let frontend = FrontendHealth::with_clock(
        ProxyApi::new(&tls, token)
            .expect("api")
            .with_timeout(Duration::from_millis(500)),
        Arc::new(|| INSTALL_MS),
    );
    let operator = routes(vec![
        ("/v1/enroll/nodes", enrolled("st1", ADDR)),
        (
            "/v1/members",
            members(vec![member(ADDR, "active", Some(COMMIT))]),
        ),
        ("/v1/nodes/dm", node(true, "serving", Some(INSTALL_MS - 1))),
    ]);
    let all = HostHealth::new(frontend, adapter(&operator, "nh-router"));
    let v = target();
    assert_eq!(
        all.check(&host("fe", Role::Frontend), &v),
        Ok(Health::Healthy)
    );
    assert_eq!(proxy.requests().len(), 1);
    assert!(operator.requests().is_empty());
    assert_eq!(
        all.check(&host("st1", Role::Storage), &v),
        Ok(Health::Healthy)
    );
    assert_eq!(proxy.requests().len(), 1);
    assert_eq!(operator.requests().len(), 2);
    // A daemon whose status is older than the announced install is not healthy; unannounced
    // it is.
    assert_eq!(
        all.check(&host("dm", Role::Daemon), &v),
        Ok(Health::Healthy)
    );
    all.installing(&host("dm", Role::Daemon));
    assert!(matches!(
        all.check(&host("dm", Role::Daemon), &v),
        Ok(Health::Waiting(_))
    ));
    // The frontend's announcement goes to the frontend: the done swap began after INSTALL_MS.
    all.installing(&host("fe", Role::Frontend));
    assert_eq!(
        all.check(&host("fe", Role::Frontend), &v),
        Ok(Health::Healthy)
    );
}
