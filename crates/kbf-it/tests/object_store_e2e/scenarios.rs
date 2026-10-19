//! The failure scenarios. Each test names the defect it catches and the mutant that was
//! planted to see it red (the list, with the results, is in the pull request).

use std::io::Write as _;
use std::net::TcpStream;
use std::time::Duration;

use kbf_types::object_store_api::{
    AuditAction, ConnectionPutAnswer, NodeConvergence, NodeStatus, Status,
};

use crate::farm::{Farm, Frontend, NEW, OLD, TOKEN, listing};
use crate::node::{Node, Seat, no_keys, own_keys};
use crate::operator::{Operator, binding_body, connection_body};

fn node<'a>(status: &'a Status, seat: &Seat) -> &'a NodeConvergence {
    status
        .convergence
        .nodes
        .iter()
        .find(|n| n.id == seat.addr)
        .unwrap_or_else(|| panic!("{} is not in {status:?}", seat.name))
}

/// Every node in the status has reported at least once since this frontend started.
fn all_heard(status: &Status, count: usize) -> bool {
    status.convergence.nodes.len() == count
        && status
            .convergence
            .nodes
            .iter()
            .all(|n| n.last_seen_secs.is_some())
}

/// Catches a status that calls the farm converged while a node is down or has not applied
/// the generation. Mutants (each seen red): `Convergence::of` counting every active node as
/// applied; and all three places that judge an `ok` against the generation removed at once
/// (the node's `for_beat`, `Acks::convergence`, `Convergence::of`: any one alone is covered
/// by the other two, so this test cannot tell which was removed).
///
/// The node that is down is named, `pending`, until it comes back; the one restarted first
/// runs on the registered keys alone (it has none of its own: the S3 service refuses any
/// other pair, and a node on a stale store could not write); and the farm converges only
/// when the last node has applied.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_that_is_down_during_the_put_is_pending_until_it_comes_back() {
    let farm = Farm::new("down-during-put", OLD).await;
    let frontend = Frontend::start(&farm).await;
    let op = Operator(frontend.api);
    let (seat_a, seat_b) = (Seat::new(&farm, "a"), Seat::new(&farm, "b"));
    let a = Node::start(&farm, &frontend, &seat_a, own_keys).await;
    let b = Node::start(&farm, &frontend, &seat_b, own_keys).await;
    op.status_when("both nodes to have reported", |s| all_heard(s, 2))
        .await;

    b.stop().await;
    op.register(farm.s3.endpoint(), OLD).await;
    let written = op.status().await;
    assert_eq!(written.generation, 2);
    assert!(!written.convergence.converged, "{written:?}");
    assert_eq!(
        (written.convergence.expected, written.convergence.applied),
        (2, 0)
    );
    for seat in [&seat_a, &seat_b] {
        assert_eq!(node(&written, seat).status, NodeStatus::Pending);
    }

    // The running node does not take the registration until it restarts (no hot swap).
    a.stop().await;
    let a = Node::start(&farm, &frontend, &seat_a, no_keys).await;
    a.round_trip("a, on the registered keys").await;
    let half = op
        .status_when("a to apply generation 2", |s| {
            s.convergence.nodes.iter().any(|n| {
                n.id == seat_a.addr && n.status == NodeStatus::Ok && n.applied_generation == Some(2)
            })
        })
        .await;
    assert!(!half.convergence.converged, "{half:?}");
    assert_eq!(
        (half.convergence.expected, half.convergence.applied),
        (2, 1)
    );
    assert_eq!(node(&half, &seat_b).status, NodeStatus::Pending);
    assert!(
        node(&half, &seat_b).last_seen_secs.is_some(),
        "b is named with when it was last heard"
    );

    let b = Node::start(&farm, &frontend, &seat_b, no_keys).await;
    b.round_trip("b, back").await;
    let done = op
        .status_when("the farm to converge", |s| s.convergence.converged)
        .await;
    assert_eq!(done.generation, 2);
    assert_eq!(
        (done.convergence.expected, done.convergence.applied),
        (2, 2)
    );
    drop((a, b));
}

