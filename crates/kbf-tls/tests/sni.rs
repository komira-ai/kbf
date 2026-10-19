//! `SniCerts`: the server certificate chosen by the name in the ClientHello, over real
//! handshakes, and the checks `from_config` makes before a list is accepted.

mod support;

use std::path::Path;
use std::sync::Arc;

use kbf_tls::{CertEntry, SniCerts, TlsError, server_config_optional_client_sni};
use rcgen::ExtendedKeyUsagePurpose::ServerAuth;
use rustls::DigitallySignedStruct;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use support::{Ca, Leaf, ca, dir, leaf};
use tokio_rustls::{TlsAcceptor, TlsConnector};

const PUBLIC: &str = "reapi.example.com";
const FARM: &str = "farm.internal.example";

/// Accepts any server certificate: these tests look at which one was sent, and the
/// verification against a root is the proxy's handshake test.
#[derive(Debug)]
struct AcceptAny;

impl ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _: &CertificateDer<'_>,
        _: &[CertificateDer<'_>],
        _: &ServerName<'_>,
        _: &[u8],
        _: UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn verify_tls13_signature(
        &self,
        _: &[u8],
        _: &CertificateDer<'_>,
        _: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }
    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// The DER of the certificate the server sent to a client that asks for `sni` (`None`: the
/// client sends no server name).
async fn served_for(certs: &SniCerts, ca: &Ca, sni: Option<&str>) -> Vec<u8> {
    let server =
        server_config_optional_client_sni(certs.clone(), ca.pem.as_bytes()).expect("server config");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut client = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAny))
        .with_no_client_auth();
    client.enable_sni = sni.is_some();
    let (near, far) = tokio::io::duplex(1 << 16);
    let accept = tokio::spawn(async move { TlsAcceptor::from(server).accept(far).await });
    let name = ServerName::try_from(sni.unwrap_or("unnamed.invalid").to_owned()).expect("name");
    let tls = TlsConnector::from(Arc::new(client))
        .connect(name, near)
        .await
        .expect("handshake");
    let der = tls.get_ref().1.peer_certificates().expect("chain")[0]
        .as_ref()
        .to_vec();
    drop(tls);
    let _ = accept.await;
    der
}

struct Pair {
    dir: std::path::PathBuf,
    public: Leaf,
    farm: Leaf,
    ca: Ca,
}

fn write(dir: &Path, stem: &str, leaf: &Leaf) -> (std::path::PathBuf, std::path::PathBuf) {
    let (cert, key) = (
        dir.join(format!("{stem}.pem")),
        dir.join(format!("{stem}.key")),
    );
    std::fs::write(&cert, &leaf.cert).expect("cert");
    std::fs::write(&key, &leaf.key).expect("key");
    (cert, key)
}

fn entry(dir: &Path, stem: &str, leaf: &Leaf, names: &[&str], default: bool) -> CertEntry {
    let (cert, key) = write(dir, stem, leaf);
    CertEntry {
        names: names.iter().map(|n| (*n).to_owned()).collect(),
        cert,
        key,
        default,
    }
}

/// A public-name certificate and a farm-name certificate, from two different CAs, and the
/// client CA the server verifies with.
fn pair(test: &str) -> Pair {
    let (public_ca, farm_ca) = (ca("public CA"), ca("farm CA"));
    Pair {
        dir: dir(test),
        public: leaf(&public_ca, PUBLIC, ServerAuth, false),
        farm: leaf(&farm_ca, FARM, ServerAuth, false),
        ca: farm_ca,
    }
}

fn two(p: &Pair, public_default: bool) -> Vec<CertEntry> {
    vec![
        entry(&p.dir, "public", &p.public, &[PUBLIC], public_default),
        entry(&p.dir, "farm", &p.farm, &[FARM], !public_default),
    ]
}

fn refused(entries: &[CertEntry]) -> String {
    SniCerts::from_config(entries)
        .expect_err("refused")
        .to_string()
}

/// Each configured name is served its own certificate.
///
/// Kills: one certificate for every name (the behavior before SNI), a name table keyed
/// the wrong way round.
#[tokio::test]
async fn each_name_gets_its_own_certificate() {
    let p = pair("each-name");
    let certs = SniCerts::from_config(&two(&p, true)).expect("config");
    assert_eq!(served_for(&certs, &p.ca, Some(PUBLIC)).await, p.public.der);
    assert_eq!(served_for(&certs, &p.ca, Some(FARM)).await, p.farm.der);
}

/// No name, and a name nobody configured, get the default; which one is the default is the
/// flag, not the order.
///
/// Kills: a default that is the first or last entry; an unknown name that is refused or that
/// gets a configured name's certificate.
#[tokio::test]
async fn no_name_and_an_unknown_name_get_the_default() {
    let p = pair("default");
    let public_default = SniCerts::from_config(&two(&p, true)).expect("config");
    assert_eq!(served_for(&public_default, &p.ca, None).await, p.public.der);
    assert_eq!(
        served_for(&public_default, &p.ca, Some("other.example.com")).await,
        p.public.der
    );
    let farm_default = SniCerts::from_config(&two(&p, false)).expect("config");
    assert_eq!(served_for(&farm_default, &p.ca, None).await, p.farm.der);
    assert_eq!(
        served_for(&farm_default, &p.ca, Some("other.example.com")).await,
        p.farm.der
    );
    // The named entry of the non-default is still served by name.
    assert_eq!(
        served_for(&farm_default, &p.ca, Some(PUBLIC)).await,
        p.public.der
    );
}

