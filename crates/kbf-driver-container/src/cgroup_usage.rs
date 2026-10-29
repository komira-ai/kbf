//! What a lease cgroup counted, read when the action ended (`cpu.stat`, `memory.peak`,
//! `memory.numa_stat`) into the [`CgroupUsage`] the lease's result carries.
//!
//! Measurement only: nothing here decides anything. A file that is not there (cgroup
//! v1, a kernel without `memory.peak`, a build without NUMA) leaves its fields absent,
//! and a file that does not parse does too; neither is an error, since the lease's
//! result does not depend on them.
//!
//! The NUMA split is the one a sampler saw while the action ran, if it held more than
//! at its end: the kernel uncharges anonymous memory when the processes exit, so
//! `memory.numa_stat` after the end shows little more than page cache ([`NumaSample`]).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kbf_proto::worker::{CgroupUsage, NumaNodeBytes, ResourceUsage, UsageBasis};

use crate::off_worker::off_worker;

/// How often [`NumaSample::watch`] reads `memory.numa_stat` while the action runs, unless
/// `ContainerConfig::numa_every` says otherwise.
pub(crate) const NUMA_EVERY: Duration = Duration::from_secs(1);

/// The counters of `cpu.stat` the usage takes.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct CpuStat {
    pub(crate) usage: Option<u64>,
    user: Option<u64>,
    system: Option<u64>,
    periods: Option<u64>,
    throttled: Option<u64>,
    pub(crate) throttled_usec: Option<u64>,
}

/// The `key value` lines of `cpu.stat`: each wanted key's value that parses.
pub(crate) fn cpu_stat(text: &str) -> CpuStat {
    let mut stat = CpuStat::default();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        let (Some(key), Some(value)) = (words.next(), words.next()) else {
            continue;
        };
        let Ok(value) = value.parse::<u64>() else {
            continue;
        };
        match key {
            "usage_usec" => stat.usage = Some(value),
            "user_usec" => stat.user = Some(value),
            "system_usec" => stat.system = Some(value),
            "nr_periods" => stat.periods = Some(value),
            "nr_throttled" => stat.throttled = Some(value),
            "throttled_usec" => stat.throttled_usec = Some(value),
            _ => {}
        }
    }
    stat
}

/// The `max` and `oom_kill` counts of a `memory.events`.
fn memory_events(text: &str) -> (Option<u64>, Option<u64>) {
    let count = |key: &str| {
        text.lines().find_map(|line| {
            let mut words = line.split_whitespace();
            (words.next()? == key).then(|| words.next()?.parse().ok())?
        })
    };
    (count("max"), count("oom_kill"))
}

/// The `total` (microseconds stalled) of the `kind` (`some` or `full`) line of a
/// pressure file (`some avg10=0.00 avg60=0.00 avg300=0.00 total=4120`).
fn pressure_total(text: &str, kind: &str) -> Option<u64> {
    let line = text
        .lines()
        .find(|line| line.split_whitespace().next() == Some(kind))?;
    line.split_whitespace()
        .find_map(|field| field.strip_prefix("total="))?
        .parse()
        .ok()
}

/// How many CPUs a `cpuset.cpus.effective` list (`0-3,8,10-11`) names; `None` for an
/// empty or malformed list.
fn cpu_list_len(text: &str) -> Option<u32> {
    let mut count: u32 = 0;
    for part in text.trim().split(',') {
        let (lo, hi) = match part.split_once('-') {
            Some((lo, hi)) => (lo.parse::<u32>().ok()?, hi.parse::<u32>().ok()?),
            None => {
                let one = part.parse::<u32>().ok()?;
                (one, one)
            }
        };
        count = count.checked_add(hi.checked_sub(lo)?.checked_add(1)?)?;
    }
    Some(count)
}

/// The `anon` bytes of a `memory.stat`.
fn anon_bytes(text: &str) -> Option<u64> {
    text.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        (words.next()? == "anon").then(|| words.next()?.parse().ok())?
    })
}

/// `anon` plus `file` bytes per node of a `memory.numa_stat` (lines such as
/// `anon N0=4096 N1=8192`; cgroup v2 counts bytes). Empty if neither line is there.
fn numa_stat(text: &str) -> BTreeMap<u32, u64> {
    let mut nodes = BTreeMap::new();
    for line in text.lines() {
        let mut words = line.split_whitespace();
        if !matches!(words.next(), Some("anon" | "file")) {
            continue;
        }
        for word in words {
            let Some((node, bytes)) = word.strip_prefix('N').and_then(|w| w.split_once('=')) else {
                continue;
            };
            if let (Ok(node), Ok(bytes)) = (node.parse::<u32>(), bytes.parse::<u64>()) {
                let held: &mut u64 = nodes.entry(node).or_default();
                *held = held.saturating_add(bytes);
            }
        }
    }
    nodes
}

