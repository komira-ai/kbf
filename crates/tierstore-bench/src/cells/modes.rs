//! The modes of the cold store an arm runs in: healthy, or a fault the proxy adds.
//!
//! A mode says which sizes it is run at, how many requests a pass sends and how many are in
//! flight. The put cells and the read cells have a table each, the plan's.

use crate::proxy::FaultMode;

/// 1 KiB.
pub const KIB: u64 = 1024;
/// 1 MiB.
pub const MIB: u64 = 1024 * KIB;

/// How an arm's cold store is treated.
pub struct Mode {
    /// The name in the arm label.
    pub name: &'static str,
    /// The fault the proxy is set to; `None` when no proxy is in the path.
    pub fault: Option<FaultMode>,
    /// The sizes the mode is run at.
    pub sizes: Sizes,
    /// Requests per pass.
    pub counts: Counts,
    /// Requests in flight; `None` for the plan's.
    pub in_flight: Option<u32>,
}

/// The sizes a mode is run at.
pub enum Sizes {
    /// Every size the run asks for.
    All,
    /// These, if the run asks for them.
    Listed(&'static [u64]),
    /// The smallest the run asks for: the fault is on the request, not the body.
    Smallest,
}

/// How many requests a pass of a mode sends.
pub enum Counts {
    /// The plan's, by size and requests in flight.
    Plan,
    /// This many.
    Fixed(u64),
    /// The count of the first row whose size is at least the arm's.
    BySize(&'static [(u64, u64)]),
}

impl Mode {
    /// Whether the mode is run at `size`, `smallest` being the smallest of the run.
    #[must_use]
    pub fn wants(&self, size: u64, smallest: u64) -> bool {
        match self.sizes {
            Sizes::All => true,
            Sizes::Listed(listed) => listed.contains(&size),
            Sizes::Smallest => size == smallest,
        }
    }

    /// Requests per pass at `size` with `in_flight` workers; `plan` is the cell's count for the
    /// modes that follow the plan.
    #[must_use]
    pub fn requests(&self, size: u64, in_flight: u32, plan: fn(u64, u32) -> u64) -> u64 {
        match &self.counts {
            Counts::Plan => plan(size, in_flight),
            Counts::Fixed(n) => *n,
            Counts::BySize(rows) => rows
                .iter()
                .find(|(up_to, _)| size <= *up_to)
                .map_or(0, |(_, n)| *n),
        }
    }
}

/// The mode of a run with no fault proxy: the store as it is.
fn healthy() -> Mode {
    Mode {
        name: "healthy",
        fault: None,
        sizes: Sizes::All,
        counts: Counts::Plan,
        in_flight: None,
    }
}

/// The control of every faulted arm: the proxy in the path, adding nothing.
fn proxy_pass() -> Mode {
    Mode {
        name: "proxy-pass",
        fault: Some(FaultMode::Pass),
        sizes: Sizes::All,
        counts: Counts::Plan,
        in_flight: None,
    }
}

/// A mode that adds `ms` milliseconds to every request.
fn slow(name: &'static str, ms: u64, sizes: Sizes, counts: Counts) -> Mode {
    Mode {
        name,
        fault: Some(FaultMode::Latency { ms }),
        sizes,
        counts,
        in_flight: Some(1),
    }
}

/// A mode in which the store does not answer: `requests` requests, 6 in flight.
fn stopped(name: &'static str, fault: FaultMode, requests: u64) -> Mode {
    Mode {
        name,
        fault: Some(fault),
        sizes: Sizes::Smallest,
        counts: Counts::Fixed(requests),
        in_flight: Some(6),
    }
}

/// The modes of the put cells: healthy; or, with a proxy, the control, +50 ms and +500 ms (4 KiB
/// 200 puts, 1 MiB 100, 16 MiB 20), a 10 MiB/s cap (1 MiB 50, 16 MiB 10), refused and
/// blackholed (30 puts, 6 in flight).
#[must_use]
pub fn put_modes(faults: bool) -> Vec<Mode> {
    if !faults {
        return vec![healthy()];
    }
    let latency = || Counts::BySize(&[(MIB - 1, 200), (MIB, 100), (u64::MAX, 20)]);
    let listed = Sizes::Listed(&[4 * KIB, MIB, 16 * MIB]);
    let listed_again = Sizes::Listed(&[4 * KIB, MIB, 16 * MIB]);
    vec![
        proxy_pass(),
        slow("latency-50ms", 50, listed, latency()),
        slow("latency-500ms", 500, listed_again, latency()),
        Mode {
            name: "bandwidth-10mib-s",
            fault: Some(FaultMode::Bandwidth {
                bytes_per_sec: 10 * MIB,
            }),
            sizes: Sizes::Listed(&[MIB, 16 * MIB]),
            counts: Counts::BySize(&[(MIB, 50), (u64::MAX, 10)]),
            in_flight: Some(1),
        },
        stopped("refused", FaultMode::Refuse, 30),
        stopped("blackhole", FaultMode::Blackhole, 30),
    ]
}

/// The modes of the read cells: healthy; or, with a proxy, the control at every size, then at
/// 1 MiB +50 ms and +500 ms (30 reads) and the store refusing and blackholing (30 reads, 6 in
/// flight).
#[must_use]
pub fn read_modes(faults: bool) -> Vec<Mode> {
    if !faults {
        return vec![healthy()];
    }
    vec![
        proxy_pass(),
        slow("latency-50ms", 50, Sizes::Listed(&[MIB]), Counts::Fixed(30)),
        slow(
            "latency-500ms",
            500,
            Sizes::Listed(&[MIB]),
            Counts::Fixed(30),
        ),
        stopped("refused", FaultMode::Refuse, 30),
        stopped("blackhole", FaultMode::Blackhole, 30),
    ]
}
