# A faster job start with the tree still stored first: one tree file, and the costs of the write and the submit

> **Status: proposal. Nothing here is built.** This page measures three steps of a job's start and
> ranks the changes that make each one cheaper. Every change keeps today's order: the action tree is
> stored completely, its last object last, before the job is submitted. Section 6 lists the work, each
> item with a test that fails before it and a measurement to take after it.

komira-ai/kbf#2431 proposed submitting a job while its tree was still being written. It was closed:
overlapping the two saved about 30 ms and brought back the risk of a start failing on a tree that was
not stored yet. This page keeps the order and asks what each step costs:

1. **The tree write** (`tree_manifests` and `tree_root`, 35-48 ms at the p50).
2. **The `SubmitJob` round trip** (`rpc`, 22-28 ms at the p50).
3. **The host's tree attach** (76 ms at the p50 on a light morning, 215 ms on a busy evening).

## 1. The answer in one page

- **The largest cut is one tree file per action.** The file is written once by kbf and read whole by
  the host. It replaces about 370-1,350 directory manifests per tree. Today each is read, hashed,
  parsed twice as JSON and indexed on the heap at every attach (section 4).
  - The format exists already: tierstore's tree list v2 (`TSTREE`). kbf already writes it for Macs.
    tierstore already serves it in place, by binary search, with no per-entry parse.
  - What is missing is small:
    - tierstore: one branch in `read_tree`, so that `trees/<digest>` may name a tree list;
    - kbf: a third `--executor-input-tree` form that gives the job that host path.
  - Expected attach (inferred): about 7-10 ms for a typical tree, against 76-215 ms today.
  - Expected tree write (inferred): one create-only PUT of 0.2-0.3 MB instead of two serial stages.
    That is about 15-20 ms against 35-48 ms.
  - What is lost is per-directory dedup across trees. Every start now fetches its own whole file of
    0.2-3 MB. That costs one fill instead of about 14, and store space that needs a retention rule
    (section 4.6).
- **The tree write is plain S3 over HTTP/1.1 to a node's tierstore.** tierstore offers no faster
  write path to a remote writer: its binary protocols are node-local (section 2).
  - Each new manifest is one PUT. Up to 16 go at once, with no batch, and the root is one more PUT
    after them.
  - The ~500 manifests are not 500 PUTs: kbf's memo skips every directory it has stored already.
  - The root's stage, 13-17 ms at the p50, is 5-7 times the store's own PUT attempt at the p50
    (2.5 ms, measured). The rest is spent before or between attempts: waiting for one of the slots
    that tree writes share with every pack upload, or waiting out a 503 `SlowDown` inside the slot.
    This is inferred, and it is the first thing to measure.
- **`SubmitJob`'s queries are cheap alone. Its 22 ms is not split by any measurement yet.**
  - Its statements are prepared and each uses a primary key. EXPLAIN on a scratch copy of the schema,
    with 6.7 million rows, gives 0.02-0.24 ms per statement. One submit alone takes about 1 ms, commit
    flush included.
  - At the pool's 8 concurrent clients the same script takes 24.6 ms on the scratch copy, because every
    commit holds the one `head` row through its flush. So commit queueing is a live candidate for much
    of the 22 ms.
  - Two other waits are on the path: 11 sequential round trips to the database, and a mutex that a
    placement pass holds while it re-reads the job tables, which the handler takes before it answers.
  - Measure the phases first (item 0). Then cut the wait on the mutex (section 3.4) and the round
    trips. Keep `synchronous_commit` on.

The ranked plan (section 6, p50 of a start, every saving inferred unless marked):

| rank | change | repo | saves | work |
|---|---|---|---|---|
| 0 | timers for what is not measured yet (slot waits and 503 waits of the tree writes, the phases of `SubmitJob`, the manifests written per tree) | kbf, jobscheduler | 0 (it decides 2, 3 and 5) | small |
| 1 | one tree file per action on Linux: `trees/<list digest>` read as a tree list | tierstore, kbf | attach about 65 ms (light) to 200 ms (busy); tree write 20-30 ms | medium |
| 2 | `SubmitJob` answers before it adds the job to the placer's tables | jobscheduler | the wait behind a pass's table read: 0 when no pass is reading, up to the read when one is | small |
| 3 | tree writes do not queue behind pack uploads for store slots | kbf | up to about 10 of the root's 13-17 ms, if item 0 shows the slot wait | small |
| 4 | one job read and no separate clock read in `SubmitJob` | jobscheduler | 2 of 11 round trips, 0.1-0.6 ms | small |
| 5 | the pass reads queued jobs in batches, not one `get` each | jobscheduler | shortens the reads behind 2 and frees pool connections | small |
| 6 | the submit transaction in fewer statements | jobscheduler | about 5 round trips (8 to 3), under 1 ms on a local socket | medium |
| 7 | a mapped (not heap) backing for the tree file | tierstore | about 0.1 ms per attach at 2.4 MB (scratch); a memory saving | medium |

Line references are to these trees, as read:

| repository | commit | note |
|---|---|---|
| komira-ai/kbf | `09e01954e` | |
| komira-ai/tierstore | `5f90f0cd` | kbf compiles `tierstore` and `objstore-s3` at `ad492f81` (`Cargo.toml:87-88`) |
| komira-ai/jobscheduler | `c63400e61` | kbf compiles the `js-*` crates at `7b556352` (`Cargo.toml:76-83`) |

Words used for numbers:
- **measured**: the farm's own logs and metrics, read only, or a scratch measurement on a
  workstation (always called "scratch");
- **inferred**: worked out from measured numbers, with the arithmetic shown.

## 2. The tree write

### 2.1 What carries a `cas/l/<sha256>` write

- **The stages.** `TreeWriter::write_as` builds every manifest and the root in memory, then names the
  tree (`crates/kbf-front/src/action_tree.rs:591-592`). It then writes in three stages
  (`action_tree.rs:593-620`):
  1. blobs not yet stored anywhere;
  2. the input root's manifests, beside `.kbf`;
  3. the root.
