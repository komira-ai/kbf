use std::fs::TryLockError;
use std::io::Read;
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Barrier};
use std::thread;

use super::*;

pub(crate) use crate::testkit::{A, B, C, c, full_state, host_root};

/// Catches a state that does not read back as written (a field dropped or renamed), a
/// file without its schema number first, and a missing file read as anything but "no
/// state yet". The bytes are pinned: the next build reads what this one wrote.
#[test]
fn the_state_round_trips_with_its_schema() {
    let (root, dir) = host_root("deploy-host-state-round-trip");
    let lock = Locked::acquire(&root).expect("lock");
    assert_eq!(lock.read().expect("read"), None);
    lock.write(&full_state()).expect("write");
    assert_eq!(lock.read().expect("read"), Some(full_state()));
    let text = fs::read_to_string(dir.join("var/lib/kbf-installer/state.json")).expect("file");
    assert_eq!(
        text,
        format!(
            "{{\n  \"schema\": 2,\n  \"current\": \"{A}\",\n  \"previous\": \"{B}\",\n  \
             \"staged\": \"{C}\",\n  \"pending_confirm\": {{\n    \"activation\": \"3-9\",\n    \
             \"commit\": \"{A}\",\n    \"previous\": \"{B}\",\n    \"deadline_wall\": 1800000000,\n    \
             \"deadline_mono_s\": 86400,\n    \"starts_left\": 2\n  }},\n  \
             \
             \"floor\": {{\n    \"epoch\": 3,\n    \"order_id\": 9,\n    \"generation\": 7\n  }},\n  \
             \"serving_instance\": \"a\",\n  \"last_acked_order_id\": 9,\n  \"unacked\": [\n    {{\n      \
             \"epoch\": 3,\n      \"order_id\": 10,\n      \"first_seen\": 1700000000,\n      \
             \"record\": \"{{}}\"\n    }}\n  ]\n}}\n"
        )
    );
    lock.write(&State::default()).expect("write");
    assert_eq!(lock.read().expect("read"), Some(State::default()));
}

/// Catches a state read and written without the lock held: while one run holds it, no
/// other may take it, and once it is dropped another may.
///
/// Mutant seen red: `acquire` without `lock()` (the second lock is taken at once).
#[test]
fn the_lock_is_held_while_a_locked_lives() {
    let (root, dir) = host_root("deploy-host-state-lock");
    let held = Locked::acquire(&root).expect("lock");
    let other = File::open(dir.join("run/kbf-installer/lock")).expect("the lock file exists");
    assert!(
        matches!(other.try_lock(), Err(TryLockError::WouldBlock)),
        "a second run took the lock while the first held it"
    );
    held.write(&full_state()).expect("write");
    drop(held);
    free_once_dropped(&other);
}

/// The lock on `other` is free the moment the holder is dropped: one try, no wait.
///
/// `Locked`'s `Drop` unlocks explicitly, so a child forked meanwhile and still holding a
/// duplicate of the descriptor does not keep the `flock` held.
fn free_once_dropped(other: &File) {
    other
        .try_lock()
        .expect("the lock is free once its holder is dropped");
}

/// Catches a lock that is not released once its holder is dropped while processes are being
/// spawned: the drop must free it for good, though a forked child briefly keeps a duplicate
/// of the descriptor.
///
/// Mutant: the `Drop` of `Locked` removed (the close alone releases the lock): with the
/// spawner thread running, the single `try_lock` failed 6 of 300 rounds before the unlock
/// on drop. `tests/state_lock.rs` holds the deterministic version with a forked child.
#[test]
fn the_lock_is_free_after_a_drop_while_children_are_spawned() {
    let (root, dir) = host_root("deploy-host-state-lock-spawn");
    let stop = Arc::new(AtomicBool::new(false));
    let spawner = {
        let stop = Arc::clone(&stop);
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let _ = std::process::Command::new("/bin/true").status();
            }
        })
    };
    for _ in 0..300 {
        let held = Locked::acquire(&root).expect("lock");
        let other = File::open(dir.join("run/kbf-installer/lock")).expect("the lock file exists");
        drop(held);
        free_once_dropped(&other);
    }
    stop.store(true, Ordering::Relaxed);
    spawner.join().expect("spawner");
}

