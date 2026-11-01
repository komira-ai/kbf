//! The daemon's delegated cgroup subtree, and what its leases may use of the node.
//!
//! Under a systemd unit with `Delegate=yes`, the daemon's own cgroup (the `0::` line of
//! `/proc/self/cgroup`) is the daemon's to manage. cgroup v2 lets a cgroup that enables
//! controllers for its children hold no process itself (the no-internal-process rule),
//! so at start [`delegate`]:
//!
//! 1. refuses a host without cgroup v2 at the mount (cgroup v1, or a hybrid mount), a
//!    daemon in the root cgroup, and a cgroup that does not offer `cpu`, `memory` and
//!    `pids`, each with the fix in the message;
//! 2. makes the leaf `supervisor/` and moves every process of its cgroup into it (the
//!    daemon, and anything its unit started beside it), listing again if a process
//!    appeared meanwhile;
//! 3. enables `+cpu +memory +pids` for its children;
//! 4. writes `supervisor/memory.min` (`--supervisor-memory-min-mib`): memory the
//!    kernel does not reclaim from the daemon while it uses no more than that, so a
//!    node short of memory takes it from the builds and not from the process that
//!    reports them. The kernel caps a cgroup's protection at its ancestors', so the
//!    unit and its slice need `MemoryMin=` at least as large; [`Delegation`] names
//!    the nearest ancestor (below the root) whose `memory.min` is lower;
//! 5. makes `actions/`, under which every lease cgroup is made, and enables the same
//!    three in it;
//! 6. writes `actions/memory.max` when given one (`--actions-memory-max-gib`): the most
//!    memory all leases together may use, so the rest of the node stays for what else
//!    runs on it.
//!
//! [`limit_cpus`] then writes `actions/cpu.max`: the leases' CPUs, which is the CPUs the
//! daemon may use minus `--reserve-cpus`. It is a bandwidth limit on all leases together,
//! not pinning; the daemon and the server keep the rest. [`unlimit_cpus`] writes `max`
//! instead, for a farm that places by measured load (`--actions-cpu-max`).
//!
//! A daemon whose own cgroup is already a `supervisor` leaf (a second start in the same
//! unit, without the unit restarting) works on its parent; the steps find their work
//! done. [`adopt`] is the other way in: an `actions/` cgroup someone else set up
//! (`--cgroup-parent`), checked and not changed but for `memory.max` when given.
//!
//! [`enable_cpuset`] is for `--lease-placement=numa-node` alone: it enables `+cpuset` in
//! `actions/` and its parent, where the three above do not.
//!
//! [`capacity`] reads what `actions/` may use: the lowest `memory.max` on the path
//! from `actions/` up to the root (so a unit's own `MemoryMax=` counts), and the CPUs
//! in the nearest `cpuset.cpus.effective`, lowered by the lowest `cpu.max` quota on the
//! path (so a unit's `CPUQuota=` counts). The daemon lowers its node report's `cpus`
//! and `mem_gib` to them.

use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use kbf_daemon::Capacity;

use crate::numa::CpuSet;

/// The controllers enabled for the delegated cgroup's children and for `actions/`'s.
const CONTROLLERS: [&str; 3] = ["cpu", "memory", "pids"];
/// The period of the `cpu.max` written for the leases, in microseconds (the kernel's
/// default).
const CPU_PERIOD_US: u64 = 100_000;
/// The leaf the daemon's processes move into.
pub const SUPERVISOR: &str = "supervisor";
/// The cgroup every lease cgroup is made under.
pub const ACTIONS: &str = "actions";
/// The first pause before the processes are listed and moved again when enabling
/// controllers fails with EBUSY (a process appeared after the listing, or a thread
/// that has begun to exit still counts as in the cgroup); each pause doubles up to
/// [`MOVE_PAUSE_MAX`].
const MOVE_PAUSE_FIRST: Duration = Duration::from_millis(1);
/// The longest pause between two tries.
const MOVE_PAUSE_MAX: Duration = Duration::from_millis(50);
/// How long, in pauses, the set-up lists and moves again before it gives up.
const MOVE_PAUSE_TOTAL: Duration = Duration::from_secs(5);
/// Where the kernel names the cgroups of this process.
pub const PROC_SELF_CGROUP: &str = "/proc/self/cgroup";

