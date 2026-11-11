//! `debs --pins <file> --dir <dir>`: the Debian packages a CI job installs with `dpkg`, each
//! pinned by its exact version and SHA-256 in a file of `.github/debs/`, so that a normal run
//! needs no package archive and no `apt-get update`.
//!
//! The pins file has a table per Debian architecture (`amd64`, `arm64`), each with `mirrors`
//! (HTTPS base URLs of one archive, tried in their order) and `debs` (per package its `package`,
//! `version`, `path` below a mirror, and `sha256`). The verb takes the table of the host's
//! architecture and the directory the actions cache restores and saves, and:
//!
//! 1. refuses the directory when it holds a file no pin names, so that
//!    `dpkg --install --recursive <dir>` installs the pinned packages and nothing else;
//! 2. for each pin, in the file's order, keeps the file of that name in the directory when it
//!    is there and its SHA-256 is the pinned one, and refuses it, without replacing it, when its
//!    SHA-256 is another;
//! 3. when it is not there, downloads it from each mirror in turn, in [`WAITS`]`.len()` rounds
//!    with the pause of [`WAITS`] before each round, each attempt given up after
//!    [`ATTEMPT_TIMEOUT`], and keeps the first download whose SHA-256 is the pinned one (written
//!    through a `.part` file and renamed, mode 0644). A download of other bytes is one more
//!    failed attempt. When every attempt failed, the error names each one, in order.
//!
//! So a job whose packages the actions cache restored makes no network call, and a job whose
//! cache missed survives one mirror being unreachable; either way only the pinned bytes reach
//! the directory.

use std::path::{Path, PathBuf};
use std::time::Duration;

use rustls::pki_types::CertificateDer;
use serde::Deserialize;

use crate::fetch::{self, Sha256};

/// The pause before each round of downloads: none before the first.
pub const WAITS: [Duration; 3] = [
    Duration::ZERO,
    Duration::from_secs(10),
    Duration::from_secs(30),
];

/// How long one download may take.
pub const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(120);

/// The flags of `cargo xtask debs`.
#[derive(clap::Args)]
pub struct Args {
    /// The pins file (`.github/debs/<set>.toml`).
    #[arg(long)]
    pub pins: PathBuf,
    /// The directory the actions cache restores and saves; created when missing.
    #[arg(long)]
    pub dir: PathBuf,
}

/// A Debian architecture the pins files have a table for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arch {
    /// `amd64` (Rust's `x86_64`).
    Amd64,
    /// `arm64` (Rust's `aarch64`).
    Arm64,
}

impl Arch {
    /// The Debian architecture of Rust's `std::env::consts::ARCH` value `arch`.
    pub fn of(arch: &str) -> Result<Arch, Error> {
        match arch {
            "x86_64" => Ok(Arch::Amd64),
            "aarch64" => Ok(Arch::Arm64),
            other => Err(Error::Arch(other.to_owned())),
        }
    }

    /// The architecture of the host this program was built for.
    pub fn host() -> Result<Arch, Error> {
        Arch::of(std::env::consts::ARCH)
    }

    /// Its name: the table of a pins file, and the last part of a package's file name.
    pub fn name(self) -> &'static str {
        match self {
            Arch::Amd64 => "amd64",
            Arch::Arm64 => "arm64",
        }
    }
}

/// Why `debs` failed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    /// The host's architecture has no Debian name here.
    #[error("this host's architecture {0:?} is neither x86_64 nor aarch64")]
    Arch(String),
    /// The pins file could not be read, or does not keep its rules.
    #[error("the pins file {path:?}: {why}")]
    Pins {
        /// The file.
        path: PathBuf,
        /// What is wrong.
        why: String,
    },
    /// The directory, or a file in it, could not be read or written.
    #[error("{path:?}: {why}; nothing was installed")]
    Dir {
        /// The directory or the file.
        path: PathBuf,
        /// The system's reason.
        why: String,
    },
    /// The directory holds a file no pin names.
    #[error("{dir:?} holds {name:?}, which no pin names; nothing was installed")]
    Stray {
        /// The directory.
        dir: PathBuf,
        /// The file's name.
        name: String,
    },
    /// A file in the directory is not the pinned file.
    #[error(
        "the cached {path:?} has SHA-256 {actual}, not the pinned {expected}; nothing was installed"
    )]
    CachedMismatch {
        /// The file.
        path: PathBuf,
        /// The pinned digest.
        expected: Sha256,
        /// The digest of the file.
        actual: Sha256,
    },
    /// No mirror gave the pinned file.
    #[error("{name}: no download was the pinned file: {}; nothing was installed", attempts.join("; "))]
    Unavailable {
        /// The package's file name.
        name: String,
        /// Each attempt, in order: its URL and why it failed.
        attempts: Vec<String>,
    },
}

