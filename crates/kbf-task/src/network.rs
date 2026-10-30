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
//! | `socket`, `socketpair` | unless the domain is `AF_UNIX`. Every other domain is refused, not only `AF_INET` and `AF_INET6`: `AF_NETLINK` (interface and routing queries), `AF_PACKET`, `AF_VSOCK`, `AF_BLUETOOTH`, `AF_XDP` and the rest, so a protocol family added later is refused too |
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
//! # What it does not do
//!
//! The filter stops the Command from making a socket. It does not stop it from using one it is
//! handed. The Command runs as the same user as this tool, so a sandbox that keeps this tool's
//! `/proc` entry out of its reach (`ptrace`, `pidfd_getfd`) is what protects the sockets this
//! tool holds open; a fresh network namespace is the second layer that makes the first
//! unnecessary to trust, and [`confine`] is where it is added.
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

/// Applies `network` to the process about to be started. This is the one place a confinement
/// layer is added; today there is one, the seccomp filter.
///
/// # Errors
/// None today; the second layer may fail to be set up.
pub fn confine(process: &mut Process, network: Network) -> Result<(), RunError> {
    match network {
        Network::Allowed => Ok(()),
        Network::Denied => deny_sockets(process),
    }
}

#[cfg(not(target_os = "linux"))]
fn deny_sockets(_process: &mut Process) -> Result<(), RunError> {
    eprintln!("kbf-task: this system cannot deny the Command the network; it runs with it");
    Ok(())
}

#[cfg(target_os = "linux")]
fn deny_sockets(process: &mut Process) -> Result<(), RunError> {
    let filter = seccomp::filter();
    // SAFETY: the hook runs between fork and exec in the child and only makes the two system
    // calls of `install` on memory built before the fork: no allocation, no lock, no panic.
    unsafe {
        process.pre_exec(move || seccomp::install(&filter));
    }
    Ok(())
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
    pub fn filter() -> Vec<sock_filter> {
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
        let tail = [
            (LOAD, OFFSET_ARG0, To::Next, To::Next),
            (JEQ, AF_UNIX, To::Allow, To::Deny),
            (RET, ALLOW, To::Next, To::Next),
            (RET, DENY, To::Next, To::Next),
            (RET, KILL, To::Next, To::Next),
        ];
        let index = |to: To, from: usize| -> u8 {
            let at = match to {
                To::Next => from + 1,
                To::Allow => n + 2,
                To::Domain => n,
                To::Deny => n + 3,
                To::Kill => n + 4,
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
