//! The certificate builder: the design's table, field by field, and what the real checks
//! of the TLS stack and of the node's client make of the result.

mod support;

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use kbf_ca::{
    Authority, CLOCK_FLOOR_UNIX, CaError, Entropy, Issued, NodeCert, Policy, PublicKey,
    SystemEntropy, UpdaterCert, parse_csr,
};
use kbf_proto::kbf::enroll::v1::IssuedCert;
use kbf_types::{HostRole, NodeId, Role};
use rcgen::{
    BasicConstraints, CertificateParams, CustomExtension, DistinguishedName, DnType,
    ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose, SanType,
};
use rustls::RootCertStore;
use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::ServerCertVerifier;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::WebPkiClientVerifier;
use support::{
    External, KU_CRL_SIGN, KU_DIGITAL_SIGNATURE, KU_KEY_CERT_SIGN, NOW, Script, Stream, View,
    public_of, table_extension_oids, view,
};
use x509_parser::prelude::{FromDer, X509Certificate};

/// The roles of a node (a host's updater is not one).
const NODE_ROLES: [Role; 2] = [Role::Storage, Role::Daemon];

fn authority() -> Authority {
    Authority::create(External::new(), Policy::default(), NOW, &mut Stream(1)).expect("a root")
}

fn node(id: &str) -> NodeId {
    id.parse().expect("a node id")
}

fn key() -> PublicKey {
    public_of(&KeyPair::generate().expect("key"))
}

fn issue(ca: &Authority, id: &str, role: Role, key: &PublicKey, advertise: Option<&str>) -> Issued {
    ca.issue_node(
        &NodeCert {
            node_id: &node(id),
            role,
            key,
            advertise,
        },
        NOW,
        &mut Stream(9),
    )
    .expect("a certificate")
}

fn signed_by(der: &[u8], issuer_spki_of: &[u8]) -> bool {
    let (_, cert) = X509Certificate::from_der(der).expect("cert");
    let (_, issuer) = X509Certificate::from_der(issuer_spki_of).expect("issuer");
    cert.verify_signature(Some(issuer.public_key())).is_ok()
}

