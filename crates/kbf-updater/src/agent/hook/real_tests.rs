//! The real wiring of the hook from its flags, with certificates in a scratch directory, the
//! time a test gives it, and a frontend of the test's own on a port the system picked: who the
//! host is, which node it drains, what it can reach and what it asks.

use std::io::{BufRead, BufReader};
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kbf_types::HostRole;

use super::WorldError;
use super::flags::Flags;
use super::real::{Time, world, world_at};
use crate::agent::build::{Access, NO_CREDENTIAL};
use crate::agent::flags::{NodeApi, Timing};
use crate::agent::pki::{Pki, write_files};
use crate::agent::testing::ManualClock;
use crate::inventory::Role;
use crate::ports::Clock;
use crate::testutil::scratch;

/// Unix seconds "now" of the time the tests give the world.
const NOW: u64 = 10_000;

/// A frontend of the test's own: a loopback listener on a port the system picked, so no other
/// program can be listening there. It reads the head of each request, keeps it, and hangs up
/// without an answer, so every call to it fails the same way on every host, and the test sees
/// what was asked.
struct HangUp {
    addr: SocketAddr,
    heads: Arc<Mutex<Vec<String>>>,
}

impl HangUp {
    fn start() -> HangUp {
        let listener = TcpListener::bind("127.0.0.1:0").expect("a port the system picks");
        let addr = listener.local_addr().expect("its address");
        let heads = Arc::new(Mutex::new(Vec::new()));
        let kept = heads.clone();
        std::thread::spawn(move || {
            for stream in listener.incoming().map_while(Result::ok) {
                let mut reader = BufReader::new(stream);
                let mut head = String::new();
                let mut line = String::new();
                while reader.read_line(&mut line).is_ok_and(|n| n > 0) && line != "\r\n" {
                    head.push_str(&line);
                    line.clear();
                }
                // Kept before the hang-up: the caller's call ends only after it.
                kept.lock().unwrap().push(head);
            }
        });
        HangUp { addr, heads }
    }

    /// The request lines asked so far, in order.
    fn requests(&self) -> Vec<String> {
        let heads = self.heads.lock().unwrap();
        heads
            .iter()
            .map(|head| head.lines().next().unwrap_or_default().to_owned())
            .collect()
    }

    /// Every request asked so far carried the operator token.
    fn all_carried(&self, token: &str) -> bool {
        let heads = self.heads.lock().unwrap();
        let header = format!("Authorization: Bearer {token}");
        !heads.is_empty() && heads.iter().all(|head| head.contains(&header))
    }
}

struct Rig {
    dir: PathBuf,
    flags: Flags,
    frontend: HangUp,
    token: String,
}

impl Rig {
    fn new(name: &str, role: HostRole) -> Rig {
        let dir = scratch(name);
        let pki = Pki::new();
        let leaf = pki.updater("n1", role);
        let files = write_files(&dir, &pki, &leaf);
        let token = "t".repeat(40);
        let token_file = dir.join("token");
        std::fs::write(&token_file, &token).unwrap();
        std::fs::set_permissions(&token_file, std::fs::Permissions::from_mode(0o600)).unwrap();
        let cordons = dir.join("cordons");
        std::fs::create_dir(&cordons).unwrap();
        let frontend = HangUp::start();
        let flags = Flags {
            record_dir: dir.join("record"),
            channel: "stable".to_owned(),
            helper_socket: dir.join("no-helper.sock"),
            access: Access {
                files,
                frontend: None,
                node_api: Some(NodeApi {
                    addr: frontend.addr,
                    token_file,
                }),
                cordon_dir: cordons,
                daemon_node: None,
            },
            timing: Timing::DEFAULT,
        };
        Rig {
            dir,
            flags,
            frontend,
            token,
        }
    }
}

