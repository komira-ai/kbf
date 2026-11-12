//! The put directory (`--stage-and-commit`): the run's packs and large objects are written as
//! files into `--out-dir` and committed by `rename`, instead of PUT to presigned URLs.
//!
//! In a job the directory is a mount of tierstore's put directory: a file written there and
//! renamed to a name without `.part` becomes an object under the key prefix the frontend chose,
//! and the `rename` returns only once the store has it. kbf-task does not know the key or the
//! store; it knows the names, which are the run's slots: pack 0 is `pack`, pack `s` is
//! `pack<s>` and large object `i` is `L<i>`.
//!
//! [`OutDir::store`] runs in these phases, each timed into the manifest's `timing`
//! ([`kbf_task_pack::Timing`]):
//!
//! 1. walk (`walk_ms`): the outputs were already walked and hashed when they were collected
//!    ([`crate::task::collected`]); this is that time.
//! 2. hash (`hash_ms`): the blobs are placed in slots ([`crate::place::place`]) and each pack is
//!    laid out from its blobs' digests alone ([`PackLayout::plan_digests`]): no output is read.
//!    Pack 0 is laid out with the manifest as wide as it can be written, so every cap and every
//!    rule of the manifest is checked here, before a byte is written: a run that would be refused
//!    fails (exit 12) with the directory untouched.
//! 3. write (`write_ms`): each large object, then packs `N-1` down to 1, is written as
//!    `<name>.part`, created new (`O_EXCL`), sequentially, a large object from the file it was
//!    recorded from and a pack by [`PackLayout::write_to`]. Each output is read once here, and
//!    that one read is checked against its digest, gives each record's CRC-32C for the footer and
//!    the object's SHA-256; a blob whose bytes are no longer its digest's fails the run (exit 12,
//!    `output-changed`).
//! 4. commit (`commit_ms`): each of them, in the same order, is renamed to
//!    `<name>.sha256-<hex>.part`, which tells the store the digest to expect, and then committed
//!    by renaming that to `<name>`.
//!
//! Then the manifest is written with those times (and `pack_ms`, the four together), and pack 0,
//! the manifest its first record, is written and committed the same way, last. So pack 0's own
//! write and commit are in no timing, and pack 0 exists only once every other object does.
//!
//! The run's exit code is 0 only when every commit `rename` returned 0 (the Command's own exit
//! code is in the manifest). A commit that fails with `EBUSY` (the store still sees the file
//! open), `EIO` (the store did not take it before its deadline) or `EINTR` is tried again, up to
//! [`COMMIT_ATTEMPTS`] times in all, after the pauses of [`PAUSES`]; any other answer, or the last
//! attempt's, fails the run (exit 13) with the errno and what the store means by it (`EEXIST`:
//! the key holds other bytes; `EBADMSG`: the bytes are not the digest's; `EFBIG`: too large for
//! the store). A file that cannot be created or written is exit 13 too. Once a run fails, every
//! `.part` file it made and did not commit is removed; an object it committed stays, an orphan
//! no pack 0 names.
//!
//! The file is not synced and its close is not checked: the commit `rename` is the only point
//! the store acts on, and it refuses bytes that are not the digest the name gives.

use std::fs::OpenOptions;
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use kbf_task_pack::{LayoutWriteError, PackLayout, TaskManifest};
use kbf_types::Digest;

use crate::digest;
use crate::outputs::Blobs;
use crate::pack::{MAX_LARGE_BYTES, PackError};
use crate::place::{manifest_reserve, place};
use crate::plan::Limits;
use crate::task::{Failure, millis};

/// The bytes read from a blob, and buffered for the file, at one time.
const COPY_BYTES: usize = 1 << 20;

/// The pauses before the second and later attempts of a commit `rename`.
pub(crate) const PAUSES: [Duration; 3] = [
    Duration::from_millis(100),
    Duration::from_secs(1),
    Duration::from_secs(5),
];

/// The most attempts of one commit `rename`.
pub(crate) const COMMIT_ATTEMPTS: usize = PAUSES.len() + 1;

/// The name of pack `slot`: `pack` for pack 0, which holds the manifest, `pack<slot>` for the
/// others.
#[must_use]
pub(crate) fn pack_name(slot: usize) -> String {
    match slot {
        0 => "pack".to_owned(),
        _ => format!("pack{slot}"),
    }
}

/// The name of large object `slot`: `L<slot>`.
#[must_use]
pub(crate) fn large_name(slot: usize) -> String {
    format!("L{slot}")
}