/// The delegated cgroup and the `actions/` cgroup under it, as names relative to the
/// cgroup mount, starting with `/`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Delegation {
    /// The daemon's delegated cgroup (its unit's).
    pub root: String,
    /// `<root>/actions`: what `--cgroup-parent` names when given.
    pub actions: String,
    /// The nearest cgroup from `root` up (the root cgroup excluded) whose
    /// `memory.min` is below the one written on `supervisor/`, with its value: the
    /// kernel protects the daemon no further than that. `None` when every ancestor
    /// that shows a `memory.min` protects at least as much.
    pub memory_min_capped: Option<(String, u64)>,
}

/// Why the daemon cannot use its cgroup. Each message names the fix.
#[derive(Debug, thiserror::Error)]
pub enum DelegateError {
    /// `/proc/self/cgroup` has no cgroup v2 line.
    #[error(
        "{PROC_SELF_CGROUP} has no cgroup v2 (\"0::\") line: this host runs cgroup v1; \
         the container driver needs cgroup v2 alone (boot with systemd.unified_cgroup_hierarchy=1)"
    )]
    NoUnified,
    /// The mount has no `cgroup.controllers`: not cgroup v2 there.
    #[error(
        "{} has no cgroup.controllers: cgroup v2 is not mounted there (a hybrid or v1 host); \
         the container driver needs cgroup v2 alone (boot with systemd.unified_cgroup_hierarchy=1)",
        .0.display()
    )]
    NotV2(PathBuf),
    /// The daemon is in the root cgroup, which it may not manage.
    #[error(
        "the daemon runs in the root cgroup: start it in a systemd unit with Delegate=yes, \
         whose cgroup it then manages"
    )]
    RootCgroup,
    /// The cgroup lacks controllers the leases need.
    #[error(
        "cgroup {cgroup} {what} {missing} (it lists {listed:?}): {fix}",
        missing = .missing.join(" ")
    )]
    Controllers {
        cgroup: String,
        /// "does not offer" or "does not enable".
        what: &'static str,
        missing: Vec<&'static str>,
        listed: String,
        fix: String,
    },
    /// A cgroup file or directory could not be read, made or written.
    #[error("{what} {}: {source}{}", path.display(), hint(source))]
    Io {
        what: &'static str,
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// The fix for a cgroup the daemon may not write.
fn hint(e: &io::Error) -> &'static str {
    match e.kind() {
        io::ErrorKind::PermissionDenied | io::ErrorKind::ReadOnlyFilesystem => {
            " (the daemon's user may not write its cgroup: run it in a systemd unit with \
             Delegate=yes and User= set to that user)"
        }
        _ => "",
    }
}

/// The cgroup files the steps read and write, with the cgroup named relative to the
/// mount (`/` is the root). [`Mount`] is the kernel's; the tests have a fake that keeps
/// the kernel's rules.
pub(crate) trait CgroupFs {
    fn read(&self, cgroup: &str, file: &str) -> io::Result<String>;
    fn write(&self, cgroup: &str, file: &str, value: &str) -> io::Result<()>;
    fn mkdir(&self, cgroup: &str) -> io::Result<()>;
    /// Whether this process may make cgroups in `cgroup`.
    fn writable(&self, cgroup: &str) -> io::Result<()>;
    /// The path of `file` in `cgroup`, for messages.
    fn path(&self, cgroup: &str, file: &str) -> PathBuf;
    /// Waits `how_long` (the tests record it instead).
    fn pause(&self, how_long: Duration);
}

/// The cgroup v2 filesystem mounted at a directory.
pub(crate) struct Mount<'a>(pub(crate) &'a Path);

impl CgroupFs for Mount<'_> {
    fn read(&self, cgroup: &str, file: &str) -> io::Result<String> {
        std::fs::read_to_string(self.path(cgroup, file))
    }

    fn write(&self, cgroup: &str, file: &str, value: &str) -> io::Result<()> {
        std::fs::write(self.path(cgroup, file), value)
    }

    fn mkdir(&self, cgroup: &str) -> io::Result<()> {
        std::fs::create_dir(self.path(cgroup, ""))
    }

    fn writable(&self, cgroup: &str) -> io::Result<()> {
        rustix::fs::access(self.path(cgroup, ""), rustix::fs::Access::WRITE_OK)
            .map_err(io::Error::from)
    }

    fn pause(&self, how_long: Duration) {
        std::thread::sleep(how_long);
    }

    fn path(&self, cgroup: &str, file: &str) -> PathBuf {
        let relative = cgroup.trim_start_matches('/');
        // `join("")` would add a trailing `/`.
        let dir = if relative.is_empty() {
            self.0.to_owned()
        } else {
            self.0.join(relative)
        };
        if file.is_empty() { dir } else { dir.join(file) }
    }
}

