//! `cli::run` over real sockets: two listening fakes, one for the update API (`--api`) and one
//! for the frontend the gates read. The unit tests hand `deploy` its clients; these prove the
//! command line builds them from the right addresses, which no fake client can.
//!
//! The update API answers a preflight that passes; the observed frontend's `/metrics` reports
//! storage under-replicated, so the deploy ends at the preflight gate (exit 3, `gate_red`)
//! without changing anything. What the test looks at is which listener was asked what.

use std::fs;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::cli::{self, FrontendConfig, Host, current_uid};
use crate::testkit::{A, B};

/// A listener that answers `GET`s from `answer(path)` as `(status, body)` and keeps the
/// request lines it saw.
struct Fake {
    addr: SocketAddr,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Fake {
    fn start(answer: impl Fn(&str) -> (u16, String) + Send + 'static) -> Fake {
        Fake::on(TcpListener::bind("127.0.0.1:0").unwrap(), answer)
    }

    fn on(listener: TcpListener, answer: impl Fn(&str) -> (u16, String) + Send + 'static) -> Fake {
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&seen);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { return };
                let mut head = Vec::new();
                let mut byte = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if stream.read(&mut byte).unwrap_or(0) == 0 {
                        break;
                    }
                    head.push(byte[0]);
                }
                let text = String::from_utf8_lossy(&head).into_owned();
                let line = text.lines().next().unwrap_or("").to_owned();
                let path = line.split(' ').nth(1).unwrap_or("").to_owned();
                log.lock().unwrap().push(line);
                let (status, body) = answer(&path);
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        Fake { addr, seen }
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }

    fn saw_metrics(&self) -> bool {
        self.seen().iter().any(|l| l.starts_with("GET /metrics"))
    }
}

/// An update API whose preflight passes for `A` on one Linux node.
fn update_api() -> Fake {
    Fake::start(|path| {
        let body: Option<Value> = match path {
            "/v1/nodes" => Some(json!({"server": {"commit": &A[..12]}, "nodes": [{
                "node_id": "n1", "connected": true, "placement": {"state": "serving"},
                "update": {"state": "confirmed", "running_commit": &B[..12], "platform": "linux-x86_64",
                           "offered_generation": 7, "up_to_date": false, "availability": "ok"}}]})),
            "/v1/releases" => Some(json!({"releases": [{"commit": A, "platforms": [{
                "platform": "linux-x86_64", "complete": true,
                "components": [{"name": "kbf", "sha256": "x", "size": 1, "attested": true}]}]}]})),
            "/v1/update" => Some(json!({"desired": {"commit": B, "generation": 7},
                                        "frontend": {"order_id": 1, "state": "confirmed"}})),
            "/v1/update/frontend" => Some(json!({"active_slot": null})),
            _ => None,
        };
        body.map_or((404, "{}".to_owned()), |b| (200, b.to_string()))
    })
}

/// A frontend whose storage is under-replicated.
fn frontend() -> Fake {
    Fake::start(|path| {
        match path {
        "/metrics" => (
            200,
            "kbf_frontend_relay_untraced 0\nkbf_cache_lost_blobs 0\nkbf_cache_blob_losses_total 1\n\
             kbf_under_replicated_blobs 2\n"
                .to_owned(),
        ),
        _ => (404, "{}".to_owned()),
    }
    })
}

struct Run {
    code: i32,
    summary: Value,
}

fn run(name: &str, api: &Fake, observe_flags: &[String]) -> Run {
    let (_, dir) = crate::testkit::host_root(&format!("deploy-observe-{name}"));
    let token = dir.join("token");
    fs::write(&token, "0123456789abcdef0123456789abcdef0123456789abcdef\n").unwrap();
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
    let mut args: Vec<String> = [
        "--commit",
        A,
        "--api",
        &api.addr.to_string(),
        "--token-file",
        token.to_str().unwrap(),
        "--lock",
        dir.join("lock").to_str().unwrap(),
        "--result",
        dir.join("result.json").to_str().unwrap(),
        "--platform",
        "linux-x86_64",
        "--apply",
    ]
    .map(str::to_owned)
    .to_vec();
    args.extend(observe_flags.iter().cloned());
    let host = Host {
        root: true,
        config_owner: current_uid(),
        frontend: FrontendConfig {
            toml: dir.join("none.toml"),
            units: Vec::new(),
        },
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = cli::run(
        &args,
        &mut std::io::empty(),
        &mut out,
        &mut err,
        false,
        &host,
    );
    let line = String::from_utf8(out).unwrap();
    let summary = serde_json::from_str(line.trim()).unwrap_or_else(|_| {
        panic!("no summary: {line:?} {}", String::from_utf8_lossy(&err));
    });
    Run { code, summary }
}

fn ended_at_the_storage_gate(r: &Run) {
    assert_eq!(r.code, 3, "{}", r.summary);
    assert_eq!(r.summary["reason"], "gate_red", "{}", r.summary);
    assert!(
        r.summary["detail"]
            .as_str()
            .unwrap()
            .contains("e_under_replicated"),
        "{}",
        r.summary
    );
}

/// Proves `--observe` makes the command line read `/metrics` from that address and not from
/// `--api`: the observed listener is asked for `/metrics`, the update API never is, and the
/// deploy ends at the storage gate the observed listener's metrics decide. Catches an
/// observe client built from the update API's address.
///
/// Mutants planted: `observe` defaulting to `--api` in `cli::run`.
#[test]
fn observe_names_the_listener_the_gates_read() {
    let (api, front) = (update_api(), frontend());
    let r = run(
        "fixed",
        &api,
        &["--observe".to_owned(), front.addr.to_string()],
    );
    ended_at_the_storage_gate(&r);
    assert!(front.saw_metrics(), "{:?}", front.seen());
    assert!(
        !api.seen()
            .iter()
            .any(|l| l.contains("/metrics") || l.contains("/v1/invocations")),
        "gate reads reached --api: {:?}",
        api.seen()
    );
    assert!(api.seen().iter().any(|l| l.starts_with("GET /v1/nodes")));
}

/// Proves `--observe-base-port` finds the serving child by its slot's operator port (base
/// plus 2 for slot A, plus 12 for slot B): with the update API naming no serving slot, the
/// only child that answers is read. Here slot B's port is closed and slot A's is the fake.
/// Catches a wrong offset and a probe that does not run.
///
/// Mutants planted: slot A's operator offset changed.
#[test]
fn observe_base_port_reads_the_child_that_answers() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let front = Fake::on(listener, |path| match path {
        "/metrics" => (
            200,
            "kbf_under_replicated_blobs 2\nkbf_cache_lost_blobs 0\n".to_owned(),
        ),
        _ => (404, "{}".to_owned()),
    });
    let api = update_api();
    let base = (port - 2).to_string();
    let r = run("slots", &api, &["--observe-base-port".to_owned(), base]);
    assert!(front.saw_metrics(), "{:?} {}", front.seen(), r.summary);
    assert!(
        !api.seen().iter().any(|l| l.contains("/metrics")),
        "{:?}",
        api.seen()
    );
    assert_eq!(r.code, 3, "{}", r.summary);
}
