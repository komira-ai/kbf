# The pack index over the store: rows are a location cache, checked on every answer

Status: DESIGN (2026-11-19). Heal on confirmed absence for reads and job starts (section 5.3) is built
(komira-ai/kbf#2380, komira-ai/kbf#2382). Everything else is proposed and lands in the phases of section 10.

Claims are tagged:

- **[code]**: true at the commit this document was written against, with the file or function.
- **[built]**: landed.
- **[measured]**: with how it was measured.
- **[proposed]**: not built.

The constants of section 3 are provisional until phase 1a measures what they bound.

Related:

- [metadata-in-postgres.md](metadata-in-postgres.md): the tables. Its section 2.3 is superseded by section 6 here.
- [actions-as-jobs.md](actions-as-jobs.md) section 6: what a client sees when a pack is lost.
- [trees-for-action-inputs.md](trees-for-action-inputs.md) section 5.4: its retention paragraph is superseded by
  sections 3 to 6 here.

The retention of [storage.md](storage.md) (`min_ttl`, `Collect`) belongs to the earlier metadata store (`kbf-meta`),
not to the pack index. So does the retention in the documents that quote it: `write-path.md`, `invocations.md`,
`docs/guides/storage.md` and `docs/deploy/storage-node.md`. This design does not change it.

## 1. The decision

kbf keeps rows about bytes in its Postgres database:

- where each blob is (`blob` and `pack`);
- what each Directory holds (`directory`);
- what each action produced (`action_result`).

The bytes are objects in the store: SeaweedFS (filer and S3 gateway, two copies of every object) behind the nodes'
tierstores. The store is general-purpose, and objects may be deleted underneath kbf. That is accepted.

So the `blob`, `pack` and `directory` rows are **a location cache, not a source of truth**. A row says where a blob was
put. Whether it is still there is the store's to say, and kbf asks before it answers:

> **Rule.** Every "do you have this" answer kbf gives a client checks the packs it would answer from against the store,
> in **one batched existence check** per answer, before it says "present". A pack the store lacks is answered missing,
> and its rows are dropped (heal). The client then uploads again or runs the producer again. The worst case is a
> rebuild. A stale hit is possible only inside two stated bounds:
>
> - the positive cache, 30 s after a pack was last found present (section 3.5);
> - the window after a pack is recorded, in which it is taken as present without asking. That window is set just above
>   the filers' measured replication lag (section 3.4).

The answers that check are:

- GetActionResult, for the packs of the closure;
- FindMissingBlobs;
- the Execute input walk;
- the job tree's `locate`.

Consequences:

- **"Rows name existing objects" is checked at answer time, not kept by construction.** A row naming a deleted object
  is harmless. The next answer that would use it finds the object gone and drops the row.
- **The collector is for space only** (section 6). Correctness does not depend on how or when it deletes: it needs no
  rows-first ordering, no grace and no claim on deterministic keys.
- **`action_result` rows stay authoritative.** They are in no object (`migrations/0003_action_cache.sql`) **[code]**, so
  they cannot be rebuilt from the store, and losing them means rebuilds. A row whose outputs are gone is a miss through
  the same check, and the rebuild replaces it.

## 2. Where things are today

| What | Where | Status |
|---|---|---|
| Blob location: `blob (hash, size, pack_id, off, last_touch_us)`, primary key `(hash, size)`, index `blob_pack (pack_id)`; `pack (pack_id, key UNIQUE, writer, size, created_us, blob_count, live_blobs)` | `crates/kbf-pack-index/migrations/0002_pack_index.sql` | **[code]** |
| Recording: a pack is recorded only after the store acknowledged its create-only PUT; the first location of a blob wins | `Packer::commit`, `postgres.rs` `record_in` | **[code]** |
| FindMissingBlobs, the action-cache closure check (`closed.rs` `lacking`), the Execute walk and `put_large`'s early answer read rows only | `Packs::missing`, `Packs::locate`, `large.rs` `locate_stored` | **[code]** |
| The store is asked only by a checked action tree (one HEAD per pack) and by `locate` (HEADs of `cas/l/` objects) | `action_tree/check.rs`, `job_read/locate.rs` | **[code]** |
| The walk remembers a verified Directory for 45 s (`VERIFIED_TTL`); the action tree's memos keep manifests and places for 10 min (`MEMO_TTL`) | `input_walk.rs`, `action_tree.rs` | **[code]** |
| Heal: an absence seen on a read or a job's start is confirmed by a second look 10 to 60 min later, and then the whole pack is forgotten (`forget_pack(key, min_age)`, one transaction) | `crates/kbf-pack/src/lost.rs` | **[built]** |
| Collection of recorded packs: none. The only object deletes are the janitor's `cas/b/` orphans and a failed PUT's key | `sweep.rs`, `Packer::discard` | **[code]** |

Keys **[code]**:

- **Unique per write:** frontend packs `cas/f/<frontend>/<boot>/<seq>` and task packs `cas/b/<build>/<nonce>...`.
- **Deterministic:** `cas/l/<sha256>`, which holds large blobs, action-tree manifests and tree lists.

## 3. The check

### 3.1 What is checked, and in what order

Each answer resolves its digests to locations in one `LOCATE`-shaped statement, which also returns each pack's
`created_us` and `size`. The distinct object keys then go through three steps, in order:

1. **Recently recorded packs are taken as present.** A pack recorded less than `TRUST_RECENT` ago is not checked
   (3.4). kbf recorded it only after the store acknowledged its PUT **[code]**.
2. **The positive cache.** A key found present less than `PRESENT_FOR` (30 s) ago is taken as present (3.5).
3. **One batched existence check** of the rest (3.2).

A blob is answered present only when its row exists and its pack passed.

| Answer | Keys it checks |
|---|---|
| FindMissingBlobs | the packs of the present blobs |
| GetActionResult (a client's, and Execute's own lookup) | the packs of every blob of the closure: files, stdout, stderr, Tree blobs and the files inside them. Any pack missing makes it a miss |
| The Execute input walk (`input_walk.rs`), before anything is queued | the packs of the input files **and of the input root's Directory blobs**, including Directories the walk took from `directory` rows or its cache |
| A job tree (`place.rs`, the executor's `require_held`, a memo hit) | every object the tree names: the packs of its spans, the `cas/l/` objects it names whole, and its manifests |
| `put_large`'s early "already stored" | the large blob's own `cas/l/` key |

**What is not checked: kbf's own write path.** The action-cache write's `checked`, and the records made by the ingest
and the Packer, read rows only. kbf has just written those packs and been told so by the store. A check there could
only meet the filer's lag (3.4) and refuse kbf's own good output.

**Memories that skip a check obey the same bound.** The walk's verified-Directory memory and the action tree's memos
are caches of earlier checks. Phase 2 keeps each entry only until `PRESENT_FOR` after the check it rests on, instead of
45 s or 10 min. A memo entry still saves its manifest writes for 10 min. Only the existence of what it names is asked
again, through the cache.

### 3.2 How: the filer's batch lookup, then an independent HEAD to confirm

**The batch is the filer's `LookupDirectoryEntries` over gRPC** (SeaweedFS 4.54, `weed/pb/filer.proto`). One request
carries up to 4,096 names and returns each entry with its attributes. Larger batches are split and sent at once.

- **Mapping.** A key `cas/...` is the filer path `<root>/cas/...`, where `<root>` is the bucket's directory
  (`--pack-store-filer-root`, the S3 gateway's `/buckets/<bucket>`).
- **Present** means the entry exists and its file size is at least the pack's recorded size.
- **The client** is generated from SeaweedFS's `filer.proto` (Apache-2.0) with the workspace's `tonic-prost-build`, in
  `kbf-objstore` (`FilerPresence`).
- **Which filer:** the frontend names it with `--pack-store-filer <host:port>` (3.4).

**Confirmation is a HEAD through the store.** A key the filer answers absent, or shorter than recorded, is asked once
more with a HEAD through the existing store client (`ObjectStore::len`). That goes through tierstore and the S3
gateway, so it is a second, independent probe:

- **Absent or short there too:** the pack is missing.
- **Present there:** the filer was behind or wrong. The pack is present, and the disagreement is counted
  (`kbf_pack_check_disagreed_total`).

The HEAD runs only for absences, so it costs nothing in the common case. It is what lets a missing answer drop rows at
once (5.2) without a sick or lagging filer costing healthy packs.

**When the filer is not there.** With no filer configured, or when the filer refuses or fails a batch, the batch is
done with HEADs alone, 32 at a time (`HeadPresence`). Both probes implement one trait, `Presence::present(keys)`.

**The start-up check.** At start the frontend writes its self-test key (`cas/f/<name>/<boot>/selftest`) **[code]**,
waits `TRUST_RECENT`, and looks the key up through the filer. A filer that cannot find it is not used: the frontend
logs why and uses HEADs. A wrong root or port is found once, at start.

**What the check proves.** A filer entry or a HEAD proves that the object's entry exists, with its size. It does not
prove that every chunk is readable. A lost chunk under an intact entry is the read path's to find (`Fetch::Lost`, and
the heal of 5.3).

### 3.3 What a check answers

| The store answers | kbf answers | Rows |
|---|---|---|
| present | present | the key enters the positive cache; a heal note on the key is cleared (`LossGuard::held`); the pack's use is noted (6.1) |
| absent or short, and the HEAD agrees | **missing** | forgotten now (5.2) |
| an error, a timeout or a refused call, on both probes | from rows, as today, counted (`kbf_pack_check_unverified_total`) | unchanged |

**Why an error answers from rows.** A failed check is not an absence. That is heal's rule, and the reason
komira-ai/kbf#2382 never forgets a pack on a 5xx. The alternatives are worse:

- Answering missing would turn a store outage into a storm of uploads and rebuilds.
- Answering UNAVAILABLE would fail every build while the store is unreachable, and those builds cannot read their
  inputs anyway.

The cost: while the store cannot be asked, a lost pack is answered present. That window is the outage itself.

### 3.4 The filers' replication lag: check the local filer, trust recent packs

The filers' stores are per node and replicate asynchronously. A pack written through another node's tierstore reaches
the local filer only after the local filer has synced. Until then the local filer answers it absent.

**One filer per frontend, the local one.** `--pack-store-filer` names the filer on the frontend's host, or the nearest
node's if the host runs none. A frontend always asks the same filer, so a pack it has seen present stays present to it
until the pack is deleted. The answer never flips with whichever filer happened to be asked.

**Recent packs are not checked** (3.1 step 1). A job's outputs are written through the node that ran it, and the next
action's Execute names them seconds later. That client cannot upload them: it never had them. Without this rule a
lagging filer would turn every fresh output into MISSING and a rerun.

The bound:

> `TRUST_RECENT` > the filers' replication lag (the time from a PUT through one node's tierstore until another node's
> filer finds the key)

Phase 1a measures the lag. `TRUST_RECENT` is then set to the larger of 60 s and twice the observed maximum.

This is a stated window. A pack deleted out of band within `TRUST_RECENT` of its record is answered present until the
window ends. Nothing kbf runs deletes a pack that young.

A lag longer than the bound costs an upload or a rerun, never a stale hit. The HEAD of 3.2 usually catches it anyway,
because it asks through a different node.

### 3.5 The positive cache, and its bound

The cache holds keys found present:

- each for at most `PRESENT_FOR` (30 s) after the check that found it;
- at most 100,000 keys per frontend, oldest dropped first;
- no negative answers. A missing pack is checked again on every answer, so an upload that restores it is seen at once.

The memories of 3.1 last no longer than the cache.

> Outside `TRUST_RECENT`, a pack deleted out of band can be answered present for at most `PRESENT_FOR` (30 s) after its
> last positive check on that frontend.

A job that reads such a pack fails on its mount and reports `source missing: <key>`. Phase 2 extends `Front::lost` so
that this report also **evicts the key from the cache** and from the memos. The retry's tree then asks the store, gets
"missing", and answers the client MISSING. The result is a rebuild, one retry later.

### 3.6 Cost on the Execute to enqueue path

The goal is 100 ms from Execute to the first QUEUED (`kbf_execute_presubmit_seconds`). The check adds one batched call
per answer, for the keys that are neither recent nor cached, plus a HEAD per absence.

Measured on a farm node (read-only, warm, by hand):

- **A direct indexed query of the filer's Postgres** for 1,000 pack keys under `cas/b/`: **3.3 to 4.2 ms**
  **[measured]**. That is the filer's own lookup work for one batch, without gRPC.
- **32 parallel HEADs**: **200 keys in 143 ms** **[measured]**. That is the cost of the fallback.

**The gRPC batch is not yet measured.** `LookupDirectoryEntries` runs each name as its own primary-key lookup, on at
most 32 workers (`weed/server/filer_grpc_server_batch_lookup.go`), behind a gRPC hop. For 1,000 keys that is 1,000
point queries, not one.

Phase 1a measures it from a frontend host to its filer: the p50 and p99 of batches of 10, 100, 1,000 and 4,096 keys.
The numbers go into section 8 before phase 2 is written.

**The gate:** phase 2 puts the check on the Execute path only if a 1,000-key batch's p99 is under 20 ms. If it is not,
the next step is chosen on those numbers, between:

- reading the filer's store directly (the 3.3 to 4.2 ms path, at the cost of depending on SeaweedFS's table layout);
- moving the walk's check after the enqueue, so that the job's checked tree catches a missing input instead.

