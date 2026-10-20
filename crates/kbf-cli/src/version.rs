//! `kbf --version --json`: what this build is, for a host to check what it runs.
//!
//! One JSON object on one line of stdout, exit 0. The fields are the contract (a host's
//! check and the artifacts workflow, `cargo xtask build-commit check --json`, read
//! them), sorted by name:
//!
//! - `admin_protocol`: the version of the admin calls of `kbf --mode frontend` and of the
//!   `role` answer ([`kbf_server::managed::ADMIN_PROTOCOL`]). It is the versioned contract
//!   between the proxy and the child processes it starts: `GET role`, `POST quiesce`,
//!   `resume`, `takeover` and `stand-down` (`docs/api.md`);
//! - `commit`: the first 12 hex digits of the commit built, or `unknown` for a build
//!   with no git checkout ([`kbf_server::BUILD_COMMIT`]);
//! - `meta_log`: the highest metadata-log machine version this build runs
//!   ([`kbf_server::raft_log::RUNS`], read through `.get()`; there is no second copy of the
//!   number). A rollout compares it across two builds: going from N to N-1 is reversible
//!   only if `meta_log(N) <= meta_log(N-1)`, because a log raised past what N-1 runs
//!   cannot be opened by it. A build that does not print the field is unknown, and
//!   unknown is not reversible;
//! - `min_protocol` and `protocol`: the oldest and the newest `kbf.worker.v1` protocol
//!   version this binary speaks ([`kbf_server::worker`]); the daemon half speaks exactly
//!   `protocol` (a test pins the two halves equal);
//! - `profile`: the Cargo profile, `release` or `debug` ([`PROFILE`]);
//! - `state_schema`: the newest Postgres state schema this binary embeds a migration for
//!   ([`kbf_state::postgres::schema::latest`]); `kbf-installer` migrates the database to it
//!   before this release starts;
//! - `version`: the package version.
//!
//! Only the two words `--version --json`, in either order and nothing else, ask for it;
//! `--json` anywhere else is the usage error it always was.

use std::ffi::OsString;

use serde_json::{Map, Value};

/// The Cargo profile this binary was built with: `release` or `debug`.
pub const PROFILE: &str = env!("KBF_BUILD_PROFILE");

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
    fields.insert(
        "admin_protocol".to_owned(),
        kbf_server::managed::ADMIN_PROTOCOL.into(),
    );
    fields.insert("commit".to_owned(), kbf_server::BUILD_COMMIT.into());
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
    fields.insert(
        "state_schema".to_owned(),
        kbf_state::postgres::schema::latest().into(),
    );
    fields.insert("version".to_owned(), env!("CARGO_PKG_VERSION").into());
    Value::Object(fields)
}

/// The line `kbf --version --json` prints, with its line end.
#[must_use]
pub fn line() -> String {
    format!("{}\n", identity())
}
