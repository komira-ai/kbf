//! The renewal schedule of the agent without a frontend: a host that is not enrolled has no
//! renewer, a renewer waits for its time, and a renewal that fails is logged and tried again
//! later. A renewal that works is run against the frontend's real enroll listener in
//! `kbf-server`'s tests.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use kbf_enroll::answer::Cert;
use kbf_enroll::config::{FrontendUrl, HostName};
use kbf_enroll::store::{Node, Store, StoredConfig};
use kbf_testkit::dir::TestDir;
use kbf_types::{CaPin, Role, RoleSet};
use rcgen::{CertificateParams, KeyPair};

use super::renew::{FrontendRenewer, NAP_MAX_S, Renewer, nap};

/// A host enrolled with a certificate valid until `not_after`, whose renewal is due at `due`.
fn enrolled(dir: &TestDir, due: i64, not_after: i64) -> Store {
    let store = Store::open(&dir.join("state")).unwrap();
    store
        .pending(RoleSet::new([Role::Updater]).unwrap())
        .unwrap();
    let staged = std::fs::read_to_string(dir.join("state/pki/staging/updater.key")).unwrap();
    let key = KeyPair::from_pem(&staged).unwrap();
    let cert = Cert {
        role: Role::Updater,
        pem: CertificateParams::new(vec!["host-1".to_owned()])
            .unwrap()
            .self_signed(&key)
            .unwrap()
            .pem(),
        serial: "a1".to_owned(),
        not_before: 1,
        not_after,
    };
    let node = Node::new(
        &"host-1".parse().unwrap(),
        &CaPin::from_bytes([3; 32]),
        std::slice::from_ref(&cert),
        due,
        StoredConfig::default(),
    );
    store.stage(&[cert], b"CA", &node).unwrap();
    store.commit().unwrap();
    store
}

fn url(port: u16) -> FrontendUrl {
    FrontendUrl {
        host: HostName::Dns("localhost".to_owned()),
        port,
    }
}

fn closed_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Rig {
    now: Arc<AtomicI64>,
    logs: Arc<Mutex<Vec<String>>>,
    renewer: FrontendRenewer,
}

fn rig(dir: &TestDir, due: i64, not_after: i64) -> Rig {
    let store = enrolled(dir, due, not_after);
    let now = Arc::new(AtomicI64::new(1_000));
    let logs = Arc::new(Mutex::new(Vec::new()));
    let (clock, sink) = (Arc::clone(&now), Arc::clone(&logs));
    let renewer = FrontendRenewer::new(
        url(closed_port()),
        store,
        Arc::new(move || clock.load(Ordering::SeqCst)),
        Arc::new(move |line| sink.lock().unwrap().push(line.to_owned())),
        0,
    )
    .unwrap();
    Rig { now, logs, renewer }
}

/// A host that was never enrolled has nothing to renew, and says what to do.
#[test]
fn a_host_that_is_not_enrolled_has_no_renewer() {
    let dir = TestDir::under(std::env::temp_dir(), "rn-none");
    let store = Store::open(&dir.join("state")).unwrap();
    let err = FrontendRenewer::new(url(1), store, Arc::new(|| 0), Arc::new(|_| {}), 0)
        .err()
        .unwrap();
    assert!(err.contains("kbf-updater enroll"), "{err}");
}

/// Before its time a round calls nothing and waits the rest of the way, at most a minute at a
/// time (a wall clock that jumped is noticed within it) and at least a second.
#[test]
fn before_its_time_a_round_waits_and_calls_nothing() {
    let dir = TestDir::under(std::env::temp_dir(), "rn-wait");
    let near = rig(&dir, 1_030, 5_000);
    assert_eq!(near.renewer.round().as_secs(), 30);
    near.now.store(1_029, Ordering::SeqCst);
    assert_eq!(near.renewer.round().as_secs(), 1);
    assert!(near.logs.lock().unwrap().is_empty());
    let other = TestDir::under(std::env::temp_dir(), "rn-far");
    let far = rig(&other, 100_000, 200_000);
    assert_eq!(far.renewer.round().as_secs(), NAP_MAX_S);
    assert_eq!(nap(-5).as_secs(), 1);
    assert_eq!(nap(0).as_secs(), 1);
    assert_eq!(nap(i64::MAX).as_secs(), NAP_MAX_S);
}

/// A renewal that cannot reach the frontend is logged and tried again after five minutes, then
/// ten (the wait doubles); a certificate with under a day left says how long it has.
#[test]
fn a_failed_renewal_is_logged_and_retried_later_with_a_longer_wait() {
    let dir = TestDir::under(std::env::temp_dir(), "rn-fail");
    let rig = rig(&dir, 900, 1_000 + 30 * 86_400);
    // Due, and the frontend is not there: logged, and the next look is a minute away at most.
    assert_eq!(rig.renewer.round().as_secs(), NAP_MAX_S);
    let first = rig.logs.lock().unwrap().clone();
    assert_eq!(first.len(), 1);
    assert!(first[0].contains("will retry"), "{first:?}");
    // Not due again for five minutes.
    rig.now.store(1_000 + 299, Ordering::SeqCst);
    rig.renewer.round();
    assert_eq!(rig.logs.lock().unwrap().len(), 1);
    // Due after five: tried and failed again; the wait is now ten minutes.
    rig.now.store(1_000 + 300, Ordering::SeqCst);
    rig.renewer.round();
    assert_eq!(rig.logs.lock().unwrap().len(), 2);
    rig.now.store(1_000 + 300 + 599, Ordering::SeqCst);
    rig.renewer.round();
    assert_eq!(rig.logs.lock().unwrap().len(), 2);
    rig.now.store(1_000 + 300 + 600, Ordering::SeqCst);
    rig.renewer.round();
    assert_eq!(rig.logs.lock().unwrap().len(), 3);
}

#[test]
fn a_certificate_close_to_expiry_says_so() {
    let dir = TestDir::under(std::env::temp_dir(), "rn-expiring");
    let rig = rig(&dir, 900, 1_000 + 3_600);
    rig.renewer.round();
    let logs = rig.logs.lock().unwrap().clone();
    assert!(logs[0].contains("expires in 3600 s"), "{logs:?}");
}
