//! The reconciler: copies what the proxies journal to the cold tier, and frees the hot tier.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::Duration;

use futures::stream::{self, StreamExt};

use crate::journal::{
    PROXIES_PREFIX, USAGE_KEY, Usage, checkpoint_key, decode_journal, journal_key, journal_prefix,
    load_checkpoint, seq_of, to_body, write_object,
};
use crate::lease::Lease;
use crate::state::{Books, Stream};
use crate::{Backend, Clock, Error, JournalEntry, JournalOp};

/// The settings of a [`Reconciler`].
#[derive(Clone)]
pub struct ReconcilerConfig {
    /// Names this reconciler in the lease. No two reconcilers that run at the same time have
    /// the same name.
    pub holder: String,
    /// The bytes the hot tier is meant to hold at most.
    pub hot_budget: u64,
    /// The share of the budget (0.8, say) above which the eviction starts.
    pub high_water: f64,
    /// The share of the budget (0.7, say) down to which the eviction goes on.
    pub low_water: f64,
    /// The most objects copied to the cold tier at once. Size it to what the cold tier
    /// takes in parallel.
    pub parallel: usize,
    /// How long after its last renewal a lease may be taken over. Passes must be run more
    /// often than this.
    pub lease_ttl: Duration,
    /// How long a journal object is kept, counted from the time it was written, once it is
    /// applied and evicted past.
    pub retention: Duration,
    /// The most journal objects asked for in one listing.
    pub page_size: usize,
    /// The time, for the lease, the retention and the usage object. It must agree with the
    /// times the hot tier gives its objects to within the lease ttl and the retention.
    pub clock: Arc<dyn Clock>,
}

impl ReconcilerConfig {
    /// The settings for a reconciler `holder` and a hot tier of `hot_budget` bytes, with
    /// high and low water of 0.8 and 0.7, 16 parallel copies, a lease ttl of 30 seconds, a
    /// retention of an hour, and listings of 100 journal objects.
    #[must_use]
    pub fn new(holder: &str, hot_budget: u64, clock: Arc<dyn Clock>) -> Self {
        Self {
            holder: holder.to_owned(),
            hot_budget,
            high_water: 0.8,
            low_water: 0.7,
            parallel: 16,
            lease_ttl: Duration::from_secs(30),
            retention: Duration::from_secs(3600),
            page_size: 100,
            clock,
        }
    }
}

/// What one [`Reconciler::pass`] did.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct PassReport {
    /// Whether this reconciler held the lease. If not, nothing else was done.
    pub held: bool,
    /// The objects copied to the cold tier, or found there already.
    pub copied: usize,
    /// The objects removed from the hot tier.
    pub evicted: usize,
    /// The journal objects deleted.
    pub pruned: usize,
    /// The calls that failed, in the order they did. The steps that depend on a failed one
    /// were left for the next pass.
    pub failed: Vec<Error>,
}

/// The outcome of the copies of one journal object: the key if it is in the cold tier now,
/// or nothing if the hot tier no longer holds it.
type Copies = Vec<Result<Option<String>, Error>>;

fn millis(duration: Duration) -> u64 {
    duration.as_millis() as u64
}