/// Catches a lock file or state file another user can read or write: the state says
/// which release a root service runs, and anyone who could hold the lock could stall
/// every deploy. Both are made 0600, and a rewrite keeps the state at 0600.
///
/// Mutant seen red: the state file made 0644 (as before this test).
#[test]
fn the_lock_and_the_state_are_root_s_alone() {
    let (root, dir) = host_root("deploy-host-state-modes");
    let lock = Locked::acquire(&root).expect("lock");
    lock.write(&full_state()).expect("write");
    lock.write(&State::default()).expect("rewrite");
    for file in ["run/kbf-installer/lock", "var/lib/kbf-installer/state.json"] {
        let mode = fs::metadata(dir.join(file)).expect(file).mode() & 0o7777;
        assert_eq!(mode, 0o600, "{file}");
    }
}

/// Catches writers that do not exclude each other: each of several threads adds one to
/// the order floor many times, through read, change and write under the lock; an update
/// lost to a concurrent writer leaves the total short.
///
/// Mutant seen red: `acquire` without `lock()`.
#[test]
fn concurrent_writers_lose_no_update() {
    let (root, _dir) = host_root("deploy-host-state-writers");
    const THREADS: u64 = 4;
    const EACH: u64 = 25;
    let start = Arc::new(Barrier::new(THREADS as usize));
    let writers: Vec<_> = (0..THREADS)
        .map(|_| {
            let (root, start) = (root.clone(), Arc::clone(&start));
            thread::spawn(move || {
                start.wait();
                for _ in 0..EACH {
                    let lock = Locked::acquire(&root).expect("lock");
                    let mut state = lock.read().expect("read").unwrap_or_default();
                    state.floor.order_id += 1;
                    lock.write(&state).expect("write");
                }
            })
        })
        .collect();
    for w in writers {
        w.join().expect("writer");
    }
    let lock = Locked::acquire(&root).expect("lock");
    let floor = lock.read().expect("read").expect("written").floor.order_id;
    assert_eq!(floor, THREADS * EACH);
}

/// Catches a write into the state file itself (a crash or power cut part-way would
/// leave half a state, which no later run could read): the old file, held open, must
/// keep the old state whole, the path must name a new file, and no copy may be left.
///
/// Mutant seen red: the state written in place (`fs::write` of the path).
#[test]
fn a_write_replaces_the_file_and_never_changes_the_old_one() {
    let (root, dir) = host_root("deploy-host-state-replace");
    let path = dir.join("var/lib/kbf-installer/state.json");
    let lock = Locked::acquire(&root).expect("lock");
    lock.write(&State::default()).expect("write");
    let before = fs::read(&path).expect("file");
    let mut old = File::open(&path).expect("open the old file");
    let old_inode = old.metadata().expect("stat").ino();
    lock.write(&full_state()).expect("write");
    let mut kept = Vec::new();
    old.read_to_end(&mut kept).expect("read the old file");
    assert_eq!(kept, before, "the old file was changed in place");
    assert_ne!(fs::metadata(&path).expect("stat").ino(), old_inode);
    let names: Vec<_> = fs::read_dir(dir.join("var/lib/kbf-installer"))
        .expect("list")
        .map(|e| e.expect("entry").file_name())
        .collect();
    let mut names: Vec<_> = names.into_iter().filter(|n| n != "releases").collect();
    names.sort();
    // The state and the copy of the state before it; no temporary file.
    assert_eq!(
        names,
        ["state.json", "state.json.bak"],
        "a temporary copy was left"
    );
}

