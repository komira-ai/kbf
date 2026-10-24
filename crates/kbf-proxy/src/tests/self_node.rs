//! A host's updater at the proxy: the three calls about its own node (drain, uncordon and
//! status) with its updater certificate and no token, and nothing else. The child gets the
//! operator token the proxy holds and never anything the host sent; every other call, other
//! node, other route and other certificate role is refused before a child is asked.
//!
//! The matrix gives every connection the identity a verified certificate would have had, so it
//! is fast; one test makes the real handshake with a certificate of a throwaway CA.

use kbf_peer::PeerRole;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::kit::{Access, CAPS, CLIENT_IP, OPERATOR_TOKEN, SECRET};
use super::pki::client_config;
use super::pool_kit::Rig;
use super::secure::{connect_tls, h2_over_tls, tls_rig};
use crate::auth::Buck2Auth;
use crate::identity::Identity;

const DRAIN: &str = "/v1/nodes/host-1:drain";
const UNCORDON: &str = "/v1/nodes/host-1:uncordon";
const STATUS: &str = "/v1/nodes/host-1";
/// What a host sends in place of the token: a value the proxy must not pass on.
const HOST_TOKEN: &str = "whatever-the-host-holds-0123456789abcdef";

fn identity(role: PeerRole, node: &str) -> Identity {
    Identity {
        node_id: node.into(),
        role,
        ips: Vec::new(),
    }
}

fn rig(mode: Buck2Auth, role: PeerRole, node: &str) -> Rig {
    Rig::start_secure(mode, Access::Plain(Some(identity(role, node))))
}

fn authorization(rig: &Rig, path: &str) -> Vec<Option<String>> {
    rig.a()
        .state
        .headers_of(path)
        .iter()
        .map(|h| {
            h.get("authorization")
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        })
        .collect()
}

/// The three calls reach the child, whatever the host put in `Authorization` (nothing, or a
/// token of its own), and the child sees the operator token and not the host's.
#[tokio::test(start_paused = true)]
async fn an_updater_drains_uncordons_and_reads_its_own_node() {
    for host_sends in [None, Some(HOST_TOKEN)] {
        let rig = rig(Buck2Auth::None, PeerRole::Updater, "host-1");
        let mut client = rig.client().await;
        client.bearer = host_sends.map(str::to_owned);
        for (method, path, body) in [
            ("POST", DRAIN, &b"{\"deadline_secs\":600}"[..]),
            ("GET", STATUS, b""),
            ("POST", UNCORDON, b""),
        ] {
            let reply = client.plain(method, path, body).await;
            assert_eq!(reply.status, Some(200), "{method} {path} {host_sends:?}");
            assert_eq!(
                authorization(&rig, path),
                [Some(format!("Bearer {OPERATOR_TOKEN}"))],
                "{method} {path}"
            );
        }
        assert_eq!(rig.a().state.dispatches(), 3);
    }
}

/// An updater is refused every other call before the child is asked, with the operator token
/// beside its certificate or not: another node's id (every spelling of it), the cordon, a
/// listing, the wrong method, a query, the update API, the operator API at large, the metrics
/// and the worker and build-cache paths.
#[tokio::test(start_paused = true)]
async fn an_updater_is_refused_everything_but_its_own_nodes_three_calls() {
    let calls: [(&str, &str); 22] = [
        ("POST", "/v1/nodes/host-2:drain"),
        ("POST", "/v1/nodes/host-2:uncordon"),
        ("GET", "/v1/nodes/host-2"),
        ("POST", "/v1/nodes/host-1:cordon"),
        ("GET", "/v1/nodes"),
        ("GET", "/v1/nodes/host-1/leases"),
        ("POST", "/v1/nodes/host-1"),
        ("GET", "/v1/nodes/host-1:drain"),
        ("DELETE", "/v1/nodes/host-1"),
        ("GET", "/v1/nodes/host-1?state=up"),
        ("POST", "/v1/nodes/host-1:drain?reason=x"),
        ("POST", "/v1/nodes/HOST-1:drain"),
        ("POST", "/v1/nodes/host-1%3Adrain"),
        ("POST", "/v1/nodes/host-1/../host-2:drain"),
        ("GET", "/v1/status"),
        ("GET", "/v1/enroll/tokens"),
        ("POST", "/v1/update/desired"),
        ("POST", "/v1/update/frontend"),
        ("GET", "/metrics"),
        ("POST", "/kbf.worker.v1.Worker/Session"),
        ("POST", "/kbf.enroll.v1.ColdStore/GetKeys"),
        ("POST", CAPS),
    ];
    for token in [None, Some(OPERATOR_TOKEN)] {
        for mode in [Buck2Auth::None, Buck2Auth::Mtls] {
            let rig = rig(mode, PeerRole::Updater, "host-1");
            let mut client = rig.client().await;
            client.bearer = token.map(str::to_owned);
            for (method, path) in calls {
                let reply = client.plain(method, path, b"").await;
                // An HTTP path answers 403; a gRPC path answers 200 with the gRPC status 7
                // (permission denied).
                let refused = reply.status == Some(403)
                    || (reply.status == Some(200) && reply.grpc_status().as_deref() == Some("7"));
                assert!(refused, "{method} {path} {mode:?} {token:?}: {reply:?}");
            }
            assert_eq!(rig.a().state.dispatches(), 0, "the child was asked");
        }
    }
}