/// Names compare case-insensitively, on both sides: the client's and the configuration's.
///
/// Kills: a lookup by the exact bytes of the name.
#[tokio::test]
async fn names_match_without_regard_to_case() {
    let p = pair("case");
    let mut entries = two(&p, false);
    entries[0].names = vec!["REAPI.Example.COM.".to_owned()];
    let certs = SniCerts::from_config(&entries).expect("config");
    for sni in [
        "reapi.example.com",
        "REAPI.EXAMPLE.COM",
        "Reapi.Example.Com",
    ] {
        assert_eq!(
            served_for(&certs, &p.ca, Some(sni)).await,
            p.public.der,
            "{sni}"
        );
    }
}

/// A certificate with a wildcard name covers one label under it, and a configured name must be
/// covered.
///
/// Kills: a SAN check that compares only exact strings, or that lets a wildcard cover two
/// labels.
#[test]
fn a_wildcard_san_covers_one_label() {
    let ca = ca("wild CA");
    let wild = leaf(&ca, "*.example.com", ServerAuth, false);
    let d = dir("wildcard");
    let ok = entry(&d, "wild", &wild, &["a.example.com"], true);
    SniCerts::from_config(&[ok]).expect("one label");
    for bad in ["a.b.example.com", "example.com"] {
        let e = entry(&d, "wild", &wild, &[bad], true);
        assert!(
            matches!(SniCerts::from_config(&[e]), Err(TlsError::Config(_))),
            "{bad}"
        );
    }
}

/// `from_config` refuses a key that is not the certificate's, and says which files.
///
/// Kills: a list accepted with a key that fails every handshake.
#[test]
fn a_key_that_is_not_the_certificates_is_refused() {
    let p = pair("mismatch");
    let mut entries = two(&p, true);
    let other = leaf(&ca("third CA"), PUBLIC, ServerAuth, false);
    std::fs::write(&entries[0].key, &other.key).expect("swap key");
    let why = refused(&entries);
    assert!(
        why.contains("public.key") && why.contains("public.pem"),
        "{why}"
    );
}

/// A name that is not among the certificate's names is refused.
///
/// Kills: the name table trusted without the certificate (a typo that serves the wrong host's
/// certificate).
#[test]
fn a_name_the_certificate_does_not_carry_is_refused() {
    let p = pair("not-in-san");
    let mut entries = two(&p, true);
    entries[0].names = vec!["other.example.com".to_owned()];
    let why = refused(&entries);
    assert!(
        why.contains("other.example.com") && why.contains("subject alternative"),
        "{why}"
    );
}

/// A name configured twice, even in another case or on another entry, is refused.
///
/// Kills: a later entry silently replacing an earlier one.
#[test]
fn a_duplicate_name_is_refused() {
    let p = pair("duplicate");
    let mut entries = two(&p, true);
    entries[1].names = vec![FARM.to_owned(), PUBLIC.to_uppercase()];
    let why = refused(&entries);
    assert!(why.contains("twice") && why.contains(PUBLIC), "{why}");
}

/// The default is explicit: two or more entries need exactly one; an entry with no names must
/// be it; names are plain; an empty list and an unreadable file are refused.
///
/// Kills: a default picked by position; two defaults with a silent winner; a wildcard
/// configured name that a lookup could never hit.
#[test]
fn the_list_must_name_one_default_and_plain_names() {
    let p = pair("shape");
    let mut none = two(&p, true);
    none[0].default = false;
    assert!(refused(&none).contains("exactly one"));
    let mut both = two(&p, true);
    both[1].default = true;
    assert!(refused(&both).contains("exactly one"));
    let mut nameless = two(&p, true);
    nameless[1].names.clear();
    assert!(refused(&nameless).contains("no names"));
    let mut wildcard = two(&p, true);
    wildcard[1].names = vec!["*.internal.example".to_owned()];
    assert!(refused(&wildcard).contains("plain DNS name"));
    assert!(refused(&[]).contains("no server certificate"));
    let mut missing = two(&p, true);
    missing[0].cert = p.dir.join("absent.pem");
    assert!(matches!(
        SniCerts::from_config(&missing),
        Err(TlsError::Read { .. })
    ));
}

/// A list of one needs no flag and no names: it is the old single-certificate form, served for
/// every name.
///
/// Kills: the single-certificate configuration no longer loading.
#[tokio::test]
async fn one_entry_with_no_names_serves_every_name() {
    let p = pair("single");
    let e = entry(&p.dir, "only", &p.farm, &[], false);
    let certs = SniCerts::from_config(&[e]).expect("one entry");
    for sni in [Some(FARM), Some("anything.example.com"), None] {
        assert_eq!(served_for(&certs, &p.ca, sni).await, p.farm.der);
    }
}

/// `reject_expired` refuses a list with an expired leaf; `from_config` alone does not.
///
/// Kills: a reload that swaps in an expired certificate; a first load that refuses one.
#[test]
fn an_expired_leaf_is_refused_only_when_asked() {
    let p = pair("expired");
    let old = leaf(&p.ca, PUBLIC, ServerAuth, true);
    let e = entry(&p.dir, "old", &old, &[PUBLIC], true);
    let certs = SniCerts::from_config(&[e]).expect("loads");
    assert!(matches!(
        certs.reject_expired(),
        Err(TlsError::Expired { .. })
    ));
}
