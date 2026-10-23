//! The health of storage and daemon hosts against a fake operator API, route by route.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::api::{ErrorCode, RollState};
use crate::health::{InstallTimes, commit_of, names};
use crate::inventory::{Host, Role};
use crate::node_health::{FRONTEND_REFUSAL, NodeHealth};
use crate::operator::OperatorApi;
use crate::ports::{Health, HealthCheck, PortError, PortErrorKind};
use crate::testutil::{FakeApi, KEY, TOKEN, closed_port, fake_api_by, file, http, scratch};
use crate::updater_tests::{Rig, all, inventory};
use crate::version::Version;
use crate::{Ports, Timing, Updater};

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

/// Catches two members at one address being resolved by order: the first row is active and
/// built from the target, the second is a stale duplicate (and the other way round); neither
/// order may pass for healthy, and the wait says why. One member at the address, beside
/// others at other addresses, is still healthy.
#[test]
fn two_members_at_the_nodes_address_wait_in_either_order() {
    let good = member(ADDR, "active", Some(COMMIT));
    let stale = member(&ADDR.to_ascii_uppercase(), "down", Some(OTHER));
    for rows in [
        vec![good.clone(), stale.clone()],
        vec![stale.clone(), good.clone()],
    ] {
        let server = storage(rows);
        let why = waiting(&storage_check(&server, "nh-dup"));
        assert!(why.contains("more than one storage member"), "{why}");
    }
    let elsewhere = member("192.0.2.12:8982", "active", Some(OTHER));
    let server = storage(vec![elsewhere, good]);
    assert_eq!(storage_check(&server, "nh-one"), Ok(Health::Healthy));
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
        health.installing(&dm, &target());
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

/// Catches a retried install moving the freshness bar: the daemon's newest status arrived
/// between the first install call and the second (the first attempt landed late), and the
/// roll's second call must not make that status look stale.
#[test]
fn a_retried_install_does_not_move_the_daemons_freshness_bar() {
    let dm = host("dm", Role::Daemon);
    let server = routes(vec![(
        "/v1/nodes/dm",
        node(true, "serving", Some(INSTALL_MS + 1_000)),
    )]);
    let now = Arc::new(Mutex::new(INSTALL_MS));
    let clock = Arc::clone(&now);
    let health = NodeHealth::with_api(
        api(&server, "nh-dm-retry"),
        Arc::new(move || *clock.lock().expect("lock")),
    );
    health.installing(&dm, &target());
    *now.lock().expect("lock") = INSTALL_MS + 5_000;
    health.installing(&dm, &target());
    assert_eq!(daemon_check(&health), Ok(Health::Healthy));
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
/// or more digits than a commit has, or with other text after them. The commit is the text after
/// the last `+`: an earlier `+` belongs to the upstream part, and a `+` suffix after the commit
/// (`+b1`) leaves a version whose commit cannot be verified. A reported commit matches
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
        ("0.1.7+g0123456789ab-3+b1", None),
        ("1.0+dfsg+g0123456789ab-2", Some("0123456789ab")),
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

/// Catches the install time being lost or crossed between hosts, or moved by a retry: the
/// first call of a host for a version wins (a first attempt that lands late would otherwise
/// start its swap before the recorded time), another host's is its own, a newer version
/// starts its own clock, and a host never announced has none.
#[test]
fn install_times_are_kept_per_host_and_version_and_the_first_call_wins() {
    let now = Arc::new(Mutex::new(10u64));
    let clock = Arc::clone(&now);
    let times = InstallTimes::new(Arc::new(move || *clock.lock().expect("lock")));
    let v = target();
    let newer = Version::parse("0.1.8+g0123456789ab-4").expect("version");
    assert_eq!(times.at("a", &v), None);
    times.note("a", &v);
    *now.lock().expect("lock") = 20;
    times.note("b", &v);
    assert_eq!((times.at("a", &v), times.at("b", &v)), (Some(10), Some(20)));
    *now.lock().expect("lock") = 30;
    times.note("a", &v);
    assert_eq!((times.at("a", &v), times.at("b", &v)), (Some(10), Some(20)));
    times.note("a", &newer);
    assert_eq!(
        (times.at("a", &v), times.at("a", &newer)),
        (Some(10), Some(30))
    );
    assert!(crate::health::system_wall_clock()() > 1_700_000_000_000);
}

/// Catches a frontend being judged like a storage node, or waited on: its health is the
/// refusal at once, with the text that says the proxy's drain control is missing, and the
/// operator API is not asked anything (a version without a commit changes nothing).
#[test]
fn a_frontend_host_is_refused_without_asking_the_api() {
    let server = routes(vec![]);
    let health = adapter(&server, "nh-fe");
    let fe = host("fe", Role::Frontend);
    let refused = Ok(Health::Abandoned(FRONTEND_REFUSAL.to_owned()));
    assert_eq!(health.check(&fe, &target()), refused);
    let bare = Version::parse("2.0-1").expect("version");
    assert_eq!(health.check(&fe, &bare), refused);
    assert_eq!(
        FRONTEND_REFUSAL,
        "frontend updates need the kbf-proxy drain control, which is not built yet"
    );
    // Asked before the installer: the frontend is refused, the other roles are not.
    assert_eq!(health.refusal(&fe), Some(FRONTEND_REFUSAL.to_owned()));
    assert_eq!(health.refusal(&host("st1", Role::Storage)), None);
    assert_eq!(health.refusal(&host("dm", Role::Daemon)), None);
    assert!(server.requests().is_empty());
}

/// Catches a roll that goes past a frontend it cannot judge: a roll over every host puts the
/// frontend first, the roll fails at it with `swap_abandoned` and the refusal text before the
/// installer is called, so the frontend is not changed. No host is installed, checked or
/// asked about, and the operator API is not called.
#[test]
fn a_roll_with_a_frontend_fails_at_it_and_touches_no_host_after() {
    let server = routes(vec![]);
    let rig = Rig::new("nh-roll-fe");
    let ports = Ports {
        safe: Box::new(rig.stop.clone()),
        installer: Box::new(rig.inst.clone()),
        health: Box::new(adapter(&server, "nh-roll-fe")),
        clock: Box::new(rig.clock.clone()),
    };
    let updater = Updater::open(inventory(), rig.store(), ports, Timing::default()).expect("open");
    let started = updater.start(&all(TARGET, "run-1")).expect("start");
    let roll = updater.drive(&started.roll.id).expect("drive");
    assert_eq!(roll.state, RollState::Failed);
    let failure = roll.failure.expect("failure");
    assert_eq!(failure.code, ErrorCode::SwapAbandoned);
    assert!(
        failure.detail.contains(FRONTEND_REFUSAL),
        "{}",
        failure.detail
    );
    // The installer, the safe-to-stop check and the health port are not called for any host,
    // the frontend included.
    assert!(rig.log.events().is_empty(), "{:?}", rig.log.events());
    assert!(server.requests().is_empty());
}

/// Catches a stale install time: a host rolled to v1, then v2, then v1 again must be held to
/// the third install, not the first. A status the v2 process sent between the second and the
/// third install is older than the last install, and must not read as a restart.
#[test]
fn a_later_roll_to_the_same_version_starts_a_new_freshness_bar() {
    let dm = host("dm", Role::Daemon);
    let v1 = target();
    let v2 = Version::parse("0.1.8+g0123456789ab-4").expect("version");
    let server = routes(vec![(
        "/v1/nodes/dm",
        node(true, "serving", Some(INSTALL_MS + 5_000)),
    )]);
    let now = Arc::new(Mutex::new(INSTALL_MS));
    let clock = Arc::clone(&now);
    let health = NodeHealth::with_api(
        api(&server, "nh-dm-rolls"),
        Arc::new(move || *clock.lock().expect("lock")),
    );
    health.installing(&dm, &v1);
    health.settled(&dm, &v1);
    *now.lock().expect("lock") = INSTALL_MS + 2_000;
    health.installing(&dm, &v2);
    health.settled(&dm, &v2);
    *now.lock().expect("lock") = INSTALL_MS + 10_000;
    health.installing(&dm, &v1);
    // The status at +5 s is the v2 process's: before the last install.
    let why = waiting(&health.check(&dm, &v1));
    assert!(why.contains("has not restarted"), "{why}");
}
