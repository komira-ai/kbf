//! The release a package carries (its seed), and the rules by which an order may use it.
//!
//! A package of `kbf-installer` can carry one signed `kbf` release, built from the commit the
//! installer is built from. It lies in a directory named for the first 12 hex digits of that
//! commit, beside the trust directory ([`crate::layout::Layout::seed_dir`]):
//!
//! ```text
//! seed/<commit12>/manifest.json
//!                 kbf-<commit>-<platform>.tar.gz
//!                 kbf-<commit>-<platform>.tar.gz.sigstore.json
//! ```
//!
//! The tarball is stored byte for byte, since the attestation binds those bytes. The manifest
//! ([`Manifest`]) is root's, like the trust files, and says what the package builder says the
//! release is: its commit and `seq`, the digests of the tarball and the bundle, what
//! `kbf --version --json` declares, and the digests of the three trust files the package
//! ships beside it. Nothing in it is believed on its own: the release goes through the same
//! verify pipeline as a fetched one ([`crate::stage::Pipeline`]), which holds the tarball to
//! the manifest's digest and size, the attestation to the pinned workflow and the binary to
//! the manifest's facts. The manifest names the release; it does not vouch for it.
//!
//! This module is the data and the two rules an order meets before the pipeline is asked:
//! [`admit`] (the order names the seed's commit, and the seed's `seq` is above the floor) and
//! [`read_floor`]. Reading the files and verifying them is
//! [`SeedFetcher`](crate::helper::fetcher_seed::SeedFetcher)'s.
//!
//! The floor is the root-owned file `seq_floor` beside `state.json`: one decimal number, the
//! highest `seq` that is not allowed: a seed's `seq` must be strictly above it. A host with none
//! refuses every staged order (fail closed). [`crate::seq_floor`] writes it: it starts at the
//! `seq` of the release that runs and rises when a swap is confirmed. `setup` installs the first
//! release before any floor exists, so it asks none of the seed.
//!
//! Between orders the run loop looks at the seed
//! ([`Fetcher::look`](crate::helper::fetcher::Fetcher::look)) and tells the frontend,
//! in `status.json`, the seed it would take an order for ([`Available`]): the package's files
//! whole and attested, and the `seq` above the floor, by the same rule an order meets
//! ([`above_floor`]). It is a report: nothing is installed, staged or moved by it.

use std::io;

use kbf_types::ReleasePlatform;
use kbf_update::Facts;
use serde::{Deserialize, Serialize};

use crate::CommitId;
use crate::helper::fetcher::FetchError;
use crate::layout::Layout;
use crate::record::Sha256Hex;

/// The manifest's file name in a seed directory.
pub const MANIFEST: &str = "manifest.json";

/// The most a manifest may be.
pub const MAX_MANIFEST_BYTES: usize = 16 * 1024;

/// The one component a seed carries: a release of `kbf` alone.
pub const COMPONENT: &str = "kbf";

/// `manifest.json`: a closed set of keys, every one required. Digests are 64 lowercase hex
/// characters, the commit 40, and the platform one a release is built for.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// The commit the release was built from, in full.
    pub commit: CommitId,
    /// The number of commits behind it, which puts two builds in order; the binary declares
    /// the same number.
    pub seq: u64,
    /// `linux-x86_64`, `linux-arm64` or `darwin-arm64`.
    pub platform: String,
    pub tarball_sha256: Sha256Hex,
    pub tarball_size: u64,
    pub bundle_sha256: Sha256Hex,
    /// What `kbf --version --json` declares of the release.
    pub protocol: u32,
    pub min_protocol: u32,
    pub state_schema: u32,
    pub profile: String,
    /// The digests of the trust files the package ships (`gh`, the trusted root and
    /// `installer.conf`), so that a package mixed from two builds can be told.
    pub gh_sha256: Sha256Hex,
    pub trusted_root_sha256: Sha256Hex,
    pub installer_conf_sha256: Sha256Hex,
}

/// What a seed's manifest says the release is: the commit and `seq`, and the facts the
/// release is expected to declare.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeedFacts {
    pub commit: CommitId,
    pub seq: u64,
    pub facts: Facts,
}

/// What `status.json` says of a seed the run loop has found whole and above the floor: the
/// release an operator can order by its commit. Nothing more is told of it (its facts are the
/// installer's to check at the order).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Available {
    pub commit: CommitId,
    pub seq: u64,
}

impl Available {
    /// The seed `seed` as `status.json` tells it.
    pub fn of(seed: &SeedFacts) -> Available {
        Available {
            commit: seed.commit.clone(),
            seq: seed.seq,
        }
    }
}

/// A seed found whole, and the digest of the manifest (sha256 of `manifest.json`, lowercase
/// hex) it was found by: while the manifest is the same one, the package is not checked again.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Looked {
    pub digest: String,
    pub seed: SeedFacts,
}

