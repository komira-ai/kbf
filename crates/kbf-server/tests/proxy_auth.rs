//! Who a call is behind a TLS-terminating proxy (`kbf_server::proxy_auth`, issue #1095,
//! `docs/design/frontend-proxy.md` section 3): the secret file, the decision over a peer
//! address and headers, the layers on the gRPC and HTTP listeners, the operator gates and the
//! flag. Over real listeners: `proxy_auth_listeners.rs`.

#![cfg(unix)]

use std::convert::Infallible;
use std::future::{Ready, ready};
use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use axum::Router;
use axum::body::Body;
use axum::extract::ConnectInfo;
use axum::routing::get;
use clap::Parser as _;
use kbf_peer::PeerRole;
use kbf_server::api::{Write, write_request};
use kbf_server::identity::{Presented, presented};
use kbf_server::proxy_auth::{
    Caller, Identity, NODE_ID, PROXY_AUTH, ProxyAuthLayer, ProxySecret, ProxySecretError, ROLE,
    Refusal, Trust, classify, gate_requests, strip,
};
use kbf_server::token::{ApiToken, TokenFileError};
use kbf_server::{Args, ConfigError};
use kbf_storage::service::Admission;
use tonic::Code;
use tonic::codegen::http::{self, HeaderMap, HeaderName, HeaderValue, StatusCode};
use tonic::codegen::{Service, http::Extensions};
use tonic::transport::server::TcpConnectInfo;

/// The 32 bytes of the test secret, and the header value that presents them.
const SECRET: [u8; 32] = [0x5a; 32];
const TOKEN: &str = "kbf-test-token-0123456789abcdef0123456789";
const REMOTE: &str = "192.0.2.10:4000";
const LOOPBACK: &str = "127.0.0.1:4000";

fn presented_secret() -> String {
    hex::encode(SECRET)
}

/// A file in a directory of its own, removed with it when this drops.
struct Owned {
    path: PathBuf,
    _dir: kbf_testkit::dir::TestDir,
}

impl std::ops::Deref for Owned {
    type Target = std::path::Path;

    fn deref(&self) -> &std::path::Path {
        &self.path
    }
}

/// A file holding `content` with `mode`, unique to this call.
fn file(content: &[u8], mode: u32) -> Owned {
    let dir =
        kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "kbf-server-proxy-auth");
    let path = dir.join("secret");
    std::fs::write(&path, content).expect("write the file");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode)).expect("chmod");
    Owned { path, _dir: dir }
}

fn secret() -> Arc<ProxySecret> {
    Arc::new(ProxySecret::from_file(&file(&SECRET, 0o600)).expect("a usable secret"))
}

fn addr(text: &str) -> SocketAddr {
    text.parse().expect("an address")
}

fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.append(
            HeaderName::from_bytes(name.as_bytes()).expect("a name"),
            HeaderValue::from_str(value).expect("a value"),
        );
    }
    map
}

// ---- the secret file ---------------------------------------------------------------

/// Catches: a secret file with mode 0600 or 0400 refused, the 32 bytes read as anything but
/// the secret (the header carries them as lower-case hex), a secret that admits another
/// value, and a `Debug` that shows the secret.
#[test]
fn an_owner_only_file_of_32_bytes_is_the_secret() {
    for mode in [0o600, 0o400] {
        let secret = ProxySecret::from_file(&file(&SECRET, mode)).expect("usable");
        assert!(secret.admits(presented_secret().as_bytes()), "{mode:04o}");
        assert!(!secret.admits(hex::encode([0x5b; 32]).as_bytes()));
    }
    let secret = ProxySecret::from_file(&file(&SECRET, 0o600)).expect("usable");
    assert_eq!(format!("{secret:?}"), "ProxySecret(..)");
}

