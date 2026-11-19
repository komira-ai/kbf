# The pack index over the store: rows are a location cache, checked on every answer

Status: DESIGN (2026-11-19). Heal on reads and job starts (komira-ai/kbf#2380, komira-ai/kbf#2382) is built and is
reshaped by phase 2. Everything else is proposed and lands in the phases of section 9.

Claims are tagged:

- **[code]**: true at the commit this document was written against, with the file or function.
- **[buck2]**: read from Buck2's source at its release tag `2026-10-01` (the version kbf's release workflow pins).
  Paths are relative to that tree.
- **[measured]**: with how it was measured.
- **[proposed]**: not built.

`VERIFIED_FOR` and the filers' replication lag are provisional until phase 1a measures the lag.

Related:

- [metadata-in-postgres.md](metadata-in-postgres.md): the tables. Its section 2.3 is superseded by section 10 here.
- [actions-as-jobs.md](actions-as-jobs.md) section 6: what a client sees when a pack is lost.
- [trees-for-action-inputs.md](trees-for-action-inputs.md) section 5.4: its retention paragraph is superseded by
  sections 3 to 5 here.

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

> **Rule.** Every "do you have this" answer kbf gives a client is made only from packs **verified within
> `VERIFIED_FOR`**. Each pack row carries one clock, its last-verified time, which every frontend shares. A pack is
> verified when kbf's own PUT of it succeeds, or when a check finds it in the store. A pack verified longer ago is checked
> first, in **one batched existence check** per answer. A pack the store lacks is answered missing, and its rows are
> dropped. The client then uploads again or runs the producer again.

The answers that use the rule:

- GetActionResult, for the packs of the closure;
- FindMissingBlobs;
- the Execute input walk;
- the job tree's `locate`.

**The bound.** A pack deleted underneath kbf can be answered present for at most `VERIFIED_FOR` after its last
verification. The default is 5 min. It is one flag, and it may be set up to 1 h. That is the only window for a stale
hit.

**An "exists" answer is a promise.** A client trusts it for its own window: Buck2 does for `cas_ttl_secs`. So the
client's window is set equal to `VERIFIED_FOR` (6.2), and kbf itself deletes nothing in this design.

**A deletion by the store inside that window fails a build. It is not a rebuild.** Buck2 does not recover from MISSING
(section 6). The user reruns once the window has passed, which recovers an input the client still has (it uploads it
again). For an output the daemon holds only by digest, the user restarts the daemon (`./buck2 kill`) and builds again, which reruns the
producer.

For a client that asks again, the worst case is a rebuild.

Consequences:

- **"Rows name existing objects" is checked at answer time, not kept by construction.** A row naming a deleted object
  is harmless: the next answer that would use it finds the object gone and drops the row.
- **`action_result` rows stay authoritative.** They are in no object (`migrations/0003_action_cache.sql`) **[code]**, so
  they cannot be rebuilt from the store, and losing them means rebuilds. A row whose outputs are gone is a miss through
  the same rule, and the rebuild replaces it.
- **Collection is out of scope** (section 10). With the rule, a collector is needed only for space.

## 2. Where things are today

| What | Where | Status |
|---|---|---|
| Blob location: `blob (hash, size, pack_id, off, last_touch_us)`, primary key `(hash, size)`, index `blob_pack (pack_id)`; `pack (pack_id, key UNIQUE, writer, size, created_us, blob_count, live_blobs)` | `crates/kbf-pack-index/migrations/0002_pack_index.sql` | **[code]** |
| Recording: a pack is recorded only after the store acknowledged its create-only PUT; the first location of a blob wins | `Packer::commit`, `postgres.rs` `record_in` | **[code]** |
| FindMissingBlobs, the action-cache closure check (`closed.rs` `lacking`), the Execute walk and `put_large`'s early answer read rows only | `Packs::missing`, `Packs::locate`, `large.rs` `locate_stored` | **[code]** |
| The store is asked only by a checked action tree (one HEAD per pack) and by `locate` (HEADs of `cas/l/` objects) | `action_tree/check.rs`, `job_read/locate.rs` | **[code]** |
| The walk remembers a verified Directory for 45 s (`VERIFIED_TTL`); the action tree's memos keep manifests and places for 10 min (`MEMO_TTL`) | `input_walk.rs`, `action_tree.rs` | **[code]** |
| Heal: an absence seen on a read or a job's start is noted. A second look 10 to 60 min later confirms it, a pack under an hour old is never forgotten, and then the whole pack is forgotten (`forget_pack`, one transaction) | `crates/kbf-pack/src/lost.rs` | **[code]** |
| An Execute whose input is missing is answered FAILED_PRECONDITION with one MISSING violation per blob, before any job is queued | `execution.rs` `missing_status` | **[code]** |

Keys **[code]**:

- **Unique per write:** frontend packs `cas/f/<frontend>/<boot>/<seq>` and task packs `cas/b/<build>/<nonce>...`.
- **Deterministic:** `cas/l/<sha256>`, used for large blobs, action-tree manifests and tree lists.

