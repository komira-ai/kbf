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
//! `SSH_ORIGINAL_COMMAND`. (The one argument it takes anywhere is `--version`, which prints
//! the name and the package version and runs nothing; the release checks the signed Mac
//! binary with it. `--version` beside anything else is refused with the closed line,
//! unaudited: that is not an sshd request and changes nothing.) Exactly two requests are accepted, matched as a whole string
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

/// The one line `--version` prints, which the release pipeline reads from the signed
/// binary: the name and the package version.
fn version_line() -> String {
    format!("kbf-host-install {}", env!("CARGO_PKG_VERSION"))
}

/// Whether the arguments are exactly `--version`. sshd passes none, so this never answers
/// a request; it is how the release checks the binary it signed runs.
fn asks_for_version(args: &[std::ffi::OsString]) -> bool {
    matches!(args, [only] if only == "--version")
}

/// Whether an argument tries to be the version flag (`--version`, `--version=1`) without the
/// arguments being exactly that. Such an invocation is not an sshd request (sshd passes no
/// arguments) and changes nothing, so it is refused before the audit log is opened.
fn names_version(args: &[std::ffi::OsString]) -> bool {
    args.iter()
        .any(|arg| arg.to_str().is_some_and(|a| a.starts_with("--version")))
}

/// The program: handle `SSH_ORIGINAL_COMMAND` against the production paths and exit with
/// the status of the answer.
#[must_use]
pub fn main() -> ExitCode {
    let args: Vec<std::ffi::OsString> = std::env::args_os().skip(1).collect();
    if asks_for_version(&args) {
        println!("{}", version_line());
        return ExitCode::SUCCESS;
    }
    if names_version(&args) {
        return ExitCode::from(host::respond(
            &outcome::Outcome::Refused(outcome::Refusal::BadCommand),
            &mut std::io::stdout(),
            &mut std::io::stderr(),
        ));
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsString;

    fn args(words: &[&str]) -> Vec<OsString> {
        words.iter().map(OsString::from).collect()
    }

    #[test]
    fn only_the_lone_flag_asks_for_the_version() {
        assert!(asks_for_version(&args(&["--version"])));
        for other in [
            &[][..],
            &["-V"],
            &["--version", "query"],
            &["query"],
            &["--versions"],
        ] {
            assert!(!asks_for_version(&args(other)), "{other:?}");
        }
    }

    #[test]
    fn a_version_flag_beside_anything_else_is_named_but_does_not_ask() {
        for other in [
            &["--version", "query"][..],
            &["--version=1"],
            &["x", "--version"],
        ] {
            assert!(names_version(&args(other)), "{other:?}");
            assert!(!asks_for_version(&args(other)), "{other:?}");
        }
        for other in [&[][..], &["-V"], &["query"]] {
            assert!(!names_version(&args(other)), "{other:?}");
        }
    }

    #[test]
    fn the_version_line_names_the_binary_and_the_package_version() {
        assert_eq!(
            version_line(),
            format!("kbf-host-install {}", env!("CARGO_PKG_VERSION"))
        );
    }
}
