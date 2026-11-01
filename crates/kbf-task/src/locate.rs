//! Reading inputs where the listener's `GET /locate` points, falling back to the listener per
//! blob.
//!
//! With `--locate` the task asks the listener's `GET /locate` once, before it fetches anything,
//! and reads each input where the answer points: a byte range of a pack object, or a whole large
//! object, straight from the store the answer names. The answer is a hint and never trusted:
//! every byte read from the store passes the same check against its digest as a byte from the
//! listener ([`stream_checked`]), and a blob whose direct read fails in any way (the store is
//! down, the grant is refused or has expired, the read is short or long or wrong, the object
//! is gone) is read from the listener instead. Hashing, the size bound and the exit codes stay
//! in the one fetch path.
//!
//! # The answer
//!
//! Lines of JSON, one object each. The first line, and only the first, is the header, naming the store, its bucket and
//! the read grants (bearer tokens, each good for a few hundred exact keys and for a few minutes):
//!
//! ```text
//! {"store":"http://host:port","bucket":"b","grants":["<token>","<token>"]}
//! ```
//!
//! Each later line is one digest of the run's scope, `g` being the index in `grants` of the grant
//! that lists the blob's key:
//!
//! ```text
//! {"digest":"<hex>/<size>","pack":{"key":"cas/f/...","offset":N,"length":N},"g":0}
//! {"digest":"<hex>/<size>","large":{"key":"cas/l/<hex>","size":N},"g":1}
//! {"digest":"<hex>/<size>","absent":true}
//! ```
//!
//! A line the task cannot use is the same as an `absent` one: a digest that is not well formed or
//! is empty, a length or size that is not the digest's, a key that is not a plain object key (or,
//! for a large object, not `cas/l/<hex>` of the digest), a `g` that names no usable grant. With no
//! grants (an older server, or none minted) nothing is located and the listener serves every
//! blob. A grant is sent to the store only, in `x-tierstore-grant`; it is never written to a log
//! or an error message, and has no readable `Debug`.

use std::collections::HashMap;
use std::io::{Read as _, Seek as _, Write};
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde_json::Value;

use super::*;

/// The most of a `/locate` answer the task reads; a longer one is not used.
pub const MAX_LOCATE_BYTES: u64 = 256 << 20;

/// The longest grant the task takes, which is the longest header the store takes.
const MAX_GRANT_BYTES: usize = 64 * 1024;

/// The most a blob may be to be read into memory; a larger one (a hostile answer can put any
/// size on a pack line) waits in a spool file while it is checked.
pub const HELD_IN_MEMORY: i64 = 256 << 10;

/// The longest object key the task asks for.
const MAX_KEY_BYTES: usize = 1024;

/// Where `/locate` says a blob can be read without the listener.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    /// `length` bytes at `offset` of the pack object `key`.
    Pack {
        /// The object key of the pack.
        key: String,
        /// Where the blob starts in the pack.
        offset: u64,
        /// The blob's length.
        length: u64,
    },
    /// The whole object `key`, which is `cas/l/<hex>` of the blob.
    Large {
        /// The object key.
        key: String,
    },
}

/// A store read grant: a bearer secret, so it has no readable `Debug`.
struct Grant(String);

impl std::fmt::Debug for Grant {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Grant(<redacted>)")
    }
}

/// What a `/locate` answer said, as far as it is usable.
#[derive(Debug, Default)]
pub struct Table {
    /// Each located blob by its `<hex>/<size>`, with the index of its grant.
    places: HashMap<String, (Place, usize)>,
    grants: Vec<Grant>,
    store: Option<(String, String)>,
}

/// Whether `text` can be a grant: visible ASCII only, not empty, within the store's header limit.
fn usable_grant(text: &str) -> bool {
    !text.is_empty() && text.len() <= MAX_GRANT_BYTES && text.bytes().all(|b| b.is_ascii_graphic())
}

/// Whether `key` is a plain object key: `/`-separated names of letters, digits, `-`, `_` and `.`,
/// none empty, `.` or `..`.
fn plain_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= MAX_KEY_BYTES
        && key.split('/').all(|name| {
            !matches!(name, "" | "." | "..")
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
        })
}