/// Catches: the mode check removed or loosened for the proxy secret (a secret that group or
/// others can read lets any local user claim to be the proxy).
///
/// Mutant seen red: `owner_only` accepting 0644.
#[test]
fn a_secret_file_others_can_reach_is_refused() {
    for mode in [0o644, 0o640, 0o604, 0o660, 0o700, 0o4600] {
        let refused = ProxySecret::from_file(&file(&SECRET, mode));
        let Err(ProxySecretError::File(TokenFileError::Mode { mode: got, .. })) = refused else {
            panic!("mode {mode:04o}: {refused:?}");
        };
        assert_eq!(got, mode);
    }
}

/// Catches: a secret of any other length accepted (an empty file would admit an empty
/// header, the hex text of the secret or a trailing newline would be hashed as part of it).
#[test]
fn a_file_that_is_not_32_bytes_is_refused() {
    let mut newline = SECRET.to_vec();
    newline.push(b'\n');
    for content in [
        Vec::new(),
        SECRET[..31].to_vec(),
        newline,
        presented_secret().into_bytes(),
        vec![0; 4096],
    ] {
        let len = content.len();
        let refused = ProxySecret::from_file(&file(&content, 0o600));
        assert!(
            matches!(refused, Err(ProxySecretError::Length { .. })),
            "{len} bytes: {refused:?}"
        );
    }
}

/// Catches: a secret compared as a prefix or extension (the presented value must be the
/// whole 64 digits, lower case): none, a truncation, an extension, upper case, the raw
/// bytes, the bearer scheme.
///
/// Mutant seen red: `admits` as `presented.starts_with(secret)`.
#[test]
fn the_secret_is_matched_whole() {
    let secret = ProxySecret::from_file(&file(&SECRET, 0o600)).expect("usable");
    let good = presented_secret();
    assert!(secret.admits(good.as_bytes()));
    for wrong in [
        String::new(),
        good[..63].to_owned(),
        format!("{good}0"),
        format!("{good} "),
        good.to_uppercase(),
        format!("Bearer {good}"),
    ] {
        assert!(!secret.admits(wrong.as_bytes()), "{wrong:?}");
    }
    assert!(!secret.admits(&SECRET));
}

// ---- the decision ------------------------------------------------------------------

fn proxied_headers() -> HeaderMap {
    headers(&[
        (PROXY_AUTH, &presented_secret()),
        (NODE_ID, "node-a"),
        (ROLE, "daemon"),
    ])
}

/// Catches: the three results of the table in the module docs run together: no secret
/// configured means the headers are never believed, a configured secret refuses a peer that
/// is not loopback (or unknown) however right its secret, a loopback peer without the
/// secret, or with a wrong one, is `Unverified` whatever identity it claims, and only
/// loopback with the secret is `Proxied` with the identity named.
///
/// Mutants seen red: the headers believed without the secret check; believed from a
/// non-loopback peer (192.0.2.10); no refusal of an unknown peer.
#[test]
fn only_loopback_with_the_secret_is_believed() {
    let secret = secret();
    let believed = Trust::Proxied(Identity {
        node_id: Some("node-a".to_owned()),
        role: Some(PeerRole::Daemon),
    });
    let ok = |peer: &str, h: &HeaderMap| classify(Some(&secret), Some(addr(peer)), h);
    assert_eq!(ok(LOOPBACK, &proxied_headers()), Ok(believed.clone()));
    assert_eq!(ok("[::1]:4000", &proxied_headers()), Ok(believed.clone()));
    assert_eq!(
        ok("[::ffff:127.0.0.1]:4000", &proxied_headers()),
        Ok(believed)
    );

    for refused in [REMOTE, "[2001:db8::1]:4000", "198.51.100.1:1"] {
        assert_eq!(
            ok(refused, &proxied_headers()),
            Err(Refusal::NotLoopback),
            "{refused}"
        );
        assert_eq!(ok(refused, &HeaderMap::new()), Err(Refusal::NotLoopback));
    }
    assert_eq!(
        classify(Some(&secret), None, &proxied_headers()),
        Err(Refusal::NotLoopback),
        "an unknown peer is not loopback"
    );

    let claims = [(NODE_ID, "node-a"), (ROLE, "daemon")];
    let wrong = hex::encode([0x5b; 32]);
    for pairs in [
        claims.to_vec(),
        [&claims[..], &[(PROXY_AUTH, wrong.as_str())]].concat(),
        [&claims[..], &[(PROXY_AUTH, "")]].concat(),
        [&claims[..], &[(PROXY_AUTH, &presented_secret()[..63])]].concat(),
    ] {
        assert_eq!(ok(LOOPBACK, &headers(&pairs)), Ok(Trust::Unverified));
    }
    assert_eq!(ok(LOOPBACK, &HeaderMap::new()), Ok(Trust::Unverified));

    for peer in [Some(addr(LOOPBACK)), Some(addr(REMOTE)), None] {
        assert_eq!(classify(None, peer, &proxied_headers()), Ok(Trust::Direct));
    }
}

