//! An operator's force over HTTP and on disk: who may ask for one, what each refusal answers,
//! what a failed write leaves, what survives a restart, and what the release before reads of
//! the files a force writes (`crate::force`, `crate::ops_file`).

use std::collections::BTreeMap;
use std::fs;

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, StatusCode};
use kbf_testkit::dir::TestDir;
use kbf_types::HostRole;
use kbf_updater::wire::{Health, Heartbeat, Outcome, UpdateResult};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::decide::{Decision, Hold, decide_forced};
use crate::desired::{DesiredStore, StoreError};
use crate::force::FEATURE;
use crate::force_tests::{GATED, agent, force, force_body, operate, rig, send, update};
use crate::http::{DESIRED_PATH, OPERATOR_PATH, Surface, handle};
use crate::ops::{self, AuditEntry, Force, OperateError, OpsError, OpsState, OpsStore, Write};
use crate::ops_downgrade_tests::previous;
use crate::ops_file::{halts_path, joined};
use crate::service::{Coordinator, FEATURES};
use crate::services::Services;
use crate::testutil::{
    OPERATOR, Rig, beat, ci_token, clock_of, fleet, id, last, ops_of, secs, unhealthy, updating,
    version,
};

async fn post(rig: &Rig, path: &str, bearer: Option<&str>, body: &str) -> (StatusCode, Value) {
    let mut builder = Request::builder().method("POST").uri(path);
    if let Some(token) = bearer {
        builder = builder.header("authorization", format!("Bearer {token}"));
    }
    let request = builder
        .body(Full::new(Bytes::from(body.to_owned())))
        .expect("a request");
    let response = handle(
        &rig.coordinator,
        Surface::Control,
        &None,
        request,
        std::time::Duration::from_secs(30),
    )
    .await;
    let code = response.status();
    let bytes = response
        .into_body()
        .collect()
        .await
        .expect("a body")
        .to_bytes();
    (code, serde_json::from_slice(&bytes).expect("JSON"))
}

const DESIRED_2: &str = r#"{"version":"2.0","min_up":{"storage":0,"frontend":0}}"#;

/// Catches: `forced-v1` advertised without the coordinator acting on it, or `forced` sent to an
/// agent that cannot read it: under a force, the agent that announced the name is told it and
/// gets `forced: true` with its order, and the same order to an agent that did not is the bytes
/// it has always read. Without a force, nobody gets `forced`.
pub(crate) async fn forced_v1_is_acted_on() {
    let rig = rig("force-v1-proof", GATED);
    rig.set(DESIRED_2).await;
    let plain = send(&rig, "d1", agent("2.0", Health::Healthy));
    assert!(!plain.forced);
    force(&rig, "1.5", r#","max_updating":{"daemon":2}"#);
    let answer = send(&rig, "d1", agent("2.0", Health::Healthy));
    assert!(
        answer
            .features
            .as_ref()
            .is_some_and(|f| f.iter().any(|n| n == FEATURE))
    );
    assert_eq!(answer, update("1.5"));
    assert!(answer.forced);
    let mut older = agent("2.0", Health::Healthy);
    older.agent_features = vec!["halt-v1".to_owned()];
    let answer = send(&rig, "d2", older);
    assert_eq!(answer, update("1.5"));
    assert!(!answer.forced);
    assert_eq!(
        serde_json::to_string(&answer).expect("json"),
        r#"{"reply":"update","version":"1.5","features":["halt-v1"],"halts":{"kbf":1}}"#
    );
}

#[tokio::test]
async fn forced_v1_is_advertised_and_acted_on() {
    assert!(FEATURES.contains(&FEATURE));
    forced_v1_is_acted_on().await;
}

/// Catches: a force taken from CI's credential, or from none: CI's token is 403 `forbidden`, no
/// bearer and a wrong one are 401, and none of them forces anything.
#[tokio::test]
async fn a_ci_token_cannot_force() {
    let rig = rig("force-http-ci", GATED);
    rig.set(DESIRED_2).await;
    let body = force_body("1.5", "");
    let (code, answer) = post(&rig, OPERATOR_PATH, Some(&ci_token()), &body).await;
    assert_eq!(code, StatusCode::FORBIDDEN);
    assert_eq!(answer, json!({"error": "forbidden"}));
    for bearer in [None, Some("nope")] {
        let (code, answer) = post(&rig, OPERATOR_PATH, bearer, &body).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED, "{bearer:?}");
        assert_eq!(answer, json!({"error": "unauthorized"}));
    }
    assert_eq!(rig.coordinator.ops().marks("kbf").force, None);
    let (code, entry) = post(&rig, OPERATOR_PATH, Some(OPERATOR), &body).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(entry["action"], "force");
    assert_eq!(entry["by"], "operator");
}

