//! The history writer core: events into per-invocation chunks, chunks into immutable
//! objects, and pointer rows into the `ui/` index, object first and row second.
//!
//! The server owns the loop: it offers events to the [`EventQueue`] from the request
//! path and calls [`Writer::step`] on a cadence of its choosing (twice a second keeps
//! index writes at the background lane's two transactions a second). [`Writer::step`]
//! calls the blocking [`Store`], so the server runs it where blocking is allowed.
//!
//! - **Chunks.** An invocation's records are gathered per writer and cut into a chunk at
//!   [`Limits::chunk_bytes`] of raw records, when the invocation closes, when its oldest
//!   unflushed record has waited [`Limits::chunk_age_ms`], or when its stream or UTC day
//!   changes (a chunk holds one stream, so a run id's sequence locates its block).
//! - **Object first, pointer second.** A cut chunk is written with `put_new` (a key is
//!   never reused, so `AlreadyExists` on a retry is success); only then do its `ichunk`,
//!   `stream` and `invid` rows enter [`Pending`]. A crash between the two leaves an
//!   orphan object, never a row naming an object that was not written. A failed write is
//!   counted (`stage="put"`) and retried with the same key and bytes next step.
//! - **Bounded memory.** Chunks not yet written count against [`Limits::buffer_bytes`];
//!   past it the oldest is dropped (`stage="buffer"`) and its invocation's header counts
//!   the hole. Index rows past [`Pending`]'s bounds, and rows whose transaction failed,
//!   are dropped (`stage="index"`) and their object kept for re-insertion once the
//!   backlog drains: the rows come back from the object's footer with no LIST.
//!
//! TODO: mark the index transactions background once the `kbf-state` trait has the
//! mark (the C7 background lane); until then they are ordinary transactions, sent at
//! most [`Limits::txns_per_step`] a step.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

use bytes::Bytes;
use kbf_objstore::{ByteRange, ObjectKey, ObjectStore, ObjectStoreError};
use kbf_state::{Cond, Store, Txn};
use prost::Message;

use crate::container::{self, Encoded, Header};
use crate::keys::{Day, Family, HistoryRoot, InvKey, WriterId, rows};
use crate::pending::{Kept, KeptObject, Pending, PendingLimits, Row};
use crate::queue::{Event, EventQueue, InvRef};
use crate::records::{
    ChunkBody, ChunkRecord, ChunkRow, ChunkSummary, DigestRow, FailedRun, HitRecord, InvState,
    InvocationHeader, RunRecord, Slow, StreamRow,
};
use crate::run_id::{Minter, StreamId};
use crate::scoped::Scoped;

const MS_PER_DAY: u64 = 86_400_000;

/// How many of the slowest runs an invocation's summary keeps, by each measure.
pub const SLOWEST: usize = 20;

/// The writer's bounds and cadence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Raw record bytes at which a chunk is cut.
    pub chunk_bytes: usize,
    /// How long the oldest unflushed record of an invocation waits before its chunk is
    /// cut, milliseconds.
    pub chunk_age_ms: u64,
    /// Raw bytes of chunks not yet written, at most (inside `--invocation-memory`).
    pub buffer_bytes: usize,
    /// The pending index bounds.
    pub pending: PendingLimits,
    /// Objects kept for re-insertion at most.
    pub kept: usize,
    /// Rows per index transaction at most.
    pub txn_rows: usize,
    /// Bytes per index transaction at most.
    pub txn_bytes: usize,
    /// Index transactions per step at most.
    pub txns_per_step: usize,
    /// Kept objects re-read per step at most.
    pub reinserts_per_step: usize,
}

impl Default for Limits {
    fn default() -> Limits {
        Limits {
            chunk_bytes: 256 * 1024,
            chunk_age_ms: 120_000,
            buffer_bytes: 256 * 1024 * 1024,
            pending: PendingLimits::default(),
            kept: 65_536,
            txn_rows: 256,
            txn_bytes: 256 * 1024,
            txns_per_step: 1,
            reinserts_per_step: 64,
        }
    }
}