/// Catches: an identity read wrongly. Each of the node and the role is optional (none is
/// anonymous), a role must be one of the three exact names, and a header that is empty,
/// repeated, or not visible ASCII is refused rather than guessed at.
#[test]
fn the_identity_is_the_node_and_the_role_or_it_is_refused() {
    let secret = secret();
    let with = |pairs: &[(&str, &str)]| {
        let mut all = vec![(PROXY_AUTH, presented_secret())];
        all.extend(pairs.iter().map(|(k, v)| (*k, (*v).to_owned())));
        let all: Vec<(&str, &str)> = all.iter().map(|(k, v)| (*k, v.as_str())).collect();
        classify(Some(&secret), Some(addr(LOOPBACK)), &headers(&all))
    };
    let identity = |node_id: Option<&str>, role| {
        Ok(Trust::Proxied(Identity {
            node_id: node_id.map(str::to_owned),
            role,
        }))
    };
    assert_eq!(with(&[]), identity(None, None), "anonymous");
    assert_eq!(
        with(&[(NODE_ID, "n1")]),
        identity(Some("n1"), None),
        "a node without a role"
    );
    for role in PeerRole::ALL {
        assert_eq!(with(&[(ROLE, role.name())]), identity(None, Some(role)));
    }
    for bad in ["", "Daemon", "DAEMON", "admin", " daemon", "daemon "] {
        let got = HeaderValue::from_str(bad).map(|_| with(&[(ROLE, bad)]));
        if let Ok(got) = got {
            assert_eq!(got, Err(Refusal::Malformed(ROLE)), "{bad:?}");
        }
    }
    assert_eq!(
        with(&[(NODE_ID, "")]),
        Err(Refusal::Malformed(NODE_ID)),
        "an empty node id"
    );
    assert_eq!(
        with(&[(NODE_ID, "a"), (NODE_ID, "b")]),
        Err(Refusal::Malformed(NODE_ID)),
        "two node ids"
    );
    assert_eq!(
        with(&[(ROLE, "daemon"), (ROLE, "daemon")]),
        Err(Refusal::Malformed(ROLE)),
        "two roles"
    );
    let mut raw = headers(&[(PROXY_AUTH, &presented_secret())]);
    raw.insert(
        NODE_ID,
        HeaderValue::from_bytes(b"node-\xff").expect("a value"),
    );
    assert_eq!(
        classify(Some(&secret), Some(addr(LOOPBACK)), &raw),
        Err(Refusal::Malformed(NODE_ID)),
        "a node id that is not ASCII"
    );
}

/// Catches: a refusal with the wrong gRPC code: a call that came the wrong way is
/// UNAUTHENTICATED, a malformed identity PERMISSION_DENIED.
#[test]
fn a_refusal_has_its_grpc_code() {
    assert_eq!(Refusal::NotLoopback.status().code(), Code::Unauthenticated);
    let malformed = Refusal::Malformed(NODE_ID).status();
    assert_eq!(malformed.code(), Code::PermissionDenied);
    assert!(malformed.message().contains(NODE_ID));
}

