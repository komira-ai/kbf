//! The fetch child against real servers: `kbf-installer fetch-child` is started by the
//! parent's `fetch_component` exactly as on a host (the real binary, an empty environment, the
//! confinement of `Confine::FETCH`; only the switch of user is left out, as these tests do not
//! run as root), and talks to a local HTTPS file server (the `url` source) or a local gRPC
//! `ReleaseService` over mutual TLS (the `frontend` source).
//!
//! Each test names the rule of contract section 5 it holds and the mutant that turns it red.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use kbf_installer::CommitId;
use kbf_installer::binary::Arch;
use kbf_installer::fetch::describe::{DescribeFault, Described};
use kbf_installer::fetch::pipe::{PipeFault, Received};
use kbf_installer::fetch::{
    ChildProgram, Component, FetchError, FrontendSource, Limits, Source, describe_release,
    fetch_component,
};
use kbf_installer::fixtures::{
    COMMIT, FakeRunner, elf, file, me, ran, tar_gz, trusted_host_on, version_json,
};
use kbf_installer::helper::fetcher::{FetchError as Refused, Fetcher, ReleaseLocator};
use kbf_installer::helper::fetcher_real::RealFetcher;
use kbf_installer::helper::leases::LeaseSource;
use kbf_installer::layout::Platform;
use kbf_installer::settings::Settings;
use kbf_installer::stage::{Policy, Supported, Timeouts};
use kbf_installer::testkit::host_root_on;
use kbf_types::ReleasePlatform;
use sha2::{Digest, Sha256};

const BIN: &str = env!("CARGO_BIN_EXE_kbf-installer");

fn sha(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// A body of `n` bytes that is not the same byte repeated.
fn body(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 7 % 251) as u8).collect()
}

// ------------------------------------------------------------------------------------ PKI

/// A CA, and certificates issued from it.
struct Pki {
    ca: rcgen::CertifiedIssuer<'static, rcgen::KeyPair>,
}

struct Leaf {
    cert: String,
    key: String,
}

impl Pki {
    fn new() -> Pki {
        let mut ca = rcgen::CertificateParams::new(Vec::<String>::new()).expect("CA params");
        ca.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        ca.distinguished_name
            .push(rcgen::DnType::CommonName, "kbf fetch test CA");
        ca.key_usages = vec![
            rcgen::KeyUsagePurpose::KeyCertSign,
            rcgen::KeyUsagePurpose::DigitalSignature,
        ];
        let key = rcgen::KeyPair::generate().expect("key");
        Pki {
            ca: rcgen::CertifiedIssuer::self_signed(ca, key).expect("CA"),
        }
    }

    fn leaf(&self, names: &[&str], usage: rcgen::ExtendedKeyUsagePurpose) -> Leaf {
        let names: Vec<String> = names.iter().map(|n| (*n).to_owned()).collect();
        let mut params = rcgen::CertificateParams::new(names).expect("params");
        params.extended_key_usages = vec![usage];
        let key = rcgen::KeyPair::generate().expect("key");
        let cert = params.signed_by(&key, &self.ca).expect("sign");
        Leaf {
            cert: cert.pem(),
            key: key.serialize_pem(),
        }
    }

    fn server(&self, names: &[&str]) -> Leaf {
        self.leaf(names, rcgen::ExtendedKeyUsagePurpose::ServerAuth)
    }

    fn client(&self) -> Leaf {
        self.leaf(&["node"], rcgen::ExtendedKeyUsagePurpose::ClientAuth)
    }

    fn ca_pem(&self) -> String {
        self.ca.pem()
    }
}

// ------------------------------------------------------------------- the HTTPS file server

/// What the HTTPS server does with a request for a file.
#[derive(Clone)]
enum Plan {
    /// 200 with these bytes.
    Body(Vec<u8>),
    /// 302 to this URL.
    Redirect(String),
    /// This status and no body.
    Status(u16),
    /// 200, a length of `declared`, but only `sent` bytes and then a close.
    Short { declared: usize, sent: usize },
    /// 200 with the first `sent` bytes of the body, then nothing, for as long as the test runs.
    Stall { declared: usize, sent: usize },
    /// A tree of files: 200 with the bytes of the path asked for, 404 for any other.
    Files(Vec<(String, Vec<u8>)>),
}

struct Https {
    addr: SocketAddr,
    /// The path of every request received.
    hits: Arc<Mutex<Vec<String>>>,
}

impl Https {
    fn start(leaf: &Leaf, plan: Plan) -> Https {
        use rustls::pki_types::pem::PemObject as _;
        use rustls::pki_types::{CertificateDer, PrivateKeyDer};
        let chain: Vec<CertificateDer<'static>> =
            CertificateDer::pem_slice_iter(leaf.cert.as_bytes())
                .collect::<Result<_, _>>()
                .expect("chain");
        let key = PrivateKeyDer::from_pem_slice(leaf.key.as_bytes()).expect("key");
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_no_client_auth()
        .with_single_cert(chain, key)
        .expect("server config");
        let config = Arc::new(config);
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        let hits = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&hits);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let (config, plan, seen) = (Arc::clone(&config), plan.clone(), Arc::clone(&seen));
                std::thread::spawn(move || {
                    let _ = answer(stream, config, &plan, &seen);
                });
            }
        });
        Https { addr, hits }
    }

    fn url(&self) -> String {
        format!("https://localhost:{}", self.addr.port())
    }

    fn hits(&self) -> Vec<String> {
        self.hits.lock().expect("lock").clone()
    }
}

fn answer(
    mut tcp: TcpStream,
    config: Arc<rustls::ServerConfig>,
    plan: &Plan,
    seen: &Mutex<Vec<String>>,
) -> std::io::Result<()> {
    let mut conn = rustls::ServerConnection::new(config).map_err(std::io::Error::other)?;
    let mut tls = rustls::Stream::new(&mut conn, &mut tcp);
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if tls.read(&mut byte)? == 0 {
            return Ok(());
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).into_owned();
    let path = head
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    seen.lock().expect("lock").push(path.clone());
    let reply = |status: &str, extra: &str, len: usize| {
        format!("HTTP/1.1 {status}\r\n{extra}Content-Length: {len}\r\nConnection: close\r\n\r\n")
    };
    match plan {
        Plan::Files(files) => match files.iter().find(|(at, _)| *at == path) {
            Some((_, bytes)) => {
                tls.write_all(reply("200 OK", "", bytes.len()).as_bytes())?;
                tls.write_all(bytes)?;
            }
            None => tls.write_all(reply("404 Not Found", "", 0).as_bytes())?,
        },
        Plan::Body(bytes) => {
            tls.write_all(reply("200 OK", "", bytes.len()).as_bytes())?;
            tls.write_all(bytes)?;
        }
        Plan::Redirect(to) => {
            tls.write_all(reply("302 Found", &format!("Location: {to}\r\n"), 0).as_bytes())?;
        }
        Plan::Status(code) => {
            tls.write_all(reply(&format!("{code} Nope"), "", 0).as_bytes())?;
        }
        Plan::Short { declared, sent } => {
            tls.write_all(reply("200 OK", "", *declared).as_bytes())?;
            tls.write_all(&body(*sent))?;
        }
        Plan::Stall { declared, sent } => {
            tls.write_all(reply("200 OK", "", *declared).as_bytes())?;
            tls.write_all(&body(*sent))?;
            tls.flush()?;
            std::thread::sleep(Duration::from_secs(600));
        }
    }
    tls.flush()?;
    tls.conn.send_close_notify();
    tls.flush()
}

// ------------------------------------------------------------------ the gRPC release server

/// What the gRPC server does with a `Fetch`.
#[derive(Clone)]
enum Serve {
    /// These bytes, in chunks.
    Bytes(Vec<u8>),
    NotFound,
    /// The first `sent` bytes and then nothing.
    Stall {
        sent: usize,
    },
    /// The first `sent` bytes and then an error status.
    #[cfg(target_os = "linux")]
    Break {
        sent: usize,
    },
}

/// What the gRPC server does with a `Describe`.
#[derive(Clone)]
enum Describing {
    /// This description.
    Of(kbf_proto::update::ReleaseDescription),
    /// This status.
    Status(tonic::Code, &'static str),
}

struct Grpc {
    port: u16,
    /// The DER certificates the client presented, per call.
    peers: Arc<Mutex<Vec<usize>>>,
    asked: Arc<Mutex<Vec<String>>>,
    /// The commit and platform of each `Describe`.
    described: Arc<Mutex<Vec<(String, String)>>>,
}

#[derive(Clone)]
struct Svc {
    serve: Serve,
    describing: Describing,
    peers: Arc<Mutex<Vec<usize>>>,
    asked: Arc<Mutex<Vec<String>>>,
    described: Arc<Mutex<Vec<(String, String)>>>,
}

type Chunks = std::pin::Pin<
    Box<dyn futures::Stream<Item = Result<kbf_proto::update::FetchChunk, tonic::Status>> + Send>,
>;

#[tonic::async_trait]
impl kbf_proto::update::release_service_server::ReleaseService for Svc {
    type FetchStream = Chunks;

    async fn describe(
        &self,
        request: tonic::Request<kbf_proto::update::DescribeRequest>,
    ) -> Result<tonic::Response<kbf_proto::update::ReleaseDescription>, tonic::Status> {
        self.peers
            .lock()
            .expect("lock")
            .push(request.peer_certs().map_or(0, |c| c.len()));
        let ask = request.get_ref();
        self.described
            .lock()
            .expect("lock")
            .push((ask.commit.clone(), ask.platform.clone()));
        match &self.describing {
            Describing::Of(description) => Ok(tonic::Response::new(description.clone())),
            Describing::Status(code, message) => Err(tonic::Status::new(*code, *message)),
        }
    }

