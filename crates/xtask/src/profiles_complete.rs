//! `profiles-complete`: refuses a directory of raw coverage profiles in which a shard's
//! profiles are missing.
//!
//! The coverage run of `ci.yml` runs its tests in shards; each shard names its raw profiles
//! `kbf-<core>-<part>-<process>-<n>.profraw`, and the `test` job puts every shard's files in
//! one directory and makes one report of them. A shard whose artifact did not come, or came
//! empty, would not fail that report: llvm-cov reports on whatever profiles are there, so the
//! tests that shard ran would count as never run, and a crate could read under 99% (or, with a
//! lenient floor, over it) for a reason the log does not give. This verb is the check at that
//! boundary: every `--prefix` (one per shard) must begin at least one non-empty `.profraw` file
//! of `--dir`, and no `.profraw` file may begin none of them (a shard the run does not list).

use std::path::{Path, PathBuf};

use clap::Args as ClapArgs;

use crate::Error;

/// Stray files named in a refusal before it says how many more there are.
const SHOWN: usize = 5;

/// Flags of `profiles-complete`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// The directory the profiles are in.
    #[arg(long)]
    pub dir: PathBuf,
    /// The file-name prefix of one shard's profiles; once for each shard.
    #[arg(long = "prefix", required = true)]
    pub prefixes: Vec<String>,
}

/// The `.profraw` files of `dir` as `(name, bytes)`, in name order.
fn profiles(dir: &Path) -> Result<Vec<(String, u64)>, Error> {
    let what = || format!("read {}", dir.display());
    let spawn = |e: std::io::Error| Error::Spawn {
        what: what(),
        why: e.to_string(),
    };
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(spawn)? {
        let entry = entry.map_err(spawn)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let meta = entry.metadata().map_err(spawn)?;
        if meta.is_file() && name.ends_with(".profraw") {
            found.push((name, meta.len()));
        }
    }
    found.sort();
    Ok(found)
}

/// Check the directory: one line for each shard saying how many profiles it has, or why not.
pub fn run(args: &Args) -> Result<Vec<String>, Error> {
    let files = profiles(&args.dir)?;
    let mut problems = Vec::new();
    let mut lines = Vec::new();
    for prefix in &args.prefixes {
        let own: Vec<_> = files
            .iter()
            .filter(|(n, _)| n.starts_with(prefix))
            .collect();
        let bytes: u64 = own.iter().map(|(_, len)| len).sum();
        if own.is_empty() {
            problems.push(format!(
                "no raw profile in {} begins `{prefix}`: that shard's profiles did not arrive",
                args.dir.display()
            ));
        } else if bytes == 0 {
            problems.push(format!(
                "every raw profile that begins `{prefix}` is empty ({} file(s))",
                own.len()
            ));
        } else {
            lines.push(format!(
                "profiles-complete: `{prefix}` {} file(s), {bytes} byte(s)",
                own.len()
            ));
        }
    }
    let stray: Vec<&str> = files
        .iter()
        .map(|(n, _)| n.as_str())
        .filter(|n| !args.prefixes.iter().any(|p| n.starts_with(p.as_str())))
        .collect();
    if !stray.is_empty() {
        let shown: Vec<&str> = stray.iter().copied().take(SHOWN).collect();
        problems.push(format!(
            "{} raw profile(s) begin with none of the shards' prefixes, e.g. {}",
            stray.len(),
            shown.join(", ")
        ));
    }
    if problems.is_empty() {
        Ok(lines)
    } else {
        Err(Error::Refused(problems))
    }
}

#[cfg(test)]
mod tests;
