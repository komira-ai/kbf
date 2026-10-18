//! The cold-store registration's acks and its `ColdStore` service, without a network:
//! which nodes count, what an answer holds, who is refused at every call. Each test names
//! the defect it catches.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use kbf_enroll_registry::{
    EnrollRequest, IssuedRole, TokenSpec, create_token, enroll_txn, revoke_node,
};
use kbf_map::{DenyEntry, Map, Member, MemberId, MemberState, Rule};
use kbf_proto::kbf::enroll::v1::StoreConfig;
use kbf_proto::kbf::map::v1::{ApplyStatus, HeartbeatRequest};
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use kbf_storage::service::{Admission, ROLE_URI_PREFIX};
use kbf_types::{NodeId, Role, RoleSet, TokenId, TokenSecret};
use rcgen::{CertificateParams, KeyPair, SanType};
use tonic::Code;

use super::acks::{ACK_TTL, Acks, Convergence, NodeStatus};
use super::service::ColdStoreService;
use super::{Snapshot, SourceError, StoreConfigSource};
use crate::members::{Members, MembershipConfig};

/// A secret no test output may contain.
const SENTINEL: &str = "SENTINEL-secret-9f3a41c07be5";

fn config(generation: u64) -> StoreConfig {
    StoreConfig {
        connection: "main".to_owned(),
        role: "cold".to_owned(),
        endpoint: "http://s3.example:9000".to_owned(),
        region: "us-east-1".to_owned(),
        bucket: "bkt".to_owned(),
        prefix: "kbf/".to_owned(),
        conditional_put: true,
        min_put_rate: 0,
        access_key_id: "AKIATEST".to_owned(),
        secret_access_key: SENTINEL.to_owned(),
        secret_version: generation,
    }
}

/// A source with a fixed generation, or the error it was told to fail with.
struct Fixed {
    generation: u64,
    fail: Option<SourceError>,
}

impl StoreConfigSource for Fixed {
    fn generation(&self) -> Result<u64, SourceError> {
        self.fail.map_or(Ok(self.generation), Err)
    }

    fn snapshot(&self) -> Result<Snapshot, SourceError> {
        if let Some(e) = self.fail {
            return Err(e);
        }
        Ok(Snapshot {
            generation: self.generation,
            config: (self.generation > 0).then(|| config(self.generation)),
        })
    }
}

fn fixed(generation: u64) -> Arc<dyn StoreConfigSource> {
    Arc::new(Fixed {
        generation,
        fail: None,
    })
}

fn service(source: Arc<dyn StoreConfigSource>) -> ColdStoreService {
    ColdStoreService::new(source, Admission::Open)
}

fn beat(generation: u64, status: ApplyStatus) -> HeartbeatRequest {
    HeartbeatRequest {
        applied_objstore_generation: generation,
        apply_status: status as i32,
        ..Default::default()
    }
}

fn member(id: u64, state: MemberState) -> Member {
    Member {
        id: MemberId(id),
        addr: format!("node-{id}:8982"),
        zone: String::new(),
        weight: 1,
        state,
        boot: 1,
    }
}

fn map_of(members: Vec<Member>, denied: Vec<DenyEntry>) -> Map {
    Map::new(1, members, Rule::Rendezvous, 1, 1, denied).expect("a map")
}

fn actives(n: u64) -> Map {
    map_of(
        (1..=n).map(|i| member(i, MemberState::Active)).collect(),
        Vec::new(),
    )
}

/// Acks over a fresh store with a clock the test moves.
fn rig() -> (Acks, Arc<ManualClock>, Arc<Fake>) {
    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_secs(1_000_000));
    let store = Arc::new(Fake::with_flaw(Arc::clone(&clock), Flaw::None));
    (
        Acks::new(Arc::clone(&store) as Arc<dyn Store>),
        clock,
        store,
    )
}

