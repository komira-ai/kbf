//! The state file's lock: one holder at a time, a refusal that names the holder and changes
//! nothing, and a lock that is free once its holder lets go.

use std::fs::{self, File, OpenOptions};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use kbf_testkit::dir::TestDir;

use crate::state_lock::{LockError, MAX_HOLDER_BYTES, NO_HOLDER_NOTE, StateLock, path_of};

fn state_in(dir: &TestDir) -> PathBuf {
    dir.path().join("desired.json")
}

/// Holds the lock file's `flock` the way another process would, with `note` in it.
fn held_by_hand(state: &Path, note: &[u8]) -> File {
    let path = path_of(state);
    fs::write(&path, note).expect("written");
    let file = OpenOptions::new().write(true).open(&path).expect("opened");
    file.try_lock().expect("locked");
    file
}

fn refusal(state: &Path) -> (PathBuf, String) {
    match StateLock::acquire(state, "pid=2 heartbeat=b control=b") {
        Err(LockError::Held { path, holder }) => (path, holder),
        other => panic!("not refused as held: {other:?}"),
    }
}

/// Catches: a lock file not beside the state file, or not named after it.
#[test]
fn the_lock_file_is_the_state_files_name_with_lock_added() {
    assert_eq!(
        path_of(Path::new("/srv/state/desired.json")),
        Path::new("/srv/state/desired.json.lock")
    );
    assert_eq!(
        path_of(Path::new("desired.json")),
        Path::new("desired.json.lock")
    );
}

/// Catches: a second holder admitted, or a refusal that does not name the holder, or that
/// rewrites the holder's note: the second take is refused with the path and the first
/// holder's note, by its whole message, and the lock file still holds the first note.
#[test]
fn a_second_take_is_refused_naming_the_holder_and_changes_nothing() {
    let dir = TestDir::under(std::env::temp_dir(), "lock-second");
    let state = state_in(&dir);
    let first = StateLock::acquire(&state, "pid=1 heartbeat=a control=a").expect("the first");
    assert_eq!(first.path(), path_of(&state));
    let before = fs::read(path_of(&state)).expect("read");
    assert_eq!(before, b"pid=1 heartbeat=a control=a\n");

    let Err(refused) = StateLock::acquire(&state, "pid=2 heartbeat=b control=b") else {
        panic!("a second holder was admitted");
    };
    assert_eq!(
        refused.to_string(),
        format!(
            "{}: another kbf-coordinator holds the lock of this state file (pid=1 \
             heartbeat=a control=a); a second coordinator on one state file is refused",
            path_of(&state).display()
        )
    );
    assert_eq!(fs::read(path_of(&state)).expect("read"), before);
    assert!(!state.exists(), "the state file is not touched");
    drop(first);
}

/// Catches: a lock that outlives its holder: once the first is dropped (what the kernel does
/// for a process that exits, however it exits) the next take succeeds and its note replaces
/// the first one whole, a longer old note leaving nothing behind.
#[test]
fn the_lock_is_free_once_its_holder_lets_go_and_the_next_note_replaces_the_old() {
    let dir = TestDir::under(std::env::temp_dir(), "lock-free");
    let state = state_in(&dir);
    let first = StateLock::acquire(&state, "pid=1 a long note that is longer than the next")
        .expect("the first");
    drop(first);
    let next = StateLock::acquire(&state, "pid=3").expect("free again");
    assert_eq!(fs::read(path_of(&state)).expect("read"), b"pid=3\n");
    drop(next);
}

/// Catches: a lock file that others may read or write: it is made owner-only.
#[test]
fn a_new_lock_file_is_owner_only() {
    let dir = TestDir::under(std::env::temp_dir(), "lock-mode");
    let state = state_in(&dir);
    let lock = StateLock::acquire(&state, "pid=1").expect("taken");
    let mode = fs::metadata(lock.path())
        .expect("stat")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

/// Catches: a holder that has not written its note yet (it took the lock a moment ago) being
/// shown as an empty name: the refusal says it left no note.
#[test]
fn a_holder_with_no_note_is_named_as_such() {
    let dir = TestDir::under(std::env::temp_dir(), "lock-empty");
    let state = state_in(&dir);
    let _held = held_by_hand(&state, b"");
    assert_eq!(
        refusal(&state),
        (path_of(&state), NO_HOLDER_NOTE.to_owned())
    );
    let _held_blank = held_by_hand(&path_with(&dir, "blank.json"), b" \n\t\n");
    assert_eq!(refusal(&path_with(&dir, "blank.json")).1, NO_HOLDER_NOTE);
}

fn path_with(dir: &TestDir, name: &str) -> PathBuf {
    dir.path().join(name)
}

/// Catches: a note that reaches the operator's terminal raw or whole: control characters are
/// shown as `?`, the ends are trimmed, and no more than the first 256 bytes are shown.
#[test]
fn a_note_is_shown_printable_trimmed_and_bounded() {
    let dir = TestDir::under(std::env::temp_dir(), "lock-note");
    let state = state_in(&dir);
    let _held = held_by_hand(&state, b"  pid=7\x1b[31m red\x07\n");
    assert_eq!(refusal(&state).1, "pid=7?[31m red");

    let long = path_with(&dir, "long.json");
    let note = "x".repeat(1000);
    let _held_long = held_by_hand(&long, note.as_bytes());
    let shown = refusal(&long).1;
    assert_eq!(
        shown,
        "x".repeat(usize::try_from(MAX_HOLDER_BYTES).expect("small"))
    );
}

/// Catches: a start that goes on, or blames another holder, when the lock file cannot be made:
/// a state file in a directory that does not exist is an I/O error naming the lock file, and
/// nothing is created.
#[test]
fn a_lock_file_that_cannot_be_made_is_an_error_naming_it() {
    let dir = TestDir::under(std::env::temp_dir(), "lock-nodir");
    let state = dir.path().join("missing").join("desired.json");
    let Err(LockError::Io { path, source }) = StateLock::acquire(&state, "pid=1") else {
        panic!("not an I/O error");
    };
    assert_eq!(path, path_of(&state));
    assert_eq!(source.kind(), std::io::ErrorKind::NotFound);
    assert!(!dir.path().join("missing").exists());
}
