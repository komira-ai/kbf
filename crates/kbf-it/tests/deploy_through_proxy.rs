//! The `kbf-installer deploy` client against the real `kbf-proxy`, over real sockets and
//! real TLS: the transport a deploy uses, proved on the server it will meet.
//!
//! A deploy speaks HTTP/1.1 over TLS to the proxy's public port with the operator token. The
//! proxy serves the update API itself (here a handler that records the requests and answers
//! with the API's own types) and passes every other `/v1/*` call to the active child's
//! operator port (here a small HTTP/2 server that records what reached it). So the test
//! fails if the client frames a request in a way the proxy's HTTP/1.1 server refuses, does
//! not read the proxy's answers to their end, sends the token under another name, or a
//! release upload does not arrive whole at the child.
//!
//! What is not here: the deploy's steps (`kbf-installer`'s own tests play the API), and the
//! swap behind the update route (the proxy verb of `kbf-installer`).

use std::net::{Ipv4Addr, SocketAddr};
use std::os::fd::OwnedFd;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kbf_installer::CommitId;
use kbf_installer::deploy::api::{Https, Operator, Reply, Request, Trust};
use kbf_installer::proxy::api::model::{StatusBody, UpdateId, UpdateRequest as ApiRequest};
use kbf_installer::proxy::model::Phase;
use kbf_proxy::config::ProxyConfig;
use kbf_proxy::control::{UpdateHandler, UpdateReply, UpdateRequest};
use kbf_proxy::slot::Slot;
use kbf_proxy::{Proxy, ProxyHandle};
use kbf_testkit::dir::TestDir;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, ExtendedKeyUsagePurpose, IsCa, KeyPair,
    KeyUsagePurpose, SanType,
};
use tonic::codegen::{Bytes, http};

const TOKEN: &str = "operator-token-0123456789-abcdefghijkl";
const COMMIT: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const SECS: Duration = Duration::from_secs(20);

/// What reached the child: method, path, the `Authorization` header and the body's length.
type Seen = Arc<Mutex<Vec<(String, String, String, usize)>>>;

/// What reached the update route: method, path and body.
type Updates = Arc<Mutex<Vec<(String, String, Vec<u8>)>>>;

/// The update handler: records the request and answers as the API does.
struct Api(Updates);

