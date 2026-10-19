//! `--proxy-auth-file` on a real `kbf frontend` (`start_frontend`): the frontend builds its
//! worker listener, REAPI listener and operator API itself (it does not call
//! `bind_server_with_state`), so it must hand each of them the proxy's secret. Plain-text
//! listeners, as behind the proxy (which terminates TLS), with no TLS flag.

use std::os::unix::fs::PermissionsExt as _;

use futures::channel::mpsc::unbounded;
use kbf_proto::worker::{
    Capability, DaemonMessage, Hello, ServerMessage, daemon_message, server_message,
    worker_client::WorkerClient,
};
use tonic::Code;
use tonic::metadata::{MetadataKey, MetadataValue};

use super::*;

const SECRET: [u8; 32] = [0x3c; 32];
const TOKEN: &str = "kbf-frontend-token-0123456789abcdef0123456789";

fn private_file(dir: &Path, name: &str, content: &[u8]) -> String {
    let path = dir.join(name);
    std::fs::write(&path, content).expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    path.to_str().expect("UTF-8").to_owned()
}

/// A worker session for `node-a` carrying `claims` as request headers: `Welcome`, or the
/// status the frontend refused it with.
async fn session(worker: SocketAddr, claims: &[(&str, String)]) -> Result<(), tonic::Status> {
    let entry = |key: &str, value: &str| Capability {
        key: key.to_owned(),
        value: value.to_owned(),
    };
    let (tx, rx) = unbounded();
    tx.unbounded_send(DaemonMessage {
        message: Some(daemon_message::Message::Hello(Hello {
            protocol_version: 1,
            node_id: "node-a".to_owned(),
            capabilities: vec![
                entry("arch", "x86_64"),
                entry("cpus", "1"),
                entry("mem_gib", "1"),
                entry("drivers", "fake"),
            ],
            ..Hello::default()
        })),
    })
    .expect("open");
    let mut request = tonic::Request::new(rx);
    for (name, value) in claims {
        let key = MetadataKey::from_bytes(name.as_bytes()).expect("a key");
        let value: MetadataValue<_> = value.parse().expect("a value");
        request.metadata_mut().insert(key, value);
    }
    let channel = Endpoint::from_shared(format!("http://{worker}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    let mut inbound = WorkerClient::new(channel)
        .session(request)
        .await?
        .into_inner();
    match inbound.message().await? {
        Some(ServerMessage {
            message: Some(server_message::Message::Welcome(_)),
        }) => Ok(()),
        other => panic!("expected Welcome or an error, got {other:?}"),
    }
}

/// One POST of a cordon of an unknown node with `headers`; the status code (404 is "past
/// every gate").
async fn cordon(api: SocketAddr, headers: &[(&str, &str)]) -> u16 {
    let mut stream = TcpStream::connect(api).await.expect("connect to the API");
    let mut request = String::from(
        "POST /v1/nodes/ghost:cordon HTTP/1.1\r\nHost: kbf\r\nContent-Length: 0\r\n\
         Content-Type: application/json\r\n",
    );
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("Connection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).await.expect("send");
    let mut response = String::new();
    stream.read_to_string(&mut response).await.expect("read");
    response
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("no status in {response:?}"))
}

/// Catches a frontend that reads `--proxy-auth-file` and builds a listener without it: its
/// worker listener must take the proxy's identity and refuse a caller that does not present
/// the secret (no TLS flag is given), and its operator API must refuse a local write that
/// lacks the secret even with the token, and a write through the proxy that lacks the token.
///
/// Mutant seen red: the frontend's worker listener built with no proxy.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_behind_the_proxy_takes_its_identity_and_gates_writes() {
    let dir = scratch_dir("proxy-auth");
    let secret = private_file(&dir, "proxy-secret", &SECRET);
    let token = private_file(&dir, "token", format!("{TOKEN}\n").as_bytes());
    let front = Front::start(
        &flags(&[
            "--storage=http://127.0.0.1:1",
            &format!("--proxy-auth-file={secret}"),
            &format!("--api-token-file={token}"),
        ])
        .expect("flags"),
    )
    .await;
    let presented = hex::encode(SECRET);
    let forwarded = |node: &str, role: &str| {
        vec![
            ("x-kbf-proxy-auth", presented.clone()),
            ("x-kbf-node-id", node.to_owned()),
            ("x-kbf-role", role.to_owned()),
        ]
    };

    let anonymous = [
        ("x-kbf-node-id", "node-a".to_owned()),
        ("x-kbf-role", "daemon".to_owned()),
    ];
    let refused = session(front.worker, &anonymous).await;
    assert_eq!(
        refused.expect_err("no secret").code(),
        Code::Unauthenticated
    );
    session(front.worker, &forwarded("node-a", "daemon"))
        .await
        .expect("the proxy vouched for node-a");
    let forged = session(front.worker, &forwarded("node-b", "daemon")).await;
    assert_eq!(forged.expect_err("forged").code(), Code::PermissionDenied);

    let bearer = format!("Bearer {TOKEN}");
    assert_eq!(cordon(front.api, &[("Authorization", &bearer)]).await, 403);
    assert_eq!(
        cordon(front.api, &[("x-kbf-proxy-auth", &presented)]).await,
        401
    );
    let through = [
        ("x-kbf-proxy-auth", presented.as_str()),
        ("Authorization", &bearer),
    ];
    assert_eq!(cordon(front.api, &through).await, 404);
}

/// The status of a REAPI ByteStream read for an absent blob carrying `claims` as headers.
async fn reapi_read(reapi: SocketAddr, claims: &[(&str, String)]) -> Code {
    use kbf_proto::google::bytestream::ReadRequest;
    use kbf_proto::google::bytestream::byte_stream_client::ByteStreamClient;
    let mut request = tonic::Request::new(ReadRequest {
        resource_name: format!("blobs/{}/0", "0".repeat(64)),
        read_offset: 0,
        read_limit: 0,
    });
    for (name, value) in claims {
        let key = MetadataKey::from_bytes(name.as_bytes()).expect("a key");
        let value: MetadataValue<_> = value.parse().expect("a value");
        request.metadata_mut().insert(key, value);
    }
    let channel = Endpoint::from_shared(format!("http://{reapi}"))
        .expect("endpoint")
        .connect()
        .await
        .expect("connect");
    match ByteStreamClient::new(channel).read(request).await {
        Ok(stream) => stream
            .into_inner()
            .message()
            .await
            .map_or_else(|s| s.code(), |_| Code::Ok),
        Err(status) => status.code(),
    }
}

/// Catches a frontend whose REAPI listener is built without the proxy's secret: a call
/// that presents the secret with an identity that is not one well-formed value must be
/// refused `PERMISSION_DENIED` by the proxy layer before anything else reads it; without
/// the layer it reaches the storage lookup and answers some other status.
///
/// Mutant seen red: the frontend's REAPI listener built with `ProxyAuthLayer::new(None)`.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontends_reapi_listener_is_behind_the_proxy() {
    let dir = scratch_dir("proxy-auth-reapi");
    let secret = private_file(&dir, "proxy-secret", &SECRET);
    let front = Front::start(
        &flags(&[
            "--storage=http://127.0.0.1:1",
            &format!("--proxy-auth-file={secret}"),
        ])
        .expect("flags"),
    )
    .await;
    let claims = [
        ("x-kbf-proxy-auth", hex::encode(SECRET)),
        ("x-kbf-role", "admin".to_owned()),
    ];
    assert_eq!(
        reapi_read(front.reapi, &claims).await,
        Code::PermissionDenied
    );
}