/// The digest a line's `digest` field names, if it is a well formed non-empty one.
fn line_digest(value: &Value) -> Option<Digest> {
    let (hash, size) = value.get("digest")?.as_str()?.split_once('/')?;
    let digest = Digest {
        hash: hash.to_owned(),
        size_bytes: size.parse().ok()?,
    };
    (check_digest(&digest).is_ok() && digest.size_bytes > 0).then_some(digest)
}

/// The place a line names for `digest`: a pack span whose length is the digest's size, or the
/// large object `cas/l/<hex>` of the size of the digest.
fn line_place(value: &Value, digest: &Digest) -> Option<Place> {
    let size = digest.size_bytes.unsigned_abs();
    if let Some(pack) = value.get("pack") {
        let key = pack.get("key")?.as_str()?;
        let offset = pack.get("offset")?.as_u64()?;
        let length = pack.get("length")?.as_u64()?;
        let fits = length == size && offset.checked_add(length).is_some() && plain_key(key);
        return fits.then(|| Place::Pack {
            key: key.to_owned(),
            offset,
            length,
        });
    }
    let large = value.get("large")?;
    let key = large.get("key")?.as_str()?;
    let fits = large.get("size")?.as_u64()? == size && key == format!("cas/l/{}", digest.hash);
    fits.then(|| Place::Large {
        key: key.to_owned(),
    })
}

impl Table {
    /// The usable entries of a `/locate` answer. Lines that cannot be used are skipped.
    #[must_use]
    pub fn parse(body: &[u8]) -> Table {
        let mut table = Table::default();
        let mut usable: Vec<bool> = Vec::new();
        for (at, line) in body.split(|b| *b == b'\n').enumerate() {
            let Ok(value) = serde_json::from_slice::<Value>(line) else {
                continue;
            };
            if value.get("digest").is_none() {
                if at == 0 {
                    table.header(&value, &mut usable);
                }
                continue;
            }
            let Some(digest) = line_digest(&value) else {
                continue;
            };
            let Some(place) = line_place(&value, &digest) else {
                continue;
            };
            let grant = value.get("g").and_then(Value::as_u64);
            let Some(grant) = grant
                .and_then(|g| usize::try_from(g).ok())
                .filter(|g| usable.get(*g) == Some(&true))
            else {
                continue;
            };
            table.places.entry(label(&digest)).or_insert((place, grant));
        }
        table
    }

    /// Takes the store, bucket and grants of the header line `value`, if it has all three.
    fn header(&mut self, value: &Value, usable: &mut Vec<bool>) {
        let text = |name: &str| value.get(name).and_then(Value::as_str);
        let grants = value.get("grants").and_then(Value::as_array);
        if let (Some(store), Some(bucket), Some(grants)) = (text("store"), text("bucket"), grants) {
            self.store = Some((store.to_owned(), bucket.to_owned()));
            for grant in grants {
                let grant = grant.as_str().filter(|g| usable_grant(g));
                usable.push(grant.is_some());
                self.grants
                    .push(Grant(grant.unwrap_or_default().to_owned()));
            }
        }
    }

    /// Where `digest` can be read and the grant to read it with, if the answer said so.
    #[must_use]
    pub fn locate(&self, digest: &Digest) -> Option<(&Place, &str)> {
        self.places
            .get(&label(digest))
            .map(|(place, grant)| (place, self.grants[*grant].0.as_str()))
    }

    /// Where `digest` can be read, if the answer said so.
    #[must_use]
    pub fn place(&self, digest: &Digest) -> Option<&Place> {
        self.locate(digest).map(|(place, _)| place)
    }

    /// The store the answer names: its base URL and bucket.
    #[must_use]
    pub fn store(&self) -> Option<(&str, &str)> {
        self.store.as_ref().map(|(u, b)| (u.as_str(), b.as_str()))
    }

    /// How many blobs the answer located.
    #[must_use]
    pub fn len(&self) -> usize {
        self.places.len()
    }

