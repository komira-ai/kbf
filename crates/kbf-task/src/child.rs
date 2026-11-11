//! What the process that becomes the Command does between `fork` and `exec` on Linux: the
//! namespace steps of the network layer ([`crate::network`]) and of the sandbox
//! ([`crate::sandbox`]) as one list, the two-byte report to the parent, and the processes that
//! keep the Command off PID 1 of its namespace ([`chain`]).
//!
//! It all runs in a child of a multi-threaded process before `exec`, so it makes only system
//! calls on memory made before the fork: no allocation, no lock, no C library function that may
//! take one. A new process is made with the `clone` system call itself ([`fork`]), not the C
//! library's `fork`, whose handlers take locks a thread of the parent may have held.
//!
//! # The steps
//!
//! [`plan`] lists them, each with the number the report names it by ([`STEPS`]):
//!
//! - for a Command that is not root: dumpable on, `unshare` of a user, mount and PID namespace
//!   (and a network one for a Command denied the network), `setgroups` denied, the user and group
//!   id mapped to themselves, dumpable off, then `lo` raised (network namespace only);
//! - for root: `unshare` of a mount and PID namespace only (a user namespace that maps one id would
//!   make `chown`, `setuid` and `setgroups` fail for it);
//! - then the sandbox's mount steps ([`crate::sandbox`]).
//!
//! They run in order up to the first that fails ([`Plan::apply`]); what came before it holds and
//! nothing after it is done. The child reports the failed step and its `errno` (or that none
//! failed) on a pipe the parent reads once the Command has been started ([`Report::read`]).

use std::ffi::{CStr, CString};
use std::io::{self, Read as _};
use std::os::fd::{AsRawFd as _, OwnedFd, RawFd};

use rustix::fs::{Mode, OFlags};
use rustix::mount::{MountFlags, MountPropagationFlags};
use rustix::process::{DumpableBehavior, Pid, Resource, Signal, WaitOptions, WaitStatus};

use crate::network::Layers;
use crate::sandbox::Sandbox;

/// No step failed.
pub const OK: u8 = 0;
pub const UNSHARE: u8 = 1;
pub const SETGROUPS: u8 = 2;
pub const UID_MAP: u8 = 3;
pub const GID_MAP: u8 = 4;
pub const LOOPBACK: u8 = 5;
pub const DUMPABLE: u8 = 7;
pub const PRIVATE: u8 = 8;
pub const COVER: u8 = 9;
pub const BIND: u8 = 10;
pub const MKDIR: u8 = 11;
pub const KEEP: u8 = 12;
pub const SEAL: u8 = 13;
pub const CHDIR: u8 = 14;
pub const HOLD: u8 = 15;

/// The steps of the sandbox's mount namespace are numbered from here: a failure of one of them
/// leaves the network namespace whole.
pub const FIRST_MOUNT_STEP: u8 = PRIVATE;

/// The step numbers the child reports, with the names the log gives them.
pub const STEPS: [(u8, &str); 14] = [
    (UNSHARE, "unshare"),
    (SETGROUPS, "setgroups"),
    (UID_MAP, "uid_map"),
    (GID_MAP, "gid_map"),
    (LOOPBACK, "loopback"),
    (DUMPABLE, "dumpable"),
    (PRIVATE, "private"),
    (COVER, "cover"),
    (BIND, "bind"),
    (MKDIR, "mkdir"),
    (KEEP, "keep"),
    (SEAL, "seal"),
    (CHDIR, "chdir"),
    (HOLD, "hold"),
];

/// The name of step `number`, if it is one.
pub fn step_name(number: u8) -> Option<&'static str> {
    STEPS
        .iter()
        .find(|(n, _)| *n == number)
        .map(|(_, name)| *name)
}

/// A system call result as an error: a negative result is the `errno` the call left.
pub fn cvt(result: libc::c_long) -> io::Result<libc::c_long> {
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result)
    }
}

/// [`cvt`] for a call that returns a C `int`.
fn cvt_int(result: libc::c_int) -> io::Result<libc::c_int> {
    cvt(libc::c_long::from(result)).map(|_| result)
}