/// Records the writer has dropped, by stage. The queue's own count is
/// [`EventQueue::dropped`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Drops {
    /// Records dropped with a chunk buffer past [`Limits::buffer_bytes`].
    pub buffer: u64,
    /// Chunk writes that failed (each retried).
    pub put: u64,
    /// Index rows dropped (each restored from its object later, when it has one).
    pub index: u64,
}

/// Records gathered for one chunk.
#[derive(Debug, Default)]
struct Building {
    day: Option<Day>,
    stream: Option<u32>,
    runs: Vec<RunRecord>,
    failed: Vec<FailedRun>,
    hits: Vec<HitRecord>,
    raw: usize,
    since_ms: u64,
}

impl Building {
    fn records(&self) -> usize {
        self.runs.len() + self.failed.len() + self.hits.len()
    }

    /// Whether a record of `day` and `stream` must start a new chunk.
    fn breaks_on(&self, day: Day, stream: Option<u32>) -> bool {
        let other_day = self.day.is_some_and(|d| d != day);
        let other_stream = matches!((self.stream, stream), (Some(a), Some(b)) if a != b);
        other_day || other_stream
    }
}

/// A chunk encoded and waiting for its write.
#[derive(Debug)]
struct Cut {
    inv: InvKey,
    key: ObjectKey,
    encoded: Encoded,
    seq: u32,
    row: ChunkRow,
    stream_first_ms: u64,
    records: usize,
}

/// What the writer holds for one invocation.
#[derive(Debug, Default)]
struct Inv {
    header: InvocationHeader,
    building: Building,
    slowest_execution: Vec<Slow>,
    slowest_wall: Vec<Slow>,
    /// The first commit time of each stream this writer has seen, its row key's time.
    stream_first: BTreeMap<u32, u64>,
}

/// The history writer core. See the module docs.
pub struct Writer<O> {
    objects: Scoped<O>,
    core: Core,
}

/// Everything of the writer but the object store: no I/O but the blocking index.
struct Core {
    root: HistoryRoot,
    index: Arc<dyn Store>,
    writer: WriterId,
    minter: Arc<Minter>,
    queue: Arc<EventQueue>,
    limits: Limits,
    /// The next chunk's sequence. One count for every invocation the writer writes, so
    /// no chunk key or `ichunk` row is made twice in a writer's life, whatever it has
    /// forgotten and reopened.
    next_chunk: u32,
    invs: BTreeMap<InvKey, Inv>,
    ready: VecDeque<Cut>,
    /// Bytes in chunks not yet written: cut ones as encoded, building ones raw.
    held: usize,
    pending: Pending,
    kept: Kept,
    drops: Drops,
    /// An index transaction failed this step: the index is not draining.
    index_failed: bool,
}

impl<O: ObjectStore> Writer<O> {
    /// A writer with id `writer` that writes objects under `root` in `objects` and rows
    /// to `index`, draining `queue` and refilling `minter`'s stream blocks.
    #[must_use]
    pub fn new(
        objects: O,
        root: HistoryRoot,
        index: Arc<dyn Store>,
        writer: WriterId,
        minter: Arc<Minter>,
        queue: Arc<EventQueue>,
        limits: Limits,
    ) -> Writer<O> {
        Writer {
            objects: Scoped::history(objects, &root),
            core: Core {
                root,
                index,
                writer,
                minter,
                queue,
                limits,
                next_chunk: 0,
                invs: BTreeMap::new(),
                ready: VecDeque::new(),
                held: 0,
                pending: Pending::new(limits.pending),
                kept: Kept::new(limits.kept),
                drops: Drops::default(),
                index_failed: false,
            },
        }
    }

    /// One turn of the writer at time `now_ms`: reserve a stream block if the minter
    /// needs one, take the queued events into chunks, cut the chunks that are due, write
    /// them, apply up to [`Limits::txns_per_step`] index transactions, and, once nothing
    /// is pending, read back kept objects to restore their rows.
    pub async fn step(&mut self, now_ms: u64) {
        self.core.prepare(now_ms);
        let mut failed = VecDeque::new();
        while let Some(cut) = self.core.ready.pop_front() {
            let body = Bytes::from(cut.encoded.bytes.clone());
            let result = self.objects.put_new(&cut.key, body, None).await;
            if let Some(cut) = self.core.written(cut, result, now_ms) {
                failed.push_back(cut);
            }
        }
        self.core.ready = failed;
        self.core.apply_pending();
        for _ in 0..self.core.reinsert_budget() {
            let Some(object) = self.core.kept.pop() else {
                break;
            };
            let read = self.read_footer(&object).await;
            if !self.core.restore(object, read) {
                break;
            }
        }
        self.core.prune(now_ms);
    }

