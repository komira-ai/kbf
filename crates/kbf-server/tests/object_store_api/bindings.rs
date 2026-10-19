//! Bindings: the role `cold`, what it refuses, and when it cannot be removed.

use kbf_types::object_store_api::{
    BindingPutAnswer, Deleted, MemberStanding, NodeConvergence, NodeStatus, Status,
};

use super::harness::{BIND_BODY, Farm, SENTINEL, conn_body};

const COLD: &str = "/v1/object-stores/bindings/cold";

fn node(id: &str, applied: u64, status: NodeStatus, state: MemberStanding) -> NodeConvergence {
    NodeConvergence {
        id: id.to_owned(),
        state,
        applied_generation: Some(applied),
        status,
        last_seen_secs: Some(1),
    }
}

async fn with_connection() -> Farm {
    let farm = Farm::new();
    let put = farm
        .call(
            "PUT",
            "/v1/object-stores/connections/cold",
            &conn_body(SENTINEL),
        )
        .await;
    assert_eq!(put.status, 201);
    farm
}

// Catches a first bind that answers anything but 201 and the new generation, a probe that
// does not look at the bucket being bound (a typo in the bucket would register and fail on
// every node), and an idempotent retry that bumps the generation or answers `changed:true`.
#[tokio::test]
async fn a_bind_probes_its_bucket_and_a_retry_changes_nothing() {
    let farm = with_connection().await;
    let bound = farm.call("PUT", COLD, BIND_BODY).await;
    assert_eq!(bound.status, 201, "{}", bound.body);
    let answer: BindingPutAnswer = bound.json();
    assert_eq!((answer.generation, answer.changed), (2, true));
    assert_eq!(
        answer.verified,
        Some(super::harness::ScriptedProbe::passing())
    );
    let calls = farm.probe.calls();
    assert_eq!(calls.len(), 1);
    assert_eq!(
        (calls[0].bucket.as_str(), calls[0].prefix.as_str()),
        ("farm-bucket", "cold/")
    );
    assert_eq!(calls[0].secret, SENTINEL, "the connection's stored key");
    assert_eq!(calls[0].endpoint, "http://s3.example:9000");

    let again = farm.call("PUT", COLD, BIND_BODY).await;
    assert_eq!(again.status, 200);
    let again: BindingPutAnswer = again.json();
    assert_eq!((again.generation, again.changed), (2, false));
    assert_eq!(farm.generation(), 2);

    // The verify field may turn the probe off.
    let farm = with_connection().await;
    let quiet = BIND_BODY.replace("}", r#","verify":false}"#);
    let bound = farm.call("PUT", COLD, &quiet).await;
    assert_eq!(bound.status, 201);
    assert_eq!(bound.json::<BindingPutAnswer>().verified, None);
    assert!(farm.probe.calls().is_empty());
}

// Catches a bind that commits after a failed probe, one that names a connection that does not
// exist, a bad bucket or prefix, and roles other than `cold` (`history` and `backup` have no
// consumer; nonsense is no role): each is refused with its own closed code and writes nothing.
#[tokio::test]
async fn a_bind_that_cannot_work_is_refused_and_writes_nothing() {
    let farm = with_connection().await;
    farm.probe.fail_at_get();
    let failed = farm.call("PUT", COLD, BIND_BODY).await;
    assert_eq!(
        (failed.status, failed.code().as_str()),
        (422, "probe_failed")
    );
    assert_eq!(farm.generation(), 1);
    assert_eq!(
        farm.call("GET", "/v1/object-stores", "")
            .await
            .json::<Status>()
            .bindings
            .len(),
        0
    );
    *farm.probe.result.lock().expect("result") = super::harness::ScriptedProbe::passing();

    let cases = [
        (
            COLD,
            r#"{"connection":"missing","bucket":"farm-bucket"}"#,
            422,
            "unknown_connection",
        ),
        (
            COLD,
            r#"{"connection":"cold","bucket":"NOT A BUCKET"}"#,
            422,
            "invalid_bucket",
        ),
        (
            COLD,
            r#"{"connection":"cold","bucket":"ab"}"#,
            422,
            "invalid_bucket",
        ),
        (
            COLD,
            r#"{"connection":"cold","bucket":"farm-bucket","prefix":"a/../b"}"#,
            422,
            "invalid_prefix",
        ),
        (
            COLD,
            r#"{"connection":"../x","bucket":"farm-bucket"}"#,
            400,
            "bad_body",
        ),
        (COLD, r#"{"connection":"cold"}"#, 400, "bad_body"),
        (
            COLD,
            r#"{"connection":"cold","bucket":"farm-bucket","extra":1}"#,
            400,
            "bad_body",
        ),
        (
            "/v1/object-stores/bindings/history",
            BIND_BODY,
            422,
            "role_not_supported",
        ),
        (
            "/v1/object-stores/bindings/backup",
            BIND_BODY,
            422,
            "role_not_supported",
        ),
        (
            "/v1/object-stores/bindings/nonsense",
            BIND_BODY,
            422,
            "role_not_supported",
        ),
    ];
    for (path, body, status, code) in cases {
        let reply = farm.call("PUT", path, body).await;
        assert_eq!(
            (reply.status, reply.code().as_str()),
            (status, code),
            "{path} {body}: {}",
            reply.body
        );
    }
    assert_eq!(farm.generation(), 1);
    for role in ["history", "backup", "nonsense"] {
        let gone = farm
            .call("DELETE", &format!("/v1/object-stores/bindings/{role}"), "")
            .await;
        assert_eq!(
            (gone.status, gone.code().as_str()),
            (422, "role_not_supported"),
            "{role}"
        );
    }
}

// Catches an identity that moves after the first bind (bucket, prefix or endpoint: every node
// would be pointed at a different store), a credential-only change that is refused, and an
// `expect_generation` that is ignored.
#[tokio::test]
async fn the_store_identity_is_fixed_once_bound_but_credentials_rotate() {
    let farm = with_connection().await;
    farm.call("PUT", COLD, BIND_BODY).await;
    for moved in [
        r#"{"connection":"cold","bucket":"other-bucket","prefix":"cold/"}"#,
        r#"{"connection":"cold","bucket":"farm-bucket","prefix":"other/"}"#,
    ] {
        let reply = farm.call("PUT", COLD, moved).await;
        assert_eq!(
            (reply.status, reply.code().as_str()),
            (409, "identity_change"),
            "{moved}"
        );
    }
    let endpoint = conn_body(SENTINEL).replace("s3.example", "elsewhere.example");
    let reply = farm
        .call("PUT", "/v1/object-stores/connections/cold", &endpoint)
        .await;
    assert_eq!(
        (reply.status, reply.code().as_str()),
        (409, "identity_change")
    );
    assert_eq!(farm.generation(), 2);

    let rotated = farm
        .call(
            "PUT",
            "/v1/object-stores/connections/cold",
            &conn_body("rotated-secret"),
        )
        .await;
    assert_eq!(rotated.status, 200, "{}", rotated.body);

    let stale = BIND_BODY.replace("}", r#","expect_generation":1}"#);
    let mismatch = farm.call("PUT", COLD, &stale).await;
    assert_eq!(
        (mismatch.status, mismatch.code().as_str()),
        (409, "generation_mismatch")
    );
}

// Catches an unbind that ignores the nodes (one that runs on the binding would be cut from
// its configuration), one that counts a node that has not applied it, and the plain cases:
// no binding is 404, a free binding is deleted with a new generation. A node that reports a
// failure is not "using" it either; a node at the binding's generation or later is.
#[tokio::test]
async fn an_unbind_is_refused_while_a_node_has_applied_the_binding() {
    use MemberStanding::{Active, Draining};
    let farm = with_connection().await;
    let none = farm.call("DELETE", COLD, "").await;
    assert_eq!(
        (none.status, none.code().as_str()),
        (404, "no_such_binding")
    );
    farm.call("PUT", COLD, BIND_BODY).await; // generation 2

    farm.acks.set(vec![
        node("behind", 1, NodeStatus::Ok, Active),
        node("failed", 2, NodeStatus::ProbeFailed, Active),
        node("fresh", 0, NodeStatus::Pending, Active),
    ]);
    let free = farm.call("DELETE", COLD, "").await;
    assert_eq!(free.status, 200, "{}", free.body);
    assert_eq!(free.json::<Deleted>(), Deleted { generation: 3 });

    farm.call("PUT", COLD, BIND_BODY).await; // generation 4
    farm.acks
        .set(vec![node("running", 4, NodeStatus::Ok, Draining)]);
    let used = farm.call("DELETE", COLD, "").await;
    assert_eq!(
        (used.status, used.code().as_str()),
        (409, "in_use"),
        "{}",
        used.body
    );
    assert_eq!(farm.generation(), 4);
    farm.acks
        .set(vec![node("ahead", 9, NodeStatus::Ok, Active)]);
    assert_eq!(farm.call("DELETE", COLD, "").await.status, 409);
    farm.acks.set(vec![]);
    assert_eq!(farm.call("DELETE", COLD, "").await.status, 200);
}
