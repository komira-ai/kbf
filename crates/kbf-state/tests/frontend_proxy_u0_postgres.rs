//! Unit U0 of `docs/design/frontend-proxy.md` against a real Postgres server: that the
//! generation fence is atomic with each write (section 8), and that the Unix-socket URL the
//! design gives (section 11) opens a store. The report is
//! `docs/design/frontend-proxy-u0.md`.
//!
//! The fence tests are `#[ignore]`d like the rest of this crate's server tests and run in the
//! `state-postgres` CI job (`KBF_TEST_POSTGRES_URL`). The socket tests also need
//! `KBF_TEST_POSTGRES_SOCKET_DIR`, the directory the same server's Unix socket is in; they
//! print that they skipped when it is not set, because the CI service container shares no
//! socket directory with the job.
#![cfg(feature = "postgres")]

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use kbf_state::postgres::{Flaw, PostgresBackend, PostgresConfig, PostgresStore};
use kbf_state::{Cond, Fenced, GENERATION, Store, StoreError, Txn};
use postgres::{Client, NoTls};

const URL_VAR: &str = "KBF_TEST_POSTGRES_URL";
const SOCKET_VAR: &str = "KBF_TEST_POSTGRES_SOCKET_DIR";

fn url() -> String {
    std::env::var(URL_VAR).unwrap_or_else(|_| panic!("{URL_VAR} must name a postgres:// URL"))
}

/// A store in a schema of its own (dropped with the backend).
fn store(backend: &PostgresBackend) -> (Arc<PostgresStore>, String) {
    let config = backend.fresh_config();
    let schema = config.schema().to_owned();
    let store = PostgresStore::open(&config).expect("the test database opens");
    (Arc::new(store), schema)
}

/// Writers of the schema's connections that are blocked on a lock right now.
fn waiting(schema: &str) -> i64 {
    let mut client = Client::connect(&url(), NoTls).expect("a monitoring connection");
    client
        .query_one(
            "SELECT count(*) FROM pg_stat_activity \
             WHERE application_name = $1 AND wait_event_type = 'Lock'",
            &[&schema],
        )
        .expect("the activity view")
        .get(0)
}