    /// Invocations the writer holds state for.
    #[must_use]
    pub fn open_invocations(&self) -> usize {
        self.core.invs.len()
    }

    async fn read_footer(&self, object: &KeptObject) -> Result<Bytes, ObjectStoreError> {
        let range = ByteRange::new(object.footer_offset, u64::from(object.footer_len));
        // A chunk's footer range holds at least the 24-byte trailer.
        let range = range.ok_or(ObjectStoreError::Unsupported("an empty footer range"))?;
        self.objects.get_range(&object.key, range).await
    }

    /// The writer's drop counts.
    #[must_use]
    pub fn drops(&self) -> Drops {
        self.core.drops
    }

    /// Index rows waiting to be applied.
    #[must_use]
    pub fn pending_rows(&self) -> usize {
        self.core.pending.len()
    }

    /// Objects whose rows are waiting to be restored.
    #[must_use]
    pub fn kept_objects(&self) -> usize {
        self.core.kept.len()
    }

    /// Chunks cut and not yet written.
    #[must_use]
    pub fn unwritten_chunks(&self) -> usize {
        self.core.ready.len()
    }
}

impl Core {
    /// The part of a step before any object is written.
    fn prepare(&mut self, now_ms: u64) {
        self.index_failed = false;
        self.refill_streams();
        self.minter.forget_before(Day::from_ms(now_ms));
        for event in self.queue.drain() {
            self.ingest(event, now_ms);
        }
        self.cut_due(now_ms);
    }

    /// Forgets invocations with nothing buffered or unwritten that are closed or were
    /// last seen before yesterday (UTC). A later record of one reloads its header from
    /// the index and continues it.
    fn prune(&mut self, now_ms: u64) {
        let yesterday = Day::from_ms(now_ms.saturating_sub(MS_PER_DAY));
        let ready = &self.ready;
        self.invs.retain(|key, s| {
            let idle = s.building.records() == 0 && !ready.iter().any(|c| &c.inv == key);
            let done = s.header.state() == InvState::Closed
                || Day::from_ms(s.header.last_seen_ms) < yesterday;
            !(idle && done)
        });
    }

    /// How many kept objects to read back this step: none while rows are pending or
    /// after an index transaction failed (their rows would only fail again).
    fn reinsert_budget(&self) -> usize {
        if self.pending.is_empty() && !self.index_failed {
            self.limits.reinserts_per_step
        } else {
            0
        }
    }

    fn refill_streams(&mut self) {
        if !self.minter.needs_block() {
            return;
        }
        if let Ok(Ok(commit)) = self.index.txn(Txn::new().next(rows::STREAMS_COUNTER)) {
            // An empty counter list is a backend bug; block 0 is refused, so it is safe.
            self.minter
                .add_block(commit.counters.first().copied().unwrap_or(0));
        }
    }

    /// The writer's state for `inv`, made on first sight. A restarted frontend continues
    /// the header a previous life wrote; a failed read starts a fresh header.
    fn open(&mut self, inv: &InvRef) -> &mut Inv {
        let index = &self.index;
        self.invs.entry(inv.key.clone()).or_insert_with(|| {
            let header = index
                .get(&rows::invid(&inv.key))
                .ok()
                .flatten()
                .and_then(|v| InvocationHeader::decode(v.value.as_slice()).ok())
                .unwrap_or_else(|| InvocationHeader {
                    record_id: inv.record_id.clone().unwrap_or_default(),
                    idless: inv.record_id.is_none(),
                    ..InvocationHeader::default()
                });
            Inv {
                header,
                ..Inv::default()
            }
        })
    }

    /// The writer's state for an invocation it has already opened.
    fn state(&mut self, inv: &InvKey) -> &mut Inv {
        self.invs.entry(inv.clone()).or_default()
    }