/// Catches a reader that takes a state it does not understand: another schema (a newer
/// kbf-deploy wrote it), no schema, a field it does not know, a commit id that is not 40
/// lowercase hex characters, or something that is not a JSON object.
#[test]
fn a_state_this_build_does_not_know_is_refused() {
    let path = Path::new("/var/lib/kbf-installer/state.json");
    let full = format!(
        "\"current\":\"{A}\",\"previous\":null,\"staged\":null,\
         \"pending_confirm\":null,\
         \"floor\":{{\"epoch\":0,\"order_id\":0,\"generation\":0}},\
         \"serving_instance\":null,\"last_acked_order_id\":0,\"unacked\":[]"
    );
    let ok = format!("{{\"schema\":2,{full}}}");
    assert_eq!(
        parse(path, ok.as_bytes()).expect("parses").current,
        Some(c(A))
    );
    let schemas = [
        (format!("{{\"schema\":1,{full}}}"), "1"),
        (format!("{{\"schema\":\"2\",{full}}}"), "\"2\""),
    ];
    for (text, found) in schemas {
        let err = parse(path, text.as_bytes()).expect_err(&text);
        assert_eq!(
            err.to_string(),
            format!(
                "/var/lib/kbf-installer/state.json has schema {found}; this kbf-installer reads \
                 only schema 2"
            )
        );
    }
    let upper = A.to_uppercase();
    let refused = [
        (format!("{{\"schema\":2,{full},\"extra\":1}}"), "unknown field"),
        (
            format!("{{\"schema\":2,{}}}", full.replace(A, &upper)),
            "is not a commit id",
        ),
        (
            format!(
                "{{\"schema\":2,{}}}",
                full.replace(
                    "\"pending_confirm\":null",
                    &format!("\"pending_confirm\":{{\"activation\":\"1-1\",\"commit\":\"{A}\",\"previous\":null,\"x\":0}}")
                )
            ),
            "unknown field",
        ),
        (format!("{{{full}}}"), "has no schema"),
        ("{}".to_owned(), "has no schema"),
        ("[1]".to_owned(), "is not an installer state"),
        ("{".to_owned(), "is not an installer state"),
    ];
    for (text, why) in refused {
        let err = parse(path, text.as_bytes()).expect_err(&text);
        assert!(
            matches!(err, StateError::Parse { .. } | StateError::NoSchema { .. }),
            "{text}: {err:?}"
        );
        assert!(err.to_string().contains(why), "{text}: {err}");
    }
}

/// Catches errors read as "no state" or passed over: a lock whose directory is missing,
/// a state directory that is missing (or holds a stale copy the write cannot clear), and
/// a state path that is a directory.
#[test]
fn errors_name_the_path_and_change_nothing() {
    let dir = kbf_host::testkit::scratch("deploy-host-state-errors");
    let root = Layout::linux(kbf_host::path::Root::new(&dir).expect("absolute"));
    // The lock's directory is made when missing; a file where `/run` should be stops it.
    fs::write(dir.join("run"), "").expect("seed");
    let err = Locked::acquire(&root).expect_err("no /run");
    assert!(matches!(err, StateError::Lock { .. }), "{err:?}");
    fs::remove_file(dir.join("run")).expect("remove");

    fs::create_dir(dir.join("run")).expect("seed");
    let lock = Locked::acquire(&root).expect("lock");
    let err = lock.write(&full_state()).expect_err("no state directory");
    assert!(matches!(err, StateError::Write { .. }), "{err:?}");
    assert!(err.to_string().contains("state.json"), "{err}");

    fs::create_dir_all(dir.join("var/lib/kbf-installer/.state.json.tmp/x")).expect("seed");
    let err = lock
        .write(&full_state())
        .expect_err("a stale copy that is a directory");
    assert!(matches!(err, StateError::Write { .. }), "{err:?}");
    assert!(!dir.join("var/lib/kbf-installer/state.json").exists());

    fs::create_dir(dir.join("var/lib/kbf-installer/state.json")).expect("seed");
    // A directory where the state should be is not a state: with no backup, unusable.
    let err = lock.read().expect_err("a directory");
    assert!(
        matches!(&err, StateError::Unusable { primary, .. } if primary.contains("cannot read")),
        "{err:?}"
    );
    assert!(matches!(status(&root), Err(StateError::Unusable { .. })));
}

/// Catches a status that reads a link it cannot read as missing, or a file or directory
/// at `current` as a link, and a match that takes anything but exactly
/// `releases/<commit>` (an absolute target, another commit, a link with no state).
#[test]
fn status_reads_the_current_link_and_matches_it_to_the_state() {
    let (root, dir) = host_root("deploy-host-state-status");
    let link = dir.join("var/lib/kbf-installer/releases/current");
    let none = status(&root).expect("status");
    assert_eq!(
        none,
        Status {
            exists: false,
            state: State::default(),
            link: CurrentLink::Missing
        }
    );
    assert!(none.link_matches());

    Locked::acquire(&root)
        .expect("lock")
        .write(&full_state())
        .expect("write");
    let cases = [
        (None, false),
        (Some(A.to_owned()), true),
        (Some(B.to_owned()), false),
        (Some(format!("/var/lib/kbf-installer/releases/{A}")), false),
    ];
    for (target, matches) in cases {
        let _ = fs::remove_file(&link);
        if let Some(t) = &target {
            std::os::unix::fs::symlink(t, &link).expect("link");
        }
        let got = status(&root).expect("status");
        assert!(got.exists);
        assert_eq!(got.state, full_state());
        assert_eq!(got.link_matches(), matches, "{target:?}");
    }
    let no_current = Status {
        exists: true,
        state: State::default(),
        link: CurrentLink::To(PathBuf::from(A)),
    };
    assert!(!no_current.link_matches());

    fs::remove_file(&link).expect("remove");
    fs::create_dir(&link).expect("a directory");
    assert_eq!(status(&root).expect("status").link, CurrentLink::NotALink);

    fs::remove_dir(&link).expect("remove");
    fs::remove_dir(dir.join("var/lib/kbf-installer/releases")).expect("remove");
    fs::write(dir.join("var/lib/kbf-installer/releases"), "")
        .expect("a file where the directory is");
    let err = status(&root).expect_err("ENOTDIR");
    assert!(
        err.to_string()
            .contains("var/lib/kbf-installer/releases/current"),
        "{err}"
    );
}