/// Catches: `x-kbf-*` headers left in place (a handler could read one), or a header that
/// merely looks like one removed (`x-kbf` alone, `kbf-map-generation`, `x-kbfx`).
///
/// Mutant seen red: no strip.
#[test]
fn strip_removes_every_reserved_header_and_no_other() {
    let mut map = headers(&[
        ("x-kbf-node-id", "node-a"),
        ("X-KBF-Role", "daemon"),
        ("x-kbf-proxy-auth", "secret"),
        ("x-kbf-anything-else", "1"),
        ("x-kbf-node-id", "again"),
        ("authorization", "Bearer t"),
        ("kbf-map-generation", "7"),
        ("x-kbf", "kept"),
        ("x-kbfx", "kept"),
    ]);
    strip(&mut map);
    let mut kept: Vec<&str> = map.keys().map(HeaderName::as_str).collect();
    kept.sort_unstable();
    assert_eq!(
        kept,
        ["authorization", "kbf-map-generation", "x-kbf", "x-kbfx"]
    );
}

// ---- the gRPC layer ----------------------------------------------------------------

/// What the service behind the layer saw of each call: its trust and its `x-kbf-*` headers.
#[derive(Clone, Default)]
struct Seen(Arc<Mutex<Vec<Call>>>);

/// What one call looked like to the service: its trust, and its `x-kbf-*` headers.
type Call = (Option<Trust>, Vec<String>);

struct Inner(Seen);

impl Service<http::Request<()>> for Inner {
    type Response = http::Response<String>;
    type Error = Infallible;
    type Future = Ready<Result<Self::Response, Infallible>>;

    fn poll_ready(&mut self, _: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, request: http::Request<()>) -> Self::Future {
        let reserved = request
            .headers()
            .keys()
            .filter(|name| name.as_str().starts_with("x-kbf-"))
            .map(|name| name.as_str().to_owned())
            .collect();
        let trust = request.extensions().get::<Trust>().cloned();
        self.0.0.lock().expect("lock").push((trust, reserved));
        ready(Ok(http::Response::new("inner".to_owned())))
    }
}

fn grpc_request(peer: Option<SocketAddr>, h: &HeaderMap) -> http::Request<()> {
    let mut request = http::Request::new(());
    *request.uri_mut() = "/kbf.worker.v1.Worker/Session".parse().expect("a uri");
    *request.headers_mut() = h.clone();
    if let Some(remote_addr) = peer {
        request.extensions_mut().insert(TcpConnectInfo {
            local_addr: None,
            remote_addr: Some(remote_addr),
        });
    }
    request
}

async fn through_layer(
    secret: Option<Arc<ProxySecret>>,
    peer: Option<SocketAddr>,
    h: &HeaderMap,
) -> (http::Response<String>, Seen) {
    use tower_layer::Layer as _;
    let seen = Seen::default();
    let mut service = ProxyAuthLayer::new(secret).layer(Inner(seen.clone()));
    let response = service
        .call(grpc_request(peer, h))
        .await
        .expect("the layer never fails");
    (response, seen)
}

fn grpc_status(response: &http::Response<String>) -> Option<&str> {
    response
        .headers()
        .get("grpc-status")
        .map(|v| v.to_str().expect("ASCII"))
}

