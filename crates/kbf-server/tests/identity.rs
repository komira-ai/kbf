//! The node binding and the deny list of the worker listener (issue #79), without a
//! network: certificates are made here and handed over as DER.

#[path = "support/hooked.rs"]
mod hooked;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use hooked::Hooked;
use kbf_map::DenyEntry;
use kbf_server::identity::{
    DENY_SNAPSHOT_SWEEPS, Denied, DenyList, DenyListError, PeerCert, Peers,
};
use kbf_server::members::{Members, MembershipConfig};
use kbf_state::{Cond, Store, Txn};
use rcgen::{CertificateParams, DnType, KeyPair, SanType, SerialNumber};
use tonic::Code;

/// A self-signed certificate with these subjectAltNames, common name and serial.
fn cert(sans: Vec<SanType>, cn: &str, serial: &[u8]) -> Vec<u8> {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
    params.subject_alt_names = sans;
    params.distinguished_name.push(DnType::CommonName, cn);
    params.serial_number = Some(SerialNumber::from(serial.to_vec()));
    let key = KeyPair::generate().expect("key");
    params.self_signed(&key).expect("sign").der().to_vec()
}

fn dns(name: &str) -> SanType {
    SanType::DnsName(name.try_into().expect("an IA5 name"))
}

/// The role URI the certificate table writes, spelled out here.
fn role(name: &str) -> SanType {
    SanType::URI(
        format!("urn:kbf:role:{name}")
            .try_into()
            .expect("an IA5 name"),
    )
}

fn peer(sans: Vec<SanType>) -> PeerCert {
    PeerCert::from_der(&cert(sans, "ignored", &[1])).expect("a certificate")
}

/// A directory of its own, removed when it drops, and `name` in it (not made).
fn scratch(name: &str) -> (kbf_testkit::dir::TestDir, PathBuf) {
    let dir = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "kbf-server-identity");
    let path = dir.join(name);
    (dir, path)
}

/// Catches: a node id taken from the common name (a certificate with CN `node-b` and
/// no DNS name would pass as `node-b`), a certificate naming several nodes accepted for
/// any of them, an IP or other subjectAltName read as a node, and a mismatch accepted.
#[test]
fn a_certificate_names_exactly_one_node_by_dns_name() {
    let ip = SanType::IpAddress([10, 0, 0, 1].into());
    let a = peer(vec![ip.clone(), dns("node-a")]);
    assert!(a.names("node-a").is_ok());
    let other = a
        .names("node-b")
        .expect_err("node-a's certificate as node-b");
    assert_eq!(other.code(), Code::PermissionDenied);
    assert!(
        other.message().contains("\"node-a\""),
        "{}",
        other.message()
    );
    assert_eq!(
        a.names("NODE-A").expect_err("compared exactly").code(),
        Code::PermissionDenied
    );

    let cn_only = PeerCert::from_der(&cert(vec![], "node-b", &[1])).expect("certificate");
    let two = peer(vec![dns("node-a"), dns("node-b")]);
    let ip_only = peer(vec![ip]);
    for (bad, count) in [(cn_only, 0), (two, 2), (ip_only, 0)] {
        for node in ["node-a", "node-b"] {
            let refused = bad.names(node).expect_err("not exactly one DNS name");
            assert_eq!(refused.code(), Code::PermissionDenied);
            assert!(refused.message().ends_with(&format!("names {count}")));
        }
    }
}

/// Catches: a serial compared in a spelling the deny list does not use (the DER sign
/// byte, or upper case), and a public-key hash of the wrong bytes.
#[test]
fn serial_and_public_key_are_spelled_as_the_deny_list_spells_them() {
    // 0x8f has its high bit set, so DER prefixes a zero byte; the list never writes it.
    let der = cert(vec![dns("node-a")], "node-a", &[0x8f, 0x0a]);
    let peer = PeerCert::from_der(&der).expect("certificate");
    assert_eq!(peer.serial(), "8f0a");
    let (_, parsed) = x509_parser::parse_x509_certificate(&der).expect("parse");
    let spki = {
        use sha2::Digest;
        sha2::Sha256::digest(parsed.public_key().raw)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<String>()
    };
    assert_eq!(peer.spki_sha256(), spki);
    assert_eq!(peer.spki_sha256().len(), 64);

    let zero = PeerCert::from_der(&cert(vec![], "z", &[0])).expect("certificate");
    assert_eq!(zero.serial(), "0");
}

