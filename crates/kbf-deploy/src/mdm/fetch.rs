//! `mdm fetch`: download the pinned archives into a directory, as the operator's own
//! user, checking each against its pin.
//!
//! It runs as a user, never as root (a download is never root's), and refuses a
//! directory inside a git work tree. An archive already in the directory is kept when
//! its sha256 is the pin and refused when it is not (delete it and re-run). A missing
//! one is printed as a `DRY-RUN:` line, or with `--apply` downloaded to `<file>.part`
//! over HTTPS (redirects followed, to `https://` only), checked, and renamed to its
//! name; a download whose sha256 is not the pin is renamed to `<file>.rejected` for
//! inspection and fails the run. Nothing downloaded is run: `mdm install` checks each
//! archive against its pin again, as root, and unpacks it itself.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::MdmError;
use super::pins::{Pin, Pins, Tool};
use crate::https::Client;
use kbf_host::plan::{DRY_RUN_NOTE, Mode, NOTHING_TO_DO};

/// What `mdm fetch` was asked for.
pub struct Fetch {
    /// Where the archives go: absolute, outside every git work tree.
    pub dir: PathBuf,
    /// Only these tools; every pinned tool when empty.
    pub only: Vec<Tool>,
}

/// Check what is in the directory, then print or carry out the downloads.
pub fn run(
    f: &Fetch,
    pins: &Pins,
    euid: u32,
    client: &Client,
    mode: Mode,
    out: &mut dyn Write,
) -> Result<(), MdmError> {
    if euid == 0 {
        return Err(MdmError::FetchAsRoot);
    }
    if !f.dir.is_absolute() {
        return Err(MdmError::Relative(f.dir.clone()));
    }
    outside_git(&f.dir)?;
    let mut todo = Vec::new();
    for pin in pins.all() {
        if !f.only.is_empty() && !f.only.contains(&pin.tool) {
            continue;
        }
        let file = f.dir.join(pin.archive());
        match fs::read(&file) {
            Ok(bytes) => {
                let got = hex::encode(Sha256::digest(&bytes));
                if got != pin.sha256_hex() {
                    return Err(MdmError::KeptDiffers {
                        file,
                        got,
                        want: pin.sha256_hex(),
                    });
                }
                say(
                    out,
                    &format!(
                        "kept: {} ({} {}, sha256 is the pin)",
                        file.display(),
                        pin.tool,
                        pin.version
                    ),
                )?;
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => todo.push((pin, file)),
            Err(source) => return Err(MdmError::Io { path: file, source }),
        }
    }
    if todo.is_empty() {
        return say(out, NOTHING_TO_DO);
    }
    for (pin, file) in todo {
        say(
            out,
            &format!(
                "fetch: {} {} from {} (pin {})",
                pin.tool,
                pin.version,
                pin.url,
                pin.sha256_hex()
            ),
        )?;
        let step = format!(
            "download {} to {}.part, check its sha256, rename it to {}",
            pin.url,
            file.display(),
            file.display()
        );
        match mode {
            Mode::DryRun => say(out, &format!("DRY-RUN: {step}"))?,
            Mode::Apply => {
                say(out, &format!("+ {step}"))?;
                download(client, pin, &f.dir, &file)?;
                say(out, &format!("ok: {}", file.display()))?;
            }
        }
    }
    if mode == Mode::DryRun {
        say(out, DRY_RUN_NOTE)?;
    }
    Ok(())
}

fn say(out: &mut dyn Write, line: &str) -> Result<(), MdmError> {
    writeln!(out, "{line}").map_err(MdmError::Write)
}

/// Refuse `dir` when it or a directory above it holds `.git`: archives and keys are
/// never kept where they could be committed.
fn outside_git(dir: &Path) -> Result<(), MdmError> {
    for above in dir.ancestors() {
        let git = above.join(".git");
        if fs::symlink_metadata(&git).is_ok() {
            return Err(MdmError::InGitTree {
                dir: dir.to_owned(),
                git,
            });
        }
    }
    Ok(())
}

/// Download `pin` to `<file>.part`, check it, and rename it to `file`.
fn download(client: &Client, pin: &Pin, dir: &Path, file: &Path) -> Result<(), MdmError> {
    let io_at = |path: &Path| {
        let path = path.to_owned();
        move |source| MdmError::Io { path, source }
    };
    fs::create_dir_all(dir).map_err(io_at(dir))?;
    let part = with_suffix(file, ".part");
    remove_if_there(&part).map_err(io_at(&part))?;
    let target = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&part)
        .map_err(io_at(&part))?;
    let mut w = Hashing {
        inner: target,
        hash: Sha256::new(),
    };
    let answer = client.follow(&pin.url, &mut w);
    let status = match answer {
        Ok(a) => a.status,
        Err(e) => {
            remove_if_there(&part).map_err(io_at(&part))?;
            return Err(e.into());
        }
    };
    if !(200..300).contains(&status) {
        remove_if_there(&part).map_err(io_at(&part))?;
        return Err(MdmError::Status {
            url: pin.url.to_string(),
            status,
        });
    }
    w.flush().map_err(io_at(&part))?;
    let got = hex::encode(w.hash.finalize());
    w.inner.sync_all().map_err(io_at(&part))?;
    if got != pin.sha256_hex() {
        let kept = with_suffix(file, ".rejected");
        fs::rename(&part, &kept).map_err(io_at(&kept))?;
        return Err(MdmError::Rejected {
            tool: pin.tool,
            got,
            want: pin.sha256_hex(),
            kept,
        });
    }
    fs::rename(&part, file).map_err(io_at(file))
}

fn with_suffix(file: &Path, suffix: &str) -> PathBuf {
    let mut name = file.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

/// Remove the file at `path`, if there is one (a part left by an interrupted run).
fn remove_if_there(path: &Path) -> io::Result<()> {
    match fs::remove_file(path) {
        Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

/// A writer that hashes what it writes.
struct Hashing<W: Write> {
    inner: W,
    hash: Sha256,
}

impl<W: Write> Write for Hashing<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hash.update(&buf[..n]);
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
#[path = "fetch_tests.rs"]
mod tests;
