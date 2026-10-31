//! The execution metadata of a result the frontend rebuilt from a task's manifest.
//!
//! The manifest's `timing` is the task's own account of how long its steps took (fetching the
//! inputs, running the command, writing the packs), in milliseconds. It is a claim, not a
//! measurement of the frontend's: it is carried so that run history and the estimator read an
//! executor run as they read a daemon's (`input_fetch_*`, `execution_*`, `output_upload_*`),
//! and is never used to decide anything.

use std::time::SystemTime;

use kbf_pack::Timing;
use kbf_proto::reapi::ExecutedActionMetadata;

/// The metadata the task's claim makes, or `None` when it claims nothing.
///
/// The manifest holds durations and no instants, so the three phases are laid back to back and
/// anchored at `end`, the time the frontend read the task's pack: the task wrote the pack
/// before then, so no phase starts before the task did, and the last one ends no later than
/// the frontend's own completion. Each is the task's duration to the millisecond.
///
/// A phase the task reports as 0 ms is left out (both of its timestamps absent), never
/// recorded as an instant: the tool writes 0 for a step it did not time, and a step that took
/// under a millisecond is not told apart from it. The phases after it keep their place, as if
/// it had no width. A manifest that claims no phase gives `None`, not an empty message.
pub(super) fn claimed(timing: &Timing, end: SystemTime) -> Option<ExecutedActionMetadata> {
    let _ = (timing, end);
    None
}
