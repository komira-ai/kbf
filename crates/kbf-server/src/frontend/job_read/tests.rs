//! The job-facing read listener over real TLS on loopback: a client that trusts the farm CA
//! reads the inputs of an Action by IP address with a read grant, and the start refuses what
//! could not serve a job. The frontend's own wiring is the last tests: the same listener
//! reads through the blob path the REAPI listener uses, and is absent without its flags.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::os::unix::fs::PermissionsExt as _;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use bytes::Bytes;
use futures::StreamExt as _;
use futures::future::BoxFuture;
use kbf_front::grants::{GrantKey, Grants};
use kbf_front::job_read::{BlobSource, Limits};
use kbf_objstore::s3::fake::FakeS3;
use kbf_proto::reapi::{Action, Directory, DirectoryNode, FileNode};
use kbf_storage::{ReadStream, StorageError};
use kbf_types::Digest;
use prost::Message as _;
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType,
};
use rustls::RootCertStore;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, ServerName};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;
use tokio_rustls::TlsConnector;

use super::{JobReadFlags, JobTls, Prepared, Serving, Timeouts};
use crate::frontend::pack_tests::{flags_of, index, pack_flags, scrape, started, upload};
use crate::frontend::{command, parse};
use crate::listen::Binder;
use crate::test_dir::scratch_dir;

const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef-the-farm-secret";
const BUILD: &str = "build-0007";

struct Ca {
    issuer: CertifiedIssuer<'static, KeyPair>,
    pem: String,
}

fn ca(name: &str) -> Ca {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.distinguished_name.push(DnType::CommonName, name);
    params.key_usages = vec![
        KeyUsagePurpose::KeyCertSign,
        KeyUsagePurpose::DigitalSignature,
    ];
    let issuer = CertifiedIssuer::self_signed(params, KeyPair::generate().expect("CA key"))
        .expect("CA certificate");
    let pem = issuer.pem();
    Ca { issuer, pem }
}

fn ip(text: &str) -> SanType {
    SanType::IpAddress(text.parse::<IpAddr>().expect("an address"))
}

fn dns(name: &str) -> SanType {
    SanType::DnsName(name.try_into().expect("a DNS name"))
}

/// The files of one listener in a directory: a leaf of `ca` with exactly the names `sans`, its
/// key, and the grant secret.
struct Files {
    cert: std::path::PathBuf,
    key: std::path::PathBuf,
    secret: std::path::PathBuf,
}

fn files(dir: &Path, ca: &Ca, sans: Vec<SanType>) -> Files {
    let mut params = CertificateParams::new(Vec::<String>::new()).expect("leaf params");
    params.subject_alt_names = sans;
    params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let key = KeyPair::generate().expect("leaf key");
    let cert = params.signed_by(&key, &ca.issuer).expect("sign leaf");
    std::fs::create_dir_all(dir).expect("the directory");
    let files = Files {
        cert: dir.join("job.pem"),
        key: dir.join("job.key"),
        secret: dir.join("grant.secret"),
    };
    std::fs::write(&files.cert, cert.pem()).expect("certificate");
    std::fs::write(&files.key, key.serialize_pem()).expect("key");
    std::fs::write(&files.secret, [SECRET, b"\n".as_slice()].concat()).expect("secret");
    std::fs::set_permissions(&files.secret, std::fs::Permissions::from_mode(0o600)).expect("mode");
    files
}

fn flags(files: &Files, listen: &str) -> JobReadFlags {
    JobReadFlags {
        listen: listen.parse().expect("an address"),
        tls: Some(JobTls {
            cert: files.cert.clone(),
            key: files.key.clone(),
        }),
        grant_secret_file: files.secret.clone(),
        max_blob_bytes: Limits::default().max_blob_bytes,
        reads_in_flight: 32,
        read_budget_bytes: Limits::default().read_budget_bytes,
        read_idle_secs: Limits::default().held_idle.as_secs(),
        store: None,
    }
}

/// The TLS flags of `files`, changed by `change`.
fn tls_of(files: &Files, change: impl FnOnce(&mut JobTls)) -> Option<JobTls> {
    let mut tls = JobTls {
        cert: files.cert.clone(),
        key: files.key.clone(),
    };
    change(&mut tls);
    Some(tls)
}

