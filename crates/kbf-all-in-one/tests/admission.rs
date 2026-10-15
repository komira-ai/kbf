//! The deny list over both data planes: `kbf_cas_conformance::a_denied_caller_is_refused`
//! against the transfer engine (the remote plane, over mutual TLS to the worker
//! listener of a server started as `kbf-server` starts) and against `LocalDataPlane`
//! (calls into the same kind of server), each with the daemon denied by its node and by
//! its certificate's public key. A denial must refuse the same calls on both planes.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use clap::Parser as _;
use kbf_all_in_one::LocalDataPlane;
use kbf_cas_conformance::{Refusable, Subject, a_denied_caller_is_refused};
use kbf_transfer::{TransferConfig, Transfers};
use rcgen::{CertificateParams, CertifiedIssuer, IsCa, KeyPair};
use sha2::Digest as _;
use tonic::transport::{Certificate, ClientTlsConfig, Endpoint, Identity};

/// What the deny list names the daemon by.
#[derive(Clone, Copy)]
enum By {
    Node,
    PublicKey,
}

/// One test's PKI, deny list and servers.
struct Farm {
    dir: PathBuf,
    by: By,
}

/// Farms made by this process so far: each test has a directory of its own.
static MADE: AtomicU64 = AtomicU64::new(0);

/// Servers started by this process so far.
static STARTED: AtomicU64 = AtomicU64::new(0);

impl Farm {
    fn new(by: By) -> Self {
        let n = MADE.fetch_add(1, Ordering::SeqCst);
        let dir = Path::new(env!("CARGO_TARGET_TMPDIR"))
            .join("kbf-all-in-one-admission")
            .join(format!("{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("scratch");
        pki(&dir);
        std::fs::write(dir.join("deny"), "# nothing denied yet\n").expect("deny list");
        Self { dir, by }
    }

    fn at(&self, name: &str) -> String {
        self.dir.join(name).display().to_string()
    }

    /// A server with the worker listener under mutual TLS and this farm's deny list,
    /// serving for as long as the test runs.
    async fn server(&self) -> kbf_server::ServerHandle {
        // A store and a metadata log of its own for each server this farm starts.
        let n = STARTED.fetch_add(1, Ordering::SeqCst);
        let args = kbf_server::Args::try_parse_from([
            "kbf-server".to_owned(),
            "--listen=127.0.0.1:0".to_owned(),
            "--worker-listen=127.0.0.1:0".to_owned(),
            format!("--cold-store=file://{}", self.at(&format!("cold-{n}"))),
            format!("--meta-dir={}", self.at(&format!("meta-{n}"))),
            format!("--worker-tls-cert={}", self.at("server.pem")),
            format!("--worker-tls-key={}", self.at("server.key")),
            format!("--worker-client-ca={}", self.at("ca.pem")),
            format!("--worker-deny-list={}", self.at("deny")),
        ])
        .expect("flags");
        let never = || Ok(std::future::pending::<()>());
        kbf_server::start(&args, never).await.expect("start")
    }

    /// Puts the daemon on the deny list, by renaming a new list over the old one.
    fn deny(&self) {
        let entry = match self.by {
            By::Node => "node node-1".to_owned(),
            By::PublicKey => format!("spki-sha256 {}", spki_sha256(&self.dir.join("node.pem"))),
        };
        let next = self.dir.join("deny.next");
        std::fs::write(&next, format!("{entry}\n")).expect("write");
        std::fs::rename(&next, self.dir.join("deny")).expect("rename");
    }
}

/// A CA, the node's certificate (`node-1`, its one DNS name) and the server's
/// (`localhost`), written into `dir`.
fn pki(dir: &Path) {
    let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
    ca.is_ca = IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let ca = CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("key")).expect("CA");
    std::fs::write(dir.join("ca.pem"), ca.pem()).expect("write");
    for (name, dns) in [("node", "node-1"), ("server", "localhost")] {
        let key = KeyPair::generate().expect("key");
        let cert = CertificateParams::new(vec![dns.to_owned()])
            .expect("params")
            .signed_by(&key, &ca)
            .expect("sign");
        std::fs::write(dir.join(format!("{name}.pem")), cert.pem()).expect("write");
        std::fs::write(dir.join(format!("{name}.key")), key.serialize_pem()).expect("write");
    }
}

/// SHA-256 of the certificate's DER SubjectPublicKeyInfo, hex: what a deny list's
/// `spki-sha256` entry names.
fn spki_sha256(pem: &Path) -> String {
    let pem = std::fs::read(pem).expect("read");
    let (_, pem) = x509_parser::pem::parse_x509_pem(&pem).expect("PEM");
    let cert = pem.parse_x509().expect("certificate");
    let digest = sha2::Sha256::digest(cert.tbs_certificate.subject_pki.raw);
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

/// The in-process plane, admitted as the daemon whose certificate is `node.pem`.
struct Local(Farm);

impl Subject for Local {
    type Cas = LocalDataPlane;

    async fn fresh(&self) -> Arc<LocalDataPlane> {
        let server = self.0.server().await;
        let pem = std::fs::read(self.0.dir.join("node.pem")).expect("read");
        let caller = kbf_server::Caller::from_pem(&pem).expect("caller");
        let plane = LocalDataPlane::new(server.local(caller), tokio::runtime::Handle::current());
        tokio::spawn(server.serve());
        Arc::new(plane)
    }

    fn scratch(&self) -> &Path {
        &self.0.dir
    }
}

impl Refusable for Local {
    async fn deny(&self) {
        self.0.deny();
    }
}

/// The transfer engine over mutual TLS to the worker listener, as the daemon whose
/// certificate is `node.pem`.
struct Remote(Farm);

impl Subject for Remote {
    type Cas = Transfers;

    async fn fresh(&self) -> Arc<Transfers> {
        let server = self.0.server().await;
        let worker = server.worker;
        tokio::spawn(server.serve());
        let read = |name: &str| std::fs::read(self.0.dir.join(name)).expect("read");
        let tls = ClientTlsConfig::new()
            .ca_certificate(Certificate::from_pem(read("ca.pem")))
            .identity(Identity::from_pem(read("node.pem"), read("node.key")))
            .domain_name("localhost");
        let endpoint = Endpoint::from_shared(format!("https://{worker}"))
            .expect("endpoint")
            .tls_config(tls)
            .expect("TLS");
        Arc::new(Transfers::new(endpoint, TransferConfig::default()))
    }

    fn scratch(&self) -> &Path {
        &self.0.dir
    }
}

impl Refusable for Remote {
    async fn deny(&self) {
        self.0.deny();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_a_denied_node_is_refused() {
    a_denied_caller_is_refused(&Local(Farm::new(By::Node))).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_a_denied_public_key_is_refused() {
    a_denied_caller_is_refused(&Local(Farm::new(By::PublicKey))).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_a_denied_node_is_refused() {
    a_denied_caller_is_refused(&Remote(Farm::new(By::Node))).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_a_denied_public_key_is_refused() {
    a_denied_caller_is_refused(&Remote(Farm::new(By::PublicKey))).await;
}