/// The state directory of a `host_root` scratch host.
fn state_dir(dir: &Path) -> PathBuf {
    dir.join("var/lib/kbf-installer")
}

/// A state that is not [`full_state`], to tell two writes apart.
fn other_state() -> State {
    State {
        current: Some(c(B)),
        last_acked_order_id: 41,
        ..State::default()
    }
}

fn damage(path: &Path) {
    fs::write(path, "{ not json").expect("damage");
}

/// The names in `dir`, sorted: nothing but the state files may appear.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("list")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// S21: `state.json.bak` is the last state that read back whole: a write keeps the file it
/// replaces there, so after two writes the backup is the first state and the file the second,
/// and after a third the backup is the second.
///
/// Catches a backup that is not made (a lost state has nothing to fall back to), one made
/// from the new state (it would equal the file), and one that lags a write behind.
///
/// Mutants seen red: the copy to `state.json.bak` left out of `write`; the copy made after
/// the file is replaced.
#[test]
fn the_backup_is_the_state_before_the_last_write() {
    let (root, dir) = host_root("state-bak-last-good");
    let lock = Locked::acquire(&root).expect("lock");
    lock.write(&other_state()).expect("first");
    assert!(
        !state_dir(&dir).join("state.json.bak").exists(),
        "a first write has nothing to keep"
    );
    lock.write(&full_state()).expect("second");
    let bak = |dir: &Path| {
        let bytes = fs::read(state_dir(dir).join("state.json.bak")).expect("backup");
        parse(Path::new("bak"), &bytes).expect("a whole state")
    };
    assert_eq!(bak(&dir), other_state());
    assert_eq!(lock.read().expect("read"), Some(full_state()));
    lock.write(&State::default()).expect("third");
    assert_eq!(bak(&dir), full_state());
    assert_eq!(
        fs::metadata(state_dir(&dir).join("state.json.bak"))
            .expect("stat")
            .mode()
            & 0o7777,
        0o600
    );
}

/// S21: a `state.json` that is damaged or gone is read from the backup, which says so
/// (`from_backup`), and the read writes nothing. The same for the dry-run reader and `status`.
///
/// Catches a damaged file taken for an empty host (the next write would forget the release
/// the host runs), and a read that "repairs" by writing.
///
/// Mutant seen red: the backup never read (`read_file(&self.backup)` answering "missing").
#[test]
fn a_damaged_or_missing_state_is_read_from_the_backup() {
    let (root, dir) = host_root("state-bak-read");
    let lock = Locked::acquire(&root).expect("lock");
    lock.write(&other_state()).expect("first");
    lock.write(&full_state()).expect("second");
    let file = state_dir(&dir).join("state.json");
    let before = names(&state_dir(&dir));

    damage(&file);
    let damaged = fs::read(&file).expect("bytes");
    let loaded = lock.load().expect("falls back");
    assert_eq!(
        loaded,
        Loaded {
            state: Some(other_state()),
            from_backup: true
        }
    );
    assert_eq!(read_unlocked(&root).expect("dry"), Some(other_state()));
    let st = status(&root).expect("status");
    assert!(st.exists);
    assert_eq!(st.state, other_state());
    assert_eq!(fs::read(&file).expect("untouched"), damaged);

    fs::remove_file(&file).expect("remove");
    assert!(lock.load().expect("falls back").from_backup);
    assert_eq!(
        names(&state_dir(&dir)).len(),
        before.len() - 1,
        "no file made"
    );
}

