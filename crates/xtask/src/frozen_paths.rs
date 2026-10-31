//! `frozen-paths --base <rev> --head <rev>`: refuses a change that touches a path in
//! [`FROZEN`].
//!
//! tierstore is moving to a repository of its own. From the commit the import copies until the
//! import has landed there, nothing in this repository may change the paths that move, or the
//! copy would miss it. `tierstore-move-freeze.yml` runs this on every pull request (it has no
//! `paths:` filter, so the check it makes required always reports) and the verb is deleted with
//! that workflow in the change that switches this repository over.
//!
//! A path is frozen when it is a [`FROZEN`] entry or lies under one, by whole directory names:
//! `crates/tierstore-bench/` is not under `crates/tierstore`. The change list is `git diff
//! --name-only --no-renames`, so a rename is listed as the deletion of the old path and the
//! addition of the new, and a rename out of (or into) a frozen path touches it.

use std::path::Path;

use clap::Args as ClapArgs;

use crate::Error;

/// The paths that move, relative to the repository root: a directory (and all it holds) or a
/// file.
pub const FROZEN: &[&str] = &[
    "crates/tierstore",
    "crates/objstore-s3",
    "packaging/tierstore",
    ".github/workflows/tierstore-site.yml",
    ".github/workflows/tierstore-site-publish.yml",
];

/// Flags of `frozen-paths`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// The revision the change starts from (on a pull request, the base branch).
    #[arg(long)]
    pub base: String,
    /// The revision the change ends at.
    #[arg(long)]
    pub head: String,
}

/// The [`FROZEN`] entry `path` is, or is under.
fn frozen(path: &str) -> Option<&'static str> {
    FROZEN.iter().copied().find(|entry| {
        path == *entry
            || path
                .strip_prefix(entry)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// One refusal line for each of `paths` that is frozen, in order.
pub fn touched<'a>(paths: impl IntoIterator<Item = &'a str>) -> Vec<String> {
    paths
        .into_iter()
        .filter_map(|path| {
            frozen(path).map(|entry| {
                format!(
                    "{path}: under `{entry}`, which is frozen until the tierstore import has \
                     landed in its new repository; open the change in komira-ai/tierstore after \
                     the import instead"
                )
            })
        })
        .collect()
}

/// The paths `git diff` lists between `base` and `head` in `root`.
fn changed(root: &Path, base: &str, head: &str) -> Result<Vec<String>, Error> {
    let args = ["diff", "--name-only", "--no-renames", "-z", base, head];
    let out = crate::git_stdout(root, &args)?;
    Ok(String::from_utf8_lossy(&out)
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect())
}

/// The verb: the line it prints when no changed path is frozen.
///
/// # Errors
/// `git diff` failed, or a changed path is frozen (one line for each).
pub fn run(args: &Args, root: &Path) -> Result<String, Error> {
    let paths = changed(root, &args.base, &args.head)?;
    let refused = touched(paths.iter().map(String::as_str));
    if refused.is_empty() {
        return Ok(format!(
            "ok: {} changed paths, none under a frozen path",
            paths.len()
        ));
    }
    Err(Error::Refused(refused))
}
