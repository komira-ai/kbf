//! `--lease-placement=numa-node`: each lease runs on the CPUs of one NUMA node.
//!
//! The default, `none`, is what the driver always did: a lease may run on any CPU the
//! daemon's cgroup may use. With `numa-node` the driver reads the node topology once at
//! start ([`start`]) and gives each lease a node ([`Placer::place`]): the lease cgroup
//! gets `cpuset.cpus` (that node's CPUs) and `cpuset.mems` (that node), and its
//! container's cgroup, a child, inherits both. The node is the one with the least load
//! when the lease is placed, a load being the CPU the running leases booked (a lease that
//! booked none counts as one CPU), the lowest node id on a tie. The load is given back
//! when the [`Pin`] is dropped, with the lease.
//!
//! A lease is not pinned, and books no node, when it is not a share of one node: its memory
//! limit is the whole host's (`whole_host`, the memory the node reports to the scheduler,
//! which runs such a lease alone) or more than the largest node holds ([`NodeMemory`]). It
//! gets every CPU and memory node the daemon may use and logs `numa_node=all`.
//!
//! `--reserve-cpus` is a bandwidth limit on `actions/` (`cpu.max`), not a set of cores, so
//! the pinned CPUs are the node's CPUs (within what the daemon's cgroup may use) and the
//! reserve is still taken off the leases' CPU time as before.
//!
//! A host with fewer than two nodes that hold CPUs (one socket, no `/sys/devices/system/node`,
//! a cpuset that confines the daemon to one node) is run as `none`, with one log line.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::Path;
use std::str::FromStr;
use std::sync::{Arc, Mutex, PoisonError};

use crate::delegate::{self, DelegateError};
use crate::input_phases::Spot;

/// How leases are placed on the node's CPUs (`--lease-placement`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LeasePlacement {
    /// Any CPU the daemon may use.
    #[default]
    None,
    /// One NUMA node's CPUs and memory per lease.
    NumaNode,
}

impl FromStr for LeasePlacement {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, String> {
        match text {
            "none" => Ok(Self::None),
            "numa-node" => Ok(Self::NumaNode),
            other => Err(format!(
                "{other:?} is not a lease placement: none or numa-node"
            )),
        }
    }
}

/// A set of CPU numbers, as the kernel writes one (`0-21,44-65`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CpuSet(BTreeSet<u32>);

impl CpuSet {
    #[must_use]
    pub fn len(&self) -> usize {
        self.0.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// The CPUs in both.
    #[must_use]
    pub fn intersection(&self, other: &Self) -> Self {
        Self(self.0.intersection(&other.0).copied().collect())
    }
}

impl FromStr for CpuSet {
    type Err = String;

    /// A kernel CPU list: numbers and `first-last` ranges separated by commas. Empty
    /// (a node with only memory) is the empty set.
    fn from_str(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let mut cpus = BTreeSet::new();
        if text.is_empty() {
            return Ok(Self(cpus));
        }
        let number = |part: &str| {
            part.parse::<u32>()
                .map_err(|e| format!("{text:?}: {part:?}: {e}"))
        };
        for part in text.split(',') {
            let (first, last) = match part.split_once('-') {
                Some((a, b)) => (number(a)?, number(b)?),
                None => (number(part)?, number(part)?),
            };
            if last < first {
                return Err(format!("{text:?}: {part:?} runs backwards"));
            }
            cpus.extend(first..=last);
        }
        Ok(Self(cpus))
    }
}

impl fmt::Display for CpuSet {
    /// The kernel's form: ranges of two or more as `first-last`, comma-separated.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut runs: Vec<(u32, u32)> = Vec::new();
        for &cpu in &self.0 {
            match runs.last_mut() {
                Some((_, last)) if *last + 1 == cpu => *last = cpu,
                _ => runs.push((cpu, cpu)),
            }
        }
        for (i, (first, last)) in runs.iter().enumerate() {
            if i > 0 {
                f.write_str(",")?;
            }
            if first == last {
                write!(f, "{first}")?;
            } else {
                write!(f, "{first}-{last}")?;
            }
        }
        Ok(())
    }
}

/// The NUMA nodes under `dir` (`node<N>/cpulist`), by node id.
///
/// # Errors
/// `dir` cannot be listed, or a `cpulist` cannot be read or parsed.
pub fn read_nodes(dir: &Path) -> Result<Vec<(u32, CpuSet)>, String> {
    let mut nodes = Vec::new();
    // An entry that cannot be listed is skipped: it names no node.
    let entries = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|n| n.strip_prefix("node"))
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let list = entry.path().join("cpulist");
        let text =
            std::fs::read_to_string(&list).map_err(|e| format!("{}: {e}", list.display()))?;
        let cpus = text
            .parse()
            .map_err(|e| format!("{}: {e}", list.display()))?;
        nodes.push((id, cpus));
    }
    nodes.sort_by_key(|(id, _)| *id);
    Ok(nodes)
}

