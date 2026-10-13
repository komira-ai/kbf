# The write path: segment upload, grouping and a local write tier (**planned**)

How bytes get from an upload into the store, and what an acknowledgement promises.
Today's path is in [storage.md](storage.md#writes). This document plans three changes:
**write grouping** (concurrent small REAPI uploads share one segment, one object PUT
and one metadata commit), **segment upload** (the daemon sends an action's outputs to
the server as whole segments over the worker listener, committed with one metadata
commit), and a **local write tier** (segments are written and synced to a server's
NVMe or SSD, acknowledged, and flushed to the object store in the background).
Nothing here is built yet.

## 1. The problem, measured

On a pilot node, one action's output was a toolchain of 15,189 files (307 MB). Its
upload took 45 minutes and then failed with
`CAS call for blob .../156925448 failed: Unavailable: object store: request to the store failed`.

What the code does with that output today:

- The daemon uploads each file with its own ByteStream `Write`
  (`kbf-daemon/src/cas.rs`), up to 64 at once (`kbf_outputs::UPLOADS_IN_FLIGHT`).
- Each `Write` buffers the whole blob, verifies it, and calls `Cache::store_blobs`
  with one blob (`kbf-front/src/bytestream.rs`). `BatchUpdateBlobs` calls it once per
  request (`kbf-front/src/cas.rs`).
- `store_blobs` (`kbf-front/src/cache.rs`) packs only the blobs of **its own call**
  into segments. So each file becomes its own segment: one `put_new`, then two
  `get_range` calls to read the trailer and footer back (`read_footer`), then one
  `Command::PutBlobs`. Under `--meta-log=raft` each commit is proposed, synced and
  applied under the log's lock, one at a time (`kbf-server/src/raft_log/mod.rs`).
  Only then is the caller answered.
- `kbf-objstore` does not retry. One failed PUT fails that blob's upload, and the
  daemon fails the action.

For the toolchain that is 15,189 PUTs, 30,378 GETs and 15,189 Raft commits. The
bucket on that node sits on erasure-coded HDDs with 0.6 to 1.3 s write latency,
shared with another workload's storage. The measured throughput, about 5.6 blobs a
second, is that latency divided by the client's concurrency, plus queueing in the store.

The store's cost is per request, not per byte. The fix is to make far fewer requests,
and to take the slow store off the path that acknowledges an upload.

## 2. Decided, and what stays the same

- **Architecture (decided).** Uploads are written to local NVMe/SSD on the server and
  flushed to the object store asynchronously (section 6). Segment upload (section 4) is
  the daemon's write path. REAPI batching and write grouping (section 3) stay for buck2
  and other standard clients.
- **The index model (decided).** Clients hold digests: an `ActionResult` names output
  files and `Tree`s by digest, and buck2 asks for each by digest. The server's Raft
  index maps each digest to `Location { store, object, offset }`; the length is the
  digest's size. A segment ([storage.md](storage.md#segments)) is the farm's "one
  tarball": many records back to back, with an index in its footer. A flush from the
  tier to the store changes only the **object's** state (staged, then stored). No
  blob's location changes.
- **Not goals (decided):** dedup across versions, and base-plus-diff storage. Two
  toolchain versions are two sets of segments. A file whose bytes are identical in both
  has one digest and is stored once already, because a blob already present is touched,
  not written again. FastCDC chunking (`kbf-segments`) remains the planned tool for
  large single files; nothing here depends on it.
- The three answers (`Present`, `Unavailable`, `Absent`), the closure check, and
  "never report a blob present before its bytes are durable" all hold. Section 6.3
  changes what *durable* means while the tier holds a segment. That is the one real
  trade in this design, stated there plainly.

## 3. Write grouping (REAPI clients)

### 3.1 Shape

A `WriteGroup` sits between the callers (`Write`, `BatchUpdateBlobs`, anything that
calls `store_blobs`) and `put_segment`. It has one **open segment** (a
`SegmentWriter`) and the callers waiting on it.

1. A caller's blobs are verified and checked against the index as today (present ones
   are touched, not written). The rest go to the group with a one-shot reply channel.
