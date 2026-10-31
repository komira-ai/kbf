//! A farm CA, the certificates it issues, a client for the two listeners, and a running
//! coordinator on loopback.

#![allow(dead_code)]

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use base64::Engine as _;
use kbf_ca::{Authority, FileSigner, NodeCert, Policy, PublicKey, SystemEntropy, UpdaterCert};
use kbf_coordinator::config::{Bound, Parts, run};
use kbf_coordinator::desired::DesiredStore;
use kbf_coordinator::service::Coordinator;
use kbf_oidc::testkit::{STATIC_ISSUER, good_claims, keys, mint, static_verifier};
use kbf_testkit::dir::TestDir;
use kbf_tls::TlsSource;
use kbf_types::{HostRole, Role};
use rcgen::{KeyPair, PublicKeyData};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName};
use rustls::{ClientConfig, RootCertStore};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::task::JoinHandle;
use tokio_rustls::TlsConnector;

pub const OPERATOR: &str = "operator-token-0123456789abcdef0123456789";

pub const BEAT: &str = r#"{"platform":"linux-x86_64","version":"1.0","health":"healthy"}"#;

pub const STATE: &str = r#"{"version":"2.0","max_updating":{"daemon":1},
    "min_up":{"storage":1,"frontend":1}}"#;

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("after 1970")
        .as_secs()
        .try_into()
        .expect("fits")
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

pub fn pem_of(label: &str, der: &[u8]) -> String {
    let body = base64::engine::general_purpose::STANDARD.encode(der);
    let lines: Vec<&str> = body
        .as_bytes()
        .chunks(64)
        .map(|c| std::str::from_utf8(c).expect("ascii"))
        .collect();
    format!(
        "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
        lines.join("\n")
    )
}

/// A certificate and the key it was issued for.
#[derive(Clone)]
pub struct Leaf {
    pub der: Vec<u8>,
    pub key: Vec<u8>,
}

impl Leaf {
    pub fn cert_pem(&self) -> String {
        pem_of("CERTIFICATE", &self.der)
    }

    pub fn key_pem(&self) -> String {
        pem_of("PRIVATE KEY", &self.key)
    }
}

/// A farm CA.
pub struct Pki {
    pub ca: Authority,
}

fn key_pair() -> (KeyPair, PublicKey) {
    let pair = KeyPair::generate().expect("a key");
    let public = PublicKey::from_spki(&pair.subject_public_key_info()).expect("a P-256 key");
    (pair, public)
}

impl Pki {
    pub fn new() -> Self {
        let signer = Arc::new(FileSigner::generate().expect("a key"));
        let ca = Authority::create(signer, Policy::default(), now(), &mut SystemEntropy)
            .expect("a root");
        Self { ca }
    }

    pub fn root_pem(&self) -> String {
        self.ca.bundle().to_pem()
    }

    pub fn coordinator(&self) -> Leaf {
        let (pair, public) = key_pair();
        let issued = self
            .ca
            .issue_coordinator(
                &["localhost", "127.0.0.1"],
                &public,
                now(),
                &mut SystemEntropy,
            )
            .expect("issued");
        Leaf {
            der: issued.der,
            key: pair.serialize_der(),
        }
    }

    pub fn updater(&self, name: &str, role: HostRole) -> Leaf {
        let (pair, public) = key_pair();
        let node_id = name.parse().expect("a node id");
        let cert = UpdaterCert {
            node_id: &node_id,
            host_role: &role,
            key: &public,
        };
        let issued = self
            .ca
            .issue_updater(&cert, now(), &mut SystemEntropy)
            .expect("issued");
        Leaf {
            der: issued.der,
            key: pair.serialize_der(),
        }
    }

    /// A storage node's certificate: the farm's own, not an updater's.
    pub fn storage_node(&self, name: &str) -> Leaf {
        let (pair, public) = key_pair();
        let node_id = name.parse().expect("a node id");
        let cert = NodeCert {
            node_id: &node_id,
            role: Role::Storage,
            key: &public,
            advertise: None,
        };
        let issued = self
            .ca
            .issue_node(&cert, now(), &mut SystemEntropy)
            .expect("issued");
        Leaf {
            der: issued.der,
            key: pair.serialize_der(),
        }
    }
}

/// How a client presents itself.
pub fn client_config(pki: &Pki, identity: Option<&Leaf>) -> Arc<ClientConfig> {
    let mut roots = RootCertStore::empty();
    roots
        .add(CertificateDer::from(pki.ca.root_der().to_vec()))
        .expect("a root");
    let builder = ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .expect("versions")
        .with_root_certificates(roots);
    let config = match identity {
        Some(leaf) => builder
            .with_client_auth_cert(
                vec![CertificateDer::from(leaf.der.clone())],
                PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf.key.clone())),
            )
            .expect("a client certificate"),
        None => builder.with_no_client_auth(),
    };
    Arc::new(config)
}

