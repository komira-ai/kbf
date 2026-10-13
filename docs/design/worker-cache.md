# Worker inputs: a shared blob transfer engine and a per-node NVMe file cache

Status: design, revision 2 (revised after an adversarial review). Nothing here is built.
Every "today" claim cites a file on `main` at `d7f25cf` (#524). Every number marked
*unmeasured* is an estimate that the PR introducing it must replace with a measurement.

This document covers four changes to how a kbf-daemon moves blobs:

1. bounded concurrent fetch and upload (fix 1);
2. FindMissingBlobs before upload (fix 2);
3. batched small blobs, BatchReadBlobs and BatchUpdateBlobs (fix 3);
4. a content-addressed file cache on each node's NVMe disk, hard-linked (Linux
   containers) or cloned (macOS) into each lease's input root (fix 4).

Fixes 1 and 2 are in progress on the branch `daemon/parallel-cas` (stage A, section 9),
which nothing here waits for. Fixes 1 to 3, and later the server's own object-store
traffic, become one **shared transfer engine** (section 4). Fix 4 is the cache (sections
5 to 7).

This is the design behind two lines that already promise it: `daemon.md` § Planned
("Prefetch and a local cache") and `storage.md` § Planned ("Locality").

---

## 1. What the code does today

| Step | Code | Behaviour |
|---|---|---|
| Input tree | `crates/kbf-daemon/src/tree.rs` `materialize` | A depth-first walk: one `fetch_message` per Directory, then **one awaited `fetch` per file, in series**. Files are created `O_EXCL` with mode 0644 or 0755. Both drivers call it (`kbf-driver-container/src/tree.rs` wraps it; `kbf-driver-native/src/runtime.rs:325`). |
| Blob connection | `crates/kbf-node/src/main.rs:456-472` `cas_client` | **One** `Endpoint::connect_lazy()` channel, which is one HTTP/2 connection, carries every blob of every lease. It refuses a `--cas` URL that is not `https://`. The worker stream has its own connection, built at `crates/kbf-daemon/src/daemon.rs:480` (`endpoint`). Neither sets HTTP/2 window sizes or a decode limit, so tonic's defaults apply. |
| Blob read | `crates/kbf-daemon/src/cas.rs` `CasClient::get` | One ByteStream `Read` per blob. Each blob is hashed when it arrives (`fetch`). No FindMissing, no Batch, no GetTree, no compression. |
| Blob write | `CasClient::put` / `put_chunks` | One ByteStream `Write` per blob, 1 MiB per message (`WRITE_CHUNK_BYTES`). Outputs are uploaded in series (`tree.rs` `collect`; container `outputs.rs`; native `kbf-outputs`). Every output is uploaded, even one the server already holds. |
| Worker listener | `crates/kbf-server/src/blobs.rs` | **Serves only ByteStream.** The `ContentAddressableStorage` service (FindMissing, Batch*, GetTree) answers UNIMPLEMENTED there. Every call is admitted against the node certificate (`admit`, line 39). Under plain text every call is refused UNAUTHENTICATED. |
| Server batch | `crates/kbf-front/src/cas.rs` `batch_read_blobs` | Up to 4 MiB of blob data (`MAX_BATCH_TOTAL_BYTES`, `kbf-front/src/lib.rs:89`); the gRPC message cap is 5 MiB (`MAX_MESSAGE_BYTES`, line 93). Blobs are read with `cache.read_blob` **one after another** (`cas.rs:196-198`), each a range GET on the object store (`storage.md` § Reads). |
| Container lease | `crates/kbf-driver-container/src/runtime.rs:252-277`, `podman.rs` | `materialize` writes the input root `lease/root`, which becomes the **overlay lower** (`--volume=root:/kbf/root:O,upperdir=upper,workdir=work`). `prepare_outputs` **renames** every input that an output names from `root` into `upper` (`tree.rs:188`). Then `podman unshare chown -hR 1:1` runs over root, upper and work; after exit, `chown -hR 0:0` runs over the same paths (`runtime.rs:274-277,421-428`). Containers run with `--userns=nomap` (`podman.rs:136`), so the daemon's uid maps to nothing inside. |
| Native lease | `crates/kbf-driver-native/src/runtime.rs`, `lib.rs` | Writes the input root into the lease directory and runs the action **as the daemon's user**. On macOS a `sandbox-exec` profile lets the action write only under its lease directory. On Linux nothing is enforced. |
| Per-lease timing | `runtime.rs:322-341` (container), native `:487` | `input_fetch_start/completed_timestamp` cover all of prepare. Resource usage travels as an `auxiliary_metadata` Any (`kbf-daemon/src/usage.rs`). |
| Placement | `scheduler.md` § Placement | The first live, feasible worker in name order. No locality input. `Heartbeat` (`worker.proto:251`) carries `seq`, `report_hash` and `running`. Capabilities live in the Hello and are hashed (`report.rs`). |

**Stage A as of this revision.** The `daemon/parallel-cas` work is not pushed yet. In its
current form it fetches a lease's Directory messages a level at a time and its file
blobs up to 64 at a time and 256 MiB in flight (`FETCHES_IN_FLIGHT`,
`FETCH_BYTES_IN_FLIGHT`). It uploads up to 64 outputs at a time (`UPLOADS_IN_FLIGHT`),
each distinct digest once. These limits are constants **per call**, so 16 concurrent
leases can have 1,024 streams on the one connection. It has no FindMissing, because
the worker listener cannot answer one. Section 4 replaces the per-call limits with one
limit for the whole process.

**The zig case.** Every compile's input root carries the unpacked zig toolchain:
thousands to tens of thousands of files (*unmeasured*; stage C1a reports the count).
Today each file costs one network round trip per action, in series, because nothing
is kept between leases.

---

## 2. Prior art

The sources were read, not run or measured: Buildbarn bb-remote-execution `582d07ff`
and bb-storage `6ded598f`, BuildBuddy `c422445c`, NativeLink `a435beba`, REAPI
`6def1c5d`, buck2 `a8d6b29e`. A cell says "not found" when the research looked and saw
nothing; that is a search result, not proof.

