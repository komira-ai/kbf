//! The tree store: one directory per shared tree, named by its Directory digest, built
//! once per node and pinned while a lease mounts it
//! (`docs/design/fast-sandbox.md#43-eviction-and-accounting`).
//!
//! ```text
//! <dir>/<hash>-<size>      a finished tree (its name appears by one rename)
//! <dir>/.tmp-<n>           a tree being built; removed at start
//! <dir>/.dead-<n>          an evicted tree being removed; removed at start
//! ```
//!
//! **One build per digest.** The first [`TreeStore::acquire`] of a digest builds it in
//! its own task; every other acquire of that digest waits for the builder's slot to
//! close and looks again (no file lock). A builder that fails, or is dropped, closes
//! its slot, and one waiter builds in its place.
//!
//! **Pins.** A [`TreeLease`] pins its tree. Only an unpinned tree is evicted: renamed
//! to `.dead-<n>` (a new acquire builds afresh under the digest's name) and removed in
//! the background. The budget counts every tree's file bytes, linked or not.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use kbf_proto::reapi::Digest;
use tokio::sync::watch;

use super::build::{build, bytes_under, publish, remove_tree};
use crate::cache::{Cache, InputStats};
use crate::cas::Cas;
use crate::tree::{Loaded, TreeError, io as io_error};

/// What a node's tree store has done.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct TreeStats {
    /// Trees in the store, and their file bytes.
    pub trees: u64,
    pub bytes: u64,
    /// Trees a lease holds (not evictable).
    pub pinned: u64,
    /// Trees built, and acquires that found the tree built (a hit) or waited for
    /// another's build.
    pub builds: u64,
    pub hits: u64,
    pub waits: u64,
    /// Trees evicted.
    pub evictions: u64,
}

enum Tree {
    /// A task is building it; the receiver closes when that task is done.
    Building(watch::Receiver<()>),
    Ready {
        bytes: u64,
        pins: u32,
        /// The store's tick at the last acquire: the LRU order.
        used: u64,
    },
}

#[derive(Default)]
struct State {
    trees: HashMap<String, Tree>,
    /// File bytes of the `Ready` trees.
    bytes: u64,
    tick: u64,
}

#[derive(Default)]
struct Counters {
    builds: AtomicU64,
    hits: AtomicU64,
    waits: AtomicU64,
    evictions: AtomicU64,
}

pub(super) struct Inner {
    dir: PathBuf,
    max_bytes: u64,
    state: Mutex<State>,
    counters: Counters,
    next: AtomicU64,
}

/// The node's tree store.
pub struct TreeStore {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for TreeStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TreeStore")
            .field("dir", &self.inner.dir)
            .finish_non_exhaustive()
    }
}

/// A tree pinned for a lease: its path is mounted read-only. Dropping it unpins the
/// tree, and a store over budget then evicts.
pub struct TreeLease {
    inner: Arc<Inner>,
    name: String,
    path: PathBuf,
}

impl TreeLease {
    /// The tree's directory on the host.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl std::fmt::Debug for TreeLease {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TreeLease")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Drop for TreeLease {
    fn drop(&mut self) {
        let mut state = self.inner.state();
        if let Some(Tree::Ready { pins, .. }) = state.trees.get_mut(&self.name) {
            *pins -= 1;
        }
        let dead = self.inner.evict(&mut state);
        drop(state);
        self.inner.remove_later(dead);
    }
}

/// The directory name of a tree: `<hash>-<size>`, or `None` for a digest that is not
/// a SHA-256 one (its name could leave the store).
fn name_of(digest: &Digest) -> Option<String> {
    let hex = digest.hash.len() == 64
        && digest
            .hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    (hex && digest.size_bytes >= 0).then(|| format!("{}-{}", digest.hash, digest.size_bytes))
}

