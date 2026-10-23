//! The frontend's single-writer lock, probed and taken the way the SQLite store takes it.

mod support;

use std::fs::{self, File, TryLockError};
use std::io;
use std::os::unix::fs::symlink;
use std::path::Path;

use kbf_state_copy::lock;
use support::{golden_copy, open, scratch};

/// Asserts that a file with no lock file beside it is not held: the probe takes nothing, and
/// creates nothing.
#[test]
fn a_state_file_with_no_lock_file_is_not_held_and_gets_none() {
    let dir = scratch("lock-none");
    let db = lock::canonical(&golden_copy(&dir)).expect("the file is there");
    let guard = lock::take(&db).expect("nothing holds it");
    drop(guard);
    assert!(!lock::beside(&db, ".lock").exists());
}

/// Asserts that the lock the SQLite store holds on a state file is the lock the probe finds:
/// refused while the store is open, naming the lock file, and free again once it is closed.
#[test]
fn a_state_file_a_running_store_has_open_is_refused() {
    let dir = scratch("lock-store");
    let db = dir.join("state.db");
    let store = open(&db);
    let name = lock::canonical(&db).expect("the file is there");
    let err = lock::take(&name).expect_err("the store holds the lock");
    assert_eq!(err.code(), 2);
    let text = err.to_string();
    assert!(
        text.contains("state.db.lock is held by a running frontend"),
        "{text}"
    );
    drop(store);
    drop(lock::take(&name).expect("the store let go of it"));
}

/// Asserts that a second take is refused while a guard is alive and succeeds once the guard
/// is dropped, so the lock is held for exactly as long as the guard.
#[test]
fn the_lock_is_held_until_the_guard_is_dropped() {
    let dir = scratch("lock-guard");
    let db = dir.join("state.db");
    drop(open(&db));
    let name = lock::canonical(&db).expect("the file is there");
    let first = lock::take(&name).expect("the lock is free");
    let err = lock::take(&name).expect_err("the guard holds it");
    assert_eq!(err.code(), 2);
    drop(first);
    drop(lock::take(&name).expect("the guard let go of it"));
}

/// Asserts that a name that is a link to the state file finds the lock of the file itself, so
/// a frontend started by one name and a tool run by another still exclude each other.
#[test]
fn a_link_to_the_state_file_finds_the_same_lock() {
    let dir = scratch("lock-link");
    let db = dir.join("state.db");
    let link = dir.join("link.db");
    let store = open(&db);
    symlink(&db, &link).expect("make the link");
    let name = lock::canonical(&link).expect("the link resolves");
    assert_eq!(name, lock::canonical(&db).expect("the file is there"));
    let err = lock::take(&name).expect_err("the store holds the lock");
    assert_eq!(err.code(), 2);
    drop(store);
}

/// Asserts that a lock file that cannot be opened is a failure that says nothing about who
/// holds the state file.
#[test]
fn a_lock_file_that_cannot_be_opened_fails() {
    let dir = scratch("lock-loop");
    let db = golden_copy(&dir);
    let name = lock::canonical(&db).expect("the file is there");
    let lock_file = lock::beside(&name, ".lock");
    symlink(&lock_file, &lock_file).expect("make a link that loops");
    let err = lock::take(&name).expect_err("the lock file does not open");
    assert_eq!(err.code(), 1);
    assert!(err.to_string().contains("state.db.lock"), "{err}");
}

/// Asserts that a path that is not there fails, naming it.
#[test]
fn a_state_file_that_is_not_there_fails() {
    let dir = scratch("lock-missing");
    let err = lock::canonical(&dir.join("missing.db")).expect_err("no such file");
    assert_eq!(err.code(), 1);
    assert!(err.to_string().contains("missing.db"), "{err}");
}

/// Asserts that a try-lock that was taken is fine, one that would block is a refusal naming
/// the lock file, and any other failure is a failure and not a refusal.
#[test]
fn each_answer_of_a_try_lock_is_judged_as_what_it_says() {
    let lock_file = Path::new("/var/lib/kbf/state.db.lock");
    assert!(lock::judged(lock_file, Ok(())).is_ok());
    let blocked = lock::judged(lock_file, Err(TryLockError::WouldBlock));
    let err = blocked.expect_err("a held lock is refused");
    assert_eq!(err.code(), 2);
    assert!(
        err.to_string()
            .contains("/var/lib/kbf/state.db.lock is held")
    );
    let broken = TryLockError::Error(io::Error::other("no locks here"));
    let err = lock::judged(lock_file, Err(broken)).expect_err("a broken call fails");
    assert_eq!(err.code(), 1);
    assert!(err.to_string().contains("no locks here"), "{err}");
}

/// Asserts that a file the tool opened to probe is not left locked behind it: a plain lock
/// of the same file is free after the guard is gone.
#[test]
fn a_dropped_guard_leaves_the_lock_file_free() {
    let dir = scratch("lock-free");
    let db = dir.join("state.db");
    drop(open(&db));
    let name = lock::canonical(&db).expect("the file is there");
    drop(lock::take(&name).expect("the lock is free"));
    let file = File::open(lock::beside(&name, ".lock")).expect("the lock file is there");
    assert!(file.try_lock().is_ok());
    assert!(fs::metadata(lock::beside(&name, ".lock")).is_ok());
}

/// Asserts that a name gets a suffix appended, not an extension replaced.
#[test]
fn a_suffix_is_appended_to_the_whole_name() {
    let path = Path::new("/a/b/state.db");
    assert_eq!(lock::beside(path, ".lock"), Path::new("/a/b/state.db.lock"));
    assert_eq!(lock::beside(path, "-wal"), Path::new("/a/b/state.db-wal"));
}
