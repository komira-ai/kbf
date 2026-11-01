//! Where the registration socket may be: its directory (owner, register group, mode), every
//! directory above it, and what may be at the socket's path before it is bound.

mod support;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
use std::os::unix::net::UnixListener;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use kbf_step_runner::register::{
    Declared, Grant, Registry, RegistryError, Server, check_socket_dir,
};
use support::{FakeClock, Scratch};

fn me() -> u32 {
    rustix::process::geteuid().as_raw()
}

fn my_group() -> u32 {
    rustix::process::getegid().as_raw()
}

struct Nothing;

impl Declared for Nothing {
    fn grant(&self, _name: &str) -> Option<Grant> {
        None
    }
}

/// A scratch directory with `outer/run`, the socket's directory, mode 0750.
struct Place {
    scratch: Scratch,
    outer: PathBuf,
    run: PathBuf,
}

impl Place {
    fn new(name: &str) -> Place {
        let scratch = Scratch::new(name);
        let outer = scratch.path().join("outer");
        let run = outer.join("run");
        fs::create_dir_all(&run).unwrap();
        fs::set_permissions(&run, fs::Permissions::from_mode(0o750)).unwrap();
        fs::create_dir_all(scratch.path().join("state")).unwrap();
        Place {
            scratch,
            outer,
            run,
        }
    }

    fn bind(&self, sock: &Path, group: u32) -> Result<Server, RegistryError> {
        let state = self.scratch.path().join("state");
        let registry = Arc::new(Registry::load(&state, FakeClock::new(), &Nothing).unwrap());
        Server::bind(sock, me(), group, registry, Arc::new(Nothing))
    }
}

fn host_owner(meta: &fs::Metadata) -> (u32, u32) {
    (meta.uid(), meta.gid())
}

/// Which file a directory is: its device and inode.
fn id(meta: &fs::Metadata) -> (u64, u64) {
    (meta.dev(), meta.ino())
}

fn id_of(path: &Path) -> (u64, u64) {
    id(&fs::metadata(path).unwrap())
}

fn refused_at(result: Result<impl Sized, RegistryError>) -> PathBuf {
    match result {
        Err(RegistryError::SocketDir(at)) => at,
        Err(other) => panic!("not refused for its place: {other:?}"),
        Ok(_) => panic!("accepted"),
    }
}

/// Catches a socket directory of the right owner and mode but another group being accepted (a
/// `root:wheel 0750` directory passed): the directory's group must be the register group, and
/// the socket bound in it takes that group. The test's own group is the one its directories have.
#[test]
fn the_socket_directory_must_have_the_register_group() {
    let place = Place::new("regp-group");
    let sock = place.run.join("s");
    let other = my_group().wrapping_add(1);
    assert_eq!(refused_at(place.bind(&sock, other)), place.run);
    assert!(!sock.exists());
    let _server = place.bind(&sock, my_group()).unwrap();
    assert_eq!(fs::metadata(&sock).unwrap().gid(), my_group());
}

/// Catches the group being read for any owner but root, or root's directory being refused for
/// its group alone: a directory owned by root (as read) with the register group is accepted,
/// with another group it is refused, and a root-owned directory of the right group above which a
/// directory has another owner is refused at that directory.
#[test]
fn a_root_owned_directory_is_held_to_the_register_group_too() {
    let place = Place::new("regp-root");
    let (run, outer) = (id_of(&place.run), id_of(&place.outer));
    const REGISTER: u32 = 977;
    // Everything read as root's, group 0, and the socket's directory with group `group`.
    let as_root = |group: u32| {
        move |meta: &fs::Metadata| -> (u32, u32) { (0, if id(meta) == run { group } else { 0 }) }
    };
    check_socket_dir(&place.run, 0, REGISTER, &as_root(REGISTER)).unwrap();
    let wheel = as_root(REGISTER + 1);
    assert_eq!(
        refused_at(check_socket_dir(&place.run, 0, REGISTER, &wheel)),
        place.run
    );
    // A directory above it owned by another user (here `outer`).
    let foreign_above = |meta: &fs::Metadata| -> (u32, u32) {
        match id(meta) {
            at if at == run => (0, REGISTER),
            at if at == outer => (4242, 0),
            _ => (0, 0),
        }
    };
    assert_eq!(
        refused_at(check_socket_dir(&place.run, 0, REGISTER, &foreign_above)),
        place.outer
    );
}

