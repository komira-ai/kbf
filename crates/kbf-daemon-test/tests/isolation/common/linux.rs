//! The Linux node of the farm, and the probes that read `/proc` and the cgroup tree: what
//! only a Linux node has. The platform-neutral probes and the [`NodeView`] trait are
//! `kbf-testkit`'s ([`kbf_testkit::probe`], [`kbf_testkit::node_view`]); the Mac legs add a
//! `MacNode` beside this in their own test target.

use std::collections::{BTreeMap, BTreeSet};
use std::os::unix::fs::MetadataExt as _;
use std::path::{Path, PathBuf};

use kbf_testkit::node_view::{Isolation, NodeView, Proc};
use kbf_testkit::probe::{self, Kind, LEASE_PREFIX};

fn io(path: &Path, e: &std::io::Error) -> String {
    format!("{}: {e}", path.display())
}

/// The Linux node of the farm: one storage-and-daemon node over the stand-in `crun`.
pub struct LinuxNode {
    /// The farm's directory; `node-1` is below it.
    pub farm: PathBuf,
    /// Pids of the farm's own roles (the kbf processes), which are not residue.
    pub roles: Vec<u32>,
    /// Every pid the farm's roles have had, live or ended: each leads a process group of
    /// its own (the harness starts it so), and a process still in that group after its
    /// role ended, or beside a live one, belongs to the farm whatever its environment and
    /// working directory say.
    pub groups: Vec<u32>,
}

impl LinuxNode {
    pub fn new(farm: &Path, roles: Vec<u32>) -> Self {
        Self {
            farm: farm.to_owned(),
            roles,
            groups: Vec::new(),
        }
    }

    /// The same node, finding orphans also by the process groups of `groups`.
    #[must_use]
    pub fn with_groups(mut self, groups: Vec<u32>) -> Self {
        self.groups = groups;
        self
    }

    /// The group and session ids that mark a process as the farm's: the farm's roles'
    /// groups, and the last action the stand-in `crun` started (`crun-state/pid` and
    /// `start-pid`: it makes the action the leader of its own session and group, and
    /// keeps the record after the lease ends).
    fn group_ids(&self) -> Vec<u32> {
        let mut ids = self.groups.clone();
        for record in ["pid", "start-pid"] {
            let path = self.node().join("bin/crun-state").join(record);
            if let Some(pid) = std::fs::read_to_string(path)
                .ok()
                .and_then(|t| t.trim().parse().ok())
            {
                ids.push(pid);
            }
        }
        ids
    }

    fn node(&self) -> PathBuf {
        self.farm.join("node-1")
    }
}

impl NodeView for LinuxNode {
    fn scratch(&self) -> PathBuf {
        self.node().join("data")
    }

    fn isolation(&self) -> Isolation {
        Isolation::Nothing
    }

    fn lease_dirs(&self) -> Result<BTreeSet<String>, String> {
        let leases = self.scratch().join("leases");
        let mut names = BTreeSet::new();
        for entry in probe::read_dir(&leases)? {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !self.kept(&Path::new("leases").join(&name)) {
                names.insert(name);
            }
        }
        Ok(names)
    }

    fn lease_cgroups(&self) -> Result<BTreeSet<String>, String> {
        let tree = probe::walk(&self.node().join("bin/cgroup"))?;
        Ok(tree
            .into_iter()
            .filter(|(rel, e)| {
                e.is_dir()
                    && rel
                        .file_name()
                        .is_some_and(|n| n.to_string_lossy().starts_with(LEASE_PREFIX))
            })
            .map(|(rel, _)| rel.display().to_string())
            .collect())
    }

    fn processes(&self) -> Result<BTreeSet<Proc>, String> {
        let members = cgroup_members(&self.node().join("bin/cgroup"))?;
        processes_of(&self.farm, &self.roles, &self.group_ids(), &members)
    }

    fn slot_users(&self) -> Result<BTreeSet<String>, String> {
        Ok(BTreeSet::new())
    }

    fn session_leftovers(&self) -> Result<BTreeSet<String>, String> {
        Ok(BTreeSet::new())
    }