/// One HTTP/1.1 exchange over TLS: the status and the body, or why there was none.
pub async fn exchange(
    addr: SocketAddr,
    config: &Arc<ClientConfig>,
    method: &str,
    path: &str,
    bearer: Option<&str>,
    body: &str,
) -> std::io::Result<(u16, String)> {
    let tcp = TcpStream::connect(addr).await?;
    let name = ServerName::try_from("localhost").expect("a name");
    let mut tls = TlsConnector::from(Arc::clone(config))
        .connect(name, tcp)
        .await?;
    let auth = bearer.map_or_else(String::new, |t| format!("Authorization: Bearer {t}\r\n"));
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\n{auth}Connection: close\r\n\
         Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    tls.write_all(request.as_bytes()).await?;
    let mut raw = Vec::new();
    tls.read_to_end(&mut raw).await?;
    let text = String::from_utf8_lossy(&raw).into_owned();
    let status = text
        .strip_prefix("HTTP/1.1 ")
        .and_then(|rest| rest.get(..3))
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| std::io::Error::other(format!("no HTTP answer: {text:?}")))?;
    let body = text
        .split_once("\r\n\r\n")
        .map_or("", |(_, b)| b)
        .to_owned();
    Ok((status, body))
}

/// A token CI would send.
pub fn ci_token() -> String {
    mint(&keys()[0], &good_claims(STATIC_ISSUER))
}

/// A coordinator serving on loopback, on a clock the test sets.
pub struct Farm {
    pub pki: Arc<Pki>,
    pub bound: Bound,
    pub clock: Arc<AtomicU64>,
    pub dir: Arc<TestDir>,
    pub coordinator: Arc<Coordinator>,
    pub tls: (TlsSource, TlsSource),
    task: JoinHandle<()>,
}

impl Farm {
    pub async fn start(name: &str) -> Self {
        let pki = Arc::new(Pki::new());
        let dir = Arc::new(TestDir::under(std::env::temp_dir(), name));
        Self::start_on(pki, dir, 1_000, Duration::from_secs(3_600)).await
    }

    /// A coordinator that starts now (clock `at`) over the CA and the directory given: what a
    /// restart makes.
    pub async fn start_on(pki: Arc<Pki>, dir: Arc<TestDir>, at: u64, reload: Duration) -> Self {
        let clock = Arc::new(AtomicU64::new(at));
        let seconds = Arc::clone(&clock);
        let store = DesiredStore::open(&dir.path().join("desired.json")).expect("a store");
        let (verifier, _) = static_verifier();
        let coordinator = Arc::new(Coordinator::new(
            store,
            verifier,
            OPERATOR.to_owned(),
            Arc::new(move || Duration::from_secs(seconds.load(Ordering::SeqCst))),
        ));
        let leaf = pki.coordinator();
        let (cert, key) = (leaf.cert_pem(), leaf.key_pem());
        let write = |name: &str, text: &str| {
            let path = dir.path().join(name);
            std::fs::write(&path, text).expect("written");
            path
        };
        let files = kbf_tls::TlsFiles {
            cert: write("coordinator.pem", &cert),
            key: write("coordinator.key", &key),
            client_ca: None,
        };
        let ca = write("ca.pem", &pki.root_pem());
        let mut with_ca = files.clone();
        with_ca.client_ca = Some(ca);
        let heartbeat_tls =
            TlsSource::files("heartbeat", with_ca, reload).expect("a heartbeat source");
        let control_tls = TlsSource::files("control", files, reload).expect("a control source");
        let parts = Parts {
            coordinator: Arc::clone(&coordinator),
            heartbeat_tls: heartbeat_tls.clone(),
            control_tls: control_tls.clone(),
            heartbeat_listen: "127.0.0.1:0".parse().expect("an address"),
            control_listen: "127.0.0.1:0".parse().expect("an address"),
            reload,
        };
        let (tx, rx) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            run(parts, |bound| {
                let _ = tx.send(bound);
            })
            .await
            .expect("serving");
        });
        let bound = rx.await.expect("bound");
        Self {
            pki,
            bound,
            clock,
            dir,
            coordinator,
            tls: (heartbeat_tls, control_tls),
            task,
        }
    }

    pub fn at(&self, seconds: u64) {
        self.clock.store(seconds, Ordering::SeqCst);
    }

    /// Stops serving: what a crash leaves.
    pub fn stop(self) -> (Arc<Pki>, Arc<TestDir>) {
        self.task.abort();
        (self.pki, self.dir)
    }

    pub async fn beat(&self, who: &Leaf, body: &str) -> std::io::Result<(u16, String)> {
        let config = client_config(&self.pki, Some(who));
        exchange(
            self.bound.heartbeat,
            &config,
            "POST",
            "/v1/heartbeat",
            None,
            body,
        )
        .await
    }

    pub async fn control(
        &self,
        method: &str,
        path: &str,
        bearer: Option<&str>,
        body: &str,
    ) -> std::io::Result<(u16, String)> {
        let config = client_config(&self.pki, None);
        exchange(self.bound.control, &config, method, path, bearer, body).await
    }

    pub async fn status(&self) -> serde_json::Value {
        let (code, body) = self
            .control("GET", "/v1/status", Some(OPERATOR), "")
            .await
            .expect("answered");
        assert_eq!(code, 200, "{body}");
        serde_json::from_str(&body).expect("JSON")
    }
}
