//! Reading the action's inputs through the read grant: blobs by digest, checked, and the input
//! tree laid out under a directory.
//!
//! A [`BlobSource`] hands over the bytes of a blob, either whole ([`BlobSource::get`]) or streamed
//! into a writer ([`BlobSource::fetch_to`]). Nothing a source returns is trusted: every blob
//! passes through a check of its length and SHA-256 against its digest as it streams, and a body
//! longer than the digest says is refused at the first byte past the size, never buffered.
//! [`HttpBlobSource`] is the source over the read grant (a base URL and a bearer token); tests use
//! in-memory ones.
//!
//! Directory, Action and Command messages are held in memory and decoded; a message whose digest
//! names more than [`MAX_MESSAGE_BYTES`] is refused before any request. File blobs are never held
//! whole: each streams from the response to its first path on disk.
//!
//! [`materialize`] writes the tree under an input root digest into a directory. Directory messages
//! come from clients, so every name is checked before it touches the filesystem: a name with a
//! slash, `.`, `..` or a NUL, empty, or used twice in one directory refuses the tree. Files are
//! created with `O_EXCL` in directories this module made, so no write follows a symlink the tree
//! contains.
//!
//! # Concurrency
//!
//! The tree is read breadth-first: each level's distinct Directory messages are fetched together,
//! then every distinct file blob. In both phases at most `concurrency` requests are in flight, on
//! that many threads of a scope pulling from one shared queue of digests (see
//! [`BlobSource::fetch_many`]); [`HttpBlobSource`] shares one `ureq` agent among them, so
//! keep-alive connections are pooled. The first failure stops new requests; requests already in
//! flight finish, and that first error is returned.
//!
//! With [`HttpBlobSource::with_batch`], blobs under 64 KiB are asked for many to a request instead
//! (see the `batch` module): the same bounded concurrency counts a batch as one request.
//!
//! # Copies
//!
//! A blob named by several paths is fetched once, streamed to the first path. The other paths are
//! made from that file once its hash has been checked, each a copy of its own (its own inode and its
//! own mode, so a write or a chmod through one path does not show through another). A blob that
//! fails its check, and a copy that fails partway, are removed from their paths, so a file this
//! module created is a complete, verified one; the tree around it is left for the caller to
//! remove.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::io::{Read, Write};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use kbf_proto::reapi::Digest;
use prost::Message;
use sha2::{Digest as _, Sha256};

/// Why an input could not be read or laid out.
#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    /// A digest is not a lowercase SHA-256 hex string with a non-negative size.
    #[error("invalid digest {0:?}")]
    BadDigest(String),
    /// The source could not give the blob: unreachable, refused, or absent.
    #[error("blob {digest}: {why}")]
    Source {
        /// The digest asked for, as `<hex>/<size>`.
        digest: String,
        /// What went wrong.
        why: String,
    },
    /// The source gave bytes that are not the blob its digest names.
    #[error("blob {digest} does not match its digest: {why}")]
    Mismatch {
        /// The digest asked for, as `<hex>/<size>`.
        digest: String,
        /// How the bytes differ.
        why: String,
    },
    /// The tree breaks a rule: the client's error.
    #[error("{0}")]
    Invalid(String),
    /// A filesystem operation failed.
    #[error("{path}: {source}")]
    Io {
        /// The path that was being made or written.
        path: PathBuf,
        /// The operating system's error.
        #[source]
        source: std::io::Error,
    },
}

fn io(path: &Path) -> impl FnOnce(std::io::Error) -> FetchError + '_ {
    move |source| FetchError::Io {
        path: path.to_owned(),
        source,
    }
}

/// `<hex>/<size>`: how a digest is written in URLs, errors and the manifest.
#[must_use]
pub fn label(digest: &Digest) -> String {
    format!("{}/{}", digest.hash, digest.size_bytes)
}

/// Refuses a digest that is not 64 lowercase hex digits with a size of at least 0.
fn check_digest(digest: &Digest) -> Result<(), FetchError> {
    let hex = digest.hash.len() == 64
        && digest
            .hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if hex && digest.size_bytes >= 0 {
        Ok(())
    } else {
        Err(FetchError::BadDigest(label(digest)))
    }
}