/// What one look at the host finds, as the run loop reads it.
#[derive(Debug, PartialEq, Eq)]
pub struct Noticed {
    /// The word `release_source` holds in `installer.toml` (`url`, `frontend` or `bundled`),
    /// when the file could be read.
    pub source: Option<&'static str>,
    /// The package's seed found whole; `None` when the package carries no seed at all (the
    /// normal state of a host whose package is without one); an error when it carries one
    /// that is not whole, or the host cannot say.
    pub seed: Result<Option<Looked>, FetchError>,
}

impl Manifest {
    /// The manifest in `bytes`.
    ///
    /// # Errors
    /// Why they are not one, in words for a refusal: too big, not the closed set of keys with
    /// the right types, or a platform no release is built for.
    pub fn parse(bytes: &[u8]) -> Result<Manifest, String> {
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(format!(
                "{MANIFEST} is {} bytes; at most {MAX_MANIFEST_BYTES}",
                bytes.len()
            ));
        }
        let manifest: Manifest =
            serde_json::from_slice(bytes).map_err(|e| format!("{MANIFEST}: {e}"))?;
        if ReleasePlatform::parse(&manifest.platform).is_none() {
            return Err(format!(
                "{MANIFEST}: platform {:?} is not one a release is built for",
                manifest.platform
            ));
        }
        Ok(manifest)
    }

    /// The facts the release is expected to declare.
    pub fn facts(&self) -> Facts {
        Facts {
            platform: self.platform.clone(),
            protocol: self.protocol,
            min_protocol: self.min_protocol,
            state_schema: self.state_schema,
            profile: self.profile.clone(),
        }
    }

    /// The commit, `seq` and facts, as the fetcher reports them.
    pub fn seed_facts(&self) -> SeedFacts {
        SeedFacts {
            commit: self.commit.clone(),
            seq: self.seq,
            facts: self.facts(),
        }
    }

    /// The tarball's file name: `kbf-<commit>-<platform>.tar.gz`.
    pub fn tarball_name(&self) -> String {
        format!("{COMPONENT}-{}-{}.tar.gz", self.commit, self.platform)
    }

    /// The attestation bundle's file name: the tarball's and `.sigstore.json`.
    pub fn bundle_name(&self) -> String {
        format!("{}.sigstore.json", self.tarball_name())
    }
}

/// The floor in `seq_floor`: `None` when the file is not there.
///
/// # Errors
/// The file cannot be read or does not hold one decimal number: the answer is not "no floor",
/// which would admit a seed, but a refusal.
pub fn read_floor(host: &Layout) -> Result<Option<u64>, FetchError> {
    let path = host.join(&host.seq_floor());
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            let why = format!("seq_floor: {} cannot be read: {e}", path.display());
            return Err(FetchError::Unavailable(why));
        }
    };
    match text.trim().parse::<u64>() {
        Ok(floor) => Ok(Some(floor)),
        Err(_) => {
            let why = format!("seq_floor: {} is not one number", path.display());
            Err(FetchError::Unavailable(why))
        }
    }
}

/// Whether an order for `commit` may use the seed `seed` says it carries, against the floor:
/// the order names the seed's commit (all 40 digits), and the seed's `seq` is strictly above
/// the floor. A host with no floor admits nothing.
///
/// # Errors
/// [`FetchError::Order`] under the word `commit_or_ref`, saying which rule.
pub fn admit(seed: &SeedFacts, commit: &CommitId, floor: Option<u64>) -> Result<(), FetchError> {
    let refuse = |why: String| FetchError::Order {
        word: "commit_or_ref",
        why,
    };
    if seed.commit != *commit {
        return Err(refuse(format!(
            "seed_mismatch: this host carries {}, not {commit}",
            seed.commit
        )));
    }
    above_floor(seed.seq, floor).map_err(refuse)
}

/// Why a host with no floor admits no seed.
const NO_FLOOR: &str = "seq_floor: no floor is recorded yet, so no seed is above it";

/// Whether a seed of `seq` is above `floor`: the rule [`admit`] applies at an order and the
/// run loop applies before it tells a seed is available, so that what is announced is what an
/// order would be taken on. A host with no floor has none to be above.
///
/// # Errors
/// The rule, in the words an operator reads.
pub fn above_floor(seq: u64, floor: Option<u64>) -> Result<(), String> {
    match floor {
        Some(floor) if seq > floor => Ok(()),
        Some(floor) => Err(format!("seq_floor: seed seq {seq} is not above {floor}")),
        None => Err(NO_FLOOR.to_owned()),
    }
}

#[cfg(test)]
#[path = "seed_tests.rs"]
mod tests;
