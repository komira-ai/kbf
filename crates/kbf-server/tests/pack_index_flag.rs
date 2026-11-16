//! `--pack-index` on a started frontend and a started cache server, against a real Postgres
//! server: either given the flag carries the pack index and one without it carries none, and a
//! frontend start on a schema without the pack tables is refused with the line that says what
//! to run. The flag's refusals that need no server (no state store, a SQLite one) are the cache
//! server's unit tests (`src/start.rs`).
//!
//! The tests need a server: `KBF_TEST_POSTGRES_URL` names a server they may create and drop
//! databases on. Each test makes a database of its own. Without the variable they fail under
//! CI (where the platform sets `CI`, and the job's service container is meant to be named), and
//! elsewhere they pass without running, after one line on stderr that names the variable
//! ([`skip_reason`]).

use std::error::Error;
use std::future::pending;
use std::path::Path;
use std::sync::Arc;

use clap::Parser as _;
use kbf_meta::Retention;
use kbf_objstore::{Floor, FsStore};
use kbf_pack_index::schema::test_database;
use kbf_pack_index::{BlobDigest, NewPack, PackBlob, PackIndex};
use kbf_server::frontend::{self, FrontendFlags, FrontendHandle, start_frontend};
use kbf_server::{Args, RaftMetaLog, ServerHandle, interrupted, serve_with};
use kbf_state::postgres::{PostgresConfig, TestDatabase, schema};
use kbf_testkit::dir::TestDir;

/// The flags of a frontend on loopback ports over the state store at `url`, with
/// `--pack-index` as `pack_index` says.
fn flags(url: &str, pack_index: bool) -> FrontendFlags {
    let argv = [
        "frontend",
        "--storage=http://127.0.0.1:1",
        "--listen=127.0.0.1:0",
        "--worker-listen=127.0.0.1:0",
        "--api-listen=127.0.0.1:0",
    ];
    let command = frontend::command();
    let matches = command.try_get_matches_from(argv).expect("flags");
    let mut flags = frontend::parse(&matches).expect("a frontend's flags");
    flags.server.state_store = Some(url.parse().expect("a postgres URL"));
    flags.server.pack_index = pack_index;
    flags
}

fn started(flags: &FrontendFlags) -> Result<FrontendHandle, Box<dyn Error>> {
    start_frontend(flags, || Ok(pending::<()>()))
}

/// Runs `f` on a thread of its own and returns its result. A test database makes a runtime
/// of its own to make and to drop a database, and tokio refuses that on a thread that is
/// running tasks.
fn off_runtime<R: Send>(f: impl FnOnce() -> R + Send) -> R {
    std::thread::scope(|scope| match scope.spawn(f).join() {
        Ok(result) => result,
        Err(panic) => std::panic::resume_unwind(panic),
    })
}

/// The variable that names the test server.
const URL_VAR: &str = "KBF_TEST_POSTGRES_URL";

/// Why a test that needs the test server does not run, or `None` when it runs: it runs when
/// the server is named (`url_set`), and under CI (`in_ci`) whether or not it is, so that a job
/// that lost the variable fails, naming it, instead of passing without the tests.
fn skip_reason(url_set: bool, in_ci: bool) -> Option<&'static str> {
    (!url_set && !in_ci).then_some(
        "skipped: KBF_TEST_POSTGRES_URL is not set, so there is no Postgres test server; \
         set it to postgres://USER@HOST:PORT/postgres to run this test (under CI it fails instead)",
    )
}

/// A database of its own on the test server, made by `make` and dropped off the runtime
/// (see [`off_runtime`]), so that it is also dropped when a test fails.
struct Database(Option<TestDatabase>);

impl Database {
    /// The database `make` makes, or `None`, after the reason on stderr, when
    /// [`skip_reason`] gives one.
    fn new(make: impl FnOnce() -> TestDatabase + Send) -> Option<Self> {
        let url_set = std::env::var_os(URL_VAR).is_some();
        let in_ci = std::env::var_os("CI").is_some();
        if let Some(reason) = skip_reason(url_set, in_ci) {
            eprintln!("{reason}");
            return None;
        }
        Some(Self(Some(off_runtime(make))))
    }

    /// The URL of the database.
    fn url(&self) -> String {
        self.0.as_ref().expect("held until dropped").url()
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        if let Some(database) = self.0.take() {
            off_runtime(move || drop(database));
        }
    }
}

/// A database whose schema stops at the first migration, before the pack tables.
fn before_the_pack_tables() -> TestDatabase {
    let db = TestDatabase::empty();
    let config = PostgresConfig::parse(&db.url()).expect("parses");
    schema::apply(&config, &schema::embedded(), 1).expect("the first migration only");
    db
}

/// A pack of one blob.
fn pack(key: &str) -> NewPack {
    let digest = BlobDigest {
        hash: [7; 32],
        size: 10,
    };
    NewPack {
        key: key.to_owned(),
        writer: "fe1".to_owned(),
        size: 26,
        blobs: vec![PackBlob { digest, offset: 0 }],
    }
}

/// Asserts that the frontend carries the pack index only when the flag is given, and that the
/// index it carries works: a pack recorded through it is found.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_carries_the_pack_index_when_the_flag_is_given() {
    let Some(db) = Database::new(test_database) else {
        return;
    };
    let off = started(&flags(&db.url(), false)).expect("starts without the flag");
    assert!(off.pack_index().is_none());
    drop(off);
    let on = started(&flags(&db.url(), true)).expect("starts with the flag");
    let index: Arc<dyn PackIndex> = on.pack_index().expect("the index is open");
    let key = "cas/f/fe1/1/1";
    let recorded = index.record_pack(&pack(key)).expect("recorded");
    assert!(recorded.created);
    let digest = pack(key).blobs[0].digest;
    let found = index.locate(&[digest]).expect("located");
    let first = found[0].as_ref().expect("a row");
    assert_eq!(first.pack_key, key);
    assert_eq!(first.offset, 0);
}

