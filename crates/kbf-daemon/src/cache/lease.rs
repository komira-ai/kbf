//! A lease's input files through the cache (`docs/design/worker-cache.md#62-what-a-lease-sees-behaviour-changes`).
//!
//! For each blob of the input tree, each of its two keys (plain and executable) that
//! the tree names:
//!
//! - an **empty** blob is created directly at each of its names, never looked up or
//!   linked;
//! - a name an output path covers gets a **private copy** (writable, as without the
//!   cache): the driver moves it into the overlay's upper directory, where the action
//!   may change it, so it must never be the cache's inode;
//! - the other names all get **one inode**: hard links to the entry, or, when linking
//!   is not possible (copy mode, the link limit near, `EMLINK`, `EXDEV`), one private
//!   copy for the lease, dated and moded as the entry, with every other name linked to
//!   it. A fallback is per key per lease, never per name: names already linked to the
//!   entry are unlinked first.
//!
//! A link that finds the entry gone (`ENOENT`) settles the key again and retries.

use std::io::{self, Write as _};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures::{StreamExt as _, stream};
use rustix::fs::{Timespec, Timestamps};
use tokio::sync::Semaphore;

use super::fill::{Budget, Fetch, InputCounts, InputStats, Source, blocking};
use super::layout::{FIXED_MTIME, Key};
use super::{Cache, LinkMode, bump};
use crate::cas::{CasError, label};
use crate::tree::{Copies, TreeError};

/// How many links short of the filesystem's limit an entry stops being linked.
const LINK_HEADROOM: u64 = 1000;

/// A key's names in one lease: those linked to one inode, and the private ones.
struct Names {
    key: Key,
    shared: Vec<PathBuf>,
    private: Vec<PathBuf>,
}

/// Writes every copy of each blob in `files` through `cache`, at most
/// [`Cache::link_concurrency`] keys at once and `bytes` of fetched blobs in memory, as
/// the module docs say. A name under one of `private` (absolute paths) gets a private
/// copy. The keys are pinned until it returns. After the first error no key is
/// started, and those started are waited for.
pub(crate) async fn write_files(
    cas: &dyn Fetch,
    cache: &Cache,
    files: Vec<Copies>,
    private: &[PathBuf],
    bytes: usize,
) -> Result<InputStats, TreeError> {
    let counts = InputCounts::default();
    let mut blobs = Vec::new();
    for blob in files {
        blobs.push(plan(blob, private, &counts)?);
    }
    let _pins = cache.pin(blobs.iter().flatten().map(|n| n.key).collect());
    let permits = Semaphore::new(bytes);
    let budget = Budget {
        permits: &permits,
        bytes,
    };
    let failed = AtomicBool::new(false);
    let (budget, failed, counts_ref) = (&budget, &failed, &counts);
    // A blob's two keys one after the other: the second is then a local copy of the
    // first, not a second fetch.
    let mut placed = stream::iter(blobs)
        .map(|keys| async move {
            for names in keys {
                if failed.load(Ordering::SeqCst) {
                    return Ok(());
                }
                let placed = place(cas, cache, names, budget, counts_ref).await;
                if placed.is_err() {
                    failed.store(true, Ordering::SeqCst);
                    return placed;
                }
            }
            Ok(())
        })
        .buffer_unordered(cache.link_concurrency());
    let mut first = Ok(());
    while let Some(outcome) = placed.next().await {
        if first.is_ok() {
            first = outcome;
        }
    }
    drop(placed);
    first.map(|()| counts.stats())
}

/// The keys of `blob`'s copies, with their names; an empty blob is written at once.
fn plan(blob: Copies, private: &[PathBuf], counts: &InputCounts) -> Result<Vec<Names>, TreeError> {
    // A digest that names no SHA-256 blob is one no CAS holds.
    let key = |executable| {
        Key::of(&blob.digest, executable)
            .ok_or_else(|| TreeError::Cas(CasError::Missing(label(&blob.digest))))
    };
    let (mut plain, mut executable) = (Names::of(key(false)?), Names::of(key(true)?));
    for (path, is_executable) in blob.copies {
        bump(&counts.files);
        if blob.digest.size_bytes == 0 {
            bump(&counts.empty_files);
            create(&path, Mode::Writable(is_executable), &mut |_| Ok(()))
                .map_err(crate::tree::io(&path))?;
            continue;
        }
        let names = if is_executable {
            &mut executable
        } else {
            &mut plain
        };
        if private.iter().any(|p| path.starts_with(p)) {
            names.private.push(path);
        } else {
            names.shared.push(path);
        }
    }
    Ok([plain, executable]
        .into_iter()
        .filter(|n| !n.shared.is_empty() || !n.private.is_empty())
        .collect())
}

impl Names {
    fn of(key: Key) -> Self {
        Self {
            key,
            shared: Vec::new(),
            private: Vec::new(),
        }
    }
}

