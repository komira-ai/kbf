//! Who may call what, and who the child is told the caller is: the auth matrix over every
//! route class, and the TLS terminator end to end (handshake, identity, headers at a fake
//! child that records them, HTTP/1.1, reload).
//!
//! The matrix uses a listener that hands every connection the identity the test wants a
//! verified certificate to have had, so it is fast and covers every row. The TLS tests make
//! real handshakes with certificates of a throwaway CA over in-memory pipes, so what is
//! checked is what a client and a child experience.

use std::io;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use kbf_peer::PeerRole;
use rustls::pki_types::ServerName;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio_rustls::TlsConnector;
use tokio_rustls::client::TlsStream;

use super::kit::{
    Access, BUCK2_TOKEN, CAPS, Client, OPERATOR, OPERATOR_TOKEN, Rig, SECRET, client_builder,
    finish,
};
use super::pki::{Pki, client_config};
use crate::auth::Buck2Auth;
use crate::identity::Identity;
use crate::route::{Route, route_of};
use crate::scratch::scratch;

const WORKER: &str = "/kbf.worker.v1.Worker/Session";
const ENROLL: &str = "/kbf.enroll.v1.Enroll/Enroll";
const COLD: &str = "/kbf.enroll.v1.ColdStore/GetKeys";
const UPDATE: &str = "/v1/update/frontend";
const WRONG: &str = "wrong-token-0123456789-abcdefghijklmnopq";

fn identity(role: PeerRole) -> Identity {
    Identity {
        node_id: "node-1.farm".into(),
        role,
    }
}

/// What a call came to: `"ok"` (it was let through; for a path the child serves, the child
/// answered), or the gRPC code of the refusal: 16, 7 or 8 (HTTP 401, 403, 429 are the same
/// three).
async fn call(client: &mut Client, path: &str) -> String {
    let (status, grpc) = if route_of(path).grpc() {
        let reply = client.unary(path).await;
        (reply.status, reply.grpc_status())
    } else {
        (client.plain("GET", path, b"").await.status, None)
    };
    match (status, grpc.as_deref()) {
        (Some(401), _) | (_, Some("16")) => "16".into(),
        (Some(403), _) | (_, Some("7")) => "7".into(),
        (Some(429), _) | (_, Some("8")) => "8".into(),
        _ => "ok".into(),
    }
}

/// One cell of the matrix: a proxy in `mode` whose client presented `role` (a verified
/// certificate, or none) and `bearer`, calling `path`. Also checks the child saw the call
/// if and only if it was let through to a child path.
async fn cell(mode: Buck2Auth, role: Option<PeerRole>, bearer: Option<&str>, path: &str) -> String {
    let rig = Rig::start_secure(mode, Access::Plain(role.map(identity))).await;
    let mut client = rig.client().await;
    client.bearer = bearer.map(str::to_owned);
    let got = call(&mut client, path).await;
    let reached = rig.a.state.dispatches();
    let expected = usize::from(got == "ok" && matches!(route_of(path), Route::Child(_)));
    assert_eq!(
        reached, expected,
        "{path} {mode:?} {role:?} {bearer:?}: {got}"
    );
    got
}

type Row = (
    Buck2Auth,
    Option<PeerRole>,
    Option<&'static str>,
    &'static str,
);

async fn check(path: &str, rows: &[Row]) {
    for &(mode, role, bearer, want) in rows {
        let got = cell(mode, role, bearer, path).await;
        assert_eq!(got, want, "{path} {mode:?} {role:?} {bearer:?}");
    }
}

const OP: Option<&str> = Some(OPERATOR_TOKEN);
const BK: Option<&str> = Some(BUCK2_TOKEN);
const BAD: Option<&str> = Some(WRONG);
const NO: Option<&str> = None;
const DAEMON: Option<PeerRole> = Some(PeerRole::Daemon);
const STORAGE: Option<PeerRole> = Some(PeerRole::Storage);
const FRONTEND: Option<PeerRole> = Some(PeerRole::Frontend);
const ANON: Option<PeerRole> = None;

