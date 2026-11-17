//! The health of storage hosts against a fake operator API, route by route.

use std::time::Duration;

use serde_json::{Value, json};

use crate::api::{ErrorCode, RollState};
use crate::health::{commit_of, names};
use crate::inventory::{Host, Member, Role};
use crate::node_health::{MEMBER_ELSEWHERE, NO_MEMBER_REFUSAL, NodeHealth};
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

fn host(alias: &str, role: Role) -> Host {
    Host {
        alias: alias.to_owned(),
        address: "secret-host.farm.example".to_owned(),
        role,
        host_key: KEY.to_owned(),
        node_id: None,
        member: None,
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
    OperatorApi::new(server.addr, token)
        .expect("api")
        .with_timeout(Duration::from_millis(500))
}

fn adapter(server: &FakeApi, name: &str) -> NodeHealth {
    NodeHealth::with_api(api(server, name))
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
        )
        .expect("api"),
    );
    let e = dead
        .check(&host("st1", Role::Storage), &target())
        .expect_err("refused");
    assert!(e.is_unreachable(), "{e}");
}

/// Catches a version without a commit being judged: no host can be shown to run it, so a
/// storage host fails at once and nothing is asked.
#[test]
fn a_version_without_a_commit_fails_a_storage_host() {
    let server = storage(vec![member(ADDR, "active", Some(COMMIT))]);
    let v = Version::parse("2.0-1").expect("version");
    let h = host("st1", Role::Storage);
    let Ok(Health::Abandoned(why)) = adapter(&server, "nh-nocommit").check(&h, &v) else {
        panic!("expected a failure");
    };
    assert!(why.contains("names no commit"), "{why}");
    let held = adapter(&server, "nh-nocommit").check_held(&h, &v);
    assert!(matches!(held, Ok(Health::Abandoned(_))), "{held:?}");
    assert!(server.requests().is_empty());
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

/// Asserts that a frontend that is no pool member is refused before it is installed, with the
/// text that says to list it with an instance and a base, that a frontend member is not
/// refused and is not judged here, whatever the version, and that no other role is refused.
/// The operator API is not asked anything.
#[test]
fn a_frontend_host_that_is_no_member_is_refused_without_asking_the_api() {
    let server = routes(vec![]);
    let health = adapter(&server, "nh-fe");
    let fe = host("fe", Role::Frontend);
    let member = Member {
        instance: "a".to_owned(),
        base_port: 18000,
    };
    let pooled = Host {
        member: Some(member),
        ..fe.clone()
    };
    let elsewhere = Ok(Health::Abandoned(MEMBER_ELSEWHERE.to_owned()));
    let bare = Version::parse("2.0-1").expect("version");
    for version in [target(), bare] {
        assert_eq!(health.check(&fe, &version), elsewhere);
        assert_eq!(health.check(&pooled, &version), elsewhere);
        assert_eq!(health.check_held(&pooled, &version), elsewhere);
    }
    // Asked before the installer: the frontend that is no member is refused, nothing else is.
    assert_eq!(health.refusal(&fe), Some(NO_MEMBER_REFUSAL.to_owned()));
    assert_eq!(health.refusal(&pooled), None);
    assert_eq!(health.refusal(&host("st1", Role::Storage)), None);
    assert!(NO_MEMBER_REFUSAL.contains("instance=<name> and base=<port>"));
    assert!(server.requests().is_empty());
}

/// Asserts that a roll over a frontend that is no pool member is refused at it: a roll over
/// every host puts the frontend first, the roll fails at it with `swap_abandoned` and the
/// refusal text before the installer is called, so the frontend is not changed. No host is
/// installed, checked or asked about, and the operator API is
/// not called.
#[test]
fn a_roll_with_a_frontend_that_is_no_member_fails_at_it_and_touches_no_host_after() {
    let server = routes(vec![]);
    let rig = Rig::new("nh-roll-fe");
    let ports = Ports {
        safe: Box::new(rig.stop.clone()),
        installer: Box::new(rig.inst.clone()),
        health: Box::new(adapter(&server, "nh-roll-fe")),
        clock: Box::new(rig.clock.clone()),
        frontend: Box::new(rig.front.clone()),
    };
    let updater = Updater::open(inventory(), rig.store(), ports, Timing::default()).expect("open");
    let started = updater.start(&all(TARGET, "run-1")).expect("start");
    let roll = updater.drive(&started.roll.id).expect("drive");
    assert_eq!(roll.state, RollState::Failed);
    let failure = roll.failure.expect("failure");
    assert_eq!(failure.code, ErrorCode::SwapAbandoned);
    assert!(
        failure.detail.contains(NO_MEMBER_REFUSAL),
        "{}",
        failure.detail
    );
    // The installer, the safe-to-stop check and the health port are not called for any host,
    // the frontend included.
    assert!(rig.log.events().is_empty(), "{:?}", rig.log.events());
    assert!(server.requests().is_empty());
}
