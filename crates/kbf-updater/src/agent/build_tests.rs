//! The real ports of the agent, and the process around it, with files in a scratch directory and
//! nothing reachable: the identity read from the certificate, the roles an agent will serve, what
//! each role drains, and the exit codes of the mode.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use kbf_testkit::dir::TestDir;
use kbf_types::HostRole;

use super::build::{
    NO_CREDENTIAL, NoCredential, build, cordon_dir, drain_for, frontend_ports, identity_of_files,
    node_ports, run_agent, run_config, service_ports, unix_now,
};
use super::coordinator::{Endpoint, TlsFiles};
use super::flags::{Config, NodeApi, ServiceConfig, ServiceDrain, ServiceHealth, Timing};
use super::local::{Limits, SocketLocal};
use super::pki::{Pki, write_files};
use super::run::Stop;
use super::status::host_platform;
use super::testing::host;
use crate::inventory::Role;
use crate::operator::OperatorApi;
use crate::ports::{Drain, HealthCheck};
use crate::version::Version;

struct Rig {
    dir: TestDir,
    pki: Pki,
    config: Config,
}

impl Rig {
    fn new(name: &str, role: HostRole) -> Self {
        let dir = TestDir::under(std::env::temp_dir(), name);
        let pki = Pki::new();
        let leaf = pki.updater("n1", role);
        let files = write_files(&dir, &pki, &leaf);
        let token = dir.join("token");
        std::fs::write(&token, "t".repeat(40)).unwrap();
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
        let config = Config {
            coordinator: Endpoint::parse("localhost:1").unwrap(),
            files,
            channel: "stable".to_owned(),
            helper_socket: dir.join("no-helper.sock"),
            frontend: None,
            enroll_frontend: None,
            state_dir: None,
            cordon_dir: None,
            daemon_node: None,
            node_api: Some(NodeApi {
                addr: "127.0.0.1:1".parse().unwrap(),
                token_file: token,
            }),
            timing: Timing {
                heartbeat: Duration::from_millis(20),
                ..Timing::DEFAULT
            },
            services: Vec::new(),
        };
        Self { dir, pki, config }
    }
}

/// The agent knows who it is from its certificate: the node and the host role, and nothing else
/// is a flag; a file that is not a certificate, holds none, or is no updater's says what is wrong.
#[test]
fn the_identity_comes_from_the_certificate() {
    let rig = Rig::new("bd-identity", HostRole::MAC_DAEMON);
    let who = identity_of_files(&rig.config.files).unwrap();
    assert_eq!(
        (who.node_id.to_string(), who.role),
        ("n1".to_owned(), HostRole::MAC_DAEMON)
    );

    let missing = TlsFiles {
        cert: rig.dir.join("none.pem"),
        ..rig.config.files.clone()
    };
    assert!(
        identity_of_files(&missing)
            .unwrap_err()
            .contains("none.pem")
    );
    std::fs::write(rig.dir.join("empty.pem"), "").unwrap();
    let empty = TlsFiles {
        cert: rig.dir.join("empty.pem"),
        ..rig.config.files.clone()
    };
    assert!(
        identity_of_files(&empty)
            .unwrap_err()
            .contains("holds no certificate")
    );
    std::fs::write(
        rig.dir.join("junk.pem"),
        "-----BEGIN CERTIFICATE-----\n!!\n-----END CERTIFICATE-----\n",
    )
    .unwrap();
    let junk = TlsFiles {
        cert: rig.dir.join("junk.pem"),
        ..rig.config.files.clone()
    };
    assert!(identity_of_files(&junk).unwrap_err().contains("junk.pem"));
    // A storage node's certificate is not an updater's.
    let node = rig.pki.storage_node("n1");
    std::fs::write(rig.dir.join("node.pem"), node.cert_pem()).unwrap();
    let not_updater = TlsFiles {
        cert: rig.dir.join("node.pem"),
        ..rig.config.files.clone()
    };
    assert!(
        identity_of_files(&not_updater)
            .unwrap_err()
            .contains("role is not `updater`")
    );
}

/// An agent is built for a storage, daemon, Mac or frontend host; files that make no TLS
/// configuration and an operator token file the API client does not accept are refused.
#[test]
fn an_agent_is_built_for_every_kind_of_host() {
    assert!(
        host_platform().is_some(),
        "tests run where kbf has releases"
    );
    for role in [
        HostRole::STORAGE,
        HostRole::DAEMON,
        HostRole::MAC_DAEMON,
        HostRole::FRONTEND,
    ] {
        let rig = Rig::new("bd-built", role.clone());
        let agent = build(&rig.config).unwrap_or_else(|e| panic!("{role}: {e}"));
        let status = agent.status();
        assert_eq!(status.version, Version::parse("0").unwrap());
        assert_eq!(status.health, crate::wire::Health::Unhealthy);
    }

    let mut rig = Rig::new("bd-nofiles", HostRole::DAEMON);
    std::fs::write(&rig.config.files.ca, "").unwrap();
    assert!(
        build(&rig.config)
            .err()
            .unwrap()
            .contains("holds no certificate")
    );

    let mut rig2 = Rig::new("bd-notoken", HostRole::DAEMON);
    rig2.config.node_api.as_mut().unwrap().token_file = rig2.dir.join("absent");
    assert!(build(&rig2.config).err().unwrap().contains("--token-file"));
    rig.config.files.cert = rig.dir.join("absent.pem");
    assert!(build(&rig.config).is_err());
}

