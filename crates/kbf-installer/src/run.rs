//! Running a program: the one place this crate starts a process, behind a trait so a
//! test plays the program.
//!
//! Every program the installer runs is run the same way: by absolute path, with an
//! environment of exactly the variables it is given (never the caller's), in a directory
//! it is given, with no standard input, for at most a time it is given, and, when it is
//! given a [`RunAs`], as that user with no supplementary groups. [`SystemRunner`] does
//! it; a test's runner records the [`Spawn`] it is handed (so a test reads the argv, the
//! environment and the user that would have been used) and answers as the test says.

use std::ffi::OsString;
use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// A user and group a program runs as. Never root: the settings refuse that.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RunAs {
    pub uid: u32,
    pub gid: u32,
}

/// The `nobody` account: what a program that reads untrusted bytes runs as unless the
/// host's settings name another. Linux gives it 65534; on macOS it is -2, which a `uid_t`
/// holds as 4294967294 (UNVERIFIED on the farm's Macs: a Mac's settings may name it).
pub const fn nobody(platform: crate::layout::Platform) -> RunAs {
    match platform {
        crate::layout::Platform::Linux => RunAs {
            uid: 65534,
            gid: 65534,
        },
        crate::layout::Platform::MacOs => RunAs {
            uid: 4_294_967_294,
            gid: 4_294_967_294,
        },
    }
}

/// One program to run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spawn {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// The whole environment: nothing else is passed.
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub run_as: Option<RunAs>,
    /// The program is killed (with the processes it started) after this long.
    pub timeout: Duration,
}

/// How a program ended, and the first [`CAPTURE`] bytes it wrote to each output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Ran {
    /// The exit status; `None` when a signal ended it (the timeout's kill included).
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub timed_out: bool,
}

/// The most of an output kept; the rest is read and dropped so the program never blocks
/// on a full pipe.
pub const CAPTURE: u64 = 64 * 1024;

pub trait Runner {
    /// Run `spawn` to its end. An error is a program that could not be started.
    fn run(&self, spawn: &Spawn) -> io::Result<Ran>;
}

/// Starts real processes.
pub struct SystemRunner;

impl Runner for SystemRunner {
    fn run(&self, spawn: &Spawn) -> io::Result<Ran> {
        // The descriptors to mark are found here, in the parent, where listing a directory
        // may allocate; the child only makes system calls on numbers copied in.
        start(spawn, Inherited::of(FIRST_INHERITED)?)
    }
}

/// Run `spawn` with `inherited` kept from the program. Not generic over how they are found, so
/// a test can hand it a way that fails.
fn start(spawn: &Spawn, inherited: Inherited) -> io::Result<Ran> {
    if !spawn.program.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not an absolute path", spawn.program.display()),
        ));
    }
    let mut command = Command::new(&spawn.program);
    command
        .args(&spawn.args)
        .env_clear()
        .envs(spawn.env.iter().map(|(k, v)| (k, v)))
        .current_dir(&spawn.cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Its own process group, so the timeout can kill what it started too.
        .process_group(0);
    // SAFETY: the closure runs in the child between fork and exec; it makes system
    // calls on numbers copied in and allocates nothing.
    unsafe {
        command.pre_exec(before_exec(spawn.run_as.filter(needs_switch), inherited));
    }
    let mut child = command.spawn()?;
    let out = capture(child.stdout.take().expect("stdout is piped"));
    let err = capture(child.stderr.take().expect("stderr is piped"));
    let end = Instant::now() + spawn.timeout;
    let (status, timed_out) = loop {
        // The program is looked at without being reaped, so that its pid (and with it
        // the process group's number) cannot be reused before the group is killed.
        if exited(child.id())? {
            kill_group(child.id());
            break (child.wait()?, false);
        }
        if Instant::now() >= end {
            kill_group(child.id());
            break (child.wait()?, true);
        }
        thread::sleep(Duration::from_millis(5));
    };
    // One window for both readers, from now: a descendant that left the group and holds
    // the pipes costs the run this long once, not once per pipe.
    let until = Instant::now() + READ_GRACE;
    Ok(Ran {
        code: status.code(),
        stdout: out.finish(until)?,
        stderr: err.finish(until)?,
        timed_out,
    })
}

/// The first descriptor a program does not inherit: 0, 1 and 2 are its own.
const FIRST_INHERITED: u32 = 3;

/// How long the readers of a program's output get, after the program and its group are
/// dead, to reach the end of the pipes. A descendant that left the group (`setsid`) can
/// hold them open for as long as it lives; the output read so far is then all there is.
pub(crate) const READ_GRACE: Duration = Duration::from_secs(2);

