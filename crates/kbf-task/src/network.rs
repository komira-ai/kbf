//! The Command runs without the network; only this tool's own fetch and upload phases have it.
//!
//! # The rule
//!
//! Before the Command is executed, the child (between `fork` and `exec`) sets `no_new_privs` and
//! installs a seccomp filter. Every process the Command starts inherits both, and the filter
//! cannot be removed. It needs no capability. The filter, by system call:
//!
//! | call | refused with `EPERM` |
//! |------|----------------------|
//! | `socket`, `socketpair` | unless the domain is `AF_UNIX` (or, in a namespace of the Command's own, `AF_INET` or `AF_INET6`, see below). Every other domain is refused, not only `AF_INET` and `AF_INET6`: `AF_NETLINK` (interface and routing queries), `AF_PACKET`, `AF_VSOCK`, `AF_BLUETOOTH`, `AF_XDP` and the rest, so a protocol family added later is refused too |
//! | `io_uring_setup`, `io_uring_enter`, `io_uring_register` | always: `IORING_OP_SOCKET` makes a socket without the `socket` call, so a ring would step round the rule above |
//!
//! `AF_UNIX` stays open (a build tool's own local sockets, `socketpair` for a pipe-like pair).
//! `EPERM` is the answer to a refused call; the same errno for every refusal keeps the
//! diagnosis plain ("operation not permitted"). A program that asks for a family the kernel does
//! not have would get `EAFNOSUPPORT`; the filter does not pretend to be that kernel.
//!
//! A process that makes a system call under another ABI is killed (`SIGSYS`, seen as exit code
//! 159), because the numbers the filter tests are the ones of the ABI the tool was built for
//! (`x86_64` or `aarch64`): the architecture is checked first, which refuses 32-bit
//! compatibility calls, and on `x86_64` a number with the x32 bit (`0x4000_0000`) set, which
//! would otherwise reach `socket` under another number. `socketcall(2)`, the single entry that
//! 32-bit architectures use for every socket call, does not exist on `x86_64` or `aarch64`, and
//! the architecture check kills the one ABI that has it.
//!
//! The filter is built before the fork. The code that runs after it is two system calls
//! (`prctl`, `seccomp`) and no allocation, as a `pre_exec` hook must. If either fails (a
//! kernel without seccomp, or an outer filter that forbids installing one) the Command is not
//! started and the run fails with the errno (exit 11): the network is never left open by
//! accident.
//!
//! `no_new_privs` also means a setuid program the Command starts does not gain its privileges.
//!
//! # The second layer: a network namespace
//!
//! Before the filter, the child gives itself a user and network namespace of its own with the
//! same user and group id inside as outside, and raises `lo` in it (`netns`, below). The
//! Command then has a namespace with one interface and no route. Which filter follows depends
//! on whether every one of those steps succeeded:
//!
//! - **All succeeded** (`netns (loopback only) + seccomp`): the filter also lets `AF_INET` and
//!   `AF_INET6` sockets be made, so the Command can use `127.0.0.1` (a test that starts a server
//!   and connects to it). Nothing leaves the namespace; a connect to any other address fails
//!   with `ENETUNREACH`. `AF_NETLINK`, `AF_PACKET`, every other non-Unix domain and io_uring
//!   stay refused, and the architecture rules are the same.
//! - **Any step failed** (`seccomp only, no inet (netns: <step> <errno>)`): the strict filter
//!   alone, which refuses `AF_INET` and `AF_INET6` as well. A host that restricts unprivileged
//!   user namespaces (on Ubuntu, `apparmor_restrict_unprivileged_userns`, which leaves a
//!   namespace without capabilities unless the container's profile allows `userns`) takes this
//!   path. The step is `unshare`, `setgroups`, `uid_map`, `gid_map` or `loopback`. A step that
//!   fails does not fail the run.
//!
//! A Command that runs as root (effective uid 0) gets no namespace: one that maps only its own
//! id would make `chown` to another user, `setuid`, `setgid` and `setgroups` fail for it, and
//! `tar`, `dpkg` and `apt` do those. It runs under the strict filter, and the line says
//! `seccomp only, no inet (netns: skipped for uid 0)`. Between the writes to its own `/proc`
//! files and its `exec` the child is non-dumpable again.
//!
//! Both filters are built before the fork; the child picks one after the steps. It reports the
//! outcome on a pipe it writes just before the `exec`, and the tool writes one line per run
//! ([`layers_notice`]): `kbf-task: action <digest>: network: netns (loopback only) + seccomp`,
//! `network: seccomp only, no inet (netns: <step> <errno>)` or `network: open
//! (kbf-network=allow)`.
//!
//! # What it does not do
//!
//! The filter stops the Command from making a socket. It does not stop it from using one it is
//! handed. The Command runs as the same user as this tool, so the sockets this tool holds open
//! are protected from `ptrace`, `pidfd_getfd` and `/proc/<pid>/mem` by [`protect_this_process`],
//! which the binary calls first: it makes the tool non-dumpable, and the Command (a child that
//! becomes dumpable again at its own `exec`) cannot take them. [`confine`] is where a layer is
//! added.
//!
//! # Opting in
//!
//! The platform property [`PROPERTY`]`=allow` (docs/platform-properties.md) turns the filter
//! off for one action, and [`allowed_notice`] is the line the tool prints. Any other value, and
//! the property named twice, is refused ([`from_platform`]). The property is part of the
//! action's digest, so the action with the network is a different action.
//!
//! # Other systems
//!
//! The filter exists on Linux only, and this tool runs in Linux containers (the release is a
//! static Linux build). Built for another system, which only developers' machines and the
//! test suites do, the Command runs with the network and a line on standard error says so
//! for each run: refusing instead would stop the tool's tests from running there, and the
//! warning keeps anyone from taking such a build for a sandbox.