/// S21: a write after a fall-back keeps the backup that was good: the damaged file is not
/// copied over it, and the new state is written to `state.json`.
///
/// Catches a backup made from whatever `state.json` holds (the last good state would be
/// destroyed by the very write that follows the damage).
///
/// Mutant seen red: the "reads back as a state" test dropped from the copy in `write`.
#[test]
fn a_damaged_file_never_replaces_the_good_backup() {
    let (root, dir) = host_root("state-bak-keep");
    let lock = Locked::acquire(&root).expect("lock");
    lock.write(&other_state()).expect("first");
    lock.write(&full_state()).expect("second");
    let file = state_dir(&dir).join("state.json");
    let bak = state_dir(&dir).join("state.json.bak");
    let good = fs::read(&bak).expect("backup");
    damage(&file);
    lock.write(&State::default()).expect("third");
    assert_eq!(
        fs::read(&bak).expect("backup"),
        good,
        "the backup was overwritten"
    );
    assert_eq!(lock.read().expect("read"), Some(State::default()));
    assert!(!lock.load().expect("load").from_backup);
}

/// S21: when neither file is usable and the host has been activated (a `current` or
/// `previous` link), the state is not taken to be empty: a read fails with `Unusable` (which
/// needs an operator: exit 5), a write fails the same way, and no file is created, changed or
/// removed. The same when `state.json` is simply gone and there is no backup.
///
/// Catches the dangerous repair: a lost state read as a fresh host, and the next verb writing
/// a state that forgets `current`, `previous` and a trial in progress.
///
/// Mutant seen red: `load` answering `Loaded { state: None, .. }` when both files fail.
#[test]
fn an_activated_host_without_a_usable_state_is_not_an_empty_one_and_nothing_is_written() {
    let (root, dir) = host_root("state-unusable");
    let lock = Locked::acquire(&root).expect("lock");
    lock.write(&other_state()).expect("first");
    lock.write(&full_state()).expect("second");
    std::os::unix::fs::symlink(A, state_dir(&dir).join("releases/current")).expect("link");
    let file = state_dir(&dir).join("state.json");
    let bak = state_dir(&dir).join("state.json.bak");
    damage(&file);
    damage(&bak);
    let before: Vec<_> = names(&state_dir(&dir));
    let bytes = (fs::read(&file).expect("f"), fs::read(&bak).expect("b"));

    for err in [
        lock.read().expect_err("read"),
        lock.write(&State::default()).expect_err("write"),
        read_unlocked(&root).expect_err("dry"),
        status(&root).expect_err("status"),
    ] {
        assert!(matches!(err, StateError::Unusable { .. }), "{err:?}");
        assert!(err.needs_operator());
        assert!(err.to_string().contains("nothing was changed"), "{err}");
    }
    assert_eq!(names(&state_dir(&dir)), before);
    assert_eq!(
        (fs::read(&file).expect("f"), fs::read(&bak).expect("b")),
        bytes
    );

    // Gone, with no backup: the same.
    fs::remove_file(&file).expect("remove");
    fs::remove_file(&bak).expect("remove");
    let err = lock.write(&State::default()).expect_err("write");
    assert!(matches!(err, StateError::Unusable { .. }), "{err:?}");
    assert!(!file.exists() && !bak.exists(), "a write made a file");
}

/// S21: a host that was never activated (no state, no backup, no link) is empty, even with
/// a release directory already under `releases/` (a release is published before the state
/// that names it is written, so a first install has one), and the first write works.
///
/// Catches the guard over-reaching: a first install refused as "unusable".
#[test]
fn a_host_never_activated_starts_empty_even_with_a_published_release() {
    let (root, dir) = host_root("state-fresh");
    fs::create_dir(state_dir(&dir).join("releases").join(A)).expect("published");
    let lock = Locked::acquire(&root).expect("lock");
    assert_eq!(
        lock.load().expect("empty"),
        Loaded {
            state: None,
            from_backup: false
        }
    );
    lock.write(&other_state()).expect("first write");
    assert_eq!(lock.read().expect("read"), Some(other_state()));
}

/// A `state.json` of another schema is a migration's business: it is refused as such, and
/// the backup is not tried (an older file would quietly undo the migration).
///
/// Catches a fall-back on every error, which would serve a schema-1 host its schema-2 backup
/// of a state from before the migration.
///
/// Mutant seen red: the `Err(e @ StateError::Schema { .. })` arm of `load` removed.
#[test]
fn another_schema_is_refused_and_the_backup_is_not_tried() {
    let (root, dir) = host_root("state-schema-no-fallback");
    let lock = Locked::acquire(&root).expect("lock");
    lock.write(&other_state()).expect("first");
    lock.write(&full_state()).expect("second");
    let file = state_dir(&dir).join("state.json");
    fs::write(&file, "{\"schema\":1,\"current\":null}").expect("schema 1");
    let err = lock.load().expect_err("refused");
    assert!(matches!(err, StateError::Schema { .. }), "{err:?}");
    assert!(!err.needs_operator());
}

