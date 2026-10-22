//! `kbf-host-install`: the root side of the host-driven package update.
//!
//! `kbf-updater`, on the frontend host, reaches each host over ssh with a key whose
//! `authorized_keys` entry forces this program:
//!
//! ```text
//! from="203.0.113.10",restrict,command="/usr/libexec/kbf/kbf-host-install" ssh-ed25519 AAAA...
//! ```
//!
//! sshd then runs it with no arguments, and the request is the one string in
//! `SSH_ORIGINAL_COMMAND`. Exactly two requests are accepted, matched as a whole string
//! ([`request`]):
//!
//! - `install kbf=<version> from <channel>`: install that version of the `kbf` package
//!   from a channel the root-owned allowlist names ([`config`]), never an older version
//!   than the installed one ([`debver`]), through `apt-get` restricted to the channel's one
//!   signed source, in a transient systemd unit an ssh drop cannot kill ([`plan`]);
//! - `query`: print what is installed, what runs and whether an install is in progress.
//!
//! Nothing runs through a shell: every process is an `execve` with an explicit argument
//! vector and a cleared environment ([`runner`]). The answer is one JSON line on stdout
//! ([`outcome`]); the exit status is 0 (done), 2 (refused: nothing ran) or 3 (failed). Every
//! invocation appends one line to the audit log ([`audit`]).
//!
//! The paths this program reads as root are constants of [`host::Host::production`]. The
//! only way to point them elsewhere is a `cfg(test)` constructor: no environment variable
//! and no argument does it.

#![forbid(unsafe_code)]

mod audit;
mod config;
mod debver;
mod host;
mod locks;
mod outcome;
mod plan;
mod request;
mod runner;
mod trust;

use std::process::ExitCode;

/// The program: handle `SSH_ORIGINAL_COMMAND` against the production paths and exit with
/// the status of the answer.
#[must_use]
pub fn main() -> ExitCode {
    let code = host::dispatch(
        &host::Host::production(),
        &runner::SystemRunner,
        &host::Environment::from_process(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    );
    ExitCode::from(code)
}
