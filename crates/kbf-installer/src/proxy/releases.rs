//! `kbf-proxy`'s own copies of the `kbf` it runs, and the [`RootOrders`] that goes with them.
//!
//! After a package replaces `/usr/bin/kbf` the old file is gone, and the proxy must still be
//! able to start, adopt and put back the release a child runs. So the proxy keeps a copy of
//! each `kbf` it runs, in a directory it owns, and starts children only from those copies.
//!
//! # Copies
//!
//! [`Copies`] keeps `<root>/<commit>/kbf` (0755) and `<root>/<commit>/VERIFIED`. A copy is
//! built where nobody looks and renamed into place, so a directory named for a commit is a
//! whole, probed copy and nothing else:
//!
//! 1. The source is opened once. The bytes are copied into `.partial-<pid>-<n>/kbf` and
//!    hashed as they go; the copy is fsynced.
//! 2. The copy (not the source) is run for `--version --json` ([`probe_version`]) and must say
//!    it is the release build of the commit asked for.
//! 3. Only then is `VERIFIED` written (`sha256 <hex>`), the directory fsynced, renamed to
//!    `<commit>` and the root fsynced.
//!
//! A commit that already has a directory is never rewritten: the same bytes are the same
//! release (nothing is copied or run again), other bytes are refused. A directory is trusted
//! only with its marker and when its binary still hashes to what the source hashes to.
//! A killed copy leaves a `.partial-*` entry, which [`Copies::new`] removes. A copy that is no
//! longer wanted is taken out by [`Copies::prune`], which is told which commits to keep.
//!
//! The words a [`Copies::ensure`] refusal starts with are `copy_failed`, `probe_failed`,
//! `commit_mismatch` and `differs`.
//!
//! # Orders
//!
//! [`LocalOrders`] is the swap's order port for a proxy that stages its own copies. It keeps
//! one status file in the shape of the root half's `status.json` (`installed_commit`,
//! `staged_commit` and `staged{}`), so every reader of that file reads it unchanged
//! ([`StatusFile`]). Nothing is spooled and no other process is asked: `stage` records the
//! facts it is given for a commit that has a verified copy, `confirm` makes the staged release
//! the installed one and `revert` drops it. The file is replaced by a rename, so a read sees
//! one whole version.
//!
//! # Wiring
//!
//! [`Local`] is what `kbf-installer proxy --kbf-binary PATH` turns on: the copies and the orders
//! in the proxy's state directory (`releases/` and `releases.json`, [`RELEASES`] and [`STATUS`])
//! for the kbf the package installed at `PATH`. A start that finds no installed release
//! recorded adopts that kbf ([`Local::adopt`]): it is copied, and the copy is recorded as the
//! installed release.

use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::migrator::probe_version;
use super::model::{Locator, PortError, StagedFacts};
use super::ports::{RootOrders, StatusFacts};
use super::root_orders::StatusFile;
use crate::CommitId;
use crate::layout::{RELEASE_BINARY, VERIFIED};
use crate::run::Runner;

/// The name a copy being made has in the root, up to its process id and number.
const PARTIAL: &str = ".partial-";

/// How many bytes are read from the source at a time.
const CHUNK: usize = 64 * 1024;

/// The directory of the copies, in the proxy's state directory.
pub const RELEASES: &str = "releases";

/// The status file of [`LocalOrders`], in the proxy's state directory.
pub const STATUS: &str = "releases.json";

/// Why a copy was not made. The text of each starts with its word.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CopyError {
    /// The source could not be read, or the copy could not be written, synced or put in place.
    #[error("copy_failed: {0}")]
    CopyFailed(String),
    /// The copy did not run, did not finish cleanly, or did not print a version object. A
    /// probe straight after the write can lose to another thread's fork (text file busy); the
    /// caller may try again.
    #[error("probe_failed: {0}")]
    ProbeFailed(String),
    /// The copy says it is another commit's build, or not a release build.
    #[error("commit_mismatch: {0}")]
    CommitMismatch(String),
    /// The commit has a copy already, and it is not these bytes (or has no marker for them).
    #[error("differs: {0}")]
    Differs(String),
}

/// The proxy's copies of `kbf`, one directory per commit under one root.
#[derive(Debug)]
pub struct Copies {
    root: PathBuf,
    next: AtomicU64,
}