/// The most a Directory, Action or Command message may be: these are held in memory, so a digest
/// naming more is refused before any request.
pub const MAX_MESSAGE_BYTES: i64 = 4 << 20;

/// Streams one blob into the writer it is handed: what [`BlobSource::fetch_many`] gives its
/// callback for each digest.
pub type Fetch<'a> = &'a mut dyn FnMut(&mut dyn Write) -> Result<(), FetchError>;

/// What [`BlobSource::fetch_many`] calls for digest number `index`: it must call the [`Fetch`]
/// it is given exactly once and return its result, or its own error.
pub type Each<'a> = dyn Fn(usize, Fetch<'_>) -> Result<(), FetchError> + Sync + 'a;

/// Where blobs come from. Implement [`BlobSource::get`]; a source that can stream overrides
/// [`BlobSource::fetch_to`], and one that can ask for many blobs in one request overrides
/// [`BlobSource::fetch_many`].
pub trait BlobSource: Sync {
    /// The bytes the source holds for `digest`, unchecked.
    ///
    /// # Errors
    /// The blob cannot be had: [`FetchError::Source`].
    fn get(&self, digest: &Digest) -> Result<Vec<u8>, FetchError>;

    /// Writes the bytes the source holds for `digest`, unchecked, into `out` as they arrive. A
    /// source that streams stops reading once `out` refuses a write; the default holds the blob
    /// whole through [`BlobSource::get`].
    ///
    /// # Errors
    /// The blob cannot be had ([`FetchError::Source`]), or `out` fails.
    fn fetch_to(&self, digest: &Digest, out: &mut dyn Write) -> Result<(), FetchError> {
        write_whole(self.get(digest), digest, out)
    }

    /// Fetches every digest in `digests`, at most `concurrency` (at least 1) requests in flight,
    /// by calling `each(index, fetch)` for each; `fetch` streams that digest's blob into the
    /// writer `each` gives it. Returns the first error, after which no new request starts and
    /// those in flight finish.
    ///
    /// This is the one place a source that has a batch request would send its requests: the
    /// default is one [`BlobSource::fetch_to`] per digest, on a pool of threads.
    ///
    /// # Errors
    /// The first error any `each` returned.
    fn fetch_many(
        &self,
        digests: &[Digest],
        concurrency: usize,
        each: &Each<'_>,
    ) -> Result<(), FetchError> {
        run_bounded(concurrency, digests, &|index, digest| {
            each(index, &mut |out| self.fetch_to(digest, out))
        })
    }

    /// The blob `digest` names: [`BlobSource::fetch_to`] with the digest itself checked first, and
    /// the bytes checked against it as they arrive (length, then SHA-256).
    ///
    /// # Errors
    /// The digest is malformed, the source fails, or the bytes are not the blob.
    fn verified(&self, digest: &Digest) -> Result<Vec<u8>, FetchError> {
        verified_bytes(digest, &mut |out| self.fetch_to(digest, out))
    }

    /// One line saying how the inputs were read, when the source has more than one way to read
    /// them; the default has nothing to say.
    fn report(&self) -> Option<String> {
        None
    }
}

/// The blob `digest` names, streamed through `fetch` into memory and checked: the default of
/// [`BlobSource::verified`], outside the trait so that it is one function whatever the source.
fn verified_bytes(digest: &Digest, fetch: Fetch<'_>) -> Result<Vec<u8>, FetchError> {
    let mut bytes = Vec::new();
    stream_checked(digest, Path::new(""), &mut bytes, fetch)?;
    Ok(bytes)
}

/// Writes `bytes`, what a source held for `digest`, into `out`: the default of
/// [`BlobSource::fetch_to`], outside the trait so that it is one function whatever the source.
fn write_whole(
    bytes: Result<Vec<u8>, FetchError>,
    digest: &Digest,
    out: &mut dyn Write,
) -> Result<(), FetchError> {
    out.write_all(&bytes?).map_err(|e| FetchError::Source {
        digest: label(digest),
        why: format!("the bytes could not be taken: {e}"),
    })
}

