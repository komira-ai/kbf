//! A repoint: the one call that moves the store the role `cold` names.

use kbf_state::Store as _;
use kbf_types::object_store_api::{
    AuditAction, AuditList, AuditOutcome, ConnectionInfo, MemberStanding, NodeConvergence,
    NodeStatus, RepointAnswer, Status, StoreIdentity,
};

use super::harness::{BIND_BODY, Farm, SENTINEL, ScriptedProbe, conn_body, registered};

const REPOINT: &str = "/v1/object-stores/connections/cold:repoint";
const OLD: &str = "http://s3.example:9000 farm-bucket/cold/";
const NEW_ENDPOINT: &str = "http://127.0.0.1:30900";

fn full(generation: u64) -> String {
    format!(
        r#"{{"endpoint":"{NEW_ENDPOINT}","bucket":"av2dev1","prefix":"kbf/","expect_generation":{generation}}}"#
    )
}

fn identity(endpoint: &str, bucket: &str, prefix: &str) -> StoreIdentity {
    StoreIdentity {
        endpoint: endpoint.to_owned(),
        bucket: bucket.to_owned(),
        prefix: prefix.to_owned(),
    }
}

/// The bytes of one registration row.
fn row(farm: &Farm, key: &str) -> Option<Vec<u8>> {
    farm.store.get(key).expect("read").map(|v| v.value)
}

async fn status(farm: &Farm) -> Status {
    farm.call("GET", "/v1/object-stores", "").await.json()
}

async fn audit(farm: &Farm) -> Vec<(AuditAction, AuditOutcome)> {
    let list: AuditList = farm.call("GET", "/v1/object-stores/audit", "").await.json();
    list.entries.iter().map(|e| (e.action, e.outcome)).collect()
}

/// The raw rows of the registration, for "nothing was written".
fn rows(farm: &Farm) -> String {
    farm.state_dump()
}

// Catches a repoint that leaves the endpoint, the bucket or the prefix where they were,
// one that changes only some of them (nodes would be told a store that never existed), a
// generation that does not move (no node would learn of it) or moves twice, the connection's
// key replaced or lost, a probe of the old store (or with another key) in place of the new
// one, and nodes that applied the old generation still counted as converged.
#[tokio::test]
async fn a_repoint_replaces_the_identity_and_bumps_the_generation() {
    let farm = Farm::new();
    registered(&farm).await; // generation 2
    farm.acks.set(vec![NodeConvergence {
        id: "n1".to_owned(),
        state: MemberStanding::Active,
        applied_generation: Some(2),
        status: NodeStatus::Ok,
        last_seen_secs: Some(1),
    }]);
    let files = farm.files();

    let reply = farm.call("POST", REPOINT, &full(2)).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let answer: RepointAnswer = reply.json();
    assert_eq!(answer.generation, 3);
    assert!(answer.changed);
    assert_eq!(
        answer.from,
        identity("http://s3.example:9000", "farm-bucket", "cold/")
    );
    assert_eq!(answer.to, identity(NEW_ENDPOINT, "av2dev1", "kbf/"));
    assert_eq!(answer.verified, ScriptedProbe::passing());

    let calls = farm.probe.calls();
    let probed = calls.last().expect("the new store was probed");
    assert_eq!(probed.endpoint, NEW_ENDPOINT);
    assert_eq!(
        (probed.bucket.as_str(), probed.prefix.as_str()),
        ("av2dev1", "kbf/")
    );
    assert_eq!(probed.secret, SENTINEL, "the registered key");
    assert_eq!(probed.access_key_id, "AKID");

    let now = status(&farm).await;
    assert_eq!(now.generation, 3);
    let connection = &now.connections[0];
    assert_eq!(connection.endpoint, NEW_ENDPOINT);
    assert_eq!(connection.updated_generation, 3);
    let binding = &now.bindings[0];
    assert_eq!(
        (binding.bucket.as_str(), binding.prefix.as_str()),
        ("av2dev1", "kbf/")
    );
    assert_eq!(binding.updated_generation, 3);
    assert_eq!(now.convergence.nodes[0].status, NodeStatus::Pending);
    assert!(!now.convergence.converged);
    assert_eq!(farm.files(), files, "the key is not rewritten");
    let shown = farm
        .call("GET", "/v1/object-stores/connections/cold", "")
        .await
        .json::<ConnectionInfo>();
    assert_eq!(shown.access_key_id, "AKID");
}

