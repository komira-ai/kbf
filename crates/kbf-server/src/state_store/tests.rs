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
/// could not open), a relative or empty SQLite path (a database created wherever the
/// process happens to run), a Postgres URL that this client cannot use (no host, TLS asked
/// for) taken and failing only at the first connection, a Postgres URL not taken when it
/// is a Unix-socket one, and a refusal that carries any of the value (a mistyped
/// Postgres URL holds its password).
#[test]
fn a_sqlite_path_and_a_postgres_url_are_accepted_and_nothing_else() {
    assert_eq!(
        parse("sqlite:///var/lib/kbf/state.db"),
        Ok(StateStoreUri::Sqlite(PathBuf::from(
            "/var/lib/kbf/state.db"
        )))
    );
    for relative in ["sqlite://state.db", "sqlite://", "sqlite:///"] {
        assert_eq!(parse(relative), Err(StateStoreUriError::SqlitePath));
    }
    for url in [
        "postgres:///kbf?host=/run/postgresql",
        "postgresql:///kbf?host=/run/postgresql",
        "postgres://kbf@/kbf?host=/run/postgresql",
        "postgres://db.example/kbf",
    ] {
        assert!(
            matches!(parse(url), Ok(StateStoreUri::Postgres(_))),
            "{url}: {:?}",
            parse(url)
        );
    }
    for unusable in [
        "postgres://",
        "postgres:///kbf",
        "postgres://kbf:hunter2@db/state?sslmode=require",
        "postgres://kbf:hunter2@db:notaport/state",
    ] {
        let refused = parse(unusable).expect_err(unusable);
        assert_eq!(refused, StateStoreUriError::PostgresUrl, "{unusable}");
        assert!(!format!("{refused} {refused:?}").contains("hunter2"));
    }
    for other in [
        "postgres:kbf:hunter2@db/state",
        "file:///var/lib/kbf",
        "/var/lib/kbf/state.db",
        "mysql://kbf:hunter2@db/state",
        "",
    ] {
        let refused = parse(other).expect_err(other);
        assert_eq!(refused, StateStoreUriError::Unsupported, "{other}");
        assert!(!format!("{refused} {refused:?}").contains("hunter2"));
    }
}

/// Catches: the default Postgres store of an all-in-one drifting from the URL the design
/// names (`postgres:///kbf?host=/run/postgresql`, role and database `kbf`, the OS
/// package's socket directory), or being a value the parser would refuse.
#[test]
fn the_local_postgres_store_is_the_designs_socket_url() {
    assert_eq!(
        StateStoreUri::local_postgres(),
        parse("postgres:///kbf?host=/run/postgresql").expect("parses")
    );
    assert_eq!(
        StateStoreUri::local_postgres().identity(),
        PathBuf::from("postgres:///kbf?host=/run/postgresql")
    );
}

/// Asserts that an open of either store keeps the epoch it holds: a restart of a frontend,
/// or the start of another one over the same Postgres, does not change it, so the daemons
/// that reconnect keep the runs of the leases they hold. The epoch changes when an
/// operator raises it.
#[test]
fn every_open_of_either_store_keeps_the_epoch() {
    for uri in [
        StateStoreUri::local_postgres(),
        StateStoreUri::Sqlite(PathBuf::from("/x.db")),
    ] {
        assert_eq!(uri.epoch_policy(), EpochPolicy::Keep, "{uri:?}");
    }
}

