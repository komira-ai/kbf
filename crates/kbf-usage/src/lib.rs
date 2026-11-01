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
//! - [`KbfUsageSink`] and [`RunKeys`]: the sink and the key source a `js-usage` `UsageHub`
//!   is made over, so the scheduler libraries' own recorder runs on kbf's store.
//! - [`UsageMode`]: `off`, `record` or `on`.
//!
//! Nothing here reads a clock or a flag: days and wall minutes are arguments. The farm on the
//! js core calls this crate under `--usage-profiles record` (`kbf-server`, `farm_js::usage`)
//! and gives the scheduler none of the curves, so it changes no placement. Admission reads a
//! curve only for an action that also has a prediction; handing the curves over is a separate
//! change.
#![forbid(unsafe_code)]

mod hub_sink;
mod mode;
mod name;
mod shapes;
mod sink;

pub use hub_sink::{KbfUsageSink, RunKeys};
pub use mode::{BadMode, UsageMode};
pub use name::{HostNames, profile_name};
pub use shapes::{MAX_SHAPES, ShapeIndex};
pub use sink::{Flushed, ProfileSink, SinkStats};