/// The code a step's error is reported as: its `errno`, at most 255.
pub fn code(error: &io::Error) -> u8 {
    u8::try_from(error.raw_os_error().unwrap_or(0)).unwrap_or(u8::MAX)
}

/// `PR_SET_DUMPABLE`: on or off.
///
/// # Errors
/// The `prctl` failed.
pub fn set_dumpable(on: bool) -> io::Result<()> {
    let behaviour = if on {
        DumpableBehavior::Dumpable
    } else {
        DumpableBehavior::NotDumpable
    };
    Ok(rustix::process::set_dumpable_behavior(behaviour)?)
}

/// One thing the child does to its own process.
#[derive(Debug)]
pub enum Step {
    Dumpable(bool),
    /// `unshare` with these `CLONE_NEW*` flags.
    Unshare(libc::c_int),
    /// The bytes written, in one `write`, to the file at the path.
    Write(CString, Vec<u8>),
    Loopback,
    /// Every mount of the new mount namespace made private, so nothing done below reaches the
    /// namespace it was copied from.
    Private,
    /// The directory opened (`O_PATH`, not following a symlink in its place) and moved onto
    /// `fd`, a descriptor the parent reserved (and closes once the process has started): a
    /// bind's source, named `/proc/self/fd/<fd>` from then on whatever covers its path. It must
    /// be opened after the `unshare`: a mount of the namespace the parent is in cannot be the
    /// source of a bind made in another.
    Hold {
        dir: CString,
        fd: OwnedFd,
    },
    /// An empty tmpfs mounted on the directory.
    Cover(CString),
    /// The directory `from` (a `/proc/self/fd/<n>` path) bound at `to`, not recursively.
    Bind {
        from: CString,
        to: CString,
    },
    /// The directory made, mode 0755; one that exists is no failure.
    Mkdir(CString),
    /// The mount on the directory (a cover) made read-only.
    Seal(CString),
    /// The working directory entered again, in the namespace as it now is.
    Chdir(CString),
}

/// What the child needs, made before the fork.
pub struct Plan {
    /// A network namespace is among the namespaces made.
    netns: bool,
    /// A user namespace is among them (the Command is not root).
    userns: bool,
    /// The process group of the tool, which the process outside the PID namespace joins
    /// ([`chain`]).
    group: Pid,
    steps: Vec<(u8, Step)>,
    write_end: RawFd,
}

/// The parent's end of the pipe the child reports on.
pub struct Report {
    read: OwnedFd,
    // Held so that the child's copy is the only one once the parent lets this go.
    write: OwnedFd,
    /// The Command is denied the network.
    denied: bool,
    /// The Command runs as root, which gets no user or network namespace.
    root: bool,
}

/// How the steps went, in the child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Outcome {
    /// The step that failed, or [`OK`].
    pub step: u8,
    /// Its `errno` (0 for [`OK`]).
    pub errno: u8,
    /// The `unshare` step succeeded: a PID namespace exists for the children of this process,
    /// and its first child must not be the Command ([`chain`]).
    pub unshared: bool,
}

/// Whether the namespace layer is left out for a Command run as `euid`: root, for which a
/// namespace that maps only its own id (`0 0 1`) breaks `chown` to another user, `setuid`,
/// `setgid` and `setgroups`, which `tar`, `dpkg` and `apt` do.
pub fn skips_namespace(euid: u32) -> bool {
    euid == 0
}

/// A pipe whose two ends close on `exec`.
///
/// # Errors
/// The `pipe2` failed.
pub fn make_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    Ok(rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?)
}