/// Every host's node is drained through the frontend, a storage host's too (it may run a
/// daemon): with the frontend out of reach, the drain of a storage host fails as a daemon's
/// does and is not taken for done.
#[test]
fn every_host_is_drained_through_the_frontend() {
    let rig = Rig::new("bd-drain", HostRole::DAEMON);
    let node = rig.config.node_api.clone().unwrap();
    let api = OperatorApi::new(node.addr, node.token_file)
        .unwrap()
        .with_timeout(Duration::from_millis(200));
    for role in [Role::Storage, Role::Daemon, Role::MacDaemon] {
        let drain = drain_for(role, &api);
        let h = host(role);
        assert!(
            drain.drain(&h, Some(Duration::from_secs(1))).is_err(),
            "{role:?}"
        );
        assert!(drain.undrain(&h).is_err(), "{role:?}");
    }
}

/// `run_config` builds the agent and runs it until it is stopped: with nothing reachable it
/// heartbeats into the void and ends with exit 0 when told to; an agent that cannot be built is
/// exit 1 with the reason.
#[test]
fn the_process_runs_until_stopped_and_says_why_it_cannot_start() {
    let rig = Rig::new("bd-run", HostRole::DAEMON);
    let (code, text) = run_config(&rig.config, |stop| {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            stop.stop();
        });
    });
    assert_eq!((code, text.as_str()), (0, ""));

    let mut broken = Rig::new("bd-run-broken", HostRole::FRONTEND);
    broken.config.files.cert = broken.dir.join("absent.pem");
    let (code, text) = run_config(&broken.config, |_| unreachable!("not started"));
    assert_eq!(code, 1);
    assert!(
        text.starts_with("kbf-updater: ") && text.contains("absent.pem"),
        "{text}"
    );
}

/// `run_agent` hands the caller the handle before it runs.
#[test]
fn the_caller_gets_the_stop_handle_before_the_agent_runs() {
    let rig = Rig::new("bd-ready", HostRole::STORAGE);
    let agent = build(&rig.config).unwrap();
    let stop_seen = Arc::new(std::sync::Mutex::new(None::<Stop>));
    let slot = Arc::clone(&stop_seen);
    let (code, _) = run_agent(&agent, move |stop| {
        stop.stop();
        *slot.lock().unwrap() = Some(stop);
    });
    assert_eq!(code, 0);
    assert!(stop_seen.lock().unwrap().is_some());
}

/// The clock the deadlines are read against is the wall clock in Unix seconds.
#[test]
fn the_clock_is_unix_seconds() {
    assert!(unix_now() > 1_700_000_000);
}

/// An agent given no operator API credential is built, but it refuses every update order before
/// anything is fetched, and drains nothing.
#[test]
fn an_agent_without_the_operator_credential_refuses_every_update() {
    let mut rig = Rig::new("bd-nocred", HostRole::DAEMON);
    rig.config.node_api = None;
    assert!(build(&rig.config).is_ok());
    let h = host(Role::Daemon);
    let v = Version::parse("1.2.3").unwrap();
    let none = NoCredential(NO_CREDENTIAL);
    assert!(none.refusal(&h).unwrap().contains("neither --frontend"));
    assert!(none.check(&h, &v).is_err());
    assert!(none.check_held(&h, &v).is_err());
    assert!(none.drain(&h, Some(Duration::from_secs(1))).is_err());
    assert!(none.leases(&h).is_err());
    assert!(none.undrain(&h).is_err());
}

