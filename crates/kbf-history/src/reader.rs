//! Reads history back: a run by its id, an invocation's header and newest summary by
//! its key, and its records a bounded page of chunks at a time.
//!
//! A run id finds its own chunk with no per-run row: its time bytes give the UTC day,
//! the newest `ui/stream/<day>/<stream>/<first ms>` row at or before its time gives the
//! invocation and writer (so a fallback stream id reused after a restart still resolves
//! to its own life's chunk), the `ichunk` rows of that invocation and writer give the
//! chunk holding its sequence, and the chunk's footer gives the block. That is the
//! index plus at most three range reads: footer, run block, failed-run block.
//!
//! Every answer holds only records whose instance `allow` accepts, and an invocation
//! is answered only when `allow` accepts every instance its header lists (an
//! invocation spanning instances is all-must-allow, ui-data.md section 8).
//!
//! Index calls go through the [`HistoryIndex`]'s offload, so the reader's `async fn`s
//! may be awaited on a runtime worker.
//!
//! TODO: read the index with a bounded range scan once the `kbf-state` trait has one
//! (suite clause C8, added by a sibling change); until then both lookups use
//! `scan_prefix`, which returns every row under the prefix.

use bytes::Bytes;
use kbf_objstore::{ByteRange, ObjectKey, ObjectStore, ObjectStoreError};
use kbf_state::StoreError;
use prost::Message;

use crate::container::{self, BlockEntry, Footer, FormatError};
use crate::index::HistoryIndex;
use crate::keys::{Day, HistoryRoot, IndexRoot, InvKey, WriterId};
use crate::records::{
    ChunkBody, ChunkRecord, ChunkRow, ChunkSummary, FailedRun, HitRecord, InvState,
    InvocationHeader, RunRecord, SECTION_FAILED, SECTION_RUN, StreamRow, chunk_key,
};
use crate::run_id::RunId;
use crate::scoped::Scoped;