/// The plan for a Command run as `uid` and `gid` (the effective ids, read once before the
/// fork), denied the network or not, with the sandbox's `mounts` after the namespace steps, and
/// the pipe it reports on, made by `pipe`. `group` is the tool's own process group.
///
/// # Errors
/// `pipe` failed.
pub fn plan(
    uid: u32,
    gid: u32,
    denied: bool,
    group: Pid,
    mounts: Vec<(u8, Step)>,
    pipe: fn() -> io::Result<(OwnedFd, OwnedFd)>,
) -> io::Result<(Plan, Report)> {
    let (read, write) = pipe()?;
    let root = skips_namespace(uid);
    let netns = denied && !root;
    let mut steps = if root {
        vec![(
            UNSHARE,
            Step::Unshare(libc::CLONE_NEWNS | libc::CLONE_NEWPID),
        )]
    } else {
        let net = if netns { libc::CLONE_NEWNET } else { 0 };
        vec![
            (DUMPABLE, Step::Dumpable(true)),
            (
                UNSHARE,
                Step::Unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS | libc::CLONE_NEWPID | net),
            ),
            (
                SETGROUPS,
                Step::Write(c"/proc/self/setgroups".to_owned(), b"deny".to_vec()),
            ),
            (
                UID_MAP,
                Step::Write(
                    c"/proc/self/uid_map".to_owned(),
                    format!("{uid} {uid} 1").into_bytes(),
                ),
            ),
            (
                GID_MAP,
                Step::Write(
                    c"/proc/self/gid_map".to_owned(),
                    format!("{gid} {gid} 1").into_bytes(),
                ),
            ),
            // The `/proc/self` writes are done: back to non-dumpable before anything else.
            (DUMPABLE, Step::Dumpable(false)),
        ]
    };
    if netns {
        steps.push((LOOPBACK, Step::Loopback));
    }
    steps.extend(mounts);
    let plan = Plan {
        netns,
        userns: !root,
        group,
        steps,
        write_end: write.as_raw_fd(),
    };
    Ok((
        plan,
        Report {
            read,
            write,
            denied,
            root,
        },
    ))
}

impl Plan {
    /// The steps, each with the number it is reported by, in the order they run.
    #[cfg(test)]
    pub fn steps(&self) -> &[(u8, Step)] {
        &self.steps
    }

    /// After the steps and the report, when the `unshare` succeeded: the processes that keep the
    /// Command off PID 1 ([`chain`]). Returns only in the process that is to become the Command.
    ///
    /// # Errors
    /// As [`chain`].
    pub fn chain(&self, leave: Leave) -> io::Result<()> {
        chain(leave, self.group, self.userns)
    }

    /// Whether, after `outcome`, the Command is in a network namespace of its own with `lo` up:
    /// the plan makes one and no step of it failed.
    pub fn netns_made(&self, outcome: Outcome) -> bool {
        self.netns && (outcome.step == OK || outcome.step >= FIRST_MOUNT_STEP)
    }

    /// Runs the steps in the child, up to the first that fails. Only system calls on memory
    /// made before the fork. The child is left non-dumpable.
    pub fn apply(&mut self) -> Outcome {
        let outcome = run_steps(&mut self.steps);
        // Whatever happened, the child is not dumpable from here to its `exec`.
        let _ = set_dumpable(false);
        outcome
    }

    /// Writes `outcome` to the parent. A failed write is not an error of the run: the parent
    /// then reports that the child did not say.
    pub fn tell(&self, outcome: Outcome) {
        let bytes = [outcome.step, outcome.errno];
        // SAFETY: `bytes` is two readable bytes.
        unsafe { libc::write(self.write_end, bytes.as_ptr().cast(), 2) };
    }
}

impl Report {
    /// What the child said, once it has executed: the network layers and the sandbox.
    pub fn read(self) -> (Layers, Sandbox) {
        drop(self.write);
        let mut said = [0u8; 2];
        let mut read = std::fs::File::from(self.read);
        let said = read.read_exact(&mut said).ok().map(|()| (said[0], said[1]));
        (layers(self.denied, self.root, said), sandbox(said))
    }
}

/// The network layers a report means, for a Command `denied` the network or not and run as
/// `root` or not.
pub fn layers(denied: bool, root: bool, said: Option<(u8, u8)>) -> Layers {
    let Some((step, errno)) = said else {
        return if denied {
            Layers::Unreported
        } else {
            Layers::Open
        };
    };
    if !denied {
        Layers::Open
    } else if root {
        Layers::SeccompOnlyForRoot
    } else if step == OK || step >= FIRST_MOUNT_STEP {
        Layers::SeccompAndNetns
    } else {
        step_name(step).map_or(Layers::Unreported, |step| Layers::SeccompOnly {
            step,
            errno: i32::from(errno),
        })
    }
}

