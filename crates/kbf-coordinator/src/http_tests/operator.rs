//! Over HTTP: `POST /v1/operator` takes the operator token only, validates the action,
//! persists it, audits it, and changes nothing a host is told.

use super::*;
use crate::http::{MAX_OPERATOR_BYTES, OPERATOR_PATH};
use crate::ops::{
    Exclusion, HostOps, MAX_EXCLUDED, MAX_HOSTS, OperateError, OpsError, OpsState, OpsStore,
};
use crate::testutil::Log;

const EXCLUDE: &str = r#"{"action":"exclude","service":"kbf","node_id":"d1","reason":"disk swap"}"#;
const INCLUDE: &str = r#"{"action":"include","service":"kbf","node_id":"d1"}"#;

async fn post(rig: &Rig, bearer: Option<&str>, body: &str) -> (StatusCode, Value) {
    call(
        rig,
        Surface::Control,
        &None,
        request("POST", OPERATOR_PATH, bearer, body),
    )
    .await
}

fn ops_file(rig: &Rig) -> std::path::PathBuf {
    rig.dir.path().join("desired.json.ops")
}

/// A coordinator whose ops writes all fail.
fn unwritable(rig: &mut Rig) {
    let writer = Box::new(|_: &OpsState| {
        Err(OpsError::Io(crate::desired::StoreError::Io {
            path: "ops.json".into(),
            source: std::io::Error::other("disk full"),
        }))
    });
    rig.coordinator = Rig::start(&rig.dir, &rig.clock)
        .with_ops(OpsStore::with_writer(OpsState::default(), writer));
}

/// Catches: the operator route taking CI's credential (or none): no bearer, CI's OIDC token,
/// a wrong token, the operator token with a character added or removed, and a bearer in
/// another scheme are each 401 and change nothing: no ops file, no status section. Only the
/// operator token makes the change, and the answer is the audit entry it made.
#[tokio::test]
async fn only_the_operator_token_posts_an_operator_action() {
    let rig = Rig::new("op-token");
    let ci = ci_token();
    let longer = format!("{OPERATOR}x");
    let shorter = &OPERATOR[1..];
    for bearer in [
        None,
        Some(ci.as_str()),
        Some("nope"),
        Some(longer.as_str()),
        Some(shorter),
    ] {
        let (code, body) = post(&rig, bearer, EXCLUDE).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED, "{bearer:?}");
        assert_eq!(body, json!({"error": "unauthorized"}));
    }
    let basic = hyper::Request::builder()
        .method("POST")
        .uri(OPERATOR_PATH)
        .header("authorization", format!("Basic {OPERATOR}"))
        .body(Full::new(Bytes::from(EXCLUDE)))
        .expect("a request");
    let (code, _) = call(&rig, Surface::Control, &None, basic).await;
    assert_eq!(code, StatusCode::UNAUTHORIZED);
    assert!(!ops_file(&rig).exists());
    assert!(status(&rig).await.get("ops").is_none());

    let (code, entry) = post(&rig, Some(OPERATOR), EXCLUDE).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        entry,
        json!({"seq":1,"by":"operator","action":"exclude","service":"kbf","node_id":"d1","reason":"disk swap"})
    );
}

/// Catches: the token being checked after the body is parsed (a body naming an unknown
/// service must not be told apart from any other body by a caller with no token).
#[tokio::test]
async fn a_caller_without_the_token_learns_nothing_about_the_body() {
    let rig = Rig::new("op-token-first");
    for body in ["", "not json", r#"{"action":"explode"}"#, EXCLUDE] {
        let (code, answer) = post(&rig, None, body).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED, "{body}");
        assert_eq!(answer, json!({"error": "unauthorized"}));
    }
}

/// Catches: an exclusion that is only in memory: it shows in the status, is in the file, and
/// is there after a restart.
#[tokio::test]
async fn an_exclusion_survives_a_restart() {
    let mut rig = Rig::new("op-exclude-restart");
    let (code, _) = post(&rig, Some(OPERATOR), EXCLUDE).await;
    assert_eq!(code, StatusCode::OK);
    let want = json!([{"service":"kbf","node_id":"d1","reason":"disk swap"}]);
    assert_eq!(status(&rig).await["ops"]["excluded"], want);
    rig.restart();
    assert_eq!(status(&rig).await["ops"]["excluded"], want);
    assert_eq!(status(&rig).await["ops"]["audit"][0]["seq"], 1);
}

