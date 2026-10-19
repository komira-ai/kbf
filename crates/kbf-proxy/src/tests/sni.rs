//! One port, two server certificates: the one a client is served is chosen by the name it
//! asks for. Every test is a real TLS handshake through the proxy, and the clients verify the
//! certificate against their own root, as a Buck2 client and a build node do.

use std::sync::Arc;

use kbf_tls::CertEntry;

use super::kit::{Access, Rig};
use super::pki::{Pki, client_config};
use super::secure::connect_tls_as;
use crate::auth::Buck2Auth;
use crate::scratch::scratch;
use crate::tls::TlsPaths;

const WORKER: &str = "/kbf.worker.v1.Worker/Session";
const PUBLIC: &str = "reapi.example.com";
const FARM: &str = "farm.internal.example";

/// The public name's certificate comes from a public CA, the farm name's from the farm CA,
/// and the farm CA is also what verifies client certificates.
struct Setup {
    rig: Rig,
    public_ca: Pki,
    farm_ca: Pki,
    public_der: Vec<u8>,
    farm_der: Vec<u8>,
}

async fn setup(test: &str) -> Setup {
    let (public_ca, farm_ca) = (Pki::new("public CA"), Pki::new("farm CA"));
    let (public, farm) = (public_ca.server_named(PUBLIC), farm_ca.server_named(FARM));
    let dir = scratch(test);
    let entry = |stem: &str, leaf: &super::pki::Leaf, name: &str, default: bool| {
        let (cert, key) = (
            dir.join(format!("{stem}.pem")),
            dir.join(format!("{stem}.key")),
        );
        std::fs::write(&cert, &leaf.cert).expect("cert");
        std::fs::write(&key, &leaf.key).expect("key");
        CertEntry {
            names: vec![name.to_owned()],
            cert,
            key,
            default,
        }
    };
    let client_ca = dir.join("client-ca.pem");
    std::fs::write(&client_ca, &farm_ca.ca_pem).expect("client CA");
    let paths = TlsPaths {
        certs: vec![
            entry("public", &public, PUBLIC, true),
            entry("farm", &farm, FARM, false),
        ],
        client_ca,
    };
    let rig = Rig::start_secure(Buck2Auth::None, Access::Tls(paths)).await;
    Setup {
        rig,
        public_ca,
        farm_ca,
        public_der: public.der,
        farm_der: farm.der,
    }
}

fn served<T>(tls: &tokio_rustls::client::TlsStream<T>) -> Vec<u8> {
    tls.get_ref().1.peer_certificates().expect("a certificate")[0]
        .as_ref()
        .to_vec()
}

/// A Buck2 client (trusts only the public root) asking for the public name and a build node
/// (trusts only the farm CA) asking for the farm name each verify the certificate they are
/// given, and each fails against the other's. Two clients, two roots, one port.
///
/// Kills: one certificate for every name: whichever it is, the client of the other root fails
/// to verify it.
#[tokio::test(start_paused = true)]
async fn two_clients_with_different_roots_each_verify_their_names_certificate() {
    let s = setup("sni-two-roots").await;
    let buck2 = || client_config(&s.public_ca.ca_pem, None, &["h2"]);
    let node_cert = s.farm_ca.client(&["node-1.farm"], Some("daemon"));
    let node = || client_config(&s.farm_ca.ca_pem, Some(&node_cert), &["h2"]);

    let tls = connect_tls_as(&s.rig, buck2(), PUBLIC)
        .await
        .expect("buck2");
    assert_eq!(served(&tls), s.public_der);
    let tls = connect_tls_as(&s.rig, node(), FARM).await.expect("node");
    assert_eq!(served(&tls), s.farm_der);

    // Each name's certificate is not acceptable to the other root.
    assert!(connect_tls_as(&s.rig, buck2(), FARM).await.is_err());
    assert!(connect_tls_as(&s.rig, node(), PUBLIC).await.is_err());

    // And both are the same proxy: the node is served a worker call through the farm name.
    let mut client = h2_over_tls_as(&s, node(), FARM).await;
    assert_eq!(client.unary(WORKER).await.text(), "a");
}

