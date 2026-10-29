//! The proxy: a store over a hot and a cold backend.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use futures::{StreamExt, stream};
use tokio::sync::{Mutex as AsyncMutex, Notify, OnceCell};
use tokio::time::Instant;

use crate::journal::{
    USAGE_KEY, Usage, encode_journal, is_reserved, journal_key, journal_prefix, load_checkpoint,
    proxy_key, seq_of,
};
use crate::{
    Backend, ByteRange, Clock, Error, JournalEntry, JournalOp, Listed, Meta, OpenUpload, PartBody,
    UploadsAfter,
};

/// The most calls to the cold tier that one listing of uploads makes to fill a page that the
/// filter of reserved keys cut short.
const UPLOAD_LISTING_CALLS: u32 = 100;

/// The settings of a [`Proxy`].
#[derive(Clone)]
pub struct ProxyConfig {
    /// Names the proxy's journal and its checkpoint. It is not empty and has no `/`. Every
    /// running proxy has an id of its own, and a proxy that restarts keeps its id.
    pub proxy_id: String,
    /// The bytes the hot tier is meant to hold, which the usage the reconciler publishes is
    /// compared with.
    pub hot_budget: u64,
    /// The share of the budget (0.95, say) above which a put is delayed by `back_pressure`
    /// and a read does not copy the object it found in the cold tier into the hot tier.
    pub critical: f64,
    /// How long a put waits before it is written to the hot tier while the usage is above the
    /// critical share.
    pub back_pressure: Duration,
    /// The most entries one journal object holds. A batch that reaches it is written at once.
    pub batch_entries: usize,
    /// How long a batch waits for more entries before it is written.
    pub linger: Duration,
    /// How long the usage object that was read is used before it is read again, and how long
    /// the hot tier is taken to be full after it said so.
    pub usage_refresh: Duration,
    /// The age of the usage object past which it is ignored, as if it were missing.
    pub usage_stale_after: Duration,
    /// The time, for the entries and for the age of the usage object.
    pub clock: Arc<dyn Clock>,
}

impl ProxyConfig {
    /// The settings for a proxy `proxy_id` and a hot tier of `hot_budget` bytes, with a
    /// critical share of 0.95, a back-pressure of 20 ms, batches of at most 64 entries that
    /// wait 50 ms, a usage object that is read every second and ignored after 30 seconds.
    #[must_use]
    pub fn new(proxy_id: &str, hot_budget: u64, clock: Arc<dyn Clock>) -> Self {
        Self {
            proxy_id: proxy_id.to_owned(),
            hot_budget,
            critical: 0.95,
            back_pressure: Duration::from_millis(20),
            batch_entries: 64,
            linger: Duration::from_millis(50),
            usage_refresh: Duration::from_secs(1),
            usage_stale_after: Duration::from_secs(30),
            clock,
        }
    }
}

/// How full the hot tier is taken to be, least full first.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum Level {
    Open,
    Critical,
    Full,
}

/// What the proxy believes of the hot tier's capacity.
#[derive(Default)]
struct Capacity {
    /// When the usage object was last read, and the bytes it gave if it gave any.
    read: Option<(Instant, Option<u64>)>,
    /// Until when the hot tier is taken to be full, after it said so.
    full_until: Option<Instant>,
}

/// One journal object being filled, and the outcome of writing it.
#[derive(Default)]
struct Batch {
    entries: Mutex<Vec<JournalEntry>>,
    /// Notified when the batch reaches its size, to cut the linger short.
    full: Notify,
    /// Set once, by whichever caller waiting on the batch writes it.
    outcome: OnceCell<Result<(), Error>>,
}

fn locked<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // No method panics while holding the lock, so a poisoned state is still whole.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn is_data(object: &Listed) -> bool {
    !is_reserved(&object.key)
}

fn refuse_reserved(key: &str) -> Result<(), Error> {
    if is_reserved(key) {
        Err(Error::Reserved(key.to_owned()))
    } else {
        Ok(())
    }
}