fn prepare(flags: &JobReadFlags) -> Result<Prepared, super::JobReadError> {
    Prepared::new(flags, Duration::from_secs(30), &Binder::new())
}

/// Why `flags` are refused.
fn refusal(flags: &JobReadFlags) -> String {
    prepare(flags).err().expect("refused").to_string()
}

/// A source over a map of blobs.
struct Blobs(HashMap<Digest, Bytes>);

impl BlobSource for Blobs {
    fn read_blob<'a>(&'a self, digest: &'a Digest) -> BoxFuture<'a, Result<Bytes, StorageError>> {
        let found = self.0.get(digest).cloned();
        Box::pin(async move { found.ok_or(StorageError::NotFound(*digest)) })
    }

    fn read_stream<'a>(
        &'a self,
        digest: &'a Digest,
    ) -> BoxFuture<'a, Result<ReadStream, StorageError>> {
        let found = self.0.get(digest).cloned();
        Box::pin(async move {
            let blob = found.ok_or(StorageError::NotFound(*digest))?;
            let chunks: Vec<Result<Bytes, StorageError>> = blob
                .chunks(STREAM_CHUNK)
                .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
                .collect();
            Ok(futures::stream::iter(chunks).boxed())
        })
    }
}

/// The size of the chunks a test source streams a blob in.
const STREAM_CHUNK: usize = 1024;

/// One Action with one input file, and a blob that is in no scope.
struct World {
    blobs: Vec<(Digest, Bytes)>,
    action: Digest,
    input: Digest,
    input_bytes: Bytes,
    stranger: Digest,
}

fn proto(digest: &Digest) -> kbf_proto::reapi::Digest {
    kbf_front::digest_to_proto(digest)
}

fn world() -> World {
    let mut blobs = Vec::new();
    let mut keep = |bytes: Vec<u8>| {
        let bytes = Bytes::from(bytes);
        let digest = kbf_segments::sha256(&bytes);
        blobs.push((digest, bytes));
        digest
    };
    let input = keep(b"the bytes of one input file".to_vec());
    let sub = Directory {
        files: vec![FileNode {
            name: "input".to_owned(),
            digest: Some(proto(&input)),
            ..FileNode::default()
        }],
        ..Directory::default()
    };
    let sub = keep(sub.encode_to_vec());
    let root = Directory {
        directories: vec![DirectoryNode {
            name: "dir".to_owned(),
            digest: Some(proto(&sub)),
        }],
        ..Directory::default()
    };
    let root = keep(root.encode_to_vec());
    let command = keep(b"the command".to_vec());
    let action = Action {
        command_digest: Some(proto(&command)),
        input_root_digest: Some(proto(&root)),
        ..Action::default()
    };
    let action = keep(action.encode_to_vec());
    let stranger = keep(b"another tenant's bytes".to_vec());
    let input_bytes = blobs[0].1.clone();
    World {
        blobs,
        action,
        input,
        input_bytes,
        stranger,
    }
}

impl World {
    fn source(&self) -> Arc<Blobs> {
        Arc::new(Blobs(self.blobs.iter().cloned().collect()))
    }

    /// A grant for the Action, valid for an hour.
    fn token(&self) -> String {
        let key = GrantKey::from_secret(SECRET).expect("the secret is long enough");
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("after 1970")
            .as_secs();
        Grants::system(key)
            .issue(BUILD, self.action, now + 3_600)
            .expect("a valid build id")
            .expose()
            .to_owned()
    }
}

/// What a client got.
struct Got {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Got {
    fn header(&self, name: &str) -> Option<&str> {
        let found = self.headers.iter().find(|(key, _)| key == name);
        found.map(|(_, value)| value.as_str())
    }