/// Worker and storage paths: a verified certificate of a worker or storage role, in every
/// mode, and no token stands in for it.
///
/// Kills: a wrong-role certificate admitted; a token or no certificate admitted.
#[tokio::test(start_paused = true)]
async fn worker_paths_need_a_worker_or_storage_certificate() {
    use Buck2Auth::{Bearer, Mtls, None as Open};
    check(
        WORKER,
        &[
            (Open, ANON, NO, "16"),
            (Open, DAEMON, NO, "ok"),
            (Open, STORAGE, NO, "ok"),
            (Open, FRONTEND, NO, "7"),
            (Open, DAEMON, OP, "ok"),
            (Open, ANON, OP, "16"),
            (Mtls, DAEMON, NO, "ok"),
            (Bearer, STORAGE, NO, "ok"),
            (Bearer, ANON, BK, "16"),
            (Bearer, FRONTEND, BK, "7"),
        ],
    )
    .await;
}

/// The cold-store keys are on the worker listener: the same rule.
///
/// Kills: ColdStore left outside the table, or open.
#[tokio::test(start_paused = true)]
async fn cold_store_is_a_worker_path() {
    use Buck2Auth::None as Open;
    check(
        COLD,
        &[
            (Open, ANON, NO, "16"),
            (Open, STORAGE, NO, "ok"),
            (Open, DAEMON, NO, "ok"),
            (Open, FRONTEND, NO, "7"),
        ],
    )
    .await;
}

/// REAPI follows `buck2_auth`: `none` is open, `mtls` needs a verified certificate of any
/// role, `bearer` needs the Buck2 key and nothing else stands in for it: not a certificate,
/// not the operator token, and a wrong key next to a good certificate is a refusal.
///
/// Kills: a bearer mode that falls back to the certificate; a prefix or any-token compare;
/// `mtls` that admits anonymous; `none` that insists on credentials.
#[tokio::test(start_paused = true)]
async fn reapi_follows_buck2_auth() {
    use Buck2Auth::{Bearer, Mtls, None as Open};
    check(
        CAPS,
        &[
            (Open, ANON, NO, "ok"),
            (Open, FRONTEND, NO, "ok"),
            (Open, ANON, BAD, "ok"),
            (Mtls, ANON, NO, "16"),
            (Mtls, FRONTEND, NO, "ok"),
            (Mtls, DAEMON, NO, "ok"),
            (Mtls, STORAGE, NO, "ok"),
            (Mtls, DAEMON, BAD, "ok"),
            (Mtls, ANON, BK, "16"),
            (Bearer, ANON, BK, "ok"),
            (Bearer, ANON, NO, "16"),
            (Bearer, ANON, BAD, "16"),
            (Bearer, ANON, OP, "16"),
            (Bearer, DAEMON, NO, "16"),
            (Bearer, DAEMON, BAD, "16"),
            (Bearer, DAEMON, BK, "ok"),
        ],
    )
    .await;
}

/// Operator paths, the update API and `/metrics` take the operator token in every mode,
/// whatever the certificate says; the Buck2 key is not it.
///
/// Kills: an operator path open to a certificate; the Buck2 key accepted as the operator
/// token; the update API on a weaker rule than the operator API.
#[tokio::test(start_paused = true)]
async fn operator_update_and_metrics_take_the_operator_token() {
    use Buck2Auth::{Bearer, Mtls, None as Open};
    for path in [OPERATOR, UPDATE, "/metrics"] {
        for mode in [Open, Mtls, Bearer] {
            check(
                path,
                &[
                    (mode, ANON, OP, "ok"),
                    (mode, ANON, NO, "16"),
                    (mode, ANON, BAD, "16"),
                    (mode, ANON, BK, "16"),
                    (mode, STORAGE, NO, "16"),
                    (mode, STORAGE, OP, "ok"),
                ],
            )
            .await;
        }
    }
}

