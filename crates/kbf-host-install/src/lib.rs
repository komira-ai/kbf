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

/// What a client of the forced command may rely on: the answers it prints and the requests
/// it accepts. The client (`kbf-updater`) dev-depends on this to test its reading of the
/// answers against the real types; nothing here is for production code.
pub mod contract {
    pub use crate::outcome::{Failure, FailureCode, Outcome, QueryReport, Refusal};

    /// The version and the channel of `command`, when it is an `install` request this
    /// program accepts; `None` for anything it would refuse and for `query`.
    #[must_use]
    pub fn install_request(command: &str) -> Option<(String, String)> {
        match crate::request::Request::parse(command) {
            Ok(crate::request::Request::Install { version, channel }) => {
                Some((version.as_str().to_owned(), channel.as_str().to_owned()))
            }
            _ => None,
        }
    }

    /// Whether `command` is the `query` request.
    #[must_use]
    pub fn is_query(command: &str) -> bool {
        matches!(
            crate::request::Request::parse(command),
            Ok(crate::request::Request::Query)
        )
    }
}

/// The program: handle `SSH_ORIGINAL_COMMAND` against the production paths and exit with
/// the status of the answer.
#[must_use]
pub fn main() -> ExitCode {
    let command = std::env::var_os("SSH_ORIGINAL_COMMAND");
    let code = host::handle(
        &host::Host::production(),
        &runner::SystemRunner,
        command.as_deref(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    );
    ExitCode::from(code)
}