## 3. The clock

### 3.1 One column, shared by every frontend

`pack.verified_us` holds the time the pack was last verified. It is added by migration 5 as
`bigint NOT NULL DEFAULT 0`, expand only.

**Every time comes from the database's clock.** Writes use the index clock (`now()` in the statement, or the injected
clock in tests), and `LOCATE` compares in SQL (`verified_us >= now - $window`). Clock skew between frontend hosts
therefore cannot stretch the window.

| Event | `verified_us` |
|---|---|
| kbf's own PUT of the pack succeeds, and it is recorded (`record_pack`, including a record of a key already present, such as a `cas/l/` blob uploaded again) | `now`, in the record's transaction |
| a check (3.3) finds the pack in the store | the time the check's probe **started**, written after the answer by one statement per check (in the background, not awaited). The statement writes a row only if that time is later than both the row's last verification and its last clear |
| a job reports the pack lost at mount (`source missing: <key>`), or a client's read of it is answered absent by the store | **cleared**, at once: it is set to minus the clear time, which `LOCATE` reads as not verified. A check that started before the clear cannot overwrite it, so kbf's retry, and the next answer, check the pack instead of trusting it |

The PUT counts as a check: the store has just acknowledged the object, so there is nothing to ask. It also guards a
just-written pack against the filers' replication lag:

- **A pack recorded within `VERIFIED_FOR` (by `created_us`, the time of its PUT) is present, even if its clock was
  cleared.**
- **Such a pack is never forgotten.**

So a job that fails to mount a fresh output on a node whose filer lags does not cost the pack. Its retry finds the pack
present by its PUT, and mounts once the filer has caught up. `VERIFIED_FOR` must therefore exceed the lag (3.4). This is
the only young-pack rule, and it uses the same window. The built heal's separate one-hour minimum age goes.

**What a clear reaches.** A clear reaches the frontend that saw the absence at once: `Front::lost` also drops its memos
naming the pack **[code]**. It reaches every other frontend through the column, the next time it reads the pack's
row. A frontend keeps at most **a per-process copy of the column**, the value its own `LOCATE` returned, and trusts it
no longer than `VERIFIED_FOR` from the time that copy holds. Another frontend's memo can therefore trust a cleared pack
until its copy's window ends. That stays within the bound of section 1.

### 3.2 What an answer does

Each answer resolves its digests to locations in one `LOCATE`-shaped statement, which also returns each pack's `key`,
`size` and `verified_us`. Then:

1. **Packs with `verified_us` within `VERIFIED_FOR` are present**, with no store call.
2. **The rest are checked in one batch** (3.3).

A blob is answered present only when its row exists and its pack is present.