/// Catches: the password of a URL printed by anything that prints the flags (`Debug` of
/// `Args` is logged), and the store identity carrying it to another process.
/// A URL with user information, a `password` parameter, and both.
#[test]
fn a_postgres_urls_password_is_in_neither_its_debug_text_nor_its_identity() {
    for url in [
        "postgres://kbf:hunter2@db.example/kbf",
        "postgres://db.example/kbf?password=hunter2",
        "postgres://kbf:hunter2@db.example/kbf?password=hunter2&connect_timeout=3",
    ] {
        let uri = parse(url).expect(url);
        assert!(!format!("{uri:?}").contains("hunter2"), "{uri:?}");
        let identity = uri.identity();
        let identity = identity.to_str().expect("text");
        assert!(!identity.contains("hunter2"), "{identity}");
        assert!(!identity.contains("kbf:"), "{identity}");
        assert!(identity.contains("db.example/kbf"), "{identity}");
    }
    // Two spellings of one database agree; two databases do not.
    let one = parse("postgres://kbf:hunter2@db.example/kbf").expect("one");
    let other = parse("postgres://kbf@db.example/kbf").expect("other");
    let elsewhere = parse("postgres://kbf@db.example/other").expect("elsewhere");
    assert_eq!(one.identity(), other.identity());
    assert_ne!(one.identity(), elsewhere.identity());
    let connect = parse("postgres://db.example/kbf?connect_timeout=3").expect("timeout");
    assert!(
        connect
            .identity()
            .to_string_lossy()
            .ends_with("?connect_timeout=3"),
        "other parameters stay: {connect:?}"
    );
}

/// Catches: an open of a Postgres store that cannot be reached succeeding (the
/// scheduler would run in memory behind a flag that promises durability), failing
/// with anything but [`StateError::Open`], or saying the URL's password. The socket
/// directory is one that does not exist.
#[test]
fn an_unreachable_postgres_store_refuses_the_open_without_the_password() {
    let dir = scratch("no-socket");
    let url = format!("postgres://kbf:hunter2@/kbf?host={}", dir.display());
    let uri = parse(&url).expect("parses");
    let refused = uri.open().err().expect("nothing listens");
    assert!(matches!(&refused, StateError::Open(_)), "{refused:?}");
    let said = format!("{refused} {refused:?}");
    assert!(!said.contains("hunter2"), "{said}");
}

/// A fresh directory for `name`, removed when it drops.
fn scratch(name: &str) -> kbf_testkit::dir::TestDir {
    crate::test_dir::scratch_dir(&format!("kbf-server-state-uri-{name}"))
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
    assert_eq!(store.epoch, EpochPolicy::Keep);
    let refused = uri.open().err().expect("held");
    assert!(
        matches!(&refused, StateError::Open(why) if why.contains("another kbf process")),
        "{refused:?}"
    );
    drop(store);
    uri.open().expect("free once the first is gone");
}

/// Asserts that opening a SQLite store writes no generation row, however often it is opened
/// (the file's exclusive lock is what keeps it to one process), and that the identity of a
/// SQLite store is its file.
#[test]
fn a_sqlite_open_writes_no_generation_row_and_the_store_is_named_by_its_file() {
    let dir = scratch("generation");
    let uri = StateStoreUri::Sqlite(dir.join("state.db"));
    assert_eq!(uri.identity(), dir.join("state.db"));
    for _ in 0..2 {
        let state = uri.open().expect("opened");
        assert_eq!(state.store.get(kbf_state::GENERATION).expect("read"), None);
    }
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
}

/// Catches: a path that cannot be a state file (a directory) taken, and an error that
/// does not say which.
#[test]
fn a_sqlite_value_that_cannot_be_opened_names_the_path() {
    let dir = scratch("bad");
    std::fs::create_dir_all(&dir).expect("dir");
    let refused = StateStoreUri::Sqlite(dir.to_path_buf())
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
    let mut lock = dir.as_os_str().to_owned();
    lock.push(".lock");
    let _ = std::fs::remove_file(lock);
}

