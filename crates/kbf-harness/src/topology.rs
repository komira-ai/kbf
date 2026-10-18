//! What a farm is made of and what it reports: the binaries it runs, the driver, the roles,
//! the topologies of the rehearsal and of the beta farm, the limits of a hosted runner, and
//! the records of a start or a failure to start. Re-exported by [`crate::farm`], where the
//! farm itself is.

use std::path::PathBuf;

use serde::Serialize;

use crate::interrupt::Interrupt;

/// The binaries a farm runs, by path (a library cannot use `CARGO_BIN_EXE_*`).
#[derive(Clone, Debug)]
pub struct Bins {
    /// `kbf`.
    pub kbf: PathBuf,
    /// The stand-in `podman`, for [`Driver::StandInContainer`].
    pub fake_podman: Option<PathBuf>,
}

/// What runs the actions on the daemons.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub enum Driver {
    /// `--driver=native`: each action is a plain process in its lease directory. No
    /// container path is exercised; any argv a build sends runs as sent.
    Native,
    /// `--driver=container` as rendered, over the stand-in `podman` (which runs one
    /// planted argv whatever the lease asks for), a planted cgroup tree and id files.
    StandInContainer,
}

/// A role of the farm.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize)]
pub enum Role {
    Frontend,
    /// A storage node (with its daemon, in a [`NodeMode::StorageAndDaemon`] farm),
    /// numbered from 1.
    Node(usize),
    /// A plain daemon, numbered from 1.
    Daemon(usize),
    Envoy,
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Role::Frontend => f.write_str("frontend"),
            Role::Node(n) => write!(f, "node-{n}"),
            Role::Daemon(n) => f.write_str(&daemon_id(*n)),
            Role::Envoy => f.write_str("envoy"),
        }
    }
}

/// The node id of plain daemon `n` (from 1).
#[must_use]
pub fn daemon_id(n: usize) -> String {
    format!("daemon-{n}")
}

/// What a storage node runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NodeMode {
    /// `kbf storage`: storage alone. Its actions run on the plain daemons.
    Storage,
    /// `kbf storage-and-daemon`: storage and a daemon in one process. The daemon half
    /// runs the container driver only.
    StorageAndDaemon,
}

/// The ceilings that keep the farm small enough for a hosted runner (about 4 vCPU and
/// 16 GB), as extra flags per kind of process, given to the unit's own flags with
/// [`Rendered::set`] (so they replace a rendered value and are added when there is none).
#[derive(Clone, Debug, Default)]
pub struct Limits {
    pub frontend: Vec<String>,
    pub node: Vec<String>,
    pub daemon: Vec<String>,
}

impl Limits {
    /// No limits: the unit's flags and the binaries' defaults.
    #[must_use]
    pub fn none() -> Self {
        Self::default()
    }

    /// The limits of a hosted runner: a small hot tier and a small write buffer on each
    /// storage node (the stores and the write tier are a storage node's: a frontend refuses
    /// these flags), and one CPU kept by each daemon.
    #[must_use]
    pub fn small() -> Self {
        Self {
            frontend: Vec::new(),
            node: vec![
                format!("--hot-store-max-bytes={}", 256 << 20),
                format!("--write-bytes-in-flight={}", 128 << 20),
            ],
            // A daemon keeps 8 CPUs for the farm's processes unless told otherwise, and
            // leases the rest: none on a runner of 4.
            daemon: vec!["--reserve-cpus=1".to_owned()],
        }
    }
}

/// What a farm is made of.
#[derive(Clone, Debug)]
pub struct Topology {
    /// How many storage nodes there are (the farm's replication needs two).
    pub storage_nodes: usize,
    pub node_mode: NodeMode,
    /// How many plain daemons there are.
    pub daemons: usize,
    pub driver: Driver,
    pub limits: Limits,
    /// An Envoy to put in front of the frontend (the `box` topology, the frontend as
    /// instance `a`): the binary's path. `None` runs the frontend alone.
    pub envoy: Option<PathBuf>,
    /// Flags to set on one role after everything else (a mutant's, or a test's).
    pub overrides: Vec<(Role, Vec<String>)>,
    /// Whether the run was asked to stop: every wait of the farm looks at it.
    pub interrupt: Interrupt,
}

impl Topology {
    /// The rehearsal's: `storage_nodes` storage-and-daemon nodes and a daemon, the
    /// stand-in container driver, no limits, no Envoy.
    #[must_use]
    pub fn rehearsal(storage_nodes: usize) -> Self {
        Self {
            storage_nodes,
            node_mode: NodeMode::StorageAndDaemon,
            daemons: 1,
            driver: Driver::StandInContainer,
            limits: Limits::none(),
            envoy: None,
            overrides: Vec::new(),
            interrupt: Interrupt::new(),
        }
    }

    /// The beta farm's: three storage nodes, two plain daemons running actions as plain
    /// processes (the native driver, which only a plain daemon takes), a hosted runner's
    /// limits, and the Envoy given.
    #[must_use]
    pub fn beta(envoy: Option<PathBuf>) -> Self {
        Self {
            storage_nodes: 3,
            node_mode: NodeMode::Storage,
            daemons: 2,
            driver: Driver::Native,
            limits: Limits::small(),
            envoy,
            overrides: Vec::new(),
            interrupt: Interrupt::new(),
        }
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

/// One start of one role, for the result document: the stand-in for a boot counter.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
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

/// The node id of storage-and-daemon node `n` (from 1).
#[must_use]
pub fn node_id(n: usize) -> String {
    format!("node-{n}")
}