fn judge(acks: &Acks, map: &Map, generation: u64) -> Convergence {
    acks.convergence(map, generation).expect("read")
}

fn record(acks: &Acks, member: u64, boot: u64, beat: &HeartbeatRequest) {
    acks.record(MemberId(member), boot, beat).expect("record");
}

/// Catches: a frontend that claims convergence it never saw. With no heartbeat since the
/// start, every active member is `pending` with no `last_seen`, and the farm is not
/// converged; one that reports the generation with status ok makes it converged only when
/// all of them have.
#[test]
fn a_node_never_heard_from_is_pending_and_the_farm_is_not_converged() {
    let (acks, clock, _store) = rig();
    let map = actives(2);
    let got = judge(&acks, &map, 4);
    assert!(!got.converged);
    assert_eq!((got.expected, got.applied), (2, 0));
    for node in &got.nodes {
        assert_eq!(node.status, NodeStatus::Pending);
        assert_eq!((node.applied_generation, node.last_seen), (None, None));
    }
    record(&acks, 1, 1, &beat(4, ApplyStatus::Ok));
    let half = judge(&acks, &map, 4);
    assert!(!half.converged, "one of two");
    assert_eq!((half.expected, half.applied), (2, 1));
    record(&acks, 2, 1, &beat(4, ApplyStatus::Ok));
    clock.advance(Duration::from_secs(7));
    let all = judge(&acks, &map, 4);
    assert!(all.converged);
    assert_eq!(all.applied, 2);
    assert_eq!(all.nodes[0].last_seen, Some(Duration::from_secs(7)));
    assert_eq!(all.nodes[0].applied_generation, Some(4));
}

/// Catches: "ok" at an older generation counted as applied (a node that applied 3 is not
/// converged on 4), and the reverse after a restore: a node at a higher generation than the
/// frontend's is not converged either, so it refetches (see the service tests).
#[test]
fn ok_counts_only_at_the_generation_being_judged() {
    let (acks, _clock, _store) = rig();
    let map = actives(1);
    record(&acks, 1, 1, &beat(3, ApplyStatus::Ok));
    let got = judge(&acks, &map, 4);
    assert!(!got.converged);
    assert_eq!(got.nodes[0].status, NodeStatus::Pending, "stale, not ok");
    assert!(
        !judge(&acks, &map, 2).converged,
        "newer than the frontend's"
    );
    assert!(judge(&acks, &map, 3).converged);
}

/// Catches: a draining, joining, catching-up or down member, or one a `node` deny entry
/// names (in any case), counted in `expected`, which would hold the farm unconverged for a
/// node that will never apply.
#[test]
fn members_that_are_not_active_or_are_denied_are_not_expected() {
    let (acks, _clock, _store) = rig();
    let map = map_of(
        vec![
            member(1, MemberState::Active),
            member(2, MemberState::Draining),
            member(3, MemberState::Joining),
            member(4, MemberState::CatchingUp),
            member(5, MemberState::Down),
            member(6, MemberState::Active),
        ],
        vec![
            DenyEntry::Node("NODE-6".to_owned()),
            DenyEntry::Serial("ab".to_owned()),
        ],
    );
    record(&acks, 1, 1, &beat(2, ApplyStatus::Ok));
    let got = judge(&acks, &map, 2);
    assert_eq!((got.expected, got.applied), (1, 1));
    assert!(got.converged, "{got:?}");
    assert_eq!(got.nodes.len(), 6, "every member is listed");
}

