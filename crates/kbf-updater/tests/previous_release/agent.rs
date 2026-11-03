//! The `agent` module of the previous release's `update.rs`, as that file reads it: this
//! crate's `flags`, `hold`, `local` and `status`, and the shipped file as `update`.

pub use kbf_updater::agent::{flags, hold, local, status};

// The shipped file, whole; the parts of it no test here calls are dead in this crate.
#[allow(dead_code)]
#[path = "v0_0_63_update.rs"]
pub mod update;