/// Catches only the socket's own directory being checked: a directory above it that its group
/// (whatever group that is) or others can write is refused, and so is one owned by another user
/// or reached through a link; the same place closed again is accepted.
#[test]
fn every_directory_above_the_socket_is_checked() {
    let place = Place::new("regp-above");
    let sock = place.run.join("s");
    for mode in [0o770, 0o757] {
        fs::set_permissions(&place.outer, fs::Permissions::from_mode(mode)).unwrap();
        assert_eq!(
            refused_at(place.bind(&sock, my_group())),
            place.outer,
            "{mode:o}"
        );
    }
    fs::set_permissions(&place.outer, fs::Permissions::from_mode(0o755)).unwrap();

    let outer = id_of(&place.outer);
    let foreign = |meta: &fs::Metadata| -> (u32, u32) {
        let (uid, gid) = host_owner(meta);
        (if id(meta) == outer { 4242 } else { uid }, gid)
    };
    assert_eq!(
        refused_at(check_socket_dir(&place.run, me(), my_group(), &foreign)),
        place.outer
    );

    let link = place.scratch.path().join("link");
    symlink(&place.outer, &link).unwrap();
    assert_eq!(
        refused_at(place.bind(&link.join("run").join("s"), my_group())),
        link
    );
    let _server = place.bind(&sock, my_group()).unwrap();
}

/// Catches a socket directory named by a path that is not absolute or that has `..` being
/// checked: what is checked would not be the directory the socket is bound in.
#[test]
fn the_socket_directory_must_be_absolute_and_plain() {
    let place = Place::new("regp-plain");
    let dotted = place.outer.join("..").join("outer").join("run");
    for dir in [Path::new("run"), Path::new(""), dotted.as_path()] {
        assert!(
            matches!(
                check_socket_dir(dir, me(), my_group(), &host_owner),
                Err(RegistryError::SocketDir(_))
            ),
            "{dir:?}"
        );
    }
    assert!(matches!(
        place.bind(Path::new("s"), my_group()),
        Err(RegistryError::SocketDir(_))
    ));
}

/// Catches a bind that removes whatever is at the socket's path when nothing answers there (a
/// regular file answers a connect with `ECONNREFUSED` too): a regular file, a directory and a
/// link to a stale socket are left in place and the start fails; a stale socket of the updater
/// is removed and the start succeeds.
#[test]
fn only_a_stale_socket_of_the_updater_is_removed() {
    let place = Place::new("regp-stale");
    let sock = place.run.join("s");
    fs::write(&sock, "not a socket").unwrap();
    let err = place.bind(&sock, my_group()).err().unwrap();
    assert!(
        matches!(&err, RegistryError::InTheWay(p) if p == &sock),
        "{err:?}"
    );
    assert!(err.to_string().contains("in the way"));
    assert_eq!(fs::read_to_string(&sock).unwrap(), "not a socket");
    fs::remove_file(&sock).unwrap();

    fs::create_dir(&sock).unwrap();
    assert!(matches!(
        place.bind(&sock, my_group()),
        Err(RegistryError::InTheWay(_))
    ));
    assert!(sock.is_dir());
    fs::remove_dir(&sock).unwrap();

    let elsewhere = place.run.join("elsewhere");
    drop(UnixListener::bind(&elsewhere).unwrap());
    symlink(&elsewhere, &sock).unwrap();
    assert!(matches!(
        place.bind(&sock, my_group()),
        Err(RegistryError::InTheWay(_))
    ));
    assert!(fs::symlink_metadata(&sock).unwrap().is_symlink());
    fs::remove_file(&sock).unwrap();

    drop(UnixListener::bind(&sock).unwrap());
    let _server = place.bind(&sock, my_group()).unwrap();
    assert!(std::os::unix::net::UnixStream::connect(&sock).is_ok());
}