    /// Whether the answer located no blob.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.places.is_empty()
    }
}

/// What to read from the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Want {
    /// The whole object, which is `size` bytes.
    Whole {
        /// The object's length.
        size: u64,
    },
    /// `length` bytes from `offset`.
    Range {
        /// The first byte.
        offset: u64,
        /// How many bytes.
        length: u64,
    },
}

/// Why a read from the store failed.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// The store could not be reached (connection refused, no route, timed out): the rest of the
    /// run does not try it again.
    #[error("{0}")]
    Unreachable(String),
    /// The store answered, or the read broke: refused, not found, short, broken.
    #[error("{0}")]
    Failed(String),
}

/// A store that objects can be read from with a grant.
pub trait ObjectStore: Sync {
    /// Writes what `want` names of the object `key` into `out`, presenting `grant`, as it
    /// arrives. The writer may refuse bytes past what the caller expects; the read then ends.
    /// The error text must not hold the grant.
    ///
    /// # Errors
    /// Why the object could not be read.
    fn read(
        &self,
        key: &str,
        want: Want,
        grant: &str,
        out: &mut dyn Write,
    ) -> Result<(), StoreError>;
}

/// The store over HTTP: `GET <endpoint>/<bucket>/<key>` with the grant in `x-tierstore-grant`
/// (never an `Authorization` header), and a `Range` for a span.
pub struct HttpObjectStore {
    agent: ureq::Agent,
    /// `<endpoint>/<bucket>`.
    base: String,
}

/// Whether `bucket` is a plain bucket name: 1 to 63 lowercase letters, digits, `-` and `.`, not
/// starting or ending with `.` (so never `.` or `..`, which a URL path would resolve).
fn plain_bucket(bucket: &str) -> bool {
    let chars = bucket
        .bytes()
        .all(|b| b.is_ascii_lowercase() | b.is_ascii_digit() | matches!(b, b'-' | b'.'));
    (1..=63).contains(&bucket.len()) & chars & !bucket.starts_with('.') & !bucket.ends_with('.')
}

/// Whether `endpoint` is only visible ASCII without `?`, `#`, `@` (user information) or `\`: the
/// endpoint is a scheme, a host and perhaps a port, nothing a request could be steered with.
fn plain_endpoint(endpoint: &str) -> bool {
    endpoint
        .bytes()
        .all(|b| b.is_ascii_graphic() & !matches!(b, b'?' | b'#' | b'@' | b'\\'))
}

/// Whether `url` starts with `scheme` (which ends in `://`), in any case.
fn has_scheme(url: &str, scheme: &str) -> bool {
    url.get(..scheme.len())
        .is_some_and(|start| start.eq_ignore_ascii_case(scheme))
}

impl HttpObjectStore {
    /// A store at `endpoint` (`https://`, or `http://` with `allow_plain_http`; a trailing `/` is
    /// dropped) for `bucket`, trusting only `roots` (DER certificates).
    ///
    /// # Errors
    /// `roots` is empty, the bucket is not a plain name, or the scheme is not allowed.
    pub fn new(
        endpoint: &str,
        bucket: &str,
        roots: &[Vec<u8>],
        allow_plain_http: bool,
    ) -> Result<Self, FetchError> {
        let refuse = |why: &str| Err(FetchError::Invalid(format!("the store: {why}")));
        if roots.is_empty() {
            return refuse("no trusted root certificates");
        }
        if !plain_bucket(bucket) {
            return refuse("the bucket is not a plain name");
        }
        if !plain_endpoint(endpoint) {
            return refuse("the endpoint holds a character a store address does not");
        }
        if !(has_scheme(endpoint, "https://")
            || (allow_plain_http && has_scheme(endpoint, "http://")))
        {
            return refuse(
                "the endpoint is not an https URL (an http one needs --allow-plain-http-grant)",
            );
        }
        Ok(Self {
            agent: http_agent(roots, !allow_plain_http),
            base: format!("{}/{bucket}", endpoint.trim_end_matches('/')),
        })
    }
}