    async fn fetch(
        &self,
        request: tonic::Request<kbf_proto::update::FetchRequest>,
    ) -> Result<tonic::Response<Chunks>, tonic::Status> {
        use kbf_proto::update::FetchChunk;
        self.peers
            .lock()
            .expect("lock")
            .push(request.peer_certs().map_or(0, |c| c.len()));
        self.asked
            .lock()
            .expect("lock")
            .push(request.get_ref().sha256.clone());
        let chunk = |b: &[u8]| Ok(FetchChunk { data: b.to_vec() });
        match &self.serve {
            Serve::NotFound => Err(tonic::Status::not_found("no stored file has that digest")),
            Serve::Bytes(bytes) => {
                let chunks: Vec<_> = bytes.chunks(1000).map(chunk).collect();
                Ok(tonic::Response::new(Box::pin(futures::stream::iter(
                    chunks,
                ))))
            }
            #[cfg(target_os = "linux")]
            Serve::Break { sent } => {
                let first = body(*sent);
                let broken = Err(tonic::Status::internal("the store failed"));
                Ok(tonic::Response::new(Box::pin(futures::stream::iter(vec![
                    chunk(&first),
                    broken,
                ]))))
            }
            Serve::Stall { sent } => {
                let first = body(*sent);
                let stream =
                    futures::stream::iter(vec![chunk(&first)]).chain(futures::stream::pending());
                Ok(tonic::Response::new(Box::pin(stream)))
            }
        }
    }
}

use futures::StreamExt as _;

impl Grpc {
    /// A worker listener that wants a client certificate signed by `client_ca`, and does not
    /// describe.
    fn start(server: &Leaf, client_ca: &str, serve: Serve) -> Grpc {
        let no = Describing::Status(tonic::Code::Unimplemented, "this server does not describe");
        Grpc::start_with(server, client_ca, serve, no)
    }

    /// The same, answering `Describe` as `describing` says.
    fn describing(server: &Leaf, client_ca: &str, describing: Describing) -> Grpc {
        Grpc::start_with(server, client_ca, Serve::NotFound, describing)
    }

    fn start_with(server: &Leaf, client_ca: &str, serve: Serve, describing: Describing) -> Grpc {
        use tonic::transport::{Certificate, Identity, Server, ServerTlsConfig};
        let peers = Arc::new(Mutex::new(Vec::new()));
        let asked = Arc::new(Mutex::new(Vec::new()));
        let described = Arc::new(Mutex::new(Vec::new()));
        let svc = Svc {
            serve,
            describing,
            peers: Arc::clone(&peers),
            asked: Arc::clone(&asked),
            described: Arc::clone(&described),
        };
        let tls = ServerTlsConfig::new()
            .identity(Identity::from_pem(&server.cert, &server.key))
            .client_ca_root(Certificate::from_pem(client_ca));
        let std_listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        std_listener.set_nonblocking(true).expect("nonblocking");
        let port = std_listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .expect("runtime");
            runtime.block_on(async move {
                let listener = tokio::net::TcpListener::from_std(std_listener).expect("listener");
                let incoming = tonic::transport::server::TcpIncoming::from(listener);
                let _ = Server::builder()
                    .tls_config(tls)
                    .expect("tls")
                    .add_service(
                        kbf_proto::update::release_service_server::ReleaseServiceServer::new(svc),
                    )
                    .serve_with_incoming(incoming)
                    .await;
            });
        });
        Grpc {
            port,
            peers,
            asked,
            described,
        }
    }

    fn url(&self) -> String {
        format!("https://localhost:{}", self.port)
    }
}

// ---------------------------------------------------------------------------- the harness

struct Rig {
    dir: PathBuf,
}

impl Rig {
    fn new(name: &str) -> Rig {
        Rig {
            dir: kbf_host::testkit::scratch(&format!("fetch-e2e-{name}")),
        }
    }

    fn write(&self, name: &str, text: &str) -> PathBuf {
        let path = self.dir.join(name);
        fs::write(&path, text).expect("write");
        path
    }

    fn frontend(&self, url: &str, ca: &str, client: &Leaf) -> Source {
        Source::Frontend(FrontendSource {
            url: url.to_owned(),
            ca_file: self.write("ca.pem", ca),
            node_cert: self.write("node.pem", &client.cert),
            node_key: self.write("node.key", &client.key),
        })
    }

    fn url_source(&self, base: &str, ca: &str) -> Source {
        Source::Url {
            base: base.to_owned(),
            ca_file: Some(self.write("url-ca.pem", ca)),
        }
    }

    /// Pull `bytes` (declared as `size`, named by `digest`) and give back the result and the
    /// file the parent wrote.
    fn pull(
        &self,
        source: &Source,
        digest: &str,
        size: u64,
        timeout: Duration,
    ) -> (Result<Received, FetchError>, Vec<u8>, Duration) {
        let child = ChildProgram {
            program: PathBuf::from(BIN),
            run_as: None,
        };
        let limits = Limits {
            max_component_bytes: 1 << 20,
            fetch_timeout: timeout,
        };
        let path = self.dir.join("download");
        let mut out = File::create(&path).expect("file");
        let started = Instant::now();
        let got = fetch_component(
            &child,
            source,
            limits,
            &Component {
                sha256: digest,
                size,
            },
            (&self.dir, 0),
            &mut out,
        );
        (
            got,
            fs::read(&path).expect("the download"),
            started.elapsed(),
        )
    }
}

const LONG: Duration = Duration::from_secs(120);

fn pki_and_server() -> (Pki, Leaf) {
    let pki = Pki::new();
    let server = pki.server(&["localhost"]);
    (pki, server)
}

fn short_log(got: &Result<Received, FetchError>) -> String {
    match got {
        Err(FetchError::Pipe(PipeFault::Short { log, .. })) => log.clone(),
        other => panic!("not a short read: {other:?}"),
    }
}

// ------------------------------------------------------------------------- the url source

/// The happy path, and the shape of the request: the blob is asked for at
/// `<base path>/files/<sha256>`, nothing else, and the bytes come back exactly.
///
/// Catches a URL assembled differently from the store layout the contract fixes, and bytes
/// altered on the way through the pipe.
#[test]
fn the_url_source_serves_the_bytes_at_files_sha256() {
    let (pki, leaf) = pki_and_server();
    let bytes = body(300_000);
    let server = Https::start(&leaf, Plan::Body(bytes.clone()));
    let rig = Rig::new("url-ok");
    let source = rig.url_source(&format!("{}/mirror", server.url()), &pki.ca_pem());
    let (got, file, _) = rig.pull(&source, &sha(&bytes), bytes.len() as u64, LONG);
    let got = got.expect("fetched");
    assert_eq!((got.count, got.sha256), (bytes.len() as u64, sha(&bytes)));
    assert_eq!(file, bytes);
    assert_eq!(server.hits(), [format!("/mirror/files/{}", sha(&bytes))]);
}

/// A redirect is a failed fetch and is not followed: the second server, which holds exactly
/// the bytes asked for, is never contacted, and the first is asked once.
///
/// Rule: "The child follows no redirect at all; any 3xx is a failed fetch." A followed redirect
/// is how a compromised mirror sends a host to another source than the one configured.
///
/// Mutant seen red: `.max_redirects(0)` removed from the ureq agent (the fetch then succeeds
/// from the second server).
#[test]
fn a_redirect_is_a_failed_fetch_and_is_not_followed() {
    let (pki, leaf) = pki_and_server();
    let bytes = body(5000);
    let target = Https::start(&leaf, Plan::Body(bytes.clone()));
    let first = Https::start(
        &leaf,
        Plan::Redirect(format!("{}/files/{}", target.url(), sha(&bytes))),
    );
    let rig = Rig::new("url-redirect");
    let source = rig.url_source(&first.url(), &pki.ca_pem());
    let (got, file, _) = rig.pull(&source, &sha(&bytes), bytes.len() as u64, LONG);
    assert!(short_log(&got).contains("302"), "{got:?}");
    assert!(file.is_empty());
    assert_eq!(first.hits().len(), 1);
    assert_eq!(
        target.hits(),
        Vec::<String>::new(),
        "the redirect was followed"
    );
}

/// Any answer but 200 is a failed fetch with nothing written (a 204 or a 206 is not "the
/// bytes").
#[test]
fn only_a_200_is_a_fetch() {
    let (pki, leaf) = pki_and_server();
    let bytes = body(100);
    for code in [204_u16, 206, 404, 500] {
        let server = Https::start(&leaf, Plan::Status(code));
        let rig = Rig::new(&format!("url-status-{code}"));
        let source = rig.url_source(&server.url(), &pki.ca_pem());
        let (got, file, _) = rig.pull(&source, &sha(&bytes), 100, LONG);
        assert!(
            short_log(&got).contains(&code.to_string()),
            "{code}: {got:?}"
        );
        assert!(file.is_empty());
    }
}

/// A source that sends more than it declared: the child is killed the moment one byte more
/// arrives, and the file never holds more than the declared size.
///
/// Rule: "at most `size` bytes (one more kills the child)".
///
/// Mutants seen red: `count > caps.size` in the pipe made `>=` (the exact fetch fails); the
/// parent passing `size` instead of `size + 1` as the child's limit (a source that sends too
/// much is then silently cut to size and accepted if the prefix hashes right, which here it
/// does not, so the test names the cause in the error type too).
#[test]
fn a_source_that_sends_more_than_declared_is_refused() {
    let (pki, leaf) = pki_and_server();
    let bytes = body(20_000);
    let server = Https::start(&leaf, Plan::Body(bytes.clone()));
    let rig = Rig::new("url-oversize");
    let source = rig.url_source(&server.url(), &pki.ca_pem());
    let declared = 15_000;
    let (got, file, _) = rig.pull(&source, &sha(&bytes), declared, LONG);
    assert!(
        matches!(got, Err(FetchError::Pipe(PipeFault::Oversize { size })) if size == declared),
        "{got:?}"
    );
    assert!(
        file.len() as u64 <= declared,
        "{} bytes written",
        file.len()
    );
}