/// Runs `work(index, item)` for every item on up to `concurrency` threads that take items from
/// one shared queue in order. After the first error no thread takes another item; the others
/// finish the one they have. Returns that first error.
fn run_bounded<T: Sync>(
    concurrency: usize,
    items: &[T],
    work: &(dyn Fn(usize, &T) -> Result<(), FetchError> + Sync),
) -> Result<(), FetchError> {
    let next = AtomicUsize::new(0);
    let stopped = AtomicBool::new(false);
    let first: Mutex<Option<FetchError>> = Mutex::new(None);
    std::thread::scope(|scope| {
        for _ in 0..concurrency.max(1).min(items.len()) {
            scope.spawn(|| {
                // A worker that unwinds (a panic in `work`) ends here too, and this stops the
                // others from taking new items while the scope carries the panic out. A worker
                // that ends normally has run out of items or seen `stopped` already, so setting
                // it again changes nothing. There is no test of the panic itself: it would need a
                // branch in test code to tell the panic from the other exits.
                let _stop_on_exit = StopOnExit(&stopped);
                while !stopped.load(Ordering::SeqCst) {
                    let index = next.fetch_add(1, Ordering::SeqCst);
                    let Some(item) = items.get(index) else { break };
                    if let Err(error) = work(index, item) {
                        stopped.store(true, Ordering::SeqCst);
                        first.lock().expect("first error").get_or_insert(error);
                    }
                }
            });
        }
    });
    match first.into_inner().expect("first error") {
        Some(error) => Err(error),
        None => Ok(()),
    }
}

/// Sets the flag when dropped, whether the worker holding it returned or unwound.
struct StopOnExit<'a>(&'a AtomicBool);

impl Drop for StopOnExit<'_> {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

/// A writer in front of `inner` that checks what passes through against `digest`: it refuses the
/// first write that would take the total past the digest's size, and hashes what it passes on.
/// Why it refused (or why `inner` failed) is kept in `failure`, since the `io::Error` a source
/// sees cannot carry it.
struct Checked<'a> {
    inner: &'a mut dyn Write,
    digest: &'a Digest,
    path: &'a Path,
    hasher: Sha256,
    seen: u64,
    failure: Option<FetchError>,
}

impl Write for Checked<'_> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let (digest, path) = (self.digest, self.path);
        let size = digest.size_bytes.unsigned_abs();
        if self.seen + buf.len() as u64 > size {
            self.failure.get_or_insert_with(|| FetchError::Mismatch {
                digest: label(digest),
                why: format!("it is longer than {size} bytes"),
            });
            return Err(std::io::Error::other("longer than its digest says"));
        }
        if let Err(source) = self.inner.write_all(buf) {
            let kind = source.kind();
            self.failure.get_or_insert_with(|| FetchError::Io {
                path: path.to_owned(),
                source,
            });
            return Err(std::io::Error::from(kind));
        }
        self.hasher.update(buf);
        self.seen += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Streams the blob `digest` names into `inner` through `fetch`, checking it as it goes: the
/// digest is well formed, the body is not longer than its size, and at the end it is exactly that
/// long and its SHA-256 matches. `path` names `inner` in an error when writing to it fails. Not
/// generic over the writer, so its branches are one function.
fn stream_checked(
    digest: &Digest,
    path: &Path,
    inner: &mut dyn Write,
    fetch: Fetch<'_>,
) -> Result<(), FetchError> {
    check_digest(digest)?;
    let mut checked = Checked {
        inner,
        digest,
        path,
        hasher: Sha256::new(),
        seen: 0,
        failure: None,
    };
    let fetched = fetch(&mut checked);
    if let Some(failure) = checked.failure.take() {
        return Err(failure);
    }
    fetched?;
    checked.flush().map_err(io(path))?;
    let mismatch = |why: String| FetchError::Mismatch {
        digest: label(digest),
        why,
    };
    if i64::try_from(checked.seen).ok() != Some(digest.size_bytes) {
        return Err(mismatch(format!("it has {} bytes", checked.seen)));
    }
    let hash = hex::encode(checked.hasher.finalize());
    if hash != digest.hash {
        return Err(mismatch(format!("its SHA-256 is {hash}")));
    }
    Ok(())
}