/// Whether a switch to `user` is needed: not when the caller already is that user, which
/// is the only case an unprivileged caller can ask for.
pub(crate) fn needs_switch(user: &RunAs) -> bool {
    rustix::process::geteuid().as_raw() != user.uid
        || rustix::process::getegid().as_raw() != user.gid
}

/// The three system calls that change who a process is, so a test plays them (and reads
/// the order they are made in).
pub trait Creds {
    /// `setgroups(0, NULL)`: no supplementary groups.
    fn set_groups_none(&self) -> io::Result<()>;
    fn set_gid(&self, gid: u32) -> io::Result<()>;
    fn set_uid(&self, uid: u32) -> io::Result<()>;
}

/// The real calls.
pub struct Libc;

impl Creds for Libc {
    fn set_groups_none(&self) -> io::Result<()> {
        // SAFETY: setgroups with no groups reads no memory.
        check(unsafe { libc::setgroups(0, std::ptr::null()) })
    }

    fn set_gid(&self, gid: u32) -> io::Result<()> {
        // SAFETY: setgid takes any number; one the caller may not take fails.
        check(unsafe { libc::setgid(gid) })
    }

    fn set_uid(&self, uid: u32) -> io::Result<()> {
        // SAFETY: as setgid.
        check(unsafe { libc::setuid(uid) })
    }
}

/// `Ok` for the 0 a system call returns, else its `errno`.
pub(crate) fn check(result: libc::c_int) -> io::Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Switch the calling process to `user`: no supplementary groups, then the group, then
/// the user (the group first, because a process that is no longer root cannot change it),
/// as a login would not keep the root helper's. The first call that fails ends it.
pub fn switch_user(creds: &dyn Creds, user: RunAs) -> io::Result<()> {
    creds.set_groups_none()?;
    creds.set_gid(user.gid)?;
    creds.set_uid(user.uid)
}

/// The descriptors a program must not inherit: those numbered `first` or more.
pub(crate) enum Inherited {
    /// All of them, by one `close_range` in the child (Linux 5.11 and later, when the
    /// kernel and the sandbox allow it).
    Range(u32),
    /// These, found by listing the process's descriptor directory in the parent (macOS, and
    /// Linux without `close_range`).
    Listed(Vec<libc::c_int>),
    /// A marking that fails with this errno, for a test of what the start does then.
    #[cfg(test)]
    Fails(i32),
}

impl Inherited {
    /// How to keep descriptors from `first` up from a program: the range call when it
    /// works (probed on a range that holds no descriptor, so nothing of this process is
    /// changed), else the descriptors listed now.
    pub(crate) fn of(first: u32) -> io::Result<Inherited> {
        Inherited::choose(first, range_cloexec(u32::MAX, u32::MAX), Path::new(FD_DIR))
    }

    /// [`Inherited::of`] given the probe's answer and the directory to list.
    fn choose(first: u32, probe: io::Result<()>, dir: &Path) -> io::Result<Inherited> {
        match probe {
            Ok(()) => Ok(Inherited::Range(first)),
            Err(_) => Ok(Inherited::Listed(listed_in(dir, first)?)),
        }
    }
}

/// `CLOSE_RANGE_CLOEXEC`: mark the range close-on-exec instead of closing it. Closing would
/// take the standard library's own pipe, over which a failed exec reports its error.
#[cfg(target_os = "linux")]
const CLOSE_RANGE_CLOEXEC: u32 = 4;

/// Mark descriptors `first` to `last` close-on-exec with one call. An error is the kernel's:
/// `ENOSYS` before Linux 5.11, or a sandbox that does not allow the call.
#[cfg(target_os = "linux")]
fn range_cloexec(first: u32, last: u32) -> io::Result<()> {
    // SAFETY: close_range reads no memory and changes only descriptors of this process.
    check(unsafe { libc::syscall(libc::SYS_close_range, first, last, CLOSE_RANGE_CLOEXEC) } as _)
}

/// There is no `close_range` here.
#[cfg(not(target_os = "linux"))]
fn range_cloexec(_first: u32, _last: u32) -> io::Result<()> {
    Err(io::Error::from_raw_os_error(libc::ENOSYS))
}

/// Where a process lists its open descriptors.
#[cfg(target_os = "linux")]
const FD_DIR: &str = "/proc/self/fd";
#[cfg(not(target_os = "linux"))]
const FD_DIR: &str = "/dev/fd";

