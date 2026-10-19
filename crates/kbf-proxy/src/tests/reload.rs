//! Reloading the certificate, key and client CA: as one unit, all or nothing.

use std::sync::atomic::Ordering;

use super::kit::{Access, CAPS, Rig};
use super::pki::{Pki, client_config};
use super::secure::{connect_tls, h2_over_tls, tls_rig};
use crate::auth::Buck2Auth;
use crate::scratch::scratch;

/// The DER of the certificate the server presented.
fn served<T>(tls: &tokio_rustls::client::TlsStream<T>) -> Vec<u8> {
    tls.get_ref().1.peer_certificates().expect("a certificate")[0]
        .as_ref()
        .to_vec()
}

fn failed(rig: &Rig) -> u64 {
    rig.core.metrics.tls_reload_failed.load(Ordering::Relaxed)
}

/// A reload replaces certificate, key and client CA together: a handshake that starts
/// afterwards is served the new certificate and verifies against the new CA (and not the old
/// one), while a connection open across the reload keeps working on what it was accepted under.
///
/// Kills: a reload that never swaps; one that swaps the certificate but keeps the old CA (a
/// half-applied unit); one that ends the old connections.
#[tokio::test(start_paused = true)]
async fn a_reload_applies_certificate_key_and_ca_as_one_unit() {
    let (rig, old, paths) = tls_rig("reload-unit", Buck2Auth::None).await;
    let old_client = old.client(&["node-1.farm"], Some("daemon"));
    let mut open = h2_over_tls(&rig, client_config(&old.ca_pem, Some(&old_client), &["h2"])).await;
    assert_eq!(open.unary(CAPS).await.text(), "a");

    let new = Pki::new("renewed farm CA");
    let leaf = new.server();
    new.install(&paths, &leaf, &new.ca_pem);
    rig.tls
        .as_ref()
        .expect("TLS")
        .reload()
        .expect("a good unit");

    let new_client = new.client(&["node-2.farm"], Some("daemon"));
    let config = client_config(&new.ca_pem, Some(&new_client), &["h2"]);
    let tls = connect_tls(&rig, config).await.expect("the new unit");
    assert_eq!(served(&tls), leaf.der, "the new certificate");
    let mut fresh = h2_over_tls(&rig, client_config(&new.ca_pem, Some(&new_client), &["h2"])).await;
    assert_eq!(
        fresh.unary(CAPS).await.text(),
        "a",
        "a client of the new CA is verified"
    );

    assert!(
        connect_tls(&rig, client_config(&old.ca_pem, None, &["h2"]))
            .await
            .is_err(),
        "the old CA no longer vouches for the server"
    );
    assert_eq!(
        open.unary(CAPS).await.text(),
        "a",
        "the open connection lives on"
    );
    assert_eq!(failed(&rig), 0);
}

/// A reload that fails in any part leaves the old unit serving, counts, and returns the error;
/// the next good reload applies.
///
/// Kills: a failed reload that applies the part that parsed (a half-applied unit), drops the
/// old configuration, or goes uncounted.
#[tokio::test(start_paused = true)]
async fn a_failed_reload_keeps_the_old_unit_and_counts() {
    let (rig, old, paths) = tls_rig("reload-failed", Buck2Auth::None).await;
    let reload = || rig.tls.as_ref().expect("TLS").reload();
    let old_der = {
        let tls = connect_tls(&rig, client_config(&old.ca_pem, None, &["h2"]))
            .await
            .expect("tls");
        served(&tls)
    };
    let old_client = old.client(&["node-1.farm"], Some("daemon"));

    // A good new certificate, a client CA file that is not a CA.
    let new = Pki::new("renewed farm CA");
    new.install(&paths, &new.server(), "not a CA bundle");
    assert!(reload().is_err());
    // A good new CA, a certificate whose key is not in the key file.
    let (other, leaf) = (Pki::new("third CA"), new.server());
    let mismatched = crate::tests::pki::Leaf {
        key: other.server().key,
        ..leaf
    };
    new.install(&paths, &mismatched, &new.ca_pem);
    assert!(reload().is_err());
    // A file that is not there.
    std::fs::remove_file(&paths.client_ca).expect("remove");
    let missing = reload().expect_err("a missing file");
    assert!(
        matches!(missing, crate::TlsReloadError::Read { .. }),
        "{missing}"
    );
    assert_eq!(failed(&rig), 3);

    // Still the old certificate and the old CA.
    let tls = connect_tls(&rig, client_config(&old.ca_pem, Some(&old_client), &["h2"]))
        .await
        .expect("the old unit still serves");
    assert_eq!(served(&tls), old_der);
    let mut served_client =
        h2_over_tls(&rig, client_config(&old.ca_pem, Some(&old_client), &["h2"])).await;
    assert_eq!(
        served_client.unary(CAPS).await.text(),
        "a",
        "the old CA still verifies"
    );

    let text = rig.core.metrics.render(&rig.core.slots, &rig.core.queue);
    assert!(
        text.contains("kbf_proxy_tls_reload_failed_total 3"),
        "{text}"
    );

    // The repaired unit applies.
    new.install(&paths, &new.server(), &new.ca_pem);
    reload().expect("the repaired unit");
    assert_eq!(failed(&rig), 3);
}

/// An expired certificate is accepted at the first load (as a server has always started) and
/// refused by a reload.
///
/// Kills: a reload that swaps in an expired certificate.
#[tokio::test(start_paused = true)]
async fn a_reload_refuses_an_expired_certificate() {
    let pki = Pki::new("expiry CA");
    let paths = pki.write(&scratch("reload-expired"), &pki.server());
    let rig = Rig::start_secure(Buck2Auth::None, Access::Tls(paths.clone())).await;
    pki.install(&paths, &pki.expired_server(), &pki.ca_pem);
    assert!(rig.tls.as_ref().expect("TLS").reload().is_err());
    assert_eq!(failed(&rig), 1);
}

/// The first load accepts an expired certificate (a server has always started on one and a
/// renewal catches up); only a reload refuses it.
///
/// Kills: a first load that refuses an expired certificate.
#[test]
fn the_first_load_accepts_an_expired_certificate() {
    let pki = Pki::new("first-load CA");
    let paths = pki.write(&scratch("first-expired"), &pki.expired_server());
    let metrics = std::sync::Arc::new(crate::metrics::Metrics::default());
    assert!(crate::tls::TlsReloader::load(paths, metrics).is_ok());
}
