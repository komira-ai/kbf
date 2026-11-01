//! Reading an action's outputs from the overlay's upper directory without following a
//! symlink at any level, within limits.
//!
//! The upper directory is the action's to shape. It can replace the working directory,
//! or any directory on an output's path, with a symlink to a host path
//! (`rm -rf d; ln -s /host/dir d`, output `d/key.pem`). So no output is read by path.
//! The upper directory is opened once, and every component below it, the working
//! directory's included, is opened relative to its parent's descriptor with
//! `O_NOFOLLOW`. A component on the way that is absent, a file or a symlink means the
//! action did not create the output there, and the output is left out. The last
//! component is examined with `AT_SYMLINK_NOFOLLOW`: a symlink is recorded as one,
//! never dereferenced, and a file is opened with `O_NOFOLLOW` and its type checked
//! again on the descriptor (an entry that changed is an error). The same holds inside
//! an output directory. `.`, `..` and empty components are refused before the walk.
//!
//! The action also decides how deep, how wide and how large its outputs are. An output
//! directory is walked iteratively, its stack of open directories a `Vec` on the heap,
//! so no depth of tree can overflow a thread's stack and take the daemon down. The
//! walk stops with [`TreeError::Limit`], failing the action, once its outputs pass any
//! of the [`OutputLimits`]: directory depth, entries, file bytes. Only the directory
//! being read holds a descriptor: the walk returns to a parent through the child's
//! `..`, and refuses a `..` that is not the directory it came from (device and inode).
//!
//! The outputs are read in three steps, none of which copies a byte. One blocking task
//! lists the output paths and walks each output directory (names, kinds, symlink
//! targets) and holds no file open. Every regular file found is then hashed exactly
//! once, on the node's [`CpuPool`] (`--cpu-threads`): SHA-256 for its digest and CRC-32C
//! for the footer of the segment it is sent in, one chunk at a time, one chunk in memory
//! per thread ([`crate::hashing`]). The Directory messages and the Tree are built from
//! the digests, and the blobs are stored with [`Cas::put_records`], each file as a
//! [`FileSource`] the transfer engine reads again, by name and without following a
//! symlink, only when it sends the file ([`crate::hashing`] stamps what it hashed; a
//! file that changed since is refused, and the server checks every record's digest as it
//! arrives). One past the byte limit is refused after `limit + 1` bytes. The action's
//! stdout and stderr are stored within their own limit: hashed and sent in the same
//! `put_records` as the outputs when the store has no intake ([`crate::logs`]), else
//! one by one through the intake ([`collect_log`]).
//!
//! A store in this process (the in-process plane, [`Cas::intake`]) takes each file's
//! chunks as the hash pass reads them, so each output file, and stdout and stderr, is
//! read exactly once: those files are stored when the intake is finished, before the
//! Directory messages and the Tree go to [`Cas::put_records`], and are never read again.
//!
//! REAPI names are UTF-8 strings, sorted by code point. A name or symlink target in an
//! output that is not UTF-8 fails the action ([`TreeError::NotUtf8`]) rather than
//! being recorded altered: two such names could become one, and the sort order would
//! no longer be REAPI's.
//!
//! The listing is one task of tokio's blocking pool; the hashing runs on the CPU pool.

use std::collections::HashSet;
use std::ffi::OsStr;
use std::os::fd::{AsFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use kbf_daemon::cas::{
    Cas, CasError, FileSource, Intake, OPEN_DIRECTORY, OPEN_FILE, Record, Source, descend,
    proto_digest,
};
use kbf_daemon::cpu_pool::CpuPool;
use kbf_proto::reapi::{
    ActionResult, Digest, Directory, DirectoryNode, FileNode, OutputDirectory, OutputFile,
    OutputSymlink, SymlinkNode, Tree,
};
use prost::Message;
use rustix::fs::{AtFlags, CWD, Dir, FileType, Mode, OFlags, Stat};
use rustix::io::Errno;

use crate::cas::FileBlob;
use crate::hashing::{Digested, Job, Outcome, hash_all, read_through};
use crate::logs::{Logs, after_outputs, missing_only, with_outputs};
use crate::tree::{Exceeded, TreeError, check_relative};

/// How much output one action may leave. An action past a limit fails with
/// [`TreeError::Limit`], and none of its outputs is recorded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::Args)]
pub struct OutputLimits {
    /// The deepest directory an output directory may hold, in levels below it (its own
    /// entries are level 1).
    #[arg(long = "output-max-depth", default_value_t = OutputLimits::DEFAULT.max_depth)]
    pub max_depth: usize,
    /// The most entries (files, directories, symlinks and anything else) one action's
    /// outputs may hold together, the declared outputs included.
    #[arg(long = "output-max-entries", default_value_t = OutputLimits::DEFAULT.max_entries)]
    pub max_entries: u64,
    /// The most bytes one action's output files may hold together.
    #[arg(long = "output-max-bytes", default_value_t = OutputLimits::DEFAULT.max_bytes)]
    pub max_bytes: u64,
    /// The most bytes each of the action's stdout and stderr may hold.
    #[arg(
        long = "output-max-stdio-bytes",
        default_value_t = OutputLimits::DEFAULT.max_stdio_bytes
    )]
    pub max_stdio_bytes: u64,
}