impl Copies {
    /// The copies under `root`: made when missing (with the directories above it) and set to
    /// 0755 whatever the umask says, and stripped of every `.partial-*` entry a killed copy
    /// left. Nothing else in it is touched.
    ///
    /// Only one `Copies` may work in a root at a time (the sweep removes what another one is
    /// making); the proxy has one.
    ///
    /// # Errors
    /// The root cannot be made, listed, or swept.
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Copies> {
        let root = root.into();
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true).mode(0o755);
        builder.create(&root)?;
        fs::set_permissions(&root, Permissions::from_mode(0o755))?;
        for entry in fs::read_dir(&root)? {
            let entry = entry?;
            if !entry.file_name().to_string_lossy().starts_with(PARTIAL) {
                continue;
            }
            if entry.file_type()?.is_dir() {
                fs::remove_dir_all(entry.path())?;
            } else {
                fs::remove_file(entry.path())?;
            }
        }
        Ok(Copies {
            root,
            next: AtomicU64::new(0),
        })
    }

    /// The directory the copies are in.
    #[must_use]
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The binary of `commit`'s copy, whether or not it exists.
    #[must_use]
    pub fn program(&self, commit: &CommitId) -> PathBuf {
        self.dir(commit).join(RELEASE_BINARY)
    }

    /// Whether `commit` has a copy with its binary and its marker. This looks at the two
    /// names only; [`ensure`](Self::ensure) is what compares bytes.
    #[must_use]
    pub fn verified(&self, commit: &CommitId) -> bool {
        let is_file = |path: PathBuf| fs::metadata(path).is_ok_and(|meta| meta.is_file());
        is_file(self.program(commit)) && is_file(self.dir(commit).join(VERIFIED))
    }

    fn dir(&self, commit: &CommitId) -> PathBuf {
        self.root.join(commit.as_str())
    }

    /// The binary of `commit`'s copy, made from the file at `src` if the commit has none.
    ///
    /// A commit that has a directory is not copied again: its marker and binary must say the
    /// sha256 of `src`, and then nothing is run (the first probe stands). The copy is run as
    /// the caller, so a kbf that cannot be run by the proxy's own user is `probe_failed`.
    ///
    /// Two calls for one commit at the same time are not coordinated: the one that loses the
    /// rename reports `copy_failed`, and a call again finds the winner's copy.
    ///
    /// # Errors
    /// [`CopyError`]. No `.partial-*` entry is left; a failure of the last fsync leaves the
    /// finished copy in place.
    pub fn ensure(
        &self,
        runner: &dyn Runner,
        src: &Path,
        commit: &CommitId,
    ) -> Result<PathBuf, CopyError> {
        let dir = self.dir(commit);
        let source = ctx(File::open(src), "open", src)?;
        if fs::symlink_metadata(&dir).is_ok() {
            let sum = ctx(stream(source, None), "read", src)?;
            if !holds(&dir, &sum) {
                return Err(CopyError::Differs(dir.display().to_string()));
            }
            return Ok(self.program(commit));
        }

        let n = self.next.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let path = self.root.join(format!("{PARTIAL}{pid}-{n}"));
        let mut builder = fs::DirBuilder::new();
        builder.mode(0o755);
        ctx(builder.create(&path), "make", &path)?;
        // From here on the directory is ours: it goes unless it was renamed into place.
        let partial = Partial(path);
        let chmod = fs::set_permissions(&partial.0, Permissions::from_mode(0o755));
        ctx(chmod, "make", &partial.0)?;
        let binary = partial.0.join(RELEASE_BINARY);
        let sum = ctx(copy_into(source, &binary), "copy to", &binary)?;

        let probed = probe_version(runner, &binary);
        let version = probed.map_err(probe_failed)?;
        version
            .check(commit)
            .map_err(|fault| CopyError::CommitMismatch(fault.to_string()))?;

        let placed = publish(&partial.0, &dir, &sum, &self.root);
        ctx(placed, "install", &dir)?;
        Ok(self.program(commit))
    }

    /// Remove the copy of every commit that is not in `keep`; the commits removed. Entries
    /// that are not a commit's directory (a `.partial-*` entry, a stray file or link, even one
    /// named like a commit) are left alone.
    ///
    /// A copy is first renamed to a `.partial-gone-*` name, so a name that is a commit is never
    /// a copy with some of its files gone (a later [`ensure`](Self::ensure) of the commit would
    /// refuse that as `differs`); a removal that is cut short leaves an entry that
    /// [`Copies::new`] sweeps.
    ///
    /// # Errors
    /// The root cannot be listed, or a copy cannot be renamed or removed; the copies before it
    /// are gone.
    pub fn prune(&self, keep: &[CommitId]) -> io::Result<Vec<CommitId>> {
        let pid = std::process::id();
        let mut removed = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(commit) = name.to_str().and_then(|name| CommitId::new(name).ok()) else {
                continue;
            };
            let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
            if !is_dir || keep.contains(&commit) {
                continue;
            }
            let n = self.next.fetch_add(1, Ordering::Relaxed);
            let gone = self.root.join(format!("{PARTIAL}gone-{pid}-{n}"));
            fs::rename(entry.path(), &gone)?;
            fs::remove_dir_all(&gone)?;
            removed.push(commit);
        }
        Ok(removed)
    }
}