    fn text(&self) -> &str {
        std::str::from_utf8(&self.body).expect("text")
    }
}

fn client(trust: &str) -> TlsConnector {
    let mut roots = RootCertStore::empty();
    for der in CertificateDer::pem_slice_iter(trust.as_bytes()) {
        roots.add(der.expect("CA PEM")).expect("add root");
    }
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let config = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("protocol versions")
        .with_root_certificates(roots)
        .with_no_client_auth();
    TlsConnector::from(Arc::new(config))
}

/// `method` `path` at `addr`, dialled by its IP address, trusting the CAs of `trust`, with the
/// bearer `token` if there is one.
async fn request(
    addr: SocketAddr,
    trust: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
) -> std::io::Result<Got> {
    send(addr, trust, method, path, token, "", &[]).await
}

/// [`request`] with the header lines `extra` (each ending in CRLF) and then `body` after the
/// headers.
async fn send(
    addr: SocketAddr,
    trust: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    extra: &str,
    body: &[u8],
) -> std::io::Result<Got> {
    let tcp = TcpStream::connect(addr).await?;
    let name = ServerName::IpAddress(addr.ip().into());
    let mut tls = client(trust).connect(name, tcp).await?;
    exchange(&mut tls, addr, method, path, token, extra, body).await
}

/// One request and its answer over `io`, a connection to `addr` that is already secured, or
/// plain.
async fn exchange<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    tls: &mut S,
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    extra: &str,
    body: &[u8],
) -> std::io::Result<Got> {
    let auth = token
        .map(|token| format!("Authorization: Bearer {token}\r\n"))
        .unwrap_or_default();
    let head = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\n{auth}{extra}Connection: close\r\n\r\n"
    );
    tls.write_all(head.as_bytes()).await?;
    // A server that answers without reading the body may have closed by now.
    let _ = tls.write_all(body).await;
    let mut raw = Vec::new();
    // The server closes after its answer; whether it says so first does not matter.
    let _ = tls.read_to_end(&mut raw).await;
    // A server that aborts a response it has not flushed leaves no head at all.
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::UnexpectedEof))?;
    let head = std::str::from_utf8(&raw[..split]).expect("a text head");
    let mut lines = head.lines();
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse().ok())
        .expect("a status line");
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(key, value)| (key.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    Ok(Got {
        status,
        headers,
        body: raw[split + 4..].to_vec(),
    })
}

fn path_of(digest: &Digest) -> String {
    format!("/{digest}")
}

/// A listener on a loopback port over `source`, serving until the test ends.
fn listening(
    files: &Files,
    source: Arc<Blobs>,
    timeouts: Timeouts,
) -> (SocketAddr, crate::listen::Latch) {
    let prepared = prepare(&flags(files, "127.0.0.1:0")).expect("prepared");
    let addr = prepared.local();
    let Serving { latch, task, .. } = prepared.start(source, timeouts).expect("started");
    tokio::spawn(task);
    (addr, latch)
}

/// Catches: a listener that serves a wrong length or wrong bytes, one that wants a client
/// certificate (a job holds none), one that cannot be dialled by IP address with the farm CA
/// as the only trust, and one that answers with a content length missing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_trusting_the_farm_ca_reads_an_input_by_ip_address_with_its_grant() {
    let dir = scratch_dir("job-read-ok");
    let farm = ca("farm CA");
    let files = files(dir.path(), &farm, vec![ip("127.0.0.1")]);
    let world = world();
    let (addr, _latch) = listening(&files, world.source(), Timeouts::default());
    let got = request(
        addr,
        &farm.pem,
        "GET",
        &path_of(&world.input),
        Some(&world.token()),
    )
    .await
    .expect("connects");
    assert_eq!(got.status, 200, "{}", got.text());
    assert_eq!(got.body, world.input_bytes);
    let length = world.input_bytes.len().to_string();
    assert_eq!(got.header("content-length"), Some(length.as_str()));
    assert_eq!(got.header("content-type"), Some("application/octet-stream"));
}

/// Catches: a refusal that does not tell a client how to ask again (`WWW-Authenticate` on 401,
/// `Allow` on 405), one that says more than its reason code, and a route or a blob outside the
/// grant being served over the wire.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn refusals_carry_their_codes_and_headers_over_tls() {
    let dir = scratch_dir("job-read-refusals");
    let farm = ca("farm CA");
    let files = files(dir.path(), &farm, vec![ip("127.0.0.1")]);
    let world = world();
    let (addr, _latch) = listening(&files, world.source(), Timeouts::default());
    let token = world.token();
    let ask = |method: &'static str, path: String, token: Option<String>| {
        let pem = farm.pem.clone();
        async move {
            request(addr, &pem, method, &path, token.as_deref())
                .await
                .expect("connects")
        }
    };

    let got = ask("GET", path_of(&world.input), None).await;
    assert_eq!(got.status, 401);
    assert_eq!(got.text(), "missing_token\n");
    assert_eq!(got.header("www-authenticate"), Some("Bearer"));
    assert_eq!(
        got.header("content-type"),
        Some("text/plain; charset=utf-8")
    );

    let got = ask("GET", path_of(&world.stranger), Some(token.clone())).await;
    assert_eq!(got.status, 403);
    assert_eq!(got.text(), "out_of_scope\n");

    let got = ask("POST", path_of(&world.input), Some(token.clone())).await;
    assert_eq!(got.status, 405);
    assert_eq!(got.header("allow"), Some("GET"));

    let got = ask("GET", "/metrics".to_owned(), Some(token.clone())).await;
    assert_eq!(got.status, 404);
    let got = ask("GET", format!("{}?x=1", path_of(&world.input)), Some(token)).await;
    assert_eq!(got.status, 200, "a query string is not part of the path");
}