impl OutputLimits {
    /// 512 levels, a million entries, 16 GiB of files, 1 GiB each of stdout and stderr.
    pub const DEFAULT: Self = Self {
        max_depth: 512,
        max_entries: 1_000_000,
        max_bytes: 16 << 30,
        max_stdio_bytes: 1 << 30,
    };
}

impl Default for OutputLimits {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// What one action's outputs have used of its [`OutputLimits`] so far.
struct Budget {
    limits: OutputLimits,
    entries: u64,
}

impl Budget {
    fn new(limits: OutputLimits) -> Self {
        Self { limits, entries: 0 }
    }

    fn entries_left(&self) -> u64 {
        self.limits.max_entries.saturating_sub(self.entries)
    }

    /// Counts `n` entries found at `shown`.
    fn entries(&mut self, n: u64, shown: &Path) -> Result<(), TreeError> {
        if n > self.entries_left() {
            return Err(limit(shown, Exceeded::Entries, self.limits.max_entries));
        }
        self.entries += n;
        Ok(())
    }
}

pub(crate) fn limit(shown: &Path, what: Exceeded, limit: u64) -> TreeError {
    TreeError::Limit {
        path: shown.to_owned(),
        what,
        limit,
    }
}

/// Opens a directory, refusing a symlink in its place.
pub(crate) const DIRECTORY: OFlags = OPEN_DIRECTORY;

/// Opens a regular file for reading, refusing a symlink, and never blocking on a FIFO
/// or claiming a terminal that replaced it after the `stat`.
pub(crate) const FILE: OFlags = OPEN_FILE;

/// What a name in a directory is, read without following it.
enum Kind {
    File {
        /// Its size when listed.
        size: u64,
    },
    Directory,
    Symlink,
    /// A socket, FIFO or device (an overlay whiteout is a character device): left out.
    Other,
}

fn kind(stat: &Stat) -> Kind {
    match FileType::from_raw_mode(stat.st_mode) {
        FileType::RegularFile => Kind::File {
            size: u64::try_from(stat.st_size).unwrap_or(0),
        },
        FileType::Directory => Kind::Directory,
        FileType::Symlink => Kind::Symlink,
        _ => Kind::Other,
    }
}

#[cfg(test)]
thread_local! {
    /// How many tasks this thread has sent to the blocking pool, for the test that checks the
    /// walk does not send one per file. Per thread, so tests that run side by side do not
    /// count each other's (the test runtime is single-threaded and runs the walk's calls on
    /// the test's own thread).
    pub(crate) static HOPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Runs `f` on the blocking pool; a system call error is reported against `shown`
/// (a path for the message only, never opened).
pub(crate) async fn blocking<T: Send + 'static>(
    shown: &Path,
    f: impl FnOnce() -> std::io::Result<T> + Send + 'static,
) -> Result<T, TreeError> {
    let io = |source: std::io::Error| TreeError::Io {
        path: shown.to_owned(),
        source,
    };
    #[cfg(test)]
    HOPS.with(|hops| hops.set(hops.get() + 1));
    // A task that panicked (a bug here) or was cancelled (the runtime shutting down)
    // is an I/O error like the call's own.
    tokio::task::spawn_blocking(f)
        .await
        .map_err(std::io::Error::other)
        .and_then(|done| done)
        .map_err(io)
}

/// What `name` in `dir` is, or `None` when it is absent.
fn examine(dir: &OwnedFd, name: &OsStr) -> std::io::Result<Option<Kind>> {
    match rustix::fs::statat(dir, name, AtFlags::SYMLINK_NOFOLLOW) {
        Ok(stat) => Ok(Some(kind(&stat))),
        Err(Errno::NOENT) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// A log read once: what [`collect_log`] stores, or why it does not.
enum Log {
    /// More than the limit.
    TooLarge,
    /// Taken by the store as it was read, its digest this; stored when `intake` is
    /// finished.
    Taken(kbf_types::Digest, Arc<dyn Intake>),
    /// Hashed, to be read again as it is sent.
    Hashed(FileBlob),
}

/// The log open as `fd`, read through once: handed to `intake` as it is hashed when
/// there is one that takes it, else hashed ([`FileBlob::hash`]) to be sent later. At most
/// `max + 1` bytes are read, one chunk held at a time. The type is checked on the
/// descriptor: an entry that stopped being a regular file after it was examined is an
/// error, never read.
fn read_log(fd: OwnedFd, max: u64, intake: Option<Arc<dyn Intake>>) -> std::io::Result<Log> {
    let stat = rustix::fs::fstat(&fd)?;
    if !matches!(kind(&stat), Kind::File { .. }) {
        return Err(std::io::Error::other(
            "is no longer a regular file once opened",
        ));
    }
    let size = u64::try_from(stat.st_size).unwrap_or(0);
    let file = std::fs::File::from(fd);
    // A log past the limit is refused without being offered; the read still stops one
    // byte past it.
    let taking = intake
        .filter(|_| size <= max)
        .and_then(|intake| Some((intake.file(size)?, intake)));
    let Some((mut taking, intake)) = taking else {
        return Ok(FileBlob::hash(file, max)?.map_or(Log::TooLarge, Log::Hashed));
    };
    let read = read_through(&file, size, max, &mut Vec::new(), Some(taking.as_mut()))?;
    Ok(read.map_or(Log::TooLarge, |(digest, crc32c)| {
        taking.finish(digest, crc32c);
        Log::Taken(digest, intake)
    }))
}

/// The target of the symlink `name` in `dir`, as written; never followed.
fn read_link(dir: &OwnedFd, name: &OsStr) -> std::io::Result<Vec<u8>> {
    Ok(rustix::fs::readlinkat(dir, name, Vec::new())?.into_bytes())
}

/// `bytes` as a REAPI string, or [`TreeError::NotUtf8`] naming `what` at `shown`.
fn utf8(bytes: Vec<u8>, shown: &Path, what: &'static str) -> Result<String, TreeError> {
    String::from_utf8(bytes).map_err(|_| TreeError::NotUtf8 {
        path: shown.to_owned(),
        what,
    })
}

/// Stores the log the driver captured at `path` (the action's stdout or stderr) in
/// the CAS one chunk at a time, and returns its digest. Into a store with an intake
/// ([`Cas::intake`]) the log is read once, hashed and handed over in the same pass;
/// otherwise it is hashed, then read again as it is sent. A log of more than `max`
/// bytes fails with [`TreeError::Limit`] once `max + 1` bytes are read. The log is
/// opened without following a symlink.
pub(crate) async fn collect_log(
    cas: &impl Cas,
    path: &Path,
    max: u64,
) -> Result<kbf_proto::reapi::Digest, TreeError> {
    let (open, intake) = (path.to_owned(), cas.intake());
    let read = blocking(path, move || {
        read_log(
            rustix::fs::openat(CWD, &open, FILE, Mode::empty())?,
            max,
            intake,
        )
    })
    .await?;
    match read {
        Log::Hashed(blob) => Ok(blob.store(cas).await?),
        other => kept(other, path, max).await,
    }
}

/// The digest of a log [`read_log`] left nothing to send of: stored by the intake that
/// took it, or refused as more than `max` bytes.
async fn kept(log: Log, path: &Path, max: u64) -> Result<Digest, TreeError> {
    match log {
        Log::Taken(digest, intake) => {
            intake.finish().await?;
            Ok(proto_digest(&digest))
        }
        _ => Err(limit(path, Exceeded::Stdio, max)),
    }
}

/// Stores the files `intake` took, when there is one: before any message that names
/// them is stored.
async fn finish(intake: Option<Arc<dyn Intake>>) -> Result<(), CasError> {
    match intake {
        Some(intake) => intake.finish().await,
        None => Ok(()),
    }
}

/// An entry of a directory being walked: its name, what it is, and for a symlink its
/// target.
struct Entry {
    name: Vec<u8>,
    kind: Kind,
    link: Vec<u8>,
}

/// The entries of `dir` (without `.` and `..`), sorted by name as REAPI wants, each
/// with what it is and, for a symlink, where it points (read here, not by a task of its
/// own). Stops after `max + 1` entries, so a directory past the entry limit is never
/// read whole; the caller refuses it.
fn list(dir: &OwnedFd, max: u64) -> std::io::Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for entry in Dir::read_from(dir)? {
        let name = entry?.file_name().to_bytes().to_vec();
        if name == b"." || name == b".." {
            continue;
        }
        let stat = rustix::fs::statat(dir, OsStr::from_bytes(&name), AtFlags::SYMLINK_NOFOLLOW)?;
        let kind = kind(&stat);
        let link = if matches!(kind, Kind::Symlink) {
            read_link(dir, OsStr::from_bytes(&name))?
        } else {
            Vec::new()
        };
        entries.push(Entry { name, kind, link });
        if entries.len() as u64 > max {
            break;
        }
    }
    entries.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(entries)
}

/// A directory's device and inode.
pub(crate) type Identity = (u64, u64);

pub(crate) fn identity(fd: &impl AsFd) -> std::io::Result<Identity> {
    let stat = rustix::fs::fstat(fd)?;
    Ok((stat.st_dev, stat.st_ino))
}

/// Opens the directory `name` in `dir`: its descriptor, identity and entries (at most
/// `max + 1`, see [`list`]).
fn open_listed(
    dir: &OwnedFd,
    name: &OsStr,
    max: u64,
) -> std::io::Result<(OwnedFd, Identity, Vec<Entry>)> {
    let fd = rustix::fs::openat(dir, name, DIRECTORY, Mode::empty())?;
    let id = identity(&fd)?;
    let entries = list(&fd, max)?;
    Ok((fd, id, entries))
}

/// Opens the parent of `dir` through its `..`, refusing any directory but `expected`:
/// the walk goes back only where it came from.
pub(crate) fn back_to_parent(dir: &OwnedFd, expected: Identity) -> std::io::Result<OwnedFd> {
    let fd = rustix::fs::openat(dir, "..", DIRECTORY, Mode::empty())?;
    if identity(&fd)? != expected {
        return Err(std::io::Error::other(
            "the walk's parent directory was replaced",
        ));
    }
    Ok(fd)
}

/// Reads each output path under `working_directory` in `upper` and records it in
/// `result`, all of them together within `limits`, and stores the blobs through `cas`.
/// See the module comment for what is followed (nothing) and what is left out. The files
/// are hashed on `pool`, once each; the blobs are then stored with [`Cas::put_records`].
/// With `logs`, the action's stdout and stderr are stored too and their digests set in
/// `result`: in that same `put_records` when the store has no intake ([`crate::logs`]),
/// else through the intake once the outputs are stored.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn collect(
    cas: &impl Cas,
    upper: &Path,
    pool: &CpuPool,
    working_directory: &str,
    paths: &[String],
    limits: OutputLimits,
    logs: Option<&Logs<'_>>,
    result: &mut ActionResult,
) -> Result<(), TreeError> {
    check_relative("working directory", working_directory)?;
    let base = upper.join(working_directory);
    let upper_fd = {
        let upper = upper.to_owned();
        Arc::new(
            blocking(&base, move || {
                Ok(rustix::fs::openat(CWD, &upper, DIRECTORY, Mode::empty())?)
            })
            .await?,
        )
    };
    let wd_parts: Vec<String> = working_directory
        .split('/')
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect();
    let started = Instant::now();
    let mut listing = {
        let upper_fd = Arc::clone(&upper_fd);
        let (owned_base, paths) = (base.clone(), paths.to_vec());
        blocking(&base, move || {
            Ok(list_outputs(
                &upper_fd,
                &wd_parts,
                &owned_base,
                &paths,
                limits,
            ))
        })
        .await??
    };
    let listed = started.elapsed();
    let jobs: Arc<[Job]> = std::mem::take(&mut listing.jobs).into();
    let intake = cas.intake();
    let hashed = hash_all(
        pool,
        &upper_fd,
        Arc::clone(&jobs),
        limits.max_bytes,
        intake.clone(),
    )
    .await;
    let hash_time = started.elapsed() - listed;
    let files = jobs.len();
    let digested = check_hashed(
        hashed,
        limits.max_bytes,
        &std::mem::take(&mut listing.shown),
    )?;
    let taken = digested.iter().filter(|d| d.taken).count();
    // Digesting every Directory message of a large tree is hashing: on the CPU pool, not
    // on an async worker and not on the blocking pool, which has no bound on the cores
    // it takes.
    let (mut blobs, assembled) = {
        let (jobs, upper_fd) = (Arc::clone(&jobs), Arc::clone(&upper_fd));
        let mut entries = std::mem::take(result);
        pool.run(move || {
            let blobs = assemble(listing, &digested, &jobs, &upper_fd, &mut entries);
            (blobs, entries)
        })
        .await
        .map_err(io_at(&base))?
    };
    *result = assembled;
    let plain = intake.is_none();
    with_outputs(&mut blobs, logs, plain, result).await?;
    let finishing = Instant::now();
    finish(intake).await?;
    let take_ms = finishing.elapsed().as_millis() as u64;
    let count = blobs.len() + taken;
    // The CAS is asked about every digest at once (the transfer engine cuts it into
    // FindMissing calls and sends them together), and only the missing blobs are sent.
    let asked = Instant::now();
    let blobs = missing_only(cas, blobs).await;
    let find_missing = asked.elapsed();
    let sending = blobs.len();
    let stored = Instant::now();
    cas.put_records(blobs).await?;
    after_outputs(cas, logs, plain, result).await?;
    tracing::info!(
        files,
        blobs = count,
        taken,
        take_ms,
        missing = sending,
        list_ms = listed.as_millis() as u64,
        hash_ms = hash_time.as_millis() as u64,
        find_missing_ms = find_missing.as_millis() as u64,
        upload_ms = stored.elapsed().as_millis() as u64,
        "outputs phases"
    );
    Ok(())
}

/// The error of a system call on `path`, for `map_err`.
fn io_at(path: &Path) -> impl FnOnce(std::io::Error) -> TreeError + '_ {
    move |source| TreeError::Io {
        path: path.to_owned(),
        source,
    }
}