/// The sandbox a report means.
pub fn sandbox(said: Option<(u8, u8)>) -> Sandbox {
    match said {
        None => Sandbox::Unreported,
        Some((OK, _)) => Sandbox::Made,
        Some((step, errno)) => {
            step_name(step).map_or(Sandbox::Unreported, |step| Sandbox::Incomplete {
                step,
                errno: i32::from(errno),
            })
        }
    }
}

/// Runs `steps` in order up to the first that fails.
pub fn run_steps(steps: &mut [(u8, Step)]) -> Outcome {
    let mut unshared = false;
    for (number, step) in steps {
        if let Err(error) = run_step(step) {
            return Outcome {
                step: *number,
                errno: code(&error),
                unshared,
            };
        }
        unshared |= matches!(step, Step::Unshare(_));
    }
    Outcome {
        step: OK,
        errno: 0,
        unshared,
    }
}

/// The flags of every cover: no set-id, no device, nothing to run (it holds nothing).
const COVER_FLAGS: MountFlags = MountFlags::NOSUID
    .union(MountFlags::NODEV)
    .union(MountFlags::NOEXEC);

pub fn run_step(step: &mut Step) -> io::Result<()> {
    match step {
        Step::Dumpable(on) => set_dumpable(*on),
        // SAFETY: integer argument only. The child is single-threaded, so no other thread sees
        // a descriptor table or namespace change.
        Step::Unshare(flags) => cvt_int(unsafe { libc::unshare(*flags) }).map(drop),
        Step::Write(path, line) => write_file(path, line),
        Step::Loopback => raise_loopback(),
        Step::Private => Ok(rustix::mount::mount_change(
            c"/",
            MountPropagationFlags::PRIVATE | MountPropagationFlags::REC,
        )?),
        Step::Hold { dir, fd } => hold(dir, fd),
        Step::Cover(dir) => Ok(rustix::mount::mount(
            c"tmpfs",
            dir.as_c_str(),
            c"tmpfs",
            COVER_FLAGS,
            c"mode=0755,size=16k",
        )?),
        Step::Bind { from, to } => Ok(rustix::mount::mount_bind(from.as_c_str(), to.as_c_str())?),
        Step::Mkdir(dir) => match rustix::fs::mkdir(dir.as_c_str(), Mode::from_raw_mode(0o755)) {
            Err(rustix::io::Errno::EXIST) => Ok(()),
            done => Ok(done?),
        },
        Step::Seal(dir) => Ok(rustix::mount::mount_remount(
            dir.as_c_str(),
            MountFlags::BIND | MountFlags::RDONLY | COVER_FLAGS,
            c"",
        )?),
        Step::Chdir(dir) => Ok(rustix::process::chdir(dir.as_c_str())?),
    }
}

/// Opens the directory `dir` and moves the descriptor onto `fd`, closing on `exec`.
fn hold(dir: &CStr, fd: &mut OwnedFd) -> io::Result<()> {
    let opened = rustix::fs::open(
        dir,
        OFlags::PATH | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?;
    Ok(rustix::io::dup3(
        &opened,
        fd,
        rustix::io::DupFlags::CLOEXEC,
    )?)
}

/// Writes `line` to the file at `path` in one `write`, as the kernel's map files need.
pub fn write_file(path: &CStr, line: &[u8]) -> io::Result<()> {
    let file = rustix::fs::open(path, OFlags::WRONLY | OFlags::CLOEXEC, Mode::empty())?;
    rustix::io::write(&file, line)?;
    Ok(())
}

/// Sets `IFF_UP | IFF_RUNNING` on `lo` of the namespace the process is in.
pub fn raise_loopback() -> io::Result<()> {
    // SAFETY: integer arguments only.
    let fd =
        cvt_int(unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) })?;
    let result = set_loopback_flags(fd);
    // SAFETY: `fd` was opened above.
    unsafe { libc::close(fd) };
    result
}