/// The name an object is written under: `<name>.part`.
#[must_use]
pub(crate) fn part_name(name: &str) -> String {
    format!("{name}.part")
}

/// The name that tells the store the digest to expect: `<name>.sha256-<hex>.part`.
#[must_use]
pub(crate) fn digest_part_name(name: &str, digest: &Digest) -> String {
    format!("{name}.sha256-{}.part", hex::encode(digest.hash))
}

/// Why an object was not written or committed. Each names the object.
#[derive(Debug, thiserror::Error)]
pub enum OutDirError {
    /// `<name>.part` could not be created.
    #[error("{name}: cannot create it in the put directory: {source}")]
    Create {
        /// The object's name.
        name: String,
        /// The error of the create.
        #[source]
        source: io::Error,
    },
    /// `<name>.part` could not be written.
    #[error("{name}: cannot write it to the put directory: {source}")]
    Write {
        /// The object's name.
        name: String,
        /// The error of the write.
        #[source]
        source: io::Error,
    },
    /// `<name>.part` could not be renamed to the name of its digest.
    #[error("{name}: cannot rename it to the name of its digest: {source}")]
    Digest {
        /// The object's name.
        name: String,
        /// The error of the rename.
        #[source]
        source: io::Error,
    },
    /// The commit `rename` to `<name>` failed.
    #[error(
        "{name}: the commit failed on attempt {attempts} of {COMMIT_ATTEMPTS}: {source}{}",
        meaning(source)
    )]
    Commit {
        /// The object's name.
        name: String,
        /// The attempt that failed last.
        attempts: usize,
        /// The error of that attempt.
        #[source]
        source: io::Error,
    },
}

/// What the store means by the errno of a failed commit, as the end of a message.
fn meaning(error: &io::Error) -> &'static str {
    match error.raw_os_error() {
        Some(libc::EEXIST) => "; the key holds another object",
        Some(libc::EBADMSG) => "; the bytes are not those of the digest the name gives",
        Some(libc::EFBIG) => "; the object is larger than the store takes",
        Some(libc::EIO) => "; the store did not take it before its deadline",
        Some(libc::EBUSY) => "; the store still sees the file open",
        _ => "",
    }
}

/// Whether a failed commit is tried again.
fn retried(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EBUSY | libc::EIO | libc::EINTR)
    )
}

/// Creates a file, new, for writing.
type Create = dyn Fn(&Path) -> io::Result<Box<dyn Write>>;

/// Renames a file: the digest rename and the commit.
type Rename = dyn Fn(&Path, &Path) -> io::Result<()>;

/// The real create: `O_CREAT | O_EXCL`, so a name that is already there is refused.
fn create_new(path: &Path) -> io::Result<Box<dyn Write>> {
    let file = OpenOptions::new().write(true).create_new(true).open(path)?;
    Ok(Box::new(file))
}

/// The real rename: the file system's, which in tierstore's put directory, for a name without
/// `.part`, returns once the store has the object.
fn rename(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::rename(from, to)
}

/// The put directory a run writes into.
pub struct OutDir {
    dir: PathBuf,
    create: Box<Create>,
    rename: Box<Rename>,
    pause: Box<dyn Fn(Duration)>,
}

impl OutDir {
    /// The put directory at `dir`, which must be a directory (a symlink to one is followed).
    /// Files are created and renamed through the file system, and the pauses between attempts
    /// of a commit are sleeps.
    ///
    /// # Errors
    /// [`Failure::Setup`] when `dir` is not a directory or cannot be looked at.
    pub fn open(dir: &Path) -> Result<Self, Failure> {
        let setup = |why: String| Failure::Setup(format!("--out-dir {}: {why}", dir.display()));
        match std::fs::metadata(dir) {
            Ok(meta) if meta.is_dir() => Ok(Self {
                dir: dir.to_owned(),
                create: Box::new(create_new),
                rename: Box::new(rename),
                pause: Box::new(std::thread::sleep),
            }),
            Ok(_) => Err(setup("not a directory".to_owned())),
            Err(e) => Err(setup(e.to_string())),
        }
    }

    /// The same directory, creating its files through `create`.
    #[cfg(test)]
    pub(crate) fn with_create(
        self,
        create: impl Fn(&Path) -> io::Result<Box<dyn Write>> + 'static,
    ) -> Self {
        Self {
            create: Box::new(create),
            ..self
        }
    }

