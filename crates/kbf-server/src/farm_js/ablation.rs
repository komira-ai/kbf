//! `kbf_ablation_*` on `GET /metrics`: what the js core did in a run, partitioned so that the
//! rows of the ablation (js alone, `--placement-load on`, `--placement-predictions` shadow or
//! on) can be told apart from one scrape, without reading the scheduler's whole counter set.
//!
//! - **Placements** (`kbf_ablation_placements_total{decision}`) are the scheduler's own
//!   load counters, split into disjoint cases: `by_load`, a lease the load rule admitted on a
//!   node with a usable reading; `fallback_charge`, one admitted on a node charged by
//!   prediction alone; `refused_over_capacity`, a queued request, counted once per round,
//!   that had a node with room apart from the load rule and found none it admitted. All three
//!   stay 0 while placement does not read the load.
//! - **Charges** (`kbf_ablation_charges_total{source}`) are the leases admitted with a
//!   learned time profile (`profile_cpu`, `profile_memory`) and those whose key had a curve
//!   that was not used (`curve_flat`), again the scheduler's counts.
//! - **Bookings** (`kbf_ablation_bookings_total{source}`) are counted here, once for each lease
//!   the core starts, by what the lease was charged: `prediction` (a learned estimate),
//!   `cold` (the cold costs of a key with no estimate), `retry` (a rerun up the memory
//!   ladder, charged at its rung) or `default` (the request's cores and no memory). A
//!   charge that is read back from the scheduler, so the counter follows what admission did
//!   and not what the adapter meant it to do.
//! - **OOM retries** (`kbf_ablation_oom_retries_total{rung_gib}`) are the leases given up
//!   after passing their own memory limit, by the whole GiB they run again at. No series is
//!   written for a rung nothing was retried at.
//!
//! The nodes charged by prediction because their cgroup has a CPU quota are not counted again
//! here: `kbf_placement_cpu_limited_nodes` is that gauge.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use kbf_sched::{LoadCounters, RequeueReason};
use kbf_types::LeaseKind;
use kbf_types::sched::usage::LeaseCost;

/// One GiB.
const GIB: u64 = 1 << 30;

/// What a lease was charged when the core started it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Booking {
    /// A learned estimate.
    Prediction,
    /// The cold costs of a key with no estimate.
    Cold,
    /// A rerun up the memory ladder.
    Retry,
    /// The request's cores and no memory.
    Default,
}

impl Booking {
    /// Every source, in the order the series are written.
    pub(crate) const ALL: [Self; 4] = [Self::Prediction, Self::Cold, Self::Retry, Self::Default];

    /// The value of the `source` label.
    const fn label(self) -> &'static str {
        match self {
            Self::Prediction => "prediction",
            Self::Cold => "cold",
            Self::Retry => "retry",
            Self::Default => "default",
        }
    }

    const fn slot(self) -> usize {
        self as usize
    }

    /// The source of `cost`, what admission charged a lease of `kind` (`None`: no cost is
    /// recorded for it, so it is the default). A VM is charged what it books, which is not a
    /// prediction. A prediction of no memory at all reads as the default.
    pub(crate) fn of(kind: LeaseKind, cost: Option<LeaseCost>) -> Self {
        match cost {
            Some(cost) if kind != LeaseKind::Vm => {
                if cost.retry {
                    Self::Retry
                } else if cost.cold {
                    Self::Cold
                } else if cost.memory_bytes > 0 {
                    Self::Prediction
                } else {
                    Self::Default
                }
            }
            _ => Self::Default,
        }
    }
}

/// The counts the adapter keeps, since the process started.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Ablation {
    bookings: [u64; 4],
    /// Retries by the whole GiB of the rung they run again at.
    oom_retries: BTreeMap<u64, u64>,
}

impl Ablation {
    /// Counts one lease started with `booking`.
    pub(crate) fn booked(&mut self, booking: Booking) {
        self.bookings[booking.slot()] += 1;
    }

    /// Counts a lease given up for `reason`: a retry if the action passed its own memory
    /// limit, nothing for any other reason.
    pub(crate) fn requeued(&mut self, reason: RequeueReason) {
        if let RequeueReason::OutOfMemory { raised, .. } = reason {
            *self.oom_retries.entry(raised / GIB).or_default() += 1;
        }
    }

    /// The count of leases started with `booking`.
    #[cfg(test)]
    pub(crate) fn bookings(&self, booking: Booking) -> u64 {
        self.bookings[booking.slot()]
    }

    /// The count of retries that run again at `gib` GiB.
    #[cfg(test)]
    pub(crate) fn oom_retries(&self, gib: u64) -> u64 {
        self.oom_retries.get(&gib).copied().unwrap_or(0)
    }

    /// The disjoint placement cases of `counters`: `[by_load, fallback_charge,
    /// refused_over_capacity]`.
    pub(crate) const fn placements(counters: &LoadCounters) -> [u64; 3] {
        [
            counters.admitted.saturating_sub(counters.admitted_fallback),
            counters.admitted_fallback,
            counters.blocked,
        ]
    }

    /// Appends the families to `out`: the adapter's counts, and the scheduler's `counters`
    /// as the placement and charge cases.
    pub(crate) fn render(&self, out: &mut String, counters: &LoadCounters) {
        let [by_load, fallback, refused] = Self::placements(counters);
        family(
            out,
            "kbf_ablation_placements_total",
            "Placements by the load rule, by decision: a lease admitted on a node with a usable \
             reading (by_load), admitted on a node charged by prediction alone \
             (fallback_charge), and queued requests, once per round, refused for want of \
             capacity under the rule (refused_over_capacity); all 0 while placement does not \
             read the load",
            "decision",
            &[
                ("by_load", by_load),
                ("fallback_charge", fallback),
                ("refused_over_capacity", refused),
            ],
        );
        family(
            out,
            "kbf_ablation_charges_total",
            "Leases admitted by load, by how their key's learned curve was used: charged by \
             curve in CPU, charged by curve in memory, or the key had a curve that was not used",
            "source",
            &[
                ("profile_cpu", counters.profile_cpu),
                ("profile_memory", counters.profile_memory),
                ("curve_flat", counters.curve_flat),
            ],
        );
        let bookings: Vec<_> = Booking::ALL
            .into_iter()
            .map(|b| (b.label(), self.bookings[b.slot()]))
            .collect();
        family(
            out,
            "kbf_ablation_bookings_total",
            "Leases started, by what admission charged them: a learned estimate (prediction), \
             the cold costs of a key with none (cold), a rerun up the memory ladder (retry), \
             or the request's cores and no memory (default)",
            "source",
            &bookings,
        );
        let name = "kbf_ablation_oom_retries_total";
        let _ = writeln!(
            out,
            "# HELP {name} Leases given up after passing their own memory limit, by the whole \
             GiB they run again at\n# TYPE {name} counter"
        );
        for (gib, count) in &self.oom_retries {
            let _ = writeln!(out, "{name}{{rung_gib=\"{gib}\"}} {count}");
        }
    }
}

/// One counter family with a series for each of `samples`, labelled `label`.
fn family(out: &mut String, name: &str, help: &str, label: &str, samples: &[(&str, u64)]) {
    let _ = writeln!(out, "# HELP {name} {help}\n# TYPE {name} counter");
    for (value, count) in samples {
        let _ = writeln!(out, "{name}{{{label}=\"{value}\"}} {count}");
    }
}
