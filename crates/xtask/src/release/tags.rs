//! The release tags, `v<MAJOR>.<MINOR>.<PATCH>`, and Rule S: a re-run of an old run must
//! not tag, publish or deploy over a newer release.
//!
//! A release tag is `v` and three plain decimal numbers. Anything else (`v0.1.2-rc1`,
//! `v0.1.2-probe`, `v01.2.3`, `latest`) is no release tag: it is never counted and never
//! the highest. Versions compare number by number, so 10 is above 9.

use std::path::Path;

use serde_json::Value;

use super::api;
use crate::Error;
use crate::pages::{PER_PAGE, Pages};

/// The `MAJOR.MINOR` a `VERSION` file holds: a release's version without its patch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Base {
    pub major: u64,
    pub minor: u64,
}

/// A release version: the base and the patch number, in that order of precedence.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct Version {
    pub base: Base,
    pub patch: u64,
}

/// `text` as a number in the one spelling it is written with: digits, no sign, no
/// leading zero (`0` is the only number that starts with one), and it fits.
fn number(text: &str) -> Option<u64> {
    text.parse().ok().filter(|n: &u64| n.to_string() == text)
}

impl Base {
    /// The contents of a `VERSION` file: `MAJOR.MINOR` and one line end, nothing else
    /// (no `v`, no patch, no space, no second line).
    pub fn parse_file(text: &str) -> Option<Base> {
        let (major, minor) = text.strip_suffix('\n')?.split_once('.')?;
        Some(Base {
            major: number(major)?,
            minor: number(minor)?,
        })
    }
}

impl Version {
    /// The version a release tag names, or none for any other tag.
    pub fn parse_tag(tag: &str) -> Option<Version> {
        let mut parts = tag.strip_prefix('v')?.split('.');
        let version = Version {
            base: Base {
                major: number(parts.next()?)?,
                minor: number(parts.next()?)?,
            },
            patch: number(parts.next()?)?,
        };
        parts.next().is_none().then_some(version)
    }

    /// The tag of this version.
    pub fn tag(&self) -> String {
        format!("v{self}")
    }
}

impl std::fmt::Display for Version {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}.{}", self.base.major, self.base.minor, self.patch)
    }
}

/// The entries of the GitHub list `got` from `gh api <path>`. A list that is not there is
/// an error, not an empty list: a guard that reads "no tags" from a failed call would let
/// anything through.
fn entries(got: Option<Value>, path: &str) -> Result<Vec<Value>, Error> {
    match got {
        Some(Value::Array(got)) => Ok(got),
        _ => Err(Error::Failed(format!("gh api {path}: no list of entries"))),
    }
}

/// Every entry of a GitHub list at `path`, page after page (100 a page) until a page is
/// short; [`Pages`] ends it with an error on a repeated page or at its cap.
fn listed(gh: &Path, path: &str) -> Result<Vec<Value>, Error> {
    let mut all = Vec::new();
    let mut pages = Pages::new(path);
    loop {
        let got = api(
            gh,
            &format!("{path}?per_page={PER_PAGE}&page={}", pages.number()),
        )?;
        let got = entries(got, path)?;
        let more = pages.more(&got)?;
        all.extend(got);
        if !more {
            return Ok(all);
        }
    }
}

/// The versions of the release tags of `repo` (its `v*` refs). GitHub's
/// `git/matching-refs` does not page: it ignores `per_page` and `page` and answers every
/// matching ref, with no `Link` header, so it is read with one call.
pub fn tag_versions(gh: &Path, repo: &str) -> Result<Vec<Version>, Error> {
    let path = format!("repos/{repo}/git/matching-refs/tags/v");
    let refs = entries(api(gh, &path)?, &path)?;
    Ok(refs
        .iter()
        .filter_map(|r| Version::parse_tag(r["ref"].as_str()?.strip_prefix("refs/tags/")?))
        .collect())
}

/// The versions of the releases of `repo`, drafts among them (a draft names a tag it
/// may not have yet, and its number is taken all the same).
pub fn release_versions(gh: &Path, repo: &str) -> Result<Vec<Version>, Error> {
    let releases = listed(gh, &format!("repos/{repo}/releases"))?;
    Ok(releases
        .iter()
        .filter_map(|r| Version::parse_tag(r["tag_name"].as_str()?))
        .collect())
}

/// How a commit stands to the highest release tag, when it does not contradict it.
#[derive(Debug, PartialEq, Eq)]
pub enum Standing {
    /// No release tag exists.
    Untagged,
    /// The highest tag names this very commit.
    Is(Version),
    /// The highest tag names an ancestor of this commit.
    Ahead(Version),
}

/// The refusal of a commit that is not at or after the highest release tag `tag`.
fn not_after(tag: &str, sha: &str, status: &str) -> Error {
    Error::Refused(vec![format!(
        "superseded: the highest release tag is {tag} and {sha} is not at or after it \
         (compare says {status}): a re-run of an older run must not tag, publish or deploy"
    )])
}

/// A commit's place against the highest release tag, as `release plan` reads it.
#[derive(Debug, PartialEq, Eq)]
pub enum Placed {
    /// At or after the highest tag, or no tag exists.
    Stands(Standing),
    /// An ancestor of the highest tag (GitHub's compare says `behind`): a newer commit was
    /// released already.
    Behind(Version),
}

/// Rule S as `release plan` applies it: [`Placed::Behind`] is an answer, not a refusal
/// (the run is an old one that finished late). A tag that diverged from `sha`, an
/// unknown commit or a compare that fails still refuses.
pub fn placed(gh: &Path, repo: &str, sha: &str) -> Result<Placed, Error> {
    let Some(top) = tag_versions(gh, repo)?.into_iter().max() else {
        return Ok(Placed::Stands(Standing::Untagged));
    };
    let tag = top.tag();
    let compared = api(gh, &format!("repos/{repo}/compare/{tag}...{sha}"))?;
    match compared.as_ref().and_then(|v| v["status"].as_str()) {
        Some("identical") => Ok(Placed::Stands(Standing::Is(top))),
        Some("ahead") => Ok(Placed::Stands(Standing::Ahead(top))),
        Some("behind") => Ok(Placed::Behind(top)),
        status => Err(not_after(&tag, sha, status.unwrap_or("nothing"))),
    }
}

/// Rule S: the highest release tag of `repo` must name `sha` or an ancestor of it
/// (GitHub's compare of `<tag>...<sha>` is `identical` or `ahead`). `behind` is no
/// answer to accept: it means a newer release exists, which is what this guard is for.
/// Anything else, an unknown commit or a compare that fails, refuses too.
///
/// Run it before the first write of `release tag`, `release publish` and a site
/// deployment, so a re-run of an old run does nothing.
pub fn superseded(gh: &Path, repo: &str, sha: &str) -> Result<Standing, Error> {
    match placed(gh, repo, sha)? {
        Placed::Stands(standing) => Ok(standing),
        Placed::Behind(top) => Err(not_after(&top.tag(), sha, "behind")),
    }
}
