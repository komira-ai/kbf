//! The map's deny list at the storage endpoints, over real mutual TLS: a certificate an
//! entry of the map names is refused PERMISSION_DENIED on every method, after the role is
//! judged, and from the first call after the node holds the map that lists it.

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use kbf_map::DenyEntry;
use rcgen::SanType;
use tonic::{Code, Status};

use super::fake::{Held, Mem, connect, serve_routes};
use super::roles::{Pki, RPCS, call, call_rpc, dns, expected, ip, uri};
use crate::Storage;
use crate::replicate::MapSource;
use crate::service::{Admission, Config, Endpoint, PeerRole, StorageService};

fn config() -> Config {
    Config {
        write_bytes_in_flight: 1 << 20,
    }
}

/// Every method a caller can make, with the endpoint whose roles it takes: the rows of
/// [`RPCS`], and the copy a storage node sends another (a `Write` that names itself one),
/// which `RPCS` leaves out because only one role may send it.
fn methods() -> Vec<(&'static str, Endpoint)> {
    let mut methods = RPCS.to_vec();
    methods.push(("Write (a copy)", Endpoint::WriteLeg));
    methods
}

/// One call of the method `rpc` names (a row of [`methods`]).
async fn invoke(
    rpc: &str,
    endpoint: Endpoint,
    channel: &tonic::transport::Channel,
) -> Result<(), Status> {
    match endpoint {
        Endpoint::WriteLeg => call(Endpoint::WriteLeg, channel).await,
        _ => call_rpc(rpc, channel).await,
    }
}

/// How far the store has been reached: puts, reads and `Segments` calls.
fn reached(mem: &Mem) -> (usize, usize, usize) {
    (
        mem.puts.load(Ordering::SeqCst),
        mem.reads.load(Ordering::SeqCst),
        mem.segments.calls.lock().expect("lock").len(),
    )
}

/// A certified service over `mem` that judges calls by the map `held` holds, on a
/// loopback port. A node with no map is given little time to fetch one.
async fn serve(pki: &Pki, mem: &Arc<Mem>, held: &Arc<Held>) -> SocketAddr {
    let storage: Arc<dyn Storage> = Arc::clone(mem) as Arc<dyn Storage>;
    let service = StorageService::new(storage, Admission::Certified, config())
        .with_map(Arc::clone(held) as Arc<dyn MapSource>)
        .with_refetch_wait(Duration::from_millis(50));
    serve_routes(service.routes(), Some(pki.server())).await
}

fn refused_as_denied(answer: &Result<(), Status>) -> Option<&Status> {
    answer
        .as_ref()
        .err()
        .filter(|status| status.code() == Code::PermissionDenied)
}

/// Catches a deny list that some method does not apply, or applies before the role: for
/// each of the three roles, one certificate the map lists (by the hash of its public key)
/// and one it does not, each calling every method over mutual TLS, `Segments` and the copy
/// a storage node sends another among them. The listed certificate is refused
/// PERMISSION_DENIED saying `denied: spki-sha256 <hash>` on every method its role may
/// call, and `may not call` (the role judged first) on the rest; the other certificate is
/// refused only on the methods its role may not call. A refused call reaches no store, and
/// one that is admitted reaches the generation check (the caller carries none, so the node
/// that holds generation 8 answers STALE_MAP).
///
/// Mutants seen red: `guard` passing no list (the role check alone); the `Segments`
/// handlers calling `Admission::guard` with no list; `Admission::guard_leaf` judging the
/// list before the role.
#[tokio::test]
async fn a_listed_certificate_is_refused_on_every_method_and_the_role_comes_first() {
    let pki = Pki::new();
    let mem = Arc::new(Mem::default());
    let held = Held::at(7);
    let addr = serve(&pki, &mem, &held).await;
    let mut listed = Vec::new();
    let mut unlisted = Vec::new();
    for (at, role) in (0x10_u64..).zip(PeerRole::ALL) {
        let sans = |prefix: &str| vec![dns(&format!("{prefix}-{role}")), uri(role.name())];
        listed.push((role, pki.caller(sans("listed"), at)));
        unlisted.push((role, pki.caller(sans("unlisted"), at + 0x10)));
    }
    let list: Vec<DenyEntry> = listed
        .iter()
        .map(|(_, caller)| DenyEntry::SpkiSha256(caller.spki_sha256.clone()))
        .collect();
    held.set_denying(8, list);

    for (role, caller) in &listed {
        let channel = pki.connect(addr, caller).await;
        for (rpc, endpoint) in methods() {
            let before = reached(&mem);
            let answer = invoke(rpc, endpoint, &channel).await;
            let status = refused_as_denied(&answer)
                .unwrap_or_else(|| panic!("{role} listed, on {rpc}: {answer:?}"));
            if expected(endpoint).contains(&role.name()) {
                let want = format!("denied: spki-sha256 {}", caller.spki_sha256);
                assert_eq!(status.message(), want, "{role} on {rpc}");
            } else {
                assert!(
                    status.message().contains("may not call"),
                    "{role} on {rpc}: the role is judged first: {}",
                    status.message()
                );
            }
            assert_eq!(reached(&mem), before, "{role} on {rpc} reached the store");
        }
    }
    for (role, caller) in &unlisted {
        let channel = pki.connect(addr, caller).await;
        for (rpc, endpoint) in methods() {
            let answer = invoke(rpc, endpoint, &channel).await;
            let allowed = expected(endpoint).contains(&role.name());
            assert_eq!(
                refused_as_denied(&answer).is_none(),
                allowed,
                "{role} unlisted, on {rpc}: {answer:?}"
            );
        }
        let generation = call(Endpoint::FindMissing, &channel)
            .await
            .expect_err("a caller with no generation is behind the node's");
        assert_eq!(generation.code(), Code::FailedPrecondition, "{role}");
    }
}

