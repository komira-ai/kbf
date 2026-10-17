//! `--state-store` values.

use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;

use clap::Parser;

use super::*;
use crate::Args;

fn parse(value: &str) -> Result<StateStoreUri, StateStoreUriError> {
    value.parse()
}

/// Catches: a value the design does not admit accepted (a `file://` store the server
/// could not open, a `postgres://` one nothing opens yet), a relative or empty SQLite
/// path (a database created wherever the process happens to run), and a refusal that
/// carries any of the value (a mistyped Postgres URL holds its password).
#[test]
fn only_a_sqlite_path_is_accepted() {
    assert_eq!(
        parse("sqlite:///var/lib/kbf/state.db"),
        Ok(StateStoreUri::Sqlite(PathBuf::from(
            "/var/lib/kbf/state.db"
        )))
    );
    for relative in ["sqlite://state.db", "sqlite://", "sqlite:///"] {
        assert_eq!(parse(relative), Err(StateStoreUriError::SqlitePath));
    }
    for other in [
        "postgres://",
        "postgres://kbf@db.example/state",
        "postgres://kbf:hunter2@db/state",
        "postgres:kbf:hunter2@db/state",
        "file:///var/lib/kbf",
        "/var/lib/kbf/state.db",
        "",
    ] {
        let refused = parse(other).expect_err(other);
        assert_eq!(refused, StateStoreUriError::Unsupported, "{other}");
        assert!(!format!("{refused} {refused:?}").contains("hunter2"));
    }
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kbf-server-state-uri-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

fn mode(path: &std::path::Path) -> u32 {
    std::fs::metadata(path)
        .expect("metadata")
        .permissions()
        .mode()
        & 0o777
}

/// Catches: a `sqlite://` store that is not opened (the scheduler would run in memory
/// behind a flag that promises durability), one whose directory must exist beforehand,
/// and one that does not hold its file against a second frontend. The epoch policy comes
/// with the store.
#[test]
fn a_sqlite_value_opens_its_file_and_holds_it() {
    let dir = scratch("open");
    let file = dir.join("deeper").join("state.db");
    let uri = StateStoreUri::Sqlite(file.clone());
    let store = uri.open().expect("opened");
    assert!(file.exists(), "the file is made, with its directory");
    assert_eq!(store.epoch, EpochPolicy::Bump);
    let refused = uri.open().err().expect("held");
    assert!(
        matches!(&refused, StateError::Open(why) if why.contains("another kbf process")),
        "{refused:?}"
    );
    drop(store);
    uri.open().expect("free once the first is gone");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Catches: the scheduler's state (every operation's request, every lease) readable by
/// other users of the machine: the directory the open makes is 0700, and the state file,
/// its lock and the journal files SQLite makes beside it are 0600, whatever the umask
/// (the open does not rely on it). A directory that exists keeps its mode.
#[test]
fn the_state_directory_and_files_are_private() {
    let dir = scratch("modes");
    let made = dir.join("a").join("b");
    let file = made.join("state.db");
    let store = StateStoreUri::Sqlite(file.clone()).open().expect("opened");
    assert_eq!(mode(&dir.join("a")), 0o700);
    assert_eq!(mode(&made), 0o700);
    let mut seen = 0;
    for entry in std::fs::read_dir(&made).expect("dir") {
        let entry = entry.expect("entry").path();
        assert_eq!(mode(&entry), 0o600, "{}", entry.display());
        seen += 1;
    }
    assert!(seen >= 2, "the file and its lock were checked: {seen}");
    drop(store);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Catches: a path that cannot be a state file (a directory) taken, and an error that
/// does not say which.
#[test]
fn a_sqlite_value_that_cannot_be_opened_names_the_path() {
    let dir = scratch("bad");
    std::fs::create_dir_all(&dir).expect("dir");
    let refused = StateStoreUri::Sqlite(dir.clone())
        .open()
        .err()
        .expect("a directory");
    assert!(matches!(refused, StateError::Open(_)), "{refused:?}");
    assert!(
        refused.to_string().contains(&dir.display().to_string()),
        "{refused}"
    );
    // A directory that cannot be made (its parent is a file) is the path's fault too.
    let file = dir.join("a-file");
    std::fs::write(&file, "x").expect("write");
    let under = StateStoreUri::Sqlite(file.join("deeper").join("state.db"));
    let refused = under.open().err().expect("no directory");
    assert!(matches!(refused, StateError::Open(_)), "{refused:?}");
    assert!(refused.to_string().contains("a-file"), "{refused}");
    let _ = std::fs::remove_dir_all(&dir);
    let mut lock = dir.into_os_string();
    lock.push(".lock");
    let _ = std::fs::remove_file(lock);
}

/// Catches: the flag not reaching the server's arguments, a default other than the
/// in-memory scheduler, and a value other than a SQLite path accepted by the command
/// line.
#[test]
fn the_flag_takes_a_sqlite_path_and_defaults_to_none() {
    let args = Args::try_parse_from(["kbf-server"]).expect("no flag");
    assert_eq!(args.state_store, None);
    assert!(args.open_state_store().expect("none").is_none());
    let args = Args::try_parse_from(["kbf-server", "--state-store=sqlite:///x.db"]).expect("flag");
    assert_eq!(
        args.state_store,
        Some(StateStoreUri::Sqlite(PathBuf::from("/x.db")))
    );
    let refused =
        Args::try_parse_from(["kbf-server", "--state-store=file:///x"]).expect_err("scheme");
    assert!(
        refused
            .to_string()
            .contains("only sqlite:///<absolute path>"),
        "{refused}"
    );
}

/// Catches: a refused `--state-store` that quotes its value back. clap echoes the value of
/// any parser's refusal (`invalid value '...'`), the command line is world-readable, and
/// the error goes to stderr and the journal. Each value holds a password in a form a
/// mistyped URL takes: user information, no `://` at all (the whole value used to be
/// taken for the scheme), `://` after the user information, a `password=` parameter, and
/// a value that is not UTF-8.
#[test]
fn a_refused_state_store_never_echoes_its_value() {
    for value in [
        "postgres://kbf:hunter2@db/x",
        "postgres:kbf:hunter2@db/x",
        "postgres:kbf:hunter2@db://x",
        "postgres://db/x?user=kbf&password=hunter2",
        "postgres://db/x?password=hunter2",
        "sqlite://hunter2",
    ] {
        let refused = Args::try_parse_from(["kbf-server", &format!("--state-store={value}")])
            .expect_err(value);
        let said = refused.to_string();
        for secret in ["hunter2", "kbf:", "kbf@", "password="] {
            assert!(!said.contains(secret), "{value}: {said}");
        }
        assert!(said.contains("--state-store: "), "{value}: {said}");
    }
    use std::os::unix::ffi::OsStringExt as _;
    let mut raw = b"--state-store=postgres://kbf:hunter2@db/\xff".to_vec();
    raw.push(b'x');
    let refused = Args::try_parse_from([
        std::ffi::OsString::from("kbf-server"),
        std::ffi::OsString::from_vec(raw),
    ])
    .expect_err("not UTF-8");
    assert!(!refused.to_string().contains("hunter2"), "{refused}");
}
