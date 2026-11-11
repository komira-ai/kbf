//! The worker listener's copy of the deny rows stays usable while the sweeper's writes wait,
//! is read again just before the listeners serve, and readiness says when it is not usable.
//! The process clock is tokio's, paused: a test moves it by hand and waits for what the
//! server did, never for time to pass.

use std::sync::mpsc;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use kbf_state::fake::{Fake, Flaw};
use kbf_state::{
    Changes, Commit, CommitStats, Cond, Conflict, ManualClock, Revision, ScanPage, ScanRange,
    Store, StoreError, StoreTime, Txn, Versioned,
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use tonic::Code;

use super::{CHECK, denying, probing};
use crate::health::{Checks, Readiness};
use crate::identity::{PeerCert, Peers};
use crate::members::{Members, MembershipConfig};

const TTL: Duration = Duration::from_secs(60);
/// One beat: a third of the time to live. The copy is good for three.
const BEAT: Duration = Duration::from_secs(20);

/// A fake store whose reaps can be held (`hold_reaps`) until the test lets them go: a
/// write that waits, as one queued behind another frontend's long write on the commit
/// head does. It says when a reap begins to wait.
struct Holding {
    fake: Fake,
    /// While `Some`, a reap waits until the sender is dropped.
    held: Mutex<Option<mpsc::Receiver<()>>>,
    reaping: UnboundedSender<()>,
}

impl Holding {
    fn new() -> (Arc<Self>, UnboundedReceiver<()>) {
        let (reaping, reaps) = unbounded_channel();
        let store = Arc::new(Self {
            fake: Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None),
            held: Mutex::new(None),
            reaping,
        });
        (store, reaps)
    }

    /// Holds every reap from now on, until the returned sender is dropped.
    fn hold_reaps(&self) -> mpsc::Sender<()> {
        let (release, held) = mpsc::channel();
        *self.held.lock().unwrap_or_else(PoisonError::into_inner) = Some(held);
        release
    }
}

