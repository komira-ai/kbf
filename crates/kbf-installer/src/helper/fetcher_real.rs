//! The real [`Fetcher`]: the unprivileged fetch child brings each component of an order into
//! a scratch directory of root's ([`crate::fetch`]), then the verify pipeline
//! ([`crate::stage`]) checks what arrived in the order of the contract and installs it as
//! `releases/<commit>/`. This module is the joint between the two and decides nothing
//! itself: every check is one of theirs.
//!
//! For one release it
//!
//! 1. learns what to pull. An order that carries its components (an activation, from the
//!    offer the node holds) is taken as it stands. A `frontend_update` order carries a commit and
//!    nothing else, so the release source is asked what the commit holds for this host's
//!    platform ([`fetch::describe_release`], through the confined child: the root half does not
//!    read the source's bytes); an activation that names no component is refused. Nothing is
//!    made before this;
//! 2. makes `work/<commit>.partial/` ([`Partial`]);
//! 3. pulls each component into a new file in its `dl/`, with the digest and size the order (or
//!    the description) names ([`fetch::fetch_component`]), and for a described component first
//!    its attestation bundle by the digest the description names, which becomes the component's
//!    attestation text as an offer's would be;
//! 4. hands the files to [`Pipeline::verify`], with the order's kind and, for an
//!    activation, the facts it expects;
//! 5. reports what the installed binary declared ([`crate::stage::Staged::installed`]).
//!
//! A description is a claim, as an offer is: it names which digests to fetch and the sizes that
//! bound them, and nothing it says is believed. Every file is counted and hashed as it arrives
//! and held to the digest that named it, and the pipeline then re-derives the rest (the
//! attestation of each tarball against its bundle, the shape, the binary's own report).
//!
//! The scratch directory is removed on every way out but a success: a [`Partial`] removes
//! itself when dropped, and the pipeline takes it (and drops it on a refusal).
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
use kbf_types::ReleasePlatform;
use kbf_update::{Facts, Locator};

use crate::binary::Arch;
use crate::fetch::pipe::{PipeFault, Received as Pulled};
use crate::fetch::{self, ChildProgram, Component, Limits, Source};
use crate::helper::fetcher::{FetchError, Fetcher, InstalledRelease, ReleaseLocator};
use crate::helper::leases::LeaseSource;
use crate::layout::{Layout, Platform};
use crate::refusal::Refusal;
use crate::run::Runner;
use crate::settings::Settings;
use crate::stage::{Fetched, Order, OrderKind, Partial, Pipeline, Policy, Received, Verifier};
use crate::state;

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

/// A component to pull: its locator (the attestation text empty until its bundle is pulled,
/// for a described one) and the bundle that is to become that text.
struct Piece {
    locator: Locator,
    bundle: Option<Bundle>,
}

/// An attestation bundle a description names, to be fetched by its digest.
struct Bundle {
    sha256: String,
    size: u64,
}

/// The release platform of a host of `os` on `arch`; `None` for a pair no release is built for
/// (a Mac on x86-64).
fn host_platform(os: Platform, arch: Arch) -> Option<ReleasePlatform> {
    ReleasePlatform::from_report(os.name(), &arch.to_string())
}

impl RealFetcher {
    /// What to pull for `rec`: the order's own components as they stand, or, for a
    /// `frontend_update` that carries none, the components the release source describes for
    /// this host. Nothing is made on the host first.
    fn plan(&self, kind: OrderKind, rec: &ReleaseLocator) -> Result<Vec<Piece>, FetchError> {
        if !rec.components.is_empty() {
            return Ok(rec
                .components
                .iter()
                .map(|locator| Piece {
                    locator: locator.clone(),
                    bundle: None,
                })
                .collect());
        }
        // The pipeline has no release to name without a component, and an activation's come
        // with it.
        if kind == OrderKind::Activate {
            return Err(FetchError::Rejected(
                "the order names no component to fetch".to_owned(),
            ));
        }
        let (os, arch) = (self.host.platform(), self.settings.arch);
        let platform = host_platform(os, arch).ok_or_else(|| {
            FetchError::Rejected(format!(
                "platform: no release is built for {} on {arch}",
                os.name()
            ))
        })?;
        let described =
            fetch::describe_release(&self.child, &self.source, self.limits, &rec.commit, platform)
                .map_err(fetch_error)?;
        Ok(described
            .into_iter()
            .map(|d| Piece {
                locator: Locator {
                    name: d.name,
                    sha256: d.sha256,
                    size: d.size,
                    attestation: String::new(),
                },
                bundle: Some(Bundle {
                    sha256: d.bundle_sha256,
                    size: d.bundle_size,
                }),
            })
            .collect())
    }

