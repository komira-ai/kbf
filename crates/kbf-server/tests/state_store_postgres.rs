//! `--state-store=postgres://...` opened as a start opens it, against a real server.
//!
//! The tests are `#[ignore]`d so `cargo test --workspace` stays hermetic; the
//! `state-postgres` job in ci.yml runs them with `--include-ignored` against a service
//! container. `KBF_TEST_POSTGRES_URL` names a database they may create and drop the
//! `kbf_state` schema in (the schema a flag-opened store lives in, as the flag takes no
//! other). One test holds that schema from start to end, so the checks that need the same
//! rows run in order inside it.

use std::sync::Arc;

use kbf_front::Cache;
use kbf_server::{EpochPolicy, Farm, StateError, StateStore, StateStoreUri};
use kbf_state::{Cond, GENERATION, StoreError, Txn};

const URL_VAR: &str = "KBF_TEST_POSTGRES_URL";

fn url() -> String {
    std::env::var(URL_VAR).unwrap_or_else(|_| {
        panic!("{URL_VAR} must name a postgres:// URL the tests may create schemas in")
    })
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

/// The default schema, dropped, so the test starts on a store that has never been used.
fn fresh_database() {
    let mut client = postgres::Client::connect(&url(), postgres::NoTls).expect("connect");
    client
        .batch_execute("DROP SCHEMA IF EXISTS kbf_state CASCADE")
        .expect("drop the schema");
}

/// Catches: a Postgres `--state-store` that is parsed and then not opened; a fresh store
/// that is not given an epoch of its own, or one that is not bumped by the next start; an
/// open after a handover that bumps the epoch or does not expect the generation it was
/// told; and a store opened without the writer fence (a handle left over from an earlier
/// writer could still write after its successor started).
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn a_flag_opened_postgres_store_takes_generations_and_epochs_as_sqlite_does() {
    fresh_database();
    let uri = uri(&url());
    assert_eq!(uri.epoch_policy(), EpochPolicy::Bump);

    let first = uri.open().expect("a first start on a fresh database");
    let named: i64 = postgres::Client::connect(&url(), postgres::NoTls)
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
    assert_eq!(first.epoch, EpochPolicy::Bump);
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
    let bumped = farm_over(second).lease_epoch();
    assert_eq!(
        bumped,
        epoch + 1,
        "every start but a handover is a new epoch"
    );

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
        bumped,
        "a handover keeps the epoch"
    );
}

/// Catches: a Postgres server that acknowledges a commit before it is flushed
/// (`synchronous_commit` off) accepted as the scheduler's state, and a refusal that does
/// not name the setting or that quotes the URL's password. The URL here sets the value on
/// every connection it opens.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn a_server_that_does_not_flush_commits_refuses_the_start() {
    let base = url();
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