async fn h2_over_tls_as(
    s: &Setup,
    config: Arc<rustls::ClientConfig>,
    host: &str,
) -> super::kit::Client {
    let tls = connect_tls_as(&s.rig, config, host)
        .await
        .expect("handshake");
    let (send, connection) = super::kit::client_builder()
        .handshake(tls)
        .await
        .expect("h2");
    super::kit::Client::new(send, tokio::spawn(connection))
}

/// A ClientHello with no server name, and one with a name nobody configured, are served the
/// default, which the configuration marks as the public name's certificate.
///
/// Kills: a default taken from the last entry; an unknown name that fails the handshake
/// instead of getting the default.
#[tokio::test(start_paused = true)]
async fn no_name_and_an_unknown_name_are_served_the_default() {
    let s = setup("sni-default").await;
    // No SNI: the client verifies against the public name but does not send it.
    let mut quiet = client_config(&s.public_ca.ca_pem, None, &["h2"]);
    Arc::get_mut(&mut quiet).expect("unshared").enable_sni = false;
    let tls = connect_tls_as(&s.rig, quiet, PUBLIC).await.expect("no SNI");
    assert_eq!(served(&tls), s.public_der);

    // An unknown name: the certificate is the default's, so only a client that does not check
    // names (the public root has no other name to offer) sees which one; it fails to verify
    // here, which is the right answer for a name the certificate does not carry, and the
    // failure is the client's, after the proxy sent the default.
    let other = connect_tls_as(
        &s.rig,
        client_config(&s.public_ca.ca_pem, None, &["h2"]),
        "unknown.example.com",
    )
    .await;
    assert!(other.is_err());
    let farm_client = connect_tls_as(
        &s.rig,
        client_config(&s.farm_ca.ca_pem, None, &["h2"]),
        "unknown.example.com",
    )
    .await;
    assert!(
        farm_client.is_err(),
        "the default is not the farm certificate"
    );
}

/// A name in another case is the same name.
///
/// Kills: a lookup by the exact bytes of the name.
#[tokio::test(start_paused = true)]
async fn the_name_matches_without_regard_to_case() {
    let s = setup("sni-case").await;
    let node = s.farm_ca.client(&["node-1.farm"], Some("daemon"));
    let config = client_config(&s.farm_ca.ca_pem, Some(&node), &["h2"]);
    let tls = connect_tls_as(&s.rig, config, "FARM.Internal.EXAMPLE")
        .await
        .expect("handshake");
    assert_eq!(served(&tls), s.farm_der);
}

/// Client certificates are asked for, verified and required exactly as before, whichever name
/// the client used: a worker call with no certificate is `UNAUTHENTICATED` on both names, a
/// farm-CA certificate is served on both, and a certificate of another CA fails the handshake
/// on both.
///
/// Kills: a client verifier that depends on the name (none on one of them); a name that skips
/// the per-path certificate requirement.
#[tokio::test(start_paused = true)]
async fn client_certificates_are_required_where_they_were_on_both_names() {
    let s = setup("sni-mtls").await;
    let node = s.farm_ca.client(&["node-1.farm"], Some("daemon"));
    let stranger = Pki::new("other CA").client(&["node-9.farm"], Some("daemon"));
    for (host, trust) in [(PUBLIC, &s.public_ca.ca_pem), (FARM, &s.farm_ca.ca_pem)] {
        let mut anonymous = h2_over_tls_as(&s, client_config(trust, None, &["h2"]), host).await;
        let reply = anonymous.unary(WORKER).await;
        assert_eq!(
            reply.grpc_status().as_deref(),
            Some("16"),
            "{host}: {reply:?}"
        );

        let mut verified =
            h2_over_tls_as(&s, client_config(trust, Some(&node), &["h2"]), host).await;
        assert_eq!(verified.unary(WORKER).await.text(), "a", "{host}");

        let mut tls = connect_tls_as(&s.rig, client_config(trust, Some(&stranger), &["h2"]), host)
            .await
            .expect("the client side completes");
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let _ = tls.write_all(b"x").await;
        let mut buf = [0_u8; 64];
        assert!(matches!(tls.read(&mut buf).await, Err(_) | Ok(0)), "{host}");
    }
    assert_eq!(s.rig.a.state.dispatches(), 2);
}