fn set_loopback_flags(fd: libc::c_int) -> io::Result<()> {
    // SAFETY: an all-zero `ifreq` is valid.
    let mut request: libc::ifreq = unsafe { std::mem::zeroed() };
    request.ifr_name[0] = b'l' as libc::c_char;
    request.ifr_name[1] = b'o' as libc::c_char;
    // The `SIOC*` constants are `c_ulong`, `ioctl`'s request is `c_ulong` on glibc but `c_int`
    // on musl (the release builds), so each request is cast to `libc::Ioctl` (0x89xx fits both).
    // SAFETY: `request` is an `ifreq`, as both requests take.
    cvt_int(unsafe { libc::ioctl(fd, libc::SIOCGIFFLAGS as libc::Ioctl, &raw mut request) })?;
    // SAFETY: the kernel filled the flags member of the union.
    unsafe {
        request.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
    }
    // SAFETY: as above.
    cvt_int(unsafe { libc::ioctl(fd, libc::SIOCSIFFLAGS as libc::Ioctl, &raw mut request) })
        .map(drop)
}

/// How a process of the chain ends: `_exit` in the child of a real run; `exit` in a test, which
/// writes the coverage profile.
pub type Leave = unsafe extern "C" fn(libc::c_int) -> !;

/// A copy of this process, as `fork` makes, by the `clone` system call itself: the C library's
/// `fork` runs handlers that take locks, which a thread of the multi-threaded parent may have
/// held when the first fork was made. Returns the child's pid in the parent and 0 in the child.
///
/// # Errors
/// The `clone` failed (no process could be made).
pub fn fork() -> io::Result<libc::pid_t> {
    // SAFETY: a clone with no flag but the signal sent to the parent at the child's end shares
    // nothing with the parent; the child goes on from here on its copy of the stack, as after
    // `fork`. The arguments past the flags (stack, thread ids, TLS) are all zero, in either
    // architecture's order.
    let pid = cvt(unsafe {
        libc::syscall(
            libc::SYS_clone,
            libc::c_long::from(libc::SIGCHLD),
            0 as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
            0 as libc::c_long,
        )
    })?;
    libc::pid_t::try_from(pid).map_err(|_| io::Error::from_raw_os_error(libc::EOVERFLOW))
}

/// Keeps the Command off PID 1 of the PID namespace the `unshare` step made. Called by the
/// child once its steps and its report are done; returns only in the process that is to become
/// the Command.
///
/// - The calling process (outside the namespace, the one the tool started) makes the namespace's
///   first process, then leaves the Command's process group for `group` (the tool's own), waits
///   for the first process and leaves with its exit code ([`relay`]). Out of the group, it is
///   neither ended by a Command that signals its own group (`kill 0`) nor by the tool's kill of
///   the Command's group at the deadline, so it still reaps the first process, and the tool's
///   resource figures still cover every process of the run.
/// - The first process (PID 1 in the namespace) is killed when the caller dies
///   (`PR_SET_PDEATHSIG`), makes the Command's process, then reaps every process handed to it
///   until the Command's ends, and leaves with the Command's exit code ([`reap`]). When it
///   ends, the kernel kills every process left in the namespace, and its end is seen by the
///   caller only once they are all gone.
/// - The Command's process, when `userns` (it is in a user namespace of its own, where it holds
///   every capability until it is executed), drops every capability and sets `no_new_privs`
///   ([`drop_capabilities`]), and returns `Ok`, to be executed.
///
/// An exit code is passed up as it is; a process a signal ended is passed up as 128 plus the
/// signal. Both waiting processes close every descriptor first, so the pipe on which the parent
/// learns that the `exec` happened closes when it does.
///
/// # Errors
/// A process could not be made, or the death signal, `no_new_privs` or the capabilities could
/// not be set: the process that failed returns the error, and the start of the Command fails
/// with it.
pub fn chain(leave: Leave, group: Pid, userns: bool) -> io::Result<()> {
    if let Some(init) = Pid::from_raw(fork()?) {
        relay(init, group, leave);
    }
    rustix::process::set_parent_process_death_signal(Some(Signal::KILL))?;
    if let Some(command) = Pid::from_raw(fork()?) {
        reap(command, leave);
    }
    if userns {
        drop_capabilities()?;
    }
    Ok(())
}

