//! An action's processes: which processes on the node belong to it, how much memory
//! they hold, and how they are all ended.
//!
//! The action's first process leads a new process group. A process belongs to the
//! action when it is in that group (and no older than its leader, so a recycled group
//! id is never taken for it), when it descends from a process that belongs, or when it
//! was seen belonging earlier and is still the same process (pid and start time): a
//! process that leaves the group (`setsid`) and is orphaned (its parent exits, so it
//! is reparented to `launchd` or `init`) is still the action's once a poll has seen it.
//! One that does both between two polls is not seen; a per-lease user would close
//! that gap, and is a follow-up.
//!
//! The pure part, [`Tracker`] and the text parsers, is tested on every platform. The
//! process table and per-process memory come from `/proc` on Linux and from `libproc`
//! on macOS ([`snapshot`], [`rusage`]).

use std::collections::{BTreeMap, BTreeSet};

/// One process in a snapshot of the process table.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub pgid: i32,
    /// When the process started, in a platform unit that only grows; with `pid` it
    /// names one process for its whole life.
    pub start: u64,
    /// Exited but not yet reaped: holds no memory and cannot be signalled further.
    pub zombie: bool,
}

/// The processes of one action, followed across snapshots.
#[derive(Debug)]
pub struct Tracker {
    /// The leader's pid, which is also the group's id.
    leader: i32,
    /// The leader's start time, once a snapshot has shown it.
    leader_start: Option<u64>,
    /// Every process seen belonging, by pid, with its start time.
    known: BTreeMap<i32, u64>,
    /// Every pid ever seen belonging, the leader's included: whose a sandbox denial is.
    seen: BTreeSet<i32>,
    /// Whether a snapshot has shown the group empty. An empty group's id can be
    /// recycled, so from then on membership is by descent and memory only.
    group_gone: bool,
    /// This process: never the action's, whatever a snapshot says.
    me: i32,
}

impl Tracker {
    /// A tracker for the action whose leader is `leader` (the pid of a process spawned
    /// into a new process group). `me` is the daemon's own pid.
    #[must_use]
    pub fn new(leader: i32, me: i32) -> Self {
        Self {
            leader,
            leader_start: None,
            known: BTreeMap::new(),
            seen: BTreeSet::from([leader]),
            group_gone: false,
            me,
        }
    }

    /// A tracker for an action an earlier daemon started, known only by its run record:
    /// the leader's pid and its start time ([`crate::record`]).
    #[must_use]
    pub fn resume(leader: i32, leader_start: u64, me: i32) -> Self {
        Self {
            leader_start: Some(leader_start),
            ..Self::new(leader, me)
        }
    }

    /// The leader's pid and process group id.
    #[must_use]
    pub fn leader(&self) -> i32 {
        self.leader
    }

    /// Every pid seen belonging to the action so far, and its leader's.
    #[must_use]
    pub fn seen(&self) -> &BTreeSet<i32> {
        &self.seen
    }

    /// The live processes of the action in `snapshot`, sorted by pid; every one of
    /// them, and every zombie of the action, is remembered for later snapshots.
    pub fn members(&mut self, snapshot: &[Proc]) -> Vec<Proc> {
        if self.leader_start.is_none() {
            self.leader_start = snapshot
                .iter()
                .find(|p| p.pid == self.leader)
                .map(|p| p.start);
        }
        let floor = self.leader_start.unwrap_or(0);
        // Another process under the leader's pid: the kernel hands out no pid that
        // still names a process group, so the action's group emptied first, and any
        // group of that id now is someone else's.
        if snapshot
            .iter()
            .any(|p| p.pid == self.leader && p.start != floor)
        {
            self.group_gone = true;
        }
        let mut children: BTreeMap<i32, Vec<&Proc>> = BTreeMap::new();
        for p in snapshot {
            children.entry(p.ppid).or_default().push(p);
        }
        let (gone, leader) = (self.group_gone, self.leader);
        let in_group = |p: &&Proc| !gone && p.pgid == leader && p.start >= floor;
        if !snapshot.iter().any(|p| in_group(&p)) {
            self.group_gone = true;
        }
        let mut pending: Vec<&Proc> = snapshot
            .iter()
            .filter(|p| in_group(p) || self.known.get(&p.pid) == Some(&p.start))
            .collect();
        let mut found: BTreeMap<i32, Proc> = BTreeMap::new();
        while let Some(p) = pending.pop() {
            if p.pid == self.me || p.pid <= 1 || found.insert(p.pid, *p).is_some() {
                continue;
            }
            if let Some(kids) = children.get(&p.pid) {
                pending.extend(kids.iter().copied());
            }
        }
        // Forget processes that are gone, so the map stays as small as the action.
        let alive: BTreeSet<(i32, u64)> = snapshot.iter().map(|p| (p.pid, p.start)).collect();
        self.known
            .retain(|pid, start| alive.contains(&(*pid, *start)));
        self.known.extend(found.values().map(|p| (p.pid, p.start)));
        self.seen.extend(found.keys());
        found.into_values().filter(|p| !p.zombie).collect()
    }
}

