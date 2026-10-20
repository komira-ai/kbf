//! A node's certificate is replaced in place every few days (`kbf-enroll`'s renewal
//! renames a new file over the old one). A daemon that runs for longer than that must
//! present the new certificate on its next connection, to the frontend and to the storage
//! nodes, without a restart; a stream already up keeps the certificate it was made with.
//!
//! The client certificate each test starts with is expired, so the server's TLS refuses
//! it; the test then does what a renewal does and sees the next connection succeed.
//!
//! The tests that run a whole daemon wait for what the daemon does, never for the clock:
//! they see an attempt fail, replace the certificate, and wait for the session. The
//! server is dialled by its address, so no name lookup and no listener of another test
//! can answer in its place.

mod support;

use std::io;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use kbf_daemon::connect::Resolve;
use kbf_daemon::{Daemon, DaemonConfig, Event, FakeRuntime, NodeReport, TlsFiles, storage_nodes};
use kbf_map::{Member, MemberId, MemberState};
use kbf_route::{Liveness, Nodes as _};
use kbf_types::{Digest, DigestFunction};
use rcgen::{
    BasicConstraints, CertificateParams, CertifiedIssuer, DnType, ExtendedKeyUsagePurpose, IsCa,
    KeyPair, KeyUsagePurpose, SanType, date_time_ymd,
};
use support::storage_node::serve_tls;
use support::{PROMPT, Peer, Pki, scratch, serve_worker};
use tokio::net::TcpListener;
use tokio::sync::mpsc::UnboundedReceiver;
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

/// How long a test waits for a daemon that is not stuck. A bound that catches a hang, not
/// a window an attempt must fit in: one attempt can take the daemon's own welcome timeout
/// (10 s) to give up.
const HANG: Duration = Duration::from_secs(60);

/// A daemon for the server at `server` over `files`, built and not yet running, and the
/// events it will report. Building reads the TLS files once, to fail early; each attempt
/// reads them again. The server is dialled by its address; the name its certificate must
/// carry is the one in `files`.
fn built(files: &TlsFiles, server: SocketAddr) -> (Daemon, UnboundedReceiver<Event>) {
    let url = format!("https://{server}");
    let mut config = DaemonConfig::new(url, files.clone(), "node-1".to_owned());
    config.reconnect_after = Duration::from_millis(5);
    config.reconnect_max = Duration::from_millis(40);
    let runtime = Arc::new(FakeRuntime::new(Duration::ZERO));
    let report = NodeReport::new([("cpus", "1"), ("drivers", "fake"), ("mem_gib", "1")]);
    let (events_tx, events) = tokio::sync::mpsc::unbounded_channel();
    let daemon = Daemon::new(config, runtime, report).expect("daemon config");
    (daemon.with_events(events_tx), events)
}

/// A running daemon and what it reports. Dropping it stops the daemon.
struct Running {
    task: JoinHandle<()>,
    events: UnboundedReceiver<Event>,
}

impl Drop for Running {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Running {
    /// Runs `daemon`, which reports to `events`.
    fn start(daemon: Daemon, events: UnboundedReceiver<Event>) -> Self {
        let task = tokio::spawn(daemon.run(std::future::pending()));
        Self { task, events }
    }

    /// Forgets the events reported so far.
    fn forget(&mut self) {
        while self.events.try_recv().is_ok() {}
    }

    /// Why the daemon's next attempt to connect failed.
    ///
    /// # Panics
    /// If the server takes a session first (it accepted a certificate it must refuse), if
    /// no attempt fails within `HANG`, or if the daemon or the server stops.
    async fn next_refusal(&mut self, sessions: &mut UnboundedReceiver<Peer>) -> String {
        let waiting = async {
            loop {
                tokio::select! {
                    peer = sessions.recv() => {
                        peer.expect("server running");
                        panic!("the server took a session from a daemon with a bad certificate");
                    }
                    event = self.events.recv() => {
                        let event = event.expect("the daemon stopped");
                        if let Event::ConnectFailed { reason, .. } = event {
                            return reason;
                        }
                    }
                }
            }
        };
        let attempted = timeout(HANG, waiting).await;
        attempted.expect("the daemon makes an attempt")
    }

