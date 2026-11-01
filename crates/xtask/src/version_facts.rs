//! `release version-facts`: what `kbf --version --json` says of one platform's build, kept
//! as the release asset `kbf-<commit>-<platform>.version.json` (docs/artifacts.md).
//!
//! The age check of a release need facts of each platform's `kbf` (its
//! `meta_log`, the protocols, `seq`). The binary states them and nothing on the frontend
//! records them, and only a runner of a platform can run its binary: a Linux runner cannot
//! run the macOS `kbf`. So each build job runs the `kbf` it has just built and keeps the
//! bytes it printed.
//!
//! `release version-facts --binary <kbf> --commit <sha> --platform <p> --out <dir>`
//! ([`emit`]) runs `<kbf> --version --json` as the installer runs a staged binary
//! ([`kbf_installer::run::SystemRunner`]: an empty environment, no standard input, a time
//! limit, the process group killed with it), reads what it printed with the installer's own
//! parser ([`kbf_installer::binary::Version`]: at most
//! [`kbf_installer::binary::VERSION_CAP`] bytes, exactly the fields a build reports), and
//! refuses unless that is the release build of `--commit` (a non-zero exit, a timeout, an
//! answer the parser refuses, another commit or a debug build). Only then does it write,
//! in `<dir>`:
//!
//! - [`name`]`(commit, platform)`: the bytes printed, unchanged. It is the stem of the
//!   `kbf` tarball `package` writes for the same commit and platform
//!   ([`crate::package::asset_stem`]), and [`SUFFIX`];
//!
//! and it appends that file's line to `<dir>/SHA256SUMS.<platform>` ([`crate::sums`]), as
//! `package` does for the files it writes. Neither the file nor its line may exist yet.
//! A refusal writes nothing, not even `<dir>`.
//!
//! The release set holds the file because [`facts_of`] names it: `verify-assets sums`
//! (and so `release check` and `release publish`) refuses a listed `kbf`
//! tarball whose facts are not listed (no flag waives it) and a listed facts file whose tarball is not ([`tarball_of`]),
//! and `verify-assets
//! attestations` checks the facts for build provenance, as an SBOM is (they have no
//! bundle).

use std::path::PathBuf;
use std::time::Duration;

use kbf_installer::CommitId;
use kbf_installer::binary::Version;
use kbf_installer::refusal::VersionFault;
use kbf_installer::run::{Ran, Runner, Spawn, SystemRunner};

use crate::fetch::Sha256;
use crate::package::{self, Platform};
use crate::{Error, build_commit, sums};

/// The binary whose facts are kept: the one a node runs.
pub const BINARY: &str = "kbf";

/// What the facts file's name adds to the stem of the tarball's.
pub const SUFFIX: &str = ".version.json";

/// How long the binary has to print its facts (the installer allows a probe the same).
pub const TIMEOUT: Duration = Duration::from_secs(10);

/// The arguments of `release version-facts`.
#[derive(clap::Args)]
pub struct Args {
    /// The `kbf` just built (on macOS, signed): it is run with `--version --json`.
    #[arg(long)]
    pub binary: PathBuf,
    /// The commit it was built from, a full SHA (on Actions, `GITHUB_SHA`).
    #[arg(long)]
    pub commit: String,
    /// The platform it was built for.
    #[arg(long, value_enum)]
    pub platform: Platform,
    /// The directory the facts and their line of `SHA256SUMS.<platform>` go in; created
    /// when missing.
    #[arg(long)]
    pub out: PathBuf,
}

/// The name of the facts file of the `kbf` built from `commit` for `platform`: the stem of
/// its tarball's name, and [`SUFFIX`].
pub fn name(commit: &str, platform: Platform) -> String {
    let stem = package::asset_stem(BINARY, commit, platform);
    format!("{stem}{SUFFIX}")
}

/// The facts file that goes with `asset`, when `asset` is a `kbf` tarball
/// (`kbf-<commit>-<platform>.tar.gz`; the tarballs of `kbf-daemon` and the others start the
/// same way and have none).
pub fn facts_of(asset: &str) -> Option<String> {
    let stem = asset.strip_suffix(".tar.gz")?;
    let rest = stem.strip_prefix(BINARY)?.strip_prefix('-')?;
    let (commit, platform) = rest.split_once('-')?;
    let is_kbf = build_commit::is_commit(commit) && Platform::named(platform).is_some();
    is_kbf.then(|| format!("{stem}{SUFFIX}"))
}

/// The `kbf` tarball `file` states the facts of, when `file` is a facts file
/// (`kbf-<commit>-<platform>.version.json`): the inverse of [`facts_of`].
pub fn tarball_of(file: &str) -> Option<String> {
    let stem = file.strip_suffix(SUFFIX)?;
    let tarball = format!("{stem}.tar.gz");
    facts_of(&tarball).map(|_| tarball)
}

/// Whether `file` is the facts file of the `kbf` built from `commit` for one of the
/// platforms: exactly [`name`], not any file that ends in [`SUFFIX`].
pub fn is_facts_of(file: &str, commit: &str) -> bool {
    for platform in Platform::ALL {
        if file == name(commit, platform) {
            return true;
        }
    }
    false
}

/// Why what `kbf --version --json` printed is not the facts of the release build of
/// `commit`: it did not end with status 0, or the installer's parser refuses it, or it is
/// another commit's or a debug build.
fn judged(ran: &Ran, commit: &CommitId) -> Result<(), VersionFault> {
    if ran.code != Some(0) {
        return Err(VersionFault::Failed(ran.code));
    }
    Version::parse(&ran.stdout)?.check(commit)
}

/// The `release version-facts` verb with `timeout` for the binary: the lines to print.
pub fn emit(a: &Args, timeout: Duration) -> Result<Vec<String>, Error> {
    let Ok(commit) = CommitId::new(&a.commit) else {
        return Err(build_commit::not_full_commit("--commit", &a.commit));
    };
    let binary = a.binary.display();
    let found = std::fs::canonicalize(&a.binary);
    let program = found.map_err(Error::spawn(format!("find {binary}")))?;
    let spawn = Spawn {
        program,
        args: vec!["--version".into(), "--json".into()],
        // Nothing of the runner's environment (a token, say) reaches the binary.
        env: Vec::new(),
        cwd: PathBuf::from("/"),
        run_as: None,
        timeout,
    };
    let started = SystemRunner.run(&spawn);
    let ran = started.map_err(Error::spawn(format!("run {binary}")))?;
    if ran.timed_out {
        return Err(Error::Failed(format!(
            "{binary} --version --json did not answer in {timeout:?}"
        )));
    }
    if let Err(fault) = judged(&ran, &commit) {
        return Err(Error::Failed(format!(
            "{binary} --version --json said: {fault}"
        )));
    }
    let file = name(&a.commit, a.platform);
    let (sums_path, mut lines) = package::open_sums(&a.out, a.platform)?;
    if lines.iter().any(|l| l.name == file) {
        return Err(Error::Failed(format!(
            "{} already lists {file}",
            sums_path.display()
        )));
    }
    package::write_new(&a.out.join(&file), &ran.stdout)?;
    let sha256 = Sha256::of(&ran.stdout);
    lines.push(sums::Line {
        sha256,
        name: file.clone(),
    });
    package::write_sums(&sums_path, &lines)?;
    let recorded = format!("recorded {file}:");
    Ok(vec![recorded, format!("{sha256}  {file}")])
}

/// The `release version-facts` verb: the lines to print.
pub fn run(a: &Args) -> Result<Vec<String>, Error> {
    emit(a, TIMEOUT)
}

#[cfg(test)]
mod tests;