/// The probes and the read-only UI stay open to an updater's certificate as to anyone; the
/// proxy answers them itself.
#[tokio::test(start_paused = true)]
async fn an_updater_still_reaches_what_is_open_to_everyone() {
    let rig = rig(Buck2Auth::None, PeerRole::Updater, "host-1");
    let mut client = rig.client().await;
    client.bearer = None;
    assert_eq!(client.plain("GET", "/healthz", b"").await.status, Some(200));
}

/// A certificate of any other role gets none of this: with no token its calls on the node routes
/// are unauthenticated, as before, and the proxy puts no token in for it; with a wrong token
/// they are too.
#[tokio::test(start_paused = true)]
async fn node_and_frontend_certificates_get_no_scoped_calls() {
    for role in [PeerRole::Daemon, PeerRole::Storage, PeerRole::Frontend] {
        let rig = rig(Buck2Auth::None, role, "host-1");
        let mut client = rig.client().await;
        for bearer in [None, Some(HOST_TOKEN.to_owned())] {
            client.bearer = bearer.clone();
            for (method, path) in [("POST", DRAIN), ("GET", STATUS), ("POST", UNCORDON)] {
                let reply = client.plain(method, path, b"").await;
                assert_eq!(
                    reply.status,
                    Some(401),
                    "{role:?} {method} {path} {bearer:?}"
                );
            }
        }
        assert_eq!(rig.a().state.dispatches(), 0, "{role:?}");
    }
}

/// With a real handshake: an updater certificate of the farm's CA is accepted at the door, its
/// role and node id reach the child as the identity headers, its calls are admitted for its
/// node only, and the host's own `Authorization` is replaced. A coordinator's certificate (a
/// role the proxy serves nothing) closes the connection.
#[tokio::test(start_paused = true)]
async fn an_updater_certificate_is_admitted_at_the_door_for_its_own_node() {
    let (rig, pki, _) = tls_rig("self-node-e2e", Buck2Auth::None).await;
    let updater = pki.client(&["host-1"], Some("updater"));
    let mut client = h2_over_tls(&rig, client_config(&pki.ca_pem, Some(&updater), &["h2"])).await;
    client.bearer = Some(HOST_TOKEN.to_owned());
    let reply = client.plain("POST", DRAIN, b"{}").await;
    assert_eq!(reply.status, Some(200), "{reply:?}");
    let seen = rig.a().state.headers_of(DRAIN);
    assert_eq!(seen.len(), 1);
    let header = |name: &str| {
        seen[0]
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned)
    };
    assert_eq!(
        header("authorization"),
        Some(format!("Bearer {OPERATOR_TOKEN}"))
    );
    assert_eq!(header("x-kbf-role").as_deref(), Some("updater"));
    assert_eq!(header("x-kbf-node-id").as_deref(), Some("host-1"));
    assert_eq!(header("x-kbf-proxy-auth"), Some(hex::encode(SECRET)));
    assert_eq!(header("x-kbf-client-addr"), Some(CLIENT_IP.to_string()));

    let other = client.plain("POST", "/v1/nodes/host-2:drain", b"{}").await;
    assert_eq!(other.status, Some(403));
    assert_eq!(rig.a().state.dispatches(), 1);

    let coordinator = pki.client(&["coordinator.farm"], Some("coordinator"));
    let config = client_config(&pki.ca_pem, Some(&coordinator), &["h2"]);
    let mut tls = connect_tls(&rig, config).await.expect("the handshake");
    let _ = tls.write_all(b"x").await;
    let mut buf = [0_u8; 16];
    assert!(matches!(tls.read(&mut buf).await, Err(_) | Ok(0)));
}
