//! What the test files share: the golden bodies, a pool, requests, and a raw HTTP call that
//! does not go through the client under test.
#![allow(dead_code)]

use kbf_lease_client::fake::{FakeLeaseServer, Pool};
use kbf_lease_client::{AcquireRequest, Client, LeaseId, Resources, RunLimits, RunSpec};
use serde_json::Value;
use std::time::Duration;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

pub const IMAGE: &str =
    "reg.example/kbf-task@sha256:0000000000000000000000000000000000000000000000000000000000000000";
pub const GIB: u64 = 1 << 30;

/// A golden body, parsed.
pub fn golden(name: &str) -> Value {
    let path = format!("{}/tests/golden/{name}.json", env!("CARGO_MANIFEST_DIR"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path}: {e}"));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}"))
}

/// Two hosts of 16 cores and 64 GiB under the class `kbf-linux`.
pub fn pool() -> Pool {
    Pool::new("kbf-linux", &["node1", "node2"], 16_000, 64 * GIB)
}

pub async fn fake() -> (FakeLeaseServer, Client) {
    let server = FakeLeaseServer::start(pool()).await;
    let client = server.client().unwrap();
    (server, client)
}

/// A request for a whole host of `pool()`.
pub fn whole_host() -> AcquireRequest {
    let resources = Resources {
        cpu_millis: 16_000,
        memory_bytes: 64 * GIB,
        ..Resources::default()
    };
    AcquireRequest::new("kbf-linux", resources, Duration::from_secs(6 * 3600))
}

/// A lease on a host of `pool()`, granted.
pub async fn lease(client: &Client) -> LeaseId {
    match client.acquire(&whole_host()).await.unwrap() {
        kbf_lease_client::Acquired::Granted(granted) => granted.id,
        other => panic!("expected a granted lease, got {other:?}"),
    }
}

/// A run of `cpu_millis` and `gib` GiB.
pub fn run_of(cpu_millis: u64, gib: u64) -> RunSpec {
    let limits = RunLimits {
        cpu_millis,
        memory_bytes: gib * GIB,
        pids: None,
        wall_ms: None,
        disk_bytes: None,
    };
    RunSpec::new(IMAGE, limits)
}

/// The numbers and strings of a body replaced by placeholders, so two bodies compare by
/// their fields and kinds.
pub fn shape(value: &Value) -> Value {
    match value {
        Value::Number(_) => Value::from(0),
        Value::String(_) => Value::from("s"),
        Value::Array(items) => Value::Array(items.iter().map(shape).collect()),
        Value::Object(map) => {
            Value::Object(map.iter().map(|(k, v)| (k.clone(), shape(v))).collect())
        }
        other => other.clone(),
    }
}

/// One HTTP/1.1 exchange written by hand; the status and the body.
pub async fn raw(
    origin: &str,
    token: Option<&str>,
    method: &str,
    path: &str,
    body: &str,
) -> (u16, String) {
    let authority = origin.trim_start_matches("http://");
    let mut tcp = tokio::net::TcpStream::connect(authority).await.unwrap();
    let auth = token.map_or_else(String::new, |t| format!("Authorization: {t}\r\n"));
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {authority}\r\n{auth}Connection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    tcp.write_all(request.as_bytes()).await.unwrap();
    let mut answer = String::new();
    tcp.read_to_string(&mut answer).await.unwrap();
    let (head, body) = answer.split_once("\r\n\r\n").unwrap();
    let status = head.split(' ').nth(1).unwrap().parse().unwrap();
    (status, body.to_owned())
}

/// `raw` with the fake's own token, the body parsed as JSON (null when empty).
pub async fn raw_json(
    server: &FakeLeaseServer,
    method: &str,
    path: &str,
    body: &str,
) -> (u16, Value) {
    let token = format!("Bearer {}", kbf_lease_client::fake::TOKEN);
    let (status, text) = raw(&server.origin(), Some(&token), method, path, body).await;
    let parsed = if text.is_empty() {
        Value::Null
    } else {
        serde_json::from_str(&text).unwrap()
    };
    (status, parsed)
}

/// The status and code of a refusal; panics on any other error.
pub fn refused(error: kbf_lease_client::Error) -> (u16, kbf_lease_client::ErrorCode) {
    let api = error
        .api()
        .unwrap_or_else(|| panic!("not a refusal: {error:?}"));
    (api.status, api.code.clone())
}