    /// Waits for the next session the daemon makes, once the certificate has been
    /// replaced and the events up to then forgotten, and returns the attempts that failed
    /// on the way, each as the address dialled and why.
    ///
    /// At most one attempt may fail: the one that read the old certificate before the
    /// replacement and was still going when it happened. A second failure is an attempt
    /// that began after the replacement and still presented the old certificate, and the
    /// test fails at once, not after `HANG`.
    ///
    /// # Panics
    /// On a second failed attempt, if no session comes within `HANG`, and if the daemon
    /// or the server stops.
    async fn connected_after_renewal(
        &mut self,
        sessions: &mut UnboundedReceiver<Peer>,
    ) -> Vec<String> {
        let mut refused = Vec::new();
        let waiting = async {
            loop {
                tokio::select! {
                    peer = sessions.recv() => {
                        peer.expect("server running");
                        return;
                    }
                    event = self.events.recv() => {
                        let event = event.expect("the daemon stopped");
                        if let Event::ConnectFailed { server, reason } = event {
                            refused.push(format!("{server}: {reason}"));
                        }
                    }
                }
                assert!(
                    refused.len() <= 1,
                    "an attempt after the renewal failed too: {refused:?}"
                );
            }
        };
        let connected = timeout(HANG, waiting).await;
        assert!(
            connected.is_ok(),
            "no session within {HANG:?} of the renewal; failed attempts: {refused:?}"
        );
        refused
    }
}

/// A fake server on a loopback port: the sessions it takes, its task and its address.
async fn worker(pki: &Pki) -> (UnboundedReceiver<Peer>, JoinHandle<()>, SocketAddr) {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("address");
    let (sessions, server) = serve_worker(pki.server_tls(), listener);
    (sessions, server, addr)
}

/// Catches: a daemon that reads its certificate once, at start (the defect: a node
/// renewed its certificate and, after the old one expired, every reconnect presented the
/// old one until the process was restarted). The daemon starts with an expired
/// certificate, which the server refuses; the certificate file is replaced as a renewal
/// replaces it; the daemon's next attempt connects.
///
/// The test sees an attempt fail before it replaces the file, and that the failure is the
/// server's refusal, not an unreadable identity. It allows one more failure after (the
/// attempt that had read the file before): the attempts after that must connect, however
/// long the one in flight takes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_renewed_certificate_is_presented_by_the_next_connection() {
    let authority = Authority::new();
    let pki = authority.pki(&scratch("identity-control"));
    let (mut sessions, server, addr) = worker(&pki).await;
    let (daemon, events) = built(&pki.client, addr);
    let mut daemon = Running::start(daemon, events);

    let reason = daemon.next_refusal(&mut sessions).await;
    assert!(
        !reason.starts_with("client identity"),
        "the first attempt failed before it presented a certificate: {reason}"
    );
    authority.renew(&pki.client);
    daemon.forget();
    daemon.connected_after_renewal(&mut sessions).await;
    server.abort();
}

/// Catches: a daemon that is stopped by an identity it cannot read at one attempt (a file
/// mid-replacement, an operator's mistake): the attempt fails, the daemon lives, and it
/// connects when a certificate the server accepts is in place.
///
/// The daemon is built, which reads the files, and the certificate removed before it
/// runs, so its first attempt cannot read it: that attempt fails as an identity failure.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_certificate_that_cannot_be_read_fails_the_attempt_not_the_daemon() {
    let authority = Authority::new();
    let pki = authority.pki(&scratch("identity-missing"));
    let (mut sessions, server, addr) = worker(&pki).await;
    let (daemon, events) = built(&pki.client, addr);
    std::fs::remove_file(&pki.client.cert).expect("remove the certificate");
    let mut daemon = Running::start(daemon, events);

    let reason = daemon.next_refusal(&mut sessions).await;
    assert!(
        reason.starts_with("client identity"),
        "the attempt failed for another reason: {reason}"
    );
    assert!(!daemon.task.is_finished(), "the daemon stopped");
    authority.renew(&pki.client);
    daemon.forget();
    daemon.connected_after_renewal(&mut sessions).await;
    server.abort();
}

/// Names the server by the addresses the test gives, the same on every round.
struct Listed(Vec<SocketAddr>);

impl Resolve for Listed {
    fn resolve(&self, _: String, _: u16) -> BoxFuture<'static, io::Result<Vec<SocketAddr>>> {
        let answer = self.0.clone();
        Box::pin(async move { Ok(answer) })
    }
}

/// Catches: an attempt that is under way when the certificate is replaced deciding the
/// attempts after it. The one in flight keeps the identity it read (a handshake that has
/// begun is not redone); the next attempt, to the next address of the same round, reads
/// the files again and presents the new certificate, with no refusal to learn from first.
/// An attempt can be in flight for longer than `PROMPT` when the certificate is replaced;
/// here the test holds one open and ends the stall itself, so the interleaving is the same
/// every run.
///
/// The server is named by two addresses. The first accepts the connection and says
/// nothing, so the daemon waits there with the (expired) identity it read for that
/// attempt. The test replaces the certificate, then closes the connection: that attempt
/// fails, and the attempt on the real server must be accepted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_attempt_after_a_stalled_one_presents_the_renewed_certificate() {
    let authority = Authority::new();
    let pki = authority.pki(&scratch("identity-stalled"));
    let (mut sessions, server, served) = worker(&pki).await;
    let stall = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let stalled = stall.local_addr().expect("address");
    let (daemon, events) = built(&pki.client, served);
    let daemon = daemon.with_resolver(Arc::new(Listed(vec![stalled, served])));
    let mut daemon = Running::start(daemon, events);

    let (held, _) = timeout(HANG, stall.accept())
        .await
        .expect("the daemon dials the stalled address")
        .expect("accept");
    authority.renew(&pki.client);
    daemon.forget();
    drop(held);
    let failed = daemon.connected_after_renewal(&mut sessions).await;
    let marker = format!("({stalled})");
    assert_eq!(
        failed.len(),
        1,
        "the stalled attempt fails once: {failed:?}"
    );
    assert!(
        failed.iter().all(|attempt| attempt.contains(&marker)),
        "an attempt that began after the renewal failed: {failed:?}"
    );
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