/// A source that sends exactly one byte more than declared is also seen (the child's limit is
/// size + 1, so the extra byte reaches the parent).
#[test]
fn one_byte_too_many_is_seen() {
    let (pki, leaf) = pki_and_server();
    let bytes = body(1001);
    let server = Https::start(&leaf, Plan::Body(bytes.clone()));
    let rig = Rig::new("url-one-over");
    let source = rig.url_source(&server.url(), &pki.ca_pem());
    let (got, _, _) = rig.pull(&source, &sha(&bytes), 1000, LONG);
    assert!(
        matches!(got, Err(FetchError::Pipe(PipeFault::Oversize { .. }))),
        "{got:?}"
    );
}

/// Fewer bytes than declared is a failed fetch, whatever the digest of the prefix.
#[test]
fn a_short_body_is_a_failed_fetch() {
    let (pki, leaf) = pki_and_server();
    let bytes = body(10_000);
    let server = Https::start(
        &leaf,
        Plan::Short {
            declared: 10_000,
            sent: 4_000,
        },
    );
    let rig = Rig::new("url-short");
    let source = rig.url_source(&server.url(), &pki.ca_pem());
    let (got, file, _) = rig.pull(&source, &sha(&bytes), 10_000, LONG);
    assert!(
        matches!(
            got,
            Err(FetchError::Pipe(PipeFault::Short { got: 0..=4000, .. }))
        ),
        "{got:?}"
    );
    assert!(file.len() <= 4000);
}

/// The right number of bytes with the wrong content is refused by the parent's comparison of
/// what it hashed with what the order names, and the refusal word is `hash`.
///
/// Rule: verification step 2 ("the SHA-256 computed while reading equals the record's").
///
/// Mutant seen red: the `got.sha256 != component.sha256` comparison removed in
/// `fetch_component`.
#[test]
fn bytes_of_the_right_size_and_the_wrong_digest_are_refused() {
    let (pki, leaf) = pki_and_server();
    let served = body(5000);
    let asked_for = sha(&body(5001)); // some other blob's name
    let server = Https::start(&leaf, Plan::Body(served));
    let rig = Rig::new("url-wrong-bytes");
    let source = rig.url_source(&server.url(), &pki.ca_pem());
    let (got, _, _) = rig.pull(&source, &asked_for, 5000, LONG);
    let err = got.expect_err("refused");
    assert!(matches!(err, FetchError::Digest { .. }), "{err:?}");
    assert_eq!(err.word(), "hash");
}

/// A source that stops sending is cut off at the time limit, not waited for: a stall ends the
/// fetch within moments of the limit given, and kills the child.
///
/// Rule: "bounded time" (no byte for 60 s, `fetch_timeout_s` in all; shortened here).
///
/// Mutant seen red: the `quiet` deadline removed from the pipe's wait (the fetch then ends at
/// the whole-fetch limit instead, which the bound below also catches when the limit is long).
#[test]
fn a_source_that_stalls_is_cut_off() {
    let (pki, leaf) = pki_and_server();
    let bytes = body(10_000);
    let server = Https::start(
        &leaf,
        Plan::Stall {
            declared: 10_000,
            sent: 3_000,
        },
    );
    let rig = Rig::new("url-stall");
    let source = rig.url_source(&server.url(), &pki.ca_pem());
    let (got, _, took) = rig.pull(&source, &sha(&bytes), 10_000, Duration::from_secs(3));
    assert!(
        matches!(
            got,
            Err(FetchError::Pipe(
                PipeFault::Stalled { .. } | PipeFault::TimedOut { .. }
            ))
        ),
        "{got:?}"
    );
    assert!(took < Duration::from_secs(20), "{took:?}");
}

/// The real child cannot be looked into by another process of its user: `/proc/<pid>/fd` of the
/// running child is refused to this test (same user, no privilege). A process a compromised child
/// left behind outside its process group is such a process, and the child of the next fetch holds
/// the renewed key on its descriptor 4: readable through `/proc` unless the child has made itself
/// non-dumpable. (Root reads any process, so the test says so and stops there when run as root.)
///
/// Catches a child that leaves itself dumpable after its `exec`, which a `prctl` before the exec
/// cannot prevent (the exec makes it dumpable again).
///
/// Mutant: the `forbid_inspection` call removed from `main`.
#[cfg(target_os = "linux")]
#[test]
fn the_fetch_child_cannot_be_looked_into_by_its_users_other_processes() {
    use std::process::{Command, Stdio};
    if rustix::process::geteuid().is_root() {
        eprintln!("skipped: root can look into every process, so the check proves nothing");
        return;
    }
    let (pki, leaf) = pki_and_server();
    let bytes = body(10_000);
    let server = Https::start(
        &leaf,
        Plan::Stall {
            declared: 10_000,
            sent: 3_000,
        },
    );
    let rig = Rig::new("url-dumpable");
    let ca = rig.write("ca.pem", &pki.ca_pem());
    let mut command = Command::new(BIN);
    command
        .args(["fetch-child", "--kind", "url", "--url", &server.url()])
        .args(["--sha256", &sha(&bytes), "--limit", "10001", "--ca-file"])
        .arg(&ca)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    // The child refuses to run holding a descriptor it was not lent, and other tests of this
    // binary leave one open on purpose while they run. The parent's start marks them all
    // close-on-exec; so does this one (best effort: before Linux 5.11 the call is not there).
    // SAFETY: the closure makes one system call, on numbers, between fork and exec.
    unsafe {
        use std::os::unix::process::CommandExt as _;
        command.pre_exec(|| {
            let _ = libc::syscall(libc::SYS_close_range, 3_u32, u32::MAX, 4_u32);
            Ok(())
        });
    }
    let mut child = command.spawn().expect("the child starts");
    // The server was asked for the file: the child is past its start-up and is waiting for more.
    let until = Instant::now() + Duration::from_secs(60);
    let mut ended = None;
    while server.hits().is_empty() && Instant::now() < until {
        ended = child.try_wait().expect("the child can be waited for");
        if ended.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let seen = std::fs::read_link(format!("/proc/{}/fd/0", child.id()));
    let _ = child.kill();
    let mut said = String::new();
    let _ = child
        .stderr
        .take()
        .expect("standard error is piped")
        .read_to_string(&mut said);
    let _ = child.wait();
    assert!(
        !server.hits().is_empty(),
        "the child never asked the server (ended: {ended:?}); it said: {said}"
    );
    let err = seen.expect_err("another process of the user must not be able to look into it");
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied, "{err}");
}

/// A server whose certificate the client's roots do not vouch for is not talked to: the
/// transport is authenticated even though the bytes are trusted by verification.
///
/// Catches a client that skips certificate verification.
#[test]
fn a_server_under_another_ca_is_not_trusted() {
    let (_, leaf) = pki_and_server();
    let other = Pki::new();
    let bytes = body(100);
    let server = Https::start(&leaf, Plan::Body(bytes.clone()));
    let rig = Rig::new("url-bad-ca");
    let source = rig.url_source(&server.url(), &other.ca_pem());
    let (got, file, _) = rig.pull(&source, &sha(&bytes), 100, LONG);
    assert!(
        matches!(got, Err(FetchError::Pipe(PipeFault::Short { .. }))),
        "{got:?}"
    );
    assert!(file.is_empty());
}

/// The same server, reached under a name its certificate does not carry, is refused.
#[test]
fn a_server_under_another_name_is_not_trusted() {
    let pki = Pki::new();
    let leaf = pki.server(&["elsewhere.example"]);
    let bytes = body(100);
    let server = Https::start(&leaf, Plan::Body(bytes.clone()));
    let rig = Rig::new("url-bad-name");
    let source = rig.url_source(&server.url(), &pki.ca_pem());
    let (got, _, _) = rig.pull(&source, &sha(&bytes), 100, LONG);
    assert!(
        matches!(got, Err(FetchError::Pipe(PipeFault::Short { .. }))),
        "{got:?}"
    );
}

/// A descriptor the parent's process holds open and inheritable is not the child's: the child
/// checks its own table before it connects, and the fetch still succeeds because the parent
/// marked it close-on-exec.
///
/// Rule: "closes every descriptor but 0, 1, 2 and 4".
///
/// Mutant seen red: the `range_cloexec` marking left out of the child's start (the child then
/// finds the stray descriptor and the fetch fails).
#[test]
fn a_descriptor_the_parent_holds_does_not_reach_the_child() {
    let (pki, leaf) = pki_and_server();
    let bytes = body(1000);
    let server = Https::start(&leaf, Plan::Body(bytes.clone()));
    let rig = Rig::new("url-stray");
    let held = File::open("/").expect("a directory");
    // SAFETY: dup of a descriptor this test owns; dup clears close-on-exec, on purpose.
    let leaked = unsafe { libc::dup(std::os::fd::AsRawFd::as_raw_fd(&held)) };
    assert!(leaked > 2);
    let source = rig.url_source(&server.url(), &pki.ca_pem());
    let (got, file, _) = rig.pull(&source, &sha(&bytes), 1000, LONG);
    // SAFETY: closing the descriptor made above.
    unsafe { libc::close(leaked) };
    got.expect("fetched");
    assert_eq!(file, bytes);
}

// --------------------------------------------------------------------- the frontend source

/// The happy path over mutual TLS: the frontend sees the node's certificate (so the identity
/// was lent and used), is asked for the blob by its digest, and the bytes come back exactly.
///
/// Rule: "for the `frontend` source the parent lends the node certificate and key ... over
/// descriptor 4".
///
/// Mutants seen red: the parent not lending (`lend` passed as `None`: the child has no
/// identity and the fetch fails); the child not presenting the identity (`.identity(..)` left
/// out: the server's handshake fails).
#[test]
fn the_frontend_source_presents_the_node_certificate_and_serves_the_bytes() {
    let (pki, server) = pki_and_server();
    let bytes = body(250_000);
    let grpc = Grpc::start(&server, &pki.ca_pem(), Serve::Bytes(bytes.clone()));
    let rig = Rig::new("front-ok");
    let source = rig.frontend(&grpc.url(), &pki.ca_pem(), &pki.client());
    let (got, file, _) = rig.pull(&source, &sha(&bytes), bytes.len() as u64, LONG);
    let got = got.expect("fetched");
    assert_eq!(got.sha256, sha(&bytes));
    assert_eq!(file, bytes);
    assert_eq!(*grpc.peers.lock().expect("lock"), [1]);
    assert_eq!(*grpc.asked.lock().expect("lock"), [sha(&bytes)]);
}

