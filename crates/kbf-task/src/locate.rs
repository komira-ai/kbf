//! Reading inputs where the listener's `GET /locate` points, falling back to the listener per
//! blob.
//!
//! (Interface only: the bodies are filled in by the next commit.)

use std::collections::HashMap;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::AtomicU64;

use super::*;

/// The most of a `/locate` answer the task reads; a longer one is not used.
pub const MAX_LOCATE_BYTES: u64 = 256 << 20;

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
    /// The whole object `key`, which is `cas/l/<hex>`.
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
    places: HashMap<String, (Place, usize)>,
    grants: Vec<Grant>,
    store: Option<(String, String)>,
}

impl Table {
    /// The usable entries of a `/locate` answer.
    #[must_use]
    pub fn parse(_body: &[u8]) -> Table {
        Table::default()
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

/// A store that objects can be read from with a grant.
pub trait ObjectStore: Sync {
    /// Writes what `want` names of the object `key` into `out`, presenting `grant`.
    ///
    /// # Errors
    /// Why the object could not be read.
    fn read(&self, key: &str, want: Want, grant: &str, out: &mut dyn Write) -> Result<(), String>;
}

/// The store over HTTP: `GET <endpoint>/<bucket>/<key>` with `x-tierstore-grant`.
pub struct HttpObjectStore {
    _agent: ureq::Agent,
}

impl HttpObjectStore {
    /// A store at `endpoint` for `bucket`.
    ///
    /// # Errors
    /// No roots, or a bucket name that is not a plain one.
    pub fn new(
        _endpoint: &str,
        _bucket: &str,
        roots: &[Vec<u8>],
        allow_plain_http: bool,
    ) -> Result<Self, FetchError> {
        Ok(Self {
            _agent: http_agent(roots, !allow_plain_http),
        })
    }
}

impl ObjectStore for HttpObjectStore {
    fn read(&self, _: &str, _: Want, _: &str, _: &mut dyn Write) -> Result<(), String> {
        Err("not built".to_owned())
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
        write!(f, "{self:?}")
    }
}

/// A [`BlobSource`] that reads where a [`Table`] points and asks the listener for the rest.
pub struct LocatedSource {
    inner: Box<dyn BlobSource>,
    _store: Option<Box<dyn ObjectStore>>,
    _counts: [AtomicU64; 4],
    _failure: Mutex<Option<String>>,
    _spool: PathBuf,
}

impl LocatedSource {
    /// A source over `inner` that reads from `store` where `table` points. With no store every
    /// blob is the listener's.
    #[must_use]
    pub fn new(
        inner: Box<dyn BlobSource>,
        store: Option<Box<dyn ObjectStore>>,
        _table: Table,
        spool: PathBuf,
    ) -> Self {
        Self {
            inner,
            _store: store,
            _counts: Default::default(),
            _failure: Mutex::new(None),
            _spool: spool,
        }
    }

    /// Asks `listener` for its table once, then builds the source over it and the store the
    /// answer names.
    #[must_use]
    pub fn open(
        listener: HttpBlobSource,
        roots: &[Vec<u8>],
        allow_plain_http: bool,
        spool: PathBuf,
    ) -> Self {
        let _ = (roots, allow_plain_http);
        Self::new(Box::new(listener), None, Table::default(), spool)
    }

    /// How many blobs were read each way so far.
    #[must_use]
    pub fn counts(&self) -> Counts {
        Counts::default()
    }
}

impl BlobSource for LocatedSource {
    fn get(&self, digest: &Digest) -> Result<Vec<u8>, FetchError> {
        self.inner.get(digest)
    }

    fn fetch_to(&self, digest: &Digest, out: &mut dyn Write) -> Result<(), FetchError> {
        self.inner.fetch_to(digest, out)
    }

    fn fetch_many(
        &self,
        digests: &[Digest],
        concurrency: usize,
        each: &Each<'_>,
    ) -> Result<(), FetchError> {
        self.inner.fetch_many(digests, concurrency, each)
    }

    fn report(&self) -> Option<String> {
        None
    }
}

impl HttpBlobSource {
    /// The listener's `GET /locate` answer.
    ///
    /// # Errors
    /// The listener could not be reached or did not answer 200.
    pub fn locate(&self) -> Result<Vec<u8>, FetchError> {
        self.locate_within(MAX_LOCATE_BYTES)
    }

    /// [`locate`](Self::locate) with the answer capped at `max` bytes.
    pub(super) fn locate_within(&self, _max: u64) -> Result<Vec<u8>, FetchError> {
        let _ = &self.base;
        Err(FetchError::Invalid("not built".to_owned()))
    }
}
