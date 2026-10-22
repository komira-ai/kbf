//! Crates git sees renamed: a floor follows its crate to the new name, so a pure
//! rename does not read as a floor added under the new name (`no-lowering --base-ref`).
//!
//! A crate is renamed when `crates/A/Cargo.toml` becomes `crates/B/Cargo.toml` in
//! `git diff --find-renames <base> HEAD` and the package `name` differs between the two
//! versions of the file. The floors file is keyed by the directory name, so the map is
//! from `B` to `A`. [`lowered_renamed`] reads `B`'s floor against `A`'s in the base. A
//! crate whose directory moved under the same package name is not renamed here, and a
//! crate that is really new has no base row, so it keeps the 99.00 default.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::Command;

use crate::floors::Floors;
use crate::raise::{Lowered, lowered};

/// Which version of a `Cargo.toml`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    Base,
    Head,
}

/// Why git could not answer.
#[derive(Debug, thiserror::Error)]
pub enum GitError {
    #[error("start git: {0}")]
    Spawn(#[from] std::io::Error),
    #[error("git {args}: {stderr}")]
    Failed { args: String, stderr: String },
}

/// The `[package]` name of a `Cargo.toml` text.
pub fn package_name(toml: &str) -> Option<String> {
    let mut in_package = false;
    for line in toml.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_package = line == "[package]";
        } else if in_package
            && let Some((key, value)) = line.split_once('=')
            && key.trim() == "name"
        {
            return value
                .trim()
                .strip_prefix('"')
                .and_then(|v| v.split_once('"'))
                .map(|(name, _)| name.to_owned());
        }
    }
    None
}

/// The crate directory of a `crates/<dir>/Cargo.toml` path.
fn crate_dir(path: &str) -> Option<&str> {
    let dir = path.strip_prefix("crates/")?.strip_suffix("/Cargo.toml")?;
    (!dir.contains('/')).then_some(dir)
}

/// The `(old dir, new dir)` of every crate manifest renamed in `git diff --name-status
/// -z` output.
pub fn manifest_moves(raw: &[u8]) -> Vec<(String, String)> {
    let text = String::from_utf8_lossy(raw);
    let mut fields = text.split('\0');
    let mut out = Vec::new();
    while let Some(status) = fields.next() {
        if status.starts_with('R') {
            let old = fields.next().unwrap_or_default();
            let new = fields.next().unwrap_or_default();
            if let (Some(old), Some(new)) = (crate_dir(old), crate_dir(new)) {
                out.push((old.to_owned(), new.to_owned()));
            }
        } else if !status.is_empty() {
            fields.next();
        }
    }
    out
}

/// New directory to old directory, for the moves whose package name differs;
/// `package(side, dir)` is the package name in `dir`'s manifest on that side.
pub fn renamed_crates(
    moves: &[(String, String)],
    package: impl Fn(Side, &str) -> Option<String>,
) -> BTreeMap<String, String> {
    moves
        .iter()
        .filter(|(old, new)| {
            let (before, after) = (package(Side::Base, old), package(Side::Head, new));
            before.is_some() && after.is_some() && before != after
        })
        .map(|(old, new)| (new.clone(), old.clone()))
        .collect()
}

fn git(repo: &Path, args: &[&str]) -> Result<Vec<u8>, GitError> {
    let out = Command::new("git")
        .arg("-C")
        .arg(repo)
        .args(args)
        .output()?;
    if out.status.success() {
        return Ok(out.stdout);
    }
    Err(GitError::Failed {
        args: args.join(" "),
        stderr: String::from_utf8_lossy(&out.stderr).trim_end().to_owned(),
    })
}

/// The crates renamed between `base_ref` and `HEAD` of the repository at `repo`, as git
/// finds them: new directory to old.
///
/// # Errors
/// Git does not start or does not know `base_ref`.
pub fn git_renames(repo: &Path, base_ref: &str) -> Result<BTreeMap<String, String>, GitError> {
    let diff = [
        "diff",
        "--find-renames",
        "--name-status",
        "-z",
        base_ref,
        "HEAD",
    ];
    let moves = manifest_moves(&git(repo, &diff)?);
    let package = |side: Side, dir: &str| {
        let rev = match side {
            Side::Base => base_ref,
            Side::Head => "HEAD",
        };
        let spec = format!("{rev}:crates/{dir}/Cargo.toml");
        let text = git(repo, &["show", &spec]).ok()?;
        package_name(&String::from_utf8_lossy(&text))
    };
    Ok(renamed_crates(&moves, package))
}

/// [`lowered`], reading the floor of a renamed crate (`renames`: new directory to old)
/// against the base's row for the old name.
pub fn lowered_renamed(
    base: &Floors,
    head: &Floors,
    renames: &BTreeMap<String, String>,
) -> Vec<Lowered> {
    let mut base = base.clone();
    for (new, old) in renames {
        if let Some(floor) = base.remove(old) {
            base.insert(new.clone(), floor);
        }
    }
    lowered(&base, head)
}

#[cfg(test)]
mod tests;
