//! The cold-store registration on the worker listener, over mutual TLS and behind the
//! role table: who may fetch it, what a heartbeat's answer announces and what its request
//! reports, and how the frontend's acks turn into convergence. Each test names the defect
//! it catches.

#[path = "support/hooked.rs"]
mod hooked;
#[path = "support/store_config.rs"]
mod support;

use std::sync::Arc;

use kbf_map::{DenyEntry, MemberId};
use kbf_proto::kbf::map::v1 as pb;
use kbf_server::store_config::acks::NodeStatus;
use support::{
    SENTINEL, Source, cold, der, enroll, farm, farm_denying, membership, registration, want,
};
use tonic::{Code, Status};

/// The unary the farm's registered store replaced: a raw call, since no client exists.
async fn raw(channel: &tonic::transport::Channel, path: &'static str) -> Status {
    let mut grpc = tonic::client::Grpc::new(channel.clone());
    grpc.ready().await.expect("ready");
    let call = grpc.unary(
        tonic::Request::new(()),
        tonic::codegen::http::uri::PathAndQuery::from_static(path),
        tonic_prost::ProstCodec::<(), ()>::default(),
    );
    call.await.expect_err("refused")
}

/// Catches: the storage-only row missing or widened. A storage certificate gets the store
/// and its keys in one answer; a daemon, a frontend, a certificate with no role and one
/// with a role that is no role are refused by the table, before the service; a method not in
/// the table (the old `GetKeys`) is refused for the storage certificate too.
///
/// Mutants seen red: the row `&[Daemon, Storage]` (the daemon is served); the row deleted
/// (the storage node is refused).
#[tokio::test]
async fn only_a_storage_certificate_is_served_the_store() {
    let farm = farm(Source::new(7), false).await;
    let storage = farm.pki.client(Some("storage"), &["node-a"]);
    let channel = farm.pki.channel(farm.addr, storage).await;
    let got = cold(&channel)
        .get_store_config(want(0))
        .await
        .expect("served")
        .into_inner();
    assert_eq!(got.generation, 7);
    let store = got.config.as_ref().expect("the store");
    assert_eq!(
        store.secret_access_key, SENTINEL,
        "the secret is in the answer"
    );
    assert_eq!(store.secret_version, 7);

    for (role, names) in [
        (Some("daemon"), "node-a"),
        (Some("frontend"), "node-a"),
        (None, "node-a"),
        (Some("admin"), "node-a"),
    ] {
        let identity = farm.pki.client(role, &[names]);
        let channel = farm.pki.channel(farm.addr, identity).await;
        let refused = cold(&channel)
            .get_store_config(want(0))
            .await
            .expect_err("refused");
        assert_eq!(refused.code(), Code::PermissionDenied, "{role:?}");
        assert!(
            refused.message().contains("role"),
            "refused by the table, which names the role: {}",
            refused.message()
        );
        assert!(!refused.message().contains(SENTINEL));
    }
    let refused = raw(&channel, "/kbf.enroll.v1.ColdStore/GetKeys").await;
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert!(
        refused.message().contains("not served"),
        "{}",
        refused.message()
    );
}

/// Catches: a node served once and then for ever. A node denied (a `deny/` row, as
/// revocation writes) between two calls on the same connection is refused at the second,
/// with no registry in play; another node on its own connection is not. A check made only
/// when the connection opens would serve both.
///
/// Mutant seen red: `admit_leaf` skipping the `deny/` rows.
#[tokio::test]
async fn a_node_denied_on_a_live_connection_is_refused_at_its_next_call() {
    let farm = farm(Source::new(3), false).await;
    let a = farm
        .pki
        .channel(farm.addr, farm.pki.client(Some("storage"), &["node-a"]))
        .await;
    let b = farm
        .pki
        .channel(farm.addr, farm.pki.client(Some("storage"), &["node-b"]))
        .await;
    cold(&a)
        .get_store_config(want(0))
        .await
        .expect("first call");
    farm.members
        .deny(&DenyEntry::Node("node-a".to_owned()), "revoked")
        .expect("deny");
    let refused = cold(&a)
        .get_store_config(want(0))
        .await
        .expect_err("denied");
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert!(!refused.message().contains(SENTINEL));
    cold(&b)
        .get_store_config(want(0))
        .await
        .expect("node-b is served");
}

