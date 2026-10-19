//! What the store holds of one release, as a caller that was given no offer asks for it:
//! `ReleaseService.Describe` ([`super::grpc`]) and `GET /v1/releases/{commit}`
//! ([`super::http`]), which answer from [`describe`] and differ only in how they say it.
//!
//! - **The same rule as an offer.** A platform is described only when it is complete: every
//!   tarball it needs ([`ReleasePlatform::components`]) stored with its attestation, exactly
//!   what [`super::offers`] offers. A description carries those tarballs and no others.
//! - **Every file named can be fetched.** A component carries the digest and size of its
//!   tarball and of its attestation bundle; both are served under their digests
//!   ([`ReleaseStore::open_tarball`]), so a bundle stored before the store linked bundles
//!   into `files/` is reported as lacking until it is put again.
//! - **One platform, or all that are complete.** For a platform asked, anything it lacks
//!   fails the call and is named; for none asked, a platform that is not complete is left
//!   out, and the call fails only when no platform is complete.
//! - **A static mirror's file.** [`view`] is the JSON of a description, the body of
//!   `releases/<commit>.json` in a `url` source (`docs/api.md`).

use std::fmt;
use std::io;

use kbf_proto::update::{ReleaseComponent, ReleaseDescription};
use kbf_types::ReleasePlatform;
use serde_json::json;
use sha2::{Digest, Sha256};

use super::key::{Commit, Key, Sha256Hex};
use super::store::{PlatformListing, ReleaseStore};

/// The `schema` of [`view`]'s JSON: raised only by a change a reader of the old one would
/// misread (a field added is not one).
pub const SCHEMA: u32 = 1;

/// What a platform lacks of a tarball it needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Missing {
    /// The tarball is not stored.
    Tarball,
    /// The tarball is stored and its attestation bundle is not.
    Attestation,
    /// The bundle is stored but not under its digest in `files/`, so it cannot be fetched:
    /// it was stored before the store linked bundles there; putting it again does.
    BundleFile,
}

/// A tarball (or its bundle) a platform needs and the store does not serve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Lack {
    pub platform: ReleasePlatform,
    pub component: &'static str,
    pub missing: Missing,
}

impl fmt::Display for Lack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (platform, name) = (self.platform, self.component);
        match self.missing {
            Missing::Tarball => write!(f, "{platform} has no tarball of {name}"),
            Missing::Attestation => write!(f, "{platform} has no attestation of {name}"),
            Missing::BundleFile => write!(
                f,
                "{platform}: the attestation of {name} is not served by its digest; put it again"
            ),
        }
    }
}

fn joined(lacks: &[Lack]) -> String {
    let texts: Vec<_> = lacks.iter().map(Lack::to_string).collect();
    texts.join("; ")
}

/// Why a release is not described.
#[derive(Debug, thiserror::Error)]
pub enum DescribeError {
    /// No tarball of the commit is stored, for any platform.
    #[error("no tarball of commit {0} is stored")]
    Unknown(Commit),
    /// The platform asked is not complete, or none is: `lacks` says what each one is missing.
    #[error("release {commit} is incomplete: {}", joined(.lacks))]
    Incomplete { commit: Commit, lacks: Vec<Lack> },
    /// The store's directory failed.
    #[error("the release store: {0}")]
    Store(#[from] io::Error),
}

/// What one platform came to.
enum Found {
    Complete(Vec<ReleaseComponent>),
    Lacking(Vec<Lack>),
}

/// The tarballs `platform` needs, from what the store holds of it (`held`, `None` when it
/// holds no tarball for it), or what each lacks.
async fn platform_of(
    store: &ReleaseStore,
    commit: &Commit,
    platform: ReleasePlatform,
    held: Option<&PlatformListing>,
) -> Result<Found, io::Error> {
    let (mut components, mut lacks) = (Vec::new(), Vec::new());
    for &name in platform.components() {
        let lack = |missing| Lack {
            platform,
            component: name,
            missing,
        };
        let stored = held.and_then(|h| h.components.iter().find(|c| c.component.as_str() == name));
        let Some(info) = stored else {
            lacks.push(lack(Missing::Tarball));
            continue;
        };
        if !info.attested {
            lacks.push(lack(Missing::Attestation));
            continue;
        }
        let key = Key {
            commit: commit.clone(),
            platform,
            component: info.component,
        };
        // Attested, and a key never changes: a bundle missing now is a store that fails (a
        // file taken out of the directory between the listing and this read, which no input
        // can bring about), so the error is built where it is used, not in a closure no
        // run enters.
        let bundle = store
            .attestation(&key)
            .await?
            .ok_or(io::Error::from(io::ErrorKind::NotFound))?;
        let bundle_sha256 = Sha256Hex::of_digest(&Sha256::digest(&bundle));
        if store.open_tarball(&bundle_sha256).await?.is_none() {
            lacks.push(lack(Missing::BundleFile));
            continue;
        }
        components.push(ReleaseComponent {
            name: name.to_owned(),
            platform: platform.as_str().to_owned(),
            tarball: format!("{name}-{commit}-{platform}.tar.gz"),
            sha256: info.sha256.to_string(),
            size: info.size,
            bundle_sha256: bundle_sha256.to_string(),
            bundle_size: bundle.len() as u64,
        });
    }
    Ok(if lacks.is_empty() {
        Found::Complete(components)
    } else {
        Found::Lacking(lacks)
    })
}

/// What the store holds of `commit` for `platform`, or for every platform it is complete
/// for, in [`ReleasePlatform::ALL`] order and then the platform's own component order.
///
/// # Errors
/// [`DescribeError`]: no tarball of the commit; the platform asked incomplete, or (none
/// asked) no platform complete; or the directory failing.
pub async fn describe(
    store: &ReleaseStore,
    commit: &Commit,
    platform: Option<ReleasePlatform>,
) -> Result<ReleaseDescription, DescribeError> {
    let listing = store
        .commit(commit.clone())
        .await?
        .ok_or_else(|| DescribeError::Unknown(commit.clone()))?;
    let wanted: Vec<_> = match platform {
        Some(asked) => vec![asked],
        None => listing.platforms.iter().map(|p| p.platform).collect(),
    };
    let (mut components, mut lacks) = (Vec::new(), Vec::new());
    for one in wanted {
        let held = listing.platforms.iter().find(|p| p.platform == one);
        match platform_of(store, commit, one, held).await? {
            Found::Complete(found) => components.extend(found),
            Found::Lacking(missing) => lacks.extend(missing),
        }
    }
    // An incomplete platform fails a call that asked for it; one among others is left out.
    if !lacks.is_empty() && (platform.is_some() || components.is_empty()) {
        return Err(DescribeError::Incomplete {
            commit: commit.clone(),
            lacks,
        });
    }
    Ok(ReleaseDescription {
        commit: commit.to_string(),
        components,
    })
}

/// The JSON of a description: `{"schema", "commit", "components": [{"name", "platform",
/// "tarball", "sha256", "size", "bundle_sha256", "bundle_size"}]}`, the fields of
/// [`ReleaseDescription`] by their proto names.
#[must_use]
pub fn view(description: &ReleaseDescription) -> serde_json::Value {
    let components: Vec<_> = description
        .components
        .iter()
        .map(|c| {
            json!({
                "name": c.name,
                "platform": c.platform,
                "tarball": c.tarball,
                "sha256": c.sha256,
                "size": c.size,
                "bundle_sha256": c.bundle_sha256,
                "bundle_size": c.bundle_size,
            })
        })
        .collect();
    json!({
        "schema": SCHEMA,
        "commit": description.commit,
        "components": components,
    })
}
