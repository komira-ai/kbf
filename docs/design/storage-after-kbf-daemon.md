# Storage after `kbf storage-and-daemon`: what tierstore replaces

Status: DESIGN, for the maintainers and the kbf storage owner. Nothing here is built. Co-owned: the questions in
section 8 are the storage owner's to answer before any step of section 6 starts.

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
  (`S3Store`; query keys `endpoint`, `region`, `conditional-put`, `min-put-rate`; the key pair is the standard
  `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`). [code: `crates/kbf-server/src/stores.rs` `StoreUri`, `S3Uri`]
- `--hot-store` is optional: the write tier (`kbf-tier`). A segment is staged as a block file in the hot directory,
  synced with `fdatasync`, and acknowledged (on a chain, once both owners hold it); the flusher later PUTs it to the
  cold store as a multipart upload, reads the footer back, and commits `ObjectFlushed`. Flushed blocks stay on the
  hot disk as a read cache. Without a hot store, uploads go straight to the cold store. [code:
  `crates/kbf-tier/src/lib.rs`, `crates/kbf-front/src/cache/tier.rs`, `cache/flush.rs`]
- The hot store's budget is a share (80%) of the file system's free space less a floor, at most
  `--hot-store-max-bytes`. [code: `stores.rs` `HOT_STORE_SHARE_PERCENT`]
- **Key layout.** Segment keys are `<prefix><log id>/cas/<epoch>/<seq>` for a node that names its objects by a
  writer epoch of its log, and `<root>cas/<member>/<boot>/<seq>` (no log id; `<root>` is the prefix without the log
  id) for a node registered with a frontend, which is the farm shape. Both layouts are read. [code:
  `crates/kbf-server/src/storage_node.rs` module docs "Object names"; `docs/design/storage.md` "Writes"] A prefix of
  the form `<name>/<log id>/...` therefore describes the unregistered naming; see question Q1.
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
  `Missing`. The flusher re-hashes every record of a staged block before it writes the footer. The segment reader
  checks length, CRC-32C and SHA-256. Three answers (`Present`, `Unavailable`, `Absent`) keep "could not read" from
  becoming "does not exist". [code: `crates/kbf-front/src/cache/read.rs`, `cache/flush.rs`, `crates/kbf-segments`,
  `docs/design/storage.md` "Three answers, not two"]
- **Rebuild.** A new owner lists the store in key order and adopts segments from their footers (`AdoptSegment`,
  `RebuildProgress`, `RebuildDone`). It needs `list` with a start-after cursor. [code:
  `crates/kbf-front/src/cache/rebuild.rs`, `crates/kbf-server/src/rebuild.rs`]

### 2.4 Who calls the storage half, and over what

| Caller | Calls | Protocol | Code |
|---|---|---|---|
| Frontend (clients' REAPI traffic, Execute's input check) | `FindMissingBlobs`, `ByteStream.Read` and `Write`, `kbf.storage.v1.ActionStore` `Get`, `Prepare`, `Commit` | gRPC over mutual TLS; the certificate's role (`urn:kbf:role:<name>`) is checked per endpoint; every call carries the map generation, `STALE_MAP` on a stale one | `crates/kbf-storage/src/service/mod.rs`, `admission.rs`; `crates/kbf-peer` |
| Daemon (input fetch, output upload) | `ByteStream.Read`, `FindMissingBlobs`, `kbf.worker.v1.Segments` `Begin`, `PutRange`, `Commit` to the owners the map names; in-process calls (`InProcess`) on `storage-and-daemon` | the same gRPC; in process with no socket on the combined node | `crates/kbf-transfer`, `crates/kbf-route`, `crates/kbf-storage/src/service/in_process.rs` |
| Peer storage nodes | chain replication of a write to the second owner; read repair; repair journal | gRPC over mutual TLS | `crates/kbf-storage/src/replicate`, `repair` |
| Frontend (registration) | map, membership, deny list, cold-store registration (`ColdStore/GetStoreConfig`) | gRPC on the worker listener | `docs/design/architecture-v2.md` section 6 |
| Tasks (job-read path) | `GET /locate`, reads of large blobs under `cas/l/<hex>` in tierstore with a signed read grant | HTTP, S3 GET with `x-tierstore-grant` | `crates/kbf-task/src/locate.rs`, `crates/kbf-front/src/job_read/locate.rs`, `large_backfill.rs` |

