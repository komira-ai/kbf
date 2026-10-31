//! The coordinator over real sockets: mutual TLS with certificates the farm CA issues, the
//! OIDC call and the status over TLS, and what a restart keeps.

mod support;

use std::sync::Arc;
use std::time::Duration;

use kbf_types::HostRole;
use support::{BEAT, Farm, OPERATOR, Pki, STATE, ci_token, client_config};

/// Catches: an enrolled updater not being served over mutual TLS, or being told the wrong
/// thing: with no desired state it is told nothing, and once CI has set one, a host behind
/// is told to update to that version and no more.
#[tokio::test]
async fn an_enrolled_updater_is_answered_over_mutual_tls() {
    let farm = Farm::start("farm-mtls").await;
    let who = farm.pki.updater("d1", HostRole::DAEMON);
    farm.at(2_000);
    let (code, body) = farm.beat(&who, BEAT).await.expect("answered");
    assert_eq!((code, body.as_str()), (200, r#"{"reply":"nothing"}"#));
    let (code, _) = farm
        .control("POST", "/v1/desired", Some(&ci_token()), STATE)
        .await
        .expect("answered");
    assert_eq!(code, 200);
    let (code, body) = farm.beat(&who, BEAT).await.expect("answered");
    assert_eq!(
        (code, body.as_str()),
        (200, r#"{"reply":"update","version":"2.0"}"#)
    );
}

/// Catches: a connection served that TLS should have refused: a client with no certificate,
/// one with a certificate from another CA, and one whose certificate may not be used as a
/// client's (the coordinator's own); each fails before an HTTP answer.
#[tokio::test]
async fn a_client_without_a_certificate_of_this_farm_is_not_served() {
    let farm = Farm::start("farm-forged").await;
    farm.at(2_000);
    let anonymous = client_config(&farm.pki, None);
    let refused = support::exchange(
        farm.bound.heartbeat,
        &anonymous,
        "POST",
        "/v1/heartbeat",
        None,
        BEAT,
    );
    assert!(refused.await.is_err(), "no client certificate");

    let other = Pki::new();
    let forged = other.updater("d1", HostRole::DAEMON);
    assert!(
        farm.beat(&forged, BEAT).await.is_err(),
        "another CA's certificate"
    );

    let server_only = farm.pki.coordinator();
    assert!(
        farm.beat(&server_only, BEAT).await.is_err(),
        "serverAuth only"
    );
    assert_eq!(farm.status().await["hosts"], serde_json::json!([]));
}

/// Catches: a certificate of the farm's own that is not an updater's being taken for one: a
/// storage node's certificate chains to the CA and passes TLS, and is refused 403.
#[tokio::test]
async fn a_farm_certificate_that_is_not_an_updaters_is_refused() {
    let farm = Farm::start("farm-storage-cert").await;
    let storage = farm.pki.storage_node("s1");
    let (code, body) = farm.beat(&storage, BEAT).await.expect("answered");
    assert_eq!((code, body.as_str()), (403, r#"{"error":"forbidden"}"#));
    assert_eq!(farm.status().await["hosts"], serde_json::json!([]));
}

/// Catches: the control listener honouring a credential it should not: over TLS the operator
/// token is refused for the OIDC call, the CI token for the status, and a client
/// certificate is not asked for; the status lists what the updaters said.
#[tokio::test]
async fn ci_sets_the_state_and_the_operator_reads_it_over_https() {
    let farm = Farm::start("farm-control").await;
    let (code, body) = farm
        .control("POST", "/v1/desired", Some(OPERATOR), STATE)
        .await
        .expect("answered");
    assert_eq!((code, body.as_str()), (401, r#"{"error":"unauthorized"}"#));
    let (code, _) = farm
        .control("GET", "/v1/status", Some(&ci_token()), "")
        .await
        .expect("answered");
    assert_eq!(code, 401);
    let (code, _) = farm
        .control("POST", "/v1/desired", Some(&ci_token()), STATE)
        .await
        .expect("answered");
    assert_eq!(code, 200);
    farm.beat(&farm.pki.updater("d1", HostRole::DAEMON), BEAT)
        .await
        .expect("answered");
    let status = farm.status().await;
    assert_eq!(status["desired"]["version"], "2.0");
    assert_eq!(status["hosts"][0]["node_id"], "d1");
    assert_eq!(status["hosts"][0]["role"], "daemon");
}

/// Catches: the desired state lost by a restart, and the fleet not being rebuilt from
/// heartbeats: the restarted coordinator has the state CI set and no host; it orders nothing
/// for thirty seconds; then a host that heartbeats as updating holds its role's only slot, so
/// a second daemon is not started beside it.
#[tokio::test]
async fn after_a_restart_the_state_stands_and_an_updating_host_is_counted() {
    let farm = Farm::start("farm-restart").await;
    let (code, _) = farm
        .control("POST", "/v1/desired", Some(&ci_token()), STATE)
        .await
        .expect("answered");
    assert_eq!(code, 200);
    let (pki, dir) = farm.stop();

    let farm = Farm::start_on(pki, dir, 5_000, Duration::from_secs(3_600)).await;
    let status = farm.status().await;
    assert_eq!(status["desired"]["version"], "2.0");
    assert_eq!(status["hosts"], serde_json::json!([]));
    assert_eq!(status["quiet"], true);

    let (d1, d2) = (
        farm.pki.updater("d1", HostRole::DAEMON),
        farm.pki.updater("d2", HostRole::DAEMON),
    );
    let busy = BEAT.replace("}", r#","updating":"2.0"}"#);
    let (_, early) = farm.beat(&d1, &busy).await.expect("answered");
    assert_eq!(early, r#"{"reply":"nothing"}"#);
    farm.at(5_030);
    farm.beat(&d1, &busy).await.expect("answered");
    let (_, second) = farm.beat(&d2, BEAT).await.expect("answered");
    assert_eq!(second, r#"{"reply":"nothing"}"#, "d1 holds the daemon slot");
    let done = BEAT.replace("1.0", "2.0");
    farm.beat(&d1, &done).await.expect("answered");
    let (_, second) = farm.beat(&d2, BEAT).await.expect("answered");
    assert_eq!(second, r#"{"reply":"update","version":"2.0"}"#);
}

/// Catches: a listener that stops serving after a bad client: plain text sent to the TLS
/// port, a client that connects and says nothing, and a client that finishes its handshake
/// and sends no request are each dropped, and the next updater is still served.
#[tokio::test]
async fn bad_clients_do_not_stop_the_listener() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let farm = Farm::start("farm-bad-clients").await;
    let mut plain = tokio::net::TcpStream::connect(farm.bound.heartbeat)
        .await
        .expect("connected");
    plain
        .write_all(b"GET / HTTP/1.1\r\n\r\n")
        .await
        .expect("sent");
    let mut sink = Vec::new();
    let _ = plain.read_to_end(&mut sink).await;
    let silent = tokio::net::TcpStream::connect(farm.bound.heartbeat)
        .await
        .expect("connected");
    drop(silent);
    let who = farm.pki.updater("d1", HostRole::DAEMON);
    let (code, _) = farm.beat(&who, BEAT).await.expect("still served");
    assert_eq!(code, 200);
}

/// Catches: a certificate the farm CA renewed not being picked up without a restart: after
/// the files are replaced, new handshakes present the new certificate; and files that do not
/// make a configuration leave the one in use. Time is paused, so the reload interval passes
/// whenever the test waits; the test waits on what the listeners present and on the
/// reloader's own counters.
#[tokio::test(start_paused = true)]
async fn a_renewed_coordinator_certificate_is_served_without_a_restart() {
    let pki = Arc::new(Pki::new());
    let dir = Arc::new(kbf_testkit::dir::TestDir::under(
        std::env::temp_dir(),
        "farm-reload",
    ));
    let farm = Farm::start_on(
        Arc::clone(&pki),
        Arc::clone(&dir),
        1_000,
        Duration::from_millis(50),
    )
    .await;
    let before = presented(&farm, farm.bound.control).await;
    let renewed = pki.coordinator();
    std::fs::write(dir.path().join("coordinator.key"), renewed.key_pem()).expect("written");
    std::fs::write(dir.path().join("coordinator.pem"), renewed.cert_pem()).expect("written");
    let hang_guard = std::time::Instant::now();
    for addr in [farm.bound.control, farm.bound.heartbeat] {
        while presented(&farm, addr).await != renewed.der {
            assert!(
                hang_guard.elapsed() < Duration::from_secs(60),
                "the renewed certificate was never presented"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    assert_ne!(renewed.der, before);
    let rejected = farm.tls.0.reloads().rejected;
    std::fs::write(dir.path().join("coordinator.pem"), "garbage").expect("written");
    while farm.tls.0.reloads().rejected == rejected {
        assert!(
            hang_guard.elapsed() < Duration::from_secs(60),
            "the garbage was never looked at"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(
        presented(&farm, farm.bound.heartbeat).await,
        renewed.der,
        "the one in use stays"
    );
}

/// The leaf a listener presents to a client that trusts the farm CA.
async fn presented(farm: &Farm, addr: std::net::SocketAddr) -> Vec<u8> {
    let config = client_config(&farm.pki, None);
    let tcp = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connected");
    let name = rustls::pki_types::ServerName::try_from("localhost").expect("a name");
    let tls = tokio_rustls::TlsConnector::from(config)
        .connect(name, tcp)
        .await;
    let tls = match tls {
        Ok(tls) => tls,
        Err(e) => panic!("handshake: {e}"),
    };
    tls.get_ref().1.peer_certificates().expect("a chain")[0]
        .as_ref()
        .to_vec()
}
