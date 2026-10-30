//! The closed minutes of each running lease's usage, and each lease's start clock, for the
//! heartbeat's `NodeLoad` (`lease_clocks` and `minutes`).
//!
//! The sampler is the heartbeat: every beat reads the node and each running lease once
//! ([`crate::runtime::Runtime::node_load`]) and [`LeaseMinutes::beat`] feeds that one reading
//! to the jobscheduler's `Binner`. There is no second timer and no second read. So there
//! are no samples while no stream is up (a gap of more than three windows is dropped and
//! counted), the first sample of a lease is its baseline, and a lease that runs for less
//! than about 35 seconds has no minute: absent, never zero.
//!
//! Delivery. At most [`MAX_LOAD_MINUTES`] minutes ride a heartbeat and one batch is in
//! flight at a time. A `HeartbeatAck` of `seq` is cumulative (the stream is ordered and the
//! server notes the load before it acknowledges), so it frees the batch sent at `seq` or
//! before. A stream that ends with the batch not acknowledged may or may not have
//! delivered it, so the collector keeps the batch itself (it does not hand it back to the
//! binner) and the next stream sends it first and alone: it is at most
//! [`MAX_LOAD_MINUTES`], and every minute still in the binner is newer than every minute in
//! it. The scheduler ignores a minute not above the newest it has seen of a lease, so a
//! lease's minutes must reach it in ascending order; handing the batch back to the binner
//! would stamp its minutes with the latest close time and let a newer minute of a lease
//! go out ahead of the older one when more than a batch is waiting. A minute sent twice is
//! merged by the receiver with the larger value of each field. A kept batch is dropped and
//! counted once it is older than the binner's age limit, but it ages from when the batch was
//! first taken (it keeps that time across streams), not from when its minutes closed, so it
//! can outlive the binner's own limit by up to one batch interval. A server that withholds
//! acknowledgements keeps the batch in flight; minutes that close under it wait in the
//! binner, which drops those older than ten minutes.
//!
//! A lease's last, partial minute is closed by [`LeaseMinutes::lease_ended`] and rides the
//! first heartbeat after it, not the lease's Result.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use js_agent_usage::{Binner, ClosedMinute, Counters, LeaseSample, MAX_UNSENT_AGE_USEC, Sample};
use kbf_proto::worker::{self, LeaseClock, LeaseMinute};
use kbf_proto::{MAX_LOAD_LEASES, MAX_LOAD_MINUTES};
use kbf_types::LeaseId;

use crate::lease::proto_lease_id;
use crate::runtime::NodeReading;

/// The least sampling window: a heartbeat interval shorter than this (the flag accepts one
/// millisecond) would turn every window into a gap, and no minute would ever close.
const USAGE_WINDOW_FLOOR: Duration = Duration::from_secs(1);

/// The window a stream bins by: `window` when set (tests), else the Welcome's heartbeat
/// `interval`, at least [`USAGE_WINDOW_FLOOR`].
pub(crate) fn usage_window(interval: Duration, window: Option<Duration>) -> Duration {
    window.unwrap_or_else(|| interval.max(USAGE_WINDOW_FLOOR))
}

/// What one heartbeat carries besides the node's own counters.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Attached {
    /// The start of each running lease, oldest first, at most [`MAX_LOAD_LEASES`].
    pub(crate) clocks: Vec<LeaseClock>,
    /// The leases the reading covers, in the order of `clocks`, at most [`MAX_LOAD_LEASES`].
    pub(crate) covered: Vec<worker::LeaseId>,
    /// The minutes that closed, oldest minute first, at most [`MAX_LOAD_MINUTES`].
    pub(crate) minutes: Vec<LeaseMinute>,
}

/// A batch sent and not yet acknowledged.
struct InFlight {
    /// The heartbeat that carried it.
    seq: u64,
    batch: Batch,
}

/// Minutes taken from the binner, with the reading clock at which they were first taken (no
/// minute of the batch closed later than that).
struct Batch {
    minutes: Vec<ClosedMinute>,
    taken_usec: u64,
}