/// Catches: bytes that are not a certificate taken for one with no names.
#[test]
fn garbage_is_not_a_certificate() {
    let error = PeerCert::from_der(b"not a certificate").expect_err("garbage");
    assert!(error.contains("does not parse"), "{error}");
}

/// Catches: entries silently dropped or misread (a typo would leave a certificate
/// trusted), comments and blank lines refused, and spellings of the same serial that
/// do not match.
#[test]
fn the_deny_list_parses_every_entry_or_names_the_bad_line() {
    let denied = Denied::parse(
        "# retired nodes\n\
         \n\
         serial 00:8F:0A   # leading zeros, colons and case are spelling only\n\
         spki-sha256 ABCDEF0123456789abcdef0123456789abcdef0123456789abcdef0123456789\n\
         node mac-07\n   \n",
    )
    .expect("a good list");
    let a = PeerCert::from_der(&cert(vec![dns("node-a")], "node-a", &[0x8f, 0x0a])).expect("c");
    assert_eq!(
        denied.refuses(Some(&a), "node-a"),
        Some("certificate serial 8f0a".to_owned())
    );
    assert_eq!(
        denied.refuses(None, "mac-07"),
        Some("node mac-07".to_owned())
    );
    assert_eq!(denied.refuses(None, "mac-08"), None);
    assert_eq!(
        denied.refuses(None, "MAC-07"),
        Some("node mac-07".to_owned()),
        "a node id is compared up to ASCII case"
    );
    let other = peer(vec![dns("node-a")]);
    assert_eq!(denied.refuses(Some(&other), "node-a"), None);

    let by_key = Denied::parse(&format!(
        "spki-sha256 {}",
        other.spki_sha256().to_uppercase()
    ))
    .expect("a key entry");
    assert_eq!(
        by_key.refuses(Some(&other), "node-a"),
        Some(format!("public key {}", other.spki_sha256()))
    );
    assert_eq!(by_key.refuses(Some(&a), "node-a"), None);
    assert_eq!(Denied::parse(""), Ok(Denied::default()));

    for (text, line, why) in [
        ("serial", 1, "serial needs a value"),
        ("\nserial 0a extra", 2, "one kind and one value"),
        ("serial :", 1, "a serial is hex digits"),
        ("serial 0x1", 1, "a serial is hex digits"),
        ("spki-sha256 abcd", 1, "64 hex digits"),
        (
            &format!("spki-sha256 {}", "g".repeat(64)),
            1,
            "64 hex digits",
        ),
        ("fingerprint ab", 1, "unknown entry kind \"fingerprint\""),
    ] {
        let (at, message) = Denied::parse(text).expect_err(text);
        assert_eq!(at, line, "{text}");
        assert!(message.contains(why), "{text}: {message}");
    }
}

/// Catches: a storage node's certificate judged by one name only, or by none. A storage
/// certificate carries several names, and a `node` entry denies it when any DNS name in
/// it is listed, up to ASCII case (an IP name is no node id, and a listed name that is only
/// a prefix is not that node); its serial and key are judged as for a daemon, a listed node
/// being reported before them.
///
/// Catches: only the first DNS name looked at; names compared with `==`.
#[test]
fn a_storage_certificate_is_denied_by_any_of_its_names_its_serial_or_its_key() {
    let several = PeerCert::from_der(&cert(
        vec![
            SanType::IpAddress([192, 0, 2, 1].into()),
            dns("node-a"),
            dns("node-a.farm.example"),
            role("storage"),
        ],
        "cn",
        &[0x0a],
    ))
    .expect("certificate");
    let denied = |text: &str| Denied::parse(text).expect("a list");
    assert_eq!(
        denied("node node-a.farm.example").refuses_cert(&several),
        Some("node node-a.farm.example".to_owned())
    );
    assert_eq!(
        denied("node node-a").refuses_cert(&several),
        Some("node node-a".to_owned())
    );
    for not_listed in ["node node-b", "node node-", "node 192.0.2.1", ""] {
        assert_eq!(
            denied(not_listed).refuses_cert(&several),
            None,
            "{not_listed}"
        );
    }
    assert_eq!(
        denied("node NODE-A.Farm.Example").refuses_cert(&several),
        Some("node NODE-A.Farm.Example".to_owned()),
        "up to ASCII case, and the entry is reported as listed"
    );
    assert_eq!(
        denied("serial 0A").refuses_cert(&several),
        Some("certificate serial a".to_owned())
    );
    let key = format!("spki-sha256 {}", several.spki_sha256());
    assert_eq!(
        denied(&key).refuses_cert(&several),
        Some(format!("public key {}", several.spki_sha256()))
    );
    assert_eq!(
        denied(&format!("{key}\nserial 0a\nnode node-a")).refuses_cert(&several),
        Some("node node-a".to_owned())
    );
}

