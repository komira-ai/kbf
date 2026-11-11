//! `channels check-apt`: the release installed the way a host installs it, from what was published
//! to the suite `testing`, before the suite `stable` changes. `.github/workflows/release.yml` runs
//! it in its `check-apt-amd64` and `check-apt-arm64` jobs, in a clean Ubuntu container on an amd64
//! and an arm64 runner, at release time only.
//!
//! `check-apt` brings the image's packages up to date (the pinned image is older than the
//! archive), adds `packages.komira.ai` with the published keyring (a binary `.gpg`, referred to by
//! a deb822 `Signed-By`) and the suite `testing`, installs `kbf=<version>`, and then holds the
//! installed system to what the package promises: dpkg lists the package at the version,
//! `kbf --version --json` names the commit and its native version (`<major>.<minor>.<seq>` of
//! what it prints) is the version installed, every file of the package ([`files`]) is there, the
//! host agent's units and account are not, no unit is enabled (by a link under `/etc` or under
//! the package's own unit directory) and no `kbf` process runs.
//!
//! The programs (`apt-get`, `dpkg`, `dpkg-query`, `curl`, the installed binary) run behind
//! [`Host`], so a test answers in place of them; `--root` is where the checked system is (`/`),
//! so a test points it at a scratch directory.

use std::path::{Path, PathBuf};
use std::process::Command;

use serde_json::Value;

use super::apt::state::PACKAGE;
use super::aws::Answer;
use super::{Error, is_plain_version};
use crate::pkg_linux::{
    AGENT_SYSUSERS_PATH, AGENT_UNITS, KBF_PATH, KBF_UNITS, NEEDRESTART_PATH, SLICE, SYSUSERS_PATH,
    TMPFILES_PATH, TRUST_DIR, TRUST_FILES, UNIT_DIR,
};

/// Where the package puts the binary (below the root).
pub const BINARY: &str = "usr/bin/kbf";

/// The files of the package below the root, with no leading slash: the binary, the units of the
/// modes and the slice, the sysusers, tmpfiles and needrestart files, and the trust files.
#[must_use]
pub fn files() -> Vec<String> {
    let below = |path: &str| path.trim_start_matches('/').to_owned();
    let mut files = vec![
        below(KBF_PATH),
        below(SYSUSERS_PATH),
        below(TMPFILES_PATH),
        below(NEEDRESTART_PATH),
    ];
    files.extend(units().map(|unit| below(&format!("{UNIT_DIR}/{unit}"))));
    files.extend(
        TRUST_FILES
            .iter()
            .map(|(name, _)| below(&format!("{TRUST_DIR}/{name}"))),
    );
    files
}

/// The units of the package: the package enables none of them.
fn units() -> impl Iterator<Item = &'static str> {
    KBF_UNITS.into_iter().chain([SLICE])
}

/// What the package of the host agent installed, and this one does not (below the root): its
/// units and its sysusers file.
fn agent_files() -> impl Iterator<Item = String> {
    AGENT_UNITS
        .iter()
        .map(|unit| format!("{}/{unit}", UNIT_DIR.trim_start_matches('/')))
        .chain([AGENT_SYSUSERS_PATH.trim_start_matches('/').to_owned()])
}

/// Where the keyring is put (below the root) and the name the source file refers to it by.
pub const KEYRING: &str = "usr/share/keyrings/komira-archive-keyring.gpg";
const KEYRING_PATH: &str = "/usr/share/keyrings/komira-archive-keyring.gpg";

/// The source file (below the root).
pub const SOURCES: &str = "etc/apt/sources.list.d/komira.sources";

/// The keyring's file name in the repository.
pub const KEYRING_NAME: &str = "komira-archive-keyring.gpg";

/// The programs outside, behind a trait so a test answers in place of them.
pub trait Host {
    /// `program <args>`. A refusal is an [`Answer`] that is not `ok`.
    ///
    /// # Errors
    /// The program could not be started.
    fn run(&self, program: &Path, args: &[String]) -> Result<Answer, Error>;
}

/// The real programs.
pub struct RealHost;

impl Host for RealHost {
    fn run(&self, program: &Path, args: &[String]) -> Result<Answer, Error> {
        let out = Command::new(program)
            .args(args)
            .env("DEBIAN_FRONTEND", "noninteractive")
            .output()
            .map_err(Error::spawn(program.display().to_string()))?;
        Ok(Answer {
            ok: out.status.success(),
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        })
    }
}

fn strings(args: &[&str]) -> Vec<String> {
    args.iter().map(|a| (*a).to_owned()).collect()
}

/// Runs `program <args>`; its standard output, or why it failed.
fn must(host: &dyn Host, program: &Path, args: &[&str]) -> Result<String, Error> {
    let answer = host.run(program, &strings(args))?;
    if answer.ok {
        return Ok(answer.stdout);
    }
    Err(Error::Failed(format!(
        "{} {} failed: {}",
        program.display(),
        args.join(" "),
        answer.stderr.trim_end()
    )))
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), Error> {
    let what = || format!("write {}", path.display());
    std::fs::create_dir_all(path.parent().unwrap_or(path)).map_err(Error::spawn(what()))?;
    std::fs::write(path, bytes).map_err(Error::spawn(what()))
}