/// The fields of a Linux `/proc/<pid>/stat` line this module reads, or `None` when it
/// does not parse. The command name is in parentheses and may hold anything, so the
/// fields are counted from the last `)`.
#[must_use]
pub fn parse_linux_stat(text: &str) -> Option<Proc> {
    let (head, rest) = text.rsplit_once(')')?;
    let pid = head.split_once(" (")?.0.trim().parse().ok()?;
    let fields: Vec<&str> = rest.split_whitespace().collect();
    // After the name: state (3), ppid (4), pgrp (5), ... starttime (22).
    let state = *fields.first()?;
    Some(Proc {
        pid,
        ppid: fields.get(1)?.parse().ok()?,
        pgid: fields.get(2)?.parse().ok()?,
        start: fields.get(19)?.parse().ok()?,
        zombie: matches!(state, "Z" | "X"),
    })
}

/// The memory a Linux process holds that it alone pays for, from its
/// `/proc/<pid>/status`: anonymous and shared-memory pages resident, plus what is
/// swapped out (`RssAnon + RssShmem + VmSwap`), in bytes. The nearest Linux measure
/// to macOS's physical footprint. `None` without those lines (a kernel thread, a
/// zombie).
#[must_use]
pub fn parse_linux_status(text: &str) -> Option<u64> {
    let kib = |key: &str| {
        text.lines().find_map(|line| {
            let rest = line.strip_prefix(key)?.strip_prefix(':')?;
            rest.trim().strip_suffix("kB")?.trim().parse::<u64>().ok()
        })
    };
    let anon = kib("RssAnon")?;
    let total = anon + kib("RssShmem").unwrap_or(0) + kib("VmSwap").unwrap_or(0);
    Some(total * 1024)
}

/// The arguments of a process as one line, from the NUL-separated `/proc/<pid>/cmdline`
/// (or the argument strings after the path in a macOS `KERN_PROCARGS2` block): the
/// strings joined by a space, lossily decoded.
#[must_use]
pub fn join_args(raw: &[u8]) -> String {
    let raw = raw.strip_suffix(&[0]).unwrap_or(raw);
    String::from_utf8_lossy(raw).replace('\0', " ")
}

/// The program name and the arguments from a macOS `KERN_PROCARGS2` block: an `int`
/// argument count, the executable path and its NUL padding, then `argc` argument
/// strings (the environment follows them and is not read). `None` for a block too
/// short to hold the count.
#[must_use]
pub fn parse_procargs2(raw: &[u8]) -> Option<(String, String)> {
    let argc = u32::from_ne_bytes(raw.get(..4)?.try_into().ok()?) as usize;
    let rest = &raw[4..];
    let path_end = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
    let path = String::from_utf8_lossy(&rest[..path_end]);
    let name = path.rsplit('/').next().unwrap_or_default().to_owned();
    let after = &rest[path_end..];
    let first = after.iter().position(|&b| b != 0).unwrap_or(after.len());
    let args: Vec<String> = (after[first..].split(|&b| b == 0).take(argc))
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect();
    Some((name, args.join(" ")))
}

/// What one read of a process gives: its footprint now, and, where the platform keeps
/// them, the counters of its whole life ([`crate::usage`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rusage {
    /// Physical footprint now, in bytes.
    pub footprint: u64,
    /// High-water mark and CPU time of the process so far: macOS only.
    pub lifetime: Option<crate::usage::Lifetime>,
}