impl ObjectStore for HttpObjectStore {
    fn read(
        &self,
        key: &str,
        want: Want,
        grant: &str,
        out: &mut dyn Write,
    ) -> Result<(), StoreError> {
        let (range, size, expected) = match want {
            Want::Whole { size } => (None, size, 200),
            Want::Range { offset, length } => {
                let last = offset.saturating_add(length).saturating_sub(1);
                (Some(format!("bytes={offset}-{last}")), length, 206)
            }
        };
        let mut request = self
            .agent
            .get(&format!("{}/{key}", self.base))
            .config()
            .timeout_global(Some(request_timeout(
                i64::try_from(size).unwrap_or(i64::MAX),
            )))
            .build()
            .header("x-tierstore-grant", grant);
        if let Some(range) = &range {
            request = request.header("Range", range);
        }
        let mut response = request.call().map_err(|e| {
            let unreachable = matches!(
                e,
                ureq::Error::Io(_)
                    | ureq::Error::Timeout(_)
                    | ureq::Error::HostNotFound
                    | ureq::Error::ConnectionFailed
            );
            if unreachable {
                StoreError::Unreachable(e.to_string())
            } else {
                StoreError::Failed(e.to_string())
            }
        })?;
        let status = response.status().as_u16();
        if status != expected {
            return Err(StoreError::Failed(format!(
                "the store answered {status}, not {expected}"
            )));
        }
        std::io::copy(&mut response.body_mut().as_reader(), out)
            .map(|_| ())
            .map_err(|e| StoreError::Failed(e.to_string()))
    }
}

/// How many blobs were read each way.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// By a byte range of a pack.
    pub range: u64,
    /// As a whole large object.
    pub large: u64,
    /// By the listener after a read from the store failed.
    pub fallback: u64,
    /// By the listener because `/locate` did not locate them.
    pub absent: u64,
}

impl std::fmt::Display for Counts {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} by range, {} by large object, {} by the listener after a failed read, \
             {} by the listener (not located)",
            self.range, self.large, self.fallback, self.absent
        )
    }
}

const RANGE: usize = 0;
const LARGE: usize = 1;
const FALLBACK: usize = 2;
const ABSENT: usize = 3;

/// A [`BlobSource`] that reads where a [`Table`] points and asks the listener for the rest.
pub struct LocatedSource {
    inner: Box<dyn BlobSource>,
    store: Option<Box<dyn ObjectStore>>,
    table: Table,
    counts: [AtomicU64; 4],
    /// Why `/locate` was not used, or the first direct read that failed.
    failure: Mutex<Option<String>>,
    /// Where a large blob is held while it is checked.
    spool: PathBuf,
    /// Set by the first read that could not reach the store: the rest go to the listener.
    down: AtomicBool,
}

/// A blob read from the store and checked: held in memory, or in an unlinked spool file.
enum Ready {
    Bytes(Vec<u8>),
    Spooled(std::fs::File),
}

/// Makes an unlinked file in `dir`: the checked bytes of a large blob wait in it until they are
/// copied on, so a blob that fails its check never reaches the writer it was read for. The
/// unlink cannot fail in a way that matters: a file left behind holds only input bytes.
fn spool_file(dir: &Path) -> Result<std::fs::File, FetchError> {
    static SPOOLS: AtomicU64 = AtomicU64::new(0);
    let name = format!(
        ".locate-spool-{}-{}",
        std::process::id(),
        SPOOLS.fetch_add(1, Ordering::SeqCst)
    );
    let path = dir.join(name);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(io(&path))?;
    let _ = std::fs::remove_file(&path);
    Ok(file)
}

/// Reads from `store` into `out`, an error naming the digest. A read that could not reach the
/// store sets `down`, so that no other blob tries it. Not generic.
fn read_store(
    store: &dyn ObjectStore,
    down: &AtomicBool,
    key: &str,
    want: Want,
    grant: &str,
    digest: &Digest,
    out: &mut dyn Write,
) -> Result<(), FetchError> {
    store.read(key, want, grant, out).map_err(|error| {
        down.fetch_or(
            matches!(error, StoreError::Unreachable(_)),
            Ordering::SeqCst,
        );
        FetchError::Source {
            digest: label(digest),
            why: error.to_string(),
        }
    })
}

