//! `GET /v1/object-stores` and the audit log.

use kbf_types::object_store_api::{
    AuditAction, AuditList, AuditOutcome, MemberStanding, NodeConvergence, NodeStatus, Status,
};

use super::harness::{BIND_BODY, Farm, SENTINEL, conn_body, registered};

const STATUS: &str = "/v1/object-stores";
const AUDIT: &str = "/v1/object-stores/audit";

fn node(
    id: &str,
    state: MemberStanding,
    applied: Option<u64>,
    status: NodeStatus,
) -> NodeConvergence {
    NodeConvergence {
        id: id.to_owned(),
        state,
        applied_generation: applied,
        status,
        last_seen_secs: applied.map(|_| 3),
    }
}

// Catches a status that leaves out the desired state or the generation, one that shows a secret,
// and a fresh registry that claims convergence: with no node known (a frontend that just
// started, or no storage node at all) `converged` is false, never a vacuous true.
#[tokio::test]
async fn the_status_shows_the_desired_state_and_never_a_convergence_it_does_not_know() {
    let farm = Farm::new();
    let empty = farm.call("GET", STATUS, "").await;
    assert_eq!(empty.status, 200, "{}", empty.body);
    assert_eq!(empty.header("cache-control"), Some("no-store"));
    let empty: Status = empty.json();
    assert_eq!(empty.generation, 0);
    assert!(empty.connections.is_empty() && empty.bindings.is_empty());
    assert!(!empty.convergence.converged);
    assert_eq!(empty.convergence.expected, 0);

    registered(&farm).await;
    let status = farm.call("GET", STATUS, "").await;
    assert!(!status.body.contains(SENTINEL));
    let status: Status = status.json();
    assert_eq!(status.generation, 2);
    assert_eq!(status.connections[0].name.as_str(), "cold");
    assert_eq!(status.bindings[0].bucket, "farm-bucket");
    assert_eq!(status.bindings[0].prefix, "cold/");
    assert_eq!(status.bindings[0].updated_generation, 2);
    assert!(!status.convergence.converged, "no node has reported");
}

// Catches the convergence of the nodes read wrong: a node that reports ok at another
// generation shown as ok, draining and denied nodes counted as expected, an old build (no
// report) hidden in `pending`, and the nodes not named: the answer is the registry's generation
// against what each node says, with every node listed by id.
#[tokio::test]
async fn the_convergence_follows_the_nodes_and_the_generation() {
    use MemberStanding::{Active, Denied, Draining};
    let farm = Farm::new();
    registered(&farm).await;
    farm.acks.set(vec![
        node("b", Active, Some(2), NodeStatus::Ok),
        node("a", Active, Some(2), NodeStatus::Ok),
        node("leaving", Draining, Some(1), NodeStatus::Ok),
        node("revoked", Denied, None, NodeStatus::Pending),
    ]);
    let done: Status = farm.call("GET", STATUS, "").await.json();
    assert!(done.convergence.converged, "{:?}", done.convergence);
    assert_eq!(
        (done.convergence.expected, done.convergence.applied),
        (2, 2)
    );
    let ids: Vec<_> = done
        .convergence
        .nodes
        .iter()
        .map(|n| n.id.as_str())
        .collect();
    assert_eq!(ids, ["a", "b", "leaving", "revoked"]);

    // A new generation: everyone is behind until they report again.
    farm.call(
        "PUT",
        "/v1/object-stores/connections/cold",
        &conn_body("rotated"),
    )
    .await;
    let behind: Status = farm.call("GET", STATUS, "").await.json();
    assert_eq!(behind.generation, 3);
    assert!(!behind.convergence.converged);
    assert_eq!(behind.convergence.applied, 0);
    assert!(
        behind
            .convergence
            .nodes
            .iter()
            .filter(|n| n.id == "a" || n.id == "b")
            .all(|n| n.status == NodeStatus::Pending)
    );

    // An old build is named, counted apart, and blocks.
    farm.acks.set(vec![
        node("a", Active, Some(3), NodeStatus::Ok),
        node("old", Active, None, NodeStatus::Unsupported),
    ]);
    let skew: Status = farm.call("GET", STATUS, "").await.json();
    assert!(!skew.convergence.converged);
    assert_eq!(
        (
            skew.convergence.expected,
            skew.convergence.applied,
            skew.convergence.unsupported
        ),
        (1, 1, 1)
    );
}

// Catches an audit that is oldest first, ignores `limit`, loses an outcome (an unchanged retry
// and a failed-probe refusal are different things: only writes leave rows), takes a limit
// that is zero, negative or not a number, takes a query parameter it does not know, or
// walks off the end with a huge limit.
#[tokio::test]
async fn the_audit_lists_the_newest_first_and_checks_its_limit() {
    let farm = Farm::new();
    registered(&farm).await;
    farm.call(
        "PUT",
        "/v1/object-stores/connections/cold",
        &conn_body(SENTINEL),
    )
    .await; // unchanged
    farm.call("PUT", "/v1/object-stores/bindings/cold", BIND_BODY)
        .await; // unchanged
    farm.probe.fail_at_get();
    farm.call(
        "PUT",
        "/v1/object-stores/connections/cold",
        &conn_body("failing"),
    )
    .await; // refused

    let all = farm.call("GET", AUDIT, "").await;
    assert_eq!(all.status, 200, "{}", all.body);
    assert!(!all.body.contains(SENTINEL));
    let all: AuditList = all.json();
    let rows: Vec<_> = all
        .entries
        .iter()
        .map(|e| (e.seq, e.action, e.subject.as_str(), e.outcome, e.generation))
        .collect();
    assert_eq!(
        rows,
        [
            (
                4,
                AuditAction::PutBinding,
                "cold",
                AuditOutcome::Unchanged,
                2
            ),
            (
                3,
                AuditAction::PutConnection,
                "cold",
                AuditOutcome::Unchanged,
                2
            ),
            (2, AuditAction::PutBinding, "cold", AuditOutcome::Created, 2),
            (
                1,
                AuditAction::PutConnection,
                "cold",
                AuditOutcome::Created,
                1
            ),
        ]
    );
    let two: AuditList = farm
        .call("GET", &format!("{AUDIT}?limit=2"), "")
        .await
        .json();
    assert_eq!(
        two.entries.iter().map(|e| e.seq).collect::<Vec<_>>(),
        [4, 3]
    );
    let huge: AuditList = farm
        .call("GET", &format!("{AUDIT}?limit=1000000"), "")
        .await
        .json();
    assert_eq!(
        huge.entries.len(),
        4,
        "a limit above the cap is capped, not refused"
    );
    for bad in [
        "limit=0",
        "limit=-1",
        "limit=many",
        "limit=",
        "limit=1&limit=2",
        "limit=1&other=2",
        "other=1",
    ] {
        let reply = farm.call("GET", &format!("{AUDIT}?{bad}"), "").await;
        assert_eq!(
            (reply.status, reply.code().as_str()),
            (400, "bad_query"),
            "{bad}"
        );
    }
}
