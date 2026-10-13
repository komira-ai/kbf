//! STAND-IN for write grouping (`docs/design/write-path.md`, section 3), so the tests
//! that pin it compile and fail first: the API they name, with the behaviour of the
//! cache before grouping. Each call still writes its own segments, nothing is admitted
//! or retried, and a call's failure is every blob's answer. The next commit replaces
//! this file.

use std::num::{NonZeroU32, NonZeroUsize};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use kbf_objstore::ObjectStore;
use tokio::sync::OwnedSemaphorePermit;

use super::{Cache, CacheError, VerifiedBlob};
use crate::meta_log::MetaLog;

/// The wait before the first retry of a failed segment PUT.
pub const PUT_BACKOFF: Duration = Duration::from_millis(100);
/// How much longer each wait between PUT attempts is than the one before.
pub const PUT_BACKOFF_FACTOR: u32 = 4;
/// The most HTTP/2 streams per connection on either listener.
pub const MAX_CONCURRENT_STREAMS: u32 = 200;
/// The HTTP/2 flow-control window of each stream on either listener.
pub const STREAM_WINDOW_BYTES: u32 = 1 << 20;
/// The HTTP/2 flow-control window of each connection on either listener.
pub const CONNECTION_WINDOW_BYTES: u32 = MAX_CONCURRENT_STREAMS * STREAM_WINDOW_BYTES;

/// How the cache groups and bounds uploads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WriteConfig {
    /// `--write-linger`.
    pub linger: Duration,
    /// `--write-own-segment-bytes`.
    pub own_segment_bytes: u64,
    /// `--write-bytes-in-flight`.
    pub bytes_in_flight: u64,
    /// `--write-puts-in-flight`.
    pub puts_in_flight: NonZeroUsize,
    /// `--store-put-attempts`.
    pub put_attempts: NonZeroU32,
}

impl WriteConfig {
    /// The defaults of `docs/design/write-path.md`.
    pub const DEFAULT: Self = Self {
        linger: Duration::from_millis(2),
        own_segment_bytes: 16 << 20,
        bytes_in_flight: 2 << 30,
        puts_in_flight: NonZeroUsize::MIN.saturating_add(3),
        put_attempts: NonZeroU32::MIN.saturating_add(2),
    };
}

impl Default for WriteConfig {
    fn default() -> Self {
        Self::DEFAULT
    }
}

/// Bytes admitted (the stand-in admits nothing).
#[derive(Clone, Debug, Default)]
pub struct Admitted {
    _permit: Option<Arc<OwnedSemaphorePermit>>,
}

#[derive(Debug)]
pub(super) struct Group {
    buffered: AtomicU64,
    buffered_peak: AtomicU64,
}

impl Group {
    pub(super) const fn new(_config: WriteConfig) -> Self {
        Self {
            buffered: AtomicU64::new(0),
            buffered_peak: AtomicU64::new(0),
        }
    }
}

/// Bytes a ByteStream `Write` holds while it receives a blob.
#[derive(Debug)]
pub(crate) struct Buffering<'a> {
    group: &'a Group,
    held: u64,
}

impl Buffering<'_> {
    pub(crate) fn add(&mut self, n: u64) {
        self.held += n;
        let now = self.group.buffered.fetch_add(n, Ordering::Relaxed) + n;
        self.group.buffered_peak.fetch_max(now, Ordering::Relaxed);
    }
}

impl Drop for Buffering<'_> {
    fn drop(&mut self) {
        self.group.buffered.fetch_sub(self.held, Ordering::Relaxed);
    }
}

impl<M: MetaLog, O: ObjectStore> Cache<M, O> {
    /// The same cache under `config` (the stand-in ignores it).
    #[must_use]
    pub fn with_writes(mut self, config: WriteConfig) -> Self {
        self.group = Arc::new(Group::new(config));
        self
    }

    /// Admits nothing and never waits.
    pub async fn admit(&self, _bytes: u64) -> Admitted {
        Admitted::default()
    }

    /// The most bytes ByteStream `Write`s have held in their receive buffers at once.
    pub fn write_buffer_peak(&self) -> u64 {
        self.group.buffered_peak.load(Ordering::Relaxed)
    }

    pub(crate) fn buffering(&self) -> Buffering<'_> {
        Buffering {
            group: &self.group,
            held: 0,
        }
    }

    /// [`Cache::store_blobs`].
    ///
    /// # Errors
    /// As [`Cache::store_blobs`].
    pub async fn store_admitted(
        &self,
        blobs: Vec<VerifiedBlob>,
        _admitted: Admitted,
    ) -> Result<(), CacheError> {
        self.store_blobs(blobs).await
    }

    /// [`Cache::store_blobs`], its one answer given to every blob.
    ///
    /// # Errors
    /// Never; each blob's answer is the call's.
    pub async fn store_each(
        &self,
        blobs: Vec<VerifiedBlob>,
        _admitted: Admitted,
    ) -> Result<Vec<Result<(), CacheError>>, CacheError> {
        let n = blobs.len();
        let answer = self.store_blobs(blobs).await;
        Ok(vec![answer; n])
    }
}
