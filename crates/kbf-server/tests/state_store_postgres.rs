//! `--state-store=postgres://...` opened as a start opens it, against a real server.
//!
//! The tests are `#[ignore]`d so `cargo test --workspace` stays hermetic; the
//! `state-postgres` job in ci.yml runs them with `--include-ignored` against a service
//! container. `KBF_TEST_POSTGRES_URL` names a server they may create and drop databases on.
//! The schema a flag-opened store lives in is `kbf_state`, as the flag takes no other, so
//! each test makes a database of its own and uses that one: nextest runs every test in a
//! process of its own, side by side, and a process-wide lock cannot keep them off one
//! schema (the test that drops it saw the tables another was applying).

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use kbf_front::Cache;
use kbf_server::{EpochPolicy, Farm, StateError, StateStore, StateStoreUri};
use kbf_state::{Cond, GENERATION, StoreError, Txn};

const URL_VAR: &str = "KBF_TEST_POSTGRES_URL";

fn server_url() -> String {
    std::env::var(URL_VAR).unwrap_or_else(|_| {
        panic!("{URL_VAR} must name a postgres:// URL the tests may create databases in")
    })
}

static NEXT: AtomicU64 = AtomicU64::new(0);

/// A database made for one test and dropped with it, named `kbf_t_<pid>_<n>`: no other test
/// (of this process or another) reads or writes it.
struct Scratch {
    name: String,
}

impl Scratch {
    fn new() -> Scratch {
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let name = format!("kbf_t_{}_{n}", std::process::id());
        let mut admin =
            postgres::Client::connect(&server_url(), postgres::NoTls).expect("the test server");
        admin
            .batch_execute(&format!("CREATE DATABASE \"{name}\""))
            .expect("create the test's database");
        Scratch { name }
    }

    /// `KBF_TEST_POSTGRES_URL` with this database in place of its own.
    fn url(&self) -> String {
        with_database(&server_url(), &self.name)
    }

    /// The tables as `kbf-installer setup` leaves them: kbf creates no table itself.
    fn install_schema(&self) {
        let config =
            kbf_state::postgres::PostgresConfig::parse(&self.url()).expect("the URL parses");
        let schema = kbf_state::postgres::schema::embedded();
        kbf_state::postgres::schema::apply(&config, &schema, kbf_state::postgres::schema::latest())
            .expect("the installer's step");
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if let Ok(mut admin) = postgres::Client::connect(&server_url(), postgres::NoTls) {
            let _ = admin.batch_execute(&format!(
                "DROP DATABASE IF EXISTS \"{}\" WITH (FORCE)",
                self.name
            ));
        }
    }
}

/// `url` with its database (the path after the host) replaced, its query kept.
fn with_database(url: &str, database: &str) -> String {
    let (head, query) = url
        .split_once('?')
        .map_or((url, None), |(h, q)| (h, Some(q)));
    let (scheme, rest) = head.split_once("://").expect("a postgres:// URL");
    let authority = rest.split_once('/').map_or(rest, |(a, _)| a);
    let mut out = format!("{scheme}://{authority}/{database}");
    if let Some(query) = query {
        out.push('?');
        out.push_str(query);
    }
    out
}

#[test]
fn with_database_replaces_only_the_database() {
    for (from, to) in [
        ("postgres://u:p@h:5432/postgres", "postgres://u:p@h:5432/d"),
        (
            "postgres://u@h/x?sslmode=disable",
            "postgres://u@h/d?sslmode=disable",
        ),
        ("postgres://u@h", "postgres://u@h/d"),
    ] {
        assert_eq!(with_database(from, "d"), to);
    }
}

fn uri(url: &str) -> StateStoreUri {
    url.parse().expect("a postgres:// URL the store accepts")
}

fn generation(state: &StateStore) -> u64 {
    let row = state.store.get(GENERATION).expect("read").expect("a row");
    u64::from_be_bytes(row.value.try_into().expect("8 bytes"))
}

fn farm_over(state: StateStore) -> Farm {
    Farm::new(
        Arc::new(Cache::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    )
    .with_state(Some(state))
    .expect("a farm over the store")
}

/// Catches a start that creates the tables itself (the installer owns the schema) and a
/// refusal that is not one clear line: a start on a database nobody ran `setup` on fails
/// as a `StateError::Open` that says what to run, without the URL, and leaves no schema.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn a_start_on_a_database_without_the_installers_schema_is_refused() {
    let db = Scratch::new();
    let refused = uri(&db.url()).open().err().expect("refused");
    let said = refused.to_string();
    assert!(matches!(refused, StateError::Open(_)), "{said}");
    assert!(
        said.contains("kbf-installer setup") && !said.contains('\n'),
        "{said}"
    );
    assert!(!said.contains("kbf-ci-throwaway"), "{said}");
    let mut client = postgres::Client::connect(&db.url(), postgres::NoTls).expect("connect");
    let made = client
        .query(
            "SELECT 1 FROM pg_namespace WHERE nspname = 'kbf_state'",
            &[],
        )
        .expect("catalog");
    assert!(made.is_empty(), "the refused start created the schema");
}

