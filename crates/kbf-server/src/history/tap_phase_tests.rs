use kbf_history::records::PhaseTimes;
use kbf_invocation::{AttemptEnd, EndedKind, Run};

use super::*;

/// Catches the marker lost between a run and its record: a task's claim is recorded as
/// one, a daemon's run as measured, and an attempt with no run records none (unspecified).
#[test]
fn a_record_keeps_where_the_phase_times_came_from() {
    let record = |run: Option<Run>| {
        let mut ended = attempt(Some(AttemptEnd::Accepted));
        ended.run = run.map(Box::new);
        record_of(&row(vec![ended], None), 0, EndedKind::Finished).0
    };
    let claim = Run {
        phase_times: kbf_invocation::PhaseTimes::TaskClaim,
        ..Run::default()
    };
    assert_eq!(record(Some(claim)).phase_times(), PhaseTimes::TaskClaim);
    assert_eq!(
        record(Some(Run::default())).phase_times(),
        PhaseTimes::Measured
    );
    assert_eq!(record(None).phase_times(), PhaseTimes::Unspecified);
}
