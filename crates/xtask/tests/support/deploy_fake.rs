//! The fakes of the `deploy-farm` process tests: a coordinator and a runner token service on
//! loopback over TLS with a CA of the test's own, a virtual clock, and the flags of a run.
//!
//! A test file includes it with `#[path = "support/deploy_fake.rs"] mod deploy_fake;`.
#![allow(dead_code)] // Each test binary uses its own part of this module.

use std::cell::{Cell, RefCell};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rcgen::{CertificateParams, CertifiedIssuer, IsCa, KeyPair};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use serde_json::{Value, json};
use xtask::deploy_farm::{Args, Clock, Error, Plan, Roots, TokenService, deploy_on};

pub const RUNNER_KEY: &str = "runner-key";

/// One request the fake saw.
#[derive(Clone, Debug)]
pub struct Seen {
    pub method: String,
    pub path: String,
    pub authorization: String,
    pub body: String,
}

/// What the fake coordinator answers.
pub struct Script {
    /// The answers of `GET /v1/convergence`, one per read (the first is the baseline); the
    /// last repeats. A number is a
    /// status with no body.
    pub reads: Vec<Result<Value, u16>>,
    /// The status of `POST /v1/desired`.
    pub post: u16,
    /// How many of the first POSTs answer 503 before `post` is used.
    pub post_503s: usize,
    /// The status of the runner's token service.
    pub token: u16,
    /// What the token service answers instead of a token, as bytes.
    pub token_answer: Option<Vec<u8>>,
}

impl Script {
    /// A script whose first answer is the baseline read before the POST (a farm of two
    /// storage hosts and one daemon, one more daemon down), then `reads`.
    pub fn new(reads: Vec<Result<Value, u16>>) -> Self {
        Self::after(Ok(farm("converged", 0, 0)), reads)
    }

    /// A script whose baseline read is `baseline`, then `reads`.
    pub fn after(baseline: Result<Value, u16>, reads: Vec<Result<Value, u16>>) -> Self {
        let mut all = vec![baseline];
        all.extend(reads);
        Self {
            reads: all,
            post: 200,
            post_503s: 0,
            token: 200,
            token_answer: None,
        }
    }
}

pub struct Fake {
    pub base: String,
    pub ca: CertificateDer<'static>,
    pub ca_pem: String,
    pub seen: Arc<Mutex<Vec<Seen>>>,
}

impl Fake {
    pub fn requests(&self, path: &str) -> Vec<Seen> {
        let seen = self.seen.lock().expect("lock");
        seen.iter()
            .filter(|r| r.path.starts_with(path))
            .cloned()
            .collect()
    }

    pub fn service(&self) -> TokenService {
        TokenService {
            url: format!("{}/token?api-version=2.0", self.base),
            key: RUNNER_KEY.to_owned(),
        }
    }

    pub fn plan(&self) -> Plan {
        Plan {
            url: self.base.clone(),
            service: "kbf".to_owned(),
            audience: self.base.clone(),
            version: "2.0.0+g000000000002".to_owned(),
            min_up_storage: 1,
            min_up_frontend: 1,
            require_coordinator_current: false,
            deadline: Duration::from_secs(30),
            stall: Duration::from_secs(30),
            every: Duration::from_secs(1),
        }
    }
}

pub fn response(status: u16, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

pub fn read_request(stream: impl Read) -> Option<Seen> {
    let mut reader = BufReader::new(stream);
    let mut first = String::new();
    reader.read_line(&mut first).ok()?;
    let mut words = first.split_whitespace();
    let method = words.next()?.to_owned();
    let path = words.next()?.to_owned();
    let (mut length, mut authorization) = (0, String::new());
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':')?;
        match name.to_ascii_lowercase().as_str() {
            "content-length" => length = value.trim().parse().ok()?,
            "authorization" => authorization = value.trim().to_owned(),
            _ => {}
        }
    }
    let mut body = vec![0; length];
    reader.read_exact(&mut body).ok()?;
    Some(Seen {
        method,
        path,
        authorization,
        body: String::from_utf8(body).ok()?,
    })
}