- **Each manifest write.** `store_built` sends each write through
  `buffered(place::WRITES_IN_FLIGHT)` (`action_tree.rs:822`). `WRITES_IN_FLIGHT` is 16
  (`action_tree/place.rs:51`).
  - Each write then takes one of `MANIFEST_WRITES` = 32 slots (`action_tree.rs:134`,
    `action_tree/flight.rs:58-90`). Those slots are shared by every tree the frontend is writing.
  - Then `store_object` calls `put` and takes `AlreadyExists` as success (`place.rs:237-266`).
- **The store.** The store is the frontend's packs store, `Packs` (`kbf-server/src/executor/front.rs:336`),
  over `StoreWrites` (`kbf-front/src/store_writes.rs:42`).
  - `StoreWrites` holds 64 slots (`--pack-store-writes`), taken in `put_new` and `put_stream`
    (`store_writes.rs:68-90`). Every pack upload of the frontend also takes one of these slots, and a
    write keeps its slot while it waits out a 503 `SlowDown` (the module doc, `store_writes.rs:1-30`).
  - Then a write reaches `objstore-s3`'s `S3Store::put_stream`.
- **The protocol.** It is an S3 `PUT /cas/l/<hex>` with `If-None-Match: *`, signed SigV4 with
  `UNSIGNED-PAYLOAD`, over a pooled `reqwest` client.
  - The client is built with `rustls-no-provider` only and no `http2` feature. So it uses HTTP/1.1
    keep-alive, with one connection per request in flight (inferred from the features).
  - The pack store is plain `http://`: the client refuses `https://` (ARCHITECTURE.md, "S3 over TLS").
  - Idle connections close after 15 s.
- **The server.** It is the tierstore S3 front of one of the farm's nodes
  (`docs/deploy/kbf-modes.md:73-76`). Its handler is `Server::dispatch`, then `put_streamed`
  (tierstore `server/ops.rs:438`, `:634-686`). It allows 32 puts at once, then waits and answers 503
  `SlowDown` (`ops.rs:595-619`). Behind it is SeaweedFS (`docs/design/pack-index-over-the-store.md:35`).

### 2.2 Round trips per tree

- **One PUT per object.** Every new manifest, `.kbf` and the root is its own PUT. There is no batch,
  no pipelining on a connection, and no HEAD before a PUT.
- **Only new directories are written.** A directory in kbf's memo (100,000 entries, 600 s,
  `action_tree.rs:130-137`) is not built or written, and neither is anything below it
  (`action_tree.rs:742-785`).
- **A tree of about 500 manifests is the host's count, not kbf's.** The host counts the whole tree,
  base included, as the attach reads it (372 at the p50 on the light morning, 1,353 on the busy evening,
  section 4.1). On the host, a median of 14 manifests per attach were new to the node, and 98% of all
  the manifests read in the window came from the node's cache (measured).
  - 505 PUTs at 16 at a time would be 32 waves. `tree_manifests` is 23-31 ms at the p50 (measured). If
    a wave costs what the root's stage costs (13-17 ms), the stage writes one or two waves, about 14-30
    manifests. If a wave costs the 2.5 ms PUT attempt, it is 9-12 waves, about 140-190 manifests. Both
    are inferred.
  - No metric counts the manifests kbf writes per tree. Item 0 adds one, and it decides between the
    two readings.
- **Two serial stages.** The manifests stage ends, then the root's begins. Its round-trip floor is
  therefore two PUTs, plus the wait for a slot before each.

### 2.3 Does the root wait for an acknowledgement it does not need?

- **What the acknowledgement means.** tierstore answers a create-only PUT once the object has landed
  in the hot tier. Two routes do this, chosen by the store's format:
  - **entries:** five serial requests to the hot tier (tierstore `tiered/entry_put.rs:1-50`);
  - **marks:** two waves (`tiered/marked_put.rs:1-37`, from `docs/design/put-without-lock.md`).
  - The copy to the cold tier comes after the acknowledgement (`entry_put.rs:1-10`).
- **What the reader needs.** The host's attach reads `cas/l/<hex>` through its node's fill
  (`server/treefs/core/placed.rs:271-290`). So it needs exactly read-after-write visibility of the
  object in the store, which the acknowledgement gives. Nothing weaker would do: an object
  acknowledged but not yet readable would bring back the failure of komira-ai/kbf#2385.
- **Where the root's 13-17 ms goes.** The store's PUT is fast. The frontend's own histogram of
  pack-store requests gives a PUT p50 of 2.5 ms, p90 15.4 ms (`kbf_pack_store_request_seconds{op="put"}`,
  measured, n = 198,655, 2026-11-24, all sizes).
  - But the root's stage, one PUT of a small object, is 13-17 ms at the p50 (measured).
  - The histogram wraps the raw store (`Counted`, `kbf-server/src/frontend/pack_flags.rs:328`), so it
    is the time of one attempt. It leaves out the shared slots of `Flights` (32 across all trees) and
    `StoreWrites` (64, shared with every pack upload), and any 503 `SlowDown` waited out inside a slot.
    It also covers objects of every size.
  - The difference fits time spent outside the attempt, in one of those. This is inferred: no timer
    splits it.
  - Item 0 times the slot waits and the 503 waits apart from the request. Item 3 removes the slot wait;
    it does not remove a 503 wait.
- **The route.** The route the farm's tierstore takes (entries or marks) is configuration. It is not in
  any repository. If it is entries, moving it to marks is a configuration fix, not code (B20). The
  2.5 ms PUT p50 suggests a fast route is already in force.

### 2.4 Is kbf on the fastest write path tierstore offers?

Yes, for objects named `cas/l/<hex>`. tierstore's write paths:

| path | what it is | for kbf's tree? |
|---|---|---|
| S3 PUT, create-only, streamed (`ops.rs:508-520`) | what kbf uses | yes |
| multipart upload (`ops.rs:510-518`) | for bodies over `single_put_max` (`objstore-s3`, `backend.rs:366`) | no: trees are small |
| fill socket `put` (`server/fill_socket/put.rs:1-40`) | a node's own fs hands over a staged file | node-local only |
| trees gRPC `CreateTree` (`tierstore-trees-api/.../trees.proto:5-39`) | makes a tree on one node | no: the node is not known when the tree is written |