fn version_flag(version: &str) -> Result<(), Error> {
    if is_plain_version(version) {
        Ok(())
    } else {
        Err(Error::Usage(format!(
            "--version {version:?} is not <major>.<minor>.<patch>"
        )))
    }
}

/// The version a native package of a build carries: `<major>.<minor>.<seq>` for the workspace
/// version `version` (`<major>.<minor>.<patch>`) and the build's commit count `seq`.
///
/// # Errors
/// A `version` that is not three dot-separated decimal numbers.
fn native_version(version: &str, seq: u64) -> Result<String, String> {
    kbf_package_hook::native_version(version, seq).map_err(|e| e.to_string())
}

/// The binary at `binary` says the release it is: `--version --json` names the commit (its first
/// 12 hex digits, and the whole commit when it prints it) and a `version` and `seq` whose native
/// version is `version`. What it said, for the report.
fn says(host: &dyn Host, binary: &Path, commit: &str, version: &str) -> Result<String, Error> {
    let said = must(host, binary, &["--version", "--json"])?;
    let line = said.lines().next().unwrap_or_default();
    let wrong = |why: String| Error::Failed(format!("`kbf --version --json` said {line:?}: {why}"));
    let json: Value = serde_json::from_str(line).map_err(|e| wrong(format!("not JSON ({e})")))?;
    let text = |name: &str| json.get(name).and_then(Value::as_str);
    let short = commit.get(..12).unwrap_or_default();
    match text("commit") {
        Some(said) if said == short => {}
        other => return Err(wrong(format!("its commit is {other:?}, not {short:?}"))),
    }
    if let Some(full) = text("commit_full").filter(|full| *full != commit) {
        return Err(wrong(format!(
            "its commit_full is {full:?}, not {commit:?}"
        )));
    }
    let seq = json
        .get("seq")
        .and_then(Value::as_u64)
        .ok_or_else(|| wrong("it has no numeric seq".to_owned()))?;
    let native = native_version(text("version").unwrap_or_default(), seq).map_err(wrong)?;
    if native != version {
        return Err(wrong(format!(
            "its native version is {native:?}, not {version:?}"
        )));
    }
    Ok(line.to_owned())
}

/// What `check-apt` is asked.
pub struct Apt<'a> {
    /// The system to check: `/`, or a scratch directory in a test.
    pub root: &'a Path,
    /// The Debian architecture of the machine, `amd64` or `arm64`.
    pub arch: &'a str,
    /// The release, `<major>.<minor>.<patch>`.
    pub version: &'a str,
    /// The release's full commit.
    pub commit: &'a str,
    /// The repository, `https://packages.komira.ai/apt`.
    pub apt_url: &'a str,
    /// The `apt-get` program.
    pub apt_get: &'a Path,
    /// The `dpkg-query` program.
    pub dpkg_query: &'a Path,
    /// The `dpkg` program.
    pub dpkg: &'a Path,
    /// The `curl` program.
    pub curl: &'a Path,
}

/// The deb822 source of the suite `testing`, signed by the keyring alone.
#[must_use]
pub fn source(apt_url: &str, arch: &str) -> String {
    format!(
        "Types: deb\nURIs: {apt_url}\nSuites: testing\nComponents: main\nArchitectures: {arch}\nSigned-By: {KEYRING_PATH}\n"
    )
}

/// The units of the package that are enabled, as the links and files that enable them: a unit of
/// the administrator below `etc/systemd/system`, and an entry of a `.wants` or `.requires`
/// directory below it or below the package's own unit directory.
fn enabled_links(root: &Path) -> Vec<PathBuf> {
    let admin = root.join("etc/systemd/system");
    let vendor = root.join(UNIT_DIR.trim_start_matches('/'));
    let mut found = Vec::new();
    for name in units().chain(AGENT_UNITS) {
        let unit = admin.join(name);
        if unit.symlink_metadata().is_ok() {
            found.push(unit);
        }
        for base in [&admin, &vendor] {
            for dir in std::fs::read_dir(base).into_iter().flatten().flatten() {
                let link = dir.path().join(name);
                if dir.path().is_dir() && link.symlink_metadata().is_ok() {
                    found.push(link);
                }
            }
        }
    }
    found.sort();
    found
}

