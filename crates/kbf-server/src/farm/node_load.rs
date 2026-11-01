//! What each node's daemon reports of its `actions/` cgroup (`Heartbeat.node_load`): the
//! readings the server accepts, and the load it shows in the node view
//! (`GET /v1/nodes/{id}`). Nothing here admits or ranks: it parses, keeps and shows, and
//! hands each accepted reading to the scheduler, which does.
//!
//! The accept rule, per node:
//! - A reading is taken only if its `seq` is above the last accepted one of the same daemon
//!   session. A session is one daemon process (`Hello.instance_id`; a daemon that sends none
//!   is taken to be a new session on each stream). A new session resets the baseline: its
//!   first reading is accepted whatever its `seq`, and it is not measured against the
//!   readings of the process before.
//! - A reading that fails [`validate`] counts as absent: it is dropped, and the last accepted
//!   reading stays the baseline. A reading with no `seq`, whose clock or CPU counter went
//!   back (or whose clock did not move), whose CPU since the last accepted reading implies
//!   more than four cores per hardware thread, or whose working set is above four times the
//!   node's memory, fails. A node that reported no capacity has no hardware threads or
//!   memory to compare with, and that check is skipped.
//! - What is kept is the last accepted reading and the rates of the last two windows (a
//!   window is the time between two accepted readings), so the measured CPU can be the
//!   larger of two windows without keeping three readings.
//! - Readings are stale once the time since the last accepted one, on the farm's clock, is
//!   above three windows (a window being the span of the newest one). A node with fewer than
//!   two accepted readings has no window, so it is never stale and has no measured CPU.
//!
//! The lists of a reading (`covered`, `lease_clocks`, `minutes`) are vetted when the reading
//! is accepted (`NodeLoad::vet` of `kbf-proto`): a record that breaks a rule, repeats an
//! earlier one or is past the cap of its list is dropped alone and counted ([`Lists`], shown
//! in the node view), and the heartbeat is not refused for it. The minutes of a reading the
//! accept rule refuses are counted too, as refused, and used for nothing.
//!
//! An accepted reading, with its vetted lists, is converted ([`sched_load`]) and fed to the
//! scheduler on the same heartbeat, in every `--placement-load` mode: the scheduler keeps the
//! readings whether or not it places by them, so the feed is warm before a flip to `on`.

use std::collections::BTreeMap;

use kbf_proto::Vetted;
use kbf_proto::worker::NodeLoad;
use kbf_types::sched::usage;
use kbf_types::{LeaseId, Resources, WorkerId};

use super::{Farm, FarmTime, State, StreamId};
use crate::PlacementLoad;
use crate::fleet::LoadView;

mod feed;

use feed::{Lists, sched_load};

/// How many cores of CPU per hardware thread a window may imply before it is not believed.
const MAX_CORES_PER_THREAD: u128 = 4;
/// How many times the node's memory a working set may be before it is not believed.
const MAX_MEMORY_MULTIPLE: u64 = 4;
/// How many windows without a reading make the load stale.
const STALE_AFTER_WINDOWS: u128 = 3;

/// What each node's readings have come to: the legacy driver keeps it in its side state, the
/// js core in its adapter. Both take a reading through [`accept`] and show a node's through
/// [`shown`], so the accept rule is one.
pub(crate) type NodeLoads = BTreeMap<WorkerId, Kept>;

/// Which daemon session a reading belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Session {
    /// The daemon process that named itself.
    Process(String),
    /// A daemon that named no process: the stream it sent on.
    Stream(StreamId),
}

impl Session {
    /// The session of a stream opened by the daemon process `instance` (empty when it named
    /// none).
    pub(crate) fn of(instance: &str, stream: StreamId) -> Self {
        if instance.is_empty() {
            Self::Stream(stream)
        } else {
            Self::Process(instance.to_owned())
        }
    }
}

/// One window between two accepted readings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Window {
    /// The CPU the window used, in thousandths of a CPU.
    millicores: u64,
    /// Its length, in microseconds.
    span_usec: u64,
}