/// Catches: an old node that sends neither field shown as pending for ever (it would be
/// indistinguishable from "not fetched yet"), or counted as expected. Status 0, an
/// explicit unsupported and a number this build does not know are all recorded as
/// unsupported, listed apart and named; they keep the farm from converged.
#[test]
fn a_node_that_does_not_report_is_unsupported_and_named() {
    for status in [
        beat(0, ApplyStatus::Unspecified),
        beat(5, ApplyStatus::Unsupported),
        HeartbeatRequest {
            apply_status: 77,
            ..Default::default()
        },
    ] {
        let (acks, _clock, _store) = rig();
        let map = actives(2);
        record(&acks, 1, 1, &status);
        record(&acks, 2, 1, &beat(5, ApplyStatus::Ok));
        let got = judge(&acks, &map, 5);
        assert_eq!(got.unsupported, vec![MemberId(1)], "{status:?}");
        assert_eq!((got.expected, got.applied), (1, 1));
        assert!(!got.converged, "an unsupported node cannot hold the keys");
        assert_eq!(got.nodes[0].status, NodeStatus::Unsupported);
        assert_eq!(got.nodes[0].status.code(), "unsupported");
    }
}

/// Catches: a node's failed probe or identity guard shown as ok or pending, and a status
/// code spelling that drifts from the operator API's closed set.
#[test]
fn probe_failures_and_identity_mismatches_are_shown_and_block_convergence() {
    let (acks, _clock, _store) = rig();
    let map = actives(3);
    record(&acks, 1, 1, &beat(2, ApplyStatus::Ok));
    record(&acks, 2, 1, &beat(1, ApplyStatus::ProbeFailed));
    record(&acks, 3, 1, &beat(1, ApplyStatus::IdentityMismatch));
    let got = judge(&acks, &map, 2);
    let codes: Vec<&str> = got.nodes.iter().map(|n| n.status.code()).collect();
    assert_eq!(codes, ["ok", "probe_failed", "identity_mismatch"]);
    assert!(!got.converged);
    assert_eq!(NodeStatus::Pending.code(), "pending");
    record(&acks, 2, 1, &beat(2, ApplyStatus::Pending));
    assert_eq!(judge(&acks, &map, 2).nodes[1].status.code(), "pending");
}

/// Catches: an older boot's late heartbeat overwriting a newer boot's report, and a node
/// that registered again keeping the ack of its previous run (it applied nothing yet).
#[test]
fn a_new_boot_replaces_and_a_registration_clears() {
    let (acks, _clock, _store) = rig();
    let map = actives(1);
    record(&acks, 1, 2, &beat(4, ApplyStatus::Ok));
    record(&acks, 1, 1, &beat(1, ApplyStatus::ProbeFailed));
    assert!(judge(&acks, &map, 4).converged, "boot 1 is older than 2");
    record(&acks, 1, 3, &beat(4, ApplyStatus::Pending));
    assert!(!judge(&acks, &map, 4).converged, "boot 3 replaces 2");
    record(&acks, 1, 3, &beat(4, ApplyStatus::Ok));
    acks.forget(MemberId(1)).expect("forget");
    let got = judge(&acks, &map, 4);
    assert!(!got.converged);
    assert_eq!(got.nodes[0].status, NodeStatus::Pending);
}

/// Catches: an unchanged answer that carries the secret. A node that holds the
/// generation is sent two numbers and no store, whatever the source returned; and a
/// frontend with nothing registered sends none.
#[tokio::test]
async fn an_unchanged_answer_carries_no_secret() {
    let service = service(fixed(5));
    let same = service.answer(5).await.expect("answer");
    assert_eq!((same.generation, same.config.is_none()), (5, true));
    assert!(!format!("{same:?}").contains(SENTINEL));
    let none = ColdStoreService::new(fixed(0), Admission::Open)
        .answer(0)
        .await
        .expect("answer");
    assert_eq!((none.generation, none.config.is_none()), (0, true));
}

/// Catches: a node refused the store because its generation is not lower: after a restore
/// of the state store the frontend's generation can be below what a node applied, and any
/// number that differs gets the whole store, so the node converges. Also pins that the
/// answer holds what the source's snapshot held.
#[tokio::test]
async fn any_different_generation_gets_the_whole_store() {
    let service = service(fixed(5));
    for have in [0, 4, 6, 99] {
        let got = service.answer(have).await.expect("answer");
        let config = got.config.as_ref().expect("a store");
        assert_eq!(got.generation, 5, "have {have}");
        assert_eq!(
            config.secret_access_key, SENTINEL,
            "the sentinel is in the answer"
        );
        assert_eq!(config.bucket, "bkt");
    }
}

