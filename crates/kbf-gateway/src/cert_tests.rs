use rcgen::{CertificateParams, ExtendedKeyUsagePurpose, KeyPair, SanType};

use super::*;

/// The DER of a self-signed certificate naming `names` with `usages`.
fn der(names: &[&str], usages: Vec<ExtendedKeyUsagePurpose>) -> Vec<u8> {
    let names: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
    let mut params = CertificateParams::new(names).expect("params");
    params.extended_key_usages = usages;
    let key = KeyPair::generate().expect("key");
    params
        .self_signed(&key)
        .expect("certificate")
        .der()
        .to_vec()
}

/// Catches: a server certificate refused, or its node read from anywhere but the one
/// DNS name (a second certificate for the same host with an IP name is still one node).
#[test]
fn a_server_certificate_names_its_node() {
    let usages = || {
        vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ]
    };
    assert_eq!(
        server_node(&der(&["kbf1.farm.test"], usages())).as_deref(),
        Ok("kbf1.farm.test")
    );
    let mut params = CertificateParams::new(vec!["kbf1.farm.test".to_owned()]).expect("params");
    params.extended_key_usages = usages();
    params
        .subject_alt_names
        .push(SanType::IpAddress("192.0.2.5".parse().expect("address")));
    let key = KeyPair::generate().expect("key");
    let with_ip = params.self_signed(&key).expect("certificate");
    assert_eq!(
        server_node(with_ip.der()).as_deref(),
        Ok("kbf1.farm.test"),
        "an IP name is not a node name"
    );
}

/// Catches: a daemon's certificate (clientAuth only), or one with no extended key usage,
/// registering as a server; a certificate for no node, or for several, picking one.
#[test]
fn other_certificates_are_refused() {
    let client = der(&["node-1"], vec![ExtendedKeyUsagePurpose::ClientAuth]);
    assert_eq!(server_node(&client), Err(Refusal::NotServerAuth));
    let bare = der(&["node-1"], Vec::new());
    assert_eq!(server_node(&bare), Err(Refusal::NotServerAuth));

    let both = || {
        vec![
            ExtendedKeyUsagePurpose::ServerAuth,
            ExtendedKeyUsagePurpose::ClientAuth,
        ]
    };
    assert_eq!(server_node(&der(&[], both())), Err(Refusal::DnsNames(0)));
    assert_eq!(
        server_node(&der(&["a.farm.test", "b.farm.test"], both())),
        Err(Refusal::DnsNames(2))
    );
    assert!(matches!(server_node(b"not DER"), Err(Refusal::Unparsed(_))));
}

/// Catches: the wrong gRPC status for a refusal (a bad certificate is UNAUTHENTICATED,
/// a good one of the wrong kind PERMISSION_DENIED).
#[test]
fn refusals_map_to_statuses() {
    use tonic::Code;
    assert_eq!(
        Status::from(Refusal::Unparsed("x".to_owned())).code(),
        Code::Unauthenticated
    );
    assert_eq!(
        Status::from(Refusal::NotServerAuth).code(),
        Code::PermissionDenied
    );
    assert_eq!(
        Status::from(Refusal::DnsNames(2)).code(),
        Code::PermissionDenied
    );
}