/// The time of a test: a clock that moves only when the test (or a wait) moves it.
fn manual() -> (Arc<ManualClock>, Time) {
    let clock = Arc::new(ManualClock::default());
    let time = Time {
        clock: clock.clone(),
        unix_now: Arc::new(|| NOW),
    };
    (clock, time)
}

/// The host is who its certificate says: the node and the role, the helper at the flag's
/// socket (nothing there here, which is said when it is asked), and a drain that is the
/// frontend's at the flag's address, asked about the host's own node with the operator token
/// (the test's frontend hangs up, which is said when it is asked).
///
/// Catches: a hook that wires another node than the host's own, or that calls the frontend
/// without the token it was given.
#[test]
fn a_daemon_host_is_its_certificates_node() {
    let rig = Rig::new("hook-real-daemon", HostRole::DAEMON);
    let world = world_at(&rig.flags, manual().1).unwrap();
    assert_eq!(
        (world.host.alias.as_str(), world.host.role),
        ("n1", Role::Daemon)
    );
    assert_eq!(world.host.node_id.as_deref(), Some("n1"));
    assert!(
        world.local.query().is_err(),
        "no helper listens at the socket"
    );
    assert!(
        world.drain.leases(&world.host).is_err(),
        "the frontend hangs up"
    );
    assert_eq!(rig.frontend.requests(), ["GET /v1/nodes/n1 HTTP/1.1"]);
    assert!(rig.frontend.all_carried(&rig.token));
}

/// The world's clock and Unix seconds are the ones it is given: a wait on the world's clock
/// moves the given clock by exactly that much, at once, with no time passing on the system's;
/// what the given clock reads, the world reads; the Unix seconds are the given ones.
///
/// Catches: a world that waits on the system clock whatever it is given (a test of it then
/// sleeps for real, and one that asserts what it reads depends on how fast the host is).
#[test]
fn the_world_reads_and_waits_on_the_time_it_is_given() {
    let rig = Rig::new("hook-real-time", HostRole::DAEMON);
    let (clock, time) = manual();
    let world = world_at(&rig.flags, time).unwrap();
    assert_eq!(world.clock.now(), Duration::ZERO);
    clock.advance(Duration::from_secs(7));
    assert_eq!(world.clock.now(), Duration::from_secs(7));
    world.clock.sleep(Duration::from_secs(3600));
    assert_eq!(clock.now(), Duration::from_secs(3607));
    assert_eq!((world.unix_now)(), NOW);
}

/// The drain dials the frontend the flags name, and no other: with another listener up on the
/// same loopback (as a program on some fixed port would be), the call reaches the rig's own
/// frontend, on the port the system picked for it, and the other listener is never asked.
///
/// Catches: wiring that dials an address of its own (a fixed port), whose outcome then depends
/// on what happens to listen there on the host the test runs on.
#[test]
fn the_drain_dials_the_frontend_it_is_given_and_no_other() {
    let squatter = HangUp::start();
    let rig = Rig::new("hook-real-port", HostRole::DAEMON);
    assert_ne!(rig.frontend.addr, squatter.addr);
    let world = world_at(&rig.flags, manual().1).unwrap();
    assert!(world.drain.placed(&world.host).is_err());
    assert!(world.drain.undrain(&world.host).is_err());
    assert_eq!(
        rig.frontend.requests(),
        [
            "GET /v1/nodes/n1 HTTP/1.1",
            "POST /v1/nodes/n1:uncordon HTTP/1.1"
        ]
    );
    assert!(squatter.requests().is_empty());
}

