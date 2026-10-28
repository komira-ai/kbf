//! A daemon's busy reading, from its heartbeat (`Heartbeat.busy_millis`), for the
//! scheduler's admission by measured CPU ([`kbf_sched::CpuAdmission`]).

use kbf_sched::Event;
use kbf_types::WorkerId;

use super::{Farm, StreamId};

impl Farm {
    /// The busy reading of a heartbeat of `worker` on `stream`, in CPU-milliseconds per
    /// second: given to the scheduler when the heartbeat carries one. A heartbeat with none
    /// (a daemon that does not measure, or has no window yet) changes nothing: the
    /// reading the scheduler holds ages out by itself, and a worker with none is admitted to
    /// as without the feature. Ignored if `stream` was replaced. Call it before
    /// [`Farm::heartbeat`] so that the placement that follows the heartbeat sees it.
    pub fn note_busy(&self, worker: &WorkerId, stream: StreamId, busy_millis: Option<u64>) {
        let Some(busy_millis) = busy_millis else {
            return;
        };
        let mut state = self.lock();
        if !state.is_current(worker, stream) {
            return;
        }
        let now = self.now();
        state.feed_quiet(
            now,
            Event::Busy {
                worker: worker.clone(),
                busy_millis,
            },
        );
    }
}

#[cfg(test)]
mod tests;
