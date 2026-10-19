//! What `release verdict` and `release notes` ask of `git`: a tag's object, whether a
//! commit is on main, the release tags in version order and the commits between two.

use std::path::Path;
use std::process::Command;

use crate::Error;

/// Runs `git -C <repo> <args>`: its stdout, or the error holding what it printed on stderr.
fn git(repo: &Path, args: &[&str]) -> Result<String, Error> {
    let out = git_output(repo, args)?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(Error::Failed(format!(
            "git {}: {}",
            args.join(" "),
            err.trim_end()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn git_output(repo: &Path, args: &[&str]) -> Result<std::process::Output, Error> {
    Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()
        .map_err(Error::spawn("git".to_owned()))
}

/// A tag as `git cat-file` shows it.
#[derive(Debug, PartialEq, Eq)]
pub enum Tag {
    /// No tag object: the name is a bare ref to a commit, with no message.
    Lightweight,
    /// An annotated tag.
    Annotated {
        /// The kind of object it names (`commit` for a release).
        kind: String,
        /// The object it names.
        object: String,
        /// The name it was made with.
        name: String,
        /// The message, verbatim.
        message: String,
    },
}

/// The tag `tag` of the repository at `repo`.
pub fn read_tag(repo: &Path, tag: &str) -> Result<Tag, Error> {
    let refname = format!("refs/tags/{tag}");
    // `-t` prints `tag` for an annotated tag and `commit` for a bare ref to a commit.
    let kind = git(repo, &["cat-file", "-t", &refname])?;
    if kind.trim_end() != "tag" {
        return Ok(Tag::Lightweight);
    }
    let raw = git(repo, &["cat-file", "tag", &refname])?;
    let (headers, message) = raw.split_once("\n\n").unwrap_or((raw.as_str(), ""));
    // A header git would not have written reads as empty, and no check passes an empty one.
    let header = |key: &str| {
        headers
            .lines()
            .find_map(|l| l.strip_prefix(key)?.strip_prefix(' '))
            .unwrap_or_default()
            .to_owned()
    };
    Ok(Tag::Annotated {
        kind: header("type"),
        object: header("object"),
        name: header("tag"),
        message: message.to_owned(),
    })
}

/// Whether `commit` is `main_ref` or an ancestor of it.
pub fn on_main(repo: &Path, commit: &str, main_ref: &str) -> Result<bool, Error> {
    let out = git_output(repo, &["merge-base", "--is-ancestor", commit, main_ref])?;
    match out.status.code() {
        Some(0) => Ok(true),
        // 1 is "not an ancestor"; anything else (an unknown ref) is an error, not a no.
        Some(1) => Ok(false),
        _ => Err(Error::Failed(format!(
            "git merge-base --is-ancestor {commit} {main_ref}: {}",
            String::from_utf8_lossy(&out.stderr).trim_end()
        ))),
    }
}

/// The `v*` tags, oldest version first.
pub fn release_tags(repo: &Path) -> Result<Vec<String>, Error> {
    let listed = git(repo, &["tag", "--list", "v*", "--sort=version:refname"])?;
    Ok(listed.lines().map(str::to_owned).collect())
}

/// `<abbreviated sha> <subject>` of each commit in `since..commit` (all of `commit`'s
/// history when `since` is none), newest first.
pub fn commits(repo: &Path, since: Option<&str>, commit: &str) -> Result<Vec<String>, Error> {
    let range = since.map_or_else(|| commit.to_owned(), |s| format!("refs/tags/{s}..{commit}"));
    let listed = git(repo, &["log", "--format=%h %s", &range])?;
    Ok(listed.lines().map(str::to_owned).collect())
}
