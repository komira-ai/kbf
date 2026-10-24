//! The two package formats of a Linux daemon host: the commands that ask the package
//! manager what it has, what a package file is, and put a package file in place, and the
//! readers of what they print. Pure: nothing here runs anything.
//!
//! A release names a package by one version text, the deb's: `0.1.<seq>+g<12 hex>-<run>`. An
//! rpm of the same build splits it into a Version and a Release (`0.1.<seq>` and
//! `<run>.g<12 hex>`, `xtask`'s `pkg_version`); [`deb_spelling`] puts the two back together so
//! that both formats are compared with the version the update names, and with each other, in
//! one spelling.
//!
//! No command here resolves dependencies over the network or reads a repository: `dpkg
//! --install` and `rpm --upgrade` work on the one file they are given. A rollback is the
//! same command on an older file (`dpkg` downgrades by itself; `rpm` needs `--oldpackage`).

use std::path::Path;
use std::time::Duration;

use kbf_installer::binary::Arch;

use crate::mac::fetch::AssetKind;
use crate::mac::release::PREFIX_LEN;
use crate::runner::{Cmd, Exec};

/// The package every release carries, in both formats.
pub const PACKAGE: &str = "kbf";

/// `dpkg`.
pub const DPKG: &str = "/usr/bin/dpkg";

/// `dpkg-deb`.
pub const DPKG_DEB: &str = "/usr/bin/dpkg-deb";

/// `dpkg-query`.
pub const DPKG_QUERY: &str = "/usr/bin/dpkg-query";

/// `rpm`.
pub const RPM: &str = "/usr/bin/rpm";

/// How long one look at a package or at the package database may take.
const LOOK: Duration = Duration::from_secs(60);

/// How long an installation of a package file may take.
const INSTALL: Duration = Duration::from_secs(900);

/// The format of the packages a host takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Deb,
    Rpm,
}

/// What a package file says about itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Meta {
    pub name: String,
    /// In the deb's spelling, whichever format the file is.
    pub version: String,
    pub arch: String,
}

fn cmd(program: &str, args: &[&str], timeout: Duration) -> Cmd {
    Cmd {
        program: program.to_owned(),
        args: args.iter().map(|a| (*a).to_owned()).collect(),
        timeout: Some(timeout),
        ..Cmd::default()
    }
}

fn with_file(mut cmd: Cmd, file: &Path) -> Cmd {
    cmd.args.push(file.display().to_string());
    cmd
}

impl Format {
    /// Which release file this format takes.
    pub fn asset(self) -> AssetKind {
        match self {
            Self::Deb => AssetKind::Deb,
            Self::Rpm => AssetKind::Rpm,
        }
    }