/// What the listing found for one output path.
enum Output {
    File {
        path: String,
    },
    Directory {
        path: String,
        /// The directory's tree, `trees[tree]` of the listing.
        tree: usize,
    },
    Symlink {
        path: String,
        target: String,
    },
}

/// A listed directory output: its Directory messages in pre-order, the root first, with
/// file digests unset; and for `dirs[i]` the index of its parent and of its node in the
/// parent's `directories` (unused for the root).
struct Skeleton {
    dirs: Vec<Directory>,
    parents: Vec<(usize, usize)>,
}

/// Where the digest of the file a [`Job`] hashes goes.
enum Place {
    /// The output `outputs[i]`, a file.
    Output(usize),
    /// File number `file` of `dirs[dir]` of the tree `trees[tree]`.
    Node {
        tree: usize,
        dir: usize,
        file: usize,
    },
}

/// The output paths, listed and not hashed.
struct Listing {
    outputs: Vec<Output>,
    trees: Vec<Skeleton>,
    jobs: Vec<Job>,
    places: Vec<Place>,
    /// The path shown for each job in errors; never opened.
    shown: Vec<PathBuf>,
}

/// Lists each of `paths` below the working directory `wd_parts` of `upper`, within
/// `limits`. One blocking task: no descriptor is kept but the directory being read.
fn list_outputs(
    upper: &Arc<OwnedFd>,
    wd_parts: &[String],
    base: &Path,
    paths: &[String],
    limits: OutputLimits,
) -> Result<Listing, TreeError> {
    let mut budget = Budget::new(limits);
    let mut listing = Listing {
        outputs: Vec::new(),
        trees: Vec::new(),
        jobs: Vec::new(),
        places: Vec::new(),
        shown: Vec::new(),
    };
    for path in paths {
        if path.is_empty() {
            return Err(TreeError::Invalid("an output path is empty".to_owned()));
        }
        check_relative("output path", path)?;
        let shown = base.join(path);
        let (dirs, name) = path.rsplit_once('/').unwrap_or(("", path));
        let mut parts: Vec<String> = wd_parts.to_vec();
        parts.extend(
            dirs.split('/')
                .filter(|part| !part.is_empty())
                .map(str::to_owned),
        );
        let dirs: Arc<[String]> = parts.into();
        // Absent, a file or a symlink on the way: the action did not create the output.
        let Some(parent) = descend(upper, &dirs).map_err(io_at(&shown))? else {
            continue;
        };
        match examine(&parent, OsStr::new(name)).map_err(io_at(&shown))? {
            None | Some(Kind::Other) => {}
            Some(Kind::File { size }) => {
                budget.entries(1, &shown)?;
                listing.places.push(Place::Output(listing.outputs.len()));
                listing.outputs.push(Output::File { path: path.clone() });
                listing.jobs.push(Job {
                    dirs: Arc::clone(&dirs),
                    name: name.to_owned(),
                    size,
                });
                listing.shown.push(shown);
            }
            Some(Kind::Directory) => {
                budget.entries(1, &shown)?;
                let mut parts = dirs.to_vec();
                parts.push(name.to_owned());
                let tree = listing.trees.len();
                let skeleton = walk(
                    &mut listing,
                    &mut budget,
                    tree,
                    &parent,
                    name,
                    &shown,
                    parts.into(),
                )?;
                listing.trees.push(skeleton);
                listing.outputs.push(Output::Directory {
                    path: path.clone(),
                    tree,
                });
            }
            Some(Kind::Symlink) => {
                budget.entries(1, &shown)?;
                let target = read_link(&parent, OsStr::new(name)).map_err(io_at(&shown))?;
                listing.outputs.push(Output::Symlink {
                    path: path.clone(),
                    target: utf8(target, &shown, "symlink target")?,
                });
            }
        }
    }
    Ok(listing)
}