/// The runner that works through the journals the [`Proxy`](crate::Proxy)s write.
///
/// There is one runner: whoever holds the lease `_tiering/lease` in the hot tier (see
/// `Lease` in the source for how it is taken, renewed and taken over). Run
/// [`Reconciler::pass`] on an interval shorter than the lease ttl, on as many machines as
/// there are, and call [`Reconciler::stop`] to give the lease up. A pass that finds the lease
/// held by another does nothing; a pass that fails to renew it stops, and forgets what it
/// knew, since another reconciler may have worked meanwhile.
///
/// A pass does these steps, and never lists the data prefixes of either tier (it lists
/// `_tiering/proxies/`, one small object for each proxy, and, for each proxy, the journal
/// objects above the last one it read):
///
/// 1. **Tail the journals.** For each proxy, in the order of the journal objects from its
///    checkpoint, a journal object is read and applied: its puts are copied from the hot tier
///    to the cold tier, up to [`ReconcilerConfig::parallel`] at a time, create-only (an
///    object the cold tier holds already counts as copied, and a key the hot tier no longer
///    holds is skipped); then its deletes are applied, by deleting the key from the cold tier
///    unless the hot tier holds it again and the cold tier's copy is of that object (the same
///    size and content tag). Then the object counts as applied.
/// 2. **Evict.** The reconciler adds up the bytes of the objects the hot tier holds from the
///    journals alone: the sizes of the puts, less the deletes and the evictions. Above
///    [`ReconcilerConfig::high_water`] of the budget it deletes objects from the hot tier,
///    oldest first by the time in their journal entries (journal order within a proxy), down
///    to [`ReconcilerConfig::low_water`]. Only an object of an applied journal object is a
///    candidate, so the cold tier holds it, and the reconciler neither lists the hot tier nor
///    asks it about the object. Before it deletes anything it reads the journals once more,
///    so that an object that was deleted or put again since is skipped.
/// 3. **Checkpoint.** For each proxy, `_tiering/checkpoint/<proxy_id>` is written (an `If-Match`
///    put, after the first): every journal object up to a number is applied, and every entry
///    before a position is done with (the eviction folds its progress into the checkpoint,
///    so a restart does not count an eviction twice). After a crash the reconciler resumes
///    from the checkpoints; whatever it had done since is done again, which is harmless.
/// 4. **Prune.** A journal object is deleted when the stored checkpoint has applied it and
///    moved past its entries, and it was written at least [`ReconcilerConfig::retention`]
///    ago. A journal object that holds an object the hot tier may still hold is kept: the
///    journal is how a restarted reconciler learns what the hot tier holds, so it has about
///    one entry for each object in the hot tier.
/// 5. **Publish.** `_tiering/usage` is rewritten with the bytes the hot tier holds, the copy
///    lag (the bytes of those that the cold tier does not hold yet) and the time. The same
///    two numbers are in [`Reconciler::hot_bytes`] and [`Reconciler::copy_lag`], for metrics.
///
/// A journal object that cannot be read or does not decode stops the journal of its proxy
/// there; the failure is in the report of every pass until it is dealt with.
///
/// Known limits:
///
/// - An eviction deletes the key from the hot tier without asking it. The journals are read
///   again just before the eviction, but the copies of a pass can take long, so an object that
///   is deleted and put again through a proxy after that read (or whose journal object the
///   listing does not show yet) is deleted from the hot tier before it is copied. The window
///   is the time of that read and the deletes that follow it. Closing it takes a delete that
///   is conditional on the content tag, which [`Backend`] does not have.
/// - A pass that takes longer than the lease ttl loses the lease, and goes on to the end:
///   it may store an older checkpoint over the one of the reconciler that took over. Keep a
///   pass shorter than the ttl (set `parallel` and the ttl accordingly), and do not cancel a
///   pass.
/// - A put that went straight to the cold tier because the hot tier was full is not
///   journaled, so a delete entry of the same key that is applied after it deletes it.
/// - After a restart the journal objects before the position of the stored checkpoint that
///   are still younger than the retention are not read again, and are never deleted.
pub struct Reconciler {
    hot: Arc<dyn Backend>,
    cold: Arc<dyn Backend>,
    config: ReconcilerConfig,
    lease: Lease,
    books: Books,
}

impl Reconciler {
    /// A reconciler over `hot` and `cold`. Nothing is read or written until a pass.
    #[must_use]
    pub fn new(hot: Arc<dyn Backend>, cold: Arc<dyn Backend>, config: ReconcilerConfig) -> Self {
        let ttl = millis(config.lease_ttl);
        let clock = Arc::clone(&config.clock);
        let lease = Lease::new(Arc::clone(&hot), clock, &config.holder, ttl);
        Self {
            hot,
            cold,
            config,
            lease,
            books: Books::default(),
        }
    }

    /// The bytes of the objects the hot tier holds, by the journals read so far.
    #[must_use]
    pub fn hot_bytes(&self) -> u64 {
        self.books.usage()
    }

    /// The bytes of the objects the hot tier holds that the cold tier does not hold yet.
    #[must_use]
    pub fn copy_lag(&self) -> u64 {
        self.books.lag()
    }

    /// Gives the lease up, if it is held, and forgets what was read.
    pub async fn stop(&mut self) {
        self.lease.release().await;
        self.books = Books::default();
    }

