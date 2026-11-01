//! The heartbeat over mutual TLS, against a coordinator played by a TLS server in the test: who
//! it talks to, what it sends, how it reads the answer, and what it does with the files it was
//! given.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use kbf_testkit::dir::TestDir;
use kbf_types::{HostRole, ReleasePlatform};
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::WebPkiClientVerifier;
use rustls::{RootCertStore, ServerConfig, ServerConnection, StreamOwned};

use super::coordinator::{
    Coordinator, CoordinatorError, Endpoint, HttpsCoordinator, REPLY_MAX, TlsFiles, client_config,
};
use super::pki::{Leaf, Pki, write_files};
use crate::identity::identity_of;
use crate::version::Version;
use crate::wire::{Health, Heartbeat, Reply, ServiceBeat, ServiceReply};

fn beat() -> Heartbeat {
    Heartbeat {
        platform: ReleasePlatform::LinuxX86_64,
        version: Version::parse("0.1.5+gaaaaaaaaaaaa-2").unwrap(),
        health: Health::Healthy,
        last_update: None,
        updating: None,
        services: None,
        agent_features: Vec::new(),
        step: None,
        commit: None,
        step_secs: Default::default(),
        promotion_pending: false,
        deferred: None,
    }
}

/// What the server does with a request.
#[derive(Clone, Debug)]
pub(super) enum Plan {
    /// Write these bytes and close properly.
    Say(Vec<u8>),
    /// Write these bytes and drop the connection with no TLS close.
    SayAndCut(Vec<u8>),
    /// Read the request and say nothing for a while.
    Silent,
    /// Read the request, then send the start of an answer a few bytes at a time, for longer
    /// than the client allows for the whole of it.
    Trickle,
}

/// What the server saw of one connection.
#[derive(Debug, Default)]
pub(super) struct Seen {
    /// The request, head and body, as bytes; none when the handshake failed.
    pub(super) request: Vec<u8>,
    /// The client's certificate, DER.
    pub(super) client: Option<Vec<u8>>,
    pub(super) handshake_failed: bool,
    /// The client hung up while the server was still writing its answer a byte at a time.
    pub(super) client_left: bool,
}

pub(super) fn server_config(pki: &Pki, leaf: &Leaf) -> Arc<ServerConfig> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(pki.ca.root_der().to_vec()))
        .unwrap();
    let verifier = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider.clone())
        .build()
        .unwrap();
    let mut config = ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_client_cert_verifier(verifier)
        .with_single_cert(
            vec![CertificateDer::from(leaf.der.clone())],
            PrivateKeyDer::from_pem_slice(leaf.key_pem().as_bytes()).unwrap(),
        )
        .unwrap();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Arc::new(config)
}

/// Serve connections: `answers` gives the bytes to write back for each request in turn (none
/// to say nothing and wait), and the server ends after the last. Each connection's `Seen`.
pub(super) fn serve(
    config: Arc<ServerConfig>,
    answers: Vec<Plan>,
) -> (SocketAddr, JoinHandle<Vec<Seen>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = std::thread::spawn(move || {
        let mut all = Vec::new();
        for answer in answers {
            let (tcp, _) = listener.accept().unwrap();
            all.push(one(&config, tcp, answer));
        }
        all
    });
    (addr, handle)
}