/// Catches: an unchanged answer that carries the secret, and a node told "unchanged" when
/// its number is merely different: the same generation is two numbers, a lower or higher
/// one gets the whole store (a restored frontend), and nothing registered is generation 0
/// with no store.
///
/// Mutant seen red: the service answering the config for an equal generation.
#[tokio::test]
async fn an_unchanged_generation_is_answered_without_the_secret() {
    let source = Source::new(7);
    let farm = farm(source.clone(), false).await;
    let channel = farm
        .pki
        .channel(farm.addr, farm.pki.client(Some("storage"), &["node-a"]))
        .await;
    let same = cold(&channel)
        .get_store_config(want(7))
        .await
        .expect("same")
        .into_inner();
    assert_eq!((same.generation, same.config.is_none()), (7, true));
    for have in [0, 6, 8] {
        let full = cold(&channel)
            .get_store_config(want(have))
            .await
            .expect("full")
            .into_inner();
        assert_eq!(
            full.config.expect("a store").secret_access_key,
            SENTINEL,
            "have {have}"
        );
    }
    // The frontend's generation goes back (a restored state store): a node at 7 refetches.
    source.set(2);
    let back = cold(&channel)
        .get_store_config(want(7))
        .await
        .expect("back")
        .into_inner();
    assert_eq!(back.generation, 2);
    assert!(back.config.is_some(), "a lower generation is not ignored");
    source.set(0);
    let none = cold(&channel)
        .get_store_config(want(0))
        .await
        .expect("none")
        .into_inner();
    assert_eq!((none.generation, none.config.is_none()), (0, true));
}

/// Catches: a secret that cannot be read answered as success or as a plain failure the node
/// would not retry: the status is UNAVAILABLE with fixed text, and says nothing of the secret.
#[tokio::test]
async fn a_secret_that_is_not_readable_is_unavailable() {
    let source = Source::new(7);
    source.break_secret();
    let farm = farm(source, false).await;
    let channel = farm
        .pki
        .channel(farm.addr, farm.pki.client(Some("storage"), &["node-a"]))
        .await;
    let refused = cold(&channel)
        .get_store_config(want(0))
        .await
        .expect_err("unavailable");
    assert_eq!(refused.code(), Code::Unavailable);
    assert_eq!(
        refused.message(),
        "the registered store's secret is not available"
    );
}