/// A TLS server on loopback that answers by `script`.
pub fn serve(script: Script) -> Fake {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(params, KeyPair::generate().expect("key")).expect("CA");
    let key = KeyPair::generate().expect("key");
    let cert = CertificateParams::new(vec!["127.0.0.1".to_owned()])
        .expect("params")
        .signed_by(&key, &ca)
        .expect("sign");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = Arc::new(
        rustls::ServerConfig::builder_with_provider(provider)
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
    let seen = Arc::new(Mutex::new(Vec::new()));
    let log = Arc::clone(&seen);
    let tokens = AtomicUsize::new(0);
    let reads = AtomicUsize::new(0);
    let posts = AtomicUsize::new(0);
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let conn = rustls::ServerConnection::new(Arc::clone(&config)).expect("conn");
            let mut tls = rustls::StreamOwned::new(conn, stream.expect("accept"));
            let Some(request) = read_request(&mut tls) else {
                continue;
            };
            let answer = if request.path.starts_with("/token") {
                if request.authorization != format!("Bearer {RUNNER_KEY}") || script.token != 200 {
                    response(script.token.max(401), "{}")
                } else if let Some(raw) = &script.token_answer {
                    let mut answer = format!(
                        "HTTP/1.1 200 X\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        raw.len()
                    )
                    .into_bytes();
                    answer.extend_from_slice(raw);
                    answer
                } else {
                    let n = tokens.fetch_add(1, Ordering::SeqCst);
                    response(200, &json!({"value": format!("tok-{n}")}).to_string())
                }
            } else if !request.authorization.starts_with("Bearer tok-") {
                response(401, r#"{"error":"unauthorized"}"#)
            } else if request.method == "POST" {
                if posts.fetch_add(1, Ordering::SeqCst) < script.post_503s {
                    response(503, "{}")
                } else {
                    response(script.post, "{}")
                }
            } else if request.path == "/v1/elsewhere" {
                response(200, &farm("converged", 0, 0).to_string())
            } else {
                let at = reads
                    .fetch_add(1, Ordering::SeqCst)
                    .min(script.reads.len() - 1);
                match &script.reads[at] {
                    Ok(body) => response(200, &body.to_string()),
                    Err(302) => b"HTTP/1.1 302 X\r\nLocation: /v1/elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
                    Err(status) => response(*status, "{}"),
                }
            };
            log.lock().expect("lock").push(request);
            let _ = tls.write_all(&answer);
            tls.conn.send_close_notify();
            let _ = tls.flush();
        }
    });
    Fake {
        base,
        ca: ca.der().clone(),
        ca_pem: ca.pem(),
        seen,
    }
}

pub fn farm(state: &str, updating: u64, failed: u64) -> Value {
    json!({"desired": "2.0.0+g000000000002", "quiet": false, "state": state, "roles": [
        {"role": "storage", "at_desired": 2, "updating": updating, "failed": failed, "down": 0},
        {"role": "daemon", "at_desired": 1, "updating": 0, "failed": 0, "down": 1},
    ]})
}

/// A virtual clock: `sleep` moves `now` and is recorded, so no test waits for a deadline.
#[derive(Default)]
pub struct Time {
    pub now: Cell<Duration>,
    pub slept: RefCell<Vec<Duration>>,
}

impl Time {
    pub fn clock(&self) -> Clock<'_> {
        Clock {
            now: Box::new(|| self.now.get()),
            sleep: Box::new(|d| {
                self.now.set(self.now.get() + d);
                self.slept.borrow_mut().push(d);
            }),
        }
    }
}

/// `deploy_on` a fresh virtual clock.
pub fn deploy(
    plan: &Plan,
    service: &TokenService,
    roots: &Roots<'_>,
    out: &mut impl Write,
) -> Result<(), Error> {
    deploy_on(plan, service, roots, &Time::default().clock(), out)
}

pub fn run(fake: &Fake, plan: &Plan) -> (Result<(), Error>, String) {
    run_timed(fake, plan, &Time::default())
}

pub fn run_timed(fake: &Fake, plan: &Plan, time: &Time) -> (Result<(), Error>, String) {
    let mut out = Vec::new();
    let ca = [fake.ca.clone()];
    let roots = Roots {
        runner: &ca,
        coordinator: &ca,
    };
    let result = deploy_on(plan, &fake.service(), &roots, &time.clock(), &mut out);
    (result, String::from_utf8(out).expect("utf8"))
}

pub fn args_for(fake: &Fake, ca_pem: &str) -> Args {
    use clap::Parser;
    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: Args,
    }
    Cli::parse_from([
        "x",
        "--url",
        &fake.base,
        "--version",
        "2.0.0+g000000000002",
        "--ca-pem",
        ca_pem,
        "--every-secs",
        "1",
    ])
    .args
}

/// A CA certificate that signed nothing the fake serves.
pub fn fake_ca() -> (Vec<CertificateDer<'static>>, String) {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(params, KeyPair::generate().expect("key")).expect("CA");
    (vec![ca.der().clone()], ca.pem())
}

pub fn service_args(fake: &Fake, service: &str, version: &str) -> Args {
    let mut args = args_for(fake, &fake.ca_pem);
    args.service = service.to_owned();
    args.version = version.to_owned();
    args
}