/// A directory on the walk's stack: what is left of its entries and what is built of
/// its Directory.
struct Frame {
    /// The path shown in errors; never opened.
    shown: PathBuf,
    /// The directories from the upper directory down to it, by name, itself included.
    parts: Arc<[String]>,
    /// Checked when the walk comes back to it from a child.
    id: Identity,
    entries: std::vec::IntoIter<Entry>,
    directory: Directory,
    /// Its place in the output's `dirs`.
    slot: usize,
}

/// Opens and lists the directory `name` in `parent`, counting its entries against the
/// budget. `parts` names it from the upper directory down.
fn open_frame(
    budget: &mut Budget,
    parent: &OwnedFd,
    name: &OsStr,
    shown: PathBuf,
    parts: Arc<[String]>,
    slot: usize,
) -> Result<(OwnedFd, Frame), TreeError> {
    let max = budget.entries_left();
    let (fd, id, entries) = open_listed(parent, name, max).map_err(io_at(&shown))?;
    budget.entries(entries.len() as u64, &shown)?;
    let frame = Frame {
        shown,
        parts,
        id,
        entries: entries.into_iter(),
        directory: Directory::default(),
        slot,
    };
    Ok((fd, frame))
}

/// The directory `name` in `parent` (shown as `shown`, named `parts` from the upper
/// directory down) as Directory messages in pre-order, the directory first, with the
/// parent links the digests are filled in by. Each file found becomes a job of `listing`
/// whose digest goes to its node; symlinks are recorded, never followed.
///
/// Iterative, not recursive: the directories being read are a `Vec` of frames, so the
/// tree's depth costs heap, not thread stack, and the depth limit is checked before a
/// directory is opened.
#[allow(clippy::too_many_arguments)]
fn walk(
    listing: &mut Listing,
    budget: &mut Budget,
    tree: usize,
    parent: &OwnedFd,
    name: &str,
    shown: &Path,
    parts: Arc<[String]>,
) -> Result<Skeleton, TreeError> {
    let mut dirs = vec![Directory::default()];
    let mut parents = vec![(0, 0)];
    let (mut here, mut root) =
        open_frame(budget, parent, OsStr::new(name), shown.to_owned(), parts, 0)?;
    // The frames below the root; the last is the directory `here` holds open.
    let mut stack: Vec<Frame> = Vec::new();
    loop {
        let depth = stack.len();
        let top = stack.last_mut().unwrap_or(&mut root);
        let Some(Entry {
            name: raw,
            kind,
            link,
        }) = top.entries.next()
        else {
            // The top directory is done: the walk is, or goes back to its parent.
            let Some(done) = stack.pop() else {
                dirs[0] = root.directory;
                return Ok(Skeleton { dirs, parents });
            };
            let top = stack.last_mut().unwrap_or(&mut root);
            dirs[done.slot] = done.directory;
            here = back_to_parent(&here, top.id).map_err(io_at(&top.shown))?;
            continue;
        };
        let path = top.shown.join(OsStr::from_bytes(&raw));
        let os_name = PathBuf::from(OsStr::from_bytes(&raw));
        // UTF-8 names sort by code point as their bytes do, so `list`'s order is REAPI's.
        let name = utf8(raw, &path, "name")?;
        match kind {
            Kind::Other => {}
            Kind::File { size } => {
                listing.places.push(Place::Node {
                    tree,
                    dir: top.slot,
                    file: top.directory.files.len(),
                });
                top.directory.files.push(FileNode {
                    name: name.clone(),
                    ..FileNode::default()
                });
                listing.jobs.push(Job {
                    dirs: Arc::clone(&top.parts),
                    name,
                    size,
                });
                listing.shown.push(path);
            }
            Kind::Symlink => {
                top.directory.symlinks.push(SymlinkNode {
                    name,
                    target: utf8(link, &path, "symlink target")?,
                    ..SymlinkNode::default()
                });
            }
            Kind::Directory => {
                // The root is level 0 and `stack` holds the levels below it, so this
                // directory is level `stack.len() + 1`.
                let mut parts = top.parts.to_vec();
                parts.push(name.clone());
                let max_depth = budget.limits.max_depth;
                if depth >= max_depth {
                    return Err(limit(&path, Exceeded::Depth, max_depth as u64));
                }
                let slot = dirs.len();
                let (parent_slot, node) = (top.slot, top.directory.directories.len());
                top.directory
                    .directories
                    .push(DirectoryNode { name, digest: None });
                let (fd, frame) =
                    open_frame(budget, &here, os_name.as_os_str(), path, parts.into(), slot)?;
                // Held by the slot until the frame is done, so `dirs` stays in pre-order.
                dirs.push(Directory::default());
                parents.push((parent_slot, node));
                stack.push(frame);
                here = fd;
            }
        }
    }
}