/// Probes are open to everyone; enrollment is open to a caller with no certificate (the child
/// checks the enrollment token) in every mode, and reaches the child's enrollment listener.
///
/// Kills: a probe behind a credential; enrollment behind `mtls`.
#[tokio::test(start_paused = true)]
async fn probes_and_enrollment_are_open() {
    use Buck2Auth::{Bearer, Mtls, None as Open};
    for mode in [Open, Mtls, Bearer] {
        for path in ["/healthz", "/readyz", ENROLL] {
            assert_eq!(cell(mode, ANON, NO, path).await, "ok", "{path} {mode:?}");
        }
    }
    let rig = Rig::start_secure(Mtls, Access::Plain(None)).await;
    let mut client = rig.client().await;
    client.bearer = None;
    assert_eq!(call(&mut client, ENROLL).await, "ok");
    assert_eq!(rig.a.state.headers_of(ENROLL).len(), 1);
}

/// A refusal is counted and never reaches a child, a hold or a permit.
///
/// Kills: auth after the hold queue or after the dial.
#[tokio::test(start_paused = true)]
async fn a_refused_call_never_waits_in_the_hold_queue() {
    let rig = Rig::start_secure(Buck2Auth::Mtls, Access::Plain(None)).await;
    let guard = rig.core.slots.hold(Duration::from_secs(20)).expect("hold");
    let mut client = rig.client().await;
    assert_eq!(call(&mut client, CAPS).await, "16");
    assert_eq!(rig.core.queue.held(), 0, "refused before it could be held");
    assert_eq!(
        rig.core
            .metrics
            .render(&rig.core.slots, &rig.core.queue)
            .matches("auth_refused_total 1")
            .count(),
        1
    );
    drop(guard);
}

/// A node id that cannot be a header value is refused as `PERMISSION_DENIED`, never forwarded
/// and never sent as something else.
///
/// Kills: a lossy identity header; forwarding with no identity.
#[tokio::test(start_paused = true)]
async fn an_identity_that_cannot_be_a_header_is_refused() {
    let bad = Identity {
        node_id: "bad\nname".into(),
        role: PeerRole::Daemon,
    };
    let rig = Rig::start_secure(Buck2Auth::None, Access::Plain(Some(bad))).await;
    let reply = rig.client().await.unary(WORKER).await;
    assert_eq!(reply.grpc_status().as_deref(), Some("7"), "{reply:?}");
    assert_eq!(rig.a.state.dispatches(), 0);
}

/// A refusal is sent from the request headers, so the stream is closed before the client's
/// message is. A client that sends its message after the answer has landed must still read the
/// answer (RFC 9113 section 8.1), not fail on the closed stream.
///
/// Kills: a test client that treats `InactiveStreamId` from `send_data` as a failure of the
/// call, which failed `bind_run_hold_release_and_shutdown` when the refusal won the race.
#[tokio::test(start_paused = true)]
async fn a_refusal_that_beats_the_message_is_still_read() {
    let rig = Rig::start_secure(Buck2Auth::Bearer, Access::Plain(None)).await;
    let mut client = rig.client().await;
    let reply = client.unary_after(CAPS, Duration::from_secs(1)).await;
    assert_eq!(reply.grpc_status().as_deref(), Some("16"), "{reply:?}");
    assert!(reply.error.is_none(), "{reply:?}");
    assert_eq!(rig.a.state.dispatches(), 0);
}