/// Valid JSON with no `schema` at all (`{}`) is a damaged file: the backup is tried. Only an
/// explicit other schema number skips it (see `another_schema_is_refused_and_the_backup_is_not_tried`).
///
/// Catches a truncated or emptied file being reported as "another schema", which would stop a
/// host that has a good backup.
///
/// Mutant seen red: a missing `schema` read as `null` and refused as a schema.
#[test]
fn json_without_a_schema_is_damaged_so_the_backup_is_tried() {
    let (root, dir) = host_root("state-no-schema");
    let lock = Locked::acquire(&root).expect("lock");
    lock.write(&other_state()).expect("first");
    lock.write(&full_state()).expect("second");
    fs::write(state_dir(&dir).join("state.json"), "{}").expect("emptied");
    let loaded = lock.load().expect("falls back");
    assert_eq!(
        loaded,
        Loaded {
            state: Some(other_state()),
            from_backup: true
        }
    );
}

/// Make `<dir>/<name>` a directory so that replacing the file fails at its temporary name.
fn block(dir: &Path, name: &str) {
    fs::create_dir_all(state_dir(dir).join(name).join("x")).expect("block");
}

/// The order of a write is the backup, then the state. A write that dies after the backup
/// (here: the state cannot be replaced) leaves `state.json` as it was and a backup equal to it;
/// one that dies before (the backup cannot be written) leaves both untouched.
///
/// Catches the order reversed: a death between the two would then leave a new state with a
/// backup two writes old (or none), and a damaged new file would fall back to the wrong state.
///
/// Mutant seen red: the backup written after the state.
#[test]
fn a_write_that_dies_between_the_backup_and_the_state_loses_nothing() {
    let (root, dir) = host_root("state-write-order");
    let lock = Locked::acquire(&root).expect("lock");
    lock.write(&other_state()).expect("first");
    lock.write(&full_state()).expect("second");
    let (file, bak) = (
        state_dir(&dir).join("state.json"),
        state_dir(&dir).join("state.json.bak"),
    );
    let (state_before, bak_before) = (fs::read(&file).expect("f"), fs::read(&bak).expect("b"));

    // The backup cannot be written: nothing changes.
    block(&dir, ".state.json.bak.tmp");
    lock.write(&State::default()).expect_err("no backup");
    assert_eq!(
        (fs::read(&file).expect("f"), fs::read(&bak).expect("b")),
        (state_before.clone(), bak_before)
    );
    fs::remove_dir_all(state_dir(&dir).join(".state.json.bak.tmp")).expect("unblock");

    // The state cannot be written: the backup already holds the state that stays.
    block(&dir, ".state.json.tmp");
    lock.write(&State::default()).expect_err("no state");
    assert_eq!(fs::read(&file).expect("f"), state_before);
    assert_eq!(
        fs::read(&bak).expect("b"),
        state_before,
        "the backup was not made first"
    );
}

/// Catches a verb that queues behind a pass for ever, or gives up at once, or reports the
/// wrong thing: with the lock held `acquire_within` waits its time and then says busy; when
/// the holder lets go within the time it gets the lock; a lock file that cannot be opened is
/// a lock error (not busy).
///
/// Mutants planted: no wait (busy at once with a holder that lets go in time); the busy
/// error turned into a lock error.
#[test]
fn acquire_within_waits_for_a_holder_then_says_busy() {
    use std::time::{Duration, Instant};
    let (root, _dir) = host_root("acquire-within");
    let held = Locked::acquire(&root).expect("held");
    let started = Instant::now();
    let err = Locked::acquire_within(&root, Duration::from_millis(60)).expect_err("busy");
    assert!(matches!(err, StateError::Busy { .. }), "{err}");
    assert!(started.elapsed() >= Duration::from_millis(60));
    let holder = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(150));
        drop(held);
    });
    Locked::acquire_within(&root, Duration::from_secs(20)).expect("got it once it was let go");
    holder.join().expect("the holder ends");
    let lock = root.join(&root.lock());
    fs::remove_dir_all(lock.parent().expect("a directory")).expect("no lock directory");
    let err = Locked::acquire_within(&root, Duration::ZERO).expect_err("cannot open");
    assert!(matches!(err, StateError::Lock { .. }), "{err}");
}
