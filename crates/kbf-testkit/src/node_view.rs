//! What a test may ask of a node, whatever the platform: [`NodeView`]. THE MAC SEAM.
//!
//! The two isolation suites of `kbf-daemon-test` call only this trait and the probes of
//! [`crate::probe`] built on it, so the Mac legs (the per-slot users, the simulators,
//! `whole-machine` lease directories, the launchd jobs and the UI session; daemon.md
//! "macOS") add a `MacNode` that implements it (in their own
//! test target of `kbf-daemon-test`, importing this module: no `#[path]`) and pick it where the
//! suites pick `LinuxNode`, and the tests run unchanged. What a Mac leg must
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

/// What stands between two actions of one node.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
