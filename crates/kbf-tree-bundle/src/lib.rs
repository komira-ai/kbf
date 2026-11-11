//! The kbf base tree's bundle (docs/design/actions-as-jobs.md, 3.3).
//!
//! - [`tree`]: a tree of files, directories and symlinks, and the tierstore manifests
//!   (`tree_manifest` version 1) it is written as, bottom-up, with the facts an action tree's
//!   limits are counted with (its entries and depth). What tierstore refuses is refused here,
//!   by name, before anything is written.
//! - [`bundle`]: the release asset `kbf-base-tree-<commit>-<platform>.tree.tar`, a tar of the
//!   tree's facts and of every object (each file's bytes and each manifest) named by its
//!   SHA-256. `cargo xtask base-tree` writes it; the frontend reads it at its start and
//!   publishes the objects to `cas/l/<sha256>`.
//!
//! The crate does no I/O of its own: it writes to and reads from what it is given.

pub mod bundle;
pub mod tree;

pub use bundle::{BundleError, Facts, Item, Normalised, ReadError, Writer, file_name, read};
pub use tree::{Built, Dir, Node, TreeError};