/// A store over a HOT and a COLD [`Backend`] that is itself a [`Backend`].
///
/// The proxy needs no state for correctness: any number of proxies, each with an id of its
/// own, work over the same two backends, and one that restarts finds what it needs in them.
/// Objects are immutable. "Flushed" means that the cold tier holds the object, and a
/// [`Reconciler`](crate::Reconciler) does the flushing from the journals the proxies write.
///
/// - **Get** reads the hot tier, else the cold tier. An object it read from the cold tier is
///   copied back into the hot tier, create-only (it is fine if another read got there
///   first), unless the hot tier is above the critical mark; the copy is journaled, in the
///   background, so that the reconciler can evict it again. A failed copy-back does not
///   fail the read. A ranged read that misses the hot tier reads the whole object from the
///   cold tier, since the hot tier takes the whole of it, and cuts the range from that.
/// - **Head** reads the hot tier, else the cold tier, and copies nothing back.
/// - **Put** is refused with [`Error::AlreadyExists`] if the key exists in the cold tier (it
///   may have been evicted from the hot tier) or in the hot tier. The cold tier is asked
///   first (a HEAD); then the object is written to the hot tier, create-only; then the cold
///   tier is asked again, and if it now holds the key (a copy to the cold tier and a delete from
///   the hot tier came between the first ask and the create) the object just created is removed
///   from the hot tier and the put is refused, since the reconciler copies an object to the cold
///   tier before it deletes it from the hot tier, so a create that wins the hot tier after that
///   delete always finds the cold copy; then an entry
///   is appended to the journal; then the put is acknowledged. **The acknowledgement waits
///   for the journal object that holds the entry to be written to the hot tier**: an object
///   that is acknowledged and not journaled would be in no one's list of objects to copy to
///   the cold tier, and so could be lost with the hot tier. A put whose journal append fails
///   is not acknowledged, and the object it wrote to the hot tier is removed, best effort:
///   left there it would never be copied, and would make a retry of the put fail with
///   [`Error::AlreadyExists`].
/// - **Streamed put** ([`Backend::put_stream`]) is a put whose body arrives as chunks and is
///   never held whole: the chunks go on to the hot tier's own streamed put (or the cold tier's,
///   where the hot tier is full), which makes the object visible, create-only, only after the
///   last chunk, and the journal entry is appended after that. A body that ends with an error
///   (a checksum that does not match, a connection that was cut) leaves no object in either
///   tier and no journal entry. The checks of a put (the key exists in either tier, reserved
///   keys) are made before the first chunk is taken. If the hot tier answers [`Error::Full`]
///   after it took some of the body, the bytes cannot be offered to the cold tier, so the put
///   fails with [`Error::Full`] (the tier is latched full, and the client's retry goes to the
///   cold tier); if it took none, the cold tier gets the whole body as for a held put.
/// - **Delete** removes the object from the hot tier and from the cold tier, then appends a
///   delete entry, which lets the reconciler undo a copy to the cold tier that was under way.
/// - **List** is the union of both tiers without duplicates (the hot tier's entry where both
///   hold the key), in key order, without the keys under the reserved prefixes `_journal/` and
///   `_tiering/`. [`Backend::list_page`] pages through the merged listing: the last key of a
///   page is the continuation token, and `max_keys` is kept to. A put or a delete of a key
///   under a reserved prefix is refused with [`Error::Reserved`].
/// - **Multipart uploads** (`create_upload`, `upload_part`, `complete_upload`, `abort_upload`)
///   go straight to the cold tier: the parts are passed on as they arrive, so the proxy holds no
///   part, in memory or on disk, and the uploads are the cold tier's own (they outlive the
///   proxy where the cold tier keeps them). The completion is a create-only put to the cold
///   tier, and so is refused with [`Error::AlreadyExists`] where either tier holds the key. It
///   journals nothing, as a put that goes to the cold tier of a full hot tier journals nothing:
///   the object is in the cold tier, so a get, a head, a listing and a delete find it, and the
///   reconciler has nothing to copy. It is in the hot tier only after a get copies it back.
///   The cold tier must do multipart uploads (the provided methods of [`Backend`] do not).
///   [`Backend::list_uploads`] lists the cold tier's open uploads, which are the only ones: the
///   proxy keeps no state of an upload.
///
/// The journal is written in batches of at most [`ProxyConfig::batch_entries`] entries, each
/// batch after [`ProxyConfig::linger`] at most, as `_journal/<proxy_id>/<seq>`, create-only,
/// one object at a time. The numbers of a proxy go up by one from the first; a proxy that
/// starts lists its own journal prefix and its checkpoint, and goes on after the highest
/// number it finds.
///
/// **Capacity.** The proxy reads the usage that the reconciler publishes (`_tiering/usage`,
/// at most every [`ProxyConfig::usage_refresh`]); a usage that is missing, unreadable or
/// older than [`ProxyConfig::usage_stale_after`] counts as not full. Above
/// [`ProxyConfig::critical`] of the budget a put waits [`ProxyConfig::back_pressure`] first.
/// At the budget, or when the hot tier answers a put (of the object, or of the journal object
/// that holds its entry) with [`Error::Full`], puts go straight to the cold tier, create-only
/// and unjournaled, and never fail for lack of room in the hot tier; reads still use both
/// tiers, and the hot tier is used again once the usage falls (or a
/// [`ProxyConfig::usage_refresh`] after it said it was full).
///
/// **Known limits.**
///
/// - A put that is dropped (its caller gave up) after the object reached the hot tier and
///   before its journal object was written leaves the object in the hot tier with no entry:
///   it is never copied or evicted, and a retry of the put is refused as existing. Bound the
///   calls inside the backends rather than dropping a put.
/// - The checks and the write of a put are not atomic. Two puts of the same key at once, one to
///   the hot tier and one (the hot tier being full for it, or having just said so) to the cold
///   tier, may both succeed, whether through one proxy or two; a presigned PUT of the S3 front
///   never takes that path ([`Backend::put_stream_hot_only`] answers [`Error::Full`] instead), so the
///   hot tier's create-only write alone decides it. For the other keys the race remains: the
///   reconciler then counts the cold tier's object as the copy of the hot tier's, and the object
///   of the cold tier is the one that stays.
/// - A get that copies an object back into the hot tier while it is deleted may put it back
///   there, with a put entry after the delete entry.
pub struct Proxy {
    inner: Arc<Inner>,
}

