//! The attestation checks of a release, behind a seam so tests use recorded answers, and
//! the pinned `gh` that makes them (used by `release fetch-previous`).

use std::path::Path;

use super::{ARTIFACTS_WORKFLOW, MAIN};
use crate::Error;
use crate::sums;

/// The attestation checks, behind a seam so tests use recorded answers.
pub trait Attester {
    /// The artifacts of `commit` in `dir` carry the attestations of the artifacts workflow.
    fn artifacts(&self, dir: &Path, commit: &str) -> Result<(), Error>;
}

/// The attestations as the pinned `gh` verifies them.
pub struct Gh<'a> {
    /// The repository, `<owner>/<repo>`.
    pub repo: &'a str,
    /// A `trusted_root.jsonl`, as `verify-assets attestations` takes it.
    pub trusted_root: Option<&'a Path>,
    /// The `gh` program.
    pub gh: &'a Path,
    /// An empty directory for the verification's flipped copy.
    pub scratch: &'a Path,
}

impl Attester for Gh<'_> {
    fn artifacts(&self, dir: &Path, commit: &str) -> Result<(), Error> {
        let workflow_ref = format!("{}/.github/workflows/{ARTIFACTS_WORKFLOW}", self.repo);
        let source_ref = format!("refs/heads/{MAIN}");
        let a = sums::Attestations {
            repo: self.repo,
            workflow_ref: &workflow_ref,
            source_ref: &source_ref,
            commit,
            trusted_root: self.trusted_root,
            gh: self.gh,
            scratch: self.scratch,
        };
        sums::verify_attestations(dir, &a).map(drop)
    }
}