Placement is rendezvous hashing over the digest and the members, the top R (two) being the owners; the primary
forwards a write to the replica and acknowledges when both hold it on the hot tier; the rank-1 owner alone flushes
(`OneFlusher`). [code: `docs/design/architecture-v2.md` section 8 and `crates/kbf-storage/src/flush`,
`replicate/chain.rs`] No client other than another storage node ever reads the cold store directly, except the packs
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
| Cold store (`S3Store` to the bucket) | **tierstore** | `--cold-store s3://<bucket>/<prefix>?endpoint=<tierstore>` [proposed]. Same client crate on both sides [code]. |
| Write tier: staged blocks, `fdatasync` ack | **tierstore hot tier** (shape B) or stacked above it (shape A) | Section 5. Not a one-for-one swap: see durability, section 5.2. |
| Flusher: seal, multipart PUT, footer read-back, `ObjectFlushed` | **tierstore reconciler** copies hot to cold; kbf keeps the footer read-back | With no write tier, kbf writes a segment with `put_new` or `put_stream` and reads the footer back [code: `docs/design/storage.md` Writes step 4]. `ObjectFlushed` and the staged table become unused on that node. |
| Re-hash of every record at flush | **stays in kbf** | tierstore does not hash a storage node's objects. Without the tier the hash happens at ingress (`VerifiedBlob`) and at read; see section 5.3. |
| Flushed-copy read cache on the hot disk | **tierstore hot tier** (promotion) | Whole-object promotion; amplification, section 5.4. |
| Hot-store eviction | **tierstore water marks** | `file://` eviction in `evict.rs` is unrelated and stays. |
| Cold `list`, rebuild by footers | stays in kbf, served by tierstore's listing | Must pass the conformance suite, section 6, step 0. |
| `may_hold`, `get_range` exactness | stays in kbf; tierstore must satisfy | `S3Store` refuses a ranged read not answered with the exact range [code: `storage.md` S3Store]. |
| CAS index, action cache, closure check, `Touch`, retention, three answers | **stays in kbf** | No S3 shape. |
| Raft metadata log on the hot disk | **stays in kbf** | Today it lives in `<hot store>/meta` when the hot store is a directory; with no hot store it needs `--meta-dir` [code: `stores.rs`]. |
| Map, HRW placement, chain replication, repair, handoff, read repair | **stays in kbf** | Node-level replication, which tierstore does not do. |
| Peers and mutual TLS roles, deny list | **stays in kbf** | tierstore has no mTLS; node to local tierstore is loopback with a key. |
| Collection of dead segments | **new, in kbf, through tierstore's GC key** | Nothing exists today (section 2.3). |
| Cold-store registration (`ColdStore/GetStoreConfig`) | **question** | One farm-wide store identity vs a node-local endpoint, Q4. |
| Object Lock retention (`put_new(retain_until)`) | **dropped on this store** | tierstore documents no Object Lock; a store without it must refuse a retention date, so no caller may pass one. Q5. |
| Packs, `cas/l/`, task grants | **same tierstore, other prefixes** | No change to their code. |

## 5. Proposal

### 5.1 Two shapes

- **Shape A: tierstore as the cold store, kbf hot store unchanged.** Per node, `tierstore` runs beside `kbf` with a
  flash `dir:` hot tier and the MinIO bucket as its cold tier; the node's `--cold-store` endpoint is the local tierstore.
  The prefix and bucket are unchanged, so every existing segment stays readable under the same key and nothing moves.
  A segment is written twice on flash (kbf block, then tierstore journal and object) and copied to cold once.
  [proposed]
- **Shape B: tierstore as the whole tier.** As A, with `--hot-store` unset on that node: the write tier, flusher and
  staged table are not used, and a segment is written once to flash by tierstore. [proposed]

Recommendation: A first, on one node, to learn whether tierstore is a correct and fast `ObjectStore` for the existing
reads and flushes; B only when the three questions of 5.2 to 5.4 are answered with measurements. A is reversible by one
flag; B changes what an acknowledgement means.

### 5.2 Durability before the copy (the decision B turns on)

With the kbf write tier, a write is acknowledged when **both owners** have fdatasynced it (with `--ram-ack-bytes` at 0).
[code: `docs/design/architecture-v2.md` section 8, "Writes: chain replication"] With tierstore alone, an acknowledgement is
**one** flash journal; the loss of that disk before the copy loses the object. [code: tierstore `limits.md`] The chain
still sends the blob to the replica, so under B each owner would write the blob into its own segment through its own
tierstore, and the pre-copy durability stays two nodes. The price is that the "one flusher" rule (`OneFlusher`, so the
cold store gets one copy) is a property of the write tier; whether it holds without one is the storage owner's to say
(Q2). If it does not, cold bytes double, and with no collector (section 2.3) the doubling is permanent.

### 5.3 Integrity stays in kbf

