//! `mdm-host fetch`: download the pinned archives into a directory, as the operator's own user,
//! checking each against its pin.
//!
//! It refuses to run as root: it does not drop privileges, so run it as your own user (a
//! download is never root's). It also refuses a directory inside a git work tree, found after
//! every symbolic link in `--dir` is resolved, so a link into a repository does not hide it.
//! An archive already in the directory is kept when its sha256 is the pin and refused when it
//! is not (delete it and re-run). A missing one is printed as a `DRY-RUN:` line, or
//! with `--apply` downloaded, checked, and written to its name; a download whose sha256 is not
//! the pin is written to `<file>.rejected` for inspection and fails the run.

use std::io::Write;
use std::path::Path;

use host_step::{DRY_RUN_NOTE, Mode, NOTHING_TO_DO};
use mdm_core::path::AbsPath;
use mdm_core::pins::{Pin, Pins, Tool};
use sha2::{Digest, Sha256};

use crate::{Env, MdmError};

/// The largest archive downloaded, in bytes.
pub const MAX_ARCHIVE: u64 = 256 * 1024 * 1024;

/// What `fetch` was asked for.
pub struct Fetch {
    /// Where the archives go: absolute, outside every git work tree.
    pub dir: String,
    /// Only these tools; every pinned tool when empty.
    pub only: Vec<Tool>,
}

/// Check what is in the directory, then print or carry out the downloads.
pub fn run(
    env: &Env<'_>,
    f: &Fetch,
    pins: &Pins,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<(), MdmError> {
    let host = env.host;
    if host.euid() == 0 {
        return Err(MdmError::FetchAsRoot);
    }
    AbsPath::new(&f.dir)?;
    let dir = AbsPath::new(&host.canonical(&f.dir)?)?;
    outside_git(env, dir.as_str(), &f.dir)?;
    let mut todo = Vec::new();
    for pin in pins.all() {
        let wanted = f.only.is_empty() || f.only.contains(&pin.tool);
        if !wanted {
            continue;
        }
        let file = format!("{dir}/{}", pin.archive());
        match host.read(&file)? {
            Some(bytes) => {
                let got = hex::encode(Sha256::digest(&bytes));
                if got != pin.sha256_hex() {
                    return Err(MdmError::KeptDiffers {
                        file,
                        got,
                        want: pin.sha256_hex(),
                    });
                }
                let kept = format!(
                    "kept: {file} ({} {}, sha256 is the pin)",
                    pin.tool, pin.version
                );
                writeln!(out, "{kept}")?;
            }
            None => todo.push((pin, file)),
        }
    }
    if todo.is_empty() {
        writeln!(out, "{NOTHING_TO_DO}")?;
    }
    for (pin, file) in &todo {
        let plan = format!(
            "fetch: {} {} from {} (pin {})",
            pin.tool,
            pin.version,
            pin.url,
            pin.sha256_hex()
        );
        writeln!(out, "{plan}")?;
        let step = format!("download {} to {file} and check its sha256", pin.url);
        match mode {
            Mode::DryRun => writeln!(out, "DRY-RUN: {step}")?,
            Mode::Apply => {
                writeln!(out, "+ {step}")?;
                download(env, pin, &dir, file)?;
                writeln!(out, "ok: {file}")?;
            }
        }
    }
    if mode == Mode::DryRun && !todo.is_empty() {
        writeln!(out, "{DRY_RUN_NOTE}")?;
    }
    Ok(())
}

/// Refuse `dir` when it or a directory above it holds `.git`: archives and keys are never kept
/// where they could be committed.
fn outside_git(env: &Env<'_>, dir: &str, given: &str) -> Result<(), MdmError> {
    for above in Path::new(dir).ancestors() {
        let root = above.to_str().unwrap_or_default().trim_end_matches('/');
        let git = format!("{root}/.git");
        if env.host.stat(&git)?.is_some() {
            return Err(MdmError::InGitTree {
                dir: given.to_owned(),
                git,
            });
        }
    }
    Ok(())
}

/// Download `pin`, check it, and write it to `file`.
fn download(env: &Env<'_>, pin: &Pin, dir: &AbsPath, file: &str) -> Result<(), MdmError> {
    let host = env.host;
    let owner = (host.euid(), host.egid());
    let url = pin.url.to_string();
    let bytes = env
        .net
        .get(&url, MAX_ARCHIVE)
        .map_err(|source| MdmError::Download {
            url: url.clone(),
            source,
        })?;
    if host.stat(dir.as_str())?.is_none() {
        host.mkdir(dir.as_str(), 0o755, owner)?;
    }
    let got = hex::encode(Sha256::digest(&bytes));
    if got != pin.sha256_hex() {
        let kept = format!("{file}.rejected");
        host.write(&kept, 0o644, owner, &bytes)?;
        return Err(MdmError::Rejected {
            tool: pin.tool.to_string(),
            got,
            want: pin.sha256_hex(),
            kept,
        });
    }
    host.write(file, 0o644, owner, &bytes)?;
    Ok(())
}