tierstore's binary protocols (the fill socket and the mount protocol) serve a node's own processes. No
remote writer can use them. A new "many manifests in one request" verb would save
`(waves − 1) × 13-17 ms` (inferred). Section 4 removes the many manifests instead, which saves the
same and the attach's cost too. So this page does not propose that verb.

## 3. The `SubmitJob` round trip

### 3.1 The client side

- **The timer.** `submit_job` takes `Instant::now()` (`crates/kbf-server/src/executor/run.rs:537`),
  awaits `api.submit_job(spec.clone())` (`:538`), and records step `rpc` (`:539`).
  - It runs after `tokio::join!(tree_written, put_row)` (`run.rs:443`, `:462-466`), as the order
    requires.
- **The client.** `kbf_job_client::Client` is built once (`kbf-server/src/frontend/executor_flags.rs:707-719`)
  and is gRPC over one lazily connected HTTP/2 channel (`crates/kbf-job-client/src/client.rs:1-7`,
  `:94-99`).
  - TLS is rustls, handshaken once per connection (`dial.rs:84-92`), with keep-alive pings every 10 s
    (`client.rs:35-41`). The dial sets `TCP_NODELAY` (`dial.rs:83`).
  - Auth is a static bearer token, read once from a file and attached per call (`client.rs:265-268`).
  - There is no per-call connect, TLS, DNS or token minting. The client never retries; the executor's
    retry is timed as `backoff`, not `rpc` (`run.rs:535-547`).
- **The spec.** It is a few hundred bytes (inferred from `executor/submit.rs:374-397`).

So nothing on the client side explains 22 ms.

### 3.2 The server side, in order

`submit_job` (jobscheduler `crates/js-lease-server/src/job_api/submit.rs:235-284`):

1. **Auth, role, validation.** In memory (`job_api/auth.rs:87-108`, `submit.rs:245`).
2. **`get_job`.** This runs on the blocking pool behind 32 permits (`job_api/mod.rs:254`, `:271`), on
   one of 8 pooled Postgres connections (`DEFAULT_MAX_CONNECTIONS`, `js-state/src/postgres/mod.rs:197`). It reads the job only to
   skip admission for a job already held (`submit.rs:246-253`).
3. **Admission.** `can_ever_serve` runs in memory (`submit.rs:255-263`).
4. **`insert_queued`.** It does these, in order (`js-jobs/src/store.rs:343-369`):
   - it reads the same job again (`same_job`);
   - it reads the database clock (`store.now()`, `postgres/mod.rs:920`);
   - it runs one transaction (`postgres/tx.rs`): `BEGIN`, the clock again (`tx.rs:146`), `reserve`,
     `load`, `LOCK TABLE head IN SHARE UPDATE EXCLUSIVE MODE` (`tx.rs:190-191`), the head move,
     one `write` statement, then `COMMIT` (`tx.rs:251`);
   - the commit waits for the WAL flush. `synchronous_commit` must be on, and a connection without it is
     refused (`postgres/mod.rs:26`, `schema.rs:641-646`).
5. **`poke`.** `note_inserted` runs on the blocking pool (`submit.rs:290-299`), then the poller is
   notified. `note_inserted` takes the placer's `tables` mutex (`js-sched-placer/src/jobs_seen.rs:269-285`).
   - A pass holds that same mutex for its whole re-read of four tables: `live_allocations`, `queued`,
     `marks` and `cordons` (`jobs_seen.rs:225-255`).
   - The re-read of `queued` is one range read per 500 keys (`SWEEP_BATCH`, `js-jobs/src/result.rs:36`)
     and then one `get` per job (`rows_of`, `js-jobs/src/store/index.rs:222-235`).
   - So a submit that arrives while a pass re-reads waits for all of it before it answers.
6. **The answer** is built from the commit's record (`submit.rs:281-283`). The placement pass itself is
   never on this path: it runs on the poller task (`poller.rs:91-102`).

That is **11 sequential round trips to Postgres** for a job with a client-chosen id (kbf sends one):
two reads, the clock, and eight in the transaction. A minted id costs nine.

### 3.3 The queries, with EXPLAIN on a scratch copy

**The setup.** This was a scratch copy only, and no live or replica database was touched:
- Postgres 17 on a workstation, on a Unix socket, `fdatasync` 0.89 ms (`pg_test_fsync`);
- the schema from jobscheduler's two migrations (`js-state/src/postgres/migrations/0001_state.sql:5-23`,
  `0002_...`);
- 6.7 million `kv` rows: 1.68 million ended jobs, which is 10,000 jobs an hour kept 7 days
  (`js-lease-server/src/config/retention.rs:10`), with their allocations and indexes, plus 1,500 queued jobs, 500 running
  ones and 100,000 feed rows.

The job store is a key/value table, and every index is a primary key (`0001_state.sql`). A job is three
`kv` rows and one `feed` row per key (`js-jobs/src/store/index.rs:44-51`, `:80-88`).

| statement | plan | time (scratch, measured) |
|---|---|---|
| `get` of `job/<id>` (absent) | Index Scan `kv_pkey` | 0.07 ms |
| `SELECT now` | Result | 0.15 ms |
| `reserve` (3 keys, `INSERT … ON CONFLICT … WHERE false`) | Insert on `kv` | 0.22 ms |
| `load` (3 keys, `= ANY`) | Index Scan `kv_pkey` | 0.02 ms |
| head move (`UPDATE head … RETURNING rev`) | Update, 1 row | 0.03 ms |
| `write` (one CTE: feed, `kv` upsert, delete) | Index Scan `kv_pkey` | 0.24 ms |
| the pass's scan of `idx/job/queued/` (1,500 keys) | Index Scan `kv_pkey` | 0.80 ms, plus one `get` per key |

- **The statements.** All are prepared once per connection (`js-state/src/postgres/session.rs:82-165`).
  `BEGIN`, `LOCK` and `COMMIT` are simple queries. No statement scans `kv` sequentially, and no index
  is missing.
- **The whole submit**, timed with `pgbench -M prepared`:

  | script | `synchronous_commit` on | off |
  |---|---|---|
  | today's 11 statements, 1 client | 1.02 ms | 0.22 ms |
  | today's 11 statements, 8 clients (the pool) | 24.6 ms mean, 325 commits/s | 1.0 ms |
  | one pipelined round trip (`BEGIN; LOCK; one CTE; COMMIT`) | 1.03 ms | 0.05 ms |

