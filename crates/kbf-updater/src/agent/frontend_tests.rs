//! The agent's calls to the frontend with the host's own certificate and no token, against a
//! frontend played by a TLS server in the test: what it presents, what it sends (the three
//! call about its node and nothing else), whom it will talk to, and what the agent builds from
//! the flags it is given.

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt;
use std::thread::JoinHandle;
use std::time::Duration;

use kbf_testkit::dir::TestDir;
use kbf_types::HostRole;

use super::build::{NO_CREDENTIAL, node_health};
use super::coordinator::{Endpoint, TlsFiles};
use super::coordinator_tests::{Plan, Seen, http, ok, serve, server_config};
use super::flags::{Config, NodeApi, Timing};
use super::pki::{Leaf, Pki, write_files};
use super::testing::host;
use crate::identity::{Identity, identity_of};
use crate::inventory::Role;
use crate::node_health::NodeHealth;
use crate::operator::OperatorApi;
use crate::ports::{Health, HealthCheck, PortError, PortErrorKind};
use crate::version::Version;

/// A storage host's own view: its member active on the commit `aaaaaaaaaaaa`, nothing owed.
const OWN: &str = r#"{"node":"n1","half":"member","member":{"id":1,"state":"active","build_commit":"aaaaaaaaaaaa"},"storage":{"members_active":3,"under_replicated_blobs":0}}"#;

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
        let updater = pki.updater("n1", HostRole::STORAGE);
        let files = write_files(&dir, &pki, &updater);
        Self {
            pki,
            dir,
            files,
            updater,
        }
    }

    fn identity(&self) -> Identity {
        identity_of(&self.updater.der).unwrap()
    }

    /// A frontend that answers `answers` in turn, with the proxy's certificate from this CA.
    fn frontend(&self, answers: Vec<Plan>) -> (SocketAddr, JoinHandle<Vec<Seen>>) {
        serve(server_config(&self.pki, &self.pki.frontend()), answers)
    }

    fn api(&self, addr: SocketAddr) -> OperatorApi {
        OperatorApi::as_updater(
            Endpoint::parse(&format!("localhost:{}", addr.port())).unwrap(),
            self.files.clone(),
            self.identity(),
        )
        .with_timeout(Duration::from_secs(5))
    }

    fn config(&self, addr: Option<SocketAddr>) -> Config {
        Config {
            coordinator: Endpoint::parse("localhost:1").unwrap(),
            files: self.files.clone(),
            channel: "stable".to_owned(),
            helper_socket: self.dir.join("no-helper.sock"),
            frontend: addr.map(|a| Endpoint::parse(&format!("localhost:{}", a.port())).unwrap()),
            enroll_frontend: None,
            state_dir: None,
            cordon_dir: None,
            node_api: None,
            timing: Timing::DEFAULT,
            services: Vec::new(),
        }
    }
}

fn head_of(seen: &Seen) -> (Vec<String>, String) {
    let text = String::from_utf8(seen.request.clone()).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    (head.lines().map(str::to_owned).collect(), body.to_owned())
}

/// The target the storage view `OWN` is healthy for.
fn target() -> Version {
    Version::parse("0.1.5+gaaaaaaaaaaaa-2").unwrap()
}

/// A storage host's health, read as its updater does through `api`.
fn health_of(api: OperatorApi) -> Result<Health, PortError> {
    NodeHealth::for_updater(api).check(&host(Role::Storage), &target())
}

/// A storage host's agent reads its own node's storage health through the frontend with its
/// updater certificate: one call, the path of the operator API with the node id of the
/// certificate in it, mutual TLS with that certificate, and no `Authorization` header, no proxy
/// secret and no token anywhere in a request.
#[test]
fn the_agent_reads_its_storage_health_with_its_certificate_and_no_token() {
    let rig = Rig::new("fe-one");
    let (addr, server) = rig.frontend(vec![ok(OWN)]);
    assert_eq!(health_of(rig.api(addr)), Ok(Health::Healthy));

    let seen = server.join().unwrap();
    let calls: Vec<String> = seen.iter().map(|s| head_of(s).0[0].clone()).collect();
    assert_eq!(calls, ["GET /v1/nodes/n1/storage-health HTTP/1.1"]);
    for one in &seen {
        let (lines, _) = head_of(one);
        assert!(lines.contains(&format!("Host: localhost:{}", addr.port())));
        for line in &lines {
            let lower = line.to_ascii_lowercase();
            assert!(
                !lower.starts_with("authorization") && !lower.starts_with("x-kbf-"),
                "{line}"
            );
        }
        let who = identity_of(one.client.as_deref().expect("a client certificate")).unwrap();
        assert_eq!(
            (who.node_id.to_string(), who.role),
            ("n1".to_owned(), HostRole::STORAGE)
        );
    }
}

