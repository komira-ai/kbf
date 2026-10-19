//! `server_config_optional_client`: a client certificate that is asked for, verified when
//! it is presented, and not required.
//!
//! Every handshake is a real one over an in-memory pipe, so what is checked is what a
//! client and a server experience and not what the configuration says it holds.

mod support;

use std::sync::Arc;

use kbf_tls::{TlsError, server_config_optional_client};
use rustls::RootCertStore;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName};
use support::{Ca, Leaf, ca, client_leaf, leaf, server_leaf};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_rustls::{TlsAcceptor, TlsConnector};

fn config(
    server: &Leaf,
    ca: &Ca,
    reject_expired: bool,
) -> Result<Arc<rustls::ServerConfig>, TlsError> {
    server_config_optional_client(
        server.cert.as_bytes(),
        server.key.as_bytes(),
        ca.pem.as_bytes(),
        reject_expired,
    )
}

fn client_config(trust: &Ca, identity: Option<&Leaf>) -> Arc<rustls::ClientConfig> {
    let mut roots = RootCertStore::empty();
    for der in CertificateDer::pem_slice_iter(trust.pem.as_bytes()) {
        roots.add(der.expect("CA PEM")).expect("add root");
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_root_certificates(roots);
    let mut config = match identity {
        Some(identity) => builder
            .with_client_auth_cert(
                CertificateDer::pem_slice_iter(identity.cert.as_bytes())
                    .collect::<Result<_, _>>()
                    .expect("client chain"),
                PrivateKeyDer::from_pem_slice(identity.key.as_bytes()).expect("client key"),
            )
            .expect("client identity"),
        None => builder.with_no_client_auth(),
    };
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(config)
}

/// What one handshake did, from both ends.
struct Outcome {
    /// The server's `accept`.
    server: std::io::Result<Option<Vec<u8>>>,
    /// The first byte the client read after its handshake, or the error it got.
    client: std::io::Result<()>,
    alpn: Option<Vec<u8>>,
}

/// One handshake, then one byte from the server to the client: with TLS 1.3 a client
/// certificate the server refuses shows to the client only at that read.
async fn handshake(
    server: Arc<rustls::ServerConfig>,
    client: Arc<rustls::ClientConfig>,
) -> Outcome {
    let (near, far) = tokio::io::duplex(1 << 16);
    let accept = tokio::spawn(async move {
        let mut tls = TlsAcceptor::from(server).accept(far).await?;
        let leaf = tls
            .get_ref()
            .1
            .peer_certificates()
            .map(|chain| chain[0].as_ref().to_vec());
        tls.write_all(b"x").await?;
        tls.flush().await?;
        Ok(leaf)
    });
    let name = ServerName::try_from("localhost").expect("server name");
    let connected = TlsConnector::from(client).connect(name, near).await;
    let (client, alpn) = match connected {
        Ok(mut tls) => {
            let alpn = tls.get_ref().1.alpn_protocol().map(<[u8]>::to_vec);
            let mut byte = [0_u8; 1];
            (tls.read_exact(&mut byte).await.map(|_| ()), alpn)
        }
        Err(e) => (Err(e), None),
    };
    Outcome {
        server: accept.await.expect("the server task"),
        client,
        alpn,
    }
}

/// A client with no certificate is served; the server sees none.
///
/// Kills: a verifier built with `.build()`, which makes the certificate mandatory.
#[tokio::test]
async fn a_client_without_a_certificate_is_served_anonymously() {
    let ca = ca("optional CA");
    let server = server_leaf(&ca);
    let done = handshake(
        config(&server, &ca, false).expect("config"),
        client_config(&ca, None),
    )
    .await;
    assert_eq!(done.server.expect("accepted"), None);
    done.client.expect("served");
}

/// A client certificate of the CA is verified and reaches the server.
///
/// Kills: a configuration that never asks for the certificate (`with_no_client_auth`).
#[tokio::test]
async fn a_certificate_of_the_ca_is_verified_and_seen() {
    let ca = ca("verified CA");
    let (server, client) = (server_leaf(&ca), client_leaf(&ca, "node-1.farm"));
    let done = handshake(
        config(&server, &ca, false).expect("config"),
        client_config(&ca, Some(&client)),
    )
    .await;
    assert_eq!(done.server.expect("accepted"), Some(client.der));
    done.client.expect("served");
}

/// A certificate of another CA fails the handshake; the server never treats it as
/// anonymous, and the client is not served.
///
/// Kills: a verifier that lets a failed certificate through as no certificate.
#[tokio::test]
async fn a_certificate_of_another_ca_is_a_handshake_error() {
    let (ca, other) = (ca("farm CA"), ca("other CA"));
    let (server, stranger) = (server_leaf(&ca), client_leaf(&other, "node-1.farm"));
    let done = handshake(
        config(&server, &ca, false).expect("config"),
        client_config(&ca, Some(&stranger)),
    )
    .await;
    assert!(done.server.is_err(), "the server accepted a stranger");
    assert!(done.client.is_err(), "the client was served");
}

/// A certificate that was not issued for client authentication is refused as well.
///
/// Kills: a verifier that ignores the extended key usage.
#[tokio::test]
async fn a_server_only_certificate_is_not_a_client_certificate() {
    let ca = ca("usage CA");
    let server = server_leaf(&ca);
    let wrong = leaf(
        &ca,
        "node-1.farm",
        rcgen::ExtendedKeyUsagePurpose::ServerAuth,
        false,
    );
    let done = handshake(
        config(&server, &ca, false).expect("config"),
        client_config(&ca, Some(&wrong)),
    )
    .await;
    assert!(done.server.is_err());
    assert!(done.client.is_err());
}

/// ALPN picks `h2` when offered and `http/1.1` otherwise, and early data is off.
///
/// Kills: ALPN `h2` only; early data enabled.
#[tokio::test]
async fn alpn_is_h2_then_http11_and_early_data_is_off() {
    let ca = ca("alpn CA");
    let server = server_leaf(&ca);
    let config = config(&server, &ca, false).expect("config");
    assert_eq!(
        config.alpn_protocols,
        [b"h2".to_vec(), b"http/1.1".to_vec()]
    );
    assert_eq!(config.max_early_data_size, 0);

    let mut offers_both = (*client_config(&ca, None)).clone();
    offers_both.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    let done = handshake(Arc::clone(&config), Arc::new(offers_both)).await;
    assert_eq!(done.alpn.as_deref(), Some(&b"h2"[..]));

    let done = handshake(config, client_config(&ca, None)).await;
    assert_eq!(done.alpn.as_deref(), Some(&b"http/1.1"[..]));
}

/// Files that do not make a configuration are refused, each for its own reason.
///
/// Kills: an error path that swallows the reason or accepts the files.
#[test]
fn files_that_do_not_make_a_configuration_are_refused() {
    let ca = ca("refusal CA");
    let server = server_leaf(&ca);
    let other = server_leaf(&ca);
    let make = |cert: &str, key: &str, roots: &str| {
        server_config_optional_client(cert.as_bytes(), key.as_bytes(), roots.as_bytes(), false)
    };

    assert!(matches!(
        make("", &server.key, &ca.pem),
        Err(TlsError::Pem { .. })
    ));
    assert!(matches!(
        make(&server.cert, "not a key", &ca.pem),
        Err(TlsError::Pem { .. })
    ));
    assert!(matches!(
        make(&server.cert, &server.key, ""),
        Err(TlsError::Pem { .. })
    ));
    // A bundle of one certificate that is not a CA certificate parses but is no trust
    // anchor: no usable CA.
    let garbage = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
    assert!(matches!(
        make(&server.cert, &server.key, garbage),
        Err(TlsError::Pem { .. })
    ));
    // The key of another leaf.
    assert!(matches!(
        make(&server.cert, &other.key, &ca.pem),
        Err(TlsError::Config(_))
    ));
    // The "leaf" is not an X.509 certificate.
    assert!(matches!(
        make(garbage, &server.key, &ca.pem),
        Err(TlsError::Pem { .. })
    ));
}

/// A reload refuses an expired leaf; the first load takes it.
///
/// Kills: `reject_expired` ignored in either direction.
#[test]
fn an_expired_leaf_is_refused_only_on_request() {
    let ca = ca("expiry CA");
    let expired = leaf(
        &ca,
        "localhost",
        rcgen::ExtendedKeyUsagePurpose::ServerAuth,
        true,
    );
    assert!(matches!(
        config(&expired, &ca, true),
        Err(TlsError::Expired { .. })
    ));
    assert!(config(&expired, &ca, false).is_ok());
}