/// The body of a streamed put, which more than one tier may be offered in turn: a view of it is
/// a stream of its chunks, and the next view goes on where the last one stopped. The proxy
/// counts the bytes that were taken (the size of the object, for the journal), and a tier that
/// refuses the put before it took a byte is followed by the other tier, which gets the whole
/// body.
struct Offered<'a> {
    /// The rest of the body; `None` once it has ended or failed.
    rest: AsyncMutex<Option<PartBody<'a>>>,
    taken: AtomicU64,
}

impl<'a> Offered<'a> {
    fn new(body: PartBody<'a>) -> Self {
        Self {
            rest: AsyncMutex::new(Some(body)),
            taken: AtomicU64::new(0),
        }
    }

    /// The number of bytes of the body taken so far.
    fn taken(&self) -> u64 {
        self.taken.load(Ordering::SeqCst)
    }

    /// The rest of the body as a stream. It ends after the end of the body or an error of it.
    fn view(&self) -> PartBody<'_> {
        stream::unfold(self, |offered| async move {
            let mut rest = offered.rest.lock().await;
            let next = rest.as_mut()?.next().await;
            match &next {
                Some(Ok(chunk)) => {
                    offered
                        .taken
                        .fetch_add(chunk.len() as u64, Ordering::SeqCst);
                }
                Some(Err(_)) | None => *rest = None,
            }
            drop(rest);
            Some((next?, offered))
        })
        .boxed()
    }
}