    fn ingest(&mut self, event: Event, now_ms: u64) {
        match event {
            Event::Run {
                inv,
                record,
                failed,
            } => {
                let day = Day::from_ms(record.time_ms);
                self.break_if(&inv, day, Some(record.stream), now_ms);
                let state = self.open(&inv);
                note(&mut state.header, &record.instance, record.time_ms);
                state.header.runs += 1;
                state.header.failed += u64::from(record.failed());
                rank(&mut state.slowest_execution, &record, record.execution_us());
                rank(&mut state.slowest_wall, &record, record.wall_us());
                state
                    .stream_first
                    .entry(record.stream)
                    .or_insert(record.time_ms);
                let mut size = record.encoded_len();
                let b = start(&mut state.building, day, now_ms);
                b.stream = Some(record.stream);
                if let Some(mut f) = failed {
                    f.seq = record.seq;
                    size += f.encoded_len();
                    b.failed.push(f);
                }
                b.runs.push(*record);
                self.grew(&inv.key, size, now_ms);
            }
            Event::Hit { inv, mut record } => {
                let day = Day::from_ms(record.time_ms);
                self.break_if(&inv, day, None, now_ms);
                let state = self.open(&inv);
                note(&mut state.header, &record.instance, record.time_ms);
                state.header.hits += 1;
                let b = start(&mut state.building, day, now_ms);
                // A chunk holds far fewer than 2^32 hits.
                record.row = b.hits.len() as u32;
                let size = record.encoded_len();
                b.hits.push(record);
                self.grew(&inv.key, size, now_ms);
            }
            Event::Close { inv } => {
                self.minter.close(&inv.key);
                let state = self.open(&inv);
                state.header.state = InvState::Closed.into();
                if state.building.records() > 0 {
                    self.cut(&inv.key, now_ms);
                } else {
                    let value = state.header.encode_to_vec();
                    self.queue_row(rows::invid(&inv.key), value, Cond::Any, None);
                }
            }
        }
    }

    /// Cuts `inv`'s chunk if a record of `day` and `stream` cannot join it.
    fn break_if(&mut self, inv: &InvRef, day: Day, stream: Option<u32>, now_ms: u64) {
        if self.open(inv).building.breaks_on(day, stream) {
            self.cut(&inv.key, now_ms);
        }
    }

    /// Accounts `size` new raw bytes in `inv`'s chunk: cuts it at the chunk size, then
    /// sheds the oldest unwritten chunks while the writer holds too much.
    fn grew(&mut self, inv: &InvKey, size: usize, now_ms: u64) {
        let chunk_bytes = self.limits.chunk_bytes;
        self.held += size;
        let b = &mut self.state(inv).building;
        b.raw += size;
        if b.raw >= chunk_bytes {
            self.cut(inv, now_ms);
        }
        self.shed();
    }

    /// Drops the oldest chunks not yet written while the writer holds more than
    /// [`Limits::buffer_bytes`]: cut chunks first, in write order, then building chunks,
    /// longest waiting first. Each dropped record is counted, and its invocation's
    /// header counts the hole.
    fn shed(&mut self) {
        let limit = self.limits.buffer_bytes;
        while self.held > limit {
            let Some(cut) = self.ready.pop_front() else {
                break;
            };
            self.held -= cut.encoded.bytes.len();
            self.lost(&cut.inv, cut.records);
        }
        let mut waiting: Vec<(u64, InvKey)> = self
            .invs
            .iter()
            .filter(|(_, s)| s.building.records() > 0)
            .map(|(k, s)| (s.building.since_ms, k.clone()))
            .collect();
        waiting.sort();
        for (_, inv) in waiting {
            if self.held <= limit {
                break;
            }
            let b = std::mem::take(&mut self.state(&inv).building);
            self.held -= b.raw;
            self.lost(&inv, b.records());
        }
    }

    fn lost(&mut self, inv: &InvKey, records: usize) {
        self.drops.buffer += records as u64;
        self.state(inv).header.dropped += records as u64;
    }

    fn cut_due(&mut self, now_ms: u64) {
        let due: Vec<InvKey> = self
            .invs
            .iter()
            .filter(|(_, s)| {
                s.building.records() > 0
                    && now_ms.saturating_sub(s.building.since_ms) >= self.limits.chunk_age_ms
            })
            .map(|(k, _)| k.clone())
            .collect();
        for inv in due {
            self.cut(&inv, now_ms);
        }
    }