/// A source whose generation moves at every `snapshot`, with the secret version the same
/// number: a response is consistent exactly when the two agree.
struct Moving(AtomicU64, AtomicUsize);

impl StoreConfigSource for Moving {
    fn generation(&self) -> Result<u64, SourceError> {
        Ok(self.0.load(Ordering::SeqCst))
    }

    fn snapshot(&self) -> Result<Snapshot, SourceError> {
        self.1.fetch_add(1, Ordering::SeqCst);
        let generation = self.0.fetch_add(1, Ordering::SeqCst) + 1;
        Ok(Snapshot {
            generation,
            config: Some(config(generation)),
        })
    }
}

/// Catches: an answer built from two reads of the registration (the generation from one,
/// the store from another) under a concurrent change: the store in an answer is the one of
/// the generation it names, and one answer is one read.
#[tokio::test]
async fn an_answer_is_one_snapshot() {
    let moving = Arc::new(Moving(AtomicU64::new(0), AtomicUsize::new(0)));
    let service = Arc::new(service(Arc::clone(&moving) as Arc<dyn StoreConfigSource>));
    let mut calls = Vec::new();
    for _ in 0..32 {
        let service = Arc::clone(&service);
        calls.push(tokio::spawn(async move { service.answer(0).await }));
    }
    for call in calls {
        let got = call.await.expect("joined").expect("answer");
        let config = got.config.expect("a store");
        assert_eq!(config.secret_version, got.generation);
    }
    assert_eq!(moving.1.load(Ordering::SeqCst), 32, "one read per answer");
}

/// Catches: a failure that reaches a node as the wrong code, or with text that carries the
/// secret, a path or the store's own error. Each source failure is UNAVAILABLE (the node
/// retries) with the fixed text of the error.
#[tokio::test]
async fn a_source_that_cannot_answer_is_unavailable_with_fixed_text() {
    for (fail, text) in [
        (
            SourceError::SecretUnavailable,
            "the registered store's secret is not available",
        ),
        (SourceError::Store, "the state store failed"),
    ] {
        let service = ColdStoreService::new(
            Arc::new(Fixed {
                generation: 5,
                fail: Some(fail),
            }),
            Admission::Open,
        );
        let refused = service.answer(0).await.expect_err("unavailable");
        assert_eq!(refused.code(), Code::Unavailable);
        assert_eq!(refused.message(), text);
        assert!(!format!("{refused:?}").contains(SENTINEL));
    }
}

fn cert(role: Option<&str>, dns: &[&str]) -> Vec<u8> {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
    params.subject_alt_names = dns
        .iter()
        .map(|n| SanType::DnsName((*n).try_into().expect("IA5")))
        .collect();
    if let Some(role) = role {
        params.subject_alt_names.push(SanType::URI(
            format!("{ROLE_URI_PREFIX}{role}").try_into().expect("IA5"),
        ));
    }
    let key = KeyPair::generate().expect("key");
    params.self_signed(&key).expect("sign").der().to_vec()
}

fn fake_store() -> Arc<Fake> {
    let clock = Arc::new(ManualClock::default());
    clock.advance(Duration::from_secs(1_000_000));
    Arc::new(Fake::with_flaw(clock, Flaw::None))
}