    /// What the format calls the CPU `arch`.
    pub fn arch_name(self, arch: Arch) -> &'static str {
        match (self, arch) {
            (Self::Deb, Arch::X86_64) => "amd64",
            (Self::Deb, Arch::Aarch64) => "arm64",
            (Self::Rpm, Arch::X86_64) => "x86_64",
            (Self::Rpm, Arch::Aarch64) => "aarch64",
        }
    }

    /// Ask the package database for the installed `kbf`.
    pub fn installed(self) -> Cmd {
        match self {
            Self::Deb => cmd(
                DPKG_QUERY,
                &[
                    "--show",
                    "--showformat=${db:Status-Abbrev} ${Version}\\n",
                    PACKAGE,
                ],
                LOOK,
            ),
            Self::Rpm => cmd(
                RPM,
                &["--query", "--queryformat=%{VERSION} %{RELEASE}\\n", PACKAGE],
                LOOK,
            ),
        }
    }

    /// The installed version (in the deb's spelling) that the answer of [`Format::installed`]
    /// names; none when the package is not installed.
    ///
    /// # Errors
    /// The package manager failed, or printed something unexpected.
    pub fn read_installed(self, exec: &Exec) -> Result<Option<String>, String> {
        if exec.code == Some(1) {
            // Both tools answer 1 for a package they have no record of.
            return Ok(None);
        }
        if !exec.success() {
            return Err(format!(
                "exit status {:?}: {}",
                exec.code,
                exec.stderr_text().trim()
            ));
        }
        let text = exec.stdout_text();
        let mut words = text.split_whitespace();
        match self {
            Self::Deb => {
                // The status abbreviation: wanted, then the current state (`i` installed,
                // `n` not, `c` configuration files left), then an error flag.
                let status = words.next().ok_or("nothing was printed")?;
                if matches!(status.chars().nth(1), Some('n' | 'c')) {
                    return Ok(None);
                }
                let version = words.next().ok_or("no version was printed")?;
                Ok(Some(version.to_owned()))
            }
            Self::Rpm => {
                let (version, release) = words
                    .next()
                    .zip(words.next())
                    .ok_or("no version and release were printed")?;
                deb_spelling(version, release)
                    .map(Some)
                    .ok_or_else(|| format!("the release {release:?} names no build"))
            }
        }
    }

    /// Ask what the package file `file` is.
    pub fn inspect(self, file: &Path) -> Cmd {
        match self {
            Self::Deb => cmd(
                DPKG_DEB,
                &[
                    "--field",
                    &file.display().to_string(),
                    "Package",
                    "Version",
                    "Architecture",
                ],
                LOOK,
            ),
            Self::Rpm => with_file(
                cmd(
                    RPM,
                    &[
                        "--query",
                        "--package",
                        "--queryformat=Package: %{NAME}\nVersion: %{VERSION}\nRelease: %{RELEASE}\nArchitecture: %{ARCH}\n",
                    ],
                    LOOK,
                ),
                file,
            ),
        }
    }

    /// What [`Format::inspect`] printed, as a [`Meta`].
    ///
    /// # Errors
    /// A field is missing or twice, or an rpm release names no build.
    pub fn read_meta(self, text: &str) -> Result<Meta, String> {
        let field = |name: &str| -> Result<String, String> {
            let mut found = text.lines().filter_map(|l| {
                l.strip_prefix(name)
                    .and_then(|rest| rest.strip_prefix(": "))
                    .map(str::trim)
            });
            match (found.next(), found.next()) {
                (Some(value), None) if !value.is_empty() => Ok(value.to_owned()),
                _ => Err(format!("the package has no single {name}")),
            }
        };
        let version = match self {
            Self::Deb => field("Version")?,
            Self::Rpm => {
                let release = field("Release")?;
                deb_spelling(&field("Version")?, &release)
                    .ok_or_else(|| format!("the release {release:?} names no build"))?
            }
        };
        Ok(Meta {
            name: field("Package")?,
            version,
            arch: field("Architecture")?,
        })
    }

    /// Put the package file `file` in place: the file alone, no repository and no
    /// dependency resolution, whatever version is there now (an older one included).
    pub fn install(self, file: &Path) -> Cmd {
        with_file(
            match self {
                Self::Deb => cmd(DPKG, &["--force-confold", "--install", "--"], INSTALL),
                Self::Rpm => cmd(
                    RPM,
                    &["--upgrade", "--oldpackage", "--replacepkgs", "--"],
                    INSTALL,
                ),
            },
            file,
        )
    }
}

/// The deb's spelling of an rpm's Version and Release: `0.1.7` and `3.gabcdef012345` are
/// `0.1.7+gabcdef012345-3`. None when the release is not `<run>.g<12 hex>`.
pub fn deb_spelling(version: &str, release: &str) -> Option<String> {
    let (run, sha) = release.split_once(".g")?;
    let run_ok = !run.is_empty() && run.bytes().all(|b| b.is_ascii_digit());
    let sha_ok =
        sha.len() == PREFIX_LEN && sha.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    (run_ok && sha_ok && !version.is_empty()).then(|| format!("{version}+g{sha}-{run}"))
}

/// Why `SHA256SUMS` did not give a file's digest.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SumsError {
    /// A line is not `<64 hex>  <name>`, or a name is listed twice.
    Malformed,
    /// The file is not listed.
    NotListed,
}

/// The sha256 that `sums` (the text of a `SHA256SUMS`) lists for the file `name`.
///
/// # Errors
/// [`SumsError`].
pub fn sums_entry(sums: &str, name: &str) -> Result<String, SumsError> {
    if !sums.is_empty() && !sums.ends_with('\n') {
        return Err(SumsError::Malformed);
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut found = None;
    for line in sums.lines() {
        let (hash, listed) = line.split_once("  ").ok_or(SumsError::Malformed)?;
        let hash_ok =
            hash.len() == 64 && hash.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if !hash_ok || listed.is_empty() || listed.contains('/') || !seen.insert(listed) {
            return Err(SumsError::Malformed);
        }
        if listed == name {
            found = Some(hash.to_owned());
        }
    }
    found.ok_or(SumsError::NotListed)
}

#[cfg(test)]
#[path = "package_format_tests.rs"]
mod tests;
