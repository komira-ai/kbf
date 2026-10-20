//! The black-box probes: functions over the files and processes of a node that return
//! `Err(what)` when they see residue or a leak. Every probe is used by a test that must pass
//! on a clean run and by a `planted_` test that must see it return `Err` (README.md
//! "Red first"), so a probe that cannot fail does not get in.
//!
//! A probe never skips what it cannot read: any error other than "gone" (ENOENT, ESRCH
//! for a process that ended between the listing and the read) is an `Err`. The stand-in
//! `crun` makes a lease's upper directory mode 000 while it is "container-owned"
//! (fake_crun.rs), and a walk that skipped it would pass exactly where a leak would sit.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use super::node::{NodeView, Proc};

/// The name a lease's directory and cgroup start with: `LEASE_PREFIX` of
/// kbf-driver-container's container.rs, "Lease scratch" of docs/design/daemon.md. Kept as
/// a literal here so a rename fails these tests, never passes them.
pub const LEASE_PREFIX: &str = "kbf-lease-";

/// How long the reaping of a process may take after the lease it belonged to ended. The
/// lease directory, cgroup and crun state are NOT given this time: the lease's own
/// `lease_ms` includes removing them (daemon.md "Leases"), so they must be gone when
/// `Execute` returns.
pub const SETTLE: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Link,
    Other,
}

/// What a walk saw of one path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub kind: Kind,
    /// Apparent length in bytes.
    pub len: u64,
    /// Allocated bytes (blocks of 512).
    pub alloc: u64,
    pub mode: u32,
}

impl Entry {
    pub fn is_dir(&self) -> bool {
        self.kind == Kind::Dir
    }
}

fn io(path: &Path, e: &std::io::Error) -> String {
    format!("{}: {e}", path.display())
}

/// The entries of `dir`; a missing `dir` is an error (a probe on a wrong path must not pass).
pub fn read_dir(dir: &Path) -> Result<Vec<std::fs::DirEntry>, String> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|e| io(dir, &e))? {
        out.push(entry.map_err(|e| io(dir, &e))?);
    }
    Ok(out)
}

fn entry_of(path: &Path) -> Result<Option<Entry>, String> {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        // Removed between the listing and the stat: gone.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(io(path, &e)),
    };
    let kind = if meta.is_dir() {
        Kind::Dir
    } else if meta.is_file() {
        Kind::File
    } else if meta.file_type().is_symlink() {
        Kind::Link
    } else {
        Kind::Other
    };
    Ok(Some(Entry {
        kind,
        len: meta.len(),
        alloc: meta.blocks() * 512,
        mode: meta.mode() & 0o7777,
    }))
}

/// Every path below `root` (not following links), relative to `root`, with what `stat` says.
/// A directory `prune` names (by its absolute path) is listed but not entered.
///
/// # Errors
/// `root` is missing, or anything cannot be read for a reason other than having gone.
pub fn walk_pruned(
    root: &Path,
    prune: &dyn Fn(&Path) -> bool,
) -> Result<BTreeMap<PathBuf, Entry>, String> {
    let mut found = BTreeMap::new();
    let top = entry_of(root)?.ok_or_else(|| format!("{}: missing", root.display()))?;
    if !top.is_dir() {
        return Err(format!("{}: not a directory", root.display()));
    }
    let mut todo = vec![root.to_owned()];
    while let Some(dir) = todo.pop() {
        let listing = match std::fs::read_dir(&dir) {
            Ok(l) => l,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && dir != root => continue,
            Err(e) => return Err(io(&dir, &e)),
        };
        for item in listing {
            let item = item.map_err(|e| io(&dir, &e))?;
            let path = item.path();
            let Some(entry) = entry_of(&path)? else {
                continue;
            };
            let rel = path.strip_prefix(root).expect("below root").to_owned();
            if entry.is_dir() && !prune(&path) {
                todo.push(path);
            }
            found.insert(rel, entry);
        }
    }
    Ok(found)
}

/// [`walk_pruned`] with nothing pruned.
pub fn walk(root: &Path) -> Result<BTreeMap<PathBuf, Entry>, String> {
    walk_pruned(root, &|_| false)
}

