//! `fetch --url <https url> --sha256 <hex> --out <path> [--extract <format> [--member <path>]]`:
//! installs a pinned tool for a CI job.
//!
//! The order is the point:
//!
//! 1. The whole download is read into memory over HTTPS (redirects followed, each one
//!    HTTPS too; the host's trusted roots; a status other than 2xx is an error).
//! 2. Its SHA-256 is compared with `--sha256`. A mismatch is refused here, before any
//!    byte is unpacked or written: nothing is created on disk.
//! 3. It is unpacked in memory: as is, from zstd (`--extract zst`), or one regular file
//!    (`--member`, its path inside the archive) out of a gzip or xz tar
//!    (`--extract tar.gz`, `--extract tar.xz`).
//! 4. The file is written to `<out>.part` beside `--out`, made executable (0755) and
//!    renamed over `--out`. When writing or renaming fails, `<out>.part` is removed, so
//!    `--out` holds either the whole new file or what it held before.

use std::fmt;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;

use rustls::pki_types::CertificateDer;
use sha2::Digest;

/// The largest download read: well above the biggest pinned tool (about 60 MB), and a
/// bound on the memory a wrong URL can take.
pub const MAX_DOWNLOAD: u64 = 512 * 1024 * 1024;

/// How long one download may take, redirects included.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// A SHA-256 digest, written as 64 lowercase hex digits.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Sha256(pub [u8; 32]);

impl Sha256 {
    /// The digest of `bytes`.
    pub fn of(bytes: &[u8]) -> Sha256 {
        Sha256(sha2::Sha256::digest(bytes).into())
    }
}

impl fmt::Display for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&hex::encode(self.0))
    }
}

impl fmt::Debug for Sha256 {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// A `--sha256` value that is not 64 lowercase hex digits.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
#[error("not a SHA-256: {0:?} (expected 64 lowercase hex digits)")]
pub struct NotASha256(String);

impl FromStr for Sha256 {
    type Err = NotASha256;

    fn from_str(s: &str) -> Result<Sha256, NotASha256> {
        let mut digest = [0; 32];
        let lowercase = s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        match hex::decode_to_slice(s, &mut digest) {
            Ok(()) if lowercase => Ok(Sha256(digest)),
            _ => Err(NotASha256(s.to_owned())),
        }
    }
}

/// What `--extract` names.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// zstd-compressed: the file is the decompressed download.
    Zst,
    /// A gzip-compressed tar: the file is its `--member`.
    #[value(name = "tar.gz")]
    TarGz,
    /// An xz-compressed tar: the file is its `--member`.
    #[value(name = "tar.xz")]
    TarXz,
}

/// How the installed file comes out of the download.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unpack {
    /// The download is the file.
    Raw,
    /// The download is zstd-compressed (one frame or several).
    Zst,
    /// The download is a tar archive, gzip-compressed (`xz: false`) or xz-compressed;
    /// the file is the regular file at `member` in it.
    Tar { xz: bool, member: PathBuf },
}

impl Unpack {
    /// `--extract` and `--member` as given; `Err` names the flag that is missing or
    /// out of place.
    pub fn new(extract: Option<Format>, member: Option<PathBuf>) -> Result<Unpack, String> {
        match (extract, member) {
            (None, None) => Ok(Unpack::Raw),
            (Some(Format::Zst), None) => Ok(Unpack::Zst),
            (Some(Format::TarGz), Some(member)) => Ok(Unpack::Tar { xz: false, member }),
            (Some(Format::TarXz), Some(member)) => Ok(Unpack::Tar { xz: true, member }),
            (Some(Format::TarGz | Format::TarXz), None) => {
                Err("--extract tar.gz and tar.xz need --member, the file to take".to_owned())
            }
            (_, Some(_)) => Err("--member is a file in a tar: it needs --extract tar.gz or \
                                 --extract tar.xz"
                .to_owned()),
        }
    }

    /// The file this unpacking makes of `bytes`.
    fn apply(&self, bytes: &[u8]) -> Result<Vec<u8>, Error> {
        match self {
            Unpack::Raw => Ok(bytes.to_vec()),
            Unpack::Zst => unzstd(bytes),
            Unpack::Tar { xz: false, member } => {
                let mut tar = Vec::new();
                flate2::read::MultiGzDecoder::new(bytes)
                    .read_to_end(&mut tar)
                    .map_err(unpack_error)?;
                tar_member(&tar, member)
            }
            Unpack::Tar { xz: true, member } => {
                let mut tar = Vec::new();
                lzma_rs::xz_decompress(&mut &bytes[..], &mut tar).map_err(unpack_error)?;
                tar_member(&tar, member)
            }
        }
    }
}