How many keys one answer asks after the recent rule and the cache is also unmeasured. Phase 1b adds the histogram
`kbf_pack_check_keys` for it.

## 4. Why not the filer as the index

The check uses the filer as an **existence oracle**: kbf names a key, and the filer says whether it is there. Making
the filer the index stays rejected:

- **No refcounts.** Deleting a pack's entry never touches entries pointing into it (`weed/filer/filer_delete_entry.go`).
- **Attributes are not queryable by value.** "Every digest in pack P" has no index.
- **Still two records.** Pack objects and digest entries would have to agree, exactly as rows and objects must now.
- **About N lookups per FindMissingBlobs.** The batch lookup runs one point query per name, so N digests would cost N
  lookups. The oracle asks per pack, and one answer's thousands of digests live in a handful of packs.
- **No multi-entry atomicity.** Recording a pack of N blobs would take N creates, where `record_pack` is one
  transaction.

## 5. A pack found missing

### 5.1 The answer

Each answer uses its protocol's own way of saying "missing":

- **FindMissingBlobs** lists the blob, and the client uploads it.
- **GetActionResult** answers NOT_FOUND, and the client runs the action.
- **The Execute walk** answers FAILED_PRECONDITION with a MISSING violation per blob **[code]** (`missing_status`),
  before any job is queued, so no mount can fail. The client uploads the blob (a source), or must run its producer (an
  output; section 7).
