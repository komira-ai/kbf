# Tree metadata in Postgres; the frontend runs no tierstore

Status: DECIDED (direction); step 1 of section 6 (the `directory` table, the walk over it and a pack store reached through
several endpoints) is built by the change that adds this document; steps 2 to 4 are operator steps. Claims are tagged **[code]** (true at this change, with the file or function), **[built here]** (added by
this change) or **[proposed]**.

## 1. The decision

kbf cares about two kinds of metadata: where each blob's bytes are (a pack key, an offset, a length) and what each
Directory holds (its files, subdirectories and symlinks). Both live in the Postgres database the state store already
uses (schema `kbf_state`), as typed tables beside the key/value tables. The bytes live in one bucket behind the farm
nodes' tierstores, and the frontend reaches that bucket through the six nodes' tierstore endpoints with failover. The
frontend host runs no tierstore of its own.

Postgres, not a graph database: the only traversal kbf makes is a walk of a Merkle tree from its root, level by level,
and each level is a batch of lookups by primary key. That is one indexed query per level in Postgres, which is already
deployed, backed up and migrated by `kbf-installer` with expand-only migrations, and already holds the blob locations.
A graph database would add a second store to run and keep consistent with the first, for a query shape that needs no
graph engine.

## 2. What is where

| What | Where | Rows | Status |
|---|---|---|---|
| Blob location: digest -> pack key, offset (length is the digest's size) | tables `pack` and `blob` (migration 2, `crates/kbf-pack-index/migrations/0002_pack_index.sql`) | one per distinct blob, one per pack | **[code]** |
| Action result: action digest -> encoded `ActionResult` | table `action_result` (migration 3) | one per action | **[code]** |
| Directory: digest -> the encoded REAPI `Directory` | table `directory` (migration 4, `0004_directory.sql`) | one per distinct Directory a walk read | **[built here]** |
| Bytes: packs `cas/f/<writer>/<boot>/<seq>`, large blobs and tree lists `cas/l/<sha256>`, put dirs `cas/b/...` | the bucket behind the tierstores | objects | **[code]** |

The blob location is not read from pack footers. A footer (`crates/kbf-segments/src/layout.rs`) lists each blob's
digest, offset and CRC; kbf reads footers when it ingests a pack a task wrote (`ingest_put_dir`), in the one-off
adoption tool (`kbf-pack-adopt`) and in the storage members' cache paths (`kbf-front/src/cache`: index rebuild, flush),
never on the blob read path. The read path is an index query and then a ranged GET of the pack
(`crates/kbf-pack/src/reader.rs`) **[code]**. So there are no footer reads on the hot path to delete.

### 2.1 The `directory` table

```sql
CREATE TABLE directory (
    hash bytea NOT NULL,        -- SHA-256 of body
    size bigint NOT NULL,       -- length of body
    body bytea NOT NULL,        -- the Directory message exactly as the client uploaded it
    written_us bigint NOT NULL, -- the store's clock at the insert
    PRIMARY KEY (hash, size));
```

- **Content-addressed and immutable.** The index checks that `body` hashes to `hash` and has length `size` before it
  inserts, and refuses the whole call otherwise; a second insert of a digest is `ON CONFLICT DO NOTHING`. A row is
  therefore never a wrong answer for its digest, whichever frontend wrote it.
- **One row per Directory, the message as bytes**, not one row per entry. The walk needs the decoded Directory in
  the application anyway, the bytes are what the digest names, and an entry table would hold a row per file of every
  tree (a toolchain of 100,000 files is 100,000 rows) to save nothing.
- **Bounded.** A body over 4 MiB (`MAX_DIRECTORY_BYTES`, tens of thousands of entries) is not recorded; the walk reads
  such a Directory from its blob every time. A call names at most 1,000 digests (`MAX_BATCH`).
- **Index.** The primary key only. Every query is a lookup by `(hash, size)` from an `UNNEST` of the level's digests.
- **Outside the head lock.** Like `pack` and `blob`, the table is outside `kv`, `feed` and the commit head, so a write
  never waits behind a state transaction (`tests/postgres_directory.rs`).

### 2.2 Row counts and growth

Not measured: the frontend host's `SELECT count(*) FROM kbf_state.blob` and `... pack` have not been run for this
document. The repository's own estimates (`docs/design/metadata-snapshots.md` section 7.2, estimates, not
measurements): about 1 M new blobs a day at the pilot ceiling, about 8.3 M blobs at steady state once blobs untouched
for about 8 days are collected. At about 200 bytes per `blob` row with its two indexes (inferred from the column widths
and fillfactor 70) that is about 1.8 GB, and about 0.2 GB a day before collection. `pack` rows are bytes divided by
32 MiB, tens of thousands: negligible.

