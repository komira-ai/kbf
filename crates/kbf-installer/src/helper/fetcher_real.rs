//! The real [`Fetcher`]: the unprivileged fetch child brings each component of an order into
//! a scratch directory of root's ([`crate::fetch`]), then the verify pipeline
//! ([`crate::stage`]) checks what arrived in the order of the contract and installs it as
//! `releases/<commit>/`. This module is the joint between the two and decides nothing
//! itself: every check is one of theirs.
//!
//! For one release it
//!
//! 1. refuses an order that names no component (nothing is made);
//! 2. makes `work/<commit>.partial/` ([`Partial`]);
//! 3. pulls each component into a new file in its `dl/`, with the digest and size the order
//!    names ([`fetch::fetch_component`]);
//! 4. hands the files to [`Pipeline::verify`], with the order's kind and, for an
//!    activation, the facts it expects;
//! 5. reports what the installed binary declared ([`crate::stage::Staged::installed`]).
//!
//! The scratch directory is removed on every way out but a success: a [`Partial`] removes
//! itself when dropped, and the pipeline takes it (and drops it on a refusal).
//!
//! Only an activation is complete. A `frontend_update` order names no component and nothing
//! yet looks a release up by its commit, so its fetch ends with a message that says so.
//!
//! The binary does not build a [`RealFetcher`] yet: `build_helper` still gives the helper
//! [`NotWired`](super::fetcher::NotWired). A real one is unreachable until the helper has a
//! real lease source (the cycle checks leases before it fetches, and the pipeline checks again,
//! and [`leases::NotWired`](super::leases::NotWired) refuses both), and building it needs the
//! source, timeout and size cap of `installer.toml` on every host (`run` reads the file only
//! when `--installer-config` is given), a [`Policy`] (nothing builds one outside the tests) and
//! the fetch user on the host.

use std::ffi::OsStr;
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;

use kbf_host::exec::Exec;
use kbf_host::step::Owner;
use kbf_update::{Facts, Locator};

use crate::fetch::pipe::PipeFault;
use crate::fetch::{self, ChildProgram, Component, Limits, Source};
use crate::helper::fetcher::{FetchError, Fetcher, InstalledRelease, ReleaseLocator};
use crate::helper::leases::LeaseSource;
use crate::layout::Layout;
use crate::refusal::Refusal;
use crate::run::Runner;
use crate::settings::Settings;
use crate::stage::{Fetched, Order, OrderKind, Partial, Pipeline, Policy, Received, Verifier};
use crate::state;

/// Why a `frontend_update` is not fetched yet.
pub const NO_COMPONENT_LIST: &str = "a frontend_update names no component, and nothing yet \
     looks up a release's components by its commit (the release-describe design is open)";

/// What pulling and verifying a release needs from the host. The fields are the pipeline's
/// own, plus where the bytes come from.
pub struct RealFetcher {
    pub host: Layout,
    pub settings: Settings,
    pub policy: Policy,
    /// Says who owns the trust list and reads it; the host's own on a host.
    pub trust: Box<dyn Exec>,
    pub runner: Box<dyn Runner>,
    /// Observed again at the pipeline's lease step, after the cycle's own check.
    pub leases: Box<dyn LeaseSource>,
    /// Who owns the scratch directory and the release: root on a host.
    pub owner: Owner,
    pub child: ChildProgram,
    pub source: Source,
    pub limits: Limits,
}

impl Fetcher for RealFetcher {
    fn fetch(&self, rec: &ReleaseLocator) -> Result<InstalledRelease, FetchError> {
        self.pull(OrderKind::FrontendUpdate, rec, None)
    }

    fn fetch_activation(
        &self,
        rec: &ReleaseLocator,
        expected: &Facts,
    ) -> Result<InstalledRelease, FetchError> {
        self.pull(OrderKind::Activate, rec, Some(expected))
    }
}