- **A job tree** is refused as lost (`NoTree::Lost`, MISSING to the client) as soon as the check answers missing.
  Today `check_objects` refuses only once heal has forgotten the pack **[code]**. In phase 2 the check's answer
  decides.

### 5.2 The rows go at once

A pack the check found missing (the filer answered absent, and the HEAD agrees) is forgotten in the same answer with
`forget_pack(key, TRUST_RECENT)`. That one transaction removes every `blob` row of the pack and its `pack` row. It
refuses a pack recorded within `TRUST_RECENT` **[code: `Forgot::Young`]**.

Because the rows are gone:

- the client's upload is recorded fresh, with no stale row left to win the conflict;
- `put_large` finds no row, so it writes the object;
- the next answer does not check the dead pack again.

The two probes are what make an immediate forget safe:

- A filer that answers absent for a pack that exists is contradicted by the HEAD, and nothing is forgotten.
- The HEAD alone never forgets anything.
- The forget runs only from the check, once per key per answer, so a store that loses many packs at once costs one
  statement per pack.

A blob can become missing even though its bytes still exist elsewhere. If its row was in the forgotten pack, and its
bytes are also in another pack that has no row for it (the first location wins), it is missing too. The client uploads
it again.

### 5.3 Heal on reads and job starts [built]

