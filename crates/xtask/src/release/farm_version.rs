//! `release farm-version`: the version text the farm's update coordinator is told to
//! converge on, derived from the release's own version facts.
//!
//! A host reports `<semver>+g<12 hex digits of the commit>` in its heartbeat (for example
//! `0.1.0+g0123456789ab`), the coordinator decides the farm has converged by comparing that
//! text with the desired version, and a host's installer finds the commit in the text it is
//! told to install. The release tag (`0.0.13`) is neither: it is the number of the release,
//! and a build says its own version and commit. So the desired version is read from what the
//! build said of itself, the release asset `kbf-<commit>-linux-x86_64.version.json`
//! ([`crate::version_facts`]): `<its version>+g<its commit>`.
//!
//! `release farm-version --repo <owner/repo> --tag <tag> --commit <sha> [--output <file>]`:
//!
//! 1. `gh release download <tag>` fetches `SHA256SUMS` and the facts of `--commit` into a
//!    directory of this process alone (removed again);
//! 2. the facts must be the file `SHA256SUMS` lists, byte for byte;
//! 3. they must be what the release build of `--commit` printed (the parser and check the
//!    installer holds a staged binary to: an unreadable answer, another commit, a debug
//!    build), and their full commit, when they state one, is `--commit`;
//! 4. the text made must be the form `deploy-farm` accepts
//!    ([`crate::deploy_farm::check_version`]).
//!
//! Only then it prints `version=<text>` and appends that line to `--output` (on Actions,
//! `GITHUB_OUTPUT`). Any refusal prints nothing and writes nothing. It runs no downloaded
//! file.

use std::path::{Path, PathBuf};

use kbf_installer::binary::{VERSION_CAP, Version};

use super::{append_output, gh_run};
use crate::Error;
use crate::build_commit::full_commit;
use crate::deploy_farm::check_version;
use crate::fetch::Sha256;
use crate::package::Platform;
use crate::{sums, version_facts};

/// The platform whose facts are read: the one the release job that runs this can always name,
/// and the same version and commit as the others' (one commit, one workspace version).
const PLATFORM: Platform = Platform::LinuxX86_64;

/// The arguments of `release farm-version`.
#[derive(clap::Args)]
pub struct Args {
    /// The repository, `<owner>/<repo>` (on Actions, `GITHUB_REPOSITORY`).
    #[arg(long)]
    pub repo: String,
    /// The release's tag.
    #[arg(long)]
    pub tag: String,
    /// The commit the release was built from, a full SHA.
    #[arg(long)]
    pub commit: String,
    /// Append `version=<text>` to this file (on Actions, `GITHUB_OUTPUT`).
    #[arg(long)]
    pub output: Option<PathBuf>,
    /// The `gh` program.
    #[arg(long, default_value = "gh")]
    pub gh: PathBuf,
}

/// The version text of the build whose facts are `bytes`, which `sums_text` must list for
/// `name`, and which must be the release build of `commit`.
///
/// # Errors
/// [`Error::Failed`] when the sums do not list these bytes, the facts are not that build's,
/// or the text made is not the form `deploy-farm` takes.
pub fn derive(bytes: &[u8], name: &str, sums_text: &str, commit: &str) -> Result<String, Error> {
    let lines = sums::parse(sums_text)
        .map_err(|p| Error::Failed(format!("SHA256SUMS: {}", p.join("; "))))?;
    let listed: Vec<&sums::Line> = lines.iter().filter(|l| l.name == name).collect();
    if listed.len() != 1 || listed[0].sha256 != Sha256::of(bytes) {
        return Err(Error::Failed(format!(
            "{name} is not the file SHA256SUMS lists (listed {} time(s))",
            listed.len()
        )));
    }
    let said = Version::parse(bytes).map_err(|e| Error::Failed(format!("{name}: {e}")))?;
    said.check(commit)
        .map_err(|e| Error::Failed(format!("{name} is not the release build of {commit}: {e}")))?;
    if let Some(full) = &said.commit_full
        && full != commit
    {
        return Err(Error::Failed(format!(
            "{name} says the full commit is {full}, not {commit}"
        )));
    }
    let text = format!("{}+g{}", said.version, said.commit);
    check_version(&text).map_err(|e| Error::Failed(e.to_string()))?;
    Ok(text)
}

/// `path` as text, refused over `cap` bytes.
fn read_capped(path: &Path, cap: usize) -> Result<Vec<u8>, Error> {
    let shown = path.display();
    let length = std::fs::metadata(path)
        .map_err(Error::spawn(format!("read {shown}")))?
        .len();
    if length > cap as u64 {
        return Err(Error::Failed(format!("{shown} is over {cap} bytes")));
    }
    std::fs::read(path).map_err(Error::spawn(format!("read {shown}")))
}

/// The `release farm-version` verb: the lines to print.
pub fn run(a: &Args) -> Result<Vec<String>, Error> {
    full_commit("--commit", &a.commit)?;
    let name = version_facts::name(&a.commit, PLATFORM);
    let dir = std::env::temp_dir().join(format!("xtask-farm-version-{}", std::process::id()));
    std::fs::create_dir(&dir).map_err(Error::spawn(format!("create {}", dir.display())))?;
    let said = download_and_derive(a, &name, &dir);
    let _ = std::fs::remove_dir_all(&dir);
    let lines = vec![format!("version={}", said?)];
    if let Some(output) = &a.output {
        append_output(output, &lines)?;
    }
    Ok(lines)
}

fn download_and_derive(a: &Args, name: &str, dir: &Path) -> Result<String, Error> {
    let dir_text = dir.to_string_lossy();
    gh_run(
        &a.gh,
        &[
            "release",
            "download",
            &a.tag,
            "--repo",
            &a.repo,
            "--dir",
            &dir_text,
            "--pattern",
            sums::SUMS,
            "--pattern",
            name,
        ],
    )?;
    let facts = read_capped(&dir.join(name), VERSION_CAP)?;
    let listed = read_capped(&dir.join(sums::SUMS), 1 << 20)?;
    let listed = String::from_utf8(listed)
        .map_err(|_| Error::Failed("SHA256SUMS is not UTF-8".to_owned()))?;
    derive(&facts, name, &listed, &a.commit)
}