struct Inner {
    hot: Arc<dyn Backend>,
    cold: Arc<dyn Backend>,
    config: ProxyConfig,
    /// The batch that takes the next entry, if there is one.
    open: Mutex<Option<Arc<Batch>>>,
    /// The number of the next journal object. It is held while an object is written, so that
    /// the objects are written one at a time, and a number is used again if its write failed.
    next_seq: AsyncMutex<u64>,
    capacity: Mutex<Capacity>,
}

impl Proxy {
    /// Starts a proxy over `hot` and `cold`.
    ///
    /// It lists the journal objects of its own id in the hot tier, reads its checkpoint, and
    /// tells the reconciler that it has a journal.
    ///
    /// # Errors
    /// [`Error::Other`] if the proxy id is empty or has a `/`, or the checkpoint is not
    /// readable; the error of the hot tier if it fails one of those calls.
    pub async fn open(
        hot: Arc<dyn Backend>,
        cold: Arc<dyn Backend>,
        config: ProxyConfig,
    ) -> Result<Self, Error> {
        let id = config.proxy_id.as_str();
        if id.is_empty() || id.contains('/') {
            return Err(Error::Other(format!("invalid proxy id {id:?}")));
        }
        let prefix = journal_prefix(id);
        let listed = hot.list(&prefix).await?;
        let newest = listed
            .iter()
            .filter_map(|object| seq_of(&prefix, &object.key))
            .max();
        let checkpoint = load_checkpoint(hot.as_ref(), id).await?;
        match hot.put(&proxy_key(id), Bytes::new()).await {
            Ok(()) | Err(Error::AlreadyExists(_)) => {}
            Err(error) => return Err(error),
        }
        let next = newest.unwrap_or(0).max(checkpoint.seq) + 1;
        let inner = Inner {
            hot,
            cold,
            config,
            open: Mutex::new(None),
            next_seq: AsyncMutex::new(next),
            capacity: Mutex::new(Capacity::default()),
        };
        Ok(Self {
            inner: Arc::new(inner),
        })
    }
}

impl Inner {
    fn entry(&self, op: JournalOp, key: &str, size: u64) -> JournalEntry {
        JournalEntry {
            op,
            key: key.to_owned(),
            size,
            time: self.config.clock.now(),
        }
    }

    /// Adds an entry to the open batch, which becomes the batch that is written when it is
    /// full.
    fn join(&self, entry: JournalEntry) -> Arc<Batch> {
        let mut open = locked(&self.open);
        let batch = Arc::clone(open.get_or_insert_with(Arc::default));
        let count = {
            let mut entries = locked(&batch.entries);
            entries.push(entry);
            entries.len()
        };
        if count >= self.config.batch_entries {
            *open = None;
            batch.full.notify_one();
        }
        batch
    }

    /// Writes the batch as the next journal object: after its linger, or at once if it is
    /// full. A number that is taken (by a write that failed after it reached the hot tier)
    /// is skipped.
    async fn write_batch(&self, batch: &Arc<Batch>) -> Result<(), Error> {
        let wait = tokio::time::timeout(self.config.linger, batch.full.notified());
        let _ = wait.await;
        {
            let mut open = locked(&self.open);
            let _ = open.take_if(|current| Arc::ptr_eq(current, batch));
        }
        let entries = locked(&batch.entries).clone();
        let body = encode_journal(&entries);
        let mut next = self.next_seq.lock().await;
        loop {
            let key = journal_key(&self.config.proxy_id, *next);
            match self.hot.put(&key, body.clone()).await {
                Ok(()) => {
                    *next += 1;
                    return Ok(());
                }
                Err(Error::AlreadyExists(_)) => *next += 1,
                Err(error) => return Err(error),
            }
        }
    }

    /// Appends an entry to the journal: waits until the journal object that holds it is
    /// written to the hot tier. Callers that wait on one batch share its outcome, and if the
    /// one that writes it is dropped, another takes over.
    async fn append(&self, entry: JournalEntry) -> Result<(), Error> {
        let batch = self.join(entry);
        let outcome = batch.outcome.get_or_init(|| self.write_batch(&batch));
        outcome.await.clone()
    }

