//! `kbf-installer --version` and `kbf-installer --version --json`: what this build is,
//! for a host (or the artifacts workflow) to check what it runs.
//!
//! `--version` prints `kbf-installer <package version>+<first 12 hex digits of the
//! commit built>` ([`VERSION`]), the line `cargo xtask build-commit check` reads.
//!
//! `--version --json` (exactly those two words, in either order, and nothing else)
//! prints one JSON object on one line of stdout, exit 0, the fields sorted by name:
//!
//! - `commit`: the first 12 hex digits of the commit built, or `unknown` for a build with
//!   no git checkout ([`BUILD_COMMIT`]);
//! - `profile`: the Cargo profile, `release` or `debug` ([`PROFILE`]);
//! - `version`: the package version.
//!
//! These are the fields of `kbf --version --json` (`docs/artifacts.md`) that describe a
//! build. The ones that describe what a kbf release speaks (`protocol`, `min_protocol`,
//! `state_schema`) are the release's to declare and the installer's to check, not its own.
//! The artifacts workflow runs `cargo xtask build-commit check --json` on the binary it
//! builds and `check-asset` on the tarball it packages, so the file that ships names the
//! commit and a release build.

use std::ffi::OsString;

use serde_json::{Map, Value};

/// The first 12 hex digits of the commit this binary is built from, or `unknown`
/// (`build.rs`).
pub const BUILD_COMMIT: &str = env!("KBF_BUILD_COMMIT");

/// The Cargo profile this binary is built with: `release` or `debug` (`build.rs`).
pub const PROFILE: &str = env!("KBF_BUILD_PROFILE");

/// What `--version` prints after the program's name: the package version, `+`, and
/// [`BUILD_COMMIT`].
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "+", env!("KBF_BUILD_COMMIT"));

/// Whether `args` (the program's name first) are exactly `--version --json` or
/// `--json --version`.
#[must_use]
pub fn asked(args: &[OsString]) -> bool {
    matches!(args, [_, a, b] if (a == "--version" && b == "--json") || (a == "--json" && b == "--version"))
}

/// The identity of this build.
#[must_use]
pub fn identity() -> Value {
    // Inserted in the order they print: sorted, whatever map `serde_json` is built with.
    let mut fields = Map::new();
    fields.insert("commit".to_owned(), BUILD_COMMIT.into());
    fields.insert("profile".to_owned(), PROFILE.into());
    fields.insert("version".to_owned(), env!("CARGO_PKG_VERSION").into());
    Value::Object(fields)
}

/// The line `kbf-installer --version --json` prints, with its line end.
#[must_use]
pub fn line() -> String {
    format!("{}\n", identity())
}

#[cfg(test)]
#[path = "version_tests.rs"]
mod tests;
