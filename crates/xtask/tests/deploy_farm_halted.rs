//! `deploy-farm` against a coordinator that answers `halted`: the wait ends at once, as a
//! failure that names the host and the check.

use std::cell::Cell;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rcgen::{CertificateParams, CertifiedIssuer, IsCa, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde_json::{Value, json};
use xtask::deploy_farm::{Clock, Plan, Roots, TokenService, deploy_on};

const VERSION: &str = "2.0.0+g000000000002";

/// A coordinator and token service on loopback, over TLS with a CA of the test's own.
struct Fake {
    base: String,
    ca: CertificateDer<'static>,
    reads: Arc<AtomicUsize>,
}

fn response(status: u16, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// The method and path of a request, with its body read and dropped.
fn read_request(stream: impl Read) -> Option<(String, String)> {
    let mut reader = BufReader::new(stream);
    let mut first = String::new();
    reader.read_line(&mut first).ok()?;
    let mut words = first.split_whitespace();
    let (method, path) = (words.next()?.to_owned(), words.next()?.to_owned());
    let mut length = 0;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            length = value.trim().parse().ok()?;
        }
    }
    reader.read_exact(&mut vec![0; length]).ok()?;
    Some((method, path))
}

/// The convergence reads: the baseline, then `halted` with `halt` (when there is one).
fn serve(halt: Option<Value>) -> Fake {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(params, KeyPair::generate().expect("key")).expect("CA");
    let key = KeyPair::generate().expect("key");
    let cert = CertificateParams::new(vec!["127.0.0.1".to_owned()])
        .expect("params")
        .signed_by(&key, &ca)
        .expect("sign");
    let config = Arc::new(
        rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der())),
        )
        .expect("server config"),
    );
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let base = format!(
        "https://127.0.0.1:{}",
        listener.local_addr().expect("addr").port()
    );
    let reads = Arc::new(AtomicUsize::new(0));
    let counted = Arc::clone(&reads);
    let tokens = Mutex::new(0);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let conn = rustls::ServerConnection::new(Arc::clone(&config)).expect("conn");
            let mut tls = rustls::StreamOwned::new(conn, stream.expect("accept"));
            let Some((method, path)) = read_request(&mut tls) else {
                continue;
            };
            let answer = if path.starts_with("/token") {
                let mut n = tokens.lock().expect("lock");
                *n += 1;
                response(200, &json!({"value": format!("tok-{n}")}).to_string())
            } else if method == "POST" {
                response(200, "{}")
            } else {
                let mut body = json!({"desired": VERSION, "quiet": false, "roles": [
                    {"role": "storage", "at_desired": 2, "updating": 0, "failed": 0, "down": 0}
                ]});
                if counted.fetch_add(1, Ordering::SeqCst) == 0 {
                    body["state"] = json!("converged");
                } else {
                    body["state"] = json!("halted");
                    if let Some(halt) = &halt {
                        body["halt"] = halt.clone();
                    }
                }
                response(200, &body.to_string())
            };
            let _ = tls.write_all(&answer);
            tls.conn.send_close_notify();
            let _ = tls.flush();
        }
    });
    Fake {
        base,
        ca: ca.der().clone(),
        reads,
    }
}

fn deploy(fake: &Fake) -> (Result<(), xtask::deploy_farm::Error>, String) {
    let plan = Plan {
        url: fake.base.clone(),
        service: "kbf".to_owned(),
        audience: fake.base.clone(),
        version: VERSION.to_owned(),
        min_up_storage: 1,
        min_up_frontend: 1,
        require_coordinator_current: false,
        deadline: Duration::from_secs(30),
        stall: Duration::from_secs(30),
        every: Duration::from_secs(1),
    };
    let service = TokenService {
        url: format!("{}/token?api-version=2.0", fake.base),
        key: "runner-key".to_owned(),
    };
    let ca = [fake.ca.clone()];
    let roots = Roots {
        runner: &ca,
        coordinator: &ca,
    };
    let now = Cell::new(Duration::ZERO);
    let clock = Clock {
        now: Box::new(|| now.get()),
        sleep: Box::new(|d| now.set(now.get() + d)),
    };
    let mut out = Vec::new();
    let result = deploy_on(&plan, &service, &roots, &clock, &mut out);
    (result, String::from_utf8(out).expect("utf8"))
}

/// Proves: `halted` ends the wait at the read that says it, as a failure that names the host,
/// the check and the version, after the table was printed; no read follows. An unknown state
/// would only have stalled until the stall limit.
#[test]
fn a_halted_farm_fails_at_once_naming_the_host_and_the_check() {
    let fake = serve(Some(
        json!({"node_id": "s1", "check": "disk_tainted", "target": VERSION}),
    ));
    let (result, out) = deploy(&fake);
    let failure = result.expect_err("a halted farm is a failure").to_string();
    assert!(failure.starts_with("halted"), "{failure}");
    assert!(failure.contains("s1"), "{failure}");
    assert!(failure.contains("disk_tainted"), "{failure}");
    assert!(failure.contains(VERSION), "{failure}");
    assert_eq!(fake.reads.load(Ordering::SeqCst), 2);
    assert!(
        out.contains(&format!("farm: halted, desired {VERSION}")),
        "{out}"
    );
}

/// Proves: a coordinator that says `halted` with no `halt` object is still a failure, and the
/// message says the host is not known instead of naming a blank.
#[test]
fn a_halted_answer_without_the_halt_still_fails() {
    let fake = serve(None);
    let (result, _) = deploy(&fake);
    let failure = result.expect_err("a halted farm is a failure").to_string();
    assert!(failure.starts_with("halted"), "{failure}");
    assert!(failure.contains("unknown"), "{failure}");
    assert_eq!(fake.reads.load(Ordering::SeqCst), 2);
}