/// The descriptors of this process numbered `first` or more (the one that lists them
/// included, which is closed again by the time anyone looks).
fn listed_in(dir: &Path, first: u32) -> io::Result<Vec<libc::c_int>> {
    let mut fds = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let name = entry?.file_name();
        let number = name.to_str().and_then(|n| n.parse::<libc::c_int>().ok());
        fds.extend(number.filter(|fd| u32::try_from(*fd).is_ok_and(|fd| fd >= first)));
    }
    Ok(fds)
}

/// Mark one descriptor close-on-exec. A descriptor that is not open is fine (the directory
/// listing's own is gone by now); `F_GETFD` fails for no other reason than `EBADF`.
fn cloexec(fd: libc::c_int) -> io::Result<()> {
    // SAFETY: fcntl on a number only reports EBADF when it is not open.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Ok(());
    }
    // SAFETY: as above.
    check(unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) })
}

/// What the child runs between fork and exec: keep the descriptors `inherited` names from
/// the program (so it holds only its own standard streams, whatever the helper's parent left
/// open), then become `user` ([`switch_user`]) when it is given one. A failure ends the
/// start: a program that might hold a descriptor it should not is not run. A named function,
/// so that the closure is not part of [`SystemRunner::run`] (which the coverage run cannot
/// see finish in the child) and a test can run it in this process.
fn before_exec(
    user: Option<RunAs>,
    inherited: Inherited,
) -> impl FnMut() -> io::Result<()> + Send + Sync + 'static {
    before_exec_confined(user, inherited, None, None)
}

/// [`before_exec`] for a program that handles bytes nobody has vouched for: it also keeps the
/// descriptor `keep` (the lent identity) open across the exec, and applies `confine` (limits,
/// `no_new_privs`, the parent-death signal). The limits that need root are set before the
/// switch of user; after it the user's other processes are killed (`Confine::after_switch`),
/// then `RLIMIT_NPROC` and the parent-death signal are set (a change of user clears the
/// signal, and the process limit counts every process of the user it is set for).
pub(crate) fn before_exec_confined(
    user: Option<RunAs>,
    inherited: Inherited,
    confine: Option<Confine>,
    keep: Option<libc::c_int>,
) -> impl FnMut() -> io::Result<()> + Send + Sync + 'static {
    before_exec_with(Libc, user, inherited, confine, keep)
}

/// [`before_exec_confined`] with the system calls it makes given, so a test plays them (a real
/// run would limit the test process itself) and reads the order they are made in.
pub(crate) fn before_exec_with<S: Creds + Confiner + Send + Sync + 'static>(
    sys: S,
    user: Option<RunAs>,
    inherited: Inherited,
    confine: Option<Confine>,
    keep: Option<libc::c_int>,
) -> impl FnMut() -> io::Result<()> + Send + Sync + 'static {
    // Read here, in the parent, so that the child can tell whether it has been orphaned.
    let parent = sys.pid();
    move || {
        match &inherited {
            Inherited::Range(first) => range_cloexec(*first, u32::MAX)?,
            Inherited::Listed(fds) => fds.iter().try_for_each(|fd| cloexec(*fd))?,
            #[cfg(test)]
            Inherited::Fails(errno) => return Err(io::Error::from_raw_os_error(*errno)),
        }
        if let Some(fd) = keep {
            sys.keep_open(fd)?;
        }
        if let Some(confine) = confine {
            confine.before_switch(&sys)?;
        }
        if let Some(user) = user {
            switch_user(&sys, user)?;
        }
        match confine {
            Some(confine) => confine.after_switch(&sys, parent, user),
            None => Ok(()),
        }
    }
}

/// What the fetch child is not allowed to do, besides having no environment and no file it can
/// write: set between fork and exec by [`before_exec_confined`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Confine {
    /// `RLIMIT_AS`, in bytes (Linux; macOS does not enforce it).
    pub address_space: u64,
}

/// The processes a confined program, switched to a user of its own, may have.
pub const PROCESSES: u64 = 64;
/// The descriptors a confined program may have open: 0, 1, 2, the lent identity and a few for
/// its connections.
pub const OPEN_FILES: u64 = 16;

impl Confine {
    /// The fetch child (contract section 5): 1 GiB of address space.
    pub const FETCH: Confine = Confine {
        address_space: 1 << 30,
    };