The heal path that komira-ai/kbf#2380 and komira-ai/kbf#2382 built stays as it is, for absences seen outside an answer:
a client's ByteStream or BatchRead, and a job that failed to start on `source missing: <key>`.

- A first absence is noted, and a second one 10 to 60 min later confirms it.
- A pack recorded under an hour ago is never forgotten.
- A key is forgotten at most once an hour.
- A 5xx is never an absence.
- The memos and places naming the key are dropped before a job's retry.

With the check in place, these paths mostly find the pack already forgotten (`NotRecorded`), and a present check clears
their notes.

## 6. The collector: space only [proposed]

Because of the check, no deletion can make an answer wrong. The collector only keeps the bucket and the tables from
growing, in its simplest shape.

### 6.1 Use, by pack

A collector that judged by age alone would delete a toolchain's packs every retention and rebuild them. So it judges
by use. The check already names every pack an answer uses, so use is recorded **per pack**, not per blob:

- **The column.** `pack.last_used_us` (migration 5, `bigint NOT NULL DEFAULT 0`, expand only).
- **The notes.** Each pack the check finds present or takes from the cache, and each pack the Reader reads, is noted in
  memory. No I/O happens on the request.
- **The writes.** One task per frontend writes the notes every 60 s, with one
  `UPDATE pack SET last_used_us = now WHERE key = ANY($1) AND last_used_us < now - 1 h` per batch.

