//! Rule `cpuset`, moved from `kbf-deploy linux check-units`: the cgroup kbf runs in keeps
//! every CPU and NUMA node of the node. The daemon offers the node's CPUs to its leases,
//! and `--lease-placement=numa-node` pins each lease to one NUMA node's CPUs and memory; an
//! `AllowedCPUs=` or `AllowedMemoryNodes=` narrower than the node keeps the unit's cgroup,
//! and every lease in it, off the rest.
//!
//! check-units compared those two keys of the unit and `kbf.slice` (parsed as systemd parses
//! them) with the node's. The units are static now and a drop-in, `systemctl set-property`
//! or a parent cgroup can narrow them as well, so kbf reads what they cause: the
//! `cpuset.cpus.effective` and `cpuset.mems.effective` of its own cgroup (`/proc/self/cgroup`,
//! cgroup v2), from the nearest ancestor that has them, against the node's own sets, the
//! kernel's `/sys/devices/system/cpu/online` and `/sys/devices/system/node/has_memory` (the
//! nodes with memory, which is what `cpuset.mems.effective` follows; a memory-less node is
//! online but never in it). Where
//! one cannot be read (another system, a container without `/sys`), the list is not
//! compared and a warning says so. Only the container driver places leases, so only a
//! daemon on it is checked.

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use super::{Host, Needs, Report};

/// The cgroup file system.
const CGROUP_ROOT: &str = "/sys/fs/cgroup";

/// The keys the rule is about, what each lists, the kernel file with the node's own set,
/// and the cgroup file with the set the cgroup keeps.
const KEYS: [(&str, &str, &str, &str); 2] = [
    (
        "AllowedCPUs",
        "CPUs",
        "/sys/devices/system/cpu/online",
        "cpuset.cpus.effective",
    ),
    (
        "AllowedMemoryNodes",
        "NUMA nodes",
        "/sys/devices/system/node/has_memory",
        "cpuset.mems.effective",
    ),
];

/// A set of CPU or NUMA node numbers, as sorted, disjoint, non-adjacent ranges.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Ids(Vec<(u32, u32)>);

impl Ids {
    /// A list as the kernel writes it (`0-3,8`); `None` when a word is malformed or
    /// reversed.
    pub fn strict(text: &str) -> Option<Ids> {
        let mut ranges: Vec<(u32, u32)> = Vec::new();
        for word in text.trim().split(',') {
            let (a, b) = match word.split_once('-') {
                Some((a, b)) => (a.parse().ok()?, b.parse().ok()?),
                None => {
                    let n = word.parse().ok()?;
                    (n, n)
                }
            };
            if a > b {
                return None;
            }
            ranges.push((a, b));
        }
        ranges.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::with_capacity(ranges.len());
        for (a, b) in ranges {
            match merged.last_mut() {
                Some(last) if a <= last.1.saturating_add(1) => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        Some(Ids(merged))
    }

    /// Whether every number of `other` is in this set.
    pub fn covers(&self, other: &Ids) -> bool {
        other
            .0
            .iter()
            .all(|&(a, b)| self.0.iter().any(|&(x, y)| x <= a && b <= y))
    }
}

impl fmt::Display for Ids {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let words: Vec<String> = self
            .0
            .iter()
            .map(|&(a, b)| {
                if a == b {
                    a.to_string()
                } else {
                    format!("{a}-{b}")
                }
            })
            .collect();
        f.write_str(&words.join(","))
    }
}

pub(super) fn check(needs: &Needs, host: &dyn Host, report: &mut Report) {
    if !needs.cpuset {
        return;
    }
    let cgroup = match own_cgroup(host) {
        Ok(cgroup) => cgroup,
        Err(why) => {
            report.warnings.push(format!(
                "cpuset: {why}, so the cgroup's CPUs were not compared with the node's"
            ));
            return;
        }
    };
    for (key, what, online, file) in KEYS {
        let all = match list(host, Path::new(online)).map_err(|e| e.why) {
            Ok(all) => all,
            Err(why) => {
                report.warnings.push(format!(
                    "cpuset: the node's {what} cannot be read ({why}), so {key} was not compared"
                ));
                continue;
            }
        };
        match effective(host, &cgroup, file) {
            Ok(set) if set.covers(&all) => {}
            Ok(set) => report.refuse(
                format!(
                    "the cgroup {} allows only {key}={set}, narrower than the node's {what} {all}",
                    cgroup.display()
                ),
                "the daemon places leases on every one of them (--lease-placement=numa-node pins a \
                 lease to one NUMA node), and the cgroup keeps them off the rest",
                format!(
                    "remove {key}= from the unit, its drop-ins and kbf.slice (or whatever else \
                     narrows the cgroup), and restart"
                ),
            ),
            Err(why) => report.warnings.push(format!(
                "cpuset: the cgroup's {what} cannot be read ({why}), so {key} was not compared"
            )),
        }
    }
}

/// The cgroup v2 path of this process, from `/proc/self/cgroup`.
fn own_cgroup(host: &dyn Host) -> Result<PathBuf, String> {
    let file = Path::new("/proc/self/cgroup");
    let text = host
        .read(file)
        .map_err(|e| format!("{}: {e}", file.display()))?;
    String::from_utf8_lossy(&text)
        .lines()
        .find_map(|l| l.strip_prefix("0::").map(PathBuf::from))
        .ok_or_else(|| format!("{} names no cgroup v2 path", file.display()))
}

/// The set `file` holds in the nearest cgroup, from `cgroup` up to the root, that has it.
fn effective(host: &dyn Host, cgroup: &Path, file: &str) -> Result<Ids, String> {
    for dir in cgroup.ancestors() {
        let relative = dir.to_string_lossy();
        let path = Path::new(CGROUP_ROOT)
            .join(relative.trim_start_matches('/'))
            .join(file);
        match list(host, &path) {
            Err(e) if e.not_found => {}
            other => return other.map_err(|e| e.why),
        }
    }
    Err(format!(
        "no {file} in {} or above it: the cpuset controller is not enabled there",
        cgroup.display()
    ))
}

/// Why a list could not be read.
struct Unread {
    why: String,
    not_found: bool,
}

/// The set in the kernel file `path`.
fn list(host: &dyn Host, path: &Path) -> Result<Ids, Unread> {
    let bytes = host.read(path).map_err(|e| Unread {
        why: format!("{}: {e}", path.display()),
        not_found: e.kind() == io::ErrorKind::NotFound,
    })?;
    Ids::strict(&String::from_utf8_lossy(&bytes)).ok_or_else(|| Unread {
        why: format!("{} is not a list of numbers", path.display()),
        not_found: false,
    })
}

#[cfg(test)]
#[path = "cpuset_tests.rs"]
mod tests;
