# The write path: grouping and a local write tier (**planned**)

How bytes get from an upload into the store, and what an acknowledgement promises.
Today's path is in [storage.md](storage.md#writes); this document plans two changes to
it: **write grouping** (many small uploads share one segment, one object PUT and one
metadata commit) and a **local write tier** (segments are written and synced to a
server's local NVMe or SSD, acknowledged, and flushed to the object store in the
background). Nothing here is built yet.

## 1. The problem, measured

On a pilot node, one action's output was a toolchain of 15,189 files (307 MB). Its
upload took 45 minutes and then failed with
`CAS call for blob .../156925448 failed: Unavailable: object store: request to the store failed`.

What the code does with that output today:

- The daemon uploads each file with its own ByteStream `Write`
  (`kbf-daemon/src/cas.rs`), up to 64 at once (`kbf_outputs::UPLOADS_IN_FLIGHT`).
- Each `Write` buffers the blob, verifies it, and calls `Cache::store_blobs` with one
  blob (`kbf-front/src/bytestream.rs`). `BatchUpdateBlobs` calls it once per request
  (`kbf-front/src/cas.rs`).
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

## 2. What stays the same

- **Per-file digests stay.** REAPI clients (buck2, Bazel) name every file by its
  digest and ask for it by that name: `FindMissingBlobs`, `ByteStream.Read`, a
  `Tree`'s entries. The farm must answer per file. That says nothing about how files
  are *stored*: a segment ([storage.md](storage.md#segments)) already holds many
  blobs in one object, each found by `(object, offset)` from the index. A segment
  is the farm's "one tarball", with an index in its footer.
- **Goal: a package is a few objects, not one per file.** 307 MB of toolchain should
  be about three 128 MiB objects in the store.
- **Not a goal: dedup across versions.** Two toolchain versions are two sets of
  segments. A file whose bytes are identical in both has one digest and is stored
  once already, because a blob already present is touched, not written again. Nothing
  here builds base-plus-diff storage. FastCDC chunking (`kbf-segments`) remains the
  planned tool for large single files that change slightly; it does nothing for many
  small files.
- The three answers (`Present`, `Unavailable`, `Absent`), the closure check, and
  "never report a blob present before its bytes are durable" all hold. Section 4
  changes what *durable* means for an acknowledged blob. That section states the
  change plainly; it is the one real trade in this design.

## 3. Write grouping

### 3.1 Shape

A `WriteGroup` sits between the callers (`Write`, `BatchUpdateBlobs`, the daemon's
action-result path, anything that calls `store_blobs`) and `put_segment`. It has one
**open segment** (a `SegmentWriter`) and the list of callers waiting on it.

1. A caller's blobs are verified and checked against the index as today (present ones
   are touched, not written). The rest go to the group with a one-shot reply channel.
2. The group pushes them into the open segment. A digest already in the open segment
   is a no-op push (`SegmentWriter` already does this), and both callers wait on the
   same segment.
3. The open segment is **sealed** when the next blob would not fit
   (`WriteError::Full`), or when the oldest blob in it has waited `--write-linger`
   (default 2 ms). A lone write is never held longer than the linger.
4. A sealed segment is written as today: one `put_new`, the footer read back, one
   `Command::PutBlobs` listing every blob in it. Then **every caller with a blob in
   the segment is answered at once**. A caller whose blobs span two segments is
   answered when the last one commits.

No caller is answered before its segment's `PutBlobs` has committed, so the
durability guarantee is today's: **an acknowledged blob is in the object store and
indexed in the committed metadata log**.

### 3.2 Large blobs

A blob of `--write-own-segment-bytes` (default 16 MiB) or more bypasses the group: it
is written as a segment of its own (`put_whole`, as today) and does not wait for a
linger or hold up small writes. Such blobs are still held in memory until verified,
as today (`MAX_BLOB_BYTES`, 1 GiB). Streaming them to disk instead is part of the tier
(section 4.2), where the bytes have somewhere to go before the PUT.

### 3.3 Bounds and backpressure

- `--write-bytes-in-flight` (default 512 MiB) bounds the bytes held in the open
  segment plus sealed segments still being written. A caller that would exceed it
  waits for space (an async semaphore counted in bytes) before its bytes are taken.
  The gRPC call just takes longer, and the client's own concurrency limit slows it
  down. Nothing is dropped and nothing is refused for load.
- `--write-puts-in-flight` (default 4) bounds concurrent segment PUTs. A segment
  sealed while all are busy waits sealed; new blobs go to a new open segment, still
  under the byte bound.
- Commits stay one at a time, in the order PUTs finish. Each commit now carries up to
  a segment's worth of blobs instead of one.

### 3.4 Failure

- **PUT fails.** The PUT is retried up to `--store-put-attempts` (default 3) on
  `Unavailable` and timeouts only, with backoff (100 ms, then 400 ms), each retry
  under a **new object id** (the next sequence number). A retry never reuses a key, so
  a first PUT that the store kept but reported as failed cannot make the retry fail on
  `AlreadyExists` under `conditional_put`. The object it may have left behind is an
  orphan, which the planned sweep ([storage.md](storage.md#planned)) removes by
  comparing the bucket with farm state. If every attempt fails, **every caller in that
  segment gets `UNAVAILABLE` and nothing in it is indexed**. Callers in other segments
  are not affected. A client retry starts over and is grouped again.
- **Read-back disagrees** (trailer or footer does not parse, or a blob is missing from
  it): `INTERNAL` for every caller in the segment, nothing indexed, not retried. A
  store that acknowledges and then returns something else is a fault to surface, not
  to hide.
- **Commit fails** after a good PUT: every caller gets the commit's error and nothing
  is indexed. The object is an orphan, as today.
- A caller that disconnects while waiting changes nothing: its blobs are still
  written and indexed with the segment (a later upload finds them present).

### 3.5 Footer read-back: kept

Read-back costs two GETs per segment. Today that is two per blob; after grouping it is
two per few hundred blobs. It is the only check that the store holds a parseable object
of the length we wrote before we commit locations into it. SigV4 signs the payload
hash, so the store refuses a body damaged in transit, but that does not prove the
object was kept whole. The check is cheap now, and with the tier (section 4) it runs off
the acknowledgement path. Dropping it would save little and lose the last end-to-end
check before an index entry exists.

## 4. The local write tier

### 4.1 Shape

With `--write-tier-dir=<path>` (on NVMe or SSD) and `--write-tier-bytes=<budget>`,
a sealed group is not PUT. It is **appended to a local block file** and synced, and
then committed. A block is laid out as a segment: records back to back from offset 0,
with the footer added when the block is sealed. So a blob's offset in the block is its
offset in the object the block becomes, and moving it to the store changes no blob's
location. This follows the model of Buildbarn's `LocalBlobAccess`: append blobs to
large local blocks, keep a key-to-location map, acknowledge after the append.

1. The server has one open block, named like any segment, by `ObjectId` (its writer
   epoch and the next sequence number), at `<dir>/<epoch>/<seq>`.
2. A group's records are appended at the block's current end. Then `fdatasync`, then
   one commit: `Command::PutStaged { tier, object, blobs, len }`. It records each blob
   at `Location { store: CONFIGURED, object, offset }`, as `PutBlobs` would, and marks
   the object **staged on `tier`** with `len` committed bytes and its blob list. All or
   nothing.
3. Callers are answered after that commit.
4. A block is sealed when it reaches the segment limit (128 MiB) or after
   `--write-tier-seal-after` (default 30 s). The flusher then builds the footer from
   the staged blob list, re-reads every record and checks its SHA-256, appends the
   footer, PUTs the object at its usual key, reads the footer back, and commits
   `Command::ObjectFlushed { object }`. That clears the staged mark.
5. **Reads**: a staged object is read from its block file, verified as every read is.
   A flushed object may still be read locally while its file remains; otherwise it is
   read from the store as today.
6. **Eviction**: when the tier's files exceed the budget, flushed blocks are deleted,
   least recently read first. **A staged block is never evicted.** When the unflushed
   bytes alone reach the budget, new groups fall back to the direct path of section 3
   (slower, and durable in the store), and a metric says so.

The index gains one small table: staged objects, each with its tier, committed length
and blob list. It holds only what has not been flushed, so it is bounded by the
budget. `Location` keeps naming the object's home in the store; "not there yet" lives in
one place, the staged table, not in every blob's entry.

`tier` is the server's tier id, committed once by `Command::AddTier { server, id }` the
first time a directory is opened. The id is also written into the directory
(`<dir>/TIER`), so a server that comes up with a different or empty directory can
tell it is not the disk the index remembers.

### 4.2 Large blobs on the tier

A blob over `--write-own-segment-bytes` is streamed into its own block as it arrives
(ByteStream already gets bytes in order from offset 0). It is hashed as it is written,
synced, and committed as a staged object of one record. Memory per stream is then one
message, not the whole blob. The 1 GiB `MAX_BLOB_BYTES` refusal can be relaxed later
for the tier path. The direct path keeps it.

### 4.3 What an acknowledgement means now

**With the tier on, an acknowledged blob is on one server's local disk (synced) and in
the committed metadata log. It is not yet in the object store.** It survives a process
crash, a restart and a power loss of that host. It does **not** survive losing that
disk before the block is flushed. The window is the seal delay plus the flush time:
about 30 s plus one PUT in normal operation, longer if the store is down. Two metrics
report it: the unflushed bytes and the age of the oldest unflushed block, and an alert
fires on the age.

Today's deployment is one server, and its Raft log is on that server's local disk
(`--raft-dir`). Losing that host already loses the metadata committed since the last
copied snapshot ([storage.md](storage.md#planned)). The tier adds a second thing on
that host that a disk loss takes. It adds no new host whose failure loses data.

**Later, with three servers** ([deployment-topology.md](deployment-topology.md)):
the receiving server sends each group to the tiers of two other servers before it
commits. `PutStaged` names every tier that synced it, and the ack waits for two syncs,
so one lost disk loses nothing. Reads of a staged object go to any server that holds
it. Until that exists, the guarantee is the one-disk guarantee above, and the docs say
so.

### 4.4 Crash and restart

Every start takes a new writer epoch (`Cache::open`), so a block from an earlier start
never gets another append. At open, for each object staged on this server's tier:

- The file is present and at least `len` bytes long: bytes past `len` were appended
  but never committed, and no caller was answered for them. They are truncated. The
  block is sealed and queued for the flusher.
- **The file is missing or short, or `<dir>/TIER` is absent or names another id:** the
  disk was lost or replaced. The server commits `ObjectUnreachable { reason: Missing }`
  for every object staged on that tier. Their blobs become `Unavailable`:
  `FindMissingBlobs` reports them missing, clients upload them again, and the new
  upload heals the index ([storage.md](storage.md#three-answers-not-two)). An action
  entry whose closure names one is a miss, and the action runs again. Nothing is
  reported `NOT_FOUND`, and no wrong bytes are served. A new tier id is committed for
  the new directory.

Crash points in the flusher:

- After the PUT and before `ObjectFlushed`: on restart the block is still staged and
  is flushed again under **the same key**. That key holds the same bytes. On
  `AlreadyExists` (`conditional_put`), the flusher reads the footer back, and if it
  matches the block, commits `ObjectFlushed`. If it does not match, the object is
  marked `Corrupt` and stays staged, so reads keep going to the local copy and an
  alert fires.
- A record whose SHA-256 fails during the flusher's re-read means the local disk
  damaged it. The block is not uploaded. It is marked `ObjectUnreachable { Corrupt }`,
  and its blobs heal by re-upload as above.

### 4.5 Log format

`PutStaged`, `ObjectFlushed` and `AddTier` are new `kbf-log` command variants. A binary
that does not know them must refuse to open the log, not skip the entry: a skipped
`PutStaged` would leave blobs pointing at an object that is in no store. The snapshot
encoding ([metadata-snapshots.md](metadata-snapshots.md)) gains the staged table.

## 5. Expected effect on the 15,189-file upload

| | PUTs | GETs (read-back) | Raft commits | on the ack path |
|---|---|---|---|---|
| today | 15,189 | 30,378 | 15,189 | every PUT |
| grouping, daemon unchanged | about 240 | about 480 | about 240 | about 240 PUTs |
| grouping + daemon batching | about 3 | about 6 | about 3 | 3 PUTs |
| tier | about 3 (flusher) | about 6 (flusher) | about 240 `PutStaged` + 3 `ObjectFlushed` | local syncs only |

- **Grouping alone**: a group can hold only blobs whose callers are waiting at once,
  and the daemon sends 64 single-blob `Write`s at a time, so groups hold about 64 blobs
  (15,189 / 64 is about 240). The 64 callers wait on one segment together, so the
  rounds run one after another: about 240 x (one PUT plus read-back), roughly 5 to 10
  minutes at the measured latency. That is an estimate, to be measured.
- **With daemon batching** (`BatchUpdateBlobs` up to 4 MiB through the transfer engine
  of [worker-cache.md](worker-cache.md)), 64 batches in flight carry about 256 MiB.
  Segments then fill to 128 MiB, so 307 MB becomes 3 objects.
- **With the tier**, the ack waits on an NVMe sync and a Raft commit (milliseconds
  each), so the upload is bounded by the network and hashing. Appends fill blocks to
  128 MiB whatever the client's concurrency, so the store gets 3 objects either way.
  The failure in section 1 cannot fail the action: a store outage delays the flush, not
  the upload.

## 6. Rollout

1. **PR 1, grouping** (`kbf-front`, new module `write_group.rs`, since `cache.rs` is
   near its size limit). The PUT retry is in this PR. Flags: `--write-linger`,
   `--write-own-segment-bytes`, `--write-bytes-in-flight`, `--write-puts-in-flight`,
   `--store-put-attempts`. No format change and no new command. It can ship and run
   on the pilot alone. Measure the toolchain upload before and after.
2. **PR 2, metadata**: `PutStaged`, `ObjectFlushed`, `AddTier` in `kbf-meta` and
   `kbf-log`, the staged table, the refusal of unknown variants, and snapshot
   encoding if snapshots have landed. Nothing writes them yet.
3. **PR 3, the tier**: block files, append and sync, the flusher, local reads,
   eviction, restart recovery, metrics. Flags: `--write-tier-dir`,
   `--write-tier-bytes`, `--write-tier-seal-after`. Off unless `--write-tier-dir` is
   given. The OSS default is no tier, which is today's guarantee.
4. **PR 4**: the tier in `kbf-sim-cell`, then the tier on the pilot, with the
   unflushed-age alert in place first.
5. Separately: daemon batching through the transfer engine (worker-cache.md).

## 7. Tests, and the defect each catches

Grouping (PR 1), over `MemoryStore` wrapped to count and fault requests:

| Test | Catches |
|---|---|
| 64 concurrent single-blob `store_blobs` make at most a few PUTs and as many `PutBlobs` | grouping not engaged (one segment per call); mutant: seal on every push |
| a store whose `put_new` blocks: no caller returns, and `is_durable` is false, until the commit | acknowledging before the commit |
| a failing store: every caller in the segment gets `UNAVAILABLE`, none of its blobs is present, a caller in another segment succeeds | indexing a failed segment; one failure spreading to other segments |
| commit refused after a good PUT: all callers fail, nothing present | acknowledging on PUT alone |
| PUT fails twice, then succeeds, on a `conditional_put` store: one `PutBlobs`, keys all differ | a retry under the same key, refused by `AlreadyExists` |
| a lone write under a paused clock commits at the linger | waiting forever for a full segment |
| a blob at `--write-own-segment-bytes` gets its own segment without waiting for the linger | large blobs held up by, or packed with, small ones |
| the same digest from two callers: one record, both answered | duplicate records; a caller never answered |
| a stalled store: a caller past `--write-bytes-in-flight` waits, and memory stays bounded | unbounded buffering |
| no segment exceeds the segment limit | packing past `MAX_SEGMENT_BYTES` |

Tier (PRs 2 to 4), over `kbf-store`'s `Fs` trait with a disk that drops unsynced
writes at a simulated crash:

| Test | Catches |
|---|---|
| acknowledged blobs read back after a crash | acknowledging before `fdatasync`; mutant: skip the sync |
| crash between the append and `PutStaged`: tail truncated, nothing indexed | uncommitted bytes treated as stored |
| crash after the flush PUT, before `ObjectFlushed`: one object, staged mark cleared | a double flush failing, or orphaning the block |
| a tiny budget never evicts a staged block | eviction of the only copy |
| a replaced tier directory: staged objects `Missing`, blobs reported missing, re-upload heals, action lookups miss, no `NOT_FOUND` | a lost disk turned into "absent", or into served bytes |
| a corrupted local record is not uploaded and is marked `Corrupt` | flushing damaged bytes into the store |
| tier full: writes fall back to direct PUTs and are durable in the store | writes refused or lost when the budget is reached |
| a log holding `PutStaged`, decoded by a reader without the variant, is refused | silently skipping a staged write |
| `kbf-sim-cell`: no acknowledged blob is ever absent or unreadable unless a disk-loss fault was injected, and then only as `Unavailable` | any crash interleaving the tables above miss |

## 8. Open questions

- Linger default: 2 ms costs a lone write 2 ms and groups a 64-wide burst. To be
  measured on the pilot in PR 1.
- Whether the flusher should also pack small sealed blocks from a slow client into one
  object (a compaction). That would change offsets and so blob locations. Not planned;
  appending already fills blocks.
- Whether, with three servers, the ack should wait for two tier syncs or one sync plus
  the PUT, whichever comes first.