/// A host enrolled in a state directory: the files `kbf-updater enroll` leaves (the key and the
/// certificate of an updater, the CA, and `node.json` with its renewal due long from now).
fn enrolled_in(dir: &TestDir, pki: &Pki, role: HostRole) -> std::path::PathBuf {
    use kbf_enroll::answer::Cert;
    use kbf_enroll::store::{Node, Store, StoredConfig};
    let now = i64::try_from(unix_now()).unwrap();
    let state = dir.join("state");
    let store = Store::open(&state).unwrap();
    let leaf = pki.updater("n1", role);
    let pki_dir = state.join("pki");
    std::fs::write(pki_dir.join("updater.pem"), leaf.cert_pem()).unwrap();
    std::fs::write(pki_dir.join("updater.key"), leaf.key_pem()).unwrap();
    std::fs::set_permissions(
        pki_dir.join("updater.key"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    std::fs::write(pki_dir.join("ca.pem"), pki.root_pem()).unwrap();
    let cert = Cert {
        role: kbf_types::Role::Updater,
        pem: leaf.cert_pem(),
        serial: "a1".to_owned(),
        not_before: 1,
        not_after: now + 200_000,
    };
    let node = Node::new(
        &"n1".parse().unwrap(),
        &kbf_types::CaPin::from_bytes([3; 32]),
        &[cert],
        now + 100_000,
        StoredConfig::default(),
    );
    let json = serde_json::to_vec(&node).unwrap();
    std::fs::write(pki_dir.join("node.json"), json).unwrap();
    std::fs::set_permissions(
        pki_dir.join("node.json"),
        std::fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    drop(store);
    state
}

/// An agent given a state directory and a frontend is built with the host's own certificate for
/// the frontend (no token) and a renewer for it; with a state directory that holds no enrollment
/// it is not built, and says to enroll.
#[test]
fn an_agent_with_a_state_directory_renews_what_it_was_enrolled_with() {
    let dir = TestDir::under(std::env::temp_dir(), "bd-state");
    let pki = Pki::new();
    let state = enrolled_in(&dir, &pki, HostRole::DAEMON);
    let store = kbf_enroll::store::Store::unopened(&state);
    let config = Config {
        coordinator: Endpoint::parse("localhost:1").unwrap(),
        files: TlsFiles {
            cert: store.cert_path(kbf_types::Role::Updater),
            key: store.key_path(kbf_types::Role::Updater),
            ca: store.ca_path(),
        },
        channel: "stable".to_owned(),
        helper_socket: dir.join("no-helper.sock"),
        frontend: Some(Endpoint::parse("localhost:1").unwrap()),
        enroll_frontend: Some(Endpoint::parse("localhost:1").unwrap()),
        state_dir: Some(state.clone()),
        cordon_dir: None,
        node_api: None,
        daemon_node: None,
        timing: Timing {
            heartbeat: Duration::from_millis(20),
            ..Timing::DEFAULT
        },
        services: Vec::new(),
    };
    let agent = build(&config).expect("an agent");
    assert_eq!(agent.status().health, crate::wire::Health::Unhealthy);
    let (code, text) = run_config(&config, |stop| {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(150));
            stop.stop();
        });
    });
    assert_eq!((code, text.as_str()), (0, ""));

    std::fs::remove_file(state.join("pki/node.json")).unwrap();
    let err = build(&config).err().unwrap();
    assert!(err.contains("kbf-updater enroll"), "{err}");
}

/// A frontend host's agent drains no node unless it was told the host runs a daemon (and
/// then drains that node through the frontend, which here cannot be reached); its health is the
/// helper's, never the roll's; `--daemon-node` is refused for any other host, and its node is
/// the daemon's.
#[test]
fn a_frontend_hosts_agent_drains_a_node_only_when_the_host_runs_a_daemon() {
    let rig = Rig::new("bd-fe-ports", HostRole::FRONTEND);
    let local = Arc::new(SocketLocal::new(
        &rig.config.helper_socket,
        Limits::default(),
    ));
    let identity = identity_of_files(&rig.config.files).unwrap();
    let h = host(Role::Frontend);

    let (drain, health) = frontend_ports(&rig.config, &identity, &local).unwrap();
    assert_eq!(drain.drain(&h, Some(Duration::from_secs(1))), Ok(()));
    assert_eq!(drain.leases(&h), Ok(0));
    assert_eq!(drain.undrain(&h), Ok(()));
    assert_eq!(health.refusal(&h), None);

    let mut with_daemon = rig.config.clone();
    with_daemon.daemon_node = Some("d1".to_owned());
    let (drain, _) = frontend_ports(&with_daemon, &identity, &local).unwrap();
    assert!(drain.drain(&h, Some(Duration::from_secs(1))).is_err());
    // The agent is built.
    assert!(build(&with_daemon).is_ok());

    // No credential at all for a daemon's drain: the refusal says so, as for any host.
    let mut bare = with_daemon.clone();
    bare.node_api = None;
    let (drain, _) = frontend_ports(&bare, &identity, &local).unwrap();
    assert!(drain.drain(&h, Some(Duration::from_secs(1))).is_err());

    for role in [HostRole::STORAGE, HostRole::DAEMON, HostRole::MAC_DAEMON] {
        let mut other = Rig::new("bd-fe-flag", role.clone());
        other.config.daemon_node = Some("d1".to_owned());
        let err = build(&other.config).err().unwrap();
        assert!(err.contains("--daemon-node"), "{role}: {err}");
    }
}

