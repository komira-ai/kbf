//! The adapters that hand the registry and the acks to the rest of the frontend. Each test
//! names the defect it catches.

use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kbf_map::{DenyEntry, MemberState};
use kbf_proto::kbf::map::v1::{ApplyStatus, HeartbeatRequest};
use kbf_state::{
    Changes, Commit, CommitStats, Conflict, Revision, ScanPage, ScanRange, Store, StoreError,
    StoreTime, Txn, Versioned,
};
use kbf_types::object_store_api::{BindingRole, MemberStanding, NodeStatus};

use super::super::tests::{Counting, SENTINEL, as_store, bind, fake, name, put, scratch};
use super::*;
use crate::members::{Members, MembershipConfig, Progress, Registrant};

const SECOND: &str = "SECOND-s3-secret-0a1b2c3d4e5f";

fn source(store: &Arc<dyn Store>, dir: &Path) -> (Arc<Registry>, RegistrySource) {
    let registry = Arc::new(
        Registry::open(Arc::clone(store), dir, Counting::arc()).expect("open the registry"),
    );
    (Arc::clone(&registry), RegistrySource::new(registry))
}

/// A registered `cold` store: generation 2 (the connection, then the binding).
fn bound(registry: &Registry) {
    let mut connection = put(SENTINEL);
    connection.region = Some("eu-test-1".to_owned());
    connection.conditional_put = Some(true);
    connection.min_put_rate = std::num::NonZeroU64::new(4096);
    registry
        .put_connection(&name("cold"), &connection)
        .expect("connection");
    registry
        .put_binding(BindingRole::Cold, &bind("cold", "bucket"))
        .expect("binding");
}

// Catches a source that stamps the registry's generation on nothing: a connection with no
// binding is no registration, so the snapshot is generation 0 and carries no store, while
// the cheap `generation()` a heartbeat announces still reads the registry's own.
#[test]
fn a_connection_without_a_binding_is_generation_zero() {
    let store = as_store(&fake());
    let (registry, source) = source(&store, &scratch("src-none").join("s"));
    assert_eq!(source.snapshot().expect("snapshot").generation, 0);
    registry
        .put_connection(&name("cold"), &put(SENTINEL))
        .expect("connection");
    assert_eq!(source.generation().expect("generation"), 1);
    let snapshot = source.snapshot().expect("snapshot");
    assert_eq!(snapshot.generation, 0);
    assert!(snapshot.config.is_none());
}

// Catches a snapshot that pairs the wrong pieces: the connection's endpoint and key id, the
// binding's bucket and prefix, the secret read from its file, and `secret_version` the
// generation (a rotation moves it, so the node reopens on a new key).
#[test]
fn a_snapshot_is_the_binding_the_connection_and_the_secret_at_one_generation() {
    let store = as_store(&fake());
    let (registry, source) = source(&store, &scratch("src-whole").join("s"));
    bound(&registry);
    let snapshot = source.snapshot().expect("snapshot");
    assert_eq!(snapshot.generation, 2);
    let config = snapshot.config.expect("registered");
    assert_eq!(
        (
            config.connection.as_str(),
            config.role.as_str(),
            config.endpoint.as_str(),
            config.bucket.as_str(),
            config.prefix.as_str(),
        ),
        ("cold", "cold", "http://s3.example:9000", "bucket", "farm/")
    );
    assert_eq!(
        (
            config.access_key_id.as_str(),
            config.secret_access_key.as_str()
        ),
        ("AKID", SENTINEL)
    );
    assert_eq!(
        (
            config.region.as_str(),
            config.conditional_put,
            config.min_put_rate,
            config.secret_version
        ),
        ("eu-test-1", true, 4096, 2)
    );

    let mut next = put(SECOND);
    next.region = Some("eu-test-1".to_owned());
    next.conditional_put = Some(true);
    next.min_put_rate = std::num::NonZeroU64::new(4096);
    registry
        .put_connection(&name("cold"), &next)
        .expect("rotate");
    let rotated = source.snapshot().expect("snapshot");
    assert_eq!(rotated.generation, 3);
    let config = rotated.config.expect("registered");
    assert_eq!(
        (config.secret_version, config.secret_access_key.as_str()),
        (3, SECOND)
    );
}

/// A store that runs `hook` after every `scan_prefix` it serves: a write by the other
/// frontend child, landing between the pieces of a reading.
struct Interleaved {
    inner: Arc<dyn Store>,
    hook: Box<dyn Fn() + Send + Sync>,
}

impl Store for Interleaved {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        self.inner.apply_batch(batch)
    }
    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.inner.get(key)
    }
    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        let rows = self.inner.scan_prefix(prefix);
        (self.hook)();
        rows
    }
    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.inner.scan_range(scan)
    }
    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.inner.changes(after, limit)
    }
    fn reap_expired(&self) -> Result<usize, StoreError> {
        self.inner.reap_expired()
    }
    fn commit_stats(&self) -> CommitStats {
        self.inner.commit_stats()
    }
    fn now(&self) -> StoreTime {
        self.inner.now()
    }
}