/// Catches: a server that starts with a deny list it cannot read or parse (the first
/// daemon would find out instead of the operator).
#[test]
fn a_bad_deny_list_is_refused_at_open() {
    let (_missing_dir, missing) = scratch("missing.deny");
    let _ = std::fs::remove_file(&missing);
    assert!(matches!(
        DenyList::open(&missing),
        Err(DenyListError::Read { path, .. }) if path == missing
    ));
    let (_bad_dir, bad) = scratch("bad.deny");
    std::fs::write(&bad, "node a\nserial zz\n").expect("write");
    let error = DenyList::open(&bad).expect_err("a bad line");
    assert!(matches!(error, DenyListError::Parse { line: 2, .. }));
    assert!(
        error
            .to_string()
            .ends_with("bad.deny:2: a serial is hex digits, colons allowed: \"zz\""),
        "{error}"
    );
}

/// Catches: `certificate_entries` (the count behind the warning of `Args::proxy`) reading a
/// file that changed after `open` as though it were still good: a list removed or rewritten
/// as garbage is an error naming the path (and the line), not a count of zero.
///
/// Catches: the read error mapped to an empty list; the parse result unwrapped to
/// its default (`unwrap_or_default`).
#[test]
fn a_list_that_goes_bad_after_open_is_an_error_when_its_certificate_entries_are_counted() {
    let (_path_dir, path) = scratch("counted.deny");
    std::fs::write(
        &path,
        "node a\nserial 0a\nspki-sha256 {}\n".replace("{}", &"0".repeat(64)),
    )
    .expect("write");
    let list = DenyList::open(&path).expect("open");
    assert_eq!(list.certificate_entries().expect("two"), 2);

    std::fs::write(&path, "node a\nserial zz\n").expect("garbage");
    let error = list.certificate_entries().expect_err("garbage");
    assert!(
        matches!(error, DenyListError::Parse { line: 2, .. }),
        "{error}"
    );
    assert!(error.to_string().contains("counted.deny:2"), "{error}");

    std::fs::remove_file(&path).expect("remove");
    let error = list.certificate_entries().expect_err("removed");
    assert!(
        matches!(&error, DenyListError::Read { path: p, .. } if *p == path),
        "{error}"
    );
}

/// Catches: plain text that asks for a certificate (every test cell would be refused),
/// mutual TLS that lets a stream through without one or with one it cannot read, and
/// a certificate that is read but not returned for the stream.
#[test]
fn a_certified_listener_needs_a_readable_certificate() {
    let der = cert(vec![dns("node-a"), role("daemon")], "node-a", &[7]);
    assert_eq!(
        Peers::Unauthenticated.peer(Some(&der)).expect("plain"),
        None
    );
    assert_eq!(Peers::Unauthenticated.peer(None).expect("plain"), None);
    let certified = Peers::Certified {
        deny_list: None,
        denied_rows: None,
    };
    let none = certified.peer(None).expect_err("no certificate");
    assert_eq!(none.code(), Code::Unauthenticated);
    let garbage = certified.peer(Some(b"junk")).expect_err("junk");
    assert_eq!(garbage.code(), Code::Unauthenticated);
    let peer = certified
        .peer(Some(&der))
        .expect("a certificate")
        .expect("some");
    assert_eq!(peer.serial(), "7");
}