    /// Encodes `inv`'s building chunk and queues it for its write.
    fn cut(&mut self, inv: &InvKey, now_ms: u64) {
        let writer = self.writer;
        let seq = self.next_chunk;
        // 2^32 chunks in one life: at the design's rates (about 100,000 a day) that is
        // a century, so the wrap is never reached.
        self.next_chunk = seq.wrapping_add(1);
        let state = self.state(inv);
        let b = std::mem::take(&mut state.building);
        let day = b.day.unwrap_or(Day::from_ms(now_ms));
        let records = b.records();
        let raw = b.raw;
        let stream = b.stream.unwrap_or_default();
        let mut row = ChunkRow {
            day: day.number(),
            stream,
            first_seq: b.runs.iter().map(|r| r.seq).min().unwrap_or_default(),
            last_seq: b.runs.iter().map(|r| r.seq).max().unwrap_or_default(),
            // Counts in one chunk are bounded by its size.
            runs: b.runs.len() as u32,
            hits: b.hits.len() as u32,
            failed: b.runs.iter().filter(|r| r.failed()).count() as u32,
            ..ChunkRow::default()
        };
        let times = b
            .runs
            .iter()
            .map(|r| r.time_ms)
            .chain(b.hits.iter().map(|h| h.time_ms));
        row.first_ms = times.clone().min().unwrap_or_default();
        row.last_ms = times.max().unwrap_or_default();
        let stream_first_ms = state
            .stream_first
            .get(&stream)
            .copied()
            .unwrap_or(row.first_ms);
        let summary = ChunkSummary {
            inv_key: inv.as_str().to_owned(),
            writer: writer.0,
            seq,
            row: Some(row.clone()),
            stream_first_ms,
            header: Some(state.header.clone()),
            slowest_execution: state.slowest_execution.clone(),
            slowest_wall: state.slowest_wall.clone(),
            digests: Vec::new(),
        };
        let header = Header {
            family: Family::InvocationChunk,
            writer,
            day,
            seq,
            base_ms: row.first_ms,
        };
        let encoded = encode_chunk(&header, b, summary);
        row.bytes = encoded.bytes.len() as u64;
        row.footer_offset = encoded.footer_offset;
        row.footer_len = encoded.footer_len;
        self.held = self.held - raw + encoded.bytes.len();
        self.ready.push_back(Cut {
            inv: inv.clone(),
            key: self.root.chunk(day, inv, writer, seq),
            encoded,
            seq,
            row,
            stream_first_ms,
            records,
        });
    }

    /// Takes the answer to `cut`'s write: on success (or `AlreadyExists`, a retried
    /// write that had landed) its rows are queued; on failure it is counted and given
    /// back to be retried.
    fn written(
        &mut self,
        cut: Cut,
        result: Result<(), ObjectStoreError>,
        now_ms: u64,
    ) -> Option<Cut> {
        match result {
            Ok(()) | Err(ObjectStoreError::AlreadyExists(_)) => {}
            Err(_) => {
                self.drops.put += 1;
                return Some(cut);
            }
        }
        self.held -= cut.encoded.bytes.len();
        let origin = KeptObject {
            key: cut.key,
            footer_offset: cut.encoded.footer_offset,
            footer_len: cut.encoded.footer_len,
        };
        let writer = self.writer;
        let state = self.state(&cut.inv);
        state.header.chunks += 1;
        state.header.last_chunk_ms = now_ms;
        let header = state.header.encode_to_vec();
        if cut.row.runs > 0 {
            let value = StreamRow {
                inv_key: cut.inv.as_str().to_owned(),
                writer: writer.0,
            };
            let day = Day::from_number(cut.row.day);
            let key = rows::stream(day, StreamId(cut.row.stream), cut.stream_first_ms);
            self.queue_row(
                key,
                value.encode_to_vec(),
                Cond::Absent,
                Some(origin.clone()),
            );
        }
        let key = rows::ichunk(&cut.inv, writer, cut.seq);
        self.queue_row(
            key,
            cut.row.encode_to_vec(),
            Cond::Absent,
            Some(origin.clone()),
        );
        self.queue_row(rows::invid(&cut.inv), header, Cond::Any, Some(origin));
        None
    }

