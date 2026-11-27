# A faster job start with the tree still stored first: one tree file, and the costs of the write and the submit

> **Status: proposal. Of its items only 1e, the collector of tree lists, is built.** This page measures three steps of a job's start and
> ranks the changes that make each one cheaper. Every change keeps today's order: the action tree is
> stored completely, its last object last, before the job is submitted. Section 6 lists the work, each
> item with a test that fails before it (except 1a, a configuration step) and a measurement to take
> after it.

komira-ai/kbf#2431 proposed submitting a job while its tree was still being written. It was closed:
overlapping the two saved about 30 ms and brought back the risk of a start failing on a tree that was
not stored yet. This page keeps the order and asks what each step costs:

1. **The tree write** (`tree_manifests` and `tree_root`, 35-48 ms at the p50).
2. **The `SubmitJob` round trip** (`rpc`, 22-28 ms at the p50).
3. **The host's tree attach** (76 ms at the p50 on a light morning, 215 ms on a busy evening).

## 1. The answer in one page

- **The target is one tree file per action, written once by kbf and mapped by the host.** It replaces
  about 370-1,350 directory manifests per tree and their root. Today each manifest is read, hashed,
  parsed twice as JSON and indexed on the heap at every attach (section 4).
  - **The file exists already.** It is tierstore's tree list v2 (`TSTREE`), one object at
    `cas/l/<sha256>`. kbf writes it for every Mac job, and for Linux jobs under
    `--executor-input-tree provider`. tierstore serves it in place, by binary search, with no parse
    per entry. Linux jobs still use the default, `manifests`.
  - **The Linux hosts can already take it.** The farm's six Linux build nodes run the tierstore and
    js-agent releases the provider needs, and their tierstore and agent have the mount provider
    configured. What is left is a check of each agent's capability and the frontend's flag
    (section 4.3).
  - **What changes in code** (section 4.5):
    - tierstore: tree lists are mapped, not read onto the heap, under a new budget,
      `fs.tree_list_ram_bytes` (512 MiB by default; 2 GiB on a farm's busy hosts);
    - kbf: the list's base part is encoded once per frontend, and its sidecar is written beside the
      list, not after it.
  - **Expected attach** (inferred): about 7-10 ms for a typical tree, against 76-215 ms today.
  - **Expected tree write** (inferred): the list's and its sidecar's create-only PUTs at once, instead
    of two serial stages: about 15-20 ms against 35-48 ms, once the sidecar no longer follows the
    list.
  - **What is lost** is per-directory dedup across trees. Every start writes and fetches its own whole
    file of 0.3-7 MB. That costs one fill instead of about 14, and bytes that need a retention rule.
    At these sizes neither changes the start (section 4.8).
- **The tree write is plain S3 over HTTP/1.1 to tierstore's S3 front**, through kbf's `ObjectStore`
  put path, with S3's errors (a 503 `SlowDown` is waited out, `action_tree/retry.rs`). That is
  tierstore's fastest write path for a remote writer: its binary protocols are node-local (section 2).
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
| 1a | the provider form on one Linux frontend lane, measured against the other (configuration only) | kbf flag | attach about 65 ms (light) to 200 ms (busy), less what the provider adds (not measured on Linux); the tree write 15-33 ms against the manifest form, once 1b is in | small |
| 1b | the list's base encoded once, and its sidecar written beside it | kbf | the base encode, and the sidecar's serial stage (`tree_sidecar`, not yet read); together at most the 60 ms the list form adds at kbf today | small |
| 1c | tree lists mapped, not on the heap, under `fs.tree_list_ram_bytes` | tierstore | the two heap copies of every list in memory (each 0.2-5.2 GiB on a busy host); 0.003-0.25 ms per attach | medium |
| 1e | a collector for tree lists no job names | kbf | no time; the bytes lists add (4.8). Before the provider form is the default | small |
| 1d | only if 1a shows the provider's own cost: `trees/<list digest>` read as a tree list | tierstore, kbf | what 1a measures the provider to add | medium |
| 2 | `SubmitJob` answers before it adds the job to the placer's tables | jobscheduler | the wait behind a pass's table read: 0 when no pass is reading, up to the read when one is | small |
| 3 | tree writes do not queue behind pack uploads for store slots | kbf | up to about 10 of the root's 13-17 ms, if item 0 shows the slot wait | small |
| 4 | one job read and no separate clock read in `SubmitJob` | jobscheduler | 2 of 11 round trips, 0.1-0.6 ms | small |
| 5 | the pass reads queued jobs in batches, not one `get` each | jobscheduler | shortens the reads behind 2 and frees pool connections | small |
| 6 | the submit transaction in fewer statements | jobscheduler | about 5 round trips (8 to 3), under 1 ms on a local socket | medium |

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
- **The server.** It is a tierstore S3 front named by `--pack-store-endpoint`: a node's tierstore, or a
  tierstore that proxies the farm's hot and cold stores (`docs/deploy/kbf-modes.md:73-76`). Its handler
  is `Server::dispatch`, then `put_streamed` (tierstore `server/ops.rs:438`, `:634-686`). It allows 32
  puts at once, then waits and answers 503 `SlowDown` (`ops.rs:595-619`). Behind it is SeaweedFS
  (`docs/design/pack-index-over-the-store.md:35`).
- **The errors are S3's.** A 503 is waited out and the write sent again, and a pooled connection the
  store had closed is tried once more (`action_tree/retry.rs:1-20`, `store_writes.rs:21-27`). Any other
  refusal fails the tree.

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
  records out (`crates/tierstore-tree/src/image.rs:1-8`). So every list in the fs's memory is a heap
  copy of its whole file (`Tree::image`, `server/treefs/tree.rs:142-155`).
- **Today only placements read it.** That is the mount provider's form (`--executor-input-tree
  provider`, and every Mac job; kbf `crates/kbf-server/src/executor/config.rs:31-42`,
  `executor/out.rs:122-131`). `trees/<digest>` always reads manifests (`placed.rs:241-306`).

So the one tree file per action, written once by kbf, needs **no new format**. What it needs is the
Linux jobs to use it, and the host to map it instead of copying it onto the heap.

### 4.3 The provider form on Linux: what is in place, and what blocks the switch

**How a Linux job would use it.** The frontend's `--executor-input-tree provider`:
- writes the action's tree list, then its L2 sidecar (`cas/l2/<id>`, `action_tree/listing.rs:381-389`);
- gives the job the layer `{provider: "tierstore", ref: <the list's key>}` (`executor/out.rs:122-131`).

The host's agent then asks its node's tierstore to `Prepare` the tree as soon as the job is assigned
(jobscheduler `crates/js-agent/src/lifecycle/machine/providing.rs`). tierstore's `Prepare`
(`server/provider/prepare.rs:140-246`):
- fills the list;
- hashes it on one thread while it walks it on another;
- writes it as the tree's image (`local_cache/trees/create.rs:392-416`);
- answers with a placement.

The fs then serves the placement from that image (`placed.rs:247-263`, `image_of`), and the list's
ranges are queued as reads ahead of the job, so its files are fetched while the sandbox is made.

**What is in place on the farm** (read only, from each node and the deploy record):

| step | what | state |
|---|---|---|
| 1 | tierstore with the mount-provider interface (komira-ai/tierstore#337) | done: five reachable nodes run tierstore 0.3.184 |
| 2 | js-agent 0.1.250 or later (komira-ai/jobscheduler#899) | done: those nodes run js-agent 0.1.401 |
| 3 | the node's tierstore serves the provider socket, and its agent is started with it | done on all six build nodes, by the deploy record; the files are root-only and were not re-read for this page |
| 4 | each agent reports the capability `mount-provider.tierstore` | not checked: the job service's list of agents needs an operator's token |
| 5 | the frontend's `executor-input-tree = "provider"` | done after this page was written: the whole frontend, not one lane; measured in [`docs/measurements/provider-input-tree-linux.md`](../measurements/provider-input-tree-linux.md) |

**Measured so far.**
- **At kbf:** the executor's `start_ms` p50 was 184 ms for a Mac job, which is always the list form
  (n = 2,229), against 124 ms for a Linux job in the manifest form (n = 9,146). That is the per-job
  join of the morning of 2026-11-25, over starts of at most 5 s. The forms differ in more than the tree,
  so 60 ms bounds what the list form adds at kbf; it does not measure it.
- **At the host:** `Prepare`'s time has not been measured on Linux, and the Macs' agent logs were not
  in the join. No Linux job had run in the provider form on the farm when this page was written;
  item 1a's measurement is in `docs/measurements/provider-input-tree-linux.md`.

**What blocks the switch, in order:**
1. **Step 4**, which an operator can read in a minute from the job service's list of agents. A node
   that lacks the capability takes no provider job at all (placement requires it,
   `docs/design/actions-as-jobs.md:515-518`). It loses capacity; it does not fail starts.
2. **kbf's list cost.**
   - The list form re-encodes the whole base for every tree (`tstree::encode`, `listing.rs:348`, over
     `BaseTree::listed`, `action_tree/base.rs:91`), with no per-directory memo.
   - It writes the sidecar only after the list (`listing.rs:381-389`), which is two serial PUTs, as
     manifests and root are today.
   - Without item 1b, the switch keeps today's write cost and adds the encode.
3. **The provider's own cost on Linux**, not measured: the fill, the hash, the image write, the walk,
   the placement, and the agent's mount. Item 1a measures it on one lane before the default moves.
4. **Memory.** Each list in memory is held twice on the heap, by the tree store and by the fs
   (section 4.6). A busy host keeps 560-760 trees in memory at once, so 0.3-7 MiB lists cost
   0.2-5.2 GiB of heap per copy. Item 1c maps them once, under a budget.

**What is safe already.** A list a host refuses is read as manifests on the next run, which spends no
attempt (`Ingest::list_refused`, `executor/outcome.rs:105-112`, `executor/front_miss.rs`). Turning the
flag back to `manifests` restores today's behaviour for the next run.

### 4.4 Size

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
| 2,800 entries | 2,800 | about 0.5-0.6 MB | about 0.23-0.30 MB |
| ten times larger | 28,000 | about 5-6 MB | about 2.3-3.0 MB |
| a 7 MB list | about 90,000 | about 16-19 MB | 7 MB |

- The list is about 0.4-0.5 times the size of the manifests; tierstore's spec says "a third" of v2
  against its v1 JSON list.
- The list repeats the base tree's entries in every action's file (`BaseTree::listed`,
  `listing.rs:7-14`). The table counts them in the entries. Below, lists are sized at 0.3 MB (the
  2,800-entry tree), 0.6-0.8 MB (the busy evening's 1,353-manifest trees, about 7,500 entries) and up
  to 7 MB. No sample of real list sizes was taken for this page.
- The limits are far away:
  - a list is limited to 256 MiB (`MAX_TREE_LIST_BYTES`, `action_tree/listing.rs:64`; `tstree.rs:330`);
  - a whole-object fill is limited to 64 MiB (`WHOLE_FILL_LIMIT`, tierstore `server/local_cache/mod.rs:288`).

### 4.5 What changes

**kbf** (item 1b):

1. **The base encoded once.** The base's records, names and keys are encoded once per frontend and
   base tree. Only the input's records are appended per tree. Today `tstree::encode`
   (`listing.rs:348`) walks the listed base (`action_tree/base.rs:91`) every time, and the list form
   has only the `(action, form)` memo of 600 s
   (`executor/front.rs:784-785`).
2. **The sidecar beside the list.** The sidecar's PUT is made beside the list's, not after it.
   The tree is stored once both have ended (`write_listing_as` awaits both, as now,
   `listing.rs:370-392`). The provider needs the sidecar, so it stays before the submit.
   - Today no sidecar and no unlisted row names a list the store does not have (`listing.rs:28-35`,
     `sidecar.rs:10-18`). The new rule keeps that for the pack index's unlisted row, which is still
     written only after the list. A sidecar may now be stored before its list for as long as one PUT
     takes.
   - A list whose PUT fails leaves its sidecar behind. No job was given that list. The sidecar is
     named by the list's id, so a later write of the same list makes it valid again; otherwise it is
     an object nothing names, as a pack whose record failed is today. The
     two module docs change with the code (P10).
   - A sidecar's failure still never fails the list (`listing.rs:404-411`).
3. **The order is unchanged.** Blobs, then the list and its sidecar, then `SubmitJob`. The guard
   `every_attempt_submits_its_job_only_once_its_tree_is_stored` (`executor/run_tree_tests.rs:38`)
   covers the provider form too.

**tierstore** (item 1c): mapped tree lists under a budget (section 4.6).
- `image_of` (`placed.rs:306-316`) maps the image file instead of `read_image` (`placed.rs:92`).
- `TreeImage` reads its records from a mapping through a record reader over `vm-memory`, which copies
  each record out with no `unsafe`, as `tierstore-tree` does (`crates/tierstore-tree/src/image.rs:95-135`).
  The heap backing stays for tests and for a host that cannot map.
- `TreeImage::checked` still walks the whole list once at attach, so a damaged list is refused there,
  not in a running job.
- **No second hash.** `Prepare` refuses a list unless its hash is the key's (`prepare.rs`, `image_v2`),
  and the whole-object fill does the same (`server/local_cache/fill.rs:807-852`). The image is not
  hashed again at attach (the doc of `image_of`).

**Only if item 1a shows the provider's own cost** (item 1d): the host path. `read_tree`
(`placed.rs:241`) would take a root whose bytes begin with `TSTREE` as a tree list, with no `fill-tree`
and no `flatten_in_waves`. kbf would get a third form, `--executor-input-tree list`, whose layer is
`<mp>/trees/<sha256 of the list>` and which needs no sidecar. That keeps the layer a host path and
skips `Prepare`, the placement and the agent's mount. It is a new accepted flag value and a new name
form in the fs, so it is built only if 1a shows that the provider adds a cost these remove.

**jobscheduler:** nothing.

### 4.6 The RAM budget for mapped tree lists: `fs.tree_list_ram_bytes`

**Today a list is held twice on the heap.** Both copies are in the one tierstore process that runs the
fs and the provider:
- the tree store's copy: `Prepare` hands it the `TreeImage` (`server/provider/prepare.rs:149-158`),
  which keeps the list's bytes for the placement's life (`Content::Image`,
  `local_cache/trees/state.rs:42`);
- the fs's copy: it reads the image file again into a `Vec` (`read_image`, `placed.rs:92`, `:306-316`).

Item 1c replaces both with one read-only mapping of the image file per list, shared by the store and
the fs, and budgeted once.

**The image file never changes.** It is written beside the walk and renamed into place
(`local_cache/trees/create.rs:392-416`). The cache never writes, punches or truncates it while a tree
names it, as `tierstore-tree`'s mapped images also require (`crates/tierstore-tree/src/image.rs:1-8`).
A file truncated under a mapping would raise `SIGBUS`. Anything that truncates an image in place is
out of scope, and the store's own code does not.

**The setting.**
- Name: `tree_list_ram_bytes`, an integer number of bytes in the `fs` interface's params, beside
  `tree_idle_secs`.
  - It is read in `server/config_interface.rs:125`, passed on in `server/config_fs.rs:206` and `:238`,
    and defaulted in `server/treefs/config.rs:93`.
  - The list of fs params in the config refusal's golden (`server/config_trees_tests.rs:159`) gains
    it.
- Default: 536,870,912 (512 MiB). That holds about 1,700 lists of 0.3 MiB or 73 of 7 MiB, enough for
  a workstation or a small host.
- A farm's busy build hosts set it to 2 GiB (`tree_list_ram_bytes = 2147483648`).
- A value under one page (4,096) is refused at start, by name, as other params are.

**What it budgets.**
- It budgets the mapped bytes of the lists in memory: each list's file length, rounded up to the
  page, counted once however many of the store and the fs use it.
- It counts lists only. A manifest tree's heap index is counted as now (`heap_bytes`,
  `core/memory.rs:84`, `:397`).

**Resident or budgeted.**
- The pages are budgeted, not locked. A list is mapped read-only and private. The attach's shape walk
  reads every page once, so a list is resident when its first lookup is answered. No `MAP_POPULATE` is
  needed, and no `mlock`: locking needs a privilege or a raised `RLIMIT_MEMLOCK` that the `tierstore`
  user does not have.
- Under memory pressure the kernel may drop a mapped page. It re-reads it from the image file on the
  node's disk at the next touch. That costs a page fault, never a wrong answer.
- A list the local cache evicts while it is mapped keeps its file open until the mapping is released.
  The budget counts it until then.

**What "held" means.** The code already has two kinds of attachment (`server/treefs/attach.rs:153-160`):
- a **run attachment** (`has_run`): a run attached the tree through the control socket;
- an **implicit attachment**: a lookup of `trees/<digest>` attached it (`core/implicit.rs:140-165`).
  It ends once no request has named the tree for `fs.tree_idle_secs` (900 s by default).

The idle sweep drops a tree only when it has neither, and no request has named it for that long
(`Core::drop_idle`, `core/memory.rs:417-441`). It does this under the lifecycle lock, and frees the tree
after the lock is released. Whether a provider placement's tree is run-attached or only implicitly
attached was not established for this page. The rule below holds either way.

**At the cap.** Before a new list is mapped, the fs releases mappings until the new one fits:
- **Which lists.** A list is held while it has a run attachment or a request in flight on it. Lists
  with only an implicit attachment are released, the least recently used first: the one no request
  has named for longest, by the `used` stamp the idle sweep reads.
- **How.** A release takes the same steps as the sweep, under the same lock: it ends the implicit
  attachment, drops the tree from memory and releases its mapping. In effect it is the sweep with a
  shorter idle bound, the shortest that frees enough.
- **Releasing is not ending.** Every inode of a tree can be derived from its number (`ino.rs`). A
  request that names a dropped tree's inode reads the tree again and is answered as before, as after
  the idle sweep:
  - a placement's tree through `fills.image` while the placement lives (`placed.rs:246-263`); a
    running job's placement lives until its job ends;
  - a placement that has ended is `ENOENT`, as after the sweep (`core/memory.rs:27-30`).

  The cost is a re-read from the node's disk and a shape walk: under 1 ms for a typical list, a few ms
  for a large one (section 4.7).
- **A mount never fails for the cap.** If every mapped list is held and the new one still does not
  fit, it is mapped anyway, past the cap.
  - It is counted (`tierstore_fs_tree_list_over_cap_total`) and logged as a rate-limited warning:
    `tree list <digest> mapped past fs.tree_list_ram_bytes: <bytes> of <cap> mapped, every mapped list held`.
  - The next release a detach allows brings the total back under the cap.
- **The idle sweep is unchanged.** The cap only makes lists with no run and no request in flight
  leave earlier when memory is short.

**Observed** (on the fs's report, `server/treefs`):

| metric | kind | what |
|---|---|---|
| `tierstore_fs_tree_lists_mapped` | gauge | lists mapped now |
| `tierstore_fs_tree_list_mapped_bytes` | gauge | their bytes, as budgeted |
| `tierstore_fs_tree_list_ram_bytes` | gauge | the setting |
| `tierstore_fs_tree_list_releases_total{why="cap"\|"idle"}` | counter | mappings released, and why |
| `tierstore_fs_tree_list_over_cap_total` | counter | lists mapped past the cap |

The idle sweep's info line (`say_memory` and `memory_line`, `core/memory.rs:403`, `:441`) gains:
`tree lists: <n> mapped, <MiB> of <cap MiB>; <k> released for the cap and <i> idle since the last sweep`.

**Checked against the working set.** Sizes in this part are in MiB.
- **Trees in memory at once.** On the busy evening of 2026-11-25, the most trees any node attached
  within 960 s was 560-702, depending on the node (measured: tierstore's per-attach line, 18:20-21:10Z,
  five nodes). A busier window reached about 760. With the 900 s idle sweep, that is about how many
  trees a node keeps in memory.
- **Their bytes.** The lists are sized at 0.3 MiB (the 2,800-entry tree of 4.4) to 7 MiB, and about
  0.6-0.8 MiB for the busy evening's 1,353-manifest trees. 560-760 lists at 0.3-7 MiB each is
  0.2-5.2 GiB.
  - 2 GiB holds the whole window while the mean list is under about 2.7 MiB (2,048 MiB / 760).
  - Above that mean, the cap releases implicitly attached lists before 900 s. That costs a re-read
    only if a request names one of them again.
- **Held lists.** Only lists with a run attachment or a request in flight are held. 2 GiB is passed
  only if those add up to more than 2 GiB: for example more than 292 such lists of 7 MiB, or more than
  3,400 of 0.6 MiB. How many jobs a host runs at once was not measured; it is bounded by its cores,
  not by its tree count.

**Tests that fail before.** These run on the fs's rigs, on the core's injected clock. The clock is
advanced between uses, so the use stamps never tie.
- `a_list_past_the_cap_releases_the_least_recently_used_list_with_no_run_and_no_request`;
- `a_list_a_run_attached_is_never_released_for_the_cap`;
- `a_list_with_a_request_in_flight_is_never_released_for_the_cap`;
- `a_mount_past_the_cap_with_every_list_held_still_attaches_and_is_counted`;
- `a_released_list_is_read_again_by_the_next_request_under_its_tree_number`;
- `a_released_placement_tree_whose_placement_ended_is_enoent`;
- `the_store_and_the_fs_share_one_mapping_of_a_list_and_it_is_budgeted_once`;
- `a_mapped_list_answers_entry_child_children_mode_and_links_as_the_heap_image_does`, on a large
  generated tree;
- `bytes_damaged_after_the_map_read_as_absent_not_a_panic`;
- config: `tree_list_ram_bytes_defaults_to_512_mib`, `tree_list_ram_bytes_reads_2_gib`,
  `tree_list_ram_bytes_under_a_page_is_refused_by_name`, and the fs-params golden;
- the report: each metric moves as the cases above say.

### 4.7 Expected attach and write (inferred)

Attach of a list not yet on the node, which is every attach: a list is unique to its action, and 3,919
of 3,925 morning attaches were the first of their digest on their node (measured). The sum below is for
the list itself; the provider form adds what `Prepare`, the placement and the agent's mount cost, which
item 1a measures.

| term | basis | 0.3 MB | 3 MB | 7 MB |
|---|---|---|---|---|
| one whole-object fill: a round trip to the store | the frontend's own GET p50 to the store, 3.9 ms (measured, all sizes; the node's fill path was not timed) | 4 | 4 | 4 |
| transfer at 1-10 Gbit/s | not measured | 0.3-2.4 | 2.4-24 | 5.6-56 |
| SHA-256 during the fill, on a node without SHA extensions | 5.5 ms/MB (measured, tierstore `tree-manifest.md`) | 1.7 | 17 | 39 |
| the image written and mapped, then the shape walk | 0.76 ms/MB walk (measured, same doc) | 0.3 | 2.5 | 5.5 |
| tree number and keep | | under 1 | under 1 | under 1 |
| **attach** | | **about 7-10 ms** | **about 25-50 ms** | **about 55-105 ms** |

- **Today:** 76 ms at the p50 on the light morning, for trees of 372 manifests, and 215 ms on the busy
  evening, for trees of 1,353 manifests (measured). The evening's trees are about 7,500 entries, a list
  of about 0.6-0.8 MB, so about 10-15 ms by the same sum (inferred).
- **The saving** is about 65 ms at the p50 on the light morning and about 200 ms on the busy evening,
  comparing like sizes, less what the provider form adds. Both assume that host load slows the list's
  few steps less than it slows parsing hundreds of manifests. Item 1a's measurement checks that.
- **Very large trees.** A 7 MB list (about 90,000 entries) is about 55-105 ms. The manifest path of a
  tree that size was not measured on the farm; the workstation's 52 ms for 31,000 entries, times the
  seven of 4.1, suggests several hundred ms (inferred).
- **The hash dominates a large list.** It is already paid today, once per manifest, inside
  `parse_manifest`.
- **The local work** on a workstation (scratch, measured, `sha2` with SHA extensions): read, hash and
  walk of a 0.22 MB list took 0.10 ms, against 4.0 ms for the same 2,800 entries as manifests. For
  31,000 entries it was 1.1 ms against 52 ms.
- **What mapping saves.** Against reading the file whole, mapping saved 3 µs on the 0.22 MB list and
  83 µs on the 2.4 MB list (scratch, measured), about 0.25 ms at 7 MB (inferred). The time gain is one
  object and no parse. Mapping is for memory: it replaces a heap copy of every list in memory with
  pages the kernel may drop and re-read, under the budget of 4.6.

The write, on kbf's side:
- **Today** it is two serial stages, 35-48 ms at the p50 (`tree_manifests` + `tree_root`, measured).
- **With the list and its sidecar beside it** (item 1b) it is one round of PUTs. Take it as the root's
  lone PUT (13-17 ms) plus the body: 0.3 MB at 1-10 Gbit/s is 0.3-2.4 ms. So about 15-20 ms, a saving
  of about 20-30 ms (inferred).
- **This holds only if the encode is cheap** (item 1b), and less if item 3 removes the slot wait first.

### 4.8 What is lost

- **Per-directory dedup across trees.**
  - Today a directory unchanged since another tree is written once (kbf's memo) and fetched once per
    node (98% of manifests came from the node's cache, measured).
  - With one file per action, every start writes and fetches its whole list: 0.3-7 MB where today it
    is about 14 small manifests.
- **Why it does not matter at 0.3-7 MB.**
  - **Time.** One fill of 0.6 MB costs one round trip and under 5 ms of transfer at 1 Gbit/s. The 14
    fills it replaces cost a round trip per level, on top of reading every cached manifest. Even a
    7 MB list's whole attach (55-105 ms, 4.7) is under what the busy evening's attach takes today for
    trees whose list would be about 0.7 MB (215 ms at the p50).
  - **Memory.** The node's cache is no longer shared between trees. But the budget of 4.6 bounds what
    the mapped lists hold, and the manifest trees' heap index (about 100 bytes and an allocation or two per
    entry) is no longer built.
  - **Bytes (inferred).** At the morning's rate of about 5,600 starts an hour (12,974 Executes in
    2 h 19 min, measured) and 0.6-7 MB a list, that is about 3.4-39 GB an hour (1.7 GB at 0.3 MB), written once and read
    once. That is small against a node's network link: one 10 Gbit/s link moves 4.5 TB an hour.
- **What it does need: a retention rule.** Nothing deleted under `cas/l/` before item 1e
  (`docs/design/actions-as-jobs.md:379`). Manifests could do without because they dedup; lists cannot.
  - A list is needed while its jobs can still start: the 600 s memo plus resubmits. A collector that
    removes lists older than a day, which no job names, is enough.
  - It is item 1e. It must land before the provider form is the default on every frontend.
- **The base tree is repeated in every list.** A shared base file plus a per-action delta would win
  back those bytes. But that is two objects, a new format (B8) and an ordering between them, so this
  page does not propose it.

### 4.9 Rollout and older releases

- **B8.** The tree list v2 does not change. Its golden (tierstore `tests/stored_formats/tree_list_v2.rs`)
  must stay byte-identical, and that test is the evidence. The mapped backing reads the same bytes.
- **The provider form is already in production** for every Mac job, and the Linux hosts have the
  provider configured (4.3). Moving Linux to it changes no format and no interface.
- **The order of adoption.**
  1. Item 0's timers.
  2. An operator checks step 4 of 4.3.
  3. Item 1a: `executor-input-tree = "provider"` on one frontend lane, measured against the other, with
     the hosts' tierstore memory watched. This needs no code, so it comes first and sizes the rest.
  4. Items 1b and 1c, in either order, each measured on that lane.
  5. Item 1d only if step 3 or 4 shows a cost of the provider that it removes.
  6. Item 1e, the collector.
  7. Then the default.

  Turning the flag back to `manifests` restores today's behaviour for the next run.
- **Failure cases of the provider form on Linux:**

| case | what happens |
|---|---|
| the list or sidecar write fails | no job is submitted, as today (`run.rs:452-454`) |
| a blob is lost before the list is written | MISSING, as today |
| a host without the capability | the job is never placed there; other hosts take it |
| a host's tierstore refuses the list (`tree list not supported`, `ListVersionUnsupported`) | the next run reads the tree as manifests, and no attempt is spent (`outcome.rs:105-112`) |
| the list is absent at `Prepare` | `no tree list at <key>` (`prepare.rs:265`), refused as above |
| the list is damaged (its hash is wrong) | `Prepare` refuses it (`image_v2`) |
| a mapped page is dropped under memory pressure | it is read again from the image file at the next touch |
| the budget is full and every list is held | the list is mapped past the cap, counted and logged (4.6) |
| a file's blob is lost after the attach | the running job's read fails as it does today with manifests |

## 5. What this does not change

- **The order.** Blobs, then the tree's last object, then `SubmitJob`. The guard test stays.
- **The Mac form.** Macs keep the mount provider. Items 1b and 1c make their lists cheaper too.
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

**1a. The provider form on one Linux lane** (configuration; an operator's step)
- **Change:** step 4 of 4.3 checked, then `executor-input-tree = "provider"` on one frontend lane. No
  code. It can come first, as the measurement that sizes 1b-1d.
- **Saves:** attach about 65 ms (light) to 200 ms (busy), less what `Prepare`, the placement and the
  agent's mount add, which nothing has measured on Linux. The tree write: 35-48 ms today against about
  15-20 ms for the list and its sidecar at once, so 15-33 ms, once 1b is in; before 1b, the sidecar's
  serial PUT keeps two stages (p50, inferred).
- **Fails before:** not a code change, so no new test. What it relies on holds today and keeps holding:
  - the miss path's tests (`executor/front_miss.rs`): a refused list is read as manifests next and
    spends no attempt;
  - the order guard `every_attempt_submits_its_job_only_once_its_tree_is_stored` in the provider form.
- **Measured after,** one lane against the other, 30 minutes light and 30 of burst:
  - the agent's `Started` line from `taken` to `resolved`, and tierstore's `Prepare` time;
  - tierstore's resident memory on the lane's hosts;
  - kbf's `tree_list`, `tree_sidecar` and phase `start`;
  - `list-refused` and `input-unavailable` by attempt;
  - the fs's resident memory.

**1b. kbf: the base encoded once, and the sidecar beside the list** (4.5)
- **Saves:** the base encode and the sidecar's serial stage (`tree_sidecar`, which item 0 reads).
  Together they are at most the 60 ms the list form adds at kbf at the p50 (measured as a bound).
- **Fails before:**
  - `the_base_is_encoded_once_for_many_trees`: an encoder that counts the base records it encodes,
    over two trees of one base;
  - `the_sidecar_put_is_in_flight_beside_the_lists_put`: a store whose two PUTs are gated by explicit
    events. Both must be in flight before either is released, and `write_listing` must return only once
    both have ended. Today the sidecar starts after the list's PUT returns (`listing.rs:381-389`);
  - `an_unlisted_row_is_recorded_only_after_the_list_is_stored`, which holds today and keeps holding;
  - `a_list_whose_put_fails_fails_the_tree_and_no_job_is_submitted`.
- **Measured after:** `tree_list` and `tree_sidecar` against 1a's numbers.

**1c. tierstore: mapped tree lists under `fs.tree_list_ram_bytes`** (4.5, 4.6)
- **Saves:** the two heap copies of every list in memory, the tree store's and the fs's (each
  0.2-5.2 GiB on a busy host at 0.3-7 MiB a list, inferred), and 0.003-0.25 ms per attach.
- **Fails before:** the tests of 4.6.
- **Holds today and keeps holding:** the v2 golden, and every placement-image test with the heap
  backing.
- **Measured after:** the metrics of 4.6 on a busy host, and the attach's time by list size.

**1d. Only if 1a shows the provider's own cost: `trees/<list digest>` read as a tree list** (4.5)
- **Saves:** what 1a measures `Prepare`, the placement, the agent's mount and the sidecar to cost.
- **Fails before**, tierstore (rigs of `core/placed_image_tests.rs` and `core/tree_fill_tests.rs`):
  - `a_tree_list_at_trees_digest_is_attached_from_one_fill`: one whole fill, no `fill-tree`, no
    manifest read, and the entries by index equal the image's. Today it is `EIO`, because
    `parse_manifest` refuses the list;
  - `a_tree_list_that_fails_its_shape_walk_is_eio_and_takes_no_number`;
  - `fill_tree_stops_at_a_root_that_is_a_tree_list`.
- **Across releases (B8),** both directions:
  - the new reader on old data: `a_manifest_root_at_trees_digest_takes_the_manifest_path`;
  - the old reader on new data: `a_tree_list_given_to_the_manifest_reader_is_refused_not_misread`.
    It runs on today's `read_tree`, asserts `EIO` and no tree number, and pins what an older host does
    with the new name form;
  - kbf: `a_list_attempt_that_fails_input_unavailable_runs_its_next_attempt_as_manifests`. An older
    fs answers `EIO`, which reaches kbf only as `input-unavailable`, so this fallback spends one
    attempt of `RESUBMITS` (`outcome.rs:153`).
- **Fails before**, kbf: `the_list_form_gives_the_job_trees_slash_the_lists_digest`, and
  `the_list_form_writes_its_blobs_then_one_list_and_no_sidecar`.
- **Measured after:** as 1a, the third lane against the provider lane.

**1e. kbf: a collector for tree lists no job names** (4.8)
- **Change:** a sweep that deletes `cas/l/<id>` lists, and their `cas/l2/<id>` sidecars, older than a
  day that no operation's row names. It is dry run by default and reports a count of what it would
  delete.
- **Saves:** no time; it bounds the bytes the lists add.
- **Fails before:** `a_list_older_than_a_day_that_no_operation_names_is_deleted_with_its_sidecar`,
  `a_list_an_open_operation_names_is_kept_whatever_its_age`, and `a_dry_run_deletes_nothing_and_counts`.
- **Measured after:** lists and bytes under `cas/l/`, by day.
- **Built** (kbf `crates/kbf-server/src/frontend/list_sweep.rs`): the frontend's leader runs it
  with `--executor-list-sweep-key-file` and `--executor-list-sweep-sidecar-key-file`, a dry run
  unless `--executor-list-sweep-apply` is given. As built:
  - an operation's `exec/op/` row gains an optional `tree`, the key of the list its attempt's job
    reads; no row named the list before, and a row with no list keeps its bytes (B8);
  - the lists are found from their sidecars, `cas/l2/`, so no manifest or large blob of `cas/l/`
    is read or listed; a list with no sidecar is not collected;
  - the age is the time the store says the list was written (a HEAD), and an old list the pack
    index records as a pack is kept too;
  - the measurement is `kbf_tree_list_sweep_lists` and `kbf_tree_list_sweep_list_bytes` by age in
    whole days, counted by every pass, a dry run included.

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

### Not proposed

| change | why not |
|---|---|
| submitting before the tree is stored (komira-ai/kbf#2431) | it reorders the write and the submit |
| `synchronous_commit` off for the submit | it breaks the store's rule C6, for about 0.9 ms (section 3.4) |
| a wider manifest window (16 to 32), or a tierstore verb for many manifests in one request | item 1 removes the many manifests |
| locking mapped lists in RAM (`mlock`, `MAP_LOCKED`) | it needs a privilege the `tierstore` user does not have, and a dropped page costs only a fault (4.6) |

## 7. Decisions

- **D1. The provider form first on Linux; the host-path list form only if it is measured to pay.** The
  provider form is in production for every Mac job, and the Linux hosts have it configured (4.3). So
  moving Linux to it is configuration, and its cost can be measured on one lane before any code. The
  host-path form would skip `Prepare`, the placement and the sidecar. But it is a new name form and a
  new flag value, so it waits for a measurement that shows a cost it removes.
- **D2. The sidecar stays, beside the list.** The provider needs it before the submit. Writing it
  beside the list removes the second serial PUT and keeps the pack index's unlisted row after the list.
- **D3. No second hash at attach.** The fill checks the object against its key as it lands. A
  placement's image is not hashed again at attach either.
- **D4. Lists are mapped, under a budget, with no `unsafe`.** Mapping saves little time (83 µs at
  2.4 MB, scratch), but it replaces the two heap copies of every list in memory (the tree store's and the fs's) with one
  mapping whose pages the kernel may drop.
  The crate forbids `unsafe`, so the mapping goes through `vm-memory`, which copies each record out, as
  `tierstore-tree` does.
- **D4a. The budget's default is 512 MiB; a farm's busy hosts set 2 GiB.** The default suits a small
  host. 2 GiB holds the busiest measured 900 s of trees while lists average under about 2.7 MiB (4.6).
- **D4b. The pages are budgeted, not locked.** Locking needs a privilege the `tierstore` user lacks. The
  attach's shape walk already brings every page in once.
- **D4c. A mount never fails for the budget.** When every mapped list is held (a run attachment or a
  request in flight), the new one is mapped past the cap and counted, because a refused mount would fail a job that has a host and a
  tree.
- **D5. `synchronous_commit` stays on.** See section 3.4.
- **D6. EXPLAIN on a scratch copy with synthetic rows, not a replica.** No read-only replica of the
  job store was available. The schema is the code's own, and the row counts are twice the farm's
  measured rate kept for the retention the code sets. That gives the plans and an upper bound on
  their cost, but not the farm's disk or socket.
- **D7. A tree's "about 500 manifests" is the host's count of the whole tree.** kbf writes only the
  directories it has not stored. The page uses the attach line's counts, which are per tree and per
  node.
- **D8. (1d only) A list-form attempt that fails `input-unavailable` falls back to manifests.** The
  job service cannot place by tierstore release, so the fallback bounds what an older host costs a run
  to one attempt.

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
| the network rate for a multi-MB fill | given as a range in 4.7 |
| `Prepare`, the placement and the agent's mount on Linux | no Linux job had run in the provider form; the Macs' agent logs were not in the join. Measured since, as the agent's wait on them, in `docs/measurements/provider-input-tree-linux.md` (item 1a) |
| step 4 of 4.3, each agent's capability | the job service's list of agents needs an operator's token |
| how many jobs a host runs at once, and so how many lists are held | not in the per-attach line |
