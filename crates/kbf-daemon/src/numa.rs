//! The host's NUMA layout, read once when the daemon starts and logged in one INFO line
//! (`numa layout:`) so a lease's per-node memory (`numa_bytes` on the `lease phases:`
//! line) can be read against the nodes and the CPUs each one has.
//!
//! Read from `/sys/devices/system/node/node<N>/cpulist`. A host that exposes no nodes
//! (a kernel without NUMA, a sandbox without sysfs) has an empty layout, which is not an
//! error. Linux only: the daemon's macOS build has no such files.

use std::fmt;
use std::path::Path;

/// Where Linux lists the NUMA nodes.
pub const NODE_ROOT: &str = "/sys/devices/system/node";

/// One NUMA node and the CPUs it holds.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Node {
    /// The node's number (`node<N>`).
    pub id: u32,
    /// Its `cpulist` as the kernel writes it (`0-15,32-47`).
    pub cpulist: String,
    /// How many CPUs the list names.
    pub cpus: u32,
}

/// The nodes of a host, by number.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Layout {
    /// The nodes, in order of number.
    pub nodes: Vec<Node>,
}

impl Layout {
    /// The layout under `root` (`/sys/devices/system/node`, [`NODE_ROOT`]): every
    /// `node<N>` directory with a `cpulist` that reads and parses. A memory-only node
    /// has an empty list and counts with 0 CPUs.
    #[must_use]
    pub fn read(root: &Path) -> Self {
        let Ok(entries) = std::fs::read_dir(root) else {
            return Self::default();
        };
        let mut nodes: Vec<Node> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name();
                let id: u32 = name.to_str()?.strip_prefix("node")?.parse().ok()?;
                let text = std::fs::read_to_string(entry.path().join("cpulist")).ok()?;
                let cpulist = text.trim().to_owned();
                let cpus = cpu_count(&cpulist)?;
                Some(Node { id, cpulist, cpus })
            })
            .collect();
        nodes.sort_by_key(|node| node.id);
        Self { nodes }
    }

    /// The CPUs of every node together.
    #[must_use]
    pub fn cpus(&self) -> u32 {
        self.nodes.iter().map(|n| n.cpus).sum()
    }
}

/// `nodes=2 cpus=64 node0=0-15,32-47 node1=16-31,48-63`: a node with no CPUs is
/// `node<N>=-`.
impl fmt::Display for Layout {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "nodes={} cpus={}", self.nodes.len(), self.cpus())?;
        for node in &self.nodes {
            let list = if node.cpulist.is_empty() {
                "-"
            } else {
                &node.cpulist
            };
            write!(f, " node{}={list}", node.id)?;
        }
        Ok(())
    }
}

/// How many CPUs a kernel CPU list (`0-3,8,10-11`) names; `None` if it is not one.
fn cpu_count(list: &str) -> Option<u32> {
    if list.is_empty() {
        return Some(0);
    }
    let mut count = 0u32;
    for part in list.split(',') {
        let (first, last) = match part.split_once('-') {
            Some((a, b)) => (a.parse::<u32>().ok()?, b.parse::<u32>().ok()?),
            None => {
                let one = part.parse::<u32>().ok()?;
                (one, one)
            }
        };
        count = count.checked_add(last.checked_sub(first)?.checked_add(1)?)?;
    }
    Some(count)
}

/// Logs the layout of this host's NUMA nodes in one INFO line (`nodes=0 cpus=0` on a
/// host that reports none).
pub fn log_layout() {
    let layout = Layout::read(Path::new(NODE_ROOT));
    tracing::info!("numa layout: {layout}");
}

#[cfg(test)]
#[path = "numa_tests.rs"]
mod tests;
