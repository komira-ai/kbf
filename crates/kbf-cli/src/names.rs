//! The one table of modes: their names and what each runs.
//!
//! Every place in this crate that spells a mode reads it from here: the clap commands,
//! the dispatch, the help and the tests. The names follow the arch-v2 design; renaming
//! a mode is changing its constant, and moving a mode from "not built" to an entry
//! point is changing its row of [`MODES`].

/// The frontend (the REAPI front end).
pub const FRONTEND: &str = "frontend";
/// The storage node (the server half).
pub const STORAGE: &str = "storage";
/// The build node.
pub const DAEMON: &str = "daemon";
/// Storage and build node in one process.
pub const STORAGE_AND_DAEMON: &str = "storage-and-daemon";
/// Frontend, storage and build node in one process.
pub const ALL_IN_ONE: &str = "all-in-one";

/// What a mode runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry {
    /// The worker daemon: `kbf_node::run`, the flags of `kbf-daemon`.
    Daemon,
    /// The server and the daemon in one process: `kbf_all_in_one::run`, the flags of
    /// `kbf-all-in-one`.
    AllInOne,
    /// Not built yet: the mode exits 2 saying which work-plan unit builds it.
    NotBuilt(&'static str),
}

/// A mode and what it runs.
#[derive(Clone, Copy, Debug)]
pub struct Mode {
    /// The word after `kbf`.
    pub name: &'static str,
    /// What it runs.
    pub entry: Entry,
}

/// Every mode, in help order: exactly five.
pub const MODES: [Mode; 5] = [
    Mode {
        name: FRONTEND,
        entry: Entry::NotBuilt("av2-07"),
    },
    Mode {
        name: STORAGE,
        entry: Entry::NotBuilt("av2-06"),
    },
    Mode {
        name: DAEMON,
        entry: Entry::Daemon,
    },
    Mode {
        name: STORAGE_AND_DAEMON,
        entry: Entry::NotBuilt("av2-06"),
    },
    Mode {
        name: ALL_IN_ONE,
        entry: Entry::AllInOne,
    },
];

/// The one line an unbuilt mode prints to stderr before it exits 2.
#[must_use]
pub fn not_built_line(mode: &str, unit: &str) -> String {
    format!("kbf {mode}: mode not built yet (work-plan unit {unit})")
}