/// An accepted reading.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Accepted {
    seq: u64,
    mono_usec: u64,
    cpu_usage_usec: u64,
    working_set_bytes: u64,
    active_file_bytes: u64,
    covered: Vec<LeaseId>,
    cpu_limited: bool,
    throttled_usec: Option<u64>,
    pressure_some_total_usec: Option<u64>,
    /// When the server took it.
    taken: FarmTime,
}

/// What the server keeps of one node's readings.
#[derive(Clone, Debug)]
pub(crate) struct Kept {
    session: Session,
    last: Accepted,
    /// The newest window first.
    windows: [Option<Window>; 2],
    /// What the lists of the session's readings came to.
    lists: Lists,
}

/// What [`Kept::take`] made of one reading.
struct Taken {
    /// What is kept of the node after it: the reading if it was accepted, else what was kept
    /// before (nothing, for a reading of a new session that was refused).
    kept: Option<Kept>,
    /// The lists of the reading as vetted, when it was accepted; `None` when it was refused.
    vetted: Option<Vetted>,
}

/// The window from `before` to `now`, `None` when the clock did not move forward or the
/// counter went back. The rate is an integer, computed wide so it cannot overflow.
fn window(before: &Accepted, now: &NodeLoad) -> Option<Window> {
    let used = now.cpu_usage_usec.checked_sub(before.cpu_usage_usec)?;
    let span = now
        .mono_usec
        .checked_sub(before.mono_usec)
        .filter(|&s| s > 0)?;
    let millis = u128::from(used) * 1000 / u128::from(span);
    Some(Window {
        millicores: u64::try_from(millis).unwrap_or(u64::MAX),
        span_usec: span,
    })
}

/// Whether `reading` is believable after `last` on a node of `capacity`: see the module
/// text. Returns the window since `last` (`None` for a first reading).
fn validate(
    reading: &NodeLoad,
    last: Option<&Accepted>,
    capacity: &Resources,
) -> Result<Option<Window>, ()> {
    if reading.seq == 0 {
        return Err(());
    }
    if capacity.memory_bytes > 0
        && reading.memory_working_set_bytes
            > capacity.memory_bytes.saturating_mul(MAX_MEMORY_MULTIPLE)
    {
        return Err(());
    }
    let Some(last) = last else {
        return Ok(None);
    };
    let since = window(last, reading).ok_or(())?;
    // `millicores` over 1000 per core, against 4 cores per thread of `cpu_millis`/1000
    // threads: millicores > 4 * cpu_millis.
    if capacity.cpu_millis > 0
        && u128::from(since.millicores) > MAX_CORES_PER_THREAD * u128::from(capacity.cpu_millis)
    {
        return Err(());
    }
    Ok(Some(since))
}

impl Kept {
    /// Takes `reading` of `session` at `now`, if the accept rule does. A reading it refuses
    /// counts its minutes as refused on what was kept; one it takes has its lists vetted and
    /// counted.
    fn take(
        kept: Option<Self>,
        session: &Session,
        reading: &NodeLoad,
        capacity: &Resources,
        now: FarmTime,
    ) -> Taken {
        // A reading of another session starts again.
        let previous = kept.filter(|k| &k.session == session);
        if previous.as_ref().is_some_and(|k| reading.seq <= k.last.seq) {
            return Self::refused(previous, reading);
        }
        let since = validate(reading, previous.as_ref().map(|k| &k.last), capacity);
        let Ok(since) = since else {
            return Self::refused(previous, reading);
        };
        let windows = match (&previous, since) {
            (Some(k), Some(w)) => [Some(w), k.windows[0]],
            _ => [None, None],
        };
        let vetted = reading.vet();
        let mut lists = previous.map_or_else(Lists::default, |k| k.lists);
        lists.took(&vetted);
        let kept = Self {
            session: session.clone(),
            last: Accepted {
                seq: reading.seq,
                mono_usec: reading.mono_usec,
                cpu_usage_usec: reading.cpu_usage_usec,
                working_set_bytes: reading.memory_working_set_bytes,
                active_file_bytes: reading.memory_active_file_bytes,
                covered: vetted
                    .covered
                    .iter()
                    .map(|l| LeaseId::new(l.term, l.seq))
                    .collect(),
                cpu_limited: reading.actions_cpu_limited,
                throttled_usec: reading.cpu_throttled_usec,
                pressure_some_total_usec: reading.cpu_pressure_some_total_usec,
                taken: now,
            },
            windows,
            lists,
        };
        Taken {
            kept: Some(kept),
            vetted: Some(vetted),
        }
    }