- **What the numbers say.**
  - With one client, the commit flush is nearly all of the 1 ms.
  - With 8 clients, every commit holds `head` until its flush ends (`tx.rs:14-21`). So commits run one
    after another, about 3 ms each here. The farm's peak was 193 submits in one second, measured from
    the job service's log on 2026-11-25. So on a disk like this one, the head lock is within a factor of
    two of full at the peak (inferred).
  - The farm's disk and its socket round trip are not measured.

**Where the 22 ms goes.** The `rpc` p50 of 22-28 ms is from the frontend's histogram of 2026-11-24
(cumulative from 16:44Z, and its last 14 minutes). The offset below is from the per-job join of the
morning of 2026-11-25. They were not taken in the same window.
- One submit alone is about 1 ms of Postgres on this scratch copy. Eight at once take 24.6 ms each,
  queued on `head`. How many writers the farm's job store has at once, and its disk, are not measured.
- The job service logs a submit 0 ms (p50) or 2 ms (p90) before kbf stamps the answer (measured,
  n = 12,974). So the answer's way back is short.
- The rest is spent before the handler ends. The candidates are all waits (inferred, in the order the
  code reaches them):
  - the 32 blocking-pool permits and the 8 pool connections. A pass and the heartbeats use the same
    pool;
  - the queue on `head` behind other writers' commits and flushes;
  - the `tables` mutex during a pass's re-read (section 3.2, step 5). With the farm's queue in the
    thousands on a capacity-bound day, that re-read is thousands of sequential `get`s.
- No metric splits these. Item 0 adds a phase histogram to the handler.

### 3.4 What can be cut

- **The wait on `tables` (item 2).** `note_inserted` is an optimisation for the next pass. It never
  changes the answer: it adds the job only when the submit's commit is the only write since the tables
  were read (`jobs_seen.rs:274`), and the pass re-reads otherwise. So the handler can answer first and
  note the job afterwards, or skip the note when the mutex is held (`try_lock`). Either way the job is
  still placed, by the re-read.
- **Two round trips (item 4).**
  - The handler and `insert_queued` read the same job twice (`submit.rs:246`, `store.rs:343`). One read
    serves both: run admission first (it is in memory), and read the job only when admission refuses,
    which is the only case where being held changes the answer (`submit.rs:17-27`).
  - `store.now()` is a round trip only to stamp `submitted_at`, and the transaction reads the clock
    again (`tx.rs:146`). The stamp can come from the transaction's own read. That changes the
    `Store`/`Txn` interface of `js-state`, which is internal to jobscheduler.
  - Three round trips outside the transaction become one: about 2 × 0.07-0.3 ms (scratch, and a
    socket round trip).
- **The pass's re-read (item 5).** One `get` per queued job (`rows_of`, `index.rs:222-235`) becomes one
  `WHERE key = ANY($1)` per page of 500. That shortens every wait on the mutex and on the pool.
- **Fewer statements (item 6).** The clock read, `reserve`, `load`, `LOCK`, the head move and `write`
  can be one statement for an insert-only job, so the transaction goes from 8 statements to 3 (`BEGIN`,
  the one statement, `COMMIT`). That saves about 5 round trips, under 1 ms on a local socket. One round
  trip in all, as the scratch prototype ran it (1.02 against 1.03 ms with the flush, 0.22 against
  0.05 ms without it), needs pipelining, which the synchronous `postgres` client does not have. It
  changes the generic transaction path of `js-state`, so it comes last.
- **Not proposed: `synchronous_commit` off for the submit.** On the scratch copy it saves 0.9 ms. But
  the store's rule C6 is that an acknowledged write is never lost (`postgres/mod.rs:26`), and a lost
  submit would leave kbf watching a job the job service does not know. The flush is not where the time
  goes.

## 4. The host's attach, and one tree file per action

### 4.1 What the attach does today

A FUSE lookup of `trees/<digest>` (tierstore `server/treefs/node_fs.rs:180`) reaches `Core::resolve`
(`treefs/core/implicit.rs:53-66`). A tree not attached yet is read by `read_tree`
(`treefs/core/placed.rs:241-306`):

