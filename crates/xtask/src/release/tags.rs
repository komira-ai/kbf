//! The release tags, `v<MAJOR>.<MINOR>.<PATCH>`, and Rule S: a re-run of an old run must
//! not tag, publish or deploy over a newer release.
//!
//! A release tag is `v` and three plain decimal numbers. Anything else (`v0.1.2-rc1`,
//! `v0.1.2-probe`, `v01.2.3`, `latest`) is no release tag: it is never counted and never
//! the highest. Versions compare number by number, so 10 is above 9.

use std::collections::BTreeSet;
use std::path::Path;

use serde_json::Value;

use super::{api, graphql};
use crate::Error;
use crate::pages::MAX_PAGES;

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

/// The GraphQL read of one page of releases: the two fields a release is read for, and the
/// cursor of the next page. REST's `repos/<repo>/releases` answers each release with every
/// asset it carries (81 a release here, about 145 kB): a page of 100 is about 16 MB and
/// takes GitHub 5 to 8 seconds, close enough to its 10-second limit that it answers 504.
pub const RELEASES_QUERY: &str = "query($owner: String!, $name: String!, $after: String) { \
     repository(owner: $owner, name: $name) { releases(first: 100, after: $after) { \
     pageInfo { hasNextPage endCursor } nodes { tagName isDraft } } } }";

/// What the release list is named in errors.
fn releases_what(repo: &str) -> String {
    format!("gh api graphql releases of {repo}")
}

/// Every release of `repo` (`{tagName, isDraft}`), page after page until GitHub says no
/// page follows. A page that is not there, a next page with no cursor, a cursor given
/// twice (the read would never end) or a next page after [`MAX_PAGES`] is an error.
fn releases(gh: &Path, repo: &str) -> Result<Vec<Value>, Error> {
    let what = releases_what(repo);
    // A `repo` with no `/` asks for an empty name, which GraphQL refuses.
    let (owner, name) = repo.split_once('/').unwrap_or((repo, ""));
    let mut all = Vec::new();
    let mut seen = BTreeSet::new();
    let mut after: Option<String> = None;
    for n in 1..=MAX_PAGES {
        let mut vars = vec![("owner", owner), ("name", name)];
        if let Some(cursor) = &after {
            vars.push(("after", cursor));
        }
        let got = graphql(gh, RELEASES_QUERY, &vars)?;
        let list = &got["data"]["repository"]["releases"];
        let (Some(nodes), Some(more)) = (
            list["nodes"].as_array(),
            list["pageInfo"]["hasNextPage"].as_bool(),
        ) else {
            return Err(Error::Failed(format!(
                "{what}: page {n}: no list of entries"
            )));
        };
        all.extend(nodes.iter().cloned());
        if !more {
            return Ok(all);
        }
        let Some(cursor) = list["pageInfo"]["endCursor"].as_str() else {
            return Err(Error::Failed(format!(
                "{what}: page {n} says another page follows but gives no cursor to it"
            )));
        };
        if !seen.insert(cursor.to_owned()) {
            return Err(Error::Failed(format!(
                "{what}: page {n} gives the cursor {cursor:?} again, so the list has no end \
                 to read to"
            )));
        }
        after = Some(cursor.to_owned());
    }
    Err(Error::Failed(format!(
        "{what}: page {MAX_PAGES} says another page follows, the most pages read of one \
         list: stopped rather than read on"
    )))
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
    Ok(releases(gh, repo)?
        .iter()
        .filter_map(|r| Version::parse_tag(r["tagName"].as_str()?))
        .collect())
}

/// The tags of the published releases of `repo`: every release that is not a draft. A
/// release whose `isDraft` is not a boolean is an error, not a guess either way.
pub fn published_tags(gh: &Path, repo: &str) -> Result<BTreeSet<String>, Error> {
    let what = releases_what(repo);
    let mut published = BTreeSet::new();
    for release in releases(gh, repo)? {
        let tag = release["tagName"].as_str();
        match (tag, release["isDraft"].as_bool()) {
            (Some(tag), Some(false)) => {
                published.insert(tag.to_owned());
            }
            (Some(_), Some(true)) => {}
            _ => {
                return Err(Error::Failed(format!(
                    "{what}: a release without a tagName or a boolean isDraft: {release}"
                )));
            }
        }
    }
    Ok(published)
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