    /// Before the switch of user: `RLIMIT_FSIZE` 0 (it cannot create or grow a file),
    /// `RLIMIT_CORE` 0, `RLIMIT_NOFILE`, `RLIMIT_AS` and `no_new_privs`.
    fn before_switch(self, sys: &dyn Confiner) -> io::Result<()> {
        sys.limit(libc::RLIMIT_FSIZE as libc::c_int, 0)?;
        sys.limit(libc::RLIMIT_CORE as libc::c_int, 0)?;
        sys.limit(libc::RLIMIT_NOFILE as libc::c_int, OPEN_FILES)?;
        sys.address_space(self.address_space)?;
        sys.no_new_privs()
    }

    /// After the switch of user (`switched`, the user it was switched to): every other process
    /// of that user is killed, at most [`PROCESSES`] processes are allowed (both only for a
    /// user the program was switched to, who has none besides), and the program dies with the
    /// parent on Linux. `parent` is the pid of the process that started this one; if it is
    /// already gone the start fails (the signal would never come).
    ///
    /// The kill is of what an earlier program of the user left behind: a descendant that
    /// left its process group (`setsid`) outlives the group kill that ends a fetch, and once
    /// this program has exec'd it is dumpable by the user's other processes, so such a
    /// survivor could read what this one is lent. After the switch, so the signal is the
    /// user's own and never root's (`kill(-1)` as root reaches every process on the host); a
    /// switch to uid 0 is refused for that reason.
    fn after_switch(
        self,
        sys: &dyn Confiner,
        parent: libc::pid_t,
        switched: Option<RunAs>,
    ) -> io::Result<()> {
        if let Some(user) = switched {
            if user.uid == 0 {
                return Err(io::Error::other(
                    "refusing to signal every process of root's: a confined program is never root",
                ));
            }
            sys.signal_others(libc::SIGKILL)?;
            sys.limit(libc::RLIMIT_NPROC as libc::c_int, PROCESSES)?;
        }
        sys.die_with_parent()?;
        if sys.parent_pid() == parent {
            Ok(())
        } else {
            Err(io::Error::other("the parent is gone"))
        }
    }
}

/// The calls a confined start makes besides the change of user ([`Creds`]), so a test plays
/// them. Those that have no meaning off Linux do nothing there.
pub trait Confiner {
    /// This process's id.
    fn pid(&self) -> libc::pid_t;
    /// The id of the process that started this one.
    fn parent_pid(&self) -> libc::pid_t;
    /// Lower both limits of `resource` to `value`.
    fn limit(&self, resource: libc::c_int, value: u64) -> io::Result<()>;
    /// `RLIMIT_AS` (Linux only; macOS does not enforce it).
    fn address_space(&self, bytes: u64) -> io::Result<()>;
    /// `PR_SET_NO_NEW_PRIVS` (Linux).
    fn no_new_privs(&self) -> io::Result<()>;
    /// `PR_SET_PDEATHSIG` with SIGKILL (Linux).
    fn die_with_parent(&self) -> io::Result<()>;
    /// Keep `fd` open across exec.
    fn keep_open(&self, fd: libc::c_int) -> io::Result<()>;
    /// Send `signal` to every other process this process may signal: after a switch of user,
    /// the other processes of that user. None to signal is not an error.
    fn signal_others(&self, signal: libc::c_int) -> io::Result<()>;
}

impl Confiner for Libc {
    fn pid(&self) -> libc::pid_t {
        // SAFETY: getpid takes no arguments.
        unsafe { libc::getpid() }
    }

    fn parent_pid(&self) -> libc::pid_t {
        // SAFETY: getppid takes no arguments.
        unsafe { libc::getppid() }
    }

    fn limit(&self, resource: libc::c_int, value: u64) -> io::Result<()> {
        let limit = libc::rlimit {
            rlim_cur: value as libc::rlim_t,
            rlim_max: value as libc::rlim_t,
        };
        // SAFETY: setrlimit reads the struct it is given.
        #[cfg(target_os = "linux")]
        let rc = unsafe { libc::setrlimit(resource as _, &raw const limit) };
        // SAFETY: as above.
        #[cfg(not(target_os = "linux"))]
        let rc = unsafe { libc::setrlimit(resource, &raw const limit) };
        check(rc)
    }

    #[cfg(target_os = "linux")]
    fn address_space(&self, bytes: u64) -> io::Result<()> {
        self.limit(libc::RLIMIT_AS as libc::c_int, bytes)
    }