// Catches bucket and prefix that are not defaulted to the binding's own (a repoint of the
// endpoint alone would blank the prefix and write under the bucket's root), and a given
// empty prefix that is mistaken for an absent one.
#[tokio::test]
async fn a_repoint_with_only_an_endpoint_keeps_the_bucket_and_prefix() {
    let farm = Farm::new();
    registered(&farm).await;
    let body = format!(r#"{{"endpoint":"{NEW_ENDPOINT}","expect_generation":2}}"#);
    let reply = farm.call("POST", REPOINT, &body).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let answer: RepointAnswer = reply.json();
    assert_eq!(answer.to, identity(NEW_ENDPOINT, "farm-bucket", "cold/"));

    let body = format!(
        r#"{{"endpoint":"{NEW_ENDPOINT}","bucket":"other","prefix":"","expect_generation":3}}"#
    );
    let answer: RepointAnswer = farm.call("POST", REPOINT, &body).await.json();
    assert_eq!(answer.to, identity(NEW_ENDPOINT, "other", ""));
    assert_eq!(answer.generation, 4);
}

// Catches a repoint to the identity the registry is at bumping the generation (a retry whose
// answer was lost would re-restart nothing but move every node's `pending`), or leaving
// no audit row, as every other unchanged write does.
#[tokio::test]
async fn a_repoint_to_the_current_identity_changes_nothing() {
    let farm = Farm::new();
    registered(&farm).await;
    let body = r#"{"endpoint":"http://s3.example:9000","expect_generation":2}"#;
    let reply = farm.call("POST", REPOINT, body).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let answer: RepointAnswer = reply.json();
    assert!(!answer.changed);
    assert_eq!(answer.generation, 2);
    assert_eq!(answer.from, answer.to);
    assert_eq!(farm.generation(), 2);
    assert_eq!(
        audit(&farm).await.first(),
        Some(&(AuditAction::Repoint, AuditOutcome::Unchanged))
    );
}

// Catches an `expect_generation` that is ignored or compared loosely, and a refused repoint
// that still wrote a row, an audit row or the generation.
#[tokio::test]
async fn a_repoint_with_a_stale_generation_changes_nothing() {
    let farm = Farm::new();
    registered(&farm).await; // generation 2
    let before = rows(&farm);
    for stale in [0, 1, 3] {
        let reply = farm.call("POST", REPOINT, &full(stale)).await;
        assert_eq!(
            (reply.status, reply.code().as_str()),
            (409, "generation_mismatch"),
            "{stale}: {}",
            reply.body
        );
    }
    assert_eq!(rows(&farm), before);
    assert_eq!(farm.generation(), 2);
}

// Catches a repoint that writes before it has checked the new store (every node would be
// handed an endpoint nothing answers on), and a failed probe that is not reported with its
// steps. A change in the connection's key and the audit log stay as they were.
#[tokio::test]
async fn a_repoint_whose_endpoint_fails_verification_changes_nothing() {
    let farm = Farm::new();
    registered(&farm).await;
    let before = rows(&farm);
    farm.probe.fail_at_get();
    let reply = farm.call("POST", REPOINT, &full(2)).await;
    assert_eq!(
        (reply.status, reply.code().as_str()),
        (422, "probe_failed"),
        "{}",
        reply.body
    );
    let body: serde_json::Value = serde_json::from_str(&reply.body).expect("json");
    assert_eq!(body["verified"]["get"], false);
    assert_eq!(rows(&farm), before);
    assert_eq!(farm.generation(), 2);
}

// Catches a repoint of a connection that no role names (it would move nothing and bump the
// generation), of one that is not the bound one while another is, and of one that does not
// exist: each is refused with its own code and writes nothing.
#[tokio::test]
async fn a_repoint_of_an_unbound_connection_is_refused() {
    let farm = Farm::new();
    let put = farm
        .call(
            "PUT",
            "/v1/object-stores/connections/spare",
            &conn_body(SENTINEL),
        )
        .await;
    assert_eq!(put.status, 201);
    let spare = "/v1/object-stores/connections/spare:repoint";
    let unbound = farm.call("POST", spare, &full(1)).await;
    assert_eq!(
        (unbound.status, unbound.code().as_str()),
        (409, "not_bound"),
        "{}",
        unbound.body
    );

    registered(&farm).await; // `cold` is bound, `spare` is not
    let before = rows(&farm);
    let other = farm.call("POST", spare, &full(3)).await;
    assert_eq!(
        (other.status, other.code().as_str()),
        (409, "not_bound"),
        "{}",
        other.body
    );
    let missing = farm
        .call(
            "POST",
            "/v1/object-stores/connections/nobody:repoint",
            &full(3),
        )
        .await;
    assert_eq!(
        (missing.status, missing.code().as_str()),
        (404, "no_such_connection")
    );
    assert_eq!(rows(&farm), before);
}

// Catches a body the server would read as something else: `verify` is not a field of a
// repoint (it is always probed), the generation is required, an unusable endpoint, bucket or
// prefix is refused before any probe, and https is refused as it is everywhere. Nothing is
// probed and nothing is written.
#[tokio::test]
async fn a_repoint_body_that_is_incomplete_or_unusable_is_refused_before_a_probe() {
    let farm = Farm::new();
    registered(&farm).await;
    let probes = farm.probe.calls().len();
    let before = rows(&farm);
    let cases = [
        (
            format!(r#"{{"endpoint":"{NEW_ENDPOINT}"}}"#),
            400,
            "bad_body",
        ),
        (
            format!(r#"{{"endpoint":"{NEW_ENDPOINT}","expect_generation":2,"verify":false}}"#),
            400,
            "bad_body",
        ),
        (
            r#"{"endpoint":"https://s3.example","expect_generation":2}"#.to_owned(),
            422,
            "tls_unsupported",
        ),
        (
            r#"{"endpoint":"s3.example:9000","expect_generation":2}"#.to_owned(),
            422,
            "invalid_endpoint",
        ),
        (
            format!(
                r#"{{"endpoint":"{NEW_ENDPOINT}","bucket":"NOT A BUCKET","expect_generation":2}}"#
            ),
            422,
            "invalid_bucket",
        ),
        (
            format!(r#"{{"endpoint":"{NEW_ENDPOINT}","prefix":"a/../b","expect_generation":2}}"#),
            422,
            "invalid_prefix",
        ),
    ];
    for (body, status, code) in cases {
        let reply = farm.call("POST", REPOINT, &body).await;
        assert_eq!(
            (reply.status, reply.code().as_str()),
            (status, code),
            "{body}: {}",
            reply.body
        );
    }
    assert_eq!(farm.probe.calls().len(), probes);
    assert_eq!(rows(&farm), before);
}

// Catches a repoint that loses a race to another writer reported as done, or that leaves
// the connection row moved while the binding is not (the two change in one transaction).
#[tokio::test]
async fn a_repoint_that_loses_a_race_changes_nothing() {
    use std::sync::atomic::Ordering::SeqCst;
    let farm = Farm::new();
    registered(&farm).await;
    let conn = row(&farm, "objstore/conn/cold");
    let bind = row(&farm, "objstore/bind/cold");
    farm.flaky.race_next_write.store(true, SeqCst);
    let reply = farm.call("POST", REPOINT, &full(2)).await;
    assert_eq!(
        (reply.status, reply.code().as_str()),
        (409, "conflict"),
        "{}",
        reply.body
    );
    assert_eq!(row(&farm, "objstore/conn/cold"), conn);
    assert_eq!(row(&farm, "objstore/bind/cold"), bind);
}

// Catches an audit row that says only that something was repointed: it must carry the
// identity before and after, the generation the repoint made, and a refused repoint leaves
// no row at all.
#[tokio::test]
async fn a_repoint_is_audited_with_the_old_and_new_identity() {
    let farm = Farm::new();
    registered(&farm).await;
    farm.call("POST", REPOINT, &full(1)).await; // refused
    let reply = farm.call("POST", REPOINT, &full(2)).await;
    assert_eq!(reply.status, 200, "{}", reply.body);
    let list: AuditList = farm.call("GET", "/v1/object-stores/audit", "").await.json();
    assert_eq!(list.entries.len(), 3, "put, bind, repoint: {list:?}");
    let entry = &list.entries[0];
    assert_eq!(entry.action, AuditAction::Repoint);
    assert_eq!(entry.subject, "cold");
    assert_eq!(entry.outcome, AuditOutcome::Changed);
    assert_eq!(entry.generation, 3);
    let new = format!("{NEW_ENDPOINT} av2dev1/kbf/");
    assert_eq!(
        entry.detail.as_deref(),
        Some(format!("{OLD} -> {new}").as_str())
    );
    assert_eq!(list.entries[1].detail, None, "other actions have no detail");
    assert!(
        !farm.state_dump().contains(SENTINEL),
        "no row holds the key"
    );
}

// Pins what exists: this arrives green. A connection PUT that names another endpoint, and a
// bind that names another bucket or prefix, stay refused (409 `identity_change`) while the
// role is bound; only a repoint moves the store.
#[tokio::test]
async fn connection_put_still_refuses_an_identity_change() {
    let farm = Farm::new();
    registered(&farm).await;
    let moved = conn_body(SENTINEL).replace("s3.example:9000", "127.0.0.1:30900");
    let reply = farm
        .call("PUT", "/v1/object-stores/connections/cold", &moved)
        .await;
    assert_eq!(
        (reply.status, reply.code().as_str()),
        (409, "identity_change"),
        "{}",
        reply.body
    );
    let rebind = BIND_BODY.replace("farm-bucket", "av2dev1");
    let reply = farm
        .call("PUT", "/v1/object-stores/bindings/cold", &rebind)
        .await;
    assert_eq!(
        (reply.status, reply.code().as_str()),
        (409, "identity_change"),
        "{}",
        reply.body
    );
    assert_eq!(farm.generation(), 2);
}