/// Catches: the registration generation missing from either answer, a heartbeat's report
/// dropped, a registration that keeps the acks of the boot before it, and a stale boot's
/// heartbeat recorded.
///
/// Mutants seen red: both answers announcing 0; `forget` not called at the register.
#[tokio::test]
async fn heartbeats_announce_the_generation_and_report_what_the_node_applied() {
    let source = Source::new(4);
    let farm = farm(source.clone(), false).await;
    let channel = farm
        .pki
        .channel(farm.addr, farm.pki.client(Some("storage"), &["node-a"]))
        .await;
    let mut node = membership(&channel);
    let joined = node
        .register(registration("node-a"))
        .await
        .expect("registers")
        .into_inner();
    assert_eq!(joined.objstore_generation, 4);
    let map = kbf_map::Map::try_from(joined.map.expect("a map")).expect("a map");

    let pending = farm.acks.convergence(&map, 4).expect("read");
    assert!(!pending.converged, "nothing reported yet");

    let beat = |generation: u64, status: pb::ApplyStatus| pb::HeartbeatRequest {
        member_id: joined.member_id,
        boot: joined.boot,
        have_generation: map.generation(),
        applied_objstore_generation: generation,
        apply_status: status as i32,
        ..Default::default()
    };
    source.set(5);
    let answer = node
        .heartbeat(beat(4, pb::ApplyStatus::Ok))
        .await
        .expect("beats")
        .into_inner();
    assert_eq!(answer.objstore_generation, 5, "the frontend moved on");
    let behind = farm.acks.convergence(&map, 5).expect("read");
    assert!(!behind.converged, "the node holds 4, the farm is at 5");
    assert_eq!(behind.nodes[0].status, NodeStatus::Pending);
    assert_eq!(behind.nodes[0].applied_generation, Some(4));

    node.heartbeat(beat(5, pb::ApplyStatus::Ok))
        .await
        .expect("beats");
    assert!(farm.acks.convergence(&map, 5).expect("read").converged);

    // Registering again is a new boot that has applied nothing.
    let again = node
        .register(registration("node-a"))
        .await
        .expect("again")
        .into_inner();
    assert!(again.boot > joined.boot);
    assert!(!farm.acks.convergence(&map, 5).expect("read").converged);

    // A heartbeat of the old boot is refused, and records nothing.
    let stale = node
        .heartbeat(beat(5, pb::ApplyStatus::Ok))
        .await
        .expect_err("old boot");
    assert_eq!(stale.code(), Code::FailedPrecondition);
    assert!(!farm.acks.convergence(&map, 5).expect("read").converged);

    // The acks refuse an older boot themselves (called directly: the stale heartbeat above is
    // refused before it gets here): a report of boot 1 never replaces boot 2's.
    let member = MemberId(joined.member_id);
    farm.acks
        .record(member, again.boot, &beat(5, pb::ApplyStatus::Ok))
        .expect("the newer boot's report");
    farm.acks
        .record(member, joined.boot, &beat(1, pb::ApplyStatus::ProbeFailed))
        .expect("the older boot's report");
    assert!(farm.acks.convergence(&map, 5).expect("read").converged);
}

/// Catches: an older node (one that sends neither field) counted as pending for ever,
/// or as applied: it is `unsupported`, named, and the farm is not converged while it is
/// active.
#[tokio::test]
async fn a_node_without_the_fields_is_unsupported() {
    let farm = farm(Source::new(4), false).await;
    let channel = farm
        .pki
        .channel(farm.addr, farm.pki.client(Some("storage"), &["node-a"]))
        .await;
    let mut node = membership(&channel);
    let joined = node
        .register(registration("node-a"))
        .await
        .expect("registers")
        .into_inner();
    let map = kbf_map::Map::try_from(joined.map.expect("a map")).expect("a map");
    node.heartbeat(pb::HeartbeatRequest {
        member_id: joined.member_id,
        boot: joined.boot,
        ..Default::default()
    })
    .await
    .expect("an old node's heartbeat");
    let got = farm.acks.convergence(&map, 4).expect("read");
    assert_eq!(got.unsupported, vec![MemberId(joined.member_id)]);
    assert_eq!(got.expected, 0);
    assert!(!got.converged);
}