/// Catches an entry kind that is not applied, or applied too widely: a certificate listed
/// by its serial and one listed by a node id (the DNS name it carries) are refused on a
/// method every role may call and on one only a daemon may call, saying the entry; a
/// certificate whose only name is an IP address spelled like a listed node, and one that
/// no entry names, are not refused.
///
/// Mutants seen red: `node` entries matched against nothing; a serial compared with the
/// certificate's key hash.
#[tokio::test]
async fn a_serial_and_a_node_id_deny_as_a_key_hash_does() {
    let pki = Pki::new();
    let mem = Arc::new(Mem::default());
    let held = Held::at(1);
    let addr = serve(&pki, &mem, &held).await;
    let daemon = |names: Vec<SanType>, serial: u64| {
        pki.caller([names, vec![uri("daemon")]].concat(), serial)
    };
    let by_serial = daemon(vec![dns("serial-host")], 0xa1b);
    let by_node = daemon(vec![dns("mac-07")], 0xb2c);
    let by_ip = daemon(vec![ip("192.0.2.7")], 0xc3d);
    let bystander = daemon(vec![dns("mac-08")], 0xd4e);
    assert_eq!(by_serial.serial, "a1b");
    held.set_denying(
        2,
        vec![
            DenyEntry::Serial(by_serial.serial.clone()),
            DenyEntry::Node("mac-07".to_owned()),
            DenyEntry::Node("192.0.2.7".to_owned()),
        ],
    );
    for (caller, entry) in [(&by_serial, "serial a1b"), (&by_node, "node mac-07")] {
        let channel = pki.connect(addr, caller).await;
        for endpoint in [Endpoint::FindMissing, Endpoint::Segments] {
            let answer = call(endpoint, &channel).await;
            let status = refused_as_denied(&answer)
                .unwrap_or_else(|| panic!("{entry} on {endpoint:?}: {answer:?}"));
            assert_eq!(status.message(), format!("denied: {entry}"));
        }
    }
    for caller in [&by_ip, &bystander] {
        let channel = pki.connect(addr, caller).await;
        let answer = call(Endpoint::FindMissing, &channel).await;
        assert!(refused_as_denied(&answer).is_none(), "{answer:?}");
    }
}

/// Catches an entry that waits for anything but the node's map: one connection stays open
/// while the node's map changes. The call before the map lists the certificate is admitted,
/// the first call after it is refused (the connection is not reopened, the certificate is
/// not read again), and once a later generation drops the entry the same connection is
/// admitted again.
///
/// Mutant seen red: the list read once, when the service is built.
#[tokio::test]
async fn an_entry_counts_from_the_first_call_after_the_node_holds_its_generation() {
    let pki = Pki::new();
    let mem = Arc::new(Mem::default());
    let held = Held::at(1);
    let addr = serve(&pki, &mem, &held).await;
    let caller = pki.caller(vec![dns("node-a"), uri("frontend")], 0x77);
    let channel = pki.connect(addr, &caller).await;
    let entry = DenyEntry::SpkiSha256(caller.spki_sha256.clone());

    let before = call(Endpoint::FindMissing, &channel).await;
    assert!(refused_as_denied(&before).is_none(), "{before:?}");
    held.set_denying(2, vec![entry]);
    let during = call(Endpoint::FindMissing, &channel).await;
    assert!(refused_as_denied(&during).is_some(), "{during:?}");
    held.set_denying(3, Vec::new());
    let after = call(Endpoint::FindMissing, &channel).await;
    assert!(refused_as_denied(&after).is_none(), "{after:?}");
}

/// Catches a node that refuses what it has no list for: a node that has no map yet (it
/// answers UNAVAILABLE, having none to judge by, and not PERMISSION_DENIED), and a
/// plain-text node, which has no certificate to name whatever its map lists (it admits the
/// call and reaches the generation check).
///
/// Mutants seen red: a missing map read as a list that denies every caller; an empty
/// certificate read as one an entry names.
#[tokio::test]
async fn a_node_with_no_map_or_no_certificate_to_read_denies_nothing() {
    let pki = Pki::new();
    let mem = Arc::new(Mem::default());
    let held = Held::none();
    let addr = serve(&pki, &mem, &held).await;
    let caller = pki.caller(vec![dns("node-a"), uri("frontend")], 0x77);
    let channel = pki.connect(addr, &caller).await;
    let answer = call(Endpoint::FindMissing, &channel).await;
    assert_eq!(
        answer.expect_err("no map yet").code(),
        Code::Unavailable,
        "a node with no map judges nothing by it"
    );

    let listed = Held::at(4);
    listed.set_denying(5, vec![DenyEntry::Node("node-a".to_owned())]);
    let storage: Arc<dyn Storage> = Arc::clone(&mem) as Arc<dyn Storage>;
    let plain = StorageService::new(storage, Admission::Open, config())
        .with_map(Arc::clone(&listed) as Arc<dyn MapSource>);
    let plain = connect(serve_routes(plain.routes(), None).await).await;
    let answer = call(Endpoint::FindMissing, &plain).await;
    assert_eq!(
        answer.expect_err("behind the node's generation").code(),
        Code::FailedPrecondition
    );
}