fn one(config: &Arc<ServerConfig>, tcp: TcpStream, answer: Plan) -> Seen {
    tcp.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut tls = StreamOwned::new(ServerConnection::new(Arc::clone(config)).unwrap(), tcp);
    let mut seen = Seen::default();
    while tls.conn.is_handshaking() {
        if tls.conn.complete_io(&mut tls.sock).is_err() {
            seen.handshake_failed = true;
            return seen;
        }
    }
    seen.client = tls
        .conn
        .peer_certificates()
        .and_then(|c| c.first())
        .map(|c| c.as_ref().to_vec());
    let mut chunk = [0_u8; 4096];
    loop {
        let n = match tls.read(&mut chunk) {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        seen.request.extend_from_slice(&chunk[..n]);
        if let Some(end) = seen.request.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&seen.request[..end]).to_ascii_lowercase();
            let length = head
                .lines()
                .find_map(|l| l.strip_prefix("content-length: "))
                .and_then(|v| v.trim().parse::<usize>().ok())
                .unwrap_or(0);
            if seen.request.len() >= end + 4 + length {
                break;
            }
        }
    }
    match answer {
        Plan::Say(bytes) => {
            let _ = tls.write_all(&bytes);
            tls.conn.send_close_notify();
            let _ = tls.flush();
        }
        Plan::SayAndCut(bytes) => {
            let _ = tls.write_all(&bytes);
            let _ = tls.flush();
        }
        // Hold the connection until the client has hung up on it; the 5 s read timeout of
        // this connection is only a hang guard, and `client_left` says it was not what ended
        // the wait.
        Plan::Silent => {
            seen.client_left = loop {
                match tls.read(&mut chunk) {
                    Ok(0) => break true,
                    Ok(_) => {}
                    Err(e) => {
                        break !matches!(
                            e.kind(),
                            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                        );
                    }
                }
            };
        }
        Plan::Trickle => {
            for byte in b"HTTP/1.1 200 OK\r\nX-Slow: yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy".iter()
            {
                if tls.write_all(&[*byte]).and_then(|()| tls.flush()).is_err() {
                    seen.client_left = true;
                    break;
                }
                std::thread::sleep(Duration::from_millis(60));
            }
        }
    }
    seen
}

pub(super) fn http(status: &str, headers: &str, body: &str) -> Plan {
    Plan::Say(
        format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{headers}\
             Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .into_bytes(),
    )
}

pub(super) fn ok(body: &str) -> Plan {
    http("200 OK", "", body)
}

struct Rig {
    pki: Pki,
    dir: TestDir,
    files: TlsFiles,
    updater: Leaf,
}

impl Rig {
    fn new(name: &str) -> Self {
        let pki = Pki::new();
        let dir = TestDir::under(std::env::temp_dir(), name);
        let updater = pki.updater("n1", HostRole::DAEMON);
        let files = write_files(&dir, &pki, &updater);
        Self {
            pki,
            dir,
            files,
            updater,
        }
    }

    fn client(&self, addr: SocketAddr) -> HttpsCoordinator {
        HttpsCoordinator::new(
            Endpoint::parse(&format!("localhost:{}", addr.port())).unwrap(),
            self.files.clone(),
        )
        .with_timeout(Duration::from_secs(5))
    }

    /// A coordinator that answers `answers`, with its certificate from this CA.
    fn coordinator(&self, answers: Vec<Plan>) -> (SocketAddr, JoinHandle<Vec<Seen>>) {
        serve(server_config(&self.pki, &self.pki.coordinator()), answers)
    }
}

/// A heartbeat is one POST of the heartbeat's JSON to the heartbeat path, with the length of the
/// body and `Connection: close`; the updater's certificate is what the coordinator sees of who
/// is calling (its node and its host role), and the body says neither. The reply is read.
#[test]
fn a_heartbeat_is_a_post_of_the_heartbeat_over_mutual_tls() {
    let rig = Rig::new("co-ok");
    let (addr, server) = rig.coordinator(vec![ok(
        r#"{"reply":"update","version":"0.1.7+gbbbbbbbbbbbb-3"}"#,
    )]);
    let reply = rig.client(addr).heartbeat(&beat()).unwrap();
    assert_eq!(
        reply,
        Reply::Update {
            version: Version::parse("0.1.7+gbbbbbbbbbbbb-3").unwrap()
        }
    );
    let seen = server.join().unwrap().remove(0);
    let text = String::from_utf8(seen.request).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    let lines: Vec<&str> = head.lines().collect();
    assert_eq!(lines[0], "POST /v1/heartbeat HTTP/1.1");
    assert!(lines.contains(&format!("Host: localhost:{}", addr.port()).as_str()));
    assert!(lines.contains(&"Content-Type: application/json"));
    assert!(lines.contains(&"Connection: close"));
    assert!(lines.contains(&format!("Content-Length: {}", body.len()).as_str()));
    assert_eq!(serde_json::from_str::<Heartbeat>(body).unwrap(), beat());
    for forbidden in ["n1", "daemon", "node", "role"] {
        assert!(!body.contains(forbidden), "{forbidden} in {body}");
    }
    let who = identity_of(&seen.client.expect("the client presented a certificate")).unwrap();
    assert_eq!(
        (who.node_id.to_string(), who.role),
        ("n1".to_owned(), HostRole::DAEMON)
    );
}