/// Catches: the refusals of a force and of CI's write of the version it left told apart from
/// nothing, or told with text: a force below `min_version` is 400 `below_min_version`, a bad
/// one 400 `bad_request`, and CI's desired state of the version left is 409 `held_bad`.
#[tokio::test]
async fn the_refusals_answer_their_codes() {
    let rig = rig("force-http-codes", GATED);
    rig.set(DESIRED_2).await;
    let (code, answer) = post(&rig, OPERATOR_PATH, Some(OPERATOR), &force_body("0.9", "")).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert_eq!(answer, json!({"error": "below_min_version"}));
    let bad = force_body("1.5", r#","max_updating":{"daemon":0}"#);
    let (code, answer) = post(&rig, OPERATOR_PATH, Some(OPERATOR), &bad).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert_eq!(answer, json!({"error": "bad_request"}));
    force(&rig, "1.5", "");
    let (code, answer) = post(&rig, DESIRED_PATH, Some(&ci_token()), DESIRED_2).await;
    assert_eq!(code, StatusCode::CONFLICT);
    assert_eq!(answer, json!({"error": "held_bad"}));
}

/// Catches: a force half made when the ops file cannot be written: the answer is 500 and the
/// marks, the halt and the desired state are as they were.
#[tokio::test]
async fn a_force_whose_marks_cannot_be_written_changes_nothing() {
    let mut rig = rig("force-http-unwritable", GATED);
    rig.set(DESIRED_2).await;
    let writer = Box::new(|_: &OpsState| {
        Err(OpsError::Io(StoreError::Io {
            path: "ops.json".into(),
            source: std::io::Error::other("disk full"),
        }))
    });
    rig.coordinator = Rig::start(&rig.dir, &rig.clock)
        .with_services(Services::parse(GATED).expect("services"))
        .with_ops(OpsStore::with_writer(OpsState::default(), writer));
    let (code, answer) = post(&rig, OPERATOR_PATH, Some(OPERATOR), &force_body("1.5", "")).await;
    assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(answer, json!({"error": "internal"}));
    assert_eq!(rig.coordinator.ops().state(), OpsState::default());
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    assert_eq!(status.services["kbf"].version, version("2.0"));
}

/// Catches: a force whose desired state could not be written answered as made: the answer is
/// 500, the marks are written (the force is recorded, the version left is marked), and since
/// the desired version is not the forced one no force stands; the same force asked again over
/// a writable store stands.
#[tokio::test]
async fn a_force_whose_desired_state_cannot_be_written_fails_and_is_asked_again() {
    let mut rig = rig("force-http-desired", GATED);
    rig.set(DESIRED_2).await;
    // A desired state in a directory that is not there: read as none, never written.
    let gone =
        DesiredStore::open(&rig.dir.path().join("gone").join("desired.json")).expect("no file");
    let (verifier, _) = kbf_oidc::testkit::static_verifier();
    rig.coordinator = Coordinator::new(gone, verifier, OPERATOR.to_owned(), clock_of(&rig.clock))
        .with_services(Services::parse(GATED).expect("services"))
        .with_ops(ops_of(&rig.dir));
    let err = operate(&rig, &force_body("1.5", "")).expect_err("not written");
    assert!(matches!(err, OperateError::Desired(_)));
    assert!(
        err.to_string()
            .starts_with("the desired state could not be written: ")
    );
    let (code, answer) = post(&rig, OPERATOR_PATH, Some(OPERATOR), &force_body("1.5", "")).await;
    assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(answer, json!({"error": "internal"}));
    let marks = rig.coordinator.ops().marks("kbf");
    assert_eq!(marks.force.as_ref().map(|f| f.generation), Some(2));
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    assert!(status.per_service["kbf"].forced.is_none());
    // The marks of the attempts are on disk: a start over the real desired state stands forced
    // once the force is asked again.
    rig.coordinator =
        Rig::start(&rig.dir, &rig.clock).with_services(Services::parse(GATED).expect("services"));
    force(&rig, "1.5", "");
    let status = rig.coordinator.status(Some(OPERATOR)).expect("status");
    assert_eq!(
        status.per_service["kbf"]
            .forced
            .as_ref()
            .map(|f| f.generation),
        Some(3)
    );
}

/// Catches: a force or an unmark taken by the ops store alone, which has no desired state to set
/// and no fleet to read: `ops::operate` refuses both, and changes nothing.
#[test]
fn the_ops_store_alone_does_not_force_or_unmark() {
    let store = OpsStore::volatile();
    for body in [
        force_body("1.5", ""),
        r#"{"action":"unmark_bad","service":"kbf","version":"1.5","reason":"r"}"#.to_owned(),
    ] {
        let err = ops::operate(&store, &Services::default(), "operator", body.as_bytes())
            .expect_err("refused");
        assert_eq!(
            err.to_string(),
            "a force or an unmark is made by the coordinator, not the ops store alone"
        );
    }
    assert_eq!(store.state(), OpsState::default());
}

/// Catches: the forced decision answering for what it was not asked: a service with no
/// definition, one with no desired state, and a host the coordinator has not heard from are
/// held for those reasons, as the ordinary decision holds them.
#[test]
fn the_forced_decision_holds_what_it_cannot_decide() {
    let services = Services::default();
    let marks = ops::ServiceOps::default();
    let fleet = fleet(secs(100), &[]);
    let desired = BTreeMap::from([("kbf".to_owned(), crate::testutil::desired("1.5", 0, 0))]);
    let none = BTreeMap::new();
    let ask = |service: &str, desired: &BTreeMap<_, _>| {
        decide_forced(
            &services,
            desired,
            &marks,
            service,
            &fleet,
            &id("d1"),
            secs(100),
        )
    };
    assert_eq!(ask("nope", &desired), Decision::Hold(Hold::NoDefinition));
    assert_eq!(ask("kbf", &none), Decision::Hold(Hold::NoDesired));
    assert_eq!(ask("kbf", &desired), Decision::Hold(Hold::NotAMember));
}

/// Catches: a force that a restart forgets: the force, its generation and the hold on the mark
/// it made are read back from the halts file, and the hosts are still forced.
#[tokio::test]
async fn a_force_survives_a_restart() {
    let mut rig = rig("force-restart", GATED);
    rig.set(DESIRED_2).await;
    send(&rig, "d1", agent("2.0", Health::Healthy));
    force(&rig, "1.5", r#","max_updating":{"daemon":4}"#);
    let before = rig.coordinator.ops().marks("kbf");
    crate::force_tests::restart(&mut rig, GATED);
    let after = rig.coordinator.ops().marks("kbf");
    assert_eq!(after.force, before.force);
    assert_eq!(after.force_generation, 1);
    assert_eq!(after.bad_by_force, [version("2.0")]);
    let answer = send(&rig, "d1", agent("2.0", Health::Healthy));
    assert_eq!(answer, update("1.5"));
    assert!(answer.forced);
}

/// The halts file reader of the release before the force release: its records as it read them,
/// keys it does not know skipped.
#[derive(Deserialize)]
#[allow(dead_code)] // Read only to be accepted.
struct PreviousHalts {
    #[serde(default)]
    services: BTreeMap<String, PreviousService>,
    #[serde(default)]
    ops: Option<String>,
}

#[derive(Deserialize)]
#[allow(dead_code)] // Read only to be accepted.
struct PreviousService {
    #[serde(default)]
    halt: Option<Value>,
    #[serde(default)]
    halt_acked: Vec<Value>,
    #[serde(default)]
    halt_seen: Vec<Value>,
    #[serde(default)]
    halt_generation: u64,
}

fn forced_state(state: &mut OpsState) {
    let marks = state.services.entry("kbf".to_owned()).or_default();
    marks.bad.push(version("2.0"));
    marks.bad_by_force.push(version("2.0"));
    marks.force_generation = 3;
    marks.halt_generation = 1;
    marks.force = Some(Force {
        version: version("1.5"),
        generation: 3,
        reason: "2.0 corrupts leases".to_owned(),
        max_updating: BTreeMap::from([("daemon".to_owned(), 3)]),
    });
}

fn written(name: &str) -> (TestDir, std::path::PathBuf) {
    let dir = TestDir::under(std::env::temp_dir(), name);
    let path = dir.path().join("ops.json");
    OpsStore::open(&path)
        .expect("a store")
        .change(
            Write::BeforeMemory,
            AuditEntry::new("operator", "seed"),
            forced_state,
        )
        .expect("written");
    (dir, path)
}

/// Catches: a force's record written where the release before refuses it (B8: the release
/// before on this release's data): its ops file has no key of the force, and its halts file
/// reader takes the halts file, whose force keys it skips. This release reads all of it back.
#[test]
fn the_release_before_reads_the_files_of_a_force() {
    let (_dir, path) = written("force-prev-reads");
    let text = fs::read_to_string(&path).expect("ops");
    assert_eq!(previous::reads(&text), Ok(()), "{text}");
    assert!(!text.contains("force"), "{text}");
    let halts = fs::read_to_string(halts_path(&path)).expect("halts");
    assert!(
        serde_json::from_str::<PreviousHalts>(&halts).is_ok(),
        "{halts}"
    );
    let mut want = OpsState::default();
    forced_state(&mut want);
    let back = OpsStore::open(&path).expect("read").marks("kbf");
    assert_eq!(back.force, want.services["kbf"].force);
    assert_eq!(back.force_generation, 3);
    assert_eq!(back.bad_by_force, [version("2.0")]);
}

/// Catches: this release reading more into a halts file than the ops file beside it supports, or
/// refusing one it cannot read all of: a hold on a mark the release before cleared (CI set the
/// version) is dropped; a force record or a held mark it cannot read is passed over and counted,
/// and the rest is read (B8: this release on the release before's, or a later one's, data).
#[test]
fn a_halts_file_is_read_against_its_ops_file_and_what_cannot_be_read_is_skipped() {
    let (_dir, path) = written("force-this-reads");
    let mut ops: Value =
        serde_json::from_str(&fs::read_to_string(&path).expect("ops")).expect("json");
    ops["services"]["kbf"]["bad"] = json!([]);
    let state: OpsState = serde_json::from_value(ops).expect("an ops state");
    let (joined_state, skipped) = joined(&path, state.clone()).expect("joined");
    assert_eq!(skipped, 0);
    assert!(joined_state.services["kbf"].bad_by_force.is_empty());
    assert!(joined_state.services["kbf"].force.is_some());

    let mut halts: Value =
        serde_json::from_str(&fs::read_to_string(halts_path(&path)).expect("halts")).expect("json");
    halts["services"]["kbf"]["force"] = json!({"version": 7});
    halts["services"]["kbf"]["bad_by_force"] = json!(["2.0", ["not", "a", "version"]]);
    halts["services"]["kbf"]["force"]["later"] = json!(true);
    fs::write(halts_path(&path), serde_json::to_vec(&halts).expect("json")).expect("written");
    let base: OpsState =
        serde_json::from_str(&fs::read_to_string(&path).expect("ops")).expect("an ops state");
    let (read, skipped) = joined(&path, base).expect("joined");
    assert_eq!(skipped, 2);
    assert_eq!(read.services["kbf"].force, None);
    assert_eq!(read.services["kbf"].bad_by_force, [version("2.0")]);
    assert_eq!(read.services["kbf"].force_generation, 3);

    // A field a later release adds to a force is skipped, and the force is read.
    halts["services"]["kbf"]["force"] =
        json!({"version":"1.5","generation":3,"reason":"r","later":true});
    fs::write(halts_path(&path), serde_json::to_vec(&halts).expect("json")).expect("written");
    let base: OpsState =
        serde_json::from_str(&fs::read_to_string(&path).expect("ops")).expect("an ops state");
    let (read, _) = joined(&path, base).expect("joined");
    assert_eq!(
        read.services["kbf"]
            .force
            .as_ref()
            .map(|f| f.version.clone()),
        Some(version("1.5"))
    );
}

/// Catches: the count of forces lost for a service the ops file no longer lists, so a later
/// force would repeat a generation already told: it is kept, as a halt's clear generation is.
#[test]
fn a_force_generation_is_kept_for_a_service_the_ops_file_no_longer_lists() {
    let (_dir, path) = written("force-gen-kept");
    let (read, _) = joined(&path, OpsState::default()).expect("joined");
    assert_eq!(read.services["kbf"].force_generation, 3);
}

/// The marks of a force to 1.5 that raised nothing, with `acked` failure reports.
fn forced_marks(acked: Vec<ops::FailureAck>) -> ops::ServiceOps {
    ops::ServiceOps {
        force: Some(Force {
            version: version("1.5"),
            generation: 1,
            reason: "r".to_owned(),
            max_updating: BTreeMap::new(),
        }),
        failure_acked: acked,
        ..ops::ServiceOps::default()
    }
}

fn failed(target: &str) -> UpdateResult {
    UpdateResult::new(version(target), Outcome::Failed)
}

/// Catches: the forced decision ordering a host that cannot take it, or holding one for a
/// failure that is not of the target: a host whose `kbf` is absent is held; a host whose last
/// update failed another version is ordered; one that failed the target is ordered only while
/// the force acknowledges that very report.
#[test]
fn the_forced_decision_reads_absence_and_failures_by_their_target() {
    let services = Services::default();
    let desired = BTreeMap::from([("kbf".to_owned(), crate::testutil::desired("1.5", 0, 0))]);
    let now = secs(100);
    let ask = |b: Heartbeat, marks: &ops::ServiceOps| {
        let fleet = fleet(secs(99), &[("d1", HostRole::DAEMON, b)]);
        decide_forced(&services, &desired, marks, "kbf", &fleet, &id("d1"), now)
    };
    let order = Decision::Update {
        version: version("1.5"),
    };
    let mut absent = beat("2.0");
    absent.health = Health::Absent;
    assert_eq!(
        ask(absent, &forced_marks(vec![])),
        Decision::Hold(Hold::Absent)
    );
    let other = last(beat("2.0"), "1.9", Outcome::Failed);
    assert_eq!(ask(other, &forced_marks(vec![])), order);
    let here = last(beat("2.0"), "1.5", Outcome::Failed);
    assert_eq!(
        ask(here.clone(), &forced_marks(vec![])),
        Decision::Hold(Hold::FailedHere)
    );
    let ack = ops::FailureAck {
        node_id: "d1".to_owned(),
        report: failed("1.5"),
    };
    assert_eq!(ask(here, &forced_marks(vec![ack])), order);
}

/// Catches: a forced slot counted for a host that no longer works on the force, or not counted
/// for one that does: for an agent that announced `forced-v1`, a live host updating or under an
/// order holds one, a silent one does not, and one at the forced version holds none whatever
/// its health; for an agent that did not, the ordinary slot rule answers.
#[test]
fn a_forced_slot_is_held_by_live_work_on_the_force_only() {
    use crate::fleet::{Fleet, Node};
    let node = |features: &[&str]| Node {
        role: "daemon".to_owned(),
        features: features.iter().map(|f| (*f).to_owned()).collect(),
        seen: secs(10),
        announced: BTreeMap::new(),
    };
    let forced = version("1.5");
    let mut fleet = Fleet::new(secs(0));
    let host = |b: Heartbeat, fleet: &mut Fleet, name: &str, features: &[&str]| {
        fleet.record(id(name), HostRole::DAEMON, b, secs(10));
        fleet.note_node(id(name), node(features));
    };
    host(updating(beat("2.0"), "1.5"), &mut fleet, "d1", &[FEATURE]);
    let mut sick = beat("1.5");
    sick.health = Health::Unhealthy;
    host(sick, &mut fleet, "d2", &[FEATURE]);
    host(
        unhealthy(last(beat("2.0"), "1.5", Outcome::Failed)),
        &mut fleet,
        "d3",
        &[],
    );
    host(beat("2.0"), &mut fleet, "d4", &[FEATURE]);
    fleet.order("kbf", id("d4"), forced.clone(), secs(10));
    let holds = |name: &str, now: u64| {
        let h = fleet.get(&id(name)).expect("a host");
        fleet.holds_forced_slot("kbf", &id(name), h, &forced, secs(now))
    };
    assert!(holds("d1", 20));
    assert!(!holds("d1", 100), "silent past the down limit");
    assert!(!holds("d2", 20));
    assert!(holds("d3", 20), "the ordinary rule: failed and not healthy");
    assert!(holds("d4", 20));
}

/// Catches: a version left that was already bad marked twice, or not held: it is in `bad` and
/// in `bad_by_force` once, and its `mark_bad` is audited all the same; a host's failure of
/// another version is not acknowledged by the force.
#[tokio::test]
async fn a_force_from_a_version_already_bad_holds_it_once() {
    let rig = rig("force-left-bad", GATED);
    rig.set(DESIRED_2).await;
    rig.coordinator
        .ops()
        .change(
            Write::BeforeMemory,
            AuditEntry::new("test", "mark_bad"),
            |state| {
                let marks = state.services.entry("kbf".to_owned()).or_default();
                marks.bad.push(version("2.0"));
            },
        )
        .expect("marked");
    let mut other = agent("2.0", Health::Healthy);
    other.last_update = Some(failed("1.9"));
    send(&rig, "d1", other);
    force(&rig, "1.5", "");
    let marks = rig.coordinator.ops().marks("kbf");
    assert_eq!(marks.bad, [version("2.0")]);
    assert_eq!(marks.bad_by_force, [version("2.0")]);
    assert!(marks.failure_acked.is_empty());
    let actions: Vec<String> = rig
        .coordinator
        .ops()
        .state()
        .audit
        .iter()
        .map(|e| e.action.clone())
        .collect();
    assert!(actions.ends_with(&[
        "force".to_owned(),
        "mark_bad".to_owned(),
        "clear_halt".to_owned()
    ]));
}