2. The group pushes them into the open segment. A digest already in it is a no-op push
   (`SegmentWriter::push` already does this), and both callers wait on that segment.
3. The open segment is **sealed** when the next blob would not fit (`WriteError::Full`)
   or when its oldest blob has waited `--write-linger` (default 2 ms).
4. A sealed segment is written as today: one `put_new`, the footer read back, one
   `Command::PutBlobs` listing every blob in it. Then every caller with a blob in it is
   answered. A caller whose blobs span two segments is answered when both are settled.

Latency: grouping itself adds at most the linger. After the seal a segment also waits
for a PUT slot (`--write-puts-in-flight`), the commits queued ahead of it and, with the
tier on, the shared block's sync.

No caller is answered before its segment's `PutBlobs` has committed. Without the tier
the guarantee is today's: **an acknowledged blob is in the object store and indexed in
the committed metadata log**.

### 3.2 Large blobs

A blob of `--write-own-segment-bytes` (default 16 MiB) or more bypasses the group and
is written as a segment of its own (`put_whole`, as today), so it neither waits for a
linger nor holds up small writes. It is still held in memory until verified
(`MAX_BLOB_BYTES`, 1 GiB) until the tier lets it stream to disk (section 6.2).

### 3.3 Bounds and backpressure

- **Admission.** `--write-bytes-in-flight` (default 1 GiB) is an async semaphore
  counted in bytes. A ByteStream `Write` takes a permit for the size named in its
  resource name **before it reads its first message**, so the bytes it buffers in
  `received` are covered. `BatchUpdateBlobs` takes a permit for its total before it
  calls `store_blobs`. Own-segment blobs count too. The server refuses to start with a
  value below `MAX_BLOB_BYTES`, so a single legal blob can always be admitted. A caller
  over the bound waits; nothing is refused for load.
- **What it does not cover.** A `BatchUpdateBlobs` request is decoded by tonic before
  its handler runs: up to the 4 MiB message limit per stream, times the HTTP/2
  concurrent-stream limit. That limit is set explicitly on both listeners and named in
  the flag reference, so the total is a stated number, not "bounded".
- `--write-puts-in-flight` (default 4) bounds concurrent segment PUTs. A segment
  sealed while all are busy waits; new blobs go to a new open segment.
- Commits stay one at a time. Each now carries up to a segment's worth of blobs.

### 3.4 Failure

