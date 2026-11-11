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
//! same user and group id inside as outside, and raises `lo` in it ([`crate::child`]; the same
//! `unshare` makes the mount and PID namespaces of the sandbox, [`crate::sandbox`], whose steps
//! follow). The Command then has a namespace with one interface and no route. Which filter
//! follows depends on whether every one of the network namespace's steps succeeded (a failed
//! step of the sandbox's mounts, which come after, leaves it whole):
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
//! A Command that runs as root (effective uid 0) gets no user or network namespace: a user
//! namespace that maps only its own id would make `chown` to another user, `setuid`, `setgid`
//! and `setgroups` fail for it, and `tar`, `dpkg` and `apt` do those. (It gets the sandbox's
//! mount and PID namespaces, which need no user namespace for root.) It runs under the strict
//! filter, and the line says `seccomp only, no inet (netns: skipped for uid 0)`. Between the
//! writes to its own `/proc` files and its `exec` the child is non-dumpable again.
//!
//! Both filters are built before the fork; the child picks one after the steps. It reports the
//! outcome on a pipe it writes before the `exec`, and the tool writes one line per run
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
//! # macOS
//!
//! There is no seccomp or namespace on macOS. The Command alone runs under `sandbox-exec`
//! ([`launch`]): this tool runs outside any sandbox (a sandbox cannot be nested on macOS) and starts
//! `/usr/bin/sandbox-exec -p <profile> <program> <args...>`. The profile ([`seatbelt_profile`],
//! written by `kbf-seatbelt`, the code the Mac daemon's driver also uses) allows everything, then:
//!
//! - for a Command denied the network, denies every network operation but to loopback and to Unix
//!   sockets; for one allowed it, adds no network clause;
//! - denies reading and writing each file this tool was given a credential or presigned URL in
//!   (`--grant-token-file`, `--pack-url-file`, `--put-urls-file`), and renaming or removing each
//!   directory above it, for either kind of Command: a rename, unlink, hard link or directory rename
//!   cannot give the file a path the read deny does not name.
//!
//! What it does not do, as the driver's own profile does not: any Unix socket stays allowed (the
//! system resolver, `mDNSResponder`, is reached through one, so a DNS lookup still leaves the node
//! and a name the Command looks up can carry data out), and a connect to the node's own
//! non-loopback address was allowed under the denial on the macOS runner. Writes are not limited by
//! this profile, except to those files and directories. `mach-lookup` of helper daemons and other
//! services on the node stay open, and the Command (the same user) can see this tool's argv and process
//! (`protect_this_process` does nothing on macOS). A credential given as a flag value (`--insecure-argv-secrets`) is in this tool's
//! argv, which the profile does not hide. Egress of the pool user is limited by the host's packet
//! filter, not by this tool.
//!
//! The line the tool writes names the layer: `network: sandbox-exec profile (...)`.
//!
//! # Other systems
//!
//! A system that is neither Linux nor macOS has no confine, and a Command denied the network is
//! refused there ([`RunError::NoConfine`], exit 15). The test-only feature `unconfined-for-tests`
//! turns the refusal into a warning line and a run with the network, so a developer's machine can
//! run the suite; it is refused in a release build ([`crate`] documents the compile error).

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use kbf_proto::reapi::{Action, Command};
use kbf_types::Digest;
use tokio::process::Command as Process;

use crate::run::RunError;
use crate::sandbox::{Layout, Sandbox};

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
    /// This system has neither layer (neither Linux nor macOS); only the test feature gets here.
    Unavailable,
    /// macOS: the Command runs under a `sandbox-exec` profile that denies every network operation
    /// but to loopback and Unix sockets ([`seatbelt_profile`]).
    Seatbelt,
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
            Layers::Seatbelt => write!(
                f,
                "sandbox-exec profile (no network but loopback and unix sockets)"
            ),
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
        crate::child::set_dumpable(false)?;
    }
    Ok(())
}