/// Streams `digest` through `fetch` into memory (at most [`MAX_MESSAGE_BYTES`], checked before
/// the request) and verifies it. Not generic over the message, so its branches are one function.
fn message_bytes(digest: &Digest, fetch: Fetch<'_>) -> Result<Vec<u8>, FetchError> {
    if digest.size_bytes > MAX_MESSAGE_BYTES {
        return Err(FetchError::Invalid(format!(
            "message blob {} is {} bytes, over the {MAX_MESSAGE_BYTES}-byte limit for a message",
            label(digest),
            digest.size_bytes
        )));
    }
    verified_bytes(digest, fetch)
}

/// The refusal of a verified message blob that does not decode.
fn undecodable(digest: &Digest, error: &prost::DecodeError) -> FetchError {
    FetchError::Invalid(format!("blob {} does not decode: {error}", label(digest)))
}

/// Fetches a message blob and decodes it as `M`.
///
/// # Errors
/// The digest names more than [`MAX_MESSAGE_BYTES`] ([`FetchError::Invalid`]), the blob cannot be
/// read or checked ([`BlobSource::verified`]), or it does not decode as `M`
/// ([`FetchError::Invalid`]).
pub fn fetch_message<M: Message + Default>(
    source: &dyn BlobSource,
    digest: &Digest,
) -> Result<M, FetchError> {
    let bytes = message_bytes(digest, &mut |out| source.fetch_to(digest, out))?;
    decode_message(&bytes, digest)
}

/// `bytes`, a verified blob `digest` names, decoded as `M`.
fn decode_message<M: Message + Default>(bytes: &[u8], digest: &Digest) -> Result<M, FetchError> {
    M::decode(bytes).map_err(|e| undecodable(digest, &e))
}

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// The time one request for a blob of `size` bytes may take in all: 60 s and 1 s more for each
/// 8 MiB, at most an hour.
fn request_timeout(size: i64) -> Duration {
    const BASE: u64 = 60;
    const PER_SECOND: u64 = 8 << 20;
    const MAX: u64 = 3600;
    Duration::from_secs((BASE + size.unsigned_abs() / PER_SECOND).min(MAX))
}

/// The most requests [`materialize`] may have in flight, and the number of idle keep-alive
/// connections the agent keeps for them.
pub const MAX_CONCURRENCY: usize = 256;

/// The blobs of a read grant: `GET <base>/<hex>/<size>` with `Authorization: Bearer <token>`,
/// over HTTPS with rustls on the ring provider, trusting only the roots it is given. No redirect
/// is followed and no proxy is used.
pub struct HttpBlobSource {
    agent: ureq::Agent,
    base: String,
    authorization: String,
    /// Whether small blobs are asked for in batches (see the `batch` module).
    batch: bool,
    /// Set once the grant has answered that it has no batch route.
    batch_off: AtomicBool,
    /// Makes the one stderr line about a missing batch route.
    batch_notice: std::sync::Once,
}

impl std::fmt::Debug for HttpBlobSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HttpBlobSource")
            .field("base", &self.base)
            .finish_non_exhaustive()
    }
}

/// The agent every read uses: rustls on the ring provider trusting only `roots`, no redirect, no
/// proxy, a pool of idle connections for [`MAX_CONCURRENCY`] requests.
fn http_agent(roots: &[Vec<u8>], https_only: bool) -> ureq::Agent {
    let roots: Vec<ureq::tls::Certificate<'static>> = roots
        .iter()
        .map(|der| ureq::tls::Certificate::from_der(der).to_owned())
        .collect();
    let tls = ureq::tls::TlsConfig::builder()
        .provider(ureq::tls::TlsProvider::Rustls)
        .root_certs(ureq::tls::RootCerts::Specific(Arc::new(roots)))
        .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .build();
    ureq::Agent::config_builder()
        .https_only(https_only)
        .proxy(None)
        .max_redirects(0)
        .http_status_as_error(false)
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .max_idle_connections(MAX_CONCURRENCY)
        .max_idle_connections_per_host(MAX_CONCURRENCY)
        .tls_config(tls)
        .build()
        .into()
}