/// Catches: the layer letting a call through that the decision refuses (a remote peer, an
/// unknown peer, a malformed identity: the service behind never runs and the answer is the
/// gRPC status), and an identity or trust that reaches the service wrongly: every call
/// reaches it with every `x-kbf-*` header gone, and with the trust the decision gave.
///
/// Mutants seen red: headers not stripped from an unverified request; a non-loopback peer
/// let through.
#[tokio::test]
async fn the_grpc_layer_applies_the_decision_and_strips_the_headers() {
    let mut claim = proxied_headers();
    claim.insert("x-kbf-extra", HeaderValue::from_static("1"));
    let believed = Trust::Proxied(Identity {
        node_id: Some("node-a".to_owned()),
        role: Some(PeerRole::Daemon),
    });

    let (answer, seen) = through_layer(Some(secret()), Some(addr(LOOPBACK)), &claim).await;
    assert_eq!(answer.body(), "inner");
    assert_eq!(
        *seen.0.lock().expect("lock"),
        [(Some(believed), Vec::new())]
    );

    let mut forged = claim.clone();
    forged.insert(PROXY_AUTH, HeaderValue::from_static("wrong"));
    let (answer, seen) = through_layer(Some(secret()), Some(addr(LOOPBACK)), &forged).await;
    assert_eq!(answer.body(), "inner");
    assert_eq!(
        *seen.0.lock().expect("lock"),
        [(Some(Trust::Unverified), Vec::new())],
        "unverified, and no claim reaches the service"
    );

    let (answer, seen) = through_layer(None, Some(addr(REMOTE)), &claim).await;
    assert_eq!(answer.body(), "inner");
    assert_eq!(
        *seen.0.lock().expect("lock"),
        [(Some(Trust::Direct), Vec::new())],
        "without a proxy, direct, and the headers are stripped all the same"
    );

    for peer in [Some(addr(REMOTE)), None] {
        let (answer, seen) = through_layer(Some(secret()), peer, &claim).await;
        assert_eq!(grpc_status(&answer), Some("16"), "UNAUTHENTICATED");
        assert!(seen.0.lock().expect("lock").is_empty(), "{peer:?}");
    }
    let mut malformed = claim.clone();
    malformed.insert(ROLE, HeaderValue::from_static("admin"));
    let (answer, seen) = through_layer(Some(secret()), Some(addr(LOOPBACK)), &malformed).await;
    assert_eq!(grpc_status(&answer), Some("7"), "PERMISSION_DENIED");
    assert!(seen.0.lock().expect("lock").is_empty());
}

/// Catches: `presented` reading the certificate when the proxy vouched for the call, or the
/// proxy's word when it did not: only `Trust::Proxied` gives the proxy's identity; every
/// other trust, and none, falls to the connection's certificate (none here).
#[test]
fn presented_prefers_the_proxy_only_when_it_vouched() {
    let identity = Identity {
        node_id: Some("node-a".to_owned()),
        role: Some(PeerRole::Storage),
    };
    let with = |trust: Option<Trust>| {
        let mut extensions = Extensions::new();
        if let Some(trust) = trust {
            extensions.insert(trust);
        }
        presented(&extensions)
    };
    assert_eq!(
        with(Some(Trust::Proxied(identity.clone()))),
        Presented::Proxy(identity)
    );
    for other in [None, Some(Trust::Direct), Some(Trust::Unverified)] {
        assert_eq!(with(other), Presented::Leaf(None));
    }
    assert_eq!(
        Presented::Leaf(Some(vec![1, 2])).leaf(),
        Some(&[1_u8, 2][..])
    );
    assert_eq!(Presented::Proxy(Identity::default()).leaf(), None);
}

// ---- the operator gates ------------------------------------------------------------

fn token() -> ApiToken {
    ApiToken::from_file(&file(format!("{TOKEN}\n").as_bytes(), 0o600)).expect("a token")
}

/// The status a cordon of `mac-1` gets from the gates alone.
fn gated(caller: &Caller, with_token: bool) -> Result<(), StatusCode> {
    let bearer = format!("Bearer {TOKEN}");
    let mut pairs = vec![("content-type", "application/json")];
    if with_token {
        pairs.push(("authorization", &bearer));
    }
    let h = headers(&pairs);
    let write = Write {
        peer: caller,
        headers: &h,
        target: "mac-1:cordon",
        body: b"",
    };
    write_request(Some(&token()), &write)
        .map(drop)
        .map_err(|(code, _)| code)
}