    /// Journals a put of an object that the hot tier holds, and takes the object out of the
    /// hot tier if that fails.
    async fn journal_put(&self, key: &str, size: u64) -> Result<(), Error> {
        let appended = self.append(self.entry(JournalOp::Put, key, size)).await;
        if appended.is_err() {
            let _ = self.hot.delete(key).await;
        }
        appended
    }

    fn latch_full(&self) {
        let until = Instant::now() + self.config.usage_refresh;
        locked(&self.capacity).full_until = Some(until);
    }

    fn is_fresh(&self, usage: &Usage) -> bool {
        let age = self.config.clock.now().saturating_sub(usage.time);
        u128::from(age) <= self.config.usage_stale_after.as_millis()
    }

    async fn refresh_usage(&self, now: Instant) -> Option<u64> {
        let fetched = self.hot.get(USAGE_KEY, None).await.ok();
        let usage = fetched.and_then(|body| serde_json::from_slice::<Usage>(&body).ok());
        let hot_bytes = usage.filter(|u| self.is_fresh(u)).map(|u| u.hot_bytes);
        locked(&self.capacity).read = Some((now, hot_bytes));
        hot_bytes
    }

    fn classify(&self, hot_bytes: Option<u64>) -> Level {
        let budget = self.config.hot_budget;
        let critical = (self.config.critical * budget as f64) as u64;
        match hot_bytes {
            Some(bytes) if bytes >= budget => Level::Full,
            Some(bytes) if bytes >= critical => Level::Critical,
            _ => Level::Open,
        }
    }

    async fn level(&self) -> Level {
        let now = Instant::now();
        let (full_until, read) = {
            let capacity = locked(&self.capacity);
            (capacity.full_until, capacity.read)
        };
        if full_until.is_some_and(|until| now < until) {
            return Level::Full;
        }
        let hot_bytes = match read {
            Some((at, bytes)) if now.duration_since(at) < self.config.usage_refresh => bytes,
            _ => self.refresh_usage(now).await,
        };
        self.classify(hot_bytes)
    }