    /// Takes or renews the lease and, if it holds it, does the steps above once.
    pub async fn pass(&mut self) -> PassReport {
        let mut report = PassReport::default();
        if !self.lease.ensure().await {
            self.books = Books::default();
            return report;
        }
        report.held = true;
        self.discover(&mut report).await;
        let ids: Vec<String> = self.books.streams.keys().cloned().collect();
        for id in &ids {
            self.advance(id, &mut report).await;
        }
        self.books.trim_stale();
        self.evict(&ids, &mut report).await;
        for id in &ids {
            self.persist(id, &mut report).await;
            self.prune(id, &mut report).await;
        }
        self.publish(&mut report).await;
        report
    }

    /// Learns the proxies that have a journal, and where each one's checkpoint is.
    async fn discover(&mut self, report: &mut PassReport) {
        let listed = match self.hot.list(PROXIES_PREFIX).await {
            Ok(listed) => listed,
            Err(error) => {
                report.failed.push(error);
                return;
            }
        };
        let ids = listed
            .iter()
            .filter_map(|object| object.key.strip_prefix(PROXIES_PREFIX));
        for id in ids {
            if self.books.streams.contains_key(id) {
                continue;
            }
            match load_checkpoint(self.hot.as_ref(), id).await {
                Ok(checkpoint) => {
                    let stream = Stream::resume(checkpoint);
                    self.books.streams.insert(id.to_owned(), stream);
                }
                Err(error) => report.failed.push(error),
            }
        }
    }

    async fn advance(&mut self, id: &str, report: &mut PassReport) {
        if let Err(error) = self.read_new(id).await {
            report.failed.push(error);
        }
        self.apply_pending(id, report).await;
    }

    /// Reads the journal objects of a proxy that follow the last one read, as far as their
    /// numbers go on without a gap (a listing need not show the latest writes).
    async fn read_new(&mut self, id: &str) -> Result<(), Error> {
        let prefix = journal_prefix(id);
        let page_size = self.config.page_size.max(1);
        loop {
            let after = journal_key(id, self.books.stream(id).read_through);
            let listing = self.hot.list_page(&prefix, Some(&after), page_size);
            let page = listing.await?;
            let last_page = page.len() < page_size;
            for object in page {
                let want = self.books.stream(id).read_through + 1;
                if seq_of(&prefix, &object.key) != Some(want) {
                    return Ok(());
                }
                let body = self.hot.get(&object.key, None).await?;
                let entries = decode_journal(&body)?;
                self.books.read_object(id, want, object.modified, entries);
            }
            if last_page {
                return Ok(());
            }
        }
    }

    /// Applies the journal objects that are read and not applied, in order, until one fails.
    async fn apply_pending(&mut self, id: &str, report: &mut PassReport) {
        loop {
            let Some((seq, entries)) = self.books.stream(id).pending.front().cloned() else {
                return;
            };
            let copied = match self.apply_object(&entries).await {
                Ok(copied) => copied,
                Err(error) => {
                    report.failed.push(error);
                    return;
                }
            };
            report.copied += copied.len();
            for (index, entry) in entries.iter().enumerate() {
                if entry.op != JournalOp::Put {
                    continue;
                }
                let at = (seq, index as u64);
                if copied.contains(&entry.key) {
                    self.books.copied(id, at, &entry.key, entry.time);
                } else {
                    self.books.gone(id, at, &entry.key);
                }
            }
            let stream = self.books.stream(id);
            stream.checkpoint = seq;
            stream.pending.pop_front();
        }
    }

    /// Copies the puts of one journal object, then applies its deletes. Says which keys are
    /// in the cold tier now.
    async fn apply_object(&self, entries: &[JournalEntry]) -> Result<BTreeSet<String>, Error> {
        let puts = entries.iter().filter(|entry| entry.op == JournalOp::Put);
        let keys: BTreeSet<&str> = puts.map(|entry| entry.key.as_str()).collect();
        let copies = stream::iter(keys).map(|key| self.copy_key(key));
        let parallel = self.config.parallel.max(1);
        let outcomes: Copies = copies.buffer_unordered(parallel).collect().await;
        let mut copied = BTreeSet::new();
        for outcome in outcomes {
            if let Some(key) = outcome? {
                copied.insert(key);
            }
        }
        for entry in entries.iter().filter(|e| e.op == JournalOp::Delete) {
            self.delete_cold(&entry.key).await?;
        }
        Ok(copied)
    }