/// The allocated and apparent bytes below `root` (a file or a directory), each inode
/// once.
pub fn bytes_below(root: &Path) -> Result<(u64, u64), String> {
    let mut seen = BTreeSet::new();
    let (mut alloc, mut apparent) = (0, 0);
    let mut count = |path: &Path| -> Result<(), String> {
        let meta = std::fs::symlink_metadata(path).map_err(|e| io(path, &e))?;
        if seen.insert((meta.dev(), meta.ino())) {
            alloc += meta.blocks() * 512;
            apparent += meta.len();
        }
        Ok(())
    };
    count(root)?;
    if !entry_of(root)?.is_some_and(|e| e.is_dir()) {
        return Ok((alloc, apparent));
    }
    for rel in walk(root)?.keys() {
        match count(&root.join(rel)) {
            Ok(()) => {}
            // Gone since the walk: the walk is not a snapshot of a busy tree.
            Err(e) if e.contains("No such file") => {}
            Err(e) => return Err(e),
        }
    }
    Ok((alloc, apparent))
}

/// What a node showed at one moment.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub leases: BTreeSet<String>,
    pub cgroups: BTreeSet<String>,
    pub pids: BTreeSet<u32>,
    /// Every path of the scratch directory that is not kept on purpose.
    pub files: BTreeMap<PathBuf, Entry>,
    /// Allocated bytes of the kept-on-purpose directories (input cache, trees, images).
    pub kept: u64,
    /// Allocated and apparent bytes of the content stores (hot, cold, meta, state).
    pub stores: (u64, u64),
    pub users: BTreeSet<String>,
    pub sessions: BTreeSet<String>,
    pub homes: BTreeSet<String>,
}

impl Snapshot {
    /// # Errors
    /// Anything that cannot be read (see the module doc).
    pub fn take(node: &dyn NodeView) -> Result<Self, String> {
        let scratch = node.scratch();
        let files = walk_pruned(&scratch, &|abs| {
            abs.strip_prefix(&scratch).is_ok_and(|rel| node.kept(rel))
        })?;
        let mut kept = 0;
        for (rel, entry) in &files {
            if node.kept(rel) && entry.kind != Kind::Link {
                kept += bytes_below(&scratch.join(rel))?.0;
            }
        }
        let files = files
            .into_iter()
            .filter(|(rel, _)| !node.kept(rel))
            .collect();
        let mut stores = (0, 0);
        for dir in node.stores() {
            // The data directory's own kept stores are counted in `kept`.
            if !dir.starts_with(&scratch) && dir.exists() {
                let (a, b) = bytes_below(&dir)?;
                stores = (stores.0 + a, stores.1 + b);
            }
        }
        Ok(Self {
            leases: node.lease_dirs()?,
            cgroups: node.lease_cgroups()?,
            pids: node.processes()?.iter().map(|p| p.pid).collect(),
            files,
            kept,
            stores,
            users: node.slot_users()?,
            sessions: node.session_leftovers()?,
            homes: node.home_dirs()?,
        })
    }

    /// What is on the node now that was not in `before`, other than processes: a lease
    /// directory or cgroup, a path outside the kept ones, a path that grew, a slot user,
    /// session or home. Empty when the node is as it was.
    pub fn residue_since(&self, before: &Self) -> Vec<String> {
        let mut bad = Vec::new();
        let new = |name: &str, now: &BTreeSet<String>, was: &BTreeSet<String>, bad: &mut Vec<_>| {
            for item in now.difference(was) {
                bad.push(format!("{name} left: {item}"));
            }
        };
        new("lease directory", &self.leases, &before.leases, &mut bad);
        new("lease cgroup", &self.cgroups, &before.cgroups, &mut bad);
        new("slot user", &self.users, &before.users, &mut bad);
        new("session", &self.sessions, &before.sessions, &mut bad);
        new("home directory", &self.homes, &before.homes, &mut bad);
        for (path, entry) in &self.files {
            match before.files.get(path) {
                None => bad.push(format!("new path: {}", path.display())),
                Some(was) if entry.len > was.len || entry.alloc > was.alloc => bad.push(format!(
                    "grew: {} ({} -> {} bytes)",
                    path.display(),
                    was.len,
                    entry.len
                )),
                Some(_) => {}
            }
        }
        bad
    }
}

