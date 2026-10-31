//! The lock file: one holder, the name checked against the file, links and file systems refused.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use kbf_testkit::dir::TestDir;

use super::lock::{NETWORK, check_lock_path, check_type, try_take};

/// Asserts that the lock is held by the one that took it until it is dropped: a second try, while
/// the first is held, gets nothing and is not an error; the file is made with mode 0600; and a
/// try after the drop takes it. It would catch a lock that two processes can hold.
#[test]
fn the_lock_has_one_holder_until_it_is_dropped() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-lock-holder");
    let path = dir.path().join("instance.lock");
    let first = try_take(&path).unwrap().expect("the first try takes it");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    assert!(try_take(&path).unwrap().is_none());
    assert!(try_take(&path).unwrap().is_none());
    drop(first);
    let again = try_take(&path).unwrap();
    assert!(again.is_some(), "a try after the drop takes it");
}

/// Asserts that a lock says it is current while its name leads to the file it holds, and is not
/// when the file was moved away, replaced by another file, removed, or turned into a link. It
/// would catch a holder that takes a lock on a file nothing else will look at for the lock.
#[test]
fn a_lock_is_current_only_while_its_name_leads_to_its_file() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-lock-current");
    let path = dir.path().join("instance.lock");
    let lock = try_take(&path).unwrap().unwrap();
    assert!(lock.is_current());
    let moved = dir.path().join("moved");
    std::fs::rename(&path, &moved).unwrap();
    assert!(!lock.is_current(), "moved away");
    std::fs::write(&path, b"").unwrap();
    assert!(!lock.is_current(), "replaced by another file");
    std::fs::remove_file(&path).unwrap();
    assert!(!lock.is_current(), "removed");
    std::os::unix::fs::symlink(&moved, &path).unwrap();
    assert!(!lock.is_current(), "a link to the very file");
    std::fs::remove_file(&path).unwrap();
    std::fs::rename(&moved, &path).unwrap();
    assert!(lock.is_current(), "put back");
}

/// Asserts that a lock file that is a link is refused with an error and not followed, and that a
/// path whose directory is not there is an error, not a lock that is not held.
#[test]
fn a_link_and_a_missing_directory_are_errors() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-lock-link");
    let target = dir.path().join("target");
    std::fs::write(&target, b"").unwrap();
    let link = dir.path().join("link.lock");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    assert!(try_take(&link).is_err());
    assert!(try_take(&dir.path().join("none/instance.lock")).is_err());
}

/// Asserts that the types of the network file systems are refused with the path in the message,
/// and the types of local ones are not: it would catch a lock file on an NFS mount, whose lock is
/// not a promise between hosts, taken as if it were one.
#[test]
fn a_network_file_system_is_refused() {
    let path = Path::new("/mnt/shared/instance.lock");
    for magic in NETWORK {
        let why = check_type(magic, path).unwrap_err().to_string();
        assert!(why.contains("/mnt/shared/instance.lock"), "{why}");
        assert!(why.contains("network file system"), "{why}");
    }
    // ext4, tmpfs, xfs, btrfs.
    for magic in [0xEF53, 0x0102_1994, 0x5846_5342, 0x9123_683E] {
        assert!(check_type(magic, path).is_ok(), "{magic:#x}");
    }
}

/// Asserts that a lock path must be absolute and name a file, with the words a refusal uses.
#[test]
fn a_lock_path_is_an_absolute_path_that_names_a_file() {
    assert_eq!(
        check_lock_path(Path::new("/var/lib/t/instance.lock")),
        Ok(())
    );
    let relative = check_lock_path(Path::new("instance.lock"));
    assert_eq!(relative, Err("must be an absolute path"));
    let empty = check_lock_path(Path::new(""));
    assert_eq!(empty, Err("must be an absolute path"));
    let root = check_lock_path(Path::new("/"));
    assert_eq!(root, Err("must name a file, not a directory"));
}