/// The pins file as written: a table per architecture.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    amd64: Option<Table>,
    arm64: Option<Table>,
}

/// The pins of one architecture, as written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Table {
    mirrors: Vec<String>,
    debs: Vec<Written>,
}

/// One pin, as written.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Written {
    package: String,
    version: String,
    path: String,
    sha256: String,
}

/// One pinned package.
#[derive(Debug, PartialEq, Eq)]
pub struct Pin {
    /// Its file name: `<package>_<version without epoch>_<arch or all>.deb`.
    pub name: String,
    /// Its path below a mirror (`pool/...`).
    pub path: String,
    /// Its SHA-256.
    pub sha256: Sha256,
}

/// The pins of one architecture.
#[derive(Debug, PartialEq, Eq)]
pub struct Pins {
    /// The base URLs to download from, in order.
    pub mirrors: Vec<String>,
    /// The packages, in order.
    pub debs: Vec<Pin>,
}

/// The pins of `arch` in the pins file `text`, or what is wrong with it.
pub fn parse(text: &str, arch: Arch) -> Result<Pins, String> {
    let file: File = toml::from_str(text).map_err(|e| e.message().to_owned())?;
    let table = match arch {
        Arch::Amd64 => file.amd64,
        Arch::Arm64 => file.arm64,
    }
    .ok_or_else(|| format!("no [{}] table", arch.name()))?;
    if table.mirrors.is_empty() {
        return Err(format!("[{}] names no mirror", arch.name()));
    }
    for mirror in &table.mirrors {
        let host = mirror.strip_prefix("https://").unwrap_or_default();
        if host.is_empty() || mirror.ends_with('/') || mirror.contains(['?', '#', '@']) {
            return Err(format!(
                "the mirror {mirror:?} is not an https:// URL with no trailing '/', query, \
                 fragment or user"
            ));
        }
    }
    if table.debs.is_empty() {
        return Err(format!("[{}] pins no package", arch.name()));
    }
    let mut debs: Vec<Pin> = Vec::new();
    for written in table.debs {
        let pin = pin(written, arch)?;
        if debs.iter().any(|d| d.name == pin.name) {
            return Err(format!("{} is pinned twice", pin.name));
        }
        debs.push(pin);
    }
    Ok(Pins {
        mirrors: table.mirrors,
        debs,
    })
}

/// `written` checked: a digest, and a path below `pool/` that ends in the file name its
/// package, version and architecture make.
fn pin(written: Written, arch: Arch) -> Result<Pin, String> {
    let sha256: Sha256 = written.sha256.parse().map_err(|e| format!("{e}"))?;
    let version = written
        .version
        .split_once(':')
        .map_or(written.version.as_str(), |(_, v)| v);
    let names = [arch.name(), "all"].map(|a| format!("{}_{version}_{a}.deb", written.package));
    let name = written.path.rsplit('/').next().unwrap_or_default();
    let inside = written.path.starts_with("pool/")
        && !written.path.split('/').any(|s| s.is_empty() || s == "..");
    if !inside || !names.iter().any(|n| n == name) {
        return Err(format!(
            "the path {:?} of {} {} is not pool/.../{} (or the `all` package)",
            written.path, written.package, written.version, names[0]
        ));
    }
    Ok(Pin {
        name: name.to_owned(),
        path: written.path,
        sha256,
    })
}

/// The download of one URL: its bytes, or why it failed (one line).
pub type Get<'a> = &'a dyn Fn(&str) -> Result<Vec<u8>, String>;