    fn home_dirs(&self) -> Result<BTreeSet<String>, String> {
        Ok(BTreeSet::new())
    }

    fn kept(&self, rel: &Path) -> bool {
        // Whole components, never a prefix: `cache-evil` is not `cache`.
        const KEPT: [&str; 7] = [
            "cache",
            "trees",
            "leases/daemon-instance",
            "leases/oci-images",
            "leases/oci-bin",
            "leases/oci-helper",
            "leases/oci-helper-state",
        ];
        KEPT.iter().any(|k| rel.starts_with(k))
    }

    fn stores(&self) -> Vec<PathBuf> {
        let node = self.node();
        vec![
            node.join("hot"),
            node.join("cold"),
            node.join("meta"),
            node.join("state"),
            self.scratch().join("cache"),
            self.scratch().join("trees"),
            self.scratch().join("leases/oci-images"),
            self.scratch().join("leases/oci-bin"),
            self.scratch().join("leases/oci-helper"),
            self.scratch().join("leases/oci-helper-state"),
            self.farm.join("frontend/state"),
        ]
    }

    fn empty_between_leases(&self) -> Vec<PathBuf> {
        vec![self.scratch().join("leases/oci-helper-state")]
    }

    fn content_addressed(&self) -> Vec<PathBuf> {
        vec![self.scratch().join("cache"), self.scratch().join("trees")]
    }

    fn surroundings(&self) -> PathBuf {
        self.node()
    }

    fn surroundings_expected(&self, rel: &Path) -> bool {
        // Whole paths, never a prefix. The rehearsal topology's node: its configuration, the
        // leaf and CA certificates and keys, the id maps, and the stand-in's binaries.
        const EXPECTED: [&str; 17] = [
            "bin",
            "bin/crun",
            "bin/kbf-act",
            "bin/newgidmap",
            "bin/newuidmap",
            "bin/self-cgroup",
            "etc",
            "etc/ca.pem",
            "etc/node.key",
            "etc/node.pem",
            "etc/storage.key",
            "etc/storage.pem",
            "ids",
            "ids/passwd",
            "ids/subgid",
            "ids/subuid",
            "kbf.toml",
        ];
        EXPECTED.iter().any(|e| rel == Path::new(e))
    }

    fn surroundings_exempt(&self, rel: &Path) -> bool {
        // `data` is the scratch directory, looked at on its own; `hot`, `cold`, `meta` and
        // `state` are the server's stores (their logs are inside them); `bin/cgroup` is
        // the planted cgroup tree (`lease_cgroups` looks at it) and `bin/crun-state` the
        // stand-in's records, rewritten on every lease. Nothing else is exempt: not
        // `bin`, `etc`, `ids`, nor the node's configuration.
        const EXEMPT: [&str; 7] = [
            "data",
            "hot",
            "cold",
            "meta",
            "state",
            "bin/cgroup",
            "bin/crun-state",
        ];
        EXEMPT.iter().any(|e| rel.starts_with(e))
    }
}

fn gone(e: &std::io::Error) -> bool {
    // ENOENT, or ESRCH for a process that ended while it was being read.
    matches!(e.raw_os_error(), Some(2 | 3))
}

/// What `/proc/<pid>/stat` says of a process.
#[derive(Clone, Copy, Debug)]
struct Stat {
    state: char,
    ppid: u32,
    /// The process group id.
    pgrp: u32,
    /// The session id.
    session: u32,
}