/// A service's ports are what its settings say: with a node drain and node health it drains and
/// judges through the frontend as `kbf` does (here: an API nobody answers, so every call fails);
/// with no drain its drain does nothing and cannot fail; with a readiness URL its health is that
/// URL's.
#[test]
fn a_services_ports_follow_its_settings() {
    let rig = Rig::new("bd-service-ports", HostRole::DAEMON);
    let identity = identity_of_files(&rig.config.files).unwrap();
    let h = host(Role::Daemon);
    let target = Version::parse("0.1.7+gbbbbbbbbbbbb-3").unwrap();
    let service = |drain, health, socket: &str| ServiceConfig {
        name: "db".to_owned(),
        channel: "db-ch".to_owned(),
        helper_socket: socket.into(),
        drain,
        health,
    };

    let node = service_ports(
        &rig.config,
        &identity,
        &h,
        &service(ServiceDrain::Node, ServiceHealth::Node, "/run/db.sock"),
    )
    .unwrap();
    assert_eq!((node.name.as_str(), node.channel.as_str()), ("db", "db-ch"));
    assert!(node.drain.drain(&h, Some(Duration::from_secs(1))).is_err());
    assert!(node.health.check(&h, &target).is_err());

    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let server = std::thread::spawn(move || {
        use std::io::{Read, Write};
        let (mut tcp, _) = listener.accept().unwrap();
        let mut buf = [0_u8; 512];
        let _ = tcp.read(&mut buf).unwrap();
        tcp.write_all(b"HTTP/1.1 200 OK\r\n\r\n").unwrap();
    });
    let ready = service_ports(
        &rig.config,
        &identity,
        &h,
        &service(
            ServiceDrain::None,
            ServiceHealth::Ready {
                addr,
                path: "/readyz".to_owned(),
            },
            "/run/db.sock",
        ),
    )
    .unwrap();
    assert_eq!(ready.drain.drain(&h, Some(Duration::from_secs(1))), Ok(()));
    assert_eq!(ready.drain.leases(&h), Ok(0));
    assert_eq!(ready.drain.undrain(&h), Ok(()));
    assert_eq!(
        ready.health.check(&h, &target),
        Ok(crate::ports::Health::Healthy)
    );
    server.join().unwrap();
    // Its own helper does not exist, and the answer says so.
    assert!(ready.local.query().is_err());

    // The agent with services is built from the same flags.
    let mut config = rig.config.clone();
    config.services = vec![service(
        ServiceDrain::None,
        ServiceHealth::Node,
        "/run/db.sock",
    )];
    assert!(build(&config).is_ok());
}

/// The marks of the agent's cordons are kept in `--cordon-dir`, else the state directory, else
/// the directory the placed key is in.
#[test]
fn the_cordon_marks_go_where_the_flags_say() {
    let mut rig = Rig::new("bd-cordon", HostRole::DAEMON);
    assert_eq!(
        cordon_dir(&rig.config),
        rig.config.files.key.parent().unwrap()
    );
    rig.config.state_dir = Some(PathBuf::from("/state"));
    assert_eq!(cordon_dir(&rig.config), PathBuf::from("/state"));
    rig.config.cordon_dir = Some(PathBuf::from("/cordons"));
    assert_eq!(cordon_dir(&rig.config), PathBuf::from("/cordons"));
}

/// An agent whose cordon directory it cannot write does not start with a drain it could not
/// use: the error names `--cordon-dir`, whatever the node is drained through.
///
/// Catches: every drain refused at the first update, with nothing at start to say why.
#[test]
fn a_cordon_directory_the_agent_cannot_write_stops_the_build_naming_the_flag() {
    let mut rig = Rig::new("bd-cordon-ro", HostRole::DAEMON);
    rig.config.cordon_dir = Some(rig.dir.join("missing"));
    let local = Arc::new(SocketLocal::new(
        &rig.config.helper_socket,
        Limits::default(),
    ));
    let identity = identity_of_files(&rig.config.files).unwrap();
    let why = node_ports(&rig.config, &identity, Role::Daemon, "kbf", &local)
        .err()
        .expect("refused");
    assert!(why.contains("--cordon-dir"), "{why}");
}

/// Catches: a role the agent has no duties for being built into an agent.
#[test]
fn a_role_the_agent_has_no_duties_for_is_refused() {
    let cache: HostRole = "cache".parse().unwrap();
    assert_eq!(
        super::build::kind_of(&cache).unwrap_err(),
        "cache is a host role this agent has no duties for"
    );
    assert_eq!(
        super::build::kind_of(&HostRole::MAC_DAEMON),
        Ok(Role::MacDaemon)
    );
}
