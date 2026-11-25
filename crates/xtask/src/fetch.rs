//! `fetch --url <https url> --sha256 <hex> --out <path> [--extract <format> [--member <path>]]
//! [--cache <file> [--url <https url>]...]`: installs a pinned tool for a CI job.
//!
//! The order is the point:
//!
//! 1. The whole download is read into memory over HTTPS (redirects followed, each one
//!    HTTPS too; the host's trusted roots; a status other than 2xx is an error).
//! 2. Its SHA-256 is compared with `--sha256`. A mismatch is refused here, before any
//!    byte is unpacked or written: nothing is created on disk.
//! 3. It is unpacked in memory: as is, from zstd (`--extract zst`), or one regular file
//!    (`--member`, its path inside the archive) out of a gzip or xz tar
//!    (`--extract tar.gz`, `--extract tar.xz`) or a zip (`--extract zip`).
//! 4. The file is written to `<out>.part` beside `--out`, made executable (0755) and
//!    renamed over `--out`. When writing or renaming fails, `<out>.part` is removed, so
//!    `--out` holds either the whole new file or what it held before.
//!
//! With `--cache <file>` ([`fetch_cached`]), the file the actions cache restores and saves holds
//! the download as it was fetched, and steps 1 and 2 become:
//!
//! - when the file is there, it is the download: its SHA-256 is compared with `--sha256` and a
//!   mismatch is refused, the file left as it is and nothing installed. No network is used;
//! - when it is not, each `--url` is tried in turn, in [`WAITS`]`.len()` rounds with the pause
//!   of [`WAITS`] before each round, each attempt given up after [`ATTEMPT_TIMEOUT`]; the first
//!   download whose SHA-256 is the pinned one is written to the file (through a `.part` file,
//!   mode 0644) and installed. A download of other bytes is one more failed attempt. When every
//!   attempt failed, the error names each one, in order, and nothing is written.
//!
//! Without `--cache` there is one `--url` and one attempt, given up after [`DOWNLOAD_TIMEOUT`].

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
pub const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);

/// The pause before each round of downloads of [`first_pinned`]: none before the first.
pub const WAITS: [Duration; 3] = [
    Duration::ZERO,
    Duration::from_secs(10),
    Duration::from_secs(30),
];

/// How long one attempt of [`fetch_cached`] may take, redirects included: a stalled host gives
/// way to the next `--url` well before [`DOWNLOAD_TIMEOUT`].
pub const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(300);

/// The download of one URL: its bytes, or why it failed (one line).
pub type Get<'a> = &'a dyn Fn(&str) -> Result<Vec<u8>, String>;

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

/// The flags of `cargo xtask fetch`.
#[derive(clap::Args)]
pub struct Args {
    /// Where to download it from (HTTPS; redirects must stay on HTTPS). With `--cache`, given
    /// once per URL to try, in order.
    #[arg(long, required = true)]
    pub url: Vec<String>,
    /// The SHA-256 of the download (before unpacking), 64 lowercase hex digits.
    #[arg(long)]
    pub sha256: Sha256,
    /// The file to install; its directory is created when missing.
    #[arg(long)]
    pub out: PathBuf,
    /// How the download is packed (default: it is the file).
    #[arg(long, value_enum)]
    pub extract: Option<Format>,
    /// The file to take out of a tar or zip (`--extract tar.gz`, `tar.xz` or `zip`): its
    /// path inside the archive.
    #[arg(long)]
    pub member: Option<PathBuf>,
    /// The file the actions cache restores and saves: the download as fetched, used when it is
    /// there and its SHA-256 is the pinned one, written when it is not.
    #[arg(long)]
    pub cache: Option<PathBuf>,
}

impl Args {
    /// How the file comes out of the download ([`Unpack::new`]); `Err` also when `--url` is
    /// given more than once without `--cache`.
    pub fn unpack(&self) -> Result<Unpack, String> {
        if self.cache.is_none() && self.url.len() > 1 {
            return Err(
                "--url is given once without --cache: only a cached fetch tries \
                        several URLs"
                    .to_owned(),
            );
        }
        Unpack::new(self.extract, self.member.clone())
    }
}

