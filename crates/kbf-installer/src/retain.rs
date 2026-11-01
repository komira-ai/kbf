//! Which releases a host keeps, and `retain.json`, the file that says what must stay besides the
//! links: the rule and the file are `kbf-retain`'s (the helper's Mac build takes them without
//! this crate), and where the file is on a host with this layout is here.
//!
//! A host keeps, in `releases/`, the release that runs (`current`), the one before it
//! (`previous`), every pinned release, the coordinator's `last_good`, and the newest
//! [`DEFAULT_KEEP`] others (never fewer than [`MIN_KEEP`]). Releases are removed only after a
//! swap is confirmed, never while one is on trial ([`crate::release::gc`]).

use std::path::PathBuf;

pub use kbf_retain::{
    DEFAULT_KEEP, FILE, Kept, Known, MIN_KEEP, RetainError, RetainLock, Retained, at_least, lines,
    listing, parse_keep, surplus,
};

use super::layout::Layout;

/// Where `retain.json` is on a host with the installer's layout: in its state directory.
#[must_use]
pub fn path(host: &Layout) -> PathBuf {
    host.join(&host.state_dir()).join(FILE)
}

#[cfg(test)]
#[path = "retain_tests.rs"]
mod tests;