`directory` rows are one per distinct Directory of the trees walked: a fraction of the blobs (inferred: a Directory per
directory of every input root, typically a few percent of the blobs), each a few hundred bytes to a few KB. Its size is
of the order of the `blob` table's or smaller.

For scale, measured on the six nodes' tierstores (`tierstore_cache_objects` and `tierstore_cache_bytes` on each node's
`/metricz`, read-only): 974,110 cached objects and 153.3 GB in total. These are cache entries, not index rows; they
show the working set is in the hundreds of thousands of objects, not billions.

### 2.3 Garbage collection

Nothing deletes a recorded pack today: `PackIndex::dead_packs` has no caller outside tests; the objects deleted are the
key of a failed PUT attempt (`Packer::discard`) and orphans under `cas/b/` (the janitor) **[code]**. When pack collection is built **[proposed]**, it deletes `blob` rows untouched for the grace
period, then the objects of packs no row points at (`dead_packs`), and deletes the `directory` row of a digest in the
same transaction as that digest's `blob` row. A `directory` row whose blob was collected is still a correct answer for
its digest, so the order of the two deletes is not a correctness question, only one of size. Until then `directory`
rows, like packs, are never deleted.

## 3. Reads: the walk before a job is submitted

`collect_closure_keeping` (`crates/kbf-front/src/prepare_run/closure.rs`) walks the input root level by level. Each
level **[built here]**:

1. asks the `directory` rows for every Directory of the level in one call per 1,000 digests (`Backend::directories`);
2. reads only the Directories without a row from their packs (`Backend::read_blob`: index lookup, ranged GET, SHA-256
   check), 16 at a time;
3. records those as rows (`Backend::record_directories`), so the next walk of the same tree, on any frontend over the
   same database, reads none of them from the object store. The rows of the whole walk are recorded once, after it
   ends, by a task the preparation does not wait for; the record is skipped, and counted, when the bound on such
   tasks (`DETACHED_WRITES`) has no room, and the next walk then reads those Directories from their packs again.

The rows never decide the answer. A failed read of the rows is logged and the level is read from the blobs; a failed
record is logged; an index that keeps no rows answers every digest as having none. A row whose
bytes do not decode is refused as its blob would be (`tests/stored/directories.rs`).