    fn queue_row(&mut self, key: String, value: Vec<u8>, cond: Cond, origin: Option<KeptObject>) {
        let row = Row {
            value,
            cond,
            origin,
        };
        if let Err(row) = self.pending.insert(key, row) {
            self.dropped_row(row);
        }
    }

    fn dropped_row(&mut self, row: Row) {
        self.drops.index += 1;
        if let Some(origin) = row.origin {
            self.kept.push(origin);
        }
    }

    fn apply_pending(&mut self) {
        for _ in 0..self.limits.txns_per_step {
            let batch = self
                .pending
                .take(self.limits.txn_rows, self.limits.txn_bytes);
            if batch.is_empty() {
                return;
            }
            self.apply(batch);
        }
    }

    /// Applies one batch of rows in one transaction. A pointer row that already exists
    /// (a restored row, or a write retried after it landed) fails its `Absent` check;
    /// it is there, so it leaves the batch and the rest is applied again. A failed
    /// transaction drops its rows, each counted and its object kept.
    fn apply(&mut self, mut batch: Vec<(String, Row)>) {
        while !batch.is_empty() {
            let txn = batch.iter().fold(Txn::new(), |t, (key, row)| {
                t.put(key.clone(), row.value.clone(), row.cond)
            });
            match self.index.txn(txn) {
                Ok(Ok(_)) => return,
                Ok(Err(conflict)) => {
                    batch.remove(conflict.index.min(batch.len() - 1));
                }
                Err(_) => {
                    self.index_failed = true;
                    for (_, row) in batch {
                        self.dropped_row(row);
                    }
                    return;
                }
            }
        }
    }

    /// Restores the rows of a kept object from its footer (`read`). Returns whether to
    /// go on with the next kept object: a failed read keeps the object and stops for
    /// this step; a footer that does not decode is left to the weekly sweep.
    fn restore(&mut self, object: KeptObject, read: Result<Bytes, ObjectStoreError>) -> bool {
        let Ok(bytes) = read else {
            self.kept.push(object);
            return false;
        };
        let parsed = container::decode_footer(&bytes)
            .ok()
            .and_then(|(_, footer)| ChunkSummary::decode(footer.summary.as_slice()).ok())
            .and_then(|s| {
                let inv = InvKey::parse(&s.inv_key).ok();
                inv.zip(s.row.clone()).map(|(inv, row)| (inv, row, s))
            });
        let Some((inv, mut row, summary)) = parsed else {
            return true;
        };
        // The summary was written before the object's length and footer position were
        // known; the footer and trailer run to the object's end.
        row.footer_offset = object.footer_offset;
        row.footer_len = object.footer_len;
        row.bytes = object.footer_offset + u64::from(object.footer_len);
        let writer = WriterId(summary.writer);
        // The chunk row first: with little room it is the row that matters most.
        let key = rows::ichunk(&inv, writer, summary.seq);
        self.queue_row(key, row.encode_to_vec(), Cond::Absent, Some(object.clone()));
        let header = summary.header.unwrap_or_default().encode_to_vec();
        self.queue_row(
            rows::invid(&inv),
            header,
            Cond::Absent,
            Some(object.clone()),
        );
        if row.runs > 0 {
            let value = StreamRow {
                inv_key: summary.inv_key,
                writer: writer.0,
            };
            let day = Day::from_number(row.day);
            let key = rows::stream(day, StreamId(row.stream), summary.stream_first_ms);
            self.queue_row(key, value.encode_to_vec(), Cond::Absent, Some(object));
        }
        true
    }
}