fn total(nodes: &BTreeMap<u32, u64>) -> u64 {
    nodes.values().fold(0, |a, b| a.saturating_add(*b))
}

/// The number in the file `name` of the cgroup `dir`, if it is there and parses.
fn read_u64(dir: &Path, name: &str) -> Option<u64> {
    std::fs::read_to_string(dir.join(name))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// What one reading of a cgroup's memory files found.
struct Sample {
    /// Anon + swap, when both files were there.
    anon_swap: Option<u64>,
    /// The NUMA split, when `memory.numa_stat` could be read.
    numa: Option<BTreeMap<u32, u64>>,
}

/// Reads the memory files of the cgroup `dir`. Blocking.
fn read_sample(dir: &Path) -> Sample {
    // Anon + swap is what the action touched, without the page cache `memory.peak`
    // counts; both files must be there, or the sample is not anon + swap.
    let anon_swap = match (
        std::fs::read_to_string(dir.join("memory.stat"))
            .ok()
            .and_then(|text| anon_bytes(&text)),
        read_u64(dir, "memory.swap.current"),
    ) {
        (Some(anon), Some(swap)) => Some(anon.saturating_add(swap)),
        _ => None,
    };
    let numa = std::fs::read_to_string(dir.join("memory.numa_stat"))
        .ok()
        .map(|text| numa_stat(&text));
    Sample { anon_swap, numa }
}

/// The NUMA split of the sample that held the most memory.
#[derive(Debug, Default)]
pub(crate) struct NumaSample {
    best: BTreeMap<u32, u64>,
    /// The largest anon + swap any sample read.
    anon_swap_peak: Option<u64>,
    /// Counts each sample that read some memory, if the runtime counts them.
    counted: Option<Arc<AtomicU64>>,
}

impl NumaSample {
    /// A sample that adds one to `counted` for each reading of `memory.numa_stat` that
    /// held memory ([`crate::ContainerRuntime::numa_samples`]).
    pub(crate) fn counting(counted: Arc<AtomicU64>) -> Self {
        Self {
            best: BTreeMap::new(),
            anon_swap_peak: None,
            counted: Some(counted),
        }
    }

    /// Reads `memory.numa_stat` of the cgroup `dir` and keeps it if it holds more than
    /// every earlier sample (a sample that cannot be read changes nothing).
    pub(crate) fn observe(&mut self, dir: &Path) {
        self.keep(read_sample(dir));
    }

    /// Keeps `sample` if it holds more than every earlier one.
    fn keep(&mut self, sample: Sample) {
        if let Some(now) = sample.anon_swap {
            self.anon_swap_peak = Some(self.anon_swap_peak.map_or(now, |p| p.max(now)));
        }
        let Some(now) = sample.numa else {
            return;
        };
        if let Some(counted) = &self.counted {
            counted.fetch_add(u64::from(total(&now) > 0), Ordering::Release);
        }
        if total(&now) > total(&self.best) {
            self.best = now;
        }
    }

    /// Samples every `every` (the driver passes [`NUMA_EVERY`]), for as long as it is
    /// polled.
    ///
    /// The files are read on the blocking pool: `memory.numa_stat` and `memory.stat` of a
    /// busy cgroup can take the kernel a while, and this runs for every lease.
    pub(crate) async fn watch(&mut self, dir: &Path, every: Duration) {
        let dir = dir.to_owned();
        loop {
            tokio::time::sleep(every).await;
            let at = dir.clone();
            self.keep(off_worker(move || read_sample(&at)).await);
        }
    }

    /// The usage of the cgroup `dir`, read after the action's processes have exited
    /// (the cgroup is not removed yet), with the best NUMA sample: the end counts as
    /// one more sample.
    pub(crate) fn finish(mut self, dir: &Path) -> CgroupUsage {
        self.observe(dir);
        let stat = std::fs::read_to_string(dir.join("cpu.stat"))
            .map(|text| cpu_stat(&text))
            .unwrap_or_default();
        let read = |name: &str| std::fs::read_to_string(dir.join(name)).ok();
        let events = read("memory.events")
            .map(|text| memory_events(&text))
            .unwrap_or_default();
        let cpu_pressure = read("cpu.pressure");
        let memory_pressure = read("memory.pressure");
        CgroupUsage {
            cpu_usage_micros: stat.usage,
            cpu_user_micros: stat.user,
            cpu_system_micros: stat.system,
            cpu_periods: stat.periods,
            cpu_throttled_periods: stat.throttled,
            cpu_throttled_micros: stat.throttled_usec,
            memory_peak_bytes: read_u64(dir, "memory.peak"),
            memory_events_max: events.0,
            memory_events_oom_kill: events.1,
            memory_swap_peak_bytes: read_u64(dir, "memory.swap.peak"),
            memory_anon_swap_peak_bytes: self.anon_swap_peak,
            cpu_pressure_some_micros: cpu_pressure
                .as_deref()
                .and_then(|text| pressure_total(text, "some")),
            memory_pressure_some_micros: memory_pressure
                .as_deref()
                .and_then(|text| pressure_total(text, "some")),
            memory_pressure_full_micros: memory_pressure
                .as_deref()
                .and_then(|text| pressure_total(text, "full")),
            cpus_effective: std::fs::read_to_string(dir.join("cpuset.cpus.effective"))
                .ok()
                .and_then(|text| cpu_list_len(&text)),
            // The daemon fills it in once the run has ended (`Runtime::take_peak_window_millicores`).
            peak_window_millicores: None,
            numa: self
                .best
                .into_iter()
                .map(|(node, bytes)| NumaNodeBytes { node, bytes })
                .collect(),
        }
    }
}

/// Where the kernel reports CPU pressure stall information.
pub(crate) const CPU_PSI: &str = "/proc/pressure/cpu";

/// What the lease looked like when it started, for the usage it reports: the node's CPU
/// pressure then and the CPU the scheduler booked. A sample of CPU use taken on a busy
/// node understates the need, so the estimator reads the pressure beside it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Start {
    /// `some avg10` of the CPU pressure file, in hundredths of a percent; `None` when
    /// the file is missing or does not parse (a kernel without pressure information).
    pub(crate) cpu_pressure_centi: Option<u32>,
    /// The CPU booked for the lease, in thousandths of a CPU; `None` when not booked.
    pub(crate) booked_millicpus: Option<u64>,
}