/// A node certificate the frontend's client CA does not know is not admitted: the identity the
/// child presents is the one lent, not any other.
#[test]
fn a_node_certificate_from_another_ca_is_refused_by_the_frontend() {
    let (pki, server) = pki_and_server();
    let stranger = Pki::new();
    let bytes = body(100);
    let grpc = Grpc::start(&server, &pki.ca_pem(), Serve::Bytes(bytes.clone()));
    let rig = Rig::new("front-stranger");
    let source = rig.frontend(&grpc.url(), &pki.ca_pem(), &stranger.client());
    let (got, file, _) = rig.pull(&source, &sha(&bytes), 100, LONG);
    assert!(
        matches!(got, Err(FetchError::Pipe(PipeFault::Short { .. }))),
        "{got:?}"
    );
    assert!(file.is_empty());
    assert_eq!(
        grpc.asked.lock().expect("lock").len(),
        0,
        "the call reached the service"
    );
}

/// A frontend whose certificate is not signed by the pinned CA is not talked to.
///
/// Rule: the `frontend` source "authenticates the server with the pinned CA".
///
/// Mutant seen red: the child trusting any certificate, or adding the system roots to the
/// pinned CA when the pinned one fails.
#[test]
fn a_frontend_under_another_ca_is_not_trusted() {
    let (pki, server) = pki_and_server();
    let pinned = Pki::new();
    let bytes = body(100);
    let grpc = Grpc::start(&server, &pki.ca_pem(), Serve::Bytes(bytes.clone()));
    let rig = Rig::new("front-bad-ca");
    let source = rig.frontend(&grpc.url(), &pinned.ca_pem(), &pki.client());
    let (got, _, _) = rig.pull(&source, &sha(&bytes), 100, LONG);
    assert!(
        matches!(got, Err(FetchError::Pipe(PipeFault::Short { .. }))),
        "{got:?}"
    );
    assert_eq!(grpc.asked.lock().expect("lock").len(), 0);
}

/// A frontend whose certificate names another host than the URL's is not talked to: the name
/// is checked against the SAN.
///
/// Rule: "the frontend name checked against the certificate SAN".
///
/// Mutant seen red: `.domain_name(host)` pointing at the certificate's own name instead of the
/// URL's host.
#[test]
fn a_frontend_whose_certificate_names_another_host_is_not_trusted() {
    let pki = Pki::new();
    let server = pki.server(&["elsewhere.example"]);
    let bytes = body(100);
    let grpc = Grpc::start(&server, &pki.ca_pem(), Serve::Bytes(bytes.clone()));
    let rig = Rig::new("front-bad-name");
    let source = rig.frontend(&grpc.url(), &pki.ca_pem(), &pki.client());
    let (got, _, _) = rig.pull(&source, &sha(&bytes), 100, LONG);
    assert!(
        matches!(got, Err(FetchError::Pipe(PipeFault::Short { .. }))),
        "{got:?}"
    );
    assert_eq!(grpc.asked.lock().expect("lock").len(), 0);
}

/// The frontend does not hold the digest: a failed fetch that names why, nothing written.
#[test]
fn a_digest_the_frontend_does_not_hold_is_a_failed_fetch() {
    let (pki, server) = pki_and_server();
    let grpc = Grpc::start(&server, &pki.ca_pem(), Serve::NotFound);
    let rig = Rig::new("front-not-found");
    let source = rig.frontend(&grpc.url(), &pki.ca_pem(), &pki.client());
    let (got, file, _) = rig.pull(&source, &sha(b"x"), 100, LONG);
    assert!(short_log(&got).contains("NotFound"), "{got:?}");
    assert!(file.is_empty());
}

/// A frontend that streams more than the declared size is cut off at one byte more, as a file
/// server is.
#[test]
fn a_frontend_that_sends_more_than_declared_is_refused() {
    let (pki, server) = pki_and_server();
    let bytes = body(20_000);
    let grpc = Grpc::start(&server, &pki.ca_pem(), Serve::Bytes(bytes.clone()));
    let rig = Rig::new("front-oversize");
    let source = rig.frontend(&grpc.url(), &pki.ca_pem(), &pki.client());
    let (got, file, _) = rig.pull(&source, &sha(&bytes), 15_000, LONG);
    assert!(
        matches!(
            got,
            Err(FetchError::Pipe(PipeFault::Oversize { size: 15_000 }))
        ),
        "{got:?}"
    );
    assert!(file.len() <= 15_000);
}

/// A frontend that sends the right number of the wrong bytes is refused on the digest.
#[test]
fn a_frontend_that_sends_the_wrong_bytes_is_refused() {
    let (pki, server) = pki_and_server();
    let grpc = Grpc::start(&server, &pki.ca_pem(), Serve::Bytes(body(3000)));
    let rig = Rig::new("front-wrong");
    let source = rig.frontend(&grpc.url(), &pki.ca_pem(), &pki.client());
    let (got, _, _) = rig.pull(&source, &sha(b"other"), 3000, LONG);
    assert!(matches!(got, Err(FetchError::Digest { .. })), "{got:?}");
}

/// A frontend that stalls is cut off.
#[test]
fn a_frontend_that_stalls_is_cut_off() {
    let (pki, server) = pki_and_server();
    let grpc = Grpc::start(&server, &pki.ca_pem(), Serve::Stall { sent: 500 });
    let rig = Rig::new("front-stall");
    let source = rig.frontend(&grpc.url(), &pki.ca_pem(), &pki.client());
    let (got, _, took) = rig.pull(&source, &sha(b"x"), 5000, Duration::from_secs(3));
    assert!(
        matches!(
            got,
            Err(FetchError::Pipe(
                PipeFault::Stalled { .. } | PipeFault::TimedOut { .. }
            ))
        ),
        "{got:?}"
    );
    assert!(took < Duration::from_secs(20), "{took:?}");
}

/// The key file is opened without following a link: a link in its place is a refusal before a
/// child exists, so nothing was asked of the frontend.
///
/// Rule: "the installer opens them `O_NOFOLLOW` at each fetch".
///
/// Mutant seen red: `O_NOFOLLOW` removed from `read_pem` (the link is followed and the fetch
/// succeeds).
#[test]
fn a_linked_key_file_is_refused_before_any_child_starts() {
    let (pki, server) = pki_and_server();
    let bytes = body(100);
    let grpc = Grpc::start(&server, &pki.ca_pem(), Serve::Bytes(bytes.clone()));
    let rig = Rig::new("front-link");
    let client = pki.client();
    let real = rig.write("real.key", &client.key);
    let link = rig.dir.join("linked.key");
    std::os::unix::fs::symlink(&real, &link).expect("link");
    let source = Source::Frontend(FrontendSource {
        url: grpc.url(),
        ca_file: rig.write("ca.pem", &pki.ca_pem()),
        node_cert: rig.write("node.pem", &client.cert),
        node_key: link,
    });
    let (got, _, _) = rig.pull(&source, &sha(&bytes), 100, LONG);
    assert!(matches!(got, Err(FetchError::Identity(_))), "{got:?}");
    assert_eq!(grpc.asked.lock().expect("lock").len(), 0);
}

/// Neither the error nor its words carry the key, whatever went wrong.
#[test]
fn the_key_is_in_no_error() {
    let (pki, server) = pki_and_server();
    let stranger = Pki::new();
    let grpc = Grpc::start(&server, &pki.ca_pem(), Serve::NotFound);
    let rig = Rig::new("front-nokey");
    let client = stranger.client();
    let source = rig.frontend(&grpc.url(), &pki.ca_pem(), &client);
    let (got, _, _) = rig.pull(&source, &sha(b"x"), 10, LONG);
    let shown = format!("{got:?}");
    assert!(!shown.contains("PRIVATE KEY"), "{shown}");
    let body_of_key = client.key.lines().nth(1).expect("key body");
    assert!(!shown.contains(body_of_key), "{shown}");
}

// ----------------------------------------------------------------------- the child's flags

/// The child started by hand, as a parent that closes everything above the standard streams
/// would start it (the test harness may hand its own children descriptors, which the child
/// rightly refuses).
fn child_command(args: &[&str]) -> std::process::Command {
    let mut command = std::process::Command::new(BIN);
    command.arg("fetch-child").args(args).env_clear();
    // An instrumented run (coverage) names where each process writes its profile; without it
    // the child writes one into the working directory, inside the repository.
    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        command.env("LLVM_PROFILE_FILE", profile);
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt as _;
        // SAFETY: the closure runs between fork and exec and makes one system call.
        unsafe {
            command.pre_exec(|| {
                libc::syscall(libc::SYS_close_range, 3_u32, u32::MAX, 0_u32);
                Ok(())
            });
        }
    }
    command
}

fn run_child(args: &[&str]) -> (bool, String) {
    let out = child_command(args).output().expect("runs");
    (
        out.status.success(),
        String::from_utf8_lossy(&out.stderr).into_owned(),
    )
}

