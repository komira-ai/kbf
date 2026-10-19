//! `release verdict`: the check that a `v*` tag carries a gamma verdict for exactly the
//! files being released (docs/design/kbf-cicd.md, section 8.1). The verdict is the tag
//! message, unsigned: who may push a tag is the tag ruleset's business, not this check's.

use std::path::Path;

use kbf_gate::{CommitId, ResultDoc, SetHash, Stage};

use super::git::{self, Tag};
use crate::Error;
use crate::package::read;
use crate::sums::SUMS;

/// The shortest bake a release accepts: 24 hours.
pub const MIN_BAKE_SECS: u64 = 24 * 60 * 60;

/// The waiver that excuses a bake that did not meet its minimum load.
pub const LOAD_WAIVER: &str = "min-load";

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

/// A verdict that passed every check.
pub struct Verified {
    /// The verdict, as the tag carries it.
    pub doc: ResultDoc,
    /// The hash of the set in `dir`, which the verdict names.
    pub set_hash: SetHash,
}

/// The set hash of the release files in `dir`: [`SetHash::of_sums`] of its `SHA256SUMS`,
/// the one definition the bake evaluator uses too.
pub fn set_hash_of(dir: &Path) -> Result<SetHash, Error> {
    Ok(SetHash::of_sums(&read(&dir.join(SUMS))?))
}

/// Checks the tag's verdict against the release: every problem found, or the verdict.
pub fn verify(s: &Subject) -> Result<Verified, Error> {
    CommitId::parse(s.commit).map_err(|e| Error::Usage(format!("--commit: {e}")))?;
    let set_hash = set_hash_of(s.dir)?;
    let tag = git::read_tag(s.repo_dir, s.tag)?;
    let Tag::Annotated {
        kind,
        object,
        name,
        message,
    } = tag
    else {
        return Err(Error::Refused(vec![format!(
            "tag {} is lightweight: it has no message to carry a verdict; make it with `git tag -a`",
            s.tag
        )]));
    };
    let mut problems = Vec::new();
    if kind != "commit" || object != s.commit {
        problems.push(format!(
            "tag {} names {kind} {object}, not commit {}",
            s.tag, s.commit
        ));
    }
    if name != s.tag {
        problems.push(format!("tag object is named {name}, not {}", s.tag));
    }
    if !git::on_main(s.repo_dir, s.commit, s.main_ref)? {
        problems.push(format!("{} is not on {}", s.commit, s.main_ref));
    }
    let doc = match ResultDoc::from_bytes(message.as_bytes()) {
        Ok(doc) => doc,
        Err(e) => {
            problems.push(format!("tag {} carries no usable verdict: {e}", s.tag));
            return Err(Error::Refused(problems));
        }
    };
    problems.extend(judge(&doc, s, &set_hash));
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    Ok(Verified { doc, set_hash })
}

/// What is wrong with a parsed verdict for this release. The parser has already refused a
/// green status over a red or missing gate, so green status is the whole gate check here.
fn judge(doc: &ResultDoc, s: &Subject, set_hash: &SetHash) -> Vec<String> {
    let mut problems = Vec::new();
    if doc.stage != Stage::Gamma {
        problems.push(format!(
            "the verdict is for stage {:?}, not Gamma",
            doc.stage
        ));
    }
    if !doc.passed() {
        problems.push(format!("the verdict is {:?}, not green", doc.status));
    }
    if doc.commit.as_str() != s.commit {
        problems.push(format!(
            "the verdict names commit {}, not {}",
            doc.commit.as_str(),
            s.commit
        ));
    }
    if &doc.set_hash != set_hash {
        problems.push(format!(
            "the verdict names set {}, the files in {} are set {}",
            doc.set_hash.as_str(),
            s.dir.display(),
            set_hash.as_str()
        ));
    }
    if doc.tag.as_deref() != Some(s.tag) {
        problems.push(format!(
            "the verdict names tag {:?}, not {}",
            doc.tag.as_deref().unwrap_or(""),
            s.tag
        ));
    }
    match doc.bake.and_then(|b| Some(b.end?.saturating_sub(b.start))) {
        Some(secs) if secs >= MIN_BAKE_SECS => {}
        Some(secs) => problems.push(format!(
            "the bake lasted {secs} s, less than {MIN_BAKE_SECS} s (24 h)"
        )),
        None => problems.push("the verdict records no finished bake".to_owned()),
    }
    let load_met = doc.load.is_some_and(|l| l.met());
    if !load_met && !doc.waivers.iter().any(|w| w.what == LOAD_WAIVER) {
        problems.push(format!(
            "the bake's minimum load was not met and no `{LOAD_WAIVER}` waiver is recorded"
        ));
    }
    problems
}
