//! The release verbs: `install`, `stage`, `rollback` and `gc`, on the layout of
//! [`super::layout`].
//!
//! Each reads the deploy state, plans its steps through a [`Planner`] and prints them
//! (`DRY-RUN: <step>`) unless `--apply` is given. With `--apply` it takes the deploy lock
//! first ([`Locked`]) and holds it until the state is written, so two runs cannot both
//! move the links; a dry run takes no lock and writes nothing. Each refuses while a
//! commit-confirm deploy is pending (the state's `pending_confirm`): that deploy owns the
//! links until it is confirmed or rolled back.
//!
//! - `install` copies a release into `releases/<commit>/` (root's, 0755, so the user a
//!   service runs as cannot replace its own binary; units 0644), refuses a release
//!   directory that is there with other content, checks that each copied binary's
//!   sha256 is the source's, then points `previous` at the old current release and
//!   `current` at the new one and writes the state. Only the files the source names are
//!   compared: a file the release directory has beyond them is not noticed. A release
//!   holds the binaries its source directory has: `kbf` (all a macOS release has),
//!   `kbf-daemon` (a Linux one may add it) or both.
//!   A new release is made in `releases/.stage-<commit>.<rand>/`,
//!   its files and directories synced, then renamed to `releases/<commit>/` and the parent
//!   synced (S19): the final name never holds a half-written release, and the state is
//!   written only after all of it.
//! - `stage` does the copy and the checks of `install` and no more: no link moves, and
//!   the state's `staged` names the release, which `gc` then keeps. It is what a
//!   release that was verified elsewhere ([`crate::stage`]) goes through, and it
//!   proceeds while a deploy is pending, since it touches nothing that deploy owns.
//! - `rollback` points `current` at the previous release and removes `previous` (the
//!   state keeps no release before it); it refuses when there is no previous release or
//!   its binary is gone.
//! - `gc` removes release directories, never the current, previous or staged one, nor
//!   one the `current` or `previous` link points to.
//!
//! None of them restarts a service: restarting the units is the operator's
//! (`systemctl restart`; see the SEAM comment in `switch`). An apply ends by saying so.

use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use kbf_host::exec::{Entry, Exec, Kind, Meta};
use kbf_host::path::HostPath;
use kbf_host::plan::{HostError, Mode, Plan, Planner, RunAs, execute};
use kbf_host::step::{Owner, Perms};
use sha2::{Digest, Sha256};

use super::CommitId;
use super::layout::{self, Layout, Platform};
use super::retain::{self, RetainError, Retained};
use super::state::{self, Locked, State, StateError};

/// Who owns a release: root, so only root can replace what a service runs.
pub const ROOT: Owner = Owner { uid: 0, gid: 0 };
/// A release directory and the directories above it: root writes, everyone reads.
pub const DIR: Perms = Perms::new(0o755);
/// The release binary: root writes, everyone runs.
pub const BINARY: Perms = Perms::new(0o755);
/// A unit file in a release.
pub const UNIT: Perms = Perms::new(0o644);