/// Catches: the operator gate trusting the proxy's word instead of the token (a proxied
/// write needs the bearer token), admitting a loopback caller that did not present the
/// secret when a proxy is configured, or a proxied call from a remote address; and, with
/// no proxy, any change from the loopback-and-token rule.
///
/// Mutants seen red: a proxied request admitted without the token; an unverified loopback
/// request admitted.
#[test]
fn a_write_through_the_proxy_needs_the_token_and_the_secret() {
    let caller = |peer: &str, trust: Trust| Caller {
        addr: addr(peer),
        trust,
    };
    let proxied = Trust::Proxied(Identity::default());
    assert_eq!(gated(&caller(LOOPBACK, proxied.clone()), true), Ok(()));
    assert_eq!(
        gated(&caller(LOOPBACK, proxied.clone()), false),
        Err(StatusCode::UNAUTHORIZED),
        "the proxy's secret is not the token"
    );
    assert_eq!(
        gated(&caller(LOOPBACK, Trust::Unverified), true),
        Err(StatusCode::FORBIDDEN),
        "behind a proxy, a local caller without the secret may not write"
    );
    assert_eq!(
        gated(&caller(REMOTE, proxied), true),
        Err(StatusCode::FORBIDDEN),
        "not from a remote address, whatever it presents"
    );
    assert_eq!(gated(&Caller::direct(addr(LOOPBACK)), true), Ok(()));
    assert_eq!(
        gated(&Caller::direct(addr(LOOPBACK)), false),
        Err(StatusCode::UNAUTHORIZED)
    );
    assert_eq!(
        gated(&Caller::direct(addr(REMOTE)), true),
        Err(StatusCode::FORBIDDEN)
    );
}

// ---- the HTTP layer ----------------------------------------------------------------

/// A router whose one route answers with the trust its handler was given, the peer address,
/// and the `x-kbf-*` headers it can still see (sorted, comma separated).
fn echo(proxy: Option<Arc<ProxySecret>>, layered: bool) -> Router {
    async fn handler(caller: Caller, headers: HeaderMap) -> String {
        let mut reserved: Vec<&str> = headers
            .keys()
            .map(HeaderName::as_str)
            .filter(|name| name.starts_with("x-kbf-"))
            .collect();
        reserved.sort_unstable();
        format!(
            "{:?} {} [{}]",
            caller.trust,
            caller.addr,
            reserved.join(",")
        )
    }
    let router = Router::new().route("/", get(handler));
    if layered {
        router.layer(axum::middleware::from_fn_with_state(proxy, gate_requests))
    } else {
        router
    }
}

async fn call(router: &mut Router, peer: Option<&str>, h: &HeaderMap) -> (StatusCode, String) {
    let mut request = axum::http::Request::new(Body::empty());
    *request.headers_mut() = h.clone();
    if let Some(peer) = peer {
        request.extensions_mut().insert(ConnectInfo(addr(peer)));
    }
    let response = router.call(request).await.expect("a response");
    let status = response.status();
    let body = axum::body::to_bytes(response.into_body(), 1 << 16)
        .await
        .expect("a body");
    (status, String::from_utf8(body.to_vec()).expect("UTF-8"))
}