    /// `previous` after a refused `reading`: the minutes it carried are counted, if the node
    /// has a state to count them in.
    fn refused(mut previous: Option<Self>, reading: &NodeLoad) -> Taken {
        if let Some(k) = previous.as_mut() {
            k.lists.refused(reading.minutes.len());
        }
        Taken {
            kept: previous,
            vetted: None,
        }
    }

    /// Whether the last accepted reading is older than three windows at `now`.
    fn stale(&self, now: FarmTime) -> bool {
        let Some(newest) = self.windows[0] else {
            return false;
        };
        let age_usec =
            u128::from(now.as_millis().saturating_sub(self.last.taken.as_millis())) * 1000;
        age_usec > STALE_AFTER_WINDOWS * u128::from(newest.span_usec)
    }

    /// The larger rate of the last two windows, in thousandths of a CPU.
    fn measured_cpu_millicores(&self) -> Option<u64> {
        self.windows.iter().flatten().map(|w| w.millicores).max()
    }

    /// The load as the node view shows it. `capacity` is what the node reported.
    pub(crate) fn view(&self, capacity: &Resources, now: FarmTime) -> LoadView {
        let stale = self.stale(now);
        let last = &self.last;
        // The working set less the active file cache beyond a quarter of the node's memory:
        // retained active file pages must not make an idle node look full.
        let excess = last
            .active_file_bytes
            .saturating_sub(capacity.memory_bytes / 4);
        LoadView {
            measured_cpu_millicores: if stale {
                None
            } else {
                self.measured_cpu_millicores()
            },
            memory_working_set_bytes: last.working_set_bytes,
            memory_active_file_bytes: last.active_file_bytes,
            admission_working_set_bytes: last.working_set_bytes.saturating_sub(excess),
            covered: last.covered.iter().map(ToString::to_string).collect(),
            actions_cpu_limited: last.cpu_limited,
            cpu_throttled_usec: last.throttled_usec,
            cpu_pressure_some_total_usec: last.pressure_some_total_usec,
            lists: self.lists.view(),
            stale,
        }
    }
}

/// Takes `load`, a reading of `worker`'s `session` at `now` on a node of `capacity`, into
/// `loads` if the accept rule ([module](self)) does. Returns the accepted reading with its
/// vetted lists as the scheduler takes it; `None` when it was refused. Both drivers of the
/// farm take readings through this.
pub(crate) fn accept(
    loads: &mut NodeLoads,
    worker: &WorkerId,
    session: &Session,
    load: &NodeLoad,
    capacity: &Resources,
    now: FarmTime,
) -> Option<usage::NodeLoad> {
    let taken = Kept::take(loads.remove(worker), session, load, capacity, now);
    if let Some(next) = taken.kept {
        loads.insert(worker.clone(), next);
    }
    taken.vetted.map(|vetted| sched_load(load, &vetted))
}

/// The load of `worker` as the node view shows it at `now`, if a reading was accepted.
pub(crate) fn shown(
    loads: &NodeLoads,
    worker: &WorkerId,
    capacity: &Resources,
    now: FarmTime,
) -> Option<LoadView> {
    loads.get(worker).map(|kept| kept.view(capacity, now))
}

