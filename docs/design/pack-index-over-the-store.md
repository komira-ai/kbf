# The pack index over the store: a row means the object exists

Status: DESIGN (2026-11-18). Section 6 (heal on confirmed absence) is built (komira-ai/kbf#2380, komira-ai/kbf#2382);
everything else is proposed and lands in the phases of section 11. Claims are tagged **[code]** (true at the commit this
document was written against, with the file or function), **[built]** (heal, landed) or **[proposed]**. Nothing here is
measured unless it says so.

Related: [metadata-in-postgres.md](metadata-in-postgres.md) (the tables; its section 2.3 is superseded by sections 3 to 5
here), [actions-as-jobs.md](actions-as-jobs.md) section 6 (what a client sees when a pack is lost),
[trees-for-action-inputs.md](trees-for-action-inputs.md) section 5.4 (its retention paragraph is superseded by sections 3
to 5 here). The retention of [storage.md](storage.md) (`min_ttl`, `Collect`), and the documents that quote it
(`write-path.md`, `invocations.md`, `docs/guides/storage.md`, `docs/deploy/storage-node.md`), belong to the earlier
metadata store (`kbf-meta`), not to the pack index, and this design does not change them.

## 1. The decision

kbf keeps three kinds of rows about bytes in its Postgres database: where each blob is (`blob` and `pack`), what each
Directory holds (`directory`) and what each action produced (`action_result`). The bytes are objects in the store:
SeaweedFS (filer and S3 gateway, two copies of every object) behind the nodes' tierstores. The design makes one rule
true and keeps it true:

> **Invariant.** Every `blob` row names a `pack` row whose object exists in the store.

Every "present" answer about a recorded blob is made from rows: FindMissingBlobs, the action-cache closure check, the
Execute input walk and the job tree's `locate` (`Packs::missing`, `Packs::locate`) **[code]**. The routed plane also
asks storage nodes about blobs the index lacks, and the action tree asks the store about `cas/l/` objects that have no
row, but neither path reads a row. With the invariant, a "present" answer from rows is never a lie about bytes. The
invariant is kept three ways, in order of importance:

1. **By construction when kbf deletes** (sections 4 and 5). One collector is the only process that deletes a recorded
   object. It deletes the rows first, in one transaction that re-checks recent use, and the object only after a grace.
   A row therefore never outlives its object because of anything kbf did.
2. **By heal when the store loses an object anyway** (section 6, built). A pack the store answers absent for, confirmed
   by a second look some minutes later, has all its rows dropped in one transaction.
3. **By a backstop for losses nothing has read yet** (section 7, built only if measurements call for it). An
   action-cache hit that checks its packs in the store, and a reconciler that walks the rows against the store.

What the rows are:

- `pack`, `blob` and `directory` rows are **a rebuildable cache over the store**. Losing them costs uploads and runs,
  never a wrong answer. A client whose FindMissingBlobs says "missing" uploads again. An Execute whose input is missing
  is answered FAILED_PRECONDITION with MISSING violations before any job is queued (`missing_status`,
  `crates/kbf-front/src/execution.rs`) **[code]**.
- `action_result` rows are **authoritative**. They are in no object (`migrations/0003_action_cache.sql`) **[code]**, so
  they cannot be rebuilt from the store. Losing them means rebuilds, which is the safe direction. The collector never
  deletes them (section 4.5).

The design adds no new store, no new service and no second index in the filer. It adds one table, one leader-only loop,
and one flush task per frontend that writes touches with the existing `touch` statement.

## 2. Where things are today

| What | Where | Status |
|---|---|---|
| Blob location: `blob (hash, size, pack_id, off, last_touch_us)`, primary key `(hash, size)`, index `blob_pack (pack_id)`; `pack (pack_id, key UNIQUE, writer, size, created_us, blob_count, live_blobs)` | `crates/kbf-pack-index/migrations/0002_pack_index.sql` | **[code]** |
| Action result: `action_result (hash, size, result, written_us)`, upserted (last writer wins) | `0003_action_cache.sql`, `postgres/action.rs` | **[code]** |
| Directory rows: `directory (hash, size, body, written_us)` | `0004_directory.sql` | **[code]** |
| Recording: a pack is recorded only after the store acknowledged its create-only PUT; `INSERT ... ON CONFLICT (hash, size) DO NOTHING`, so the first location of a blob wins | `Packer::commit`, `postgres.rs` `record_in` | **[code]** |
| Row removal: `drop_location` (a read found bytes that are not the digest) and `forget_pack` (heal, section 6); nothing else deletes `blob` or `pack` rows, and nothing deletes `action_result` or `directory` rows | `postgres.rs` `DROP`, `FORGET` | **[code]** |
| Touch: `PackIndex::touch` sets `last_touch_us`. Its only caller is the Reader, which notes blobs it read and flushes them, inline in the read that crosses 1,000 pending, with no timer and no flush at stop. FindMissingBlobs, `locate`, the Packer's skip and action-cache hits touch nothing | `crates/kbf-pack/src/reader.rs` `note`, `flush_touches` | **[code]** |
| Object deletes: the janitor sweeps `cas/b/` only: keys with no `pack` row whose mint time (read from the key) is past a grace of 6 h (at least 5 h), re-asking the index per key under a claim. `Packer::discard` deletes a failed PUT's key | `crates/kbf-pack/src/sweep.rs`, `crates/kbf-server/src/frontend/janitor.rs` | **[code]** |
| Collection of recorded packs: none. `PackIndex::dead_packs` has no caller outside tests | | **[code]** |

Keys **[code]**:

- **Frontend packs** `cas/f/<frontend>/<boot>/<seq>`: unique per write, never written twice.
- **Task packs and put directories** `cas/b/<build>/<nonce>...`: unique per attempt. An ingest refuses a key older than
  5 h.
- **`cas/l/<sha256>`: deterministic.** The same bytes always have the same key. Large blobs are recorded under it
  (`put_large`; `record_pack` is also called by the task ingest and `put_dir`). Several writers use it with no row: the
  ingest's write-through copy of a task's large object, `large_backfill`, action-tree manifests and tree lists, and blobs
  an action tree places whole after a length check (`place.rs`). All of them write create-only and take an existing
  object as theirs (`crates/kbf-front/src/action_tree.rs`).

## 3. Use: touches on every "present" answer [proposed]

The collector decides by `last_touch_us`, so every path that keeps a blob alive must touch it. Today only a read of the
bytes does. A blob that builds ask about a thousand times a day through FindMissingBlobs and action-cache hits, but
never download, looks unused and would be collected.

### 3.1 What is touched

The touch moves from the Reader to the pack index handle. `Packs::new` wraps the index it is given once
(`Touching<I>`), and the Packer, the Reader and `Packs` itself all hold that wrapped handle **[code: they share one
`Arc<dyn PackIndex>`]**. Every digest that `find_missing`, `locate` or `directories` answers present is noted, and so
is every digest the Reader reads. That covers every "present" answer the frontend makes from rows:

| Caller | Index call | Blobs touched |
|---|---|---|
| A client's FindMissingBlobs | `find_missing` | every present blob asked about |
| The Execute input walk (`input_walk.rs`, before the job is queued) | `directories`, `find_missing` | the Directories with a row, every input file |
| The action-cache closure check (`closed.rs` `lacking`, on a client's GetActionResult and on Execute's own lookup) | `find_missing` (and reads of the Tree blobs) | every output file, stdout, stderr, each output Tree blob and every file inside it |
| The executor's `require_held` (`prepare_run/closure.rs`) | `find_missing` | the action's files and Command |
| The Packer's skip of a blob that has a row (`packer.rs` `missing`) | `find_missing` | the skipped blob |
| `put_large`'s early answer for a recorded blob (`large.rs` `locate_stored`), a job tree's spans (`Packs::locate`) | `locate` | the blob |
| ByteStream and BatchReadBlobs (`Reader::read`) | `locate` | the blob |

Two things are not touched:

- **Directory blobs served from the executor's in-process `DirectoryCache`.** These never reach the index. A Directory
  used only that way can be retired while still in use. The cost is benign: the next Execute's walk reads it from its
  blob, finds it missing, and answers MISSING, and the client uploads it again (it built it, so it has it).
- **The Action and Command blobs of an action-cache hit.** A hit uploads no Action (`docs/design/ui-data.md`), so a hit
  does not keep them alive. Execute touches them through `require_held` and its reads. When they are collected and the
  action runs again, the client uploads them again.

### 3.2 How a touch is written

Batched, asynchronous and measured:

- **A note is an insert into an in-memory set** (`BTreeSet`, deduplicating) and nothing else: no I/O and no await on
  the request. Today's inline flush in `Reader::note` goes. No added round trip on Execute to enqueue.
- **One background task per frontend flushes the set** every 60 s (`TOUCH_FLUSH_EVERY`), or sooner once it holds
  10,000 blobs, in statements of 1,000 (`MAX_BATCH`). A flush that fails puts back the blobs it had not written, logs,
  and counts. A crash loses at most one interval of touches.
- **A touch rewrites only stale rows.** The statement updates a row only when its `last_touch_us` is more than an hour
  old (`TOUCH_QUANTUM`), so a hot blob is written at most once an hour, not once per use. It returns two counts: the
  rows that exist and the rows it updated. A touch is a heap-only update, because `last_touch_us` is in no index
  **[code]**.
- **Counters:**
  - `kbf_pack_touch_rows_total`: rows updated.
  - `kbf_pack_touch_late_total`: noted digests with no row at the flush. Every note was a "present" answer, so these are
    blobs whose rows went (retired, forgotten or dropped) between the answer and the flush.
  - `kbf_pack_touch_flush_failures_total`.

A note therefore reaches its row within the flush interval plus one flush. A row touched within the quantum is not
rewritten, so its recorded time can trail the last use by up to an hour. Section 4.3 bounds the retention with both.

### 3.3 Why asynchronous, and what that leaves open

A synchronous touch (the `MISSING` statement also updating stale rows) would close the window of section 4.4 at no
added round trip. It would also turn every FindMissingBlobs and every closure check on the Execute path into a write
statement, at a cost nobody has measured. This design keeps the touch asynchronous and measures the window it leaves
with `kbf_pack_touch_late_total`. If that counter shows the window being hit, the synchronous touch of stale rows is the
fix.

## 4. The collector [proposed]

### 4.1 Shape

A loop on the leader frontend only (the janitor's `is_leader`). It is off unless `--collect-retention-days` is given.
Each pass, every hour:

1. **Select** cold packs, oldest first, at most 1,000 per pass:

   ```sql
   SELECT p.key FROM pack p
   WHERE p.created_us < $cutoff
     AND p.pack_id > $after AND p.pack_id <= $after + 10000
     AND NOT EXISTS (SELECT 1 FROM blob b
                     WHERE b.pack_id = p.pack_id AND b.last_touch_us >= $cutoff)
   ORDER BY p.pack_id LIMIT $n
   ```

   `$cutoff` is the index clock minus the retention. `$after` is a cursor kept in memory. Each pass examines the next
   10,000 pack ids, whether or not any is cold, and moves the cursor to the end of that window; past the last pack it
   restarts from zero. A pass therefore reads the rows of at most 10,000 packs, never the whole table, and a farm whose
   old packs are mostly hot still walks forward. A pack whose rows were all dropped (`drop_location`) is cold too, once old enough. Selection only
   reads; the decision is the retire.

2. **Retire** each selected pack in its own transaction (`PackIndex::retire_pack(key, cutoff)`, 4.2).

3. **Delete** the objects of packs retired more than the grace ago (section 5).

The collector works on whole packs. A pack lives while any of its blobs is used, so a hot blob keeps its cold
neighbours. Rewriting live blobs into new packs (compaction) is not in this design. Its cost is space, never
correctness.

### 4.2 Retire: rows first, in one transaction that re-checks

`retire_pack(key, cutoff)` is one transaction:

1. **Lock the `pack` row** `FOR UPDATE`. If there is none, the answer is `NotRecorded`. If its `created_us` is at or
   after the cutoff, the answer is `Young`.
2. **Lock its `blob` rows** `FOR UPDATE` in `(hash, size)` order. That is the order `TOUCH` locks them in, so the two
   cannot deadlock. A touch committed before this step is seen. A touch that comes after waits for this transaction and
   then finds no row, and is counted late.
3. **Re-check.** If any locked row has `last_touch_us >= cutoff`, roll back. The answer is `Touched`, and the pack stays.
4. **Delete, then queue.** Delete the `directory` rows of its blobs' digests, its `blob` rows and its `pack` row. Insert
   `retired (key, retired_us)`, or refresh `retired_us` if a row for the key is already there. Commit. The answer is
   `Retired`.

From that commit, the pack's blobs are gone for every reader:

- FindMissingBlobs reports every blob of the pack missing.
- `locate` cannot name the pack.
- An action-cache entry naming one of its blobs is a miss.
- The Execute walk answers MISSING.

No reader can be told "present" about the pack from here on. The object itself still exists until section 5 deletes
it, so a reader that located the pack before the commit still gets its bytes.

The `directory` rows go with their blobs, as metadata-in-postgres.md 2.3 proposed. A `directory` row whose blob is gone
is still a correct answer for its digest. Deleting it keeps the walk from treating an input root as whole when its
Directory blob is gone, and keeps the table from growing without bound.

New table (migration 5, expand only):

```sql
CREATE TABLE retired (
    key text PRIMARY KEY,         -- the object's key, as pack.key was
    retired_us bigint NOT NULL);  -- the index clock at the retire
```

The janitor's per-key re-ask, `unreferenced_keys`, also leaves out keys that have a `retired` row. The janitor
therefore never deletes a retired `cas/b/` pack before the collector's grace; the collector deletes it, like any other
retired key. Recorded `cas/b/` packs are task outputs, most of the bytes, so the collector cannot skip them.

### 4.3 Retention

`--collect-retention-days` has no default: no flag, no collection. A value under 2 days is refused at start. The value
proposed for the farm is 7 days, close to the 8 days the row-count estimates of metadata-snapshots.md 7.2 assume.

The bound the floor keeps:

> retention > the longest build + the touch flush interval (60 s) + the touch quantum (1 h)

A build that started using a blob keeps it used. Its FindMissingBlobs, walks, hits and reads each note the blob, and
each note reaches the row within the interval, or the row was fresh within the quantum anyway. A blob is retired only
when nothing used it for a whole retention, which is longer than any build. The floor of 2 days allows a day for the
longest build and a day of margin. No build at the farm is known to run longer; if one does, the floor goes up.

### 4.4 The races, and what each costs

| Race | Outcome | Why |
|---|---|---|
| A touch and a retire of the same pack | Either the touch commits first and the retire answers `Touched`, or the retire commits first and the touch finds no row | Both lock the blob rows in one order (4.2 step 2) |
| A new record of a blob whose row is in a pack being retired (a task's ingest, a put directory) | Either the record commits first and the retire answers `Touched`, or the retire commits first and the record inserts a new row pointing at the new pack | Phase 3 changes the conflict of `INSERT_BLOBS` from `DO NOTHING` to `DO UPDATE SET last_touch_us` (the existing row keeps its `pack_id`, so the first location still wins; `live_blobs` still counts only rows inserted). The record then locks the row it meets, so it is ordered against the retire's lock of step 2. With `DO NOTHING` a record that meets the row before the retire deletes it would leave the blob with no row, though a pack just recorded holds it |
| A reader located the pack just before the retire | It reads the bytes | The object outlives the rows by the grace (section 5) |
| **The window: a blob unused for a whole retention is answered present (FindMissingBlobs, a Packer skip, an action-cache hit) while its note is still queued, and the retire commits before the flush** | Execute naming it: MISSING before any job is queued, so the client uploads it again (a source) or runs its producer again (an output; whether Buck2 does, when it holds the digest, is section 8 step 10's question, answered by phase 2). An action-cache hit already answered: a download of the output answers NOT_FOUND and that build fails; the next lookup is a miss and the action runs again. An ingest that skipped it: `checked` refuses the result, and the run is answered as failed infra and runs again | The touch is asynchronous (3.3). The window is one flush interval, for a pack nobody used for the retention. `kbf_pack_touch_late_total` counts each occurrence |
| Two frontends record and touch while the leader collects | As above | Touches and records are per-row transactions; the collector is one process |

**What the design promises, exactly.** A stale hit here means a hit, or a "present" answer, made from rows that name an
object that is already deleted.


- There is never a stale hit. A "present" answer and an action-cache hit are true when they are made: the rows they read
  name objects that exist. That is the invariant, and no race in this table breaks it.
- The window is the one way a collected blob can fail a build that was already using it. In it, a hit is true when made
  and its output is retired within the next 60 s.
- Nothing the collector does ends in a failed mount. An input it took is caught by the Execute walk or by the
  executor's `require_held`, both of which run before a job.

### 4.5 What the collector does not delete

- **`action_result` rows.** They are authoritative. A row whose closure was collected is a miss through the closure
  check (`closed.rs` `served`) **[code]**, with no reverse index from blob to action needed. The rebuild replaces it
  (`put_action` upserts). Removing rows that can only miss would be a separate change, and cannot affect correctness.
- **`cas/l/` objects with no `pack` row** (action-tree manifests, tree lists, placed and backfilled copies). The
  collector deletes only keys it retired, and section 5.2 keeps those of a retired key that something used again.
  Retention of the rest is not in this design; a lost one is rewritten by the checked tree write that heal already
  does (`TreeWriter::lost_manifest`) **[built]**.
- **Orphans**, objects no row ever named (a PUT whose record failed). Under `cas/b/` the janitor deletes them as today.
  Elsewhere they are a leak, never a wrong answer.

## 5. Deleting the object after the grace [proposed]

### 5.1 Why a grace

Rows are gone at the retire, but a reader may hold a location it took earlier:

- a job tree list or manifest naming a span of the pack, written before the retire, for a job still queued or running;
- the action-tree memo and place memo, and the executor's remembered trees (10 min, `MEMO_TTL`);
- a client between an answer and its download (4.4).

The grace is the janitor's (`DEFAULT_GRACE`, 6 h, at least 5 h, `--task-janitor-grace-secs`), which already bounds how
long a key a job was handed stays usable. It is longer than the 10 min memos and than any job's queue and run time. If a
job timeout above 6 h is ever allowed, the grace follows it.

### 5.2 The delete, and the deterministic key

For each `retired` row whose `retired_us` is older than the grace, oldest first, one transaction:

1. Lock the `retired` row, re-checking its age in the same statement
   (`SELECT ... WHERE key = $1 AND retired_us < $grace_cutoff FOR UPDATE`). If it is gone, something used the key
   again (below); if it is younger, a later retire refreshed it. Either way, skip.
2. If a `pack` row has this key, the object was recorded again. Delete the `retired` row, keep the object, and commit.
3. Check leadership, as the sweeper does before each delete. Then delete the object in the store. NotFound counts as
   success. Any other error rolls back: the row stays, the next pass tries again, and the failure is counted.
4. Delete the `retired` row. Commit.

The transaction holds one connection across one store call, bounded by the store wait of komira-ai/kbf#2379 (60 s). The
deleter is one sequential loop, so it never holds more than one.

**The race this order closes.** A blob's `cas/l/<sha>` pack is retired, and during the grace something uses the same
key again. Two cases:

- **A re-upload.** `put_large` finds no row, asks the store for the length, finds the object whole, skips the upload and
  records a new row **[code]** (`large.rs`).
- **A tree.** The action tree's `place` finds no row, asks the store for the length, and names the object whole in a
  manifest **[code]** (`place.rs`). The write-through copy, `large_backfill` and the base tree's publish
  (`base_publish.rs` `put_new`, whose tree every process reads for its whole life) do the same.

If the deleter deletes the object after that length check, the new row, or the new manifest, names a deleted object.
That is the original stale-row failure, or a failed mount.

**The claim is the `retired` row itself.** Anything that writes a `cas/l/` key, or relies on an existing `cas/l/`
object, first deletes that key's `retired` row (`PackIndex::claim_large(keys)`: `DELETE FROM retired WHERE key = ANY($1)`,
committed). Only after that does it ask the store for the length. Both orders are safe:

- **The deleter locked the row first.** The claim's `DELETE` waits for the deleter's commit, finds nothing, and
  returns. The object is gone, so the length check after it answers absent, and the user writes the object again (a
  re-upload) or reads the bytes from the CAS and writes them (a tree).
- **The claim committed first.** The deleter's step 1 finds no row and keeps the object.

The claim needs no lock of its own and holds no transaction across a store call. It is one statement per batch of keys:
`put_large`, the write-through copy, `large_backfill` and the base publish make it per key, the tree's `place` once per
batch of keys it is about to ask the store about, and the manifest writes once per batch of manifests. Writers claim
too: a deleter whose store delete succeeded and whose commit did not leaves its `retired` row, and its next pass would
delete an object a writer had put back unclaimed.

A claimed key is no longer retired, so its object is kept, now with no row, as the other `cas/l/` objects with no row
are (4.5). That is the safe direction.

A new generation of the key, the other way to avoid the race, is rejected. A `cas/l/` key is the digest. Tierstore
fetches manifests, and files a manifest names whole, by that key, so a second key for the same bytes would change the
tree format.

`cas/f/` keys are never written twice, and an ingest refuses a `cas/b/` key older than 5 h. A retired key under either
prefix is therefore never used again, and needs no claim.

### 5.3 The store key

The deleter needs delete rights on `cas/f/`, `cas/l/`, `cas/b/` and the legacy `cas/d/`. The janitor's key is limited
to `cas/b/` and stays so. The collector gets its own key, `--collect-key-file`: delete only, with no listing, since the
deleter works from `retired` and not from a listing. It is used on the leader frontend only. The operator adds the key
to tierstore's GC key list (tierstore's `server.gc_key_files`, in tierstore's configuration, not this repository).
Without the key the collector still retires rows and counts the deletes it cannot make: storage leaks, correctness
holds.

### 5.4 Counters

- `kbf_collect_packs_total{answer=retired|touched|young|not_recorded|not_migrated}`
- `kbf_collect_objects_deleted_total`
- `kbf_collect_delete_failures_total`
- `kbf_collect_retired_waiting`: rows in `retired`.

Other detail (rows deleted, bytes, claims) goes to the pass's log line.

## 6. Heal on confirmed absence [built]

kbf does not own the store. An object can vanish without the collector: a manual delete, a bucket change, a lost
volume. Heal repairs the rows when kbf sees that. It is komira-ai/kbf#2380 and komira-ai/kbf#2382. The policy is
`LossGuard` in `crates/kbf-pack/src/lost.rs`; the drop is `PackIndex::forget_pack`.

- **What counts as absence:** the store answers NotFound or InvalidRange (a range past the end), or a read returns
  fewer bytes than the blob's span (`is_absent` and `carve` in `reader.rs`, `large.rs`). A short read is only a trigger,
  like the other two: it is noted and confirmed by the same second look. A 5xx, a timeout or a reset connection is never
  an absence.
- **Confirmed by a delayed second look:** a pack's first absence is only noted. An absence at least 10 min
  (`CONFIRM_AFTER`) and at most 1 h (`CONFIRM_WITHIN`) after the first asks the store for the object's length once.
  Only an object still absent or short is forgotten. A read that gets the pack's bytes through the noted end clears
  the note, so a store whose volume server restarts while the other copy serves every object loses no pack.
- **Never a young pack:** `forget_pack(key, MIN_AGE)` compares the pack's `created_us` with the index clock inside its
  own transaction (1 h). A pack recorded again just now is `Young` and kept.
- **Once per key per hour** (`FORGET_INTERVAL`), because a `cas/l/` key is recorded again under the same name after a
  re-upload.
- **The whole pack:** every `blob` row of the pack and its `pack` row, in one transaction.
- **Memos before the retry:** a job that failed to start on tierstore's `source missing: <key>` or
  `cannot fetch manifest <sha>: upstream 404` is infra and runs again. Before the retry, the executor drops the action's
  remembered trees and the memo entries and places naming that object (`Front::lost`, `TreeWriter::lost_pack`,
  `lost_manifest`), and writes the next tree checked against the store. A confirmed loss is answered
  FAILED_PRECONDITION with MISSING violations, so the retry does not loop on the dead pack.
- **A second copy:** a blob whose row was in the forgotten pack, and whose bytes are also in a pack with no row for it
  (the first location wins), is missing too. The client uploads it again, and the upload records a new row. Nothing
  reads the other pack's footer.

The guard's notes are in memory per frontend: two frontends that each see one absence do not add up to a confirmation.
That errs toward keeping rows; a real loss is confirmed by whichever frontend sees it twice.

With the collector, heal also covers a reader that holds a location older than the grace (5.1). Its 404 finds the rows
already gone (`NotRecorded`), and nothing more happens.

The tests that hold these rules are listed under phase 0 in section 11.

## 7. The backstop, for losses nothing has read yet [proposed, gated]

Heal acts on a read. A pack lost out of band and not read since is still named by its rows. An action-cache hit over it
is served, and the first failure is the build that uses the hit (section 8 step 11). Two additions close that. Both are
**off the Execute to enqueue path** (goal 100 ms, timed by `kbf_execute_presubmit_seconds`).

The backstop is built only if section 10 shows out-of-band losses: any `kbf_pack_read_lost_forgotten_total` in 30 days
with the collector on, since the collector never leaves a row naming a deleted object. The reconciler is built for that
trigger too, or for the first restore of Postgres from a backup (section 12), whichever comes first.

### 7.1 An action-cache hit that checks its packs

On a client's GetActionResult hit, after the rows pass (`served`), the frontend asks the store for the length of each
distinct pack key of the closure, concurrently. What each answer does:

- **Absent or short:** this answer is a miss, and the absence is handed to the `LossGuard`. The rows go only by the
  guard's rules (section 6), because a miss is cheap and a forget is not.
- **A store error:** the hit stands. The rows say present, and a store outage must not become a rebuild storm.

Execute's own action-cache lookup (`execution.rs`, inside the 100 ms path) does not wait for this check. It runs the
same check after answering, so a loss it finds is noted for the next request.

A cache of keys recently found present is added only if the measured cost of the length asks calls for it, together
with its limit: a pack lost within the cache's lifetime is still served once.

### 7.2 A reconciler

An operator verb, `kbf collect reconcile`, not a standing loop. It lists the store under `cas/f/`, `cas/b/` and `cas/l/`
with the sweeper's paged listing, and compares the keys with `pack.key`.

- **A `pack` row with no object** is noted. The verb looks again 10 to 60 min later, in the same run, which is the
  confirmation rule of section 6. A key still absent is forgotten with `forget_pack(key, MIN_AGE)`.
- **An object outside `cas/b/` with neither a `pack` nor a `retired` row** is counted by prefix and bytes. Nothing is
  deleted.

## 8. The scenario, step by step

Action A ran long ago and produced output X, a blob in pack P. Nobody has used X since. The collector removes P. Later a
build of target B depends on A's output. Required: A is rebuilt; never a stale hit; never just a failed mount.

**Day 0, the first build.**

1. A runs as a job. Its ingest records P: the `pack` row and X's `blob` row in one transaction (`record_pack`), only
   after the store acknowledged P's PUT **[code]**.
2. A's `action_result` row is written last, only after `checked` finds every output held **[code]**.

**Day 0 to day 7.** Nothing asks about X: no FindMissingBlobs, no hit, no read. `last_touch_us` of P's rows stays old.

**Day 7, the collector.**

3. A pass selects P: every row's `last_touch_us` is before the cutoff (4.1).
4. `retire_pack(P)` locks P's rows and finds no recent touch. In one transaction it deletes P's `blob` rows, their
   `directory` rows and P's `pack` row, and inserts `retired (P)` (4.2). From this commit FindMissingBlobs(X) answers
   missing, and no reader can be told P holds X. A's `action_result` row stays.
5. Six hours later the deleter locks `retired (P)`, finds no `pack` row for P, deletes the object, and deletes the
   `retired` row (5.2).

**Day 9, a new build of B, in a fresh Buck2 daemon.**

6. Buck2 asks the action cache for A. `get_action` finds A's row, and `served` derives the closure and asks
   `find_missing(X)`. X has no row, so the answer is NOT_FOUND **[code]** (`closed.rs`). This is REAPI's contract: a hit
   whose blobs are missing is not served, and NOT_FOUND means execute. The answer is made from rows and the collector
   dropped them first, so it cannot be a stale hit (4.4).
7. Buck2 runs A. A's inputs went through the same steps: those the collector took are missing to FindMissingBlobs, and
   are uploaded again (sources) or rebuilt (outputs, step 6 again for their producers).
8. A's ingest writes X in a new pack P3. The Packer does not skip X, since it has no row. `record_pack` inserts X's row,
   since there is no live row to conflict with. `put_action` replaces A's row **[code]**.
9. B's Execute walks its inputs: X is present, and its tree names a span of P3. B's job runs. **A was rebuilt and B
   built.**

**The same day 9, but Buck2's daemon from day 0 is still running and holds X's digest.**

The daemon remembers A's result, does not ask the action cache again, and sends B's Execute naming X.

10. kbf's Execute walks B's inputs before anything is queued (`input_walk.rs`). It finds X missing and answers
    FAILED_PRECONDITION with a MISSING violation for `blobs/<X>` **[code]**. **No job is submitted, so no mount can
    fail.**

    What Buck2 does next is not established. It cannot upload X: it never downloaded it. To build B it must run A again.
    The repository holds no tested statement of what Buck2 does here, and its documents disagree:
    `storage-ram-ack.md` says REAPI clients re-upload on MISSING, while `failure-classes.md` and `capabilities.md` say
    Buck2 treats FAILED_PRECONDITION as a user error.

    The farm test of phase 2 answers the question, before the collector is built. Section 11 phase 2 says what happens
    in each outcome. **The collector is not turned on in production until the held-digest case ends in a rebuild.**

**The variant: P lost without the collector** (a manual delete).

11. P's rows survive. Without the backstop, step 6 is a stale hit. Buck2 builds B, B's job's tree names a span of P, and
    tierstore answers `source missing: P`. That is infra and retried. Before the retry `Front::lost` drops the memos
    naming P and notes the absence. A second absence 10 to 60 min later confirms the loss, and the rows go. From then on
    step 6 is a miss, and the flow is steps 7 to 9 **[built]**. Until the confirmation, B's attempts fail infra. This is
    the case the backstop exists for, and the reason it is gated on how often it happens.
12. With the backstop (7.1), step 6's length check finds P absent, the hit is a miss, and the flow is steps 7 to 9 at
    once.

## 9. Why not the filer as the index

SeaweedFS's filer could hold one entry per digest, `/cas-index/<digest>`, whose extended attributes name the pack key,
offset and length. The filer has the pieces: a point lookup, a batched `LookupDirectoryEntries`, create-if-absent, and
compare-and-delete on the attributes (SeaweedFS 4.54, `weed/pb/filer.proto`). It does not remove the problem this design
solves, and it costs the hot path:

- **No refcounts.** Deleting a pack's entry never touches the digest entries pointing into it
  (`weed/filer/filer_delete_entry.go`). Whoever deletes a pack must still delete its digest entries first. That is the
  collector of section 4, moved.
- **Attributes are not queryable by value.** "Every digest in pack P" has no index, so the deleter must read P's footer
  to get the list. Postgres answers it from `blob_pack`.
- **Still two records.** The pack object and the digest entries must agree, exactly as `pack` and `blob` rows must
  today. The cascade, the races and the heal all stay; only their home moves.
- **About N Postgres lookups per FindMissingBlobs.** `LookupDirectoryEntries` takes at most 4,096 names and runs each as
  its own primary-key query, on at most 32 workers (`weed/server/filer_grpc_server_batch_lookup.go`), behind a gRPC hop.
  kbf's `MISSING` is one statement for the whole batch. The filer would add that cost to every Execute walk, closure
  check and `locate`, all of which now take one round trip, against the 100 ms goal.
- **No multi-entry atomicity.** Recording a pack of N blobs becomes N creates, where `record_pack` is one transaction.

Aliasing a sub-range of a pack as a filer chunk would need a negative chunk offset that nothing in SeaweedFS tests, and
is rejected. Revisit the filer only if a writer outside kbf must share the index.

## 10. Measurements

All are taken on the farm. None is taken yet.

| Measurement | Where | Decides |
|---|---|---|
| Execute to enqueue before and after the touches | `kbf_execute_presubmit_seconds` p50 and p99 | the touches ship only if these are unchanged within noise |
| Touch write volume and failures | `kbf_pack_touch_rows_total`, `kbf_pack_touch_flush_failures_total` | the quantum and the interval |
| The window of 4.4 | `kbf_pack_touch_late_total` | whether 3.3's synchronous touch is needed |
| Retires, touched answers, deletes, the queue | `kbf_collect_*` | the retention and the per-pass limit |
| Losses the collector did not make | `kbf_pack_read_lost_forgotten_total` with the collector on | whether to build section 7 |
| Distinct packs per action-cache closure, and the store's length-ask latency | the pass's log line (phase 3), the store's request metrics | the cost of 7.1 |
| `blob`, `pack`, `retired` row counts, and the select's run time | `SELECT count(*)`, the pass's log line | that collection keeps the tables and the bucket bounded |

## 11. Phases, and the tests each lands with

Every test runs on the injected index and pack clock (`ManualClock`, the stored rig's `Rig::wait`). None asserts
timing. Pack-index tests are conformance rules, so each runs against both the memory index and Postgres. Each test
names the defect it catches.

### Interfaces each phase adds

| Phase | Added | Documents and goldens updated in the same PR |
|---|---|---|
| 1 | metrics `kbf_pack_touch_rows_total`, `kbf_pack_touch_late_total`, `kbf_pack_touch_flush_failures_total` | `docs/api.md` (metrics) |
| 2 | `cargo xtask prove-collect` and its Buck2 project | `docs/design/gamma-cicd.md` (an owner-run step, not a job) |
| 3 | migration 5 (`retired`); `PackIndex::retire_pack`, `claim_large`; flags `--collect-retention-days`, `--collect-key-file`; metrics `kbf_collect_*` | `docs/api.md`, `docs/deploy/kbf-modes.md` (operating the collector), `crates/kbf-cli/tests/golden/kbf-frontend.toml` |
| 4 | the backstop of 7.1; `kbf collect reconcile` | `docs/api.md`, `docs/deploy/kbf-modes.md` |

### Phase 0: heal [built, komira-ai/kbf#2380, komira-ai/kbf#2382]

These live in `kbf-pack` (`tests/pack/lost.rs`), in `kbf-front` (`tests/stored/lost.rs`, `tests/action_tree/lost.rs`),
and in the executor's `front_lost_tests.rs`. The ones the rules of section 6 rest on:

- `a_pack_absent_twice_some_time_apart_is_forgotten_whole`: catches a heal that drops one blob and leaves the rest of the
  pack's rows naming a dead object.
- `an_absence_sooner_than_the_wait_forgets_nothing`, `the_second_look_decides`: catch a heal that trusts a single 404.
- `a_young_pack_is_not_forgotten_until_it_is_old_enough`: catches forgetting a pack the store has not finished copying.
- `a_store_that_answers_absent_while_a_replica_restarts_loses_no_pack`, `reads_the_store_fails_forget_nothing`: catch
  a 5xx or a restart read as a loss.
- `reads_before_the_cut_of_a_short_pack_do_not_clear_its_absence`: catches a short pack kept alive by reads of its
  intact head.
- `a_key_forgotten_is_not_forgotten_again_within_the_interval`: catches a re-uploaded `cas/l/` key forgotten again at
  once.
- `a_blob_also_in_a_second_pack_is_uploaded_again_once_its_pack_is_forgotten`: catches a second copy left unreachable.
- `an_action_result_whose_output_is_lost_is_a_miss_once_its_pack_is_forgotten`: catches a stale hit after heal.
- `a_missing_source_drops_the_memos_and_the_retry_does_not_reuse_its_pack`: catches the retry loop on a memoised tree.

### Phase 1: touches (one PR)

It changes what is written to `last_touch_us` and nothing else, so it is safe alone. It must have run on every frontend
for one whole retention before the collector is turned on (phase 3).

- `find_missing_notes_the_blobs_it_answers_present`: catches FindMissingBlobs not keeping a blob alive. Without it, a
  blob a build only asks about would be collected.
- `locate_and_directories_note_what_they_answer`: catches `put_large`'s early answer, a job tree's spans or a walk's
  Directory rows not keeping their blobs alive.
- `the_packer_skip_notes_the_skipped_blob`: catches a Packer built with the unwrapped index, whose skips would keep
  nothing alive.
- `an_action_cache_hit_notes_every_blob_of_its_closure`: catches a hit that touches its files but not stdout, stderr, a
  Tree blob or the files inside it.
- `a_note_never_waits_on_the_index`: the index is held blocked while notes are made. Catches a flush back on the
  request path, which would put a database write on Execute to enqueue.
- `pending_touches_are_written_on_the_interval_without_a_full_batch`: catches today's count-only flush, under which a
  quiet frontend never writes a touch.
- `a_failed_flush_keeps_the_touches_it_did_not_write`: catches today's flush, which drops every pending touch when one
  statement fails.
- `a_touch_inside_the_quantum_writes_no_row`: catches one write per use instead of one per hour.
- `a_touch_of_a_blob_with_no_row_is_counted_late`, and its pair `a_touch_skipped_by_the_quantum_is_not_late`: catch a
  window counter that never moves, or one that counts fresh rows.

### Phase 2: the farm test of the held digest (one PR, before phase 3 is written)

The question of section 8 step 10 depends only on kbf's answers, and those depend only on rows. So it can be answered
with today's code, and it is answered first: if Buck2 cannot recover, phase 3 changes.

`cargo xtask prove-collect` sits beside `prove-farm` and reuses its Buck2 project fixture and its `Commands:` parser.
It is **an owner-run held step**:

- It runs in no workflow.
- It is aimed at a farm the owner names, with the farm's state store URL as a flag and its credentials and store key
  from the environment.
- It is **dry-run by default**: without `--delete` it builds, reports the pack it would delete, and deletes nothing.

The fixture gains two remote targets:

- `//:a` copies a nonce file, which the verb writes into the project before each case, to its output. The verb knows
  the output's bytes, and so X's digest.
- `//:b` reads `//:a`'s output.

Both targets run remotely, and every build must report `local: 0`.

1. `buck2 build //:a --materializations=none`. Buck2 now holds X's digest and not its bytes.
2. The verb looks X up (`PackIndex::locate`). X is a new digest, expected in `//:a`'s run-unique task pack `cas/b/...`.
   The verb refuses, by name, a pack that holds any blob other than `//:a`'s outputs, so the proof never deletes another
   build's bytes. The verb forgets that pack with `forget_pack(key, 0)` and deletes its object with the given key.
   This is a pack deleted between two builds in one daemon: the state section 8 step 10 describes.
3. `buck2 build //:b --materializations=none` in the same daemon. Pass: the build succeeds, and `locate(X)` names a pack
   other than the deleted one. That means `//:a` ran again and its output was recorded anew.
4. The fresh-daemon case: a new nonce, then steps 1 and 2, then `buck2 kill` (to start without the held digest), then
   step 3's build and check.

What each step catches:

- **Steps 1 and 2** catch a fixture that does not reach the state under test: X still local, X in a shared pack, or X
  not recorded. The verb fails by name instead of passing vacuously.
- **Step 3** catches the held-digest case ending in a failed build.
- **Step 4** catches a stale hit or a failed rebuild on the action-cache path of steps 6 to 9.

Its own tests (`crates/xtask/tests/prove_collect.rs`, over a fake Buck2 and a memory index and store, as
`prove_farm.rs` does):

- `without_delete_nothing_is_forgotten_or_deleted`: catches a dry run that acts.
- `a_digest_with_no_row_after_the_first_build_fails_by_name`: catches a vacuous pass.
- `a_second_build_that_fails_fails_the_proof` and `a_second_build_that_reuses_the_deleted_pack_fails_the_proof`: catch a
  proof that accepts a failed build or a non-rebuild.
- `any_local_action_fails_the_proof`: catches a run that built locally and proved nothing about the farm.

**Outcomes:**

- **Step 3 passes:** phase 3 goes ahead as written.
- **Step 3 fails:** Buck2 does not run a producer again on MISSING when it holds the digest. Phase 3 still collects, but
  the MISSING answer of an Execute names the remedy in its message: the input was collected after the retention unused,
  and a new daemon (`buck2 kill`) rebuilds it, as step 4 proves. The collector is turned on only once that outcome is
  accepted. Rebuilding a producer from kbf's side, from the `action_result` rows, would be a separate design.

### Phase 3: retire and delete (one PR, off until `--collect-retention-days` is given)

Pack index (conformance):

- `a_pack_unused_past_the_cutoff_is_retired_whole_with_its_directory_rows`: catches a partial retire that leaves some
  rows naming the pack.
- `a_pack_touched_after_it_was_selected_is_not_retired`: catches a retire that trusts the selection, which is the first
  row of 4.4.
- `a_touch_waiting_on_a_retire_finds_no_row` (Postgres): the retire holds its row locks while a second connection's
  touch waits on them, so the order comes from the lock, not from time. Catches a lock order under which the touch is
  lost or the two deadlock.
- `a_pack_recorded_after_the_cutoff_is_young`: catches retiring a pack recorded during the pass.
- `retiring_a_key_retired_before_refreshes_its_row`: catches a retire that errors on every pass for a key retired, used
  again and retired again.
- `a_retired_key_is_not_unreferenced`: catches the janitor deleting a retired `cas/b/` pack before the grace.
- `a_blob_recorded_again_after_its_pack_is_retired_gets_a_new_row`: catches a retire that blocks re-upload.
- `the_action_result_row_outlives_the_retire_and_its_hit_is_a_miss`: catches a stale hit after collection, and a
  collector that deletes authoritative rows.

Deleter and collector (`kbf-pack`, `kbf-server`, the stored rig):

- `a_retired_object_is_deleted_only_after_the_grace`: catches deleting under a reader that located the pack before the
  retire.
- `the_rows_are_gone_before_the_object_is`: the store's delete call asserts that the index already answers the blobs
  missing. Catches the wrong order, the one failure the invariant cannot survive.
- `a_large_blob_uploaded_again_during_the_grace_keeps_its_object`: the claim commits first, and the deleter keeps the
  object. Catches the deterministic-key race of 5.2 for `put_large`.
- `a_tree_that_places_a_retired_large_blob_keeps_its_object`: the same race for the tree's `place`, which records no
  row. Catches a failed mount on a key the deleter thought unused.
- `a_claim_behind_the_deleters_lock_finds_the_object_absent_and_writes_it`: the deleter locks first. Catches a user that
  trusts a length it read before the delete.
- `every_user_of_a_large_key_claims_it_first`: `put_large`, the write-through copy, `large_backfill`, the base
  publish, `place` and the manifest writes each call `claim_large` before their length check or write. Catches a path
  added later without the claim.
- `a_retire_refreshed_after_the_deleter_listed_it_keeps_the_object`: catches a deleter that trusts the age it listed
  rather than the age under its lock.
- `a_record_meeting_a_row_under_retire_keeps_the_blob_recorded` (Postgres, ordered by the locks): catches the
  `DO NOTHING` conflict that lets a retire delete the row of a blob a pack just recorded.
- `a_failed_delete_keeps_its_retired_row_for_the_next_pass` and `a_delete_of_an_absent_object_is_done`: catch an object
  that is never deleted, or a `retired` row that never goes.
- `only_the_leader_collects` and `the_collector_without_a_retention_does_nothing`: catch two collectors, or one on by
  default.
- `a_retention_under_two_days_is_refused`: catches a retention shorter than a build.
- `an_old_output_collected_is_a_miss_and_its_rebuild_is_a_hit`: the stored rig runs steps 1 to 9 of section 8 with REAPI
  calls. Catches any break in the chain from collection to rebuild.
- `an_execute_naming_a_collected_input_is_answered_missing_and_queues_no_job`: step 10. Catches a collected input
  reaching a job and its mount.
- `a_missing_answer_for_a_collected_input_names_the_remedy`, only if phase 2's step 3 fails: asserts the whole
  message by equality, and that its detail is still exactly one MISSING violation per blob. Catches a message that
  does not tell a held-digest user how to recover, or an extra detail that stops REAPI clients from uploading again.

Compatibility (`crates/kbf-integration-test/tests/mixed_version.rs`):

- `the_previous_release_runs_on_a_database_at_migration_5`: an older frontend serves, records and runs its janitor
  over `retired` rows and retired packs. Catches a migration an older release cannot open.
- `this_release_runs_on_a_database_at_migration_4_with_the_collector_off`: catches a frontend that will not start, or
  collects, before its migration.

**Turning it on in production** is an operator step, done once all of these hold:

- every frontend runs the phase 3 release, since an older one neither claims nor excludes retired keys;
- touches have run on every frontend for one whole retention;
- phase 2's outcome is accepted.

Then give the leader `--collect-retention-days 7` and `--collect-key-file`, and watch three numbers:
`kbf_collect_packs_total{answer="retired"}`, `kbf_pack_touch_late_total`, and
`kbf_executor_runs_total{outcome="missing"}` against its week before. If the late count or the missing runs climb,
remove the flag and restart the leader. Retires stop at once; the deletes already queued wait in `retired` until the
flag returns.

### Phase 4: backstop and reconciler (only on the triggers of section 7)

- `a_hit_over_a_pack_the_store_lacks_is_a_miss_and_noted`: catches the stale hit of section 8 step 11.
- `a_hit_whose_length_ask_fails_is_served`: catches a store outage that turns every hit into a rebuild.
- `executes_own_lookup_answers_before_the_check`: catches the check landing on the Execute to enqueue path.
- `a_row_with_no_object_is_forgotten_only_after_a_second_look`: catches a reconciler that forgets on one listing.
- `an_object_with_no_row_is_counted_and_kept`: catches a reconciler that deletes.

## 12. Compatibility and failure modes

- **Migration 5 adds a table** (expand only). An older release never reads `retired`, and loses nothing by not knowing
  it: it neither collects nor deletes. A newer release on a database at migration 4 runs with the collector off;
  `retire_pack` answers `NotMigrated`, which is logged.
- **Mixed releases.** An older frontend's janitor does not leave out retired keys, an older `put_large` or `place`
  makes no claim, and an older frontend touches only on reads. Section 11 phase 3 therefore makes "every frontend runs
  the phase 3 release, and touches have run for a retention" the condition for turning the collector on. Retiring
  without it would take blobs that frontend alone keeps alive.
- **The collector crashes mid-pass.** Each retire is its own transaction, and the `retired` rows are the deleter's
  queue, so a new leader resumes where the old one stopped. A delete done but not committed is done again; NotFound
  counts as success.
- **Postgres is restored from a backup.** Rows of packs deleted since the backup come back, and objects written since
  have no rows. Leave the collector off. Until the reconciler is built, heal finds the dead packs one read at a time,
  and a hit over one fails as in section 8 step 11. Once it is built, run `kbf collect reconcile` before turning the
  collector on again.
- **The store is down.** Retires go on, since they touch only rows. Deletes fail, are counted and are retried. Heal
  confirms nothing, because a 5xx is never an absence.
- **A wrong retention.** One too short is refused at start. One too long only costs space.

## 13. What is not decided here

- The retention of `cas/l/` objects with no row (manifests, tree lists, placed and backfilled copies, and claimed
  retired keys) and of orphans outside `cas/b/`. That is their own collector, after section 10 shows their size.
- Deleting `action_result` rows that can only miss.
- Compaction of packs that are mostly cold.
- A store-wide garbage collector outside kbf. If one is ever wanted, it must route deletes under `cas/` through this
  collector, since it cannot know kbf's rows. Otherwise heal and the backstop are all that protect the invariant.