/// Catches: the HTTP layer and extractor disagreeing with the decision: a remote peer, or
/// one with no address, refused 403 when a proxy is configured and its handler never run;
/// a loopback peer handed to the handler as `Proxied` only with the secret, `Unverified`
/// otherwise, `Direct` with no proxy; a handler behind no layer and a connection with no
/// address rejected by the extractor itself.
///
/// Mutant seen red: a non-loopback peer let through.
#[tokio::test]
async fn the_http_layer_applies_the_decision() {
    let mut proxied = echo(Some(secret()), true);
    let (code, body) = call(&mut proxied, Some(LOOPBACK), &proxied_headers()).await;
    assert_eq!(code, StatusCode::OK);
    assert!(
        body.starts_with("Proxied(Identity { node_id: Some(\"node-a\"), role: Some(Daemon) })")
            && body.ends_with(" []"),
        "{body}"
    );
    let (code, body) = call(&mut proxied, Some(LOOPBACK), &headers(&[(NODE_ID, "n")])).await;
    assert_eq!(
        (code, body.as_str()),
        (StatusCode::OK, "Unverified 127.0.0.1:4000 []")
    );
    for peer in [Some(REMOTE), None] {
        let (code, body) = call(&mut proxied, peer, &proxied_headers()).await;
        assert_eq!(code, StatusCode::FORBIDDEN, "{peer:?}");
        assert!(body.contains("behind a proxy"), "{body}");
    }
    let mut malformed = proxied_headers();
    malformed.insert(ROLE, HeaderValue::from_static("admin"));
    let (code, _) = call(&mut proxied, Some(LOOPBACK), &malformed).await;
    assert_eq!(code, StatusCode::FORBIDDEN);

    let mut direct = echo(None, true);
    let (code, body) = call(&mut direct, Some(REMOTE), &proxied_headers()).await;
    assert_eq!(
        (code, body.as_str()),
        (StatusCode::OK, "Direct 192.0.2.10:4000 []")
    );

    let mut bare = echo(None, false);
    let (code, body) = call(&mut bare, Some(LOOPBACK), &proxied_headers()).await;
    assert_eq!(
        code,
        StatusCode::INTERNAL_SERVER_ERROR,
        "a router with no gate layer must not pass for a direct caller: {body}"
    );
    assert!(body.contains("proxy gate"), "{body}");
    let (code, _) = call(&mut bare, None, &HeaderMap::new()).await;
    assert_eq!(code, StatusCode::INTERNAL_SERVER_ERROR);
}

// ---- the flag ----------------------------------------------------------------------

fn args(extra: &[&str]) -> Args {
    let base = [
        "kbf-server",
        "--listen",
        "127.0.0.1:0",
        "--worker-listen",
        "127.0.0.1:0",
    ];
    Args::try_parse_from(base.iter().chain(extra)).expect("flags")
}

/// Catches: a secret file others can read stopping nothing at start, and the flag read but
/// not turned into a proxy: the message names the flag, the path and the mode; with a good
/// file the listeners are behind the proxy and the worker listener needs no TLS flag.
///
/// Mutant seen red: a 0644 secret file accepted.
#[test]
fn a_secret_file_with_mode_0644_stops_the_start() {
    let open = file(&SECRET, 0o644);
    let open = open.to_str().expect("UTF-8");
    let refused = args(&["--proxy-auth-file", open])
        .proxy()
        .expect_err("a readable secret");
    assert!(matches!(
        refused,
        ConfigError::ProxyAuth(ProxySecretError::File(TokenFileError::Mode {
            mode: 0o644,
            ..
        }))
    ));
    let message = refused.to_string();
    assert!(message.starts_with("--proxy-auth-file: "), "{message}");
    assert!(
        message.contains(open) && message.contains("0644"),
        "{message}"
    );

    let good = file(&SECRET, 0o600);
    let behind_args = args(&["--proxy-auth-file", good.to_str().expect("UTF-8")]);
    let behind = behind_args
        .proxy()
        .expect("a good secret")
        .expect("behind a proxy");
    assert!(behind.secret.admits(presented_secret().as_bytes()));
    let listeners = behind_args.listeners().expect("listeners behind a proxy");
    assert!(listeners.worker_tls.is_none() && behind.deny_list.is_none());
    assert!(args(&[]).proxy().expect("no flag").is_none());
}