tierstore checks the checksums a client sends on a write and does not hash a storage node's objects on a read or in the
background. So the end-to-end proof is kbf's: ingress `VerifiedBlob`, and the SHA-256 of the exact range on every read,
with `Corrupt` and `Missing` marks. No check is removed by this design. Under B the flusher's re-hash of staged records
disappears; the equivalent is that ingress hashing already ran when the upload was accepted (the flusher's comment says
the daemon's digests are trusted at acknowledgement and checked at flush, so the daemon-supplied `Segments` path is the
one that loses a check; Q3).

### 5.4 Range reads against whole-object promotion

kbf reads one blob with a ranged GET of a segment of up to 128 MiB. A tierstore cold read fetches and promotes the whole
object. A warm farm is unaffected; a cold read costs up to a segment per blob, which improves the next reads from the
same segment and costs hot-tier space and cold bandwidth. Whether that is a win depends on how blobs of one action's
inputs sit in segments, which only a measurement can say (section 7). [proposed]

### 5.5 Garbage collection

[proposed] A collector in kbf, run by the node that owns the metadata (the rank-1 flusher for its own objects): when a
segment's every blob has been collected (the `Collect` result already returns each removed blob's location, so
"dead space" is known [code: `docs/design/storage.md` Retention]), delete the object through a tierstore **GC key** whose
prefix is the farm's object prefix. The GC key lists, heads and deletes and never reads a body, which fits a collector
that decides from its own index. A delete in tierstore removes both tiers' copies. Compaction of sparse segments and the
condemn delay stay as `docs/design/storage.md` plans them. This is new work with no code today; it does not gate the
migration, but it gates the cold bucket staying bounded, and it needs a design of its own.

### 5.6 Credentials

[proposed] The node's key to its local tierstore is a **scoped key** (read and write under the farm prefix, no delete
prefix); the collector holds the GC key; the cold tier's credential is tierstore's own (`--cold-credentials-file`) and
leaves kbf. This narrows what a storage node holds compared with a bucket key. kbf takes the pair from `AWS_*`, which
the configuration rules allow for provider-standard credentials. The "credential gap on `storage-and-daemon`" in
`docs/design/architecture-v2.md` section 9 is unchanged: the key must still be unreadable by the actions' uid.

## 6. Migration, one node at a time

Preconditions: tierstore's v1 paper has shipped; the conformance and baseline of steps 0 and 1 are recorded.

0. **Conformance, no farm change.** Run `kbf_objstore::conformance::run` (the `s3_conformance` example) against a
   tierstore with a `dir:` hot tier and an `s3:` cold tier, with Object Lock off. It must pass the base contract and
   the `conditional_put` capability, including ranged reads at the boundaries and list paging. A failure is tierstore's
   to fix or kbf's to relax, decided before step 2. [proposed; code: `crates/kbf-objstore/src/conformance.rs`]
1. **Baseline on today's farm.** Record input-fetch latency and cold GETs per build with nothing changed (section 7).
   kbf has no cold GET counter today, so the baseline comes from the bucket's own request counts plus a client-side
   timer. [proposed]
2. **Shape A on one node (the canary).** Install tierstore on the node (its unit and update hook are its own). Point
   that node's `--cold-store` at it, bucket and prefix unchanged. Keep `--hot-store`. Watch for a day under real
   builds: kbf `Missing`/`Corrupt` marks (`ObjectUnreachable`), flush retries, tierstore `read_errors`, copy lag.
3. **Gate to continue.** No new unreachable marks; flush success unchanged; baseline numbers not worse. Then the same
   for the next node, one at a time, canary first.
4. **Shape B on one node.** Only after Q2 and Q3 have answers and the duplicate-cold-bytes measurement is in. Unset
   `--hot-store`, set `--meta-dir`, restart. Same watch.
5. **Collector** (section 5.5) lands independently.

**Rollback, per node.** Shape A to the old state is: stop the node's kbf, keep its tierstore running until its copy
lag is zero, point `--cold-store` back at the bucket, start kbf. The copy lag is the trap: an object acknowledged by
tierstore and not yet copied is absent from the bucket, and a kbf pointed at the bucket would read `Missing`, mark the
object unreachable in its Raft log, and fail the read UNAVAILABLE until the blob is uploaded again. tierstore's drain
gives up the lease and exits; it does not promise a finished copy. [code: `operations.md` "Drain and restart"; kbf marks
`Missing` per `docs/design/storage.md` "Reads"] So rollback needs a way to read the copy lag, which `/metricz` does
not export today (Section 8, to tierstore). Shape B to A: set `--hot-store` again; staged-table state starts empty. A
node that wrote under B has no staged blocks to lose.

Mixed farm. Each node's index is its own Raft log and other nodes read a node's blobs through it, not through the
bucket, so a farm with some nodes on tierstore and some on the bucket serves correctly. The place where nodes read each
other's objects from the bucket is rebuild and the not-yet-built cold-store-first handoff
(`docs/design/rebalance-handoff.md`); do not run a rebuild or a handoff while a node's tierstore holds uncopied objects.
[proposed]