/// "Nothing" is a reply.
#[test]
fn nothing_is_a_reply() {
    let rig = Rig::new("co-nothing");
    let (addr, server) = rig.coordinator(vec![ok(r#"{"reply":"nothing"}"#)]);
    assert_eq!(rig.client(addr).heartbeat(&beat()), Ok(Reply::Nothing));
    server.join().unwrap();
}

/// The files are read for every heartbeat: a certificate renewed between two is the one the
/// second presents, with no restart.
#[test]
fn a_renewed_certificate_is_used_from_the_next_heartbeat() {
    let rig = Rig::new("co-renew");
    let (addr, server) = rig.coordinator(vec![
        ok(r#"{"reply":"nothing"}"#),
        ok(r#"{"reply":"nothing"}"#),
    ]);
    let client = rig.client(addr);
    client.heartbeat(&beat()).unwrap();
    let renewed = rig.pki.updater("n1", HostRole::DAEMON);
    write_files(&rig.dir, &rig.pki, &renewed);
    client.heartbeat(&beat()).unwrap();
    let seen = server.join().unwrap();
    assert_eq!(seen[0].client.as_deref(), Some(rig.updater.der.as_slice()));
    assert_eq!(seen[1].client.as_deref(), Some(renewed.der.as_slice()));
}

/// What answers must be the coordinator: a certificate of another role of the farm's own (a
/// storage node's, for the right name), one for another name, and one from another CA are all
/// refused, and in none of them is a heartbeat written.
#[test]
fn only_the_coordinator_is_talked_to() {
    let rig = Rig::new("co-who");
    // A storage node's certificate for the coordinator's name.
    let storage = rig.pki.storage_node("localhost");
    let (addr, server) = serve(
        server_config(&rig.pki, &storage),
        vec![ok(r#"{"reply":"nothing"}"#)],
    );
    let err = rig.client(addr).heartbeat(&beat()).unwrap_err();
    assert!(
        matches!(&err, CoordinatorError::Untrusted(why) if why.contains("role is not `coordinator`")),
        "{err:?}"
    );
    assert!(
        server.join().unwrap()[0].request.is_empty(),
        "nothing was sent to it"
    );

    // The coordinator's certificate, but for another name than the one the client was told.
    let (addr, server_name) = serve(
        server_config(&rig.pki, &rig.pki.coordinator_for(&["coordinator.example"])),
        vec![ok(r#"{"reply":"nothing"}"#)],
    );
    let err = rig.client(addr).heartbeat(&beat()).unwrap_err();
    assert!(matches!(err, CoordinatorError::Untrusted(_)), "{err:?}");
    assert!(server_name.join().unwrap()[0].request.is_empty());

    // A server certificate from another farm's CA.
    let strangers = Pki::new();
    let (addr, server) = serve(
        server_config(&strangers, &strangers.coordinator()),
        vec![ok(r#"{"reply":"nothing"}"#)],
    );
    let err = rig.client(addr).heartbeat(&beat()).unwrap_err();
    assert!(matches!(err, CoordinatorError::Untrusted(_)), "{err:?}");
    assert!(server.join().unwrap()[0].request.is_empty());
}

/// A status other than 200 is that status, whatever the body; a body that is not exactly a reply
/// is malformed: an unknown field, a version on `nothing`, text, a body of unknown length, a
/// body with no length, one that is not as long as it says, one over the limit, and something
/// that is not HTTP.
#[test]
fn only_a_200_with_exactly_a_reply_is_a_reply() {
    let rig = Rig::new("co-answers");
    let big = format!(r#"{{"reply":"nothing","pad":"{}"}}"#, "x".repeat(REPLY_MAX));
    let cases: Vec<(Plan, &str)> = vec![
        (http("401 Unauthorized", "", r#"{"error":"unauthorized"}"#), "status 401"),
        (http("503 Service Unavailable", "", ""), "status 503"),
        (ok(r#"{"reply":"update","version":"1.0","url":"https://example.invalid/k"}"#), "malformed"),
        (ok(r#"{"reply":"nothing","version":"1.0"}"#), "malformed"),
        (ok("not json"), "malformed"),
        (ok(""), "malformed"),
        (
            Plan::Say(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n0\r\n\r\n".to_vec()),
            "malformed",
        ),
        (
            Plan::Say(b"HTTP/1.1 200 OK\r\nConnection: close\r\n\r\n{\"reply\":\"nothing\"}".to_vec()),
            "malformed",
        ),
        // Framed two ways at once: which one is the body is not for the client to guess.
        (
            Plan::Say(
                b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nContent-Length: 19\r\n\
                  Connection: close\r\n\r\n{\"reply\":\"nothing\"}"
                    .to_vec(),
            ),
            "malformed",
        ),
        (
            Plan::Say(b"HTTP/1.1 200 OK\r\nContent-Length: 40\r\nConnection: close\r\n\r\n{\"reply\":\"nothing\"}".to_vec()),
            "malformed",
        ),
        (ok(&big), "malformed"),
        (Plan::Say(b"this is not http\r\n\r\n".to_vec()), "malformed"),
        (Plan::Say(b"HTTP/1.1 200 OK\r\nContent-Le".to_vec()), "malformed"),
        (ok(&format!(r#"{{"reply":"nothing","pad":"{}"}}"#, "x".repeat(30_000))), "malformed"),
        (Plan::Say(Vec::new()), "unreachable"),
    ];
    for (answer, expect) in cases {
        let (addr, server) = rig.coordinator(vec![answer.clone()]);
        let err = rig.client(addr).heartbeat(&beat()).unwrap_err();
        server.join().unwrap();
        let ok = match expect {
            "status 401" => err == CoordinatorError::Status(401),
            "status 503" => err == CoordinatorError::Status(503),
            "malformed" => matches!(err, CoordinatorError::Malformed(_)),
            _ => matches!(err, CoordinatorError::Unreachable(_)),
        };
        assert!(ok, "{answer:?} -> {err:?}");
    }
}

/// A coordinator that is not there, or that takes the request and never answers, is
/// unreachable; the wait is the timeout's.
#[test]
fn a_coordinator_that_is_not_there_or_does_not_answer_is_unreachable() {
    let rig = Rig::new("co-down");
    let gone = {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    let err = rig.client(gone).heartbeat(&beat()).unwrap_err();
    assert!(matches!(err, CoordinatorError::Unreachable(_)), "{err:?}");

    let (addr, server) = rig.coordinator(vec![Plan::Silent]);
    let slow = rig.client(addr).with_timeout(Duration::from_millis(100));
    let err = slow.heartbeat(&beat()).unwrap_err();
    assert!(matches!(err, CoordinatorError::Unreachable(_)), "{err:?}");
    // It was the client that gave up on the silent server, not the server that hung up.
    assert!(server.join().unwrap()[0].client_left);

    // A name that is no host's.
    let nowhere = HttpsCoordinator::new(
        Endpoint::parse("no-such-host.invalid:9").unwrap(),
        rig.files.clone(),
    );
    assert!(matches!(
        nowhere.heartbeat(&beat()),
        Err(CoordinatorError::Unreachable(_))
    ));
}

/// A server that wants a client certificate this host does not have makes the handshake fail:
/// the coordinator accepts only the farm CA's, so a certificate from another CA is no way in.
#[test]
fn a_certificate_from_another_ca_is_not_accepted_by_the_coordinator() {
    let rig = Rig::new("co-badclient");
    let strangers = Pki::new();
    let mine = strangers.updater("n1", HostRole::DAEMON);
    let files = TlsFiles {
        cert: rig.dir.join("stranger.pem"),
        key: rig.dir.join("stranger.key"),
        ca: rig.files.ca.clone(),
    };
    std::fs::write(&files.cert, mine.cert_pem()).unwrap();
    std::fs::write(&files.key, mine.key_pem()).unwrap();
    std::fs::set_permissions(&files.key, std::fs::Permissions::from_mode(0o600)).unwrap();
    let (addr, server) = rig.coordinator(vec![ok(r#"{"reply":"nothing"}"#)]);
    let client = HttpsCoordinator::new(
        Endpoint::parse(&format!("localhost:{}", addr.port())).unwrap(),
        files,
    )
    .with_timeout(Duration::from_secs(5));
    let err = client.heartbeat(&beat()).unwrap_err();
    assert!(matches!(err, CoordinatorError::Unreachable(_)), "{err:?}");
    let seen = server.join().unwrap();
    assert!(seen[0].request.is_empty());
}

/// The files must make a client configuration: all three there and PEM of their kind, a key
/// that is the certificate's, and a CA file with a certificate in it.
#[test]
fn files_that_do_not_make_a_configuration_are_named() {
    let rig = Rig::new("co-files");
    assert!(client_config(&rig.files).is_ok());
    let files_err = |mutate: &dyn Fn(&TlsFiles)| {
        let rig = Rig::new("co-files-bad");
        mutate(&rig.files);
        match client_config(&rig.files) {
            Err(CoordinatorError::Files(why)) => why,
            other => panic!("{other:?}"),
        }
    };
    type Ruin<'a> = &'a dyn Fn(&TlsFiles);
    let cases: [(&str, Ruin, &str); 9] = [
        (
            "no certificate file",
            &|f| std::fs::remove_file(&f.cert).unwrap(),
            "updater.pem",
        ),
        (
            "an empty certificate file",
            &|f| std::fs::write(&f.cert, "").unwrap(),
            "holds no certificate",
        ),
        (
            "a certificate that is not PEM",
            &|f| {
                std::fs::write(
                    &f.cert,
                    "-----BEGIN CERTIFICATE-----\n!!\n-----END CERTIFICATE-----\n",
                )
                .unwrap()
            },
            "updater.pem",
        ),
        (
            "no key file",
            &|f| std::fs::remove_file(&f.key).unwrap(),
            "updater.key",
        ),
        (
            "a key that is not PEM",
            &|f| std::fs::write(&f.key, "nope").unwrap(),
            "updater.key",
        ),
        (
            "no CA file",
            &|f| std::fs::remove_file(&f.ca).unwrap(),
            "ca.pem",
        ),
        (
            "an empty CA file",
            &|f| std::fs::write(&f.ca, "").unwrap(),
            "holds no certificate",
        ),
        (
            "a CA that is not PEM",
            &|f| {
                std::fs::write(
                    &f.ca,
                    "-----BEGIN CERTIFICATE-----\n!!\n-----END CERTIFICATE-----\n",
                )
                .unwrap()
            },
            "ca.pem",
        ),
        (
            "a CA that is not a certificate",
            &|f| {
                std::fs::write(
                    &f.ca,
                    "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n",
                )
                .unwrap()
            },
            "ca.pem",
        ),
    ];
    for (name, mutate, why) in cases {
        let got = files_err(mutate);
        assert!(got.contains(why), "{name}: {got}");
    }
    // A key that is not the certificate's.
    let other = Pki::new().updater("n1", HostRole::DAEMON);
    std::fs::write(&rig.files.key, other.key_pem()).unwrap();
    assert!(
        matches!(client_config(&rig.files), Err(CoordinatorError::Files(why)) if why.contains("not a TLS configuration"))
    );
    // The errors say it in words.
    let err = HttpsCoordinator::new(Endpoint::parse("localhost:1").unwrap(), rig.files.clone())
        .heartbeat(&beat())
        .unwrap_err();
    assert!(err.to_string().contains("TLS files"));
    for (error, text) in [
        (CoordinatorError::Unreachable("x".into()), "unreachable"),
        (
            CoordinatorError::Untrusted("x".into()),
            "not the coordinator",
        ),
        (CoordinatorError::Status(500), "HTTP 500"),
        (CoordinatorError::Malformed("x".into()), "not a reply"),
    ] {
        assert!(error.to_string().contains(text), "{error}");
    }
}

/// `--coordinator` is a DNS name and a port: not an address, no scheme, no path, nothing a
/// certificate cannot be for.
#[test]
fn an_endpoint_is_a_dns_name_and_a_port() {
    let parsed = Endpoint::parse("Coordinator.Farm.Example:8443").unwrap();
    assert_eq!(
        (parsed.host.as_str(), parsed.port),
        ("coordinator.farm.example", 8443)
    );
    for bad in [
        "",
        "coordinator",
        "coordinator:",
        "coordinator:0",
        "coordinator:65536",
        "coordinator:http",
        ":443",
        "203.0.113.5:443",
        "https://coordinator:443",
        "coordinator/path:443",
        "-bad.example:443",
        "bad-.example:443",
        "a..b:443",
        "[::1]:443",
        &format!("{}:443", "a".repeat(64)),
        &format!("{}:443", ["abcdefghij"; 26].join(".")),
    ] {
        assert!(Endpoint::parse(bad).is_err(), "{bad:?}");
    }
}

/// A server that closes the connection without the TLS close, after a whole reply, is no worse
/// than one that closes it properly: the length the reply gives decides whether it is whole.
#[test]
fn a_reply_is_whole_by_its_length_not_by_a_proper_close() {
    let rig = Rig::new("co-cut");
    let body = r#"{"reply":"nothing"}"#;
    let whole = format!(
        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let (addr, server) = rig.coordinator(vec![Plan::SayAndCut(whole.into_bytes())]);
    assert_eq!(rig.client(addr).heartbeat(&beat()), Ok(Reply::Nothing));
    server.join().unwrap();
    let half = "HTTP/1.1 200 OK\r\nContent-Length: 19\r\nConnection: close\r\n\r\n{\"reply\"";
    let (addr, server) = rig.coordinator(vec![Plan::SayAndCut(half.as_bytes().to_vec())]);
    let err = rig.client(addr).heartbeat(&beat()).unwrap_err();
    assert!(matches!(err, CoordinatorError::Malformed(_)), "{err:?}");
    server.join().unwrap();
}

/// A server that will not speak HTTP/1.1 (it offers only another protocol) cannot be talked to:
/// the handshake fails for a reason that is not its certificate, and the coordinator is
/// unreachable rather than untrusted.
#[test]
fn a_server_that_offers_another_protocol_is_unreachable() {
    let rig = Rig::new("co-alpn");
    let mut config = (*server_config(&rig.pki, &rig.pki.coordinator())).clone();
    config.alpn_protocols = vec![b"h2".to_vec()];
    let (addr, server) = serve(Arc::new(config), vec![Plan::Say(Vec::new())]);
    let err = rig.client(addr).heartbeat(&beat()).unwrap_err();
    assert!(matches!(err, CoordinatorError::Unreachable(_)), "{err:?}");
    server.join().unwrap();
}

/// A server that answers a few bytes at a time is cut off at the time allowed for the whole
/// reply, not waited for as long as each read finds a byte.
#[test]
fn a_reply_that_trickles_is_cut_off_at_the_time_allowed_for_all_of_it() {
    let rig = Rig::new("co-trickle");
    let (addr, server) = rig.coordinator(vec![Plan::Trickle]);
    let client = rig.client(addr).with_timeout(Duration::from_millis(500));
    let err = client.heartbeat(&beat()).unwrap_err();
    assert!(matches!(err, CoordinatorError::Unreachable(_)), "{err:?}");
    // The client gave up while the reply was still coming in, rather than reading it to the
    // end: the server's write to a closed connection failed before its last byte.
    let seen = server.join().unwrap();
    assert!(seen[0].client_left, "the client waited for the whole reply");
}

/// The key file is held to the rule of the token file: a key that others can read, or that is
/// a symlink, is not used.
#[test]
fn a_client_key_that_others_can_read_is_not_used() {
    let rig = Rig::new("co-keymode");
    assert!(client_config(&rig.files).is_ok());
    for mode in [0o644, 0o660, 0o640, 0o700] {
        std::fs::set_permissions(&rig.files.key, std::fs::Permissions::from_mode(mode)).unwrap();
        let err = client_config(&rig.files).err().unwrap();
        assert!(
            matches!(&err, CoordinatorError::Files(why) if why.contains("--client-key") && why.contains("0600")),
            "{mode:o}: {err:?}"
        );
    }
    std::fs::set_permissions(&rig.files.key, std::fs::Permissions::from_mode(0o400)).unwrap();
    assert!(client_config(&rig.files).is_ok());
    let link = rig.dir.join("link.key");
    std::os::unix::fs::symlink(&rig.files.key, &link).unwrap();
    let files = TlsFiles {
        key: link,
        ..rig.files.clone()
    };
    assert!(client_config(&files).is_err());
}

/// A certificate swapped on disk for another node's, or for another role's, is not used by an
/// agent that started as the first: no heartbeat is sent as one node while the drain targets
/// another.
#[test]
fn a_certificate_that_now_names_another_node_is_not_used() {
    let rig = Rig::new("co-rebind");
    let (addr, server) = rig.coordinator(vec![ok(r#"{"reply":"nothing"}"#)]);
    let bound = rig
        .client(addr)
        .bound_to(identity_of(&rig.updater.der).unwrap());
    assert!(bound.heartbeat(&beat()).is_ok(), "the pair it started with");
    server.join().unwrap();

    for (node, role) in [("n2", HostRole::DAEMON), ("n1", HostRole::MAC_DAEMON)] {
        let other = rig.pki.updater(node, role.clone());
        std::fs::write(&rig.files.cert, other.cert_pem()).unwrap();
        std::fs::write(&rig.files.key, other.key_pem()).unwrap();
        // Nothing listens at this address: the refusal comes before any connection.
        let gone = TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let err = HttpsCoordinator::new(
            Endpoint::parse(&format!("localhost:{}", gone.port())).unwrap(),
            rig.files.clone(),
        )
        .bound_to(identity_of(&rig.updater.der).unwrap())
        .heartbeat(&beat())
        .unwrap_err();
        assert!(
            matches!(&err, CoordinatorError::Files(why) if why.contains("another node or role")),
            "{node} {role}: {err:?}"
        );
    }
}

/// A heartbeat that lists services is posted with them, and its answer is read as a service
/// reply: an update that names a service, one that does not, and nothing. The reader of today
/// refuses the named one, which is why only an agent that lists services asks for the other.
#[test]
fn a_heartbeat_with_services_reads_a_reply_that_names_one() {
    let rig = Rig::new("co-services");
    let mut with_services = beat();
    with_services.services = Some(vec![ServiceBeat {
        name: "db".to_owned(),
        version: Version::parse("0.1.5+gaaaaaaaaaaaa-2").unwrap(),
        health: Health::Healthy,
        last_update: None,
        updating: None,
        step: None,
        commit: None,
        step_secs: Default::default(),
        promotion_pending: false,
        deferred: None,
    }]);
    let named = r#"{"reply":"update","service":"db","version":"0.1.7+gbbbbbbbbbbbb-3"}"#;
    let (addr, server) = rig.coordinator(vec![
        ok(named),
        ok(r#"{"reply":"update","version":"0.1.7+gbbbbbbbbbbbb-3"}"#),
        ok(r#"{"reply":"nothing"}"#),
        ok(named),
    ]);
    let client = rig.client(addr);
    let target = Version::parse("0.1.7+gbbbbbbbbbbbb-3").unwrap();
    assert_eq!(
        client.heartbeat_services(&with_services).unwrap(),
        ServiceReply::Update {
            service: Some("db".to_owned()),
            version: target.clone()
        }
    );
    assert_eq!(
        client.heartbeat_services(&with_services).unwrap(),
        ServiceReply::Update {
            service: None,
            version: target
        }
    );
    assert_eq!(
        client.heartbeat_services(&with_services).unwrap(),
        ServiceReply::Nothing
    );
    assert!(matches!(
        client.heartbeat(&with_services),
        Err(CoordinatorError::Malformed(_))
    ));
    let seen = server.join().unwrap().remove(0);
    let text = String::from_utf8(seen.request).unwrap();
    let body = text.split_once("\r\n\r\n").unwrap().1;
    assert_eq!(
        serde_json::from_str::<Heartbeat>(body).unwrap(),
        with_services
    );
    assert!(body.contains(r#""services":[{"name":"db""#), "{body}");
}

/// A heartbeat that announces `agent_features` is posted with them, and its answer is read as
/// the service reply and the `features` beside it: a coordinator that advertises some, one that
/// advertises none, and one that predates them (no field). The reader of today refuses the
/// first two, which is why the coordinator sends `features` only to an agent that announced.
#[test]
fn a_heartbeat_that_announces_features_reads_the_features_of_the_answer() {
    let rig = Rig::new("co-features");
    let mut announcing = beat();
    announcing.agent_features = vec!["deferred-v1".to_owned(), "halt-v1".to_owned()];
    let (addr, server) = rig.coordinator(vec![
        ok(r#"{"reply":"update","version":"0.1.7+gbbbbbbbbbbbb-3","features":["deferred-v1"]}"#),
        ok(r#"{"reply":"nothing","features":[]}"#),
        ok(r#"{"reply":"nothing"}"#),
        ok(r#"{"reply":"nothing","features":["Bad"]}"#),
    ]);
    let client = rig.client(addr);
    let answer = client.heartbeat_answer(&announcing).unwrap();
    assert_eq!(
        answer.reply,
        ServiceReply::Update {
            service: None,
            version: Version::parse("0.1.7+gbbbbbbbbbbbb-3").unwrap()
        }
    );
    assert_eq!(answer.features, Some(vec!["deferred-v1".to_owned()]));
    assert_eq!(
        client.heartbeat_answer(&announcing).unwrap().features,
        Some(Vec::new())
    );
    assert_eq!(client.heartbeat_answer(&announcing).unwrap().features, None);
    assert!(matches!(
        client.heartbeat_answer(&announcing),
        Err(CoordinatorError::Malformed(_))
    ));
    let seen = server.join().unwrap().remove(0);
    let text = String::from_utf8(seen.request).unwrap();
    let body = text.split_once("\r\n\r\n").unwrap().1;
    assert_eq!(serde_json::from_str::<Heartbeat>(body).unwrap(), announcing);
    assert!(
        body.contains(r#""agent_features":["deferred-v1","halt-v1"]"#),
        "{body}"
    );
}
