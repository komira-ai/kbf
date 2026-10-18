//! Adoption of a host that runs the old layout (contract section 8, P3;
//! pull-updates.md section 7.4): `/usr/local/kbf` with its `current` and `previous`
//! links, the deploy state in `/var/lib/kbf-deploy` (`state.json` schema 1 and the old
//! helper's `helper.json`), and units that run `/usr/local/kbf/current/kbf`. It moves to
//! the installer's tree with `state.json` schema 2, once, and keeps the old layout as the
//! rollback path. The test-farm hosts that run the dev installer have no `VERIFIED`
//! markers and no attestations; this is how they come over.
//!
//! The steps, each safe to run again (the re-run rule is stated at each):
//!
//! 1. **Read** the old state, memory and links. A swap on trial refuses the migration
//!    ([`MigrateError::Pending`]); links that disagree with the state refuse it
//!    ([`MigrateError::Disagree`]). Nothing is changed yet; a dry run stops after step 3.
//! 2. **Seed the floor** ([`floor`]): the old (epoch, order id) is carried over; the
//!    generation comes from the frontend, from `--min-generation`, or is zero with a
//!    warning. Re-run: the floor only goes up.
//! 3. **Name each release from its binary** ([`probe`]): `--version --json` as the fetch
//!    user, bounded. A directory whose name is not the build inside is refused.
//! 4. **Adopt each release** (`current`, then `previous`): ask the verify pipeline
//!    ([`Verifier`]) to fetch and verify it; when it cannot, copy the old directory by
//!    descriptor ([`tree`]) as **unverified** (no `VERIFIED` marker), say so in the
//!    report and in `status.json`, and never mark it verified. Re-run: a release with
//!    `VERIFIED` is skipped, a copy with the source's digest is kept, any other leftover
//!    is replaced, scratch is swept.
//! 5. **Stop, repoint, start.** The old helper's unit and the kbf units that run are
//!    stopped (the list is journalled first), the helper is disabled so only one tool
//!    manages the host, then `releases/current` and `previous`, `state.json`,
//!    `status.json` and the units' `Exec*=` lines are written, and only then are the
//!    stopped units started. Re-run: links and units are compared and only what differs
//!    is done; a unit stopped by an earlier run is started by this one.
//! 6. **Journal** `migration.json` says `done`. From then on `migrate` is a no-op and
//!    [`guard`] lets `setup` through.
//!
//! [`rollback`] undoes steps 5 and 6 while the installer has not moved on: the units get
//! their originals back, the helper is enabled again, the units are started. The old
//! tree and state were never touched, which is why this is possible.
//!
//! Stand-ins, to be replaced without changing this module's callers: [`Verifier`] by the
//! verify pipeline (P8), [`floor::FrontendFloor`] by the fetch client (P7), [`schema`]'s
//! `NewState` and `NewStatus` by P1/P2's types, and [`roots::Roots`] by `layout.rs` (P2).

mod apply;
pub mod error;
pub mod floor;
mod newtree;
mod old;
mod probe;
mod rollback;
pub mod roots;
pub mod schema;
mod services;
pub mod tree;
pub mod units;

use kbf_host::Mode;

pub use apply::migrate;
pub use error::MigrateError;
pub use floor::{FrontendFloor, NoFrontend, Seed, Source};
pub use rollback::rollback;
pub use roots::Roots;
pub use units::Unit;

use crate::CommitId;
use crate::binary::Version;
use crate::layout::Layout;
use crate::run::{RunAs, Runner};

/// The verify pipeline of contract P8, as the migration needs it: fetch the release for
/// `claimed` from the configured source, run the whole verification of section 5 on it
/// (the offer metadata the source gives is an untrusted claim that the pipeline
/// re-derives; `probed` is what the old binary said about itself), and install it as
/// `releases/<claimed>` with `VERIFIED`. An error is the reason it cannot, and the
/// migration then adopts the old directory as unverified.
pub trait Verifier {
    fn install_verified(&self, claimed: &CommitId, probed: &Version) -> Result<(), String>;
}

/// The verifier of a build without the pipeline: it never verifies, so every release is
/// adopted as unverified and says why.
pub struct NotWired;

impl Verifier for NotWired {
    fn install_verified(&self, _: &CommitId, _: &Version) -> Result<(), String> {
        Err("the verify pipeline is not part of this build (contract P8)".to_owned())
    }
}

/// What a migration needs to know, all of it from the operator's flags.
pub struct Migration {
    pub layout: Layout,
    pub roots: Roots,
    /// Who the version probe runs as (`kbf-fetch`).
    pub run_as: RunAs,
    /// The user that must own every file read and written: root on a host.
    pub owner_uid: u32,
    /// The kbf units to repoint.
    pub units: Vec<Unit>,
    /// The old helper's unit, to stop and disable.
    pub helper_unit: Option<Unit>,
    /// The installer's own unit: a rollback stops and disables it first, so the
    /// installer and the old helper never both run.
    pub installer_unit: Option<Unit>,
    pub seed: Seed,
}

/// The programs and servers a migration asks.
pub struct Hooks<'a> {
    pub runner: &'a dyn Runner,
    pub verifier: &'a dyn Verifier,
    pub frontend: &'a dyn FrontendFloor,
}

/// How a call ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// There is no old layout here.
    NothingToMigrate,
    /// An earlier migration finished.
    AlreadyMigrated,
    /// A dry run: what would be done.
    Planned,
    Migrated,
    RolledBack,
}

/// What was done (or, in a dry run, would be), for the operator.
#[derive(Debug)]
pub struct Report {
    pub outcome: Outcome,
    pub lines: Vec<String>,
    /// Things the operator must act on or know; printed after the lines.
    pub warnings: Vec<String>,
    /// The releases adopted without verification.
    pub unverified: Vec<CommitId>,
    /// The value `setup` writes into `installer.toml` as `min_generation`.
    pub min_generation: Option<u64>,
}

impl Report {
    fn new(outcome: Outcome) -> Report {
        Report {
            outcome,
            lines: Vec::new(),
            warnings: Vec::new(),
            unverified: Vec::new(),
            min_generation: None,
        }
    }

    fn line(&mut self, mode: Mode, text: impl std::fmt::Display) {
        let mark = match mode {
            Mode::DryRun => "DRY-RUN:",
            Mode::Apply => "+",
        };
        self.lines.push(format!("{mark} {text}"));
    }
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for line in &self.lines {
            writeln!(f, "{line}")?;
        }
        for warning in &self.warnings {
            writeln!(f, "WARNING: {warning}")?;
        }
        Ok(())
    }
}

/// The check `setup` and every verb that changes releases makes first: while the old
/// layout's state is the host's (it exists and no migration finished), they refuse, so
/// the old helper and the installer never both manage one host.
pub fn guard(m: &Migration) -> Result<(), MigrateError> {
    let rules = apply::rules(m);
    if old::read(&m.layout, &m.roots, &rules)?.is_none() {
        return Ok(());
    }
    let done = newtree::StateDir::open(&m.layout, &m.roots, &rules)?
        .map(|t| t.journal())
        .transpose()?
        .flatten()
        .is_some_and(|j| j.phase == schema::Phase::Done);
    if done {
        Ok(())
    } else {
        Err(MigrateError::Unmigrated(m.roots.old_state.to_string()))
    }
}

#[cfg(test)]
mod floor_tests;
#[cfg(test)]
mod refusal_tests;
#[cfg(test)]
mod rig;
#[cfg(test)]
mod rollback_tests;
#[cfg(test)]
mod testfarm_tests;
#[cfg(test)]
mod tests;