use kbf_proto::reapi::{Action, Command};
use kbf_types::Digest;
use tokio::process::Command as Process;

use crate::run::RunError;

/// The platform property that lets one action's Command use the network.
pub const PROPERTY: &str = "kbf-network";

/// The one value of [`PROPERTY`] that is accepted.
pub const ALLOW: &str = "allow";

/// Whether the Command may use the network.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Network {
    /// No sockets but `AF_UNIX` (see the module documentation). The default.
    #[default]
    Denied,
    /// Nothing is installed: the action asked for the network.
    Allowed,
}

/// What the action's platform says about the network: [`Network::Allowed`] for
/// `kbf-network=allow`, else [`Network::Denied`]. The Action's platform is read, or the
/// Command's when the Action carries none, as the frontend does; the name is read in any case.
///
/// # Errors
/// [`RunError::Invalid`] for a value other than `allow`, and for the property named twice (in
/// any spelling).
#[allow(deprecated)] // `Command::platform`: REAPI before 2.2 sends the platform there.
pub fn from_platform(action: &Action, command: &Command) -> Result<Network, RunError> {
    let platform = action.platform.as_ref().or(command.platform.as_ref());
    let mut named = platform
        .into_iter()
        .flat_map(|p| &p.properties)
        .filter(|p| p.name.eq_ignore_ascii_case(PROPERTY));
    let Some(property) = named.next() else {
        return Ok(Network::Denied);
    };
    if named.next().is_some() {
        return Err(RunError::Invalid(format!(
            "the platform property {PROPERTY} is given more than once"
        )));
    }
    if property.value == ALLOW {
        Ok(Network::Allowed)
    } else {
        Err(RunError::Invalid(format!(
            "the platform property {PROPERTY}={:?} is not {ALLOW:?}, the only value it takes",
            property.value
        )))
    }
}

/// The line written to standard error when an action runs with the network.
#[must_use]
pub fn allowed_notice(action: &Digest) -> String {
    format!(
        "kbf-task: action {action}: platform property {PROPERTY}={ALLOW}: the Command runs with the network"
    )
}

/// Which layers confine a Command that is to have no network.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Layers {
    /// The action asked for the network ([`PROPERTY`]): neither layer.
    Open,
    /// This system has neither layer (not Linux).
    Unavailable,
    /// The strict filter alone, because the Command runs as root: a namespace with a map of one
    /// id would make `chown`, `setuid` and `setgroups` fail for it (`tar`, `dpkg`), so none is made.
    SeccompOnlyForRoot,
    /// A network namespace of the Command's own, in which only `lo` exists, and the filter that
    /// lets `AF_INET` and `AF_INET6` sockets be made (they reach `lo`).
    SeccompAndNetns,
    /// The child did not report, so which filter it installed is not known.
    Unreported,
    /// The strict seccomp filter alone, which refuses every `AF_INET` and `AF_INET6` socket: a
    /// step of the namespace layer failed, which is not a failure of the run.
    SeccompOnly {
        /// The step: `unshare`, `setgroups`, `uid_map`, `gid_map` or `loopback`.
        step: &'static str,
        /// Its `errno`.
        errno: i32,
    },
}