/// Asserts that `open_indexes` gives the frontend with no storage node one Postgres handle in two
/// views: nothing without the flag, and with it the pack index and the action cache table, so
/// that a put of an action and of the pack of its output is one transaction (a pack the index
/// refuses leaves no row) and the pack is found through the pack view afterwards.
#[test]
fn the_indexes_are_one_handle_with_the_action_cache_beside_the_pack_index() {
    let Some(db) = Database::new(test_database) else {
        return;
    };
    let off = flags(&db.url(), false).server.open_indexes();
    assert!(off.expect("opens").is_none());
    let both = flags(&db.url(), true)
        .server
        .open_indexes()
        .expect("opens")
        .expect("the indexes are open");
    let action = BlobDigest {
        hash: [9; 32],
        size: 3,
    };
    let result = b"an encoded result";

    let put = both
        .actions
        .put_action(&action, result, &[pack("cas/f/fe1/2/1")])
        .expect("put");
    assert!(!put.replaced);
    let found = both
        .actions
        .get_action(&action)
        .expect("get")
        .expect("a row");
    assert_eq!(found.result, result);
    let located = both
        .packs
        .locate(&[pack("x").blobs[0].digest])
        .expect("located");
    assert_eq!(
        located[0].as_ref().expect("a row").pack_key,
        "cas/f/fe1/2/1"
    );

    let refused = NewPack {
        size: 0,
        ..pack("cas/f/fe1/2/2")
    };
    let other = BlobDigest {
        hash: [8; 32],
        size: 3,
    };
    both.actions
        .put_action(&other, result, &[refused])
        .expect_err("a pack that cannot exist is refused");
    assert!(both.actions.get_action(&other).expect("get").is_none());
}

/// Asserts that a start with the flag on a schema that has no pack tables is refused with the
/// flag named and the installer command in the line, while a start without the flag on the
/// same schema works.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_on_a_schema_without_the_pack_tables_is_refused_at_start() {
    let Some(db) = Database::new(before_the_pack_tables) else {
        return;
    };
    let refused = started(&flags(&db.url(), true));
    let said = refused.err().expect("refused").to_string();
    assert!(said.starts_with("--pack-index: "), "{said}");
    assert!(said.contains("kbf-installer setup"), "{said}");
    started(&flags(&db.url(), false)).expect("starts without the flag");
}

/// Starts a cache server over `args` on a metadata log and a cold store of its own, in the
/// directory `dir` under `root`.
async fn boot(args: &Args, root: &Path, dir: &str) -> Result<ServerHandle, Box<dyn Error>> {
    let log = RaftMetaLog::open_dir(&root.join(dir), Retention::default(), "kbf/")
        .await
        .expect("open the log");
    let (prefix, id) = (log.store_prefix(), log.log_id());
    let store =
        FsStore::open(&root.join(format!("{dir}-blobs")), Floor::Bytes(1)).expect("open the store");
    serve_with(args, None, log, store, prefix, id, interrupted).await
}

/// Asserts that `--pack-index` reaches the cache server [`serve_with`] starts: over a Postgres
/// state store a server given the flag carries the index and one without it carries none.
#[tokio::test(flavor = "current_thread")]
async fn a_started_server_carries_the_pack_index_the_flag_names() {
    let Some(db) = Database::new(test_database) else {
        return;
    };
    let root = TestDir::under(env!("CARGO_TARGET_TMPDIR"), "pack-index-server");
    let mut args = Args::parse_from([
        "kbf-server",
        "--listen=127.0.0.1:0",
        "--worker-listen=127.0.0.1:0",
    ]);
    args.state_store = Some(db.url().parse().expect("a postgres URL"));
    let off = boot(&args, root.path(), "off").await;
    let off = off.expect("starts without the flag");
    assert!(off.pack_index().is_none());
    drop(off);
    args.pack_index = true;
    let on = boot(&args, root.path(), "on").await;
    let on = on.expect("starts with the flag");
    assert!(on.pack_index().is_some());
}

/// Asserts when a test that needs the test server runs: always when the server is named, and
/// under CI also when it is not (the test then fails naming the variable); only outside CI
/// with no server named does it not run, and the line it prints names the variable.
#[test]
fn a_database_test_runs_unless_no_server_is_named_outside_ci() {
    assert_eq!(skip_reason(true, false), None);
    assert_eq!(skip_reason(true, true), None);
    assert_eq!(skip_reason(false, true), None);
    assert_eq!(
        skip_reason(false, false),
        Some(
            "skipped: KBF_TEST_POSTGRES_URL is not set, so there is no Postgres test server; \
             set it to postgres://USER@HOST:PORT/postgres to run this test (under CI it fails \
             instead)"
        )
    );
}

/// Asserts that a value made off the runtime comes back, and that a panic there reaches the
/// test with its own message (a `join().expect(..)` would end in `Any { .. }` instead).
#[test]
fn off_runtime_returns_the_value_and_re_raises_the_panic_as_it_was() {
    assert_eq!(off_runtime(|| 7), 7);
    let said = "a Postgres test server: the reason";
    let raised = std::panic::catch_unwind(|| off_runtime(|| -> u8 { std::panic::panic_any(said) }));
    let payload = raised.expect_err("the panic reaches the caller");
    assert_eq!(payload.downcast_ref::<&str>(), Some(&said));
}