/// Catches: a listener that a client trusting another CA can connect to (the farm CA is the
/// only trust of a job), and one that serves a client that dials a name the certificate lacks.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_does_not_trust_the_certificate_cannot_connect() {
    let dir = scratch_dir("job-read-trust");
    let (farm, other) = (ca("farm CA"), ca("another CA"));
    let files = files(dir.path(), &farm, vec![ip("127.0.0.1")]);
    let world = world();
    let (addr, _latch) = listening(&files, world.source(), Timeouts::default());
    let refused = request(addr, &other.pem, "GET", "/x", None).await;
    assert!(refused.is_err());
    let tcp = TcpStream::connect(addr).await.expect("tcp");
    let by_name = ServerName::try_from("localhost").expect("a name");
    let refused = client(&farm.pem).connect(by_name, tcp).await;
    assert!(refused.is_err(), "the certificate names an address only");
}

/// Catches: a client that sends nothing holding a connection (a job is untrusted code and a
/// connection takes a permit): the server ends the connection after the header timeout.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_client_that_sends_no_request_is_dropped() {
    let dir = scratch_dir("job-read-silent");
    let farm = ca("farm CA");
    let files = files(dir.path(), &farm, vec![ip("127.0.0.1")]);
    let timeouts = Timeouts {
        header: Duration::from_millis(200),
        connection: Duration::from_secs(60),
    };
    let (addr, _latch) = listening(&files, world().source(), timeouts);
    let tcp = TcpStream::connect(addr).await.expect("tcp");
    let name = ServerName::IpAddress(addr.ip().into());
    let mut tls = client(&farm.pem)
        .connect(name, tcp)
        .await
        .expect("handshake");
    let mut sink = Vec::new();
    let ended = tokio::time::timeout(Duration::from_secs(20), tls.read_to_end(&mut sink)).await;
    assert!(ended.is_ok(), "the connection is still open");
}

/// Catches: a connection that outlives its lifetime while it is busy answering (a client that
/// asks again and again on one connection): the server ends it at the lifetime.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_connection_ends_at_its_lifetime() {
    let dir = scratch_dir("job-read-lifetime");
    let farm = ca("farm CA");
    let files = files(dir.path(), &farm, vec![ip("127.0.0.1")]);
    let timeouts = Timeouts {
        header: Duration::from_secs(60),
        connection: Duration::from_millis(300),
    };
    let (addr, _latch) = listening(&files, world().source(), timeouts);
    let tcp = TcpStream::connect(addr).await.expect("tcp");
    let name = ServerName::IpAddress(addr.ip().into());
    let mut tls = client(&farm.pem)
        .connect(name, tcp)
        .await
        .expect("handshake");
    let first = b"GET /x HTTP/1.1\r\nHost: kbf\r\n\r\n";
    tls.write_all(first).await.expect("sent");
    let mut sink = Vec::new();
    let ended = tokio::time::timeout(Duration::from_secs(20), tls.read_to_end(&mut sink)).await;
    assert!(ended.is_ok(), "the connection is still open");
    assert!(
        String::from_utf8_lossy(&sink).starts_with("HTTP/1.1 404"),
        "it answered before it ended"
    );
}

