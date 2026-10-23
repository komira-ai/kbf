//! What the update row tests share: a state store on either backend.

#![allow(dead_code)]

use std::path::PathBuf;
use std::sync::Arc;

use kbf_state::fake::{Fake, Flaw};
use kbf_state::sqlite::{Options, SqliteStore};
use kbf_state::{Clock, ManualClock, Store};

/// Which backend a test runs the update rows on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backing {
    /// The in-memory fake.
    Fake,
    /// A SQLite file in a directory of the test's own.
    Sqlite,
}

/// A state store, and for SQLite the file it can be closed and opened again from.
pub struct Cell {
    store: Arc<dyn Store>,
    sqlite: Option<(Arc<SqliteStore>, PathBuf)>,
    /// The directory of the SQLite file, last so it is removed after the store is closed.
    _dir: Option<kbf_testkit::dir::TestDir>,
}

fn scratch(name: &str) -> kbf_testkit::dir::TestDir {
    kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), &format!("kbf-update-{name}"))
}

impl Cell {
    /// An empty store on `backing`; `name` names the directory of a SQLite file.
    pub fn new(backing: Backing, name: &str) -> Self {
        match backing {
            Backing::Fake => {
                let fake = Fake::with_flaw(Arc::new(ManualClock::default()), Flaw::None);
                Self {
                    store: Arc::new(fake),
                    sqlite: None,
                    _dir: None,
                }
            }
            Backing::Sqlite => {
                let dir = scratch(name);
                let path = dir.join("state.db");
                let store = Self::open(&path);
                Self {
                    store: Arc::clone(&store) as Arc<dyn Store>,
                    sqlite: Some((store, path)),
                    _dir: Some(dir),
                }
            }
        }
    }

    fn open(path: &std::path::Path) -> Arc<SqliteStore> {
        let clock: Arc<dyn Clock> = Arc::new(ManualClock::default());
        // One writer, and a test that writes one row at a time: waiting for company in
        // the group commit window would only slow the tests.
        let options = Options {
            window: std::time::Duration::ZERO,
            ..Options::default()
        };
        Arc::new(SqliteStore::open(path, clock, options).expect("a SQLite store"))
    }

    /// The store.
    pub fn store(&self) -> Arc<dyn Store> {
        Arc::clone(&self.store)
    }

    /// Closes a SQLite store and opens the same file again, as a restarted process does
    /// (its epoch is then one larger). Every other handle on the store must be gone.
    pub fn reopen(&mut self) {
        let (store, path) = self.sqlite.take().expect("a SQLite store");
        // Put a placeholder in the field `store` so that the old Arc can be the last.
        self.store = Arc::new(Fake::with_flaw(
            Arc::new(ManualClock::default()),
            Flaw::None,
        ));
        drop(store);
        let store = Self::open(&path);
        self.store = Arc::clone(&store) as Arc<dyn Store>;
        self.sqlite = Some((store, path));
    }
}