| | Buildbarn | BuildBuddy executor | NativeLink | **kbf (planned)** |
|---|---|---|---|---|
| Worker cache form | Native: a flat hard-link directory (`hardlinking_file_fetcher.go`). Virtual (the recommended mode): FUSE or NFSv4 over a local block store. | Hard-link directory with 2-character prefix subdirectories (`filecache.go`) | `FastSlowStore`: a local `FilesystemStore` in front of the gRPC CAS | Content-addressed file directory, 2-hex-character subdirectories, on the same filesystem as the lease scratch |
| Into the action dir | `link()` | `os.Link`; a reflink only for anonymous users or with `local_cache_always_clone` | `hard_link_many`; a directory `clonefile` on macOS | Container: `linkat`. macOS: `clonefile`. Linux native: **no cache** (5.2c) |
| Across a restart | Hard-link cache **wiped at start** (`main.go` `RemoveAllChildren`). The block store persists only when `persistent` is set. | Rescanned; an optional wipe after an unclean shutdown | Not covered by the research | C1a: wiped at start. C1b: rescanned, lazily re-verified |
| Executable bit | A separate key, `+x`/`-x` | Separate entries | A private 0555 copy per digest | A separate key (`.x` suffix) |
| File mode | 0444/0555, fixed mtime | **0644 (writable)** | 0444/0555 | 0444/0555, owned by the daemon's uid, fixed mtime |
| What stops an action writing a cached inode | Native: modes, plus privilege separation **when configured** (`bb_runner` `run_commands_as`). The README calls the split what makes hard-link caching safe. Without it, a same-user action can `chmod` and write (the research's inference). Virtual: by construction (`EACCES` on open for write). | Nothing in the code | Read-only modes; the research found no privilege separation | Container: overlay lower plus an unmapped owner. macOS: a CoW clone, plus a sandbox that denies the cache directory. Linux native: no cache until actions run as another uid. |
| Concurrent misses | Merged per key (`downloads` map) | Merged | Merged (`fast_slow_store.rs`) | Merged per key, in one process (one owner per cache directory) |
| Eviction | Count and bytes; RANDOM recommended, LRU when inode identity matters (Clang `#pragma once`) | LRU, bytes or a percentage of the disk (default 1 GB) | `max_bytes`/`evict_bytes`; without them, unbounded | LRU by bytes. C1b adds a free-space floor that counts only reclaimable entries. |
| Download concurrency | One limit per process, `input_download_concurrency` (examples: 10) | One limit per process, unlimited by default. A batch of N files holds N slots, so batches are capped at limit/8 files. macOS link tasks fixed at 5. | 64 per directory level, 64 for the whole directory cache | One limit per process in the transfer engine (section 4), and a separate limit for local links |
| Connections | Many ByteStream streams over one HTTP/2 connection | Not covered | Not covered | A pool of several connections (4.2) |
| FindMissing before upload | Batches of up to 10,000 (100 with native directories, for file descriptors) | Chunks of 1,000 | Yes; 64 uploads at a time | Yes, chunks of at most 1,000 |
| Batch read/write | **Never sent by the client** (`grpcclients/cas_blob_access.go:394`) | Up to 2 MiB per batch | Off by default; 128 KiB blobs, 3 MiB batches, 4 in flight | Blobs up to 64 KiB, batches up to 2 MiB, after the server reads a batch in parallel (stage S1) |
| Empty files | Not covered | Created directly, with no lookup | Not covered | Created directly (6.5) |
| Inserts | Download into the action directory, then link back | Not covered | Temp file on the same disk, then rename; each write calls `sync_all()`, so writes are capped | Temp file, then a rename that never replaces (6.4); no fsync |
| Directory-level cache | An in-memory Directory cache keyed `{digest, is_tree_root}` | `GetTree` with cached subtrees (a non-standard extension) | Whole-directory and subtree cache, `GetTree` prefetch | In-memory Directory cache, then subtree link plans (6.6) |

**The lesson.** Read-only modes stop nothing an action can undo. An action that runs as
the cache owner can `chmod u+w`, write, and `chmod a-w` again. The cases that hold are
the constructions that put the inode out of the action's reach: Buildbarn's FUSE mode,
and Buildbarn's native mode when `run_commands_as` gives actions another uid.
BuildBuddy's writable 0644 entries, and modes alone wherever actions share the owner's
uid, are open corruption paths. kbf's three drivers isolate actions three ways, so
section 5.2 settles each one separately. One of them, the Linux native driver, gets no
cache.

---

## 3. Contracts every stage keeps

1. **The server is the truth.** The local cache never answers FindMissing for an upload,
   as in Buildbarn's `ReadCachingBlobAccess` (Put and FindMissing go to the slow
   store). A blob the node holds but the server has collected is uploaded again.
2. **Every byte from the network is hashed before it is used**, batched or streamed,
   into memory or into a file (`digest_of`, as `fetch` does today).
3. **No cache flag means today's behaviour.** `--cache-dir` is unset by default. If the
   cache cannot be used, the daemon logs a warning, reports `cache_disabled_reason`,
   and runs uncached. It never refuses to serve over the cache (the "options plus at
   most a warning" ruling).
4. **Order of operations for one input file:** the local cache, then the cache's
   in-flight fill for that key, then the transfer engine (a batch if small, a stream if
   large).
5. **No caller waits for one blob before it asks for the next.** Callers hand the engine
   sets of digests (section 4).

---

## 4. The shared transfer engine

CEO ruling (2026-10-13): all bulk blob movement goes through one shared transfer engine
in kbf. That covers input fetch, output upload, cache fill, and later the server's
object-store traffic. The engine keeps a pool of several connections to kbf-server and
runs many concurrent streams across them under one in-flight limit for the whole
process. Small blobs are batched and large blobs are streamed in chunks. FindMissing
runs before every upload. Never one file at a time.

### 4.1 Where it lives and what it replaces

- A new crate, `kbf-transfer`. Its **core** knows nothing about REAPI: the permit pool,
  the byte budget, queues, merging duplicate requests, and counters. The **REAPI
  backend** in the same crate speaks ByteStream and CAS to kbf-server. One engine is
  built per daemon process (in `kbf-node`, where `cas_client` is today) and shared as an
  `Arc` by every lease, every driver and the cache.
- `CasClient` becomes the engine's per-connection wire client and stops being what a
  runtime holds. The engine's lease scope implements the existing `Cas` trait, so the
  first PR changes no driver and keeps every test that uses `MemoryCas`.
- Stage A's per-call constants (`FETCHES_IN_FLIGHT`, `FETCH_BYTES_IN_FLIGHT`,
  `UPLOADS_IN_FLIGHT`) are removed in the engine PR, not kept beside it. `materialize`
  and the upload walks hand the engine whole sets. No second limit survives to drift.
- **Later, server to object store (stage T3, not designed here).** kbf-front's segment
  reads and writes against MinIO move onto the same core with an object-store backend:
  a pool of HTTP connections, one in-flight limit, and coalesced range GETs. S1's
  parallel batch read (section 8) is a small bounded join inside `batch_read_blobs`.
  T3 replaces it; that migration is planned, not left as a second mechanism.

### 4.2 Connections

