//! The offer document: `offer.json`, what `cargo xtask release offer` writes once every
//! check of an offer has passed (docs/design/kbf-cicd.md, section 5.1), and what a deploy
//! reads (`kbf-installer deploy --offer offer.json`) before it moves a farm.
//!
//! The document is a record, not a command: it names the commit and set it is for, the
//! beta evidence (an attested result, or a waiver written down), whether going back to the
//! previous commit is reversible, and the platforms found complete. A reader refuses an
//! unknown field, another [`OFFER_VERSION`], a prod offer that waived beta, and a
//! `reversible` the two `meta_log` numbers do not support (so a hand edit cannot turn an
//! irreversible release reversible).

use serde::{Deserialize, Serialize};

use crate::doc::{CommitId, DocError, SetHash, Waiver};

/// The document's format number. A reader refuses any other.
pub const OFFER_VERSION: u32 = 1;

/// The stage an offer is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Target {
    /// The operator's own farm.
    Gamma,
    /// The publish.
    Prod,
}

/// The beta evidence an offer rests on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Beta {
    /// A beta result attested for this commit and set, every gate green.
    Attested {
        /// The run id the result document names.
        run_id: String,
    },
    /// No beta result: an operator waived it, with a reason. Never for [`Target::Prod`].
    Waived {
        /// The waiver (`what` is `beta`).
        waiver: Waiver,
    },
}

/// Whether going from release N back to N-1 is reversible: both `meta_log` numbers are
/// known and `meta_log(N) <= meta_log(N-1)`. An unknown number on either side is not
/// reversible (fail closed), and so is a first release with nothing to go back to.
pub fn reversible(meta_log: Option<u64>, previous_meta_log: Option<u64>) -> bool {
    matches!((meta_log, previous_meta_log), (Some(n), Some(p)) if n <= p)
}

/// `offer.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OfferDoc {
    /// [`OFFER_VERSION`].
    pub version: u32,
    /// When the offer was made (unix seconds).
    pub created: u64,
    /// The stage offered to.
    pub target: Target,
    /// The commit offered.
    pub commit: CommitId,
    /// The release set offered: the hash of its `SHA256SUMS`.
    pub set_hash: SetHash,
    /// The platforms found complete in the release (tarball, SBOM and bundle), by the
    /// names assets use (`linux-x86_64`, ...), sorted.
    pub platforms: Vec<String>,
    /// The beta evidence.
    pub beta: Beta,
    /// The highest metadata-log machine version the offered build runs; absent when its
    /// `kbf --version --json` was not given or lacks it.
    pub meta_log: Option<u64>,
    /// The commit the farm runs now, which a walk-back returns to; absent when none was given.
    pub previous: Option<CommitId>,
    /// `meta_log` of the previous build; absent when unknown.
    pub previous_meta_log: Option<u64>,
    /// [`reversible`] of the two numbers above.
    pub reversible: bool,
}

fn bad(why: &str) -> DocError {
    DocError::Offer(why.to_owned())
}

impl OfferDoc {
    /// Why this document is not one a reader would accept, if it is not.
    pub fn validate(&self) -> Result<(), DocError> {
        if self.version != OFFER_VERSION {
            return Err(DocError::Offer(format!(
                "version {}, this reader knows {OFFER_VERSION}",
                self.version
            )));
        }
        if self.platforms.is_empty() {
            return Err(bad("it names no platform"));
        }
        match &self.beta {
            Beta::Waived { waiver } => {
                Waiver::new(&waiver.what, &waiver.by, &waiver.reason)?;
                if self.target == Target::Prod {
                    return Err(bad("a prod offer cannot waive beta"));
                }
            }
            Beta::Attested { run_id } if run_id.trim().is_empty() => {
                return Err(bad("the attested beta names no run"));
            }
            Beta::Attested { .. } => {}
        }
        if self.previous.is_none() && self.previous_meta_log.is_some() {
            return Err(bad("a previous meta_log without a previous commit"));
        }
        if self.reversible != reversible(self.meta_log, self.previous_meta_log) {
            return Err(bad("`reversible` is not what the meta_log numbers give"));
        }
        Ok(())
    }

    /// The document as it is written: pretty JSON and a newline; a document a reader
    /// would refuse is not written.
    pub fn to_bytes(&self) -> Result<Vec<u8>, DocError> {
        self.validate()?;
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        Ok(bytes)
    }

    /// Reads a document, refusing everything [`OfferDoc::validate`] does and an unknown field.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, DocError> {
        let doc: Self = serde_json::from_slice(bytes)?;
        doc.validate()?;
        Ok(doc)
    }
}