/// The child checks its own arguments again, whatever the parent checked.
///
/// Catches a child that trusts its caller: a bad digest, a limit of zero or over the ceiling, a
/// plain-http URL, a url fetch that is lent an identity, a frontend fetch without one.
#[test]
fn the_child_checks_its_flags_again() {
    let good = sha(b"x");
    let cases: Vec<(Vec<&str>, &str)> = vec![
        (
            vec![
                "--kind",
                "url",
                "--url",
                "https://h.example",
                "--sha256",
                "ABC",
                "--limit",
                "10",
            ],
            "not a sha256",
        ),
        (
            vec![
                "--kind",
                "url",
                "--url",
                "https://h.example",
                "--sha256",
                &good,
                "--limit",
                "0",
            ],
            "limit",
        ),
        (
            vec![
                "--kind",
                "url",
                "--url",
                "https://h.example",
                "--sha256",
                &good,
                "--limit",
                "9999999999",
            ],
            "limit",
        ),
        (
            vec![
                "--kind",
                "url",
                "--url",
                "http://h.example",
                "--sha256",
                &good,
                "--limit",
                "10",
            ],
            "not a plain https URL",
        ),
        (
            vec![
                "--kind",
                "url",
                "--url",
                "https://u@h.example",
                "--sha256",
                &good,
                "--limit",
                "10",
            ],
            "not a plain https URL",
        ),
        (
            vec![
                "--kind",
                "url",
                "--url",
                "https://h.example",
                "--sha256",
                &good,
                "--limit",
                "10",
                "--identity-fd",
                "5",
            ],
            "no identity",
        ),
        (
            vec![
                "--kind",
                "frontend",
                "--url",
                "https://h.example",
                "--sha256",
                &good,
                "--limit",
                "10",
            ],
            "--identity-fd",
        ),
    ];
    for (args, said) in cases {
        let (ok, err) = run_child(&args);
        assert!(!ok && err.contains(said), "{args:?}: {err}");
    }
}

/// An unreachable port is a failed fetch with one line, and nothing on standard output.
#[test]
fn an_unreachable_source_is_one_line_on_standard_error() {
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    };
    let good = sha(b"x");
    let url = format!("https://localhost:{port}");
    let out = child_command(&[
        "--kind", "url", "--url", &url, "--sha256", &good, "--limit", "10",
    ])
    .output()
    .expect("runs");
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    let err = String::from_utf8_lossy(&out.stderr);
    assert_eq!(err.trim().lines().count(), 1, "{err}");
    assert!(err.contains("source unreachable"), "{err}");
}

/// A proxy named in the environment is not used: the url source goes straight to the server.
///
/// Rule: "No proxy environment" (the parent starts the child with an empty environment; the child
/// must not depend on that). A proxy would see the request path and could answer for the mirror.
///
/// Mutant seen red: `.proxy(None)` removed from the ureq agent (the child then connects to the
/// listener named by `HTTPS_PROXY` and the fetch fails).
#[test]
fn a_proxy_in_the_environment_is_not_used() {
    let (pki, leaf) = pki_and_server();
    let bytes = body(1000);
    let server = Https::start(&leaf, Plan::Body(bytes.clone()));
    let proxy = TcpListener::bind("127.0.0.1:0").expect("bind");
    proxy.set_nonblocking(true).expect("nonblocking");
    let proxy_url = format!(
        "http://127.0.0.1:{}",
        proxy.local_addr().expect("addr").port()
    );
    let rig = Rig::new("no-proxy");
    let ca = rig.write("ca.pem", &pki.ca_pem());
    let digest = sha(&bytes);
    let mut command = child_command(&[
        "--kind",
        "url",
        "--url",
        &server.url(),
        "--sha256",
        &digest,
        "--limit",
        "1001",
        "--ca-file",
        ca.to_str().expect("utf-8"),
    ]);
    for name in [
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "HTTP_PROXY",
    ] {
        command.env(name, &proxy_url);
    }
    let out = command.output().expect("runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(out.stdout, bytes);
    assert!(proxy.accept().is_err(), "the proxy was contacted");
    assert_eq!(server.hits(), [format!("/files/{digest}")]);
}

// ------------------------------------------------- the child started by hand (not confined)
//
// The children `fetch_component` starts are confined to write no file, so their coverage
// profile cannot be written; the same code is run here by a plain start of the binary (with the
// test's own profile variable), and the tests state what the child does on its own.

/// The arguments of a url fetch of `bytes` from `server`, trusting `ca`.
fn url_args(url: &str, ca: &std::path::Path, digest: &str, limit: u64) -> Vec<String> {
    [
        "--kind",
        "url",
        "--url",
        url,
        "--sha256",
        digest,
        "--limit",
        &limit.to_string(),
        "--ca-file",
        ca.to_str().expect("utf-8"),
    ]
    .map(str::to_owned)
    .to_vec()
}

fn by_hand(args: &[String]) -> std::process::Output {
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    child_command(&args).output().expect("runs")
}

fn said(out: &std::process::Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// Whatever the source sends beyond the limit the child is given is not written.
///
/// Mutant seen red: `.take(limit)` removed from `copy_limited` (all 1000 bytes are written).
#[test]
fn the_child_writes_no_more_than_its_limit() {
    let (pki, leaf) = pki_and_server();
    let bytes = body(1000);
    let server = Https::start(&leaf, Plan::Body(bytes.clone()));
    let rig = Rig::new("hand-limit");
    let ca = rig.write("ca.pem", &pki.ca_pem());
    let out = by_hand(&url_args(&server.url(), &ca, &sha(&bytes), 600));
    assert!(out.status.success(), "{}", said(&out));
    assert_eq!(out.stdout, bytes[..600]);
}

/// A source that answers anything but 200 is a failed fetch that names the status.
#[test]
fn the_child_names_the_status_it_was_answered() {
    let (pki, leaf) = pki_and_server();
    let server = Https::start(&leaf, Plan::Status(404));
    let rig = Rig::new("hand-status");
    let ca = rig.write("ca.pem", &pki.ca_pem());
    let out = by_hand(&url_args(&server.url(), &ca, &sha(b"x"), 10));
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(said(&out).contains("answered 404"), "{}", said(&out));
}

/// A body that ends before its declared length is a failed fetch with the transport's words,
/// and the bytes that did arrive are all the child wrote.
#[test]
fn a_body_cut_short_is_a_failed_fetch() {
    let (pki, leaf) = pki_and_server();
    let server = Https::start(
        &leaf,
        Plan::Short {
            declared: 5000,
            sent: 100,
        },
    );
    let rig = Rig::new("hand-short");
    let ca = rig.write("ca.pem", &pki.ca_pem());
    let out = by_hand(&url_args(&server.url(), &ca, &sha(b"x"), 6000));
    assert!(!out.status.success());
    assert!(said(&out).contains("source unreachable"), "{}", said(&out));
}

/// A reader that has gone away is not a source fault: the child says it cannot write.
#[test]
fn a_reader_that_has_gone_is_a_write_fault() {
    let (pki, leaf) = pki_and_server();
    let bytes = body(900_000);
    let server = Https::start(&leaf, Plan::Body(bytes.clone()));
    let rig = Rig::new("hand-gone");
    let ca = rig.write("ca.pem", &pki.ca_pem());
    let args = url_args(&server.url(), &ca, &sha(&bytes), 1_000_000);
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut command = child_command(&args);
    command
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = command.spawn().expect("starts");
    drop(child.stdout.take());
    let out = child.wait_with_output().expect("ends");
    assert!(!out.status.success());
    assert!(
        said(&out).contains("cannot write the bytes"),
        "{}",
        said(&out)
    );
}

/// The roots a url child trusts come from the file it is given and from nowhere else: a file
/// that is missing, holds no certificate or is not PEM is a failed start naming the file.
#[test]
fn a_roots_file_that_cannot_be_used_is_a_failed_start() {
    let rig = Rig::new("hand-roots");
    let digest = sha(b"x");
    let none = rig.write("none.pem", "no certificates here\n");
    let broken = rig.write(
        "broken.pem",
        "-----BEGIN CERTIFICATE-----\n@@@@\n-----END CERTIFICATE-----\n",
    );
    let missing = rig.dir.join("missing.pem");
    for (file, why) in [
        (&missing, "cannot read the roots"),
        (&none, "no certificate"),
        (&broken, "cannot read the roots"),
    ] {
        let out = by_hand(&url_args("https://localhost:1", file, &digest, 10));
        assert!(!out.status.success());
        let err = said(&out);
        assert!(err.contains(why), "{file:?}: {err}");
        assert!(err.contains(file.to_str().expect("utf-8")), "{err}");
    }
}

/// A URL that is not exactly what settings write (a host in capitals, a path on a frontend) is
/// refused by the child itself.
#[test]
fn the_child_refuses_a_url_that_is_not_in_the_form_settings_write() {
    let good = sha(b"x");
    for url in ["https://H.example", "https://h.example/"] {
        let args = [
            "--kind", "url", "--url", url, "--sha256", &good, "--limit", "10",
        ];
        let (ok, err) = run_child(&args);
        assert!(!ok);
        assert!(err.contains("not a plain https URL"), "{url}: {err}");
    }
}

// The lent identity needs a descriptor to lend it on; the child is started with exactly the
// descriptors a parent would give it (0, 1, 2 and the lent one) unless a test says otherwise.

/// What a by-hand frontend fetch is given.
#[cfg(target_os = "linux")]
struct Lent<'a> {
    id: &'a kbf_installer::fetch::identity::Identity,
    /// The number the descriptor has in the child, and in `--identity-fd`.
    at: i32,
    /// Close everything above the standard streams and the lent descriptor.
    tidy: bool,
}

#[cfg(target_os = "linux")]
fn frontend_by_hand(url: &str, digest: &str, limit: u64, lent: &Lent<'_>) -> std::process::Output {
    frontend_asked(url, &["--sha256", digest], limit, lent)
}

/// A by-hand frontend child asked for `what` (`--sha256 <digest>`, or `--describe <commit>`
/// and the like).
#[cfg(target_os = "linux")]
fn frontend_asked(url: &str, what: &[&str], limit: u64, lent: &Lent<'_>) -> std::process::Output {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::process::CommandExt as _;
    let fd = lent.id.lend().expect("lends");
    let (from, at, tidy) = (fd.as_raw_fd(), lent.at, lent.tidy);
    let mut command = std::process::Command::new(BIN);
    command
        .args(["fetch-child", "--kind", "frontend", "--url", url])
        .args(what)
        .args(["--limit", &limit.to_string()])
        .args(["--identity-fd", &at.to_string()])
        .env_clear();
    if let Some(profile) = std::env::var_os("LLVM_PROFILE_FILE") {
        command.env("LLVM_PROFILE_FILE", profile);
    }
    // SAFETY: the closure runs between fork and exec and makes only system calls.
    unsafe {
        command.pre_exec(move || {
            if from != at {
                libc::dup2(from, at);
            }
            libc::fcntl(at, libc::F_SETFD, 0);
            if tidy {
                libc::syscall(libc::SYS_close_range, 3_u32, (at - 1) as u32, 0_u32);
                libc::syscall(libc::SYS_close_range, (at + 1) as u32, u32::MAX, 0_u32);
            }
            Ok(())
        });
    }
    let out = command.output().expect("runs");
    drop(fd);
    out
}

#[cfg(target_os = "linux")]
fn identity_of(pki: &Pki, node: &Leaf) -> kbf_installer::fetch::identity::Identity {
    kbf_installer::fetch::identity::Identity {
        cert: node.cert.clone().into_bytes(),
        key: node.key.clone().into_bytes(),
        ca: pki.ca_pem().into_bytes(),
    }
}

/// A child lent an identity on descriptor 4 or any other number fetches over mutual TLS and
/// writes the bytes, no more than its limit.
///
/// Mutants seen red: the identity not presented; the limit not applied to the stream (the loop
/// condition `written < limit` removed).
#[cfg(target_os = "linux")]
#[test]
fn a_child_lent_an_identity_fetches_from_the_frontend() {
    let (pki, server) = pki_and_server();
    let bytes = body(50_000);
    let grpc = Grpc::start(&server, &pki.ca_pem(), Serve::Bytes(bytes.clone()));
    let id = identity_of(&pki, &pki.client());
    for (at, limit) in [(4, 50_001), (9, 20_500)] {
        let lent = Lent {
            id: &id,
            at,
            tidy: true,
        };
        let out = frontend_by_hand(&grpc.url(), &sha(&bytes), limit, &lent);
        assert!(out.status.success(), "fd {at}: {}", said(&out));
        let want = &bytes[..bytes.len().min(limit as usize)];
        assert_eq!(out.stdout, want, "fd {at}");
    }
    assert_eq!(*grpc.peers.lock().expect("lock"), [1, 1]);
}

/// A frontend that answers with an error, before or in the middle of the bytes, is a failed
/// fetch that names the status; one that is not there is unreachable.
#[cfg(target_os = "linux")]
#[test]
fn a_frontend_that_fails_is_a_failed_fetch() {
    let (pki, server) = pki_and_server();
    let id = identity_of(&pki, &pki.client());
    let lent = Lent {
        id: &id,
        at: 4,
        tidy: true,
    };
    let missing = Grpc::start(&server, &pki.ca_pem(), Serve::NotFound);
    let out = frontend_by_hand(&missing.url(), &sha(b"x"), 100, &lent);
    assert!(!out.status.success());
    assert!(said(&out).contains("NotFound"), "{}", said(&out));
    let broken = Grpc::start(&server, &pki.ca_pem(), Serve::Break { sent: 300 });
    let out = frontend_by_hand(&broken.url(), &sha(b"x"), 1000, &lent);
    assert!(!out.status.success());
    assert!(said(&out).contains("Internal"), "{}", said(&out));
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    };
    let out = frontend_by_hand(&format!("https://localhost:{port}"), &sha(b"x"), 100, &lent);
    assert!(!out.status.success());
    assert!(said(&out).contains("source unreachable"), "{}", said(&out));
}