/// The verb, with the host's roots: [`fetch_cached`] with `--cache` (each attempt given up after
/// [`ATTEMPT_TIMEOUT`], sleeping between rounds), else [`fetch`] of the one `--url`.
pub fn run(args: &Args, unpack: &Unpack) -> Result<String, Error> {
    let roots = host_roots();
    match &args.cache {
        Some(cache) => fetch_cached(
            &Cached {
                urls: &args.url,
                sha256: &args.sha256,
                unpack,
                out: &args.out,
                cache,
            },
            &https(&roots, ATTEMPT_TIMEOUT),
            &std::thread::sleep,
        ),
        // Without `--cache` there is one `--url` ([`Args::unpack`] refuses more; clap, none).
        None => fetch(&args.url.concat(), &args.sha256, unpack, &args.out, &roots),
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
    /// A zip archive (deflate or stored): the file is its `--member`.
    #[value(name = "zip")]
    Zip,
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
    /// The download is a zip archive; the file is the regular file at `member` in it.
    Zip { member: PathBuf },
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
            (Some(Format::Zip), Some(member)) => Ok(Unpack::Zip { member }),
            (Some(Format::TarGz | Format::TarXz | Format::Zip), None) => {
                Err("--extract tar.gz, tar.xz and zip need --member, the file to take".to_owned())
            }
            (_, Some(_)) => Err("--member is a file in a tar or a zip: it needs --extract \
                                 tar.gz, tar.xz or zip"
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
            Unpack::Zip { member } => zip_member(bytes, member),
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
    /// The cache file could not be read, or the download not written to it.
    #[error("the cache {path:?}: {why}; nothing was installed")]
    Cache { path: PathBuf, why: String },
    /// The cache file is not the pinned download.
    #[error(
        "the cached {path:?} has SHA-256 {actual}, not the pinned {expected}; nothing was installed"
    )]
    CachedMismatch {
        path: PathBuf,
        expected: Sha256,
        actual: Sha256,
    },
    /// The cache missed and no URL gave the pinned download in any round.
    #[error("the cache {cache:?} missed and no download was the pinned file: {}; nothing was installed", attempts.join("; "))]
    Unavailable {
        cache: PathBuf,
        /// Each attempt, in order: its URL and why it failed.
        attempts: Vec<String>,
    },
}

/// The error for an archive that could not be decoded.
fn unpack_error(e: impl fmt::Display) -> Error {
    Error::Unpack(e.to_string())
}

/// The roots the host trusts (the system store), for [`fetch`] and [`download`].
pub fn host_roots() -> Vec<CertificateDer<'static>> {
    rustls_native_certs::load_native_certs().certs
}

/// The TLS settings of a client that trusts `roots` and nothing else.
pub fn tls_config(roots: &[CertificateDer<'static>]) -> ureq::tls::TlsConfig {
    let roots = roots
        .iter()
        .map(|c| ureq::tls::Certificate::from_der(c.as_ref()).to_owned())
        .collect();
    ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::Rustls)
        .root_certs(ureq::tls::RootCerts::Specific(Arc::new(roots)))
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .build()
}