/// The client of the downloads: HTTPS only, trusting `roots`, each attempt given up after
/// [`ATTEMPT_TIMEOUT`].
pub fn https_agent(roots: &[CertificateDer<'static>]) -> ureq::Agent {
    fetch::agent(roots, ATTEMPT_TIMEOUT)
}

/// Downloads with [`https_agent`]: one client for every attempt.
pub fn https(roots: &[CertificateDer<'static>]) -> impl Fn(&str) -> Result<Vec<u8>, String> {
    let agent = https_agent(roots);
    move |url| fetch::download_with(&agent, url).map_err(|e| e.to_string())
}

/// The verb: [`fill`] for this host's architecture, downloading over HTTPS with the host's
/// roots and sleeping between rounds.
pub fn run(args: &Args) -> Result<Vec<String>, Error> {
    let roots = fetch::host_roots();
    fill(
        &args.pins,
        Arch::host()?,
        &args.dir,
        &https(&roots),
        &std::thread::sleep,
    )
}

/// Reads the pins of `arch` from `pins` and makes `dir` hold exactly the pinned files, from the
/// files already there or downloaded with `get`, `pause` waiting before each round: one line
/// per package.
pub fn fill(
    pins: &Path,
    arch: Arch,
    dir: &Path,
    get: Get<'_>,
    pause: &dyn Fn(Duration),
) -> Result<Vec<String>, Error> {
    let pins_path = pins;
    let pins = std::fs::read_to_string(pins)
        .map_err(|e| e.to_string())
        .and_then(|text| parse(&text, arch))
        .map_err(|why| Error::Pins {
            path: pins_path.to_owned(),
            why,
        })?;
    let mut names = std::fs::create_dir_all(dir)
        .and_then(|()| std::fs::read_dir(dir))
        .and_then(|entries| {
            entries
                .map(|e| e.map(|e| e.file_name().to_string_lossy().into_owned()))
                .collect::<std::io::Result<Vec<String>>>()
        })
        .map_err(dir_error(dir))?;
    names.sort();
    if let Some(name) = names
        .iter()
        .find(|n| !pins.debs.iter().any(|d| d.name == **n))
    {
        return Err(Error::Stray {
            dir: dir.to_owned(),
            name: name.clone(),
        });
    }
    let mut lines = Vec::new();
    for pin in &pins.debs {
        let path = dir.join(&pin.name);
        match std::fs::read(&path) {
            Ok(bytes) => {
                let actual = Sha256::of(&bytes);
                if actual != pin.sha256 {
                    return Err(Error::CachedMismatch {
                        path,
                        expected: pin.sha256,
                        actual,
                    });
                }
                lines.push(format!(
                    "ok: the cached {} has SHA-256 {}",
                    pin.name, pin.sha256
                ));
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let (bytes, line) = download(&pins.mirrors, pin, get, pause)?;
                fetch::write_file(&path, &bytes, 0o644).map_err(dir_error(&path))?;
                lines.push(line);
            }
            Err(e) => return Err(dir_error(&path)(e)),
        }
    }
    Ok(lines)
}

/// The [`Error::Dir`] of `path`, for `map_err`.
fn dir_error(path: &Path) -> impl FnOnce(std::io::Error) -> Error + '_ {
    move |e| Error::Dir {
        path: path.to_owned(),
        why: e.to_string(),
    }
}

/// The pinned bytes of `pin` from the first of `mirrors` that gives them, round after round,
/// and the line that says where they came from.
fn download(
    mirrors: &[String],
    pin: &Pin,
    get: Get<'_>,
    pause: &dyn Fn(Duration),
) -> Result<(Vec<u8>, String), Error> {
    let mut attempts = Vec::new();
    for wait in WAITS {
        pause(wait);
        for mirror in mirrors {
            let url = format!("{mirror}/{}", pin.path);
            let failed = match get(&url) {
                Ok(bytes) if Sha256::of(&bytes) == pin.sha256 => {
                    let mut line = format!("ok: {url} has SHA-256 {}", pin.sha256);
                    if !attempts.is_empty() {
                        line.push_str(&format!(
                            ", after {} failed: {}",
                            attempts.len(),
                            attempts.join("; ")
                        ));
                    }
                    return Ok((bytes, line));
                }
                Ok(bytes) => format!("SHA-256 {}, not the pinned one", Sha256::of(&bytes)),
                Err(why) => why.replace(['\r', '\n'], " "),
            };
            attempts.push(format!("{url}: {failed}"));
        }
    }
    Err(Error::Unavailable {
        name: pin.name.clone(),
        attempts,
    })
}

#[cfg(test)]
mod tests;