/// The load of every node that has one, as the node view shows it at `now`, in node-id
/// order. `capacity` is what each node reported. Both drivers of the farm list loads through
/// this, for the `kbf_node_load_*` families of `GET /metrics`.
pub(crate) fn shown_all(
    loads: &NodeLoads,
    capacity: impl Fn(&WorkerId) -> Resources,
    now: FarmTime,
) -> Vec<(WorkerId, LoadView)> {
    loads
        .iter()
        .map(|(worker, kept)| (worker.clone(), kept.view(&capacity(worker), now)))
        .collect()
}

impl Farm {
    /// The load of every node the farm has accepted a reading of, as `GET /v1/nodes/{id}`
    /// shows it now, in node-id order: the source of the `kbf_node_load_*` families of
    /// `GET /metrics` ([`crate::load_metrics`]). A node keeps its load, aging to stale, until
    /// the server restarts or its daemon's next stream starts a new session.
    #[must_use]
    pub fn node_loads(&self) -> Vec<(WorkerId, LoadView)> {
        js_arm!(self, load::node_loads());
        let state = self.lock();
        shown_all(
            &state.side.node_loads,
            |worker| state.engine.sched.capacity(worker).unwrap_or_default(),
            self.now(),
        )
    }

    /// The node load on a heartbeat of `worker` on `stream`, taken if the accept rule
    /// ([module](self)) does. A heartbeat with none changes nothing: the last accepted
    /// reading ages. Ignored if `stream` was replaced.
    ///
    /// Returns the accepted reading with its vetted lists, as the scheduler takes it, for
    /// [`Farm::heartbeat_with_load`]; `None` when there was no reading, it was refused, or
    /// the stream was replaced.
    pub fn note_node_load(
        &self,
        worker: &WorkerId,
        stream: StreamId,
        load: Option<&NodeLoad>,
    ) -> Option<usage::NodeLoad> {
        js_arm!(self, load::note_node_load(worker, stream, load));
        let load = load?;
        let mut state = self.lock();
        let link = state
            .side
            .links
            .get(worker)
            .filter(|l| l.stream == stream)?;
        let session = Session::of(link.instance.as_str(), stream);
        let capacity = state.engine.sched.capacity(worker).unwrap_or_default();
        let was_limited = state
            .side
            .node_loads
            .get(worker)
            .is_some_and(|k| k.last.cpu_limited);
        let now = self.now();
        let fed = accept(
            &mut state.side.node_loads,
            worker,
            &session,
            load,
            &capacity,
            now,
        );
        let limited = state
            .side
            .node_loads
            .get(worker)
            .is_some_and(|k| k.last.cpu_limited);
        if limited && !was_limited && self.load_is_read() {
            tracing::warn!(
                %worker,
                "the node's actions cgroup has a CPU quota while --placement-load is on: \
                 its measured CPU says nothing of the load, so placement charges it by \
                 prediction alone; a node lifts the limit itself unless it runs \
                 --actions-cpu-max off or predates the lift, so start it without that \
                 flag or upgrade it"
            );
        }
        fed
    }

    /// What is kept of `worker`'s readings, on either core.
    #[cfg(test)]
    fn kept_load(&self, worker: &WorkerId) -> Option<Kept> {
        js_arm!(self, load::kept_load(worker));
        self.lock().side.node_loads.get(worker).cloned()
    }

    /// Whether placement reads the load the nodes report (`--placement-load on`).
    fn load_is_read(&self) -> bool {
        self.settings.placement_load == PlacementLoad::On
    }

    /// The nodes whose last accepted reading, if it is not stale, says the actions cgroup has
    /// a CPU quota while placement reads the load: a misconfiguration, exported as
    /// `kbf_placement_cpu_limited_nodes`. None while placement does not read the load.
    pub(super) fn cpu_limited_nodes(&self, state: &State) -> u64 {
        if !self.load_is_read() {
            return 0;
        }
        let now = self.now();
        let limited = state
            .side
            .node_loads
            .values()
            .filter(|kept| kept.last.cpu_limited && !kept.stale(now))
            .count();
        limited as u64
    }
}

#[cfg(test)]
mod tests;