/// Wrong tokens are counted per address; past the burst even the right token is not looked
/// at (429, `RESOURCE_EXHAUSTED`), and an address that stops earns its place back. A client
/// that is right every time is never slowed down.
///
/// Kills: no limit; a limit on all calls (not failures); a limit that never lifts; the
/// token hashed before the limit is asked.
#[tokio::test(start_paused = true)]
async fn wrong_tokens_are_rate_limited_per_address() {
    let rig = Rig::start_secure(Buck2Auth::Bearer, Access::Plain(None)).await;
    let mut right = rig.client_from(super::kit::CLIENT_IP).await;
    right.bearer = Some(BUCK2_TOKEN.into());
    for _ in 0..3 * crate::auth::FAILURE_BURST {
        assert_eq!(
            call(&mut right, CAPS).await,
            "ok",
            "a good key is never limited"
        );
    }

    let other = std::net::IpAddr::V4(std::net::Ipv4Addr::new(192, 0, 2, 77));
    let mut wrong = rig.client_from(other).await;
    wrong.bearer = Some(WRONG.into());
    for n in 0..crate::auth::FAILURE_BURST {
        assert_eq!(call(&mut wrong, CAPS).await, "16", "wrong token {n}");
    }
    assert_eq!(call(&mut wrong, CAPS).await, "8", "limited");
    wrong.bearer = Some(BUCK2_TOKEN.into());
    assert_eq!(
        call(&mut wrong, CAPS).await,
        "8",
        "the right key is not looked at either"
    );
    assert_eq!(
        call(&mut right, CAPS).await,
        "ok",
        "another address is unaffected"
    );

    tokio::time::sleep(crate::auth::FAILURE_REFILL * 2).await;
    assert_eq!(
        call(&mut wrong, CAPS).await,
        "ok",
        "two periods earned two places back"
    );
}

/// Refusals of a non-gRPC path are HTTP statuses: 401 with `WWW-Authenticate`, 403 for an
/// identity the child cannot be told, and 429 once an address has presented too many wrong
/// tokens.
///
/// Kills: a gRPC status on a plain HTTP answer; a 401 that does not say how to authenticate.
#[tokio::test(start_paused = true)]
async fn http_paths_are_refused_with_http_statuses() {
    let rig = Rig::start_secure(Buck2Auth::None, Access::Plain(None)).await;
    let mut client = rig.client().await;
    client.bearer = Some(WRONG.into());
    for _ in 0..crate::auth::FAILURE_BURST {
        let reply = client.plain("GET", OPERATOR, b"").await;
        assert_eq!(reply.status, Some(401));
        let challenge = reply.headers.get("www-authenticate");
        assert_eq!(
            challenge.map(http::HeaderValue::as_bytes),
            Some(&b"Bearer"[..])
        );
        assert!(reply.grpc_status().is_none());
    }
    let limited = client.plain("GET", OPERATOR, b"").await;
    assert_eq!(limited.status, Some(429));
    assert!(limited.headers.get("www-authenticate").is_none());

    let bad = Identity {
        node_id: "bad\nname".into(),
        role: PeerRole::Daemon,
    };
    let rig = Rig::start_secure(Buck2Auth::None, Access::Plain(Some(bad))).await;
    let reply = rig.client().await.plain("GET", OPERATOR, b"").await;
    assert_eq!(reply.status, Some(403), "{reply:?}");
    assert_eq!(rig.a.state.dispatches(), 0);
}

// ---- TLS ----

pub(super) async fn connect_tls(
    rig: &Rig,
    config: Arc<rustls::ClientConfig>,
) -> io::Result<TlsStream<DuplexStream>> {
    let (near, far) = tokio::io::duplex(1 << 20);
    rig.feed_raw(far, super::kit::CLIENT_IP);
    let name = ServerName::try_from("localhost").expect("a name");
    TlsConnector::from(config).connect(name, near).await
}

pub(super) async fn h2_over_tls(rig: &Rig, config: Arc<rustls::ClientConfig>) -> Client {
    let tls = connect_tls(rig, config).await.expect("the handshake");
    let (send, connection) = client_builder().handshake(tls).await.expect("h2");
    Client::new(send, tokio::spawn(connection))
}

