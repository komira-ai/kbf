//! `kbf --version --json`: what this build is, for a host to check what it runs.
//!
//! One JSON object on one line of stdout, exit 0. The fields are the contract (a host's
//! check and the artifacts workflow, `cargo xtask build-commit check --json`, read
//! them), sorted by name:
//!
//! - `admin_protocol`: the version of the admin calls and of the `role` answer
//!   ([`ADMIN_PROTOCOL`], fixed at 1: a frontend no longer serves the calls, and the field
//!   stays for the readers of the version line);
//! - `commit`: the first 12 hex digits of the commit built, or `unknown` for a build
//!   with no git checkout ([`kbf_server::BUILD_COMMIT`]);
//! - `commit_full`: the 40 hex digits of the commit built ([`kbf_server::BUILD_COMMIT_FULL`]).
//!   It is absent, not empty, from a build that did not know them (no git checkout, or a
//!   `KBF_BUILD_COMMIT_OVERRIDE` stamp), and a build before this field prints none;
//! - `meta_log`: the highest metadata-log machine version this build runs
//!   ([`kbf_server::raft_log::RUNS`], read through `.get()`; there is no second copy of the
//!   number). It is informational: the constant is frozen and gates no feature, because
//!   this build raises no log and writes every command it knows to a log of any version.
//!   It stays in the line because a rollout compares it across two builds: going from N to
//!   N-1 is reversible only if `meta_log(N) <= meta_log(N-1)`, since a build from before
//!   skipped entries cannot open a log raised past what it runs (a later build opens it,
//!   skips the raise and loses the commands it does not know). A build that does not print
//!   the field is unknown, and unknown is not reversible;
//! - `min_protocol` and `protocol`: the oldest and the newest `kbf.worker.v1` protocol
//!   version this binary speaks ([`kbf_server::worker`]); the daemon half speaks exactly
//!   `protocol` (a test pins the two halves equal);
//! - `profile`: the Cargo profile, `release` or `debug` ([`PROFILE`]);
//! - `relay`: how far this build carries what its host's installer reports to the frontend
//!   ([`RELAY`]); the installer reads it before it rolls a `release_source = "bundled"` host back
//!   to a release, which only a `kbf` that relays can be run beside. A build before this field
//!   prints none;
//! - `seq`: the number of commits behind the commit built (`git rev-list --count HEAD`,
//!   [`build_seq`]), so two builds of one branch are put in order by it: a later commit has
//!   the larger number. It is 0 when the build could not count them (a shallow checkout,
//!   which holds only part of the history, or a source archive with no git), and nothing is
//!   newer than 0. A build before this field prints none;
//! - `state_schema`: the newest Postgres state schema this binary embeds a migration for
//!   ([`kbf_pack_index::schema::latest`]); `kbf boot-check` ([`crate::boot_check`]) reads
//!   the same number and migrates the database to it before a unit starts this binary, and
//!   `kbf-installer` migrates to it before it activates this release on a host that keeps
//!   releases;
//! - `version`: the package version.
//!
//! Only the two words `--version --json`, in either order and nothing else, ask for it;
//! `--json` anywhere else is the usage error it always was.

use std::ffi::OsString;

use serde_json::{Map, Value};

/// The Cargo profile this binary was built with: `release` or `debug`.
pub const PROFILE: &str = env!("KBF_BUILD_PROFILE");

/// The `admin_protocol` the version line prints. The admin port is gone from the frontend;
/// the field stays at the version it had, for the readers of the version line.
pub const ADMIN_PROTOCOL: u32 = 1;

/// How far this build relays what its host's installer reports to the frontend: at level 1 it
/// reads `available` and `release_source` from the installer's `status.json` and sends them in
/// its `UpdateStatus`. The installer will not roll a `release_source = "bundled"` host back to a
/// `kbf` that declares less (`kbf_installer::binary::RELAY_NEEDED`; a test pins that this build
/// meets it). Raised when a later change relays more.
pub const RELAY: u64 = 1;

/// Whether `args` (the program's name first) are exactly `--version --json` or
/// `--json --version`.
#[must_use]
pub fn asked(args: &[OsString]) -> bool {
    matches!(args, [_, a, b] if (a == "--version" && b == "--json") || (a == "--json" && b == "--version"))
}

/// The number of commits behind the commit this binary was built from, or 0 when the build
/// could not count them: [`kbf_server::BUILD_SEQ`], which the server's `build.rs` embeds.
#[must_use]
pub fn build_seq() -> u64 {
    kbf_server::BUILD_SEQ
}

/// The identity of this build.
#[must_use]
pub fn identity() -> Value {
    identity_for(kbf_server::BUILD_COMMIT_FULL)
}

/// The identity of a build whose full commit is `commit_full`: the empty text when the build
/// did not know it, and then the `commit_full` field is left out.
fn identity_for(commit_full: &str) -> Value {
    // Inserted in the order they print: sorted, whatever map `serde_json` is built with.
    let mut fields = Map::new();
    fields.insert("admin_protocol".to_owned(), ADMIN_PROTOCOL.into());
    fields.insert("commit".to_owned(), kbf_server::BUILD_COMMIT.into());
    if !commit_full.is_empty() {
        fields.insert("commit_full".to_owned(), commit_full.into());
    }
    fields.insert(
        "meta_log".to_owned(),
        kbf_server::raft_log::RUNS.get().into(),
    );
    fields.insert(
        "min_protocol".to_owned(),
        kbf_server::worker::OLDEST_PROTOCOL_VERSION.into(),
    );
    fields.insert("profile".to_owned(), PROFILE.into());
    fields.insert(
        "protocol".to_owned(),
        kbf_server::worker::PROTOCOL_VERSION.into(),
    );
    fields.insert("relay".to_owned(), RELAY.into());
    fields.insert("seq".to_owned(), build_seq().into());
    fields.insert(
        "state_schema".to_owned(),
        kbf_pack_index::schema::latest().into(),
    );
    fields.insert("version".to_owned(), env!("CARGO_PKG_VERSION").into());
    Value::Object(fields)
}

/// The line `kbf --version --json` prints, with its line end.
#[must_use]
pub fn line() -> String {
    format!("{}\n", identity())
}

#[cfg(test)]
#[path = "version_tests.rs"]
mod tests;