/// Catches (issue #79): the node check skipped at admission, the deny list read only
/// when the server starts (an entry added later must refuse the next check), and a
/// deny list that breaks after start letting every stream through (it must fail
/// closed).
#[tokio::test]
async fn admission_binds_the_node_and_reads_the_deny_list_each_time() {
    let a = peer(vec![dns("node-a")]);
    assert!(Peers::Unauthenticated.admit(None, "anything").await.is_ok());
    let without_list = Peers::Certified {
        deny_list: None,
        denied_rows: None,
    };
    assert!(without_list.admit(Some(&a), "node-a").await.is_ok());
    assert_eq!(
        without_list
            .admit(Some(&a), "node-b")
            .await
            .expect_err("bound")
            .code(),
        Code::PermissionDenied
    );

    let (_path_dir, path) = scratch("admit.deny");
    std::fs::write(&path, "# nothing denied yet\n").expect("write");
    let with_list = Peers::Certified {
        deny_list: Some(DenyList::open(&path).expect("open")),
        denied_rows: None,
    };
    assert!(with_list.admit(Some(&a), "node-a").await.is_ok());
    assert_eq!(
        with_list
            .admit(Some(&a), "node-b")
            .await
            .expect_err("bound")
            .code(),
        Code::PermissionDenied
    );

    std::fs::write(&path, format!("serial {}\n", a.serial())).expect("deny node-a's cert");
    let denied = with_list
        .admit(Some(&a), "node-a")
        .await
        .expect_err("denied");
    assert_eq!(denied.code(), Code::PermissionDenied);
    assert!(
        denied.message().contains("certificate serial"),
        "{}",
        denied.message()
    );

    std::fs::write(&path, "serial nothex\n").expect("break the list");
    let broken = with_list
        .admit(Some(&a), "node-a")
        .await
        .expect_err("fail closed");
    assert_eq!(broken.code(), Code::Unavailable);
    std::fs::remove_file(&path).expect("remove the list");
    let gone = with_list
        .admit(Some(&a), "node-a")
        .await
        .expect_err("fail closed");
    assert_eq!(gone.code(), Code::Unavailable);
}

/// Catches: a deny list cached by its path or its size alone, so that a new list of
/// the same size renamed over it (the documented way to replace it) is not seen, and a
/// cache that keeps an old read once the file breaks.
#[tokio::test]
async fn a_list_renamed_over_the_old_one_is_read_even_at_the_same_size() {
    let (_path_dir, path) = scratch("renamed.deny");
    std::fs::write(&path, "node aaaa\n").expect("write");
    let list = DenyList::open(&path).expect("open");
    let first = list.current().await.expect("read");
    assert!(first.refuses(None, "aaaa").is_some());
    let unchanged = list.current().await.expect("read");
    assert_eq!(unchanged, first);

    let (_next_dir, next) = scratch("renamed.deny.next");
    std::fs::write(&next, "node bbbb\n").expect("write");
    std::fs::rename(&next, &path).expect("rename");
    let second = list.current().await.expect("read");
    assert_eq!(
        second.refuses(None, "aaaa"),
        None,
        "the old list is still used"
    );
    assert!(second.refuses(None, "bbbb").is_some());

    // Clones share the cache, and see the file break.
    let clone = list.clone();
    std::fs::write(&path, "node\n").expect("break");
    assert!(matches!(
        clone.current().await,
        Err(DenyListError::Parse { line: 1, .. })
    ));
    assert!(list.current().await.is_err());
}

const TTL: Duration = Duration::from_secs(60);
/// One membership sweep: the sweeper refreshes every beat.
const BEAT: Duration = Duration::from_secs(20);

/// Members over an empty store, refreshed once (as the sweeper does at start), and the
/// store, for the tests of the `deny/` rows.
fn members() -> (Arc<Hooked>, Arc<Members>) {
    let store = Hooked::new();
    let config = MembershipConfig {
        ttl: TTL,
        dwell: TTL,
        replication: 2,
    };
    let members = Arc::new(Members::new(Arc::clone(&store) as Arc<dyn Store>, config));
    members.refresh().expect("the first refresh");
    (store, members)
}

