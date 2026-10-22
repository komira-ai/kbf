//! The attestation checks of a release, behind a seam so tests use recorded answers, and
//! the pinned `gh` that makes them (shared by `release offer`, `release stage-offer` and
//! `release fetch-previous`).

use std::path::Path;

use super::{ARTIFACTS_WORKFLOW, MAIN};
use crate::Error;
use crate::sums;

/// The workflow that is to attest the beta result (planned: the beta workflow signs
/// nothing today, so [`Gh::beta`] refuses every result until it does).
pub const BETA_WORKFLOW: &str = "beta.yml";

/// The attestation checks, behind a seam so tests use recorded answers.
pub trait Attester {
    /// The artifacts of `commit` in `dir` carry the attestations of the artifacts workflow.
    fn artifacts(&self, dir: &Path, commit: &str) -> Result<(), Error>;
    /// The beta result file `result` is attested by the beta workflow for `commit`.
    fn beta(&self, result: &Path, commit: &str) -> Result<(), Error>;
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

    fn beta(&self, result: &Path, commit: &str) -> Result<(), Error> {
        // As strict as a host's check of a tarball: the result's own bundle beside it,
        // offline with a trusted root, the issuer, the predicate type, the commit.
        let signer = format!("{}/.github/workflows/{BETA_WORKFLOW}", self.repo);
        let source_ref = format!("refs/heads/{MAIN}");
        let a = sums::Attestations {
            repo: self.repo,
            workflow_ref: &signer,
            source_ref: &source_ref,
            commit,
            trusted_root: self.trusted_root,
            gh: self.gh,
            scratch: self.scratch,
        };
        let name = result
            .file_name()
            .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
        let bundle = result.with_file_name(crate::bundles::name_of(&name));
        let (verified, said) = a.bundle_run(result, &bundle, &signer, commit)?;
        if !verified {
            return Err(Error::Failed(format!(
                "{} is not attested by {BETA_WORKFLOW} for {commit} (bundle {}): {said}",
                result.display(),
                bundle.display()
            )));
        }
        // The control: the same result and bundle must not verify for another commit.
        let other = crate::bundles::other_commit(commit);
        if a.bundle_run(result, &bundle, &signer, &other)?.0 {
            return Err(Error::Failed(format!(
                "control failed: the beta bundle of {} verified for commit {other}, not the \
                 commit it attests",
                result.display()
            )));
        }
        Ok(())
    }
}