/// The node is as `before` showed it, now: the lease directories, cgroups, files and users
/// at once, then the processes, which get [`SETTLE`] to be reaped.
///
/// # Errors
/// What is left, one finding per item.
pub fn clean_since(node: &dyn NodeView, before: &Snapshot) -> Result<(), String> {
    let now = Snapshot::take(node).map_err(|e| format!("cannot look: {e}"))?;
    let mut bad = now.residue_since(before);
    let end = Instant::now() + SETTLE;
    loop {
        let procs: Vec<Proc> = node
            .processes()
            .map_err(|e| format!("cannot look: {e}"))?
            .into_iter()
            .filter(|p| !before.pids.contains(&p.pid))
            .collect();
        if procs.is_empty() {
            break;
        }
        if Instant::now() > end {
            bad.extend(
                procs
                    .iter()
                    .map(|p| format!("process left: {} (state {}) {}", p.pid, p.state, p.cmd)),
            );
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    if bad.is_empty() {
        Ok(())
    } else {
        Err(bad.join("; "))
    }
}

fn gone(e: &std::io::Error) -> bool {
    // ENOENT, or ESRCH for a process that ended while it was being read.
    matches!(e.raw_os_error(), Some(2 | 3))
}

/// `/proc/<pid>/stat`: the state letter and the parent. `None` for a process that ended.
fn stat_of(pid: u32) -> Result<Option<(char, u32)>, String> {
    let path = PathBuf::from(format!("/proc/{pid}/stat"));
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if gone(&e) => return Ok(None),
        Err(e) => return Err(io(&path, &e)),
    };
    // `pid (comm) S ppid ...`, and comm may hold parentheses and spaces.
    let after = text
        .rsplit_once(')')
        .ok_or_else(|| format!("{}: unparsable", path.display()))?
        .1;
    let mut words = after.split_whitespace();
    let state = words
        .next()
        .and_then(|s| s.chars().next())
        .ok_or_else(|| format!("{}: no state", path.display()))?;
    let ppid = words
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("{}: no parent", path.display()))?;
    Ok(Some((state, ppid)))
}

fn proc_file(pid: u32, name: &str) -> Result<Option<Vec<u8>>, String> {
    let path = PathBuf::from(format!("/proc/{pid}/{name}"));
    match std::fs::read(&path) {
        Ok(b) => Ok(Some(b)),
        Err(e) if gone(&e) => Ok(None),
        Err(e) => Err(io(&path, &e)),
    }
}

/// Like [`proc_file`], but a file the user is denied reads as empty (see the caller).
fn proc_file_or_denied(pid: u32, name: &str) -> Result<Vec<u8>, String> {
    let path = PathBuf::from(format!("/proc/{pid}/{name}"));
    match std::fs::read(&path) {
        Ok(b) => Ok(b),
        Err(e) if gone(&e) || e.kind() == std::io::ErrorKind::PermissionDenied => Ok(Vec::new()),
        Err(e) => Err(io(&path, &e)),
    }
}

fn pids() -> Result<Vec<u32>, String> {
    let mut all = Vec::new();
    for entry in read_dir(Path::new("/proc"))? {
        if let Some(pid) = entry.file_name().to_str().and_then(|n| n.parse().ok()) {
            all.push(pid);
        }
    }
    Ok(all)
}