/// Why a release verb refused or failed.
#[derive(Debug, thiserror::Error)]
pub enum ReleaseError {
    #[error(transparent)]
    State(#[from] StateError),
    #[error(transparent)]
    Host(#[from] HostError),
    #[error(transparent)]
    Retain(#[from] RetainError),
    #[error("the metadata log: {0}")]
    MetaLog(#[from] crate::meta_log::MetaLogError),
    #[error("cannot read {path}: {source}")]
    Read { path: PathBuf, source: io::Error },
    #[error("{0:?} is not a UTF-8 name")]
    NotUtf8(PathBuf),
    #[error("a commit-confirm deploy of {0} is pending: confirm it or let it roll back first")]
    Pending(CommitId),
    #[error("{0} is there and is not a directory")]
    NotADirectory(HostPath),
    #[error("{path} differs from the release being installed: a release is never replaced")]
    Differs { path: HostPath },
    #[error("the copy at {path} has sha256 {got}; the source has {want}")]
    Copy {
        path: HostPath,
        want: String,
        got: String,
    },
    #[error("no previous release to roll back to")]
    NoPrevious,
    #[error("the previous release {0} is gone (no binary at its directory)")]
    PreviousGone(CommitId),
    #[error("release {0} is not installed (no binary at its directory): stage it first")]
    NotInstalled(CommitId),
    #[error("the pending swap of {commit} was made by {pending}, not by order {got:?}")]
    OtherOrder {
        commit: CommitId,
        pending: String,
        got: String,
    },
    #[error(
        "release {0} is current already: the first release is the only one record-release installs, use update"
    )]
    NotFirst(CommitId),
    #[error("no swap is pending, and {0} is not the current release")]
    NotPending(CommitId),
    #[error("{0} is staged and has not been confirmed or dropped: one frontend update at a time")]
    FrontendBusy(CommitId),
    #[error("{0} is the installed release already")]
    AlreadyInstalled(CommitId),
    #[error("{commit} is not the staged release ({})", staged.as_ref().map_or("none staged".to_owned(), ToString::to_string))]
    NotStaged {
        commit: CommitId,
        staged: Option<CommitId>,
    },
    #[error("a confirm window of {0} s is outside the {min} to {max} s the host allows", min = crate::activate::WINDOW_MIN_SECS, max = crate::activate::WINDOW_MAX_SECS)]
    Window(u64),
}

/// The error closure for a read of `path`.
fn read_failed(path: &Path) -> impl FnOnce(io::Error) -> ReleaseError + '_ {
    move |source| ReleaseError::Read {
        path: path.to_owned(),
        source,
    }
}

/// What `install --from <dir>` copies: each of `<dir>/kbf` and `<dir>/kbf-daemon` that
/// exists (at least one must), and each file of `<dir>/units/` (sorted by name) when that
/// directory exists.
#[derive(Debug)]
pub struct Source {
    /// The binaries by name, in the order of [`layout::RELEASE_BINARIES`].
    pub binaries: Vec<(&'static str, Vec<u8>)>,
    pub units: Vec<(String, Vec<u8>)>,
}

impl Source {
    pub fn read(from: &Path) -> Result<Source, ReleaseError> {
        let mut binaries = Vec::new();
        let mut missing = None;
        for name in layout::RELEASE_BINARIES {
            let bin = from.join(name);
            match fs::read(&bin) {
                Ok(bytes) => binaries.push((name, bytes)),
                Err(e) if e.kind() == io::ErrorKind::NotFound => {
                    missing.get_or_insert((bin, e));
                }
                Err(e) => return Err(read_failed(&bin)(e)),
            }
        }
        // With none, the error is the first one's: the source has no `kbf`.
        if let Some((bin, e)) = missing.filter(|_| binaries.is_empty()) {
            return Err(read_failed(&bin)(e));
        }
        let dir = from.join(layout::RELEASE_UNITS);
        let items = match fs::read_dir(&dir) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
            listed => listed
                .and_then(Iterator::collect::<io::Result<Vec<_>>>)
                .map_err(read_failed(&dir))?,
        };
        let mut units = Vec::new();
        for item in items {
            let file = item.path();
            let name = utf8(&item.file_name(), &file)?.to_owned();
            units.push((name, fs::read(&file).map_err(read_failed(&file))?));
        }
        units.sort();
        Ok(Source { binaries, units })
    }
}

fn utf8<'a>(name: &'a OsStr, full: &Path) -> Result<&'a str, ReleaseError> {
    name.to_str()
        .ok_or_else(|| ReleaseError::NotUtf8(full.to_owned()))
}

