//! What a farm is made of and what it reports: the binary it runs, the roles, the
//! topologies of the rehearsal and of the beta farm, the limits of a hosted runner, and
//! the records of a start or a failure to start. Re-exported by [`crate::farm`], where the
//! farm itself is.

use std::path::{Path, PathBuf};

/// The binaries a farm runs, by path (a library cannot use `CARGO_BIN_EXE_*`).
#[derive(Clone, Debug)]
pub struct Bins {
    /// `kbf`.
    pub kbf: PathBuf,
}

/// A role of the farm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Role {
    Frontend,
    /// A storage node (`kbf --mode=storage`), numbered from 1.
    Node(usize),
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Role::Frontend => f.write_str("frontend"),
            Role::Node(n) => write!(f, "node-{n}"),
        }
    }
}

/// The ceilings that keep the farm small enough for a hosted runner (about 4 vCPU and
/// 16 GB), as extra flags per kind of process, given to the unit's own flags with
/// [`crate::units::Unit::set`] (so they replace the unit's value and are added when there is none).
#[derive(Clone, Debug, Default)]
pub struct Limits {
    pub frontend: Vec<String>,
    pub node: Vec<String>,
}

impl Limits {
    /// What a farm of a test's disk must not leave to the disk: the free bytes a storage node's
    /// cold store keeps, in place of 5% of the filesystem. A runner whose disk has filled with
    /// build output holds less free than that, and its node then refuses every flush, hangs in
    /// its stop and is killed.
    pub const COLD_STORE_FLOOR: &'static str = "--cold-store-floor-bytes=0";

    /// No limits: the unit's flags and the binaries' defaults.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// The limits of a hosted runner: a small hot tier and a small write buffer on each
    /// storage node (the stores and the write tier are a storage node's: a frontend refuses
    /// these flags).
    #[must_use]
    pub fn small() -> Self {
        Self {
            frontend: Vec::new(),
            node: vec![
                Self::COLD_STORE_FLOOR.to_owned(),
                format!("--hot-store-max-bytes={}", 256 << 20),
                format!("--write-bytes-in-flight={}", 128 << 20),
            ],
        }
    }
}

/// What a farm is made of.
#[derive(Clone, Debug)]
pub struct Topology {
    /// How many storage nodes there are (the farm's replication needs two).
    pub storage_nodes: usize,
    pub limits: Limits,
    /// The `kbf` to run for a role in place of [`Bins::kbf`]: an older release's, for a farm
    /// of two versions. A role not listed runs [`Bins::kbf`].
    pub binaries: Vec<(Role, PathBuf)>,
}

impl Topology {
    /// The rehearsal's: `storage_nodes` storage nodes, and a cold store that keeps no free
    /// space of the disk it is on ([`Limits::COLD_STORE_FLOOR`]).
    #[must_use]
    pub fn rehearsal(storage_nodes: usize) -> Self {
        Self {
            storage_nodes,
            limits: Limits {
                node: vec![Limits::COLD_STORE_FLOOR.to_owned()],
                ..Limits::none()
            },
            binaries: Vec::new(),
        }
    }

    /// The beta farm's: three storage nodes under a hosted runner's limits.
    #[must_use]
    pub fn beta() -> Self {
        Self {
            storage_nodes: 3,
            limits: Limits::small(),
            binaries: Vec::new(),
        }
    }

    /// The `kbf` that runs `role`: its entry of [`Topology::binaries`], else `default`
    /// (the farm's [`Bins::kbf`]).
    #[must_use]
    pub fn kbf_of<'a>(&'a self, role: Role, default: &'a Path) -> &'a Path {
        let named = self.binaries.iter().find(|(r, _)| *r == role);
        named.map_or(default, |(_, kbf)| kbf.as_path())
    }
}

/// Why a farm did not start or become ready: what was waited for, and every process's
/// state and output.
#[derive(Debug, thiserror::Error)]
#[error("{what}\n{report}")]
pub struct FarmError {
    pub what: String,
    pub report: String,
}

/// One start of one role: the stand-in for a boot counter.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProcRecord {
    pub role: String,
    pub pid: u32,
    /// 0 for the first start of the role, 1 for the first restart.
    pub generation: usize,
    /// Seconds since the Unix epoch.
    pub started: u64,
    /// How it ended (`exit 0`, `signal 9`, ...); `None` while it runs.
    pub ended: Option<String>,
}

/// The node id of storage node `n` (from 1).
#[must_use]
pub fn node_id(n: usize) -> String {
    format!("node-{n}")
}