/// Sets up the daemon's delegated cgroup under the cgroup v2 mount `mount`, from the
/// text of `/proc/self/cgroup` (see the module docs). Writes `supervisor_memory_min`
/// (bytes) to `supervisor/memory.min`, and `actions/memory.max` when
/// `actions_memory_max` (bytes) is given.
///
/// # Errors
/// The host, the unit or the cgroup does not allow it; the message names the fix.
pub fn delegate(
    mount: &Path,
    proc_self_cgroup: &str,
    supervisor_memory_min: u64,
    actions_memory_max: Option<u64>,
) -> Result<Delegation, DelegateError> {
    delegate_in(
        &Mount(mount),
        proc_self_cgroup,
        supervisor_memory_min,
        actions_memory_max,
    )
}

/// Checks the `actions` cgroup someone else set up (`--cgroup-parent`): cgroup v2 at
/// `mount`, `cpu`, `memory` and `pids` enabled for its children, and a cgroup this
/// process may make lease cgroups in. Writes its `memory.max` when given one.
///
/// # Errors
/// As [`delegate`].
pub fn adopt(
    mount: &Path,
    actions: &str,
    actions_memory_max: Option<u64>,
) -> Result<(), DelegateError> {
    adopt_in(&Mount(mount), actions, actions_memory_max)
}

/// What leases under `actions` may use (see the module docs). `None` where nothing
/// limits them below the node.
///
/// # Errors
/// A limit file could not be read or parsed.
pub fn capacity(mount: &Path, actions: &str) -> Result<Capacity, DelegateError> {
    capacity_in(&Mount(mount), actions)
}

/// Limits the leases under `actions` to `cpus` CPUs together (`actions/cpu.max`), the
/// leases' share of the node after `--reserve-cpus`. A bandwidth limit, not pinning: the
/// leases run on any CPU, and all of them together are throttled to `cpus` CPUs of time.
///
/// # Errors
/// `cpu.max` could not be written (the `cpu` controller is not enabled in `actions`).
pub fn limit_cpus(mount: &Path, actions: &str, cpus: u64) -> Result<(), DelegateError> {
    limit_cpus_in(&Mount(mount), actions, cpus)
}

pub(crate) fn limit_cpus_in(
    fs: &dyn CgroupFs,
    actions: &str,
    cpus: u64,
) -> Result<(), DelegateError> {
    let quota = cpus.max(1).saturating_mul(CPU_PERIOD_US);
    fs.write(actions, "cpu.max", &format!("{quota} {CPU_PERIOD_US}"))
        .map_err(io_err(fs, "write", actions, "cpu.max"))
}

/// Lifts the CPU bandwidth limit of the leases under `actions` (`actions/cpu.max` is
/// `max`), so that the CPU they use is bounded only by the node and measured CPU can show
/// an overload (`--actions-cpu-max`). [`limit_cpus`] sets it again.
///
/// # Errors
/// `cpu.max` could not be written (the `cpu` controller is not enabled in `actions`).
pub fn unlimit_cpus(mount: &Path, actions: &str) -> Result<(), DelegateError> {
    unlimit_cpus_in(&Mount(mount), actions)
}

pub(crate) fn unlimit_cpus_in(fs: &dyn CgroupFs, actions: &str) -> Result<(), DelegateError> {
    fs.write(actions, "cpu.max", &format!("max {CPU_PERIOD_US}"))
        .map_err(io_err(fs, "write", actions, "cpu.max"))
}

/// Enables `cpuset` for the leases under `actions` (`--lease-placement=numa-node`): in
/// its parent's `cgroup.subtree_control`, so `actions/` has the cpuset files, and in
/// `actions/`'s own, so each lease cgroup has them. Returns the CPUs `actions/` may use
/// (`cpuset.cpus.effective`), `None` where the kernel shows none.
///
/// Under a systemd unit with `Delegate=yes` the unit is offered `cpuset` with the other
/// controllers; a user manager (`user@.service`) offers only `memory pids` unless a
/// drop-in names more.
///
/// # Errors
/// The parent does not offer `cpuset` (the message names the fix), or a controller list
/// could not be written (the parent holds a process, or the user may not write it).
pub fn enable_cpuset(mount: &Path, actions: &str) -> Result<Option<CpuSet>, DelegateError> {
    enable_cpuset_in(&Mount(mount), actions)
}

