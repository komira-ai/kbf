//! `merge-profiles`: merges the coverage profiles of a directory into one indexed profile.
//!
//! The coverage run of `ci.yml` splits its tests into shards, and the raw profiles
//! (`*.profraw`) of all of them are about 14 GB unpacked (1.8 GB as artifacts): the `test` job's runner ran out of disk
//! downloading them. A shard runs this on its own raw profiles and uploads the one `.profdata`
//! (megabytes); the `test` job runs it again on the shards' `.profdata` files, into the profile
//! `llvm-cov report` reads. `llvm-profdata merge` adds the counts of its inputs whatever their
//! form, so the profile made in two steps is the one made from all the raw files at once.
//!
//! The tool is the `llvm-profdata` of the toolchain this verb runs under (`cargo +<toolchain>
//! xtask` sets it for `rustc`): the one `cargo llvm-cov` runs, from the `llvm-tools-preview`
//! component. No `-failure-mode` is passed, so a corrupt input fails the merge (the default,
//! `any`), where `-failure-mode=all` would drop it and the tests it recorded would count as
//! never run.

use std::path::{Path, PathBuf};
use std::process::Command;

use clap::Args as ClapArgs;

use crate::Error;

/// Flags of `merge-profiles`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// The directory the profiles to merge are in.
    #[arg(long)]
    pub dir: PathBuf,
    /// The extension of the files to merge: `profraw` or `profdata`.
    #[arg(long)]
    pub extension: String,
    /// Merge only the files whose names begin with this (a shard's own).
    #[arg(long, default_value = "")]
    pub prefix: String,
    /// The profile to write.
    #[arg(
        long,
        conflicts_with = "workspace_profile_in",
        required_unless_present = "workspace_profile_in"
    )]
    pub out: Option<PathBuf>,
    /// Write the profile `cargo llvm-cov report` reads in this directory: `<name of the
    /// workspace directory>.profdata`, the name cargo-llvm-cov gives it. When no `.profraw`
    /// file is there and this one is, the report uses it as it is.
    #[arg(long)]
    pub workspace_profile_in: Option<PathBuf>,
    /// The `llvm-profdata` program (default: the running toolchain's own).
    #[arg(long)]
    pub llvm_profdata: Option<PathBuf>,
}

/// The files of `dir` named `<prefix>*.<extension>`, in name order, with their sizes.
fn inputs(dir: &Path, extension: &str, prefix: &str) -> Result<Vec<(PathBuf, u64)>, Error> {
    let what = || format!("read {}", dir.display());
    let mut found = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(Error::spawn(what()))? {
        let entry = entry.map_err(Error::spawn(what()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let meta = entry.metadata().map_err(Error::spawn(what()))?;
        if meta.is_file() && name.starts_with(prefix) && name.ends_with(&format!(".{extension}")) {
            found.push((entry.path(), meta.len()));
        }
    }
    found.sort();
    Ok(found)
}

/// What the running toolchain's `rustc` prints for `args`.
fn rustc(args: &[&str]) -> Result<String, Error> {
    let out = Command::new("rustc")
        .args(args)
        .output()
        .map_err(Error::spawn(format!("rustc {}", args.join(" "))))?;
    if !out.status.success() {
        return Err(Error::Failed(format!(
            "rustc {} failed ({})",
            args.join(" "),
            out.status
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The toolchain's `llvm-profdata`: `<sysroot>/lib/rustlib/<host>/bin/llvm-profdata`.
fn toolchain_llvm_profdata() -> Result<PathBuf, Error> {
    let sysroot = rustc(&["--print", "sysroot"])?;
    let verbose = rustc(&["-vV"])?;
    let host = verbose.lines().find_map(|l| l.strip_prefix("host: "));
    Ok(Path::new(sysroot.trim())
        .join("lib/rustlib")
        .join(host.unwrap_or_default().trim())
        .join("bin/llvm-profdata"))
}

fn mib(bytes: u64) -> String {
    format!("{:.1} MiB", bytes as f64 / (1024.0 * 1024.0))
}

/// The profile to write.
fn out_path(args: &Args, root: &Path) -> Result<PathBuf, Error> {
    let Some(dir) = &args.workspace_profile_in else {
        return args.out.clone().ok_or_else(|| {
            Error::Usage("merge-profiles needs --out or --workspace-profile-in".to_owned())
        });
    };
    let root = root
        .canonicalize()
        .map_err(Error::spawn(format!("resolve {}", root.display())))?;
    let name = root
        .file_name()
        .ok_or_else(|| Error::Failed(format!("{} has no directory name", root.display())))?;
    Ok(dir.join(format!("{}.profdata", name.to_string_lossy())))
}

/// Merge the profiles: one line saying what went in and what came out.
pub fn run(args: &Args, root: &Path) -> Result<Vec<String>, Error> {
    let out = out_path(args, root)?;
    let files = inputs(&args.dir, &args.extension, &args.prefix)?;
    if files.is_empty() {
        return Err(Error::Refused(vec![format!(
            "no `{}*.{}` file in {}: nothing to merge",
            args.prefix,
            args.extension,
            args.dir.display()
        )]));
    }
    let program = match &args.llvm_profdata {
        Some(p) => p.clone(),
        None => toolchain_llvm_profdata()?,
    };
    // A shard has thousands of raw profiles: the names go in a file, not on the command line.
    let mut list = out.clone().into_os_string();
    list.push(".inputs");
    let list = PathBuf::from(list);
    let names: Vec<String> = files.iter().map(|(p, _)| p.display().to_string()).collect();
    std::fs::write(&list, names.join("\n"))
        .map_err(Error::spawn(format!("write {}", list.display())))?;
    let status = Command::new(&program)
        .args(["merge", "-sparse", "-f"])
        .arg(&list)
        .arg("-o")
        .arg(&out)
        .status()
        .map_err(Error::spawn(format!("run {}", program.display())))?;
    // Best effort: the list is scratch, and a merge that failed says so below.
    let _ = std::fs::remove_file(&list);
    if !status.success() {
        return Err(Error::Failed(format!(
            "{} merge of {} file(s) in {} failed ({status})",
            program.display(),
            files.len(),
            args.dir.display()
        )));
    }
    let out_len = std::fs::metadata(&out)
        .map_err(Error::spawn(format!("read {}", out.display())))?
        .len();
    if out_len == 0 {
        return Err(Error::Failed(format!("{} is empty", out.display())));
    }
    let in_bytes: u64 = files.iter().map(|(_, n)| n).sum();
    Ok(vec![format!(
        "merge-profiles: {} `.{}` file(s), {} -> {}, {}",
        files.len(),
        args.extension,
        mib(in_bytes),
        out.display(),
        mib(out_len)
    )])
}

#[cfg(test)]
mod tests;