impl std::fmt::Display for Layers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Layers::Open => write!(f, "open ({PROPERTY}={ALLOW})"),
            Layers::Unavailable => write!(f, "none (this system has no filter)"),
            Layers::Unreported => write!(f, "unknown (no report from the child)"),
            Layers::SeccompOnlyForRoot => {
                write!(f, "seccomp only, no inet (netns: skipped for uid 0)")
            }
            Layers::SeccompAndNetns => write!(f, "netns (loopback only) + seccomp"),
            Layers::SeccompOnly { step, errno } => {
                write!(f, "seccomp only, no inet (netns: {step} {errno})")
            }
        }
    }
}

/// The line written to standard error once the Command has been started, naming the layers that
/// confine it.
#[must_use]
pub fn layers_notice(action: &Digest, layers: &Layers) -> String {
    format!("kbf-task: action {action}: network: {layers}")
}

/// Makes this process refuse `ptrace`, `pidfd_getfd` and reads of `/proc/<pid>/mem` from the
/// processes it starts, which run as the same user: they could otherwise take the sockets this
/// tool holds (the grant and the store) out of it, as `ptrace_scope` 0 allows or a Command that
/// is a descendant of a tracer permits. Call it first, before a socket is opened. The Command
/// is dumpable again after its `exec`; a child between `fork` and `exec` sets dumpable itself
/// where it must write its own `/proc` files.
///
/// A non-dumpable process has root-owned `/proc/self` files and writes no core file.
///
/// # Errors
/// The `prctl` failed.
pub fn protect_this_process() -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: prctl with integer arguments only.
        if unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

/// What `confine` set up, to be read once the process has been started.
pub struct Confinement {
    denied: bool,
    #[cfg(target_os = "linux")]
    report: Option<netns::Report>,
}

impl Confinement {
    /// The layers that confine the process that was started. Call it after a successful spawn:
    /// the child has by then written its report and executed the Command.
    #[must_use]
    pub fn layers(self) -> Layers {
        if !self.denied {
            return Layers::Open;
        }
        #[cfg(target_os = "linux")]
        {
            self.report.map_or(Layers::Unavailable, netns::Report::read)
        }
        #[cfg(not(target_os = "linux"))]
        {
            Layers::Unavailable
        }
    }
}

/// Applies `network` to the process about to be started. This is the one place a confinement
/// layer is added. On Linux, in the child before the `exec`: a network namespace of its own
/// ([`netns`]), then the seccomp filter, which is last because the namespace steps need an
/// `AF_INET` socket. See the module documentation.
///
/// # Errors
/// [`RunError::Io`] when the pipe the child reports on cannot be made.
pub fn confine(process: &mut Process, network: Network) -> Result<Confinement, RunError> {
    match network {
        Network::Allowed => Ok(Confinement {
            denied: false,
            #[cfg(target_os = "linux")]
            report: None,
        }),
        Network::Denied => deny_sockets(process),
    }
}

#[cfg(not(target_os = "linux"))]
fn deny_sockets(_process: &mut Process) -> Result<Confinement, RunError> {
    eprintln!("kbf-task: this system cannot deny the Command the network; it runs with it");
    Ok(Confinement { denied: true })
}

/// The filter that lets `AF_INET` through is for a Command that is certainly in a namespace of
/// its own: every step must have succeeded. Any other outcome, a skip for root included, gets
/// the strict one.
#[cfg(target_os = "linux")]
fn pick<'a>(
    outcome: (u8, u8),
    strict: &'a [libc::sock_filter],
    in_netns: &'a [libc::sock_filter],
) -> &'a [libc::sock_filter] {
    if outcome == (netns::OK, 0) {
        in_netns
    } else {
        strict
    }
}

#[cfg(target_os = "linux")]
fn deny_sockets(process: &mut Process) -> Result<Confinement, RunError> {
    let strict = seccomp::filter(false);
    let in_netns = seccomp::filter(true);
    // SAFETY: no arguments.
    let (uid, gid) = unsafe { (libc::geteuid(), libc::getegid()) };
    let (plan, report) = netns::plan(uid, gid).map_err(|source| RunError::Io {
        path: "the network report pipe".into(),
        source,
    })?;
    // SAFETY: the hook runs between fork and exec in the child and makes only system calls on
    // memory built before the fork (`netns::Plan::apply`, `seccomp::install`): no allocation,
    // no lock, no panic.
    unsafe {
        process.pre_exec(move || {
            let outcome = plan.apply();
            seccomp::install(pick(outcome, &strict, &in_netns))?;
            plan.tell(outcome);
            Ok(())
        });
    }
    Ok(Confinement {
        denied: true,
        report: Some(report),
    })
}