/// The host's trusted root certificates, DER encoded: what to pass to [`HttpBlobSource::new`]
/// when the grant's server uses a public CA.
#[must_use]
pub fn native_roots() -> Vec<Vec<u8>> {
    rustls_native_certs::load_native_certs()
        .certs
        .into_iter()
        .map(|c| c.as_ref().to_vec())
        .collect()
}

impl HttpBlobSource {
    /// A source for the grant at `base` (an `https://` URL; a trailing `/` is dropped), with
    /// `token` as the bearer and `roots` (DER certificates) as the only trusted roots.
    ///
    /// # Errors
    /// `roots` is empty: every connection would fail with an unhelpful message.
    pub fn new(base: &str, token: &str, roots: &[Vec<u8>]) -> Result<Self, FetchError> {
        Self::new_with(base, token, roots, false)
    }

    /// [`new`](Self::new), and with `allow_plain_http` an `http://` grant is read too (the
    /// `--allow-plain-http-grant` opt-in, for a farm on its own network). The bearer token is sent
    /// either way.
    ///
    /// # Errors
    /// `roots` is empty.
    pub fn new_with(
        base: &str,
        token: &str,
        roots: &[Vec<u8>],
        allow_plain_http: bool,
    ) -> Result<Self, FetchError> {
        if roots.is_empty() {
            return Err(FetchError::Invalid(
                "no trusted root certificates for the read grant".to_owned(),
            ));
        }
        Ok(Self::build(base, token, roots, !allow_plain_http))
    }

    fn build(base: &str, token: &str, roots: &[Vec<u8>], https_only: bool) -> Self {
        Self {
            agent: http_agent(roots, https_only),
            base: base.trim_end_matches('/').to_owned(),
            authorization: format!("Bearer {token}"),
            batch: false,
            batch_off: AtomicBool::new(false),
            batch_notice: std::sync::Once::new(),
        }
    }
}

impl BlobSource for HttpBlobSource {
    fn get(&self, digest: &Digest) -> Result<Vec<u8>, FetchError> {
        let mut bytes = Vec::new();
        self.fetch_to(digest, &mut bytes)?;
        Ok(bytes)
    }

    fn fetch_to(&self, digest: &Digest, out: &mut dyn Write) -> Result<(), FetchError> {
        check_digest(digest)?;
        let source = |why: String| FetchError::Source {
            digest: label(digest),
            why,
        };
        let url = format!("{}/{}", self.base, label(digest));
        let mut response = self
            .agent
            .get(&url)
            .config()
            .timeout_global(Some(request_timeout(digest.size_bytes)))
            .build()
            .header("Authorization", &self.authorization)
            .call()
            .map_err(|e| source(e.to_string()))?;
        let status = response.status().as_u16();
        if status != 200 {
            return Err(source(format!("the grant answered {status}")));
        }
        // One byte past the size is enough for a check to see a blob that is too long.
        let limit = digest.size_bytes.unsigned_abs() + 1;
        std::io::copy(&mut response.body_mut().as_reader().take(limit), out)
            .map(|_| ())
            .map_err(|e| source(e.to_string()))
    }

    fn fetch_many(
        &self,
        digests: &[Digest],
        concurrency: usize,
        each: &Each<'_>,
    ) -> Result<(), FetchError> {
        self.fetch_units(digests, concurrency, each)
    }
}

/// Checks one name of a Directory entry.
fn check_name(name: &str) -> Result<(), FetchError> {
    if name.is_empty() || name == "." || name == ".." || name.contains(['/', '\0']) {
        return Err(FetchError::Invalid(format!(
            "input tree entry name {name:?} is not a single path component"
        )));
    }
    Ok(())
}

/// Checks a path the Command names (working directory, output path): relative, made of
/// non-empty components other than `.` and `..`, joined by single slashes. The empty path is the
/// directory itself.
///
/// # Errors
/// [`FetchError::Invalid`], naming `what`.
pub fn check_relative(what: &str, path: &str) -> Result<(), FetchError> {
    let plain = path.is_empty()
        || path
            .split('/')
            .all(|part| !matches!(part, "" | "." | "..") && !part.contains('\0'));
    if !plain {
        return Err(FetchError::Invalid(format!(
            "{what} {path:?} must be relative and must not contain . or .."
        )));
    }
    Ok(())
}

