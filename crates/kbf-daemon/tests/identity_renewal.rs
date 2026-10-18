//! A node's certificate is replaced in place every few days (`kbf-enroll`'s renewal
//! renames a new file over the old one). A daemon that runs for longer than that must
//! present the new certificate on its next connection, to the frontend and to the storage
//! nodes, without a restart; a stream already up keeps the certificate it was made with.
//!
//! The client certificate each test starts with is expired, so the server's TLS refuses
//! it; the test then does what a renewal does and sees the next connection succeed.

mod support;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use kbf_daemon::{Daemon, DaemonConfig, FakeRuntime, NodeReport, TlsFiles, storage_nodes};
use kbf_map::{Member, MemberId, MemberState};
use kbf_route::{Liveness, Nodes as _};
use kbf_types::{Digest, DigestFunction};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType, date_time_ymd,
};
use support::storage_node::serve_tls;
use support::{PROMPT, Pki, scratch, serve_worker};
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio::time::timeout;

/// A farm CA, the server's certificate for `localhost`, and the means to issue the node's
/// client certificate again.
struct Authority {
    ca: CertifiedIssuer<'static, KeyPair>,
    client_key: KeyPair,
}

impl Authority {
    fn new() -> Self {
        let mut ca = CertificateParams::new(Vec::<String>::new()).expect("CA params");
        ca.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca.distinguished_name
            .push(DnType::CommonName, "kbf test CA");
        ca.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        Self {
            ca: CertifiedIssuer::self_signed(ca, KeyPair::generate().expect("CA key")).expect("CA"),
            client_key: KeyPair::generate().expect("client key"),
        }
    }

    /// The node's certificate, valid or expired, for the key the node keeps.
    fn client_cert(&self, expired: bool) -> String {
        let mut params = CertificateParams::new(vec!["node-1".to_owned()]).expect("params");
        let role = "urn:kbf:role:daemon".try_into().expect("an IA5 name");
        params.subject_alt_names.push(SanType::URI(role));
        params.distinguished_name.push(DnType::CommonName, "node-1");
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ClientAuth];
        if expired {
            params.not_before = date_time_ymd(2000, 1, 1);
            params.not_after = date_time_ymd(2001, 1, 1);
        }
        params
            .signed_by(&self.client_key, &self.ca)
            .expect("sign the client certificate")
            .pem()
    }

    /// A [`Pki`] whose client files hold an expired certificate.
    fn pki(&self, dir: &Path) -> Pki {
        let mut params = CertificateParams::new(vec!["localhost".to_owned()]).expect("params");
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let key = KeyPair::generate().expect("server key");
        let cert = params
            .signed_by(&key, &self.ca)
            .expect("server certificate");
        let write = |name: &str, text: &str| {
            let path = dir.join(name);
            std::fs::write(&path, text).expect("write a TLS file");
            path
        };
        let client = TlsFiles {
            ca_cert: write("ca.pem", &self.ca.pem()),
            cert: write("client.pem", &self.client_cert(true)),
            key: write("client.key", &self.client_key.serialize_pem()),
            server_name: Some("localhost".to_owned()),
        };
        Pki {
            ca: self.ca.pem(),
            server_cert: cert.pem(),
            server_key: key.serialize_pem(),
            client,
        }
    }

    /// What a renewal does: a new certificate renamed over the old file.
    fn renew(&self, files: &TlsFiles) {
        let next = files.cert.with_extension("pem.new");
        std::fs::write(&next, self.client_cert(false)).expect("write the new certificate");
        std::fs::rename(&next, &files.cert).expect("replace the certificate");
    }
}

fn daemon_of(files: &TlsFiles, server: &str) -> JoinHandle<()> {
    let mut config = DaemonConfig::new(server.to_owned(), files.clone(), "node-1".to_owned());
    config.reconnect_after = Duration::from_millis(5);
    config.reconnect_max = Duration::from_millis(40);
    let runtime = Arc::new(FakeRuntime::new(Duration::ZERO));
    let report = NodeReport::new([("cpus", "1"), ("drivers", "fake"), ("mem_gib", "1")]);
    let daemon = Daemon::new(config, runtime, report).expect("daemon config");
    tokio::spawn(daemon.run(std::future::pending()))
}

async fn worker(
    pki: &Pki,
) -> (
    tokio::sync::mpsc::UnboundedReceiver<support::Peer>,
    JoinHandle<()>,
    String,
) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let port = listener.local_addr().expect("address").port();
    let (sessions, server) = serve_worker(pki.server_tls(), listener);
    (sessions, server, format!("https://localhost:{port}"))
}

/// Catches: a daemon that reads its certificate once, at start (the defect: a node
/// renewed its certificate and, after the old one expired, every reconnect presented the
/// old one until the process was restarted). The daemon starts with an expired
/// certificate, which the server refuses; the certificate file is replaced as a renewal
/// replaces it; the daemon's next attempt connects.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_renewed_certificate_is_presented_by_the_next_connection() {
    let authority = Authority::new();
    let pki = authority.pki(&scratch("identity-control"));
    let (mut sessions, server, url) = worker(&pki).await;
    let daemon = daemon_of(&pki.client, &url);

    assert!(
        timeout(Duration::from_millis(600), sessions.recv())
            .await
            .is_err(),
        "the server took a session from a daemon with an expired certificate"
    );
    authority.renew(&pki.client);
    timeout(PROMPT, sessions.recv())
        .await
        .expect("the daemon connects with the renewed certificate")
        .expect("server running");
    daemon.abort();
    server.abort();
}

