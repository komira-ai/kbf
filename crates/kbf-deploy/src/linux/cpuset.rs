//! Rule `cpuset` of [`super::modes`]: the unit of a mode that runs builds, and
//! `kbf.slice` above it, keep every CPU and NUMA node of the node. The daemon offers the
//! node's CPUs to its leases, and `--lease-placement=numa-node` pins each lease to one
//! NUMA node's CPUs and memory; an `AllowedCPUs=` or `AllowedMemoryNodes=` narrower
//! than the node keeps the unit's cgroup, and every lease in it, off the rest.
//!
//! The lists are read as systemd reads them (systemd.resource-control(5); its
//! `config_parse_cpu_set`): numbers and `a-b` ranges separated by whitespace or commas,
//! each assignment adding to the earlier ones, an empty one resetting the list, a
//! malformed word ignored. The node's own sets are the kernel's
//! `/sys/devices/system/cpu/online` and `/sys/devices/system/node/online`, read on the
//! host `check-units` runs on: run it on the node. Where they cannot be read (another
//! system, a container without `/sys`), a set list is not compared and a
//! `WARN cpuset:` line says so. `StartupAllowedCPUs=` and `StartupAllowedMemoryNodes=`
//! (boot and shutdown only) are not read.

use std::fmt;

use kbf_host::exec::{Entry, Exec};
use kbf_host::path::HostPath;

use crate::unit::UnitFile;

/// systemd's bound on a CPU number (`CPU_SET_MAX_NCPU`): a word at or past it is ignored.
const MAX_ID: u32 = 8192;

/// The keys the rule reads, what each lists, and the kernel file with the node's own.
pub(super) const KEYS: [(&str, &str, &str); 2] = [
    ("AllowedCPUs", "CPUs", "/sys/devices/system/cpu/online"),
    (
        "AllowedMemoryNodes",
        "NUMA nodes",
        "/sys/devices/system/node/online",
    ),
];

/// A set of CPU or NUMA node numbers, as sorted, disjoint, non-adjacent ranges.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct Ids(Vec<(u32, u32)>);

impl Ids {
    /// A list as systemd reads it: malformed words, `b-a` and numbers past
    /// systemd's bound are ignored.
    pub fn lenient(value: &str) -> Ids {
        let mut ids = Ids::default();
        for word in value
            .split(|c: char| c.is_whitespace() || c == ',')
            .filter(|w| !w.is_empty())
        {
            if let Some((a, b)) = range(word).filter(|&(_, b)| b < MAX_ID) {
                ids.add(a, b);
            }
        }
        ids
    }

    /// A list as the kernel writes it (`0-3,8`); `None` when a word is malformed.
    pub fn strict(text: &str) -> Option<Ids> {
        let mut ids = Ids::default();
        for word in text.trim().split(',') {
            let (a, b) = range(word)?;
            ids.add(a, b);
        }
        Some(ids)
    }

    fn add(&mut self, a: u32, b: u32) {
        self.0.push((a, b));
        self.0.sort_unstable();
        let mut merged: Vec<(u32, u32)> = Vec::with_capacity(self.0.len());
        for &(a, b) in &self.0 {
            match merged.last_mut() {
                Some(last) if a <= last.1.saturating_add(1) => last.1 = last.1.max(b),
                _ => merged.push((a, b)),
            }
        }
        self.0 = merged;
    }

    fn extend(&mut self, other: &Ids) {
        for &(a, b) in &other.0 {
            self.add(a, b);
        }
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
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

/// `n` or `a-b` with `a <= b`.
fn range(word: &str) -> Option<(u32, u32)> {
    let (a, b) = word.split_once('-').unwrap_or((word, word));
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(a) || !digits(b) {
        return None;
    }
    let (a, b) = (a.parse().ok()?, b.parse().ok()?);
    (a <= b).then_some((a, b))
}

/// The set `key` in `[section]` leaves, every assignment applied in order; `None` when
/// none is left (never set, reset, or only malformed words): no restriction.
pub(super) fn allowed(unit: &UnitFile, section: &str, key: &str) -> Option<Ids> {
    let mut ids = Ids::default();
    for d in unit
        .directives()
        .iter()
        .filter(|d| d.section == section && d.key == key)
    {
        if d.value.trim().is_empty() {
            ids = Ids::default();
        } else {
            ids.extend(&Ids::lenient(&d.value));
        }
    }
    (!ids.is_empty()).then_some(ids)
}

/// The node's set from the kernel file `file`, or why it cannot be read.
pub(super) fn node<E: Exec>(exec: &E, file: &str) -> Result<Ids, String> {
    let path = HostPath::new(file).map_err(|e| e.to_string())?;
    match exec.inspect(&path) {
        Ok(Some(Entry::File { bytes, .. })) => Ids::strict(&String::from_utf8_lossy(&bytes))
            .ok_or_else(|| format!("{file} is not a list of numbers")),
        Ok(Some(_)) => Err(format!("{file} is not a file")),
        Ok(None) => Err(format!("no {file}")),
        Err(e) => Err(format!("{file}: {e}")),
    }
}

/// Rule `cpuset` on `unit` (the file `name`, its keys in `[section]`): failures, and
/// `WARN cpuset:` notes for a list that could not be compared.
pub(super) fn check<E: Exec>(
    exec: &E,
    name: &str,
    unit: &UnitFile,
    section: &str,
    failures: &mut Vec<String>,
    notes: &mut Vec<String>,
) {
    for (key, what, file) in KEYS {
        let Some(set) = allowed(unit, section, key) else {
            continue;
        };
        match node(exec, file) {
            Ok(all) if set.covers(&all) => {}
            Ok(all) => failures.push(format!(
                "{name} sets {key}={set}, narrower than the node's {what} {all}: the daemon \
                 places leases on every one of them (--lease-placement=numa-node pins a \
                 lease to one NUMA node), and the unit's cgroup keeps them off the rest"
            )),
            Err(why) => notes.push(format!(
                "WARN cpuset: {name} sets {key}={set}, and the node's {what} cannot be read \
                 here ({why}): run check-units on the node to compare them"
            )),
        }
    }
}

#[cfg(test)]
#[path = "cpuset_tests.rs"]
mod tests;