/// Catches: a registry check that fails open. With a registry, a certificate whose node the
/// registry has never seen is PERMISSION_DENIED even though its role and names are right;
/// a registry that cannot be read is UNAVAILABLE (retry), and one that panics in the
/// store is INTERNAL with fixed text, never the panic's.
///
/// Mutant seen red: the registry result ignored (`let _ =`).
#[tokio::test]
async fn with_a_registry_an_unenrolled_node_is_refused_and_a_failing_registry_is_not_open() {
    use std::sync::atomic::Ordering;
    let farm = farm(Source::new(7), true).await;
    let channel = farm
        .pki
        .channel(farm.addr, farm.pki.client(Some("storage"), &["node-a"]))
        .await;
    let refused = cold(&channel)
        .get_store_config(want(0))
        .await
        .expect_err("unenrolled");
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert_eq!(refused.message(), "the node is not enrolled or is revoked");

    // Enrolled, the same certificate is served.
    farm.store
        .clock
        .advance(std::time::Duration::from_secs(1_000_000));
    enroll(&*farm.store, "node-a");
    cold(&channel)
        .get_store_config(want(0))
        .await
        .expect("an active node is served");

    // A certificate that names no node, or a name that is no node id, is not enrolled.
    let direct = kbf_server::store_config::service::ColdStoreService::new(
        Source::new(7),
        kbf_storage::service::Admission::Certified,
    )
    .with_registry(Some(Arc::clone(&farm.store) as Arc<dyn kbf_state::Store>));
    let cases: [&[&str]; 2] = [&[], &["Not A Node!"]];
    for names in cases {
        let stranger = der(Some("storage"), names);
        let refused = direct
            .admit_leaf(Some(&stranger))
            .await
            .expect_err("no node");
        assert_eq!(refused.code(), Code::PermissionDenied, "{names:?}");
        assert_eq!(
            refused.message(),
            "the certificate names no enrolled node",
            "{names:?}"
        );
    }

    farm.store.fail_gets.store(1, Ordering::SeqCst);
    let down = cold(&channel)
        .get_store_config(want(0))
        .await
        .expect_err("unreadable");
    assert_eq!(down.code(), Code::Unavailable);
    assert_eq!(down.message(), "the enrollment registry cannot be read");

    farm.store.panic_gets.store(1, Ordering::SeqCst);
    let panicked = cold(&channel)
        .get_store_config(want(0))
        .await
        .expect_err("panic");
    assert_eq!(panicked.code(), Code::Internal);
    assert_eq!(panicked.message(), "the store config call failed");
}

/// Catches: a heartbeat or a registration that fails because the frontend cannot say its
/// registration generation (the state store failed): the node must keep its membership. The
/// answer carries 0, "none or cannot say", and the map as ever.
///
/// Mutant seen red: the announced number 7, not 0, when the source cannot say.
#[tokio::test]
async fn a_frontend_that_cannot_say_its_generation_announces_zero_and_still_answers() {
    let source = Source::new(4);
    let farm = farm(source.clone(), false).await;
    let channel = farm
        .pki
        .channel(farm.addr, farm.pki.client(Some("storage"), &["node-a"]))
        .await;
    let mut node = membership(&channel);
    source.go_mute();
    let joined = node
        .register(registration("node-a"))
        .await
        .expect("registers")
        .into_inner();
    assert_eq!(joined.objstore_generation, 0);
    assert!(joined.map.is_some());
    let beat = node
        .heartbeat(pb::HeartbeatRequest {
            member_id: joined.member_id,
            boot: joined.boot,
            ..Default::default()
        })
        .await
        .expect("beats")
        .into_inner();
    assert_eq!(beat.objstore_generation, 0);
    assert!(beat.map_generation > 0);
}

/// Catches: the worker listener's deny list file skipped by the store call. A node the file
/// names is served until the file is replaced and refused at its next call after, on the
/// connection it already has; a node it does not name is served throughout.
///
/// Mutant seen red: `admit_leaf` not consulting the deny list.
#[tokio::test]
async fn the_worker_deny_list_file_applies_at_every_call() {
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("store-config");
    std::fs::create_dir_all(&dir).expect("directory");
    let path = dir.join("deny.list");
    let replace = |text: &str| {
        let staged = path.with_extension("new");
        std::fs::write(&staged, text).expect("write");
        std::fs::rename(&staged, &path).expect("replace");
    };
    replace("# nobody yet\n");
    let list = kbf_server::DenyList::open(&path).expect("the list");
    let farm = farm_denying(Source::new(7), false, Some(list)).await;
    let a = farm
        .pki
        .channel(farm.addr, farm.pki.client(Some("storage"), &["node-a"]))
        .await;
    let b = farm
        .pki
        .channel(farm.addr, farm.pki.client(Some("storage"), &["node-b"]))
        .await;
    cold(&a)
        .get_store_config(want(0))
        .await
        .expect("listed later, served now");
    replace("node node-a\n");
    let refused = cold(&a)
        .get_store_config(want(0))
        .await
        .expect_err("listed");
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert!(!refused.message().contains(SENTINEL));
    cold(&b)
        .get_store_config(want(0))
        .await
        .expect("node-b is served");
}

