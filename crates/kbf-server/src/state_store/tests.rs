//! `--state-store` values.

use std::path::PathBuf;

use clap::Parser;

use super::*;
use crate::Args;

fn parse(value: &str) -> Result<StateStoreUri, StateStoreUriError> {
    value.parse()
}

/// Catches: a scheme the design does not admit accepted (a `file://` or `mysql://` store
/// the server could not open), a relative or empty SQLite path (a database created
/// wherever the process happens to run), an empty Postgres value, and a refusal that
/// echoes more than the scheme (a mistyped Postgres URL carries its password).
#[test]
fn only_sqlite_and_postgres_schemes_are_accepted() {
    assert_eq!(
        parse("sqlite:///var/lib/kbf/state.db"),
        Ok(StateStoreUri::Sqlite(PathBuf::from(
            "/var/lib/kbf/state.db"
        )))
    );
    assert_eq!(
        parse("postgres://kbf@db.example/state"),
        Ok(StateStoreUri::Postgres(
            "postgres://kbf@db.example/state".to_owned()
        ))
    );
    assert_eq!(
        parse("sqlite://state.db"),
        Err(StateStoreUriError::SqlitePath)
    );
    assert_eq!(parse("sqlite://"), Err(StateStoreUriError::SqlitePath));
    assert_eq!(parse("sqlite:///"), Err(StateStoreUriError::SqlitePath));
    assert_eq!(parse("postgres://"), Err(StateStoreUriError::PostgresEmpty));
    assert_eq!(
        parse("file:///var/lib/kbf"),
        Err(StateStoreUriError::Scheme("file".to_owned()))
    );
    assert_eq!(
        parse("postgresql://kbf:hunter2@db/state"),
        Err(StateStoreUriError::Scheme("postgresql".to_owned()))
    );
    assert_eq!(
        parse("/var/lib/kbf/state.db"),
        Err(StateStoreUriError::Scheme(
            "/var/lib/kbf/state.db".to_owned()
        ))
    );
}

/// Catches: a Postgres server named by `Debug` (the flags are logged and shown in panics),
/// and the SQLite path lost from it.
#[test]
fn a_postgres_url_is_never_printed() {
    let url = parse("postgres://kbf@db.example/state").expect("postgres");
    assert!(!format!("{url:?}").contains("db.example"));
    let file = parse("sqlite:///x.db").expect("sqlite");
    assert!(format!("{file:?}").contains("x.db"));
}

/// Catches: a password accepted on the command line, which every user of the machine can
/// read (in user information or as a parameter, whatever the case of the scheme's rest),
/// and a refusal that echoes it. A user, a port, a path and other parameters pass.
#[test]
fn a_postgres_url_with_a_password_is_refused_without_echoing_it() {
    for url in [
        "postgres://kbf:hunter2@db/state",
        "postgres://:hunter2@db/state",
        "postgres://db/state?user=kbf&password=hunter2",
        "postgres://db?password=hunter2",
    ] {
        let refused = parse(url).expect_err(url);
        assert_eq!(refused, StateStoreUriError::PostgresPassword, "{url}");
        assert!(
            !format!("{refused} {refused:?}").contains("hunter2"),
            "{url}"
        );
    }
    for url in [
        "postgres://kbf@db.example:5432/state",
        "postgres://db/state?application_name=kbf",
        "postgres://db/state?options=-c%20a@b:c",
    ] {
        assert!(parse(url).is_ok(), "{url}");
    }
}