/// The outcomes of hashing `jobs`, in job order, as digests: the first problem in job
/// order is the error. The files together are checked against `max` bytes here, in job
/// order, so which file is named does not depend on which thread was first.
fn check_hashed(
    hashed: Vec<Option<Outcome>>,
    max: u64,
    shown: &[PathBuf],
) -> Result<Vec<Digested>, TreeError> {
    let mut total = 0u64;
    let mut digested = Vec::with_capacity(hashed.len());
    for (outcome, shown) in hashed.into_iter().zip(shown) {
        let outcome = outcome.unwrap_or_else(|| Err(std::io::Error::other("was not hashed")));
        let Some(d) = outcome.map_err(|source| TreeError::Io {
            path: shown.clone(),
            source,
        })?
        else {
            return Err(limit(shown, Exceeded::Bytes, max));
        };
        total += d.digest.size_bytes;
        if total > max {
            return Err(limit(shown, Exceeded::Bytes, max));
        }
        digested.push(d);
    }
    Ok(digested)
}

/// The blobs to store, each distinct digest once, in the order they were found.
#[derive(Default)]
struct Blobs {
    records: Vec<Record>,
    found: HashSet<kbf_types::Digest>,
}

impl Blobs {
    /// Adds the blob `record` unless one with its digest was added already.
    fn add(&mut self, record: Record) {
        if self.found.insert(record.digest) {
            self.records.push(record);
        }
    }