/// The processes that belong to the farm in `farm` and are not one of its `roles`: those
/// whose command line, environment (the stand-in `crun` gives each action `ROOT`, `UPPER`
/// and `CG`, all below the farm) or working directory name the farm's directory, and the
/// children of any of them, and the zombies of the roles. A process of another user cannot be
/// ours and is not read.
///
/// # Errors
/// A process of this user cannot be read for a reason other than having ended.
pub fn processes_of(farm: &Path, roles: &[u32]) -> Result<BTreeSet<Proc>, String> {
    let me = std::process::id();
    let mine = std::fs::metadata("/proc/self").map_err(|e| io(Path::new("/proc/self"), &e))?;
    let needle = farm.to_string_lossy().into_owned().into_bytes();
    let has = |hay: &[u8]| hay.windows(needle.len()).any(|w| w == needle.as_slice());
    let mut stats = BTreeMap::new();
    for pid in pids()? {
        if pid == me {
            continue;
        }
        match std::fs::metadata(format!("/proc/{pid}")) {
            Ok(m) if m.uid() == mine.uid() => {}
            Ok(_) => continue,
            Err(e) if gone(&e) => continue,
            Err(e) => return Err(io(Path::new("/proc"), &e)),
        }
        if let Some(stat) = stat_of(pid)? {
            stats.insert(pid, stat);
        }
    }
    let mut ours = BTreeMap::new();
    for (&pid, &(state, ppid)) in &stats {
        if roles.contains(&pid) {
            continue;
        }
        let Some(cmd) = proc_file(pid, "cmdline")? else {
            continue;
        };
        // A process that is not dumpable (it ran a setuid or capable program) refuses its
        // environment and working directory to its own user; such a process of the farm is
        // found by its command line or by its parent, below.
        let env = proc_file_or_denied(pid, "environ")?;
        let cwd = match std::fs::read_link(format!("/proc/{pid}/cwd")) {
            Ok(p) => p.to_string_lossy().into_owned().into_bytes(),
            Err(e) if gone(&e) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Vec::new(),
            // A zombie has no working directory to read.
            Err(_) if state == 'Z' => Vec::new(),
            Err(e) => return Err(io(Path::new("/proc"), &e)),
        };
        let zombie_of_roles = state == 'Z' && roles.contains(&ppid);
        if has(&cmd) || has(&env) || has(&cwd) || zombie_of_roles {
            ours.insert(
                pid,
                Proc {
                    pid,
                    state,
                    cmd: String::from_utf8_lossy(&cmd)
                        .replace('\0', " ")
                        .trim()
                        .to_owned(),
                },
            );
        }
    }
    // The children of ours are ours (a daemonised child that changed its environment, a
    // process whose environment is unreadable).
    loop {
        let more: Vec<u32> = stats
            .iter()
            .filter(|(pid, (_, ppid))| ours.contains_key(ppid) && !ours.contains_key(pid))
            .map(|(&pid, _)| pid)
            .collect();
        if more.is_empty() {
            break;
        }
        for pid in more {
            let state = stats[&pid].0;
            let cmd = proc_file(pid, "cmdline")?.unwrap_or_default();
            ours.insert(
                pid,
                Proc {
                    pid,
                    state,
                    cmd: String::from_utf8_lossy(&cmd)
                        .replace('\0', " ")
                        .trim()
                        .to_owned(),
                },
            );
        }
    }
    Ok(ours.into_values().collect())
}

/// The zombies whose parent is one of `parents`.
///
/// # Errors
/// A process cannot be read.
pub fn zombie_children(parents: &[u32]) -> Result<BTreeSet<Proc>, String> {
    let mut found = BTreeSet::new();
    for pid in pids()? {
        if let Some(('Z', ppid)) = stat_of(pid)?
            && parents.contains(&ppid)
        {
            found.insert(Proc {
                pid,
                state: 'Z',
                cmd: String::new(),
            });
        }
    }
    Ok(found)
}

/// Whether process `pid` is alive and not a zombie.
pub fn alive(pid: u32) -> Result<bool, String> {
    Ok(stat_of(pid)?.is_some_and(|(state, _)| state != 'Z' && state != 'X'))
}

/// The open file descriptors of `pid`: number and what it points at.
///
/// # Errors
/// The process cannot be read (a process that ended gives an empty list).
pub fn open_fds(pid: u32) -> Result<Vec<(u32, String)>, String> {
    let dir = PathBuf::from(format!("/proc/{pid}/fd"));
    let listing = match std::fs::read_dir(&dir) {
        Ok(l) => l,
        Err(e) if gone(&e) => return Ok(Vec::new()),
        Err(e) => return Err(io(&dir, &e)),
    };
    let mut fds = Vec::new();
    for item in listing {
        let item = item.map_err(|e| io(&dir, &e))?;
        let Some(n) = item.file_name().to_str().and_then(|n| n.parse().ok()) else {
            continue;
        };
        match std::fs::read_link(item.path()) {
            Ok(target) => fds.push((n, target.to_string_lossy().into_owned())),
            Err(e) if gone(&e) => {}
            Err(e) => return Err(io(&item.path(), &e)),
        }
    }
    fds.sort();
    Ok(fds)
}

/// The test process's own descriptors before it started any farm: what the test was
/// started with (a build tool's lock file, say) and every child inherits down the chain.
static INHERITED: std::sync::OnceLock<Vec<(u32, String)>> = std::sync::OnceLock::new();