fn at(t: i64) -> UnixTime {
    UnixTime::since_unix_epoch(Duration::from_secs(u64::try_from(t).expect("positive")))
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn store(roots: &[&[u8]]) -> Arc<RootCertStore> {
    let mut store = RootCertStore::empty();
    for root in roots {
        store
            .add(CertificateDer::from(root.to_vec()))
            .expect("a root");
    }
    Arc::new(store)
}

fn as_server(roots: &[&[u8]], chain: &[&[u8]], name: &str) -> Result<(), rustls::Error> {
    let verifier = WebPkiServerVerifier::builder_with_provider(store(roots), provider())
        .build()
        .expect("verifier");
    let end = CertificateDer::from(chain[0].to_vec());
    let rest: Vec<CertificateDer<'_>> = chain[1..]
        .iter()
        .map(|c| CertificateDer::from(c.to_vec()))
        .collect();
    verifier
        .verify_server_cert(
            &end,
            &rest,
            &ServerName::try_from(name.to_owned()).expect("name"),
            &[],
            at(NOW),
        )
        .map(drop)
}

fn as_client(roots: &[&[u8]], chain: &[&[u8]]) -> Result<(), rustls::Error> {
    let verifier = WebPkiClientVerifier::builder_with_provider(store(roots), provider())
        .build()
        .expect("verifier");
    let end = CertificateDer::from(chain[0].to_vec());
    let rest: Vec<CertificateDer<'_>> = chain[1..]
        .iter()
        .map(|c| CertificateDer::from(c.to_vec()))
        .collect();
    verifier.verify_client_cert(&end, &rest, at(NOW)).map(drop)
}

fn expected(role: Role, id: &str) -> impl Fn(&View) {
    let id = id.to_owned();
    move |v: &View| {
        assert_eq!(v.uris, [format!("urn:kbf:role:{}", role.name())]);
        assert_eq!(v.dns.first().map(String::as_str), Some(id.as_str()));
        assert_eq!(v.cn, [id.as_str()]);
        assert_eq!(v.other_sans, 0);
        assert!(
            v.client_auth,
            "every node certificate is a client certificate"
        );
        assert_eq!(v.server_auth, role == Role::Storage, "only storage serves");
        assert_eq!(v.other_ekus, 0);
        assert_eq!(v.ca, Some((true, false, None)), "CA:FALSE, critical");
        assert_eq!(
            v.key_usage,
            Some((true, KU_DIGITAL_SIGNATURE)),
            "digitalSignature, critical"
        );
        assert_eq!(
            v.extension_oids,
            table_extension_oids(),
            "no extension the table lacks"
        );
    }
}

// ---- the root ------------------------------------------------------------------------

// Catches: a root that is not what the design decided: a self-signed CA with pathlen 0,
// keyCertSign and cRLSign, ten years, with identifiers, whose pin is the hash of its DER.
#[test]
fn the_root_is_a_self_signed_pathlen_zero_ca() {
    let ca = authority();
    let v = view(ca.root_der());
    assert_eq!(v.ca, Some((true, true, Some(0))));
    assert_eq!(v.key_usage, Some((true, KU_KEY_CERT_SIGN | KU_CRL_SIGN)));
    assert_eq!(v.not_before, NOW - 300);
    assert_eq!(v.not_after - v.not_before, 3650 * 86_400);
    assert_eq!(v.subject, v.issuer);
    assert!(v.subject_key_id.is_some());
    assert!(signed_by(ca.root_der(), ca.root_der()), "self-signed");
    use sha2::Digest as _;
    let hash: [u8; 32] = sha2::Sha256::digest(ca.root_der()).into();
    assert_eq!(ca.pin().as_bytes(), &hash);
    assert_eq!(ca.bundle().certs(), [ca.root_der().to_vec()]);
    assert_eq!(ca.policy(), Policy::default());
}

// Catches: two roots with one name: a rotation's old and new roots would be told apart only
// by their keys, which path building does not use to pick a candidate.
#[test]
fn two_roots_do_not_share_a_name() {
    assert_ne!(
        view(authority().root_der()).subject,
        view(authority().root_der()).subject
    );
}

// ---- the node certificate ------------------------------------------------------------

// Catches: any field of the table drifting: role URI, DNS name, EKU per role, usages,
// CA:FALSE, validity (now minus 5 minutes, 7 days), identifiers, the signature, and a
// leaf for a key other than the one the request proved.
#[test]
fn a_node_certificate_is_the_table_and_nothing_else() {
    let ca = authority();
    let root = view(ca.root_der());
    for role in NODE_ROLES {
        let key = key();
        let issued = issue(&ca, "node-1", role, &key, None);
        let v = view(&issued.der);
        expected(role, "node-1")(&v);
        assert_eq!(v.dns, ["node-1"]);
        assert!(v.ips.is_empty());
        assert_eq!(v.spki, key.spki(), "the key of the request");
        assert_eq!(v.not_before, NOW - 300);
        assert_eq!(v.not_after, v.not_before + 7 * 86_400);
        assert_eq!(
            (issued.not_before, issued.not_after),
            (v.not_before, v.not_after)
        );
        assert_eq!(issued.serial, v.serial_hex);
        assert!(v.subject_key_id.is_some());
        assert_eq!(
            v.authority_key_id, root.subject_key_id,
            "AKI is the root's SKI"
        );
        assert_eq!(v.issuer, root.subject);
        assert!(signed_by(&issued.der, ca.root_der()));
    }
}

// Catches: the CA copying anything from a request: a request for CA:TRUE, a wildcard, the
// frontend role, usages and a custom extension yields the same certificate as an innocent
// one would, for the role the token granted.
#[test]
fn a_hostile_request_gets_only_the_table() {
    let ca = authority();
    let key = KeyPair::generate().expect("key");
    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    params
        .distinguished_name
        .push(DnType::CommonName, "frontend");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.subject_alt_names = vec![
        SanType::DnsName("*.example.com".try_into().expect("ia5")),
        SanType::DnsName("farm.example.com".try_into().expect("ia5")),
        SanType::URI("urn:kbf:role:frontend".try_into().expect("ia5")),
    ];
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.extended_key_usages = vec![
        ExtendedKeyUsagePurpose::ServerAuth,
        ExtendedKeyUsagePurpose::CodeSigning,
    ];
    params.custom_extensions = vec![CustomExtension::from_oid_content(&[1, 2, 3, 4], vec![5])];
    let der = params
        .serialize_request(&key)
        .expect("csr")
        .der()
        .as_ref()
        .to_vec();
    let parsed = parse_csr(&der).expect("valid");
    for role in NODE_ROLES {
        let issued = issue(&ca, "n1", role, &parsed.key, None);
        let v = view(&issued.der);
        expected(role, "n1")(&v);
        assert_eq!(v.dns, ["n1"], "no name the request asked for");
        assert!(v.ips.is_empty());
    }
}

// Catches: the role type growing a frontend variant: a token could then grant it. The
// match has no wildcard, so a new variant stops this test compiling.
#[test]
fn a_node_certificate_cannot_be_asked_for_the_frontend_role() {
    for role in Role::ALL {
        match role {
            Role::Storage | Role::Daemon | Role::Updater => {}
        }
    }
    assert!("frontend".parse::<Role>().is_err());
}

// Catches: a certificate the TLS stack would not take: webpki's checks of the chain, the
// name and the extended key usage, as the listeners and `verify-cert` make them. A daemon
// certificate must not be usable as a TLS server, and a storage one must.
#[test]
fn issued_certificates_pass_webpki_for_the_uses_of_their_role() {
    let ca = authority();
    let root = ca.root_der();
    let storage = issue(
        &ca,
        "stor-1",
        Role::Storage,
        &key(),
        Some("stor-1.farm.example"),
    );
    let daemon = issue(&ca, "dmn-1", Role::Daemon, &key(), None);

    as_server(&[root], &[&storage.der], "stor-1").expect("storage serves under its id");
    as_server(&[root], &[&storage.der], "stor-1.farm.example").expect("and its advertise host");
    as_client(&[root], &[&storage.der]).expect("storage calls peers");
    as_client(&[root], &[&daemon.der]).expect("a daemon is a client");

    assert!(
        as_server(&[root], &[&storage.der], "other").is_err(),
        "a name it does not hold"
    );
    assert!(
        as_server(&[root], &[&daemon.der], "dmn-1").is_err(),
        "a daemon cannot serve"
    );
    let stranger = authority();
    assert!(
        as_client(&[stranger.root_der()], &[&daemon.der]).is_err(),
        "another CA"
    );
}

// Catches: a certificate the node's client would discard: the checks `kbf-enroll` runs on
// an answer (key, role, names, fields, signer, validity), on every role set.
#[test]
fn issued_certificates_pass_the_nodes_answer_checks() {
    use support::answer::{Expect, check};
    let ca = authority();
    let id = node("both-1");
    for roles in [
        vec![Role::Storage],
        vec![Role::Daemon],
        vec![Role::Storage, Role::Daemon],
    ] {
        let keys: Vec<(Role, PublicKey)> = roles.iter().map(|r| (*r, key())).collect();
        let issued: Vec<IssuedCert> = keys
            .iter()
            .map(|(role, key)| {
                let i = issue(&ca, id.as_str(), *role, key, None);
                IssuedCert {
                    role: role.name().to_owned(),
                    leaf_der: i.der,
                    serial: i.serial,
                    not_before: i.not_before,
                    not_after: i.not_after,
                }
            })
            .collect();
        let wanted: Vec<(Role, Vec<u8>)> =
            keys.iter().map(|(r, k)| (*r, k.spki().to_vec())).collect();
        let pin = ca.pin();
        let accepted = check(
            &Expect {
                pin: &pin,
                node_id: &id,
                keys: &wanted,
                now: NOW,
            },
            &issued,
            ca.bundle().to_pem().as_bytes(),
            NOW,
            NOW + 3 * 86_400,
        )
        .expect("the node accepts");
        assert_eq!(accepted.len(), roles.len());
    }
}

// ---- names ---------------------------------------------------------------------------

// Catches: a storage node's advertise host being dropped, doubled when it equals the id, or
// typed wrong (DNS versus address).
#[test]
fn a_storage_certificate_names_its_advertise_host() {
    let ca = authority();
    let cases: [(&str, Vec<&str>, Vec<&str>); 4] = [
        (
            "stor-1.farm.example",
            vec!["stor-1", "stor-1.farm.example"],
            vec![],
        ),
        ("stor-1", vec!["stor-1"], vec![]),
        ("203.0.113.7", vec!["stor-1"], vec!["203.0.113.7"]),
        ("2001:db8::7", vec!["stor-1"], vec!["2001:db8::7"]),
    ];
    for (advertise, dns, ips) in cases {
        let issued = issue(&ca, "stor-1", Role::Storage, &key(), Some(advertise));
        let v = view(&issued.der);
        assert_eq!(v.dns, dns, "{advertise}");
        assert_eq!(
            v.ips.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ips,
            "{advertise}"
        );
        expected(Role::Storage, "stor-1")(&v);
    }
}

// Catches: a name that is not a name reaching a certificate: a wildcard (a man in the
// middle for every host), uppercase, underscores, empty and over-long labels, stray dots,
// spaces and non-ASCII.
#[test]
fn a_name_that_is_not_a_host_name_is_refused() {
    let ca = authority();
    let long_label = format!("{}.example", "a".repeat(64));
    let long_name = format!("{}.example", vec!["a".repeat(60); 5].join("."));
    let bad = [
        "",
        "*",
        "*.example.com",
        "Foo.example.com",
        "a_b.example",
        "-a.example",
        "a-.example",
        "a..b",
        ".a",
        "a.",
        "a b.example",
        "a/b.example",
        "\u{e9}.example",
        "a:b",
        "1.2.3.4.5",
        "999.1.1.1",
    ];
    for name in bad
        .iter()
        .copied()
        .chain([long_label.as_str(), long_name.as_str()])
    {
        let result = ca.issue_node(
            &NodeCert {
                node_id: &node("stor-1"),
                role: Role::Storage,
                key: &key(),
                advertise: Some(name),
            },
            NOW,
            &mut Stream(2),
        );
        assert_eq!(result.unwrap_err(), CaError::Name, "{name:?}");
    }
}

// Catches: a daemon certificate with a second name: the daemon's DNS name is exactly its id.
#[test]
fn a_daemon_certificate_names_only_its_node() {
    let result = authority().issue_node(
        &NodeCert {
            node_id: &node("d1"),
            role: Role::Daemon,
            key: &key(),
            advertise: Some("d1.farm.example"),
        },
        NOW,
        &mut Stream(2),
    );
    assert_eq!(result.unwrap_err(), CaError::AdvertiseForDaemon);
}

// ---- the frontend's own certificate ---------------------------------------------------

// Catches: the frontend certificate being wrong where the node checks it before it will
// send a token: the frontend role, serverAuth, the names, a chain to the root.
#[test]
fn the_frontend_certificate_is_the_frontend_role_for_its_names() {
    let ca = authority();
    let key = key();
    let issued = ca
        .issue_frontend(
            &["farm.example.com", "203.0.113.9"],
            &key,
            NOW,
            &mut Stream(3),
        )
        .expect("issued");
    let v = view(&issued.der);
    assert_eq!(v.uris, ["urn:kbf:role:frontend"]);
    assert_eq!(v.dns, ["farm.example.com"]);
    assert_eq!(
        v.ips.iter().map(ToString::to_string).collect::<Vec<_>>(),
        ["203.0.113.9"]
    );
    assert_eq!(v.cn, ["farm.example.com"]);
    assert!(v.server_auth && v.client_auth);
    assert_eq!(v.ca, Some((true, false, None)));
    assert_eq!(v.key_usage, Some((true, KU_DIGITAL_SIGNATURE)));
    assert_eq!(v.extension_oids, table_extension_oids());
    assert_eq!(v.spki, key.spki());
    assert_eq!(
        kbf_peer::role_of(&issued.der).expect("a role").name(),
        "frontend"
    );
    as_server(&[ca.root_der()], &[&issued.der], "farm.example.com").expect("serves its name");
    as_client(&[ca.root_der()], &[&issued.der]).expect("calls storage");
}

// Catches: a frontend certificate for no name, for a wildcard, or for a name that is not
// one; and a first name that is an address becoming the subject's text.
#[test]
fn the_frontend_certificate_needs_good_names() {
    let ca = authority();
    let key = key();
    let go = |names: &[&str]| ca.issue_frontend(names, &key, NOW, &mut Stream(3));
    assert_eq!(go(&[]).unwrap_err(), CaError::NoNames);
    assert_eq!(go(&["*.example.com"]).unwrap_err(), CaError::Name);
    assert_eq!(go(&["ok.example", "Bad Name"]).unwrap_err(), CaError::Name);
    let v = view(&go(&["203.0.113.9"]).expect("an address alone").der);
    assert!(v.dns.is_empty());
    assert_eq!(v.cn, ["203.0.113.9"]);
}

// ---- the update coordinator's and the updaters' certificates -------------------------

// Catches: an updater certificate that drifts from its row of the table: the role `updater`
// and the host's role as two URIs, the node's one DNS name, clientAuth and no serverAuth, no
// extension the table lacks; a host role spelled differently from `HostRole::name`; and a
// certificate the storage service's role reader would admit as a role it serves.
#[test]
fn an_updater_certificate_names_the_updater_role_and_the_hosts_role() {
    let ca = authority();
    let golden = [
        (HostRole::FRONTEND, "urn:kbf:host-role:frontend"),
        (HostRole::STORAGE, "urn:kbf:host-role:storage"),
        (HostRole::DAEMON, "urn:kbf:host-role:daemon"),
        (HostRole::MAC_DAEMON, "urn:kbf:host-role:mac-daemon"),
    ];
    for (host_role, uri) in golden {
        let key = key();
        let issued = ca
            .issue_updater(
                &UpdaterCert {
                    node_id: &node("host-7"),
                    host_role: &host_role,
                    key: &key,
                },
                NOW,
                &mut Stream(5),
            )
            .expect("issued");
        let v = view(&issued.der);
        assert_eq!(v.uris, ["urn:kbf:role:updater", uri], "{host_role:?}");
        assert_eq!(v.dns, ["host-7"]);
        assert!(v.ips.is_empty());
        assert_eq!(v.cn, ["host-7"]);
        assert_eq!(v.other_sans, 0);
        assert!(v.client_auth && !v.server_auth);
        assert_eq!(v.other_ekus, 0);
        assert_eq!(v.ca, Some((true, false, None)));
        assert_eq!(v.key_usage, Some((true, KU_DIGITAL_SIGNATURE)));
        assert_eq!(v.extension_oids, table_extension_oids());
        assert_eq!(v.spki, key.spki());
        assert_eq!(
            kbf_peer::role_of(&issued.der),
            Ok(kbf_peer::PeerRole::Updater),
            "listeners read the role `updater` and refuse it by their own tables"
        );
        as_client(&[ca.root_der()], &[&issued.der]).expect("dials the coordinator");
        as_server(&[ca.root_der()], &[&issued.der], "host-7")
            .expect_err("an updater certificate serves nothing");
    }
}

// Catches: the updater role issued as a node's certificate, which would give it the node
// shape (a role URI and the node's names, no host role) instead of its own.
#[test]
fn a_node_certificate_cannot_be_an_updaters() {
    let ca = authority();
    let key = key();
    let id = node("host-7");
    assert_eq!(
        ca.issue_node(
            &NodeCert {
                node_id: &id,
                role: Role::Updater,
                key: &key,
                advertise: None,
            },
            NOW,
            &mut Stream(5),
        ),
        Err(CaError::NotANodeRole)
    );
}

// Catches: a coordinator certificate with the wrong role, an EKU that lets it act as a
// client, names that are not the ones given, and a chain that does not verify for the one
// use it has: a TLS server under its name.
#[test]
fn the_coordinator_certificate_is_the_coordinator_role_for_its_names() {
    let ca = authority();
    let key = key();
    let issued = ca
        .issue_coordinator(
            &["coordinator.example.com", "203.0.113.9"],
            &key,
            NOW,
            &mut Stream(4),
        )
        .expect("issued");
    let v = view(&issued.der);
    assert_eq!(v.uris, ["urn:kbf:role:coordinator"]);
    assert_eq!(v.dns, ["coordinator.example.com"]);
    assert_eq!(
        v.ips.iter().map(ToString::to_string).collect::<Vec<_>>(),
        ["203.0.113.9"]
    );
    assert_eq!(v.cn, ["coordinator.example.com"]);
    assert!(v.server_auth && !v.client_auth);
    assert_eq!(v.other_ekus, 0);
    assert_eq!(v.ca, Some((true, false, None)));
    assert_eq!(v.key_usage, Some((true, KU_DIGITAL_SIGNATURE)));
    assert_eq!(v.extension_oids, table_extension_oids());
    assert_eq!(
        kbf_peer::role_of(&issued.der),
        Err(kbf_peer::RoleError::Unknown("coordinator".to_owned()))
    );
    as_server(&[ca.root_der()], &[&issued.der], "coordinator.example.com")
        .expect("serves its name");
    as_client(&[ca.root_der()], &[&issued.der]).expect_err("it dials nobody");
}

// Catches: a coordinator certificate for no name, for a wildcard, or for a name that is not
// one (the same names rule as the frontend's).
#[test]
fn the_coordinator_certificate_needs_good_names() {
    let ca = authority();
    let key = key();
    let go = |names: &[&str]| ca.issue_coordinator(names, &key, NOW, &mut Stream(3));
    assert_eq!(go(&[]).unwrap_err(), CaError::NoNames);
    assert_eq!(go(&["*.example.com"]).unwrap_err(), CaError::Name);
    assert_eq!(go(&["ok.example", "Bad Name"]).unwrap_err(), CaError::Name);
}

// ---- the job scheduler's certificate -------------------------------------------------

// Catches: a scheduler certificate with the wrong role, an EKU that lets it act as a client
// (the signer's table forced clientAuth on every role before this one), names that are not
// the ones given, and a chain that does not verify for the one use it has: a TLS server
// under each of its names, an address and a DNS name.
#[test]
fn a_signed_scheduler_certificate_verifies_against_the_pin_with_ip_and_dns() {
    let ca = authority();
    let key = key();
    let issued = ca
        .issue_scheduler(
            &["192.0.2.7", "sched.example.com"],
            &key,
            NOW,
            &mut Stream(5),
        )
        .expect("issued");
    let v = view(&issued.der);
    assert_eq!(v.uris, ["urn:kbf:role:scheduler"]);
    assert_eq!(v.dns, ["sched.example.com"]);
    assert_eq!(
        v.ips.iter().map(ToString::to_string).collect::<Vec<_>>(),
        ["192.0.2.7"]
    );
    assert_eq!(v.cn, ["192.0.2.7"]);
    assert!(v.server_auth && !v.client_auth);
    assert_eq!(v.other_ekus, 0);
    assert_eq!(v.ca, Some((true, false, None)));
    assert_eq!(v.key_usage, Some((true, KU_DIGITAL_SIGNATURE)));
    assert_eq!(v.extension_oids, table_extension_oids());
    assert_eq!(v.spki, key.spki());
    as_server(&[ca.root_der()], &[&issued.der], "192.0.2.7").expect("serves its address");
    as_server(&[ca.root_der()], &[&issued.der], "sched.example.com").expect("serves its name");
    as_client(&[ca.root_der()], &[&issued.der]).expect_err("it dials nobody");
}

// Catches: a scheduler certificate a node would admit as a role it serves (the storage
// service reads the role from the certificate), or that passes as the frontend's.
#[test]
fn a_scheduler_certificate_is_no_peer_role() {
    let ca = authority();
    let issued = ca
        .issue_scheduler(&["192.0.2.7"], &key(), NOW, &mut Stream(6))
        .expect("issued");
    assert_eq!(
        kbf_peer::role_of(&issued.der),
        Err(kbf_peer::RoleError::Unknown("scheduler".to_owned()))
    );
    let frontend = ca
        .issue_frontend(&["192.0.2.7"], &key(), NOW, &mut Stream(7))
        .expect("issued");
    assert_ne!(view(&issued.der).uris, view(&frontend.der).uris);
    assert!(view(&frontend.der).client_auth);
}

// Catches: a scheduler certificate for no name, for a wildcard, or for a name that is not
// one (the same names rule as the frontend's).
#[test]
fn the_scheduler_certificate_needs_good_names() {
    let ca = authority();
    let key = key();
    let go = |names: &[&str]| ca.issue_scheduler(names, &key, NOW, &mut Stream(3));
    assert_eq!(go(&[]).unwrap_err(), CaError::NoNames);
    assert_eq!(go(&["*.example.com"]).unwrap_err(), CaError::Name);
    assert_eq!(go(&["ok.example", "Bad Name"]).unwrap_err(), CaError::Name);
}

// ---- serials -------------------------------------------------------------------------

// Catches: a counter or a reused serial, a negative or zero one, one over 127 bits, and a
// spelling that differs from the deny list's.
#[test]
fn a_thousand_serials_are_distinct_positive_and_in_the_deny_lists_spelling() {
    let ca = authority();
    let key = key();
    let mut seen = std::collections::BTreeSet::new();
    let mut entropy = Stream(77);
    for _ in 0..1000 {
        let issued = ca
            .issue_node(
                &NodeCert {
                    node_id: &node("n"),
                    role: Role::Daemon,
                    key: &key,
                    advertise: None,
                },
                NOW,
                &mut entropy,
            )
            .expect("issued");
        assert!(seen.insert(issued.serial.clone()), "a serial repeated");
        assert_eq!(kbf_map::normalize_serial(&issued.serial), issued.serial);
        let (_, cert) = X509Certificate::from_der(&issued.der).expect("cert");
        assert_eq!(cert.serial.to_str_radix(16), issued.serial);
        let value = u128::from_str_radix(&issued.serial, 16).expect("hex");
        assert!(
            value > 0 && value < (1 << 127),
            "positive and at most 127 bits"
        );
    }
}

// Catches: a serial whose first random byte is zero being written with its leading zero (the
// deny list would then miss it), or one with the high bit of its first nonzero byte set
// being written as a negative integer.
#[test]
fn a_serial_with_a_leading_zero_byte_is_spelt_without_it() {
    let ca = authority();
    let key = key();
    let mut block = vec![0u8; 16];
    block[1] = 0x8a;
    block[15] = 0x01;
    let issued = ca
        .issue_node(
            &NodeCert {
                node_id: &node("n"),
                role: Role::Daemon,
                key: &key,
                advertise: None,
            },
            NOW,
            &mut Script::new([block]),
        )
        .expect("issued");
    assert_eq!(issued.serial, "8a0000000000000000000000000001");
    assert_eq!(kbf_map::normalize_serial(&issued.serial), issued.serial);
    assert_eq!(view(&issued.der).serial_hex, issued.serial);
}

// Catches: bit 127 of the draw surviving (a 128-bit serial that reads as negative), and a
// zero serial being issued when the source gives zeros; the next draw is used instead.
#[test]
fn the_top_bit_is_cleared_and_zero_is_drawn_again() {
    let ca = authority();
    let key = key();
    let go = |blocks: Vec<Vec<u8>>| {
        ca.issue_node(
            &NodeCert {
                node_id: &node("n"),
                role: Role::Daemon,
                key: &key,
                advertise: None,
            },
            NOW,
            &mut Script::new(blocks),
        )
        .expect("issued")
        .serial
    };
    assert_eq!(go(vec![vec![0xff; 16]]), "7fffffffffffffffffffffffffffffff");
    let mut top_only = vec![0u8; 16];
    top_only[0] = 0x80;
    let mut last = vec![0u8; 16];
    last[15] = 0x2a;
    assert_eq!(
        go(vec![vec![0; 16], top_only, last]),
        "2a",
        "zero and bit 127 alone are drawn again"
    );
}

// ---- time ----------------------------------------------------------------------------

// Catches: a lifetime ignoring the policy, or bounds that are off by one.
#[test]
fn the_lifetime_follows_the_policy_within_its_bounds() {
    assert_eq!(Policy::default().lifetime_s(), 7 * 86_400);
    assert_eq!(Policy::new(3599).unwrap_err(), CaError::Lifetime);
    assert_eq!(Policy::new(30 * 86_400 + 1).unwrap_err(), CaError::Lifetime);
    assert_eq!(Policy::new(-1).unwrap_err(), CaError::Lifetime);
    for lifetime in [3600, 86_400, 30 * 86_400] {
        let ca = Authority::create(
            External::new(),
            Policy::new(lifetime).expect("in bounds"),
            NOW,
            &mut Stream(1),
        )
        .expect("root");
        let v = view(&issue(&ca, "n", Role::Daemon, &key(), None).der);
        assert_eq!(v.not_after - v.not_before, lifetime);
    }
}

// Catches: a CA that signs on a clock that is wrong: an unset clock reads 1970, and the
// certificates it would stamp would be valid at the wrong time for a week.
#[test]
fn a_clock_before_the_build_floor_signs_nothing() {
    assert_eq!(CLOCK_FLOOR_UNIX, 1_790_812_800, "2026-10-01T00:00:00Z");
    let now_real = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after 1970")
        .as_secs();
    assert!(
        i64::try_from(now_real).expect("fits") > CLOCK_FLOOR_UNIX,
        "the floor is in the past"
    );

    let ca = authority();
    let go = |now: i64| {
        ca.issue_node(
            &NodeCert {
                node_id: &node("n"),
                role: Role::Daemon,
                key: &key(),
                advertise: None,
            },
            now,
            &mut Stream(1),
        )
    };
    for now in [0, CLOCK_FLOOR_UNIX - 1] {
        assert_eq!(
            go(now).unwrap_err(),
            CaError::ClockBeforeFloor {
                now,
                floor: CLOCK_FLOOR_UNIX
            }
        );
    }
    assert!(go(CLOCK_FLOOR_UNIX).is_ok(), "the floor itself signs");
    let frontend = ca.issue_frontend(&["f.example"], &key(), CLOCK_FLOOR_UNIX - 1, &mut Stream(1));
    assert!(
        matches!(frontend, Err(CaError::ClockBeforeFloor { .. })),
        "the frontend too"
    );
    let root = Authority::create(External::new(), Policy::default(), 5, &mut Stream(1));
    assert!(
        matches!(root, Err(CaError::ClockBeforeFloor { .. })),
        "and a new root"
    );

    let lowered = Policy::default().with_clock_floor(100);
    assert_eq!(lowered.clock_floor(), 100);
    assert!(Authority::create(External::new(), lowered, 100, &mut Stream(1)).is_ok());
}

// Catches: a time past what the certificate format carries wrapping into a certificate that
// is valid in the past.
#[test]
fn a_time_beyond_the_formats_range_is_refused() {
    let ca = authority();
    for now in [i64::MAX, 300_000_000_000, 253_402_300_000] {
        let result = ca.issue_node(
            &NodeCert {
                node_id: &node("n"),
                role: Role::Daemon,
                key: &key(),
                advertise: None,
            },
            now,
            &mut Stream(1),
        );
        assert_eq!(result.unwrap_err(), CaError::TimeRange, "{now}");
    }
}

// ---- the signer and the process --------------------------------------------------------

// Catches: a signer that fails being reported as a certificate; nothing is returned and the
// error carries the signer's own failure text, not a key.
#[test]
fn a_failing_signer_issues_nothing() {
    let signer = External::new();
    let ca =
        Authority::create(signer.clone(), Policy::default(), NOW, &mut Stream(1)).expect("root");
    assert!(
        signer.signed.load(Ordering::SeqCst) >= 1,
        "the root was signed through the seam"
    );
    signer.fail.store(true, Ordering::SeqCst);
    let result = ca.issue_node(
        &NodeCert {
            node_id: &node("n"),
            role: Role::Daemon,
            key: &key(),
            advertise: None,
        },
        NOW,
        &mut Stream(1),
    );
    let CaError::Build(text) = result.unwrap_err() else {
        panic!("a build error")
    };
    assert!(!text.is_empty());
    assert!(Authority::create(signer, Policy::default(), NOW, &mut Stream(1)).is_err());
}

// Catches: the in-process file signer reaching production, and a signer outside the process
// being refused for no reason.
#[test]
fn production_refuses_the_in_process_signer() {
    let file = kbf_ca::FileSigner::generate().expect("key");
    let ca =
        Authority::create(Arc::new(file), Policy::default(), NOW, &mut Stream(1)).expect("root");
    assert_eq!(ca.require_external().unwrap_err(), CaError::InProcessSigner);
    assert!(authority().require_external().is_ok());
}

// Catches: a system random source that returns the same bytes, or zeros.
#[test]
fn system_entropy_differs_between_draws() {
    let mut random = SystemEntropy;
    let (mut a, mut b) = ([0u8; 32], [0u8; 32]);
    random.fill(&mut a);
    random.fill(&mut b);
    assert_ne!(a, b);
    assert_ne!(a, [0u8; 32]);
}

// Catches: a Debug of the authority that prints more than its pin.
#[test]
fn the_authority_prints_its_pin_and_nothing_else() {
    let ca = authority();
    let text = format!("{ca:?}");
    assert!(text.contains(&ca.pin().to_string()), "{text}");
    assert!(text.starts_with("Authority"), "{text}");
}