impl Store for Holding {
    fn apply_batch(&self, batch: Vec<Txn>) -> Result<Vec<Result<Commit, Conflict>>, StoreError> {
        self.fake.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<Versioned>, StoreError> {
        self.fake.get(key)
    }

    fn scan_prefix(&self, prefix: &str) -> Result<Vec<(String, Versioned)>, StoreError> {
        self.fake.scan_prefix(prefix)
    }

    fn scan_range(&self, scan: ScanRange<'_>) -> Result<ScanPage, StoreError> {
        self.fake.scan_range(scan)
    }

    fn changes(&self, after: Revision, limit: usize) -> Result<Changes, StoreError> {
        self.fake.changes(after, limit)
    }

    fn trim_feed(&self, before: Revision) -> Result<u64, StoreError> {
        self.fake.trim_feed(before)
    }

    fn reap_expired(&self) -> Result<usize, StoreError> {
        let held = self.held.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(held) = held.as_ref() {
            let _ = self.reaping.send(());
            // Returns when the test drops the sender.
            let _ = held.recv();
        }
        drop(held);
        self.fake.reap_expired()
    }

    fn commit_stats(&self) -> CommitStats {
        self.fake.commit_stats()
    }

    fn now(&self) -> StoreTime {
        self.fake.now()
    }
}

fn config() -> MembershipConfig {
    MembershipConfig {
        ttl: TTL,
        dwell: TTL,
        replication: 2,
    }
}

fn members_over(store: Arc<dyn Store>) -> Arc<Members> {
    Arc::new(Members::new(store, config()))
}

/// Members over a fake store in which `node-a` is denied, its rows read once (as the start
/// does when it opens them).
fn members_denying_node_a() -> Arc<Members> {
    let fake = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let members = members_over(fake);
    members
        .deny(&"node node-a".parse().expect("entry"), "revoked")
        .expect("deny");
    members.refresh_denied().expect("the first read");
    members
}

fn certified() -> Peers {
    Peers::Certified {
        deny_list: None,
        denied_rows: None,
    }
}

fn peer(node: &str) -> PeerCert {
    PeerCert::proxied(node, &[])
}

/// What `peers` answer for `node-a` (denied) and `node-b` (not): `None` for admitted, else
/// the code.
async fn answers(peers: &Peers) -> (Option<Code>, Option<Code>) {
    let code = |r: Result<(), tonic::Status>| r.err().map(|s| s.code());
    (
        code(peers.admit(Some(&peer("node-a")), "node-a").await),
        code(peers.admit(Some(&peer("node-b")), "node-b").await),
    )
}

/// Yields until `members`' copy of the rows is from a read that began at or after `at`: the
/// sweeper's tasks run on threads of their own, so the test waits for what they did.
async fn read_since(members: &Members, at: tokio::time::Instant) {
    while members
        .denied_snapshot()
        .is_none_or(|(_, began)| began < at)
    {
        tokio::task::yield_now().await;
    }
}

/// While the sweeper's reap waits (a write queued behind another frontend's long write on
/// the commit head), the deny rows are still read every beat, so after four beats, past the
/// three the copy is good for, the denied node is still refused PERMISSION_DENIED and the
/// other admitted. Before, the rows were read only by the map refresh that runs between the
/// sweeper's reaps, so this wait never ended: nothing read them while the reap waited, and
/// every check was refused UNAVAILABLE.
///
/// Catches: the deny rows read only in the same loop as the sweeper's writes.
#[tokio::test(start_paused = true)]
async fn a_sweep_held_up_by_a_write_does_not_age_the_deny_rows() {
    let (store, mut reaps) = Holding::new();
    let members = members_over(Arc::clone(&store) as Arc<dyn Store>);
    members
        .deny(&"node node-a".parse().expect("entry"), "revoked")
        .expect("deny");
    let peers = certified().with_denied_rows(Some(Arc::clone(&members)));
    let release = store.hold_reaps();
    let sweeper = tokio::spawn(crate::serve::sweeping(Some(Arc::clone(&members))));
    reaps.recv().await.expect("the first reap waits");

    for _ in 0..4 {
        tokio::time::advance(BEAT).await;
        read_since(&members, tokio::time::Instant::now()).await;
    }
    assert_eq!(
        answers(&peers).await,
        (Some(Code::PermissionDenied), None),
        "four beats into the wait"
    );

    drop(release);
    sweeper.abort();
}

/// The rows read just before the listeners serve: a copy made at the start of a start that
/// then took four beats (the farm's state restored) is read again, so the first daemon is
/// admitted, and the denied one refused, without waiting for the sweeper. The same peers
/// made without that read refuse both UNAVAILABLE: the copy is too old.
///
/// Catches: a start whose only read of the rows comes before its slow steps.
#[tokio::test(start_paused = true)]
async fn the_rows_are_read_again_just_before_the_listeners_serve() {
    let members = members_denying_node_a();
    tokio::time::advance(BEAT * 4).await;
    let unread = certified().with_denied_rows(Some(Arc::clone(&members)));
    assert_eq!(
        answers(&unread).await,
        (Some(Code::Unavailable), Some(Code::Unavailable)),
        "the copy the start made first is too old by now"
    );
    let read = denying(certified(), Some(&members));
    assert_eq!(answers(&read).await, (Some(Code::PermissionDenied), None));
}

/// A read before serving that fails (a corrupt `deny/` row) does not stop the start: the
/// peers come back, still checking the rows, and refuse UNAVAILABLE until a read succeeds.
/// With no members, the peers are as given: plain text admits, and certified peers check no
/// rows.
#[tokio::test]
async fn a_failing_read_before_serving_starts_anyway_and_no_members_changes_nothing() {
    let fake = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let written = fake
        .txn(Txn::new().put("deny/bogus/x", b"?".to_vec(), Cond::Any))
        .expect("write the row");
    assert!(written.is_ok());
    let members = members_over(fake);
    let peers = denying(certified(), Some(&members));
    assert!(peers.denied_rows().is_some());
    assert_eq!(
        answers(&peers).await,
        (Some(Code::Unavailable), Some(Code::Unavailable))
    );

    let plain = denying(Peers::Unauthenticated, None);
    assert_eq!(answers(&plain).await, (None, None));
    let no_rows = denying(certified(), None);
    assert!(no_rows.denied_rows().is_none());
    assert_eq!(answers(&no_rows).await, (None, None));
}

/// A read that began earlier and finishes later does not put its older rows over the copy a
/// later read left; a read that began later (or at the same instant) does replace it.
///
/// Catches: the copy of whichever read finished last, which could undo a deny a revoke had
/// just read.
#[tokio::test(start_paused = true)]
async fn an_older_read_that_finishes_last_does_not_replace_a_newer_copy() {
    let members = members_denying_node_a();
    let earlier = tokio::time::Instant::now();
    tokio::time::advance(Duration::from_secs(1)).await;
    members.refresh_denied().expect("a later read");
    let later = members.denied_snapshot().expect("a copy").1;
    members.note_denied(Vec::new(), earlier);
    let (rows, began) = members.denied_snapshot().expect("a copy");
    assert_eq!(began, later);
    assert_eq!(rows.len(), 1, "node-a is still denied");

    members.note_denied(Vec::new(), later);
    let (rows, began) = members.denied_snapshot().expect("a copy");
    assert_eq!((rows.len(), began), (0, later));
}

/// What the `deny_rows` check says, if it fails.
async fn deny_rows_failing(checks: &Checks) -> Option<String> {
    checks
        .failing()
        .await
        .into_iter()
        .find(|failing| failing.check == CHECK)
        .map(|failing| failing.reason)
}

/// `/readyz` fails `deny_rows` before any read of the rows and again once the copy is older
/// than three beats, and passes in between, at three beats exactly included. The reasons are
/// the whole text.
///
/// Catches: a fresh frontend that says it is ready while its worker listener refuses everyone,
/// and a check that disagrees with the listener's rule at its edge.
#[tokio::test(start_paused = true)]
async fn readiness_fails_while_the_worker_listener_cannot_use_the_rows() {
    let fake = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let members = members_over(fake);
    let peers = certified().with_denied_rows(Some(Arc::clone(&members)));
    let checks = probing(Checks::new(Arc::new(Readiness::default())), &peers);
    assert_eq!(
        deny_rows_failing(&checks).await.as_deref(),
        Some("the worker listener refuses every call: the deny rows: no refresh has read them yet")
    );

    members.refresh_denied().expect("read");
    assert_eq!(deny_rows_failing(&checks).await, None);
    tokio::time::advance(BEAT * 3).await;
    assert_eq!(
        deny_rows_failing(&checks).await,
        None,
        "three beats is still good"
    );
    tokio::time::advance(Duration::from_millis(1)).await;
    assert_eq!(
        deny_rows_failing(&checks).await.as_deref(),
        Some(
            "the worker listener refuses every call: the deny rows: the newest refresh that \
             read them is too old"
        )
    );
}

/// Peers that check no deny rows (plain text; certified with no state store) add no
/// `deny_rows` check, so a frontend without the rows is not held unready for them.
#[tokio::test]
async fn peers_without_the_rows_add_no_check() {
    let members = members_over(Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    )));
    for peers in [
        Peers::Unauthenticated.with_denied_rows(Some(Arc::clone(&members))),
        certified().with_denied_rows(None),
    ] {
        let checks = probing(Checks::new(Arc::new(Readiness::default())), &peers);
        assert!(checks.failing().await.is_empty());
    }
}

