//! The registration listener over real mutual TLS: who may register, and what a
//! registration does to the registry.

mod support;

use std::time::Duration;

use kbf_gateway::registry::Leader;
use kbf_proto::gateway::RegisterRequest;
use support::{Pki, TTL, start};
use tonic::Code;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint};

fn request(address: &str, is_leader: bool, term: u64) -> RegisterRequest {
    RegisterRequest {
        reapi_address: address.to_owned(),
        is_leader,
        term,
    }
}

/// Catches: a server certificate that cannot register; a registration that is accepted
/// but not recorded (name from the certificate, address, role and term from the call);
/// the answer not saying how long the entry lives.
#[tokio::test]
async fn a_server_certificate_registers() {
    let pki = Pki::new();
    let gateway = start(&pki);
    let leaders = gateway.registry.subscribe();
    let mut client = gateway.registrar(&pki.server("kbf1.farm.test"));
    let answer = client
        .register(request("192.0.2.10:8980", true, 7))
        .await
        .expect("a server may register")
        .into_inner();
    assert_eq!(answer.ttl_ms, u64::try_from(TTL.as_millis()).expect("ms"));
    assert_eq!(
        *leaders.borrow(),
        Some(Leader {
            name: "kbf1.farm.test".to_owned(),
            address: "192.0.2.10:8980".parse().expect("address"),
            term: 7,
        })
    );
}

/// Catches: a daemon's certificate (same CA, clientAuth only) registering a server, a
/// certificate with no extended key usage doing so, and a refused call still leaving an
/// entry behind.
#[tokio::test]
async fn a_daemon_certificate_is_refused() {
    let pki = Pki::new();
    let gateway = start(&pki);
    let mut daemon = gateway.registrar(&pki.daemon("node-1"));
    let refused = daemon
        .register(request("192.0.2.10:8980", true, 99))
        .await
        .expect_err("a daemon is not a server");
    assert_eq!(refused.code(), Code::PermissionDenied, "{refused}");

    let bare = pki.leaf(&["kbf9.farm.test"], Vec::new());
    let mut unmarked = gateway.registrar(&bare);
    let refused = unmarked
        .register(request("192.0.2.11:8980", true, 99))
        .await;
    assert!(refused.is_err(), "no extended key usage is no server role");
    assert!(gateway.registry.servers().is_empty());
}

/// Catches: a caller with no certificate, or one from another CA, getting through TLS
/// (the handshake must fail before any call), and an address that is not `ip:port`
/// being accepted (Envoy would be sent a name it cannot connect to).
#[tokio::test]
async fn callers_outside_the_farm_ca_and_bad_addresses_are_refused() {
    let pki = Pki::new();
    let gateway = start(&pki);

    let no_certificate = Endpoint::from_shared(format!("https://{}", gateway.registration))
        .expect("endpoint")
        .tls_config(
            ClientTlsConfig::new()
                .ca_certificate(Certificate::from_pem(&gateway.ca))
                .domain_name("localhost"),
        )
        .expect("TLS")
        .connect_timeout(Duration::from_secs(30));
    let mut client = kbf_proto::gateway::registration_client::RegistrationClient::new(
        no_certificate.connect_lazy(),
    );
    let refused = client.register(request("192.0.2.10:8980", true, 1)).await;
    assert!(refused.is_err(), "mutual TLS needs a client certificate");

    let other_farm = Pki::new();
    let mut stranger = gateway.registrar(&other_farm.server("kbf1.farm.test"));
    let refused = stranger.register(request("192.0.2.10:8980", true, 1)).await;
    assert!(refused.is_err(), "a certificate of another CA");

    let mut client = gateway.registrar(&pki.server("kbf1.farm.test"));
    let refused = client
        .register(request("kbf1.farm.test:8980", true, 1))
        .await
        .expect_err("a name is not an address");
    assert_eq!(refused.code(), Code::InvalidArgument, "{refused}");
    assert!(gateway.registry.servers().is_empty());
}

/// Catches: the same server registering again adding a second entry (a restart, or a
/// heartbeat), and a follower's registration taking the role.
#[tokio::test]
async fn a_server_registers_again_and_a_follower_does_not_lead() {
    let pki = Pki::new();
    let gateway = start(&pki);
    let mut one = gateway.registrar(&pki.server("kbf1.farm.test"));
    let mut two = gateway.registrar(&pki.server("kbf2.farm.test"));
    one.register(request("192.0.2.10:8980", true, 3))
        .await
        .expect("one");
    one.register(request("192.0.2.10:8980", true, 3))
        .await
        .expect("one again");
    two.register(request("192.0.2.20:8980", false, 9))
        .await
        .expect("two");
    assert_eq!(
        gateway.registry.servers(),
        ["kbf1.farm.test", "kbf2.farm.test"]
    );
    let leader = gateway
        .registry
        .subscribe()
        .borrow()
        .clone()
        .expect("a leader");
    assert_eq!(leader.name, "kbf1.farm.test");
}