/// The minutes of one stream's leases: see the module documentation.
pub(crate) struct LeaseMinutes {
    binner: Binner,
    in_flight: Option<InFlight>,
    /// The batch a stream ended on without its acknowledgement: it goes out first and alone.
    held: Option<Batch>,
    /// Minutes of a held batch dropped for age.
    held_aged_out: u64,
    /// The kinds of odd readings already logged, so each is logged once.
    logged: BTreeSet<&'static str>,
}

impl LeaseMinutes {
    /// A collector with the floor window, until the first stream names its own.
    pub(crate) fn new() -> Self {
        Self {
            binner: Binner::new(micros(USAGE_WINDOW_FLOOR)),
            in_flight: None,
            held: None,
            held_aged_out: 0,
            logged: BTreeSet::new(),
        }
    }

    /// A stream began: its heartbeats are numbered from 1 again and bin by `window`. The
    /// batch the last stream left unacknowledged is kept to be sent once more, first.
    pub(crate) fn new_stream(&mut self, window: Duration) {
        if let Some(sent) = self.in_flight.take() {
            self.held = Some(sent.batch);
        }
        self.binner.set_window(micros(window));
    }

    /// Takes the reading of heartbeat `seq` and returns what that heartbeat carries.
    pub(crate) fn beat(&mut self, seq: u64, reading: &NodeReading) -> Attached {
        self.binner.observe(&sample(reading));

        let mut order: Vec<_> = reading
            .leases
            .iter()
            .map(|lease| (lease.started_usec, lease.lease.term, lease.lease.seq))
            .collect();
        order.sort_unstable();
        let clocks = order
            .iter()
            .take(MAX_LOAD_LEASES)
            .map(|&(started, term, lease_seq)| LeaseClock {
                lease: Some(worker::LeaseId {
                    term,
                    seq: lease_seq,
                }),
                started_mono_usec: started,
            })
            .collect();
        // The covered leases in the order of the clocks, one not listed last, so both
        // lists lose the same newest leases on a node with more than the cap.
        let rank: BTreeMap<(u64, u64), usize> = order
            .iter()
            .enumerate()
            .map(|(at, &(_, term, lease_seq))| ((term, lease_seq), at))
            .collect();
        let mut covered: Vec<_> = reading.load.covered.clone();
        covered.sort_by_key(|id| {
            let at = rank.get(&(id.term, id.seq)).copied();
            (at.is_none(), at, id.term, id.seq)
        });
        covered.truncate(MAX_LOAD_LEASES);

        let minutes = self.take_minutes(seq, reading.load.mono_usec);
        self.warn_odd_readings();
        Attached {
            clocks,
            covered,
            minutes,
        }
    }

    /// The minutes this heartbeat carries: a new batch when none is in flight.
    fn take_minutes(&mut self, seq: u64, now_usec: u64) -> Vec<LeaseMinute> {
        if self.in_flight.is_some() {
            // Ages what is pending past the binner's limit and returns nothing.
            self.binner.take(0, now_usec);
            return Vec::new();
        }
        let held = self.held.take().filter(|held| {
            let fresh = now_usec.saturating_sub(held.taken_usec) <= MAX_UNSENT_AGE_USEC;
            if !fresh {
                let dropped = u64::try_from(held.minutes.len()).unwrap_or(u64::MAX);
                self.held_aged_out = self.held_aged_out.saturating_add(dropped);
            }
            fresh
        });
        let batch = match held {
            Some(held) => {
                // Alone: whatever the binner holds closed after it. Still ages the pending.
                self.binner.take(0, now_usec);
                held
            }
            None => Batch {
                minutes: self.binner.take(MAX_LOAD_MINUTES, now_usec),
                taken_usec: now_usec,
            },
        };
        let mut wire: Vec<_> = batch
            .minutes
            .iter()
            .filter_map(|minute| {
                let lease = lease_of(&minute.lease_id)?;
                Some((minute.minute, lease, minute))
            })
            .collect();
        // Per lease the minutes go ascending (the scheduler ignores one not above the
        // newest it has seen of a lease), whatever order the binner returned them in. Across
        // batches the order holds because a batch is taken oldest close first and a kept
        // one is sent before anything newer is taken.
        wire.sort_by_key(|&(minute, lease, _)| (minute, lease.term, lease.seq));
        let wire = wire
            .into_iter()
            .map(|(_, lease, minute)| LeaseMinute {
                lease: Some(proto_lease_id(lease)),
                minute: u32::from(minute.minute),
                cpu_mean_millis: minute.cpu_mean_millis,
                cpu_peak_window_millis: minute.cpu_peak_window_millis,
                ws_max_bytes: minute.ws_max_bytes,
            })
            .collect();
        if !batch.minutes.is_empty() {
            self.in_flight = Some(InFlight { seq, batch });
        }
        wire
    }