/// `/proc/<pid>/stat`. `None` for a process that ended.
fn stat_of(pid: u32) -> Result<Option<Stat>, String> {
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
    let mut number = |what: &str| -> Result<u32, String> {
        words
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| format!("{}: no {what} in {text:?}", path.display()))
    };
    let (ppid, pgrp, session) = (number("parent")?, number("group")?, number("session")?);
    Ok(Some(Stat {
        state,
        ppid,
        pgrp,
        session,
    }))
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
    for entry in probe::read_dir(Path::new("/proc"))? {
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
/// A process is also the farm's, whatever its command line, environment and working
/// directory say (an orphan that cleared its environment, went to `/` and was
/// reparented to init), when it is a member of one of the cgroups `members` lists, or when
/// its process group or session is one of `groups`: the group of a farm role, or the group
/// and session an action was started as. Not found by any of these: a process that left
/// the farm's groups and sessions (`setsid`), cleared its environment and went to `/`, on a
/// kernel where the stand-in's cgroups hold no `cgroup.procs`. That is tier R
/// (`tier_r_an_orphan_that_hid_is_found_by_its_cgroup`).
///
/// # Errors
/// A process of this user cannot be read for a reason other than having ended.
pub fn processes_of(
    farm: &Path,
    roles: &[u32],
    groups: &[u32],
    members: &BTreeSet<u32>,
) -> Result<BTreeSet<Proc>, String> {
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
    for (
        &pid,
        &Stat {
            state,
            ppid,
            pgrp,
            session,
        },
    ) in &stats
    {
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
        let grouped = groups.contains(&pgrp) || groups.contains(&session);
        if has(&cmd)
            || has(&env)
            || has(&cwd)
            || zombie_of_roles
            || grouped
            || members.contains(&pid)
        {
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
            .filter(|(pid, st)| ours.contains_key(&st.ppid) && !ours.contains_key(pid))
            .map(|(&pid, _)| pid)
            .collect();
        if more.is_empty() {
            break;
        }
        for pid in more {
            let state = stats[&pid].state;
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

/// The pids listed in the `cgroup.procs` files below `tree` (a cgroup tree). The stand-in's
/// planted tree has none, so this is empty there; on a real cgroup v2 mount it is the
/// membership the kernel keeps, which no process can leave by changing its environment,
/// working directory, group or session. A missing `tree` is an error.
///
/// # Errors
/// `tree` is missing, or a file of it cannot be read or parsed.
pub fn cgroup_members(tree: &Path) -> Result<BTreeSet<u32>, String> {
    let mut members = BTreeSet::new();
    for (rel, entry) in probe::walk(tree)? {
        if entry.kind != Kind::File || rel.file_name().is_none_or(|n| n != "cgroup.procs") {
            continue;
        }
        let path = tree.join(&rel);
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(io(&path, &e)),
        };
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let pid = line
                .trim()
                .parse()
                .map_err(|_| format!("{}: not a pid: {line:?}", path.display()))?;
            members.insert(pid);
        }
    }
    Ok(members)
}

/// The zombies whose parent is one of `parents`.
///
/// # Errors
/// A process cannot be read.
pub fn zombie_children(parents: &[u32]) -> Result<BTreeSet<Proc>, String> {
    let mut found = BTreeSet::new();
    for pid in pids()? {
        if let Some(Stat {
            state: 'Z', ppid, ..
        }) = stat_of(pid)?
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
    Ok(stat_of(pid)?.is_some_and(|st| st.state != 'Z' && st.state != 'X'))
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

/// Kills the processes it is given when it drops: a test that plants a process the product
/// cannot reach must not leave it to the host.
#[derive(Default)]
pub struct KillOnDrop(pub Vec<u32>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        for &pid in &self.0 {
            if let Some(pid) = i32::try_from(pid)
                .ok()
                .and_then(rustix::process::Pid::from_raw)
            {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            }
        }
    }
}

/// Whether `abs` is named for the digest of what it holds, the way the daemon's input cache
/// names its entries: `<sha256 hex>-<size>`, with `.x` for an executable.
pub fn content_addressed_file(abs: &Path) -> bool {
    let Some(name) = abs.file_name().map(|n| n.to_string_lossy().into_owned()) else {
        return false;
    };
    let name = name.strip_suffix(".x").unwrap_or(&name);
    let Some((hash, size)) = name.split_once('-') else {
        return false;
    };
    let Ok(bytes) = std::fs::read(abs) else {
        return false;
    };
    let digest = kbf_transfer::digest_of(&bytes);
    digest.hash == hash && size.parse() == Ok(digest.size_bytes)
}