/// Remembers the test process's descriptors, once, before the first farm starts.
pub fn note_inherited() {
    INHERITED.get_or_init(|| open_fds(std::process::id()).unwrap_or_default());
}

/// The descriptors of an action that are not the ones it is meant to have: 0 is the null
/// device, 1 and 2 are the lease's `stdout` and `stderr` files (the daemon opens them,
/// crun.rs `FileIo`), nothing else, except what the test itself was started with
/// ([`note_inherited`]). `Err` names each other one.
pub fn unexpected_fds(fds: &[(u32, String)]) -> Result<(), String> {
    let given = INHERITED.get().cloned().unwrap_or_default();
    let mut bad = Vec::new();
    for (n, target) in fds {
        if *n > 2 && given.contains(&(*n, target.clone())) {
            continue;
        }
        let base = Path::new(target)
            .file_name()
            .map(|b| b.to_string_lossy().into_owned());
        let wanted = match n {
            0 => target == "/dev/null",
            1 => base.as_deref() == Some("stdout") && target.contains(LEASE_PREFIX),
            2 => base.as_deref() == Some("stderr") && target.contains(LEASE_PREFIX),
            _ => false,
        };
        if !wanted {
            bad.push(format!("fd {n} -> {target}"));
        }
    }
    if bad.is_empty() {
        Ok(())
    } else {
        Err(format!("inherited descriptors: {}", bad.join(", ")))
    }
}

/// Every regular file under `roots` (the directories `prune` names are not entered) that
/// holds `needle`.
///
/// # Errors
/// A file or directory cannot be read: a canary scan that skipped what it could not open
/// would pass on exactly the paths a leak hides in.
pub fn scan(
    roots: &[PathBuf],
    needle: &[u8],
    prune: &dyn Fn(&Path) -> bool,
) -> Result<Vec<PathBuf>, String> {
    let mut hits = Vec::new();
    for root in roots {
        for (rel, entry) in walk_pruned(root, prune)? {
            let abs = root.join(&rel);
            if entry.kind != Kind::File || entry.len < needle.len() as u64 || prune(&abs) {
                continue;
            }
            let bytes = match std::fs::read(&abs) {
                Ok(b) => b,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => return Err(io(&abs, &e)),
            };
            if bytes.windows(needle.len()).any(|w| w == needle) {
                hits.push(abs);
            }
        }
    }
    Ok(hits)
}

/// Every file under `farm` that holds `needle`, other than inside the node's content stores
/// (which hold content by digest by design; their logs, `hot/log`, do not count as stores).
/// So the daemon's and server's logs, the leases' `stdout` and `stderr`, and every
/// scratch directory are searched.
///
/// # Errors
/// As [`scan`].
pub fn files_containing(
    node: &dyn NodeView,
    farm: &Path,
    needle: &[u8],
) -> Result<Vec<PathBuf>, String> {
    let stores = node.stores();
    let is_store = |abs: &Path| {
        stores
            .iter()
            .any(|s| abs.starts_with(s) && !abs.starts_with(s.join("log")))
    };
    scan(&[farm.to_owned()], needle, &is_store)
}

/// Every path under `farm` with the file name `name`.
///
/// # Errors
/// A directory cannot be read.
pub fn paths_named(farm: &Path, name: &str) -> Result<Vec<PathBuf>, String> {
    Ok(walk(farm)?
        .into_iter()
        .filter(|(rel, _)| rel.file_name().is_some_and(|n| n == name))
        .map(|(rel, _)| farm.join(rel))
        .collect())
}

/// Every path below `root` (and `root`) that anyone may write: mode bit `o+w`, other than
/// links.
///
/// # Errors
/// A path cannot be read.
pub fn world_writable(root: &Path) -> Result<Vec<PathBuf>, String> {
    let mut bad = Vec::new();
    let top = entry_of(root)?.ok_or_else(|| format!("{}: missing", root.display()))?;
    if top.mode & 0o002 != 0 {
        bad.push(root.to_owned());
    }
    for (rel, entry) in walk(root)? {
        if entry.kind != Kind::Link && entry.mode & 0o002 != 0 {
            bad.push(root.join(rel));
        }
    }
    Ok(bad)
}