    /// Adds the message `bytes` and returns its digest.
    fn message(&mut self, bytes: Vec<u8>) -> Digest {
        let record = Record::bytes(bytes);
        let digest = proto_digest(&record.digest);
        self.add(record);
        digest
    }
}

#[cfg(test)]
/// The threads [`assemble`] has run on, for the test that checks it is not an async
/// worker's. Thread ids are never reused, so tests that run side by side do not confuse
/// each other's.
pub(crate) static ASSEMBLED_ON: std::sync::Mutex<Vec<std::thread::ThreadId>> =
    std::sync::Mutex::new(Vec::new());

/// Builds the entries of `result` and the blobs to store from the listing and the digests
/// of its files: the files in job order (but those an intake took, which the intake
/// stores), then for each output directory its Directory messages (children in
/// pre-order, then the root) and its Tree.
fn assemble(
    listing: Listing,
    digested: &[Digested],
    jobs: &[Job],
    upper: &Arc<OwnedFd>,
    result: &mut ActionResult,
) -> Vec<Record> {
    let Listing {
        outputs,
        mut trees,
        places,
        ..
    } = listing;
    #[cfg(test)]
    ASSEMBLED_ON
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .push(std::thread::current().id());
    let mut blobs = Blobs::default();
    // The digest and executable bit of each output that is a file.
    let mut files: Vec<Option<(Digest, bool)>> = vec![None; outputs.len()];
    for ((job, place), d) in jobs.iter().zip(&places).zip(digested) {
        if !d.taken {
            blobs.add(Record {
                digest: d.digest,
                crc32c: d.crc32c,
                source: Source::File(FileSource::new(
                    Arc::clone(upper),
                    Arc::clone(&job.dirs),
                    job.name.clone(),
                    d.stamp,
                )),
            });
        }
        let proto = proto_digest(&d.digest);
        match place {
            Place::Output(i) => files[*i] = Some((proto, d.executable)),
            Place::Node { tree, dir, file } => {
                let node = &mut trees[*tree].dirs[*dir].files[*file];
                node.digest = Some(proto);
                node.is_executable = d.executable;
            }
        }
    }
    for (i, output) in outputs.into_iter().enumerate() {
        match output {
            Output::File { path } => {
                let (digest, is_executable) = files[i].take().unzip();
                result.output_files.push(OutputFile {
                    path,
                    digest,
                    is_executable: is_executable.unwrap_or(false),
                    ..OutputFile::default()
                });
            }
            Output::Symlink { path, target } => result.output_symlinks.push(OutputSymlink {
                path,
                target,
                ..OutputSymlink::default()
            }),
            Output::Directory { path, tree } => {
                let skeleton = std::mem::replace(
                    &mut trees[tree],
                    Skeleton {
                        dirs: Vec::new(),
                        parents: Vec::new(),
                    },
                );
                result
                    .output_directories
                    .push(directory_output(path, skeleton, &mut blobs));
            }
        }
    }
    blobs.records
}