/// Catches: a daemon that is stopped by an identity it cannot read at one attempt (a file
/// mid-replacement, an operator's mistake): the attempt fails, the daemon lives, and it
/// connects when a certificate the server accepts is in place.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_certificate_that_cannot_be_read_fails_the_attempt_not_the_daemon() {
    let authority = Authority::new();
    let pki = authority.pki(&scratch("identity-missing"));
    let (mut sessions, server, url) = worker(&pki).await;
    let daemon = daemon_of(&pki.client, &url);
    std::fs::remove_file(&pki.client.cert).expect("remove the certificate");

    assert!(
        timeout(Duration::from_millis(600), sessions.recv())
            .await
            .is_err(),
        "a daemon connected without a certificate"
    );
    assert!(!daemon.is_finished(), "the daemon stopped");
    authority.renew(&pki.client);
    timeout(PROMPT, sessions.recv())
        .await
        .expect("the daemon connects once the certificate is back")
        .expect("server running");
    daemon.abort();
    server.abort();
}

fn member(addr: String) -> Member {
    Member {
        id: MemberId(1),
        addr,
        zone: String::new(),
        weight: 1,
        state: MemberState::Active,
        boot: 0,
    }
}

fn asked() -> [Digest; 1] {
    [Digest::new(DigestFunction::Sha256, [0; 32], 1)]
}

/// Catches: storage nodes dialled with an identity read once, when the plane was built
/// (the defect, as above, for the daemon's connections to the storage owners). The
/// certificate is expired, so a call fails; it is replaced, and the next call, through
/// the same `storage_nodes`, succeeds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn storage_nodes_present_a_renewed_certificate() {
    let authority = Authority::new();
    let pki = authority.pki(&scratch("identity-nodes"));
    let node = serve_tls(&pki);
    let nodes = storage_nodes(
        &pki.client,
        Liveness {
            connect: Duration::from_secs(2),
            ..Liveness::default()
        },
    )
    .expect("TLS files");
    let owner = member(node.name());

    let refused = nodes
        .node(&owner, 1)
        .expect("a node")
        .find_missing(&asked())
        .await;
    assert!(refused.is_err(), "an expired certificate was accepted");

    authority.renew(&pki.client);
    let answer = timeout(
        PROMPT,
        nodes
            .node(&owner, 1)
            .expect("a node")
            .find_missing(&asked()),
    )
    .await
    .expect("the call is answered");
    assert_eq!(
        answer.expect("the renewed certificate is accepted").len(),
        1
    );
}

/// Catches: storage nodes that lose their working connections when a renewal is caught
/// half done (the certificate file missing for a moment), or that never look again: the
/// previous identity serves the calls meanwhile, and the replacement is taken up as soon
/// as it is readable. A certificate that did not change is not reloaded (the stamp).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn storage_nodes_keep_the_previous_identity_while_a_replacement_is_unreadable() {
    let authority = Authority::new();
    let pki = authority.pki(&scratch("identity-nodes-kept"));
    authority.renew(&pki.client);
    let node = serve_tls(&pki);
    let nodes = storage_nodes(&pki.client, Liveness::default()).expect("TLS files");
    let owner = member(node.name());
    let call = || async {
        timeout(
            PROMPT,
            nodes
                .node(&owner, 1)
                .expect("a node")
                .find_missing(&asked()),
        )
        .await
        .expect("the call is answered")
    };

    assert_eq!(call().await.expect("served").len(), 1);
    let kept = pki.client.cert.with_extension("pem.kept");
    std::fs::rename(&pki.client.cert, &kept).expect("move the certificate away");
    assert_eq!(
        call().await.expect("served by the previous identity").len(),
        1
    );
    std::fs::rename(&kept, &pki.client.cert).expect("put the certificate back");
    authority.renew(&pki.client);
    assert_eq!(call().await.expect("served by the new identity").len(), 1);
}

/// Catches storage nodes that record a replacement as taken up when it could not be read
/// (the certificate was renewed while the key file was out of place): the next call must
/// try again, not wait for the files to change once more. The old certificate is expired,
/// so the call succeeds only with the new identity.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn storage_nodes_try_again_when_the_replacement_could_not_be_read() {
    let authority = Authority::new();
    let pki = authority.pki(&scratch("identity-nodes-retry"));
    let node = serve_tls(&pki);
    let nodes = storage_nodes(
        &pki.client,
        Liveness {
            connect: Duration::from_secs(2),
            ..Liveness::default()
        },
    )
    .expect("TLS files");
    assert_eq!(nodes.liveness().connect, Duration::from_secs(2));
    let owner = member(node.name());
    let kept = pki.client.key.with_extension("key.kept");
    std::fs::rename(&pki.client.key, &kept).expect("move the key away");
    authority.renew(&pki.client);

    let early = nodes
        .node(&owner, 1)
        .expect("a node")
        .find_missing(&asked())
        .await;
    assert!(early.is_err(), "served with the expired certificate");
    std::fs::rename(&kept, &pki.client.key).expect("put the key back");
    let answer = timeout(
        PROMPT,
        nodes
            .node(&owner, 1)
            .expect("a node")
            .find_missing(&asked()),
    )
    .await
    .expect("the call is answered");
    assert_eq!(answer.expect("the renewed identity is used").len(), 1);
}
