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

/// Whether an arm of `in_flight` workers is run at `size`: up to 16 in flight at any size, more
/// only up to 1 MiB (64 in flight of 128 MiB bodies is memory the generator does not have).
#[must_use]
pub fn width_ok(in_flight: u32, size: u64) -> bool {
    in_flight <= 16 || size <= MIB
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

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(size: u64, in_flight: u32) -> u64 {
        size + u64::from(in_flight)
    }

    /// Catches a mode that is run at a size it should not be, or not run at one it should: all,
    /// the listed, and the smallest of the run only.
    #[test]
    fn a_mode_wants_the_sizes_it_names() {
        let modes = read_modes(true);
        let by_name = |name: &str| modes.iter().find(|m| m.name == name).unwrap();
        assert!(by_name("proxy-pass").wants(1, 1));
        assert!(by_name("latency-50ms").wants(MIB, 4096));
        assert!(!by_name("latency-50ms").wants(4096, 4096));
        assert!(by_name("refused").wants(4096, 4096));
        assert!(!by_name("refused").wants(MIB, 4096));
    }

    /// Catches a width allowed where it must not be: 16 in flight at any size, 64 only up to
    /// 1 MiB.
    #[test]
    fn wide_arms_are_for_small_objects_only() {
        assert!(width_ok(1, 128 * MIB));
        assert!(width_ok(16, 128 * MIB));
        assert!(width_ok(64, MIB));
        assert!(!width_ok(64, MIB + 1));
    }

    /// Catches a count taken from the wrong place: the plan's function, a fixed count, and the
    /// first row of a table whose size is at least the arm's (a size past the last row gets
    /// none).
    #[test]
    fn a_mode_counts_requests_by_the_plan_a_number_or_a_table() {
        let modes = put_modes(true);
        let by_name = |name: &str| modes.iter().find(|m| m.name == name).unwrap();
        assert_eq!(by_name("proxy-pass").requests(100, 7, plan), 107);
        assert_eq!(by_name("blackhole").requests(100, 7, plan), 30);
        let latency = by_name("latency-50ms");
        assert_eq!(latency.requests(4096, 1, plan), 200);
        assert_eq!(latency.requests(MIB, 1, plan), 100);
        assert_eq!(latency.requests(16 * MIB, 1, plan), 20);
        let table = Mode {
            counts: Counts::BySize(&[(10, 1)]),
            ..put_modes(true).remove(0)
        };
        assert_eq!(table.requests(11, 1, plan), 0);
    }

    /// Catches a run without a proxy getting a faulted mode, and the tables drifting from the
    /// plan's names: healthy alone; six modes of the put cells, five of the read cells.
    #[test]
    fn the_tables_have_the_plans_modes() {
        let names = |modes: Vec<Mode>| modes.iter().map(|m| m.name).collect::<Vec<_>>();
        assert_eq!(names(put_modes(false)), ["healthy"]);
        assert_eq!(names(read_modes(false)), ["healthy"]);
        assert_eq!(
            names(put_modes(true)),
            [
                "proxy-pass",
                "latency-50ms",
                "latency-500ms",
                "bandwidth-10mib-s",
                "refused",
                "blackhole"
            ]
        );
        assert_eq!(
            names(read_modes(true)),
            [
                "proxy-pass",
                "latency-50ms",
                "latency-500ms",
                "refused",
                "blackhole"
            ]
        );
        assert!(put_modes(false)[0].fault.is_none());
        assert_eq!(
            put_modes(true)[3].fault,
            Some(FaultMode::Bandwidth {
                bytes_per_sec: 10 * MIB
            })
        );
    }
}