    /// The same directory, renaming (the digest rename and the commit) through `rename`.
    #[cfg(test)]
    pub(crate) fn with_rename(
        self,
        rename: impl Fn(&Path, &Path) -> io::Result<()> + 'static,
    ) -> Self {
        Self {
            rename: Box::new(rename),
            ..self
        }
    }

    /// The same directory, pausing between attempts through `pause` instead of a sleep.
    #[cfg(test)]
    pub(crate) fn with_pause(self, pause: impl Fn(Duration) + 'static) -> Self {
        Self {
            pause: Box::new(pause),
            ..self
        }
    }

    /// The directory.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.dir
    }

    /// Places `blobs` into the slots `limits` allows, writes and commits every large object and
    /// pack, pack 0 last, and sets the manifest's `packs`, `large` and phase timings, as the
    /// module documentation says. `manifest.timing.pack_ms` is, on entry, the time the outputs
    /// took to collect.
    ///
    /// # Errors
    /// A [`Failure`]: a pack refusal (exit 12) before anything is written, a blob that changed
    /// (exit 12), or an [`OutDirError`] (exit 13). Every `.part` file written and not committed
    /// is removed.
    pub fn store(
        &self,
        manifest: &mut TaskManifest,
        blobs: &Blobs,
        limits: &Limits,
    ) -> Result<(), Failure> {
        let started = Instant::now();
        let walk_ms = manifest.timing.pack_ms;

        // Hash: everything that can refuse the run, before a byte is written.
        let reserve = manifest_reserve(&widest(manifest), limits)?;
        let planned = place(manifest, blobs, reserve, limits)?;
        let widest_json = widest(manifest)
            .to_json_with(MAX_LARGE_BYTES)
            .map_err(PackError::from)?;
        layout(&widest_json, &planned.packs[0])?;
        let extra = (planned.packs[1..].iter())
            .map(|digests| PackLayout::plan_digests(digests).map_err(PackError::from))
            .collect::<Result<Vec<_>, _>>()?;
        let hash_ms = millis(started.elapsed());

        // Write: the large objects, then packs N-1 down to 1.
        let writing = Instant::now();
        let mut stage = Stage::new(self);
        let mut written = Vec::new();
        for (slot, digest) in planned.large.iter().enumerate() {
            let name = large_name(slot);
            stage.large(&name, blobs, digest)?;
            written.push((name, *digest));
        }
        for (slot, pack) in extra.iter().enumerate().rev() {
            let name = pack_name(slot + 1);
            let digest = stage.pack(&name, pack, blobs, None)?;
            written.push((name, digest));
        }
        let write_ms = millis(writing.elapsed());

        // Commit, in the same order.
        let committing = Instant::now();
        for (name, digest) in &written {
            stage.commit(name, digest)?;
        }
        let commit_ms = millis(committing.elapsed());

        let timing = &mut manifest.timing;
        timing.walk_ms = Some(walk_ms);
        timing.hash_ms = Some(hash_ms);
        timing.write_ms = Some(write_ms);
        timing.commit_ms = Some(commit_ms);
        timing.pack_ms = walk_ms.saturating_add(millis(started.elapsed()));

        // Pack 0 last: the manifest, then its blobs.
        let json = manifest
            .to_json_with(MAX_LARGE_BYTES)
            .map_err(PackError::from)?;
        let pack_0 = layout(&json, &planned.packs[0])?;
        let name = pack_name(0);
        let digest = stage.pack(&name, &pack_0, blobs, Some(&json))?;
        stage.commit(&name, &digest)
    }
}

/// `manifest` with every timing at the widest it can be written (`i64::MAX`), so its JSON is
/// at least as long as the manifest's with any times this run measures.
fn widest(manifest: &TaskManifest) -> TaskManifest {
    let most = i64::MAX.unsigned_abs();
    let mut widest = manifest.clone();
    widest.timing.pack_ms = most;
    widest.timing.walk_ms = Some(most);
    widest.timing.hash_ms = Some(most);
    widest.timing.write_ms = Some(most);
    widest.timing.commit_ms = Some(most);
    widest
}

/// The layout of pack 0: the manifest `json` as its first record, then the blobs of `digests`.
fn layout(json: &[u8], digests: &[Digest]) -> Result<PackLayout, PackError> {
    let records: Vec<Digest> = std::iter::once(kbf_segments::sha256(json))
        .chain(digests.iter().copied())
        .collect();
    Ok(PackLayout::plan_digests(&records)?)
}