fn rows_only(members: &Arc<Members>) -> Peers {
    Peers::Certified {
        deny_list: None,
        denied_rows: None,
    }
    .with_denied_rows(Some(Arc::clone(members)))
}

/// A revoked serial is refused and an unrelated node is admitted, from the store's rows
/// alone (no file); the same node under another serial is not refused by that serial; a row
/// written and refreshed (as a revoke and `POST /v1/denied` do before they answer) is seen at
/// the next check, and one taken off readmits.
///
/// Catches: rows read once at start, rows ignored beside a deny file, and a serial row
/// that names the node and not the certificate.
#[tokio::test]
async fn the_stores_deny_rows_refuse_a_serial_and_a_node_at_every_check() {
    let (_store, members) = members();
    let peers = rows_only(&members);
    let revoked = PeerCert::from_der(&cert(vec![dns("node-a")], "x", &[0x0a, 0x1b])).expect("cert");
    let reissued =
        PeerCert::from_der(&cert(vec![dns("node-a")], "x", &[0x0a, 0x1c])).expect("cert");
    let other = PeerCert::from_der(&cert(vec![dns("node-b")], "x", &[0x0b])).expect("cert");
    assert!(peers.admit(Some(&revoked), "node-a").await.is_ok());

    let serial: DenyEntry = format!("serial {}", revoked.serial())
        .parse()
        .expect("entry");
    members.deny(&serial, "leaked").expect("deny");
    members.refresh().expect("refresh");
    let refused = peers
        .admit(Some(&revoked), "node-a")
        .await
        .expect_err("the serial is revoked");
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert!(
        refused.message().contains("serial"),
        "{}",
        refused.message()
    );
    assert!(peers.admit(Some(&reissued), "node-a").await.is_ok());
    assert!(peers.admit(Some(&other), "node-b").await.is_ok());

    assert!(members.allow(&serial).expect("allow"));
    members.refresh().expect("refresh");
    assert!(peers.admit(Some(&revoked), "node-a").await.is_ok());

    let node: DenyEntry = "node node-a".parse().expect("entry");
    members.deny(&node, "revoked").expect("deny");
    members.refresh().expect("refresh");
    for who in [&revoked, &reissued] {
        let refused = peers.admit(Some(who), "node-a").await.expect_err("node");
        assert_eq!(refused.code(), Code::PermissionDenied);
    }
    assert!(peers.admit(Some(&other), "node-b").await.is_ok());
}

/// A node revoked behind the farm's proxy, where there is no certificate and the proxy
/// forwards a node id, is refused by its `node` row.
#[tokio::test]
async fn a_proxied_node_is_refused_by_the_stores_node_row() {
    let (_store, members) = members();
    let peers = rows_only(&members);
    let proxied = PeerCert::proxied("node-a", &[]);
    let other = PeerCert::proxied("node-b", &[]);
    assert!(peers.admit(Some(&proxied), "node-a").await.is_ok());
    members
        .deny(&"node node-a".parse().expect("entry"), "revoked")
        .expect("deny");
    members.refresh().expect("refresh");
    let refused = peers
        .admit(Some(&proxied), "node-a")
        .await
        .expect_err("revoked");
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert!(peers.admit(Some(&other), "node-b").await.is_ok());
}

/// The file and the rows are both applied: an entry in either refuses, and neither
/// shadows the other.
///
/// Catches: the rows replacing the file's entries (or the file replacing the rows).
#[tokio::test]
async fn the_file_and_the_rows_both_refuse() {
    let (_store, members) = members();
    let (_dir, path) = scratch("both.deny");
    std::fs::write(&path, "node node-a\n").expect("write");
    let peers = Peers::Certified {
        deny_list: Some(DenyList::open(&path).expect("open")),
        denied_rows: None,
    }
    .with_denied_rows(Some(Arc::clone(&members)));
    members
        .deny(&"node node-b".parse().expect("entry"), "revoked")
        .expect("deny");
    members.refresh().expect("refresh");
    let a = peer(vec![dns("node-a")]);
    let b = peer(vec![dns("node-b")]);
    let c = peer(vec![dns("node-c")]);
    for (who, node) in [(&a, "node-a"), (&b, "node-b")] {
        let refused = peers.admit(Some(who), node).await.expect_err("denied");
        assert_eq!(refused.code(), Code::PermissionDenied, "{node}");
    }
    assert!(peers.admit(Some(&c), "node-c").await.is_ok());
}