/// A rig behind the TLS terminator over a fresh CA's files.
pub(super) async fn tls_rig(name: &str, mode: Buck2Auth) -> (Rig, Pki, crate::tls::TlsPaths) {
    let pki = Pki::new("farm CA");
    let paths = pki.write(&scratch(name), &pki.server());
    let rig = Rig::start_secure(mode, Access::Tls(paths.clone())).await;
    (rig, pki, paths)
}

fn reserved(headers: &http::HeaderMap) -> Vec<(String, String)> {
    let mut found: Vec<_> = headers
        .iter()
        .filter(|(k, _)| k.as_str().starts_with("x-kbf-"))
        .map(|(k, v)| {
            (
                k.to_string(),
                String::from_utf8_lossy(v.as_bytes()).into_owned(),
            )
        })
        .collect();
    found.sort();
    found
}

const FORGED: [(&str, &str); 4] = [
    ("x-kbf-node-id", "forged-node"),
    ("x-kbf-role", "storage"),
    ("x-kbf-proxy-auth", "guess"),
    ("x-kbf-extra", "1"),
];

/// A verified certificate reaches the child as exactly three headers (its node id, its role,
/// the secret) and nothing the client forged under those names.
///
/// Kills: headers set from the client's own; identity set with no verified certificate; an
/// inbound `x-kbf-*` that survives the strip; two values for one header.
#[tokio::test(start_paused = true)]
async fn a_verified_certificate_reaches_the_child_as_identity_headers() {
    let (rig, pki, _) = tls_rig("identity-e2e", Buck2Auth::None).await;
    let daemon = pki.client(&["node-7.farm"], Some("daemon"));
    let mut client = h2_over_tls(&rig, client_config(&pki.ca_pem, Some(&daemon), &["h2"])).await;
    let (response, mut body) = client.start_with(WORKER, false, &FORGED).await;
    body.send_data(Bytes::from_static(b"x"), true)
        .expect("body");
    assert_eq!(finish(response).await.text(), "a");

    let seen = rig.a.state.headers_of(WORKER);
    assert_eq!(seen.len(), 1);
    assert_eq!(
        reserved(&seen[0]),
        [
            ("x-kbf-node-id".to_owned(), "node-7.farm".to_owned()),
            ("x-kbf-proxy-auth".to_owned(), hex::encode(SECRET)),
            ("x-kbf-role".to_owned(), "daemon".to_owned()),
        ]
    );
}

/// A client with no certificate is anonymous to the child: the secret, and no node id or
/// role, whatever it forged.
///
/// Kills: identity taken from the request; the secret left off.
#[tokio::test(start_paused = true)]
async fn a_client_without_a_certificate_is_anonymous_to_the_child() {
    let (rig, pki, _) = tls_rig("anonymous-e2e", Buck2Auth::None).await;
    let mut client = h2_over_tls(&rig, client_config(&pki.ca_pem, None, &["h2"])).await;
    let (response, mut body) = client.start_with(CAPS, false, &FORGED).await;
    body.send_data(Bytes::from_static(b"x"), true)
        .expect("body");
    assert_eq!(finish(response).await.text(), "a");
    let seen = rig.a.state.headers_of(CAPS);
    assert_eq!(
        reserved(&seen[0]),
        [("x-kbf-proxy-auth".to_owned(), hex::encode(SECRET))]
    );
}