// Catches a fence that is a separate SELECT before the write: with one, B's takeover could
// commit while A's checked write is still in flight, and both would land. Here A's write has
// run its check and is held open at the point of commit; B's takeover must queue behind it
// (it cannot slip between A's check and A's commit), and once A commits, B takes
// generation 2 and A is refused for good. Section 8's own test: "hold A's transaction open,
// take over on B, assert A's commit fails or B's takeover waits" (here: waits).
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn a_takeover_waits_for_a_fenced_write_that_has_passed_its_check() {
    let backend = PostgresBackend::new(&url(), Flaw::None);
    let (store, schema) = store(&backend);
    let a = Fenced::take(Arc::clone(&store) as Arc<dyn Store>, None).expect("generation 1");
    let held = store.get(GENERATION).expect("read").expect("row").version;
    // What `Fenced` sends for A: the write plus the check of the generation row's version.
    let write = Txn::new()
        .put("a/held", "1", Cond::Any)
        .check(GENERATION, Cond::Version(held));
    let pending = store.begin(write).expect("A's write runs its check");

    let taker = {
        let store = Arc::clone(&store) as Arc<dyn Store>;
        thread::spawn(move || Fenced::take(store, Some(1)))
    };
    let mut polls = 0;
    while waiting(&schema) == 0 {
        polls += 1;
        assert!(
            polls < 6000,
            "B's takeover never queued behind A's open write"
        );
        assert!(
            !taker.is_finished(),
            "B took the generation while A's write was open"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert!(!taker.is_finished());
    pending.commit().expect("A's write commits");
    let b = taker.join().expect("no panic").expect("B takes after A");
    assert_eq!(b.generation(), 2);

    let landed = store
        .get("a/held")
        .expect("read")
        .expect("A's write landed");
    let taken = store.get(GENERATION).expect("read").expect("row");
    assert!(
        landed.version < taken.version,
        "A's write is ordered before B's takeover"
    );
    assert_eq!(
        a.txn(Txn::new().put("a/late", "2", Cond::Any)).unwrap_err(),
        StoreError::Fenced
    );
    assert_eq!(store.get("a/late").expect("read"), None);
}

// Catches a fence whose check and write can be separated under concurrency: four threads of
// A write as fast as they can while B takes the generation. Every row A's handle managed to
// commit must be ordered before B's takeover, and every write after it must be refused.
#[test]
#[ignore = "needs a Postgres server (KBF_TEST_POSTGRES_URL); the state-postgres CI job runs it"]
fn no_write_of_the_old_handle_commits_after_the_takeover() {
    let backend = PostgresBackend::new(&url(), Flaw::None);
    let (store, _schema) = store(&backend);
    let old = Arc::new(Fenced::take(Arc::clone(&store) as Arc<dyn Store>, None).expect("gen 1"));
    let written = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let writers: Vec<_> = (0..4)
        .map(|w| {
            let (old, written, stop) = (Arc::clone(&old), Arc::clone(&written), Arc::clone(&stop));
            thread::spawn(move || {
                let mut n = 0_u64;
                let mut refused = 0_u64;
                while !stop.load(Ordering::SeqCst) {
                    match old.txn(Txn::new().put(format!("a/{w}/{n}"), "v", Cond::Any)) {
                        Ok(Ok(_)) => {
                            written.fetch_add(1, Ordering::SeqCst);
                        }
                        Err(StoreError::Fenced) => refused += 1,
                        other => panic!("neither committed nor fenced: {other:?}"),
                    }
                    n += 1;
                }
                refused
            })
        })
        .collect();
    while written.load(Ordering::SeqCst) < 50 {
        thread::yield_now();
    }
    let _new = Fenced::take(Arc::clone(&store) as Arc<dyn Store>, Some(1)).expect("B takes");
    // Give every writer time to hit the fence, by count, not by clock.
    let taken = store.get(GENERATION).expect("read").expect("row").version;
    let mut refused = 0;
    stop.store(true, Ordering::SeqCst);
    for writer in writers {
        refused += writer.join().expect("no panic");
    }
    assert!(refused > 0, "no writer ever saw the fence");
    let rows = store.scan_prefix("a/").expect("scan");
    assert!(!rows.is_empty());
    for (key, row) in rows {
        assert!(row.version < taken, "{key} committed after the takeover");
    }
}

// ---- (b) the Unix-socket URL form ---------------------------------------------------------

// The form section 11 gives, with the socket directory of the design.
const DESIGN_URL: &str = "postgres:///kbf?host=/run/postgresql";

// Catches a URL form the client refuses before it connects: the design's URL has no host
// part and no user. It parses, and names a Unix-socket host. With no user in the URL the
// client connects as the operating-system user (tokio-postgres 0.7.18, connect_raw.rs:
// `None => whoami::username()`), which is what peer authentication matches.
#[test]
fn the_design_url_parses_to_a_unix_socket_host() {
    let config = PostgresConfig::parse(DESIGN_URL).expect("the design URL is accepted");
    drop(config);
    let pg: postgres::Config = DESIGN_URL.parse().expect("parses");
    assert_eq!(
        pg.get_hosts(),
        [postgres::config::Host::Unix("/run/postgresql".into())]
    );
    assert_eq!(pg.get_dbname(), Some("kbf"));
    assert_eq!(pg.get_user(), None, "the URL names no user");
}

fn socket_url(dir: &str, user: Option<&str>) -> Option<String> {
    let user = user.map(|u| format!("{u}@")).unwrap_or_default();
    Some(format!("postgres://{user}/postgres?host={dir}"))
}

fn socket_dir() -> Option<String> {
    let dir = std::env::var(SOCKET_VAR).ok();
    if dir.is_none() {
        eprintln!("skipped: {SOCKET_VAR} is not set");
    }
    dir
}

// Catches a design URL that cannot open a store: the user-less form of section 11 must
// open on the socket, as the operating-system user, which is the role peer authentication
// matches. Runs only against a server whose socket directory is in KBF_TEST_POSTGRES_SOCKET_DIR
// and which has a role named for the user running the test.
#[test]
#[ignore = "needs a Postgres server and KBF_TEST_POSTGRES_SOCKET_DIR (its Unix socket directory, with a role named for the running user); skipped when unset"]
fn the_user_less_design_url_opens_a_store_on_the_socket() {
    let Some(dir) = socket_dir() else { return };
    let url = socket_url(&dir, None).expect("a URL");
    let config = PostgresConfig::parse(&url).expect("parses");
    let schema = format!("kbf_u0_{}", std::process::id());
    let config = config.with_schema(&schema).expect("a schema");
    let opened = PostgresStore::open(&config);
    assert!(
        opened.is_ok(),
        "opening on the socket failed: {:?}",
        opened.err()
    );
}

// Catches a Unix-socket path that works for a bare connection but not through the store:
// with the role in the URL the store opens over the socket, passes its `synchronous_commit`
// check, and takes a write and a read.
#[test]
#[ignore = "needs a Postgres server and KBF_TEST_POSTGRES_SOCKET_DIR (its Unix socket directory); skipped when unset"]
fn a_url_with_the_role_opens_a_working_store_on_the_socket() {
    let Some(dir) = socket_dir() else { return };
    let user = std::env::var("KBF_TEST_POSTGRES_SOCKET_USER").unwrap_or_else(|_| "postgres".into());
    let url = socket_url(&dir, Some(&user)).expect("a URL");
    let backend = PostgresBackend::new(&url, Flaw::None);
    let (store, _schema) = store(&backend);
    let fenced = Fenced::take(Arc::clone(&store) as Arc<dyn Store>, None).expect("generation 1");
    fenced
        .txn(Txn::new().put("k", "v", Cond::Absent))
        .expect("store")
        .expect("commit");
    assert_eq!(store.get("k").expect("read").expect("row").value, b"v");
}