/// What `confine` set up, to be read once the process has been started.
pub struct Confinement {
    #[cfg(target_os = "linux")]
    report: crate::child::Report,
    #[cfg(not(target_os = "linux"))]
    denied: bool,
}

impl Confinement {
    /// The layers that confine the process that was started, how its sandbox came out, and the
    /// `errno` of a Command the relay could not start (none when it started). Call it after a
    /// successful spawn, once every handle of the process's builder has been dropped (a write
    /// end of the report pipe is in it).
    #[must_use]
    #[cfg(target_os = "linux")]
    pub fn read(self) -> (Layers, Sandbox, Option<i32>) {
        let (layers, sandbox, start) = self.report.read();
        let refused = match start {
            crate::child::Start::Started => None,
            crate::child::Start::NotStarted(errno) => Some(errno),
        };
        (layers, sandbox, refused)
    }

    /// The layers that confine the process that was started: the `sandbox-exec` profile on macOS
    /// for a denied Command, none on another system. There is no sandbox.
    #[must_use]
    #[cfg(not(target_os = "linux"))]
    pub fn read(self) -> (Layers, Sandbox, Option<i32>) {
        let layers = match (self.denied, cfg!(target_os = "macos")) {
            (false, _) => Layers::Open,
            (true, true) => Layers::Seatbelt,
            (true, false) => Layers::Unavailable,
        };
        (layers, Sandbox::Unavailable, None)
    }
}

/// Applies `network`, and the sandbox's `layout` ([`crate::sandbox`]), to the process about to
/// be started. This is the one place a confinement layer is added. On Linux, in the child before
/// the `exec`: the namespaces of its own and the sandbox's mounts ([`crate::child`]), then, for a
/// Command denied the network, the seccomp filter, which is last because the namespace steps need
/// an `AF_INET` socket. With `relayed` (the program `process` starts is this binary's relay,
/// [`crate::relay`]), the `unshare` also makes a PID namespace, and the relay gets the report
/// pipe as its standard input. See the module documentation.
///
/// # Errors
/// [`RunError::Io`] when the pipe the child reports on cannot be made.
#[cfg(target_os = "linux")]
pub fn confine(
    process: &mut Process,
    network: Network,
    layout: Layout,
    relayed: bool,
) -> Result<Confinement, RunError> {
    confine_with(process, network, layout, relayed, crate::child::make_pipe)
}

/// [`confine`] with the pipe maker a parameter, so a failing one can be tried.
#[cfg(target_os = "linux")]
fn confine_with(
    process: &mut Process,
    network: Network,
    layout: Layout,
    relayed: bool,
    pipe: fn() -> std::io::Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd)>,
) -> Result<Confinement, RunError> {
    let filters = (network == Network::Denied).then(|| Filters {
        strict: seccomp::filter(false),
        in_netns: seccomp::filter(true),
    });
    let uid = rustix::process::geteuid().as_raw();
    let gid = rustix::process::getegid().as_raw();
    let (mut plan, report, stdin) = crate::child::plan(
        uid,
        gid,
        filters.is_some(),
        relayed,
        layout.into_steps(),
        pipe,
    )
    .map_err(|source| RunError::Io {
        path: "the confinement report pipe".into(),
        source,
    })?;
    // SAFETY: the hook runs between fork and exec in the child and makes only system calls on
    // memory built before the fork (`child_setup`): no allocation, no lock, no panic.
    unsafe {
        process.pre_exec(move || child_setup(&mut plan, filters.as_ref()));
    }
    if let Some(stdin) = stdin {
        process.stdin(std::process::Stdio::from(stdin));
    }
    Ok(Confinement { report })
}

/// The two seccomp programs, built before the fork; the child installs one.
#[cfg(target_os = "linux")]
struct Filters {
    strict: Vec<libc::sock_filter>,
    in_netns: Vec<libc::sock_filter>,
}

