//! Shared read-only input trees (`docs/design/fast-sandbox.md#4-shared-trees`).
//!
//! An action's input root is mostly one toolchain, thousands of files that are the same
//! for every action that uses it. Linking each file into each action's input root costs
//! the same time per file per action. Here a large Directory node of the input root is
//! built once per node, in the [`TreeStore`], and each action that has it gets the
//! tree's directory as a [`SharedMount`]: the driver mounts it read-only at the node's
//! path, and the action's own input root holds an empty directory there. What is left
//! (the skeleton above the trees and the small files beside them) is written through
//! the node's input cache as before.
//!
//! What a tree must not be: a place the action writes. A node qualifies only if it is
//! large, fits the store's budget, holds no output path and not the working directory,
//! sits under no output directory, and is the highest such node on its path (see
//! `classify`). A write into a mounted tree fails (`EROFS` from the read-only mount,
//! and `EACCES` for a file: the tree's files are the cache's, owned by the daemon's
//! user and not writable). A `rename` or `link` across the mount's boundary fails
//! `EXDEV`: a tool that hard-links an input out of a tree without a copy fallback fails.
//! An action that writes into a toolchain directory now fails where it used to succeed;
//! a node without `--tree-dir` keeps the old behaviour.

mod build;
mod classify;
mod store;

use std::collections::HashSet;
use std::path::PathBuf;

use kbf_proto::reapi::Digest;

use crate::cache::{Cache, InputStats};
use crate::cas::{Cas, label};
use crate::tree::{TreeError, make_directories};
use classify::{Limits, Reserved, Summaries, load, select, summarize};
pub use store::{TreeLease, TreeStats, TreeStore};

/// The default for [`TreeConfig::min_files`].
pub const MIN_FILES: u64 = 256;
/// The default for [`TreeConfig::max_mounts`].
pub const MAX_MOUNTS: usize = 8;

/// How the tree store is opened and used (`--tree-*`).
#[derive(Clone, Debug)]
pub struct TreeConfig {
    /// `--tree-dir`: the store's directory, made if missing. It should be on the
    /// input cache's filesystem (otherwise the trees' files are copies, not links).
    pub dir: PathBuf,
    /// `--tree-max-bytes`: the most file bytes of trees kept; unpinned trees are
    /// evicted, least recently used first, past it. A node larger than this is never a
    /// tree.
    pub max_bytes: u64,
    /// `--tree-min-files`: the fewest files and symlinks a node holds to be a tree.
    pub min_files: u64,
    /// `--tree-max-mounts`: the most trees one action mounts.
    pub max_mounts: usize,
}

impl TreeConfig {
    /// A store in `dir` of at most `max_bytes`, with [`MIN_FILES`] and [`MAX_MOUNTS`].
    #[must_use]
    pub fn new(dir: PathBuf, max_bytes: u64) -> Self {
        Self {
            dir,
            max_bytes,
            min_files: MIN_FILES,
            max_mounts: MAX_MOUNTS,
        }
    }
}

/// A host directory a driver mounts read-only into the action's input root. The
/// driver's other half of this interface: the OCI runtime backend and the podman
/// driver both take these from [`Inputs::mounts`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SharedMount {
    /// Where it appears, from the input root: `/`-separated, no leading slash, never
    /// empty, never containing `:` or `,`. The input root has an empty directory there.
    pub path: String,
    /// The tree on the host. Read-only to the action; the driver must not mount it
    /// writable. Must outlive the action: held by [`Inputs::trees`].
    pub source: PathBuf,
}

/// An action's input root written with shared trees.
#[derive(Debug)]
pub struct Inputs {
    /// What the files written through the cache cost (those outside the trees).
    pub stats: InputStats,
    /// The trees to mount, by path.
    pub mounts: Vec<SharedMount>,
    /// The trees' pins: hold them until the action's container is gone.
    pub trees: Vec<TreeLease>,
    /// Files the mounted trees hold, which this action did not write.
    pub tree_files: u64,
    /// Trees this call built (the rest were there, or another call built them).
    pub trees_built: u64,
}

/// The node's tree store with the classifier's settings.
#[derive(Debug)]
pub struct Trees {
    store: TreeStore,
    summaries: Summaries,
    limits: Limits,
}

impl Trees {
    /// Opens the store (blocking: see [`TreeStore::open`]).
    ///
    /// # Errors
    /// The store's directory cannot be used.
    pub fn open(config: TreeConfig) -> std::io::Result<Self> {
        Ok(Self {
            store: TreeStore::open(config.dir, config.max_bytes)?,
            summaries: Summaries::default(),
            limits: Limits {
                min_files: config.min_files,
                max_bytes: config.max_bytes,
                max_mounts: config.max_mounts,
            },
        })
    }

    /// The store.
    #[must_use]
    pub fn store(&self) -> &TreeStore {
        &self.store
    }

    /// Writes the tree under `root` into the empty directory `dir`: the large nodes as
    /// [`SharedMount`]s (each an empty directory in `dir`), everything else through
    /// `cache` as `materialize_cached` does. `outputs` are the output paths as seen
    /// from the input root, and `working_directory` is the Command's.
    ///
    /// # Errors
    /// As `materialize_cached`, or a tree cannot be built.
    pub async fn materialize(
        &self,
        cas: &impl Cas,
        cache: &Cache,
        root: &Digest,
        dir: &std::path::Path,
        outputs: &[String],
        working_directory: &str,
    ) -> Result<Inputs, TreeError> {
        let reserved = Reserved {
            outputs,
            working_directory,
        };
        let limits = self.limits;
        // A node already known to be a tree is not read: that is the walk a warm node
        // does not repeat.
        let skip = |path: &str, digest: &Digest| {
            self.summaries
                .get(&label(digest))
                .is_some_and(|summary| classify::qualifies(path, summary, &reserved, limits))
        };
        let loaded = load(
            cas,
            root,
            &Default::default(),
            &skip,
            classify::MAX_DIRECTORIES,
        )
        .await?;
        let summaries = summarize(&loaded, &|name| self.summaries.get(name))?;
        self.summaries.extend(&summaries);
        let chosen = select(
            &loaded,
            &summaries,
            root,
            &reserved,
            limits,
            classify::MAX_DIRECTORIES,
        )?;

        let mut leases = Vec::new();
        let mut mounts = Vec::new();
        let (mut tree_files, mut built) = (0, 0);
        for candidate in &chosen {
            let (lease, stats) = self
                .store
                .acquire(cas, cache, &candidate.name, &candidate.digest, &loaded)
                .await?;
            mounts.push(SharedMount {
                path: candidate.path.clone(),
                source: lease.path().to_owned(),
            });
            tree_files += candidate.summary.files;
            built += u64::from(stats.is_some());
            leases.push(lease);
        }
        let mount_dirs: HashSet<PathBuf> = mounts.iter().map(|m| dir.join(&m.path)).collect();
        let files = make_directories(cas, root, dir, &loaded, &mount_dirs).await?;
        let private: Vec<PathBuf> = outputs.iter().map(|p| dir.join(p)).collect();
        let stats = crate::cache::lease::write_files(cas, cache, files, &private).await?;
        Ok(Inputs {
            stats,
            mounts,
            trees: leases,
            tree_files,
            trees_built: built,
        })
    }
}

#[cfg(test)]
#[path = "classify_tests.rs"]
mod classify_tests;
#[cfg(test)]
#[path = "verify_tests.rs"]
mod verify_tests;