impl LocatedSource {
    /// A source over `inner` that reads from `store` where `table` points; large objects wait in
    /// `spool` while they are checked. With no store every blob is the listener's.
    #[must_use]
    pub fn new(
        inner: Box<dyn BlobSource>,
        store: Option<Box<dyn ObjectStore>>,
        table: Table,
        spool: PathBuf,
    ) -> Self {
        Self {
            inner,
            store,
            table,
            counts: Default::default(),
            failure: Mutex::new(None),
            spool,
            down: AtomicBool::new(false),
        }
    }

    /// Asks `listener` for its table once, then builds the source over it and the store the
    /// answer names. An answer that cannot be had or used leaves every blob to the listener, and
    /// the report says why.
    #[must_use]
    pub fn open(
        listener: HttpBlobSource,
        roots: &[Vec<u8>],
        allow_plain_http: bool,
        spool: PathBuf,
    ) -> Self {
        let (table, store, note) = match listener.locate() {
            Err(error) => (Table::default(), None, Some(error.to_string())),
            Ok(body) => {
                let table = Table::parse(&body);
                let store = table.store().map(|(url, bucket)| {
                    HttpObjectStore::new(url, bucket, roots, allow_plain_http)
                });
                match store {
                    Some(Ok(store)) => {
                        let store: Box<dyn ObjectStore> = Box::new(store);
                        (table, Some(store), None)
                    }
                    Some(Err(error)) => (Table::default(), None, Some(error.to_string())),
                    None => (table, None, None),
                }
            }
        };
        let source = Self::new(Box::new(listener), store, table, spool);
        *source.failure.lock().expect("failure") = note;
        source
    }

    /// How many blobs were read each way so far.
    #[must_use]
    pub fn counts(&self) -> Counts {
        let count = |which: usize| self.counts[which].load(Ordering::SeqCst);
        Counts {
            range: count(RANGE),
            large: count(LARGE),
            fallback: count(FALLBACK),
            absent: count(ABSENT),
        }
    }

    fn count(&self, which: usize, by: u64) {
        self.counts[which].fetch_add(by, Ordering::SeqCst);
    }

    /// The store, place and grant for `digest`, if all three are there.
    fn located(&self, digest: &Digest) -> Option<(&dyn ObjectStore, &Place, &str)> {
        let store = self.store.as_deref()?;
        let (place, grant) = self.table.locate(digest)?;
        Some((store, place, grant))
    }

    /// Reads `digest` from the store, checked against it: held in memory when it is small, in
    /// the spool when it is not, whatever the answer says it is.
    fn read_located(
        &self,
        store: &dyn ObjectStore,
        place: &Place,
        grant: &str,
        digest: &Digest,
    ) -> Result<Ready, FetchError> {
        let (key, want, counted) = match place {
            Place::Pack {
                key,
                offset,
                length,
            } => (
                key,
                Want::Range {
                    offset: *offset,
                    length: *length,
                },
                RANGE,
            ),
            Place::Large { key } => (
                key,
                Want::Whole {
                    size: digest.size_bytes.unsigned_abs(),
                },
                LARGE,
            ),
        };
        let mut read =
            |out: &mut dyn Write| read_store(store, &self.down, key, want, grant, digest, out);
        let ready = if digest.size_bytes <= HELD_IN_MEMORY {
            Ready::Bytes(verified_bytes(digest, &mut read)?)
        } else {
            let mut spool = spool_file(&self.spool)?;
            stream_checked(digest, &self.spool, &mut spool, &mut read)?;
            spool.rewind().map_err(io(&self.spool))?;
            Ready::Spooled(spool)
        };
        self.count(counted, 1);
        Ok(ready)
    }
}

