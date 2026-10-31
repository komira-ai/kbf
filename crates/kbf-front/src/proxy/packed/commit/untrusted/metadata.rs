//! The execution metadata of a result the frontend rebuilt from a task's manifest.
//!
//! The manifest's `timing` is the task's own account of how long its steps took (fetching the
//! inputs, running the command, writing the packs), in milliseconds. It is a claim, not a
//! measurement of the frontend's: it is carried so that run history and the estimator read an
//! executor run as they read a daemon's (`input_fetch_*`, `execution_*`, `output_upload_*`),
//! and is never used to decide anything.

use std::time::{Duration, SystemTime};

use kbf_pack::Timing;
use kbf_proto::reapi::ExecutedActionMetadata;
use prost_types::Timestamp;

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
    let mut at = end;
    let upload = lay(&mut at, timing.pack_ms);
    let execution = lay(&mut at, timing.run_ms);
    let fetch = lay(&mut at, timing.fetch_ms);
    let claims = [upload, execution, fetch].iter().any(Option::is_some);
    claims.then(|| {
        let (input_fetch_start, input_fetch_completed) = split(fetch);
        let (execution_start, execution_completed) = split(execution);
        let (output_upload_start, output_upload_completed) = split(upload);
        ExecutedActionMetadata {
            input_fetch_start_timestamp: input_fetch_start,
            input_fetch_completed_timestamp: input_fetch_completed,
            execution_start_timestamp: execution_start,
            execution_completed_timestamp: execution_completed,
            output_upload_start_timestamp: output_upload_start,
            output_upload_completed_timestamp: output_upload_completed,
            ..ExecutedActionMetadata::default()
        }
    })
}

/// The phase of `ms` that ends at `*at`, which then moves back to where the phase began; `None`
/// for a phase of 0 ms, which leaves `*at` where it is.
///
/// A manifest holds at most `i64::MAX` ms in each duration (it refuses more), so the three are
/// under 3e16 ms together, far inside what a `SystemTime` holds before the epoch: the subtraction cannot
/// overflow, whatever a task claims.
fn lay(at: &mut SystemTime, ms: u64) -> Option<(Timestamp, Timestamp)> {
    let completed = *at;
    *at = completed - Duration::from_millis(ms);
    (ms > 0).then(|| (Timestamp::from(*at), Timestamp::from(completed)))
}

/// The start and the end of a phase, each `None` for a phase that is absent.
fn split(phase: Option<(Timestamp, Timestamp)>) -> (Option<Timestamp>, Option<Timestamp>) {
    phase.map_or((None, None), |(start, end)| (Some(start), Some(end)))
}