pub(crate) fn enable_cpuset_in(
    fs: &dyn CgroupFs,
    actions: &str,
) -> Result<Option<CpuSet>, DelegateError> {
    let up = match actions.rsplit_once('/') {
        Some(("", _)) | None => "/",
        Some((parent, _)) => parent,
    };
    let offered = read(fs, up, "cgroup.controllers")?;
    if !offered.split_whitespace().any(|c| c == "cpuset") {
        return Err(DelegateError::Controllers {
            cgroup: up.to_owned(),
            what: "does not offer",
            missing: vec!["cpuset"],
            listed: offered.trim().to_owned(),
            fix: "--lease-placement=numa-node needs it: the daemon's unit needs Delegate=yes \
                  (or Delegate=cpu cpuset memory pids), and a user manager needs a drop-in \
                  that delegates cpuset"
                .to_owned(),
        });
    }
    for cgroup in [up, actions] {
        let enabled = read(fs, cgroup, "cgroup.subtree_control")?;
        if !enabled.split_whitespace().any(|c| c == "cpuset") {
            fs.write(cgroup, "cgroup.subtree_control", "+cpuset")
                .map_err(io_err(
                    fs,
                    "enable cpuset in",
                    cgroup,
                    "cgroup.subtree_control",
                ))?;
        }
    }
    let Some(text) = read_if_there(fs, actions, "cpuset.cpus.effective")? else {
        return Ok(None);
    };
    if text.trim().is_empty() {
        return Ok(None);
    }
    text.parse::<CpuSet>()
        .map(Some)
        .map_err(|e: String| invalid(fs, actions, "cpuset.cpus.effective", &e))
}

pub(crate) fn delegate_in(
    fs: &dyn CgroupFs,
    proc_self_cgroup: &str,
    supervisor_memory_min: u64,
    actions_memory_max: Option<u64>,
) -> Result<Delegation, DelegateError> {
    let own = proc_self_cgroup
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(str::trim)
        .ok_or(DelegateError::NoUnified)?;
    require_v2(fs)?;
    let root = match own.rsplit_once('/') {
        Some((parent, SUPERVISOR)) => {
            if parent.is_empty() {
                "/"
            } else {
                parent
            }
        }
        _ => own,
    };
    if root == "/" || root.is_empty() {
        return Err(DelegateError::RootCgroup);
    }
    let offered = read(fs, root, "cgroup.controllers")?;
    require(root, &offered, "does not offer", || {
        "the daemon's unit needs Delegate=yes (or Delegate=cpu memory pids)".to_owned()
    })?;

    let supervisor = child(root, SUPERVISOR);
    make(fs, &supervisor)?;
    let mut pause = MOVE_PAUSE_FIRST;
    let mut waited = Duration::ZERO;
    loop {
        for pid in read(fs, root, "cgroup.procs")?.split_whitespace() {
            match fs.write(&supervisor, "cgroup.procs", pid) {
                // The process ended after the listing.
                Err(e) if e.raw_os_error() == Some(rustix::io::Errno::SRCH.raw_os_error()) => {}
                other => other.map_err(io_err(
                    fs,
                    "move a process into",
                    &supervisor,
                    "cgroup.procs",
                ))?,
            }
        }
        match enable(fs, root) {
            // A process appeared in `root` after the listing, or a thread that has
            // begun to exit is still counted there: the kernel skips it when it moves
            // a process and `cgroup.procs` does not list it, but `root` stays populated
            // until the exit ends. Wait, then list and move again.
            Err(e) if e.kind() == io::ErrorKind::ResourceBusy && waited < MOVE_PAUSE_TOTAL => {
                fs.pause(pause);
                waited += pause;
                pause = (pause * 2).min(MOVE_PAUSE_MAX);
            }
            other => {
                break other.map_err(io_err(
                    fs,
                    "enable controllers in",
                    root,
                    "cgroup.subtree_control",
                ))?;
            }
        }
    }

    fs.write(
        &supervisor,
        "memory.min",
        &supervisor_memory_min.to_string(),
    )
    .map_err(io_err(fs, "write", &supervisor, "memory.min"))?;
    let memory_min_capped = memory_min_below(fs, root, supervisor_memory_min)?;

    let actions = child(root, ACTIONS);
    make(fs, &actions)?;
    enable(fs, &actions).map_err(io_err(
        fs,
        "enable controllers in",
        &actions,
        "cgroup.subtree_control",
    ))?;
    if let Some(max) = actions_memory_max {
        write_memory_max(fs, &actions, max)?;
    }
    Ok(Delegation {
        root: root.to_owned(),
        actions,
        memory_min_capped,
    })
}