/// Catches: a socket left open once the drain closes its latch (connections would queue for
/// nobody), and a listener that cannot be told to stop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn closing_the_latch_closes_the_socket() {
    let dir = scratch_dir("job-read-latch");
    let farm = ca("farm CA");
    let files = files(dir.path(), &farm, vec![ip("127.0.0.1")]);
    let (addr, latch) = listening(&files, world().source(), Timeouts::default());
    assert!(TcpStream::connect(addr).await.is_ok());
    latch.close();
    let mut refused = false;
    for _ in 0..200 {
        if TcpStream::connect(addr).await.is_err() {
            refused = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(refused, "the socket is still accepting");
}

/// Catches: a start that accepts a certificate a job cannot use: one with no IP name (a job
/// dials an IP address), one that names another address than the listener's, one that is not
/// a certificate, a key that is not the certificate's, a missing file.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_start_refuses_a_certificate_that_does_not_name_the_address() {
    let dir = scratch_dir("job-read-names");
    let farm = ca("farm CA");
    let named_by_dns = files(
        &dir.path().join("dns"),
        &farm,
        vec![dns("frontend.farm.example")],
    );
    let wrong_ip = files(&dir.path().join("wrong"), &farm, vec![ip("192.0.2.7")]);
    let right_ip = files(&dir.path().join("right"), &farm, vec![ip("127.0.0.1")]);
    let listen = "127.0.0.1:0";
    let why = refusal(&flags(&named_by_dns, listen));
    assert!(why.contains("no IP subjectAltName for 127.0.0.1"), "{why}");
    assert!(why.contains("frontend.farm.example"), "{why}");
    let why = refusal(&flags(&wrong_ip, listen));
    assert!(why.contains("192.0.2.7"), "{why}");
    assert!(prepare(&flags(&right_ip, listen)).is_ok());

    // An unspecified address needs some IP name, any.
    assert!(prepare(&flags(&wrong_ip, "0.0.0.0:0")).is_ok());
    let why = refusal(&flags(&named_by_dns, "0.0.0.0:0"));
    assert!(why.contains("no IP subjectAltName for 0.0.0.0"), "{why}");

    let mut broken = flags(&right_ip, listen);
    broken.tls = tls_of(&right_ip, |tls| tls.cert = dir.path().join("absent.pem"));
    assert!(refusal(&broken).contains("absent.pem"));
    std::fs::write(dir.path().join("junk.pem"), "not a certificate").expect("junk");
    broken.tls = tls_of(&right_ip, |tls| tls.cert = dir.path().join("junk.pem"));
    assert!(refusal(&broken).contains("no PEM certificate"));
    let mut mismatched = flags(&right_ip, listen);
    mismatched.tls = tls_of(&right_ip, |tls| tls.key = wrong_ip.key.clone());
    assert!(refusal(&mismatched).contains("--job-tls-key"));
}

/// Catches: a grant secret that anyone can read, one too short to sign with, one past the file
/// limit read in part, and an address that is taken starting anyway.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_start_refuses_a_bad_secret_and_a_busy_address() {
    let dir = scratch_dir("job-read-secret");
    let farm = ca("farm CA");
    let files = files(dir.path(), &farm, vec![ip("127.0.0.1")]);
    let listen = "127.0.0.1:0";
    let secret = |bytes: &[u8], mode: u32| {
        std::fs::write(&files.secret, bytes).expect("secret");
        std::fs::set_permissions(&files.secret, std::fs::Permissions::from_mode(mode))
            .expect("mode");
        refusal(&flags(&files, listen))
    };
    assert!(secret(SECRET, 0o644).contains("mode 0644"));
    assert!(secret(b"too short", 0o600).contains("at least 32"));
    assert!(secret(&[b'x'; 5_000], 0o600).contains("--job-grant-secret-file"));
    std::fs::write(&files.secret, SECRET).expect("secret");
    std::fs::set_permissions(&files.secret, std::fs::Permissions::from_mode(0o400)).expect("mode");
    assert!(
        prepare(&flags(&files, listen)).is_ok(),
        "a 0400 secret is read"
    );

    let taken = std::net::TcpListener::bind("127.0.0.1:0").expect("a port");
    let busy = taken.local_addr().expect("its address").to_string();
    assert!(refusal(&flags(&files, &busy)).contains("127.0.0.1"));
}

