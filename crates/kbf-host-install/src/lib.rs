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
//!   from a channel the root-owned allowlist names ([`config`]), older versions included (a rollback is an install; the
//!   same version is a no-op, [`debver`]), through `apt-get` restricted to the channel's one
//!   signed source. The process sshd starts checks the request and starts ONE transient
//!   systemd unit an ssh drop cannot kill; the unit runs this program again, takes the install
//!   lock and holds it for both apt steps ([`plan`], [`host`]);
//! - `query`: print what is installed, what runs, which node this is and whether an install is in progress (the
//!   install lock held, an install unit not over, or the dpkg lock held).
//!
//! Nothing runs through a shell: every process is an `execve` with an explicit argument
//! vector and a cleared environment ([`runner`]). The answer is one JSON line on stdout
//! ([`outcome`], every line with a `result`); the exit status is 0 (done), 2 (refused:
//! nothing ran) or 3 (failed). Every invocation appends one line to the audit log
//! ([`audit`]).
//!
//! # A Mac
//!
//! On macOS ([`mac`]) the same two requests install the attested release tarball into the kit
//! layout (`/usr/local/kbf/releases/<commit>/kbf`, links `current` and `previous`) and restart
//! the daemon's launchd job on it. The install verb checks the release on one root-owned
//! copy, launches a one-shot launchd job that does the swap and the restart, and returns; the
//! result is the `apply` field of `query` (`docs/deploy/host-install.md`). The platform picks
//! the side; both are compiled everywhere.
//!
//! The paths this program reads as root are constants of [`host::Host::production`]. The
//! only way to point them elsewhere is a `cfg(test)` constructor: no environment variable
//! and no argument does it. (One variable does exist, `KBF_HOST_INSTALL_UNIT`: it makes the
//! program the body of the install unit, and counts only when sshd set none of its own
//! variables; see [`host::dispatch`].)

#![forbid(unsafe_code)]

mod audit;
mod config;
mod debver;
mod host;
mod locks;
pub(crate) mod mac;
mod node_id;
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

/// The Mac's tool command lines and the readers of what the tools print, with the runner
/// that runs them, for the test that runs them on a real Mac (`tests/real_mac.rs`). Nothing
/// here is for production code.
#[doc(hidden)]
pub mod mac_tools {
    pub use crate::mac::{fetch, release, tools};
    pub use crate::runner::{Cmd, Exec, Identity, Runner, SystemRunner};
}

/// The program: handle `SSH_ORIGINAL_COMMAND` against the production paths and exit with
/// the status of the answer.
#[must_use]
pub fn main() -> ExitCode {
    // Both sides are compiled everywhere, so the Linux build checks the Mac code.
    if cfg!(target_os = "macos") {
        return ExitCode::from(mac::main_process());
    }
    let code = host::dispatch(
        &host::Host::production(),
        &runner::SystemRunner,
        &host::Environment::from_process(),
        &mut std::io::stdout(),
        &mut std::io::stderr(),
    );
    ExitCode::from(code)
}