/// Catches a frontend restart in the middle of a rotation that costs a node its working
/// credential, or that lets the status claim a convergence nobody reported. Mutants (each
/// seen red): `Convergence::of` counting every active node as applied, and the three
/// generation judgements removed at once (the restarted frontend reads the acks the old one
/// left in the shared store and would call the farm converged on the rotated generation);
/// and `Registry::put_connection` removing the new secret version after the commit instead
/// of the old one (the restarted frontend has no secret to hand a node).
///
/// The nodes run on while the frontend is gone and write through their store; the new
/// frontend holds the rotated registration and the one secret file, shows both nodes
/// `pending` for it, and converges only as the nodes restart onto the new keys.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_restart_mid_rotation_loses_no_credential_and_claims_nothing() {
    let farm = Farm::new("restart-mid-rotation", OLD).await;
    let first = Frontend::start(&farm).await;
    let op = Operator(first.api);
    op.register(farm.s3.endpoint(), OLD).await;
    let (seat_a, seat_b) = (Seat::new(&farm, "a"), Seat::new(&farm, "b"));
    let a = Node::start(&farm, &first, &seat_a, no_keys).await;
    let b = Node::start(&farm, &first, &seat_b, no_keys).await;
    op.status_when("the farm to converge on the old keys", |s| {
        s.convergence.converged
    })
    .await;

    // The rotation: the operator pushes the new keys before the provider enables them.
    let put = op
        .put_connection(connection_body(farm.s3.endpoint(), NEW, false))
        .await;
    assert_eq!(put.status, 200, "{}", put.body);
    let answer: ConnectionPutAnswer = serde_json::from_str(&put.body).expect("an answer");
    assert!(answer.changed);
    assert_eq!(answer.generation, 3);
    assert_eq!(listing(&farm.secrets()).len(), 1, "one secret file is live");

    first.kill().await;
    a.round_trip("a, with the frontend gone").await;
    b.round_trip("b, with the frontend gone").await;

    let second = Frontend::start(&farm).await;
    let op = Operator(second.api);
    assert_eq!(
        second
            .registry
            .secret(&"main".parse().expect("a name"))
            .expect("the live secret")
            .expose(),
        NEW.secret
    );
    assert_eq!(listing(&farm.secrets()).len(), 1);
    let behind = op.status().await;
    assert_eq!(behind.generation, 3);
    assert!(!behind.convergence.converged, "{behind:?}");
    assert_eq!(behind.convergence.applied, 0);
    for seat in [&seat_a, &seat_b] {
        assert_eq!(node(&behind, seat).status, NodeStatus::Pending);
    }

    // The provider enables the new keys; the operator restarts the nodes one at a time.
    farm.provider_accepts(NEW);
    a.stop().await;
    let a = Node::start(&farm, &second, &seat_a, no_keys).await;
    a.round_trip("a, on the rotated keys").await;
    let half = op
        .status_when("a to apply generation 3", |s| {
            s.convergence.nodes.iter().any(|n| {
                n.id == seat_a.addr && n.status == NodeStatus::Ok && n.applied_generation == Some(3)
            })
        })
        .await;
    assert!(!half.convergence.converged, "{half:?}");
    assert_eq!(node(&half, &seat_b).status, NodeStatus::Pending);

    b.stop().await;
    let b = Node::start(&farm, &second, &seat_b, no_keys).await;
    b.round_trip("b, on the rotated keys").await;
    let done = op
        .status_when("the farm to converge on the new keys", |s| {
            s.convergence.converged
        })
        .await;
    assert_eq!(done.generation, 3);
    drop((a, b));
}

/// Catches a node whose registered keys fail its probe being reported applied, or being
/// left without a working store. Mutants (each seen red): the probe-failed branch of
/// `ColdPick::registered` in `cold_apply.rs` returning `Applied::running` (the status says
/// `ok` for a registration the node did not take), `Convergence::of` counting every
/// active node as applied, and `Registry::put_connection` removing the new secret version
/// (the node's fetch finds no secret).
///
/// The operator pushes keys the provider has not enabled. The restarted node says
/// `probe_failed`, names itself, applies nothing and keeps serving on the keys it had; the
/// node not yet restarted is `pending`; the farm is not converged until the provider
/// enables the keys and both nodes have restarted onto them. No answer holds a secret.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_node_given_a_bad_new_credential_keeps_its_store_and_is_named() {
    let farm = Farm::new("bad-credential", OLD).await;
    let frontend = Frontend::start(&farm).await;
    let op = Operator(frontend.api);
    op.register(farm.s3.endpoint(), OLD).await;
    let (seat_a, seat_b) = (Seat::new(&farm, "a"), Seat::new(&farm, "b"));
    let a = Node::start(&farm, &frontend, &seat_a, own_keys).await;
    let b = Node::start(&farm, &frontend, &seat_b, own_keys).await;
    op.status_when("the farm to converge", |s| s.convergence.converged)
        .await;

    let put = op
        .put_connection(connection_body(farm.s3.endpoint(), NEW, false))
        .await;
    assert_eq!(put.status, 200, "{}", put.body);

    a.stop().await;
    let a = Node::start(&farm, &frontend, &seat_a, own_keys).await;
    a.round_trip("a, on the keys it had").await;
    b.round_trip("b, on the keys it had").await;
    let failed = op
        .status_when("a to say probe_failed", |s| {
            s.convergence
                .nodes
                .iter()
                .any(|n| n.id == seat_a.addr && n.status == NodeStatus::ProbeFailed)
        })
        .await;
    assert!(!failed.convergence.converged, "{failed:?}");
    assert_eq!(node(&failed, &seat_a).applied_generation, Some(0));
    assert_eq!(node(&failed, &seat_b).status, NodeStatus::Pending);
    let text = format!("{failed:?}{:?}", op.audit().await);
    for secret in [OLD.secret, NEW.secret] {
        assert!(!text.contains(secret), "a secret is in an answer");
    }

    farm.provider_accepts(NEW);
    a.stop().await;
    b.stop().await;
    let a = Node::start(&farm, &frontend, &seat_a, no_keys).await;
    let b = Node::start(&farm, &frontend, &seat_b, no_keys).await;
    a.round_trip("a, on the new keys").await;
    b.round_trip("b, on the new keys").await;
    let done = op
        .status_when("the farm to converge on the new keys", |s| {
            s.convergence.converged
        })
        .await;
    assert_eq!(done.generation, 3);
    drop((a, b));
}