/// Encodes a chunk of `b`'s records with `summary`, adding the digest table.
fn encode_chunk(header: &Header, b: Building, mut summary: ChunkSummary) -> Encoded {
    let mut records = Vec::with_capacity(b.records());
    let bodies = b
        .runs
        .into_iter()
        .map(ChunkBody::Run)
        .chain(b.failed.into_iter().map(ChunkBody::Failed))
        .chain(b.hits.into_iter().map(ChunkBody::Hit));
    for body in bodies {
        let key = body.key().to_vec();
        let action = match &body {
            ChunkBody::Run(r) => r.action.as_ref(),
            ChunkBody::Hit(h) => h.action.as_ref(),
            ChunkBody::Failed(_) => None,
        };
        if let Some(d) = action {
            summary.digests.push(DigestRow {
                prefix: d.prefix16(),
                key: key.clone(),
            });
        }
        records.push((key, ChunkRecord { body: Some(body) }.encode_to_vec()));
    }
    summary
        .digests
        .sort_by(|a, b| (a.prefix, &a.key).cmp(&(b.prefix, &b.key)));
    container::encode(header, records, summary.encode_to_vec())
}

/// Starts `b` for a record of `day` at `now_ms` if it is empty, and returns it.
fn start(b: &mut Building, day: Day, now_ms: u64) -> &mut Building {
    if b.records() == 0 {
        b.since_ms = now_ms;
    }
    b.day = Some(day);
    b
}

/// Notes a record of `instance` at `time_ms` in an invocation's header, which reopens
/// it if it was closed.
fn note(h: &mut InvocationHeader, instance: &str, time_ms: u64) {
    h.state = InvState::Open.into();
    if let Err(at) = h.instances.binary_search_by(|i| i.as_str().cmp(instance)) {
        h.instances.insert(at, instance.to_owned());
    }
    if h.first_seen_ms == 0 || time_ms < h.first_seen_ms {
        h.first_seen_ms = time_ms;
    }
    h.last_seen_ms = h.last_seen_ms.max(time_ms);
}

/// Puts `record` among the [`SLOWEST`] runs of `list` by `us`, slowest first.
fn rank(list: &mut Vec<Slow>, record: &RunRecord, us: u64) {
    let at = list.partition_point(|s| s.us >= us);
    if at >= SLOWEST {
        return;
    }
    list.insert(
        at,
        Slow {
            run_id: record.run_id().to_bytes().to_vec(),
            us,
        },
    );
    list.truncate(SLOWEST);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_slowest_list_keeps_the_top_twenty_slowest_first() {
        // Catches: a list that grows without bound, keeps the fastest, or puts a tie
        // ahead of an earlier run of the same time.
        let mut list = Vec::new();
        for us in 0..30u64 {
            let r = RunRecord {
                seq: us as u32,
                ..RunRecord::default()
            };
            rank(&mut list, &r, us % 25);
        }
        assert_eq!(list.len(), SLOWEST);
        let times: Vec<u64> = list.iter().map(|s| s.us).collect();
        assert_eq!(times[..3], [24, 23, 22]);
        assert!(times.windows(2).all(|w| w[0] >= w[1]));
        let r = RunRecord::default();
        rank(&mut list, &r, 0);
        assert!(
            list.iter().all(|s| s.us >= 5),
            "a run faster than every kept one stays out"
        );
    }

    #[test]
    fn a_header_notes_instances_once_sorted_and_the_time_span() {
        // Catches: an instance listed twice (an all-must-allow check would ask twice),
        // or a first-seen time that a later, earlier-stamped record does not move.
        let mut h = InvocationHeader::default();
        note(&mut h, "b", 20);
        note(&mut h, "a", 30);
        note(&mut h, "b", 10);
        assert_eq!(h.instances, ["a", "b"]);
        assert_eq!((h.first_seen_ms, h.last_seen_ms), (10, 30));
    }

    #[test]
    fn a_chunk_breaks_on_a_new_day_or_stream_but_not_for_hits() {
        // Catches: two streams in one chunk (the block index would find the wrong run)
        // or a chunk spanning two days (its key carries one).
        let d1 = Day::from_number(20_261_016);
        let d2 = Day::from_number(20_261_017);
        let empty = Building::default();
        assert!(!empty.breaks_on(d1, Some(1)));
        let b = Building {
            day: Some(d1),
            stream: Some(1),
            ..Building::default()
        };
        assert!(!b.breaks_on(d1, Some(1)));
        assert!(!b.breaks_on(d1, None));
        assert!(b.breaks_on(d1, Some(2)));
        assert!(b.breaks_on(d2, None));
    }
}