impl RealFetcher {
    fn pull(
        &self,
        kind: OrderKind,
        rec: &ReleaseLocator,
        expected: Option<&Facts>,
    ) -> Result<InstalledRelease, FetchError> {
        // Before anything exists: the pipeline has no release to name without a component.
        if rec.components.is_empty() {
            return Err(match kind {
                OrderKind::FrontendUpdate => FetchError::Unavailable(NO_COMPONENT_LIST.to_owned()),
                OrderKind::Activate => {
                    FetchError::Rejected("the order names no component to fetch".to_owned())
                }
            });
        }
        let replaced = replaced_bytes(&self.host);
        let partial = Partial::create(&self.host, &rec.commit, self.owner).map_err(refused)?;
        let mut files = Vec::with_capacity(rec.components.len());
        for locator in &rec.components {
            // A failure drops `partial`, which removes the directory.
            files.push(self.pull_one(&partial, locator, replaced)?);
        }
        let order = Order {
            kind,
            commit: &rec.commit,
            components: &rec.components,
            expected,
        };
        let pipeline = Pipeline {
            host: &self.host,
            settings: &self.settings,
            policy: self.policy,
            trust: &*self.trust,
            runner: &*self.runner,
            leases: &*self.leases,
            owner: self.owner,
        };
        pipeline
            .verify(&order, Fetched { partial, files })
            .map(|staged| staged.installed())
            .map_err(refused)
    }

    /// One component through the child into `dl/<name>.tar.gz`.
    fn pull_one(
        &self,
        partial: &Partial,
        locator: &Locator,
        replaced: u64,
    ) -> Result<Received, FetchError> {
        // The name becomes a file name in root's directory.
        if !is_plain_name(&locator.name) {
            return Err(FetchError::Rejected(format!(
                "{:?} is not a plain file name",
                locator.name
            )));
        }
        let dl = partial.dl();
        let tarball = dl.join(format!("{}.tar.gz", locator.name));
        // The file is the parent's: made here, new, in a directory only its owner enters.
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tarball)
            .map_err(|e| {
                FetchError::Unavailable(format!("cannot create {}: {e}", tarball.display()))
            })?;
        let got = fetch::fetch_component(
            &self.child,
            &self.source,
            self.limits,
            &Component {
                sha256: &locator.sha256,
                size: locator.size,
            },
            (&dl, replaced),
            &mut out,
        )
        .map_err(fetch_error)?;
        Ok(Received {
            name: locator.name.clone(),
            tarball,
            bytes: got.count,
            sha256: got.sha256,
        })
    }
}

/// A name that is one file name: no separator, not `.` or `..`, not empty.
fn is_plain_name(name: &str) -> bool {
    Path::new(name).file_name() == Some(OsStr::new(name))
}

/// The bytes of the release that is current, which the new one sits beside until the swap
/// is confirmed. Counted for the disk check only, so a state or a directory that cannot be
/// read counts as none: the pipeline reads them again and refuses.
fn replaced_bytes(host: &Layout) -> u64 {
    let Ok(Some(state)) = state::read_unlocked(host) else {
        return 0;
    };
    let Some(current) = state.current else {
        return 0;
    };
    let Ok(entries) = std::fs::read_dir(host.join(&host.release_dir(&current))) else {
        return 0;
    };
    entries
        .flatten()
        .filter_map(|entry| entry.metadata().ok())
        .filter(std::fs::Metadata::is_file)
        .map(|meta| meta.len())
        .sum()
}

/// `word: text`, the contract's refusal word first. `fetch` is what an [`Unavailable`]
/// and a [`Rejected`] already begin with, so it is not said twice.
///
/// [`Unavailable`]: FetchError::Unavailable
/// [`Rejected`]: FetchError::Rejected
fn with_word(word: &str, text: &dyn std::fmt::Display) -> String {
    if word == "fetch" {
        text.to_string()
    } else {
        format!("{word}: {text}")
    }
}

/// A fetch step's failure as the trait reports it: a verdict on what was named or what
/// arrived is `Rejected`; a source or host that could not deliver is `Unavailable`.
fn fetch_error(e: fetch::FetchError) -> FetchError {
    use fetch::FetchError as Step;
    let said = with_word(e.word(), &e);
    match e {
        Step::BadDigest(_)
        | Step::Size { .. }
        | Step::Digest { .. }
        | Step::Pipe(PipeFault::Oversize { .. }) => FetchError::Rejected(said),
        Step::Disk { .. } | Step::Source(..) | Step::Identity(_) | Step::Pipe(_) => {
            FetchError::Unavailable(said)
        }
    }
}

/// A pipeline refusal as the trait reports it: a refusal with a contract word is a verdict
/// on the release (`Rejected`, the word first); one without is the host failing to do the
/// work (a scratch directory, a tool that would not start), which says nothing of the release.
fn refused(r: Refusal) -> FetchError {
    match r.word() {
        Some(word) => FetchError::Rejected(with_word(word, &r)),
        None => FetchError::Unavailable(r.to_string()),
    }
}

#[cfg(test)]
#[path = "fetcher_real_tests.rs"]
mod tests;