/// What the system's `confine` does where there is no seccomp filter. On macOS the confine is the
/// `sandbox-exec` launch ([`launch`]), which the caller has already used to make `process`: this
/// checks that it did (the program is `sandbox-exec`), and refuses a process that is not, so a
/// caller that forgot [`launch`] cannot run a denied Command with the network. Elsewhere no
/// confine is compiled in: a denied Command is refused ([`RunError::NoConfine`], exit 15), and the
/// test-only feature `unconfined-for-tests` turns the refusal into a warning line and a run with
/// the network, so a developer's machine can run the suite.
///
/// # Errors
/// [`RunError::NoConfine`] for [`Network::Denied`], on macOS when `process` is not `sandbox-exec`,
/// elsewhere unless the feature is on.
#[cfg(not(target_os = "linux"))]
pub fn confine(
    process: &mut Process,
    network: Network,
    _layout: Layout,
    _relayed: bool,
) -> Result<Confinement, RunError> {
    let denied = network == Network::Denied;
    if cfg!(target_os = "macos") {
        let sandboxed = process.as_std().get_program() == kbf_seatbelt::SANDBOX_EXEC;
        return if sandboxed || !denied {
            Ok(Confinement { denied })
        } else {
            Err(RunError::NoConfine)
        };
    }
    if let Some(warning) = without_confine(network, cfg!(feature = "unconfined-for-tests"))? {
        eprintln!("{warning}");
    }
    Ok(Confinement { denied })
}

/// The program and arguments to start for a Command: the Command itself, or on macOS `sandbox-exec`
/// with the profile of [`seatbelt_profile`] and then the Command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Launch {
    pub program: PathBuf,
    pub args: Vec<OsString>,
}

impl Launch {
    /// How an error names what was started: `program` itself, or, where it is started through
    /// another program (`sandbox-exec`), both.
    #[must_use]
    pub fn name(&self, program: &Path) -> PathBuf {
        if self.program == program {
            program.to_owned()
        } else {
            PathBuf::from(format!(
                "{} for {}",
                self.program.display(),
                program.display()
            ))
        }
    }
}

/// What to start for `program` with `args`. On macOS: `sandbox-exec -p <profile> <program> <args>`,
/// where the profile denies the network to a [`Network::Denied`] Command and, for any Command,
/// reading each of `unreadable` (the files holding a credential or a presigned URL; each is
/// resolved to its real path first, which is how the sandbox names a file; one that does not exist
/// is named as given). On another system:
/// `program` and `args` unchanged ([`confine`] decides there).
///
/// # Errors
/// [`RunError::Io`] for an `unreadable` path that cannot be resolved for a reason but its absence,
/// [`RunError::Confine`] for a path the profile cannot carry.
#[cfg(target_os = "macos")]
pub fn launch(
    program: &Path,
    args: &[String],
    network: Network,
    unreadable: &[PathBuf],
) -> Result<Launch, RunError> {
    let mut real = Vec::with_capacity(unreadable.len());
    for path in unreadable {
        real.push(match std::fs::canonicalize(path) {
            Ok(real) => real,
            // A file that is not there holds nothing to read: it is named as given, made absolute.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => std::path::absolute(path)
                .map_err(|source| RunError::Io {
                    path: path.clone(),
                    source,
                })?,
            Err(source) => {
                return Err(RunError::Io {
                    path: path.clone(),
                    source,
                });
            }
        });
    }
    let profile = seatbelt_profile(network, &real)?;
    let mut all: Vec<OsString> = vec!["-p".into(), profile.into(), program.into()];
    all.extend(args.iter().map(OsString::from));
    Ok(Launch {
        program: PathBuf::from(kbf_seatbelt::SANDBOX_EXEC),
        args: all,
    })
}

/// What to start for `program` with `args`: unchanged, on a system where the Command is not
/// wrapped.
///
/// # Errors
/// None here.
#[cfg(not(target_os = "macos"))]
#[allow(clippy::unnecessary_wraps)] // The macOS form can fail.
pub fn launch(
    program: &Path,
    args: &[String],
    _network: Network,
    _unreadable: &[PathBuf],
) -> Result<Launch, RunError> {
    Ok(Launch {
        program: program.to_owned(),
        args: args.iter().map(OsString::from).collect(),
    })
}