/// A directory being made: removed when dropped. After the rename it is no longer there to
/// remove.
struct Partial(PathBuf);

impl Drop for Partial {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// `result`, with what was being done to `path` said in the error.
fn ctx<T>(result: io::Result<T>, what: &str, path: &Path) -> Result<T, CopyError> {
    result.map_err(|e| {
        let why = format!("cannot {what} {}: {e}", path.display());
        CopyError::CopyFailed(why)
    })
}

/// A failed probe, without the migrator's own word: the words here are this module's.
fn probe_failed(e: PortError) -> CopyError {
    let text = e.0.as_str();
    let why = text.split_once(": ").map_or(text, |(_, rest)| rest);
    CopyError::ProbeFailed(why.to_owned())
}

/// What a marker says of a binary that hashes to `sum`.
fn marker_text(sum: &str) -> String {
    format!("sha256 {sum}\n")
}

/// Whether the directory `dir` is the copy of the bytes that hash to `sum`: its marker says
/// so and its binary hashes to it.
fn holds(dir: &Path, sum: &str) -> bool {
    let marker = fs::read_to_string(dir.join(VERIFIED)).ok();
    let copy = hash_file(&dir.join(RELEASE_BINARY)).ok();
    let want = marker_text(sum);
    marker.as_deref() == Some(want.as_str()) && copy.as_deref() == Some(sum)
}

fn hash_file(path: &Path) -> io::Result<String> {
    stream(File::open(path)?, None)
}

/// Read `source` to its end, writing what is read to `sink` when there is one; the sha256 of
/// it, in hex.
fn stream(mut source: File, mut sink: Option<&mut File>) -> io::Result<String> {
    let mut hash = Sha256::new();
    let mut buf = vec![0; CHUNK];
    loop {
        let n = source.read(&mut buf)?;
        if n == 0 {
            return Ok(hex::encode(hash.finalize()));
        }
        hash.update(&buf[..n]);
        if let Some(out) = &mut sink {
            out.write_all(&buf[..n])?;
        }
    }
}

/// Copy `source` to the new file `dest` (0755, whatever the umask says), fsynced and closed;
/// the sha256 of what was copied.
fn copy_into(source: File, dest: &Path) -> io::Result<String> {
    let mut out = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o755)
        .open(dest)?;
    let sum = stream(source, Some(&mut out))?;
    out.set_permissions(Permissions::from_mode(0o755))?;
    out.sync_all()?;
    Ok(sum)
}

/// Write the marker into `partial`, then move it to `dir` and make the move durable.
fn publish(partial: &Path, dir: &Path, sum: &str, root: &Path) -> io::Result<()> {
    put(&partial.join(VERIFIED), marker_text(sum).as_bytes(), 0o644)?;
    sync_dir(partial)?;
    fs::rename(partial, dir)?;
    sync_dir(root)
}

/// A file of exactly `bytes` and `mode` at `path`, fsynced.
fn put(path: &Path, bytes: &[u8], mode: u32) -> io::Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(path)?;
    file.write_all(bytes)?;
    file.set_permissions(Permissions::from_mode(mode))?;
    file.sync_all()
}

fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

/// The [`RootOrders`] of a proxy that stages its own copies: a status file of its own, written
/// by a rename, in the shape the root half's `status.json` has.
///
/// One lock covers every read-modify-write, so the proxy's threads (the swap and the one that
/// stages) cannot lose each other's change. The file's directory must exist.
#[derive(Debug)]
pub struct LocalOrders {
    file: PathBuf,
    copies: Arc<Copies>,
    lock: Mutex<()>,
}

/// What the file holds; the fields of `status.json` that the swap reads.
#[derive(Serialize)]
struct Record {
    installed_commit: Option<CommitId>,
    staged_commit: Option<CommitId>,
    staged: Option<Wire>,
}

/// `staged{}`: the facts without the commit, which is `staged_commit` beside it.
#[derive(Serialize)]
struct Wire {
    protocol: u32,
    min_protocol: u32,
    state_schema: u32,
    profile: Option<String>,
}

impl Wire {
    fn of(facts: &StagedFacts) -> Wire {
        Wire {
            protocol: facts.protocol,
            min_protocol: facts.min_protocol,
            state_schema: facts.state_schema,
            profile: facts.profile.clone(),
        }
    }
}

impl LocalOrders {
    /// Orders kept in the status file at `file` (an absolute path, in a directory that exists),
    /// for the releases `copies` holds. A file that is not there reads as nothing staged and
    /// nothing installed.
    #[must_use]
    pub fn new(file: impl Into<PathBuf>, copies: Arc<Copies>) -> LocalOrders {
        LocalOrders {
            file: file.into(),
            copies,
            lock: Mutex::new(()),
        }
    }

    /// Record `facts` as the staged release, keeping the installed one.
    ///
    /// # Errors
    /// The commit has no verified copy, the file cannot be read, or it cannot be written.
    pub fn stage(&self, facts: &StagedFacts) -> Result<(), PortError> {
        self.require_copy(&facts.commit)?;
        let _held = self.held();
        let installed = StatusFacts::installed(&self.status())?;
        self.write(installed, Some(facts))
    }

    /// Record `commit` as the installed release when none is recorded, keeping what is staged;
    /// the installed release afterwards, which is the one that was recorded if there was one.
    /// A start that finds nothing installed is the only caller.
    ///
    /// # Errors
    /// The commit has no verified copy, the file cannot be read, or it cannot be written.
    pub fn adopt(&self, commit: &CommitId) -> Result<CommitId, PortError> {
        self.require_copy(commit)?;
        let _held = self.held();
        let status = self.status();
        if let Some(recorded) = StatusFacts::installed(&status)? {
            return Ok(recorded);
        }
        let staged = StatusFacts::staged(&status)?;
        self.write(Some(commit.clone()), staged.as_ref())?;
        Ok(commit.clone())
    }

    fn require_copy(&self, commit: &CommitId) -> Result<(), PortError> {
        if self.copies.verified(commit) {
            return Ok(());
        }
        Err(PortError(format!("no verified copy of {commit}")))
    }

    fn status(&self) -> StatusFile {
        StatusFile::new(&self.file)
    }

    fn held(&self) -> MutexGuard<'_, ()> {
        self.lock.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Replace the file with a record of `installed` and `staged`.
    fn write(
        &self,
        installed: Option<CommitId>,
        staged: Option<&StagedFacts>,
    ) -> Result<(), PortError> {
        let record = Record {
            installed_commit: installed,
            staged_commit: staged.map(|facts| facts.commit.clone()),
            staged: staged.map(Wire::of),
        };
        let bytes = serde_json::to_vec(&record).expect("a record serializes");
        replace(&self.file, &bytes)
    }
}

/// `bytes` as the whole content of `path` (0644): written aside, fsynced, renamed over it, and
/// the directory fsynced.
fn replace(path: &Path, bytes: &[u8]) -> Result<(), PortError> {
    let dir = path.parent().expect("a status file has a directory");
    let temp = path.with_extension("json.new");
    let wrote = put(&temp, bytes, 0o644)
        .and_then(|()| fs::rename(&temp, path))
        .and_then(|()| sync_dir(dir));
    wrote.map_err(|e| {
        let why = format!("cannot write {}: {e}", path.display());
        PortError(why)
    })
}