/// The nodes that hold CPUs the daemon may use (`allowed`, when it is limited), each
/// with those CPUs.
fn usable(nodes: &[(u32, CpuSet)], allowed: Option<&CpuSet>) -> Vec<(u32, CpuSet)> {
    nodes
        .iter()
        .map(|(id, cpus)| {
            let cpus = allowed.map_or_else(|| cpus.clone(), |a| cpus.intersection(a));
            (*id, cpus)
        })
        .filter(|(_, cpus)| !cpus.is_empty())
        .collect()
}

/// Reads the topology under `nodes_dir` and, on a host with two or more nodes that hold
/// CPUs, enables the `cpuset` controller for the leases under `actions` and returns the
/// [`Placer`]. `None` (leases not pinned, one line logged saying why) on a host with
/// fewer, or whose node directory cannot be read. `whole_host` is the memory the node
/// reports to the scheduler in bytes, if known.
///
/// # Errors
/// The host has two nodes but `actions` cannot have `cpuset` enabled
/// ([`delegate::enable_cpuset`]); the start fails rather than run unpinned.
pub fn start(
    nodes_dir: &Path,
    cgroup_root: &Path,
    actions: &str,
    whole_host: Option<u64>,
) -> Result<Option<Arc<Placer>>, DelegateError> {
    let nodes = match read_nodes(nodes_dir) {
        Ok(nodes) => nodes,
        Err(why) => {
            tracing::info!("lease placement numa-node: no topology ({why}); not pinned");
            return Ok(None);
        }
    };
    let found = usable(&nodes, None).len();
    if found < 2 {
        tracing::info!("lease placement numa-node: {found} node(s) with CPUs; not pinned");
        return Ok(None);
    }
    let allowed = delegate::enable_cpuset(cgroup_root, actions)?;
    let usable = usable(&nodes, allowed.as_ref());
    let reached = usable.len();
    if reached < 2 {
        tracing::info!("lease placement numa-node: cpuset reaches {reached} node(s); not pinned");
        return Ok(None);
    }
    let shown: Vec<String> = usable
        .iter()
        .map(|(id, cpus)| format!("node{id}={cpus}"))
        .collect();
    tracing::info!("lease placement numa-node: {}", shown.join(" "));
    // The largest node is known only if every node the leases may use has a figure: with
    // one missing, a lease that fits it would be taken for one that spans nodes.
    let per_node = read_node_memory(nodes_dir);
    let sizes: Option<Vec<u64>> = usable
        .iter()
        .map(|(id, _)| per_node.get(id).copied())
        .collect();
    let memory = NodeMemory {
        whole_host,
        largest_node: sizes.and_then(|sizes| sizes.into_iter().max()),
    };
    Ok(Placer::with_memory(usable, memory))
}

/// The memory of the node's NUMA nodes, read from `node<N>/meminfo` (`MemTotal`), in
/// bytes by node id. A node whose file is missing or has no `MemTotal` line is left out.
#[must_use]
pub fn read_node_memory(dir: &Path) -> BTreeMap<u32, u64> {
    let mut memory = BTreeMap::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return memory;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|n| n.strip_prefix("node"))
            .and_then(|n| n.parse::<u32>().ok())
        else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(entry.path().join("meminfo")) else {
            continue;
        };
        if let Some(bytes) = text.lines().find_map(mem_total) {
            memory.insert(id, bytes);
        }
    }
    memory
}

/// The bytes of a `Node <N> MemTotal: <kB> kB` line.
fn mem_total(line: &str) -> Option<u64> {
    let mut words = line.split_whitespace().skip(2);
    if words.next()? != "MemTotal:" {
        return None;
    }
    let kib: u64 = words.next()?.parse().ok()?;
    kib.checked_mul(1024)
}

/// What a [`Placer`] knows of the node's memory, to tell a lease that is not a share of
/// one NUMA node. A field that is `None` switches its rule off.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NodeMemory {
    /// The memory the scheduler books on this node, in bytes: a lease whose limit is at
    /// least this is the whole host.
    pub whole_host: Option<u64>,
    /// The memory of the largest single NUMA node, in bytes.
    pub largest_node: Option<u64>,
}