/// The nearest cgroup from `cgroup` up to (not including) the root whose `memory.min`
/// is below `want`, with its value. Cgroups that show no `memory.min` are passed.
fn memory_min_below(
    fs: &dyn CgroupFs,
    mut cgroup: &str,
    want: u64,
) -> Result<Option<(String, u64)>, DelegateError> {
    while cgroup != "/" {
        if let Some(text) = read_if_there(fs, cgroup, "memory.min")? {
            let text = text.trim();
            let min = if text == "max" {
                u64::MAX
            } else {
                text.parse::<u64>()
                    .map_err(|e| invalid(fs, cgroup, "memory.min", &format!("{text:?}: {e}")))?
            };
            if min < want {
                return Ok(Some((cgroup.to_owned(), min)));
            }
        }
        cgroup = match cgroup.rsplit_once('/') {
            Some(("", _)) | None => "/",
            Some((parent, _)) => parent,
        };
    }
    Ok(None)
}

pub(crate) fn adopt_in(
    fs: &dyn CgroupFs,
    actions: &str,
    actions_memory_max: Option<u64>,
) -> Result<(), DelegateError> {
    require_v2(fs)?;
    let enabled = read(fs, actions, "cgroup.subtree_control")?;
    let file = fs.path(actions, "cgroup.subtree_control");
    require(actions, &enabled, "does not enable", || {
        format!(
            "enable them for its children (echo '+cpu +memory +pids' > {}), or leave \
             --cgroup-parent out and the daemon sets up its own",
            file.display()
        )
    })?;
    fs.writable(actions)
        .map_err(io_err(fs, "make lease cgroups in", actions, ""))?;
    if let Some(max) = actions_memory_max {
        write_memory_max(fs, actions, max)?;
    }
    Ok(())
}

pub(crate) fn capacity_in(fs: &dyn CgroupFs, actions: &str) -> Result<Capacity, DelegateError> {
    let mut capacity = Capacity::default();
    // The lowest `cpu.max` quota on the path, in CPUs.
    let mut quota: Option<u64> = None;
    let mut cgroup = actions;
    loop {
        if let Some(text) = read_if_there(fs, cgroup, "memory.max")? {
            let text = text.trim();
            if text != "max" {
                let max = text
                    .parse::<u64>()
                    .map_err(|e| invalid(fs, cgroup, "memory.max", &format!("{text:?}: {e}")))?;
                capacity.memory_bytes = Some(capacity.memory_bytes.map_or(max, |m| m.min(max)));
            }
        }
        if capacity.cpus.is_none()
            && let Some(text) = read_if_there(fs, cgroup, "cpuset.cpus.effective")?
            && !text.trim().is_empty()
        {
            let cpus = count_cpus(text.trim()).ok_or_else(|| {
                invalid(fs, cgroup, "cpuset.cpus.effective", &format!("{text:?}"))
            })?;
            capacity.cpus = Some(cpus);
        }
        if let Some(text) = read_if_there(fs, cgroup, "cpu.max")?
            && let Some(cpus) =
                quota_cpus(text.trim()).map_err(|reason| invalid(fs, cgroup, "cpu.max", &reason))?
        {
            quota = Some(quota.map_or(cpus, |q: u64| q.min(cpus)));
        }
        if cgroup == "/" {
            if let Some(quota) = quota {
                capacity.cpus = Some(capacity.cpus.map_or(quota, |c| c.min(quota)));
            }
            return Ok(capacity);
        }
        cgroup = match cgroup.rsplit_once('/') {
            Some(("", _)) | None => "/",
            Some((parent, _)) => parent,
        };
    }
}