/// Places every name of one key.
async fn place(
    cas: &dyn Fetch,
    cache: &Cache,
    names: Names,
    budget: &Budget<'_>,
    counts: &InputCounts,
) -> Result<(), TreeError> {
    let names = Arc::new(names);
    let mut stale = false;
    loop {
        let got = cache
            .get(cas, names.key, stale, budget, counts)
            .await
            .map_err(TreeError::Cas)?;
        let source = got.source;
        let (mode, link_max, link) = (
            cache.link_mode(),
            cache.link_max(),
            Arc::clone(&cache.inner.config.link),
        );
        let placing = Arc::clone(&names);
        let placed = blocking(move || match source {
            Source::Bytes(bytes) => from_bytes(&bytes, &placing),
            Source::Entry { path, nlink } => {
                from_entry(&path, nlink, &placing, mode, link_max, &*link)
            }
        })
        .await;
        match placed {
            Ok(fell_back) => {
                let files = (names.shared.len() + names.private.len()) as u64;
                let counted = if got.hit {
                    &counts.hits
                } else {
                    &counts.misses
                };
                counted.fetch_add(files, Ordering::Relaxed);
                if fell_back {
                    bump(&counts.link_fallbacks);
                }
                return Ok(());
            }
            Err(Placing::Gone) => stale = true,
            Err(Placing::Failed(path, source)) => return Err(TreeError::Io { path, source }),
        }
    }
}

/// Why a key's names could not all be placed.
enum Placing {
    /// The entry was gone: settle the key again.
    Gone,
    Failed(PathBuf, io::Error),
}

/// How a file a lease gets is moded and dated.
#[derive(Clone, Copy)]
enum Mode {
    /// As the cache's entry of a key: read-only, fixed mtime.
    Entry(Key),
    /// As an input without the cache: 0644 or 0755 (executable), the write's mtime.
    Writable(bool),
}

/// Places `names` from the bytes the cache could not keep: private copies, and one
/// read-only copy the shared names are linked to. Whether that was a fallback (always,
/// when there are shared names).
fn from_bytes(bytes: &[u8], names: &Names) -> Result<bool, Placing> {
    let mut fill = |file: &mut std::fs::File| file.write_all(bytes);
    for path in &names.private {
        create(path, Mode::Writable(names.key.executable), &mut fill).map_err(failed(path))?;
    }
    let Some((first, rest)) = names.shared.split_first() else {
        return Ok(false);
    };
    create(first, Mode::Entry(names.key), &mut fill).map_err(failed(first))?;
    for path in rest {
        std::fs::hard_link(first, path).map_err(failed(path))?;
    }
    Ok(true)
}

/// Places `names` from the entry at `entry` (with `nlink` links): see the module docs.
/// Whether the shared names fell back to a private copy.
fn from_entry(
    entry: &Path,
    nlink: u64,
    names: &Names,
    mode: LinkMode,
    link_max: u64,
    link: &super::LinkFn,
) -> Result<bool, Placing> {
    for path in &names.private {
        copy_new(entry, path, Mode::Writable(names.key.executable))?;
    }
    let Some((first, rest)) = names.shared.split_first() else {
        return Ok(false);
    };
    let fits = nlink + names.shared.len() as u64 <= link_max.saturating_sub(LINK_HEADROOM);
    if mode == LinkMode::Hardlink && fits {
        match link_all(entry, first, rest, link) {
            Ok(()) => return Ok(false),
            Err(e) if e.raw_os_error() == Some(libc::ENOENT) => return Err(Placing::Gone),
            Err(e) if matches!(e.raw_os_error(), Some(libc::EMLINK | libc::EXDEV)) => {}
            Err(e) => return Err(failed(first)(e)),
        }
    }
    copy_new(entry, first, Mode::Entry(names.key))?;
    for path in rest {
        std::fs::hard_link(first, path).map_err(failed(path))?;
    }
    Ok(true)
}

/// Links `first` to `entry` and each of `rest` to `first`: one inode. On an error, the
/// names already linked are unlinked again, so a fallback starts from none.
fn link_all(entry: &Path, first: &Path, rest: &[PathBuf], link: &super::LinkFn) -> io::Result<()> {
    link(entry, first)?;
    for (done, path) in rest.iter().enumerate() {
        if let Err(e) = link(first, path).or_else(|_| std::fs::copy(entry, path).map(drop)) {
            for linked in std::iter::once(first).chain(rest[..done].iter().map(PathBuf::as_path)) {
                std::fs::remove_file(linked)?;
            }
            return Err(e);
        }
    }
    Ok(())
}

/// Copies the entry at `entry` into a new file at `to`, moded and dated by `mode`.
fn copy_new(entry: &Path, to: &Path, mode: Mode) -> Result<(), Placing> {
    // An entry that cannot be opened is settled again: its slot's holder finds out why.
    let Ok(mut from) = std::fs::File::open(entry) else {
        return Err(Placing::Gone);
    };
    create(to, mode, &mut |file| io::copy(&mut from, file).map(drop)).map_err(failed(to))
}

/// The error placing `path` failed with.
fn failed(path: &Path) -> impl FnOnce(io::Error) -> Placing + '_ {
    move |e| Placing::Failed(path.to_owned(), e)
}

/// Creates `path` (`O_EXCL`), fills it with `fill`, then modes and dates it.
fn create(
    path: &Path,
    mode: Mode,
    fill: &mut dyn FnMut(&mut std::fs::File) -> io::Result<()>,
) -> io::Result<()> {
    let created = match mode {
        Mode::Entry(_) => 0o600,
        Mode::Writable(true) => 0o755,
        Mode::Writable(false) => 0o644,
    };
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(created)
        .open(path)?;
    fill(&mut file)?;
    if let Mode::Entry(key) = mode {
        rustix::fs::fchmod(&file, rustix::fs::Mode::from_raw_mode(key.mode()))?;
        let fixed = Timespec {
            tv_sec: FIXED_MTIME,
            tv_nsec: 0,
        };
        rustix::fs::futimens(
            &file,
            &Timestamps {
                last_access: fixed,
                last_modification: fixed,
            },
        )?;
    }
    Ok(())
}