/// Catches: the deny list dropped behind a proxy that has no TLS of its own (a denied node
/// would be admitted), and `--worker-deny-list` accepted with no identity to apply it to.
#[test]
fn the_deny_list_applies_behind_the_proxy() {
    let list = file(b"node mac-07\n", 0o600);
    let list = list.to_str().expect("UTF-8");
    let secret = file(&SECRET, 0o600);
    let secret = secret.to_str().expect("UTF-8");
    let behind = args(&["--proxy-auth-file", secret, "--worker-deny-list", list]);
    behind.listeners().expect("a deny list behind a proxy");
    let proxy = behind.proxy().expect("the proxy").expect("proxy");
    assert!(proxy.deny_list.is_some());
    assert!(matches!(
        args(&["--worker-deny-list", list]).listeners(),
        Err(ConfigError::DenyListWithoutIdentity)
    ));
}

/// Catches: `Args::proxy` starting a proxied server on a deny list it cannot read or parse
/// (it fails, rather than warn and go on with no list), and the failure carrying the proxy
/// secret: the error names the flag, the list path and the line, nothing of the secret.
///
/// Mutant seen red: the `DenyList::open` error dropped (`.and_then(|p| ...ok())`). The `?` on
/// `certificate_entries` after it is reached only by a list that goes bad between the two
/// reads; `tests/identity.rs` covers that call.
#[test]
fn a_deny_list_that_cannot_be_read_or_parsed_stops_a_proxied_start_without_the_secret() {
    let secret = file(&SECRET, 0o600);
    let secret = secret.to_str().expect("UTF-8");
    let bad = file(b"node a\nserial zz\n", 0o600);
    let missing = file(b"", 0o600);
    std::fs::remove_file(&missing.path).expect("remove the file, so it is missing");
    for (list, words) in [(bad, ["serial", ":2:"]), (missing, ["read", "no-such"])] {
        let list = list.to_str().expect("UTF-8");
        let error = args(&["--proxy-auth-file", secret, "--worker-deny-list", list])
            .proxy()
            .map(|_| ())
            .expect_err("a list the server cannot apply");
        assert!(matches!(error, ConfigError::DenyList(_)), "{error:?}");
        let text = format!("{error} {error:?}");
        assert!(text.contains(list), "{text}");
        for word in words {
            assert!(text.contains(word), "{word} not in {text}");
        }
        assert!(
            !text.contains(&presented_secret()) && !text.contains(&"Z".repeat(32)),
            "{text}"
        );
    }
}

/// Catches the proxy arm of the role table: a proxied call is judged by the role the proxy
/// forwarded (a daemon may open a session, a storage node may not, a call with no role is
/// UNAUTHENTICATED), under no admission only the path is checked, and a method the listener
/// does not serve is refused for a proxy as for a certificate.
///
/// Mutants seen red: the unserved-path check dropped for a proxied call; the open
/// admission not honoured for one.
#[test]
fn the_role_table_judges_a_proxied_call_by_its_forwarded_role() {
    use kbf_server::worker_roles::admit_presented;
    let session = "/kbf.worker.v1.Worker/Session";
    let unserved = "/kbf.worker.v1.Worker/Unknown";
    let by = |role: Option<PeerRole>| {
        Presented::Proxy(Identity {
            node_id: Some("n".to_owned()),
            role,
        })
    };
    let certified = |path, role| admit_presented(Admission::Certified, path, &by(role));
    assert!(certified(session, Some(PeerRole::Daemon)).is_ok());
    assert_eq!(
        certified(session, Some(PeerRole::Storage))
            .expect_err("storage")
            .code(),
        Code::PermissionDenied
    );
    assert_eq!(
        certified(session, None).expect_err("no role").code(),
        Code::Unauthenticated
    );
    assert_eq!(
        certified(unserved, Some(PeerRole::Daemon))
            .expect_err("unserved")
            .code(),
        Code::PermissionDenied
    );
    assert!(admit_presented(Admission::Open, session, &by(None)).is_ok());
    assert_eq!(
        admit_presented(Admission::Open, unserved, &by(None))
            .expect_err("unserved")
            .code(),
        Code::PermissionDenied
    );
}