impl UpdateHandler for Api {
    async fn handle(&self, request: UpdateRequest) -> UpdateReply {
        self.0.lock().expect("log").push((
            request.method.clone(),
            request.path.clone(),
            request.body.clone(),
        ));
        match request.method.as_str() {
            "POST" => UpdateReply::json(202, br#"{"update_id":"u-1","phase":"accepted"}"#.to_vec()),
            _ => {
                let status = StatusBody {
                    update_id: Some(UpdateId("u-1".to_owned())),
                    from: None,
                    to: CommitId::new(COMMIT).ok(),
                    phase: Some(Phase::Done),
                    since: Some(1),
                    active_slot: None,
                    restart_pending: false,
                    rollback_allowed: false,
                    rollback_blocked_reason: None,
                    old: None,
                    new: None,
                    failure: None,
                };
                UpdateReply::json(200, serde_json::to_vec(&status).expect("json"))
            }
        }
    }
}

/// The child's operator port: plain HTTP/2, as the proxy dials it. It reads every body to its
/// end and answers by path.
async fn child(listener: tokio::net::TcpListener, seen: Seen) {
    loop {
        let Ok((io, _)) = listener.accept().await else {
            return;
        };
        let seen = Arc::clone(&seen);
        tokio::spawn(async move {
            let Ok(mut conn) = h2::server::handshake(io).await else {
                return;
            };
            while let Some(Ok((request, mut respond))) = conn.accept().await {
                let seen = Arc::clone(&seen);
                tokio::spawn(async move {
                    let (parts, mut body) = request.into_parts();
                    let mut length = 0;
                    while let Some(Ok(chunk)) = body.data().await {
                        length += chunk.len();
                        let _ = body.flow_control().release_capacity(chunk.len());
                    }
                    let token = parts
                        .headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or_default()
                        .to_owned();
                    let (method, path) = (parts.method.to_string(), parts.uri.path().to_owned());
                    seen.lock()
                        .expect("log")
                        .push((method.clone(), path.clone(), token, length));
                    let (status, answer) = match (method.as_str(), path.as_str()) {
                        ("GET", "/v1/nodes") => (
                            200,
                            format!(r#"{{"nodes":[],"server":{{"commit":"{COMMIT}"}}}}"#),
                        ),
                        ("PUT", _) => (201, format!(r#"{{"size":{length}}}"#)),
                        _ => (404, "{}".to_owned()),
                    };
                    let response = http::Response::builder()
                        .status(status)
                        .header("content-type", "application/json")
                        .body(())
                        .expect("a response");
                    if let Ok(mut send) = respond.send_response(response, false) {
                        let _ = send.send_data(Bytes::from(answer), true);
                    }
                });
            }
        });
    }
}

struct Pki {
    ca_pem: String,
    cert: String,
    key: String,
}

fn pki() -> Pki {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let issuer =
        CertifiedIssuer::self_signed(params, KeyPair::generate().expect("key")).expect("CA");
    let mut leaf = CertificateParams::new(Vec::<String>::new()).expect("params");
    leaf.subject_alt_names = vec![SanType::DnsName("localhost".try_into().expect("ia5"))];
    leaf.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let key = KeyPair::generate().expect("key");
    let cert = leaf.signed_by(&key, &issuer).expect("sign");
    Pki {
        ca_pem: issuer.pem(),
        cert: cert.pem(),
        key: key.serialize_pem(),
    }
}

fn private(path: &Path, content: &[u8]) {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::write(path, content).expect("write");
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
}

/// A fresh directory for one run of the test, removed when it drops.
fn dir(name: &str) -> TestDir {
    TestDir::under(
        env!("CARGO_TARGET_TMPDIR"),
        &format!("deploy-proxy-{name}"),
    )
}

/// A proxy over `base`'s ports with slot A active, serving `api` on its update route.
fn start_proxy(dir: &Path, pki: &Pki, base: u16, api: Api) -> (ProxyHandle, SocketAddr) {
    private(&dir.join("server.pem"), pki.cert.as_bytes());
    private(&dir.join("server.key"), pki.key.as_bytes());
    private(&dir.join("ca.pem"), pki.ca_pem.as_bytes());
    private(&dir.join("operator.token"), TOKEN.as_bytes());
    let socket = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
    let public = socket.local_addr().expect("address");
    let config = ProxyConfig::from_toml_str(&format!(
        "operator_token_file = {:?}\nserver_cert_file = {:?}\nserver_key_file = {:?}\n\
         client_ca_file = {:?}\nproxy_auth_file = {:?}\n[upstream]\nbase_port = {base}\n",
        dir.join("operator.token"),
        dir.join("server.pem"),
        dir.join("server.key"),
        dir.join("ca.pem"),
        dir.join("proxy-auth"),
    ))
    .expect("a valid table");
    let (handle, runner) = Proxy::bind(config, OwnedFd::from(socket)).expect("bind the proxy");
    std::thread::spawn(move || runner.run(api));
    handle.slots().adopt(Slot::A, None);
    handle.slots().release_into(Slot::A).expect("A is open");
    (handle, public)
}

fn client(addr: SocketAddr, pki: &Pki, token: &str) -> Https {
    let trust = Trust {
        server_name: Some("localhost".to_owned()),
        ca_pem: Some(pki.ca_pem.clone().into_bytes()),
    };
    Https::new(addr, &trust, token.to_owned(), SECS).expect("a client")
}

fn send(https: &Https, request: &Request) -> Reply {
    https.send(request).expect("an answer")
}

/// The calls of a deploy reach the right server and come back whole: the update API's two
/// routes end at the proxy's handler with the body the deploy built, the other `/v1/*` calls
/// (the node list, a release upload of three MiB) end at the active child with the token,
/// and a wrong token is refused by the proxy before the child hears of it.
///
/// Mutants seen red: the bearer token sent under another name (the first POST is 401); a
/// `Content-Length` one short of the body's (the update request arrives cut).
#[test]
fn a_deploy_reaches_the_update_api_and_the_active_child_through_the_proxy() {
    let dir = dir("calls");
    let pki = pki();
    let child_runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("a runtime");
    let operator = std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind");
    operator.set_nonblocking(true).expect("nonblocking");
    // The child's operator listener is slot A's base port + 2.
    let base = operator.local_addr().expect("address").port() - 2;
    let seen: Seen = Arc::default();
    let listener = child_runtime
        .block_on(async { tokio::net::TcpListener::from_std(operator) })
        .expect("listener");
    child_runtime.spawn(child(listener, Arc::clone(&seen)));
    let updates = Arc::default();
    let (handle, public) = start_proxy(&dir, &pki, base, Api(Arc::clone(&updates)));
    let https = client(public, &pki, TOKEN);

    // The update API: the proxy's own route, with the request the deploy builds.
    let body = ApiRequest {
        commit: CommitId::new(COMMIT).expect("commit"),
        request_id: "deploy-test".to_owned(),
        nodes: true,
        rollback: false,
    };
    let posted = send(&https, &Request::post_json("/v1/update/frontend", &body));
    assert_eq!(
        posted.status,
        202,
        "{}",
        String::from_utf8_lossy(&posted.body)
    );
    let status = send(&https, &Request::get("/v1/update/frontend"));
    let status: StatusBody = serde_json::from_slice(&status.body).expect("a status");
    assert_eq!(status.phase, Some(Phase::Done));
    {
        let log = updates.lock().expect("log");
        assert_eq!(log.len(), 2);
        assert_eq!(
            (log[0].0.as_str(), log[0].1.as_str()),
            ("POST", "/v1/update/frontend")
        );
        let sent: ApiRequest = serde_json::from_slice(&log[0].2).expect("the API's request");
        assert_eq!(sent, body);
        assert_eq!(
            (log[1].0.as_str(), log[1].1.as_str()),
            ("GET", "/v1/update/frontend")
        );
    }

    // Everything else goes to the active child, with the token.
    let nodes = send(&https, &Request::get("/v1/nodes"));
    assert_eq!(nodes.status, 200);
    assert!(String::from_utf8_lossy(&nodes.body).contains(COMMIT));
    let component = vec![7u8; 3 * 1024 * 1024];
    let put = Request::put_raw(
        &format!("/v1/releases/{COMMIT}/linux-x86_64/kbf"),
        component,
    );
    let stored = send(&https, &put);
    assert_eq!(stored.status, 201);
    assert_eq!(
        stored.body,
        format!(r#"{{"size":{}}}"#, 3 * 1024 * 1024).into_bytes()
    );
    {
        let log = seen.lock().expect("log");
        let bearer = format!("Bearer {TOKEN}");
        assert_eq!(log.len(), 2, "{log:?}");
        assert_eq!((log[0].0.as_str(), log[0].1.as_str()), ("GET", "/v1/nodes"));
        assert_eq!(log[0].2, bearer);
        assert_eq!(log[1].0, "PUT");
        assert_eq!(
            (log[1].2.as_str(), log[1].3),
            (bearer.as_str(), 3 * 1024 * 1024)
        );
    }

    // A wrong token is refused at the proxy, for the update API and for the child's routes.
    let wrong = client(public, &pki, "not-the-operator-token-0123456789");
    assert_eq!(send(&wrong, &Request::get("/v1/nodes")).status, 401);
    assert_eq!(
        send(&wrong, &Request::get("/v1/update/frontend")).status,
        401
    );
    assert_eq!(
        seen.lock().expect("log").len(),
        2,
        "the child heard nothing of it"
    );
    assert_eq!(updates.lock().expect("log").len(), 2);
    handle.shutdown();
}