/// A per-beat read that fails (a corrupt `deny/` row) is logged and leaves the earlier copy,
/// which ages: four beats on every check is refused UNAVAILABLE and `deny_rows` fails. Once the
/// row is deleted the next beat's read readmits the node that is not denied and refuses the
/// one that is, and readiness passes again.
///
/// Catches: a failed read that ends the beat's task, or that replaces the copy with nothing.
#[tokio::test(start_paused = true)]
async fn a_failing_beat_read_ages_the_copy_and_the_next_good_one_restores_it() {
    let log = crate::test_log::capture();
    let fake = Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    ));
    let members = members_over(Arc::clone(&fake) as Arc<dyn Store>);
    members
        .deny(&"node node-a".parse().expect("entry"), "revoked")
        .expect("deny");
    members.refresh_denied().expect("the first read");
    let peers = certified().with_denied_rows(Some(Arc::clone(&members)));
    let checks = probing(Checks::new(Arc::new(Readiness::default())), &peers);
    let written = fake
        .txn(Txn::new().put("deny/bogus/x", b"?".to_vec(), Cond::Any))
        .expect("write the row");
    assert!(written.is_ok());
    let sweeper = tokio::spawn(crate::serve::sweeping(Some(Arc::clone(&members))));

    for _ in 0..4 {
        tokio::time::advance(BEAT).await;
    }
    while !log.text().contains("deny rows refresh: ") {
        tokio::task::yield_now().await;
    }
    assert_eq!(
        answers(&peers).await,
        (Some(Code::Unavailable), Some(Code::Unavailable))
    );
    assert!(deny_rows_failing(&checks).await.is_some());

    let gone = fake
        .txn(Txn::new().delete("deny/bogus/x", Cond::Any))
        .expect("delete the row");
    assert!(gone.is_ok());
    tokio::time::advance(BEAT).await;
    read_since(&members, tokio::time::Instant::now()).await;
    assert_eq!(answers(&peers).await, (Some(Code::PermissionDenied), None));
    assert_eq!(deny_rows_failing(&checks).await, None);
    sweeper.abort();
}