    #[cfg(not(target_os = "linux"))]
    fn address_space(&self, _bytes: u64) -> io::Result<()> {
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn no_new_privs(&self) -> io::Result<()> {
        // SAFETY: prctl with these arguments reads no memory.
        check(unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) })
    }

    #[cfg(not(target_os = "linux"))]
    fn no_new_privs(&self) -> io::Result<()> {
        Ok(())
    }

    #[cfg(target_os = "linux")]
    fn die_with_parent(&self) -> io::Result<()> {
        // SAFETY: prctl with these arguments reads no memory.
        check(unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL, 0, 0, 0) })
    }

    #[cfg(not(target_os = "linux"))]
    fn die_with_parent(&self) -> io::Result<()> {
        Ok(())
    }

    fn keep_open(&self, fd: libc::c_int) -> io::Result<()> {
        // SAFETY: fcntl on a descriptor this process holds; this one stays open on exec.
        check(unsafe { libc::fcntl(fd, libc::F_SETFD, 0) })
    }

    fn signal_others(&self, signal: libc::c_int) -> io::Result<()> {
        // SAFETY: kill reads no memory; -1 names every process the caller may signal except
        // init and (on Linux and macOS) the caller itself.
        let rc = unsafe { libc::kill(-1, signal) };
        signalled(rc, &io::Error::last_os_error)
    }
}

/// What the result `rc` of `kill(-1, ..)` says: `Ok` when it signalled something, and when
/// there was nothing it may signal (`ESRCH`: no other process; `EPERM`: only other users'
/// processes, which is the usual answer on a host where nothing of the fetch user is left).
/// `last` is read only for a failure. A `&dyn`, not a generic: there is one copy of this
/// function, and the tests that hand it each answer cover the arms the real call does not reach.
pub(crate) fn signalled(rc: libc::c_int, last: &dyn Fn() -> io::Error) -> io::Result<()> {
    if rc == 0 {
        return Ok(());
    }
    let e = last();
    match e.raw_os_error() {
        Some(libc::ESRCH | libc::EPERM) => Ok(()),
        _ => Err(e),
    }
}

/// Whether the child `pid` has exited, without reaping it.
fn exited(pid: u32) -> io::Result<bool> {
    let pid = libc::id_t::from(pid);
    // SAFETY: waitid writes a siginfo_t, which a zeroed one is valid for; WNOWAIT leaves
    // the child to be waited for.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::waitid(
            libc::P_PID,
            pid,
            &raw mut info,
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY (Linux): the union field is read after a successful waitid, which sets it (0
    // when nothing has exited yet). Apple's siginfo_t has it as a plain field.
    #[cfg(target_os = "linux")]
    let seen = unsafe { info.si_pid() };
    #[cfg(not(target_os = "linux"))]
    let seen = info.si_pid;
    Ok(seen != 0)
}

/// SIGKILL to the process group `pgid` (the child made itself the leader of a group of
/// its own); a group already gone is not an error.
pub(crate) fn kill_group(pgid: u32) {
    let pgid = i32::try_from(pgid).expect("a pid fits an i32");
    // SAFETY: kill reads no memory; a negative pid names a process group.
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}

/// A thread reading `pipe` to its end, keeping the first [`CAPTURE`] bytes.
pub(crate) struct Capture {
    kept: Arc<Mutex<Vec<u8>>>,
    done: mpsc::Receiver<io::Result<()>>,
}

/// A sink that keeps the first [`CAPTURE`] bytes written to it and says it took them all,
/// holding the lock only while it appends (the reader blocks in `read`, not here).
struct Keep(Arc<Mutex<Vec<u8>>>);

impl io::Write for Keep {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let mut kept = self.0.lock().expect("nothing panics holding the lock");
        let room = usize::try_from(CAPTURE).map_or(0, |cap| cap.saturating_sub(kept.len()));
        kept.extend_from_slice(&bytes[..bytes.len().min(room)]);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn capture(pipe: impl Read + Send + 'static) -> Capture {
    let kept = Arc::new(Mutex::new(Vec::new()));
    let (sender, done) = mpsc::channel();
    let mut sink = Keep(Arc::clone(&kept));
    thread::spawn(move || {
        let mut pipe = pipe;
        let read = io::copy(&mut pipe, &mut sink).map(drop);
        // The receiver is gone when the grace ran out; nobody is waiting.
        let _ = sender.send(read);
    });
    Capture { kept, done }
}

impl Capture {
    /// What was read, once the pipe ended or `until` passed (the reader is left
    /// to end on its own).
    pub(crate) fn finish(self, until: Instant) -> io::Result<Vec<u8>> {
        match self
            .done
            .recv_timeout(until.saturating_duration_since(Instant::now()))
        {
            Ok(read) => read?,
            Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => {}
        }
        Ok(self
            .kept
            .lock()
            .expect("the reader holds no lock across a panic")
            .clone())
    }
}

#[cfg(test)]
#[path = "run_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "confine_tests.rs"]
mod confine_tests;