/// Refuses a Directory whose entry names are not single path components, or that names one entry
/// twice.
fn check_names(directory: &kbf_proto::reapi::Directory) -> Result<(), FetchError> {
    let mut seen = BTreeSet::new();
    let names = directory
        .files
        .iter()
        .map(|f| &f.name)
        .chain(directory.directories.iter().map(|d| &d.name))
        .chain(directory.symlinks.iter().map(|s| &s.name));
    for name in names {
        check_name(name)?;
        if !seen.insert(name) {
            return Err(FetchError::Invalid(format!(
                "input tree names {name:?} twice in one directory"
            )));
        }
    }
    Ok(())
}

/// Where one file blob goes: each path it is written to, and whether that copy is executable.
struct Copies {
    digest: Digest,
    copies: Vec<(PathBuf, bool)>,
}

/// Writes the tree under `root` into `dir` (created if absent, and empty), with at most
/// `concurrency` (at least 1) requests in flight.
///
/// The tree is walked breadth-first: each level's distinct Directory messages are fetched
/// together, every Directory is read once however many places name it, every name is checked,
/// every directory and symlink made. Then each distinct file blob is fetched once, streamed
/// through its digest check to the first path that names it, and made at every other path (see
/// the module documentation), with the executable bit of each. A blob that is missing or does not
/// match its digest fails the call and is not left at its path; files already written stay in
/// `dir`, for the caller to remove.
///
/// # Errors
/// A blob is missing or corrupt, the tree breaks a naming rule, or a write fails.
pub fn materialize(
    source: &dyn BlobSource,
    root: &Digest,
    dir: &Path,
    concurrency: usize,
) -> Result<(), FetchError> {
    std::fs::create_dir_all(dir).map_err(io(dir))?;
    let files = make_directories(source, root, dir, concurrency)?;
    let digests: Vec<Digest> = files.iter().map(|f| f.digest.clone()).collect();
    let written = AtomicUsize::new(0);
    source.fetch_many(&digests, concurrency, &|index, fetch| {
        write_copies(&files[index], fetch)?;
        written.fetch_add(1, Ordering::SeqCst);
        Ok(())
    })?;
    all_written(written.into_inner(), files.len())
}

/// Fails when a source called back for fewer blobs than it was asked for. It is not generic, so
/// both outcomes are one branch however many sources `materialize` is used with.
fn all_written(written: usize, wanted: usize) -> Result<(), FetchError> {
    if written == wanted {
        Ok(())
    } else {
        Err(FetchError::Invalid("a source skipped a blob".to_owned()))
    }
}

/// Fetches and decodes one Directory message and checks its names.
fn fetch_directory(
    digest: &Digest,
    fetch: Fetch<'_>,
) -> Result<kbf_proto::reapi::Directory, FetchError> {
    let bytes = message_bytes(digest, fetch)?;
    let directory: kbf_proto::reapi::Directory = decode_message(&bytes, digest)?;
    check_names(&directory)?;
    Ok(directory)
}