/// A source whose store lets `writes` more rotations in between the pieces of its readings
/// (`None` for every reading), made by a second registry on the same rows and files.
fn raced(dir: &Path, writes: Option<u64>) -> (RegistrySource, Arc<AtomicU64>) {
    let inner = as_store(&fake());
    let rival = Registry::open(Arc::clone(&inner), dir, Counting::from(1_000)).expect("rival");
    bound(&rival);
    let rival = Arc::new(Mutex::new(rival));
    let left = Arc::new(AtomicU64::new(writes.unwrap_or(u64::MAX)));
    let turns = Arc::new(AtomicU64::new(0));
    let armed = Arc::new(AtomicBool::new(false));
    let hook = {
        let (rival, left, turns) = (Arc::clone(&rival), Arc::clone(&left), Arc::clone(&turns));
        let armed = Arc::clone(&armed);
        move || {
            if !armed.load(Ordering::SeqCst) || left.load(Ordering::SeqCst) == 0 {
                return;
            }
            left.fetch_sub(1, Ordering::SeqCst);
            let n = turns.fetch_add(1, Ordering::SeqCst);
            rival
                .lock()
                .expect("rival")
                .put_connection(&name("cold"), &put(&format!("rotated-secret-{n}")))
                .expect("a rotation");
        }
    };
    let store = Arc::new(Interleaved {
        inner,
        hook: Box::new(hook),
    }) as Arc<dyn Store>;
    // Opening sweeps through the store too: the rotations start after it.
    armed.store(false, Ordering::SeqCst);
    let registry = Arc::new(
        Registry::open(store, dir, Counting::from(2_000)).expect("registry over the racing store"),
    );
    armed.store(true, Ordering::SeqCst);
    (RegistrySource::new(registry), turns)
}

// Catches a reading made once: a rotation that lands between the bindings and the secret
// is seen (the generation read after differs from the one before) and the reading is made
// again, so the node gets the rotated key stamped with the generation of the rotation,
// never the new key under the old generation (or the old endpoint with the new key).
#[test]
fn a_write_between_the_pieces_makes_the_reading_again() {
    let (source, turns) = raced(&scratch("src-race").join("s"), Some(1));
    let snapshot = source.snapshot().expect("snapshot");
    assert_eq!(turns.load(Ordering::SeqCst), 1, "the rotation landed once");
    assert_eq!(snapshot.generation, 3, "the rotation made generation 3");
    let config = snapshot.config.expect("registered");
    assert_eq!(config.secret_access_key, "rotated-secret-0");
    assert_eq!(config.secret_version, 3);
}

// Catches a reading that never gives up and one that gives up too soon: a registry that
// changes under every reading is `Store` (the node retries), after exactly the readings
// the constant allows, each of which the rival disturbed once.
#[test]
fn a_registry_that_never_holds_still_is_a_store_failure() {
    let (source, turns) = raced(&scratch("src-churn").join("s"), None);
    let err = source.snapshot().expect_err("it never settles");
    assert_eq!(err, SourceError::Store);
    assert_eq!(turns.load(Ordering::SeqCst), READINGS as u64);
}

// Catches a missing secret file read as a store failure (or as no registration): the node
// is told the secret is unavailable, which it retries, and is never handed a store without
// its key.
#[test]
fn a_secret_file_that_is_gone_is_secret_unavailable() {
    let store = as_store(&fake());
    let dir = scratch("src-gone").join("s");
    let (registry, source) = source(&store, &dir);
    bound(&registry);
    for entry in std::fs::read_dir(&dir).expect("dir") {
        let path = entry.expect("entry").path();
        if path.extension().is_some_and(|e| e == "json") {
            std::fs::remove_file(path).expect("remove");
        }
    }
    assert_eq!(
        source.snapshot().expect_err("no secret"),
        SourceError::SecretUnavailable
    );
}

// ---- Reports ----

fn members(store: &Arc<dyn Store>) -> Arc<Members> {
    Arc::new(Members::new(
        Arc::clone(store),
        MembershipConfig {
            ttl: Duration::from_secs(600),
            dwell: Duration::from_secs(600),
            replication: 1,
        },
    ))
}

fn join(members: &Members, host: &str) -> (kbf_map::MemberId, u64) {
    let registration = members
        .register(&Registrant {
            instance_id: format!("{host}:8982"),
            addr: format!("{host}:8982"),
            zone: String::new(),
            weight: 1,
            protocol: crate::members::PROTOCOL,
            progress: Progress::CaughtUp,
        })
        .expect("register");
    (registration.id, registration.boot)
}

fn beat(generation: u64, status: ApplyStatus) -> HeartbeatRequest {
    HeartbeatRequest {
        applied_objstore_generation: generation,
        apply_status: status.into(),
        ..HeartbeatRequest::default()
    }
}