- **PUT fails.** It is retried up to `--store-put-attempts` (default 3) on
  `Unavailable` and timeouts only, with backoff (100 ms, then 400 ms), each retry under
  a **new object id** (the next sequence number). A retry never reuses a key, so a
  first PUT that the store kept but reported as failed cannot make the retry fail on
  `AlreadyExists` under `conditional_put`. The object it may have left is an orphan for
  the planned sweep ([storage.md](storage.md#planned)). If every attempt fails, the
  segment's blobs are not indexed and each of them is answered `UNAVAILABLE`.
- **Per-blob answers.** `BatchUpdateBlobs` reports a status per blob, so a request whose
  blobs landed in two segments gets a mixed answer: `OK` for blobs in the committed
  segment, `UNAVAILABLE` for the other. A ByteStream `Write` has one blob and one answer.
- **Read-back disagrees** (trailer or footer does not parse, or a blob is missing from
  it): `INTERNAL` for the segment's blobs, nothing indexed, not retried. A store that
  acknowledges and then returns something else is a fault to surface.
- **Commit fails** after a good PUT: the commit's error for the segment's blobs,
  nothing indexed, the object an orphan.
- A caller that disconnects while waiting changes nothing: its blobs are still written
  and indexed with the segment.

### 3.5 Footer read-back: kept

Two GETs per segment, not per blob. It is the only check that the store holds a
parseable object of the length we wrote before locations are committed into it. SigV4
signs the payload hash, so a body damaged in transit is refused, but that does not prove
the object was kept whole. With the tier it runs in the flusher, off the ack path.

## 4. Segment upload (the daemon, worker listener)

The daemon knows an action's whole output set at once. Instead of asking the server to
group thousands of single-blob calls, it builds the segments itself and uploads them as
one unit. This is kbf-specific and served only on the worker listener, under its
admission rules ([worker-protocol.md](worker-protocol.md#blobs-on-the-worker-listener)).

### 4.1 Shape

1. The daemon hashes every output file (it already does, to build the `ActionResult`)
   and calls `FindMissingBlobs` once for the set, in batches under the request limit.
2. It writes the missing blobs with `kbf_segments::SegmentWriter` into one segment, or
   several when they pass `MAX_SEGMENT_BYTES` (128 MiB). A blob larger than that is a
   segment of one record, as `put_whole` is today. The segment's **footer is the
   manifest**: (hash, size, offset, crc32c) per record, in the format the store already
   reads. (It is not the chunking `Manifest` of `kbf-segments`.) Records are written to
   a spool file on the worker's disk, not held in memory.
3. `kbf.worker.v1.Segments/Begin { segments: [{ length, footer }] }` opens an upload
   and returns its id. The server refuses a footer that does not parse, an entry past
   `index_offset`, overlapping records, or a total over its per-upload cap.
4. `Segments/PutRange { upload, segment, offset, bytes }` calls carry the record bytes,
   up to `--segment-upload-streams` (default 16) at once as HTTP/2 streams on the
   daemon's one connection, each range up to 4 MiB. A range outside the declared
   records, or overlapping one already received, is refused. A failed range is
   resent; ranges are idempotent by offset.
5. `Segments/Commit { upload }`. The server checks every byte range is present, then
   checks **every record's SHA-256 against its footer entry**, then stores each segment
   (on the tier, section 6; else one `put_new` plus read-back, as section 3), then
   commits **one** `PutBlobs` (or `PutStaged`) listing every record of every segment of
   the upload. The reply lists each digest's outcome. Only then does the daemon write
   the `ActionResult`.

A daemon cannot poison the cache this way: a record whose bytes do not match its footer
fails the commit (`INVALID_ARGUMENT`, nothing indexed), exactly as a mismatched
ByteStream write does today. A blob that became present between step 1 and the commit
is indexed once; its new record is dead bytes in the segment, reclaimed with it.

### 4.2 Bounds and failure

- Without the tier, an upload's ranges are held in memory until commit, so `Begin`
  takes a `--write-bytes-in-flight` permit for its total. With the tier they are written
  at their offsets into the segment's file as they arrive (`pwrite`), and the permit
  covers only ranges in flight.
- An upload with no call for `--segment-upload-idle` (default 60 s) is dropped and its
  spool or tier file deleted; it was never committed, so nothing points at it.
- A failed `Commit` (store, commit or hash) indexes nothing. The daemon may retry the
  whole upload once, then falls back to today's per-blob ByteStream path, so a server
  without the service (`UNIMPLEMENTED`) still works.
- A server restart drops open uploads; the daemon sees `UNAVAILABLE` and starts over.

### 4.3 Downloads: segment-range fills

The same layout serves reads. For a cache fill of a `Tree` or an action's inputs, the
daemon calls `Segments/Locate { digests }` and gets `(object, offset, length)` per
blob. Blobs that share an object and sit close together are coalesced into large
ranges (gaps under 1 MiB are read through and dropped), and `Segments/ReadRange
{ object, offset, length }` calls run in parallel on the one connection, through the
transfer engine of [worker-cache.md](worker-cache.md#4-the-shared-transfer-engine). The
server serves the range from the tier when the object is staged or still local, else
from the store. The daemon checks every blob's SHA-256 as it does today. Object ids are
stable across a flush, so a `Locate` answer stays valid; a range the server can no
longer serve is `NOT_FOUND` for that call and the daemon falls back to per-blob reads.

## 5. Capabilities (REAPI clients)

- `max_batch_total_size_bytes` is **already** advertised as `MAX_BATCH_TOTAL_BYTES`
  (4 MiB, `kbf-front/src/capabilities.rs`). Clients that read it size their
  `BatchUpdateBlobs` calls by it. Whether buck2 reads it, or uses its own batch-size
  setting, is to be checked on the pilot; the doc does not assume either.
- **zstd.** Advertise `supported_compressors` and
  `supported_batch_update_compressors` = `[ZSTD]`, and serve
  `compressed-blobs/zstd/...` on ByteStream reads and writes (refused today in
  `kbf-front/src/wire.rs`). As `capabilities.rs` says, it is advertised only in the
  same change that decodes it, since a client that sees it will send it. Stored bytes
  stay uncompressed (records are found by offset and checked by SHA-256); compression
  is on the wire only.

## 6. The local write tier

### 6.1 Shape

With `--write-tier-dir=<path>` (on NVMe or SSD) and `--write-tier-bytes=<budget>`, a
sealed group or a committed segment upload is not PUT. It is written to a **block
file** and synced, then committed. A block is laid out as a segment, so a blob's offset
in the block is its offset in the object the block becomes. This follows Buildbarn's
`LocalBlobAccess`: append blobs to large local blocks, keep a key-to-location map,
acknowledge after the append.

1. A block is named by its `ObjectId` (writer epoch, sequence number) as a flat file,
   `<dir>/<epoch>-<seq>.blk`. Creating it, and writing `<dir>/TIER`, is followed by
   `sync_dir` before any record in it is acknowledged.
2. **Grouped writes** append to the one open block. Each group reserves its byte range
   in order, writes, `fdatasync`s, then commits `Command::PutStaged { tier, objects:
   [{ object, end, blobs }] }`. It records each blob at `Location { store: CONFIGURED,
   object, offset }` and marks the object **staged on `tier`**. The state machine sets
   the object's committed length to `max(len, end)`, so commits that finish out of
   order never move it backwards, and it refuses `PutStaged` for an object that is
   sealed or flushed. A group whose commit fails leaves a gap of dead bytes below a
   later group's `end`; `Footer::parse` already tolerates gaps (it checks only that
   each entry ends at or before `index_offset`, `kbf-segments/src/layout.rs`).
3. **Segment uploads** are each their own block, written whole (section 4.2), synced,
   and committed with one `PutStaged` covering every object of the upload, each marked
   sealed in that same commit (nothing else can append to it).
4. Callers are answered after that commit.
5. **Seal.** An open block is sealed at the segment limit or after
   `--write-tier-seal-after` (default 30 s). Sealing first stops new reservations, then
   waits until every reserved range has been answered (committed or failed), then
   commits `Command::SealStaged { object }`. After that, `PutStaged` for the object is
   refused. The flusher builds the footer from the committed blob list as of the seal,
   re-reads every record and checks its SHA-256, appends the footer, PUTs the object at
   its usual key, reads the footer back, and commits `Command::ObjectFlushed { object }`.
6. **Reads.** The location answer carries the staged state from the same metadata
   query: `BlobAnswer::Present { location, staged_on }`. A staged blob is read from its
   block. `Cache::fetch` (`kbf-front/src/cache.rs`) and the batch read fallback must
   never commit `ObjectUnreachable { Missing }` for an object that was staged when the
   query ran; a local read that fails with ENOENT **after** `ObjectFlushed` falls back
   to the store. A flushed object may still be read locally while its file remains.
7. **Eviction** deletes only blocks whose `ObjectFlushed` is applied in committed
   state, never on a flusher-local flag, least recently read first. A staged block is
   never evicted. When unflushed bytes reach the budget, new writes fall back to the
   direct path (section 3), and a metric says so. **If the store is also down**, that
   fallback fails too and uploads get `UNAVAILABLE` until the store returns or the
   flusher frees space.

The index gains one small table: staged objects, each with its tier, committed length,
seal state and blob list. It holds only what has not been flushed, so it is bounded by
the budget. `tier` is the server's tier id, committed once by `Command::AddTier
{ server, id }` the first time a directory is opened, and written to `<dir>/TIER`.

**`Fs` needs work first.** `kbf-store`'s `Fs` is one flat directory with whole-file
`read` only (`kbf-store/src/fs.rs`). Flat block names fit it; a 128 MiB whole-file read
per blob read does not. PR 3 adds a ranged `read_at` and a positioned write to `Fs` and
`FaultFs`, so the crash tests run through the same trait.

### 6.2 Large blobs on the tier

A ByteStream `Write` over `--write-own-segment-bytes` streams into its own block as it
arrives (bytes come in order from offset 0), hashed as written, synced, and committed
as a staged object of one record. Memory per stream is then one message. The 1 GiB
`MAX_BLOB_BYTES` refusal can be relaxed later for the tier path; the direct path keeps it.

### 6.3 What an acknowledgement means now

**With the tier on, an acknowledged blob is on one server's local disk (synced, its
name synced) and in the committed metadata log. It is not yet in the object store.** It
survives a process crash, a restart and a power loss of that host. It does **not**
survive losing that disk before the block is flushed. The window is the seal delay plus
the flush: about 30 s plus one PUT normally, longer while the store is down. Metrics
report the unflushed bytes and the age of the oldest unflushed block; an alert fires on
the age.

Today's deployment is one server whose Raft log is on its local disk (`--raft-dir`).
Losing that host already loses metadata committed since the last copied snapshot
([storage.md](storage.md#planned)). The tier adds a second thing on that host that a
disk loss takes, and no new host whose failure loses data.

**Later, with three servers** ([deployment-topology.md](deployment-topology.md)): the
receiving server sends each block's bytes to the tiers of two others before it
commits; `PutStaged` names every tier that synced them, and the ack waits for two, so
one lost disk loses nothing. Until then the guarantee is the one-disk one above.

### 6.4 Crash and restart

Every start takes a new writer epoch (`Cache::open`), so a block from an earlier start
never gets another append. At open, the server checks `<dir>/TIER` first:

- **`TIER` missing or naming another id while objects are staged on this server's
  tier:** the server **refuses to start**. A mount mistake or a wrong flag must not
  destroy the index. Only `--write-tier-abandon=<id>`, naming the old tier, makes it
  commit `ObjectUnreachable { Missing }` for every object staged there and register a
  new tier. The blobs become `Unavailable`: `FindMissingBlobs` reports them missing,
  clients upload them again, and the new upload heals the index
  ([storage.md](storage.md#three-answers-not-two)). An action entry whose closure names
  one is a miss. Nothing is reported `NOT_FOUND` and no wrong bytes are served.
- **`TIER` matches:** for each staged object, a file of at least its committed length
  is truncated to that length (bytes past it were never answered) and, if sealed,
  queued for the flusher; an unsealed one is sealed. A **single** file missing or short
  marks only that object `ObjectUnreachable { Missing }`, with an alert.

Crash and conflict points in the flusher:

- After the PUT and before `ObjectFlushed`: the block is still staged and is flushed
  again under **the same key**. The footer is deterministic (built from the committed
  list, sorted by digest), so that key holds the same bytes. On `AlreadyExists`
  (`conditional_put`), the flusher reads the footer back; if it matches, it commits
  `ObjectFlushed`.
- If it does not match, the flusher commits `Command::FlushConflict { object }`: the
  object **stays staged**, its blobs stay `Present` and are read from the local block,
  the flusher stops retrying it, and an alert fires. It is not marked `Corrupt`, because
  `ObjectUnreachable { Corrupt }` makes `s.blob()` answer `Unavailable` and would stop
  reads of good local bytes. An operator resolves it; the block is never evicted.
- A record whose SHA-256 fails in the flusher's re-read means the local disk damaged
  it. The block is not uploaded; it is marked `ObjectUnreachable { Corrupt }` (reads
  stop, as they should), and its blobs heal by re-upload.

### 6.5 Log format

`PutStaged`, `SealStaged`, `ObjectFlushed`, `FlushConflict` and `AddTier` are new
`kbf-log` command variants. An older binary already refuses them: an unset command
decodes as `DecodeError::UnknownCommand` (`kbf-log/src/meta.rs`), the Raft machine
stops on it (`kbf-server/src/raft_log/machine.rs`), and opening the log returns
`RaftLogError::Decode` (`decoded`, `raft_log/mod.rs`). PR 2 adds a test that pins this
for the new variants. The snapshot encoding ([metadata-snapshots.md](metadata-snapshots.md))
gains the staged table.

## 7. Expected effect on the 15,189-file upload

Estimates from the measured store latency, to be measured on the pilot at each stage.

| stage | PUTs | read-back GETs | Raft commits | on the ack path | expected time |
|---|---|---|---|---|---|
| today | 15,189 | 30,378 | 15,189 | every PUT | 45 min, then failed |
| 1. grouping | about 240 | about 480 | about 240 | about 240 PUT rounds | about 5 to 10 min |
| 2. + segment upload | 3 | 6 | 1 | 3 PUTs in parallel | under a minute |
| 3. + tier | 3 (flusher) | 6 (flusher) | 1 `PutStaged`, then 1 `ObjectFlushed` per segment | local sync, one commit | network and hashing bound, seconds |

- **Grouping alone** groups only callers waiting at once, and the daemon sends 64
  single-blob `Write`s at a time, so segments hold about 64 blobs (15,189 / 64 is about
  240) and the rounds run one after another. The PUT retry removes the single-PUT
  failure of section 1.
- **Segment upload** replaces 15,189 calls with one `FindMissingBlobs` pass, one
  `Begin`, about 74 ranges of 4 MiB 16 at a time, and one `Commit`. The store sees 3
  objects of up to 128 MiB, written in parallel; each pays the HDD latency once.
- **The tier** takes the store off the ack path: a store outage delays the flush, not
  the action. Buck2 uploads through REAPI also gain: grouped blocks fill to 128 MiB
  whatever the client's concurrency.

## 8. Rollout

1. **PR 1, grouping and admission** (`kbf-front`, new module `write_group.rs`, since
   `cache.rs` is near its size limit): the group, the PUT retry, the admission permit at
   stream start, the explicit concurrent-stream limit. Flags: `--write-linger`,
   `--write-own-segment-bytes`, `--write-bytes-in-flight`, `--write-puts-in-flight`,
   `--store-put-attempts`. No format change. Ships alone; measure the toolchain upload.
2. **PR 2, segment upload**: the `Segments` service on the worker listener (`Begin`,
   `PutRange`, `Commit`, `Locate`, `ReadRange`) in `kbf-proto` and `kbf-front`, and the
   daemon's spool, upload and fallback in `kbf-daemon`/`kbf-outputs`. Direct path only:
   commit is `PutBlobs` after `put_new`. Downloads through the transfer engine follow it.
3. **PR 3, tier metadata**: the five commands in `kbf-meta` and `kbf-log`, the staged
   table, `BlobAnswer::Present { staged_on }`, the refusal test, snapshot encoding if
   snapshots have landed. Nothing writes them yet.
4. **PR 4, the tier**: the `Fs` extension, block files, append and sync, seal, the
   flusher, local reads, eviction on committed state, restart recovery, the abandon
   flag, metrics. Flags: `--write-tier-dir`, `--write-tier-bytes`,
   `--write-tier-seal-after`, `--write-tier-abandon`. Off unless `--write-tier-dir` is
   given; the OSS default is no tier, which is today's guarantee.
5. **PR 5**: the tier in `kbf-sim-cell`, then on the pilot with the unflushed-age alert
   in place first.
6. zstd on ByteStream and the capability, independent of the rest.

## 9. Tests, and the defect each catches

Grouping (PR 1), over `MemoryStore` wrapped to count and fault requests:

| Test | Catches |
|---|---|
| 64 concurrent single-blob `store_blobs` make at most a few PUTs and `PutBlobs` | grouping not engaged; mutant: seal on every push |
| a store whose `put_new` blocks: no caller returns, and `is_durable` is false, until the commit | acknowledging before the commit |
| a failing store: the segment's blobs `UNAVAILABLE` and absent; a caller in another segment succeeds | indexing a failed segment; failure spreading |
| a `BatchUpdateBlobs` spanning a failed and a good segment gets per-blob `OK` and `UNAVAILABLE` | one status smeared over the batch |
| commit refused after a good PUT: all fail, nothing present | acknowledging on PUT alone |
| PUT fails twice, then succeeds, on a `conditional_put` store: one `PutBlobs`, keys differ | retry under the same key |
| a lone write under a paused clock is sealed at the linger | waiting forever for a full segment |
| a blob at `--write-own-segment-bytes` gets its own segment without the linger | large blobs held up by small ones |
| the same digest from two callers: one record, both answered | duplicate records; a caller never answered |
| 32 concurrent 64 MiB `Write`s with a 512 MiB bound: peak buffered bytes stay under the bound | permit taken after buffering; mutant: permit in `store_blobs` |
| a start with `--write-bytes-in-flight` below `MAX_BLOB_BYTES` is refused | a legal blob waiting forever |

Segment upload (PR 2):

| Test | Catches |
|---|---|
| 15,000 small outputs: one `Begin`, one `Commit`, one `PutBlobs`, every blob present after | per-blob commits; mutant: commit per segment |
| one record's bytes altered after `Begin`: `INVALID_ARGUMENT`, nothing indexed | trusting the daemon's footer |
| a missing range at `Commit` is refused; an overlapping range is refused | indexing bytes never received |
| ranges arriving in reverse order commit correctly | assuming in-order ranges |
| a server answering `UNIMPLEMENTED`: the daemon falls back to ByteStream and the action succeeds | an old server failing new daemons |
| an idle upload is dropped and its file deleted; nothing indexed | leaked spool or tier files |
| `Locate` + coalesced `ReadRange` returns every blob of a 1,000-file tree with at most ceil(bytes / range cap) reads | fills still one call per blob |

Tier (PRs 3 to 5), over `Fs` with `FaultFs` dropping unsynced writes **and unsynced
directory entries** at a simulated crash:

| Test | Catches |
|---|---|
| acknowledged blobs read back after a crash | ack before `fdatasync`; mutant: skip the sync |
| a new block acknowledged, crash: the file still exists | ack before `sync_dir`; mutant: skip it |
| crash between append and `PutStaged`: tail truncated, nothing indexed | uncommitted bytes treated as stored |
| two groups on one block whose commits finish out of order, crash: both readable | committed length moving backwards |
| a group appended and synced but not committed when the seal fires: the seal waits; a late `PutStaged` after `SealStaged` is refused | a location indexed into an object whose footer lacks it |
| every staged blob read through `fetch` and the batch path against a store without the object: nothing marked, bytes served | a staged blob turned `Unavailable` by a reader |
| crash after the flush PUT, before `ObjectFlushed`: one object, staged mark cleared | a double flush failing, or orphaning the block |
| a same-key object whose footer differs: `FlushConflict`, blobs still `Present` and read locally | a conflict stopping reads of good bytes |
| `ObjectFlushed` commit refused: the block is not evicted | evicting the only copy on a local flag |
| a tiny budget never evicts a staged block; at the budget writes fall back to direct PUTs | eviction of the only copy; writes refused at the budget |
| a wrong or missing `TIER` refuses start; with `--write-tier-abandon` objects become `Missing`, re-upload heals, no `NOT_FOUND` | a mount mistake destroying the index |
| one block file deleted: only that object `Missing` | one lost file failing the whole tier |
| a corrupted local record is not uploaded and is marked `Corrupt` | flushing damaged bytes |
| a log holding each new variant, opened by a reader without it, is refused | silently skipping a staged write |
| `kbf-sim-cell`: no acknowledged blob is ever absent or unreadable unless a disk-loss fault was injected, and then only as `Unavailable` | interleavings the tables above miss |

## 10. Open questions

- Linger default (2 ms), measured in PR 1.
- Packing small sealed blocks into one object: changes locations; not planned.
- With three servers, whether the ack waits for two tier syncs or one sync plus the PUT.
- Whether buck2 reads `max_batch_total_size_bytes` and would send zstd if advertised
  (section 5), to be checked on the pilot before PR 6 is prioritized.