/// Each connection carries its own certificate's identity.
///
/// Kills: identity kept in one place for all connections.
#[tokio::test(start_paused = true)]
async fn two_connections_are_two_identities() {
    let (rig, pki, _) = tls_rig("two-identities", Buck2Auth::None).await;
    let one = pki.client(&["node-1.farm"], Some("daemon"));
    let two = pki.client(&["node-2.farm"], Some("storage"));
    let mut a = h2_over_tls(&rig, client_config(&pki.ca_pem, Some(&one), &["h2"])).await;
    let mut b = h2_over_tls(&rig, client_config(&pki.ca_pem, Some(&two), &["h2"])).await;
    for _ in 0..2 {
        assert_eq!(a.unary(WORKER).await.text(), "a");
        assert_eq!(b.unary(WORKER).await.text(), "a");
    }
    let mut nodes: Vec<String> = rig
        .a
        .state
        .headers_of(WORKER)
        .iter()
        .map(|h| {
            let get = |n: &str| h.get(n).map(|v| v.to_str().expect("text").to_owned());
            format!(
                "{}:{}",
                get("x-kbf-node-id").expect("id"),
                get("x-kbf-role").expect("role")
            )
        })
        .collect();
    nodes.sort();
    assert_eq!(
        nodes,
        [
            "node-1.farm:daemon",
            "node-1.farm:daemon",
            "node-2.farm:storage",
            "node-2.farm:storage"
        ]
    );
}

/// A role the path does not take is `PERMISSION_DENIED`, a role it takes is served; both on a
/// real handshake.
///
/// Kills: the role read from anywhere but the verified certificate.
#[tokio::test(start_paused = true)]
async fn a_wrong_role_certificate_is_denied_on_a_worker_path() {
    let (rig, pki, _) = tls_rig("wrong-role", Buck2Auth::None).await;
    let frontend = pki.client(&["fe-1.farm"], Some("frontend"));
    let storage = pki.client(&["st-1.farm"], Some("storage"));
    let mut denied = h2_over_tls(&rig, client_config(&pki.ca_pem, Some(&frontend), &["h2"])).await;
    let reply = denied.unary(WORKER).await;
    assert_eq!(reply.grpc_status().as_deref(), Some("7"), "{reply:?}");
    let mut served = h2_over_tls(&rig, client_config(&pki.ca_pem, Some(&storage), &["h2"])).await;
    assert_eq!(served.unary(WORKER).await.text(), "a");
    assert_eq!(rig.a.state.dispatches(), 1);
}

/// A client certificate issued by an intermediate CA, sent with it as `[leaf, intermediate]`,
/// is the leaf's identity: the child is told the leaf's node and role, never the CA's.
///
/// Kills: identity read from another certificate of the chain than the first.
#[tokio::test(start_paused = true)]
async fn the_identity_is_the_leafs_when_a_chain_is_sent() {
    let (rig, pki, _) = tls_rig("chain", Buck2Auth::None).await;
    let leaf = pki
        .intermediate()
        .client_chain(&["node-5.farm"], Some("storage"));
    let mut client = h2_over_tls(&rig, client_config(&pki.ca_pem, Some(&leaf), &["h2"])).await;
    assert_eq!(client.unary(WORKER).await.text(), "a");
    let seen = rig.a.state.headers_of(WORKER);
    assert_eq!(
        reserved(&seen[0]),
        [
            ("x-kbf-node-id".to_owned(), "node-5.farm".to_owned()),
            ("x-kbf-proxy-auth".to_owned(), hex::encode(SECRET)),
            ("x-kbf-role".to_owned(), "storage".to_owned()),
        ]
    );
}

/// A client certificate that is past its validity, or is for servers only, fails the
/// handshake: it is not served and not taken for an anonymous client.
///
/// Kills: a client verifier that ignores validity or extended key usage.
#[tokio::test(start_paused = true)]
async fn an_expired_or_server_only_client_certificate_is_not_served() {
    let (rig, pki, _) = tls_rig("bad-clients", Buck2Auth::None).await;
    let certs = [
        pki.expired_client(&["node-6.farm"], "daemon"),
        pki.server_only_client(&["node-6.farm"], "daemon"),
    ];
    for cert in &certs {
        let config = client_config(&pki.ca_pem, Some(cert), &["h2"]);
        let tls = connect_tls(&rig, config)
            .await
            .expect("the client side completes");
        assert!(closed_by_the_server(tls).await);
    }
    assert_eq!(rig.a.state.dispatches(), 0);
    assert_eq!(
        rig.core
            .metrics
            .handshake_failed
            .load(std::sync::atomic::Ordering::Relaxed),
        2
    );
}