The order today is rows, then blobs (with the packs' optional on-disk `ReadCache` beneath the blob read). A local
in-memory or NVMe cache of Directories and trees on the frontend **[proposed]** would sit above the rows: cache, then
rows, then blobs. Target **[proposed]**: a walk of a tree whose Directories all have rows costs one query per level on
the local Postgres socket, so a tree 20 levels deep is about 20 round trips, tens of milliseconds, plus the existing
FindMissing of the files (`require_held`), and no object store request for any Directory.

No recursive CTE: one query per level keeps each query a plain primary-key lookup, lets the application apply the
closure limits (`ClosureLimits`) before it reads the next level, and needs no per-entry rows (section 2.1).

The walk of the Execute input check (`crates/kbf-front/src/input_walk.rs`, with its in-memory `DirCache`) still reads
blobs; moving it onto the rows is the same change at its one seam and is left out of this change.

## 4. Writes

**CAS upload [code].** A blob a client uploads is packed, the pack is written to the object store with a create-only
PUT, and only after the store acknowledges it is the pack recorded in `pack` and `blob` (`Packer::commit`): a row never
points at an object that is not stored. One logical step, two systems, ordered so each failure is safe:

- the PUT fails: the key is deleted best effort and the pack is written again under a new key (3 attempts); when all
  fail the upload answers UNAVAILABLE and the client sends it again. A PUT whose reply was lost is a failed attempt too:
  its object stays under the old key, which no row names;
- the PUT succeeds and the record fails: the record is retried for the same key; when every attempt fails the upload
  answers UNAVAILABLE and the stored pack is an object no row points at;
- objects no row points at stay until pack collection is built (section 2.3, **[proposed]**).

**Directory rows** are written by the walk (section 3), not at upload: an uploaded blob does not say whether it is a
Directory, and decoding every upload as one would record rows for blobs that are not Directories.

**Through the farm's tierstores [built here].** `--pack-store-endpoint` may be given more than once (an array in
`kbf.toml`). The frontend builds one S3 client over all of them (`S3Store::new_rotating` of `objstore-s3`): each request
goes to the next endpoint in turn, and a request that could not connect (refused, no route, connect timeout) is sent to
the next, trying at most three endpoints for one request (`ENDPOINT_ATTEMPTS`); a request that may have reached a store is
never sent to another endpoint (a read may still be retried, `READ_RETRIES`). The janitor of `cas/b/` uses the
same endpoints. This is safe because every node's tierstore writes through to the same bucket before it acknowledges a
PUT, with no hot tier (`docs/design/storage-after-kbf-daemon.md` section 2, "Visibility"), so a pack written through one
node is read through any other.

## 5. How a node gets a job's files

Unchanged **[code]**: the frontend writes one tierstore tree list per job (`crates/kbf-front/src/action_tree/listing.rs`),
naming each file's pack span from the `blob` rows (`action_tree/place.rs`, `locate`), and the job's layer names the
list's key for tierstore's mount provider (`--executor-input-tree provider`, komira-ai/jobscheduler#896). The node's
tierstore fetches the list and the spans from the bucket. The list is built from the database; the node reads no
database.

## 6. Cut-over

Nothing moves. The frontend host's tierstore and the six nodes' tierstores front the same bucket (the frontend's
`pack-store-bucket` is the farm's bucket), and the rows are already in Postgres. Downtime is acceptable for this
change, so the order is direct:

1. **Release with migration 4 [built here].** `kbf boot-check` applies it before the frontend starts. It only adds a
   table, so the release before it runs on the new schema and never reads the table, and the new release on a database
   still at migration 3 starts and walks from the blobs, logging a warning for each failed read and record of the rows.
2. **Prove the frontend host's tierstore holds nothing the bucket lacks [proposed, operator].** It has run without a
   hot tier since the hot tier was turned off; the objects that were only in its hot tier must be in the bucket before
   step 4. A listing of the hot tier compared with the bucket is the check.
3. **Point the frontends at the farm [proposed, operator].** Set `pack-store-endpoint` to the six nodes' tierstore
   endpoints, with a key the nodes' tierstores accept that reads and writes under `cas/` and deletes under `cas/b/` (the
   janitor). Restart each frontend. With N frontends every frontend gets the same list; no frontend state depends on
   which endpoint it uses.
4. **Remove the frontend host's tierstore [proposed, operator].** Stop and remove its unit, its proxy and its key once
   the frontends have run on step 3's endpoints.

Back out of step 3 by setting the endpoint back; nothing written in between is lost, because both paths reach the same
bucket.

## 7. What is deleted

- The frontend host's tierstore, its proxy and its key (step 4, operator).
- Presigned URLs and per-job read grants leave the execution path in the prepare-path change (the node reads through its
  own tierstore); this design needs neither.
- Pack-footer reads on the hot path: there are none to delete (section 2).

## 8. Compatibility

Migration 4 is expand-only (a new table): an older release ignores it, and a newer release on an older database reads
blobs instead of rows. `PackIndex::directories` and `record_directories` default to `Unsupported`, so an index that does
not keep the rows needs no change. `--pack-store-endpoint` given once behaves exactly as before.
