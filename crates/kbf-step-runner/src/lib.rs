//! The step runner of the generic host updater: the root part's engine for one update of one
//! service (docs/design/host-updates.md, 3.2 to 3.5, migration step 4).
//!
//! - [`StepRunner`] runs the five steps of the hook contract through a [`StepLauncher`], writes
//!   the [`record::Record`] before and after each step, resumes from it after a crash, and reads
//!   exit codes as outcomes through [`update_hook::next`].
//! - [`Staged`] is the verified artifact: nothing from a release runs unless the tree still has
//!   the digest it had when it was verified.
//! - [`DeadMan`] enforces the bound of the running step on a thread of its own.
//! - [`register`] is the registration socket: liveness and version, `SO_PEERCRED`, and the
//!   `absent` and `stopped` the updater reports.
//! - [`selfupdate`] is the updater's own update with its fallback.
//!
//! The crate names no service and reads no declaration: the launcher that runs a step under the
//! rights of a declaration, and the loader of declarations, are migration step 3 and plug in
//! through [`StepLauncher`] and [`register::Declared`].

pub mod artifact;
pub mod clock;
pub mod deadman;
pub mod launcher;
pub mod record;
pub mod register;
pub mod runner;
pub mod selfupdate;

pub use artifact::{ArtifactError, Staged, tree_digest, tree_digest_with};
pub use clock::{Clock, Stamp, SystemClock};
pub use deadman::{DeadMan, Watch};
pub use launcher::{LaunchError, StepCall, StepExit, StepLauncher};
pub use record::{CrashGate, CrashPoint, Loaded, Record, RecordError, RecordStore};
pub use runner::{Bounds, Finished, RunError, StepRunner, UpdateOrder};
