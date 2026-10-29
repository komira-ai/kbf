//! What kbf learns from the measured use of its leases, for the job scheduler's admission.
//!
//! A daemon reports the closed minutes of each lease on its heartbeat. The scheduler
//! libraries (`js-usage`) turn them into per-key profiles (the last runs of an action key,
//! the curve admission charges from them) and keep the rows in the farm's state store. This
//! crate holds the parts that are kbf's own and need no farm:
//!
//! - [`profile_name`]: the estimator's [`kbf_estimator::Key`] to the name a profile is kept
//!   under. A profile is that of an action shape, not of one action digest: a digest
//!   changes with every input, so a profile kept per digest would never warm.
//! - [`ShapeIndex`]: action key to profile name, filled when an action is submitted or
//!   loaded, read by the scheduler's curve source (`KeyMap`); bounded.
//! - [`HostNames`]: a worker's id as the host id the recorder keys a node by.
//! - [`ProfileSink`]: the recorder and the profile store behind one lock, with the flush
//!   a tick calls and the curves the scheduler reads.
//! - [`UsageMode`]: `off`, `record` or `on`.
//!
//! Nothing here reads a clock or a flag: days and wall minutes are arguments, and no farm
//! code calls this crate yet, so linking it changes no placement. Admission reads a curve
//! only for an action that also has a prediction; feeding predictions is a separate change.
#![forbid(unsafe_code)]
// The first commit of the crate holds its tests and signatures with placeholder bodies, so
// that the tests fail on their assertions and not at compile time.
#![allow(dead_code, unused_variables, unused_imports)]

mod mode;
mod name;
mod shapes;
mod sink;

pub use mode::{BadMode, UsageMode};
pub use name::{HostNames, profile_name};
pub use shapes::{MAX_SHAPES, ShapeIndex};
pub use sink::{Flushed, ProfileSink, SinkStats};