Packs are tens of thousands of rows, where blobs are millions.

### 6.2 The pass

A loop runs on the leader frontend only (the janitor's `is_leader`). It is off unless `--collect-retention-days` is
given. A value under 2 days is refused.

Every hour it examines the next 10,000 pack ids through a cursor, so a pass never reads the whole table. For each pack
whose `GREATEST(last_used_us, created_us)` is older than the retention:

1. **Rows.** Delete its `blob` rows, their `directory` rows and its `pack` row, in one transaction that re-checks
   `last_used_us`.
2. **Object.** Delete its object with the collector's own key (`--collect-key-file`). The key is delete-only, reaches
   beyond the janitor's `cas/b/` limit, and is used on the leader only. A failed delete is logged and counted. Nothing
   names the object any more.

A pack with no `blob` rows left is deleted the same way once its `created_us` is a retention old.

Counters: `kbf_collect_packs_total{answer=deleted|used|delete_failed}` and `kbf_collect_bytes_deleted_total`.

### 6.3 Interactions, each ending in a rebuild at worst

**A pack used again just as it is deleted.**

- A use note waits up to 60 s for its flush, so a pass can delete a pack that an answer found present moments before.
- That answer stays present for up to `PRESENT_FOR` (3.5).
- A job that reads the pack fails on its mount, evicts the key, and its retry answers MISSING.
- This needs a pack unused for the whole retention, used again within a minute of its deletion.

**A `cas/l/` key recorded again around its delete.**

- `put_large` or the tree's `place` asks the store for the length just before the delete, and records just after.
- The new row is recent, so it is trusted for `TRUST_RECENT`.
- A job reading it fails, as above.
- All three must hold at once: the blob was unused for the retention; it was used again inside the milliseconds
  between the collector's two steps; and a job read it within `TRUST_RECENT`.

**A job reading the pack when it goes.** It fails on its mount and retries into MISSING, as with any out-of-band
delete.

None of these needs ordering, a grace or a claim to stay correct. Each costs a retry and a rebuild.

## 7. The scenario, step by step

Action A ran long ago and produced output X, a blob in pack P. The collector, or someone else, deleted P. Later a build
of target B depends on A's output. Required: A is rebuilt, there is never a stale hit, and the build never just fails
on a mount.

**Day 0.** A runs as a job. Its ingest records P and X's row only after the store acknowledged P **[code]**. A's
`action_result` row is written last.

**Day 7.** P was unused all week. The collector deletes P's rows and its object (6.2). Had someone else deleted only the
object, the rows would still be there; the steps below are the same.

**Day 9, a fresh Buck2 daemon.**

1. Buck2 asks GetActionResult(A). The row is found, and the closure resolves X.
   - If the collector deleted P, X has no row. It is a miss **[code]**.
   - If someone else deleted P, X's row names P. P is neither recent nor cached, so the batch asks the filer. The
     filer answers absent and the HEAD agrees. P is forgotten (5.2), and it is a miss.

   Either way the answer is NOT_FOUND, never a stale hit.
2. Buck2 runs A. A's inputs went through the same checks: missing ones are uploaded again, or their producers rebuilt.
3. A's ingest writes X into a new pack P3 and records it; no stale row is left to conflict. `put_action` replaces A's
   row.
4. B's Execute walk resolves X to P3. P3 is recent, so it is present. B's job runs. **A was rebuilt, and B built.**

**Day 9, with the day-0 daemon still running and holding X's digest.**

5. Buck2 skips the action cache and sends B's Execute naming X. The walk finds X missing: either it has no row, or its
   pack is absent and is then forgotten. kbf answers FAILED_PRECONDITION with MISSING for `blobs/<X>` before any job
   is queued **[code]**. **No mount can fail.**
6. Buck2 cannot upload X, which it never downloaded. To build B it must run A again. Whether it does is not
   established in this repository, and its documents disagree. `storage-ram-ack.md` says REAPI clients re-upload on
   MISSING. `failure-classes.md` and `capabilities.md` say Buck2 treats FAILED_PRECONDITION as a user error. The farm
   test of phase 3 settles it.

## 8. Measurements

| Measurement | How | Decides | Status |
|---|---|---|---|
| Filer store, direct indexed query, 1,000 keys | by hand, on a farm node, warm | the lower bound of a batch | **[measured]** 3.3 to 4.2 ms |
| HEADs, 32 in parallel | by hand, on a farm node | the cost of the fallback | **[measured]** 200 keys in 143 ms |
| gRPC `LookupDirectoryEntries`, batches of 10, 100, 1,000 and 4,096, p50 and p99 | `cargo xtask filer-presence-bench` (phase 1a), from a frontend host to its filer | the gate of 3.6: a 1,000-key p99 under 20 ms | not measured |
| The filers' replication lag | the same verb: a PUT through one node's tierstore, then polling another node's filer until the key appears, repeated | `TRUST_RECENT` (3.4) | not measured |
| Keys per check after the recent rule and the cache | `kbf_pack_check_keys` | the cache's size and lifetime | phase 1b |
| Execute to enqueue, before and after phase 2 | `kbf_execute_presubmit_seconds` p50 and p99 | phase 2 stays on only if p99 grows by under 10 ms | phase 2 |
| Packs found missing; filer and HEAD disagreeing; checks unable to answer | `kbf_pack_check_missing_total`, `kbf_pack_check_disagreed_total`, `kbf_pack_check_unverified_total` | how often objects vanish; whether the local filer can be trusted; how often the store is unreachable | phase 1b |

## 9. What a client can see

| Situation | Answer | Worst case |
|---|---|---|
| Pack present | present | none |
| Pack deleted; its rows are gone, or the check finds it | missing | the client uploads again, or runs the producer again |
| Pack deleted within 30 s of a positive check on this frontend | present until the 30 s are up | a job fails on its mount; its retry answers MISSING; a rebuild |
| Pack recorded within `TRUST_RECENT`, then deleted out of band | present until the window ends | the same |
| Pack just written through another node, with the local filer behind | present (recent) | none |
| The local filer wrong or behind, past `TRUST_RECENT` | present (the HEAD confirms it) | none; counted as a disagreement |
| Store unreachable | from rows, counted | a lost pack is answered present while the store cannot be asked |
| Buck2 holds X's digest, and X was deleted | MISSING before any job | phase 3 decides. At worst the build fails with MISSING naming X, and a new daemon rebuilds it |

## 10. Phases: one PR each, with dependencies and tests

Testing rules for every phase:

- Tests run on injected clocks (`ManualClock`, the stored rig's `Rig::wait`) and fake stores.
- Fake filers listen on loopback, port 0.
- No test asserts timing.
- Pack-index tests are conformance rules, run against both the memory index and Postgres.
- Each test names the defect it catches.

| Phase | PR | Depends on | Adds |
|---|---|---|---|
| 0 | heal on reads and job starts | built: komira-ai/kbf#2380, komira-ai/kbf#2382 | `LossGuard`, `forget_pack` |
| 1a | the filer client and its measurement | none | `FilerPresence` (vendored `filer.proto`) in `kbf-objstore`; `cargo xtask filer-presence-bench`. Its numbers go into section 8 in a doc PR |
| 1b | the check, on the path that already asks the store | 1a | the `Presence` trait, `HeadPresence`, the positive cache, the recent rule and the HEAD confirmation; flags `--pack-store-filer` and `--pack-store-filer-root`; the start-up check; the action tree's check switched from one HEAD per pack to one batch; metrics `kbf_pack_check_seconds{via}`, `kbf_pack_check_keys`, `kbf_pack_check_missing_total`, `kbf_pack_check_disagreed_total` and `kbf_pack_check_unverified_total` |
| 2 | verify on every answer | 1b, and 1a's numbers meeting the gate of 3.6 | the check in FindMissingBlobs, GetActionResult, the Execute walk (files and Directories), job trees and memos, and `put_large`; the immediate forget of 5.2; memories held to `PRESENT_FOR`; cache eviction on a job's `source missing` |
| 3 | the held-digest farm proof | 2 | `cargo xtask prove-collect`, owner-run |
| 4 | the collector, for space | 2, and turned on in production only after 3's outcome is accepted | migration 5 (`pack.last_used_us`); the use notes and their flush task; the pass; flags `--collect-retention-days` and `--collect-key-file`; `kbf_collect_*` |

The farm runs of phases 1a and 3 need someone with access to the farm's nodes. Each PR updates `docs/api.md`,
`docs/deploy/kbf-modes.md` and the frontend golden config (`crates/kbf-cli/tests/golden/kbf-frontend.toml`) for what it
adds.

### Phase 0: heal on reads and job starts [built]

The tests live in `crates/kbf-pack/tests/pack/lost.rs`, `crates/kbf-front/tests/stored/lost.rs`,
`crates/kbf-front/tests/action_tree/lost.rs` and the executor's `front_lost_tests.rs`. Among them:

- `a_pack_absent_twice_some_time_apart_is_forgotten_whole`: catches one blob's row dropped while the pack's other rows
  still name a dead object.
- `an_absence_sooner_than_the_wait_forgets_nothing` and `the_second_look_decides`: catch a read path that forgets on a
  single 404.
- `a_store_that_answers_absent_while_a_replica_restarts_loses_no_pack`: catches a restart read as a loss.
- `a_missing_source_drops_the_memos_and_the_retry_does_not_reuse_its_pack`: catches a retry loop on a memoised tree.

### Phase 1a: the filer client and its measurement

- `a_lookup_names_every_key_under_the_root`, against a fake filer service: catches a key-to-path mapping that asks for
  the wrong entries, which would answer every pack missing.
- `a_batch_over_4096_keys_is_split_and_every_key_answered`: catches keys dropped past the filer's limit.
- `an_entry_shorter_than_asked_is_short`: catches a truncated object answered present.
- `the_bench_reports_a_lag_only_after_it_saw_the_key` and `the_bench_times_the_lookup_call_only`, in
  `crates/xtask/tests/filer_presence_bench.rs` over a fake filer and store: catch a bench that reports a number it did
  not measure.

### Phase 1b: the check

- `an_absence_the_head_contradicts_is_present_and_counted`: catches a lagging or wrong filer answered as missing.
- `a_filer_that_fails_a_batch_falls_back_to_heads` and `when_both_fail_the_keys_are_unverified_not_missing`: catch a
  filer fault turned into a rebuild storm.
- `a_recent_pack_is_not_asked`: catches a fresh output answered missing behind a lagging filer.
- `a_cached_key_is_asked_again_once_its_time_is_up` and `a_missing_key_is_never_cached`: catch a cache that hides a
  deletion past its bound, and one that hides a re-upload.
- `the_cache_holds_at_most_its_bound_oldest_first`: catches unbounded memory.
- `a_filer_that_cannot_find_the_selftest_key_is_not_used`: catches a wrong root or port.
- `a_checked_tree_asks_the_store_once_per_batch`: catches a regression to one HEAD per pack.
- `a_present_check_clears_a_heal_note`: catches a note made during a lag that outlives the lag.

### Phase 2: verify on every answer

- `find_missing_lists_a_blob_whose_pack_the_store_lacks`: catches the original stale "present".
- `an_action_result_over_a_missing_pack_is_a_miss` and `executes_own_cache_lookup_checks_its_packs`: catch a stale hit
  through either lookup.
- `an_execute_naming_an_input_whose_pack_vanished_is_missing_and_queues_no_job`: run for a file, for a Directory taken
  from a `directory` row, and for a Directory taken from the walk's cache. Catches a missing input reaching a mount.
- `a_job_tree_from_a_memo_checks_its_spans_whole_objects_and_manifests`: catches a memo naming a deleted object for its
  10 min.
- `a_tree_over_a_pack_found_missing_is_refused_at_once`: catches a tree refusal that still waits for heal.
- `a_missing_pack_is_forgotten_in_the_answer_and_the_upload_records_fresh`, driven through `Packs::put`: catches the
  re-upload loop that a kept row would cause.
- `put_large_over_a_missing_pack_writes_and_records_the_object`: catches "already stored" answered for a deleted
  object.
- `a_recent_pack_found_missing_is_not_forgotten`: catches a lag that costs a healthy pack its rows.
- `a_source_missing_report_evicts_the_key_and_the_retry_answers_missing`: catches a retry served from the cache, which
  would turn a deletion into a failed build.
- `the_walks_verified_memory_lasts_no_longer_than_the_cache`: catches a second, longer window.
- `the_action_cache_write_does_not_check`: catches kbf's own just-recorded outputs refused behind a lagging filer.
- `an_answer_checks_each_pack_once_however_many_blobs_it_holds`: catches per-blob checks.

### Phase 3: the held-digest farm proof

`cargo xtask prove-collect` sits beside `prove-farm` and uses its Buck2 project fixture, with two remote targets:

- `//:a` copies a nonce file, written by the verb for each case, to its output, so the verb knows X's digest.
- `//:b` reads `//:a`'s output.

The verb runs in no workflow. It is aimed at a farm the owner names, with the store key taken from the environment. It
is dry-run unless `--delete` is given. The steps:

1. `buck2 build //:a --materializations=none`. Buck2 now holds X's digest but not its bytes.
2. Delete X's pack object in the store, and nothing else.
   - The verb refuses a pack that holds any blob other than `//:a`'s outputs.
   - The rows stay, so it is the check that must notice.
   - The verb then waits `TRUST_RECENT` plus `PRESENT_FOR`, so that the pack is no longer recent and no cache can hold
     it.
3. `buck2 build //:b --materializations=none`, in the same daemon. kbf must answer B's Execute MISSING before any job
   is submitted. Pass: the build succeeds, and X is recorded in a new pack, which shows `//:a` ran again.
4. The same with a new nonce, and `buck2 kill` before step 3's build. Pass: the same, through the action-cache miss.

Every build must report `local: 0`.

What each step catches:

- **Step 2's refusal** catches a proof that deletes another build's bytes.
- **Step 3** catches the held-digest case ending in a failed build, or in a job started on a vanished input.
- **Step 4** catches a stale hit on the action-cache path.

The verb's own tests, over a fake Buck2, store and index:

- `without_delete_nothing_is_deleted`: catches a dry run that acts.
- `the_second_build_waits_out_the_recent_window`: catches a proof that cannot pass because the pack was still trusted.
- `a_second_build_that_fails_fails_the_proof`: catches a proof that passes a failed build.
- `a_rebuild_into_the_same_pack_fails_the_proof`: catches a pass with no rebuild.
- `any_local_action_fails_the_proof`: catches a run that built locally and proved nothing about the farm.

**If step 3 fails**, Buck2 does not run a producer again on MISSING when it holds the digest. Then:

- The MISSING message names the remedy: the input was deleted, and a new daemon (`buck2 kill`) rebuilds it, as step 4
  proves.
- Phase 4's collector is turned on in production only once that outcome is accepted.
- Deletions by others happen whether or not kbf collects.

### Phase 4: the collector

- `a_pack_unused_past_the_retention_is_deleted_with_its_rows`: catches a collector that leaks.
- `a_pack_used_after_it_was_examined_is_kept`: catches deleting a pack that came back into use during the pass.
- `a_check_or_a_read_notes_the_pack_used` and `a_pack_used_within_the_hour_is_not_written_again`: catch use that is
  never recorded, and one write per use.
- `a_pack_with_no_rows_left_is_deleted_once_old`: catches emptied packs kept for ever.
- `a_failed_object_delete_is_counted_and_its_rows_stay_gone`: catches a failed delete that loops or restores rows.
- `only_the_leader_collects`, `the_collector_without_a_retention_does_nothing` and
  `a_retention_under_two_days_is_refused`: catch two collectors, a collector on by default, or a retention shorter than
  a build.
- `an_old_output_collected_is_a_miss_and_its_rebuild_is_a_hit`, on the stored rig, covering section 7 steps 1 to 4:
  catches any break in the chain from collection to rebuild.
- `the_previous_release_runs_on_a_database_at_migration_5` and
  `this_release_runs_on_a_database_at_migration_4_with_the_collector_off`, in
  `crates/kbf-integration-test/tests/mixed_version.rs`: catch a migration an older release cannot open, and a new
  release that collects before its column exists.

## 11. Compatibility and failure modes

- **Migration 5** adds a column with a default (expand only). An older release never reads `last_used_us`, and loses
  nothing: it neither checks nor collects. A newer release on a database at migration 4 runs with the collector off,
  and logs that.
- **Mixed releases, phases 1 and 2.** An older frontend answers from rows, as today: no worse than before, and heal
  still covers it.
- **Mixed releases, phase 4.** An older frontend notes no pack use. The collector is turned on only once every frontend
  runs phase 4's release and its notes have run for one retention.
- **The filer down or misconfigured:** HEADs are used. If both fail, answers come from rows, counted (3.3).
- **Postgres restored from a backup:** rows of packs deleted since the backup come back. The check answers them missing
  on first use and forgets them. No procedure is needed.

## 12. What is not decided here

- Reading the filer's store directly. That is decided only if phase 1a's gRPC numbers miss the gate.
- The retention of `cas/l/` objects with no row (manifests, tree lists, placed copies), and of orphans outside `cas/b/`.
- Deleting `action_result` rows that can only miss.
- Compaction of packs that are mostly cold.