/// Catches: a membership call that fails because the acks cannot be kept. The registration is
/// the frontend's extra; a node's membership must not depend on it. With the acks' store
/// failing (a read, a write), conflicting at every attempt, or panicking, `Register` and
/// `Heartbeat` still answer, the heartbeat's report is lost (the farm reads `pending`, the safe
/// side), and nothing about the failure reaches the node.
///
/// Mutant seen red: the error of `record` returned from `Heartbeat`.
#[tokio::test]
async fn acks_that_cannot_be_kept_do_not_fail_the_membership_calls() {
    use std::sync::atomic::Ordering;
    let farm = farm(Source::new(4), false).await;
    let channel = farm
        .pki
        .channel(farm.addr, farm.pki.client(Some("storage"), &["node-a"]))
        .await;
    let mut node = membership(&channel);
    let beat = |joined: &pb::RegisterResponse| pb::HeartbeatRequest {
        member_id: joined.member_id,
        boot: joined.boot,
        applied_objstore_generation: 4,
        apply_status: pb::ApplyStatus::Ok as i32,
        ..Default::default()
    };

    // The delete at the register fails: the node is still registered.
    farm.ack_store.fail_txns.store(1, Ordering::SeqCst);
    let joined = node
        .register(registration("node-a"))
        .await
        .expect("registers")
        .into_inner();
    let map = kbf_map::Map::try_from(joined.map.clone().expect("a map")).expect("a map");

    // The read at the heartbeat fails, then panics: the node still beats.
    farm.ack_store.fail_gets.store(1, Ordering::SeqCst);
    node.heartbeat(beat(&joined)).await.expect("beats");
    farm.ack_store.panic_gets.store(1, Ordering::SeqCst);
    node.heartbeat(beat(&joined)).await.expect("beats");
    assert!(
        !farm.acks.convergence(&map, 4).expect("read").converged,
        "nothing was kept"
    );

    // Every write conflicts: the report is dropped after its attempts, the delete is a no-op.
    farm.ack_store.conflict_on(Some("objstore/ack/"));
    node.heartbeat(beat(&joined)).await.expect("beats");
    node.register(registration("node-a"))
        .await
        .expect("registers again");
    farm.ack_store.conflict_on(None);
    assert!(!farm.acks.convergence(&map, 4).expect("read").converged);

    // And with the store back, the next report counts.
    let again = node
        .register(registration("node-a"))
        .await
        .expect("again")
        .into_inner();
    node.heartbeat(beat(&again)).await.expect("beats");
    assert!(farm.acks.convergence(&map, 4).expect("read").converged);
}

/// Catches: the service's own checks, which the listener's table hides on the wire (it refuses
/// a daemon, and a handshake without a certificate never reaches the service): called
/// directly, plain text admits without a certificate, mutual TLS without one is
/// UNAUTHENTICATED, a daemon certificate is PERMISSION_DENIED saying so, and a storage
/// certificate is admitted with the names it carries.
#[tokio::test]
async fn the_service_refuses_a_daemon_and_a_missing_certificate_itself() {
    use kbf_server::store_config::service::ColdStoreService;
    use kbf_storage::service::Admission;
    let plain = ColdStoreService::new(Source::new(1), Admission::Open);
    assert!(plain.admit_leaf(None).await.expect("plain text").is_none());

    let certified = ColdStoreService::new(Source::new(1), Admission::Certified);
    let none = certified
        .admit_leaf(None)
        .await
        .expect_err("no certificate");
    assert_eq!(none.code(), Code::Unauthenticated);
    let daemon = der(Some("daemon"), &["node-a"]);
    let refused = certified
        .admit_leaf(Some(&daemon))
        .await
        .expect_err("a daemon");
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert_eq!(
        refused.message(),
        "role daemon may not fetch the cold store"
    );
    let storage = der(Some("storage"), &["node-a"]);
    let admitted = certified.admit_leaf(Some(&storage)).await.expect("storage");
    assert_eq!(
        admitted.expect("a certificate").node().expect("a node"),
        "node-a"
    );
}