/// What the child says when the descriptor it is told to adopt is not a lent identity: a
/// standard stream, one that is not open, a descriptor the parent left open besides it.
///
/// Mutant seen red: the check that the child holds nothing else removed.
#[cfg(target_os = "linux")]
#[test]
fn a_lent_descriptor_the_child_cannot_adopt_is_a_failed_start() {
    let (pki, _) = pki_and_server();
    let id = identity_of(&pki, &pki.client());
    let url = "https://localhost:1";
    let digest = sha(b"x");
    let own = |at: i32, tidy: bool| {
        let lent = Lent { id: &id, at, tidy };
        frontend_by_hand(url, &digest, 10, &lent)
    };
    // A standard stream is not a lent descriptor.
    let (ok, err) = run_child(&[
        "--kind",
        "frontend",
        "--url",
        url,
        "--sha256",
        &digest,
        "--limit",
        "10",
        "--identity-fd",
        "1",
    ]);
    assert!(!ok);
    assert!(err.contains("standard stream"), "{err}");
    // The parent left a descriptor open besides the lent one.
    let held = File::open("/").expect("a directory");
    // SAFETY: dup of a descriptor this test owns; dup clears close-on-exec, on purpose.
    let leaked = unsafe { libc::dup(std::os::fd::AsRawFd::as_raw_fd(&held)) };
    let out = own(11, false);
    // SAFETY: closing the descriptor made above.
    unsafe { libc::close(leaked) };
    assert!(!out.status.success());
    assert!(
        said(&out).contains("which nobody lent it"),
        "{}",
        said(&out)
    );
}

/// A frontend child refuses a roots file (it trusts the pinned CA only), a URL with a path, and
/// an identity descriptor that is not open.
#[cfg(target_os = "linux")]
#[test]
fn a_frontend_child_checks_what_it_was_given() {
    let (pki, _) = pki_and_server();
    let id = identity_of(&pki, &pki.client());
    let lent = Lent {
        id: &id,
        at: 4,
        tidy: true,
    };
    let out = frontend_by_hand("https://localhost:1/path", &sha(b"x"), 10, &lent);
    assert!(
        said(&out).contains("not a plain https URL"),
        "{}",
        said(&out)
    );
    let good = sha(b"x");
    let (ok, err) = run_child(&[
        "--kind",
        "frontend",
        "--url",
        "https://localhost:1",
        "--sha256",
        &good,
        "--limit",
        "10",
        "--identity-fd",
        "77",
        "--ca-file",
        "/nonexistent",
    ]);
    assert!(!ok);
    assert!(err.contains("pinned CA only"), "{err}");
    let (ok, err) = run_child(&[
        "--kind",
        "frontend",
        "--url",
        "https://localhost:1",
        "--sha256",
        &good,
        "--limit",
        "10",
        "--identity-fd",
        "77",
    ]);
    assert!(!ok);
    assert!(err.contains("Bad file descriptor"), "{err}");
}

// ------------------------------------------------------------------ describing a release
//
// A `frontend_update` order names a commit and nothing else: the child asks the source what the
// commit holds (`GET <base>/releases/<commit>.json`, or `ReleaseService.Describe`), decodes the
// answer, and the parent reads the small form it writes (`fetch::describe`).

/// What an honest source lists for `platform`.
fn entry(platform: &str) -> Described {
    Described {
        name: "kbf".to_owned(),
        platform: platform.to_owned(),
        tarball: format!("kbf-{COMMIT}-{platform}.tar.gz"),
        sha256: sha(platform.as_bytes()),
        size: 1000,
        bundle_sha256: sha(b"bundle"),
        bundle_size: 500,
    }
}

/// The JSON file of `docs/api.md` for `entries`, with keys the reader does not know.
fn json_file(schema: u64, commit: &str, entries: &[Described]) -> Vec<u8> {
    let components: Vec<_> = entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "name": e.name, "platform": e.platform, "tarball": e.tarball,
                "sha256": e.sha256, "size": e.size,
                "bundle_sha256": e.bundle_sha256, "bundle_size": e.bundle_size,
                "added_later": true,
            })
        })
        .collect();
    serde_json::json!({
        "schema": schema, "commit": commit, "components": components, "generated": "later",
    })
    .to_string()
    .into_bytes()
}

fn proto_of(commit: &str, entries: &[Described]) -> kbf_proto::update::ReleaseDescription {
    kbf_proto::update::ReleaseDescription {
        commit: commit.to_owned(),
        components: entries
            .iter()
            .map(|e| kbf_proto::update::ReleaseComponent {
                name: e.name.clone(),
                platform: e.platform.clone(),
                tarball: e.tarball.clone(),
                sha256: e.sha256.clone(),
                size: e.size,
                bundle_sha256: e.bundle_sha256.clone(),
                bundle_size: e.bundle_size,
            })
            .collect(),
    }
}

fn json_path() -> String {
    format!("/mirror/releases/{COMMIT}.json")
}

impl Rig {
    /// Ask `source` what `COMMIT` holds for `platform`, through the confined child.
    fn describe(
        &self,
        source: &Source,
        platform: ReleasePlatform,
    ) -> Result<Vec<Described>, FetchError> {
        let child = ChildProgram {
            program: PathBuf::from(BIN),
            run_as: None,
        };
        let limits = Limits {
            max_component_bytes: 1 << 20,
            fetch_timeout: LONG,
        };
        let commit = CommitId::new(COMMIT).expect("commit");
        describe_release(&child, source, limits, &commit, platform)
    }
}

/// The `url` source is described from `<base>/releases/<commit>.json`: the one request, the
/// entries of the host's platform, and the file's other platforms and keys the reader does not
/// know are no obstacle.
///
/// Rule: the description of a `url` source is the JSON of schema 1, whose reader "ignores keys it
/// does not know". Catches a URL assembled other than `releases/<commit>.json` under the base's
/// path, a reader that fails on an added key, and another platform's entry leaking into the
/// answer.
///
/// Mutants: `releases/` spelt `release/` in the child (red: 404); `deny_unknown_fields` on the
/// JSON types (red: refused).
#[test]
fn the_url_source_is_described_from_its_json() {
    let (pki, leaf) = pki_and_server();
    let file = json_file(1, COMMIT, &[entry("linux-arm64"), entry("linux-x86_64")]);
    let server = Https::start(&leaf, Plan::Files(vec![(json_path(), file)]));
    let rig = Rig::new("describe-url");
    let source = rig.url_source(&format!("{}/mirror", server.url()), &pki.ca_pem());
    let got = rig.describe(&source, ReleasePlatform::LinuxX86_64);
    assert_eq!(got.expect("described"), [entry("linux-x86_64")]);
    assert_eq!(server.hits(), [json_path()]);
}

