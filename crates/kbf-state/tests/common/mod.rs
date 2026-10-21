//! Helpers the SQLite test files share.

#![allow(dead_code)]

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use kbf_state::conformance::Backend;
use kbf_state::sqlite::{Options, SqliteStore};
use kbf_state::{Clock, Conflict, ManualClock, Store, Txn};
use kbf_testkit::dir::TestDir;

/// A new, empty directory under the test target directory (cargo's, on a disk that syncs
/// like the one the store runs on). Its name carries `name`, the process and a count, so
/// no two tests in one run, and no two runs, share one; it is removed when the returned
/// value is dropped, so a test declares it before the stores and children that use it.
pub fn scratch(name: &str) -> TestDir {
    TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("kbf-state-{name}"))
}

/// Opens a store in `dir` with `options` on a manual clock.
pub fn open_manual(dir: &Path, options: Options) -> (SqliteStore, Arc<ManualClock>) {
    let clock = Arc::new(ManualClock::default());
    let as_clock: Arc<dyn Clock> = clock.clone();
    let store = SqliteStore::open(&dir.join("state.db"), as_clock, options).expect("open");
    (store, clock)
}

/// The suite's factory for the SQLite store: every store gets a directory of its own, and
/// a restart closes the store (its files are then as a crash leaves them, since every
/// acknowledged group was synced) and opens the same file again.
pub struct SqliteBackend {
    name: &'static str,
    opened: AtomicUsize,
    /// The directories handed to the stores, removed when the backend is dropped (a
    /// store an `open` returned lives in its directory, so they are kept here).
    dirs: Mutex<Vec<TestDir>>,
}

impl SqliteBackend {
    pub fn new(name: &'static str) -> SqliteBackend {
        SqliteBackend {
            name,
            opened: AtomicUsize::new(0),
            dirs: Mutex::new(Vec::new()),
        }
    }
}

impl Backend for SqliteBackend {
    type Store = SqliteStore;

    fn open(&self, clock: Arc<ManualClock>) -> SqliteStore {
        let n = self.opened.fetch_add(1, Ordering::SeqCst);
        let dir = scratch(&format!("{}-{n}", self.name));
        let clock: Arc<dyn Clock> = clock;
        let store =
            SqliteStore::open(&dir.join("state.db"), clock, Options::default()).expect("open");
        self.dirs.lock().expect("the list of directories").push(dir);
        store
    }

    fn restart(&self, store: SqliteStore) -> SqliteStore {
        let path = store.path().to_owned();
        let clock = store.clock();
        drop(store);
        SqliteStore::open(&path, clock, Options::default()).expect("reopen")
    }

    fn commit_out_of_order(
        &self,
        store: &SqliteStore,
        first: Txn,
        second: Txn,
        between: &mut dyn FnMut(),
    ) -> Result<(), Conflict> {
        // One writer commits in the order it is given, so there is no out-of-order commit to
        // arrange here; the interleaving that matters to the feed, a poller paging it while
        // a writer commits groups, is `sqlite_feed.rs`.
        store.txn(first).expect("the store is reachable")?;
        between();
        store.txn(second).expect("the store is reachable")?;
        Ok(())
    }
}