/// A frontend host drains nothing unless it was told it runs a daemon; with `--daemon-node`
/// it drains that node, through the frontend. `--daemon-node` on any other host is refused.
///
/// Catches: a frontend's own drain done twice (the helper does it), or a daemon node's skipped.
#[test]
fn a_frontend_host_drains_only_the_daemon_it_runs() {
    let rig = Rig::new("hook-real-frontend", HostRole::FRONTEND);
    let world_of = |flags: &Flags| world_at(flags, manual().1).unwrap();
    let plain = world_of(&rig.flags);
    assert_eq!(plain.host.role, Role::Frontend);
    assert_eq!(plain.drain.drain(&plain.host, None), Ok(()));
    assert_eq!(plain.drain.leases(&plain.host), Ok(0));
    assert_eq!(plain.health.refusal(&plain.host), None);
    assert!(rig.frontend.requests().is_empty(), "nothing to drain");

    let mut with_daemon = rig.flags.clone();
    with_daemon.access.daemon_node = Some("d1".to_owned());
    let with = world_of(&with_daemon);
    assert_eq!(with.host.node_id.as_deref(), Some("d1"));
    assert!(
        with.drain.drain(&with.host, None).is_err(),
        "the frontend hangs up"
    );
    // The cordon marks read the daemon's node before they drain it; the hang-up ends there.
    assert_eq!(rig.frontend.requests(), ["GET /v1/nodes/d1 HTTP/1.1"]);
    assert!(rig.frontend.all_carried(&rig.token));

    // The node of a daemon it runs cannot be recorded: the same refusal as a daemon host's.
    let mut unwritable = with_daemon.clone();
    unwritable.access.cordon_dir = rig.dir.join("missing");
    let why = world_at(&unwritable, manual().1).err().unwrap();
    assert!(
        matches!(&why, WorldError::Ports(text) if text.contains("--cordon-dir")),
        "{why:?}"
    );

    for role in [HostRole::STORAGE, HostRole::DAEMON, HostRole::MAC_DAEMON] {
        let mut other = Rig::new("hook-real-flag", role.clone());
        other.flags.access.daemon_node = Some("d1".to_owned());
        let why = world_at(&other.flags, manual().1).err().unwrap();
        assert!(
            matches!(&why, WorldError::Host(text) if text.contains("--daemon-node")),
            "{role}: {why:?}"
        );
    }
}

/// With no way to the frontend (neither `--frontend` nor an operator token) the host is still
/// built, and every update is refused by the health precondition with the reason that says so.
///
/// Catches: a hook that drains a node with a credential it was not given.
#[test]
fn a_host_with_no_way_to_the_frontend_refuses_in_its_health_check() {
    let mut rig = Rig::new("hook-real-bare", HostRole::DAEMON);
    rig.flags.access.node_api = None;
    let world = world_at(&rig.flags, manual().1).unwrap();
    assert_eq!(
        world.health.refusal(&world.host).as_deref(),
        Some(NO_CREDENTIAL)
    );
    assert!(world.drain.placed(&world.host).is_err());
    assert!(rig.frontend.requests().is_empty());
}

/// A cordon directory the hook cannot write stops it with the flag named, a certificate that is
/// no updater's with the file named, each as its own kind; a storage host is built like a
/// daemon's. `world` (the system's time) refuses the same as `world_at`.
///
/// Catches: a drain made with no way to remember it was ours; a missing certificate reported
/// as a port that cannot be made.
#[test]
fn a_directory_or_certificate_that_cannot_be_used_names_what_is_wrong() {
    let mut rig = Rig::new("hook-real-cordon", HostRole::DAEMON);
    rig.flags.access.cordon_dir = rig.dir.join("missing");
    let why = world(&rig.flags).err().unwrap();
    assert!(
        matches!(&why, WorldError::Ports(text) if text.contains("--cordon-dir")),
        "{why:?}"
    );

    let mut rig = Rig::new("hook-real-cert", HostRole::DAEMON);
    rig.flags.access.files.cert = rig.dir.join("none.pem");
    let why = world(&rig.flags).err().unwrap();
    assert!(
        matches!(&why, WorldError::Certificate(text) if text.contains("none.pem")),
        "{why:?}"
    );
    assert!(why.to_string().contains("none.pem"));

    let rig = Rig::new("hook-real-storage", HostRole::STORAGE);
    assert_eq!(
        world_at(&rig.flags, manual().1).unwrap().host.role,
        Role::Storage
    );
}