/// `GET /readyz` on the health router over these checks: the status and the JSON body.
async fn get_readyz(checks: Arc<Checks>) -> (axum::http::StatusCode, serde_json::Value) {
    use tower_service::Service as _;
    let mut router = crate::health::router_of(checks);
    let request = axum::http::Request::builder()
        .uri("/readyz")
        .body(axum::body::Body::empty())
        .expect("a request");
    let answer = router.call(request).await.expect("an answer");
    let code = answer.status();
    let bytes = axum::body::to_bytes(answer.into_body(), usize::MAX)
        .await
        .expect("a body");
    (code, serde_json::from_slice(&bytes).expect("a JSON body"))
}

/// Through the mounted `/readyz` route: 503 with exactly the `deny_rows` failure before any
/// read of the rows, then 200 with nothing failing once a read has found them.
///
/// Catches: a `deny_rows` check that the probe reports but the route does not answer with.
#[tokio::test]
async fn readyz_answers_503_naming_deny_rows_until_the_rows_are_read() {
    let members = members_over(Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    )));
    let peers = certified().with_denied_rows(Some(Arc::clone(&members)));
    let checks = Arc::new(probing(Checks::new(Arc::new(Readiness::default())), &peers));

    let (code, body) = get_readyz(Arc::clone(&checks)).await;
    assert_eq!(code, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(body["ready"], false);
    assert_eq!(
        body["failing"],
        serde_json::json!([{
            "check": "deny_rows",
            "reason": "the worker listener refuses every call: the deny rows: no refresh has read them yet"
        }])
    );

    members.refresh_denied().expect("read");
    let (code, body) = get_readyz(checks).await;
    assert_eq!(code, axum::http::StatusCode::OK);
    assert_eq!(body["ready"], true);
    assert_eq!(body["failing"], serde_json::json!([]));
}
