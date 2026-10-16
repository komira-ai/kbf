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

/// Catches: a Postgres URL printed by `Debug` (the flags are logged and shown in panics
/// with their password), and the SQLite path lost from it.
#[test]
fn a_postgres_url_is_never_printed() {
    let url = parse("postgres://kbf:hunter2@db/state").expect("postgres");
    assert!(!format!("{url:?}").contains("hunter2"));
    let file = parse("sqlite:///x.db").expect("sqlite");
    assert!(format!("{file:?}").contains("x.db"));
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

/// Catches: a backend that this build does not have opened silently as something else
/// (an in-memory store behind a flag that promises durability): the start is refused,
/// naming the scheme.
#[test]
fn a_scheme_without_a_backend_is_refused_at_open() {
    for (value, scheme) in [
        ("sqlite:///x.db", "sqlite"),
        ("postgres://db/x", "postgres"),
    ] {
        let refused = parse(value).expect(value).open().err().expect("refused");
        assert!(
            matches!(refused, StateError::NoBackend(s) if s == scheme),
            "{refused:?}"
        );
        assert!(refused.to_string().contains(scheme));
    }
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
