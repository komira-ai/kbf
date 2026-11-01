//! `kbf-deploy host-status`, run as the binary against a fake coordinator: an HTTPS server
//! on a loopback port under a fresh CA, which the verb is told to trust with `--ca-pem`.
//!
//! The fake answers `GET /v1/status` the way the coordinator does (the shape pinned by
//! `kbf-coordinator`'s status tests), with the operator token as the bearer.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};

use kbf_testkit::dir::TestDir;

/// The operator token of these tests: visible ASCII, longer than the shortest a token may be.
const TOKEN: &str = "k9Qz-OPERATOR-TOKEN-7f3a1c5e9b2d4f60aa11";

/// What the coordinator answers `GET /v1/status`: kbf's two hosts (one of them excluded and
/// the service halted by it), and another service with one of the same nodes.
const STATUS: &str = r#"{
  "desired": {"version": "2.1"},
  "services": {"kbf": {"version": "2.1"}},
  "quiet": false,
  "services_dropped": 0,
  "hosts": [
    {"node_id": "st1", "role": "storage", "live": true, "up": true, "holds_slot": false,
     "silent_secs": 3, "platform": "linux-x86_64", "version": "2.1", "health": "healthy",
     "last_update": null, "updating": null},
    {"node_id": "dm1", "role": "daemon", "live": true, "up": false, "holds_slot": true,
     "silent_secs": 42, "platform": "linux-x86_64", "version": "2.0", "health": "unhealthy",
     "last_update": {"target": "2.0", "outcome": "failed"}, "updating": "2.1"}
  ],
  "per_service": {
    "kbf": {"desired": {"version": "2.1"}, "roles": [], "up": 1, "slots_held": 1,
      "hosts": [
        {"node_id": "st1", "role": "storage", "live": true, "up": true, "holds_slot": false,
         "silent_secs": 3, "platform": "linux-x86_64", "version": "2.1", "health": "healthy",
         "last_update": null, "updating": null, "hold": "up_to_date"},
        {"node_id": "dm1", "role": "daemon", "live": true, "up": false, "holds_slot": true,
         "silent_secs": 42, "platform": "linux-x86_64", "version": "2.0", "health": "unhealthy",
         "last_update": {"target": "2.0", "outcome": "failed"}, "updating": "2.1",
         "hold": "updating"}
      ]},
    "other": {"desired": {"version": "7"}, "roles": [], "up": 1, "slots_held": 0,
      "hosts": [
        {"node_id": "st1", "role": "storage", "live": true, "up": true, "holds_slot": false,
         "silent_secs": 3, "platform": "linux-x86_64", "version": "7", "health": "healthy",
         "last_update": null, "updating": null, "hold": "up_to_date"}
      ]}
  },
  "ops": {
    "schema": 1,
    "excluded": [{"service": "kbf", "node_id": "dm1", "reason": "disk swap"}],
    "services": {"kbf": {"halt": {"node_id": "dm1", "check": "smoke", "target": "2.1"}}}
  },
  "commit": "0123456789ab"
}"#;

/// A fake coordinator: its address, the CA to trust it by, and the request heads it saw.
struct Fake {
    addr: SocketAddr,
    ca_pem: String,
    heads: Arc<Mutex<Vec<String>>>,
}

/// Serve HTTPS on `127.0.0.1:0` as `127.0.0.1`, answering each request with the bytes
/// `answer` makes of its head.
fn fake(answer: impl Fn(&str) -> Vec<u8> + Send + 'static) -> Fake {
    let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).expect("CA params");
    ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = rcgen::CertifiedIssuer::self_signed(ca, rcgen::KeyPair::generate().expect("key"))
        .expect("CA");
    let key = rcgen::KeyPair::generate().expect("key");
    let cert = rcgen::CertificateParams::new(vec!["127.0.0.1".to_owned()])
        .expect("server params")
        .signed_by(&key, &ca)
        .expect("server certificate");
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            rustls::pki_types::PrivateKeyDer::try_from(key.serialize_der()).expect("key der"),
        )
        .expect("server config");
    let config = Arc::new(config);
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let addr = listener.local_addr().expect("addr");
    let heads = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&heads);
    std::thread::spawn(move || {
        for tcp in listener.incoming().flatten() {
            let _ = serve_one(&config, tcp, &answer, &seen);
        }
    });
    Fake {
        addr,
        ca_pem: ca.pem(),
        heads,
    }
}

