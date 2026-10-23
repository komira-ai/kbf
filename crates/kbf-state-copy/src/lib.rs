//! `kbf-state-copy`: the one-time move of a frontend's state from its SQLite file into
//! Postgres, run by an operator as a cold cutover. It is a binary of its own, never part of
//! `kbf`, so the SQLite reader it needs does not ship in the farm's binary.
//!
//! ```text
//! kbf-state-copy <state.db> <postgres-url>            dry run: report and check, write nothing
//! kbf-state-copy <state.db> <postgres-url> --apply    copy, check, commit, check again
//! ```
//!
//! There is no way to skip a check. Exit status: 0 done, 2 refused (a precondition does not
//! hold; nothing was changed), 1 failed (a call failed or the copy did not verify; a mistake
//! on the command line is 1 too).
//!
//! # What is checked
//!
//! - **The frontend is stopped.** The single-writer lock beside the state file ([`lock`]) is
//!   not held. A copy keeps the lock until it is done, so a frontend cannot start on the
//!   file meanwhile. A state file with no lock file beside it has never been opened by a
//!   frontend, and the tool creates none: there the operator confirms that no frontend
//!   runs.
//! - **The file is clean and current.** Its layout is the one the SQLite store creates
//!   (`user_version` 1) and no write-ahead log holds writes the file does not ([`source`]).
//! - **The target is ready.** Its schema is the version this tool was built for, it holds
//!   nothing (no rows, no feed entries, no counters, a head revision of 0), and its clock is
//!   within 2 seconds of this host's ([`target`]).
//!
//! # What is copied
//!
//! Every key/value row and every counter, in one transaction. The versions are kept, so a
//! compare-and-set on a version a farm holds still means what it meant. The expiry times
//! are converted from the file's nanoseconds to the store's microseconds (rounded down),
//! both on the Unix clock. The store's head revision, and its revision sequence, are set to
//! the file's head revision, or to the highest version when that is higher, so the next
//! write gets a revision above every copied row. The change feed is not copied: its
//! history is not state, and a reader of the new store starts at the head.
//!
//! The SQL is this crate's own because the store's write API gives every write a new
//! revision, and the rows here must keep theirs.
//!
//! # How it is checked
//!
//! The report names each key family (the first path segment of a key: `op/`, `lease/`) with
//! its row count and the SHA-256 digest of its rows ([`digest`] says what is hashed). A copy
//! reads the rows back inside its transaction and compares them with the file's family by
//! family, and the head revision; a mismatch rolls the transaction back and nothing is left
//! in the target (the revision sequence is moved last, after the checks, because a sequence
//! is not rolled back). After the commit it reads them back again on a new connection; a
//! mismatch there, or a read that fails, is reported as a failure (the commit cannot be
//! undone, and the message says what the target now needs). The copy waits at most 10 s for
//! the store's commit head and 300 s for any one statement, and its connections 10 s to
//! reach the server; the schema check is the store's own call and has no such limit.
//! The state file is opened read-only and immutable and is never changed.

pub mod app;
pub mod cli;
pub mod digest;
pub mod error;
pub mod lock;
pub mod seams;
pub mod source;
pub mod target;

pub use app::{main_with, run};
pub use cli::Args;
pub use error::Error;
pub use seams::{Seams, Stage};