/// A `url` source that answers `404` or `409` for the file (the route of `docs/api.md` does, for a
/// commit it holds nothing of or nothing complete of) has said there is nothing to describe: the
/// source's answer, naming the file, and not a failure of the child. Any other status is a
/// failed fetch of the child.
///
/// Catches a refusal of the source taken for a failure of the child, and a server error taken
/// for a refusal.
///
/// Mutant: the `404 | 409` arm of the child removed (red: both are failures of the child).
#[test]
fn a_url_source_that_has_no_such_release_says_so() {
    let (pki, leaf) = pki_and_server();
    let rig = Rig::new("describe-url-status");
    for status in [404, 409] {
        let server = Https::start(&leaf, Plan::Status(status));
        let source = rig.url_source(&format!("{}/mirror", server.url()), &pki.ca_pem());
        match rig.describe(&source, ReleasePlatform::LinuxX86_64) {
            Err(FetchError::Describe(DescribeFault::Refused(why))) => {
                assert!(
                    why.contains(&format!("{} answered {status}", json_path())),
                    "{why}"
                );
            }
            other => panic!("{status}: {other:?}"),
        }
    }
    let server = Https::start(&leaf, Plan::Status(500));
    let source = rig.url_source(&format!("{}/mirror", server.url()), &pki.ca_pem());
    match rig.describe(&source, ReleasePlatform::LinuxX86_64) {
        Err(FetchError::Pipe(PipeFault::Silent { log })) => {
            assert!(log.contains("the source answered 500"), "{log}");
        }
        other => panic!("{other:?}"),
    }
}

/// A file of another schema, one that is not JSON, one of another commit and one that lacks the
/// host's platform are each refused with what is wrong with them.
///
/// Catches a file of a schema this reader does not know being read as schema 1, and a mirror's
/// file for one release being taken for another.
#[test]
fn a_url_file_that_cannot_be_used_is_refused_with_the_reason() {
    let (pki, leaf) = pki_and_server();
    let mine = [entry("linux-x86_64")];
    let cases: Vec<(Vec<u8>, &str)> = vec![
        (json_file(2, COMMIT, &mine), "schema 2"),
        (b"<html>not json</html>".to_vec(), "no schema"),
        (json_file(1, &"b".repeat(40), &mine), "it describes commit"),
        (
            json_file(1, COMMIT, &[entry("linux-arm64")]),
            "it has no kbf for linux-x86_64",
        ),
    ];
    for (n, (file, said)) in cases.into_iter().enumerate() {
        let server = Https::start(&leaf, Plan::Files(vec![(json_path(), file)]));
        let rig = Rig::new(&format!("describe-url-bad-{n}"));
        let source = rig.url_source(&format!("{}/mirror", server.url()), &pki.ca_pem());
        match rig.describe(&source, ReleasePlatform::LinuxX86_64) {
            Err(FetchError::Describe(
                DescribeFault::Refused(why) | DescribeFault::Unusable(why),
            )) => assert!(why.contains(said), "{said}: {why}"),
            other => panic!("{said}: {other:?}"),
        }
    }
}

/// The frontend source is described over mutual TLS: the call carries the node's certificate,
/// names the commit and the host's platform, and the entries come back.
///
/// Catches the platform not being asked for (a frontend would describe every one), and a
/// describe sent without the identity.
///
/// Mutants: `platform` left empty in the child's request (red: asked for ""); the identity not
/// presented (red).
#[test]
fn the_frontend_source_is_described_over_mutual_tls() {
    let (pki, server) = pki_and_server();
    let said = proto_of(COMMIT, &[entry("linux-x86_64")]);
    let grpc = Grpc::describing(&server, &pki.ca_pem(), Describing::Of(said));
    let rig = Rig::new("describe-frontend");
    let source = rig.frontend(&grpc.url(), &pki.ca_pem(), &pki.client());
    let got = rig.describe(&source, ReleasePlatform::LinuxX86_64);
    assert_eq!(got.expect("described"), [entry("linux-x86_64")]);
    let asked = (COMMIT.to_owned(), "linux-x86_64".to_owned());
    assert_eq!(*grpc.described.lock().expect("lock"), [asked]);
    assert_eq!(*grpc.peers.lock().expect("lock"), [1]);
}

/// `NOT_FOUND` and `FAILED_PRECONDITION` from `Describe` are the answer that there is no
/// description to be had, and arrive with the server's words; every other status (the role
/// table's `PERMISSION_DENIED`, a store that fails, a server that does not describe) is a failed
/// fetch of the child.
///
/// Catches the two refusals taken for failures of the child, a failure taken for a refusal, and
/// the server's message (which names what is lacking) lost.
///
/// Mutant: `Code::FailedPrecondition` removed from the child's match (red).
#[test]
fn a_frontend_that_will_not_describe_is_a_refusal_or_a_failure_by_its_status() {
    use tonic::Code;
    let (pki, server) = pki_and_server();
    let rig = Rig::new("describe-frontend-status");
    let source = |grpc: &Grpc| rig.frontend(&grpc.url(), &pki.ca_pem(), &pki.client());
    let verdicts = [
        (
            Code::NotFound,
            "no tarball of the commit is stored",
            "NotFound",
        ),
        (
            Code::FailedPrecondition,
            "linux-x86_64 has no attestation of kbf",
            "FailedPrecondition",
        ),
    ];
    for (code, message, name) in verdicts {
        let grpc = Grpc::describing(&server, &pki.ca_pem(), Describing::Status(code, message));
        match rig.describe(&source(&grpc), ReleasePlatform::LinuxX86_64) {
            Err(FetchError::Describe(DescribeFault::Refused(why))) => {
                assert_eq!(why, format!("{name}: {message}"));
            }
            other => panic!("{name}: {other:?}"),
        }
    }
    let outages = [
        (Code::PermissionDenied, "PermissionDenied"),
        (Code::Internal, "Internal"),
        (Code::Unimplemented, "Unimplemented"),
    ];
    for (code, name) in outages {
        let grpc = Grpc::describing(&server, &pki.ca_pem(), Describing::Status(code, "no"));
        match rig.describe(&source(&grpc), ReleasePlatform::LinuxX86_64) {
            Err(FetchError::Pipe(PipeFault::Silent { log })) => {
                assert!(log.contains(name), "{name}: {log}");
            }
            other => panic!("{name}: {other:?}"),
        }
    }
}

/// The parent's other checks on the child's answer hold for the frontend as for the `url` file:
/// a description with a digest that is not hex, whatever else is right, is refused before any
/// file is named.
///
/// Catches the parent relying on the child (or the server) for the shape of a digest.
#[test]
fn a_description_with_a_bad_digest_is_refused_whoever_sent_it() {
    let (pki, server) = pki_and_server();
    let mut bad = entry("linux-x86_64");
    bad.sha256 = "ABC".to_owned();
    let said = proto_of(COMMIT, &[bad]);
    let grpc = Grpc::describing(&server, &pki.ca_pem(), Describing::Of(said));
    let rig = Rig::new("describe-frontend-bad-digest");
    let source = rig.frontend(&grpc.url(), &pki.ca_pem(), &pki.client());
    match rig.describe(&source, ReleasePlatform::LinuxX86_64) {
        Err(FetchError::Describe(DescribeFault::Unusable(why))) => {
            assert!(why.contains("64 lowercase hex"), "{why}");
        }
        other => panic!("{other:?}"),
    }
}

/// A server that sends a hostile name (a space and a newline in it) is refused by the child, in the
/// form, and the parent never sees the fields: the child cannot be made to write a second
/// component line.
///
/// Catches a description with whitespace in a field being relayed as extra fields or lines.
#[test]
fn a_hostile_field_cannot_become_a_second_line() {
    let (pki, server) = pki_and_server();
    let mut evil = entry("linux-x86_64");
    evil.name = "kbf\ncomponent kbf-daemon linux-x86_64".to_owned();
    let grpc = Grpc::describing(
        &server,
        &pki.ca_pem(),
        Describing::Of(proto_of(COMMIT, &[evil])),
    );
    let rig = Rig::new("describe-frontend-hostile");
    let source = rig.frontend(&grpc.url(), &pki.ca_pem(), &pki.client());
    match rig.describe(&source, ReleasePlatform::LinuxX86_64) {
        Err(FetchError::Describe(DescribeFault::Refused(why))) => {
            assert!(why.contains("not one printable word"), "{why}");
            assert!(!why.contains('\n'));
        }
        other => panic!("{other:?}"),
    }
}

// ----------------------------------------------- the describing child started by hand

/// The arguments of a url describe of `COMMIT` from `server`, trusting `ca`.
fn describe_args(url: &str, ca: &std::path::Path, limit: u64) -> Vec<String> {
    [
        "--kind",
        "url",
        "--url",
        url,
        "--describe",
        COMMIT,
        "--limit",
        &limit.to_string(),
        "--ca-file",
        ca.to_str().expect("utf-8"),
    ]
    .map(str::to_owned)
    .to_vec()
}

/// What the child writes for a good `url` file is the form, whole, for every platform the file
/// lists (the parent chooses), with the file's unknown keys gone.
///
/// Catches a field written in another place, and the child choosing among platforms (that is the
/// parent's to do, where the choice can be checked).
#[test]
fn the_child_writes_the_description_in_the_form() {
    let (pki, leaf) = pki_and_server();
    let file = json_file(1, COMMIT, &[entry("linux-arm64"), entry("linux-x86_64")]);
    let server = Https::start(
        &leaf,
        Plan::Files(vec![("/releases/".to_owned() + COMMIT + ".json", file)]),
    );
    let rig = Rig::new("hand-describe");
    let ca = rig.write("ca.pem", &pki.ca_pem());
    let out = by_hand(&describe_args(&server.url(), &ca, 16 * 1024 + 1));
    assert!(out.status.success(), "{}", said(&out));
    let line = |e: &Described| {
        format!(
            "component {} {} {} {} {} {} {}\n",
            e.name, e.platform, e.tarball, e.sha256, e.size, e.bundle_sha256, e.bundle_size
        )
    };
    let want = format!(
        "description 1 {COMMIT}\n{}{}",
        line(&entry("linux-arm64")),
        line(&entry("linux-x86_64"))
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout), want);
}