impl Start {
    /// The start of a lease that booked `millicpus` (0 is not booked), reading the CPU
    /// pressure file at `psi`.
    pub(crate) fn read(psi: &Path, millicpus: u64) -> Self {
        Self {
            cpu_pressure_centi: std::fs::read_to_string(psi)
                .ok()
                .and_then(|text| some_avg10_centi(&text)),
            booked_millicpus: (millicpus > 0).then_some(millicpus),
        }
    }
}

/// The `avg10` of the `some` line of a pressure file (`some avg10=1.23 avg60=0.40 ...`)
/// in hundredths of a percent (123); digits past the second decimal are dropped. `None`
/// if there is no such line or the value is not a plain decimal.
fn some_avg10_centi(text: &str) -> Option<u32> {
    let line = text.lines().find(|line| line.starts_with("some "))?;
    let value = line
        .split_whitespace()
        .find_map(|field| field.strip_prefix("avg10="))?;
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    let digits = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if whole.is_empty() || !digits(whole) || !digits(fraction) {
        return None;
    }
    let fraction = format!("{fraction:0<2}");
    let centi: u32 = fraction[..2].parse().ok()?;
    whole
        .parse::<u32>()
        .ok()?
        .checked_mul(100)?
        .checked_add(centi)
}

/// The usage a result carries for a lease that ran `wall`: the cgroup's counters beside
/// the four numbers every runtime reports (user and system CPU, peak memory, wall time),
/// which come from the same files, the cgroup basis, and what the lease started under.
pub(crate) fn resource_usage(cgroup: CgroupUsage, wall: Duration, start: Start) -> ResourceUsage {
    ResourceUsage {
        cpu_user_micros: cgroup.cpu_user_micros.unwrap_or(0),
        cpu_system_micros: cgroup.cpu_system_micros.unwrap_or(0),
        peak_memory_bytes: cgroup.memory_peak_bytes.unwrap_or(0),
        wall_micros: u64::try_from(wall.as_micros()).unwrap_or(u64::MAX),
        cgroup: Some(cgroup),
        footprint: None,
        basis: UsageBasis::Cgroup.into(),
        node_cpu_pressure_centi: start.cpu_pressure_centi,
        booked_millicpus: start.booked_millicpus,
    }
}

#[cfg(test)]
#[path = "cgroup_usage_tests.rs"]
mod tests;