/// The folders of the macOS system volume that hold the helpers the system starts as a
/// user (`diskimages-helper` for an attached disk image, for one).
pub const SYSTEM_FOLDERS: [&str; 3] = ["/System/", "/usr/libexec/", "/usr/sbin/"];

/// Whether `path`, the executable of a process, is a system binary: under
/// [`SYSTEM_FOLDERS`]. Such a process is the system's, not the action's, and does not
/// end on SIGTERM; it is left out of the SIGTERM wait and the survivor report (the
/// SIGKILL still takes it). An action cannot make its own binary look like one: those
/// folders are on the sealed, read-only system volume, which no user (root included)
/// can write to, and a path is compared as `proc_pidpath` gives it, never as an
/// argument the action chose.
#[must_use]
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn is_system_binary(path: &str) -> bool {
    SYSTEM_FOLDERS.iter().any(|folder| path.starts_with(folder))
}

pub use platform::{command_of, process, rusage, snapshot};
#[cfg(target_os = "macos")]
pub use platform::{executable_of, timebase};

#[cfg(target_os = "linux")]
mod platform {
    use super::{Proc, Rusage, parse_linux_stat, parse_linux_status};

    /// Every process in `/proc` that could be read. One that exits while the table
    /// is read is left out.
    pub fn snapshot() -> std::io::Result<Vec<Proc>> {
        let mut procs = Vec::new();
        for entry in std::fs::read_dir("/proc")? {
            let name = entry?.file_name();
            let Some(pid) = name
                .to_str()
                .filter(|n| n.bytes().all(|b| b.is_ascii_digit()))
            else {
                continue;
            };
            if let Some(p) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
                .ok()
                .as_deref()
                .and_then(parse_linux_stat)
            {
                procs.push(p);
            }
        }
        Ok(procs)
    }

    /// Process `pid`, as a snapshot would show it; `None` once it is gone.
    #[must_use]
    pub fn process(pid: i32) -> Option<Proc> {
        std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .as_deref()
            .and_then(parse_linux_stat)
    }

    /// The name and the arguments (one line) of `pid`, from `/proc`; `None` once it is
    /// gone. A process with no arguments (a kernel thread, a zombie) has an empty line.
    #[must_use]
    pub fn command_of(pid: i32) -> Option<(String, String)> {
        let name = std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()?;
        let args = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
        Some((name.trim_end().to_owned(), super::join_args(&args)))
    }

    /// What `pid` holds, in bytes; `None` once it is gone. No lifetime counters: the
    /// driver reports usage on macOS only.
    #[must_use]
    pub fn rusage(pid: i32) -> Option<Rusage> {
        std::fs::read_to_string(format!("/proc/{pid}/status"))
            .ok()
            .as_deref()
            .and_then(parse_linux_status)
            .map(|footprint| Rusage {
                footprint,
                lifetime: None,
            })
    }
}

#[cfg(target_os = "macos")]
mod platform {
    use super::{Proc, Rusage};
    use crate::usage::Lifetime;

    /// `pbi_status` of a process that has exited and waits to be reaped.
    const SZOMB: u32 = 5;

