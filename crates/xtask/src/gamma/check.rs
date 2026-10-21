//! `cargo xtask gamma check-build`: after a deploy, build komira's main against the farm
//! that was just updated, with the farm's normal caching, and say what ran where.
//!
//! The build passes only when buck2 prints `BUILD SUCCEEDED` and its `Commands:` line says
//! `local: 0` (every action ran on the farm or was answered by its cache) and buck2 exited 0.
//! The last `BUILD ...` line and the last `Commands:` line of the output win, and a missing or
//! unreadable one is a failure, never a pass. `--no-remote-cache` is not an argument of this
//! check, and a target that is a flag (or an argument file) is refused, so the variable that
//! names the targets cannot add one.
//!
//! This repository's logs are public and the output of a build names the farm, so buck2's whole
//! output goes to `--log` and is not printed. What the step prints and appends to
//! `--summary` is the `Commands:` line, rebuilt from the parsed text, and the verdict: the
//! parser keeps only a line of lowercase words, digits and `: , _`, so nothing host-like gets
//! through it. The farm's buckconfig is secret material (it names the farm): it is the text of
//! the environment variable [`BUCKCONFIG_ENV`], written to `--buckconfig-file`.

use std::fs;
use std::path::Path;
use std::process::Command;

use crate::Error;

use super::append;

/// The environment variable that holds the text of the farm's buckconfig. It is a secret of
/// the `gamma` environment and not a flag or a repository variable: GitHub prints a variable
/// in the header of the step that has it, and the farm's addresses are in this text.
pub const BUCKCONFIG_ENV: &str = "GAMMA_FARM_BUCKCONFIG";

/// The flag that would make the check prove nothing about the farm's cache.
const NO_CACHE: &str = "--no-remote-cache";

/// What `gamma check-build` is given.
pub struct CheckBuild<'a> {
    /// The checkout of komira's main: its `buck2` launcher is at the top.
    pub komira_dir: &'a Path,
    /// The targets to build, separated by white space.
    pub targets: &'a str,
    /// The text of the farm's buckconfig.
    pub config: &'a str,
    /// Where the config is written; an absolute path, which `buck2 --config-file` is given.
    pub buckconfig_file: &'a Path,
    /// Where buck2's output goes. It is never printed.
    pub log: &'a Path,
    /// The file the `Commands:` line and the verdict are appended to.
    pub summary: &'a Path,
}

/// What a buck2 log says: the `Commands:` line and why the build did not pass.
#[derive(Debug, PartialEq, Eq)]
pub struct Judged {
    /// `Commands: N (cached: N, remote: N, local: N)`, rebuilt from the log's last such line;
    /// none when the log has none or it does not parse.
    pub commands: Option<String>,
    /// Why the build did not pass; none when it passed.
    pub failure: Option<&'static str>,
}

impl Judged {
    /// The line to show: the `Commands:` line, or that there was none.
    fn line(&self) -> &str {
        self.commands.as_deref().unwrap_or("Commands: not printed")
    }

    /// `PASS`, or `FAIL` and the reason.
    fn verdict(&self) -> String {
        match self.failure {
            None => "PASS".to_owned(),
            Some(why) => format!("FAIL ({why})"),
        }
    }

    /// What is appended to the job summary: the `Commands:` line and the verdict.
    pub fn summary(&self) -> String {
        let (line, verdict) = (self.line(), self.verdict());
        format!("### gamma check\n\n{line}\n\n{verdict}\n")
    }
}

/// A count: ASCII digits only, so `+1` or `1.5` is not read as one.
fn number(text: &str) -> Option<u64> {
    let digits = !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    digits.then(|| text.parse().ok()).flatten()
}

/// The text after `Commands: ` as `N (cached: N, remote: N, local: N)`: the line rebuilt, and
/// the local count. Words of other counts are kept, but only of lowercase letters, digits,
/// spaces, `:`, `,` and `_`; anything else (a name with a dot, say) is not a Commands line.
fn counts(rest: &str) -> Option<(String, u64)> {
    let (total, list) = rest.split_once(" (")?;
    let (list, _) = list.split_once(')')?;
    number(total)?;
    let mut items = list.split(", ");
    let local = items.find_map(|item| item.strip_prefix("local: "))?;
    let local = number(local)?;
    let plain = |b: u8| matches!(b, b'a'..=b'z' | b'0'..=b'9' | b' ' | b':' | b',' | b'_');
    let line = format!("Commands: {total} ({list})");
    list.bytes().all(plain).then_some((line, local))
}

