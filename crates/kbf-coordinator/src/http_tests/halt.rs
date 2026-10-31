//! Over HTTP: a host's halt report stops its service, only the operator clears it, and the
//! clear reaches an agent that announced `halt-v1` as a generation.

use std::sync::atomic::{AtomicBool, Ordering};

use super::*;
use crate::http::{CONVERGENCE_PATH, OPERATOR_PATH};
use crate::ops::{OpsError, OpsState, OpsStore};
use crate::testutil::Log;

const SERVICES_TEXT: &str = "[[service]]\nname = \"other\"\n[service.roles.daemon]\nmin_up = 0\n";

/// The report of a hook that halted the update to 2.0.
const HALT: &str =
    r#","last_update":{"target":"2.0","outcome":"failed","phase":"halt","reason":"disk_tainted"}"#;

fn beat(version: &str, extra: &str) -> String {
    format!(r#"{{"platform":"linux-x86_64","version":"{version}","health":"healthy"{extra}}}"#)
}

async fn send(rig: &Rig, host: &str, role: &str, body: &str) -> Value {
    let (code, reply) = call(
        rig,
        Surface::Heartbeat,
        &updater(host, role),
        request("POST", HEARTBEAT_PATH, None, body),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    reply
}

async fn send_raw(rig: &Rig, host: &str, role: &str, body: &str) -> String {
    let (code, bytes) = raw(
        rig,
        Surface::Heartbeat,
        &updater(host, role),
        request("POST", HEARTBEAT_PATH, None, body),
    )
    .await;
    assert_eq!(code, 200);
    String::from_utf8(bytes).expect("text")
}

async fn operate(rig: &Rig, bearer: Option<&str>, body: &str) -> (StatusCode, Value) {
    call(
        rig,
        Surface::Control,
        &None,
        request("POST", OPERATOR_PATH, bearer, body),
    )
    .await
}

fn clear_body(service: &str) -> String {
    format!(r#"{{"action":"clear_halt","service":"{service}","reason":"disk replaced"}}"#)
}

async fn clear(rig: &Rig, service: &str) -> Value {
    let (code, entry) = operate(rig, Some(OPERATOR), &clear_body(service)).await;
    assert_eq!(code, StatusCode::OK, "{entry}");
    entry
}

async fn convergence(rig: &Rig, service: &str) -> Value {
    let path = format!("{CONVERGENCE_PATH}?service={service}");
    let (code, body) = call(
        rig,
        Surface::Control,
        &None,
        request("GET", &path, Some(&ci_token()), ""),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    body
}

/// Every host of `service` with the rule that holds its order back, by node id.
async fn holds(rig: &Rig, service: &str) -> Vec<(String, Value)> {
    let status = status(rig).await;
    status["per_service"][service]["hosts"]
        .as_array()
        .expect("hosts")
        .iter()
        .map(|h| {
            (
                h["node_id"].as_str().expect("id").to_owned(),
                h["hold"].clone(),
            )
        })
        .collect()
}

/// A rig past the quiet start whose `kbf` wants 2.0 with no hosts required up.
async fn farm(name: &str) -> Rig {
    let rig = Rig::new(name);
    rig.at(1_100);
    rig.set(r#"{"version":"2.0","min_up":{"storage":0,"frontend":0}}"#)
        .await;
    rig
}

/// The same, over a services file that adds `other` (a daemon service wanting 3.0) and over
/// ops writes that `fail` makes fail.
async fn farm_of_two(name: &str, fail: &Arc<AtomicBool>) -> Rig {
    let mut rig = Rig::new(name);
    let list = format!(
        "{}services = [\"kbf\", \"other\"]\n",
        kbf_oidc::testkit::config_text(kbf_oidc::testkit::STATIC_ISSUER)
    );
    let fail = Arc::clone(fail);
    let writer = Box::new(move |_: &OpsState| {
        if fail.load(Ordering::SeqCst) {
            return Err(OpsError::Io(crate::desired::StoreError::Io {
                path: "ops.json".into(),
                source: std::io::Error::other("disk full"),
            }));
        }
        Ok(())
    });
    rig.coordinator = Rig::start_with_allow_list(&rig.dir, &rig.clock, &list)
        .with_services(crate::services::Services::parse(SERVICES_TEXT).expect("services"))
        .with_ops(OpsStore::with_writer(OpsState::default(), writer));
    for body in [
        r#"{"version":"2.0","min_up":{"storage":0,"frontend":0}}"#,
        r#"{"service":"other","version":"3.0","min_up":{"storage":0,"frontend":0}}"#,
    ] {
        let (code, _) = raw(
            &rig,
            Surface::Control,
            &None,
            request("POST", DESIRED_PATH, Some(&ci_token()), body),
        )
        .await;
        assert_eq!(code, 200);
    }
    rig.at(1_100);
    rig
}

fn calm() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}

/// Catches: a halt that stops only the host that reported it, or only one role: after the
/// report no host of any role is ordered (the daemon and the storage host would each be ordered
/// without it) and every host of the service, the frontend included, shows `halted` as the rule
/// that holds it.
#[tokio::test]
async fn a_halt_report_stops_every_order_of_the_service_on_every_role() {
    let rig = farm("halt-every-role").await;
    let ordered = send(&rig, "d1", "daemon", &beat("1.0", "")).await;
    assert_eq!(ordered, json!({"reply": "update", "version": "2.0"}));
    send(&rig, "s1", "storage", &beat("1.0", HALT)).await;
    let nothing = json!({"reply": "nothing"});
    assert_eq!(send(&rig, "d2", "daemon", &beat("1.0", "")).await, nothing);
    assert_eq!(send(&rig, "s2", "storage", &beat("1.0", "")).await, nothing);
    assert_eq!(
        send(&rig, "f1", "frontend", &beat("1.0", "")).await,
        nothing
    );
    let held = holds(&rig, "kbf").await;
    assert_eq!(held.len(), 5);
    for (host, hold) in held {
        assert_eq!(hold, "halted", "{host}");
    }
}

/// Catches: a halt of one service stopping another: with `kbf` halted by a storage host, a
/// daemon is still ordered the update of `other`; with `other` halted by that daemon, a second
/// daemon is still ordered `kbf`.
#[tokio::test]
async fn other_services_are_untouched() {
    let rig = farm_of_two("halt-isolated", &calm()).await;
    let other = r#","services":[{"name":"other","version":"1.0","health":"healthy"}]"#;
    send(&rig, "s1", "storage", &beat("1.0", HALT)).await;
    let reply = send(&rig, "d1", "daemon", &beat("2.0", other)).await;
    assert_eq!(
        reply,
        json!({"reply": "update", "service": "other", "version": "3.0"})
    );
    assert_eq!(convergence(&rig, "other").await["state"], "in_progress");

    clear(&rig, "kbf").await;
    let halted = format!(
        r#","services":[{{"name":"other","version":"1.0","health":"healthy"{}}}]"#,
        HALT.replace("2.0", "3.0")
    );
    send(&rig, "d3", "daemon", &beat("2.0", &halted)).await;
    assert_eq!(convergence(&rig, "other").await["state"], "halted");
    let kbf = send(&rig, "d4", "daemon", &beat("1.0", other)).await;
    assert_eq!(kbf, json!({"reply": "update", "version": "2.0"}));
}

/// Catches: a new desired version lifting a halt: after the version is set to 3.0 the halt of
/// the update to 2.0 stands, nobody is ordered 3.0, and the convergence still names it.
#[tokio::test]
async fn a_new_desired_version_does_not_clear_a_halt() {
    let rig = farm("halt-new-desired").await;
    send(&rig, "s1", "storage", &beat("1.0", HALT)).await;
    rig.set(r#"{"version":"3.0","min_up":{"storage":0,"frontend":0}}"#)
        .await;
    let nothing = json!({"reply": "nothing"});
    assert_eq!(send(&rig, "d1", "daemon", &beat("1.0", "")).await, nothing);
    let state = convergence(&rig, "kbf").await;
    assert_eq!(state["state"], "halted");
    assert_eq!(state["desired"], "3.0");
    assert_eq!(state["halt"]["target"], "2.0");
}

/// Catches: a halt held only in memory: after a restart, before any host has said anything,
/// the service is halted, the file holds the halt with the host and the check, and a host is
/// still ordered nothing.
#[tokio::test]
async fn a_halt_survives_a_restart() {
    let mut rig = farm("halt-restart").await;
    send(&rig, "s1", "storage", &beat("1.0", HALT)).await;
    rig.restart();
    rig.at(1_200);
    assert_eq!(convergence(&rig, "kbf").await["state"], "halted");
    let nothing = json!({"reply": "nothing"});
    assert_eq!(send(&rig, "d1", "daemon", &beat("1.0", "")).await, nothing);
    let file = std::fs::read_to_string(rig.dir.path().join("desired.json.ops")).expect("file");
    let file: Value = serde_json::from_str(&file).expect("json");
    assert_eq!(
        file["services"]["kbf"]["halt"],
        json!({"node_id": "s1", "check": "disk_tainted", "target": "2.0"})
    );
}

/// Catches: the halt being cleared by anyone but the operator: no bearer, CI's token, a wrong
/// token and a token of another scheme are each 401 and leave the halt and its generation as
/// they were; the operator token clears it, and the answer is the audit entry.
#[tokio::test]
async fn only_the_operator_token_clears_a_halt() {
    let rig = farm("halt-token").await;
    send(&rig, "s1", "storage", &beat("1.0", HALT)).await;
    let ci = ci_token();
    for bearer in [None, Some(ci.as_str()), Some("nope")] {
        let (code, body) = operate(&rig, bearer, &clear_body("kbf")).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED, "{bearer:?}");
        assert_eq!(body, json!({"error": "unauthorized"}));
    }
    assert_eq!(convergence(&rig, "kbf").await["state"], "halted");
    let entry = clear(&rig, "kbf").await;
    assert_eq!(entry["action"], "clear_halt");
    assert_eq!(entry["service"], "kbf");
    assert_eq!(entry["node_id"], "s1");
    assert_eq!(entry["reason"], "disk replaced");
    assert_eq!(entry["by"], "operator");
    assert_ne!(convergence(&rig, "kbf").await["state"], "halted");
}

/// Catches: a clear taken for a service that is not halted, unknown, or with no reason: each is
/// a 400 that changes nothing, so no generation is bumped for a clear that cleared nothing.
#[tokio::test]
async fn a_clear_needs_a_halted_known_service_and_a_reason() {
    let rig = farm("halt-clear-bad").await;
    send(&rig, "s1", "storage", &beat("1.0", HALT)).await;
    for body in [
        clear_body("nope"),
        r#"{"action":"clear_halt","service":"kbf","reason":""}"#.to_owned(),
        r#"{"action":"clear_halt","service":"kbf"}"#.to_owned(),
        r#"{"action":"clear_halt","reason":"x"}"#.to_owned(),
    ] {
        let (code, _) = operate(&rig, Some(OPERATOR), &body).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
    }
    assert_eq!(convergence(&rig, "kbf").await["state"], "halted");
    clear(&rig, "kbf").await;
    let (code, _) = operate(&rig, Some(OPERATOR), &clear_body("kbf")).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    let ops = status(&rig).await["ops"].clone();
    assert_eq!(ops["services"]["kbf"]["halt_generation"], 1);
}

/// Catches: the halted host reporting the same `last_update` until it updates again and so
/// raising the halt again at once: after the clear the same report raises nothing, the service
/// is blocked on that host (it is not ordered 2.0 again), another host is ordered, and the audit
/// holds the halt and the clear and no second halt.
#[tokio::test]
async fn a_cleared_halt_is_not_raised_again_by_the_same_report() {
    let rig = farm("halt-same-report").await;
    send(&rig, "s1", "storage", &beat("1.0", HALT)).await;
    clear(&rig, "kbf").await;
    let nothing = json!({"reply": "nothing"});
    assert_eq!(
        send(&rig, "s1", "storage", &beat("1.0", HALT)).await,
        nothing
    );
    let state = convergence(&rig, "kbf").await;
    assert_eq!(state["state"], "blocked");
    assert!(state.get("halt").is_none());
    let ordered = send(&rig, "d1", "daemon", &beat("1.0", "")).await;
    assert_eq!(ordered, json!({"reply": "update", "version": "2.0"}));
    let audit = status(&rig).await["ops"]["audit"].clone();
    let actions: Vec<&str> = audit
        .as_array()
        .expect("audit")
        .iter()
        .map(|e| e["action"].as_str().expect("action"))
        .collect();
    assert_eq!(actions, ["halt", "clear_halt"]);
}

/// Catches: the acknowledgement covering more than the report it cleared: after the clear, the
/// same host halting a newer update halts the service again, and so does another host reporting
/// the version that was cleared; a second clear replaces the host's acknowledgement.
#[tokio::test]
async fn a_new_halt_after_a_clear_halts_again() {
    let rig = farm("halt-again").await;
    send(&rig, "s1", "storage", &beat("1.0", HALT)).await;
    clear(&rig, "kbf").await;
    rig.set(r#"{"version":"3.0","min_up":{"storage":0,"frontend":0}}"#)
        .await;
    send(
        &rig,
        "s1",
        "storage",
        &beat("1.0", &HALT.replace("2.0", "3.0")),
    )
    .await;
    let state = convergence(&rig, "kbf").await;
    assert_eq!(state["state"], "halted");
    assert_eq!(state["halt"]["target"], "3.0");
    clear(&rig, "kbf").await;
    send(&rig, "s2", "storage", &beat("1.0", HALT)).await;
    let state = convergence(&rig, "kbf").await;
    assert_eq!(state["halt"]["node_id"], "s2");
    clear(&rig, "kbf").await;
    let acked = status(&rig).await["ops"]["services"]["kbf"]["halt_acked"].clone();
    assert_eq!(
        acked,
        json!([
            {"node_id": "s1", "target": "3.0"},
            {"node_id": "s2", "target": "2.0"}
        ])
    );
}

/// Catches: a halt feeding rollback or changing the desired state: after a halt and a clear the
/// desired version is the one CI set and the service has no `bad` version and no `last_good`.
#[tokio::test]
async fn a_halt_is_never_counted_toward_rollback() {
    let rig = farm("halt-no-rollback").await;
    for host in ["s1", "s2", "s3"] {
        send(&rig, host, "storage", &beat("1.0", HALT)).await;
    }
    let marks = status(&rig).await["ops"]["services"]["kbf"].clone();
    assert_eq!(marks["bad"], json!([]));
    assert_eq!(marks["last_good"], Value::Null);
    clear(&rig, "kbf").await;
    let status = status(&rig).await;
    assert_eq!(status["services"]["kbf"]["version"], "2.0");
    assert_eq!(status["ops"]["services"]["kbf"]["bad"], json!([]));
    assert_eq!(status["ops"]["services"]["kbf"]["last_good"], Value::Null);
}

/// Catches: `halted` missing from the convergence or hiding the host: the state is `halted`
/// and names the node, the check and the version, while the same report with another phase is
/// `blocked` and names no halt; a report with no reason names the check `unspecified`.
#[tokio::test]
async fn convergence_says_halted_and_names_the_host_and_check() {
    let rig = farm("halt-convergence").await;
    let after_commit = HALT.replace("halt", "after_commit");
    send(&rig, "s1", "storage", &beat("1.0", &after_commit)).await;
    let state = convergence(&rig, "kbf").await;
    assert_eq!(state["state"], "blocked");
    assert!(state.get("halt").is_none());
    send(&rig, "s2", "storage", &beat("1.0", HALT)).await;
    let state = convergence(&rig, "kbf").await;
    assert_eq!(state["state"], "halted");
    assert_eq!(
        state["halt"],
        json!({"node_id": "s2", "check": "disk_tainted", "target": "2.0"})
    );
    let rig = farm("halt-convergence-bare").await;
    let bare = r#","last_update":{"target":"2.0","outcome":"failed","phase":"halt"}"#;
    send(&rig, "s1", "storage", &beat("1.0", bare)).await;
    assert_eq!(
        convergence(&rig, "kbf").await["halt"]["check"],
        "unspecified"
    );
}

/// Catches: reports that are no halt halting: a failure after the commit, one with no phase, a
/// success that names the phase, and a beat with no update at all leave the service running.
#[tokio::test]
async fn only_a_failed_update_in_the_halt_phase_halts() {
    let rig = farm("halt-only-halt").await;
    for (n, last) in [
        r#","last_update":{"target":"2.0","outcome":"failed","phase":"after_commit"}"#,
        r#","last_update":{"target":"2.0","outcome":"failed"}"#,
        r#","last_update":{"target":"2.0","outcome":"succeeded","phase":"halt"}"#,
        "",
    ]
    .iter()
    .enumerate()
    {
        send(&rig, &format!("s{n}"), "storage", &beat("1.0", last)).await;
        assert_ne!(convergence(&rig, "kbf").await["state"], "halted", "{last}");
    }
}

/// Catches: a second report replacing the halt that stands: the host that halted first stays
/// the one the convergence names.
#[tokio::test]
async fn a_second_halt_report_does_not_replace_the_standing_one() {
    let rig = farm("halt-first-stands").await;
    send(&rig, "s1", "storage", &beat("1.0", HALT)).await;
    let other = HALT.replace("disk_tainted", "other_check");
    send(&rig, "s2", "storage", &beat("1.0", &other)).await;
    let halt = convergence(&rig, "kbf").await["halt"].clone();
    assert_eq!(halt["node_id"], "s1");
    assert_eq!(halt["check"], "disk_tainted");
    let audit = status(&rig).await["ops"]["audit"].clone();
    assert_eq!(audit.as_array().expect("audit").len(), 1);
}

/// Catches: a halt from a host an operator took out of the service, or from one whose role the
/// service does not run on: neither halts the service.
#[tokio::test]
async fn an_excluded_or_foreign_hosts_halt_report_is_ignored() {
    let rig = farm_of_two("halt-ignored", &calm()).await;
    let exclude = r#"{"action":"exclude","service":"kbf","node_id":"s1","reason":"bad disk"}"#;
    assert_eq!(
        operate(&rig, Some(OPERATOR), exclude).await.0,
        StatusCode::OK
    );
    send(&rig, "s1", "storage", &beat("1.0", HALT)).await;
    assert_ne!(convergence(&rig, "kbf").await["state"], "halted");

    // `other` runs on daemons only: a storage host that lists it is not a member.
    let foreign =
        format!(r#","services":[{{"name":"other","version":"1.0","health":"healthy"{HALT}}}]"#);
    send(&rig, "s2", "storage", &beat("2.0", &foreign)).await;
    assert_ne!(convergence(&rig, "other").await["state"], "halted");
    assert!(status(&rig).await["ops"]["services"].get("other").is_none());
}

/// Catches: a halt that is lifted because it could not be written down: with every ops write
/// failing, the report still halts the service (nobody is ordered), the failure is logged at
/// error naming the cause, and the page field `alert="halt"` is logged at error.
#[tokio::test]
async fn a_halt_holds_when_the_ops_file_cannot_be_written() {
    let fail = Arc::new(AtomicBool::new(true));
    let rig = farm_of_two("halt-unwritable", &fail).await;
    let log = Log::default();
    let _installed = log.install();
    send(&rig, "s1", "storage", &beat("1.0", HALT)).await;
    let nothing = json!({"reply": "nothing"});
    assert_eq!(send(&rig, "d1", "daemon", &beat("1.0", "")).await, nothing);
    assert_eq!(convergence(&rig, "kbf").await["state"], "halted");
    let text = log.text();
    assert!(text.contains("ERROR"), "{text}");
    assert!(text.contains("disk full"), "{text}");
    assert!(text.contains(r#"alert="halt""#), "{text}");
    assert!(text.contains("disk_tainted"), "{text}");
}

/// Catches: a clear that is only in memory or counted before it is written: with the write
/// failing the answer is 500, the halt stands, no generation is announced; the same clear once
/// the write works clears the halt and announces generation 1.
#[tokio::test]
async fn a_clear_that_cannot_be_written_leaves_the_halt() {
    let fail = calm();
    let rig = farm_of_two("halt-clear-unwritable", &fail).await;
    send(&rig, "s1", "storage", &beat("1.0", HALT)).await;
    fail.store(true, Ordering::SeqCst);
    let (code, body) = operate(&rig, Some(OPERATOR), &clear_body("kbf")).await;
    assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, json!({"error": "internal"}));
    assert_eq!(convergence(&rig, "kbf").await["state"], "halted");
    let agent = beat("2.0", r#","agent_features":["halt-v1"]"#);
    assert_eq!(
        send_raw(&rig, "d1", "daemon", &agent).await,
        r#"{"reply":"nothing","features":["halt-v1"]}"#
    );
    fail.store(false, Ordering::SeqCst);
    clear(&rig, "kbf").await;
    assert_ne!(convergence(&rig, "kbf").await["state"], "halted");
    assert_eq!(
        send_raw(&rig, "d1", "daemon", &agent).await,
        r#"{"reply":"nothing","features":["halt-v1"],"halts":{"kbf":1}}"#
    );
}

/// Pins: the bytes. Before any clear an agent that announced `halt-v1` is answered with
/// `features` alone; an agent that announced nothing, or something else, never gets `halts`
/// (a strict reader refuses it), even after a clear.
#[tokio::test]
async fn halts_go_only_to_an_agent_that_announced_halt_v1() {
    let rig = farm("halt-gate").await;
    let v1 = beat("2.0", r#","agent_features":["halt-v1"]"#);
    assert_eq!(
        send_raw(&rig, "d1", "daemon", &v1).await,
        r#"{"reply":"nothing","features":["halt-v1"]}"#
    );
    send(&rig, "s1", "storage", &beat("1.0", HALT)).await;
    clear(&rig, "kbf").await;
    assert_eq!(
        send_raw(&rig, "d1", "daemon", &v1).await,
        r#"{"reply":"nothing","features":["halt-v1"],"halts":{"kbf":1}}"#
    );
    assert_eq!(
        send_raw(&rig, "d1", "daemon", &beat("2.0", "")).await,
        r#"{"reply":"nothing"}"#
    );
    let other = beat("2.0", r#","agent_features":["deferred-v1"]"#);
    assert_eq!(
        send_raw(&rig, "d1", "daemon", &other).await,
        r#"{"reply":"nothing","features":[]}"#
    );
}

/// Catches: a generation that does not count clears or is not kept: each clear of a service
/// raises its own generation by one, the generations of the services are listed together by name,
/// and a restarted coordinator announces what it announced before.
#[tokio::test]
async fn the_generation_counts_clears_per_service_and_survives_a_restart() {
    let rig = farm_of_two("halt-generation", &calm()).await;
    let agent = beat("2.0", r#","agent_features":["halt-v1"]"#);
    let halted = format!(
        r#","services":[{{"name":"other","version":"1.0","health":"healthy"{}}}]"#,
        HALT.replace("2.0", "3.0")
    );
    for round in 1..=2 {
        send(
            &rig,
            "s1",
            "storage",
            &beat("1.0", &HALT.replace("2.0", &format!("2.{round}"))),
        )
        .await;
        clear(&rig, "kbf").await;
        let text = send_raw(&rig, "d1", "daemon", &agent).await;
        assert_eq!(
            text,
            format!(r#"{{"reply":"nothing","features":["halt-v1"],"halts":{{"kbf":{round}}}}}"#)
        );
    }
    send(&rig, "d3", "daemon", &beat("2.0", &halted)).await;
    clear(&rig, "other").await;
    assert_eq!(
        send_raw(&rig, "d1", "daemon", &agent).await,
        r#"{"reply":"nothing","features":["halt-v1"],"halts":{"kbf":2,"other":1}}"#
    );
}

/// Catches: the generation held in memory only: after a restart over the same files the agent
/// is still told generation 1, so an agent that halted under 0 lifts its hold even though the
/// heartbeat that carried the first answer was lost.
#[tokio::test]
async fn a_restarted_coordinator_still_announces_the_generation() {
    let mut rig = farm("halt-generation-restart").await;
    send(&rig, "s1", "storage", &beat("1.0", HALT)).await;
    clear(&rig, "kbf").await;
    rig.restart();
    rig.at(1_300);
    let agent = beat("2.0", r#","agent_features":["halt-v1"]"#);
    assert_eq!(
        send_raw(&rig, "d1", "daemon", &agent).await,
        r#"{"reply":"nothing","features":["halt-v1"],"halts":{"kbf":1}}"#
    );
    // The cleared report is still acknowledged: the same host reporting it halts nothing.
    send(&rig, "s1", "storage", &beat("1.0", HALT)).await;
    assert_ne!(convergence(&rig, "kbf").await["state"], "halted");
}