/// Catches: the four flags being taken apart (a listener with no certificate), the listener
/// being on by default, and the flags not reaching the frontend's parsed flags.
#[test]
fn the_flags_come_together_and_are_off_by_default() {
    let base = [
        "frontend",
        "--storage=http://127.0.0.1:1",
        "--listen=127.0.0.1:0",
        "--worker-listen=127.0.0.1:0",
        "--api-listen=127.0.0.1:0",
    ];
    let all = [
        "--job-listen=127.0.0.1:8990",
        "--job-tls-cert=/etc/kbf/job.pem",
        "--job-tls-key=/etc/kbf/job.key",
        "--job-grant-secret-file=/etc/kbf/grant.secret",
    ];
    let with = |flags: &[&str]| {
        let argv = base.iter().chain(flags.iter()).copied();
        command().try_get_matches_from(argv).map(|m| parse(&m))
    };
    let none = with(&[]).expect("matches").expect("flags");
    assert!(none.job_read.is_none());
    let flags = with(&all).expect("matches").expect("flags");
    let job = flags.job_read.expect("the flags are read");
    assert_eq!(job.listen, "127.0.0.1:8990".parse().expect("address"));
    let tls = job.tls.expect("TLS");
    assert_eq!(tls.cert, Path::new("/etc/kbf/job.pem"));
    assert_eq!(tls.key, Path::new("/etc/kbf/job.key"));
    assert_eq!(job.grant_secret_file, Path::new("/etc/kbf/grant.secret"));
    for (i, left_out) in all.iter().enumerate() {
        let rest: Vec<&str> = all.iter().filter(|f| f != &left_out).copied().collect();
        assert!(with(&rest).is_err(), "without {left_out} ({i})");
    }
}

/// Catches: the listener not wired to the frontend's own blob path (it is the REAPI listener's
/// ByteStream read, through the pack index), its counters missing from `/metrics`, and its
/// certificate files missing from the TLS reload counters. A blob a client uploaded in a
/// pack is read by the grant of an Action that names it, over TLS, from a frontend whose storage
/// node is not there.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_serves_what_its_clients_uploaded_to_the_job_listener() {
    let s3 = FakeS3::start().await;
    let dir = scratch_dir("job-read-frontend");
    let farm = ca("farm CA");
    let files = files(dir.path(), &farm, vec![ip("127.0.0.1")]);
    let mut more = pack_flags(s3.endpoint(), dir.path());
    more.extend([
        "--job-listen=127.0.0.1:0".to_owned(),
        format!("--job-tls-cert={}", files.cert.display()),
        format!("--job-tls-key={}", files.key.display()),
        format!("--job-grant-secret-file={}", files.secret.display()),
    ]);
    let handle = started(&flags_of(&more), Some(index())).expect("starts");
    let job = handle.job_read.expect("the job listener is bound");
    let (front, api) = (handle.reapi, handle.api.expect("the operator API"));
    let served = tokio::spawn(handle.serve());
    let world = world();
    let blobs: Vec<(Digest, &[u8])> = world.blobs.iter().map(|(d, b)| (*d, &b[..])).collect();
    assert!(upload(front, &blobs).await.iter().all(|code| *code == 0));

    let got = request(
        job,
        &farm.pem,
        "GET",
        &path_of(&world.input),
        Some(&world.token()),
    )
    .await
    .expect("connects");
    assert_eq!(got.status, 200, "{}", got.text());
    assert_eq!(got.body, world.input_bytes);
    let got = request(
        job,
        &farm.pem,
        "GET",
        &path_of(&world.stranger),
        Some(&world.token()),
    )
    .await
    .expect("connects");
    assert_eq!(got.status, 403);

    let text = scrape(api).await;
    let ok = "kbf_job_read_requests_total{outcome=\"ok\"} 1\n";
    assert!(text.contains(ok), "{text}");
    let bytes = format!("kbf_job_read_bytes_total {}\n", world.input_bytes.len());
    assert!(text.contains(&bytes), "{text}");
    assert!(text.contains("listener=\"job\""), "{text}");
    served.abort();
}

/// Catches: a frontend with no job flags opening a listener, or counting one: its handle has no
/// job address and its metrics no job family.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_without_the_job_flags_has_no_job_listener() {
    let handle = started(&flags_of(&[]), Some(index())).expect("starts");
    assert!(handle.job_read.is_none());
    let api = handle.api.expect("the operator API");
    let served = tokio::spawn(handle.serve());
    let text = scrape(api).await;
    assert!(!text.contains("kbf_job_read"), "{text}");
    assert!(!text.contains("listener=\"job\""), "{text}");
    served.abort();
}

mod batch;
mod held;
mod locate;
mod plain;
mod store_grant;
mod stream;
