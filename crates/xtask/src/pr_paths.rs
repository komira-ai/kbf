//! `pr-paths --base <rev> --patterns <file> --key <name> [--output <file>]`: whether a pull
//! request changes a path that a pattern file names. It prints `<key>=true` or `<key>=false`,
//! then each changed path that matched with the pattern it matched, and appends the
//! `<key>=...` line to `--output` (on Actions, `GITHUB_OUTPUT`).
//!
//! `.github/workflows/macos-pr.yml` runs its macOS jobs only when this says `true` for
//! `tools/ci/macos-pr-paths.txt`: a pull request that touches none of those paths takes no
//! macOS runner.
//!
//! The changed paths are those of `git diff --no-renames --name-only -z <base> HEAD` in the
//! root. On a pull request's merge commit with `--base HEAD^1` (the base branch it merges
//! into), they are every path the pull request adds, changes or deletes. A rename counts as
//! both its old path and its new one (`--no-renames`), so moving a file out of a matched
//! directory still matches. A base git cannot read is an error, and so is a pattern file
//! that does not parse ([`parse`]): the step fails, so the jobs that need it fail too and
//! are never skipped.
//!
//! The pattern file has one pattern per line. Blank lines and lines that start with `#` are
//! skipped. A pattern is a path relative to the repository root, in one of two forms
//! ([`Pattern`]): a file, written out exactly, or a directory and everything under it,
//! written `<dir>/**`. Every other spelling is refused, with its line ([`parse`]), so no
//! pattern is read one way here and another way by a reader.

use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Args as ClapArgs;

use crate::Error;

/// Flags of `pr-paths`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// The commit to diff `HEAD` against (`HEAD^1` on a pull request's merge commit).
    #[arg(long)]
    pub base: String,
    /// The pattern file, relative to the root or absolute.
    #[arg(long)]
    pub patterns: PathBuf,
    /// The name of the output (`macos`).
    #[arg(long)]
    pub key: String,
    /// Append `<key>=true|false` to this file (on Actions, `GITHUB_OUTPUT`).
    #[arg(long)]
    pub output: Option<PathBuf>,
}

/// One line of the pattern file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Pattern {
    /// Exactly this file.
    File(String),
    /// This directory and everything under it (written `<dir>/**`).
    Tree(String),
}

impl Pattern {
    /// Whether `path` (relative to the root, `/` between its parts) is this pattern's file,
    /// or a file under this pattern's directory.
    pub fn matches(&self, path: &str) -> bool {
        match self {
            Pattern::File(file) => path == file,
            Pattern::Tree(dir) => path
                .strip_prefix(dir.as_str())
                .is_some_and(|rest| rest.starts_with('/')),
        }
    }

    /// The pattern as the file writes it.
    pub fn spelling(&self) -> String {
        match self {
            Pattern::File(file) => file.clone(),
            Pattern::Tree(dir) => format!("{dir}/**"),
        }
    }
}

/// The patterns of a pattern file, in order, or one problem for each line that is not a
/// pattern (and one for a file that has none).
///
/// A pattern is refused when it has spaces at its start or end, starts with `/`, ends with
/// `/`, holds a backslash, an empty, `.` or `..` part, or a `*` anywhere but as the final
/// `/**`, or repeats an earlier pattern.
pub fn parse(text: &str) -> Result<Vec<Pattern>, Vec<String>> {
    let mut patterns: Vec<Pattern> = Vec::new();
    let mut problems = Vec::new();
    for (at, line) in text.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let n = at + 1;
        match pattern(line) {
            Err(why) => problems.push(format!("line {n}: `{line}` {why}")),
            Ok(p) if patterns.contains(&p) => {
                problems.push(format!("line {n}: `{line}` is listed twice"));
            }
            Ok(p) => patterns.push(p),
        }
    }
    if patterns.is_empty() && problems.is_empty() {
        problems.push("the file lists no pattern, so it would match nothing".to_owned());
    }
    if problems.is_empty() {
        Ok(patterns)
    } else {
        Err(problems)
    }
}

/// One pattern line, or why it is not one.
fn pattern(line: &str) -> Result<Pattern, &'static str> {
    if line.trim() != line {
        return Err("has spaces at its start or end");
    }
    if line.contains('\\') {
        return Err("holds a backslash; parts are separated by `/`");
    }
    let (path, tree) = match line.strip_suffix("/**") {
        Some(dir) => (dir, true),
        None => (line, false),
    };
    if path.contains('*') {
        return Err("holds a `*` that is not the final `/**`");
    }
    if path.starts_with('/') {
        return Err("starts with `/`; patterns are relative to the root");
    }
    if path.ends_with('/') {
        return Err("ends with `/`; a directory is written `<dir>/**`");
    }
    if path.split('/').any(|part| matches!(part, "" | "." | "..")) {
        return Err("has an empty, `.` or `..` part");
    }
    Ok(if tree {
        Pattern::Tree(path.to_owned())
    } else {
        Pattern::File(path.to_owned())
    })
}

/// Each path of `changed` that some pattern matches, with the first pattern that does, in
/// the order of `changed`.
pub fn matched<'a>(patterns: &'a [Pattern], changed: &'a [String]) -> Vec<(&'a str, &'a Pattern)> {
    changed
        .iter()
        .filter_map(|path| {
            patterns
                .iter()
                .find(|p| p.matches(path))
                .map(|p| (path.as_str(), p))
        })
        .collect()
}

/// The lines `pr-paths` prints for `key`, given the matches: `<key>=true|false`, then one
/// line for each match.
pub fn report(key: &str, found: &[(&str, &Pattern)]) -> Vec<String> {
    let mut lines = vec![format!("{key}={}", !found.is_empty())];
    lines.extend(
        found
            .iter()
            .map(|(path, p)| format!("changed: {path} (matches {})", p.spelling())),
    );
    lines
}

/// The paths `git diff --no-renames --name-only -z <base> HEAD` lists in `root`.
fn changed_paths(root: &Path, base: &str) -> Result<Vec<String>, Error> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args([
            "diff",
            "--no-renames",
            "--name-only",
            "-z",
            base,
            "HEAD",
            "--",
        ])
        .output()
        .map_err(Error::spawn("git".to_owned()))?;
    if !out.status.success() {
        return Err(Error::Failed(format!(
            "git diff {base} HEAD: {}",
            String::from_utf8_lossy(&out.stderr).trim_end()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout)
        .split('\0')
        .filter(|p| !p.is_empty())
        .map(str::to_owned)
        .collect())
}

/// Runs `pr-paths` in the repository `root`: the lines to print.
pub fn run(args: &Args, root: &Path) -> Result<Vec<String>, Error> {
    let file = root.join(&args.patterns);
    let text = std::fs::read_to_string(&file).map_err(Error::spawn(file.display().to_string()))?;
    let patterns = parse(&text).map_err(|problems| {
        Error::Refused(
            problems
                .into_iter()
                .map(|p| format!("{}: {p}", args.patterns.display()))
                .collect(),
        )
    })?;
    let changed = changed_paths(root, &args.base)?;
    let lines = report(&args.key, &matched(&patterns, &changed));
    if let Some(output) = &args.output {
        crate::release::append_output(output, &lines[..1])?;
    }
    Ok(lines)
}

#[cfg(test)]
mod tests;