/// What a client sees of a connection the server closed or failed: an error, or end of
/// stream, and never a byte of HTTP/2.
async fn closed_by_the_server(mut tls: TlsStream<DuplexStream>) -> bool {
    let _ = tls.write_all(b"x").await;
    let mut buf = [0_u8; 64];
    matches!(tls.read(&mut buf).await, Err(_) | Ok(0))
}

/// A certificate of another CA fails the handshake: the server never treats it as anonymous,
/// serves it nothing, and counts it.
///
/// Kills: a failed verification treated as an anonymous client.
#[tokio::test(start_paused = true)]
async fn a_certificate_of_another_ca_is_not_served() {
    let (rig, pki, _) = tls_rig("other-ca", Buck2Auth::None).await;
    let other = Pki::new("other CA");
    let stranger = other.client(&["node-9.farm"], Some("daemon"));
    let config = client_config(&pki.ca_pem, Some(&stranger), &["h2"]);
    // With TLS 1.3 the client's side completes first; the refusal is an alert on its first read.
    let tls = connect_tls(&rig, config)
        .await
        .expect("the client side completes");
    assert!(closed_by_the_server(tls).await);
    assert_eq!(rig.a.state.dispatches(), 0);
    assert_eq!(
        rig.core
            .metrics
            .handshake_failed
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
}

/// A certificate that verifies but names no usable identity (two names, a wildcard, no role)
/// closes the connection: an identity that must be guessed is not one.
///
/// Kills: an ambiguous certificate served as anonymous or with the first name.
#[tokio::test(start_paused = true)]
async fn a_certificate_without_a_usable_identity_closes_the_connection() {
    let (rig, pki, _) = tls_rig("no-identity", Buck2Auth::None).await;
    let certs = [
        pki.client(&["a.farm", "b.farm"], Some("daemon")),
        pki.client(&["*.farm"], Some("daemon")),
        pki.client(&["a.farm"], None),
    ];
    for cert in &certs {
        let config = client_config(&pki.ca_pem, Some(cert), &["h2"]);
        let tls = connect_tls(&rig, config).await.expect("the handshake");
        assert!(closed_by_the_server(tls).await);
    }
    assert_eq!(
        rig.core
            .metrics
            .identity_refused
            .load(std::sync::atomic::Ordering::Relaxed),
        3
    );
    assert_eq!(rig.a.state.dispatches(), 0);
}

/// TLS only: a client that speaks HTTP/2 in the clear is not served, and one that offers a
/// protocol the proxy does not speak is refused at the handshake.
///
/// Kills: plaintext accepted; ALPN left open.
#[tokio::test(start_paused = true)]
async fn plaintext_and_other_protocols_are_refused() {
    let (rig, pki, _) = tls_rig("plaintext", Buck2Auth::None).await;
    let mut clear = rig.client().await;
    let reply = clear.unary(CAPS).await;
    assert!(reply.status.is_none(), "{reply:?}");
    assert!(
        connect_tls(&rig, client_config(&pki.ca_pem, None, &["h3"]))
            .await
            .is_err()
    );
    assert_eq!(rig.a.state.dispatches(), 0);
}

/// A client that opens a connection and never speaks is closed at the handshake bound.
///
/// Kills: no handshake timeout.
#[tokio::test(start_paused = true)]
async fn a_silent_client_is_closed_at_the_handshake_bound() {
    let (rig, _pki, _) = tls_rig("silent", Buck2Auth::None).await;
    let (mut near, far) = tokio::io::duplex(1 << 16);
    rig.feed_raw(far, super::kit::CLIENT_IP);
    let mut buf = [0_u8; 8];
    let read = tokio::time::timeout(Duration::from_secs(30), near.read(&mut buf)).await;
    assert!(matches!(read, Ok(Ok(0))), "{read:?}");
    assert_eq!(
        rig.core
            .metrics
            .handshake_failed
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
}
