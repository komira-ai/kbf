//! The secret files on disk: the mode check, the refusals, the sweep.

use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::sync::Mutex;

use super::*;
use crate::object_store::secrets::{SecretError, SecretFiles};

// Catches a read that does not check the mode: a file any local user can read is not a
// secret file, and the routes must report it instead of using it (fail closed).
#[test]
fn a_wide_mode_file_is_refused_and_a_put_repairs_it() {
    let store = as_store(&fake());
    let root = scratch("wide");
    let dir = root.join("s");
    let reg = open(&store, &dir);
    reg.put_connection(&name("cold"), &put(SENTINEL)).unwrap();
    for wide in [0o644, 0o640, 0o604, 0o700, 0o666] {
        let path = file_of(&dir, "cold", 1);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(wide)).unwrap();
        let err = reg.secret(&name("cold")).unwrap_err();
        assert!(
            matches!(err, RegistryError::Secret(SecretError::WideMode(m)) if m == wide),
            "{wide:o}: {err}"
        );
        assert!(err.to_string().contains("must be 0600"), "{err}");
        assert!(!err.to_string().contains(SENTINEL));
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    // 0400 is tighter than 0600 and is used.
    std::fs::set_permissions(
        file_of(&dir, "cold", 1),
        std::fs::Permissions::from_mode(0o400),
    )
    .unwrap();
    assert_eq!(reg.secret(&name("cold")).unwrap().expose(), SENTINEL);

    // A wide file is not "the same secret": the same PUT writes a new, private version.
    std::fs::set_permissions(
        file_of(&dir, "cold", 1),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    let repaired = reg.put_connection(&name("cold"), &put(SENTINEL)).unwrap();
    assert_eq!(
        (repaired.generation, repaired.outcome),
        (2, AuditOutcome::Changed)
    );
    assert_eq!(listing(&dir), ["cold.0000000000000002.json"]);
    assert_eq!(mode(&file_of(&dir, "cold", 2)), 0o600);
}

// Catches a missing file reported as anything but Missing, and a PUT that cannot repair it.
#[test]
fn a_missing_file_is_reported_and_a_put_repairs_it() {
    let store = as_store(&fake());
    let root = scratch("missing");
    let dir = root.join("s");
    let reg = open(&store, &dir);
    reg.put_connection(&name("cold"), &put(SENTINEL)).unwrap();
    std::fs::remove_file(file_of(&dir, "cold", 1)).unwrap();
    let err = reg.secret(&name("cold")).unwrap_err();
    assert!(
        matches!(err, RegistryError::Secret(SecretError::Missing)),
        "{err}"
    );
    assert_eq!(err.to_string(), "the secret file is missing");
    reg.put_connection(&name("cold"), &put(SENTINEL)).unwrap();
    assert_eq!(reg.secret(&name("cold")).unwrap().expose(), SENTINEL);
}

// Catches a read that follows a link (a planted link could point it at any file) or
// accepts a directory, and a file that is not this module's JSON.
#[test]
fn a_link_a_directory_and_garbage_are_not_secret_files() {
    let store = as_store(&fake());
    let root = scratch("kinds");
    let dir = root.join("s");
    let reg = open(&store, &dir);
    reg.put_connection(&name("cold"), &put(SENTINEL)).unwrap();
    let path = file_of(&dir, "cold", 1);
    let real = dir.join("real");
    std::fs::rename(&path, &real).unwrap();
    std::os::unix::fs::symlink(&real, &path).unwrap();
    let err = reg.secret(&name("cold")).unwrap_err();
    assert!(
        matches!(err, RegistryError::Secret(SecretError::NotRegular)),
        "{err}"
    );
    std::fs::remove_file(&path).unwrap();
    std::fs::create_dir(&path).unwrap();
    let err = reg.secret(&name("cold")).unwrap_err();
    assert!(
        matches!(err, RegistryError::Secret(SecretError::NotRegular)),
        "{err}"
    );
    std::fs::remove_dir(&path).unwrap();
    for body in ["not json", "{}", r#"{"secret_access_key":7}"#, ""] {
        std::fs::write(&path, body).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let err = reg.secret(&name("cold")).unwrap_err();
        assert!(
            matches!(err, RegistryError::Secret(SecretError::Invalid)),
            "{body:?}: {err}"
        );
        std::fs::remove_file(&path).unwrap();
    }
}

// Catches a sweep that removes what it did not write (a foreign file in the directory)
// or keeps what no row names, and the temp files a crash leaves.
#[test]
fn the_sweep_removes_only_unreferenced_versions_and_temp_files() {
    let store = as_store(&fake());
    let root = scratch("sweep");
    let dir = root.join("s");
    let reg = open(&store, &dir);
    reg.put_connection(&name("cold"), &put("one")).unwrap();
    reg.put_connection(&name("hot"), &put("two")).unwrap();
    for stray in [
        "cold.00000000000000ff.json", // a version no row names
        "gone.0000000000000001.json", // a connection no row has
        ".tmp-1-1",                   // a crashed temp file
    ] {
        std::fs::write(dir.join(stray), "x").unwrap();
    }
    for foreign in [
        "notes.txt",
        "cold.json",                  // no version
        "cold.1.json",                // a version of the wrong width
        "cold.00000000000000FF.json", // upper-case hex is not our spelling
        "Cold.0000000000000001.json", // not a connection name
        "cold.000000000000000g.json", // not hex
        "a.b.0000000000000001.json",  // a dot in the name
    ] {
        std::fs::write(dir.join(foreign), "x").unwrap();
    }
    // A name that is not UTF-8 is not ours, even if it starts like a temp file's.
    let odd = std::ffi::OsStr::from_bytes(b".tmp-\xff");
    std::fs::write(dir.join(odd), "x").unwrap();
    assert_eq!(reg.sweep().unwrap(), 3);
    assert_eq!(
        listing(&dir),
        [
            ".tmp-\u{fffd}",
            "Cold.0000000000000001.json",
            "a.b.0000000000000001.json",
            "cold.0000000000000001.json",
            "cold.000000000000000g.json",
            "cold.00000000000000FF.json",
            "cold.1.json",
            "cold.json",
            "hot.0000000000000002.json",
            "notes.txt",
        ]
    );
    assert_eq!(reg.sweep().unwrap(), 0);
}

// Catches a directory made world-readable, and a failure to make a nested one.
#[test]
fn the_directory_is_made_private_with_its_parents() {
    let root = scratch("dirs");
    let deep = root.join("a/b/c");
    let _files = SecretFiles::new(&deep).unwrap();
    for part in ["a", "a/b", "a/b/c"] {
        assert_eq!(mode(&root.join(part)), 0o700, "{part}");
    }
    // Opening an existing one changes nothing.
    let _again = SecretFiles::new(&deep).unwrap();
}

// Catches an error that loses what failed: a directory path that is a file.
#[test]
fn a_secret_directory_that_is_a_file_is_an_error() {
    let root = scratch("notdir");
    let blocker = root.join("blocker");
    std::fs::write(&blocker, "x").unwrap();
    let err = SecretFiles::new(&blocker.join("below")).unwrap_err();
    assert!(matches!(err, SecretError::Io { .. }), "{err}");
    assert!(
        err.to_string()
            .starts_with("secret file create the directory:"),
        "{err}"
    );
    let store = as_store(&fake());
    assert!(matches!(
        open_now(store, &blocker.join("below"), Counting::arc()),
        Err(RegistryError::Secret(_))
    ));
}

// Catches write paths that leave a temp file behind when a step fails for real (the
// directory vanished under the registry).
#[test]
fn a_write_into_a_removed_directory_fails_and_leaves_nothing() {
    let store = as_store(&fake());
    let root = scratch("vanish");
    let dir = root.join("s");
    let reg = open(&store, &dir);
    std::fs::remove_dir(&dir).unwrap();
    let err = reg.put_connection(&name("cold"), &put("x")).unwrap_err();
    assert!(
        matches!(err, RegistryError::Secret(SecretError::Io { .. })),
        "{err}"
    );
    assert!(store.scan_prefix("objstore/").unwrap().is_empty());
    assert!(
        reg.sweep().is_err(),
        "the sweep cannot list a directory that is gone"
    );
}

// Catches a read of a directory that is missing as any other error than Missing, and
// removal that fails on an absent file.
#[test]
fn removing_an_absent_file_is_fine_and_a_failing_removal_is_reported() {
    let root = scratch("remove");
    let dir = root.join("s");
    let files = SecretFiles::new(&dir).unwrap();
    files.remove(&name("cold"), 1).unwrap();
    let blocker = file_of(&dir, "cold", 2);
    std::fs::create_dir(&blocker).unwrap();
    std::fs::write(blocker.join("inner"), "x").unwrap();
    let err = files.remove(&name("cold"), 2).unwrap_err();
    assert!(matches!(err, SecretError::Io { .. }), "{err}");
    // A path under a file cannot be opened: neither missing nor a link.
    let under = SecretFiles::new(&dir.join("x")).unwrap();
    std::fs::remove_dir(dir.join("x")).unwrap();
    std::fs::write(dir.join("x"), "f").unwrap();
    let err = under.read(&name("cold"), 1).unwrap_err();
    assert!(matches!(err, SecretError::Io { op: "open", .. }), "{err}");
}

// Catches a temp file name two writers can share: while one writer sits between its
// temp file and its rename, another writes a whole file in the same directory (two
// processes during a handover). A shared name fails the second with "already exists".
#[test]
fn two_writers_in_one_directory_never_share_a_temp_file() {
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;

    use crate::object_store::Step;

    let root = scratch("temps");

    let dir = root.join("s");
    let files = Arc::new(SecretFiles::new(&dir).unwrap());
    let (reached_tx, reached_rx) = mpsc::channel();
    let (go_tx, go_rx) = mpsc::channel::<()>();
    let go_rx = Mutex::new(go_rx);
    let first = AtomicBool::new(true);
    files.set_failpoint(move |step| {
        if step == Step::SyncTemp && first.swap(false, Ordering::SeqCst) {
            reached_tx.send(()).unwrap();
            go_rx.lock().unwrap().recv().unwrap();
        }
    });
    let held = {
        let files = Arc::clone(&files);
        std::thread::spawn(move || files.put(&name("one"), 1, &Secret::new("a".to_owned())))
    };
    reached_rx.recv().unwrap();
    files
        .put(&name("two"), 1, &Secret::new("b".to_owned()))
        .unwrap();
    go_tx.send(()).unwrap();
    held.join().unwrap().unwrap();
    assert_eq!(files.read(&name("one"), 1).unwrap().expose(), "a");
    assert_eq!(files.read(&name("two"), 1).unwrap().expose(), "b");
    assert_eq!(listing(&dir).len(), 2, "no temp file is left");
}