/// The body of `url`, over HTTPS only, trusting `roots`, given up after
/// [`DOWNLOAD_TIMEOUT`].
pub fn download(url: &str, roots: &[CertificateDer<'static>]) -> Result<Vec<u8>, Error> {
    download_with(&download_agent(roots), url)
}

/// The client of [`download`]: [`agent`] with [`DOWNLOAD_TIMEOUT`].
pub fn download_agent(roots: &[CertificateDer<'static>]) -> ureq::Agent {
    agent(roots, DOWNLOAD_TIMEOUT)
}

/// The client of every download: HTTPS only (redirects too), trusting `roots` and nothing
/// else, each request given up after `timeout` (redirects included).
pub fn agent(roots: &[CertificateDer<'static>], timeout: Duration) -> ureq::Agent {
    ureq::Agent::config_builder()
        .https_only(true)
        .timeout_global(Some(timeout))
        .tls_config(tls_config(roots))
        .build()
        .into()
}

/// The body of `url`, fetched with `agent` and read up to [`MAX_DOWNLOAD`].
pub fn download_with(agent: &ureq::Agent, url: &str) -> Result<Vec<u8>, Error> {
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
    write_file(out, &file, 0o755).map_err(|e| Error::Install {
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

/// Downloads with `agent(roots, timeout)`: one client for every attempt, each failure one line.
pub fn https(
    roots: &[CertificateDer<'static>],
    timeout: Duration,
) -> impl Fn(&str) -> Result<Vec<u8>, String> {
    let agent = agent(roots, timeout);
    move |url| download_with(&agent, url).map_err(|e| e.to_string())
}

/// The bytes of the first of `urls` whose download has SHA-256 `sha256`, round after round,
/// `pause` waiting the [`WAITS`] before each round, and where they came from:
/// `<url> has SHA-256 <hex>`, with the attempts that failed before it. `Err` is every attempt,
/// in order: its URL and why it failed (an error of `get`, or a download of other bytes).
pub fn first_pinned(
    urls: &[String],
    sha256: &Sha256,
    get: Get<'_>,
    pause: &dyn Fn(Duration),
) -> Result<(Vec<u8>, String), Vec<String>> {
    let mut attempts = Vec::new();
    for wait in WAITS {
        pause(wait);
        for url in urls {
            let failed = match get(url) {
                Ok(bytes) if Sha256::of(&bytes) == *sha256 => {
                    let mut line = format!("{url} has SHA-256 {sha256}");
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
    Err(attempts)
}

/// What [`fetch_cached`] installs, and from where.
pub struct Cached<'a> {
    /// The URLs to download from on a miss, in order.
    pub urls: &'a [String],
    /// The SHA-256 of the download (before unpacking).
    pub sha256: &'a Sha256,
    /// How the file comes out of the download.
    pub unpack: &'a Unpack,
    /// The file to install.
    pub out: &'a Path,
    /// The file the actions cache restores and saves: the download as fetched.
    pub cache: &'a Path,
}

/// Installs `req.out` from the download in `req.cache` when it is there, else from the first of
/// `req.urls` that gives the pinned bytes ([`first_pinned`]), kept in `req.cache`: the line to
/// print, which says whether the cache hit.
pub fn fetch_cached(
    req: &Cached<'_>,
    get: Get<'_>,
    pause: &dyn Fn(Duration),
) -> Result<String, Error> {
    let cache_error = |e: std::io::Error| Error::Cache {
        path: req.cache.to_owned(),
        why: e.to_string(),
    };
    let (bytes, from) = match std::fs::read(req.cache) {
        Ok(bytes) => {
            let actual = Sha256::of(&bytes);
            if actual != *req.sha256 {
                return Err(Error::CachedMismatch {
                    path: req.cache.to_owned(),
                    expected: *req.sha256,
                    actual,
                });
            }
            let from = format!(
                "the cache hit: {} has SHA-256 {actual}; nothing was downloaded",
                req.cache.display()
            );
            (bytes, from)
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let (bytes, line) =
                first_pinned(req.urls, req.sha256, get, pause).map_err(|attempts| {
                    Error::Unavailable {
                        cache: req.cache.to_owned(),
                        attempts,
                    }
                })?;
            write_file(req.cache, &bytes, 0o644).map_err(cache_error)?;
            let from = format!("the cache missed: {line}; kept in {}", req.cache.display());
            (bytes, from)
        }
        Err(e) => return Err(cache_error(e)),
    };
    install(&bytes, req.sha256, req.unpack, req.out)?;
    Ok(format!("ok: {from}; installed {}", req.out.display()))
}

/// The file `out` is written as before it is renamed over `out`.
pub fn part_path(out: &Path) -> PathBuf {
    let mut part = out.as_os_str().to_owned();
    part.push(".part");
    PathBuf::from(part)
}

/// Writes `data` to `out` (mode `mode`) through [`part_path`], removing the part file
/// when a step fails.
pub(crate) fn write_file(out: &Path, data: &[u8], mode: u32) -> std::io::Result<()> {
    out.parent().map_or(Ok(()), std::fs::create_dir_all)?;
    let part = part_path(out);
    let written = std::fs::write(&part, data)
        .and_then(|()| std::fs::set_permissions(&part, std::fs::Permissions::from_mode(mode)))
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

/// The contents of the regular file at `member` in the zip archive `zip`.
fn zip_member(zip: &[u8], member: &Path) -> Result<Vec<u8>, Error> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip)).map_err(unpack_error)?;
    let name = member.to_string_lossy();
    let mut file = match archive.by_name(&name) {
        Ok(file) => file,
        Err(zip::result::ZipError::FileNotFound) => return Err(Error::NoMember(member.to_owned())),
        Err(e) => return Err(unpack_error(e)),
    };
    // `is_file` is false for a directory and for a symbolic link (a file whose Unix type
    // bits say so).
    if !file.is_file() {
        return Err(Error::NotAFile(member.to_owned()));
    }
    let mut out = Vec::new();
    file.read_to_end(&mut out).map_err(unpack_error)?;
    Ok(out)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod cache_tests;
