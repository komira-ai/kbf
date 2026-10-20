//! What a test may ask of a node, whatever the platform: [`NodeView`]. THE MAC SEAM.
//!
//! The two suites call only this trait and the probes of `probe.rs` built on it, so the Mac
//! legs (the per-slot users, the simulators, `whole-machine` lease directories, the launchd
//! jobs and the UI session; daemon.md "macOS") add a `MacNode` that implements it and pick it
//! where the tests pick [`LinuxNode`], and the tests run unchanged. What a Mac leg must
//! provide, in the trait's terms:
//! - [`NodeView::lease_dirs`]: the entries of the scratch directory's lease area that are
//!   not kept on purpose (a whole-machine lease directory, a slot's work directory);
//! - [`NodeView::slot_users`]: per-slot users that still exist or are logged in;
//! - [`NodeView::session_leftovers`]: simulators, UI sessions, launchd jobs of a slot;
//! - [`NodeView::home_dirs`]: slot home directories that were not reset;
//! - [`NodeView::processes`]: processes of the slot users (`ps -u`), not `/proc`;
//! - [`NodeView::isolation`]: [`Isolation::PerUid`], where the cross-action probes expect
//!   live reads of another action's files to be DENIED by uid and mode (the Linux stand-in
//!   gives [`Isolation::Nothing`], and a real Linux container [`Isolation::MountNs`]).
//!
//! A Mac method that is not written yet must `unimplemented!("mac leg: ...")` so a
//! missing leg is loud, never a skip.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::probe;

/// What stands between two actions of one node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "PerUid and MountNs are the Mac leg's and the real-crun tier's"
)]
pub enum Isolation {
    /// Nothing: the stand-in `crun` (a plain process of the test's user, no namespace). A
    /// probe that asks "can B read A's file by path" must find it CAN.
    Nothing,
    /// A user of its own per action (the Mac legs): reads are denied by uid and mode.
    PerUid,
    /// A mount namespace per action (real `crun`): A's paths do not exist for B.
    MountNs,
}

/// A process the node is responsible for.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Proc {
    pub pid: u32,
    /// The `/proc/N/stat` state letter.
    pub state: char,
    /// The command line, its words joined by spaces.
    pub cmd: String,
}

/// A node as the suites see it.
pub trait NodeView {
    /// The node's scratch directory (`--data-dir`).
    fn scratch(&self) -> PathBuf;
    /// What stands between two actions here.
    fn isolation(&self) -> Isolation;
    /// The entries of the lease area that are not kept on purpose; an error when the
    /// area itself is missing.
    fn lease_dirs(&self) -> Result<BTreeSet<String>, String>;
    /// The per-lease cgroups (Linux), or what the platform has in their place.
    fn lease_cgroups(&self) -> Result<BTreeSet<String>, String>;
    /// The node's processes (those of the farm and of its actions), farm roles excluded.
    fn processes(&self) -> Result<BTreeSet<Proc>, String>;
    /// Per-slot users that exist (Mac legs; none on Linux).
    fn slot_users(&self) -> Result<BTreeSet<String>, String>;
    /// Simulators, UI sessions and launchd jobs left (Mac legs; none on Linux).
    fn session_leftovers(&self) -> Result<BTreeSet<String>, String>;
    /// Slot home directories not reset (Mac legs; none on Linux).
    fn home_dirs(&self) -> Result<BTreeSet<String>, String>;
    /// Whether `rel`, a path below [`NodeView::scratch`], is kept on purpose: the design keeps
    /// the input cache and trees and the image store between leases (daemon.md "The
    /// container driver", storage-node.md). A path is kept only if it is exactly one of
    /// these or below one of them: a name that merely starts with one is not.
    fn kept(&self, rel: &Path) -> bool;
    /// The directories of the node's content stores (the server's CAS and the daemon's
    /// input cache), which hold content by digest by design.
    fn stores(&self) -> Vec<PathBuf>;
    /// The directories that are kept on purpose and yet must be EMPTY between leases: the
    /// helper containers' job directories, which are removed after each job. Anything in
    /// one is residue, whatever it weighs.
    fn empty_between_leases(&self) -> Vec<PathBuf>;
    /// The directories of [`NodeView::stores`] that hold files named by the digest of
    /// their content (the daemon's input cache and trees): a canary may sit in one only
    /// at such a name.
    fn content_addressed(&self) -> Vec<PathBuf>;
    /// The node's own directory, which holds [`NodeView::scratch`] and more (its
    /// configuration, keys, id maps, binaries): residue outside the scratch directory is
    /// looked for there.
    fn surroundings(&self) -> PathBuf;
    /// Whether `rel`, a path below [`NodeView::surroundings`], is exempt from the look at
    /// the surroundings: the scratch directory (looked at on its own), the content
    /// stores, and the cgroup tree and the stand-in's records (which change on every
    /// lease by design). Whole components only, like [`NodeView::kept`].
    fn surroundings_exempt(&self, rel: &Path) -> bool;
    /// Whether `rel`, a path below [`NodeView::surroundings`] that is not exempt, is one
    /// the node has by design: an EXACT list of its configuration, keys, id maps and
    /// binaries. A path not on it is residue even when it was there at the baseline (a
    /// leak that every lease rewrites would otherwise be part of the baseline).
    fn surroundings_expected(&self, rel: &Path) -> bool;
}

/// The Linux node of the farm: one storage-and-daemon node over the stand-in `crun`.
pub struct LinuxNode {
    /// The farm's directory; `node-1` is below it.
    pub farm: PathBuf,
    /// Pids of the farm's own roles (the kbf processes), which are not residue.
    pub roles: Vec<u32>,
    /// Every pid the farm's roles have had, live or ended: each leads a process group of
    /// its own (the harness starts it so), and a process still in that group after its
    /// role ended, or beside a live one, belongs to the farm whatever its environment and
    /// working directory say.
    pub groups: Vec<u32>,
}

