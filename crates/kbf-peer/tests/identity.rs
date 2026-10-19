//! What a deny entry is compared with (`identity_of`) and which entry of a list names a
//! certificate (`PeerIdentity::denied_by`).

use std::net::IpAddr;

use kbf_map::DenyEntry;
use kbf_peer::{ROLE_URI_PREFIX, RoleError, identity_of};
use rcgen::{CertificateParams, DnType, KeyPair, PublicKeyData, SanType, SerialNumber};
use sha2::{Digest, Sha256};

fn dns(name: &str) -> SanType {
    SanType::DnsName(name.try_into().expect("an IA5 name"))
}

fn ip(text: &str) -> SanType {
    SanType::IpAddress(text.parse::<IpAddr>().expect("an IP address"))
}

fn role(name: &str) -> SanType {
    SanType::URI(
        format!("{ROLE_URI_PREFIX}{name}")
            .try_into()
            .expect("an IA5 name"),
    )
}

/// A self-signed certificate with these subjectAltNames and serial, as DER, and the
/// SHA-256 of its public key's SubjectPublicKeyInfo, which rcgen builds independently of
/// the reader under test.
fn cert(sans: Vec<SanType>, serial: &[u8]) -> (Vec<u8>, String) {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
    params.subject_alt_names = sans;
    params.serial_number = Some(SerialNumber::from_slice(serial));
    let key = KeyPair::generate().expect("key");
    let hash = Sha256::digest(key.subject_public_key_info())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    (params.self_signed(&key).expect("sign").der().to_vec(), hash)
}

fn serial(text: &str) -> DenyEntry {
    DenyEntry::Serial(text.to_owned())
}

fn node(text: &str) -> DenyEntry {
    DenyEntry::Node(text.to_owned())
}

/// Catches an identity read wrongly: the serial must be the certificate's own, in the
/// spelling a deny entry takes (lower-case hex, leading zeros dropped, `0` for zero), the
/// key hash must be SHA-256 of the SubjectPublicKeyInfo (not of the key bits, the
/// certificate or the subject), and the names are the ones `names_of` reads.
#[test]
fn the_identity_is_the_serial_the_key_hash_and_the_names() {
    for (raw, spelled) in [
        (&[0x0a_u8, 0x1b][..], "a1b"),
        (&[0x8f, 0x0a][..], "8f0a"),
        (&[0x01][..], "1"),
        (&[0x00][..], "0"),
    ] {
        let (der, hash) = cert(vec![dns("node-a"), ip("192.0.2.1"), role("storage")], raw);
        let identity = identity_of(&der).expect("a certificate");
        assert_eq!(identity.serial(), spelled, "{raw:?}");
        assert_eq!(identity.spki_sha256(), hash);
        assert_eq!(identity.names().to_string(), "node-a, 192.0.2.1");
        assert_eq!(identity.names().dns(), ["node-a".to_owned()]);
    }
}

/// Catches bytes that are no certificate read as one (an identity with no names, which no
/// entry would name and so no list would deny).
#[test]
fn bytes_that_are_no_certificate_have_no_identity() {
    let error = identity_of(b"not a certificate").expect_err("garbage");
    assert!(matches!(error, RoleError::NotACertificate(_)), "{error}");
}

/// Catches an entry that does not name the certificate it spells, or one that names
/// another: each kind alone names it (serial, key hash, DNS name); an entry that is a
/// near miss does not (another serial that differs in one digit, another key, a node
/// that is a prefix); an empty list names nothing; and with several entries the first
/// that names it is the one returned (the message an operator reads).
#[test]
fn an_entry_names_a_certificate_by_serial_key_or_dns_name() {
    let (der, hash) = cert(vec![dns("node-a")], &[0x0a, 0x1b]);
    let identity = identity_of(&der).expect("a certificate");
    let key = DenyEntry::SpkiSha256(hash);
    let other_key = DenyEntry::SpkiSha256("0".repeat(64));

    assert_eq!(identity.denied_by(&[]), None);
    assert_eq!(
        identity.denied_by(&[serial("a1b")]),
        Some(&serial("a1b")),
        "the serial"
    );
    assert_eq!(
        identity.denied_by(std::slice::from_ref(&key)),
        Some(&key),
        "the key"
    );
    assert_eq!(
        identity.denied_by(&[node("node-a")]),
        Some(&node("node-a")),
        "the DNS name"
    );
    let near = [
        serial("a1c"),
        serial("1b"),
        serial("0a1b"),
        other_key,
        node("node-"),
        node("node-a2"),
        node("a"),
    ];
    assert_eq!(identity.denied_by(&near), None);
    let several = [serial("a1c"), node("node-a"), key, serial("a1b")];
    assert_eq!(identity.denied_by(&several), Some(&node("node-a")));
}