/// The `sandbox-exec` profile of a Command: everything allowed, [`Network::Denied`] limited to
/// loopback and Unix sockets, and one read deny for each of `unreadable` (absolute paths). Not tied
/// to the system, so Linux runs its tests.
///
/// # Errors
/// [`RunError::Confine`] for a path that is not absolute or holds a quote, a backslash, a control
/// character or bytes that are not UTF-8.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn seatbelt_profile(network: Network, unreadable: &[PathBuf]) -> Result<String, RunError> {
    kbf_seatbelt::Profile {
        deny_network: network == Network::Denied,
        deny_read: unreadable.to_vec(),
    }
    .render()
    .map_err(|e| RunError::Confine(e.to_string()))
}

/// What a system with no confine does with `network`: nothing for [`Network::Allowed`]; for
/// [`Network::Denied`] the refusal, or, when `unconfined_for_tests`, the line to print before the
/// Command runs with the network. Not tied to the system, so Linux runs its tests.
///
/// # Errors
/// [`RunError::NoConfine`] for a denied Command when `unconfined_for_tests` is false.
#[cfg_attr(any(target_os = "linux", target_os = "macos"), allow(dead_code))]
pub(crate) fn without_confine(
    network: Network,
    unconfined_for_tests: bool,
) -> Result<Option<&'static str>, RunError> {
    match (network, unconfined_for_tests) {
        (Network::Allowed, _) => Ok(None),
        (Network::Denied, true) => Ok(Some(UNCONFINED_WARNING)),
        (Network::Denied, false) => Err(RunError::NoConfine),
    }
}

/// The line printed when the feature `unconfined-for-tests` lets a denied Command run with the network.
#[cfg_attr(any(target_os = "linux", target_os = "macos"), allow(dead_code))]
pub(crate) const UNCONFINED_WARNING: &str = "kbf-task: this system cannot deny the Command the network; it runs with it (unconfined-for-tests)";

/// The filter that lets `AF_INET` through is for a Command that is certainly in a namespace of
/// its own: every step of it must have succeeded (`netns_made`). Any other outcome, root
/// included, gets the strict one.
#[cfg(target_os = "linux")]
fn pick<'a>(
    netns_made: bool,
    strict: &'a [libc::sock_filter],
    in_netns: &'a [libc::sock_filter],
) -> &'a [libc::sock_filter] {
    if netns_made { in_netns } else { strict }
}

/// Everything the child does between `fork` and `exec`, in order: the namespace and mount
/// steps, the filter that follows from how they went (none for a Command allowed the network),
/// and the report to the parent. Only system calls on memory made before the fork. An error is
/// the errno of a filter that could not be installed, which stops the Command from starting.
#[cfg(target_os = "linux")]
fn child_setup(plan: &mut crate::child::Plan, filters: Option<&Filters>) -> std::io::Result<()> {
    let outcome = plan.apply();
    if let Some(filters) = filters {
        seccomp::install(pick(
            plan.netns_made(outcome),
            &filters.strict,
            &filters.in_netns,
        ))?;
    }
    plan.tell(outcome);
    Ok(())
}

#[cfg(target_os = "linux")]
mod seccomp {
    use std::io;

    use libc::sock_filter;

    use crate::child::cvt;

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
        cvt(libc::c_long::from(unsafe {
            libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0)
        }))?;
        // SAFETY: `program` and the filter it points at outlive the call, which copies them.
        cvt(unsafe {
            libc::syscall(
                libc::SYS_seccomp,
                libc::SECCOMP_SET_MODE_FILTER,
                0,
                &raw const program,
            )
        })
        .map(drop)
    }
}

#[cfg(test)]
#[path = "network_tests.rs"]
mod tests;