/// The `.part` files of one run that are not committed yet; they are removed when it is
/// dropped, so a failed run leaves none.
struct Stage<'a> {
    out: &'a OutDir,
    parts: Vec<PathBuf>,
}

impl<'a> Stage<'a> {
    fn new(out: &'a OutDir) -> Self {
        Self {
            out,
            parts: Vec::new(),
        }
    }

    /// Creates `<name>.part`, new, and records it, for the caller to write.
    fn create(&mut self, name: &str) -> Result<BufWriter<Box<dyn Write>>, OutDirError> {
        let path = self.out.dir.join(part_name(name));
        let file = (self.out.create)(&path).map_err(|source| OutDirError::Create {
            name: name.to_owned(),
            source,
        })?;
        self.parts.push(path);
        Ok(BufWriter::with_capacity(COPY_BYTES, file))
    }

    /// Writes the recorded blob `digest` as large object `name`, streamed from where it was
    /// recorded, and checks its bytes are the digest's. A read of the blob that fails is a
    /// failed write, as a large object's is on the presigned path.
    fn large(&mut self, name: &str, blobs: &Blobs, digest: &Digest) -> Result<(), Failure> {
        let mut file = self.create(name)?;
        let blob = digest::to_reapi(digest);
        let mut reader = blobs.reader(&blob)?;
        let len = io::copy(&mut reader, &mut file).map_err(write_error(name))?;
        file.flush().map_err(write_error(name))?;
        if len != digest.size_bytes || !reader.matches(&blob) {
            return Err(PackError::changed(digest).into());
        }
        Ok(())
    }

    /// Writes the pack `layout` as `name`: each record from the recorded blob of its digest, or,
    /// for the manifest's record, from `manifest`. Returns the digest of the whole pack.
    fn pack(
        &mut self,
        name: &str,
        layout: &PackLayout,
        blobs: &Blobs,
        manifest: Option<&[u8]>,
    ) -> Result<Digest, Failure> {
        let mut file = self.create(name)?;
        let manifest = manifest.map(|json| (kbf_segments::sha256(json), json));
        let written = layout.write_to(&mut file, |digest| -> io::Result<Box<dyn Read + '_>> {
            match manifest {
                Some((record, json)) if record == *digest => Ok(Box::new(json)),
                _ => Ok(Box::new(blobs.reader(&digest::to_reapi(digest))?)),
            }
        });
        let digest = written.map_err(|e| match e {
            LayoutWriteError::Changed { digest } => Failure::from(PackError::changed(&digest)),
            e @ LayoutWriteError::Source { .. } => Failure::Read(io::Error::other(e)),
            LayoutWriteError::Sink(source) => write_error(name)(source).into(),
        })?;
        file.flush().map_err(write_error(name))?;
        Ok(digest)
    }

    /// Renames `<name>.part` to the name of `digest`, then commits it as `name`, trying a commit
    /// that failed with `EBUSY`, `EIO` or `EINTR` again after each of [`PAUSES`].
    fn commit(&mut self, name: &str, digest: &Digest) -> Result<(), Failure> {
        let part = self.out.dir.join(part_name(name));
        let staged = self.out.dir.join(digest_part_name(name, digest));
        (self.out.rename)(&part, &staged).map_err(|source| OutDirError::Digest {
            name: name.to_owned(),
            source,
        })?;
        self.parts.retain(|p| *p != part);
        self.parts.push(staged.clone());
        let to = self.out.dir.join(name);
        for attempt in 1.. {
            match (self.out.rename)(&staged, &to) {
                Ok(()) => break,
                Err(source) if retried(&source) && attempt < COMMIT_ATTEMPTS => {
                    (self.out.pause)(PAUSES[attempt - 1]);
                }
                Err(source) => {
                    return Err(OutDirError::Commit {
                        name: name.to_owned(),
                        attempts: attempt,
                        source,
                    }
                    .into());
                }
            }
        }
        self.parts.retain(|p| *p != staged);
        Ok(())
    }
}

impl Drop for Stage<'_> {
    fn drop(&mut self) {
        for part in &self.parts {
            // Best effort: a file that cannot be removed is left to the store's sweep.
            let _ = std::fs::remove_file(part);
        }
    }
}

/// The error of a write to `name`.
fn write_error(name: &str) -> impl Fn(io::Error) -> OutDirError + '_ {
    move |source| OutDirError::Write {
        name: name.to_owned(),
        source,
    }
}

#[cfg(test)]
#[path = "out_dir_tests.rs"]
mod tests;