/// The second layer: a user and network namespace of the Command's own.
///
/// The child unshares `CLONE_NEWUSER | CLONE_NEWNET` (the user namespace is what lets an
/// unprivileged process make the network one), maps its own user and group id to themselves (so
/// the Command is the same user and owns the same files), and raises `lo`, the one interface
/// the new namespace has. The Command then has no route anywhere. This needs the host to allow
/// unprivileged user namespaces to hold their capabilities: a host that restricts them with
/// AppArmor (`apparmor_restrict_unprivileged_userns`) gives a namespace without them, `lo`
/// cannot be raised, and the step fails with `EPERM`. A step that fails is reported and does
/// not fail the run: the seccomp filter is the guarantee, this is defence in depth, for example
/// against a socket handed to the Command over a Unix socket.
#[cfg(target_os = "linux")]
mod netns {
    use std::ffi::CString;
    use std::io::{self, Read as _};
    use std::os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd};

    use super::Layers;

    pub const OK: u8 = 0;
    /// The steps, in the order they run; the number is what the child writes.
    pub const SKIPPED_FOR_ROOT: u8 = 6;
    const STEPS: [&str; 5] = ["unshare", "setgroups", "uid_map", "gid_map", "loopback"];

    /// What the child needs, made before the fork.
    pub struct Plan {
        setgroups: CString,
        uid_map: CString,
        gid_map: CString,
        skip: bool,
        uid_line: Vec<u8>,
        gid_line: Vec<u8>,
        write_end: i32,
    }

    /// The parent's end of the pipe the child reports on.
    pub struct Report {
        read: OwnedFd,
        // Held so that the child's copy is the only one once the parent lets this go.
        write: OwnedFd,
    }

    /// Whether the namespace layer is left out for a Command run as `euid`: root, for which a
    /// namespace that maps only its own id (`0 0 1`) breaks `chown` to another user, `setuid`,
    /// `setgid` and `setgroups`, which `tar`, `dpkg` and `apt` do.
    pub fn skips_namespace(euid: u32) -> bool {
        euid == 0
    }

    /// The plan for a Command run as `uid` and `gid` (the effective ids, read once before the
    /// fork), and the pipe it reports on.
    pub fn plan(uid: u32, gid: u32) -> io::Result<(Plan, Report)> {
        let mut fds = [0 as libc::c_int; 2];
        // SAFETY: `fds` has room for the two descriptors.
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: pipe2 made both descriptors and nothing else owns them.
        let (read, write) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let path = |p: &str| CString::new(p).expect("a path without NUL");
        let plan = Plan {
            setgroups: path("/proc/self/setgroups"),
            uid_map: path("/proc/self/uid_map"),
            gid_map: path("/proc/self/gid_map"),
            skip: skips_namespace(uid),
            uid_line: format!("{uid} {uid} 1").into_bytes(),
            gid_line: format!("{gid} {gid} 1").into_bytes(),
            write_end: write.as_raw_fd(),
        };
        Ok((plan, Report { read, write }))
    }

    impl Plan {
        /// Whether the plan leaves the namespace layer out.
        pub fn skips(&self) -> bool {
            self.skip
        }
    }

    impl Report {
        /// What the child said, once it has executed.
        pub fn read(self) -> Layers {
            drop(self.write);
            let mut said = [0u8; 2];
            let mut read = std::fs::File::from(self.read);
            if read.read_exact(&mut said).is_err() {
                return Layers::Unreported;
            }
            match said[0] {
                OK => Layers::SeccompAndNetns,
                SKIPPED_FOR_ROOT => Layers::SeccompOnlyForRoot,
                n => STEPS
                    .get(usize::from(n).wrapping_sub(1))
                    .map_or(Layers::Unreported, |step| Layers::SeccompOnly {
                        step,
                        errno: i32::from(said[1]),
                    }),
            }
        }
    }

    impl Plan {
        /// Runs the steps in the child; returns the step that failed (1-based) and its errno, or
        /// `(OK, 0)`. Only system calls on memory made before the fork.
        pub fn apply(&self) -> (u8, u8) {
            if self.skips() {
                return (SKIPPED_FOR_ROOT, 0);
            }
            let outcome = self.steps();
            // Whatever happened, the child is not dumpable from here to its `exec`.
            // SAFETY: prctl with integer arguments only.
            unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
            outcome
        }

        fn steps(&self) -> (u8, u8) {
            // The child is a copy of a process that made itself non-dumpable
            // (`protect_this_process`), which makes its `/proc/self` files root's. It is only
            // the child that becomes dumpable.
            // SAFETY: prctl with integer arguments only.
            unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 1, 0, 0, 0) };
            // SAFETY: integer argument only.
            if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNET) } != 0 {
                return (1, errno());
            }
            for (step, path, line) in [
                (2, &self.setgroups, &b"deny"[..]),
                (3, &self.uid_map, &self.uid_line),
                (4, &self.gid_map, &self.gid_line),
            ] {
                if let Err(e) = write_file(path, line) {
                    return (step, e);
                }
            }
            // The `/proc/self` writes are done: back to non-dumpable before anything else runs.
            // SAFETY: prctl with integer arguments only.
            unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) };
            match raise_loopback() {
                Ok(()) => (OK, 0),
                Err(e) => (5, e),
            }
        }

        /// Writes the outcome of `apply` to the parent. A failed write is not an error of the
        /// run: the parent then reports that the child did not say.
        pub fn tell(&self, outcome: (u8, u8)) {
            let bytes = [outcome.0, outcome.1];
            // SAFETY: `bytes` is two readable bytes.
            unsafe { libc::write(self.write_end, bytes.as_ptr().cast(), 2) };
        }
    }

    fn errno() -> u8 {
        u8::try_from(io::Error::last_os_error().raw_os_error().unwrap_or(0)).unwrap_or(u8::MAX)
    }

    /// Writes `line` to the file at `path` in one `write`, as the kernel's map files need.
    fn write_file(path: &CString, line: &[u8]) -> Result<(), u8> {
        // SAFETY: `path` is a NUL-terminated string.
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_WRONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(errno());
        }
        // SAFETY: `line` is readable for its length.
        let done = unsafe { libc::write(fd, line.as_ptr().cast(), line.len()) };
        let failed = usize::try_from(done) != Ok(line.len());
        let error = errno();
        // SAFETY: `fd` was opened above.
        unsafe { libc::close(fd) };
        if failed { Err(error) } else { Ok(()) }
    }

    /// Sets `IFF_UP | IFF_RUNNING` on `lo` of the namespace the process is in.
    fn raise_loopback() -> Result<(), u8> {
        // SAFETY: integer arguments only.
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0) };
        if fd < 0 {
            return Err(errno());
        }
        // SAFETY: an all-zero `ifreq` is valid.
        let mut request: libc::ifreq = unsafe { std::mem::zeroed() };
        request.ifr_name[0] = b'l' as libc::c_char;
        request.ifr_name[1] = b'o' as libc::c_char;
        // SAFETY: `request` is an `ifreq`, as both requests take.
        let mut result = unsafe { libc::ioctl(fd, libc::SIOCGIFFLAGS, &raw mut request) };
        if result == 0 {
            // SAFETY: the kernel filled the flags member of the union.
            unsafe {
                request.ifr_ifru.ifru_flags |= (libc::IFF_UP | libc::IFF_RUNNING) as libc::c_short;
            }
            // SAFETY: as above.
            result = unsafe { libc::ioctl(fd, libc::SIOCSIFFLAGS, &raw mut request) };
        }
        let error = errno();
        // SAFETY: `fd` was opened above.
        unsafe { libc::close(fd) };
        if result == 0 { Ok(()) } else { Err(error) }
    }
}