fn parses(name: &str) -> bool {
    name.split_once('-').is_some_and(|(hash, size)| {
        let digest = Digest {
            hash: hash.to_owned(),
            size_bytes: size.parse().unwrap_or(-1),
        };
        name_of(&digest).as_deref() == Some(name)
    })
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        // Each update leaves the map consistent.
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Renames unpinned trees, least recently used first, out of the store until the
    /// budget holds; returns their new names for [`Inner::remove_later`].
    fn evict(&self, state: &mut State) -> Vec<PathBuf> {
        let mut dead = Vec::new();
        while state.bytes > self.max_bytes {
            let victim = state
                .trees
                .iter()
                .filter_map(|(name, tree)| match tree {
                    Tree::Ready {
                        pins: 0,
                        used,
                        bytes,
                    } => Some((*used, name.clone(), *bytes)),
                    _ => None,
                })
                .min();
            let Some((_, name, bytes)) = victim else {
                break;
            };
            let to = self.dir.join(format!(
                ".dead-{}",
                self.next.fetch_add(1, Ordering::Relaxed)
            ));
            if let Err(e) = std::fs::rename(self.dir.join(&name), &to) {
                tracing::warn!("trees: evicting {name}: {e}");
                break;
            }
            state.trees.remove(&name);
            state.bytes -= bytes;
            self.counters.evictions.fetch_add(1, Ordering::Relaxed);
            dead.push(to);
        }
        dead
    }

    /// Removes the directories `dead` on the blocking pool, or here with no runtime.
    fn remove_later(&self, dead: Vec<PathBuf>) {
        if dead.is_empty() {
            return;
        }
        let remove = move || {
            for dir in &dead {
                if let Err(e) = remove_tree(dir) {
                    tracing::warn!("trees: removing {}: {e}", dir.display());
                }
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => drop(runtime.spawn_blocking(remove)),
            Err(_) => remove(),
        }
    }
}

/// Ends a build that did not finish: removes its slot and its `.tmp-*` directory.
struct Builder {
    inner: Arc<Inner>,
    name: String,
    tmp: PathBuf,
    done: bool,
    /// Closing this wakes the waiters.
    _slot: watch::Sender<()>,
}

impl Drop for Builder {
    fn drop(&mut self) {
        let mut state = self.inner.state();
        if !self.done {
            state.trees.remove(&self.name);
        }
        drop(state);
        if !self.done {
            self.inner.remove_later(vec![self.tmp.clone()]);
        }
    }
}

/// What [`TreeStore::acquire`] does next.
enum Step {
    /// The tree is built: it was pinned.
    Pinned,
    /// Another acquire is building it.
    Wait(watch::Receiver<()>),
    /// Nobody is: this one builds.
    Lead(Builder),
}

impl TreeStore {
    /// Opens the store in `dir` (made, mode 0700, if it is missing), blocking: removes
    /// the `.tmp-*` and `.dead-*` directories a previous run left, and adopts every
    /// tree already named (a tree is named by one rename after it was checked, so a
    /// name present means a whole tree). Anything else in `dir` is left alone.
    ///
    /// # Errors
    /// `dir` cannot be made, listed or written.
    pub fn open(dir: PathBuf, max_bytes: u64) -> std::io::Result<Self> {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::create_dir_all(&dir)?;
        // The container's user must not reach the store but through a mount.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        let mut state = State::default();
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let file = entry.file_name();
            let name = file.to_string_lossy();
            if name.starts_with(".tmp-") || name.starts_with(".dead-") {
                remove_tree(&entry.path())?;
            } else if parses(&name) && entry.file_type()?.is_dir() {
                let bytes = bytes_under(&entry.path())?;
                state.bytes = state.bytes.saturating_add(bytes);
                state.trees.insert(
                    name.into_owned(),
                    Tree::Ready {
                        bytes,
                        pins: 0,
                        used: 0,
                    },
                );
            } else {
                tracing::warn!(
                    "trees: {} is not a tree; left alone",
                    entry.path().display()
                );
            }
        }
        let inner = Arc::new(Inner {
            dir,
            max_bytes,
            state: Mutex::new(state),
            counters: Counters::default(),
            next: AtomicU64::new(0),
        });
        let dead = inner.evict(&mut inner.state());
        inner.remove_later(dead);
        Ok(Self { inner })
    }

    /// What the store has done.
    #[must_use]
    pub fn stats(&self) -> TreeStats {
        let state = self.inner.state();
        let c = &self.inner.counters;
        TreeStats {
            trees: state
                .trees
                .values()
                .filter(|t| matches!(t, Tree::Ready { .. }))
                .count() as u64,
            bytes: state.bytes,
            pinned: state
                .trees
                .values()
                .filter(|t| matches!(t, Tree::Ready { pins, .. } if *pins > 0))
                .count() as u64,
            builds: c.builds.load(Ordering::Relaxed),
            hits: c.hits.load(Ordering::Relaxed),
            waits: c.waits.load(Ordering::Relaxed),
            evictions: c.evictions.load(Ordering::Relaxed),
        }
    }

    /// The directory the store keeps its trees in.
    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.inner.dir
    }

    /// Pins the tree of `digest`, building it first if the store has none (or waiting
    /// for the build another acquire has under way). `known` is the Directory
    /// messages the caller already read; the build reads the rest. The returned
    /// stats are those of the build this call did, if it did one.
    ///
    /// # Errors
    /// `digest` is not a SHA-256 digest, a blob is missing or corrupt, the tree breaks
    /// a naming rule, or a write fails.
    pub(super) async fn acquire(
        &self,
        cas: &impl Cas,
        cache: &Cache,
        digest: &Digest,
        known: &Loaded,
    ) -> Result<(TreeLease, Option<InputStats>), TreeError> {
        let name = name_of(digest).ok_or_else(|| {
            TreeError::Invalid(format!(
                "tree digest {}/{} is not SHA-256",
                digest.hash, digest.size_bytes
            ))
        })?;
        let inner = &self.inner;
        let lease = |name: String| TreeLease {
            inner: Arc::clone(inner),
            path: inner.dir.join(&name),
            name,
        };
        loop {
            match self.step(&name) {
                Step::Pinned => return Ok((lease(name), None)),
                Step::Lead(builder) => {
                    let stats = self.lead(cas, cache, digest, known, builder).await?;
                    return Ok((lease(name), Some(stats)));
                }
                Step::Wait(mut waiting) => {
                    inner.counters.waits.fetch_add(1, Ordering::Relaxed);
                    // The sender is dropped when the builder is done, with a tree or
                    // without.
                    let _ = waiting.changed().await;
                }
            }
        }
    }

    /// What an acquire of `name` does next. Not async, so no lock is held across an
    /// await.
    fn step(&self, name: &str) -> Step {
        let inner = &self.inner;
        let mut state = inner.state();
        state.tick += 1;
        let tick = state.tick;
        match state.trees.get_mut(name) {
            Some(Tree::Ready { pins, used, .. }) => {
                *pins += 1;
                *used = tick;
                inner.counters.hits.fetch_add(1, Ordering::Relaxed);
                Step::Pinned
            }
            Some(Tree::Building(slot)) => Step::Wait(slot.clone()),
            None => {
                let (tx, rx) = watch::channel(());
                state.trees.insert(name.to_owned(), Tree::Building(rx));
                drop(state);
                let tmp = inner.dir.join(format!(
                    ".tmp-{}",
                    inner.next.fetch_add(1, Ordering::Relaxed)
                ));
                Step::Lead(Builder {
                    inner: Arc::clone(inner),
                    name: name.to_owned(),
                    tmp,
                    done: false,
                    _slot: tx,
                })
            }
        }
    }

    /// Builds the tree as `builder`, names it, and pins it.
    async fn lead(
        &self,
        cas: &impl Cas,
        cache: &Cache,
        digest: &Digest,
        known: &Loaded,
        mut builder: Builder,
    ) -> Result<InputStats, TreeError> {
        let inner = &self.inner;
        let tmp = builder.tmp.clone();
        tokio::fs::create_dir(&tmp).await.map_err(io_error(&tmp))?;
        let (summary, stats) = build(cas, cache, digest, known, &tmp).await?;
        let to = inner.dir.join(&builder.name);
        let (from, dest) = (tmp.clone(), to.clone());
        let moved = tokio::task::spawn_blocking(move || publish(&from, &dest))
            .await
            .map_err(|e| TreeError::Invalid(format!("naming the tree did not finish: {e}")))?
            .map_err(io_error(&to))?;
        inner.counters.builds.fetch_add(1, Ordering::Relaxed);
        let mut state = inner.state();
        state.tick += 1;
        let used = state.tick;
        state.bytes = state.bytes.saturating_add(summary.bytes);
        state.trees.insert(
            builder.name.clone(),
            Tree::Ready {
                bytes: summary.bytes,
                pins: 1,
                used,
            },
        );
        let dead = inner.evict(&mut state);
        drop(state);
        builder.done = true;
        // A tree already there was renamed in whole: ours is the spare.
        let mut dead = dead;
        if !moved {
            dead.push(tmp);
        }
        inner.remove_later(dead);
        Ok(stats)
    }
}
