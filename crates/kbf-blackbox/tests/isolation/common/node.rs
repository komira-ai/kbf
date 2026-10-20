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
}

/// The Linux node of the farm: one storage-and-daemon node over the stand-in `crun`.
pub struct LinuxNode {
    /// The farm's directory; `node-1` is below it.
    pub farm: PathBuf,
    /// Pids of the farm's own roles (the kbf processes), which are not residue.
    pub roles: Vec<u32>,
}

impl LinuxNode {
    pub fn new(farm: &Path, roles: Vec<u32>) -> Self {
        Self {
            farm: farm.to_owned(),
            roles,
        }
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
        probe::processes_of(&self.farm, &self.roles)
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
}