/// The process outside the namespace: joins `group`, closes every descriptor, waits for `init`
/// and leaves with its exit code.
fn relay(init: Pid, group: Pid, leave: Leave) -> ! {
    // A failure leaves it in the Command's group, where the deadline's kill ends it too, before
    // it reaps the rest; nothing else changes.
    let _ = rustix::process::setpgid(None, Some(group));
    close_all(0);
    // SAFETY: `leave` is `_exit` (or, in a test, `exit`), which takes any status.
    unsafe { leave(wait_until(init, false)) }
}

/// The namespace's first process: closes every descriptor, reaps every child until `command`
/// ends, and leaves with its exit code.
fn reap(command: Pid, leave: Leave) -> ! {
    close_all(0);
    // SAFETY: `leave` is `_exit` (or, in a test, `exit`), which takes any status.
    unsafe { leave(wait_until(command, true)) }
}

/// Sets `no_new_privs` and empties the effective, permitted and inheritable capabilities of this
/// process. A program it executes then gains none, a file capability included: under
/// `no_new_privs` the new permitted set is at most the old one, which is empty.
///
/// # Errors
/// The `prctl` or the `capset` failed.
pub fn drop_capabilities() -> io::Result<()> {
    rustix::thread::set_no_new_privs(true)?;
    let none = rustix::thread::CapabilitySet::empty();
    rustix::thread::set_capabilities(
        None,
        rustix::thread::CapabilitySets {
            effective: none,
            permitted: none,
            inheritable: none,
        },
    )?;
    Ok(())
}

/// Closes every descriptor of this process: `close_range` with `flags` (0 in a real run), or,
/// where that call fails (a kernel before 5.9, or a system call filter that does not know it),
/// each descriptor up to the open-file limit.
pub fn close_all(flags: libc::c_uint) {
    // SAFETY: closes this process's descriptors; called only by a process that uses none of them
    // again (it waits and leaves).
    if unsafe { libc::syscall(libc::SYS_close_range, 0, libc::c_uint::MAX, flags) } != 0 {
        close_each(open_limit());
    }
}

/// Closes the descriptors below `limit`.
pub fn close_each(limit: libc::c_int) {
    for fd in 0..limit {
        // SAFETY: as for `close_all`; a descriptor that is not open fails with EBADF and does
        // nothing.
        unsafe { libc::close(fd) };
    }
}

/// The open-file limit of this process, at most 2^20 (65536 where it cannot be read or has
/// none).
pub fn open_limit() -> libc::c_int {
    let limit = rustix::process::getrlimit(Resource::Nofile)
        .current
        .unwrap_or(1 << 16);
    libc::c_int::try_from(limit.min(1 << 20)).unwrap_or(1 << 20)
}

/// Waits for the child `target` and returns the exit code it is passed up as ([`exit_code`]).
/// With `any`, every child is waited for until `target` is, so a process the namespace handed to
/// its first process is reaped. A wait that fails for a reason other than a signal (it cannot
/// while `target` is this process's child) is reported as a kill by `SIGKILL`.
pub fn wait_until(target: Pid, any: bool) -> libc::c_int {
    let which = if any { None } else { Some(target) };
    loop {
        match rustix::process::waitpid(which, WaitOptions::empty()) {
            Ok(Some((pid, status))) if pid == target => return exit_code(status),
            Ok(_) | Err(rustix::io::Errno::INTR) => {}
            Err(_) => return 128 + libc::SIGKILL,
        }
    }
}

/// The exit code a wait status is passed up as: the code itself, or 128 plus the signal that
/// ended the process.
pub fn exit_code(status: WaitStatus) -> libc::c_int {
    status
        .exit_status()
        .unwrap_or_else(|| 128 + status.terminating_signal().unwrap_or(0))
}

#[cfg(test)]
#[path = "child_tests.rs"]
mod tests;