/// Catches: the flag not reaching the server's arguments, a default other than the
/// in-memory scheduler, a Postgres socket URL the command line refuses, a URL with a
/// password accepted on the command line (it is world-readable), and a value of another
/// kind accepted.
#[test]
fn the_flag_takes_a_sqlite_path_or_a_postgres_socket_url_and_defaults_to_none() {
    let args = Args::try_parse_from(["kbf-server"]).expect("no flag");
    assert_eq!(args.state_store, None);
    assert!(args.open_state_store().expect("none").is_none());
    let args = Args::try_parse_from(["kbf-server", "--state-store=sqlite:///x.db"]).expect("flag");
    assert_eq!(
        args.state_store,
        Some(StateStoreUri::Sqlite(PathBuf::from("/x.db")))
    );
    let args = Args::try_parse_from([
        "kbf-server",
        "--state-store=postgres:///kbf?host=/run/postgresql",
    ])
    .expect("flag");
    assert_eq!(args.state_store, Some(StateStoreUri::local_postgres()));
    let refused =
        Args::try_parse_from(["kbf-server", "--state-store=file:///x"]).expect_err("scheme");
    assert!(
        refused
            .to_string()
            .contains("only postgres:///<database>?host=<socket directory> or sqlite:///"),
        "{refused}"
    );
    for password in [
        "postgres://kbf:hunter2@db.example/kbf",
        "postgres://db.example/kbf?password=hunter2",
    ] {
        let refused = Args::try_parse_from(["kbf-server", &format!("--state-store={password}")])
            .expect_err(password);
        let said = refused.to_string();
        assert!(said.contains("must carry no password"), "{said}");
        assert!(!said.contains("hunter2"), "{said}");
    }
}

/// Asserts that `--pack-index` is off by default and then needs no store, that given it names
/// the Postgres state store the index would be opened on, and that the three ways it cannot be
/// honoured are told apart: no state store, a SQLite one, and a Postgres one nothing listens on.
#[test]
fn the_pack_index_flag_is_off_by_default_and_needs_a_postgres_state_store() {
    let off = Args::try_parse_from(["kbf-server"]).expect("no flag");
    assert!(!off.pack_index);
    assert!(off.pack_index_url().expect("no flag").is_none());
    assert!(off.open_pack_index().expect("no flag").is_none());
    let mut args = Args::try_parse_from(["kbf-server", "--pack-index"]).expect("the flag");
    assert!(args.pack_index);
    let refused = args.open_pack_index().err().expect("no state store");
    assert!(matches!(refused, PackIndexFlagError::NeedsStateStore));
    let said = refused.to_string();
    assert!(said.contains("needs --state-store"), "{said}");
    args.state_store = Some(StateStoreUri::Sqlite(PathBuf::from("/x.db")));
    let refused = args.open_pack_index().err().expect("SQLite");
    assert!(matches!(refused, PackIndexFlagError::NeedsPostgres));
    let said = refused.to_string();
    assert!(said.contains("not SQLite"), "{said}");
    args.state_store = Some(StateStoreUri::local_postgres());
    let url = args.pack_index_url().expect("a Postgres store");
    assert!(url.is_some());
    let dir = scratch("pack-index-no-socket");
    let text = format!("postgres://kbf:hunter2@/kbf?host={}", dir.display());
    args.state_store = Some(parse(&text).expect("parses"));
    let refused = args.open_pack_index().err().expect("nothing listens");
    assert!(matches!(refused, PackIndexFlagError::Open(_)));
    let said = refused.to_string();
    assert!(said.starts_with("--pack-index: "), "{said}");
    assert!(!said.contains("hunter2"), "{said}");
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
        "postgres://kbf:hunter2@db/x?sslmode=require",
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

/// A Postgres store whose socket is not there yet is `NotUp`, which a start waits for,
/// and a SQLite path that cannot be a database is `Refused`, which it does not. Catches
/// the two sorts swapped, or both reported as one.
#[test]
fn a_store_not_up_is_told_from_a_store_refused() {
    let dir = crate::test_dir::short_dir("state-store-not-up");
    let url = format!(
        "postgres:///kbf?host={}",
        dir.path().join("absent").display()
    );
    let not_up = parse(&url).expect("a socket URL").try_open().err();
    assert!(matches!(not_up, Some(OpenFailure::NotUp(_))), "{not_up:?}");

    // A directory is not a database file.
    let sqlite = StateStoreUri::Sqlite(dir.path().to_path_buf());
    let refused = sqlite.try_open().err();
    assert!(
        matches!(refused, Some(OpenFailure::Refused(_))),
        "{refused:?}"
    );
}