1. **The root.** If the root manifest is not in the node's cache, the node is asked to fill the chain
   itself (`fill-tree`, `placed.rs:271-276`, komira-ai/tierstore#469). The node walks every manifest of
   the tree to find the new ones (`server/local_cache/tree_fill.rs:192`).
2. **The levels.** `flatten_in_waves` reads the tree a level at a time, up to 16 manifests at once, on
   threads spawned for each level (`tree_manifest/waves.rs:34`, `:119-135`).
3. **Each manifest.** `read_manifest` opens and reads a file into a `Vec` (`placed.rs:81-88`), or fills
   it first. Then `parse_manifest` (`tree_manifest.rs:265-301`):
   - hashes it again with SHA-256;
   - parses it as JSON twice;
   - allocates a `String` per name, digest, key and link target, and turns them into `Arc<str>`.
4. **The index.** `flatten` builds a `HashMap` and one `Vec<TreeEntry>`, and `Tree::listed` builds a
   child table (`server/treefs/tree.rs:104-132`). That is about 100 bytes and one or two heap
   allocations per entry.

What the farm measured (tierstore's per-attach line, five Linux nodes, read only):

| window (2026-11-25) | n | manifests p50 / p90 | new to the node p50 | attach ms p50 / p90 |
|---|---|---|---|---|
| morning, light | 3,925 | 372 / 1,535 | 14 | 76 / 193 |
| evening, busy, with `fill-tree` | 16,008 | 1,353 / 1,652 | 0 (89% filled nothing) | 215 / 574 |

- **Little of this is fetching.**
  - In the morning, attaches with 0-4 fills took 15-22 ms.
  - In the evening, attaches that filled nothing still took 188-272 ms at the p50, by size.
  - Over all the evening's attaches, 89% of which filled nothing, the `fill-tree` request took 95 ms
    at the p50 (measured). The likely cause is that the node walks the whole tree to find what is new
    (inferred from `tree_fill.rs`).
  - So the cost is reading and parsing manifests, twice in the evening (the node, then the reader), on
    loaded hosts. This is inferred; host load was not controlled.
- **The local work alone, on a workstation (scratch, measured).** Using tierstore's own
  `parse_manifest` and `flatten`, and a tree of 2,800 entries in 504 manifests: 4.0 ms. A tree of
  31,000 entries in 5,573 manifests: 52 ms. On the farm, attaches of 300-449 manifests with 0-4 fills
  took 21 ms at the p50 (measured, n = 81), about 57 µs a manifest. That is about seven times the
  workstation's 8 µs (inferred).

### 4.2 The tree list is already the single file

tierstore's tree list v2 (spec: tierstore `crates/tierstore/docs/tree-manifest.md`, "The tree list,
version 2"; reader: `tree_list_v2.rs`, `tree_image.rs`; writer: kbf `action_tree/tstree.rs`).

- **The header.** 64 bytes: the magic `TSTREE\0\0`, a major and a minor version, required flags, and
  six (offset, count) pairs.
- **Six sections**, each aligned to 8 bytes:
  - **entry records**, 32 bytes each, in breadth-first order. Each record holds:
    - the parent and the name's offset and length;
    - the kind and mode;
    - for a file, its key number, offset and size;
    - for a directory, its first child and child count.
    A directory's children are adjacent and sorted bytewise;
  - **SHA-256** of each file, 32 bytes, at the entry's index;
  - **key offsets** and **pack keys**, each key stored once;
  - **names**, each stored once (a link's target follows it);
  - **ranges**, 24 bytes each: a prefetch hint, spans of one pack merged across a 64 KiB gap
    (`tstree.rs:35`).
- **Lookups need no index.** The spec says "the tree is the file": an entry's number is its inode, a
  listing is a slice of records, and a lookup is a binary search (`tree_image.rs:270-308`). The fs
  already answers lookup, getattr and readdir from it in place (`Body::Image`, `server/treefs/tree.rs:32-46`,
  `:151`).
- **It is read whole into one heap buffer.** `read_image` reads the file into one `Vec`
  (`placed.rs:92`). `TreeImage::checked` walks it once for shape (`placed.rs:309-316`). Nothing is
  parsed per entry.
- **It is not mapped.** The `tierstore` crate forbids `unsafe` (`lib.rs:134`). Its sibling crate
  `tierstore-tree` maps its own list format through `vm-memory`, which needs no `unsafe`, and copies
  records out (`crates/tierstore-tree/src/image.rs:1-8`).
- **Today only placements read it.** That is the mount provider's form (`--executor-input-tree
  provider`, and every Mac job; kbf `crates/kbf-server/src/executor/config.rs:31-42`,
  `executor/out.rs:122-131`). `trees/<digest>` always reads manifests (`placed.rs:241-306`).

So the "one tree file, written once by kbf and read by the host" needs **no new format**. It needs the
host path to accept the file that already exists.

### 4.3 Size

The entry costs (inferred from the encoders):
- **Tree list:** 64 bytes per entry (32 for the record, 32 for the SHA-256), plus the name once, plus at
  most 24 bytes of ranges per file before merging, plus each pack key once.
- **Manifest form:** about 225 bytes per file entry with a pack source, about 145 without one, and
  about 130 per directory, as JSON (`action_tree/encode.rs:21-60`).

For a tree of 2,800 entries of which 505 are directories: 2,295 × 225 + 505 × 130 is about 0.58 MB of
manifests, and 2,800 × (64 + a 12-20 byte name), plus ranges and keys, is about 0.23-0.30 MB of list.
The scratch benchmark's generated tree of the same counts encoded to 0.49 MB of manifests and 0.22 MB
of a list without ranges.

| tree | entries | manifest bytes (whole tree) | tree list |
|---|---|---|---|
| typical | 2,800 | about 0.5-0.6 MB | about 0.23-0.30 MB |
| ten times larger | 28,000 | about 5-6 MB | about 2.3-3.0 MB |
| a 7 MB list | about 90,000 | about 16-19 MB | 7 MB |

- The list is about 0.4-0.5 times the size of the manifests; tierstore's spec says "a third" of v2
  against its v1 JSON list.
- The limits are far away:
  - a list is limited to 256 MiB (`MAX_TREE_LIST_BYTES`, `action_tree/listing.rs:64`; `tstree.rs:330`);
  - a whole-object fill is limited to 64 MiB (`WHOLE_FILL_LIMIT`, tierstore `server/local_cache/mod.rs:288`).
- The list repeats the base tree's entries in every action's file (`BaseTree::listed`,
  `listing.rs:7-14`). The table counts them in the entries.

### 4.4 What changes

**tierstore** (komira-ai/tierstore):

1. **`read_tree`** (`server/treefs/core/placed.rs:241`). Ask for the root object as today. If its bytes
   begin with the `TSTREE` magic, take it as a tree list: run `TreeImage::checked`, and return a
   `Tree::image` with no location. Skip `fill-tree` and `flatten_in_waves`. A JSON manifest (first byte
   `{`) takes today's path unchanged.
2. **`fill-tree`** (`server/local_cache/tree_fill.rs`). Stop at a root that is a tree list. Today it
   would try `parse_manifest`, fail, and fall back to the reader's path.
3. **`Tree::image` without a location** (`server/treefs/tree.rs`). `looked_into` and `root_parent`
   already treat `None` as a tree of `trees/` (`placed.rs:371`, `:451`).
4. **No second hash.** The whole-object fill hashes the object as it lands, and refuses it unless the
   digest is the key's (`server/local_cache/fill.rs:807-852`). That is the content check; a placement's
   image is not hashed again at attach either (the doc of `image_of`, `placed.rs:306-316`). A list already in the cache was
   checked when it was filled.
5. **Docs.** `docs/design/virtual-build-fs.md` (the namespace section) and the tree list spec: a
   `trees/<digest>` may name a list.

**kbf** (komira-ai/kbf):

1. **A third form, `--executor-input-tree list`** (`executor/config.rs:31-42`,
   `frontend/executor_flags.rs:391-402`). The tree is written as one tree list, and the job's layer is
   the host path `<mp>/trees/<sha256 of the list>` (`executor/out.rs:102-108`, chosen at
   `executor/submit.rs:388-391`). This is a new accepted value of a CLI flag, so that pull request is
   an interface change.
2. **No L2 sidecar for this form.** The sidecar serves the mount provider's per-cell checks
   (`docs/deploy/kbf-modes.md:309-312`). The fs checks each blob it fills against the list's SHA-256 of
   that file, as it does against a manifest's. So the list form writes blobs, then the list, and
   nothing after it (`action_tree/listing.rs:381-389` writes the sidecar after the list today).
3. **The base encoded once.** `tstree::encode` walks the listed base for every tree today
   (`listing.rs:1-14`), and the list form has no per-directory memo: only the `(action, form)` memo of
   600 s (`executor/front.rs:784-785`). The base's records, names and keys should be encoded once per
   frontend and only the input's records appended per tree.
   - Measured, the per-job join of 2026-11-25, over starts of at most 5 s: the executor's `start_ms` of
     a Mac job, which always uses the list form, was 184 ms at the p50 (n = 2,229), against 124 ms for a
     Linux job of the manifest form (n = 9,146). The forms differ in more than the encode, so this bounds
     it rather than measures it.
   - The stage timers `tree_list` and `tree_sidecar` exist (`executor/steps.rs:42`, `:124-125`). No
     scrape of them was kept, so how the 60 ms splits is not known. Item 0 reads them before this
     change is sized.
4. **The fallback.** The next attempt of a list-form run that failed `input-unavailable` uses the
   manifest form (section 4.7).
5. **The order is unchanged.** Blobs, then the list. `named` comes before the writes, and the submit
   waits for `Ingest::stored`. The guard `every_attempt_submits_its_job_only_once_its_tree_is_stored`
   (`executor/run_tree_tests.rs:38`) covers the new form too.
6. **Docs.** `docs/design/actions-as-jobs.md` 4.7 and `docs/deploy/kbf-modes.md:296-312`.

**jobscheduler:** nothing. The layer is still a host path under the agent's allowed `trees` root.

### 4.5 Expected attach and write (inferred)

Attach of a list not yet on the node, which is every attach: a list is unique to its action, and 3,919
of 3,925 morning attaches were the first of their digest on their node (measured).

| term | basis | 0.3 MB | 3 MB | 7 MB |
|---|---|---|---|---|
| one whole-object fill: a round trip to the store | the frontend's own GET p50 to the store, 3.9 ms (measured, all sizes; the node's fill path was not timed) | 4 | 4 | 4 |
| transfer at 1-10 Gbit/s | not measured | 0.3-2.4 | 2.4-24 | 5.6-56 |
| SHA-256 during the fill, on a node without SHA extensions | 5.5 ms/MB (measured, tierstore `tree-manifest.md`) | 1.7 | 17 | 39 |
| read into one buffer, then the shape walk | 0.76 ms/MB walk (measured, same doc) | 0.3 | 2.5 | 5.5 |
| tree number and keep | | under 1 | under 1 | under 1 |
| **attach** | | **about 7-10 ms** | **about 25-50 ms** | **about 55-105 ms** |

- **Today:** 76 ms at the p50 on the light morning, for trees of 372 manifests, and 215 ms on the busy
  evening, for trees of 1,353 manifests (measured). The evening's trees are about 7,500 entries, a list
  of about 0.6-0.8 MB, so about 10-15 ms by the same sum (inferred).
- **The saving** is about 65 ms at the p50 on the light morning and about 200 ms on the busy evening,
  comparing like sizes. Both assume that host load slows the list's few steps less than it slows
  parsing hundreds of manifests. That assumption is what item 1's measurement checks.
- **Very large trees.** A 7 MB list (about 90,000 entries) is about 55-105 ms. The manifest path of a
  tree that size was not measured on the farm; the workstation's 52 ms for 31,000 entries, times the
  seven of 4.1, suggests several hundred ms (inferred).
- **The hash dominates a large list.** It is already paid today, once per manifest, inside
  `parse_manifest`.
- **The local work** on a workstation (scratch, measured, `sha2` with SHA extensions): read, hash and
  walk of a 0.22 MB list took 0.10 ms, against 4.0 ms for the same 2,800 entries as manifests. For
  31,000 entries it was 1.1 ms against 52 ms.
- **Mapping instead of reading whole** saved 3 µs on the 0.22 MB list and 83 µs on the 2.4 MB list
  (scratch, measured); a 7 MB list would save about 0.25 ms at the same rate (inferred). The gain is
  one object and no parse. A mapped backing is item 7. It saves memory more than time, and it waits
  until a host measurement shows the memory matters.

The write, on kbf's side:
- **Today** it is two serial stages, 35-48 ms at the p50 (`tree_manifests` + `tree_root`, measured).
- **With the list form** it is one PUT. Take it as the root's lone PUT (13-17 ms) plus the body:
  0.3 MB at 1-10 Gbit/s is 0.3-2.4 ms. So about 15-20 ms, a saving of about 20-30 ms (inferred).
- **This holds only if the encode is cheap** (kbf change 3), and less if item 3 removes the slot wait
  first.

### 4.6 What is lost

- **Per-directory dedup across trees.**
  - Today a directory unchanged since another tree is written once (kbf's memo) and fetched once per
    node (98% of manifests came from the node's cache, measured).
  - With one file per action, every start writes and fetches its whole list. That is 0.2-3 MB where
    today it is about 14 small manifests.
- **Why it does not matter for time.** One fill of 0.3 MB costs one round trip and a few ms of
  transfer. The 14 fills it replaces cost a round trip per level, on top of reading every cached
  manifest. Section 4.5 counts the transfer, and the list wins at the tree sizes measured on the farm
  (372 and 1,353 manifests at the p50).
- **What it costs in bytes (inferred).**
  - At the morning's rate of about 5,600 starts an hour (12,974 Executes in 2 h 19 min, measured) and
    0.3 MB a list, that is about 1.7 GB an hour, about 40 GB a day, written once and read once.
  - Nothing deletes `cas/l/` today (`docs/design/actions-as-jobs.md:379`). So the list form needs a
    retention rule, which manifests could do without because they dedup.
  - A list is needed while its jobs can still start: the 600 s memo plus resubmits. A collector that
    removes lists older than a day, which no job names, is enough. It is not part of item 1's first
    pull request. It must land before the list form is the default on every frontend.
- **The base tree is repeated in every list.** A shared base file plus a per-action delta would win
  back those bytes. But that is two objects, a new format (B8) and an ordering between them, so this
  page does not propose it.

### 4.7 Rollout and older releases

- **B8.** The tree list v2 does not change. Its golden (tierstore `tests/stored_formats/tree_list_v2.rs`)
  must stay byte-identical, and that test is the evidence. What is new is a reader that accepts an
  existing record at an existing name.
- **An older tierstore** answers `EIO` for a list under `trees/`, with its rate-limited warning:
  `parse_manifest` refuses the bytes. It never answers a wrong tree. The start fails
  `input-unavailable`, which kbf takes as `infra` (`executor/outcome.rs:488-490`) and runs again. That
  spends one of the operation's resubmits (`RESUBMITS` = 2, `outcome.rs:153`), and the next attempt
  may land on an older host again.
- **So the list form falls back.** An attempt of the list form that fails `input-unavailable` runs its
  next attempt in the manifest form, as the provider form's miss path reads a refused list as
  manifests (`Ingest::list_refused`, `executor/outcome.rs:105-112`, `executor/front_miss.rs`). The
  provider form's refusal spends no attempt, because the host names the list in its refusal; an
  older fs's `EIO` reaches kbf only as `input-unavailable`, so this fallback spends one. A run
  therefore spends at most one attempt on an older host.
- **The order of adoption.**
  1. tierstore with the change of section 6, item 1, on every Linux host.
  2. Then `--executor-input-tree list` on one frontend lane, measured against the other.
  3. Then the default.

  Turning the flag back to `manifests` restores today's behaviour for the next run.
- **Failure cases of the list form:**

| case | what happens |
|---|---|
| the list write fails | no job is submitted, as today (`run.rs:452-454`) |
| a blob is lost before the list is written | MISSING, as today |
| the list is absent at the attach (lost from the store) | the fill answers `upstream 404`, the lookup `ENOENT` at once, as for an absent root today |
| the list is damaged (its hash is wrong) | the fill refuses it (`fill.rs:852`), and the lookup is `EIO` |
| the list's bytes pass the hash but not the shape walk | `EIO`, and no tree number is taken |
| a file's blob is lost after the attach | the running job's read fails as it does today with manifests |
| an older tierstore on the host | `EIO`, `input-unavailable`; the next attempt uses the manifest form (above) |

## 5. What this does not change

- **The order.** Blobs, then the tree's last object, then `SubmitJob`. The guard test stays.
- **The Mac form.** Macs keep the mount provider. kbf change 3 makes their list cheaper too.
- **The agent's lookup pool and lifecycle worker** (komira-ai/jobscheduler#1068). The attach's queue
  time on the host is separate work.
- **The REAPI part of the start** before the executor (the walk, the input checks).

## 6. The plan, ranked by milliseconds saved per unit of work

Each item is one pull request unless it says otherwise. "Fails before" is a behaviour test that fails
on today's code. No test asserts timing (T10): waits are shown with gates and explicit progress events.

### 0. Measure what is not measured

- **kbf.**
  - Time the wait for a `Flights` slot, the wait for a `StoreWrites` slot, and the 503 waits, apart
    from the store request, for `tree_manifests`, `tree_root` and `tree_list`.
  - Count the manifests each tree writes (`kbf_executor_tree_manifests_written`).
  - Keep a scrape of `tree_list` and `tree_sidecar`.
- **jobscheduler.** A `SubmitJob` phase histogram with these phases:
  - validation;
  - `get`;
  - admission;
  - `insert_queued`, split into the pool checkout and the transaction;
  - `note_inserted`.
- **Fails before:** a test that runs one tree write, and one submit on the fake store, and reads each
  new series with its count. None of the series exists today.
- **Measured after:** 30 minutes of light load and 30 of burst. These decide the size of items 2, 3
  and 5.

### 1. One tree file per action on Linux

- **Pull requests:** tierstore 4.4 items 1-3, then kbf 4.4 items 1-2, then kbf 4.4 item 3.
- **Saves:** attach about 65 ms (light) to 200 ms (busy), tree write 20-30 ms (p50, inferred).
- **Fails before**, tierstore (rigs of `core/placed_image_tests.rs` and `core/tree_fill_tests.rs`):
  - `a_tree_list_at_trees_digest_is_attached_from_one_fill`: one whole fill, no `fill-tree`, no
    manifest read, and the entries by index equal the image's. Today it is `EIO`, because
    `parse_manifest` refuses the list.
  - `a_tree_list_that_fails_its_shape_walk_is_eio_and_takes_no_number`.
  - `fill_tree_stops_at_a_root_that_is_a_tree_list`.
- **Holds today and keeps holding:**
  - `a_digest_the_upstream_has_no_tree_of_is_not_there_and_takes_no_number` (absent means `ENOENT`
    at once);
  - the v2 golden;
  - every manifest-form test.
- **Across releases (B8)**, both directions:
  - the new reader on old data: `a_manifest_root_at_trees_digest_takes_the_manifest_path`, a JSON root
    read exactly as today, with the same entries;
  - the old reader on new data: `a_tree_list_given_to_the_manifest_reader_is_refused_not_misread`, run
    on today's `read_tree` before the change lands, asserts `EIO` and no tree number. It holds today,
    and it pins what an older host does with the new form;
  - kbf: `a_list_attempt_that_fails_input_unavailable_runs_its_next_attempt_as_manifests`.
- **Fails before**, kbf:
  - `the_list_form_gives_the_job_trees_slash_the_lists_digest`: the layer is
    `<mp>/trees/<sha256 of the list>`.
  - `the_list_form_writes_its_blobs_then_one_list_and_no_sidecar`: a store that records the order of
    its writes.
  - `the_base_is_encoded_once_for_many_trees`: an encoder that counts base records encoded.
  - With the form on, `every_attempt_submits_its_job_only_once_its_tree_is_stored`.
- **Measured after:**
  - tierstore's per-attach line for list trees, `ok in <ms>`, by list size;
  - kbf's `tree_list` and phase `start`;
  - `input-unavailable` by attempt, which stays at zero for attempt 0;
  - one lane against the other.

### 2. `SubmitJob` answers before `note_inserted`

- **Change:** `submit.rs:277-299`. The answer goes first, and the note and the poke follow on a
  detached task. Or the handler uses `try_lock` and skips the note when the mutex is held.
- **Saves:** the wait behind a pass's table re-read. Its size comes from item 0.
- **Fails before:** a placer test whose store holds the re-read of `queued` on a gate. A submit made
  while the gate is held must answer before the gate opens. Today it blocks.
- **Also:** a submitted job is still placed by the next pass, with and without the note.
- **Measured after:** the `note_inserted` phase p90, and `rpc` p50 and p90.

### 3. Tree writes do not queue behind pack uploads

- **Change:** the tree's objects take their own store slots, not the 64 `StoreWrites` slots the pack
  uploads hold (`store_writes.rs:42`). Or they go first in that gate. The server's 32-put limit still
  bounds all of them.
- **Saves:** up to about 10 of the root's 13-17 ms, if item 0 shows the wait.
- **Fails before:** in kbf-front, with every pack-upload slot held by gated pack writes, a tree's root
  write still reaches the store. Today it waits for a slot.
- **Measured after:** the slot wait of `tree_root` and `tree_list`.

### 4. One job read and no separate clock read in `SubmitJob`

- **Change:** `submit.rs:246-263` and `js-jobs/src/store.rs:343-369`.
- **Saves:** 2 of 11 round trips.
- **Fails before**, with `js-state`'s `RecordingStore`:
  - a submit of a new id makes exactly one `get` of `job/<id>` and no `now` outside the transaction
    (today: two `get`s and one `now`);
  - a held job whose admission now refuses is still answered with the job;
  - a new job whose admission refuses gets `no-capable-host` and records nothing.
- **Measured after:** the `insert_queued` phase.

### 5. The pass reads queued jobs in batches

- **Change:** `rows_of` (`js-jobs/src/store/index.rs:222-235`): one `= ANY` read per page of 500.
- **Saves:** shortens item 2's wait (until 2 lands) and the pool use of every pass.
- **Fails before:** `RecordingStore`, 1,200 queued jobs: the pass makes 3 range reads and 3 batched
  reads, not 1,200 `get`s.
- **Measured after:** `js_job_pass_duration_seconds`, and the pool-checkout phase of item 0.

### 6. The submit transaction in fewer statements

- **Change:** `js-state/src/postgres/tx.rs`: one statement for an insert-only transaction.
- **Saves:** about 5 round trips (8 statements to 3), under 1 ms on a local socket.
- **Fails before:** a Postgres test that counts the statements of a submit's transaction: 3, where
  today it is 8.
- **The rows must not change:** the `kv` and `feed` rows, with their revisions and versions, are
  identical to today's (a golden). The conflict path still answers `job-id-conflict`.
- **Measured after:** the transaction phase.

### 7. A mapped backing for the tree file

- **Change:** `tree_image.rs`. A record reader over a `vm-memory` mapping, which copies records out with
  no `unsafe`, as `tierstore-tree` does.
- **Saves:** about 0.1 ms per attach at 2.4 MB (scratch), and the heap copy of each attached list.
  It waits until a host measurement shows the memory matters.
- **Fails before:**
  - on a large generated tree, the mapped backing answers `entry`, `child`, `children`, `mode` and
    `links` exactly as the heap image does;
  - bytes damaged after the open read as absent, never a panic.
- **Measured after:** resident memory of the fs with N trees attached.

### Not proposed

| change | why not |
|---|---|
| submitting before the tree is stored (komira-ai/kbf#2431) | it reorders the write and the submit |
| `synchronous_commit` off for the submit | it breaks the store's rule C6, for about 0.9 ms (section 3.4) |
| a wider manifest window (16 to 32), or a tierstore verb for many manifests in one request | item 1 removes the many manifests |
| the mount provider form on Linux | it needs the agent's provider, the L2 sidecar and a second PUT. The host path form keeps the layer contract and needs neither |

## 7. Decisions

- **D1. The host path form, not the provider form, for Linux.** It is one branch in tierstore's
  `read_tree` and a layer kbf already builds. It needs no change to the agent, its configuration or
  jobscheduler, and no sidecar PUT after the list.
- **D2. No L2 sidecar for the list form.** The fs checks every blob it fills against the SHA-256 in
  the list, as it does against a manifest. The sidecar serves the mount provider, which this form does
  not use.
- **D3. No second hash at attach.** The fill checks the object against its key as it lands. A
  placement's image is not hashed again at attach either.
- **D4. A heap buffer first, mapping later.** Mapping saved 83 µs at 2.4 MB (scratch), about 0.25 ms
  at 7 MB (inferred). The crate forbids `unsafe`, so mapping needs the `vm-memory` refactor of item 7.
- **D5. `synchronous_commit` stays on.** See section 3.4.
- **D6. EXPLAIN on a scratch copy with synthetic rows, not a replica.** No read-only replica of the
  job store was available. The schema is the code's own, and the row counts are twice the farm's
  measured rate kept for the retention the code sets. That gives the plans and an upper bound on
  their cost, but not the farm's disk or socket.
- **D7. A tree's "about 500 manifests" is the host's count of the whole tree.** kbf writes only the
  directories it has not stored. The page uses the attach line's counts, which are per tree and per
  node.
- **D8. A list-form attempt that fails `input-unavailable` falls back to manifests.** The job service
  cannot place by tierstore release, so the fallback bounds what an older host costs a run to one
  attempt, as the provider form's miss path does.

## 8. What was not measured, and why

| number | why not |
|---|---|
| manifests kbf writes per tree | no metric (item 0) |
| slot waits of the tree writes | no timer (item 0) |
| the phases of `SubmitJob` on the farm | no metric or log line carries a duration (item 0) |
| the farm's Postgres disk, its settings beyond `synchronous_commit`, and its socket round trip | not in any repository; the live database was not touched |
| the farm's tierstore write route (entries or marks) | it is configuration, not code |
| the size of real tree lists on the farm, and the base tree's entry count | the Mac lists were not sampled |
| SHA extensions on the farm's Linux hosts | taken as absent, the slower case |
| the network rate for a multi-MB fill | given as a range in 4.5 |