/// Checks read no row of the store: after the refresh, many messages and blob calls cost
/// no read of `deny/`.
///
/// Catches: a store query (and a pooled connection) per heartbeat, result and blob call.
#[tokio::test]
async fn admitting_reads_no_deny_row_from_the_store() {
    let (store, members) = members();
    let peers = rows_only(&members);
    let a = peer(vec![dns("node-a")]);
    let reads = store.count("deny/");
    for _ in 0..50 {
        peers.admit(Some(&a), "node-a").await.expect("admitted");
    }
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    members.refresh().expect("refresh");
    assert_eq!(
        reads.load(Ordering::SeqCst),
        1,
        "a refresh reads the rows once"
    );
}

/// A refresh that fails keeps the last snapshot in use for three sweeps, still refusing
/// whom it refused; after that every caller is refused UNAVAILABLE, until a refresh reads the
/// rows again. A corrupt `deny/` row is such a failure; deleting it repairs.
///
/// Catches: one failed refresh ending every stream (no bounded staleness), a snapshot
/// trusted for ever, and a corrupt row that is guessed around.
#[tokio::test(start_paused = true)]
async fn a_failing_refresh_serves_the_last_snapshot_for_three_sweeps_and_then_refuses_all() {
    assert_eq!(DENY_SNAPSHOT_SWEEPS, 3);
    let (store, members) = members();
    let peers = rows_only(&members);
    let (a, b) = (peer(vec![dns("node-a")]), peer(vec![dns("node-b")]));
    members
        .deny(&"node node-a".parse().expect("entry"), "revoked")
        .expect("deny");
    members.refresh().expect("refresh");

    let written = store
        .txn(Txn::new().put("deny/bogus/x", b"?".to_vec(), Cond::Any))
        .expect("write the row");
    assert!(written.is_ok());
    members
        .refresh()
        .expect_err("a corrupt row fails the refresh");

    tokio::time::advance(BEAT * 3 - Duration::from_secs(1)).await;
    members.refresh().expect_err("still corrupt");
    let refused = peers
        .admit(Some(&a), "node-a")
        .await
        .expect_err("still refused");
    assert_eq!(refused.code(), Code::PermissionDenied);
    assert!(
        peers.admit(Some(&b), "node-b").await.is_ok(),
        "within the bound"
    );

    tokio::time::advance(Duration::from_secs(2)).await;
    for (who, node) in [(&a, "node-a"), (&b, "node-b")] {
        let refused = peers.admit(Some(who), node).await.expect_err("too old");
        assert_eq!(refused.code(), Code::Unavailable, "{node}");
        assert!(
            !refused.message().contains("bogus"),
            "{}",
            refused.message()
        );
    }

    let gone = store
        .txn(Txn::new().delete("deny/bogus/x", Cond::Any))
        .expect("delete the row");
    assert!(gone.is_ok());
    members.refresh().expect("repaired");
    assert!(peers.admit(Some(&b), "node-b").await.is_ok());
    assert_eq!(
        peers
            .admit(Some(&a), "node-a")
            .await
            .expect_err("revoked")
            .code(),
        Code::PermissionDenied
    );
}

/// Before any refresh has read the rows nobody is admitted.
///
/// Catches: a listener that admits on an empty list before its first refresh.
#[tokio::test]
async fn before_the_first_refresh_nobody_is_admitted() {
    let store = Hooked::new();
    let config = MembershipConfig {
        ttl: TTL,
        dwell: TTL,
        replication: 2,
    };
    let members = Arc::new(Members::new(store as Arc<dyn Store>, config));
    let refused = rows_only(&members)
        .admit(Some(&peer(vec![dns("node-a")])), "node-a")
        .await
        .expect_err("no snapshot");
    assert_eq!(refused.code(), Code::Unavailable);
}