## 7. Measurements this enables

For the tierstore paper and for the go/no-go gates:

| Measurement | Source | Today |
|---|---|---|
| Cold GETs per build | delta of `tierstore_reads_total{tier="cold"}` (and `tierstore_misses_total{tier="hot"}`) over a build, divided by builds; cross-check with the bucket's own request count | not measurable in kbf (section 2.6) |
| Cold bytes read per blob byte served | `tierstore_bytes_read_total{tier="cold"}` over `tierstore_bytes_served_total`; shows the range amplification of 5.4 | n/a |
| Hot hit rate | hits over reads, per tier, per node | n/a |
| Input fetch p50 and p99 | **client side**: a histogram in `kbf-transfer` per blob or batch read (new), since neither kbf nor tierstore exports a latency histogram; `tierstore-bench` cold-read and tree-read cells for the isolated number | not measured |
| Put-to-ack and copy lag | `tierstore-bench` cells; copy lag needs a gauge (section 8) | the flush is timed only as write-tier calls |
| Duplicate cold bytes under B | bucket bytes written per uploaded byte, before and after | n/a |

Method: the same build set against the same commit, at least a cold run and a warm run per node shape, recorded with the
bench's result JSON. [proposed]

## 8. Questions

For the kbf storage owner:

- **Q1.** The farm shape names objects `<root>cas/<member>/<boot>/<seq>` with no log id; the form
  `<prefix>/<log id>/...` is the unregistered one. Which does the farm's bucket hold, and is anything keyed by log id
  that a prefix change would strand?
- **Q2.** Without a write tier, does the chain still make both owners write the blob into their own segment (double cold
  PUT), or is there a path that gives the rank-1 owner alone the flush? Is there any S3 deletion planned that would reclaim
  the doubled bytes?
- **Q3.** Which acknowledgement checks does the daemon `Segments` upload path rely on the flusher's re-hash for, and what
  replaces it under B?
- **Q4.** A node whose `--cold-store` is a fixed flag: does it ignore the farm-wide cold-store registration
  (`cold_apply.rs` `ColdPick::Fixed` against `Registrable`)? A node-local loopback endpoint cannot be a farm-wide
  registered identity.
- **Q5.** Is `put_new(retain_until)` called anywhere on the segment path? tierstore documents no Object Lock.
- **Q6.** Rebuild after a wiped metadata directory lists the store: should a node on tierstore list tierstore (both tiers
  merged) or the bucket (cold only, missing uncopied objects)? Is a listing that omits uncopied objects acceptable?
- **Q7.** Is a farm-wide collector owned by the Raft leader's node or per node? Segments are per writer, so per node
  seems natural, but the idle node's objects then never collect.

For the tierstore side (to be raised there, not decided here): export the copy lag on `/metricz` (rollback gate); a
latency histogram per op; confirm that a listing through the front covers hot-only objects.

## 9. Decisions, with a recommendation each

1. **What "storage moves to tierstore" means.** Recommend: tierstore replaces the object layer and, later, the write
   tier of the storage node; the storage node stays a kbf process. Reject replacing the node outright: the index, the
   action cache, the closure check and the map have no equivalent in an S3 server.
2. **First shape.** Recommend A (keep `--hot-store`), canary node, then one node at a time. B only after Q2, Q3 and the
   duplicate-bytes measurement.
3. **When.** Recommend steps 0 and 1 (conformance, baseline) now, step 2 after tierstore's v1 paper ships, as proposed.
4. **Key layout.** Recommend no change: same bucket, same prefix, so no data moves and rollback has nothing to
   rewrite. Revisit only if Q1 shows the layout wrong.
5. **Garbage collection.** Recommend a separate design for a collector over a tierstore GC key; not a gate for step 2,
   a gate for any claim that the cold store stays bounded.
6. **Integrity.** Recommend no check is removed; kbf's ingress and read-time digest checks stay the proof, because
   tierstore does not hash a storage node's objects.
7. **Credentials.** Recommend a scoped key per node for the local tierstore, a GC key for the collector only, and the
   cold credential held by tierstore alone.
8. **Packs.** Recommend pointing the pack store at the same tierstore under its own prefixes, and treating the pack
   path, not the storage node, as the long-term home for blob bytes; that migration (action cache entries, closure
   check) is a separate design.
9. **Range amplification.** Recommend measuring first (section 7); do not change the segment size or ask tierstore for
   ranged promotion before the number exists.
10. **Rollback gate.** Recommend tierstore exports the copy lag before step 2; without it, rollback of a node depends
    on a manual listing.