/// Asserts that a Postgres `--state-store` is parsed and then opened; that a fresh store is
/// given an epoch of its own and the next start keeps it (every start keeps it, so a
/// frontend started over the database leaves the daemons their runs); that an open after a
/// handover expects the generation it was told; and that the store is opened with the
/// writer fence (a handle left over from an earlier writer could otherwise still write
/// after its successor started).
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn a_flag_opened_postgres_store_takes_generations_and_keeps_epochs() {
    let db = Scratch::new();
    db.install_schema();
    let uri = uri(&db.url());
    assert_eq!(uri.epoch_policy(), EpochPolicy::Keep);

    let first = uri.open().expect("a first start on a fresh database");
    let named: i64 = postgres::Client::connect(&db.url(), postgres::NoTls)
        .expect("connect")
        .query_one(
            "SELECT count(*) FROM pg_stat_activity WHERE application_name = 'kbf-server'",
            &[],
        )
        .expect("the activity view")
        .get(0);
    assert!(
        named >= 1,
        "the server's connections are named kbf-server in pg_stat_activity"
    );
    assert_eq!(first.epoch, EpochPolicy::Keep);
    assert_eq!(
        generation(&first),
        1,
        "a fresh store starts at generation 1"
    );
    let epoch = farm_over(StateStore {
        store: Arc::clone(&first.store),
        epoch: first.epoch,
        members: Default::default(),
        history: None,
    })
    .lease_epoch();
    assert!(epoch > 0, "a fresh store is given an epoch: {epoch}");

    let second = uri.open().expect("a second start");
    assert_eq!(generation(&second), 2);
    let write = || Txn::new().put("k", "v", Cond::Any);
    assert_eq!(
        first.store.txn(write()).unwrap_err(),
        StoreError::Fenced,
        "the first handle is fenced once the second has taken the next generation"
    );
    second
        .store
        .txn(write())
        .expect("the holder writes")
        .expect("commits");
    let kept = farm_over(second).lease_epoch();
    assert_eq!(kept, epoch, "a start moved the epoch");

    let refused = uri
        .open_after_handover(99)
        .err()
        .expect("not the generation");
    assert!(
        matches!(&refused, StateError::Open(why) if why.contains("generation 2, not the expected 99")),
        "{refused:?}"
    );
    let successor = uri
        .open_after_handover(2)
        .expect("the generation it was told");
    assert_eq!(successor.epoch, EpochPolicy::Keep);
    assert_eq!(generation(&successor), 3);
    assert_eq!(
        farm_over(successor).lease_epoch(),
        kept,
        "a handover keeps the epoch"
    );
}

/// Asserts that the operator's bump raises the epoch of a flag-opened Postgres store by
/// one, once, through the frontend that holds the generation; that the frontend goes on
/// serving the epoch it started under; and that the next start serves the raised epoch
/// and raises nothing more.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn a_bump_raises_the_epoch_of_a_flag_opened_postgres_store_once() {
    let db = Scratch::new();
    db.install_schema();
    let uri = uri(&db.url());
    let first = farm_over(uri.open().expect("a first start"));
    let epoch = first.lease_epoch();

    let bumped = first.bump_epoch().expect("a bump").expect("a state store");

    assert_eq!((bumped.previous, bumped.epoch), (epoch, epoch + 1));
    assert_eq!(first.lease_epoch(), epoch);
    let second = farm_over(uri.open().expect("a second start"));
    assert_eq!(second.lease_epoch(), epoch + 1);
    let again = farm_over(uri.open().expect("a third start"));
    assert_eq!(again.lease_epoch(), epoch + 1);
}

/// Catches: a Postgres server that acknowledges a commit before it is flushed
/// (`synchronous_commit` off) accepted as the scheduler's state, and a refusal that does
/// not name the setting or that quotes the URL's password. The URL here sets the value on
/// every connection it opens.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn a_server_that_does_not_flush_commits_refuses_the_start() {
    let db = Scratch::new();
    let base = db.url();
    let sep = if base.contains('?') { '&' } else { '?' };
    for setting in ["off", "local", "remote_write"] {
        let with = format!("{base}{sep}options=-c%20synchronous_commit%3D{setting}");
        let refused = uri(&with).open().err().expect("refused");
        let said = refused.to_string();
        assert!(matches!(refused, StateError::Open(_)), "{said}");
        assert!(
            said.contains("synchronous_commit") && said.contains(setting),
            "{setting}: {said}"
        );
        assert!(!said.contains("kbf-ci-throwaway"), "{said}");
    }
}
