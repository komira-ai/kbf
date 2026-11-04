use super::*;

/// The PEM of a new self-signed CA.
fn ca_pem() -> String {
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
    params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let key = rcgen::KeyPair::generate().unwrap();
    rcgen::CertifiedIssuer::self_signed(params, key)
        .unwrap()
        .pem()
}

/// Asserts that the trust anchors are exactly the certificates of the PEM given, one for one
/// and in its order, with no system root added; and that a PEM with none is refused rather
/// than left to fall back on other roots. Catches a store seeded with platform or bundled
/// roots, which would let any publicly trusted certificate receive the bearer token.
#[test]
fn the_trust_anchors_are_the_pinned_cas_and_nothing_else() {
    let (one, two) = (ca_pem(), ca_pem());
    let expected = |pem: &str| -> Vec<rustls::pki_types::TrustAnchor<'static>> {
        CertificateDer::pem_slice_iter(pem.as_bytes())
            .map(|cert| webpki_anchor(&cert.unwrap()))
            .collect()
    };
    let single = roots(one.as_bytes()).unwrap();
    assert_eq!(single.roots, expected(&one));
    let both = format!("{one}{two}");
    let pair = roots(both.as_bytes()).unwrap();
    assert_eq!(pair.len(), 2);
    assert_eq!(pair.roots, expected(&both));
    assert!(matches!(roots(b""), Err(ConfigError::BadCa(reason)) if reason == "no certificate"));
}

/// The trust anchor rustls makes of `cert`.
fn webpki_anchor(cert: &CertificateDer<'_>) -> rustls::pki_types::TrustAnchor<'static> {
    let mut store = rustls::RootCertStore::empty();
    store.add(cert.clone().into_owned()).unwrap();
    store.roots.remove(0)
}

/// Asserts the client's TLS settings beside its roots: no client certificate is presented
/// (the resolver has none), `h2` is the one protocol offered, and the provider is `ring`'s,
/// as the crate documents. Catches a client certificate or a second protocol added to the
/// settings.
#[test]
fn the_client_presents_no_certificate_and_offers_h2() {
    let config = client_config(roots(ca_pem().as_bytes()).unwrap()).unwrap();
    assert!(!config.client_auth_cert_resolver.has_certs());
    assert_eq!(config.alpn_protocols, [b"h2".to_vec()]);
    let ring = rustls::crypto::ring::default_provider();
    let suites = |p: &rustls::crypto::CryptoProvider| -> Vec<String> {
        p.cipher_suites
            .iter()
            .map(|s| format!("{:?}", s.suite()))
            .collect()
    };
    assert_eq!(suites(config.crypto_provider()), suites(&ring));
}