- `--transfer-connections` (default 4, *unmeasured*; T1 measures 1, 4 and 8 on the
  farm's network) separate HTTP/2 connections to the `--cas` URL, each from its own
  `Endpoint::connect_lazy()`.
- **Why more than one.** One connection is one TCP congestion window and one
  head-of-line queue on loss. On the server it is one hyper connection task, so all
  framing runs on one core. When it resets, every stream of every lease fails at once.
  Buildbarn uses one connection. The ruling asks for several, and T1's measurement says
  how many.
- **A new stream goes to the connection with the fewest streams in flight.** A connection
  that fails is reconnected with backoff. Its unfinished requests are retried on the
  others (4.4).
- **HTTP/2 windows are set explicitly**: `--transfer-stream-window-bytes` (default 8 MiB,
  *unmeasured*) and a connection window of eight times that. The default 64 KiB
  stream window caps one stream at about window/RTT. No setting in the code changes it
  today.
- `max_decoding_message_size(kbf_front::MAX_MESSAGE_BYTES)` is set on every client.
  tonic's 4 MiB default would refuse a full 4 MiB server batch once framing is added.

### 4.3 API

```rust
/// One per daemon process.
pub struct Transfers { /* pool, permits, byte budget, in-flight map, counters */ }

impl Transfers {
    pub fn new(endpoint: Endpoint, config: TransferConfig) -> Self;
    /// A lease's view: its own counters, and its queued work cancelled on drop.
    pub fn scope(&self, lease: &LeaseId) -> Scope;
    pub fn stats(&self) -> EngineStats;
}

impl Scope {                                   // also implements `Cas`
    /// Blobs no larger than `--transfer-memory-blob-bytes`, in completion order,
    /// each one hashed. Duplicate digests are fetched once.
    pub fn read_many(&self, digests: Vec<Digest>)
        -> impl Stream<Item = Result<(Digest, Bytes), CasError>>;
    /// One blob streamed into `file` chunk by chunk, hashed as it is written.
    pub async fn read_to(&self, digest: &Digest, file: &mut File) -> Result<(), CasError>;
    /// FindMissing over every blob, then uploads only the missing ones.
    pub async fn upload_missing(&self, blobs: Vec<Upload>) -> Result<UploadReport, CasError>;
    pub fn stats(&self) -> TransferStats;
}

pub enum Upload { Bytes(Digest, Bytes), File(Digest, PathBuf) }
```

- **`read_many`** sorts the request. Blobs no larger than `--transfer-batch-max-blob-bytes`
  go to a batch queue, once batching is on (T2). Everything else becomes ByteStream
  reads. A batch is sent when it reaches `--transfer-batch-bytes`, when the caller's
  set is exhausted, or after a short linger (2 ms, *unmeasured*), whichever comes
  first. A batch holds **one** permit, so a 500-blob batch cannot starve the streams.
  That is BuildBuddy's lesson, solved by counting a batch as one request instead of
  capping its size.
- **Duplicates are merged in memory.** Concurrent `read_many`/`read` calls for one digest,
  from any lease, share one network fetch, and the `Bytes` are shared. `read_to` is not
  merged across callers: two leases streaming the same large blob to two files fetch it
  twice when there is no cache. With a cache, the cache merges fills per key (6.4), so
  each large blob is fetched once per node. There are two maps because they hold two
  different things, in-memory results and on-disk entries. No blob is fetched twice
  while the cache is on.
- **`upload_missing`** sends FindMissing in chunks of at most `--transfer-find-missing-chunk`
  (default 1,000) digests. Missing blobs up to the batch threshold go to
  `BatchUpdateBlobs` (T2); the rest stream through ByteStream `Write` in
  `--transfer-chunk-bytes` messages (default 1 MiB, today's `WRITE_CHUNK_BYTES`). An
  upload from a file reads at most the size that was hashed, so a file still being
  appended to cannot make the upload disagree with its digest (Buildbarn's
  `UploadFile`). `UploadReport` says how many blobs and bytes were present, batched and
  streamed.
- **Before the server can answer.** The worker listener answers FindMissing and Batch
  with UNIMPLEMENTED until stage S1 is deployed. The first UNIMPLEMENTED turns that
  call off for the life of the process: it is logged once and counted in
  `find_missing_unavailable` or `batch_unavailable`. FindMissing then degrades to
  "upload everything" (today's behaviour), and batches to ByteStream. The engine ships
  before the server work with no flag day.
- **Limit discovery.** At first use the engine calls `GetCapabilities`. It takes the
  smaller of the advertised `max_batch_total_size_bytes` (where nonzero) and
  `--transfer-batch-bytes`. It never assumes 4 MiB.

### 4.4 Limits, retries, cancellation

- **One in-flight limit for the process:** `--transfer-in-flight` (default 64,
  *unmeasured*; Buildbarn's examples use 10, NativeLink 64, BuildBuddy unlimited). It
  counts concurrent requests: a ByteStream read or write, a batch call, a FindMissing
  call. Permits are granted first come, first served (`tokio::sync::Semaphore` is
  FIFO), so a big lease does not starve a small one past queue order.
  `--transfer-lease-in-flight` (unset by default) can cap one lease's share if the
  farm's metrics show a need.
- **One byte budget:** `--transfer-bytes-in-flight` (default 256 MiB, as stage A's
  constant). It bounds bytes held in memory: batched blobs, and `read_many` results not
  yet taken. A streamed read holds only its chunk buffers. A blob larger than the
  budget is fetched alone.
- **Local link and copy work is not in the engine.** Cache hits never wait behind network
  permits. They take the cache's own `--cache-link-concurrency` (6.4).
- **Retries.** A per-blob UNAVAILABLE, or a connection reset, is retried with backoff on
  another connection, up to `--transfer-retries` (default 3). A ByteStream read resumes
  from the bytes already hashed (`read_offset`). A write restarts from 0; resumable
  writes wait for `QueryWriteStatus` on the server. NOT_FOUND is `CasError::Missing`,
  the client's FAILED_PRECONDITION, and is never retried. A per-blob status inside a
  batch is handled per blob (REAPI). An INVALID_ARGUMENT for a whole batch is a bug:
  logged, then that batch is resent as ByteStream.
- **Cancellation.** Dropping a `Scope` (a lease killed mid-prepare) removes its queued
  requests that have not started. A started fetch other callers are waiting on keeps
  running. One with no waiters left is cancelled, unless more than half its bytes have
  arrived (then it finishes, for the next lease or the cache). Daemon shutdown aborts
  everything.

### 4.5 Knobs: flags, not environment variables

| Flag | Default | Meaning |
|---|---|---|
| `--cas` | (required by drivers) | The worker listener URL: `https://` with the daemon's TLS files, or `http://` (4.7) |
| `--transfer-connections` | 4 | Connections in the pool |
| `--transfer-in-flight` | 64 | Requests in flight, for the whole process |
| `--transfer-lease-in-flight` | unset | Optional cap for one lease |
| `--transfer-bytes-in-flight` | 256 MiB | Memory held by in-flight and untaken blobs |
| `--transfer-memory-blob-bytes` | 1 MiB | Largest blob `read_many` returns in memory |
| `--transfer-batch-max-blob-bytes` | 64 KiB | Largest blob put in a batch (0 = no batching) |
| `--transfer-batch-bytes` | 2 MiB | Largest batch, capped by the server's advertised limit |
| `--transfer-chunk-bytes` | 1 MiB | ByteStream write message size |
| `--transfer-find-missing-chunk` | 1,000 | Digests per FindMissing call |
| `--transfer-stream-window-bytes` | 8 MiB | HTTP/2 stream window; the connection window is 8x |
| `--transfer-retries` | 3 | Retries per blob on UNAVAILABLE or a reset |

Every default marked *unmeasured* in this section is set from T1's measurement and
written into the farm's deploy config, not hard-coded as policy.

### 4.6 Metrics, through the API

Per lease, the `Scope` writes `kbf.worker.v1.TransferStats` into `auxiliary_metadata`,
next to `ResourceUsage`: `read_calls` (ByteStream), `batch_read_calls`, `batch_blobs`,
`read_bytes`, `merged_reads` (duplicates served by another caller's fetch),
`find_missing_calls`, `outputs_present` (skipped by FindMissing), `write_calls`,
`batch_write_calls`, `written_bytes`, `retries`, `permit_wait_ms`, `transfer_ms`.
These reach agents through the invocations API's per-action view, which already
records `ExecutedActionMetadata`, and are summed by the invocation's metrics
(`GET /v1/invocations/{id}/metrics`, `invocations-api.md` 4.4). Adding the sums to that
response is part of T1.

Per process, on the operator API: `connections_ready`, `requests_in_flight`,
`queue_depth`, `bytes_in_flight`, `connection_resets`, `find_missing_unavailable`,
`batch_unavailable`, and read and write byte rates. None of this goes in the
Heartbeat.

### 4.7 TLS posture

kbf must not enforce TLS or mutual TLS (CEO ruling: TLS optional in the open-source
build; the farm's deployment uses it). The engine has no TLS policy of its own. It takes
the `Endpoint` that `--cas` describes: `https://` loads the daemon's TLS files as today,
and `http://` connects in plain text with one warning at start.

Today, three places enforce TLS on blob traffic:

- `cas_client` refuses a URL that is not `https://` (`kbf-node/src/main.rs:463`);
- the daemon refuses a server URL that is not `https://` (`kbf-daemon/src/config.rs:110`);
- the worker listener refuses every plain-text blob call (`kbf-server/src/blobs.rs:9`).

Lifting all three is **its own small PR** (stage P), not part of the engine. The hard
part is on the server: admission today is the node certificate, so a plain-text
listener needs another way to name the node. That is open question Q1. The engine works
unchanged either way.

---

## 5. How a cached file reaches a lease, per driver

kbf's three drivers isolate actions in three different ways, so each driver gets its
own answer.

### 5.1 Container driver (Linux, rootless Podman): hard links into the overlay lower

`lease/root` stays the overlay lower. Its **directories are fresh per lease**, created by
`materialize` as today. Its **files become hard links to cache entries**
(`linkat(cache, root/...)`). Two independent mechanisms keep the action from changing a
cached inode:

1. **Overlayfs never writes the lower layer.** Any write, truncate, chmod, chown or xattr
   change on a lower file copies it up into `upper`, and the lower inode is untouched.
2. **The owner is unmapped.** Under `--userns=nomap` the daemon's uid maps to nothing in
   the container, so a cached file appears owned by the overflow uid (65534). The
   container's root has no `CAP_DAC_OVERRIDE` or `CAP_FOWNER` over an inode whose owner
   it does not map, so it cannot open the file for writing or chmod it. It **can** read
   and execute it (the "other" bits are r-x). It can also unlink or rename the name,
   which leaves a whiteout in upper and drops nothing from the cache.

The action never sees the cache directory itself: it is not mounted into the container.

Three code changes are required, or the guarantee is void:

- **`chown -hR` must never touch a file in `root`.** Today `podman unshare chown -hR 1:1
  root` would re-own every cached inode on every lease, and the hand-back would re-own
  it again. The fix chowns **directories only** in `root`, plus all of `upper` and
  `work` as today. `podman unshare chown` has no type filter, so this becomes
  `podman unshare <kbf-node> chown-dirs 1:1 <root>`: a pure-Rust subcommand of the node
  binary that walks by descriptor without following symlinks (the rules of `remove.rs`).
  The hand-back to 0:0 does the same over `root`.
- **No cache inode may ever be under `upper`.** Today `prepare_outputs` `rename`s each input
  that an output names from `root` into `upper`. A renamed link would still be owned by
  the unmapped uid, so the action could not write it directly. The real danger is the
  next step: `chown -hR 1:1` over **upper**, which the design keeps, would re-own the
  shared inode to an id the container maps, and then the action could write it. The
  fix: an input that an output names is materialized as a **private copy** (copy or
  `FICLONE`), never a link. That includes every file in a moved subtree.
  `materialize` takes the output path set and copies those paths. C-2 plants the
  mutant: keep the rename, with the upper chown in place.
- **The owner stays the daemon's uid.** A cache file owned by subordinate id 1 would let
  an action open an input `O_RDWR` and copy it up, as it can today. But
  `fs.protected_hardlinks=1` (the default on Ubuntu, Debian and Fedora) refuses
  `linkat` on a file the caller does not own unless it can read and write it. The
  daemon could not link such files without entering the user namespace once per link.

**Behaviour change.** An action that opens an input file for writing in place gets EACCES
(today it gets a copy-up). REAPI lets a worker refuse this. Buildbarn's virtual mode
returns EACCES on open for write. Buildbarn native with `run_commands_as` refuses the
same way, by ordinary permission checks (inferred, not tested). BuildBuddy and NativeLink
without privilege separation do **not** refuse: their actions can chmod and write. An
output that is also an input stays writable, through the private copy. Open question
Q2 asks whether to accept this.

**What C-1 must prove on the farm before C1a merges.** The overlay behaviour above is
known for kernel overlayfs. C-1 records the graph driver (`podman info`: kernel overlay
or fuse-overlayfs) and the mount options (`index`, `userxattr`, `metacopy`). It then
checks both properties on that driver: the lower is never written, and a write to an
unmapped-owner file is refused. The refusal's errno may differ from plain permission
checks. `podman.rs:24-29` notes that writes under an unmapped-owner directory fail with
EROFS. Any errno is acceptable if the lower is unchanged. On a graph driver C-1 has not
passed, the daemon warns and runs uncached (`cache_disabled_reason=graph-driver`).

### 5.2 Native driver

**(a) macOS: `clonefile`, never a hard link, inside a sandbox that denies the cache.**

Actions run as the daemon's user, and the sandbox profile allows writes anywhere under
the lease directory (`kbf-driver-native/src/lib.rs`). A hard link in the lease
directory would therefore let an action write, chmod or `chflags` the cached inode.
`clonefile(2)` on APFS makes a new inode that shares blocks copy-on-write: a write
changes only the clone, for about the price of a link.

The clone protects the entries an action can see. What protects the cache directory
itself is the sandbox: the profile allows no writes outside the lease directory, so
`--cache-dir` must lie outside every lease directory. Test C-3b proves an action
cannot write, chmod or `chflags` a cache entry by its cache path. Without that
profile the cache is off (`cache_disabled_reason=no-sandbox`).

More on macOS:

- The cache and the scratch directory must be on one APFS volume, or `clonefile` returns
  EXDEV and the daemon copies.
- `remove_tree`'s permission and flag restoration (`kbf-outputs/src/remove.rs`) touches
  only lease-private inodes, as today.
- The call goes through the `libc` crate's `clonefile` binding, which keeps it pure Rust.
- The cache directory gets `.metadata_never_index` (no Spotlight indexing of millions of
  0444 files). It also gets the backup-exclusion attribute that
  `CSBackupSetItemExcluded` sets, so Time Machine skips it.
- The default `--cache-link-concurrency` on macOS is 5 (BuildBuddy fixes macOS link tasks
  at 5; NativeLink's comment says more than 64 per level floods the metadata lock). C2
  measures it.

**(b) Linux: no cache until actions run as another uid.**

A Linux native action runs as the daemon's uid, with nothing enforced. It can
`chmod u+w` a cache entry, rewrite it at the same size, `chmod a-w` it and reset its
mtime with `utimensat`. Owner, mode, size and type checks all pass afterwards. Cloning
or copying into the lease does not help: the cache directory itself is writable by the
action. So with the Linux native driver, `--cache-dir` is ignored: the daemon warns
and runs uncached (`cache_disabled_reason=native-same-uid`). The cache becomes possible
once actions run as a uid other than the cache owner's (Buildbarn's `run_commands_as`
split), which is not planned here.

A kernel-enforced alternative is worth a probe, but nothing here relies on it:
fs-verity (`FS_IOC_ENABLE_VERITY`, ext4 with the `verity` feature, or btrfs). It makes
a file's contents immutable while still allowing links. It is unverified, and a probe
target on the farm tests it first.

**A same-host rule.** A Linux native action running under the uid that owns a container
cache can write that cache directly, whether or not the two daemons share it. The deploy
rule is that the Linux native driver never runs under a uid that owns any `--cache-dir`.
The farm runs only the container driver on Linux.

---

## 6. The cache

### 6.1 On-disk layout

```
<cache-dir>/                     --cache-dir; same filesystem as --scratch
  v1/                            mode 0700; an unknown layout version is moved aside, never read
    blobs/<h0h1>/<sha256>-<size>     non-executable entry, mode 0444
    blobs/<h0h1>/<sha256>-<size>.x   executable entry, mode 0555
    tmp/<random>                     fills in progress
    lock                             exclusive flock: one owning daemon
```

- **The key is the digest plus the executable bit.** Hard links share mode bits, so one
  inode cannot be both 0444 and 0555. A fill of `k.x` when `k` is present is a local
  copy, not a network fetch.
- **256 subdirectories** keep each directory small (BuildBuddy's comment says the prefix
  helps throughput on many-core machines).
- **`v1/` is 0700.** Entries reach leases through their links, never through the cache
  path, so no other host user needs to list it. Entries stay readable by "other",
  because the container sees them as unmapped-owner files.
- **Owner and mode.** Every entry is owned by the daemon's uid, mode 0444/0555, with mtime
  set on insert to one fixed constant, `FIXED_MTIME` (Buildbarn's
  `DeterministicFileModificationTimestamp`).
- **Same filesystem.** At start the daemon compares `st_dev` of `--cache-dir` and
  `--scratch`. If they differ, it warns and runs the cache in **copy mode**: it still
  saves the network fetch, but copies instead of linking (`link_mode=copy`).
- **The name must not start `kbf-lease-` or `lease-`**: both drivers' sweeps remove entries
  with those prefixes (`kbf-driver-container/src/runtime.rs:623`, native `sweep.rs`). The
  daemon checks the name and warns.

### 6.2 What a lease sees: behaviour changes

1. **Identical inputs share an inode.** Within one lease, each key resolves to **exactly
   one inode**. If linking fails for a key (EMLINK, EXDEV, a lost race with eviction),
   the daemon makes **one** private copy for that lease and links the key's other
   names in the lease to it. Tools that read `(dev, ino)` then see the same answer on
   every run of identical inputs: GNU tar and cpio hard-link records, `rsync -H`, `du`,
   Clang `#pragma once`. Today every input is its own inode. With the cache, the
   identity is a function of the input tree alone.
2. **The owner shows as 65534** inside a container (5.1). Today it shows as 1.
3. **mtime is a constant.** Today it is the write time. Tools that compare input mtimes,
   such as make-style tools, see a constant.
4. **In-place writes to inputs get EACCES** in a container (5.1, Q2).

Without `--cache-dir`, none of this changes.

### 6.3 Integrity

| Threat | Defence | Cost |
|---|---|---|
| An action writes through a cached inode | By construction. Container: overlay lower plus unmapped owner, and the cache is not mounted. macOS: CoW clone, plus a sandbox that denies the cache. Linux native: no cache. | Zero per access |
| An action reaches the cache directory | Container: not mounted. macOS: sandbox profile (C-3b). Linux native: no cache. | Zero |
| A daemon bug leaves a partial or wrong file under a final name | Fills land in `tmp/`, hashed while written. They are closed, chmodded and given the fixed mtime, then renamed with `renameat2(RENAME_NOREPLACE)` (macOS: `renamex_np(RENAME_EXCL)`). An existing entry wins and the tmp file is removed, so a live entry's inode is never replaced under a lease. Nothing is ever opened for writing under `blobs/`. | One rename per insert |
| A crash or power loss mid-insert | C1a: the whole cache is wiped at start. C1b: `tmp/` is wiped at start, and a final name whose data never became durable (ext4 delayed allocation) is caught by the size check or by lazy verify. **No fsync per insert**: that would be about 20,000 fsyncs on a cold zig fetch (NativeLink caps its write concurrency because each write syncs). Lazy verify covers the gap. | None per insert |
| Bit rot, or an operator editing the cache, across a restart | C1b, **lazy verify**: after each start, an entry is re-hashed the first time a lease uses it, and only then marked verified in memory. A mismatch evicts it, and the lease fetches over the network (`cache_verify_failures`). The zig tree is hashed once per daemon start, not once per action. | Once per entry per start |
| Accidental mode or owner change | `fstatat` on every hit: owner is the daemon's uid, mode 0444/0555, `st_size` equals the key's size, regular file. A mismatch evicts the entry. **This is a check against accidents, not a defence against a same-uid writer**: that writer can restore every field (5.2b). | One stat per hit |
| Two daemons on one cache directory | An exclusive `flock` on `v1/lock`: one owner per directory. A second daemon warns and runs uncached (`cache_disabled_reason=locked`). Shared use is refused because each process would rebuild its own byte total, neither would count the other's inserts, and each would evict the other's pinned entries. | None |

Rejected: re-hashing on every link (the zig tree's full size per action, the very cost
the cache removes), and trusting entries after a restart without a verify (C1b).

### 6.4 Concurrency and fills

- **One fill per key.** An in-process map `HashMap<Key, Shared<BoxFuture<Result<(),
  CasError>>>>` behind a mutex. The first miss starts the fill (engine `read_to` into
  `tmp/`, or `read_many` for a small blob, then a write). Later misses for the key await
  it, then link. The entry leaves the map when the future resolves. A failed fill fails
  every waiter with the same `CasError`, so `Missing` stays FAILED_PRECONDITION for each
  of them. One process owns the directory (6.3), so no coordination across processes
  is needed.
- **Fills go through the engine** and take its network permits. Hits take only the
  cache's `--cache-link-concurrency` (default 64 on Linux, 5 on macOS), so a hit never
  waits behind a fetch.
- **Cancellation** follows the engine (4.4): a fill with no waiters left is cancelled
  unless more than half its bytes have arrived. Its tmp file is removed.
- **ETXTBSY.** A fill is closed before it is renamed. The remaining race is the one
  BuildBuddy hit: a child forked while a `tmp/` write descriptor is open holds it until
  its exec. If that file is renamed and executed in that window, exec fails with
  ETXTBSY. The native driver already retries a busy program for 2 s (`BUSY_WAIT`). The
  container driver maps a container that did not start to exit 126 (`program.rs`);
  C1a adds one retry of `podman start` when crun reports ETXTBSY (test C-9).

### 6.5 Sizing, eviction and the link limit

- **Budget.** `--cache-max-bytes` sets the budget; unset means no cache. C1b adds
  `--cache-min-free-bytes`, a floor on the filesystem's free space (`statvfs`). A
  starting point for the farm's deploy config is 400 GB max and 100 GB free
  (*unmeasured*; set in the farm's config, not in kbf).
- **Policy: LRU** by last link time, in memory. Buildbarn recommends RANDOM for the worst
  case, but says LRU is needed when inode identity matters, and LRU keeps a hot
  toolchain resident.
- **Eviction unlinks the cache's name only.** A running lease's links keep the inode alive;
  its space is freed when the lease directory is removed.
- **The free-space floor counts only reclaimable bytes.** An entry with `st_nlink > 1`
  (linked by a running lease) frees nothing when evicted. Free-space eviction therefore
  **skips entries with `st_nlink > 1`**, counts only `nlink == 1` entries as
  reclaimable, and stops when none are left, logging `cache_floor_unreachable`.
  Otherwise lease scratch below the floor would empty the whole cache without reaching
  it, and every later lease would be cold. Byte-budget eviction is unchanged: the
  budget counts entries, whoever links them.
- **Pins.** Keys a lease is materializing are pinned until its `materialize` returns. If
  a link still races an eviction, `linkat` returns ENOENT. The daemon then refills that
  key and links again (NativeLink's path). The pin makes the race rare and the retry
  makes it safe.
- **Empty files are created directly**, never linked or looked up (BuildBuddy). The
  empty blob is the commonest repeated digest, and linking it would reach the link
  limit soonest.
- **Link limit.** The daemon reads the limit with `pathconf(_PC_LINK_MAX)` (65,000 on
  ext4). A key whose `st_nlink` is within 1,000 of it is **copied** for the lease rather
  than linked, once per lease per key (6.2). A real EMLINK or EXDEV does the same, and
  each case bumps `cache_link_fallbacks`. Neither fails the lease. (The research grepped
  Buildbarn and found no EMLINK or EXDEV handling; it inferred such an error would
  surface as `Internal`.)

### 6.6 Directory-level caching

The zig tree is one subtree with a stable digest. There are three steps, each taken only
if per-lease metrics show the previous one is not enough:

1. **Directory cache (C4a).** An in-memory LRU of decoded `Directory` messages keyed by
   `(digest, is_tree_root)`, shared by all leases (Buildbarn's key: a Tree's root
   Directory does not hash to its own digest, so the two key spaces must not mix).
   Directory blobs also go into the file cache. A cold walk uses `GetTree` (S1): one
   streamed call, following `next_page_token`, instead of one round trip per level.
2. **Subtree link plans (C4b).** For a Directory with at least N files under it (for
   example 256), the daemon keeps the flattened `(relative path, key)` list after the
   first materialize. A later lease with the same subtree digest skips the walk and the
   name checks and runs the links in a tight loop. The plan comes from messages
   `check_name` already checked.
3. **Whole-subtree materialization.**
   - **macOS (C4c):** keep `v1/dirs/<digest>/`, an immutable materialized copy, and give
     each lease one `clonefile(dirs/<digest>, lease/root/<path>)`. `clonefile` copies
     directory modes too, and the cached directories are read-only. A walk must
     therefore make the cloned directories writable, which takes back part of the
     one-syscall saving. C4c measures what is left.
   - **Linux container:** the equivalent is a second `:O` overlay volume with the cached
     subtree as its lower. A `rename` across that mount boundary fails with EXDEV. It
     is considered only if C4b measures too slow, and is refused whenever an output path
     or the working directory lies under the subtree.

The freshness rule in `daemon.md` ("a marker left by one lease is never seen by the
next") gates every step here: test C-12.

### 6.7 Outputs into the cache (C4d)

A compile's outputs are often the next action's inputs on the same node. An output
moves into the cache only when **all** of these hold:

- the action is dead: container cgroup killed; native kill loop done;
- the output is already uploaded, so its digest is known;
- the file is regular and `st_nlink == 1`, so no other name, and no surviving process
  through another link, can reach it.

How it moves:

- **Container:** the file in `upper` was handed back by `chown 0:0`, which clears
  `security.capability`. The daemon **strips every xattr**, including the `user.overlay.*`
  that overlay writes with `userxattr`, and the POSIX ACL
  (`system.posix_acl_access`). Then it renames the file into `tmp/`, chmods it 0444/0555,
  sets the fixed mtime and inserts it (6.3). No copy is made.
- **Native (macOS):** a chmod does not revoke a write descriptor still open in a process
  that survived, so the file is never renamed. The daemon `clonefile`s it into a fresh
  inode in `tmp/`, strips the clone's xattrs and ACL (`acl_set_fd` with an empty ACL),
  and inserts the clone.
- Lazy verify still applies after a restart. Outputs never answer FindMissing (contract 1).

### 6.8 Crash and restart

- **C1a: wiped at start.** The daemon removes everything under `v1/` and starts cold, as
  Buildbarn does. This is safe even with leases that outlive the daemon. Wiping removes
  only the cache's names; a re-adopted lease's links keep its inodes alive (`daemon.md`
  § Planned, re-adoption).
- **C1b: kept.** At start, `tmp/` is wiped and `blobs/` is scanned **in the background**.
  An entry the scan has not reached yet counts as a miss, so a large cache does not
  delay the first lease. Every entry starts unverified. Recency is rebuilt from
  `st_ctime`: `link()` updates ctime, so ctime is a "last linked" clock persisted for
  free. mtime is fixed, so it cannot serve.
- **Disk full mid-fill.** The write to `tmp/` fails and the tmp file is removed. The lease
  falls back to writing its own copy (today's path), and eviction is kicked. ENOSPC in
  the cache never fails a lease that today's code would run.
- **Cache unusable** (missing, read-only, unknown layout, locked, wrong graph driver,
  native Linux): the daemon warns, reports `cache_disabled_reason`, and runs uncached.

---

## 7. Cache metrics, through the API

Per lease, `kbf.worker.v1.InputStats` goes into `auxiliary_metadata`, next to
`TransferStats` (4.6): `files`, `directories`, `empty_files`, `bytes_total`,
`cache_hits`, `cache_hit_bytes`, `cache_misses`, `fill_waits` (a waiter on another
lease's fill), `link_fallbacks`, `verify_hashes` (lazy-verify work this lease paid
for), `walk_ms`, `link_ms`, and `link_mode` (`hardlink`, `clonefile`, `copy`, or
`off:<reason>`). Network bytes are `TransferStats.read_bytes`, not counted a second
time here.

Per node, on the operator API: `cache_bytes`, `cache_entries`, `cache_evictions`,
`cache_verify_failures`, `cache_pinned_bytes`, `cache_reclaimable_bytes`,
`fs_free_bytes`, `cache_floor_unreachable`, `cache_disabled_reason`.

The cache's acceptance check is one of these numbers: **a second lease with the same zig
tree on the same node fetches 0 bytes for the toolchain** and has
`cache_hits == files - empty_files`. C1a checks this with a test counter and a log
line. C1c puts it in the API.

---

## 8. What placement learns

The cold cost of the zig tree is **once per node**. With a handful of nodes,
locality-aware placement saves at most (nodes - 1) cold fetches of one tree, which is
noise once the cache exists. So placement changes **last**, and only if metrics say so:

- **C5a, report only.** A new `Heartbeat` field, `held_trees` (at most 32 entries), lists
  the directory digests with the most cached bytes behind them, plus a `cache_bytes`
  total. The hot set stays out of the Hello's capabilities: those are hashed into
  `report_hash`, which would churn on every insert. The server stores it per worker and
  serves it on the operator API. It has no scheduling effect.
- **C5b, tie-break only, if justified.** Among feasible workers with room, the scheduler
  would prefer one that holds the request's largest input subtree. The scheduler today
  knows only the action digest, so the front would have to read the input root's
  Directories at Execute. That is new server work, and it would never be a feasibility
  constraint (Q3).

---

## 9. Rollout

| Stage | PR content | Depends on |
|---|---|---|
| **A** (`daemon/parallel-cas`, in progress) | Concurrent fetch in `materialize` and concurrent output uploads, each distinct digest once, with per-call limits (section 1) | nothing |
| **T1, the transfer engine** | `kbf-transfer` core and REAPI backend; connection pool; one in-flight limit and byte budget for the process; ByteStream reads (memory and streamed-to-file, resumable) and chunked writes; FindMissing with an UNIMPLEMENTED fallback; window and decode limits; `TransferStats` through the invocations API; the flags of 4.5. Removes stage A's per-call constants. | A |
| **S1, server** | `WorkerCas` on the worker listener: FindMissing, BatchRead, BatchUpdate, GetTree, each admitted exactly as `WorkerBlobs::admit` admits ByteStream (one admission rule). `batch_read_blobs` reads a batch's blobs concurrently (bounded, e.g. 32) and coalesces adjacent records in one segment into one range GET. Helps buck2's own batch reads with no client change. | nothing; in parallel with T1 |
| **P, plain text** | `--cas` and the server URL accept `http://` with a warning; the worker listener admits plain-text calls by the rule Q1 settles | Q1 |
| **C1a, the smallest cache that fixes zig** | Container driver only. `kbf-daemon::cache`: layout, **wipe at start**, the exclusive lock, one fill per key, byte budget, pins, the ENOENT retry, empty files created directly, link-limit copy, one inode per key per lease, `RENAME_NOREPLACE` inserts. `chown-dirs`. `prepare_outputs` copies instead of renaming. The ETXTBSY retry. Flags `--cache-dir`, `--cache-max-bytes`, `--cache-link-concurrency`. Tests C-1, C-2, C-4, C-8, C-9, C-10, C-13, C-16, C-17, C-18, C-19. | T1 planned first (see below) |
| **T2, batching (fix 3)** | `read_many` and `upload_missing` batch small blobs (4.3); per-blob status; the limit from GetCapabilities. Test C-14. | T1, S1 deployed |
| **C1b, persistence** | Background scan, `tmp/` wipe, lazy verify, stat check, ctime recency, `--cache-min-free-bytes` with the nlink-aware floor. Tests C-5, C-6, C-7, C-20. | C1a |
| **C1c, cache metrics** | `InputStats` and per-node cache metrics through the APIs (section 7) | C1a, T1 |
| **C2, macOS** | `clonefile`; the sandbox check and C-3b; Spotlight and backup exclusion; link-concurrency measurement | C1b |
| **C4a-d** | Directory cache and GetTree; subtree link plans; macOS directory clone; outputs into the cache | C1b; GetTree needs S1 |
| **C5a(-b)** | `held_trees` heartbeat report; a placement tie-break only if justified | C1c |
| **T3** | The server's object-store traffic moves onto the engine core | T1, S1 |
| Linux native cache | Not planned: it waits for actions to run under a uid other than the cache owner's (5.2b) | a uid split |

**Why the engine goes right after A and before C1a, and not merged with fix 3.**

- **Before C1a.** Cache fills are network fetches. Built on A's per-call limits, C1a
  would leave 16 leases of misses as 16 independent pools on one connection, and its
  fill path would have to be moved onto the engine later: a second migration of the
  same code. With T1 first, C1a calls `read_to`/`read_many` and is done.
- **Not merged with fix 3.** Batching is the most intricate part: per-blob status, size
  discovery, permit accounting, linger. Its gain depends on S1's parallel server read,
  and the research gives no client precedent that it pays (Buildbarn never batches).
  Tying it to T1 would put an unmeasured gain and a server dependency on the critical
  path to C1a. T1's API takes sets from day one, so turning batching on in T2 changes
  no caller.
- **Reversible.** C1a takes `&impl Cas`, so it is correct over any `Cas` and does not
  need T1 to work. If T1 slips, C1a can land first. The order is a plan, not a
  coupling.

Every new module stays under 1,000 lines; `cache` is split into
`cache/{layout,fill,evict,verify}.rs` from the start.

---

## 10. Test plan

Every test names the defect it catches and the mutant that makes it go red. C-1, C-2,
C-9, C-12 and C-18 need real rootless Podman (the M2 harness, `kbf-it`). C-3 and C-3b
need macOS. The rest run against `MemoryCas`, `kbf_front::routes` in process (which
serve FindMissing and Batch today), or a counting fake CAS.

| # | Test | Defect caught | Mutant |
|---|---|---|---|
| C-1 | Real Podman: an action tries `echo x >> in`, `truncate -s0 in`, `chmod u+w in`, `open(O_RDWR)` and `ln in in2; echo >> in2` on a cached input. A second lease reads `in`, and the entry is re-hashed. The test records the graph driver and mount options. | A cached inode that a container can write or re-own | `chown -hR 1:1` over `root`, files included |
| C-2 | Real Podman: an output path names an input file, and the action appends to it. The output is right and the cache entry is unchanged. | A linked input moved under `upper`, then re-owned by the upper chown | Keep `prepare_outputs`' rename, with the upper chown in place |
| C-3 | macOS: an action appends to, chmods and `chflags uchg`s a cloned input. The entry is unchanged, and the lease directory is still removed. | A hard link on the native driver | `linkat` instead of `clonefile` |
| C-3b | macOS: a sandboxed action tries to `chmod u+w`, write, `chmod a-w` and `utimes` a cache entry **by its cache path**. Every step is denied, and the next lease gets the right bytes. | A sandbox profile that lets actions reach `--cache-dir` | Allow writes to `--cache-dir` in the profile |
| C-4 | 32 concurrent leases over one tree with a counting CAS: each blob is fetched exactly once. | Fills not merged per key | Remove the fill map |
| C-5 | (C1b) The fake CAS stalls mid-blob, the fill is dropped, and in an integration test the process is killed. After restart, `tmp/` is empty, no final name is short, and the next lease gets the right bytes. | Writing under the final name; `tmp/` not wiped | Fill straight into `blobs/` |
| C-6 | (C1b) One byte flipped in an entry between two starts: the first use re-hashes, evicts and refetches; `cache_verify_failures == 1`; the lease succeeds. | Trusting the cache after a restart | Mark every entry verified at scan |
| C-7 | (C1b) An entry chmodded to 0644 is evicted at its next use. | No stat check on a hit | Skip the `fstatat` |
| C-8 | The budget is exceeded: LRU entries are evicted, pinned keys survive, a running lease's linked file stays readable after its cache name goes, and the byte total equals the on-disk sum. | Evicting pinned keys; wrong accounting | Ignore pins; count `st_blocks` twice |
| C-9 | An executable inserted while the daemon spawns children in a loop: no lease fails with 126/ETXTBSY beyond one retry. | The ETXTBSY race | Remove the retry |
| C-10 | One digest appears both executable and not in one tree: two entries, modes 0555 and 0444, each linked to the right names. | A key without the exec bit | Drop `.x` from the key |
| C-11 | (T1) An output present locally but missing on the server is uploaded. One the server holds is not (`outputs_present`). Against today's worker listener, FindMissing is UNIMPLEMENTED: everything is uploaded, the call is logged once, and no lease fails. | The cache answering FindMissing; no fallback | Short-circuit uploads on a local hit; treat UNIMPLEMENTED as fatal |
| C-12 | Freshness: lease 1 creates, deletes and renames files in a cached subtree. Lease 2 with the same digest sees the pristine tree (C1a, C4b, C4c). | One lease's changes leaking to the next | Reuse a lease's materialized subtree as the cached directory |
| C-13 | `--cache-dir` on another filesystem than `--scratch`: a warning, `link_mode=copy`, leases succeed. EMLINK is simulated through a fake link operation. | No EXDEV/EMLINK fallback | Remove the fallback |
| C-14 | (T2) A 3,000-blob tree is fetched in batches of at most 2 MiB. A full 4 MiB server batch decodes. A per-blob NOT_FOUND gives `Missing`. A corrupt blob in a batch is refused. One batch holds one permit. | tonic's 4 MiB default; per-blob status ignored; unhashed batch bytes; a batch taking N permits | Default decode limit; skip `digest_of`; one permit per blob |
| C-15 | (S1) A 100-blob `BatchReadBlobs` against an object store with 10 ms per GET finishes well under 100 x 10 ms. | Serial batch reads | Revert to the serial loop |
| C-16 | A second daemon on a locked `--cache-dir` warns, reports `cache_disabled_reason=locked`, runs uncached, and its leases succeed. | Two owners sharing one cache's accounting | Take the lock shared |
| C-17 | Zig-shaped tree (20,000 files; shape from the real toolchain, *unmeasured*): lease 2 fetches 0 bytes and `cache_hits == files - empty_files`. `link_ms` is reported, not gated. | The cache not consulted on the hot path | `materialize` ignoring the cache |
| C-18 | Determinism: a tree with two identical-content files `a` and `b`, run twice, once with EMLINK forced on that key. `stat` of `a` and `b` inside the action shows one inode both times, and a `tar` of the inputs is identical across runs. | A fallback that gives some names a private copy and others the cache inode | Fall back per name instead of per key per lease |
| C-19 | Linux native with `--cache-dir`: a warning, `link_mode=off:native-same-uid`, and the cache directory is never created or read. | Enabling a cache that a same-uid action can poison | Enable the cache on Linux native |
| C-20 | (C1b) Free-space floor: leases hold links to most entries, and scratch pushes free space below the floor. Only `nlink == 1` entries are evicted, `cache_floor_unreachable` is set, and the linked entries survive. | A floor that empties the cache without freeing space | Ignore `st_nlink` in free-space eviction |
| C-21 | (C4d) Outputs into the cache: an output with a second hard link, an output carrying a `user.*` xattr and an ACL, and (macOS) an output with a write descriptor held by a surviving child. The first is not inserted. The second is inserted without xattrs or ACL. The third's entry is a fresh inode, unchanged after the child writes. | An action's inode or its metadata entering the cache | Skip the nlink check; skip the strip; rename instead of clone on native |
| T-1 | (T1) 16 leases of 1,000 blobs each against a counting server: concurrent requests never exceed `--transfer-in-flight`, streams spread over every connection, and each digest is fetched once. | Per-call limits multiplying; a pool that uses one connection | Restore a per-call semaphore; always pick connection 0 |
| T-2 | (T1) A connection reset mid-stream: the read resumes on another connection from its offset, the blob hashes right, and `retries == 1`. | No retry; a resume that duplicates or skips bytes | Resume at offset 0 while appending |
| T-3 | (T1) A lease dropped with queued requests: they never start, and a shared fetch with another waiter completes. | Leaked or wrongly cancelled work | Cancel shared fetches on drop |

Wall-clock speedups are reported, never asserted.

---

## 11. Options considered and rejected

| Option | Why not (now) |
|---|---|
| **FUSE/NFSv4 lazy input root** (Buildbarn's virtual mode, its recommended one) | Rootless Podman would need `/dev/fuse` passed through and a FUSE server per lease. macOS needs macFUSE (no longer open source) or an NFSv4 server written in Rust. Buildbarn's own notes say lazy reads need prefetching (ADR 0010) and timeout compensation to perform. It is several times the size of C1a. Revisit only if actions read a small fraction of huge input roots. |
| **Buildbarn's two layers** (a persistent block store under a volatile hard-link cache) | Two caches to size and keep consistent. kbf materializes files anyway, so the file directory *is* the store. |
| **Copy every input from the cache** (no links) in containers | Correct, but writes the zig tree's full size to the NVMe per lease. Kept as the fallback mode. |
| **Bind-mount the whole cache read-only into each container** | Exposes every cached blob of every build to every action, and the paths do not match the input tree. |
| **A cache for Linux native with clone or copy** | The cache directory is writable by a same-uid action, so the stat check and lazy verify can both be beaten (5.2b). |
| **Hard links on the native driver** | Actions run as the daemon's user, and the macOS sandbox allows writes in the lease directory. |
| **Two daemons sharing one cache directory** | Each would rebuild its own byte total and LRU, neither would count the other's inserts, and pins would not cross. One owner, by lock. |
| **fsync on every insert** | About 20,000 fsyncs on a cold zig fetch; lazy verify catches a torn file after a restart. |
| **Hash every cached file on every use** | Costs the tree's size per lease. Replaced by safety by construction, a stat check per hit, and a lazy hash once per start. |
| **Local cache answers FindMissing** | The server is the truth. A blob collected server-side but held locally would never be re-uploaded. |
| **Hot set in the Hello** | Churns `report_hash`. A bounded Heartbeat field instead (C5a). |
| **Owner = subordinate id 1 for cache files** | `fs.protected_hardlinks` stops the daemon linking files it does not own. |
| **One connection with many streams** (Buildbarn's shape) | One congestion window, one server task, one failure domain. The pool's size is a flag, so 1 stays possible. |
| **Engine and batching in one PR** | Puts an unmeasured gain and a server dependency on the path to C1a (section 9). |
| **Per-call in-flight limits** (stage A as it stands) | They multiply by concurrent leases; replaced by one process-wide limit in T1. |

---

## 12. Open questions (each with a recommendation)

- **Q1. Worker listener TLS.** `blobs.rs` refuses every blob call in plain text and admits
  by node certificate. `WorkerCas` (S1) inherits that admission. kbf must not enforce TLS.
  *Recommendation:* S1 uses exactly `WorkerBlobs`' admission, so there is one rule.
  Stage P settles plain text once, for both services, in its own small PR. The options:
  (a) admit plain-text calls from any peer, with a startup warning, for open-source
  single-tenant use; (b) admit plain text only with a bearer token the worker stream
  issues per session. The farm's deployment keeps mutual TLS either way. Recommend (b)
  if it stays small, else (a).
- **Q2. In-place input writes get EACCES in containers with the cache.**
  *Recommendation:* accept it. REAPI allows it, and Buildbarn's virtual mode refuses the
  same way. If a real client breaks, add a per-action platform property
  `kbf-writable-inputs=1` that copies inputs for that action (an option, not a
  default).
- **Q3. Locality placement (C5b).** *Recommendation:* ship C5a's report only, and decide
  with a week of `InputStats` from the farm.
- **Q4. Default budget.** *Recommendation:* no default in kbf (no flag, no cache). The
  farm's deploy config sets 400 GB max and 100 GB free, then tunes from
  `cache_evictions`.
