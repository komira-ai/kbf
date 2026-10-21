//! `release verify`: the check that a `v*` tag names exactly the commit being released, that
//! the commit is on main, and which set of files is being released (docs/design/kbf-cicd.md,
//! section 8.1). Who may push a tag is the tag ruleset's business, not this check's.
//!
//! Nothing here judges a verdict. The annotated tag's message is the Beta record `release
//! verdict-write` wrote; it is read for the notes ([`Verified::record`]) and is an audit
//! record that nothing gates on: a tag without one, or with one that says it failed, verifies
//! the same. What gates a release is `release gates` (the runs) and the checks of the
//! files themselves (`release check`, the attestations, the set hash).

use std::path::Path;

use kbf_gate::{CommitId, ResultDoc, SetHash};

use super::git::{self, Tag};
use crate::Error;
use crate::package::read;
use crate::sums::SUMS;

/// What a release is checked against.
pub struct Subject<'a> {
    /// A checkout holding the tag object (not only the commit it names).
    pub repo_dir: &'a Path,
    /// The release tag.
    pub tag: &'a str,
    /// The commit being released, a full SHA.
    pub commit: &'a str,
    /// The verified release files, whose `SHA256SUMS` is the set.
    pub dir: &'a Path,
    /// The ref of main in `repo_dir`.
    pub main_ref: &'a str,
}

/// A release that passed every check.
pub struct Verified {
    /// The hash of the set in `dir`: what `publish --set-hash` must be given.
    pub set_hash: SetHash,
    /// The Beta record the tag's message carries, when it carries one that parses. Audit
    /// only: it was not consulted for the checks above.
    pub record: Option<ResultDoc>,
}

/// The set hash of the release files in `dir`: [`SetHash::of_sums`] of its `SHA256SUMS`,
/// the one definition the bake evaluator uses too.
pub fn set_hash_of(dir: &Path) -> Result<SetHash, Error> {
    Ok(SetHash::of_sums(&read(&dir.join(SUMS))?))
}

/// Checks the tag and the commit of the release: every problem found, or what was verified.
pub fn verify(s: &Subject) -> Result<Verified, Error> {
    CommitId::parse(s.commit).map_err(|e| Error::Usage(format!("--commit: {e}")))?;
    let set_hash = set_hash_of(s.dir)?;
    let mut problems = Vec::new();
    let record = match git::read_tag(s.repo_dir, s.tag)? {
        // A bare ref to a commit: it has no message and no object of its own. `commit_of`
        // peels it, and refuses a ref to a tree or a blob.
        Tag::Lightweight => {
            let object = git::commit_of(s.repo_dir, s.tag)?;
            if object != s.commit {
                problems.push(format!(
                    "tag {} names commit {object}, not commit {}",
                    s.tag, s.commit
                ));
            }
            None
        }
        Tag::Annotated {
            kind,
            object,
            name,
            message,
        } => {
            // An object of another kind has another id, so the id alone refuses a tag of a tree.
            if object != s.commit {
                problems.push(format!(
                    "tag {} names {kind} {object}, not commit {}",
                    s.tag, s.commit
                ));
            }
            if name != s.tag {
                problems.push(format!("tag object is named {name}, not {}", s.tag));
            }
            ResultDoc::from_bytes(message.as_bytes()).ok()
        }
    };
    if !git::on_main(s.repo_dir, s.commit, s.main_ref)? {
        problems.push(format!("{} is not on {}", s.commit, s.main_ref));
    }
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    Ok(Verified { set_hash, record })
}