    fn pull(
        &self,
        kind: OrderKind,
        rec: &ReleaseLocator,
        expected: Option<&Facts>,
    ) -> Result<InstalledRelease, FetchError> {
        let plan = self.plan(kind, rec)?;
        let replaced = replaced_bytes(&self.host);
        let partial = Partial::create(&self.host, &rec.commit, self.owner).map_err(refused)?;
        let mut components = Vec::with_capacity(plan.len());
        let mut files = Vec::with_capacity(plan.len());
        for piece in plan {
            // A failure drops `partial`, which removes the directory.
            let mut locator = piece.locator;
            // The name becomes a file name in root's directory.
            if !is_plain_name(&locator.name) {
                return Err(FetchError::Rejected(format!(
                    "{:?} is not a plain file name",
                    locator.name
                )));
            }
            // The bundle first: it is small, and the tarball is not worth a download without it.
            if let Some(bundle) = &piece.bundle {
                locator.attestation = self.pull_bundle(&partial, &locator.name, bundle, replaced)?;
            }
            files.push(self.pull_one(&partial, &locator, replaced)?);
            components.push(locator);
        }
        let order = Order {
            kind,
            commit: &rec.commit,
            components: &components,
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

    /// One component through the child into `dl/<name>.tar.gz`. `locator.name` is a plain name.
    fn pull_one(
        &self,
        partial: &Partial,
        locator: &Locator,
        replaced: u64,
    ) -> Result<Received, FetchError> {
        let tarball = partial.dl().join(format!("{}.tar.gz", locator.name));
        let got = self.pull_into(
            partial,
            &tarball,
            Component {
                sha256: &locator.sha256,
                size: locator.size,
            },
            replaced,
        )?;
        Ok(Received {
            name: locator.name.clone(),
            tarball,
            bytes: got.count,
            sha256: got.sha256,
        })
    }

    /// The attestation bundle of the described component `name`, through the child into
    /// `dl/<name>.bundle`, as the text the pipeline takes in an offer's place. It is held to
    /// the digest and size the description names, like any file; whether it attests the
    /// tarball is for the pipeline's attestation step.
    fn pull_bundle(
        &self,
        partial: &Partial,
        name: &str,
        bundle: &Bundle,
        replaced: u64,
    ) -> Result<String, FetchError> {
        let file = partial.dl().join(format!("{name}.bundle"));
        self.pull_into(
            partial,
            &file,
            Component {
                sha256: &bundle.sha256,
                size: bundle.size,
            },
            replaced,
        )?;
        // Read back from the file the parent made, in a directory only root enters.
        std::fs::read_to_string(&file).map_err(|e| {
            FetchError::Rejected(format!(
                "attestation: the bundle of {name} cannot be read as text: {e}"
            ))
        })
    }

    /// `component` through the child into the new file `path` in `partial`'s `dl/`.
    fn pull_into(
        &self,
        partial: &Partial,
        path: &Path,
        component: Component<'_>,
        replaced: u64,
    ) -> Result<Pulled, FetchError> {
        // The file is the parent's: made here, new, in a directory only its owner enters.
        let mut out = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .map_err(|e| {
                FetchError::Unavailable(format!("cannot create {}: {e}", path.display()))
            })?;
        fetch::fetch_component(
            &self.child,
            &self.source,
            self.limits,
            &component,
            (&partial.dl(), replaced),
            &mut out,
        )
        .map_err(fetch_error)
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
/// arrived is `Rejected`; a source or host that could not deliver is `Unavailable`. A source
/// that will not describe a release, or whose description cannot be used, is the second: the
/// release could not be had, and its refusal word is `fetch` (`Unavailable` is what the
/// frontend's cycle words `fetch`; a `Rejected` it words `stage`).
fn fetch_error(e: fetch::FetchError) -> FetchError {
    use fetch::FetchError as Step;
    let said = with_word(e.word(), &e);
    match e {
        Step::BadDigest(_)
        | Step::Size { .. }
        | Step::Digest { .. }
        | Step::Pipe(PipeFault::Oversize { .. }) => FetchError::Rejected(said),
        Step::Disk { .. }
        | Step::Source(..)
        | Step::Identity(_)
        | Step::Describe(_)
        | Step::Pipe(_) => FetchError::Unavailable(said),
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
