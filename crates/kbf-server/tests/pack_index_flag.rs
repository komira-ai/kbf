//! `--pack-index` on a started frontend and a started cache server, against a real Postgres
//! server: either given the flag carries the pack index and one without it carries none, and a
//! frontend start on a schema without the pack tables is refused with the line that says what
//! to run. The flag's refusals that need no server (no state store, a SQLite one) are the cache
//! server's unit tests (`src/start.rs`).
//!
//! The tests need a server and fail without one: `KBF_TEST_POSTGRES_URL` names a server they
//! may create and drop databases on. Each test makes a database of its own.

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

/// A database of its own on the test server, made by `make` and dropped off the runtime
/// (see [`off_runtime`]), so that it is also dropped when a test fails.
struct Database(Option<TestDatabase>);

impl Database {
    fn new(make: impl FnOnce() -> TestDatabase + Send) -> Self {
        Self(Some(off_runtime(make)))
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
    let db = Database::new(test_database);
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

/// Asserts that a start with the flag on a schema that has no pack tables is refused with the
/// flag named and the installer command in the line, while a start without the flag on the
/// same schema works.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_frontend_on_a_schema_without_the_pack_tables_is_refused_at_start() {
    let db = Database::new(before_the_pack_tables);
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
    let db = Database::new(test_database);
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