struct Rig {
    reports: Reports,
    members: Arc<Members>,
    acks: Arc<Acks>,
    registry: Arc<Registry>,
}

fn rig(label: &str) -> Rig {
    let store = as_store(&fake());
    let (registry, _) = source(&store, &scratch(label).join("s"));
    let members = members(&store);
    let acks = Arc::new(Acks::new(Arc::clone(&store)));
    Rig {
        reports: Reports::new(
            Arc::clone(&members),
            Arc::clone(&acks),
            Arc::clone(&registry),
        ),
        members,
        acks,
        registry,
    }
}

// Catches an ack source that invents nodes: a frontend with no members reports none.
#[test]
fn no_members_report_no_nodes() {
    let rig = rig("rep-none");
    assert!(rig.reports.nodes().is_empty());
}

// Catches each field of the mapping: the node is named by the address it registered, an
// Active member that applied the registry's generation is `ok` at that generation (the
// registry's own generation is what the acks are judged against), a member that never
// reported is pending with nothing applied and never seen, and a reporting member says how
// long ago.
#[test]
fn a_node_is_named_by_its_address_and_judged_against_the_registry_generation() {
    let rig = rig("rep-fields");
    bound(&rig.registry);
    let (a, boot_a) = join(&rig.members, "node-a");
    let (b, _boot_b) = join(&rig.members, "node-b");
    for id in [a, b] {
        rig.members
            .set_state(id, MemberState::Active)
            .expect("active");
    }
    rig.acks
        .record(a, boot_a, &beat(2, ApplyStatus::Ok))
        .expect("record");
    let nodes = rig.reports.nodes();
    let find = |id: &str| nodes.iter().find(|n| n.id == id).expect(id).clone();
    let (a, b) = (find("node-a:8982"), find("node-b:8982"));
    assert_eq!(
        (a.state, a.status, a.applied_generation, a.last_seen_secs),
        (MemberStanding::Active, NodeStatus::Ok, Some(2), Some(0))
    );
    assert_eq!(
        (b.state, b.status, b.applied_generation, b.last_seen_secs),
        (MemberStanding::Active, NodeStatus::Pending, None, None)
    );
}

// Catches every status the node can report not reaching the operator as itself.
#[test]
fn each_reported_status_reaches_the_operator_as_itself() {
    let rig = rig("rep-status");
    bound(&rig.registry);
    let cases = [
        ("n-ok", ApplyStatus::Ok, NodeStatus::Ok),
        ("n-pending", ApplyStatus::Pending, NodeStatus::Pending),
        ("n-probe", ApplyStatus::ProbeFailed, NodeStatus::ProbeFailed),
        (
            "n-ident",
            ApplyStatus::IdentityMismatch,
            NodeStatus::IdentityMismatch,
        ),
        ("n-unsup", ApplyStatus::Unsupported, NodeStatus::Unsupported),
    ];
    for (host, sent, _) in cases {
        let (id, boot) = join(&rig.members, host);
        rig.members
            .set_state(id, MemberState::Active)
            .expect("active");
        rig.acks.record(id, boot, &beat(2, sent)).expect("record");
    }
    let nodes = rig.reports.nodes();
    for (host, _, wanted) in cases {
        let node = nodes
            .iter()
            .find(|n| n.id == format!("{host}:8982"))
            .expect(host);
        assert_eq!(node.status, wanted, "{host}");
    }
}

// Catches a draining member counted as a participant and a denied one counted at all:
// neither is expected to apply the registration, so each is shown with the standing the
// operator API leaves out of `expected` (a node revoked for a stolen key must not keep the
// farm from `converged` for ever). A member of any other state is still counted.
#[test]
fn a_draining_node_and_a_denied_node_are_not_standing() {
    let rig = rig("rep-standing");
    bound(&rig.registry);
    let (active, _) = join(&rig.members, "node-active");
    let (draining, _) = join(&rig.members, "node-draining");
    let (denied, _) = join(&rig.members, "node-denied");
    let (joining, _) = join(&rig.members, "node-joining");
    for id in [active, draining, denied] {
        rig.members
            .set_state(id, MemberState::Active)
            .expect("active");
    }
    rig.members
        .set_state(draining, MemberState::Draining)
        .expect("draining");
    rig.members
        .set_state(joining, MemberState::Joining)
        .expect("joining");
    rig.members
        .deny(&DenyEntry::Node("node-denied".to_owned()), "revoked")
        .expect("deny");
    let nodes = rig.reports.nodes();
    let standing = |host: &str| {
        nodes
            .iter()
            .find(|n| n.id == format!("{host}:8982"))
            .expect(host)
            .state
    };
    assert_eq!(standing("node-active"), MemberStanding::Active);
    assert_eq!(standing("node-draining"), MemberStanding::Draining);
    assert_eq!(standing("node-denied"), MemberStanding::Denied);
    assert_eq!(standing("node-joining"), MemberStanding::Active);
}
