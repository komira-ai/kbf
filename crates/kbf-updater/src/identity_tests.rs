use kbf_types::{HostRole, NodeId};
use rcgen::{CertificateParams, ExtendedKeyUsagePurpose, KeyPair, SanType};

use crate::identity::{Identity, IdentityError, identity_of};

fn id(text: &str) -> NodeId {
    text.parse().expect("a node id")
}

fn uri(text: &str) -> SanType {
    SanType::URI(text.try_into().expect("an IA5 name"))
}

fn dns(name: &str) -> SanType {
    SanType::DnsName(name.try_into().expect("an IA5 name"))
}

fn cert(sans: Vec<SanType>) -> Vec<u8> {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("params");
    params.subject_alt_names = sans;
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
    let key = KeyPair::generate().expect("key");
    params.self_signed(&key).expect("sign").der().to_vec()
}

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| (*s).to_owned()).collect()
}

/// Catches: an updater certificate read as another host or role than it names: every host
/// role is read from its own URI, the node id from the one DNS name, in any order of the
/// subjectAltNames.
#[test]
fn an_updater_certificate_names_its_host_and_its_host_role() {
    for role in [
        HostRole::FRONTEND,
        HostRole::STORAGE,
        HostRole::DAEMON,
        HostRole::MAC_DAEMON,
    ] {
        let host_role = format!("urn:kbf:host-role:{role}");
        let sans = vec![dns("host-7"), uri(&host_role), uri("urn:kbf:role:updater")];
        assert_eq!(
            identity_of(&cert(sans)),
            Ok(Identity {
                node_id: id("host-7"),
                role
            })
        );
    }
    let reversed = vec![
        uri("urn:kbf:role:updater"),
        uri("urn:kbf:host-role:storage"),
        dns("s-1"),
    ];
    assert_eq!(
        identity_of(&cert(reversed)).expect("read").role,
        HostRole::STORAGE
    );
}

/// Catches: a certificate of another role being taken for an updater's: the farm's own
/// roles, the coordinator's, a role in another letter case, a role that is only a DNS name,
/// two roles, and none.
#[test]
fn a_certificate_of_another_role_is_no_updater() {
    let host = |name: &str| vec![dns("host-7"), uri("urn:kbf:host-role:storage"), uri(name)];
    for (role, named) in [
        ("urn:kbf:role:storage", &["storage"][..]),
        ("urn:kbf:role:frontend", &["frontend"]),
        ("urn:kbf:role:daemon", &["daemon"]),
        ("urn:kbf:role:coordinator", &["coordinator"]),
        ("urn:kbf:role:Updater", &["Updater"]),
        ("urn:kbf:role:admin", &["admin"]),
        ("urn:kbf:role:", &[""]),
    ] {
        assert_eq!(
            identity_of(&cert(host(role))),
            Err(IdentityError::NotAnUpdater(strings(named))),
            "{role}"
        );
    }
    let mut two = host("urn:kbf:role:updater");
    two.push(uri("urn:kbf:role:storage"));
    assert_eq!(
        identity_of(&cert(two)),
        Err(IdentityError::NotAnUpdater(strings(&[
            "updater", "storage"
        ])))
    );
    let mut dns_only = vec![dns("updater"), uri("urn:kbf:host-role:storage")];
    dns_only.push(dns("host-7"));
    assert_eq!(
        identity_of(&cert(dns_only)),
        Err(IdentityError::NotAnUpdater(vec![]))
    );
    let wrong_scheme = vec![dns("host-7"), uri("urn:other:role:updater")];
    assert_eq!(
        identity_of(&cert(wrong_scheme)),
        Err(IdentityError::NotAnUpdater(vec![]))
    );
}

/// Catches: an updater certificate with a host role that is missing, repeated, unknown or in
/// another letter case being given a role to be limited and ordered by.
#[test]
fn an_updater_must_name_exactly_one_host_role_the_farm_has() {
    let with = |host_roles: &[&str]| {
        let mut sans = vec![dns("host-7"), uri("urn:kbf:role:updater")];
        sans.extend(
            host_roles
                .iter()
                .map(|r| uri(&format!("urn:kbf:host-role:{r}"))),
        );
        cert(sans)
    };
    for names in [
        &[][..],
        &["storage", "daemon"],
        &["storage", "storage"],
        &["gateway"],
        &["cache"],
        &["Storage"],
        &["mac_daemon"],
        &[""],
    ] {
        assert_eq!(
            identity_of(&with(names)),
            Err(IdentityError::HostRole(strings(names))),
            "{names:?}"
        );
    }
}

/// Catches: a certificate with no node id, two, one that is not a node id (upper case,
/// underscore, an address) being given an identity.
#[test]
fn an_updater_must_name_exactly_one_node_id() {
    let with = |sans: Vec<SanType>| {
        let mut all = vec![uri("urn:kbf:role:updater"), uri("urn:kbf:host-role:daemon")];
        all.extend(sans);
        cert(all)
    };
    let ip = SanType::IpAddress("203.0.113.5".parse().expect("an address"));
    for (sans, named) in [
        (vec![], &[][..]),
        (vec![ip], &[]),
        (vec![dns("a"), dns("b")], &["a", "b"]),
        (vec![dns("HOST")], &["HOST"]),
        (vec![dns("host_1")], &["host_1"]),
        (vec![dns("-host")], &["-host"]),
        (vec![dns("a.b")], &["a.b"]),
    ] {
        assert_eq!(
            identity_of(&with(sans)),
            Err(IdentityError::NodeId(strings(named))),
            "{named:?}"
        );
    }
}

/// Catches: bytes that are no certificate being read as one, and error texts that do not say
/// what was wrong.
#[test]
fn bytes_that_are_no_certificate_name_no_one() {
    let why = identity_of(b"not DER").expect_err("refused");
    assert!(matches!(why, IdentityError::NotACertificate(_)));
    assert!(
        why.to_string()
            .starts_with("the client certificate does not parse")
    );
    assert_eq!(
        IdentityError::NotAnUpdater(strings(&["storage"])).to_string(),
        r#"the client certificate's role is not `updater` (it names ["storage"])"#
    );
    assert!(
        IdentityError::HostRole(vec![])
            .to_string()
            .contains("host role")
    );
    assert!(
        IdentityError::NodeId(vec![])
            .to_string()
            .contains("node id")
    );
}
