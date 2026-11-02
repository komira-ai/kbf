//! The host side of kbf's releases: where a host keeps them, the state it records about
//! them, the verbs that install, stage, roll back and remove them, and the offline check
//! that decides whether a release handed over by the unprivileged `kbf` may be staged.
//!
//! The host's records:
//!
//! - [`layout`]: the paths ([`layout::Layout`], a directory tree and its
//!   [`layout::Platform`]): each release in its own directory under
//!   `/var/lib/kbf-installer/releases/<commit>/`, the `current` and `previous` links beside them,
//!   the state under `/var/lib/kbf-installer/` (Linux) or `/var/db/kbf-installer/` (macOS) and
//!   its trust files under `/usr/share/kbf/trust/`.
//! - [`state`]: `state.json` (schema 2: the current, previous and staged commits, the one
//!   waiting to be confirmed, the order floor and the rest), with `state.json.bak` as the last
//!   good copy, read and written under an exclusive lock and replaced whole by a rename.
//! - [`release`]: the `install`, `stage`, `rollback` and `gc` verbs.
//! - [`activate`]: the swap on trial: `activate`, `confirm`, `revert`, the start check
//!   and the expiry, and the repair of the links after an interrupted verb.
//! - [`meta_log`]: the node's metadata log, saved at a release's first start and put back when
//!   the host goes back to the release that ran before (a binary from before skipped entries
//!   refuses a raised log).
//! - [`CommitId`]: a commit as 40 lowercase hex characters, the only form a release is
//!   named by.
//!
//! The root helper of the pull-based updates is [`helper`], run by the `kbf-installer`
//! binary (`run`, `boot-check`). The binary says what it is in [`version`]: `--version`
//! and `--version --json` name the commit and the profile it was built with.
//!
//! The offline checks of a release a fetch brought in:
//!
//! - [`spool`]: a directory the unprivileged process writes (the order spool), opened
//!   without following a link, each file judged by its descriptor.
//! - [`attest`]: `gh attestation verify`, pinned, offline, as an unprivileged user.
//! - [`trust`] and [`settings`]: what the check believes without looking at its content
//!   (`gh`, the trusted root, the host's settings) and the root-ownership check on it.
//! - [`stage`]: the verify pipeline, in the order of the contract: size, digest,
//!   attestation, shape, the binary's own report, the lease check, then the install behind a
//!   `VERIFIED` marker; behind the [`stage::Verifier`] trait.
//! - [`seed`]: the release the package itself carries, its manifest, and the rules by which an
//!   order that names it ([`helper::fetcher_seed`]) is admitted. It goes through the same
//!   [`stage`] pipeline as a fetched release.
//! - [`seq_floor`]: the highest `seq` a staged order may not use, kept at what the host has
//!   confirmed; the writer of the file [`seed`] reads.
//! - [`archive`], [`binary`] and [`darwin`]: the tarball, the kind of executable and its
//!   `--version --json`, and on macOS the quarantine attribute and the code signature.
//! - [`proxy_verb`]: `kbf-installer proxy`, the host of the `kbf-proxy` runtime (the one
//!   public port of a frontend host); it shares nothing with the root helper above.
//! - `proxy_pool`: what the proxy verb serves, a load balancer over frontends that run on
//!   their own (`[[frontend]]` tables). The two-slot supervision of frontend children that
//!   the verb once had was removed.
//! - [`run`]: the one place a process is started, behind a trait a test plays.
//! - [`refusal`]: every way a stage is refused, one enum with a stable code per case.
//!
//! `kbf-deploy host ...` (the manual path) calls these; so does any other program that
//! changes a host's releases, so that a host has one state file and one layout. The
//! trust rests on GitHub's build attestation of the artifacts workflow on the host's
//! source ref (docs/deploy/host-releases.md, "What is trusted").

pub mod activate;
pub mod app;
pub mod archive;
pub mod attest;
pub mod binary;
pub mod cli;
pub mod darwin;
pub mod fetch;
#[cfg(any(test, feature = "testkit"))]
pub mod fixtures;
#[cfg(any(test, feature = "testkit"))]
pub mod golden;
pub mod helper;
pub mod host_mode;
pub mod meta_log;
mod proxy_pool;
pub mod proxy_verb;
pub mod record;
pub mod record_release;
pub mod refusal;
pub mod release;
pub mod retain;
pub mod run;
pub mod seed;
#[cfg(test)]
mod seed_fixture;
pub mod seq_floor;
pub mod settings;
pub mod spool;
pub mod stage;
pub mod state;
pub mod state_v2;
pub mod status_v2;
#[cfg(any(test, feature = "testkit"))]
pub mod testkit;
pub mod trust;
pub mod version;

// These live in `kbf-setup` (the library behind `setup`) and keep their old paths here:
// `kbf_installer::layout`, `kbf_installer::setup`, `kbf_installer::CommitId`, ...
pub use kbf_setup::{
    CommitId, NotACommit, database, exit, installer_toml, layout, setup, sha256_hex,
};

#[cfg(test)]
mod tests;
#[cfg(test)]
mod units_tests;