/// Enrolls `name` in the registry of `store`.
fn enroll(store: &dyn Store, name: &str) {
    let secret = TokenSecret::from_bytes([7; 32]);
    let token = TokenId::parse("aaaaaaaaaaaa").expect("a token id");
    let spec = TokenSpec {
        roles: RoleSet::new([Role::Daemon]).expect("roles"),
        max_uses: 5,
        ttl: None,
        node_id: None,
        node_prefix: None,
        names: Vec::new(),
        replace: false,
        label: "t".to_owned(),
        created_by: "t".to_owned(),
    };
    create_token(store, &token, &secret, &spec).expect("token");
    let node: NodeId = name.parse().expect("a node id");
    let now = store.now().as_duration().as_secs();
    let mut sign = || {
        Ok(vec![IssuedRole {
            role: Role::Daemon,
            spki_sha256: [name.len() as u8; 32],
            serial: vec![1, name.len() as u8],
            not_before: now - 300,
            not_after: now + 86_400,
            cert_der: vec![1],
        }])
    };
    enroll_txn(
        store,
        &EnrollRequest {
            token_id: &token,
            secret: secret.as_bytes(),
            node_id: &node,
            csrs: &[(Role::Daemon, name.as_bytes())],
            advertise: "",
            zone: "z",
        },
        &mut sign,
    )
    .map_err(|e| format!("{e:?}"))
    .expect("enrolled");
}

fn certified(source: Arc<dyn StoreConfigSource>) -> ColdStoreService {
    ColdStoreService::new(source, Admission::Certified)
}

/// Catches: a role other than storage let in, and the certificate checks the service
/// repeats behind the listener's table: no certificate is UNAUTHENTICATED, a daemon, a
/// frontend, no role and a role that is not one are PERMISSION_DENIED; plain text admits.
#[tokio::test]
async fn only_a_storage_certificate_is_admitted() {
    let service = certified(fixed(1));
    let ok = cert(Some("storage"), &["node-a"]);
    assert!(
        service
            .admit_leaf(Some(&ok))
            .await
            .expect("admitted")
            .is_some()
    );
    let none = service.admit_leaf(None).await.expect_err("no certificate");
    assert_eq!(none.code(), Code::Unauthenticated);
    for (role, dns) in [
        (Some("daemon"), "node-a"),
        (Some("frontend"), "node-a"),
        (None, "node-a"),
        (Some("admin"), "node-a"),
    ] {
        let der = cert(role, &[dns]);
        let refused = service.admit_leaf(Some(&der)).await.expect_err("refused");
        assert_eq!(refused.code(), Code::PermissionDenied, "{role:?}");
    }
    let plain = ColdStoreService::new(fixed(1), Admission::Open);
    assert!(plain.admit_leaf(None).await.expect("plain text").is_none());
}

/// Catches: a denied or revoked node served because it was admitted once: the `deny/` rows
/// are read at every call, so a row written between two calls on one session refuses the
/// second, with no registry in play (an enroll-less frontend still denies a denied node).
#[tokio::test]
async fn a_node_denied_after_a_call_is_refused_at_the_next() {
    let store = fake_store();
    let members = Arc::new(Members::new(
        Arc::clone(&store) as Arc<dyn Store>,
        MembershipConfig::default(),
    ));
    let service = certified(fixed(1)).with_denied_rows(Some(Arc::clone(&members)));
    let der = cert(Some("storage"), &["node-a"]);
    service.admit_leaf(Some(&der)).await.expect("before");
    members
        .deny(&DenyEntry::Node("node-a".to_owned()), "test")
        .expect("deny");
    let refused = service.admit_leaf(Some(&der)).await.expect_err("after");
    assert_eq!(refused.code(), Code::PermissionDenied);
    let other = cert(Some("storage"), &["node-b"]);
    service
        .admit_leaf(Some(&other))
        .await
        .expect("another node");
}

