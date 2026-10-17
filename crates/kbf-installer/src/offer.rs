//! The offer: the one file in an inbox entry that says what the entry holds.
//!
//! `offer.json` is written by the unprivileged `kbf` process from what the frontend sent,
//! so it is untrusted: it names files and the sha256 each must have, and nothing in it is
//! believed until the attestation holds for the files it names.
//!
//! ```json
//! {
//!   "schema": 1,
//!   "commit": "<40 lowercase hex>",
//!   "bundle": { "file": "provenance.jsonl", "sha256": "<64 hex>" },
//!   "components": [
//!     { "name": "kbf", "file": "kbf-<commit>-linux-x86_64.tar.gz", "sha256": "<64 hex>" }
//!   ]
//! }
//! ```
//!
//! `bundle` is the attestation bundle for the release (one file whose attestations cover
//! every tarball); each `components` entry is a tarball holding one binary named like the
//! component (`kbf` or `kbf-daemon`). File names are plain: letters, digits, `.`, `_`,
//! `-`, at most 128 characters, no leading dot, so a name is one path component and
//! cannot be `offer.json`. No field is a path, a command or a ref: the branches a release
//! may come from are the host's setting.

use std::collections::BTreeSet;

use serde::Deserialize;

use crate::CommitId;
use crate::layout::RELEASE_BINARIES;
use crate::refusal::OfferFault;

/// The offer's file name in an inbox entry.
pub const OFFER_FILE: &str = "offer.json";
/// The `schema` this build reads.
pub const SCHEMA: u64 = 1;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOffer {
    schema: u64,
    commit: String,
    bundle: RawFile,
    components: Vec<RawComponent>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawFile {
    file: String,
    sha256: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawComponent {
    name: String,
    file: String,
    sha256: String,
}

/// A file the offer names and the sha256 its copy must have.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OfferedFile {
    pub file: String,
    pub sha256: String,
}

/// A binary the release holds: its name, and the tarball that carries it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Component {
    /// `kbf` or `kbf-daemon`.
    pub name: &'static str,
    pub tarball: OfferedFile,
}

/// A checked offer.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Offer {
    pub commit: CommitId,
    pub bundle: OfferedFile,
    pub components: Vec<Component>,
}

impl Offer {
    pub fn parse(bytes: &[u8]) -> Result<Offer, OfferFault> {
        let raw: RawOffer =
            serde_json::from_slice(bytes).map_err(|e| OfferFault::NotAnOffer(e.to_string()))?;
        if raw.schema != SCHEMA {
            return Err(OfferFault::Schema {
                found: raw.schema,
                want: SCHEMA,
            });
        }
        let commit =
            CommitId::new(&raw.commit).map_err(|_| OfferFault::BadCommit(raw.commit.clone()))?;
        if raw.components.is_empty() {
            return Err(OfferFault::NoComponents);
        }
        let mut files = BTreeSet::from([OFFER_FILE.to_owned()]);
        let bundle = offered(raw.bundle.file, raw.bundle.sha256, &mut files)?;
        let mut components: Vec<Component> = Vec::new();
        for c in raw.components {
            let name = RELEASE_BINARIES
                .into_iter()
                .find(|n| *n == c.name)
                .ok_or_else(|| OfferFault::UnknownComponent(c.name.clone()))?;
            if components.iter().any(|seen| seen.name == name) {
                return Err(OfferFault::RepeatedComponent(c.name));
            }
            components.push(Component {
                name,
                tarball: offered(c.file, c.sha256, &mut files)?,
            });
        }
        Ok(Offer {
            commit,
            bundle,
            components,
        })
    }

    /// Every file the entry may hold: the offer itself, the bundle and each tarball.
    pub fn names(&self) -> BTreeSet<String> {
        let mut names = BTreeSet::from([OFFER_FILE.to_owned(), self.bundle.file.clone()]);
        names.extend(self.components.iter().map(|c| c.tarball.file.clone()));
        names
    }
}

/// A file and its sha256, checked; `files` collects the names so none is named twice.
fn offered(
    file: String,
    sha256: String,
    files: &mut BTreeSet<String>,
) -> Result<OfferedFile, OfferFault> {
    if !plain_name(&file) {
        return Err(OfferFault::BadFileName(file));
    }
    let hex = sha256.len() == 64
        && sha256
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    if !hex {
        return Err(OfferFault::BadSha256(sha256));
    }
    if !files.insert(file.clone()) {
        return Err(OfferFault::RepeatedFile(file));
    }
    Ok(OfferedFile { file, sha256 })
}

/// A name that is one path component of plain characters.
fn plain_name(name: &str) -> bool {
    let plain = |c: char| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-');
    !name.is_empty() && name.len() <= 128 && !name.starts_with('.') && name.chars().all(plain)
}

#[cfg(test)]
#[path = "offer_tests.rs"]
mod tests;