/// Walks the tree under `root` a level at a time, checking every Directory's names and making
/// its directories and symlinks under `dir`; returns the file blobs to write, each distinct digest
/// once.
fn make_directories(
    source: &dyn BlobSource,
    root: &Digest,
    dir: &Path,
    concurrency: usize,
) -> Result<Vec<Copies>, FetchError> {
    let mut files: Vec<Copies> = Vec::new();
    let mut blob_at: HashMap<String, usize> = HashMap::new();
    let mut loaded: HashMap<String, kbf_proto::reapi::Directory> = HashMap::new();
    let mut level = vec![(root.clone(), dir.to_owned())];
    while !level.is_empty() {
        let mut wanted: Vec<Digest> = Vec::new();
        let mut queued = HashSet::new();
        for (digest, _) in &level {
            if !loaded.contains_key(&label(digest)) && queued.insert(label(digest)) {
                wanted.push(digest.clone());
            }
        }
        let slots: Vec<Mutex<Option<kbf_proto::reapi::Directory>>> =
            wanted.iter().map(|_| Mutex::new(None)).collect();
        source.fetch_many(&wanted, concurrency, &|index, fetch| {
            let directory = fetch_directory(&wanted[index], fetch)?;
            *slots[index].lock().expect("slot") = Some(directory);
            Ok(())
        })?;
        for (digest, slot) in wanted.iter().zip(slots) {
            let directory = slot
                .into_inner()
                .expect("slot")
                .ok_or_else(|| FetchError::Invalid("a source skipped a blob".to_owned()))?;
            loaded.insert(label(digest), directory);
        }
        let mut below = Vec::new();
        for (digest, here) in level {
            let directory = &loaded[&label(&digest)];
            for file in &directory.files {
                let digest = file.digest.clone().ok_or_else(|| {
                    FetchError::Invalid(format!("input file {:?} has no digest", file.name))
                })?;
                let copy = (here.join(&file.name), file.is_executable);
                let at = *blob_at.entry(label(&digest)).or_insert_with(|| {
                    files.push(Copies {
                        digest,
                        copies: Vec::new(),
                    });
                    files.len() - 1
                });
                files[at].copies.push(copy);
            }
            for link in &directory.symlinks {
                let path = here.join(&link.name);
                std::os::unix::fs::symlink(&link.target, &path).map_err(io(&path))?;
            }
            for sub in &directory.directories {
                let path = here.join(&sub.name);
                let digest = sub.digest.clone().ok_or_else(|| {
                    FetchError::Invalid(format!("input directory {:?} has no digest", sub.name))
                })?;
                std::fs::create_dir(&path).map_err(io(&path))?;
                below.push((digest, path));
            }
        }
        level = below;
    }
    Ok(files)
}

/// Streams `blob` to its first path, checked, then makes its other paths from that file. A blob
/// that fails its check is removed from the first path.
fn write_copies(blob: &Copies, fetch: Fetch<'_>) -> Result<(), FetchError> {
    let (first, first_executable) = &blob.copies[0];
    let mut file = create_file(first, *first_executable)?;
    let streamed = stream_checked(&blob.digest, first, &mut file, fetch);
    // Closed before the copies are made from it, or the failed file removed.
    drop(file);
    if let Err(error) = streamed {
        // Best effort: the error is the one to report, and the file is not complete.
        let _ = std::fs::remove_file(first);
        return Err(error);
    }
    for (path, is_executable) in &blob.copies[1..] {
        copy_of(first, path, *is_executable)?;
    }
    Ok(())
}

/// Makes `path` from the finished file `from`: a copy with the mode `path` is to have.
fn copy_of(from: &Path, path: &Path, is_executable: bool) -> Result<(), FetchError> {
    let mut source = std::fs::File::open(from).map_err(io(from))?;
    copy_from(&mut source, path, is_executable)
}

/// Creates `path` (which must not exist) from everything `source` gives. A copy that fails
/// partway is removed: the file is one this call created.
fn copy_from(source: &mut impl Read, path: &Path, is_executable: bool) -> Result<(), FetchError> {
    let mut file = create_file(path, is_executable)?;
    std::io::copy(source, &mut file).map_err(|error| {
        // Best effort: the copy's error is the one to report.
        let _ = std::fs::remove_file(path);
        FetchError::Io {
            path: path.to_owned(),
            source: error,
        }
    })?;
    Ok(())
}

/// Creates `path` with `O_EXCL` and sets the mode exactly (the process umask does not apply).
fn create_file(path: &Path, is_executable: bool) -> Result<std::fs::File, FetchError> {
    let mode = if is_executable { 0o755 } else { 0o644 };
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(mode)
        .open(path)
        .map_err(io(path))?;
    file.set_permissions(std::fs::Permissions::from_mode(mode))
        .map_err(io(path))?;
    Ok(file)
}

#[path = "batch.rs"]
mod batch;

#[path = "locate.rs"]
mod locate;
pub use locate::{
    Counts, HELD_IN_MEMORY, HttpObjectStore, LocatedSource, MAX_LOCATE_BYTES, ObjectStore, Place,
    StoreError, Table, Want,
};

#[cfg(test)]
#[path = "fetch_tests.rs"]
mod tests;