impl LinuxNode {
    pub fn new(farm: &Path, roles: Vec<u32>) -> Self {
        Self {
            farm: farm.to_owned(),
            roles,
            groups: Vec::new(),
        }
    }

    /// The same node, finding orphans also by the process groups of `groups`.
    #[must_use]
    pub fn with_groups(mut self, groups: Vec<u32>) -> Self {
        self.groups = groups;
        self
    }

    /// The group and session ids that mark a process as the farm's: the farm's roles'
    /// groups, and the last action the stand-in `crun` started (`crun-state/pid` and
    /// `start-pid`: it makes the action the leader of its own session and group, and
    /// keeps the record after the lease ends).
    fn group_ids(&self) -> Vec<u32> {
        let mut ids = self.groups.clone();
        for record in ["pid", "start-pid"] {
            let path = self.node().join("bin/crun-state").join(record);
            if let Some(pid) = std::fs::read_to_string(path)
                .ok()
                .and_then(|t| t.trim().parse().ok())
            {
                ids.push(pid);
            }
        }
        ids
    }

    fn node(&self) -> PathBuf {
        self.farm.join("node-1")
    }
}

impl NodeView for LinuxNode {
    fn scratch(&self) -> PathBuf {
        self.node().join("data")
    }

    fn isolation(&self) -> Isolation {
        Isolation::Nothing
    }

    fn lease_dirs(&self) -> Result<BTreeSet<String>, String> {
        let leases = self.scratch().join("leases");
        let mut names = BTreeSet::new();
        for entry in probe::read_dir(&leases)? {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !self.kept(&Path::new("leases").join(&name)) {
                names.insert(name);
            }
        }
        Ok(names)
    }

    fn lease_cgroups(&self) -> Result<BTreeSet<String>, String> {
        let tree = probe::walk(&self.node().join("bin/cgroup"))?;
        Ok(tree
            .into_iter()
            .filter(|(rel, e)| {
                e.is_dir()
                    && rel
                        .file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with(probe::LEASE_PREFIX))
            })
            .map(|(rel, _)| rel.display().to_string())
            .collect())
    }

    fn processes(&self) -> Result<BTreeSet<Proc>, String> {
        let members = probe::cgroup_members(&self.node().join("bin/cgroup"))?;
        probe::processes_of(&self.farm, &self.roles, &self.group_ids(), &members)
    }

    fn slot_users(&self) -> Result<BTreeSet<String>, String> {
        Ok(BTreeSet::new())
    }

    fn session_leftovers(&self) -> Result<BTreeSet<String>, String> {
        Ok(BTreeSet::new())
    }

    fn home_dirs(&self) -> Result<BTreeSet<String>, String> {
        Ok(BTreeSet::new())
    }

    fn kept(&self, rel: &Path) -> bool {
        // Whole components, never a prefix: `cache-evil` is not `cache`.
        const KEPT: [&str; 7] = [
            "cache",
            "trees",
            "leases/daemon-instance",
            "leases/oci-images",
            "leases/oci-bin",
            "leases/oci-helper",
            "leases/oci-helper-state",
        ];
        KEPT.iter().any(|k| rel.starts_with(k))
    }

    fn stores(&self) -> Vec<PathBuf> {
        let node = self.node();
        vec![
            node.join("hot"),
            node.join("cold"),
            node.join("meta"),
            node.join("state"),
            self.scratch().join("cache"),
            self.scratch().join("trees"),
            self.scratch().join("leases/oci-images"),
            self.scratch().join("leases/oci-bin"),
            self.scratch().join("leases/oci-helper"),
            self.scratch().join("leases/oci-helper-state"),
            self.farm.join("frontend/state"),
        ]
    }

    fn empty_between_leases(&self) -> Vec<PathBuf> {
        vec![self.scratch().join("leases/oci-helper-state")]
    }

    fn content_addressed(&self) -> Vec<PathBuf> {
        vec![self.scratch().join("cache"), self.scratch().join("trees")]
    }

    fn surroundings(&self) -> PathBuf {
        self.node()
    }

    fn surroundings_expected(&self, rel: &Path) -> bool {
        // Whole paths, never a prefix. The rehearsal topology's node: its configuration, the
        // leaf and CA certificates and keys, the id maps, and the stand-in's binaries.
        const EXPECTED: [&str; 17] = [
            "bin",
            "bin/crun",
            "bin/kbf-act",
            "bin/newgidmap",
            "bin/newuidmap",
            "bin/self-cgroup",
            "etc",
            "etc/ca.pem",
            "etc/node.key",
            "etc/node.pem",
            "etc/storage.key",
            "etc/storage.pem",
            "ids",
            "ids/passwd",
            "ids/subgid",
            "ids/subuid",
            "kbf.toml",
        ];
        EXPECTED.iter().any(|e| rel == Path::new(e))
    }

    fn surroundings_exempt(&self, rel: &Path) -> bool {
        // `data` is the scratch directory, looked at on its own; `hot`, `cold`, `meta` and
        // `state` are the server's stores (their logs are inside them); `bin/cgroup` is
        // the planted cgroup tree (`lease_cgroups` looks at it) and `bin/crun-state` the
        // stand-in's records, rewritten on every lease. Nothing else is exempt: not
        // `bin`, `etc`, `ids`, nor the node's configuration.
        const EXEMPT: [&str; 7] = [
            "data",
            "hot",
            "cold",
            "meta",
            "state",
            "bin/cgroup",
            "bin/crun-state",
        ];
        EXEMPT.iter().any(|e| rel.starts_with(e))
    }
}
