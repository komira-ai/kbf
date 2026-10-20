//! The black-box probes: functions over the files of a node, and a [`NodeView`] of it, that
//! return `Err(what)` when they see residue or a leak. Platform-neutral: the Linux node's
//! `/proc` and cgroup probes are `kbf-daemon-test`'s (`tests/isolation/common/linux.rs`), the
//! Mac legs' are theirs, and both implement [`NodeView`].
//!
//! Every probe is used by a test that must pass on a clean run and by a `planted_` test that
//! must see it return `Err` (`crates/kbf-daemon-test/README.md`, "Checks, and the planted
//! leak that turns each red"), so a probe that cannot fail does not get in.
//!
//! A probe never skips what it cannot read: any error other than "gone" (ENOENT, ESRCH
//! for a process that ended between the listing and the read) is an `Err`. The stand-in
//! `crun` makes a lease's upper directory mode 000 while it is "container-owned"
//! (fake_crun.rs), and a walk that skipped it would pass exactly where a leak would sit.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crate::node_view::{NodeView, Proc};

/// The name a lease's directory and cgroup start with: `LEASE_PREFIX` of
/// kbf-driver-container's container.rs, "Lease scratch" of docs/design/daemon.md. Kept as
/// a literal here so a rename fails these tests, never passes them.
pub const LEASE_PREFIX: &str = "kbf-lease-";

/// How long the reaping of a process may take after the lease it belonged to ended. The
/// lease directory, cgroup and crun state are NOT given this time: the lease's own
/// `lease_ms` includes removing them (daemon.md "Leases"), so they must be gone when
/// `Execute` returns.
pub const SETTLE: Duration = Duration::from_secs(10);

/// What kind of path an [`Entry`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A symbolic link (never followed).
    Link,
    /// A fifo, a socket or a device.
    Other,
}

/// What a walk saw of one path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    /// The kind of path.
    pub kind: Kind,
    /// Apparent length in bytes.
    pub len: u64,
    /// Allocated bytes (blocks of 512).
    pub alloc: u64,
    /// The permission and set-id bits.
    pub mode: u32,
    /// Device and inode: a hard link is the same file under two names.
    pub id: (u64, u64),
}

impl Entry {
    /// Whether the path is a directory.
    #[must_use]
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
        id: (meta.dev(), meta.ino()),
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
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
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
    let top = entry_of(root)?.ok_or_else(|| format!("{}: missing", root.display()))?;
    let mut seen = BTreeSet::from([top.id]);
    let (mut alloc, mut apparent) = (top.alloc, top.len);
    if top.is_dir() {
        // What the walk saw: a path gone since is not counted, and the walk is not a
        // snapshot of a busy tree.
        for entry in walk(root)?.values() {
            if seen.insert(entry.id) {
                alloc += entry.alloc;
                apparent += entry.len;
            }
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
    /// Everything in the directories that are kept but must be empty between leases
    /// (`NodeView::empty_between_leases`), as `<directory>/<path>`.
    pub left_in_empty: BTreeSet<String>,
    /// Every path of the node's own directory outside the scratch directory, the stores,
    /// the cgroup tree and the stand-in's records (`NodeView::surroundings_exempt`).
    pub around: BTreeMap<PathBuf, Entry>,
    /// The paths of `around` that the node does not have by design
    /// (`NodeView::surroundings_expected`), whatever the baseline held.
    pub unexpected: BTreeSet<PathBuf>,
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
        let mut left_in_empty = BTreeSet::new();
        for dir in node.empty_between_leases() {
            // A missing directory is an error: a probe on a wrong path must not pass.
            for rel in walk(&dir)?.into_keys() {
                let name = dir.file_name().map(|n| n.to_string_lossy().into_owned());
                left_in_empty.insert(format!("{}/{}", name.unwrap_or_default(), rel.display()));
            }
        }
        let around_root = node.surroundings();
        let around = walk_pruned(&around_root, &|abs| {
            abs.strip_prefix(&around_root)
                .is_ok_and(|rel| node.surroundings_exempt(rel))
        })?
        .into_iter()
        .filter(|(rel, _)| !node.surroundings_exempt(rel))
        .collect::<BTreeMap<_, _>>();
        let unexpected = around
            .keys()
            .filter(|rel| !node.surroundings_expected(rel))
            .cloned()
            .collect();
        Ok(Self {
            unexpected,
            left_in_empty,
            around,
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
        // Empty between leases, not "as it was": a baseline that already holds something
        // there does not excuse it.
        for item in &self.left_in_empty {
            bad.push(format!("left in a directory that must be empty: {item}"));
        }
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
        for path in &self.unexpected {
            bad.push(format!(
                "unexpected path outside the scratch dir: {}",
                path.display()
            ));
        }
        for (path, entry) in &self.around {
            match before.around.get(path) {
                None => bad.push(format!(
                    "new path outside the scratch dir: {}",
                    path.display()
                )),
                Some(was) if entry.len > was.len || entry.alloc > was.alloc => bad.push(format!(
                    "grew outside the scratch dir: {} ({} -> {} bytes)",
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
    clean_since_within(node, before, SETTLE)
}

/// [`clean_since`] with `settle` in place of [`SETTLE`] for the processes.
///
/// # Errors
/// What is left, one finding per item.
pub fn clean_since_within(
    node: &dyn NodeView,
    before: &Snapshot,
    settle: Duration,
) -> Result<(), String> {
    let now = Snapshot::take(node).map_err(|e| format!("cannot look: {e}"))?;
    let mut bad = now.residue_since(before);
    // The files, cgroups and users above are asserted at once; only a process gets SETTLE to
    // be reaped. The stand-in reaps before `Execute` returns, so this poll's margin is never
    // used here and a run with SETTLE of zero is just as green: what the tests prove is the
    // immediate assertions above and the first look below. The margin is for a real
    // kernel's reaping of a process group, tier R.
    let end = Instant::now() + settle;
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

/// Every file of the node's content-addressed stores (`NodeView::content_addressed`) that
/// holds `needle` and is NOT named for the digest of its content (`named_for_content` says
/// whether a path is: the digest is the product's, so the caller brings it): a copy of an action's
/// data under any other name (`copy-1`, a digest of other bytes) is a leak, while the
/// action's own input held once under its digest is the cache working.
///
/// # Errors
/// As [`scan`].
pub fn canary_outside_digests(
    node: &dyn NodeView,
    needle: &[u8],
    named_for_content: &dyn Fn(&Path) -> bool,
) -> Result<Vec<PathBuf>, String> {
    let mut leaks = Vec::new();
    for root in node.content_addressed() {
        for hit in scan(&[root], needle, &|_| false)? {
            if !named_for_content(&hit) {
                leaks.push(hit);
            }
        }
    }
    Ok(leaks)
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