/// Writes `body` to `path` as a request and keeps the connection until `committed`
/// holds, then drops it without reading the answer: the operator's connection broke after
/// the frontend had the request.
async fn put_and_lose_the_answer(
    api: std::net::SocketAddr,
    path: &'static str,
    body: String,
    committed: impl Fn() -> bool,
) {
    let mut stream = tokio::task::spawn_blocking(move || {
        let mut stream = TcpStream::connect(api).expect("connect");
        let request = format!(
            "PUT {path} HTTP/1.1\r\nHost: {api}\r\nAuthorization: Bearer {TOKEN}\r\n\
             Content-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(request.as_bytes()).expect("send");
        stream
    })
    .await
    .expect("the sending side");
    for _ in 0..2000 {
        if committed() {
            let _ = std::io::Write::flush(&mut stream);
            drop(stream);
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("the frontend never committed the request");
}

/// Catches a retry of a PUT whose answer was lost making a second change. Mutant (seen
/// red): the unchanged path of `Registry::put_connection` and `put_binding` committing a
/// generation bump, so the retry moves the generation and every node is told of a
/// registration that is the same as the one it holds.
///
/// Both PUTs commit and their answers are lost; the same two bodies sent again answer
/// `changed:false` at the generation the first sent left, the audit says `unchanged`, and a
/// node started afterwards applies that generation, not a later one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_put_whose_answer_was_lost_is_retried_without_a_second_generation() {
    let farm = Farm::new("lost-answer", OLD).await;
    let frontend = Frontend::start(&farm).await;
    let op = Operator(frontend.api);
    let connection = connection_body(farm.s3.endpoint(), OLD, true);

    let registry = std::sync::Arc::clone(&frontend.registry);
    put_and_lose_the_answer(
        frontend.api,
        "/v1/object-stores/connections/main",
        connection.clone(),
        move || registry.generation().is_ok_and(|g| g == 1),
    )
    .await;
    let registry = std::sync::Arc::clone(&frontend.registry);
    put_and_lose_the_answer(
        frontend.api,
        "/v1/object-stores/bindings/cold",
        binding_body(),
        move || registry.generation().is_ok_and(|g| g == 2),
    )
    .await;

    let again = op.put_connection(connection).await;
    assert_eq!(again.status, 200, "{}", again.body);
    let again: ConnectionPutAnswer = serde_json::from_str(&again.body).expect("an answer");
    assert!(!again.changed);
    assert_eq!(again.generation, 2);
    let rebound = op.put_binding(binding_body()).await;
    assert_eq!(rebound.status, 200, "{}", rebound.body);
    assert!(
        rebound.body.contains(r#""changed":false"#),
        "{}",
        rebound.body
    );
    assert_eq!(op.status().await.generation, 2);
    let kinds: Vec<AuditAction> = op.audit().await.entries.iter().map(|e| e.action).collect();
    assert_eq!(kinds.len(), 4, "{kinds:?}");

    let seat = Seat::new(&farm, "a");
    let a = Node::start(&farm, &frontend, &seat, no_keys).await;
    a.round_trip("a, on the registration").await;
    let done = op
        .status_when("a to apply", |s| s.convergence.converged)
        .await;
    assert_eq!(node(&done, &seat).applied_generation, Some(2));
    drop(a);
}
