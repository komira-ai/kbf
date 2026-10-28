//! `--pack-index` on a started frontend, against a real Postgres server: a frontend given the
//! flag carries the pack index and one without it carries none, and a start on a schema
//! without the pack tables is refused with the line that says what to run.
//!
//! The tests need a server and fail without one: `KBF_TEST_POSTGRES_URL` names a server they
//! may create and drop databases on. Each test makes a database of its own.

use std::error::Error;
use std::future::pending;
use std::sync::Arc;

use kbf_pack_index::schema::test_database;
use kbf_pack_index::{BlobDigest, NewPack, PackBlob, PackIndex};
use kbf_server::frontend::{self, FrontendFlags, FrontendHandle, start_frontend};
use kbf_state::postgres::{PostgresConfig, TestDatabase, schema};

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