/// Where a lease runs.
#[derive(Debug)]
pub enum Placement {
    /// On one node, until the [`Pin`] is dropped.
    Node(Pin),
    /// On every CPU and memory node the daemon may use: no cpuset, no booked load.
    All,
}

impl Placement {
    /// What the lease's `inputs phases` line says.
    #[must_use]
    pub fn spot(&self) -> Spot {
        match self {
            Self::Node(pin) => Spot::Node(pin.node()),
            Self::All => Spot::All,
        }
    }

    /// The node the lease is pinned to, if it is.
    #[must_use]
    pub fn into_pin(self) -> Option<Pin> {
        match self {
            Self::Node(pin) => Some(pin),
            Self::All => None,
        }
    }
}

/// Chooses a NUMA node for each lease and counts what is on each.
#[derive(Debug)]
pub struct Placer {
    /// The nodes, by id; each holds CPUs.
    nodes: Vec<(u32, CpuSet)>,
    /// The booked CPU (millicpus) of the leases on each node, in `nodes` order.
    load: Mutex<Vec<u64>>,
    memory: NodeMemory,
}

impl Placer {
    /// A placer over `nodes` (by id, each with CPUs); `None` without any.
    #[must_use]
    pub fn new(nodes: Vec<(u32, CpuSet)>) -> Option<Arc<Self>> {
        Self::with_memory(nodes, NodeMemory::default())
    }

    /// [`Placer::new`], knowing `memory`.
    #[must_use]
    pub fn with_memory(nodes: Vec<(u32, CpuSet)>, memory: NodeMemory) -> Option<Arc<Self>> {
        if nodes.is_empty() {
            return None;
        }
        let load = Mutex::new(vec![0; nodes.len()]);
        Some(Arc::new(Self {
            nodes,
            load,
            memory,
        }))
    }

    /// Places a lease that booked `cpu_millis` with a memory limit of `memory_bytes`.
    #[must_use]
    pub fn place_lease(self: &Arc<Self>, cpu_millis: u64, memory_bytes: u64) -> Placement {
        if self.spans_nodes(memory_bytes) {
            Placement::All
        } else {
            Placement::Node(self.place(cpu_millis))
        }
    }

    /// Whether a lease with this memory limit is not a share of one node: it is the whole
    /// host's (the scheduler runs it alone), or more than the largest node holds. No limit
    /// (0) is neither, as in the scheduler.
    fn spans_nodes(&self, memory_bytes: u64) -> bool {
        let at_least =
            |size: Option<u64>| memory_bytes > 0 && size.is_some_and(|s| memory_bytes >= s);
        at_least(self.memory.whole_host)
            || self
                .memory
                .largest_node
                .is_some_and(|node| memory_bytes > node)
    }

    /// Places a lease that booked `cpu_millis` (0 counts as one CPU) on the node with the
    /// least load, the lowest id on a tie, until the [`Pin`] is dropped.
    #[must_use]
    pub(crate) fn place(self: &Arc<Self>, cpu_millis: u64) -> Pin {
        let weight = if cpu_millis == 0 { 1000 } else { cpu_millis };
        let mut load = self.load.lock().unwrap_or_else(PoisonError::into_inner);
        // `min_by_key` keeps the first of equals: the lowest id. There is always a node.
        let index = load
            .iter()
            .enumerate()
            .min_by_key(|(_, l)| **l)
            .map_or(0, |(i, _)| i);
        load[index] = load[index].saturating_add(weight);
        Pin {
            placer: Arc::clone(self),
            index,
            weight,
        }
    }

    /// The load on each node, in node order.
    #[cfg(test)]
    fn loads(&self) -> Vec<u64> {
        self.load
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

/// One lease's node. Dropping it takes the lease's load off the node.
#[derive(Debug)]
pub struct Pin {
    placer: Arc<Placer>,
    index: usize,
    weight: u64,
}

impl Pin {
    /// The NUMA node's id.
    #[must_use]
    pub fn node(&self) -> u32 {
        self.placer.nodes[self.index].0
    }

    /// The CPUs of the node: the lease's `cpuset.cpus`.
    #[must_use]
    pub fn cpus(&self) -> &CpuSet {
        &self.placer.nodes[self.index].1
    }
}

impl Drop for Pin {
    fn drop(&mut self) {
        let mut load = self
            .placer
            .load
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        load[self.index] = load[self.index].saturating_sub(self.weight);
    }
}

#[cfg(test)]
#[path = "numa_tests.rs"]
mod tests;