/// Catches: the epoch policy swapped between the schemes: a SQLite start is a new
/// incarnation (a restored or swapped file must never resume old leases), a Postgres
/// start joins the database's.
#[test]
fn sqlite_bumps_the_epoch_and_postgres_keeps_it() {
    let sqlite = parse("sqlite:///x.db").expect("sqlite");
    let postgres = parse("postgres://db/x").expect("postgres");
    assert_eq!(sqlite.epoch_policy(), EpochPolicy::Bump);
    assert_eq!(postgres.epoch_policy(), EpochPolicy::Keep);
    assert_eq!((sqlite.scheme(), postgres.scheme()), ("sqlite", "postgres"));
}

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "kbf-server-state-uri-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Catches: a `sqlite://` store that is not opened (the scheduler would run in memory
/// behind a flag that promises durability), one whose directory must exist beforehand,
/// one that does not hold its file against a second frontend, and a password given to a
/// store with none. The epoch policy comes with the store.
#[test]
fn a_sqlite_value_opens_its_file_and_holds_it() {
    let dir = scratch("open");
    let file = dir.join("deeper").join("state.db");
    let uri = StateStoreUri::Sqlite(file.clone());
    let store = uri.open(None).expect("opened");
    assert!(file.exists(), "the file is made, with its directory");
    assert_eq!(store.epoch, EpochPolicy::Bump);
    let refused = uri.open(None).err().expect("held");
    assert!(
        matches!(&refused, StateError::Open(why) if why.contains("another kbf process")),
        "{refused:?}"
    );
    drop(store);
    let refused = uri.open(Some("secret")).err().expect("no password");
    assert!(matches!(refused, StateError::Open(_)), "{refused:?}");
    assert!(!refused.to_string().contains("secret"));
    uri.open(None).expect("free once the first is gone");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Catches: a path that cannot be a state file (a directory) taken, and an error that
/// does not say which.
#[test]
fn a_sqlite_value_that_cannot_be_opened_names_the_path() {
    let dir = scratch("bad");
    std::fs::create_dir_all(&dir).expect("dir");
    let refused = StateStoreUri::Sqlite(dir.clone())
        .open(None)
        .err()
        .expect("a directory");
    assert!(matches!(refused, StateError::Open(_)), "{refused:?}");
    assert!(
        refused.to_string().contains(&dir.display().to_string()),
        "{refused}"
    );
    let _ = std::fs::remove_dir_all(&dir);
    let mut lock = dir.into_os_string();
    lock.push(".lock");
    let _ = std::fs::remove_file(lock);
}

/// Catches: a build without the Postgres client that silently falls back to something
/// else (an in-memory store behind a flag that promises durability), or refuses without
/// saying how to get the backend.
#[cfg(not(feature = "postgres"))]
#[test]
fn postgres_without_the_feature_is_refused_naming_it() {
    let refused = parse("postgres://db/x")
        .expect("parses")
        .open(None)
        .err()
        .expect("refused");
    assert!(
        matches!(refused, StateError::NoBackend("postgres")),
        "{refused:?}"
    );
    assert!(
        refused.to_string().contains("`postgres` feature"),
        "{refused}"
    );
}

/// Catches: the password file read wrongly: its line ending kept (the server would reject
/// every login), only its first line wanted, an empty file taken as no password, and any
/// error that carries what the file holds.
#[test]
fn the_password_is_the_first_line_of_its_file() {
    let dir = scratch("password");
    std::fs::create_dir_all(&dir).expect("dir");
    let file = dir.join("pw");
    std::fs::write(&file, "hunter2\nignored\n").expect("write");
    assert_eq!(read_password(&file).expect("read"), "hunter2");
    std::fs::write(&file, "hunter2").expect("write");
    assert_eq!(read_password(&file).expect("no newline"), "hunter2");
    std::fs::write(&file, "\n").expect("write");
    let empty = read_password(&file).expect_err("empty");
    assert!(empty.to_string().contains("is empty"), "{empty}");
    let missing = read_password(&dir.join("absent")).expect_err("missing");
    assert!(missing.to_string().contains("absent"), "{missing}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Catches: the password file used without a store to use it on (a flag that does
/// nothing), and a start that reads it when the flag is left out.
#[test]
fn the_password_file_needs_a_state_store() {
    let refused = Args::try_parse_from(["kbf-server", "--state-store-password-file=/x"])
        .expect_err("needs --state-store");
    assert!(refused.to_string().contains("--state-store"), "{refused}");
    let args = Args::try_parse_from([
        "kbf-server",
        "--state-store=postgres://db/x",
        "--state-store-password-file=/x",
    ])
    .expect("both");
    assert_eq!(args.state_store_password_file, Some(PathBuf::from("/x")));
    assert!(
        Args::parse_from(["kbf-server"])
            .open_state_store()
            .expect("none")
            .is_none()
    );
}

/// Catches: the flag not reaching the server's arguments, a default other than the
/// in-memory scheduler, and a value of another scheme accepted by the command line.
#[test]
fn the_flag_takes_its_two_schemes_and_defaults_to_none() {
    let args = Args::try_parse_from(["kbf-server"]).expect("no flag");
    assert_eq!(args.state_store, None);
    let args = Args::try_parse_from(["kbf-server", "--state-store=sqlite:///x.db"]).expect("flag");
    assert_eq!(
        args.state_store,
        Some(StateStoreUri::Sqlite(PathBuf::from("/x.db")))
    );
    let refused =
        Args::try_parse_from(["kbf-server", "--state-store=file:///x"]).expect_err("scheme");
    assert!(refused.to_string().contains("scheme \"file\""), "{refused}");
}

/// A store that fails when a call is made from a thread that is running a tokio runtime,
/// as the Postgres client does (it panics there), over the fake.
struct Probe(kbf_state::fake::Fake);

fn assert_off_the_runtime() {
    assert!(
        tokio::runtime::Handle::try_current().is_err(),
        "called from a thread running a runtime"
    );
}

impl Store for Probe {
    fn apply_batch(
        &self,
        batch: Vec<kbf_state::Txn>,
    ) -> Result<Vec<Result<kbf_state::Commit, kbf_state::Conflict>>, kbf_state::StoreError> {
        assert_off_the_runtime();
        self.0.apply_batch(batch)
    }

    fn get(&self, key: &str) -> Result<Option<kbf_state::Versioned>, kbf_state::StoreError> {
        assert_off_the_runtime();
        self.0.get(key)
    }

    fn scan_prefix(
        &self,
        prefix: &str,
    ) -> Result<Vec<(String, kbf_state::Versioned)>, kbf_state::StoreError> {
        assert_off_the_runtime();
        self.0.scan_prefix(prefix)
    }

    fn changes(
        &self,
        after: kbf_state::Revision,
        limit: usize,
    ) -> Result<kbf_state::Changes, kbf_state::StoreError> {
        assert_off_the_runtime();
        self.0.changes(after, limit)
    }

    fn reap_expired(&self) -> Result<usize, kbf_state::StoreError> {
        assert_off_the_runtime();
        self.0.reap_expired()
    }

    fn commit_stats(&self) -> kbf_state::CommitStats {
        assert_off_the_runtime();
        self.0.commit_stats()
    }

    fn now(&self) -> kbf_state::StoreTime {
        assert_off_the_runtime();
        self.0.now()
    }
}

/// Catches: a blocking store client called on a runtime thread (the Postgres client
/// panics there, and every call of a handler or of an async start is one): each of the
/// store's seven calls, made inside a runtime, runs on another thread. The control is
/// the probe itself, which does fail inside a runtime.
#[tokio::test(flavor = "current_thread")]
async fn a_wrapped_stores_calls_run_off_the_runtime() {
    use kbf_state::fake::{Fake, Flaw};
    let fake = || Fake::with_flaw(Arc::new(kbf_state::ManualClock::default()), Flaw::None);
    let control = Probe(fake());
    let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| control.get("k")));
    assert!(ran.is_err(), "the probe does not see the runtime");

    let store = OffRuntime(Probe(fake()));
    let put = kbf_state::Txn::new().put("k", "v", kbf_state::Cond::Any);
    store.txn(put).expect("store").expect("put");
    assert!(store.get("k").expect("get").is_some());
    assert_eq!(store.scan_prefix("k").expect("scan").len(), 1);
    store
        .changes(kbf_state::Revision::default(), 10)
        .expect("changes");
    store.reap_expired().expect("reap");
    let _ = (store.commit_stats(), store.now());
}