/// Why a fetch failed.
#[derive(Debug, PartialEq, Eq, thiserror::Error)]
pub enum Error {
    #[error("download {url}: {why}")]
    Download { url: String, why: String },
    #[error("the download's SHA-256 is {actual}, not the pinned {expected}; nothing was installed")]
    Mismatch { expected: Sha256, actual: Sha256 },
    #[error("unpack the download: {0}")]
    Unpack(String),
    #[error("the archive has no {0:?}")]
    NoMember(PathBuf),
    #[error("{0:?} in the archive is not a regular file")]
    NotAFile(PathBuf),
    #[error("install {path:?}: {why}")]
    Install { path: PathBuf, why: String },
}

/// The error for an archive that could not be decoded.
fn unpack_error(e: impl fmt::Display) -> Error {
    Error::Unpack(e.to_string())
}

/// The roots the host trusts (the system store), for [`fetch`] and [`download`].
pub fn host_roots() -> Vec<CertificateDer<'static>> {
    rustls_native_certs::load_native_certs().certs
}

/// The body of `url`, over HTTPS only, trusting `roots`.
pub fn download(url: &str, roots: &[CertificateDer<'static>]) -> Result<Vec<u8>, Error> {
    let roots = roots
        .iter()
        .map(|c| ureq::tls::Certificate::from_der(c.as_ref()).to_owned())
        .collect();
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::Rustls)
        .root_certs(ureq::tls::RootCerts::Specific(Arc::new(roots)))
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .build();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(DOWNLOAD_TIMEOUT))
        .tls_config(tls)
        .build()
        .into();
    let failed = |e: ureq::Error| Error::Download {
        url: url.to_owned(),
        why: e.to_string(),
    };
    let mut response = agent.get(url).call().map_err(failed)?;
    let body = response.body_mut().with_config().limit(MAX_DOWNLOAD);
    body.read_to_vec().map_err(failed)
}

/// Checks `bytes` against `sha256`, unpacks them and installs the file at `out`.
pub fn install(bytes: &[u8], sha256: &Sha256, unpack: &Unpack, out: &Path) -> Result<(), Error> {
    let actual = Sha256::of(bytes);
    if actual != *sha256 {
        return Err(Error::Mismatch {
            expected: *sha256,
            actual,
        });
    }
    let file = unpack.apply(bytes)?;
    write_executable(out, &file).map_err(|e| Error::Install {
        path: out.to_owned(),
        why: e.to_string(),
    })
}

/// [`download`]s `url` and [`install`]s it at `out`: the line to print.
pub fn fetch(
    url: &str,
    sha256: &Sha256,
    unpack: &Unpack,
    out: &Path,
    roots: &[CertificateDer<'static>],
) -> Result<String, Error> {
    let bytes = download(url, roots)?;
    install(&bytes, sha256, unpack, out)?;
    Ok(format!(
        "ok: {url} has SHA-256 {sha256}; installed {}",
        out.display()
    ))
}

/// The file `out` is written as before it is renamed over `out`.
pub fn part_path(out: &Path) -> PathBuf {
    let mut part = out.as_os_str().to_owned();
    part.push(".part");
    PathBuf::from(part)
}

/// Writes `data` to `out` (mode 0755) through [`part_path`], removing the part file
/// when a step fails.
fn write_executable(out: &Path, data: &[u8]) -> std::io::Result<()> {
    out.parent().map_or(Ok(()), std::fs::create_dir_all)?;
    let part = part_path(out);
    let written = std::fs::write(&part, data)
        .and_then(|()| std::fs::set_permissions(&part, std::fs::Permissions::from_mode(0o755)))
        .and_then(|()| std::fs::rename(&part, out));
    if written.is_err() {
        // A part file that cannot be removed is reported by the write error itself.
        let _ = std::fs::remove_file(&part);
    }
    written
}

/// The decompressed contents of `bytes`, every zstd frame in turn.
fn unzstd(mut bytes: &[u8]) -> Result<Vec<u8>, Error> {
    let mut out = Vec::new();
    while !bytes.is_empty() {
        let mut frame =
            ruzstd::decoding::StreamingDecoder::new(&mut bytes).map_err(unpack_error)?;
        frame.read_to_end(&mut out).map_err(unpack_error)?;
    }
    Ok(out)
}

/// The contents of the regular file at `member` in the tar archive `tar`.
fn tar_member(tar: &[u8], member: &Path) -> Result<Vec<u8>, Error> {
    let mut archive = tar::Archive::new(tar);
    for entry in archive.entries().map_err(unpack_error)? {
        let mut entry = entry.map_err(unpack_error)?;
        if entry.path().map_err(unpack_error)? != member {
            continue;
        }
        if !entry.header().entry_type().is_file() {
            return Err(Error::NotAFile(member.to_owned()));
        }
        let mut file = Vec::new();
        entry.read_to_end(&mut file).map_err(unpack_error)?;
        return Ok(file);
    }
    Err(Error::NoMember(member.to_owned()))
}

#[cfg(test)]
mod tests;