/// The output directory `path` of the listed tree `skeleton`, whose files have digests:
/// every Directory is digested last first, so each child's digest is in its parent's node
/// when the parent is, and the blobs are added children first (in pre-order), then the
/// root and the Tree. Every Directory is stored, not only the Tree: the farm's action
/// cache refuses a result whose `root_directory_digest` it does not hold, and each digest
/// the Tree names can be fetched.
fn directory_output(path: String, skeleton: Skeleton, blobs: &mut Blobs) -> OutputDirectory {
    let Skeleton { mut dirs, parents } = skeleton;
    // Children come after their parents in `dirs`.
    let mut made: Vec<Record> = Vec::with_capacity(dirs.len());
    for slot in (0..dirs.len()).rev() {
        let record = Record::bytes(dirs[slot].encode_to_vec());
        if slot > 0 {
            let (parent, node) = parents[slot];
            dirs[parent].directories[node].digest = Some(proto_digest(&record.digest));
        }
        made.push(record);
    }
    // `made` is in reverse pre-order: the root is last.
    made.reverse();
    let root = dirs.remove(0);
    let root_record = made.remove(0);
    let root_digest = proto_digest(&root_record.digest);
    for record in made {
        blobs.add(record);
    }
    blobs.add(root_record);
    let tree = Tree {
        root: Some(root),
        children: dirs,
    };
    OutputDirectory {
        path,
        tree_digest: Some(blobs.message(tree.encode_to_vec())),
        is_topologically_sorted: false,
        root_directory_digest: Some(root_digest),
    }
}

#[cfg(test)]
#[path = "outputs_tests.rs"]
mod tests;