#[cfg(target_os = "linux")]
mod seccomp {
    use std::io;

    use libc::sock_filter;

    #[cfg(target_arch = "x86_64")]
    const AUDIT_ARCH: u32 = 0xC000_003E;
    #[cfg(target_arch = "aarch64")]
    const AUDIT_ARCH: u32 = 0xC000_00B7;
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    compile_error!("the no-network filter knows the system calls of x86_64 and aarch64 only");

    /// `struct seccomp_data`: the system call number, the architecture, and the first argument.
    const OFFSET_NR: u32 = 0;
    const OFFSET_ARCH: u32 = 4;
    const OFFSET_ARG0: u32 = 16;
    /// Set on an x32 system call number.
    #[cfg(target_arch = "x86_64")]
    const X32_BIT: u32 = 0x4000_0000;
    const AF_UNIX: u32 = 1;
    const AF_INET: u32 = 2;
    const AF_INET6: u32 = 10;

    const LOAD: u16 = (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16;
    const JEQ: u16 = (libc::BPF_JMP | libc::BPF_JEQ | libc::BPF_K) as u16;
    #[cfg(target_arch = "x86_64")]
    const JGE: u16 = (libc::BPF_JMP | libc::BPF_JGE | libc::BPF_K) as u16;
    const RET: u16 = (libc::BPF_RET | libc::BPF_K) as u16;

    const DENY: u32 = libc::SECCOMP_RET_ERRNO | libc::EPERM as u32;
    const KILL: u32 = libc::SECCOMP_RET_KILL_PROCESS;
    const ALLOW: u32 = libc::SECCOMP_RET_ALLOW;

    /// Where a jump goes.
    #[derive(Clone, Copy)]
    enum To {
        Next,
        Allow,
        Deny,
        Kill,
        Domain,
    }

    fn number(n: libc::c_long) -> u32 {
        u32::try_from(n).expect("a system call number is a small positive number")
    }

    /// The classic BPF program (see the module documentation of `network` for the rule).
    ///
    /// `inet` adds `AF_INET` and `AF_INET6` to the domains a socket may be made in: only for a
    /// Command in a network namespace of its own, where they reach `lo` and nothing else.
    pub fn filter(inet: bool) -> Vec<sock_filter> {
        // (code, k, jump if equal, jump otherwise)
        let mut main: Vec<(u16, u32, To, To)> = vec![
            (LOAD, OFFSET_ARCH, To::Next, To::Next),
            (JEQ, AUDIT_ARCH, To::Next, To::Kill),
            (LOAD, OFFSET_NR, To::Next, To::Next),
        ];
        #[cfg(target_arch = "x86_64")]
        main.push((JGE, X32_BIT, To::Kill, To::Next));
        main.extend([
            (JEQ, number(libc::SYS_socket), To::Domain, To::Next),
            (JEQ, number(libc::SYS_socketpair), To::Domain, To::Next),
            (JEQ, number(libc::SYS_io_uring_setup), To::Deny, To::Next),
            (JEQ, number(libc::SYS_io_uring_enter), To::Deny, To::Next),
            (JEQ, number(libc::SYS_io_uring_register), To::Deny, To::Next),
            (RET, ALLOW, To::Next, To::Next),
        ]);
        let n = main.len();
        // After `main`: the domain test, then the three verdicts it and `main` jump to. A BPF
        // jump goes forward only, so the verdicts follow every jump to them.
        let mut tail = vec![
            (LOAD, OFFSET_ARG0, To::Next, To::Next),
            (JEQ, AF_UNIX, To::Allow, To::Next),
        ];
        if inet {
            tail.push((JEQ, AF_INET, To::Allow, To::Next));
            tail.push((JEQ, AF_INET6, To::Allow, To::Next));
        }
        // The last test falls through to the refusal.
        tail.extend([
            (RET, DENY, To::Next, To::Next),
            (RET, ALLOW, To::Next, To::Next),
            (RET, KILL, To::Next, To::Next),
        ]);
        let deny = n + tail.len() - 3;
        let index = |to: To, from: usize| -> u8 {
            let at = match to {
                To::Next => from + 1,
                To::Allow => deny + 1,
                To::Domain => n,
                To::Deny => deny,
                To::Kill => deny + 2,
            };
            u8::try_from(at - (from + 1)).expect("a short jump")
        };
        main.into_iter()
            .chain(tail)
            .enumerate()
            .map(|(at, (code, k, yes, no))| sock_filter {
                code,
                jt: index(yes, at),
                jf: index(no, at),
                k,
            })
            .collect()
    }

    /// Sets `no_new_privs` and installs `filter` for this process and its descendants. Only
    /// system calls, no allocation: it runs between fork and exec.
    pub fn install(filter: &[sock_filter]) -> io::Result<()> {
        let program = libc::sock_fprog {
            len: u16::try_from(filter.len())
                .map_err(|_| io::Error::from_raw_os_error(libc::E2BIG))?,
            filter: filter.as_ptr().cast_mut(),
        };
        // SAFETY: prctl with integer arguments only.
        if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `program` and the filter it points at outlive the call, which copies them.
        let done = unsafe {
            libc::syscall(
                libc::SYS_seccomp,
                libc::SECCOMP_SET_MODE_FILTER,
                0,
                &raw const program,
            )
        };
        if done == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }
}

#[cfg(test)]
#[path = "network_tests.rs"]
mod tests;