/// Catches a `node` entry that names a certificate by anything but one of its DNS names, up
/// to ASCII case (as `PeerNames::covers` and DNS compare names, so a host typed as the
/// membership list shows it is not ignored; the same rule as the deny list file's `node`
/// lines): not an IP name written the same, not the role URI, not the common name, not a
/// name that is a prefix of it or has a trailing dot or a port.
///
/// Mutant seen red: the names compared with `==`.
#[test]
fn a_node_entry_names_a_dns_name_up_to_case_and_nothing_else() {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
    params
        .distinguished_name
        .push(DnType::CommonName, "common-name");
    params.subject_alt_names = vec![
        dns("Node-A"),
        ip("192.0.2.1"),
        role("storage"),
        SanType::Rfc822Name("ops@node-c".try_into().expect("an IA5 name")),
    ];
    let cert = params
        .self_signed(&KeyPair::generate().expect("key"))
        .expect("sign");
    let identity = identity_of(cert.der()).expect("a certificate");
    for not_named in [
        "192.0.2.1",
        "storage",
        "urn:kbf:role:storage",
        "common-name",
        "ops@node-c",
        "node-c",
        "node-",
        "node-a.",
        "node-a:8982",
    ] {
        assert_eq!(identity.denied_by(&[node(not_named)]), None, "{not_named}");
    }
    for named in ["Node-A", "node-a", "NODE-A"] {
        assert_eq!(
            identity.denied_by(&[node(named)]),
            Some(&node(named)),
            "{named}"
        );
    }
}

/// Catches a certificate judged by one of its names only (the first it lists): a storage
/// certificate carries a short name and a fully qualified one, in either order, and an entry
/// for either denies it, as does one in another case; an entry for the domain alone, or for
/// another host, does not. A wildcard name is denied by the entry that spells it and by no
/// host it would cover.
///
/// Mutants seen red: only the first DNS name looked at; a wildcard name skipped.
#[test]
fn a_node_entry_names_a_certificate_by_any_of_its_dns_names() {
    for sans in [
        vec![dns("node-a"), dns("node-a.farm.example"), ip("192.0.2.1")],
        vec![ip("192.0.2.1"), dns("node-a.farm.example"), dns("node-a")],
    ] {
        let (der, _) = cert(sans, &[0x01]);
        let identity = identity_of(&der).expect("a certificate");
        for listed in ["node-a", "node-a.farm.example", "NODE-A.Farm.Example"] {
            assert_eq!(
                identity.denied_by(&[node(listed)]),
                Some(&node(listed)),
                "{listed}"
            );
        }
        let others = [node("farm.example"), node("node-b.farm.example")];
        assert_eq!(identity.denied_by(&others), None);
    }

    let (der, _) = cert(vec![dns("*.farm.example")], &[0x02]);
    let identity = identity_of(&der).expect("a certificate");
    assert_eq!(
        identity.denied_by(&[node("*.farm.example")]),
        Some(&node("*.farm.example"))
    );
    assert_eq!(identity.denied_by(&[node("node-a.farm.example")]), None);
}

/// Catches a proxied identity that is more than the one node a proxy forwarded: it must
/// name that node (and only it, up to case) and no IP address, and no `serial` or
/// `spki-sha256` entry may name it, since the proxy forwards neither.
///
/// Mutants seen red: the node left out of the names; an IP host covered; a serial entry
/// that names the empty serial.
#[test]
fn a_proxied_identity_is_one_node_and_no_serial_or_key() {
    let identity = kbf_peer::PeerIdentity::proxied("node-a");
    assert_eq!(identity.names().dns(), ["node-a"]);
    assert!(identity.names().covers("NODE-A"));
    assert!(!identity.names().covers("node-b"));
    assert!(!identity.names().covers("192.0.2.1"));
    assert_eq!(identity.denied_by(&[node("node-a")]), Some(&node("node-a")));
    let by_key = [
        serial("0"),
        serial("1"),
        DenyEntry::SpkiSha256("0".repeat(64)),
    ];
    assert_eq!(identity.denied_by(&by_key), None);
}