/// A file cut at the child's limit is not a description: it is read as far as the limit and
/// refused, with the child's words, as one line and success.
///
/// Mutant: `.take(args.limit)` removed from the body read (red: the whole file is read).
#[test]
fn a_file_longer_than_the_limit_is_read_only_that_far() {
    let (pki, leaf) = pki_and_server();
    let file = json_file(1, COMMIT, &[entry("linux-x86_64")]);
    let server = Https::start(
        &leaf,
        Plan::Files(vec![("/releases/".to_owned() + COMMIT + ".json", file)]),
    );
    let rig = Rig::new("hand-describe-cut");
    let ca = rig.write("ca.pem", &pki.ca_pem());
    // Shorter than the file, longer than the refusal it comes to.
    let out = by_hand(&describe_args(&server.url(), &ca, 250));
    assert!(out.status.success(), "{}", said(&out));
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        text.starts_with("refused the file has no schema: "),
        "{text}"
    );
    assert_eq!(text.matches('\n').count(), 1);
}

/// The `404` and `409` of a `url` source are written as refusals (and the child succeeds: it
/// did what it was asked, and the answer is no); a `500` is a failure with the status named.
///
/// Mutant: the `404 | 409` arm removed (red: both fail).
#[test]
fn the_child_writes_a_refusal_for_404_and_409_and_fails_for_the_rest() {
    let (pki, leaf) = pki_and_server();
    let rig = Rig::new("hand-describe-status");
    let ca = rig.write("ca.pem", &pki.ca_pem());
    for status in [404, 409] {
        let server = Https::start(&leaf, Plan::Status(status));
        let out = by_hand(&describe_args(&server.url(), &ca, 16 * 1024 + 1));
        assert!(out.status.success(), "{}", said(&out));
        let want = format!(
            "refused {}/releases/{COMMIT}.json answered {status}\n",
            server.url()
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout), want);
    }
    let server = Https::start(&leaf, Plan::Status(500));
    let out = by_hand(&describe_args(&server.url(), &ca, 16 * 1024 + 1));
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(
        said(&out).contains("the source answered 500"),
        "{}",
        said(&out)
    );
}

/// A `url` source that is not there is the same failed start as for a blob.
#[test]
fn a_url_source_that_is_not_there_fails_the_describing_child() {
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").expect("bind");
        l.local_addr().expect("addr").port()
    };
    let rig = Rig::new("hand-describe-gone");
    let ca = rig.write("ca.pem", &Pki::new().ca_pem());
    let out = by_hand(&describe_args(
        &format!("https://localhost:{port}"),
        &ca,
        100,
    ));
    assert!(!out.status.success());
    assert!(said(&out).contains("source unreachable"), "{}", said(&out));
}

/// A by-hand frontend describe: the child lent an identity on descriptor 4.
#[cfg(target_os = "linux")]
fn frontend_describes(url: &str, platform: Option<&str>, pki: &Pki) -> std::process::Output {
    let id = identity_of(pki, &pki.client());
    let lent = Lent {
        id: &id,
        at: 4,
        tidy: true,
    };
    let mut what = vec!["--describe", COMMIT];
    if let Some(platform) = platform {
        what.extend(["--platform", platform]);
    }
    frontend_asked(url, &what, 16 * 1024 + 1, &lent)
}

/// The frontend child writes what `Describe` answered, in the form, and asks for the platform it
/// was given (or for none).
///
/// Catches the platform dropped from the request, and a field of the message in another's place.
#[cfg(target_os = "linux")]
#[test]
fn the_frontend_child_writes_what_describe_answered() {
    let (pki, server) = pki_and_server();
    let said_by_server = proto_of(COMMIT, &[entry("linux-x86_64")]);
    let grpc = Grpc::describing(&server, &pki.ca_pem(), Describing::Of(said_by_server));
    for platform in [Some("linux-x86_64"), None] {
        let out = frontend_describes(&grpc.url(), platform, &pki);
        assert!(out.status.success(), "{}", said(&out));
        let e = entry("linux-x86_64");
        let want = format!(
            "description 1 {COMMIT}\ncomponent {} {} {} {} {} {} {}\n",
            e.name, e.platform, e.tarball, e.sha256, e.size, e.bundle_sha256, e.bundle_size
        );
        assert_eq!(String::from_utf8_lossy(&out.stdout), want);
    }
    let asked: Vec<String> = grpc
        .described
        .lock()
        .expect("lock")
        .iter()
        .map(|(commit, platform)| format!("{commit} {platform:?}"))
        .collect();
    assert_eq!(
        asked,
        [
            format!("{COMMIT} \"linux-x86_64\""),
            format!("{COMMIT} \"\"")
        ]
    );
}

/// `NOT_FOUND` and `FAILED_PRECONDITION` are written as refusals with the status named, and the
/// child succeeds; any other status fails it.
///
/// Mutant: either code removed from the match (red).
#[cfg(target_os = "linux")]
#[test]
fn the_frontend_child_writes_a_refusal_for_two_statuses() {
    use tonic::Code;
    let (pki, server) = pki_and_server();
    for (code, name) in [
        (Code::NotFound, "NotFound"),
        (Code::FailedPrecondition, "FailedPrecondition"),
    ] {
        let grpc = Grpc::describing(
            &server,
            &pki.ca_pem(),
            Describing::Status(code, "it is not there"),
        );
        let out = frontend_describes(&grpc.url(), Some("linux-arm64"), &pki);
        assert!(out.status.success(), "{}", said(&out));
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            format!("refused {name}: it is not there\n")
        );
    }
    let grpc = Grpc::describing(
        &server,
        &pki.ca_pem(),
        Describing::Status(Code::Internal, "it broke"),
    );
    let out = frontend_describes(&grpc.url(), Some("linux-arm64"), &pki);
    assert!(!out.status.success());
    assert!(out.stdout.is_empty());
    assert!(said(&out).contains("Internal: it broke"), "{}", said(&out));
}

// ------------------------------------------------------ the whole of it, on the real child

struct NoLeases;

impl LeaseSource for NoLeases {
    fn live(&self) -> Result<usize, String> {
        Ok(0)
    }
}

/// A `frontend_update` that names a commit and nothing else, on a `url` source, with the real child
/// and the real pipeline (only `gh` and the binary's own report are played): the file
/// `releases/<commit>.json` is read, the bundle and then the tarball are fetched from
/// `files/<digest>`, and the release is installed. Each request is the contract's: the
/// description once, each file once, by digest.
///
/// Catches the three pieces not agreeing with one another (the form the child writes, what the
/// fetcher reads, the layout of the `url` source), a bundle fetched under another name than its
/// digest, and a file fetched twice.
///
/// Mutant: the bundle fetched by the tarball's digest (red: 404 on a second tarball fetch).
#[test]
fn a_frontend_update_is_installed_from_a_url_source_end_to_end() {
    let (pki, leaf) = pki_and_server();
    let tar = tar_gz(&[file("kbf", &elf(0x3e, "payload"))]);
    let bundle = b"{\"mediaType\":\"application/vnd.dev.sigstore.bundle+json\"}\n".to_vec();
    let mut entry = entry("linux-x86_64");
    (entry.sha256, entry.size) = (sha(&tar), tar.len() as u64);
    (entry.bundle_sha256, entry.bundle_size) = (sha(&bundle), bundle.len() as u64);
    let server = Https::start(
        &leaf,
        Plan::Files(vec![
            (json_path(), json_file(1, COMMIT, &[entry.clone()])),
            (format!("/mirror/files/{}", sha(&tar)), tar.clone()),
            (format!("/mirror/files/{}", sha(&bundle)), bundle.clone()),
        ]),
    );
    let rig = Rig::new("whole-url");
    let source = rig.url_source(&format!("{}/mirror", server.url()), &pki.ca_pem());
    let (host, _) = host_root_on("fetch-e2e-whole-url-host", Platform::Linux);
    let ids = me();
    let mut settings = Settings::defaults(&host);
    settings.run_as = ids;
    settings.arch = Arch::X86_64;
    let probe = version_json(COMMIT, "release");
    let fetcher = RealFetcher {
        host: host.clone(),
        settings,
        policy: Policy {
            max_component_bytes: 1 << 20,
            max_binary_bytes: 1 << 20,
            min_protocol: 0,
            supported: Supported {
                protocol: 10,
                min_protocol: 10,
                state_schema: 10,
            },
            timeouts: Timeouts::DEFAULT,
        },
        trust: Box::new(trusted_host_on(host.platform())),
        runner: Box::new(FakeRunner::new(move |spawn| {
            if spawn.program.file_name().is_some_and(|n| n == "gh") {
                Ok(ran(0, "", ""))
            } else {
                Ok(ran(0, &probe, ""))
            }
        })),
        leases: Box::new(NoLeases),
        owner: kbf_host::step::Owner {
            uid: ids.uid,
            gid: ids.gid,
        },
        child: ChildProgram {
            program: PathBuf::from(BIN),
            run_as: None,
        },
        source,
        limits: Limits {
            max_component_bytes: 1 << 20,
            fetch_timeout: LONG,
        },
    };
    let locator = ReleaseLocator {
        commit: CommitId::new(COMMIT).expect("commit"),
        components: Vec::new(),
    };
    let got = fetcher.fetch(&locator).map_err(|e: Refused| e.to_string());
    let got = got.expect("installed");
    assert_eq!(
        (got.commit.as_str(), got.platform.as_str()),
        (COMMIT, "linux-x86_64")
    );
    let release = host.join(&host.release_dir(&locator.commit));
    assert!(release.join("kbf").is_file() && release.join("VERIFIED").is_file());
    assert_eq!(
        server.hits(),
        [
            json_path(),
            format!("/mirror/files/{}", sha(&bundle)),
            format!("/mirror/files/{}", sha(&tar)),
        ]
    );
}