/// Catches: an include that is not remembered (the exclusion comes back after a restart) or
/// that removes another host's exclusion.
#[tokio::test]
async fn include_removes_the_exclusion_and_survives_a_restart() {
    let mut rig = Rig::new("op-include-restart");
    post(&rig, Some(OPERATOR), EXCLUDE).await;
    let other = EXCLUDE.replace("d1", "d2");
    post(&rig, Some(OPERATOR), &other).await;
    let (code, entry) = post(&rig, Some(OPERATOR), INCLUDE).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(entry["action"], "include");
    assert_eq!(entry["seq"], 3);
    assert!(entry.get("reason").is_none());
    let want = json!([{"service":"kbf","node_id":"d2","reason":"disk swap"}]);
    assert_eq!(status(&rig).await["ops"]["excluded"], want);
    rig.restart();
    assert_eq!(status(&rig).await["ops"]["excluded"], want);
    // Including a host that is not excluded is no error and is still audited.
    let (code, entry) = post(&rig, Some(OPERATOR), INCLUDE).await;
    assert_eq!((code, &entry["seq"]), (StatusCode::OK, &json!(4)));
}

/// Catches: a second exclusion of one host and service adding a row (the first reason
/// would then outlive the include of the second).
#[tokio::test]
async fn a_second_exclusion_of_the_same_host_replaces_the_reason() {
    let rig = Rig::new("op-exclude-twice");
    post(&rig, Some(OPERATOR), EXCLUDE).await;
    let again = EXCLUDE.replace("disk swap", "  new disk  ");
    let (code, _) = post(&rig, Some(OPERATOR), &again).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        status(&rig).await["ops"]["excluded"],
        json!([{"service":"kbf","node_id":"d1","reason":"new disk"}])
    );
}