impl RootOrders for LocalOrders {
    /// Always refused: a frontend is staged by [`LocalOrders::stage`], and nothing is spooled.
    fn frontend_update(&self, _locator: &Locator) -> Result<(), PortError> {
        Err(PortError("frontend_update is not an order here".to_owned()))
    }

    fn staged(&self) -> Result<Option<StagedFacts>, PortError> {
        StatusFacts::staged(&self.status())
    }

    fn installed(&self) -> Result<Option<CommitId>, PortError> {
        StatusFacts::installed(&self.status())
    }

    /// The staged release becomes the installed one and nothing stays staged. Again for a
    /// release that is installed already is `Ok` and changes nothing: a restart between a
    /// confirm and the record of it sends it twice.
    fn confirm(&self, commit: &CommitId) -> Result<(), PortError> {
        let _held = self.held();
        let status = self.status();
        let staged = StatusFacts::staged(&status)?;
        if staged.is_some_and(|facts| facts.commit == *commit) {
            return self.write(Some(commit.clone()), None);
        }
        if StatusFacts::installed(&status)?.as_ref() == Some(commit) {
            return Ok(());
        }
        Err(PortError(format!("{commit} is not the staged release")))
    }

    /// Nothing is staged afterwards, if `commit` was. `Ok` when it was not (the swap reverts
    /// a release that never got staged), and another release's staging is left alone.
    fn revert(&self, commit: &CommitId) -> Result<(), PortError> {
        let _held = self.held();
        let status = self.status();
        let staged = StatusFacts::staged(&status)?;
        if staged.is_some_and(|facts| facts.commit == *commit) {
            return self.write(StatusFacts::installed(&status)?, None);
        }
        Ok(())
    }
}

/// What `proxy --kbf-binary` turns on: the proxy's copies of `kbf` and its orders over them,
/// both in its state directory, and the kbf the package installed. One of these is made at
/// start and shared (a clone shares the copies and the one lock of the orders).
#[derive(Clone, Debug)]
pub struct Local {
    /// The kbf the package installs (`/usr/bin/kbf`): it is copied, and a start that finds no
    /// installed release recorded adopts it ([`Local::adopt`]).
    pub binary: PathBuf,
    pub copies: Arc<Copies>,
    pub orders: Arc<LocalOrders>,
}

impl Local {
    /// The copies and orders in `state_dir` ([`RELEASES`] and [`STATUS`]) for the kbf at
    /// `binary`.
    ///
    /// # Errors
    /// The copies' root cannot be made, listed, or swept (see [`Copies::new`]).
    pub fn open(binary: PathBuf, state_dir: &Path) -> io::Result<Local> {
        let copies = Arc::new(Copies::new(state_dir.join(RELEASES))?);
        let orders = LocalOrders::new(state_dir.join(STATUS), Arc::clone(&copies));
        Ok(Local {
            binary,
            copies,
            orders: Arc::new(orders),
        })
    }

    /// The release a start runs when none is recorded as installed: the kbf at
    /// [`binary`](Self::binary), copied (it names itself with `commit_full`, and the copy must
    /// name the same 12-digit commit) and recorded as installed. A release that is recorded
    /// already is left as it is and returned: the caller asks only when it found none, and a
    /// record made since is not undone.
    ///
    /// # Errors
    /// The binary does not run, does not print `commit_full`, cannot be copied, or the record
    /// cannot be written; the text says what could not be done to the binary.
    pub fn adopt(&self, runner: &dyn Runner) -> Result<CommitId, PortError> {
        let at = self.binary.display();
        let refuse = |why: String| PortError(format!("cannot adopt {at}: {why}"));
        let probed = probe_version(runner, &self.binary);
        let version = probed.map_err(|e| refuse(probe_failed(e).to_string()))?;
        let absent = || refuse("it prints no commit_full".to_owned());
        let full = version.commit_full.as_deref().ok_or_else(absent)?;
        let commit = CommitId::new(full).map_err(|e| refuse(e.to_string()))?;
        let ensured = self.copies.ensure(runner, &self.binary, &commit);
        ensured.map_err(|e| refuse(e.to_string()))?;
        self.orders.adopt(&commit)
    }
}
