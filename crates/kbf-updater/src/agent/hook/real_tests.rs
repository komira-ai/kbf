//! The real wiring of the hook from its flags, with certificates in a scratch directory and
//! nothing reachable: who the host is, which node it drains, what it can reach.

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::Duration;

use kbf_types::HostRole;

use super::flags::Flags;
use super::real::world;
use crate::agent::build::{Access, NO_CREDENTIAL};
use crate::agent::flags::{NodeApi, Timing};
use crate::agent::pki::{Pki, write_files};
use crate::inventory::Role;
use crate::testutil::scratch;

struct Rig {
    dir: PathBuf,
    flags: Flags,
}

impl Rig {
    fn new(name: &str, role: HostRole) -> Rig {
        let dir = scratch(name);
        let pki = Pki::new();
        let leaf = pki.updater("n1", role);
        let files = write_files(&dir, &pki, &leaf);
        let token = dir.join("token");
        std::fs::write(&token, "t".repeat(40)).unwrap();
        std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o600)).unwrap();
        let cordons = dir.join("cordons");
        std::fs::create_dir(&cordons).unwrap();
        let flags = Flags {
            record_dir: dir.join("record"),
            channel: "stable".to_owned(),
            helper_socket: dir.join("no-helper.sock"),
            access: Access {
                files,
                frontend: None,
                node_api: Some(NodeApi {
                    addr: "127.0.0.1:1".parse().unwrap(),
                    token_file: token,
                }),
                cordon_dir: cordons,
                daemon_node: None,
            },
            timing: Timing::DEFAULT,
        };
        Rig { dir, flags }
    }
}

/// The host is who its certificate says: the node and the role, the system clock, the helper
/// at the flag's socket (nothing there here, which is said when it is asked), and a drain that
/// is the frontend's (nothing reachable here, which is said when it is asked).
///
/// Catches: a hook that wires another node than the host's own.
#[test]
fn a_daemon_host_is_its_certificates_node() {
    let rig = Rig::new("hook-real-daemon", HostRole::DAEMON);
    let world = world(&rig.flags).unwrap();
    assert_eq!(
        (world.host.alias.as_str(), world.host.role),
        ("n1", Role::Daemon)
    );
    assert_eq!(world.host.node_id.as_deref(), Some("n1"));
    assert!((world.unix_now)() > 1_700_000_000);
    let before = world.clock.now();
    world.clock.sleep(Duration::from_millis(1));
    assert!(world.clock.now() > before);
    assert!(
        world.local.query().is_err(),
        "no helper listens at the socket"
    );
    assert!(
        world.drain.leases(&world.host).is_err(),
        "no frontend listens"
    );
}

/// A frontend host drains nothing unless it was told it runs a daemon; with `--daemon-node`
/// it drains that node, through the frontend. `--daemon-node` on any other host is refused.
///
/// Catches: a frontend's own drain done twice (the helper does it), or a daemon node's skipped.
#[test]
fn a_frontend_host_drains_only_the_daemon_it_runs() {
    let rig = Rig::new("hook-real-frontend", HostRole::FRONTEND);
    let world_of = |flags: &Flags| world(flags).unwrap();
    let plain = world_of(&rig.flags);
    assert_eq!(plain.host.role, Role::Frontend);
    assert_eq!(plain.drain.drain(&plain.host, None), Ok(()));
    assert_eq!(plain.drain.leases(&plain.host), Ok(0));
    assert_eq!(plain.health.refusal(&plain.host), None);

    let mut with_daemon = rig.flags.clone();
    with_daemon.access.daemon_node = Some("d1".to_owned());
    let with = world_of(&with_daemon);
    assert_eq!(with.host.node_id.as_deref(), Some("d1"));
    assert!(
        with.drain.drain(&with.host, None).is_err(),
        "the frontend is not reachable"
    );

    // The node of a daemon it runs cannot be recorded: the same refusal as a daemon host's.
    let mut unwritable = with_daemon.clone();
    unwritable.access.cordon_dir = rig.dir.join("missing");
    assert!(world(&unwritable).err().unwrap().contains("--cordon-dir"));

    for role in [HostRole::STORAGE, HostRole::DAEMON, HostRole::MAC_DAEMON] {
        let mut other = Rig::new("hook-real-flag", role.clone());
        other.flags.access.daemon_node = Some("d1".to_owned());
        let why = world(&other.flags).err().unwrap();
        assert!(why.contains("--daemon-node"), "{role}: {why}");
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
    let world = world(&rig.flags).unwrap();
    assert_eq!(
        world.health.refusal(&world.host).as_deref(),
        Some(NO_CREDENTIAL)
    );
    assert!(world.drain.placed(&world.host).is_err());
}

/// A cordon directory the hook cannot write stops it with the flag named, a certificate that is
/// no updater's with the file named, and a storage host is built like a daemon's.
///
/// Catches: a drain made with no way to remember it was ours.
#[test]
fn a_directory_or_certificate_that_cannot_be_used_names_what_is_wrong() {
    let mut rig = Rig::new("hook-real-cordon", HostRole::DAEMON);
    rig.flags.access.cordon_dir = rig.dir.join("missing");
    assert!(world(&rig.flags).err().unwrap().contains("--cordon-dir"));

    let mut rig = Rig::new("hook-real-cert", HostRole::DAEMON);
    rig.flags.access.files.cert = rig.dir.join("none.pem");
    assert!(world(&rig.flags).err().unwrap().contains("none.pem"));

    let rig = Rig::new("hook-real-storage", HostRole::STORAGE);
    assert_eq!(world(&rig.flags).unwrap().host.role, Role::Storage);
}