/// The last `Commands:` line of the log, or why there is none.
fn commands(log: &str) -> Result<(String, u64), &'static str> {
    let last = log.lines().rev().find_map(|l| l.split_once("Commands: "));
    let Some((_, rest)) = last else {
        return Err("no Commands line");
    };
    counts(rest).ok_or("the Commands line does not parse")
}

/// The last `BUILD SUCCEEDED` (true) or `BUILD FAILED` (false) line of the log.
fn verdict(log: &str) -> Option<bool> {
    let ends = |line: &str| match line.trim() {
        "BUILD SUCCEEDED" => Some(true),
        "BUILD FAILED" => Some(false),
        _ => None,
    };
    log.lines().rev().find_map(ends)
}

/// Reads buck2's output: passes only for `BUILD SUCCEEDED`, a Commands line of `local: 0` and
/// an exit status of 0 (`exited_ok`). The last summary of the log wins.
pub fn judge(log: &str, exited_ok: bool) -> Judged {
    let counted = commands(log);
    let failure = match (verdict(log), &counted) {
        (None, _) => Some("no BUILD SUCCEEDED or BUILD FAILED line"),
        (Some(false), _) => Some("BUILD FAILED"),
        (_, Err(why)) => Some(*why),
        (_, Ok((_, local))) if *local != 0 => Some("local commands ran"),
        _ if !exited_ok => Some("buck2 exited with a failure status"),
        _ => None,
    };
    Judged {
        commands: counted.ok().map(|(line, _)| line),
        failure,
    }
}

/// The targets of the variable: at least one, and none a flag or an argument file.
fn targets(text: &str) -> Result<Vec<&str>, Error> {
    let words: Vec<&str> = text.split_whitespace().collect();
    if words.is_empty() {
        let why = "--targets names no target".to_owned();
        return Err(Error::Usage(why));
    }
    for word in &words {
        if word.starts_with(NO_CACHE) {
            let why = format!("--targets has {NO_CACHE}: the check builds with the cache");
            return Err(Error::Usage(why));
        }
        if word.starts_with(['-', '@']) {
            let why = "--targets has a flag or an argument file".to_owned();
            return Err(Error::Usage(why));
        }
    }
    Ok(words)
}

/// The arguments of `buck2`: a plain build with the farm's config file. There is no flag here
/// that bypasses the cache, and none can be passed in.
fn buck2_args(config: &Path, targets: &[&str]) -> Vec<String> {
    let mut args = vec![
        "build".to_owned(),
        "--console".to_owned(),
        "simple".to_owned(),
        "--config-file".to_owned(),
        config.to_string_lossy().into_owned(),
    ];
    args.extend(targets.iter().copied().map(str::to_owned));
    args
}

/// The `gamma check-build` verb: the lines to print when the build passed.
pub fn check_build(c: &CheckBuild<'_>) -> Result<Vec<String>, Error> {
    let targets = targets(c.targets)?;
    if c.config.trim().is_empty() {
        let why = format!("{BUCKCONFIG_ENV} is not set or is empty");
        return Err(Error::Usage(why));
    }
    if !c.buckconfig_file.is_absolute() {
        let why = "--buckconfig-file is not an absolute path".to_owned();
        return Err(Error::Usage(why));
    }
    let what = format!("open {}", c.komira_dir.display());
    let dir = fs::canonicalize(c.komira_dir).map_err(Error::spawn(what))?;
    let what = format!("write {}", c.buckconfig_file.display());
    fs::write(c.buckconfig_file, c.config).map_err(Error::spawn(what))?;
    let out = Command::new(dir.join("buck2"))
        .current_dir(&dir)
        .args(buck2_args(c.buckconfig_file, &targets))
        .output()
        .map_err(Error::spawn("buck2".to_owned()))?;
    let mut text = out.stdout;
    text.extend_from_slice(&out.stderr);
    let what = format!("write {}", c.log.display());
    fs::write(c.log, &text).map_err(Error::spawn(what))?;
    let judged = judge(&String::from_utf8_lossy(&text), out.status.success());
    append(c.summary, &judged.summary())?;
    match judged.failure {
        None => Ok(vec![judged.line().to_owned(), judged.verdict()]),
        Some(why) => Err(Error::Failed(format!("gamma check: {why}"))),
    }
}

#[cfg(test)]
mod tests;
