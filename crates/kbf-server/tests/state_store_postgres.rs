//! `--state-store=postgres://...` opened as a start opens it, against a real server.
//!
//! The tests need a server and fail without one: `KBF_TEST_POSTGRES_URL` names a server they
//! may create and drop databases on. The schema a flag-opened store lives in is `kbf_state`,
//! as the flag takes no other, so each test makes a database of its own and uses that one:
//! nextest runs every test in a process of its own, side by side, and a process-wide lock
//! cannot keep them off one schema (the test that drops it saw the tables another was
//! applying).

use std::sync::Arc;

use kbf_front::Cache;
use kbf_server::{EpochPolicy, Farm, StateError, StateStore, StateStoreUri};
use kbf_state::postgres::{PostgresConfig, PostgresStore, TestDatabase, test_database};
use kbf_state::{
    Cond, Fenced, GENERATION, SHARED_WRITERS, Store, StoreError, TakeError, Txn, decode_generation,
};

fn uri(url: &str) -> StateStoreUri {
    url.parse().expect("a postgres:// URL the store accepts")
}

/// Asserts that the generation row of `store` holds the marker of shared frontends and
/// nothing else, which a build that takes generations cannot read as a generation.
fn assert_marked(store: &dyn Store) {
    let row = store.get(GENERATION).expect("read").expect("a row");
    assert_eq!(row.value, SHARED_WRITERS);
    assert_eq!(decode_generation(&row.value), Err(TakeError::Corrupt));
}

/// A handle on the database that is not a frontend's: what an older build opens before it
/// takes a generation.
fn raw(db: &TestDatabase) -> Arc<dyn Store> {
    let config = PostgresConfig::parse(&db.url()).expect("a config");
    Arc::new(PostgresStore::open(&config).expect("a connection"))
}

fn write() -> Txn {
    Txn::new().put("k", "v", Cond::Any)
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
fn a_start_on_a_database_without_the_installers_schema_is_refused() {
    let db = TestDatabase::empty();
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
/// frontend started over the database leaves the daemons their runs); that no start takes a
/// generation, the generation row holding the marker of shared frontends after the first
/// and after the second; that the first frontend is not fenced by the second, in its store
/// handle or in its farm; and that an open that expects a generation is refused, with the
/// store as it was.
#[test]
fn a_flag_opened_postgres_store_is_shared_and_keeps_epochs() {
    let db = test_database();
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
    assert_marked(first.store.as_ref());
    let first_farm = farm_over(StateStore {
        store: Arc::clone(&first.store),
        epoch: first.epoch,
        members: Default::default(),
        history: None,
    });
    let epoch = first_farm.lease_epoch();
    assert!(epoch > 0, "a fresh store is given an epoch: {epoch}");

    let second = uri.open().expect("a second start");
    assert_marked(second.store.as_ref());
    for state in [&first, &second] {
        state
            .store
            .txn(write())
            .expect("no start fenced the handle")
            .expect("commits");
    }
    let kept = farm_over(second).lease_epoch();
    assert_eq!(kept, epoch, "a start moved the epoch");
    first_farm
        .bump_epoch()
        .expect("the first frontend's farm writes")
        .expect("a state store");
    assert!(!first_farm.superseded(), "a start fenced the first");

    let refused = uri
        .open_after_handover(1)
        .err()
        .expect("no generation to succeed");
    assert!(
        matches!(&refused, StateError::Open(why) if why.contains("not an 8-byte number")),
        "{refused:?}"
    );
    assert_marked(first.store.as_ref());
}

/// Asserts that frontends started over one database both serve and neither is fenced: each
/// of four starts made together writes a row of its own, all four rows are there, and the
/// generation row holds the marker and nothing else, whichever start wrote it first.
#[test]
fn frontends_that_open_a_new_database_together_all_write_and_leave_the_marker() {
    let db = test_database();
    let url = db.url();
    std::thread::scope(|scope| {
        for n in 0..4 {
            let url = &url;
            scope.spawn(move || {
                let state = uri(url).open().expect("opens");
                let row = Txn::new().put(format!("k{n}"), "v", Cond::Any);
                state.store.txn(row).expect("not fenced").expect("commits");
            });
        }
    });
    let store = raw(&db);
    assert_marked(store.as_ref());
    assert_eq!(store.scan_prefix("k").expect("scan").len(), 4);
}

/// Asserts that opening a store that holds a generation (as a build that takes generations
/// left it) replaces the generation with the marker, which is the cold cutover: the old
/// build's handle is fenced and writes nothing, a build that takes generations is refused
/// on the store as corrupt, the new frontend writes, and a second open changes nothing.
#[test]
fn a_store_that_holds_a_generation_is_converted_to_the_marker_on_open() {
    let db = test_database();
    let uri = uri(&db.url());
    let old = Fenced::take(raw(&db), None).expect("an older build takes generation 1");
    old.txn(write()).expect("store").expect("commits");
    let before = old.get(GENERATION).expect("read").expect("a row");
    assert_eq!(decode_generation(&before.value), Ok(1));

    let opened = uri.open().expect("opens");

    assert_marked(opened.store.as_ref());
    assert_eq!(old.txn(write()).unwrap_err(), StoreError::Fenced);
    let refused = Fenced::take(raw(&db), None).expect_err("a build that takes generations");
    assert_eq!(refused, TakeError::Corrupt);
    opened.store.txn(write()).expect("store").expect("commits");
    let marked = opened.store.get(GENERATION).expect("read");
    let again = uri.open().expect("opens again");
    assert_eq!(again.store.get(GENERATION).expect("read"), marked);
}

/// Asserts that opening a store whose generation row holds neither the marker nor a
/// generation is refused as a `StateError::Open` that names the row, and that the row is
/// left as it was.
#[test]
fn a_store_whose_generation_row_is_junk_is_refused_on_open() {
    let db = test_database();
    let junk = raw(&db);
    let row = Txn::new().put(GENERATION, "junk", Cond::Any);
    junk.txn(row).expect("store").expect("commits");
    let before = junk.get(GENERATION).expect("read");

    let refused = uri(&db.url()).open().err().expect("refused");

    assert!(
        matches!(&refused, StateError::Open(why) if why.contains("neither a generation")),
        "{refused:?}"
    );
    assert_eq!(junk.get(GENERATION).expect("read"), before);
}

/// Asserts that the operator's bump raises the epoch of a flag-opened Postgres store by
/// one, once, through a frontend; that the frontend goes on serving the epoch it started
/// under; and that the next start serves the raised epoch and raises nothing more.
#[test]
fn a_bump_raises_the_epoch_of_a_flag_opened_postgres_store_once() {
    let db = test_database();
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
fn a_server_that_does_not_flush_commits_refuses_the_start() {
    let db = TestDatabase::empty();
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