    /// Copies one object from the hot tier to the cold tier, create-only.
    async fn copy_key(&self, key: &str) -> Result<Option<String>, Error> {
        let body = match self.hot.get(key, None).await {
            Ok(body) => body,
            Err(Error::NotFound(_)) => return Ok(None),
            Err(error) => return Err(error),
        };
        match self.cold.put(key, body).await {
            Ok(()) | Err(Error::AlreadyExists(_)) => Ok(Some(key.to_owned())),
            Err(error) => Err(error),
        }
    }

    /// Makes sure the cold tier does not hold a key that was deleted, unless the hot tier
    /// holds the key again and the cold tier's copy, if it has one, is of that object. A copy
    /// of another content (one that was under way when the key was deleted) is deleted; the
    /// entry that put the key again copies the new object.
    async fn delete_cold(&self, key: &str) -> Result<(), Error> {
        let in_hot = match self.hot.head(key).await {
            Ok(meta) => meta,
            Err(Error::NotFound(_)) => return self.cold.delete(key).await,
            Err(error) => return Err(error),
        };
        match self.cold.head(key).await {
            Ok(in_cold) if !in_cold.same_content(&in_hot) => self.cold.delete(key).await,
            Ok(_) | Err(Error::NotFound(_)) => Ok(()),
            Err(error) => Err(error),
        }
    }

    /// Deletes objects from the hot tier, oldest first, while it holds more than the low
    /// water, if it holds more than the high water. The journals of `ids` are read once more
    /// first, so that what a proxy did meanwhile is taken into account.
    async fn evict(&mut self, ids: &[String], report: &mut PassReport) {
        let budget = self.config.hot_budget as f64;
        let used = self.books.usage() as f64;
        if used <= budget * self.config.high_water {
            return;
        }
        for id in ids {
            if let Err(error) = self.read_new(id).await {
                report.failed.push(error);
                return;
            }
        }
        self.books.trim_stale();
        let low = (budget * self.config.low_water) as u64;
        while self.books.usage() > low {
            let Some((proxy, victim)) = self.books.next_victim() else {
                return;
            };
            if self.books.is_current(&proxy, victim.at, &victim.key) {
                if let Err(error) = self.hot.delete(&victim.key).await {
                    report.failed.push(error);
                    return;
                }
                self.books.forget(&victim.key);
                report.evicted += 1;
            }
            self.books.pop(&proxy);
        }
    }

    /// Stores the checkpoint of a proxy if it moved.
    async fn persist(&mut self, id: &str, report: &mut PassReport) {
        let wanted = self.books.stream(id).snapshot();
        if wanted == self.books.stream(id).persisted {
            return;
        }
        let key = checkpoint_key(id);
        match write_object(self.hot.as_ref(), &key, to_body(&wanted)).await {
            Ok(()) => self.books.stream(id).persisted = wanted,
            Err(error) => report.failed.push(error),
        }
    }

    /// Deletes the journal objects of a proxy that the stored checkpoint is done with and
    /// that are older than the retention, oldest first.
    async fn prune(&mut self, id: &str, report: &mut PassReport) {
        let now = self.config.clock.now();
        let retention = millis(self.config.retention);
        loop {
            let stream = self.books.stream(id);
            let Some(&(seq, modified)) = stream.retained.front() else {
                return;
            };
            if seq >= stream.persisted.evict_seq {
                return;
            }
            if modified.saturating_add(retention) > now {
                return;
            }
            if let Err(error) = self.hot.delete(&journal_key(id, seq)).await {
                report.failed.push(error);
                return;
            }
            self.books.stream(id).retained.pop_front();
            report.pruned += 1;
        }
    }

    async fn publish(&self, report: &mut PassReport) {
        let usage = Usage {
            hot_bytes: self.books.usage(),
            copy_lag: self.books.lag(),
            time: self.config.clock.now(),
        };
        let body = to_body(&usage);
        if let Err(error) = write_object(self.hot.as_ref(), USAGE_KEY, body).await {
            report.failed.push(error);
        }
    }
}