/// Catches: the file being made before it is needed (an upgraded farm's files change with
/// no feature in use): heartbeats, a desired state, status reads and refused actions leave no
/// ops file; the first accepted action creates it, owner-only.
#[tokio::test]
async fn no_ops_file_exists_until_the_first_action() {
    use std::os::unix::fs::PermissionsExt;
    let rig = Rig::new("op-lazy");
    rig.set(STATE).await;
    call(
        &rig,
        Surface::Heartbeat,
        &updater("d1", "daemon"),
        request("POST", HEARTBEAT_PATH, None, BEAT),
    )
    .await;
    status(&rig).await;
    post(&rig, Some(OPERATOR), r#"{"action":"explode"}"#).await;
    post(&rig, None, EXCLUDE).await;
    assert!(!ops_file(&rig).exists());
    post(&rig, Some(OPERATOR), EXCLUDE).await;
    let mode = std::fs::metadata(ops_file(&rig))
        .expect("created")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

/// Catches: an action accepted with a part the coordinator cannot stand behind: each of these
/// is a 400 `bad_request`, the answer says nothing about which part, and no state, file or
/// audit entry results. They are: not JSON, an unknown action, an unknown key, a missing
/// field, an unknown service, a bad node id, an empty, over-long or control-character reason,
/// more than 8 labels, and a label key or value that is empty, too long or has a bad character.
#[tokio::test]
async fn a_bad_action_is_refused_with_400_and_changes_nothing() {
    let rig = Rig::new("op-bad");
    let long = "x".repeat(257);
    let nine: String = (0..9).map(|n| format!(r#""k{n}":"v","#)).collect();
    let nine = format!(
        r#"{{"action":"set_labels","node_id":"d1","labels":{{{}}}}}"#,
        nine.trim_end_matches(',')
    );
    let bodies = vec![
        String::new(),
        "not json".to_owned(),
        "[]".to_owned(),
        r#"{"action":"explode"}"#.to_owned(),
        r#"{"service":"kbf"}"#.to_owned(),
        EXCLUDE.replace('}', r#","extra":1}"#),
        r#"{"action":"exclude","service":"kbf","node_id":"d1"}"#.to_owned(),
        EXCLUDE.replace(r#""kbf""#, r#""nope""#),
        INCLUDE.replace(r#""kbf""#, r#""nope""#),
        EXCLUDE.replace(r#""d1""#, r#""D 1""#),
        INCLUDE.replace(r#""d1""#, r#""-d""#),
        EXCLUDE.replace("disk swap", ""),
        EXCLUDE.replace("disk swap", "   "),
        EXCLUDE.replace("disk swap", &long),
        EXCLUDE.replace("disk swap", r"two\nlines"),
        nine,
        r#"{"action":"set_labels","node_id":"d1","labels":{"":"a"}}"#.to_owned(),
        r#"{"action":"set_labels","node_id":"d1","labels":{"a":""}}"#.to_owned(),
        r#"{"action":"set_labels","node_id":"d1","labels":{"Zone":"a"}}"#.to_owned(),
        r#"{"action":"set_labels","node_id":"d1","labels":{"zone":"a b"}}"#.to_owned(),
        format!(
            r#"{{"action":"set_labels","node_id":"d1","labels":{{"zone":"{}"}}}}"#,
            "a".repeat(33)
        ),
        r#"{"action":"set_labels","node_id":"d1","labels":{"zone":1}}"#.to_owned(),
        r#"{"action":"set_labels","node_id":"D1","labels":{}}"#.to_owned(),
    ];
    for body in &bodies {
        let (code, answer) = post(&rig, Some(OPERATOR), body).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(answer, json!({"error": "bad_request"}), "{body}");
    }
    assert!(!ops_file(&rig).exists());
    assert!(status(&rig).await.get("ops").is_none());
}

/// Catches: a body over the limit being read whole, and the wrong method on the route: a
/// body one byte over [`MAX_OPERATOR_BYTES`] is a 400 and changes nothing; GET and PUT are 405;
/// the route is not on the heartbeat listener.
#[tokio::test]
async fn the_route_takes_post_on_the_control_listener_and_bounded_bodies() {
    let rig = Rig::new("op-route");
    let padded = format!("{EXCLUDE}{}", " ".repeat(MAX_OPERATOR_BYTES));
    let (code, _) = post(&rig, Some(OPERATOR), &padded).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert!(!ops_file(&rig).exists());
    for method in ["GET", "PUT"] {
        let (code, _) = call(
            &rig,
            Surface::Control,
            &None,
            request(method, OPERATOR_PATH, Some(OPERATOR), ""),
        )
        .await;
        assert_eq!(code, StatusCode::METHOD_NOT_ALLOWED, "{method}");
    }
    let (code, _) = call(
        &rig,
        Surface::Heartbeat,
        &updater("d1", "daemon"),
        request("POST", OPERATOR_PATH, Some(OPERATOR), EXCLUDE),
    )
    .await;
    assert_eq!(code, StatusCode::NOT_FOUND);
}

/// Catches: labels that merge instead of replace, that cannot be removed, or that are not
/// remembered: a second set replaces the first, an empty set removes the host's entry, and the
/// labels of another host are untouched, across a restart.
#[tokio::test]
async fn labels_replace_the_old_set_and_an_empty_set_removes_it() {
    let mut rig = Rig::new("op-labels");
    let set = |node: &str, labels: &str| {
        format!(r#"{{"action":"set_labels","node_id":"{node}","labels":{labels}}}"#)
    };
    post(
        &rig,
        Some(OPERATOR),
        &set("d1", r#"{"zone":"a","tier":"gold"}"#),
    )
    .await;
    post(&rig, Some(OPERATOR), &set("d2", r#"{"zone":"b"}"#)).await;
    let (code, entry) = post(&rig, Some(OPERATOR), &set("d1", r#"{"zone-1":"c-d"}"#)).await;
    assert_eq!(code, StatusCode::OK);
    assert_eq!(
        entry,
        json!({"seq":3,"by":"operator","action":"set_labels","node_id":"d1"})
    );
    rig.restart();
    assert_eq!(
        status(&rig).await["ops"]["hosts"],
        json!({"d1":{"labels":{"zone-1":"c-d"}},"d2":{"labels":{"zone":"b"}}})
    );
    post(&rig, Some(OPERATOR), &set("d1", "{}")).await;
    assert_eq!(
        status(&rig).await["ops"]["hosts"],
        json!({"d2":{"labels":{"zone":"b"}}})
    );
}

/// Catches: a host able to select itself: a heartbeat carrying `labels` (or an exclusion, or
/// an ops table) changes nothing the operator set, creates no file, and shows no ops section.
#[tokio::test]
async fn labels_come_only_from_the_operator() {
    let rig = Rig::new("op-labels-heartbeat");
    let lying = r#"{"platform":"linux-x86_64","version":"1.0","health":"healthy",
        "labels":{"zone":"a"},"hosts":{"d1":{"labels":{"zone":"a"}}},"excluded":[]}"#;
    let (code, _) = call(
        &rig,
        Surface::Heartbeat,
        &updater("d1", "daemon"),
        request("POST", HEARTBEAT_PATH, None, lying),
    )
    .await;
    assert_eq!(code, StatusCode::OK);
    assert!(status(&rig).await.get("ops").is_none());
    assert!(!ops_file(&rig).exists());
    post(
        &rig,
        Some(OPERATOR),
        r#"{"action":"set_labels","node_id":"d1","labels":{"zone":"b"}}"#,
    )
    .await;
    call(
        &rig,
        Surface::Heartbeat,
        &updater("d1", "daemon"),
        request("POST", HEARTBEAT_PATH, None, lying),
    )
    .await;
    assert_eq!(
        status(&rig).await["ops"]["hosts"],
        json!({"d1":{"labels":{"zone":"b"}}})
    );
}

/// Catches: a change nobody can see afterwards: every action is logged on the audit target
/// at info with who made it, what it was, the service, the node and the reason, in the order
/// made; a refused action logs no change line.
#[tokio::test]
async fn every_action_is_logged_with_who_and_what() {
    let rig = Rig::new("op-audit-log");
    let log = Log::default();
    let _installed = log.install();
    post(&rig, Some(OPERATOR), EXCLUDE).await;
    post(
        &rig,
        Some(OPERATOR),
        r#"{"action":"set_labels","node_id":"d2","labels":{"zone":"a"}}"#,
    )
    .await;
    post(&rig, Some(OPERATOR), INCLUDE).await;
    post(&rig, Some(OPERATOR), r#"{"action":"explode"}"#).await;
    post(&rig, None, EXCLUDE).await;
    let text = log.text();
    let lines: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("kbf_coordinator::audit"))
        .collect();
    assert_eq!(lines.len(), 3, "{text}");
    for line in &lines {
        assert!(line.contains("INFO"), "{line}");
        assert!(line.contains("by=operator"), "{line}");
    }
    for want in [
        "seq=1",
        "action=exclude",
        "service=kbf",
        "node_id=d1",
        "reason=disk swap",
    ] {
        assert!(lines[0].contains(want), "{want}: {}", lines[0]);
    }
    for want in ["seq=2", "action=set_labels", "node_id=d2"] {
        assert!(lines[1].contains(want), "{want}: {}", lines[1]);
    }
    for want in ["seq=3", "action=include"] {
        assert!(lines[2].contains(want), "{want}: {}", lines[2]);
    }
}

/// Catches: a failed write reported as success or leaving the change in memory: the answer is
/// 500 `internal` (no path in it), the status shows no change, and the failure is logged at
/// error naming the cause.
#[tokio::test]
async fn an_action_that_cannot_be_written_is_a_500_and_changes_nothing() {
    let mut rig = Rig::new("op-unwritable");
    unwritable(&mut rig);
    let log = Log::default();
    let _installed = log.install();
    let (code, body) = post(&rig, Some(OPERATOR), EXCLUDE).await;
    assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(body, json!({"error": "internal"}));
    assert!(status(&rig).await.get("ops").is_none());
    let text = log.text();
    assert!(text.contains("ERROR"), "{text}");
    assert!(text.contains("disk full"), "{text}");
}

/// Catches: the ring not shown, or shown whole: after 66 actions the status holds the last 64,
/// numbered 3 to 66, oldest first.
#[tokio::test]
async fn the_audit_ring_keeps_the_last_64_and_status_shows_it() {
    let mut rig = Rig::new("op-ring");
    rig.coordinator = Rig::start(&rig.dir, &rig.clock).with_ops(OpsStore::volatile());
    for _ in 0..66 {
        let (code, _) = post(&rig, Some(OPERATOR), INCLUDE).await;
        assert_eq!(code, StatusCode::OK);
    }
    let ring = status(&rig).await["ops"]["audit"].clone();
    let seqs: Vec<u64> = ring
        .as_array()
        .expect("a ring")
        .iter()
        .map(|e| e["seq"].as_u64().expect("seq"))
        .collect();
    assert_eq!(seqs, (3..=66).collect::<Vec<u64>>());
}

/// Catches: labels reaching a decision of a service that has no selector: the same heartbeats
/// against the same desired state get the same replies, and the same per-service status, with
/// labels set on a host as without. (Exclusions do change what a host is told: they are
/// tested in `membership_tests`.)
#[tokio::test]
async fn labels_do_not_change_what_a_host_is_told_without_a_selector() {
    async fn run(name: &str, operate: bool) -> (Vec<Value>, Value) {
        let rig = Rig::new(name);
        rig.set(STATE).await;
        if operate {
            let body = r#"{"action":"set_labels","node_id":"f1","labels":{"zone":"a"}}"#;
            assert_eq!(post(&rig, Some(OPERATOR), body).await.0, StatusCode::OK);
        }
        let mut replies = Vec::new();
        for at in [1_100, 1_200, 1_300] {
            rig.at(at);
            for (name, role) in [("d1", "daemon"), ("s1", "storage"), ("f1", "frontend")] {
                let (code, reply) = call(
                    &rig,
                    Surface::Heartbeat,
                    &updater(name, role),
                    request("POST", HEARTBEAT_PATH, None, BEAT),
                )
                .await;
                assert_eq!(code, StatusCode::OK);
                replies.push(reply);
            }
        }
        let mut status = status(&rig).await;
        let per_service = status["per_service"].take();
        (replies, per_service)
    }
    let plain = run("op-decide-plain", false).await;
    let operated = run("op-decide-operated", true).await;
    assert!(
        plain.0.iter().any(|r| r["reply"] == "update"),
        "{:?}",
        plain.0
    );
    assert_eq!(plain, operated);
}

/// Catches: the token checked only after the body is read: a caller with no token or a wrong
/// one is refused with 401 before any byte of the body is taken, so a body over the limit does
/// not turn the answer into a 400, and a body that never ends does not hold the connection
/// for the patience and then answer 400. `operate` refuses such a caller too.
#[tokio::test]
async fn a_caller_without_the_token_is_refused_before_the_body_is_read() {
    let rig = Rig::new("op-token-before-read");
    let oversized = format!("{EXCLUDE}{}", " ".repeat(MAX_OPERATOR_BYTES));
    for bearer in [None, Some("nope")] {
        let (code, body) = post(&rig, bearer, &oversized).await;
        assert_eq!(code, StatusCode::UNAUTHORIZED, "{bearer:?}");
        assert_eq!(body, json!({"error": "unauthorized"}));

        let mut builder = Request::builder().method("POST").uri(OPERATOR_PATH);
        if let Some(token) = bearer {
            builder = builder.header("authorization", format!("Bearer {token}"));
        }
        let stalled = builder.body(Stalled).expect("a request");
        let answered = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            handle(
                &rig.coordinator,
                Surface::Control,
                &None,
                stalled,
                std::time::Duration::from_secs(3_600),
            ),
        )
        .await
        .expect("refused without waiting for the body");
        assert_eq!(answered.status(), StatusCode::UNAUTHORIZED, "{bearer:?}");
    }
    for bearer in [None, Some("nope")] {
        let refused = rig.coordinator.operate(bearer, EXCLUDE.as_bytes());
        assert!(matches!(refused, Err(OperateError::Unauthorized(_))));
    }
    assert!(!ops_file(&rig).exists());
}

/// A coordinator whose ops state already holds `hosts` labelled hosts and `excluded`
/// exclusions of `kbf`, named `n0`, `n1`, ...
fn full(rig: &mut Rig, hosts: usize, excluded: usize) {
    let mut state = OpsState::default();
    for n in 0..hosts {
        state.hosts.insert(
            format!("n{n}"),
            HostOps {
                labels: [("a".to_owned(), "b".to_owned())].into(),
            },
        );
    }
    for n in 0..excluded {
        state.excluded.push(Exclusion {
            service: "kbf".to_owned(),
            node_id: format!("n{n}"),
            reason: "r".to_owned(),
        });
    }
    let writer = Box::new(|_: &OpsState| Ok(()));
    rig.coordinator =
        Rig::start(&rig.dir, &rig.clock).with_ops(OpsStore::with_writer(state, writer));
}

/// Catches: a file that grows without bound: with the file at its caps, a new labelled host
/// or a new exclusion is a 400 and the log names the cap and where to raise it; what does not
/// grow it is still accepted (replacing a host's labels, removing them, replacing an
/// exclusion, an include), and after an include one more exclusion fits.
#[tokio::test]
async fn the_ops_file_counts_are_capped() {
    let mut rig = Rig::new("op-caps");
    full(&mut rig, MAX_HOSTS, MAX_EXCLUDED);
    let log = Log::default();
    let _installed = log.install();
    let labels = |node: &str, set: &str| {
        format!(r#"{{"action":"set_labels","node_id":"{node}","labels":{set}}}"#)
    };
    let exclude = |node: &str| EXCLUDE.replace("d1", node);
    let (code, _) = post(&rig, Some(OPERATOR), &labels("new", r#"{"a":"b"}"#)).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert!(log.text().contains("MAX_HOSTS"), "{}", log.text());
    assert!(log.text().contains("4096"), "{}", log.text());
    let (code, _) = post(&rig, Some(OPERATOR), &exclude("new")).await;
    assert_eq!(code, StatusCode::BAD_REQUEST);
    assert!(log.text().contains("MAX_EXCLUDED"), "{}", log.text());

    for body in [
        labels("n0", r#"{"c":"d"}"#),
        labels("n1", "{}"),
        labels("absent", "{}"),
        exclude("n0"),
        INCLUDE.replace("d1", "n1"),
    ] {
        let (code, _) = post(&rig, Some(OPERATOR), &body).await;
        assert_eq!(code, StatusCode::OK, "{body}");
    }
    let (code, _) = post(&rig, Some(OPERATOR), &exclude("new")).await;
    assert_eq!(code, StatusCode::OK);
    let (code, _) = post(&rig, Some(OPERATOR), &labels("new", r#"{"a":"b"}"#)).await;
    assert_eq!(code, StatusCode::OK);
}

/// Catches: a label key given twice in one body taken last-wins (the operator meant one of
/// them): each of these is a 400 and changes nothing; a body that is not an object of labels
/// is a 400 too.
#[tokio::test]
async fn duplicate_label_keys_are_refused() {
    let rig = Rig::new("op-dup-labels");
    for labels in [
        r#"{"a":"x","a":"y"}"#,
        r#"{"a":"x","b":"y","a":"x"}"#,
        "[]",
        r#""a""#,
    ] {
        let body = format!(r#"{{"action":"set_labels","node_id":"d1","labels":{labels}}}"#);
        let (code, answer) = post(&rig, Some(OPERATOR), &body).await;
        assert_eq!(code, StatusCode::BAD_REQUEST, "{labels}");
        assert_eq!(answer, json!({"error": "bad_request"}));
    }
    assert!(!ops_file(&rig).exists());
    let ok = r#"{"action":"set_labels","node_id":"d1","labels":{"a":"x","b":"x"}}"#;
    assert_eq!(post(&rig, Some(OPERATOR), ok).await.0, StatusCode::OK);
}
