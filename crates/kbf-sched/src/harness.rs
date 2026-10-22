//! Driving a scheduler in process, as the single-node server does.
//!
//! The tests in `tests/` and the estimator replay (`kbf-estimator-replay`) drive the real
//! [`Scheduler`] with events and carry out its effects. This is the one loop they share:
//! feed an event, feed back every `Commit` it asks for as `Committed` (the control log is
//! in process and appends at once), and hand the other effects to the caller.

use kbf_types::{Effect, FarmTime, StateMachine};

use crate::{Event, Input, Scheduler};

/// Feeds `event` at `now`, then every commit it asks for (and those they ask for), and
/// returns the other effects in the order the scheduler emitted them.
pub fn feed(sched: &mut Scheduler, now: FarmTime, event: Event) -> Vec<Effect> {
    let mut pending = sched.apply(Input::new(now, event));
    let mut out = Vec::new();
    while !pending.is_empty() {
        let mut more = Vec::new();
        for effect in pending {
            match effect {
                Effect::Commit(record) => {
                    more.extend(sched.apply(Input::new(now, Event::Committed(record))));
                }
                other => out.push(other),
            }
        }
        pending = more;
    }
    out
}
