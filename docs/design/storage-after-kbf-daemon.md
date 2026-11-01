# Storage after `kbf storage-and-daemon`: what tierstore replaces

Status: DESIGN, for the maintainers and the kbf storage owner. Nothing here is built. The storage owner reviewed the first
version against the code; their answers are folded into the facts, and section 8 lists only what is still open.

Every claim is tagged. **[code]** names the file and function (kbf paths are in this repository at the commit this
branch starts from; tierstore paths are in the tierstore repository at its main). **[proposed]** is a suggestion that
no code backs yet. Where a design document and the code disagree, the code is quoted and the document called out.

## 1. The question, and the short answer

Each farm node runs `kbf storage-and-daemon`: a storage half (hot store, cold store, metadata log, peers) and a daemon
half in one process. The proposal on the table is that "storage moves to tierstore, in its hot plus cold proxy mode on
flash, rather than staying a kbf process".

The short answer is that tierstore can replace the **object layer and the write tier** of the storage half, and
cannot replace the storage half. What `kbf storage` serves is not an object store: it is a content-addressed store
with an index, an action cache with a closure check, and a replicated placement map (section 2). Those have no S3
shape. What tierstore offers (section 3) matches the two lowest layers of `docs/design/storage.md`: "bytes" (the hot
tier and its flush) and "objects" (the cold bucket). So the proposal is read here as:

> The storage node stays a kbf process and keeps the index, the action cache, the closure check, the digest checks,
> the map and the chain. Its `--cold-store` becomes a tierstore endpoint, and, once evidence allows, its `--hot-store`
> (the kbf write tier) is switched off, because tierstore's hot tier does that job.

A second path, packs, already stores blobs in an S3 bucket without the storage nodes (section 2.5). It is where
the work moves in the longer term, and it is listed as an option, not as part of this migration.

## 2. Facts: what the storage half does today

### 2.1 Three layers, one process

- **Metadata.** `kbf_meta::MetaState` is a state machine over the CAS index (digest to segment object and offset), the
  action cache (action digest to result digest and closure), unreachable-object marks and farm time. It is applied
  from a single-voter Raft log on the node's local disk (`--meta-dir`, or `<hot store>/meta`). [code:
  `crates/kbf-meta`, `crates/kbf-server/src/raft_log`, `crates/kbf-server/src/stores.rs` module docs]
- **Bytes.** Uploads are verified against their digest (`VerifiedBlob`), packed into one open segment of at most
  128 MiB, and written. A segment is the blobs back to back, then a footer carrying each blob's hash, offset and
  CRC-32C. [code: `crates/kbf-front/src/cache/upload.rs`, `write_group.rs`, `crates/kbf-segments`; limits
  `MAX_SEGMENT_BYTES`]
- **Objects.** Segments are objects in the cold store through the `kbf_objstore::ObjectStore` trait: `put_new`,
  `get_range`, `may_hold`, `delete`, `list`. [code: `crates/kbf-objstore/src/lib.rs`]

### 2.2 The hot store and the cold store

- `--cold-store` is required: `file:///dir` (`FsStore`) or `s3://bucket[/prefix]?endpoint=http://host:port`
  (`S3Store`; query keys `endpoint`, `region`, `conditional-put`, `min-put-rate`). The key pair is either the standard
  `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`, or a key file named with `--cold-store-credentials-file`, a flag that
  is on main since kbf#2177 (a node with its own endpoint needs its own keys, 5.7). [code: `crates/kbf-server/src/stores.rs`
  `StoreUri`, `S3Uri`; `crates/kbf-cli/src/config.rs`, `crates/kbf-server/src/key_file`]
- `--hot-store` is optional: the write tier (`kbf-tier`). A segment is staged as a block file in the hot directory,
  synced with `fdatasync`, and acknowledged (on a chain, once both owners hold it); the flusher later PUTs it to the
  cold store as a multipart upload, reads the footer back, and commits `ObjectFlushed`. Flushed blocks stay on the
  hot disk as a read cache. Without a hot store, uploads go straight to the cold store. [code:
  `crates/kbf-tier/src/lib.rs`, `crates/kbf-front/src/cache/tier.rs`, `cache/flush.rs`]
- The hot store's budget is a share (80%) of the file system's free space less a floor, at most
  `--hot-store-max-bytes`. [code: `stores.rs` `HOT_STORE_SHARE_PERCENT`]