    /// Heartbeat `seq` was acknowledged, and so were the ones before it.
    pub(crate) fn acked(&mut self, seq: u64) {
        if self.in_flight.as_ref().is_some_and(|sent| sent.seq <= seq) {
            self.in_flight = None;
        }
    }

    /// `lease` ended: its open minute closes (if it ran long enough) and goes out on the
    /// next heartbeat.
    pub(crate) fn lease_ended(&mut self, lease: LeaseId) {
        self.binner.lease_ended(&key(lease));
    }

    /// The heartbeat the batch in flight rides on, if any.
    #[cfg(test)]
    pub(crate) fn in_flight(&self) -> Option<u64> {
        self.in_flight.as_ref().map(|sent| sent.seq)
    }

    /// How many minutes wait, in the binner or in a kept batch, for a heartbeat to carry them.
    #[cfg(test)]
    pub(crate) fn pending(&self) -> usize {
        self.binner.pending() + self.held.as_ref().map_or(0, |held| held.minutes.len())
    }

    /// What the binner counted of readings it could not use, with the minutes of a kept
    /// batch dropped for age added to `aged_out`.
    fn counters(&self) -> Counters {
        let mut counters = self.binner.counters();
        counters.aged_out = counters.aged_out.saturating_add(self.held_aged_out);
        counters
    }

    /// The kinds of odd reading seen for the first time: the binner counts a window that
    /// is too long (`gaps`), a counter that went back, a clock that went back, and minutes
    /// it dropped for age; each kind is logged once.
    fn fresh_kinds(&mut self) -> Vec<&'static str> {
        let Counters {
            gaps,
            counter_back,
            clock_back,
            aged_out,
            ..
        } = self.counters();
        [
            ("gaps", gaps),
            ("counter_back", counter_back),
            ("clock_back", clock_back),
            ("aged_out", aged_out),
        ]
        .into_iter()
        .filter(|&(kind, count)| count > 0 && self.logged.insert(kind))
        .map(|(kind, _)| kind)
        .collect()
    }

    fn warn_odd_readings(&mut self) {
        for kind in self.fresh_kinds() {
            tracing::warn!(kind, "lease usage reading not binned as is: counted");
        }
    }
}

fn micros(window: Duration) -> u64 {
    u64::try_from(window.as_micros()).unwrap_or(u64::MAX)
}

/// The binner's sample of a reading: every lease that has counters, and how many have none.
fn sample(reading: &NodeReading) -> Sample {
    let mut leases = Vec::new();
    let mut unreadable_leases = 0u32;
    for lease in &reading.leases {
        match lease.usage {
            Some(usage) => leases.push(LeaseSample {
                lease_id: key(lease.lease),
                started_usec: lease.started_usec,
                cpu_usage_usec: usage.cpu_usage_usec,
                working_set_bytes: usage.working_set_bytes,
            }),
            None => unreadable_leases = unreadable_leases.saturating_add(1),
        }
    }
    Sample {
        mono_usec: reading.load.mono_usec,
        cpu_usage_usec: reading.load.cpu_usage_usec,
        working_set_bytes: reading.load.memory_working_set_bytes,
        active_file_bytes: reading.load.memory_active_file_bytes,
        cpu_limited: reading.load.actions_cpu_limited,
        leases,
        unreadable_leases,
    }
}

/// The binner's name for a lease (it treats it as opaque).
fn key(lease: LeaseId) -> String {
    format!("{}-{}", lease.term, lease.seq)
}

/// The lease a [`key`] names; `None` for a string `key` never makes.
fn lease_of(name: &str) -> Option<LeaseId> {
    let (term, seq) = name.split_once('-')?;
    let lease = LeaseId::new(term.parse().ok()?, seq.parse().ok()?);
    (key(lease) == name).then_some(lease)
}

#[cfg(test)]
#[path = "lease_minutes_tests.rs"]
mod tests;