    /// Every process `libproc` lists whose BSD info could be read. One that exits
    /// while the table is read is left out.
    pub fn snapshot() -> std::io::Result<Vec<Proc>> {
        // Room for more pids than the kernel reports, so a few new ones still fit.
        // SAFETY: a null buffer asks only for the count.
        let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
        if count < 0 {
            return Err(std::io::Error::last_os_error());
        }
        let mut pids = vec![0 as libc::pid_t; count.unsigned_abs() as usize + 64];
        let bytes =
            i32::try_from(std::mem::size_of_val(pids.as_slice())).map_err(std::io::Error::other)?;
        // SAFETY: the buffer holds `bytes` bytes of pids and outlives the call.
        let n = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), bytes) };
        if n < 0 {
            return Err(std::io::Error::last_os_error());
        }
        pids.truncate(n.unsigned_abs() as usize);
        Ok(pids.into_iter().filter_map(process).collect())
    }

    /// Process `pid`, as a snapshot would show it; `None` once it is gone.
    #[must_use]
    pub fn process(pid: libc::pid_t) -> Option<Proc> {
        let mut info = std::mem::MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        let size = i32::try_from(std::mem::size_of::<libc::proc_bsdinfo>()).ok()?;
        // SAFETY: `info` is a writable proc_bsdinfo of `size` bytes; the call writes
        // at most that many and returns how many it wrote.
        let wrote = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDTBSDINFO,
                0,
                info.as_mut_ptr().cast(),
                size,
            )
        };
        if wrote != size {
            return None;
        }
        // SAFETY: the call filled the whole struct (checked above).
        let info = unsafe { info.assume_init() };
        Some(Proc {
            pid,
            ppid: i32::try_from(info.pbi_ppid).ok()?,
            pgid: i32::try_from(info.pbi_pgid).ok()?,
            start: info.pbi_start_tvsec * 1_000_000 + info.pbi_start_tvusec,
            zombie: info.pbi_status == SZOMB,
        })
    }

    /// The path of the executable `pid` runs (`proc_pidpath`); `None` once it is gone.
    #[must_use]
    pub fn executable_of(pid: i32) -> Option<String> {
        let mut path = vec![0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        let size = u32::try_from(path.len()).ok()?;
        // FFI-BOUNDARY: `path` is this frame's; proc_pidpath writes at most `size` bytes
        // into it and keeps no pointer past the call.
        // SAFETY: `path` holds `size` writable bytes; the pointer outlives the call.
        let len = unsafe { libc::proc_pidpath(pid, path.as_mut_ptr().cast(), size) };
        let len = usize::try_from(len).ok().filter(|len| *len > 0)?;
        path.truncate(len);
        String::from_utf8(path).ok()
    }

    /// The name and the arguments (one line) of `pid` (`KERN_PROCARGS2`); `None` once it
    /// is gone or when the kernel does not show them to this user.
    #[must_use]
    pub fn command_of(pid: i32) -> Option<(String, String)> {
        let mut limit: libc::c_int = 0;
        let mut size = std::mem::size_of::<libc::c_int>();
        let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
        // SAFETY: `limit` is a writable int of `size` bytes, the length KERN_ARGMAX
        // answers with; the pointers outlive the call.
        let done = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                2,
                (&raw mut limit).cast(),
                &raw mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if done != 0 || limit <= 0 {
            return None;
        }
        let mut buffer = vec![0u8; usize::try_from(limit).ok()?];
        let mut size = buffer.len();
        let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
        // SAFETY: `buffer` holds `size` writable bytes, which the call shortens to what it
        // wrote; the pointers outlive the call.
        let done = unsafe {
            libc::sysctl(
                mib.as_mut_ptr(),
                3,
                buffer.as_mut_ptr().cast(),
                &raw mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if done != 0 {
            return None;
        }
        buffer.truncate(size);
        super::parse_procargs2(&buffer)
    }

    /// The physical footprint of `pid` (what Activity Monitor calls its memory), in
    /// bytes, with its lifetime high-water mark and CPU time; `None` once it is gone.
    #[must_use]
    pub fn rusage(pid: i32) -> Option<Rusage> {
        let mut info = std::mem::MaybeUninit::<libc::rusage_info_v4>::zeroed();
        // FFI-BOUNDARY: `info` is this stack frame's; proc_pid_rusage writes the
        // RUSAGE_INFO_V4 struct (<libproc.h>, <sys/resource.h>; `libc` declares the
        // same layout as `rusage_info_v4`) into it and keeps no pointer past the call.
        // SAFETY: `info` is a writable rusage_info_v4, the struct RUSAGE_INFO_V4 fills;
        // the pointer outlives the call.
        let done =
            unsafe { libc::proc_pid_rusage(pid, libc::RUSAGE_INFO_V4, info.as_mut_ptr().cast()) };
        if done != 0 {
            return None;
        }
        // SAFETY: the call succeeded, so it filled the struct.
        let info = unsafe { info.assume_init() };
        Some(Rusage {
            footprint: info.ri_phys_footprint,
            lifetime: Some(Lifetime {
                max_footprint: info.ri_lifetime_max_phys_footprint,
                user_ticks: info.ri_user_time,
                system_ticks: info.ri_system_time,
                child_user_ticks: info.ri_child_user_time,
                child_system_ticks: info.ri_child_system_time,
                ppid: process(pid).map_or(0, |p| p.ppid),
            }),
        })
    }

    /// The machine's mach timebase, `(numer, denom)`: a tick of mach absolute time is
    /// `numer / denom` nanoseconds. `(1, 1)` if the kernel refuses (it does not).
    #[must_use]
    pub fn timebase() -> (u32, u32) {
        /// `struct mach_timebase_info` of <mach/mach_time.h>: two `uint32_t`. Declared
        /// here because `libc` marks its copy deprecated.
        #[repr(C)]
        struct Timebase {
            numer: u32,
            denom: u32,
        }
        unsafe extern "C" {
            // <mach/mach_time.h>: fills `info`, returns KERN_SUCCESS (0).
            fn mach_timebase_info(info: *mut Timebase) -> libc::c_int;
        }
        let mut info = Timebase { numer: 0, denom: 0 };
        // FFI-BOUNDARY: `info` is this frame's; the call fills it and keeps no pointer.
        // SAFETY: `info` is a writable struct of the layout the declaration above gives
        // (two u32, as in <mach/mach_time.h>).
        let done = unsafe { mach_timebase_info(&raw mut info) };
        if done == 0 && info.denom != 0 {
            (info.numer, info.denom)
        } else {
            (1, 1)
        }
    }
}

/// Sends SIGKILL to the action's process group, unless a snapshot has shown it empty,
/// and to each of `members`. A process that is already gone is not an error.
///
/// Known race: a pid (or the group id) is checked in a snapshot and signalled a moment
/// later by number. A member that exits in between and whose pid the kernel hands to
/// a new process of the same user gets that process killed instead; the daemon's user
/// only, since `kill` cannot reach another user's processes. Closing it needs a
/// handle that cannot be recycled (`pidfd_send_signal` on Linux; macOS has none for
/// arbitrary processes) or a per-lease user whose every process may be killed. The
/// window is the time between the snapshot and the signal.
///
/// Whatever a tracker or a run record claims, the daemon never signals a group id of 1
/// or less (every process of its user, or init's group), its own group or its parent's
/// group, nor its parent ([`Guards`]).
pub fn kill_all(tracker: &Tracker, members: &[Proc]) {
    signal_all(tracker, members, libc::SIGKILL);
}

/// [`kill_all`] with SIGTERM: asks the action's processes to end, and leaves the ones
/// that ignore it to a later [`kill_all`].
pub fn term_all(tracker: &Tracker, members: &[Proc]) {
    signal_all(tracker, members, libc::SIGTERM);
}

fn signal_all(tracker: &Tracker, members: &[Proc], signal: libc::c_int) {
    let guards = Guards::now();
    if guards.may_signal_group_of(tracker) {
        // SAFETY: kill(2) takes plain integers. The group id is the leader's pid, and
        // the last snapshot showed the group still had members.
        unsafe {
            libc::kill(-tracker.leader(), signal);
        }
    }
    for p in members.iter().filter(|p| guards.may_kill(p)) {
        // SAFETY: as above; `p` was in the snapshot just taken, pid and start time.
        unsafe {
            libc::kill(p.pid, signal);
        }
    }
}

/// What the daemon never signals: its own process group, its parent and its parent's
/// group.
#[derive(Clone, Copy, Debug)]
pub struct Guards {
    pub own_group: i32,
    pub parent: i32,
    pub parent_group: i32,
}

impl Guards {
    /// This process's.
    #[must_use]
    pub fn now() -> Self {
        // SAFETY: getpgrp and getppid take nothing; getpgid takes a plain integer and
        // fails (-1) for a parent that is gone, which no group id equals.
        unsafe {
            let parent = libc::getppid();
            Self {
                own_group: libc::getpgrp(),
                parent,
                parent_group: libc::getpgid(parent),
            }
        }
    }

    /// Whether group `pgid` may be signalled as an action's.
    #[must_use]
    pub fn may_signal_group(self, pgid: i32) -> bool {
        pgid > 1 && pgid != self.own_group && pgid != self.parent_group
    }

    /// Whether `tracker`'s group may be signalled by its id: not once a snapshot has
    /// shown it empty (the id may be someone else's by then), and not a refused group.
    #[must_use]
    pub fn may_signal_group_of(self, tracker: &Tracker) -> bool {
        !tracker.group_gone && self.may_signal_group(tracker.leader())
    }

    /// Whether process `p` may be signalled as an action's: not the parent, and not in a
    /// group [`Guards::may_signal_group`] refuses.
    #[must_use]
    pub fn may_kill(self, p: &Proc) -> bool {
        p.pid != self.parent && self.may_signal_group(p.pgid)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: i32, ppid: i32, pgid: i32, start: u64) -> Proc {
        Proc {
            pid,
            ppid,
            pgid,
            start,
            zombie: false,
        }
    }

    /// Catches a system helper counted as the action's (it ignores SIGTERM: every lease
    /// that attached a disk image would wait the whole grace and name
    /// `diskimages-helper`), and a path that only looks like a system folder (a sibling
    /// folder, a relative path, a lease's own directory) taken for one.
    #[test]
    fn a_binary_under_the_system_folders_is_the_systems() {
        for system in [
            "/System/Library/PrivateFrameworks/DiskImages.framework/Resources/diskimages-helper",
            "/usr/libexec/xpcproxy",
            "/usr/sbin/cfprefsd",
        ] {
            assert!(is_system_binary(system), "{system}");
        }
        for other in [
            "",
            "/usr/bin/tar",
            "/usr/local/libexec/tool",
            "/usr/libexec",
            "/System",
            "/Systems/x",
            "/usr/sbinx/tool",
            "usr/sbin/tool",
            "/private/var/folders/x/System/tool",
            "/private/var/slot/System/tool",
        ] {
            assert!(!is_system_binary(other), "{other}");
        }
    }

    fn pids(procs: &[Proc]) -> Vec<i32> {
        procs.iter().map(|p| p.pid).collect()
    }

    /// Catches a denial attributed to the wrong pids: a pid that came and went between
    /// two snapshots forgotten (the sandbox log names it after it is gone), the leader
    /// missing before any snapshot shows it, and an unrelated process or the daemon
    /// taken for the action's.
    #[test]
    fn every_pid_ever_seen_belonging_is_remembered() {
        let me = 10;
        let mut t = Tracker::new(100, me);
        assert_eq!(t.seen(), &BTreeSet::from([100]));
        let first = [
            p(me, 1, me, 5),
            p(100, me, 100, 50),
            p(101, 100, 100, 51),
            p(200, 1, 200, 1),
        ];
        t.members(&first);
        // 101 has ended by now; 102 is new.
        t.members(&[p(me, 1, me, 5), p(100, me, 100, 50), p(102, 100, 100, 60)]);
        assert_eq!(t.seen(), &BTreeSet::from([100, 101, 102]));
    }

    /// Catches: a child that left the group (`setsid`) not followed, a grandchild
    /// reparented to init once seen not followed, a recycled pid or group id taken for
    /// the action's, the daemon itself or init counted, and zombies counted as live.
    #[test]
    fn the_tree_is_the_group_its_descendants_and_what_was_seen() {
        let me = 10;
        let mut t = Tracker::new(100, me);
        assert_eq!(t.leader(), 100);
        let first = [
            p(1, 0, 1, 0),
            p(me, 1, me, 5),
            p(100, me, 100, 50),
            p(101, 100, 100, 51),
            // Left the group with setsid, still the leader's child.
            p(102, 100, 102, 52),
            p(103, 102, 102, 53),
            // Unrelated, older, in a group of its own.
            p(200, 1, 200, 1),
        ];
        assert_eq!(pids(&t.members(&first)), [100, 101, 102, 103]);
        // The leader and 102 exit; 103 is reparented to init; 104 is a fresh process
        // that happens to reuse pid 101 (another start time) outside the tree.
        let later = [
            p(1, 0, 1, 0),
            p(me, 1, me, 5),
            p(101, 1, 101, 90),
            p(103, 1, 102, 53),
            p(105, 103, 102, 91),
            Proc {
                zombie: true,
                ..p(106, 103, 102, 92)
            },
            p(200, 1, 200, 1),
        ];
        assert_eq!(pids(&t.members(&later)), [103, 105]);
        // A group id recycled by an older process is not the action's group.
        let mut t = Tracker::new(300, me);
        let recycled = [p(300, me, 300, 70), p(301, 1, 300, 60)];
        assert_eq!(pids(&t.members(&recycled)), [300]);
        // Before the leader is seen, the group is taken at its word; the daemon and
        // init never belong, whatever group they claim.
        let mut t = Tracker::new(400, me);
        let odd = [p(401, 1, 400, 3), p(me, 1, 400, 5), p(1, 0, 400, 0)];
        assert_eq!(pids(&t.members(&odd)), [401]);
        // Once the group has been seen empty, a process that later claims its id is
        // not taken for the action's.
        assert!(t.members(&[p(1, 0, 1, 0)]).is_empty());
        assert!(t.group_gone);
        assert!(t.members(&[p(400, 1, 400, 99)]).is_empty());
    }

    /// Catches: a stat line misread when the command name holds spaces or a `)`, the
    /// wrong field taken for ppid, pgrp or start time, or a zombie read as live.
    #[test]
    fn linux_stat_lines_parse_from_the_last_parenthesis() {
        let line = "4242 (we) ird (name) S 4241 4240 4240 0 -1 4194560 0 0 0 0 0 0 0 0 20 0 1 0 987654 0 0";
        assert_eq!(
            parse_linux_stat(line),
            Some(Proc {
                pid: 4242,
                ppid: 4241,
                pgid: 4240,
                start: 987_654,
                zombie: false
            })
        );
        let zombie = "7 (z) Z 1 7 7 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 55 0 0";
        assert!(parse_linux_stat(zombie).expect("zombie").zombie);
        for bad in [
            "",
            "7 (z) Z",
            "x (z) S 1 7 7 0 -1 0 0 0 0 0 0 0 0 0 20 0 1 0 55",
            "7 z S 1",
        ] {
            assert_eq!(parse_linux_stat(bad), None, "{bad:?}");
        }
    }

    /// Catches: memory read from VmRSS (file pages the action does not pay for) or in
    /// kB instead of bytes, swap left out, or a process without RssAnon counted as 0.
    #[test]
    fn linux_status_counts_anonymous_shared_and_swapped_memory() {
        let status = "Name:\tperl\nVmRSS:\t  9000 kB\nRssAnon:\t  100 kB\nRssFile:\t 8000 kB\nRssShmem:\t 20 kB\nVmSwap:\t 3 kB\n";
        assert_eq!(parse_linux_status(status), Some(123 * 1024));
        assert_eq!(parse_linux_status("RssAnon:\t5 kB\n"), Some(5 * 1024));
        assert_eq!(parse_linux_status("Name:\tkthreadd\n"), None);
    }

    /// Catches a record or tracker that gets the daemon to signal every process of its
    /// user (group id 0, -1 and the like), init's group, its own group, its parent or its
    /// parent's group.
    #[test]
    fn the_daemon_never_signals_itself_its_parent_or_everyone() {
        let g = Guards {
            own_group: 50,
            parent: 40,
            parent_group: 30,
        };
        for refused in [-7, 0, 1, 50, 30] {
            assert!(!g.may_signal_group(refused), "{refused}");
        }
        assert!(g.may_signal_group(60));
        assert!(g.may_signal_group_of(&Tracker::new(60, 2)));
        assert!(
            !g.may_signal_group_of(&Tracker::new(50, 2)),
            "its own group"
        );
        assert!(!g.may_kill(&p(40, 1, 60, 1)), "the parent");
        assert!(!g.may_kill(&p(61, 40, 50, 1)), "in the daemon's own group");
        assert!(g.may_kill(&p(61, 40, 60, 1)));
        let now = Guards::now();
        // SAFETY: getpgrp and getppid take nothing and cannot fail.
        assert_eq!(now.own_group, unsafe { libc::getpgrp() });
        assert_eq!(now.parent, unsafe { libc::getppid() });
    }

    /// Catches: SIGTERM sent as SIGKILL (a process that ignores SIGTERM would die), and
    /// SIGTERM not sent at all: a `sleep` dies of it with the signal's number, and the
    /// group is signalled by the leader's id like the kill is.
    #[test]
    fn term_all_sends_sigterm_to_the_group_and_not_sigkill() {
        use std::os::unix::process::{CommandExt as _, ExitStatusExt as _};
        let mut leader = std::process::Command::new("/bin/sleep")
            .arg("300")
            .process_group(0)
            .spawn()
            .expect("spawn");
        let pid = i32::try_from(leader.id()).expect("pid");
        let me = i32::try_from(std::process::id()).expect("pid");
        let mut t = Tracker::new(pid, me);
        let members = t.members(&process(pid).into_iter().collect::<Vec<_>>());
        assert_eq!(members.len(), 1);
        term_all(&t, &members);
        let status = leader.wait().expect("reap");
        assert_eq!(status.signal(), Some(libc::SIGTERM));
    }

    /// Catches: arguments run together (a NUL kept or dropped), a last argument that
    /// keeps its terminator, and lossy text refused.
    #[test]
    fn arguments_are_joined_by_spaces() {
        assert_eq!(join_args(b"sleep\x00300\x00"), "sleep 300");
        assert_eq!(join_args(b""), "");
        assert_eq!(join_args(b"a\xffb\0"), "a\u{fffd}b");
    }

    /// Catches: the path's NUL padding read as arguments, more arguments than the count
    /// (the environment follows them), the name taken from the wrong end of the path,
    /// and a block too short for its count read anyway.
    #[test]
    fn a_procargs_block_gives_the_name_and_the_counted_arguments() {
        let mut block = 2_u32.to_ne_bytes().to_vec();
        block.extend_from_slice(b"/usr/bin/foo\0\0\0foo\0bar baz\0HOME=/x\0");
        assert_eq!(
            parse_procargs2(&block),
            Some(("foo".to_owned(), "foo bar baz".to_owned()))
        );
        assert_eq!(parse_procargs2(&[1, 0]), None);
        let mut bare = 0_u32.to_ne_bytes().to_vec();
        bare.extend_from_slice(b"/bin/x");
        assert_eq!(
            parse_procargs2(&bare),
            Some(("x".to_owned(), String::new()))
        );
    }

    /// Catches: a command read of the wrong process, one that is empty for a live
    /// process, and one invented for a pid that does not exist.
    #[test]
    fn the_command_of_this_process_is_read() {
        let me = i32::try_from(std::process::id()).expect("pid");
        let (name, args) = command_of(me).expect("this process");
        assert!(!name.is_empty());
        assert!(args.contains(&name[..name.len().min(8)]), "{name} {args}");
        assert_eq!(command_of(i32::MAX), None);
    }

    /// Catches a group signalled by its id once a snapshot showed it empty (the id may
    /// be someone else's by then): the "ignore `group_gone`" defect.
    #[test]
    fn a_group_seen_empty_is_not_signalled_by_its_id() {
        use std::os::unix::process::CommandExt as _;
        // Waits for its stdin to close and exits 0 by itself: a signal shows as how it ended.
        let mut other = std::process::Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::null())
            .process_group(0)
            .spawn()
            .expect("spawn");
        let pid = i32::try_from(other.id()).expect("pid");
        let me = i32::try_from(std::process::id()).expect("pid");
        let mut t = Tracker::new(pid, me);
        assert!(t.members(&[]).is_empty(), "seen empty");
        kill_all(&t, &[]);
        drop(other.stdin.take());
        let status = other.wait().expect("reap");
        assert_eq!(status.code(), Some(0), "the group was signalled by its id");
    }

    /// Catches: a snapshot that misses this very process or reads its parent wrong,
    /// and a footprint that is zero or missing for a live process.
    #[test]
    fn this_process_is_in_the_snapshot_with_a_footprint() {
        let me = i32::try_from(std::process::id()).expect("pid");
        let snapshot = snapshot().expect("snapshot");
        let mine = snapshot.iter().find(|p| p.pid == me).expect("this process");
        // SAFETY: getppid takes nothing and cannot fail.
        assert_eq!(mine.ppid, unsafe { libc::getppid() });
        assert!(!mine.zombie);
        assert!(rusage(me).expect("footprint").footprint > 0);
        assert_eq!(rusage(-5), None);
    }
}