fn serve_one(
    config: &Arc<rustls::ServerConfig>,
    tcp: TcpStream,
    answer: &dyn Fn(&str) -> Vec<u8>,
    seen: &Mutex<Vec<String>>,
) -> std::io::Result<()> {
    let conn = rustls::ServerConnection::new(Arc::clone(config)).map_err(std::io::Error::other)?;
    let mut tls = rustls::StreamOwned::new(conn, tcp);
    let mut head = Vec::new();
    let mut byte = [0u8];
    while !head.ends_with(b"\r\n\r\n") {
        tls.read_exact(&mut byte)?;
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    seen.lock().expect("lock").push(head.clone());
    tls.write_all(&answer(&head))?;
    tls.conn.send_close_notify();
    tls.flush()
}

fn http(status: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

/// A coordinator that accepts `TOKEN` as the bearer of `GET /v1/status` and refuses any other.
fn coordinator() -> Fake {
    fake(|head| {
        let line = head.lines().next().unwrap_or_default();
        let bearer = format!("authorization: Bearer {TOKEN}");
        let allowed = head
            .lines()
            .any(|l| l.eq_ignore_ascii_case(&bearer) || l == bearer);
        if line != "GET /v1/status HTTP/1.1" {
            http("404 Not Found", r#"{"error":"not_found"}"#)
        } else if allowed {
            http("200 OK", STATUS)
        } else {
            http("401 Unauthorized", r#"{"error":"unauthorized"}"#)
        }
    })
}

/// A token file the verb may read: mode 0600, holding `TOKEN`.
fn token_file(dir: &Path, mode: u32) -> PathBuf {
    let path = dir.join("operator.token");
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(&path)
        .expect("create the token file");
    writeln!(file, "{TOKEN}").expect("write the token");
    // The umask may have taken bits off the mode it was created with.
    std::fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(mode))
        .expect("set the mode");
    path
}

/// What one run of the binary did.
struct Ran {
    code: Option<i32>,
    stdout: String,
    stderr: String,
    argv: Vec<String>,
}

impl Ran {
    /// Neither output holds the token, nor does the command line or its environment.
    fn assert_no_token(&self) {
        assert!(!self.stdout.contains(TOKEN), "stdout: {}", self.stdout);
        assert!(!self.stderr.contains(TOKEN), "stderr: {}", self.stderr);
        assert!(
            self.argv.iter().all(|a| !a.contains(TOKEN)),
            "argv: {:?}",
            self.argv
        );
    }
}

/// Run `kbf-deploy host-status` with `args`, in an environment that holds nothing.
fn run(args: &[&str]) -> Ran {
    let argv: Vec<String> = ["host-status"]
        .into_iter()
        .chain(args.iter().copied())
        .map(str::to_owned)
        .collect();
    let out = Command::new(env!("CARGO_BIN_EXE_kbf-deploy"))
        .args(&argv)
        .env_clear()
        .output()
        .expect("the binary runs");
    Ran {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        argv,
    }
}

/// `host-status` against `fake`, reading the token from `token`, for `node`, plus `more`.
fn against(fake: &Fake, token: &Path, node: &str, more: &[&str]) -> Ran {
    let coordinator = format!("https://127.0.0.1:{}", fake.addr.port());
    let token = token.to_str().expect("a UTF-8 path");
    let mut args = vec![
        "--coordinator",
        &coordinator,
        "--operator-token-file",
        token,
        "--node",
        node,
        "--ca-pem",
        &fake.ca_pem,
    ];
    args.extend_from_slice(more);
    run(&args)
}

fn scratch(name: &str) -> TestDir {
    TestDir::under(
        std::env::temp_dir(),
        &format!("kbf-deploy-host-status-{name}"),
    )
}

/// Behaviour: the verb prints the row of the node asked for, from the service's section of
/// the status, with the hold, the halt and the exclusion that concern it, and no other
/// host's row. It sends the token as the bearer of `GET /v1/status`.
#[test]
fn a_found_host_prints_its_row() {
    let dir = scratch("found");
    let token = token_file(dir.path(), 0o600);
    let fake = coordinator();
    let ran = against(&fake, &token, "dm1", &[]);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    for line in [
        "node_id: dm1",
        "service: kbf",
        "role: daemon",
        "version: 2.0",
        "health: unhealthy",
        "live: true",
        "up: false",
        "updating: 2.1",
        r#"last_update: {"outcome":"failed","target":"2.0"}"#,
        "last_seen: 42s ago",
        "holds_slot: true",
        "hold: updating",
        "excluded: disk swap",
        r#"halt: {"check":"smoke","node_id":"dm1","target":"2.1"}"#,
    ] {
        assert!(
            ran.stdout.lines().any(|l| l == line),
            "no line {line:?} in:\n{}",
            ran.stdout
        );
    }
    assert!(!ran.stdout.contains("st1"), "{}", ran.stdout);
    ran.assert_no_token();
    let heads = fake.heads.lock().expect("lock");
    assert_eq!(heads.len(), 1);
    assert!(
        heads[0].starts_with("GET /v1/status HTTP/1.1\r\n"),
        "{}",
        heads[0]
    );
    assert!(heads[0].contains(&format!("Bearer {TOKEN}\r\n")));
}

/// Behaviour: `--service` reads the row of that service, not kbf's.
#[test]
fn a_service_names_the_section_the_row_is_read_from() {
    let dir = scratch("service");
    let token = token_file(dir.path(), 0o600);
    let fake = coordinator();
    let ran = against(&fake, &token, "st1", &["--service", "other"]);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    assert!(
        ran.stdout.lines().any(|l| l == "service: other"),
        "{}",
        ran.stdout
    );
    assert!(
        ran.stdout.lines().any(|l| l == "version: 7"),
        "{}",
        ran.stdout
    );
    // dm1 runs kbf only.
    let missing = against(&fake, &token, "dm1", &["--service", "other"]);
    assert_eq!(missing.code, Some(1), "{}", missing.stderr);
    let unknown = against(&fake, &token, "st1", &["--service", "nope"]);
    assert_eq!(unknown.code, Some(1), "{}", unknown.stderr);
    assert!(unknown.stderr.contains("nope"), "{}", unknown.stderr);
}

/// Behaviour: a node the status does not list exits 1 and says so, and prints no row.
#[test]
fn a_missing_node_exits_one() {
    let dir = scratch("missing");
    let token = token_file(dir.path(), 0o600);
    let fake = coordinator();
    let ran = against(&fake, &token, "nope9", &[]);
    assert_eq!(ran.code, Some(1), "{}", ran.stderr);
    assert!(ran.stderr.contains("nope9"), "{}", ran.stderr);
    assert!(ran.stderr.contains("not in the status"), "{}", ran.stderr);
    assert_eq!(ran.stdout, "");
    ran.assert_no_token();
}

/// Behaviour: a coordinator that refuses the token (401) exits 1 with a text that says the
/// token was refused, and neither output holds the token.
#[test]
fn a_refused_token_says_refused_and_not_the_token() {
    let dir = scratch("refused");
    let token = token_file(dir.path(), 0o600);
    let fake = fake(|_| http("401 Unauthorized", r#"{"error":"unauthorized"}"#));
    let ran = against(&fake, &token, "dm1", &[]);
    assert_eq!(ran.code, Some(1));
    assert!(
        ran.stderr.contains("operator token was refused"),
        "{}",
        ran.stderr
    );
    assert_eq!(ran.stdout, "");
    ran.assert_no_token();
}

/// Behaviour: a token file others can read is refused with exit 2 before any connection is
/// made, and the refusal does not hold the token.
#[test]
fn a_token_file_others_can_read_is_refused() {
    let dir = scratch("mode");
    let fake = coordinator();
    for mode in [0o640, 0o604, 0o660, 0o700] {
        let sub = scratch(&format!("mode-{mode:o}"));
        let token = token_file(sub.path(), mode);
        let ran = against(&fake, &token, "dm1", &[]);
        assert_eq!(ran.code, Some(2), "{mode:o}: {}", ran.stderr);
        assert!(ran.stderr.contains("0600"), "{mode:o}: {}", ran.stderr);
        assert_eq!(ran.stdout, "");
        ran.assert_no_token();
    }
    // A file that is not there is a bad token file too.
    let gone = dir.path().join("absent.token");
    let ran = against(&fake, &gone, "dm1", &[]);
    assert_eq!(ran.code, Some(2), "{}", ran.stderr);
    assert!(fake.heads.lock().expect("lock").is_empty());
}

/// Behaviour: the token reaches the coordinator in the Authorization header and nowhere
/// else: not on the command line the verb was started with, not in its environment (which
/// the test empties), and not in anything it prints, on success or on each way to fail.
#[test]
fn the_token_is_only_ever_the_authorization_header() {
    let dir = scratch("secret");
    let token = token_file(dir.path(), 0o600);
    let good = coordinator();
    let found = against(&good, &token, "dm1", &[]);
    assert_eq!(found.code, Some(0), "{}", found.stderr);
    found.assert_no_token();
    let as_json = against(&good, &token, "dm1", &["--json"]);
    as_json.assert_no_token();
    assert_eq!(good.heads.lock().expect("lock").len(), 2);
    for head in good.heads.lock().expect("lock").iter() {
        assert!(!head.lines().next().unwrap_or_default().contains(TOKEN));
    }
    // A server the CA does not vouch for: a TLS failure, not a connection one.
    let other = coordinator();
    let coordinator_url = format!("https://127.0.0.1:{}", other.addr.port());
    let path = token.to_str().expect("a UTF-8 path");
    let untrusted = run(&[
        "--coordinator",
        &coordinator_url,
        "--operator-token-file",
        path,
        "--node",
        "dm1",
        "--ca-pem",
        &good.ca_pem,
    ]);
    assert_eq!(untrusted.code, Some(1), "{}", untrusted.stderr);
    assert!(untrusted.stderr.contains("TLS"), "{}", untrusted.stderr);
    untrusted.assert_no_token();
    assert!(other.heads.lock().expect("lock").is_empty());
    // Nothing listens: a connection failure.
    let dead = format!("127.0.0.1:{}", kbf_testkit::port::dead_addr().port());
    let refused = run(&[
        "--coordinator",
        &dead,
        "--operator-token-file",
        path,
        "--node",
        "dm1",
        "--ca-pem",
        &good.ca_pem,
    ]);
    assert_eq!(refused.code, Some(1), "{}", refused.stderr);
    assert!(refused.stderr.contains("connect"), "{}", refused.stderr);
    refused.assert_no_token();
    // A coordinator that answers something else: an HTTP failure, with its status.
    let broken = fake(|_| http("500 Internal Server Error", "{}"));
    let ran = against(&broken, &token, "dm1", &[]);
    assert_eq!(ran.code, Some(1), "{}", ran.stderr);
    assert!(ran.stderr.contains("500"), "{}", ran.stderr);
    ran.assert_no_token();
}

/// Behaviour: `--json` prints one JSON object, the host's row as the status has it, on one
/// line, and nothing else.
#[test]
fn json_is_exactly_one_object() {
    let dir = scratch("json");
    let token = token_file(dir.path(), 0o600);
    let fake = coordinator();
    let ran = against(&fake, &token, "dm1", &["--json"]);
    assert_eq!(ran.code, Some(0), "{}", ran.stderr);
    assert_eq!(ran.stderr, "");
    assert_eq!(ran.stdout.lines().count(), 1, "{}", ran.stdout);
    let values: Vec<serde_json::Value> = serde_json::Deserializer::from_str(&ran.stdout)
        .into_iter()
        .collect::<Result<_, _>>()
        .expect("JSON");
    assert_eq!(values.len(), 1);
    let want: serde_json::Value = serde_json::from_str(STATUS).expect("fixture");
    assert_eq!(values[0], want["per_service"]["kbf"]["hosts"][1]);
}

/// Behaviour: bad arguments exit 2: a coordinator that is not https, a path on it, a node
/// that is empty, a flag that is missing, and a CA that holds no certificate.
#[test]
fn bad_arguments_exit_two() {
    let dir = scratch("args");
    let token = token_file(dir.path(), 0o600);
    let token = token.to_str().expect("a UTF-8 path");
    let base = |coordinator: &'static str, node: &'static str| {
        run(&[
            "--coordinator",
            coordinator,
            "--operator-token-file",
            token,
            "--node",
            node,
        ])
    };
    for ran in [
        base("http://127.0.0.1:1", "dm1"),
        base("https://127.0.0.1:1/v1/status", "dm1"),
        base("https://127.0.0.1:1", ""),
        run(&["--coordinator", "127.0.0.1:1", "--node", "dm1"]),
        run(&[
            "--coordinator",
            "127.0.0.1:1",
            "--operator-token-file",
            token,
            "--node",
            "dm1",
            "--ca-pem",
            "not a certificate",
        ]),
    ] {
        assert_eq!(ran.code, Some(2), "{}", ran.stderr);
        assert_eq!(ran.stdout, "");
    }
}