- **Key layout.** Segment keys are `<prefix><log id>/cas/<epoch>/<seq>` for a node that names its objects by a
  writer epoch of its log, and `<root>cas/<member>/<boot>/<seq>` (no log id; `<root>` is the prefix without the log
  id) for a node registered with a frontend, which is the farm shape; member, boot and sequence are 16 hex digits.
  Both layouts are written by `ObjectKeys::key` and read by `ObjectKeys::object`; the daemon `Segments` path allocates
  keys through the same `ObjectKeys`. [code: `crates/kbf-front/src/cache/write_group.rs` `ObjectKeys::key`,
  `ObjectKeys::object`, `Writer::allocate`, `object_key`; `crates/kbf-server/src/storage_node.rs` module docs "Object
  names"] A prefix of the form `<name>/<log id>/...` therefore describes the unregistered naming only. Pack keys
  (`cas/d|f|b|l/...`) are a separate family [code: `crates/kbf-pack/src/keys.rs`]. What the production bucket actually
  holds is not in the code: it needs a listing (decision 4).
- **Both owners write cold today.** `Plan::Primary` stores a blob locally and at the replica
  (`crates/kbf-storage/src/replicate/chain.rs`), and each owner seals its own segment and calls `put_new(key, body,
  None)` under its own key (`write_group.rs`). `OneFlusher` exists (`crates/kbf-storage/src/flush/mod.rs`), but a
  registered node is given `FlushesAll`, true for every digest, and the code says `OneFlusher` is not installed [code:
  `crates/kbf-server/src/storage_node.rs` (the flush gate chosen near line 717), `crates/kbf-server/src/fill.rs`]. So
  the cold store already receives each blob from both owners. `docs/design/architecture-v2.md` section 8 ("One
  flusher") describes the intended rule, not the running one.
- **The cold store is listed when a tail pass is owed, not continuously.** A standing tail (`Cache::tail_pass`) lists
  `<root>cas/`, reads the footer of each object it has not handled, and adopts only digests the index lacks; it never
  reads an object of its own member, and skips the old `<root><log id>/cas/` prefix. There is **no cursor**: every pass
  lists the whole prefix, because uploads finish out of sequence. [code: `crates/kbf-front/src/cache/tail.rs`]
  `fill::run` ticks, but `fill::round` makes a pass only when `FillState::owed()` is set, that is when the epoch the
  frontend's heartbeat answer carried (`learn`) is above the epoch the node acknowledged (`tail_epoch_done`). An owed
  epoch is served by `settle` in attempts: each attempt makes at least two passes (the second is the guard) and at most
  `PASSES` = 8, stopping at the first pass after the first that is quiet. An attempt that never goes quiet within 8, or
  whose placement fingerprint moved, acknowledges nothing, and the next tick starts a new attempt, so under churn the
  cost of one owed epoch has no upper bound. [code: `crates/kbf-server/src/fill.rs` `owed`, `round`, `settle`, `PASSES`,
  `registered`] With no epoch bump and no restart there is **no listing at all**.
- **When an epoch is bumped.** The frontend's counter `meta/tail_epoch` is written only by a frontend started with the
  handoff setting (`Members::with_handoff`), and then (a) when a joining member's flush barrier clears, and again if the
  placement fingerprint or a write owner's boot changes while it joins [code: `crates/kbf-server/src/members/fence.rs`
  `advance_one`, `fence_step`]. The design also bumps it when a draining member has flushed (`A to B` in
  `docs/design/rebalance-handoff.md`), but that is not implemented at main: `advance` scans only the marks of joining
  members, and no drain mark or drain fence exists in kbf-server. So only joins bump the epoch, and in steady state the
  cadence is zero. A node's own **restart or registration** also owes a pass:
  `FillState::registered` sets the acknowledged epoch to 0, and the learned epoch is above 0 once the farm has ever
  bumped. A restarted tail also starts empty (`TailState` is volatile), so it reads the footer of every object that is not
  its own member's once. [code: `fill.rs` `registered`; `tail.rs` `TailState`] Which frontends in the running farm are
  started with the handoff setting is a deployment fact not read here: UNVERIFIED.
- **No retention on the segment path.** Segments are written with `put_new(..., None)` and `put_stream`, neither with
  a retention date; the only pass-throughs of `retain_until` are the history store's, and its writers pass `None`. A
  store without Object Lock must refuse a date (`FsStore` does). [code: `write_group.rs`, `cache/direct.rs`,
  `cache/flush.rs`, `crates/kbf-history/src/scoped.rs`, `writer.rs`, `crates/kbf-objstore/src/fs_store.rs`] This is a
  search result, not a proof of zero callers.
- **Cold-store identity.** A `file://` cold store is `Fixed` and reports `identity_mismatch` if the frontend announces
  a registration. An `s3://` store, tierstore included, is `Registrable`: the node fetches the registered config and
  requires `same_store` (endpoint, bucket and prefix equal). On a mismatch it keeps its flag store and reports
  `identity_mismatch`, or errors if it has no key pair; "the registration supplies keys, never a different store".
  [code: `crates/kbf-server/src/cold_apply.rs`, `storage_node.rs`, `storage_half.rs`]
- `S3Store` is `objstore_s3` re-exported: the same client crate that tierstore uses for its own S3 tiers. [code:
  `crates/kbf-objstore/src/lib.rs` `pub use objstore_s3`; tierstore `crates/objstore-s3/src/lib.rs`]

### 2.3 Eviction, garbage collection, integrity

- **Eviction** exists for a `file://` cold store only: least recently used objects are forgotten with
  `EvictObjects` and their files deleted, when free space is under twice the floor. Nothing evicts from an S3 cold
  store, because the bucket is the store of record. [code: `crates/kbf-front/src/cache/evict.rs`]
- **Garbage collection is not running.** `Cache::collect` commits `Command::Collect`; it has no caller outside tests,
  and it removes index and action entries only. No code deletes a segment object from an S3 bucket.
  `docs/design/storage.md` lists collection, condemn delay, compaction and orphan sweeps under "Planned", which
  agrees. [code: `crates/kbf-front/src/cache.rs` `collect`; a search of `kbf-server`, `kbf-storage`,
  `kbf-storage-and-daemon` and `kbf-front` finds no other `Command::Collect` committer]
- **Digest checks.** On write, every blob is hashed against the digest the client sent before it is packed
  (`VerifiedBlob`). On read, `Cache::read_blob` fetches the blob's exact range and checks its SHA-256; a failure marks
  the object `Corrupt` and the read fails UNAVAILABLE, never serving the bytes; a missing object or range marks it
  `Missing`. The flusher re-hashes every record of a staged block before it writes the footer (with a tier, the daemon's digests are trusted at acknowledgement; with none, `PutRange` hashes on receipt and `Commit` refuses an unhashed record). The segment reader
  checks length, CRC-32C and SHA-256. Three answers (`Present`, `Unavailable`, `Absent`) keep "could not read" from
  becoming "does not exist". [code: `crates/kbf-front/src/cache/read.rs`, `cache/flush.rs`, `crates/kbf-segments`,
  `docs/design/storage.md` "Three answers, not two"]
- **Rebuild.** A new owner lists the store in key order and adopts segments from their footers (`AdoptSegment`,
  `RebuildProgress`, `RebuildDone`). It needs `list` with a start-after cursor; it runs for a registered node whose log
  has no `RebuildDone`, and resumes from its progress record. [code:
  `crates/kbf-front/src/cache/rebuild.rs`, `crates/kbf-server/src/rebuild.rs`]

### 2.4 Who calls the storage half, and over what

| Caller | Calls | Protocol | Code |
|---|---|---|---|
| Frontend (clients' REAPI traffic, Execute's input check) | `FindMissingBlobs`, `ByteStream.Read` and `Write`, `kbf.storage.v1.ActionStore` `Get`, `Prepare`, `Commit` | gRPC over mutual TLS; the certificate's role (`urn:kbf:role:<name>`) is checked per endpoint; every call carries the map generation, `STALE_MAP` on a stale one | `crates/kbf-storage/src/service/mod.rs`, `admission.rs`; `crates/kbf-peer` |
| Daemon (input fetch, output upload) | `ByteStream.Read`, `FindMissingBlobs` to the owners the map names; in-process calls (`InProcess`) on `storage-and-daemon`. The storage service serves `kbf.worker.v1.Segments` (`Begin`, `PutRange`, `Commit`), but the routed client has no segment upload yet, so a remote daemon's outputs do not use it today | the same gRPC; in process with no socket on the combined node, which uses the cache's own path | `crates/kbf-transfer/src/routed.rs`, `crates/kbf-route`, `crates/kbf-storage/src/service/in_process.rs`, `segments.rs` |
| Peer storage nodes | chain replication of a write to the second owner; read repair; repair journal | gRPC over mutual TLS | `crates/kbf-storage/src/replicate`, `repair` |
| Frontend (registration) | map, membership, deny list, cold-store registration (`ColdStore/GetStoreConfig`) | gRPC on the worker listener | `docs/design/architecture-v2.md` section 6 |
| Tasks (job-read path) | `GET /locate`, reads of large blobs under `cas/l/<hex>` in tierstore with a signed read grant | HTTP, S3 GET with `x-tierstore-grant` | `crates/kbf-task/src/locate.rs`, `crates/kbf-front/src/job_read/locate.rs`, `large_backfill.rs` |

Placement is rendezvous hashing over the digest and the members, the top R (two) being the owners; the primary
forwards a write to the replica and acknowledges when both hold it on the hot tier; both owners flush (section 2.2).
[code: `crates/kbf-storage/src/replicate/chain.rs`, `crates/kbf-server/src/fill.rs`] No client other than another storage node ever reads the cold store directly, except the packs
below.

### 2.5 The pack path already uses an S3 bucket and tierstore

With `--pack-index` and the `pack-store-*` flags (off by default), the frontend writes uploads into packs (the
segment layout) in an S3-compatible store with create-only writes, records each in a pack index in the state store
before it acknowledges, and reads a blob with one ranged request, hashing it. A daemon may do the same (`--pack-write`,
`--pack-read`). The closure check of an action-cache entry counts a blob as held when the index has it. The action
cache entries themselves are still written to the storage nodes. `kbf-pack-adopt` indexes the segments the storage
nodes flushed, in place, without copying or deleting anything. Pack keys are `cas/d/<node>/<boot>/<seq>` and
`cas/f/<frontend>/<boot>/<seq>`. The frontend also copies large blobs to tierstore at `cas/l/<hex>`. [code:
`docs/deploy/kbf-modes.md` "Packs"; `docs/deploy/pack-adopt.md`; `crates/kbf-pack/src/lib.rs`;
`crates/kbf-front/src/large_backfill.rs`]

### 2.6 What is measured today

- The server exports `kbf_tier_call_seconds` and `kbf_tier_wait_seconds`, which time **write-tier disk calls**, not
  object-store requests. There is no counter of cold-store GETs and no latency histogram for a daemon's input fetch.
  [code: `crates/kbf-front/src/metrics.rs` histogram list; a search of `kbf-transfer` and `kbf-daemon` finds no
  latency histogram]

## 3. Facts: what tierstore offers

- One process: an S3 front (one bucket, path-style, SigV4 in the header, plain HTTP), a proxy over a hot and a cold
  tier, and a reconciler. A tier is `memory`, `dir:<path>` or `s3:<bucket>`. [code: tierstore
  `crates/tierstore/docs/overview.md`]
- **Write.** A put goes to the hot tier and is acknowledged when its journal entry is durable there (journal linger
  20 ms by default). The reconciler copies it to the cold tier later. When the hot tier is full the put goes straight to
  the cold tier. A put is create-only: an existing key in either tier is 412. [code: `overview.md`, `operations.md`
  "Eviction", `configuration.md` `hot_store.journal_linger`]
- **Read.** Hot, else cold; a cold hit is copied back into the hot tier (promotion) unless the hot tier is over its
  critical mark. The cold read fetches **the whole object, whatever range was asked**; the promotion copies the whole
  object. [code: `metrics.md` `tierstore_bytes_read_total`, `tierstore_promoted_bytes_total`]
- **Eviction.** Above the high water mark (80%) the reconciler evicts the oldest objects the cold tier already holds,
  down to the low water mark (70%); it never evicts an uncopied object. [code: `operations.md`]
- **Reconciler and lease.** One reconciler works at a time, under a lease in the hot tier (`lease_ttl` 30 s,
  `reconcile_interval` 5 s); any number of processes may run over the same tiers, each with its own `--proxy-id`; a
  lock file (`hot_store.lock_file`) decides which process on one node reconciles. State is in the tiers, so a restart
  needs only the same proxy id. [code: `operations.md` "Drain and restart", `configuration.md`]
- **Durability.** Losing the hot tier loses objects not yet copied; the copy lag is published in the hot tier and is
  **not** exported on `/metricz`. Nothing scrubs a storage node's objects. [code: `limits.md`]
- **Keys.** Full, read-only, presigning, GC, scoped (read, write and delete prefix lists) and grant keys. The GC key
  may list, head, delete and abort uploads under one prefix and never reads a body. `_journal/` and `_tiering/` belong
  to tierstore and are hidden from listings. [code: `keys.md`]
- **Start-up self-test.** Create-only put refuses an existing key, ranged read returns the range, a listing honours
  `start-after`, a multipart completion is create-only; a wrong answer exits with status 3. [code: `operations.md`]
- **Limits.** `--max-request-bytes` 256 MiB by default per put or part; CRC-64/NVMe and `aws-chunked` bodies are refused
  501; no TLS on the S3 listener; no Object Lock is documented. [code: `limits.md`]
- **Metrics.** Per-tier `hits`, `misses`, `read_errors`, `reads`, `bytes_served`, `bytes_read`, promotions and skipped
  promotions on `/metricz`; no latency histogram is listed. [code: `metrics.md`]
- **Updates.** `tierstore update-hook`, two instances on private ports behind one redirect, the second serving read-only
  until the first releases the lock. [code: `docs/design/tierstore-handover.md` in this repository]
- **Bench.** `tierstore-bench` has cells for put-to-ack, cold read, copy lag and tree read, a fault proxy, and a result
  JSON. [code: `ARCHITECTURE.md` row for `tierstore-bench`]

## 4. Mapping: each storage function

| Storage function today | After | Notes |
|---|---|---|
| Cold store (`S3Store` to the bucket) | **tierstore** | `--cold-store s3://<bucket>/<prefix>?endpoint=<tierstore>` [proposed]. Same client crate on both sides [code]. An `s3://` store is `Registrable`; a different endpoint is a permanent `identity_mismatch` (section 2.2, 5.7). |
| Write tier: staged blocks, `fdatasync` ack | **tierstore hot tier** (shape B) or stacked above it (shape A) | Section 5. |
| Flusher: seal, multipart PUT, footer read-back, `ObjectFlushed` | **tierstore reconciler** copies hot to cold; kbf keeps the footer read-back | With no write tier, kbf writes a segment with `put_new` or `put_stream` and reads the footer back [code: `docs/design/storage.md` Writes step 4]. `ObjectFlushed` and the staged table are unused on that node. Both owners already flush today (section 2.2), so this does not change who writes cold. |
| Re-hash of every record at flush | **moves to receive time** | With no tier, `PutRange` hashes SHA-256 and CRC-32C on receipt and `Commit` refuses an unhashed record; a segment the tier did not take is hashed before the PUT. Stronger than the flush-time check, which is there because with a tier the daemon's digests are trusted at acknowledgement. Cost: ingress CPU, unmeasured. [code: `crates/kbf-front/src/segments.rs`, `cache/upload.rs`, `cache/flush.rs`] |
| Flushed-copy read cache on the hot disk | **tierstore hot tier** (promotion) | Whole-object promotion; amplification, 5.4. |
| Hot-store eviction | **tierstore water marks** | `file://` eviction in `evict.rs` is unrelated and stays. |
| Rebuild by footers and the standing tail | stays in kbf, listing whatever `--cold-store` is | Both list the endpoint, under `keys.root` (rebuild, once, with a cursor) and `<root>cas/` (tail, with no cursor, 2 to 8 listings per attempt for each epoch owed, an unsettled attempt repeated). Cost is owed epochs times passes per epoch times objects in the prefix; 5.8. |
| `may_hold`, `get_range` exactness | stays in kbf; tierstore must satisfy | `S3Store` refuses a ranged read not answered with the exact range. |
| CAS index, action cache, closure check, `Touch`, retention, three answers | **stays in kbf** | No S3 shape. |
| Raft metadata log on the hot disk | **stays in kbf** | Lives in `<hot store>/meta` when the hot store is a directory; with no hot store it needs `--meta-dir` [code: `stores.rs`]. |
| Map, HRW placement, chain replication, repair, handoff, read repair | **stays in kbf** | Node-level replication, which tierstore does not do. |
| Peers and mutual TLS roles, deny list | **stays in kbf** | tierstore has no mTLS; node to local tierstore is loopback with a key. |
| Collection of dead segments | **new, in kbf, through tierstore's GC key** | Nothing exists today (section 2.3). |
| Cold-store registration | **stays; a node-local endpoint opts out of it** | 5.7. |
| Object Lock retention | **not needed** | The segment path passes no date (section 2.2). |
| Packs, `cas/l/`, task grants | **same tierstore, other prefixes** | No change to their code. |

## 5. Proposal

### 5.1 Two shapes

- **Shape A: tierstore as the cold store, kbf hot store unchanged.** Per node, `tierstore` runs beside `kbf` with a
  flash `dir:` hot tier and the MinIO bucket as its cold tier; the node's `--cold-store` endpoint is the local tierstore.
  The bucket and prefix are unchanged, so every existing segment stays readable under the same key and nothing moves.
  A segment is written twice on flash (kbf block, then tierstore journal and object) and copied to cold once per owner.
  [proposed]
- **Shape B: tierstore as the whole tier.** As A, with `--hot-store` unset on that node: the write tier, flusher and
  staged table are not used, and a segment is written once to flash by tierstore. [proposed]

Recommendation: A for a short soak on the canary node only, to learn whether tierstore is a correct and fast
`ObjectStore` under the existing reads, listings and flushes; then B on that node, and B directly on the rest. Shape B is
not worse than today on the two points that first looked like blockers (5.2, 5.3), so the case for staying on A is the
double flash write, which is a cost, not a risk. A is reversible by one flag; B changes what an acknowledgement means
and is reversible by one flag too (section 6).

### 5.2 Cold bytes and durability

Cold bytes are already doubled: both owners PUT their own segment, with the write tier on (section 2.2). Shape B does not
add to that, so the baseline it is compared with doubles too. Pre-copy durability under B: tierstore's acknowledgement is
one flash journal, and losing that disk before the copy loses the object [code: tierstore `limits.md`]; the chain still
sends the blob to the replica, and each owner writes it through its own tierstore, so two nodes hold it before either
copy. What the chain's acknowledgement waits for with no write tier (each owner's segment PUT acknowledged by its
tierstore, or only the primary's) is not read from the code here and is the one durability point that still needs the
storage owner's confirmation (question A below). Latency: with a write tier the acknowledgement is an `fdatasync`; without
one it is the write group's linger plus a PUT plus tierstore's journal linger (20 ms by default); put-to-ack is the
measurement that decides whether that is acceptable (section 7). [proposed]

### 5.3 Integrity stays in kbf, and the daemon upload check gets stronger under B

tierstore checks the checksums a client sends on a write and does not hash a storage node's objects on a read or in the
background. So the end-to-end proof is kbf's: ingress `VerifiedBlob`, and the SHA-256 of the exact range on every read,
with `Corrupt` and `Missing` marks. No check is removed. The flusher's re-hash of staged records exists because, with a
tier, a daemon's digests are trusted when the upload is acknowledged; with no tier, `PutRange` hashes on receipt and
`Commit` refuses an unhashed record, so a bad digest is refused at once instead of becoming a loss mark at flush time
(section 4). The price is hashing on the ingress path (`--cpu-threads`), to be measured (section 7). The remote daemon's
`Segments` path is not used for outputs yet, so the check that moves today is the in-process and cache-upload path. [code:
`crates/kbf-front/src/segments.rs`, `crates/kbf-transfer/src/routed.rs`]

### 5.4 Range reads against whole-object promotion

kbf reads one blob with a ranged GET of a segment of up to 128 MiB. A tierstore cold read fetches and promotes the whole
object. A warm farm is unaffected; a cold read costs up to a segment per blob, which improves the next reads from the
same segment and costs hot-tier space and cold bandwidth. Whether that is a win depends on how blobs of one action's
inputs sit in segments, which only a measurement can say (section 7). [proposed]

### 5.5 Garbage collection

[proposed] A collector in kbf. Collection is per node by structure: `Cache::collect` runs in one process against that
process's single-voter metadata log, and no document states a farm-wide collector. When every blob of a segment has been
collected (the `Collect` result returns each removed blob's location, so dead space is known [code:
`docs/design/storage.md` Retention]), the node deletes the object through a tierstore **GC key** whose prefix is that
node's own writer prefix (`<root>cas/<member>/`; a GC key has exactly one prefix). The GC key lists, heads and deletes and
never reads a body. A delete in tierstore removes both tiers' copies. The trap is the standing tail: it adopts other
members' objects into a node's index, so an owner that deletes a segment another node adopted leaves that node's
locations dangling, to be marked `Missing`. The trap is narrower than it first looks: a pass never reads an object of its
own member, and adopts only digests the index does not already hold (section 2.2), so a segment is adopted by another
node only for blobs that node lacked, which with both owners writing every blob is the case after a join, a wiped log or a
lost write. There is **no tail interval** to wait out: passes happen when an epoch is owed (2.2), at a cadence of joins
plus restarts (drains do not bump the epoch at main, 2.2). So a delay-based collector must cover that cadence plus a
settle that acknowledges (at least two listings, with no upper bound under churn, and the footer reads), and cannot be a fixed small constant; otherwise the collector must learn of adoptions (open item E).
This is new work with no code today; it does not gate the
migration, but it gates the cold bucket staying bounded, and needs a design of its own.

### 5.6 Credentials

[proposed] The node's key to its local tierstore is a **scoped key** (read and write under the farm prefix, no delete
prefix), given with `--cold-store-credentials-file` (on main since kbf#2177); the collector holds a
GC key; the cold tier's credential is tierstore's own (`--cold-credentials-file`) and leaves kbf. This narrows what a
storage node holds compared with a bucket key. The "credential gap on `storage-and-daemon`" in
`docs/design/architecture-v2.md` section 9 is unchanged: the key must still be unreadable by the actions' uid.

### 5.7 How a node is pointed at tierstore (registration)

The farm-wide registered cold-store identity names the bucket endpoint. A node whose `--cold-store` is an `s3://` URI
fetches that registration and requires `same_store` (endpoint, bucket and prefix equal). A node-local tierstore endpoint
differs, so the node keeps its flag store, reports `identity_mismatch` for as long as it runs that way, and needs its own
keys. Consequences [proposed]: (a) a migrated node is permanently reported as mismatched; whether the frontend, the
operator API or an alert treats that as a fault has not been read and must be checked before the canary; (b) the
registration can never be used to roll a store change across the farm, so the flag is the only switch, which is the
rollback path anyway; (c) the alternative is one endpoint string for the whole farm that each node resolves to its own
tierstore, which makes identities equal and hides the difference from the registration; it is not recommended, because the
registration then vouches for a store it does not describe. Recommended: accept the mismatch on migrated nodes, and
register the tierstore identity only if tierstore later runs as one shared endpoint.

### 5.8 The standing tail and the list cost

The tail is owed, not scheduled (section 2.2). Its listing cost is **owed epochs x passes per epoch (summed over its attempts) x objects in the
prefix**, plus one footer read for every object that is not the node's own on the first pass after a restart. Passes are
2 to 8 per attempt (`settle`), each a full listing with no cursor, and an attempt that does not settle is repeated, so
churn during an epoch multiplies the cost. In steady state, with no join or restart, the cost is zero. Nothing deletes segments (section 2.3), so objects in the prefix only grow, and each owed epoch costs more
than the last. The unit to record at baseline is therefore listings per owed epoch and their wall time at the current
object count, not a rate. Behind tierstore each listing is a tierstore list; its cost, and whether it covers objects that
are only in the hot tier, are tierstore's to state (open item F). The consequence for correctness is in section 6: if the
listing misses uncopied objects, a node can acknowledge an epoch without them in its index. How often the frontend bumps
the epoch in the running farm is UNVERIFIED (it needs the frontend's handoff setting and its join history); the
code gives only the triggers. [proposed]

## 6. Migration, one node at a time

Preconditions: tierstore's v1 paper has shipped; the conformance, listing and baseline of steps 0 and 1 are recorded; the
`identity_mismatch` question of 5.7 is answered.

0. **Conformance, no farm change.** Run `kbf_objstore::conformance::run` (the `s3_conformance` example) against a
   tierstore with a `dir:` hot tier and an `s3:` cold tier. It must pass the base contract and the `conditional_put`
   capability, including ranged reads at the boundaries and list paging. In addition, put an object only in the hot tier
   and list: record whether the listing shows it. A failure is tierstore's to fix or kbf's to relax, decided before
   step 2. [proposed; code: `crates/kbf-objstore/src/conformance.rs`]
1. **Baseline on today's farm.** List the production bucket (the only way to know which key families it holds: node
   layout, epoch layout, packs), and record input-fetch latency, cold GETs per build, cold PUT bytes per uploaded byte,
   and the listings per owed tail epoch with their wall time (section 7). kbf has no cold GET counter today, so the baseline comes from the bucket's own
   request counts plus a client-side timer. [proposed]
2. **Canary node, shape A.** Install tierstore on the node. Point the node's `--cold-store` at it, with its own key file,
   bucket and prefix unchanged. Keep `--hot-store`. Soak: no new `ObjectUnreachable` marks, flush success unchanged, no
   tierstore `read_errors`, a restart of the node completing its owed tail pass, copy lag bounded.
3. **Canary node, shape B.** Unset `--hot-store`, set `--meta-dir`, restart. Same watch, plus put-to-ack and ingress CPU
   against the baseline.
4. **The other nodes, one at a time, shape B**, each after the previous node has run a full build set clean.
5. **Collector** (section 5.5) lands independently.

**Rollback, per node.** To the old state: stop the node's kbf, keep its tierstore running until its copy lag is zero,
point `--cold-store` back at the bucket, start kbf. The copy lag is the trap: an object tierstore acknowledged and has
not yet copied is absent from the bucket, and a kbf pointed at the bucket would read `Missing`, mark the object
unreachable in its Raft log, and fail the read UNAVAILABLE until the blob is uploaded again. Tierstore's drain gives up
the lease and exits; it does not promise a finished copy. [code: tierstore `operations.md` "Drain and restart"; kbf marks
`Missing` per `docs/design/storage.md` "Reads"] So rollback needs the copy lag, which `/metricz` does not export today.
The tail adds a second effect: pointed at the bucket, a node's passes see only copied objects (see "Mixed farm"). Shape B
back to A: set `--hot-store` again; the staged table starts empty and no staged block exists to lose.

**Mixed farm.** Each node's index is its own Raft log and other nodes read a node's blobs through the owner, not through
the bucket, so a farm with some nodes on tierstore and some on the bucket serves correctly. Nodes read each other's
objects from the cold store in the rebuild and in the tail pass, and the **handoff fence relies on those passes**: a
join (and, in the design, a drain) is confirmed only when every member has acknowledged the epoch, and a node acknowledges after a pass that
began after it learned the epoch (`docs/design/rebalance-handoff.md`; `fill::round`). A pass lists whatever that node's
`--cold-store` is. If the listing cannot see an uncopied object (a node listing the bucket while another node's tierstore
still holds the object only on flash, or a tierstore listing that is cold-only), the pass completes, the epoch is
acknowledged, and the fence is confirmed **without that object in the index**: the fence then covers less than it says.
Pointing rebuild or tail at tierstore instead of the bucket changes what the fence covers in the same way. So the rule
stands, with its reason: **do not start a join, which bumps the epoch, while any tierstore in the farm has copy lag**, and
the same for a drain once drains bump the epoch as the design describes (at main they do not, 2.2), and do not rely on a restart's owed pass for an object still on flash. Between epochs nothing is lost: objects
become visible when copied, and the next owed pass adopts them. [proposed]

## 7. Measurements this enables

For the tierstore paper and for the go/no-go gates:

| Measurement | Source | Today |
|---|---|---|
| Cold GETs per build | delta of `tierstore_reads_total{tier="cold"}` (and `tierstore_misses_total{tier="hot"}`) over a build, divided by builds; cross-check with the bucket's own request count | not measurable in kbf (section 2.6) |
| Cold bytes read per blob byte served | `tierstore_bytes_read_total{tier="cold"}` over `tierstore_bytes_served_total`; shows the range amplification of 5.4 | n/a |
| Hot hit rate | hits over reads, per tier, per node | n/a |
| Input fetch p50 and p99 | **client side**: a histogram in `kbf-transfer` per blob or batch read (new), since neither kbf nor tierstore exports a latency histogram; `tierstore-bench` cold-read and tree-read cells for the isolated number | not measured |
| Put-to-ack and copy lag | `tierstore-bench` cells; copy lag needs a gauge (section 8, item F) | the flush is timed only as write-tier calls |
| Cold PUT bytes per uploaded byte | bucket bytes written over bytes uploaded; today about two (both owners flush), and B must not raise it | derived, not measured |
| Ingress CPU under B | `--cpu-threads` use during uploads, before and after | n/a |
| Tail listing cost | listings per owed epoch (2 to 8), their wall time and the footer reads of a restart, at the current object count, at baseline and behind tierstore | n/a |

Method: the same build set against the same commit, at least a cold run and a warm run per node shape, recorded with the
bench's result JSON. [proposed]

## 8. What is still open

Answered by the storage owner's review and folded into the facts above: the key layouts (2.2), that both owners flush
(2.2), the move of the digest check to receive time (4, 5.3), that `Fixed` is `file://` only and a tierstore store is
`Registrable` (2.2, 5.7), that the segment path passes no retention (2.2), what rebuild and the tail list (2.2, 5.8), and
that collection is per node by structure (5.5).

Still to settle:

- **A. Chain acknowledgement without a write tier.** Does the primary's acknowledgement wait for the replica's segment
  PUT, or only for the replica holding the blob in its cache? (5.2)
- **B. Ingress CPU under B.** Unmeasured (section 7).
- **C. `identity_mismatch` handling.** Whether the frontend or operator API treats a mismatched node specially. Not read.
- **D. Retention callers.** The search for callers of `retain_until` is a grep result, not a proof of none.
- **E. Collector and adoption.** How a per-node collector learns that another node adopted its segment (5.5).
- **F. To confirm in tierstore, not kbf** (to be raised there): the copy lag on `/metricz` (rollback gate); a latency
  histogram per operation; whether a listing through the front shows objects that are only in the hot tier; the cost of
  a listing on a large prefix (the tail lists the whole prefix, 2 to 8 times per owed epoch); how the GC key behaves
  (listing, head and delete under its one prefix, and a delete of an object in both tiers); how drain interacts with
  an unfinished copy; `start-after` paging in the front; and that `_journal/` and `_tiering/` stay hidden from
  listings.

## 9. Decisions, with a recommendation each

1. **What "storage moves to tierstore" means.** Recommend: tierstore replaces the object layer and the write tier of the
   storage node; the storage node stays a kbf process. Reject replacing the node outright: the index, the action cache,
   the closure check and the map have no equivalent in an S3 server.
2. **Shapes and order.** Recommend: a short shape A soak on the canary node, then shape B on it, then shape B on the
   other nodes one at a time. B is not worse than today on cold bytes (already doubled) or on the daemon upload check
   (stronger); it stays gated on put-to-ack and ingress CPU against the baseline.
3. **When.** Recommend steps 0 and 1 (conformance, listing, baseline) now, step 2 after tierstore's v1 paper ships, as
   proposed.
4. **Key layout.** Recommend no change: same bucket, same prefix, so no data moves and rollback has nothing to rewrite.
   First list the production bucket to learn which key families it holds (node layout, epoch layout, packs); do not
   assume `<name>/<log id>/...`.
5. **Registration.** Recommend: a migrated node runs on its flag with its own keys and a known `identity_mismatch`;
   confirm first (open item C) that this is not treated as a fault. Do not use a shared endpoint string as a workaround.
6. **Garbage collection.** Recommend a separate design: per-node collector over a per-node GC key, with a delay that covers the epoch cadence plus one settle, or an
   adoption record for the tail. Not a gate for the canary; a gate for any claim that the cold store stays
   bounded.
7. **Integrity.** Recommend no check is removed; kbf's ingress and read-time digest checks stay the proof, because
   tierstore does not hash a storage node's objects.
8. **Credentials.** Recommend a scoped key per node for the local tierstore via `--cold-store-credentials-file`, a
   per-node GC key for the collector only, and the cold credential held by tierstore alone.
9. **Packs.** Recommend pointing the pack store at the same tierstore under its own prefixes, and treating the pack path,
   not the storage node, as the long-term home for blob bytes; that migration (action cache entries, closure check) is a
   separate design.
10. **Range amplification.** Recommend measuring first (section 7); do not change the segment size or ask tierstore for
    ranged promotion before the number exists.
11. **Rollback and listing gates.** Recommend tierstore exports the copy lag and states what its listing covers before
    the canary; without the first, rollback of a node depends on a manual listing; without the second, the tail, the rebuild and the handoff fence
    cover different objects than the bucket does, in a way no one has looked at.