/// The CPUs a `cpu.max` (`<quota|max> <period>`) allows, rounded down but at least 1;
/// `None` for `max`.
fn quota_cpus(text: &str) -> Result<Option<u64>, String> {
    let (quota, period) = text.split_once(' ').unwrap_or((text, "100000"));
    if quota == "max" {
        return Ok(None);
    }
    let number = |part: &str| {
        part.trim()
            .parse::<u64>()
            .map_err(|e| format!("{text:?}: {e}"))
    };
    let (quota, period) = (number(quota)?, number(period)?);
    if period == 0 {
        return Err(format!("{text:?}: a period of 0"));
    }
    Ok(Some((quota / period).max(1)))
}

/// The number of CPUs in a kernel CPU list (`0-3,8,10-11`).
fn count_cpus(list: &str) -> Option<u64> {
    let mut count = 0u64;
    for part in list.split(',') {
        let (first, last) = match part.split_once('-') {
            Some((a, b)) => (a.parse::<u64>().ok()?, b.parse::<u64>().ok()?),
            None => {
                let n = part.parse::<u64>().ok()?;
                (n, n)
            }
        };
        count += last.checked_sub(first)? + 1;
    }
    Some(count)
}

/// Refuses a mount without `cgroup.controllers` at its root: not cgroup v2.
fn require_v2(fs: &dyn CgroupFs) -> Result<(), DelegateError> {
    match fs.read("/", "cgroup.controllers") {
        Ok(_) => Ok(()),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            Err(DelegateError::NotV2(fs.path("/", "")))
        }
        Err(e) => Err(io_err(fs, "read", "/", "cgroup.controllers")(e)),
    }
}

/// Refuses `cgroup` when `listed` (a controller list) lacks one of [`CONTROLLERS`].
fn require(
    cgroup: &str,
    listed: &str,
    what: &'static str,
    fix: impl FnOnce() -> String,
) -> Result<(), DelegateError> {
    let have: Vec<&str> = listed.split_whitespace().collect();
    let missing: Vec<&'static str> = CONTROLLERS
        .into_iter()
        .filter(|c| !have.contains(c))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(DelegateError::Controllers {
        cgroup: cgroup.to_owned(),
        what,
        missing,
        listed: listed.trim().to_owned(),
        fix: fix(),
    })
}

/// Enables [`CONTROLLERS`] for `cgroup`'s children.
fn enable(fs: &dyn CgroupFs, cgroup: &str) -> io::Result<()> {
    let value: Vec<String> = CONTROLLERS.iter().map(|c| format!("+{c}")).collect();
    fs.write(cgroup, "cgroup.subtree_control", &value.join(" "))
}

fn write_memory_max(fs: &dyn CgroupFs, cgroup: &str, max: u64) -> Result<(), DelegateError> {
    fs.write(cgroup, "memory.max", &max.to_string())
        .map_err(io_err(fs, "write", cgroup, "memory.max"))
}

/// Makes `cgroup`; one already there is fine.
fn make(fs: &dyn CgroupFs, cgroup: &str) -> Result<(), DelegateError> {
    match fs.mkdir(cgroup) {
        Err(e) if e.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        other => other.map_err(io_err(fs, "make cgroup", cgroup, "")),
    }
}

fn read(fs: &dyn CgroupFs, cgroup: &str, file: &str) -> Result<String, DelegateError> {
    fs.read(cgroup, file)
        .map_err(io_err(fs, "read", cgroup, file))
}

/// `file` in `cgroup`, or `None` when the kernel shows none (its controller is not
/// enabled there, or the root has no such file).
fn read_if_there(
    fs: &dyn CgroupFs,
    cgroup: &str,
    file: &str,
) -> Result<Option<String>, DelegateError> {
    match fs.read(cgroup, file) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(io_err(fs, "read", cgroup, file)(e)),
    }
}

fn io_err<'a>(
    fs: &'a dyn CgroupFs,
    what: &'static str,
    cgroup: &'a str,
    file: &'a str,
) -> impl FnOnce(io::Error) -> DelegateError + 'a {
    move |source| DelegateError::Io {
        what,
        path: fs.path(cgroup, file),
        source,
    }
}

fn invalid(fs: &dyn CgroupFs, cgroup: &str, file: &str, why: &str) -> DelegateError {
    io_err(fs, "parse", cgroup, file)(io::Error::new(io::ErrorKind::InvalidData, why.to_owned()))
}

/// The cgroup `name` under `parent`.
fn child(parent: &str, name: &str) -> String {
    format!("{}/{name}", parent.trim_end_matches('/'))
}

#[cfg(test)]
#[path = "delegate_tests.rs"]
mod tests;
