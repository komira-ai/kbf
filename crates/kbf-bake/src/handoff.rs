//! The hand-off from the deploy to the evaluator: what the evaluator needs to know about the
//! deploy it is to bake, in one file the deploy writes (`kbf-bake run --handoff FILE`).
//!
//! The deploy and the evaluator are separate processes on separate schedules, so nothing but
//! this file passes between them. It carries the commit, the set hash, the generation the
//! deploy set, the deploy's start (and end, once it has one), whether the release can be
//! walked back (section 8.3), and the commit a rollback would go to. `reversible` has no
//! default: a file that does not say is refused, because "unknown" must not read as "yes".

use std::path::Path;

use kbf_gate::{CommitId, SetHash};
use serde::{Deserialize, Serialize};

/// The hand-off's format number.
pub const HANDOFF_SCHEMA: u32 = 1;

/// Why a hand-off file was refused.
#[derive(Debug, thiserror::Error)]
pub enum HandoffError {
    /// The file could not be read.
    #[error("read {path}: {source}")]
    Read {
        /// The path.
        path: String,
        /// The error.
        source: std::io::Error,
    },
    /// The bytes are not a hand-off (bad JSON, an unknown or missing field, a bad commit or
    /// set hash).
    #[error("hand-off: {0}")]
    Json(#[from] serde_json::Error),
    /// A value is wrong.
    #[error("hand-off: {0}")]
    Invalid(String),
}

/// What the deploy tells the evaluator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Handoff {
    /// [`HANDOFF_SCHEMA`].
    pub schema: u32,
    /// The commit that was deployed.
    pub commit: CommitId,
    /// The hash of the release set that was deployed.
    pub set_hash: SetHash,
    /// The evaluator's id for this bake.
    pub run_id: String,
    /// When the deploy began (unix seconds): the moment the baseline is taken for.
    pub deploy_start: u64,
    /// When it ended; absent while it runs.
    pub deploy_end: Option<u64>,
    /// The generation the deploy set as desired; absent until it has.
    pub desired_generation: Option<u64>,
    /// Whether the release can be walked back (section 8.3: no contract step, no raised log
    /// version). The offer knows; the evaluator only reads it.
    pub reversible: bool,
    /// The commit that was desired before the deploy: where a rollback goes.
    pub previous_commit: Option<CommitId>,
}

impl Handoff {
    /// Read a hand-off from bytes.
    pub fn parse(bytes: &[u8]) -> Result<Self, HandoffError> {
        let h: Self = serde_json::from_slice(bytes)?;
        if h.schema != HANDOFF_SCHEMA {
            return Err(HandoffError::Invalid(format!(
                "schema {}, this reader knows {HANDOFF_SCHEMA}",
                h.schema
            )));
        }
        if h.run_id.trim().is_empty() {
            return Err(HandoffError::Invalid("run_id is empty".to_owned()));
        }
        if h.deploy_end.is_some_and(|end| end < h.deploy_start) {
            return Err(HandoffError::Invalid(
                "deploy_end is before deploy_start".to_owned(),
            ));
        }
        Ok(h)
    }

    /// Read the hand-off at `path`.
    pub fn load(path: &Path) -> Result<Self, HandoffError> {
        let bytes = std::fs::read(path).map_err(|source| HandoffError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::parse(&bytes)
    }

    /// The file as the deploy writes it: pretty JSON and a newline.
    pub fn to_bytes(&self) -> Result<Vec<u8>, HandoffError> {
        let mut bytes = serde_json::to_vec_pretty(self)?;
        bytes.push(b'\n');
        Ok(bytes)
    }
}
