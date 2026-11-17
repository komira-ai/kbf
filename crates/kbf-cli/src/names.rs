//! The one table of modes: their names and what each runs.
//!
//! Every place in this crate that spells a mode reads it from here: the clap commands,
//! the dispatch, the help and the tests. A mode is named by `--mode`; its name is also the
//! hidden alias word of the old command lines. The names follow the arch-v2 design; renaming
//! a mode is changing its constant.

/// The frontend (the REAPI front end).
pub const FRONTEND: &str = "frontend";
/// The storage node (the server half).
pub const STORAGE: &str = "storage";

/// The modes that were removed, as `--mode` spelled them ([`kbf_setup::cli::REMOVED_MODES`], one
/// list for `kbf`, `kbf setup` and `kbf-installer setup`). They are no [`MODES`] rows and no
/// values of `--mode`: asking for one exits 2 with [`REMOVED`].
pub use kbf_setup::cli::REMOVED_MODES;

/// What a command line that asks for a removed mode is told: kbf runs no daemon any more
/// ([`kbf_setup::cli::MODE_REMOVED`]).
pub use kbf_setup::cli::MODE_REMOVED as REMOVED;

/// Whether `name` is a mode that can read `kbf.toml` and enroll its host: the storage node.
/// The frontend takes its files as flags.
#[must_use]
pub fn is_registered(name: &str) -> bool {
    name == STORAGE
}

/// Whether `name` is a mode that keeps its state in the Postgres database: the frontend. The
/// storage node keeps none, so `kbf boot-check` has nothing to migrate for it.
#[must_use]
pub fn keeps_state_in_database(name: &str) -> bool {
    name == FRONTEND
}

/// What a mode runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    /// The storage node: `kbf_server::storage_node::run`, `kbf-server`'s store, write and
    /// log flags and its own listener flags.
    Storage,
    /// The frontend: `kbf_server::frontend::run`, `kbf-server`'s listener and
    /// log flags and its own storage flags.
    Frontend,
}

/// A mode and what it runs.
#[derive(Clone, Copy, Debug)]
pub struct Mode {
    /// The word after `kbf`.
    pub name: &'static str,
    /// What it runs.
    pub entry: Entry,
}

/// Every mode, in help order: exactly two.
pub const MODES: [Mode; 2] = [
    Mode {
        name: FRONTEND,
        entry: Entry::Frontend,
    },
    Mode {
        name: STORAGE,
        entry: Entry::Storage,
    },
];