/// Catches: the registry consulted once, or not at all: with a registry, a node must be an
/// `Active` row at every call. An unenrolled node, a name that is no node id and a revoked
/// node are PERMISSION_DENIED; a registry that cannot be read is UNAVAILABLE.
#[tokio::test]
async fn with_a_registry_the_node_must_be_active_at_every_call() {
    let store = fake_store();
    enroll(&*store, "node-a");
    let registry = Arc::clone(&store) as Arc<dyn Store>;
    let service = certified(fixed(1)).with_registry(Some(registry));
    let der = cert(Some("storage"), &["node-a"]);
    service.admit_leaf(Some(&der)).await.expect("active");

    for name in ["node-z", "Not A Node!"] {
        let stranger = cert(Some("storage"), &[name]);
        let refused = service
            .admit_leaf(Some(&stranger))
            .await
            .expect_err("refused");
        assert_eq!(refused.code(), Code::PermissionDenied, "{name}");
    }
    let nameless = cert(Some("storage"), &[]);
    let refused = service
        .admit_leaf(Some(&nameless))
        .await
        .expect_err("no name");
    assert_eq!(refused.code(), Code::PermissionDenied);

    revoke_node(&*store, &"node-a".parse().expect("id"), "test").expect("revoke");
    let refused = service.admit_leaf(Some(&der)).await.expect_err("revoked");
    assert_eq!(refused.code(), Code::PermissionDenied);
}

/// Catches: acks kept in one frontend child's memory. Two children run over one state store
/// and a proxy sends a node's beats to either: a report one child recorded is what the
/// other child's GET shows, and a report older than the TTL reads as pending, so
/// convergence is never claimed from a report nobody renewed.
///
/// Mutants seen red: the rows kept in the `Acks` value (the second child sees nothing); the
/// TTL dropped from the row (the lapsed report still converges).
#[test]
fn two_children_over_one_store_share_the_acks_and_a_lapsed_report_is_pending() {
    let (first, clock, store) = rig();
    let second = Acks::new(store as Arc<dyn Store>);
    let map = actives(2);
    record(&first, 1, 1, &beat(3, ApplyStatus::Ok));
    record(&second, 2, 1, &beat(3, ApplyStatus::Ok));
    for child in [&first, &second] {
        let got = judge(child, &map, 3);
        assert!(got.converged, "{got:?}");
        assert_eq!(got.applied, 2);
    }
    clock.advance(ACK_TTL + Duration::from_secs(1));
    let lapsed = judge(&second, &map, 3);
    assert!(!lapsed.converged);
    assert!(lapsed.nodes.iter().all(|n| n.status == NodeStatus::Pending));
    assert!(lapsed.nodes.iter().all(|n| n.last_seen.is_none()));
}

/// Catches: a row written by a build that knows more than this one (a status number or a
/// length this build does not read) failing the read: it reads as no report, `pending`.
#[test]
fn a_row_this_build_cannot_read_is_pending_not_an_error() {
    let (acks, _clock, store) = rig();
    let map = actives(1);
    store
        .txn(kbf_state::Txn::new().put("objstore/ack/1", vec![0u8; 5], kbf_state::Cond::Any))
        .expect("write")
        .expect("no conflict");
    let got = judge(&acks, &map, 3);
    assert_eq!(got.nodes[0].status, NodeStatus::Pending);
    assert!(!got.converged);
}

/// A source that panics when asked for the snapshot, as a bug in a store would.
struct Panics;

impl StoreConfigSource for Panics {
    fn generation(&self) -> Result<u64, SourceError> {
        Ok(1)
    }

    fn snapshot(&self) -> Result<Snapshot, SourceError> {
        panic!("{SENTINEL}");
    }
}

/// Catches: a source that panics taking the connection down, or its panic text reaching the
/// node: the answer is INTERNAL with fixed text.
#[tokio::test]
async fn a_source_that_panics_is_an_internal_error_with_fixed_text() {
    let refused = service(Arc::new(Panics))
        .answer(0)
        .await
        .expect_err("internal");
    assert_eq!(refused.code(), Code::Internal);
    assert_eq!(refused.message(), "the store config call failed");
}