| Answer | Packs it checks |
|---|---|
| FindMissingBlobs | the packs of the present blobs |
| GetActionResult (a client's, and Execute's own lookup) | the packs of every blob of the closure: files, stdout, stderr, Tree blobs and the files inside them. Any pack missing makes it a miss |
| The Execute input walk (`input_walk.rs`), before anything is queued | the packs of the input files **and of the input root's Directory blobs**, including Directories taken from `directory` rows or the walk's memory |
| A job tree (`place.rs`, the executor's `require_held`, a memo hit) | every object the tree names: the packs of its spans, the `cas/l/` objects it names whole, and its manifests |
| `put_large`'s early "already stored" | the large blob's own `cas/l/` key |

**Memories.** The walk's verified-Directory memory (45 s) and the action tree's memos (10 min) remember the
`verified_us` they rested on. An entry's packs count as present only until that time plus `VERIFIED_FOR`. After that
the packs are asked again: a memo entry still saves its manifest writes, but not the existence check.

**kbf's own write path is not checked.** The action-cache write's `checked`, and the records made by the ingest and
the Packer, read rows only. kbf has just written those packs and been told so by the store.

### 3.3 The check: the filer's batch lookup, an independent HEAD to confirm

**The batch.** The batch is the filer's `LookupDirectoryEntries` over gRPC (SeaweedFS 4.54, `weed/pb/filer.proto`).
One request carries up to 4,096 names and returns each entry with its attributes. Larger batches are split and sent at
once.

- **Mapping.** A key `cas/...` is the filer path `<root>/cas/...`. `<root>` is the bucket's directory, given by
  `--pack-store-filer-root` (the S3 gateway's `/buckets/<bucket>`).
- **Present** means the entry exists and its file size is at least the pack's recorded size.
- **The client** is generated from SeaweedFS's `filer.proto` (Apache-2.0) with the workspace's `tonic-prost-build`, in
  `kbf-objstore`.
- **Which filer.** The frontend names one filer with `--pack-store-filer <host:port>`: the one on its own host, or the
  nearest node's. A frontend always asks the same filer, so its answers do not flip with whichever filer was asked.

**Confirmation.** A key the filer answers absent, or shorter than recorded, is asked once more with a HEAD through the
existing store client (`ObjectStore::len`). That goes through tierstore and the S3 gateway, a second and independent
probe.

- **Absent or short there too:** the pack is missing.
- **Present there:** the filer was behind or wrong. The pack is present, and the disagreement is counted.

The HEAD runs only for absences.

**Fallback.** With no filer configured, or when the filer fails a batch, the batch is done with HEADs alone, 32 at a
time.

**At start.** The frontend writes its self-test key (`cas/f/<name>/<boot>/selftest`) **[code]**, waits `VERIFIED_FOR`,
and looks the key up through the filer. A filer that cannot find it is not used, and the frontend says why and uses
HEADs.

**What a check proves.** A filer entry or a HEAD proves the object's entry and its size, not that every chunk is
readable. A lost chunk under an intact entry shows up as an absence on a read, which clears the clock
(3.1), and the next check finds the entry present. So that this cannot loop, a pack cleared a second time within
`VERIFIED_FOR` of a check that found it present is treated as missing. Its entry exists, but its bytes cannot be read.
The `created_us` guard of 3.1 still applies, so lag alone never reaches this.

| The store answers | kbf answers | Rows |
|---|---|---|
| present | present | `verified_us` moves forward |
| absent or short, and the HEAD agrees | **missing** | the pack is forgotten now (section 4) |
| an error, a timeout or a refused call, on both probes | from rows, counted (`kbf_pack_check_unverified_total`) | unchanged |

**Why an error answers from rows.** A failed check is not an absence. The alternatives are worse:

- Answering missing would turn a store outage into a storm of uploads and rebuilds.
- Answering UNAVAILABLE would fail every build while the store is unreachable, and those builds cannot read their
  inputs anyway.

While the store cannot be asked, a lost pack is answered present. That window is the outage itself.

### 3.4 `VERIFIED_FOR`: the default, the flag, and what bounds it

`--pack-verified-for` defaults to 5 min. The frontend refuses a value over 1 h, or one not above the replication lag
that phase 1a measures. Two things bound it:

- **From above, the stale-hit window.** A pack deleted underneath kbf is answered present for at most `VERIFIED_FOR`
  after its last verification. A job that then reads it fails at mount, clears the clock (3.1), and its retry answers
  MISSING.
- **From below, the filers' replication lag.** A job's outputs are written through the node that ran it, and the next
  action's Execute names them seconds later. That client cannot upload them, since it never had them. The PUT verifies
  them, and that verification must outlast the time until the local filer has them. Otherwise a lagging filer would
  answer them absent.

  The HEAD of 3.3 usually catches that anyway, since it asks through another node. A lag past `VERIFIED_FOR` costs a
  rebuild, never a stale hit.

## 4. A pack found missing

### 4.1 The answer

Each answer uses the protocol's own form:

- **FindMissingBlobs** lists the blob.
- **GetActionResult** answers NOT_FOUND.
- **The Execute walk** answers FAILED_PRECONDITION with a MISSING violation per blob **[code]**, before any job is
  queued, so no mount can fail. Phase 2 adds the remedy to the message (section 6.3); the shape stays, because Bazel
  re-uploads only on that shape.
- **A job tree** is refused as lost (`NoTree::Lost`, MISSING to the client) as soon as the check answers missing. Today
  `check_objects` refuses only once heal has forgotten the pack **[code]**; in phase 2 the check's answer decides.
- **ByteStream and BatchReadBlobs** answer NOT_FOUND, as today **[code]**. Buck2 recognises only NOT_FOUND on a read as
  a lost artifact **[buck2]**.

### 4.2 The rows go at once: heal on the same clock

A pack the check found missing (the filer answered absent, and the HEAD agrees) is forgotten in the same answer, with
`forget_pack(key, probe_start)`. One transaction locks the pack row and removes every `blob` row of the pack and its
`pack` row, **only if** both of these hold:

- nothing verified the pack after the probe started: no PUT of the same key, and no other frontend's check;
- the pack was not recorded within `VERIFIED_FOR` (3.1).

Otherwise it does nothing. So:

- the client's upload is recorded fresh, with no stale row left to win the conflict;
- `put_large` finds no row, so it writes the object;
- the next answer does not check the dead pack again.

Heal has no rules of its own any more.

- **No separate young-pack rule.** A pack recorded within `VERIFIED_FOR` is never forgotten (3.1). That is the same
  window as the clock's, and it is the whole guard for a just-written pack.
- **No delayed second look.** The second look is the HEAD, an independent probe made at once.
- **One path for every absence.** An absence seen on a read or at a job's mount clears the pack's clock (3.1) and runs
  the check for that key. The check alone decides.

Phase 2 retires the built `LossGuard` timing for this (its 10 to 60 min second look, its one-hour minimum age and its
once-an-hour limit). The forget's condition on the probe's start time replaces the once-an-hour limit: a re-upload
that lands between a check and its forget is never undone.

Why a single look is enough now: the built heal waited because its absence came from a read, and a read can fail while
a volume server restarts. The check asks the filer's metadata and the S3 gateway, not a volume, and two independent
probes must agree. A volume restart does not make an entry vanish from either.

A blob whose row was in the forgotten pack, and whose bytes are also in another pack that has no row for it (the first
location wins), becomes missing too. The client uploads it again.

## 5. Why not the filer as the index

The check uses the filer as an **existence oracle**: kbf names a key, and the filer says whether it is there. Making the
filer the index stays rejected:

- **No refcounts.** Deleting a pack's entry never touches entries pointing into it (`weed/filer/filer_delete_entry.go`).
- **Attributes are not queryable by value.** "Every digest in pack P" has no index.
- **Still two records.** Pack objects and digest entries would have to agree, exactly as rows and objects must now.
- **About N lookups per FindMissingBlobs.** The batch lookup runs one point query per name, so N digests would cost N
  lookups. The oracle asks per pack, and only for packs not verified recently.
- **No multi-entry atomicity.** Recording a pack of N blobs would be N creates, where `record_pack` is one transaction.

## 6. What Buck2 does with MISSING, and what kbf does instead

### 6.1 From Buck2's source [buck2]

The scenario: a live daemon built action A remotely and holds its output X only as a digest (not downloaded). X's pack
is then deleted, and the same daemon builds B, which takes X as an input.

**Before Execute, Buck2 checks only inputs whose expiry it thinks has passed.**

- `app/buck2_execute/src/re/uploader.rs:96-116`: the margin is 1 s in OSS and 600 s at Meta.
- Outputs of a remote Execute, and of an action-cache hit, carry TTL 0 in the OSS client. That is
  `remote_execution/oss/re_grpc/src/client.rs:830` and `:714`, turned into the digests' expiry by
  `app/buck2_execute_impl/src/re/download.rs:327`. So X is checked on its next use, unless the client's own cache
  answers.
- That cache is an LRU of 500,000 FindMissingBlobs answers, both present and missing, cleared wholesale every 12 h
  (`client.rs:674-678`, `:1030-1080`).
- A present answer is taken as good for `cas_ttl_secs`, default 3 h (`client.rs:337`). During that time the digest is
  not asked about again.

**If FindMissingBlobs says X is missing**, the uploader finds no local bytes for a declared CAS artifact
(`RequiresCasDownload`, `uploader.rs:321-381`).

- **A ran 5 h ago or more, or has no recorded age:** B fails before Execute with `ReCasArtifactExpired`:
  "Your build requires an artifact that has expired in the RE CAS ... This error is currently unrecoverable. To
  proceed, you should restart Buck using `buck2 killall`" (`uploader.rs:335-358`, `:497-512`).
- **A ran under 5 h ago:** Buck2 logs a quiet soft error (`cas_missing`), skips X, and sends Execute anyway
  (`uploader.rs:359-371`). In OSS a soft error does not stop the build unless `BUCK2_HARD_ERROR` is set
  (`app/buck2_env/src/soft_error.rs`).

**When Execute reaches kbf and is answered FAILED_PRECONDITION with MISSING:**

- The OSS client never reads `PreconditionFailure`. No `.rs` file in the tree names it.
- Only the call that opens the stream is retried, and only on Unavailable, ResourceExhausted, Aborted, Cancelled and
  transient I/O errors (`client.rs:588-650`).
- The action fails at stage `remote_call_error` (`app/buck2_execute_impl/src/executors/re.rs:264-279`). Nothing
  re-uploads and nothing reruns A.
- With hybrid execution, Buck2 then tries B locally. Doing so must download X, and a NOT_FOUND there is
  `CasNotFoundError`, which also says "buck2 killall" (`app/buck2_execute_impl/src/materializers/deferred/io_handler.rs:257-264`).

**Nothing in OSS Buck2 notices a lost digest and reruns its producer.**

- No code path invalidates DICE on a CAS loss. The "restart" advice exists only in messages.
- The materializer's TTL refresh only moves expiry times. It is off by default, and in OSS it cannot extend a server's
  TTL (`app/buck2_server/src/daemon/state.rs:411-436`, `client.rs:1102-1110`).
- A failed action's error is not cached, so building again recomputes B. But A's result stays in DICE
  (`app/buck2_build_api/src/actions/calculation.rs:681-709`), so the same daemon fails B the same way every time.

**After a restart (`buck2 kill`, or `buck2 killall`), a fresh daemon asks the action cache for A before executing it.**

- `app/buck2_action_impl/src/actions/impls/run.rs:1168` and `app/buck2_execute_impl/src/executors/action_cache.rs:155-161`.
- The daemon's persisted materializer state (`buck2.sqlite_materializer_state`, on by default,
  `app/buck2_server/src/daemon/disk_state.rs:49-57`) keeps only materialized artifacts. It neither keeps a digest-only X
  nor skips A's lookup.
- With the rule of section 1, that lookup is a miss, because X's pack is found missing. A runs again, X is recorded
  anew, and B builds.

**Buck2's main branch (at `bd01284`) differs in how long it trusts an answer, not in the outcome.**

- Outputs carry `cas_ttl_secs` as their TTL, instead of 0 (`remote_execution/oss/re_grpc/src/client.rs:774-779`).
- The find-missing cache is cleared every `cas_ttl_secs`, instead of every 12 h (`client.rs:1028-1031`).
- A presence check answers any digest whose recorded expiry is more than 45 min away (`HEADROOM`) from that record,
  and asks about the rest (`app/buck2_execute/src/re/presence.rs:87`, `:171-205`).
- A finished Execute is run again only on UNAVAILABLE, ABORTED, UNKNOWN or INTERNAL. FAILED_PRECONDITION is not one of
  them (`client.rs:804`, held by a test in the same file).

With `cas_ttl_secs` at 5 min, every expiry is inside the headroom, so a digest is asked about again once the cache is
cleared, at most 5 min after the last answer. In the pinned release, the 12 h cache can answer "exists" for up to
12 h, whatever `cas_ttl_secs` says.

**So Buck2 does not recover by itself.** A live daemon that holds a deleted output's digest fails every build that needs
it, until it is restarted. The farm test of phase 0 checks this reading at runtime.

### 6.2 What kbf does instead

kbf cannot make a live Buck2 daemon rerun a producer. So it does three things.

**1. Match the client's trust window to kbf's.**

- kbf reports no TTL. FindMissingBlobs carries none, and kbf serves no `GetDigestsTtl` **[code]**. The OSS Buck2
  client therefore makes up its own window from `cas_ttl_secs` (6.1).
- Every committed Buck2 configuration that points at kbf sets `[buck2_re_client] cas_ttl_secs = 300`, equal to
  `VERIFIED_FOR`. In this repository that is the gamma fixture's `.buckconfig`, the overlay `prove-farm` renders, and
  the one `kbf-deploy pilot buckconfig` writes. The komira and buildrules repositories carry the same line, each in its
  own pull request.
- kbf pins a Buck2 release whose find-missing cache follows `cas_ttl_secs`, as main does. The pinned `2026-10-01`
  keeps a fixed 12 h cache.
- If `--pack-verified-for` is ever changed, the buckconfig line changes with it.

With both windows at 5 min, a client trusts an "exists" no longer than kbf would itself answer it without asking the
store.

**2. Say plainly what a deletion inside the window does.** It fails the build that needs the blob. Then:

- **An input the client still has** (a source): rerun after the window, 5 min. Buck2 asks again, is told missing,
  uploads the input, and the build succeeds.
- **An output the daemon holds only by digest:** restart the daemon (`./buck2 kill`) and build again. Buck2's own
  message says to restart it (with `buck2 killall`) when the producer is 5 h old or more. Otherwise the failure carries kbf's MISSING answer, whose message
  Buck2 keeps when it reports it (`app/buck2_execute/src/re/error.rs:97-118`). Phase 2 makes that message say both,
  for example: `1 input blob is not in the CAS (blobs/<hash>/<size>); the store deleted it. If you hold it, it is
  uploaded again when you rerun after 5 min; if it is an output this Buck2 daemon built earlier, run "./buck2 kill"
  and build again`.
- The status and its one MISSING detail per blob are unchanged, so Bazel and other clients still re-upload. An Execute
  answered MISSING is still right: it fails fast, before any job, and it is true. It is just not a recovery path for
  Buck2.

**3. Make the restart rebuild.** The fresh daemon's action-cache lookup of the producer is a miss (section 4), so the
producer runs again. That is the rule this design adds, and the farm test proves it.

**What is not done.** kbf does not rerun producers itself. To keep a daemon in the 5 h branch from failing,
FindMissingBlobs would have to answer "present" for bytes kbf no longer has, and rebuild them on demand. That breaks
REAPI's meaning of present, since a ByteStream read must be able to answer at once. It would also need an index from
output digest to action, recursion through lost inputs, and a producer that rebuilds the same bytes. If deletions
underneath kbf prove frequent, that is the next design. Until then, deletions are rare by assumption, and a deleted
output costs a held-digest daemon one failed build and a restart.

### 6.3 The farm test (phase 0)

This runs **before any code of this design is written**, with today's kbf, on a farm the owner names. It needs only:

- Buck2 at the pinned release;
- the gamma Buck2 project fixture of `prove-farm` (`crates/kbf-integration-test/gamma/buck2`);
- access to the farm's state store and store.

The steps:

1. Two targets on the fixture's remote-only platform (remote enabled, local disabled), so no hybrid fallback runs:
   - `//:a` copies a nonce file to its output;
   - `//:b` reads `//:a`'s output.

   Run `buck2 build //:a --materializations=none`. The daemon now holds X's digest only.
2. Delete X's pack, rows and object both. Use `forget_pack` through the state store, then a delete in the store. Check
   first that the pack holds only `//:a`'s outputs. Deleting the rows too makes today's kbf answer missing, which is
   what phase 2's check will do.
3. `buck2 build //:b --materializations=none` in the same daemon. Expected from 6.1: X is under 5 h old, so Buck2 skips
   it, sends Execute, kbf answers MISSING before any job, and the build fails at `remote_call_error`.
4. Build `//:b` again in the same daemon. Expected: the same failure. A stays cached in DICE.
5. `buck2 kill`, then `buck2 build //:b`. Expected: the action-cache lookup of `//:a` misses, `//:a` runs again, X
   is recorded in a new pack, and the build succeeds.
6. The 5 h branch: `buck2 kill`, write a new nonce, build `//:a` again (digest only), and delete its pack. Wait 5 h
   with the same daemon, then build `//:b`. Expected: `ReCasArtifactExpired` before any Execute reaches kbf.
7. The hybrid variant: steps 1 to 3 on a platform with local execution enabled. This records which error reaches the
   user, Buck2's `CasNotFoundError` or kbf's message. That decides whether kbf's message is seen at all.

8. The window: on a Buck2 with main's cache and `cas_ttl_secs = 300`, delete the pack of a *source* input of `//:b`,
   rows and object both. Build `//:b` at once. Expected: the build fails. Wait 5 min and build again. Expected: Buck2
   uploads the source again, and the build succeeds.

Each step's result, with Buck2's `Commands:` line and kbf's log for the action, is recorded in this document's
section 8.

- **If steps 3 to 8 match:** the design stands as written.
- **If step 5 does not rebuild:** the remedy of 6.2 is wrong. Nothing else is built until this document says what kbf
  does instead.
- **If step 3 recovers:** Buck2 reruns producers after all. Section 6.2's message change is then unnecessary, and is
  dropped.

## 7. Cost on the Execute to enqueue path

The goal is 100 ms from Execute to the first QUEUED (`kbf_execute_presubmit_seconds`). An answer whose packs are all
verified within `VERIFIED_FOR` makes no store call: one `LOCATE` statement, as today. Otherwise it makes one batched
call for the stale packs, plus a HEAD per absence.

Measured on a farm node (read-only, warm, by hand):

- **A direct indexed query of the filer's Postgres** for 1,000 pack keys under `cas/b/`: **3.3 to 4.2 ms**
  **[measured]**. That is the filer's own lookup work for one batch, without gRPC.
- **32 parallel HEADs**: **200 keys in 143 ms** **[measured]**. That is the cost of the fallback.

**The gRPC batch is not yet measured.** `LookupDirectoryEntries` runs each name as its own primary-key lookup, on at most
32 workers (`weed/server/filer_grpc_server_batch_lookup.go`), behind a gRPC hop.

Phase 1a measures it from a frontend host to its filer: the p50 and p99 of batches of 10, 100, 1,000 and 4,096 keys.
The gate is that phase 2 puts the check on the Execute path only if a 1,000-key batch's p99 is under 20 ms. If it is
not, the next step is chosen on those numbers:

- either read the filer's store directly, at the cost of depending on SeaweedFS's table layout;
- or move the walk's check after the enqueue, so that the job's checked tree catches a missing input instead.

## 8. Measurements

| Measurement | How | Decides | Status |
|---|---|---|---|
| What Buck2 does with MISSING under a live daemon | the farm test of 6.3 | whether 6.2 stands | read from source (6.1); not run |
| Filer store, direct indexed query, 1,000 keys | by hand, on a farm node, warm | the lower bound of a batch | **[measured]** 3.3 to 4.2 ms |
| HEADs, 32 in parallel | by hand, on a farm node | the cost of the fallback | **[measured]** 200 keys in 143 ms |
| gRPC `LookupDirectoryEntries`, batches of 10, 100, 1,000 and 4,096, p50 and p99 | `cargo xtask filer-presence-bench` (phase 1a), from a frontend host | the gate of section 7 | not measured |
| The filers' replication lag | the same verb: a PUT through one node's tierstore, then polling another node's filer until the key appears, repeated | the floor of `VERIFIED_FOR` (3.4) | not measured |
| Packs checked per answer, and the share answered from the clock | `kbf_pack_check_keys`, `kbf_pack_check_skipped_total` | whether 5 min is right | phase 1b |
| Execute to enqueue, before and after phase 2 | `kbf_execute_presubmit_seconds` p50 and p99 | phase 2 stays on only if p99 grows by under 10 ms | phase 2 |
| Packs found missing; filer and HEAD disagreeing; checks unable to answer | `kbf_pack_check_missing_total`, `kbf_pack_check_disagreed_total`, `kbf_pack_check_unverified_total` | how often objects vanish; whether the local filer can be trusted; how often the store is unreachable | phase 1b |

## 9. Phases: one PR each, with dependencies and tests

Testing rules for every phase:

- Tests run on injected clocks (`ManualClock`, the stored rig's `Rig::wait`) and fake stores.
- Fake filers listen on loopback, on port 0.
- No test asserts timing.
- Pack-index tests are conformance rules, run against both the memory index and Postgres.
- Each test names the defect it catches.

| Phase | PR | Depends on | Adds |
|---|---|---|---|
| 0 | what Buck2 does with MISSING | none; **before any code** | section 6.1 (this PR), and the farm test of 6.3, whose results a doc PR records in section 8 |
| 0b | the client's trust window | none | `cas_ttl_secs = 300` in kbf's committed Buck2 configurations (6.2), and the same line in the komira and buildrules repositories in their own PRs; the Buck2 pin moved to a release whose find-missing cache follows `cas_ttl_secs` |
| 1a | the filer client and its measurement | 0's results matching 6.1, or this document revised for them | `FilerPresence` (vendored `filer.proto`) in `kbf-objstore`; `cargo xtask filer-presence-bench`; a doc PR with the numbers |
| 1b | the clock and the check, on the path that already asks the store | 1a | migration 5 (`pack.verified_us`); the record sets it; the batch, the HEAD confirmation and the HEAD fallback; `--pack-store-filer`, `--pack-store-filer-root`, `--pack-verified-for`; the start-up check; the action tree's check switched from one HEAD per pack to one batch; metrics `kbf_pack_check_*` |
| 2 | verify on every answer | 1b, and 1a's numbers meeting the gate of section 7 | the rule in FindMissingBlobs, GetActionResult, the Execute walk (files and Directories), job trees and memos, and `put_large`; the forget of 4.2; read and mount absences clearing the clock; the `LossGuard` timing retired; the MISSING message of 6.2 |

Phase 0's farm test, and phase 1a's bench run, need someone with access to the farm's nodes. Each code PR updates
`docs/api.md`, `docs/deploy/kbf-modes.md` and the frontend golden config
(`crates/kbf-cli/tests/golden/kbf-frontend.toml`) for what it adds.

### Phase 0: what Buck2 does with MISSING

Section 6.1 is the source reading. The farm test of 6.3 checks it, and each of its steps names what it catches:

- **Step 3** catches a design that assumes Buck2 recovers on MISSING.
- **Step 4** catches a design that assumes building again is enough.
- **Step 5** catches a remedy that does not rebuild: the action cache answering a stale hit after the restart.
- **Step 6** catches the 5 h branch behaving differently from the source.
- **Step 7** catches a remedy message the user never sees.
- **Step 8** catches a client that trusts "exists" longer than kbf's window.

### Phase 0b: the client's trust window

- `the_committed_buckconfigs_trust_exists_for_verified_for`, a repository lint over the gamma fixture's `.buckconfig`,
  the `prove-farm` overlay and the pilot overlay, comparing their `cas_ttl_secs` with the default of
  `--pack-verified-for`: catches a client window that drifts from kbf's.
- The `prove-farm` and pilot overlay tests assert the line: catches an overlay that drops it.

### Phase 1a: the filer client and its measurement

- `a_lookup_names_every_key_under_the_root`, against a fake filer service: catches a key-to-path mapping that asks for
  the wrong entries, which would answer every pack missing.
- `a_batch_over_4096_keys_is_split_and_every_key_answered`: catches keys dropped past the filer's limit.
- `an_entry_shorter_than_asked_is_short`: catches a truncated object answered present.
- `the_bench_reports_a_lag_only_after_it_saw_the_key` and `the_bench_times_the_lookup_call_only`, in
  `crates/xtask/tests/filer_presence_bench.rs` over a fake filer and store: catch a bench that reports a number it did
  not measure.

### Phase 1b: the clock and the check

- `a_recorded_pack_is_verified_by_its_put`, and `recording_an_existing_key_again_moves_its_clock`: catch a fresh output,
  or a re-uploaded `cas/l/` blob, checked against a lagging filer.
- `a_pack_verified_within_the_window_is_not_asked` and `a_pack_verified_before_it_is_asked_and_its_clock_moves`: catch a
  window that is ignored, or one that never closes.
- `a_check_overtaken_by_a_clear_does_not_restore_the_clock`: catches a late background write that hides a mount
  failure.
- `the_window_is_measured_on_the_index_clock`, with frontends on skewed clocks: catches a host's clock stretching the
  bound.
- `the_window_flag_refuses_more_than_an_hour`: catches a stale-hit bound larger than the rule allows.
- `an_absence_the_head_contradicts_is_present_and_counted`: catches a lagging or wrong filer answered as missing.
- `a_filer_that_fails_a_batch_falls_back_to_heads` and `when_both_fail_the_keys_are_unverified_not_missing`: catch a
  filer fault turned into a rebuild storm.
- `a_filer_that_cannot_find_the_selftest_key_is_not_used`: catches a wrong root or port.
- `a_checked_tree_asks_the_store_once_per_batch`: catches a regression to one HEAD per pack.
- `the_clock_written_by_one_frontend_is_used_by_another` (two `Packs` over one index): catches a per-process clock that
  makes every frontend check every pack.
- Compatibility, in `crates/kbf-integration-test/tests/mixed_version.rs`:
  `the_previous_release_runs_on_a_database_at_migration_5` and
  `this_release_runs_on_a_database_at_migration_4_answering_from_rows`. These catch a migration an older release cannot
  open, and a new release that fails without its column.

### Phase 2: verify on every answer

- `find_missing_lists_a_blob_whose_pack_the_store_lacks`: catches the original stale "present".
- `an_action_result_over_a_missing_pack_is_a_miss` and `executes_own_cache_lookup_checks_its_packs`: catch a stale hit
  through either lookup.
- `an_execute_naming_an_input_whose_pack_vanished_is_missing_and_queues_no_job`, run for a file, for a Directory from a
  `directory` row, and for a Directory from the walk's memory: catches a missing input reaching a mount.
- `a_memo_entry_is_trusted_only_while_its_clock_is`: catches a memo naming a deleted object for its 10 min.
- `a_tree_over_a_pack_found_missing_is_refused_at_once`: catches a tree refusal that still waits for heal.
- `a_missing_pack_is_forgotten_in_the_answer_and_the_upload_records_fresh`, driven through `Packs::put`: catches the
  re-upload loop that a kept row would cause.
- `put_large_over_a_missing_pack_writes_and_records_the_object`: catches "already stored" answered for a deleted
  object.
- `a_mount_failure_clears_the_clock_and_the_retry_answers_missing`: catches a retry that trusts a pack a job just found
  lost, which would turn a deletion into a failed build.
- `a_read_absence_clears_the_clock_and_runs_the_check`: catches a second heal path with rules of its own.
- `a_restarting_volume_server_forgets_nothing`: reads fail while the filer and the HEAD answer present. Catches the
  single look forgetting a healthy pack.
- `a_fresh_pack_that_fails_at_mount_is_kept_and_present_by_its_put`: catches filer lag costing a job's new outputs.
- `a_forget_after_a_reupload_does_nothing`: a PUT of the same key lands between the check and its forget. Catches a
  forget that undoes a re-upload.
- `a_pack_whose_reads_keep_failing_while_its_entry_exists_is_forgotten`: catches the lost-chunk loop.
- `the_missing_message_names_the_blob_and_the_remedy`, asserting the whole message by equality and that its detail is
  still exactly one MISSING violation per blob: catches a message that does not tell a Buck2 user how to recover, or a
  detail change that stops Bazel re-uploading.
- `the_action_cache_write_does_not_check`: catches kbf's own just-recorded outputs refused behind a lagging filer.
- `an_answer_checks_each_pack_once_however_many_blobs_it_holds`: catches per-blob checks.

## 10. Out of scope: collection

Nothing in this design deletes packs. The store may delete them underneath kbf, and the rule makes that safe. A
collector for space comes later, in its own design. With the rule in place it needs no ordering, grace or claim for
correctness. Two things will matter for it:

- **It must judge by use, not age**, or it rebuilds every toolchain each retention. Each successful check and each
  PUT already move `verified_us`, so a collector can read use from that column. A pack used within `VERIFIED_FOR`
  updates nothing, so the column's precision is `VERIFIED_FOR`, which is enough against a retention measured in days.
- **It must never delete a pack a client may still trust.** Every present answer is made within `VERIFIED_FOR` of the
  pack's `verified_us`, and a client trusts it for `cas_ttl_secs` (equal to `VERIFIED_FOR`). Buck2 also allows its 45
  min `HEADROOM`. So a pack whose `verified_us` is within 2 × `VERIFIED_FOR` + 45 min is never deleted.
- **What 6.1 means for it.** Every pack it deletes may be an output some live Buck2 daemon holds. That costs that
  daemon a failed build and a restart (6.2). Its retention should be long against how long builders keep daemons.

metadata-in-postgres.md 2.3's sketch of a collector is superseded by this section. Until that design exists, no pack is
collected.

## 11. Compatibility and failure modes

- **Migration 5** adds a column with a default, expand only. An older release never reads `verified_us` and loses
  nothing: it neither checks nor sets the clock. A newer release on a database at migration 4 answers from rows, as
  today, and logs why.
- **Mixed releases.** An older frontend answers from rows, as today: no worse than before, and its heal still runs.
- **The filer down or misconfigured:** HEADs are used. If both fail, answers come from rows, counted (3.3).
- **Postgres restored from a backup:** rows of packs deleted since the backup come back, with old `verified_us`. The
  next answer that uses them checks them, finds them missing, and forgets them. No procedure is needed.

## 12. What is not decided here

- Reading the filer's store directly. That is decided only if phase 1a's gRPC numbers miss the gate.
- kbf rerunning producers itself (6.2). That is decided only if deletions underneath kbf prove frequent.
- Collection (section 10).
