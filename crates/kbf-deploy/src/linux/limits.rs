//! Rules `caps`, `swap` and `oom` of [`super::modes`], for the units of the modes that
//! run builds (`daemon`, `storage-and-daemon`, `all-in-one`): their leases live in the
//! unit's cgroup, so the unit's own limits are every build's.
//!
//! - `caps`: no `MemoryMax=`, `MemoryHigh=`, `CPUQuota=` or `AllowedCPUs=` (in any
//!   section, drop-ins included). The cap on builds is the daemon's own
//!   `actions/memory.max` (`--actions-memory-max-gib`) and CPU is not capped: a limit on
//!   the unit would also cap the supervisor, or kill it with the builds when they reach
//!   it.
//! - `swap`: no `MemorySwapMax=`. The host's swapfile is for the leases, and the daemon
//!   kills a lease that swaps at its own cap; `MemorySwapMax=0` here would turn that into
//!   an OOM kill of the unit that holds every lease.
//! - `oom`: `OOMScoreAdjust=` is an integer below 0, the build containers' score (a
//!   host-wide OOM kills the highest score first, so a lease dies before the daemon that
//!   holds them). A unit with none has 0, the containers' score.
//!
//! A key set to an empty value is unset, as systemd reads it. What the rules cannot see:
//! a limit set on `kbf.slice` or by `systemctl set-property` (it lives under
//! `/etc/systemd/system.control`, outside the unit and its drop-ins).

use crate::unit::UnitFile;

/// The keys rule `caps` refuses.
const CAPS: [&str; 4] = ["MemoryMax", "MemoryHigh", "CPUQuota", "AllowedCPUs"];

/// One line per way the unit's own limits break the rules.
pub(super) fn check(unit: &UnitFile) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for key in CAPS {
        if let Some(v) = set(unit, key) {
            out.push((
                "caps",
                format!(
                    "sets {key}={v}: builds are capped by the daemon's actions/memory.max, \
                     and a limit on the unit caps the supervisor with them"
                ),
            ));
        }
    }
    if let Some(v) = set(unit, "MemorySwapMax") {
        out.push((
            "swap",
            format!(
                "sets MemorySwapMax={v}: its leases live in this unit's cgroup and may swap, \
                 and the daemon's swap watch kills a lease that swaps at its own cap"
            ),
        ));
    }
    match set(unit, "OOMScoreAdjust").map(|v| (v, v.parse::<i64>())) {
        Some((_, Ok(n))) if n < 0 => {}
        Some((v, Ok(_))) => out.push((
            "oom",
            format!(
                "has OOMScoreAdjust={v}, not below 0: a host-wide OOM kills the highest \
                 score first, and the build containers' is 0"
            ),
        )),
        Some((v, Err(_))) => out.push((
            "oom",
            format!("has OOMScoreAdjust={v}, which is not an integer"),
        )),
        None => out.push((
            "oom",
            "gives no OOMScoreAdjust= below 0: it defaults to 0, the build containers' score"
                .to_owned(),
        )),
    }
    out
}

/// The last value of `key` in any section, unless it is empty (systemd's reset).
fn set<'a>(unit: &'a UnitFile, key: &str) -> Option<&'a str> {
    unit.get(key).map(str::trim).filter(|v| !v.is_empty())
}

#[cfg(test)]
#[path = "limits_tests.rs"]
mod tests;