    /// Checks that the hot tier has no object `key`: the hot tier is asked before the cold tier
    /// is written, since a put is refused where either tier holds the key.
    async fn check_not_in_hot(&self, key: &str) -> Result<(), Error> {
        match self.hot.head(key).await {
            Ok(_) => Err(Error::AlreadyExists(key.to_owned())),
            Err(Error::NotFound(_)) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Writes the object to the cold tier, create-only, when the hot tier is full.
    async fn put_cold(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.check_not_in_hot(key).await?;
        self.cold.put(key, body).await
    }

    /// Creates the object of a multipart upload in the cold tier, create-only, as `put_cold`
    /// does. The cold tier's own completion refuses a key it holds. The object is not
    /// journaled, as a put to the cold tier is not: it is in the cold tier, which is where the
    /// reconciler would copy it to.
    async fn complete_cold(
        &self,
        key: &str,
        id: &str,
        parts: &[(u32, String)],
    ) -> Result<String, Error> {
        refuse_reserved(key)?;
        self.check_not_in_hot(key).await?;
        self.cold.complete_upload(key, id, parts).await
    }

    /// Asks the cold tier again, after this call created `key` in the hot tier. The first ask
    /// (before the create) can be stale: the reconciler copies an object to the cold tier before it
    /// deletes it from the hot tier, so a create that wins the hot tier after that delete finds the
    /// cold copy here. If the cold tier holds the key, the object this call created is removed from
    /// the hot tier (best effort) and the put is [`Error::AlreadyExists`]; if the cold tier cannot
    /// be asked, the object is removed and the put fails with that error, since it cannot be told
    /// to be the only one.
    async fn confirm_not_tiered(&self, key: &str) -> Result<(), Error> {
        let refused = match self.cold.head(key).await {
            Ok(_) => Error::AlreadyExists(key.to_owned()),
            Err(Error::NotFound(_)) => return Ok(()),
            Err(error) => error,
        };
        let _ = self.hot.delete(key).await;
        Err(refused)
    }

    async fn put_hot(&self, key: &str, body: Bytes) -> Result<(), Error> {
        let size = body.len() as u64;
        let stored = match self.hot.put(key, body.clone()).await {
            Ok(()) => match self.confirm_not_tiered(key).await {
                Ok(()) => self.journal_put(key, size).await,
                refused => refused,
            },
            failed => failed,
        };
        match stored {
            Err(Error::Full(_)) => {
                self.latch_full();
                self.put_cold(key, body).await
            }
            other => other,
        }
    }

    async fn put_object(&self, key: &str, body: Bytes) -> Result<(), Error> {
        refuse_reserved(key)?;
        match self.cold.head(key).await {
            Ok(_) => return Err(Error::AlreadyExists(key.to_owned())),
            Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        match self.level().await {
            Level::Full => self.put_cold(key, body).await,
            Level::Critical => {
                tokio::time::sleep(self.config.back_pressure).await;
                self.put_hot(key, body).await
            }
            Level::Open => self.put_hot(key, body).await,
        }
    }

    /// Writes the object to the cold tier from the body, create-only, when the hot tier is full.
    async fn put_cold_stream(
        &self,
        key: &str,
        len: Option<u64>,
        offered: &Offered<'_>,
    ) -> Result<(), Error> {
        self.check_not_in_hot(key).await?;
        self.cold.put_stream(key, len, offered.view()).await
    }

    /// Writes the object to the hot tier from the body, then journals it. The object is in the
    /// hot tier only when the body has ended whole (the backend sees to it), and the journal
    /// entry comes after that, so a body that fails leaves neither. A hot tier with no room is
    /// latched full; the body goes to the cold tier instead if the hot tier took none of it,
    /// and else the put fails with [`Error::Full`], since the bytes taken cannot be offered
    /// again (the client's retry goes to the cold tier).
    async fn put_hot_stream(
        &self,
        key: &str,
        len: Option<u64>,
        offered: &Offered<'_>,
        hot_only: bool,
    ) -> Result<(), Error> {
        let stored = match self.hot.put_stream(key, len, offered.view()).await {
            Ok(()) => match self.confirm_not_tiered(key).await {
                Ok(()) => self.journal_put(key, offered.taken()).await,
                refused => refused,
            },
            failed => failed,
        };
        match stored {
            Err(Error::Full(_)) if hot_only => {
                self.latch_full();
                Err(Error::Full(key.to_owned()))
            }
            Err(Error::Full(_)) if offered.taken() == 0 => {
                self.latch_full();
                self.put_cold_stream(key, len, offered).await
            }
            Err(error @ Error::Full(_)) => {
                self.latch_full();
                Err(error)
            }
            other => other,
        }
    }

    /// As `put_object`, for a body that arrives as chunks: it goes to the hot tier, or to the
    /// cold tier where the hot tier is full, without being held.
    async fn put_object_stream(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
        hot_only: bool,
    ) -> Result<(), Error> {
        refuse_reserved(key)?;
        match self.cold.head(key).await {
            Ok(_) => return Err(Error::AlreadyExists(key.to_owned())),
            Err(Error::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
        let offered = Offered::new(body);
        match self.level().await {
            Level::Full if hot_only => Err(Error::Full(key.to_owned())),
            Level::Full => self.put_cold_stream(key, len, &offered).await,
            Level::Critical => {
                tokio::time::sleep(self.config.back_pressure).await;
                self.put_hot_stream(key, len, &offered, hot_only).await
            }
            Level::Open => self.put_hot_stream(key, len, &offered, hot_only).await,
        }
    }

    /// Copies an object read from the cold tier into the hot tier, and journals the copy in
    /// the background.
    async fn copy_back(self: &Arc<Self>, key: &str, body: &Bytes) {
        if self.level().await >= Level::Critical {
            return;
        }
        match self.hot.put(key, body.clone()).await {
            Ok(()) => {
                let inner = Arc::clone(self);
                let key = key.to_owned();
                let size = body.len() as u64;
                drop(tokio::spawn(async move {
                    let _ = inner.journal_put(&key, size).await;
                }));
            }
            Err(Error::Full(_)) => self.latch_full(),
            Err(_) => {}
        }
    }

    async fn get_object(
        self: &Arc<Self>,
        key: &str,
        range: Option<ByteRange>,
    ) -> Result<Bytes, Error> {
        match self.hot.get(key, range).await {
            Err(Error::NotFound(_)) => {}
            found => return found,
        }
        let body = self.cold.get(key, None).await?;
        self.copy_back(key, &body).await;
        match range {
            Some(range) => range.slice(&body),
            None => Ok(body),
        }
    }

    async fn head_object(&self, key: &str) -> Result<Meta, Error> {
        match self.hot.head(key).await {
            Err(Error::NotFound(_)) => self.cold.head(key).await,
            found => found,
        }
    }

    async fn delete_object(&self, key: &str) -> Result<(), Error> {
        refuse_reserved(key)?;
        self.hot.delete(key).await?;
        self.cold.delete(key).await?;
        self.append(self.entry(JournalOp::Delete, key, 0)).await
    }

    async fn list_objects(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        let mut in_hot = self.hot.list(prefix).await?;
        in_hot.retain(is_data);
        let in_cold = self.cold.list(prefix).await?;
        Ok(merge(in_hot, in_cold, usize::MAX))
    }

    async fn list_objects_page(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> Result<Vec<Listed>, Error> {
        let in_hot = data_page(self.hot.as_ref(), prefix, start_after, max_keys).await?;
        let in_cold = self.cold.list_page(prefix, start_after, max_keys).await?;
        Ok(merge(in_hot, in_cold, max_keys))
    }
}

/// The first `max_keys` objects of the listing of `hot` after `start_after`, without the
/// objects under the reserved prefixes: pages of the hot tier are read until enough objects
/// are left or the listing ends.
async fn data_page(
    hot: &dyn Backend,
    prefix: &str,
    start_after: Option<&str>,
    max_keys: usize,
) -> Result<Vec<Listed>, Error> {
    let mut found: Vec<Listed> = Vec::new();
    let mut after = start_after.map(str::to_owned);
    loop {
        let page = hot.list_page(prefix, after.as_deref(), max_keys).await?;
        let full_page = page.len() >= max_keys;
        after = page.last().map(|object| object.key.clone());
        found.extend(page.into_iter().filter(is_data));
        if !full_page || found.len() >= max_keys {
            break;
        }
    }
    found.truncate(max_keys);
    Ok(found)
}

/// The objects of both listings, each key once (the hot tier's entry where both have it), in
/// key order, at most `max_keys` of them.
fn merge(in_hot: Vec<Listed>, in_cold: Vec<Listed>, max_keys: usize) -> Vec<Listed> {
    let mut merged: BTreeMap<String, Listed> = BTreeMap::new();
    for object in in_hot.into_iter().chain(in_cold) {
        merged.entry(object.key.clone()).or_insert(object);
    }
    merged.into_values().take(max_keys).collect()
}

#[async_trait]
impl Backend for Proxy {
    async fn put(&self, key: &str, body: Bytes) -> Result<(), Error> {
        self.inner.put_object(key, body).await
    }

    /// Whether either tier needs the length of a streamed body (the put may go to either).
    fn needs_length(&self) -> bool {
        self.inner.hot.needs_length() || self.inner.cold.needs_length()
    }

    /// A put whose body is not held: the chunks go to the hot tier (or the cold tier of a full
    /// hot tier) as they arrive, and the object is created, create-only, only after the body has
    /// ended whole; a body that fails leaves no object and no journal entry. See [`Proxy`].
    async fn put_stream(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<(), Error> {
        self.inner.put_object_stream(key, len, body, false).await
    }

    async fn put_stream_hot_only(
        &self,
        key: &str,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<(), Error> {
        self.inner.put_object_stream(key, len, body, true).await
    }

    /// The proxy keeps objects immutable: it never replaces one.
    async fn put_if_match(&self, key: &str, _body: Bytes, _etag: &str) -> Result<String, Error> {
        Err(Error::Other(format!("the proxy does not replace {key}")))
    }

    async fn get(&self, key: &str, range: Option<ByteRange>) -> Result<Bytes, Error> {
        self.inner.get_object(key, range).await
    }

    async fn head(&self, key: &str) -> Result<Meta, Error> {
        self.inner.head_object(key).await
    }

    async fn delete(&self, key: &str) -> Result<(), Error> {
        self.inner.delete_object(key).await
    }

    async fn list(&self, prefix: &str) -> Result<Vec<Listed>, Error> {
        self.inner.list_objects(prefix).await
    }

    async fn list_page(
        &self,
        prefix: &str,
        start_after: Option<&str>,
        max_keys: usize,
    ) -> Result<Vec<Listed>, Error> {
        self.inner
            .list_objects_page(prefix, start_after, max_keys)
            .await
    }

    async fn create_upload(&self, key: &str) -> Result<String, Error> {
        refuse_reserved(key)?;
        self.inner.cold.create_upload(key).await
    }

    async fn upload_part(
        &self,
        key: &str,
        id: &str,
        number: u32,
        body: PartBody<'_>,
    ) -> Result<String, Error> {
        let sized = self.upload_part_with_len(key, id, number, None, body);
        sized.await
    }

    async fn upload_part_with_len(
        &self,
        key: &str,
        id: &str,
        number: u32,
        len: Option<u64>,
        body: PartBody<'_>,
    ) -> Result<String, Error> {
        refuse_reserved(key)?;
        let cold = &self.inner.cold;
        let upload = cold.upload_part_with_len(key, id, number, len, body);
        upload.await
    }

    async fn complete_upload(
        &self,
        key: &str,
        id: &str,
        parts: &[(u32, String)],
    ) -> Result<String, Error> {
        self.inner.complete_cold(key, id, parts).await
    }

    async fn abort_upload(&self, key: &str, id: &str) -> Result<(), Error> {
        refuse_reserved(key)?;
        self.inner.cold.abort_upload(key, id).await
    }

    /// The open uploads are the cold tier's own (see the type), so this asks the cold tier, and
    /// leaves out an upload of a reserved key: none is made through the proxy, but the tier is
    /// shared with other writers. A page that the filter cut short is made up from the next
    /// page of the tier, so that a short page is the last, as the trait says.
    async fn list_uploads(
        &self,
        prefix: &str,
        after: Option<UploadsAfter<'_>>,
        max_uploads: usize,
    ) -> Result<Vec<OpenUpload>, Error> {
        let cold = &self.inner.cold;
        let mut after = after.map(|(key, id)| (key.to_owned(), id.map(str::to_owned)));
        let mut found: Vec<OpenUpload> = Vec::new();
        let mut calls = 0;
        while found.len() < max_uploads {
            calls += 1;
            if calls > UPLOAD_LISTING_CALLS {
                let text = format!(
                    "the cold tier's listing of uploads took over {UPLOAD_LISTING_CALLS} calls"
                );
                return Err(Error::Unavailable(text));
            }
            let want = max_uploads - found.len();
            let marker = after
                .as_ref()
                .map(|(key, id)| (key.as_str(), id.as_deref()));
            let page = cold.list_uploads(prefix, marker, want).await?;
            let last = page.last().map(|up| (up.key.clone(), Some(up.id.clone())));
            let full = page.len() == want;
            found.extend(page.into_iter().filter(|up| !is_reserved(&up.key)));
            match last {
                Some(last) if full => {
                    // A full page that ends where the last one did would be asked for for ever.
                    if after.as_ref().is_some_and(|before| last <= *before) {
                        let text = "the cold tier's listing of uploads did not advance";
                        return Err(Error::Unavailable(text.to_owned()));
                    }
                    after = Some(last);
                }
                _ => break,
            }
        }
        Ok(found)
    }
}