/// The processes (by `comm`) named `kbf` below `root/proc`.
fn running(root: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for entry in std::fs::read_dir(root.join("proc"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let comm = std::fs::read_to_string(entry.path().join("comm")).unwrap_or_default();
        if comm.trim_end() == PACKAGE {
            found.push(entry.file_name().to_string_lossy().into_owned());
        }
    }
    found
}

/// The checks of an installed system, as problems.
fn installed_problems(r: &Apt, installed: &str) -> Vec<String> {
    let mut problems = Vec::new();
    if installed != r.version {
        problems.push(format!(
            "dpkg says {PACKAGE} {installed:?} is installed, not {:?}",
            r.version
        ));
    }
    for rel in files() {
        if !r.root.join(&rel).is_file() {
            problems.push(format!("the package's file /{rel} is not installed"));
        }
    }
    for rel in agent_files() {
        if r.root.join(&rel).symlink_metadata().is_ok() {
            problems.push(format!(
                "/{rel} is installed: the native package carries no host agent"
            ));
        }
    }
    for link in enabled_links(r.root) {
        problems.push(format!(
            "a unit is enabled: {} exists, and the package enables nothing",
            link.display()
        ));
    }
    for pid in running(r.root) {
        problems.push(format!(
            "{PACKAGE} is running (process {pid}), and the package starts nothing"
        ));
    }
    problems
}

/// The `check-apt` verb: the lines to print.
///
/// # Errors
/// A flag out of its form, a program that fails (naming it and what it said), a keyring that is
/// not a binary keyring, a machine of another architecture, a binary that does not say the
/// release, or one problem per way the installed system is not what the package promises.
pub fn check_apt(host: &dyn Host, r: &Apt) -> Result<Vec<String>, Error> {
    version_flag(r.version)?;
    if !super::apt::state::is_commit(r.commit) {
        return Err(Error::Usage(format!(
            "--commit {:?} is not a full commit (40 lower case hex digits)",
            r.commit
        )));
    }
    if !["amd64", "arm64"].contains(&r.arch) {
        return Err(Error::Usage(format!(
            "--arch {:?} is not amd64 or arm64",
            r.arch
        )));
    }
    if !r.apt_url.starts_with("https://") || r.apt_url.ends_with('/') {
        return Err(Error::Usage(format!(
            "--apt-url {:?} is not an https URL without a trailing /",
            r.apt_url
        )));
    }
    let machine = must(host, r.dpkg, &["--print-architecture"])?;
    if machine.trim() != r.arch {
        return Err(Error::Failed(format!(
            "this machine is {}, not {}: the check would install another package",
            machine.trim(),
            r.arch
        )));
    }
    // A clean image has no certificates: the first update goes over the mirrors' plain HTTP.
    // The image is pinned by digest, so its packages are as old as the pin: the archive has moved
    // on, and `ca-certificates` and `curl` may need newer versions of packages the image has
    // ("you have held broken packages"). So the installed packages are brought up to date before
    // anything is installed. `dist-upgrade` may install or remove packages to do so, which
    // `upgrade` never does; `--force-confold` keeps the image's configuration files, as there
    // is nobody to answer dpkg's question.
    must(host, r.apt_get, &["update"])?;
    must(
        host,
        r.apt_get,
        &[
            "dist-upgrade",
            "-y",
            "-o",
            "Dpkg::Options::=--force-confold",
        ],
    )?;
    must(
        host,
        r.apt_get,
        &[
            "install",
            "-y",
            "--no-install-recommends",
            "ca-certificates",
            "curl",
        ],
    )?;
    let keyring = r.root.join(KEYRING);
    let keyring_arg = keyring.display().to_string();
    must(
        host,
        r.curl,
        &[
            "--proto",
            "=https",
            "--tlsv1.2",
            "--fail",
            "--silent",
            "--show-error",
            "--create-dirs",
            "--output",
            &keyring_arg,
            &format!("{}/{KEYRING_NAME}", r.apt_url),
        ],
    )?;
    let bytes =
        std::fs::read(&keyring).map_err(Error::spawn(format!("read {}", keyring.display())))?;
    // A binary OpenPGP keyring starts with a packet header (bit 7 set); an armored one is text
    // that a deb822 `Signed-By` path does not accept.
    if bytes.first().is_none_or(|b| b & 0x80 == 0) {
        return Err(Error::Failed(format!(
            "{KEYRING_NAME} is not a binary keyring: apt takes a .gpg file in Signed-By, not armor"
        )));
    }
    write(&r.root.join(SOURCES), source(r.apt_url, r.arch).as_bytes())?;
    must(host, r.apt_get, &["update"])?;
    let target = format!("{PACKAGE}={}", r.version);
    must(
        host,
        r.apt_get,
        &["install", "-y", "--no-install-recommends", &target],
    )?;
    let installed = must(
        host,
        r.dpkg_query,
        &["--show", "--showformat=${Version}", PACKAGE],
    )?;
    let binary = r.root.join(BINARY);
    let said = says(host, &binary, r.commit, r.version)?;
    let problems = installed_problems(r, &installed);
    if !problems.is_empty() {
        return Err(Error::Refused(problems));
    }
    Ok(vec![format!(
        "check-apt: {PACKAGE} {} installed from the suite testing on {}; it says {said:?}; its {} files are there; the host agent is not; no unit is enabled and nothing runs",
        r.version,
        r.arch,
        files().len()
    )])
}

#[cfg(test)]
mod tests;