/// What the agent builds from its flags: the token, if it was given, is the way to the frontend
/// (the opt-in fallback); without it the certificate and `--frontend` are; with neither the
/// agent refuses.
#[test]
fn the_ports_follow_the_flags_the_agent_was_given() {
    let rig = Rig::new("fe-ports");
    let h = host(Role::Storage);

    let health = node_health(&rig.config(None), &rig.identity()).unwrap();
    assert_eq!(health.refusal(&h).unwrap(), NO_CREDENTIAL);
    assert_eq!(
        health.check(&h, &target()).unwrap_err().to_string(),
        NO_CREDENTIAL
    );

    // With the frontend, the host's own certificate asks it for the node's storage health.
    let (addr, server) = rig.frontend(vec![ok(OWN)]);
    let health = node_health(&rig.config(Some(addr)), &rig.identity()).unwrap();
    assert_eq!(health.refusal(&h), None);
    assert_eq!(health.check(&h, &target()), Ok(Health::Healthy));
    let seen = server.join().unwrap();
    assert_eq!(
        head_of(&seen[0]).0[0],
        "GET /v1/nodes/n1/storage-health HTTP/1.1"
    );
    assert_eq!(seen.len(), 1);

    // The token, given, wins: the call goes to the loopback address and the frontend is never
    // dialled (a connection of the test's own is the only one it sees).
    let (alive, watcher) = rig.frontend(vec![ok("{}")]);
    let token = rig.dir.join("token");
    std::fs::write(&token, "t".repeat(40)).unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut with_token = rig.config(Some(alive));
    with_token.node_api = Some(NodeApi {
        addr: "127.0.0.1:1".parse().unwrap(),
        token_file: token,
    });
    let health = node_health(&with_token, &rig.identity()).unwrap();
    let err = health.check(&h, &target()).unwrap_err();
    assert_eq!(err.kind(), PortErrorKind::Unreachable);
    assert!(err.to_string().contains("the operator API"), "{err}");
    drop(std::net::TcpStream::connect(alive));
    let seen = watcher.join().unwrap();
    assert!(
        seen[0].request.is_empty() && seen[0].client.is_none(),
        "{seen:?}"
    );
}

/// What answers must be the frontend: a coordinator's certificate for the right name, one for
/// another name and one from another CA are refused, and nothing is sent to any of them (no
/// request line, so the token-free credential of the host is never shown to a stranger).
#[test]
fn only_the_frontend_is_talked_to() {
    let rig = Rig::new("fe-who");
    let strangers = Pki::new();
    let cases = [
        serve(
            server_config(&rig.pki, &rig.pki.coordinator()),
            vec![ok("{}")],
        ),
        serve(
            server_config(&rig.pki, &rig.pki.storage_node("localhost")),
            vec![ok("{}")],
        ),
        serve(
            server_config(&strangers, &strangers.frontend()),
            vec![ok("{}")],
        ),
    ];
    for (addr, server) in cases {
        let err = health_of(rig.api(addr)).unwrap_err();
        assert_eq!(err.kind(), PortErrorKind::Other, "{err}");
        assert!(err.to_string().contains("is not the frontend"), "{err}");
        assert!(server.join().unwrap()[0].request.is_empty());
    }
}

/// An agent whose files were swapped for another node's certificate does not present it (the
/// node it asks about is the one it started as), whose files cannot be used say so without a
/// path, and a frontend that cannot be reached is unreachable.
#[test]
fn a_swapped_pair_a_bad_file_and_a_dead_frontend_are_refused() {
    let rig = Rig::new("fe-files");
    let (addr, server) = rig.frontend(vec![ok("{}")]);
    let api = rig.api(addr);
    let other = rig.pki.updater("n2", HostRole::STORAGE);
    write_files(&rig.dir, &rig.pki, &other);
    let err = health_of(api.clone()).unwrap_err();
    assert_eq!(err.kind(), PortErrorKind::Other);
    assert!(
        err.to_string().contains("TLS files cannot be used"),
        "{err}"
    );
    assert!(
        !err.to_string().contains(&rig.dir.display().to_string()),
        "{err}"
    );
    drop(api);
    // Nobody connected: the server is still waiting; give it a connection to end it.
    drop(std::net::TcpStream::connect(addr));
    let _ = server.join();

    // A pair that matches but is another role's is no updater's either.
    let storage = rig.pki.storage_node("n1");
    std::fs::write(&rig.files.cert, storage.cert_pem()).unwrap();
    std::fs::write(&rig.files.key, storage.key_pem()).unwrap();
    let err = health_of(api_for_dead(&rig)).unwrap_err();
    assert!(
        err.to_string().contains("TLS files cannot be used"),
        "{err}"
    );
    write_files(&rig.dir, &rig.pki, &rig.updater);
    let dead = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap()
    };
    let err = health_of(rig.api(dead)).unwrap_err();
    assert_eq!(err.kind(), PortErrorKind::Unreachable);
}

/// A client for a frontend that is never dialled: the files are judged first.
fn api_for_dead(rig: &Rig) -> OperatorApi {
    rig.api("127.0.0.1:1".parse().unwrap())
}

/// A reply the proxy sends in chunks is read whole, and a refusal of the frontend (a 403 for a
/// call that is not the node's own) is that refusal and not a success.
#[test]
fn a_chunked_reply_is_read_and_a_refusal_is_a_refusal() {
    let rig = Rig::new("fe-reply");
    let chunked = Plan::Say(
        format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n\
             5\r\n{}\r\n{:x}\r\n{}\r\n0\r\n\r\n",
            &OWN[..5],
            OWN.len() - 5,
            &OWN[5..]
        )
        .into_bytes(),
    );
    let (addr, server) = rig.frontend(vec![chunked, http("403 Forbidden", "", "{}")]);
    let api = rig.api(addr);
    assert_eq!(health_of(api.clone()), Ok(Health::Healthy));
    let err = health_of(api).unwrap_err();
    assert!(err.to_string().contains("refused the call (403)"), "{err}");
    server.join().unwrap();
}