/// Why a read failed (as opposed to finding nothing).
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    /// The index failed.
    #[error(transparent)]
    Index(#[from] StoreError),
    /// The object store failed.
    #[error(transparent)]
    Object(#[from] ObjectStoreError),
    /// An object is not a history object this build reads.
    #[error(transparent)]
    Format(#[from] FormatError),
    /// An index row does not decode.
    #[error("index row {key} does not decode: {message}")]
    Row {
        /// The row.
        key: String,
        /// Why.
        message: String,
    },
}

/// One run, as `GET /v1/runs/{id}` needs it.
#[derive(Clone, Debug, PartialEq)]
pub struct RunView {
    /// The invocation it belongs to.
    pub inv: InvKey,
    /// The run record.
    pub run: RunRecord,
    /// Its failed-run section, if it failed and has one.
    pub failed: Option<FailedRun>,
}

/// Chunks a page of records reads at most, all at once: 32 GETs in parallel, at most
/// 32 chunks (about 8 MiB of raw records) resident for one answer.
pub const MAX_PAGE_CHUNKS: usize = 32;

/// An invocation's summary, as `GET /v1/invocations/{id}` needs it: the header row and
/// the newest chunk's summary section, one range read.
#[derive(Clone, Debug, PartialEq)]
pub struct InvocationView {
    /// The header row.
    pub header: InvocationHeader,
    /// Closed with no records dropped. An invocation left open (the frontend crashed or
    /// is still writing) or with a hole is not complete.
    pub complete: bool,
    /// When not complete: the time of the last chunk written, where the gap starts.
    pub gap_from_ms: Option<u64>,
    /// The summary section of the newest chunk (the one with the latest record): its
    /// writer's slowest runs, its digest table and the header as of it. `None` when the
    /// invocation has no chunk.
    pub newest: Option<ChunkSummary>,
}

/// Where a page of an invocation's records starts: after the chunk whose `ichunk` row
/// is this one. Opaque to callers, who pass back what a page gave them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChunkCursor(String);

/// One page of an invocation's records, chunk by chunk in `ichunk` row order (writer,
/// then sequence), each chunk's runs in sequence order.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct RecordsPage {
    /// Run records.
    pub runs: Vec<RunRecord>,
    /// Failed-run sections.
    pub failed: Vec<FailedRun>,
    /// Hit records.
    pub hits: Vec<HitRecord>,
    /// Where the next page starts; `None` after the last chunk.
    pub next: Option<ChunkCursor>,
}

/// Where a run is: its chunk's key, row and invocation.
struct Located {
    inv: InvKey,
    key: ObjectKey,
    row: ChunkRow,
}

/// Reads history through the index and the object store.
pub struct Reader<O> {
    objects: Scoped<O>,
    root: HistoryRoot,
    index: HistoryIndex,
}

impl<O: ObjectStore> Reader<O> {
    /// A reader of the objects under `root` in `objects`, through the rows in `index`.
    #[must_use]
    pub fn new(objects: O, root: HistoryRoot, index: HistoryIndex) -> Reader<O> {
        Reader {
            objects: Scoped::history(objects, &root),
            root,
            index,
        }
    }

    /// The run `id`, if history holds it and its instance is allowed.
    ///
    /// # Errors
    /// [`ReadError`] when the index or the object store fails, or a row or object does
    /// not decode.
    pub async fn run(
        &self,
        id: RunId,
        allow: &dyn Fn(&str) -> bool,
    ) -> Result<Option<RunView>, ReadError> {
        let Some(at) = self.locate(id).await? else {
            return Ok(None);
        };
        let range = footer_range(&at.row)?;
        let footer = decode_tail(&self.objects.get_range(&at.key, range).await?)?;
        let run_key = chunk_key(SECTION_RUN, id.seq());
        let block = self.block(&at.key, &footer, &run_key).await?;
        let Some(run) = find_run(&block, id, allow) else {
            return Ok(None);
        };
        let failed_key = chunk_key(SECTION_FAILED, id.seq());
        let failed = if run.failed() {
            let block = self.block(&at.key, &footer, &failed_key).await?;
            find_failed(&block, id.seq())
        } else {
            None
        };
        Ok(Some(RunView {
            inv: at.inv,
            run,
            failed,
        }))
    }

    /// Invocation `inv`'s header and its newest chunk's summary, if history holds it and
    /// `allow` accepts every instance it lists: the index and one range read.
    ///
    /// # Errors
    /// As [`Reader::run`].
    pub async fn invocation(
        &self,
        inv: &InvKey,
        allow: &dyn Fn(&str) -> bool,
    ) -> Result<Option<InvocationView>, ReadError> {
        let Some(header) = self.header(inv, allow).await? else {
            return Ok(None);
        };
        let rows = self
            .chunk_rows(self.index.root().ichunk_prefix(inv))
            .await?;
        // The newest chunk holds the latest record; of equals, the last row.
        let newest = rows.iter().max_by_key(|(_, row)| row.last_ms);
        let newest = match newest {
            Some((key, row)) => {
                let object = chunk_object_key(&self.root, inv, key, row);
                let tail = self.objects.get_range(&object, footer_range(row)?).await?;
                let summary = ChunkSummary::decode(decode_tail(&tail)?.summary.as_slice())
                    .map_err(|e| FormatError::decode("chunk summary", &e))?;
                Some(summary)
            }
            None => None,
        };
        let complete = header.state() == InvState::Closed && header.dropped == 0;
        Ok(Some(InvocationView {
            gap_from_ms: (!complete).then_some(header.last_chunk_ms),
            complete,
            header,
            newest,
        }))
    }

    /// A page of invocation `inv`'s records: up to `chunks` chunks (at least 1, at most
    /// [`MAX_PAGE_CHUNKS`]) after `after`, read all at once, if history holds the
    /// invocation and `allow` accepts every instance it lists. Records of instances
    /// `allow` refuses are left out.
    ///
    /// # Errors
    /// As [`Reader::run`].
    pub async fn records(
        &self,
        inv: &InvKey,
        after: Option<&ChunkCursor>,
        chunks: usize,
        allow: &dyn Fn(&str) -> bool,
    ) -> Result<Option<RecordsPage>, ReadError> {
        if self.header(inv, allow).await?.is_none() {
            return Ok(None);
        }
        let rows = self
            .chunk_rows(self.index.root().ichunk_prefix(inv))
            .await?;
        let start = after.map_or(0, |c| rows.partition_point(|(key, _)| *key <= c.0));
        let page = &rows[start..];
        let take = chunks.clamp(1, MAX_PAGE_CHUNKS).min(page.len());
        let reads = page[..take].iter().map(|(key, row)| async move {
            let object = chunk_object_key(&self.root, inv, key, row);
            let range = ByteRange::new(0, row.bytes).ok_or(FormatError::Truncated("chunk row"))?;
            Ok::<_, ReadError>(self.objects.get_range(&object, range).await?)
        });
        let mut out = RecordsPage::default();
        for bytes in futures::future::try_join_all(reads).await? {
            add_records(&mut out, &bytes, allow)?;
        }
        out.next = (take < page.len()).then(|| ChunkCursor(page[take - 1].0.clone()));
        Ok(Some(out))
    }

    /// Invocation `inv`'s header row, if it exists and `allow` accepts every instance
    /// it lists.
    async fn header(
        &self,
        inv: &InvKey,
        allow: &dyn Fn(&str) -> bool,
    ) -> Result<Option<InvocationHeader>, ReadError> {
        let key = self.index.root().invid(inv);
        let Some(row) = self.index.get(key.clone()).await? else {
            return Ok(None);
        };
        let header = decode_row::<InvocationHeader>(&key, &row.value)?;
        Ok(header.instances.iter().all(|i| allow(i)).then_some(header))
    }

    /// Finds run `id`'s chunk through the index, or `None` when no row names it.
    async fn locate(&self, id: RunId) -> Result<Option<Located>, ReadError> {
        let rows = self.index.root();
        let streams = self
            .index
            .scan_prefix(rows.stream_prefix(id.day(), id.stream()))
            .await?;
        // Rows sort by open time: the run's stream is the newest opened at or before it.
        let found = streams
            .iter()
            .rev()
            .find(|(k, _)| IndexRoot::stream_first_ms(k).is_some_and(|t| t <= id.time_ms()));
        let Some((key, value)) = found else {
            return Ok(None);
        };
        let stream = decode_row::<StreamRow>(key, &value.value)?;
        let inv = InvKey::parse(&stream.inv_key).map_err(|e| ReadError::Row {
            key: key.clone(),
            message: e.to_string(),
        })?;
        let writer = WriterId(stream.writer);
        let prefix = format!("{}{writer}/", rows.ichunk_prefix(&inv));
        let chunk = self
            .chunk_rows(prefix)
            .await?
            .into_iter()
            .find(|(_, row)| row.holds(id.stream().0, id.seq()));
        Ok(chunk.map(|(key, row)| Located {
            key: chunk_object_key(&self.root, &inv, &key, &row),
            inv,
            row,
        }))
    }

    /// The `ichunk` rows under `prefix`, decoded, in key order.
    async fn chunk_rows(&self, prefix: String) -> Result<Vec<(String, ChunkRow)>, ReadError> {
        self.index
            .scan_prefix(prefix)
            .await?
            .into_iter()
            .map(|(k, v)| decode_row::<ChunkRow>(&k, &v.value).map(|row| (k, row)))
            .collect()
    }

    /// The records of the block of `footer` that holds `key`. A footer with no such
    /// block, for a key the index says the chunk holds, is a broken object.
    async fn block(
        &self,
        object: &ObjectKey,
        footer: &Footer,
        key: &[u8],
    ) -> Result<Vec<ChunkBody>, ReadError> {
        let entry = footer.block_for(key).cloned().unwrap_or_default();
        let stored = self.objects.get_range(object, block_range(&entry)?).await?;
        decode_records(&entry, &stored)
    }
}

/// The object key an `ichunk` row names: the writer and sequence are in the row's key
/// (`ui/ichunk/<inv>/<writer>/<seq>`), the day in its value.
fn chunk_object_key(root: &HistoryRoot, inv: &InvKey, row_key: &str, row: &ChunkRow) -> ObjectKey {
    let mut parts = row_key.rsplit('/');
    let seq = parts.next().and_then(|s| u32::from_str_radix(s, 16).ok());
    let writer = parts.next().and_then(|w| u64::from_str_radix(w, 16).ok());
    // The writer made both parts from numbers; a row key that does not parse names
    // chunk 0 of writer 0, which no writer writes, so the read finds nothing.
    root.chunk(
        Day::from_number(row.day),
        inv,
        WriterId(writer.unwrap_or_default()),
        seq.unwrap_or_default(),
    )
}

fn footer_range(row: &ChunkRow) -> Result<ByteRange, FormatError> {
    // A footer range of zero length (a broken row) reads one byte and fails to decode.
    let range = ByteRange::new(row.footer_offset, u64::from(row.footer_len).max(1));
    range.ok_or(FormatError::Truncated("footer range"))
}

fn block_range(entry: &BlockEntry) -> Result<ByteRange, FormatError> {
    ByteRange::new(entry.offset, u64::from(entry.len)).ok_or(FormatError::Truncated("block"))
}

fn decode_tail(tail: &Bytes) -> Result<Footer, ReadError> {
    Ok(container::decode_footer(tail)?.1)
}

fn decode_row<M: Message + Default>(key: &str, value: &[u8]) -> Result<M, ReadError> {
    M::decode(value).map_err(|e| ReadError::Row {
        key: key.to_owned(),
        message: e.to_string(),
    })
}

fn decode_records(entry: &BlockEntry, stored: &[u8]) -> Result<Vec<ChunkBody>, ReadError> {
    container::decode_block(entry, stored)?
        .iter()
        .map(|r| decode_record(r))
        .collect()
}

fn decode_record(bytes: &[u8]) -> Result<ChunkBody, ReadError> {
    let record = ChunkRecord::decode(bytes).map_err(|e| FormatError::Decode {
        what: "chunk record",
        message: e.to_string(),
    })?;
    Ok(record.body.ok_or(FormatError::Decode {
        what: "chunk record",
        message: "empty record".to_owned(),
    })?)
}

fn find_run(block: &[ChunkBody], id: RunId, allow: &dyn Fn(&str) -> bool) -> Option<RunRecord> {
    block.iter().find_map(|b| match b {
        ChunkBody::Run(r) if r.run_id() == id && allow(&r.instance) => Some(r.clone()),
        _ => None,
    })
}

fn find_failed(block: &[ChunkBody], seq: u32) -> Option<FailedRun> {
    block.iter().find_map(|b| match b {
        ChunkBody::Failed(f) if f.seq == seq => Some(f.clone()),
        _ => None,
    })
}

/// Adds the allowed records of one whole chunk to `view`.
fn add_records(
    view: &mut RecordsPage,
    object: &[u8],
    allow: &dyn Fn(&str) -> bool,
) -> Result<(), ReadError> {
    let decoded = container::decode(object)?;
    for record in &decoded.records {
        let body = decode_record(record)?;
        if !allow(body.instance()) {
            continue;
        }
        match body {
            ChunkBody::Run(r) => view.runs.push(r),
            ChunkBody::Failed(f) => view.failed.push(f),
            ChunkBody::Hit(h) => view.hits.push(h),
        }
    }
    Ok(())
}