/// `dir/name`, for a name read from a directory (never empty, `.`, `..` or with a `/`).
fn child(dir: &HostPath, name: &str) -> HostPath {
    HostPath::new(&format!("{dir}/{name}")).expect("a directory entry's name is one component")
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn inspect<E: Exec>(exec: &E, path: &HostPath) -> Result<Option<Entry>, ReleaseError> {
    let fail = |source| HostError::Inspect {
        path: path.clone(),
        source,
    };
    Ok(exec.inspect(path).map_err(fail)?)
}

/// The lock (only with `--apply`) and the state, refused while a deploy is pending.
fn begin(host: &Layout, mode: Mode) -> Result<(Option<Locked>, State), ReleaseError> {
    let lock = match mode {
        Mode::Apply => Some(Locked::acquire(host)?),
        Mode::DryRun => None,
    };
    let state = match &lock {
        Some(lock) => lock.read()?,
        None => state::read_unlocked(host)?,
    };
    let state = state.unwrap_or_default();
    if let Some(p) = &state.pending_confirm {
        return Err(ReleaseError::Pending(p.commit.clone()));
    }
    Ok((lock, state))
}

/// `none`, or the commit.
fn or_none(commit: &Option<CommitId>) -> String {
    commit
        .as_ref()
        .map_or("none".to_owned(), ToString::to_string)
}

/// Point `previous` at `previous` (or remove it), then `current` at `current`: until
/// `current` moves, both name whole releases. Then, with the lock (an apply), write the
/// state with those two and say what is left to do by hand.
fn switch<E: Exec>(
    exec: &mut E,
    host: &Layout,
    lock: Option<Locked>,
    was: &State,
    (current, previous): (CommitId, Option<CommitId>),
    out: &mut dyn Write,
) -> Result<(), ReleaseError> {
    let mut planner = Planner::new(&*exec);
    match &previous {
        Some(c) => planner.symlink(host.previous(), layout::release_link(c))?,
        None => planner.absent(host.previous())?,
    };
    planner.symlink(host.current(), layout::release_link(&current))?;
    let plan = planner.finish();
    let _ = writeln!(
        out,
        "links: current {} -> {current}, previous {} -> {}",
        or_none(&was.current),
        or_none(&was.previous),
        or_none(&previous)
    );
    let mode = if lock.is_some() {
        Mode::Apply
    } else {
        Mode::DryRun
    };
    // The state is the truth and goes first: a crash before the links move leaves a state the
    // next run's reconcile carries out, never links the state does not know (S23).
    if let Some(lock) = &lock {
        // A release that is current is not staged (as `activate` and `first` keep it).
        let next = State {
            current: Some(current.clone()),
            previous: previous.clone(),
            staged: was.staged.clone().filter(|c| *c != current),
            ..was.clone()
        };
        lock.write(&next)?;
        let _ = writeln!(out, "state: written to {}", host.state_file());
    }
    execute(&plan, RunAs::Root, mode, exec, &mut &mut *out)?;
    if lock.is_some() {
        // SEAM(unit restart): no verb here restarts a service; the units are the
        // packaged files (packaging/linux/units/), which name the release directory's
        // binary. When a verb restarts them, it goes here, after the switch.
        let restart = match host.platform() {
            Platform::Linux => "systemctl restart <unit>",
            Platform::MacOs => "launchctl kickstart -k system/<label>",
        };
        let binary = layout::RELEASE_BINARY;
        let _ = writeln!(
            out,
            "next, by hand: restart the units that run {}/{binary} ({restart}), \
             then check `kbf-deploy host status`",
            host.current(),
        );
    }
    Ok(())
}

/// What `install` was asked for.
#[derive(Debug)]
pub struct Install {
    pub commit: CommitId,
    pub from: PathBuf,
}

/// `host install`: copy, check, switch. `owner` is [`ROOT`] on a host (a scratch test
/// can chown only to its own user).
pub fn install<E: Exec>(
    exec: &mut E,
    host: &Layout,
    owner: Owner,
    a: &Install,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<(), ReleaseError> {
    let source = Source::read(&a.from)?;
    let (lock, state) = begin(host, mode)?;
    if lock.is_some() {
        sweep_stages(host)?;
    }
    copy_release(exec, host, owner, &a.commit, &source, mode, out)?;
    // Installing the current release again keeps its previous one.
    let previous = if state.current.as_ref() == Some(&a.commit) {
        state.previous.clone()
    } else {
        state.current.clone()
    };
    switch(exec, host, lock, &state, (a.commit.clone(), previous), out)
}

/// `host stage`: copy and check as `install` does, move no link, and record the release
/// as the state's `staged` one (with `Mode::Apply`; a dry run prints the plan).
pub fn stage<E: Exec>(
    exec: &mut E,
    host: &Layout,
    owner: Owner,
    a: &Install,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<(), ReleaseError> {
    let source = Source::read(&a.from)?;
    // Held through the copy, so a `gc` cannot remove the release between the two. The state
    // is read before anything is published: one that is unusable stops the verb with the
    // releases directory as it was (S21).
    let lock = match mode {
        Mode::Apply => Some(Locked::acquire(host)?),
        Mode::DryRun => None,
    };
    let state = match &lock {
        Some(lock) => {
            let state = lock.read()?.unwrap_or_default();
            sweep_stages(host)?;
            Some(state)
        }
        None => None,
    };
    copy_release(exec, host, owner, &a.commit, &source, mode, out)?;
    if let (Some(lock), Some(state)) = (lock, state) {
        lock.write(&State {
            staged: Some(a.commit.clone()),
            ..state
        })?;
        let _ = writeln!(out, "state: {} staged", a.commit);
    }
    Ok(())
}

/// The release made on the host: under a stage name and renamed into place when it is new
/// (S19), in place when `releases/<commit>/` is already there (then nothing is created, only
/// the modes and owner are set again). A dry run prints the plan against the final names.
/// Afterwards each binary is read back from the host and compared with the source.
fn copy_release<E: Exec>(
    exec: &mut E,
    host: &Layout,
    owner: Owner,
    commit: &CommitId,
    source: &Source,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<(), ReleaseError> {
    let final_dir = host.release_dir(commit);
    let existed = inspect(exec, &final_dir)?.is_some();
    // A new release is made apart and renamed; the plan of a dry run names the final paths.
    let staged = (!existed && mode == Mode::Apply).then(|| host.stage_dir(commit, &unique()));
    let target = staged.clone().unwrap_or_else(|| final_dir.clone());
    let copy = plan_copy(exec, host, owner, commit, &target, source)?;
    // The first binary is named in the line, as it was when a release held only `kbf`;
    // each other one gets a line of its own.
    let ((_, first), others) = source
        .binaries
        .split_first()
        .expect("a source holds at least one binary");
    let _ = writeln!(
        out,
        "release {commit}: {final_dir} ({} bytes, sha256 {}), {} unit(s)",
        first.len(),
        sha256(first),
        source.units.len()
    );
    for (name, bytes) in others {
        let _ = writeln!(
            out,
            "also {name} ({} bytes, sha256 {})",
            bytes.len(),
            sha256(bytes)
        );
    }
    let copied = execute(&copy, RunAs::Root, mode, exec, &mut &mut *out)
        .map_err(ReleaseError::from)
        .and_then(|()| {
            if mode == Mode::Apply {
                for (name, bytes) in &source.binaries {
                    let bin = target_file(&target, name);
                    verify(inspect(exec, &bin)?, bin, bytes)?;
                }
            }
            Ok(())
        })
        .and_then(|()| match &staged {
            Some(stage) => publish(host, stage, &final_dir),
            None => Ok(()),
        });
    // A dry run changes nothing, so a failed copy is always an apply. What was made apart
    // goes; a release that was there already is left as it was.
    if copied.is_err()
        && let Some(stage) = &staged
    {
        // Best effort: the error returned is the copy's.
        let _ = remove_partial(exec, host, stage);
    }
    copied
}

/// `dir/name` for a binary or `units`, in the release being made at `dir`.
fn target_file(dir: &HostPath, name: &str) -> HostPath {
    child(dir, name)
}

/// A name no other run of this program, and no other call in this run, is using: the pid, the
/// clock and a counter, in hex.
fn unique() -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{:x}{:x}{:x}", std::process::id(), nanos, n)
}

/// Remove the `.stage-` directories of `releases/` that a run killed between making a release
/// and renaming it left behind. Called with the lock held, which is what keeps a run from
/// making one meanwhile (see [`sweep_dir`], whose caller must exclude the makers).
fn sweep_stages(host: &Layout) -> Result<(), ReleaseError> {
    let dir = host.join(&host.releases());
    sweep_dir(&dir, |name| name.starts_with(layout::STAGE_PREFIX))
}

/// Remove the entries of `dir` whose names `doomed` picks, and nothing else: a directory
/// with all it holds, any other entry by itself. The type of an entry is its own, so a link
/// is removed and never followed, here or inside a directory removed. A `dir` that is not
/// there has none. The caller must see that nothing is making an entry that `doomed` picks:
/// for `releases/` the lock does it, for `work/` only where no stage of this process runs
/// (see [`crate::stage::sweep_work`]).
pub(crate) fn sweep_dir(dir: &Path, doomed: fn(&str) -> bool) -> Result<(), ReleaseError> {
    let items = match fs::read_dir(dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        listed => listed.map_err(read_failed(dir))?,
    };
    for item in items {
        let item = item.map_err(read_failed(dir))?;
        let path = item.path();
        let name = item.file_name().to_string_lossy().into_owned();
        if !doomed(name.as_str()) {
            continue;
        }
        let gone = if item.file_type().map_err(read_failed(&path))?.is_dir() {
            fs::remove_dir_all(&path)
        } else {
            fs::remove_file(&path)
        };
        gone.map_err(read_failed(&path))?;
    }
    Ok(())
}

/// Sync every file and directory of the release made at `stage`, rename it to `final_dir`
/// and sync the directory that holds the name (S19): after this returns the release is whole
/// on disk under its name, and before it nothing is under that name.
fn publish(host: &Layout, stage: &HostPath, final_dir: &HostPath) -> Result<(), ReleaseError> {
    let (from, to) = (host.join(stage), host.join(final_dir));
    sync_tree(&from).map_err(read_failed(&from))?;
    fs::rename(&from, &to).map_err(read_failed(&to))?;
    let parent = to.parent().expect("a release is in releases/");
    fs::File::open(parent)
        .and_then(|d| d.sync_all())
        .map_err(read_failed(parent))
}

/// `fsync` every file under `dir`, then every directory in it, deepest first, then `dir`.
fn sync_tree(dir: &Path) -> io::Result<()> {
    for item in fs::read_dir(dir)? {
        let item = item?;
        let path = item.path();
        if item.file_type()?.is_dir() {
            sync_tree(&path)?;
        } else {
            fs::File::open(&path)?.sync_all()?;
        }
    }
    fs::File::open(dir)?.sync_all()
}

/// Remove the release directory a failed copy made (a full disk is the usual cause), so
/// that a retry finds nothing there. A directory left half written would be refused for
/// ever: it is there with other content, and a release is never replaced.
fn remove_partial<E: Exec>(
    exec: &mut E,
    host: &Layout,
    dir: &HostPath,
) -> Result<(), ReleaseError> {
    let mut paths = Vec::new();
    contents(host, dir, &mut paths)?;
    paths.push(dir.clone());
    let mut planner = Planner::new(&*exec);
    for p in paths {
        planner.absent(p)?;
    }
    execute(
        &planner.finish(),
        RunAs::Root,
        Mode::Apply,
        exec,
        &mut io::sink(),
    )?;
    Ok(())
}

/// The directories and files of the release at `dir` (its final name, or the stage name),
/// root's; a release directory already there must hold the same files (its modes and owner
/// are then set again, nothing else). The directories above it are made when the host has
/// none, and left as they are when it has.
fn plan_copy<E: Exec>(
    exec: &E,
    host: &Layout,
    owner: Owner,
    commit: &CommitId,
    dir: &HostPath,
    source: &Source,
) -> Result<Plan, ReleaseError> {
    let units = child(dir, layout::RELEASE_UNITS);
    let mut files = Vec::new();
    for (name, bytes) in &source.binaries {
        files.push((child(dir, name), BINARY, bytes));
    }
    for (name, bytes) in &source.units {
        files.push((child(&units, name), UNIT, bytes));
    }
    let final_dir = host.release_dir(commit);
    if *dir == final_dir {
        match inspect(exec, dir)? {
            None => {}
            Some(Entry::Dir { .. }) => {
                for (path, _, want) in &files {
                    let same = match inspect(exec, path)? {
                        Some(Entry::File { bytes, .. }) => bytes == **want,
                        _ => false,
                    };
                    if !same {
                        return Err(ReleaseError::Differs { path: path.clone() });
                    }
                }
            }
            Some(_) => return Err(ReleaseError::NotADirectory(dir.clone())),
        }
    }
    let mut planner = Planner::new(exec);
    let mut dirs = Vec::new();
    // A fresh macOS has no `/usr/local`. An existing one is not touched: its owner and
    // mode are the host's, and the trust check (crate::trust) refuses one others write to.
    if let Some(usr_local) = missing_usr_local(exec, host)? {
        dirs.push(usr_local);
    }
    // The state directory and the install root are the package's or the host's: made only when
    // missing, never chmod-ed (a 0700 `/var/db/kbf-installer` stays 0700). `releases/` and the
    // release are this program's own.
    for d in [host.state_dir(), host.install_root()] {
        if inspect(exec, &d)?.is_none() && !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    dirs.extend([host.releases(), dir.clone()]);
    if !source.units.is_empty() {
        dirs.push(units);
    }
    for d in dirs {
        planner.dir(d, DIR, Some(owner))?;
    }
    for (path, perms, bytes) in files {
        planner.file(path, perms, Some(owner), bytes.clone())?;
    }
    Ok(planner.finish())
}

/// `/usr/local` when the installer tree is under it and the host has none.
fn missing_usr_local<E: Exec>(exec: &E, host: &Layout) -> Result<Option<HostPath>, ReleaseError> {
    let usr_local = HostPath::new("/usr/local").expect("a path");
    if !host.install_root().as_str().starts_with("/usr/local/") {
        return Ok(None);
    }
    Ok(inspect(exec, &usr_local)?.is_none().then_some(usr_local))
}

/// The copied binary at `path` (`copied`, read back from the host after the copy and
/// before any link names it) has the source's sha256. Not generic over the host, so one
/// copy of it is measured for coverage, whichever host a test reads it from.
fn verify(copied: Option<Entry>, path: HostPath, source: &[u8]) -> Result<(), ReleaseError> {
    let got = match copied {
        Some(Entry::File { bytes, .. }) => sha256(&bytes),
        _ => "none (no file)".to_owned(),
    };
    let want = sha256(source);
    if got != want {
        return Err(ReleaseError::Copy { path, want, got });
    }
    Ok(())
}

/// `host rollback`: `current` back to the previous release; `previous` removed.
pub fn rollback<E: Exec>(
    exec: &mut E,
    host: &Layout,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<(), ReleaseError> {
    let (lock, state) = begin(host, mode)?;
    let Some(previous) = state.previous.clone() else {
        return Err(ReleaseError::NoPrevious);
    };
    if !has_binary(exec, host, &previous) {
        return Err(ReleaseError::PreviousGone(previous));
    }
    // The state records no release before the previous one, so `previous` goes.
    switch(exec, host, lock, &state, (previous, None), out)
}

/// Whether the release `commit` has a binary in its directory. Any will do: a release
/// holds `kbf` (a macOS one holds nothing else) and, on Linux, maybe `kbf-daemon`.
pub(crate) fn has_binary<E: Exec>(exec: &E, host: &Layout, commit: &CommitId) -> bool {
    layout::RELEASE_BINARIES.iter().any(|name| {
        matches!(
            exec.stat(&host.release_file(commit, name)),
            Ok(Some(Meta {
                kind: Kind::File,
                ..
            }))
        )
    })
}

/// `host gc --keep N`: remove each release directory but the kept ones: the state's
/// current, previous and staged, what the `current` and `previous` links point to, every
/// release in `in_use` (a process executes from it), the pinned releases and `last_good` of
/// `retain.json`, and the `keep` newest others (by the directory's modification time; never
/// fewer than [`retain::MIN_KEEP`]). Nothing is removed while a swap is on trial: the swap
/// is not over, and the release it would go back to is not the only one a revert may want.
/// An entry of `releases/` that is not a directory named by a commit id is left alone.
pub fn gc<E: Exec>(
    exec: &mut E,
    host: &Layout,
    keep: usize,
    in_use: &BTreeSet<CommitId>,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<(), ReleaseError> {
    let keep = retain::at_least(keep);
    // Held through the removals, so an install cannot make a release current meanwhile.
    let lock = match mode {
        Mode::Apply => Some(Locked::acquire(host)?),
        Mode::DryRun => None,
    };
    let state = match &lock {
        Some(lock) => lock.read()?,
        None => state::read_unlocked(host)?,
    };
    let state = state.unwrap_or_default();
    if let Some(p) = &state.pending_confirm {
        let _ = writeln!(
            out,
            "removing nothing: {} is on trial (confirm it or let it go back first)",
            p.commit
        );
        return Ok(());
    }
    let retain_file = retain::path(host);
    let retained = Retained::read(&retain_file)?;
    if lock.is_some() {
        sweep_stages(host)?;
    }
    let mut kept: BTreeSet<CommitId> = [state.current, state.previous, state.staged]
        .into_iter()
        .flatten()
        .collect();
    kept.extend(retained.protected());
    kept.extend(in_use.iter().cloned());
    for link in [host.current(), host.previous()] {
        if let Some(Entry::Link { target }) = inspect(exec, &link)? {
            kept.extend(linked(&target));
        }
    }
    let found = releases(host, out)?;
    let gone = retain::surplus(&found, &kept, keep);
    let mut paths = Vec::new();
    for commit in &gone {
        let dir = host.release_dir(commit);
        contents(host, &dir, &mut paths)?;
        paths.push(dir);
    }
    let mut planner = Planner::new(&*exec);
    for p in paths {
        planner.absent(p)?;
    }
    let kept: Vec<String> = kept.iter().map(ToString::to_string).collect();
    let _ = writeln!(
        out,
        "keeping: {} and the {keep} newest others; removing: {}",
        list(&kept),
        list(&gone.iter().map(ToString::to_string).collect::<Vec<_>>())
    );
    execute(&planner.finish(), RunAs::Root, mode, exec, &mut &mut *out)?;
    if mode == Mode::Apply && !gone.is_empty() {
        // Read again: a stage may have recorded a version since.
        Retained::update(&retain_file, |r| {
            for commit in &gone {
                r.versions.remove(commit);
            }
        })?;
    }
    Ok(())
}

/// The releases `host` holds, newest first, each with its version and why it must stay, for
/// `host status`. Read without the lock, like the status.
///
/// # Errors
/// The state, `retain.json` or `releases/` cannot be read.
pub fn kept(host: &Layout) -> Result<Vec<retain::Kept>, ReleaseError> {
    let state = state::read_unlocked(host)?.unwrap_or_default();
    let retained = Retained::read(&retain::path(host))?;
    let found = releases(host, &mut io::sink())?;
    Ok(retain::listing(
        &found,
        &retain::Known {
            current: state.current.as_ref(),
            previous: state.previous.as_ref(),
            on_trial: state.pending_confirm.as_ref().map(|p| &p.commit),
            retained: Some(&retained),
        },
    ))
}

fn list(items: &[String]) -> String {
    match items {
        [] => "none".to_owned(),
        _ => items.join(" "),
    }
}

/// The commit a link target `<commit>` names (the last component, so a link written as
/// `releases/<commit>` by an older build names it too).
fn linked(target: &Path) -> Option<CommitId> {
    CommitId::new(target.file_name()?.to_str()?).ok()
}

/// Each directory in `releases/` named by a commit id, with its modification time.
pub(crate) fn releases(
    host: &Layout,
    out: &mut dyn Write,
) -> Result<Vec<(CommitId, SystemTime)>, ReleaseError> {
    let dir = host.join(&host.releases());
    let items = match fs::read_dir(&dir) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Vec::new(),
        listed => listed
            .and_then(Iterator::collect::<io::Result<Vec<_>>>)
            .map_err(read_failed(&dir))?,
    };
    let mut found = Vec::new();
    for item in items {
        let full = item.path();
        let meta = fs::symlink_metadata(&full).map_err(read_failed(&full))?;
        let commit = item
            .file_name()
            .to_str()
            .and_then(|n| CommitId::new(n).ok());
        // The `current` and `previous` links live here too: they are the layout's, not
        // strays, and are not told.
        let link = meta.file_type().is_symlink()
            && [layout::CURRENT, layout::PREVIOUS]
                .iter()
                .any(|name| item.file_name() == OsStr::new(name));
        match commit {
            Some(c) if meta.is_dir() => {
                found.push((c, meta.modified().map_err(read_failed(&full))?));
            }
            _ if link => {}
            _ => {
                let _ = writeln!(out, "left alone (not a release): {}", full.display());
            }
        }
    }
    Ok(found)
}

/// Everything under `dir`, deepest first, so each removal finds its directory empty.
/// Links are removed, never followed.
fn contents(host: &Layout, dir: &HostPath, paths: &mut Vec<HostPath>) -> Result<(), ReleaseError> {
    let full = host.join(dir);
    let items = fs::read_dir(&full)
        .and_then(Iterator::collect::<io::Result<Vec<_>>>)
        .map_err(read_failed(&full))?;
    for item in items {
        let name = item.file_name();
        let p = child(dir, utf8(&name, &item.path())?);
        if item
            .file_type()
            .map_err(read_failed(&item.path()))?
            .is_dir()
        {
            contents(host, &p, paths)?;
        }
        paths.push(p);
    }
    Ok(())
}

#[cfg(test)]
#[path = "release_tests.rs"]
mod tests;