/// Writes a checked blob into `out`. Not generic.
fn deliver(ready: Ready, digest: &Digest, out: &mut dyn Write) -> Result<(), FetchError> {
    match ready {
        Ready::Bytes(bytes) => write_whole(Ok(bytes), digest, out),
        Ready::Spooled(mut file) => {
            std::io::copy(&mut file, out)
                .map(|_| ())
                .map_err(|e| FetchError::Source {
                    digest: label(digest),
                    why: format!("the bytes could not be taken: {e}"),
                })
        }
    }
}

impl BlobSource for LocatedSource {
    fn get(&self, digest: &Digest) -> Result<Vec<u8>, FetchError> {
        let mut bytes = Vec::new();
        self.fetch_to(digest, &mut bytes)?;
        Ok(bytes)
    }

    fn fetch_to(&self, digest: &Digest, out: &mut dyn Write) -> Result<(), FetchError> {
        let Some((store, place, grant)) = self.located(digest) else {
            self.count(ABSENT, 1);
            return self.inner.fetch_to(digest, out);
        };
        if self.down.load(Ordering::SeqCst) {
            self.count(FALLBACK, 1);
            return self.inner.fetch_to(digest, out);
        }
        match self.read_located(store, place, grant, digest) {
            Ok(ready) => deliver(ready, digest, out),
            Err(error) => {
                self.failure
                    .lock()
                    .expect("failure")
                    .get_or_insert_with(|| error.to_string());
                self.count(FALLBACK, 1);
                self.inner.fetch_to(digest, out)
            }
        }
    }

    /// The located blobs first, each from the store (or, failing that, the listener) on the
    /// bounded pool; then the rest in one call of the listener's own `fetch_many`, which batches
    /// if the listener does.
    fn fetch_many(
        &self,
        digests: &[Digest],
        concurrency: usize,
        each: &Each<'_>,
    ) -> Result<(), FetchError> {
        let (located, rest): (Vec<usize>, Vec<usize>) =
            (0..digests.len()).partition(|index| self.located(&digests[*index]).is_some());
        run_bounded(concurrency, &located, &|_, index| {
            each(*index, &mut |out| self.fetch_to(&digests[*index], out))
        })?;
        let unlocated: Vec<Digest> = rest.iter().map(|index| digests[*index].clone()).collect();
        self.count(ABSENT, unlocated.len() as u64);
        self.inner
            .fetch_many(&unlocated, concurrency, &|at, fetch| each(rest[at], fetch))
    }

    fn report(&self) -> Option<String> {
        let failure = self.failure.lock().expect("failure");
        let why = failure
            .as_ref()
            .map(|why| format!("; first failure: {why}"))
            .unwrap_or_default();
        Some(format!("inputs read: {}{why}", self.counts()))
    }
}

impl HttpBlobSource {
    /// The listener's `GET /locate` answer: one request, with the bearer token.
    ///
    /// # Errors
    /// The listener could not be reached, did not answer 200, or answered more than
    /// [`MAX_LOCATE_BYTES`].
    pub fn locate(&self) -> Result<Vec<u8>, FetchError> {
        self.locate_within(MAX_LOCATE_BYTES)
    }

    /// [`locate`](Self::locate) with the answer capped at `max` bytes.
    pub(super) fn locate_within(&self, max: u64) -> Result<Vec<u8>, FetchError> {
        let invalid = |why: String| FetchError::Invalid(format!("/locate: {why}"));
        let mut response = self
            .agent
            .get(&format!("{}/locate", self.base))
            .config()
            .timeout_global(Some(request_timeout(
                i64::try_from(max).unwrap_or(i64::MAX),
            )))
            .build()
            .header("Authorization", &self.authorization)
            .call()
            .map_err(|e| invalid(e.to_string()))?;
        let status = response.status().as_u16();
        if status != 200 {
            return Err(invalid(format!("the listener answered {status}")));
        }
        let mut body = Vec::new();
        response
            .body_mut()
            .as_reader()
            .take(max + 1)
            .read_to_end(&mut body)
            .map_err(|e| invalid(e.to_string()))?;
        if body.len() as u64 > max {
            return Err(invalid(format!("the answer is over {max} bytes")));
        }
        Ok(body)
    }
}
