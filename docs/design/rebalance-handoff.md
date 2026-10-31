# Rebalance and handoff (av2-13)

Status: design. Nothing in this document is built unless it says "today". It replaces the
"handoff copy ... is not built" notes in `architecture-v2.md` (sections 6, 8.4 and the P6 row)
with a mechanism, and says what each pull request changes. Every claim about the code under
"Verified facts" was read from `feature/arch-v2`; where the first draft of this design was
wrong, the correction is marked **Corrected**.

## 1. The problem

A node joins or drains: the map moves to generation g+1 and some digests change owners.
Today:

- A joiner registers `joining` with an `unfilled/<id>` mark when the farm holds data. It takes
  writes, is never rank 1 and is read last. Nothing fills it but the writes it takes, so an
  operator promotes it by hand (`POST /v1/members/{id}`, `Members::set_state`). A joiner
  promoted early reads as missing for blobs that only an old owner holds, because the router
  keeps one previous map.
- A drain stops writes to the node and reads it first, but nothing tells the node when it may
  drop a blob, and nothing moves a staged (not yet flushed) blob off it.
- The one-flusher gate (`OneFlusher`) lets only rank 1 of `write_owners` flush a staged object.
  A Draining or displaced node is never rank 1, so its staged blobs are stranded.

The spec red test is: a join in the middle of a build never yields a false miss; a node keeps
a blob until the new owner confirms it; a rebuild after a wipe stays catching-up; and the
rebuild never collides with keys written by old servers.

## 2. Mechanism: cold store first

The cold store is the handoff channel. A blob is handed off when its bytes are durable in the
cold store (flushed and read back) and the new owner has adopted the object that holds it into
its index. No new node-to-node byte stream is needed, a crash at any step loses progress and
never data, and the three cases a copy-between-nodes design misses are covered:

- **Orphaned staged blobs.** Any node that is not a write owner of a digest flushes it (P1).
- **Blobs staged after the rebuild.** The index rebuild of a joiner is one-shot. Objects that
  other members flush after it are adopted by a standing tail pass (P3).
- **r=1.** A joiner owns nothing under the current map at r=1, so a fill check against the
  current owner sets is vacuous. The check is made against the map computed as if the joiner
  were Active (the `owners_assuming` addition in PR 1 of section 9).

Primitives, all additive:

- **P1 orphan flush.** A node flushes a staged object when it is rank 1 for the digest, or
  when it is not a write owner of it at all (this includes Draining). Duplicate flushes are
  harmless: object keys are disjoint (`cas/<member>/<boot>/<seq>`); the cost is PUT bytes,
  bounded by the staged volume.
- **P2 `flush_clear_gen`.** The highest generation g such that every object the node staged
  before it held map g is flushed. It is node-local and volatile. Objects are dated in memory by
  the generation the gate held at their first staging; an object nobody dated (staged before a
  restart, or while no map was held) counts as generation 0, so a restart holds the value back
  and never lets it run ahead. Only objects this node flushes under its current map count (a
  replica's copy of a primary's object would otherwise hold it back forever), so the value can
  go down when a map change makes an object this node's. `flush_known=false` says "cannot say"
  and blocks the barrier.
- **P3 tail adoption** (`kbf-front/src/cache/tail.rs`). Lists the cold store for objects
  written by other members and commits `AdoptSegment` through the path the rebuild uses.
  **Corrected:** the rebuild resumes from one lexicographic key, which is sound only because it
  is one pass. A standing tail cannot keep a cursor at all (as built in PR 4): keys sort by
  member, then boot, then seq, but uploads finish in no seq order (`puts_in_flight`), so a
  segment numbered below one already listed can appear later, and a cursor past it would never
  read it. Each pass lists the whole `cas/` prefix and skips what a volatile `TailState` holds
  (the handled seqs of each writer, kept as ranges), so a pass reads one footer per new object.
  It never reads an object of its own member (its puts index it; an earlier boot's are the
  rebuild's) and does not list a frozen old-layout prefix. A restart loses the state and reads
  every object once, which adopts nothing new for a log that holds them. The listing is not a
  snapshot, so a second pass that finds nothing it had not handled, after a first that adopted no
  object, is the signal (`TailReport::quiet`) that a put the first listing missed is not hiding.
- **P4 epoch fence.** `meta/tail_epoch` in the state store plus per-node acknowledgements
  (`tail_epoch_done`). No clocks are involved in the ordering argument.
- **P5 durable `handoff/base` pin** of the pre-change map, shipped first and independently.

Additions to `kbf-map`: `Map::owners_assuming(id, Active)`, `placement_fingerprint`,
`read_order_with_history`.

Grafts from two other designs that were considered:

- From pull-by-new-owner: the placement fingerprint and generation on every fill and fence
  report, so a stale report is rejected; a settle timer; a `degraded` flag that blocks
  automatic promotion; an in-transaction check that the row is not Draining plus a mark
  version check; a second tail pass; the pin cap.
- From push-by-previous-owner: a sampled audit, an epoch barrier, a per-blob
  `FindMissingBlobs` re-confirm with an owner-boot check just before any drop, the full drop conditions, and the drain surface (`drained`,
  `finish_drain`, `--exit-on-drained`).
- A fallback for a per-node `file://` cold store (section 9, PR 7).

## 3. Definition of filled

A joiner is filled when all of these hold:

1. Its index is rebuilt (the rebuild of its meta log is done, `CaughtUp`).
2. Every object staged anywhere before the barrier is flushed: every fresh member that can
   flush, whatever its state, reports `flush_clear_gen >= g_j`.
3. A tail pass that started after that barrier has finished on the joiner
   (`tail_epoch_done >= e`).
4. The joiner's fill report carries the current placement fingerprint, computed with the
   joiner assumed Active.
5. A sampled audit confirms that the joiner indexes the sampled digests.

**Corrected (audit).** The first draft assumed an ordered scan of the local index. There is
none: `MetaState.blobs` is a private `BTreeMap<Digest, BlobEntry>` and no public method
enumerates or ranges over it. The audit therefore needs a small new reader, a
`MetaState::sample_digests(from, n)` that returns up to `n` digests at or after `from` in
digest order (PR 1b below). The audit samples from the objects the tail adopted (their
footers list their digests), asks the joiner's own index through `blob()`, and for the owner
view asks `find_missing` on the joiner. The strong guarantee is the barrier, the fence and
the fingerprint match; the audit is defence in depth and is probabilistic.

**Corrected (listing consistency).** The fence argument needs a listing that started after a
PUT was acknowledged to show it. The code states the opposite as the contract: the object
store "needs no consistent listing and no read-after-write", and a listing may miss writes made
while it runs. `file://` gives the property in practice (a fresh listing after a synced write
re-reads every header); no source on the branch states it for S3, MinIO or RustFS. The design
therefore does not assume it silently: PR 4 adds a conformance case, `list_sees_completed_put`,
to `kbf-objstore`'s suite and runs it in the S3 job; a backend that fails it cannot enable
auto-promotion; and the second tail pass (section 4) is the guard against a lagging listing.

## 4. State machine and marks

Map states are unchanged: `joining`, `catching-up`, `active`, `draining`, `down`.

### Join

The `unfilled/<id>` mark, today an empty presence mark, becomes `{g_j, fp, phase, epoch, fence_boots}`.
A reader that predates this sees a mark and treats the node as unfilled, as today.

- **Phase A, flushing.** The joiner is `joining`, takes writes, is never rank 1, is read last,
  and runs its index rebuild (a node registering with an unfinished rebuild registers
  rebuilding and is written `joining` when the farm holds data; this exists today). A to B
  fires in the sweeper's `refresh` when every fresh member has `flush_clear_gen >= g_j`,
  whatever its state: Active, Draining, Joining and CatchingUp, including the joiner itself and
  any second concurrent joiner. P1 lets a node of any state flush a blob it does not own, so a
  second joiner can hold staged blobs of other owners; counting only Active and Draining
  members would let the barrier pass while those blobs are still staged. A member that has
  not yet reported `flush_known` is not fresh and blocks. A Down member still inside its dwell
  blocks it. The frontend then
  bumps `meta/tail_epoch` to e, writes `{phase=B, epoch=e, fp}` and writes `handoff/base` if
  absent.
- **Phase B, adopting.** B to Active runs in the joiner's own beat, inside `touch`, as one
  compare-and-swap, when all hold:
  - `--auto-promote` is on;
  - the index is `CaughtUp` and `tail_epoch_done >= e`;
  - the report's fingerprint equals the frontend's (assuming Active);
  - the farm is not `degraded` (no member was Down or missing from the source set during the
    fence), unless `--handoff-allow-degraded`;
  - the settle time has passed since all of these first held;
  - the sampled audit passed;
  - the row is still `joining` (not `draining`) and the mark version matches.
- **Owner boots at the fence.** When the frontend bumps `tail_epoch` to e it records, in the
  mark body, the boot of every write owner it will later ask (Active, Joining, CatchingUp) as
  `fence_boots`, read from the map it bumped under. A node's `tail_epoch_done` ack carries the
  boot that did the pass, and the ack is accepted only if it equals the recorded boot; an
  ack from another boot is stale (the index of a restarted node may be rebuilding). The drop
  re-confirm (below) checks the same recorded boots.
  Liveness: this does not stall. If an owner's boot in the map differs from `fence_boots` after
  the fence (a restarted owner, or a joiner that registered after it), the frontend re-bumps
  `meta/tail_epoch` to e' > e and records the current boots as the new `fence_boots`. Every
  owner then acks e' under its present boot, and the phase completes under the new epoch. The
  join's `tail_epoch_done >= e` and the drain's B to C condition use the current epoch.
- The swap deletes `unfilled/<id>` and writes `meta/members_formed` in one transaction.
- **Pass 2.** After the first tail pass succeeds the joiner repeats an incremental pass;
  `complete` requires zero new adoptions, or a restart of the pass if the fingerprint changed.
  This is the guard for a listing that lags a PUT.

### Drain

The `draining/<id>` mark holds `{g_d, fp, phase, epoch, fence_boots}`; `fence_boots` is written with the A to B bump.

- **A.** The drainer takes no writes and is read first. P1 makes it flush everything it stages.
- **A to B.** When its `flush_clear_gen >= g_d` the frontend bumps `tail_epoch` and writes
  `handoff/base`.
- **B to C (confirmed).** Every fresh member that takes writes (`takes_writes`, that is every
  member except Draining: Active, Joining and CatchingUp) has `tail_epoch_done >= e` with a
  matching fingerprint. Joining and CatchingUp are write owners (`map.rs:55-57`), so they are in
  the owner set and the drain waits for them. Excluding them as "never read-primary" is unsafe:
  a blob they own and have not adopted would be dropped, and the next read-primary promotion
  would find it nowhere but the cold store. A Down member inside its dwell blocks, as for a join.
  Further, none is degraded, and the settle and pin-release conditions hold. The heartbeat
  response then carries `drain_confirmed`.
- **C.** The node evicts only objects that meet every drop condition below, reports
  `drained=true`, and the operator runs `finish_drain` (or `--drain-auto-leave` does);
  `--exit-on-drained` exits the process.

Drop conditions, all required:

1. The object is flushed and the cold-store read-back (digest verified) succeeded.
2. The object is not staged.
3. The node is not a write owner of any of its blobs, or is Draining. For a drain the Draining
   carve-out always holds (a Draining member takes no writes), so this condition is redundant
   there and adds nothing to the safety argument; it matters only for a drop that is not a
   drain, which section 8 says this design does not change.
4. The epoch barrier holds: the B to C condition above (every write owner reports
   `tail_epoch_done >= e` with a matching fingerprint), so each has adopted every blob listed
   at epoch e.
5. The per-blob re-confirm, run by the drainer per object just before dropping it: for the
   object's blob digests, send each current owner (from the current map, including Joining
   and CatchingUp) a `FindMissingBlobs` request in batches of at most 1000 digests, and require
   an empty missing set from every owner.
6. The boot check: each owner's boot, taken from the map, equals its boot in `fence_boots`
   (the boot recorded at the epoch fence). A restarted owner, whose index may be rebuilding,
   fails the check and the object is retried later.
7. `EVICT_GRACE` has elapsed.
8. The drop delay has elapsed.

Bandwidth bound: at most one cold-store read per dropped object and ceil(blobs/1000) x owners
small RPCs per object, rate-limited by the drop delay and a per-drain cap of 4 concurrent
objects; the heartbeat carries no per-blob data. The audit stays a probabilistic extra, not
part of the guarantee. The drop never deletes from the cold store. Reclaim outside a drain is
out of scope (section 8 says what disk-pressure eviction does).

### Pin and its release

`handoff/base` pins the map before the change so reads can fall back to it. Today the previous
map is one deep in all five places that hold it, so two quick map bumps lose the generation
before them. The pin is released on a signal, not a dwell guess: the fence is closed at
generation >= g_p and every frontend has reported, in a new `seen_gen` field, a generation at
or beyond g_p. A dwell stays only as a floor (PR 2 as built releases on the dwell alone; the
`seen_gen` signal arrives with PR 6). The cap is 4 pins, not 4 handoffs: each map bump writes
the map it replaces as `handoff/pin/<generation>` in the same transaction, and the bump past 4
deletes the oldest, so a register bump and a promote bump of one join use two of the four. While 4 pins
are open, a new instance joining a farm that holds data, and a drain of a member that is not
draining yet, are refused (`MembershipError::PinsFull`, gRPC RESOURCE_EXHAUSTED, HTTP 409);
a refused joiner gets no identity, row or mark. Let through: a known member registering again,
a repeated drain, a lapsed pin, and any join or drain while the farm holds no data yet.
Refusing is safe; the operator retries after pins lapse.

## 5. Protocol and flag changes (all additive)

Optional fields with new numbers; an old peer ignores them.

- `HeartbeatRequest` (fields 1 to 14 exist: 1 to 10 end at `writes_known`, and PR 3 (#1293) took 11 to 14 for `flush_clear_gen` 11, `flush_known` 12, `staged_objects` 13, `staged_bytes` 14; PR 4 (#1294) takes `tail_epoch_done` = 15, the ack that carries the node's boot, which the heartbeat already sends as `boot`; the later fields `fill_fp`, `drained` and `seen_gen` (sent by map consumers that are frontends) take 16 and up).
- `HeartbeatResponse` (fields 1 to 4 exist: `map_generation` 1, `map` 2, `objstore_generation` 3, and `retained_previous` 4 from PR 2 (#1296); PR 4 takes `tail_epoch` = 5; `drain_confirmed` takes 6 and up).
- **Corrected.** `map.proto` has no separate map-fetch rpc: `service Membership` has
  `Register` and `Heartbeat`, and the map rides in `RegisterResponse.map` and
  `HeartbeatResponse.map`. The `retained_previous` field therefore goes on those two
  responses (`RegisterResponse.retained_previous` = 6, `HeartbeatResponse.retained_previous` = 4),
  not on a `MapResponse` message.
- Store keys: `handoff/base`, `meta/tail_epoch`, and the mark bodies above.
- `kbf-map`: the three additions in section 2.
- HTTP: `GET /v1/members` gains `phase`, `epoch`, `blocked_on[]`, `degraded`, `drained`; a
  `finish_drain` verb.
- Flags (not environment variables): `--auto-promote` (default off), `--handoff-settle-secs`,
  `--handoff-release-secs` (the floor), `--tail-interval-secs`, `--adopt-bytes-per-sec`,
  `--drain-auto-leave`, `--exit-on-drained`, `--handoff-allow-degraded` (default false),
  `--handoff-node-pull` (default off).
- Mixed versions: an old frontend drops the new row fields, which stalls a handoff and never
  corrupts one. Roll frontends first, then nodes, then turn on `--auto-promote`. An old
  frontend never auto-promotes; manual `active` still works.

## 6. What changes in the manual promotion of #1156

- Today a joiner stays `joining` until an operator posts `active`; the `unfilled/<id>` mark is
  cleared only there.
- After: the joiner is promoted inside its own beat when section 3 holds and `--auto-promote`
  is on. The swap is the same transaction as `set_state(Active)` (mark delete,
  `members_formed`, version check) with the added not-Draining check.
- Unchanged: join semantics (takes writes, never rank 1, read last), the mark as the only
  gate, and `POST active` as the override and rollback. The default is off until soak, so the
  first release behaves as today plus the durable pin.
- New: `blocked_on[]` and `degraded` say why a node is not promoted.

## 7. Red tests

Tests are written first and must fail on today's code before the change.

Spec tests:

1. **A join mid-build never gives a false miss.** Stage blob B on A, unflushed. Join J while it
   rebuilds. Reads find B through promotion, two further generation bumps and a frontend
   restart; zero `NOT_FOUND`. Mutants that must go red: a 1-deep previous map; no flush
   barrier.
2. **A node holds a blob until the new owner confirms.** Drain with the flush held by fault
   injection: no drop. Release it: the fence closes and the drop happens. Mutants, each red:
   drop at Draining; ignore one member's ack; drop Joining and CatchingUp owners from the ack set (scenario: a Joining owner holds the blob, so the per-blob confirm passes, but it has not acked the epoch; the test asserts `drain_confirmed` (B to C) is withheld, and the mutant makes it fire); drop Joining and CatchingUp owners from the confirm set (each separately: a blob only a Joining owner lacks must block the drop); skip the re-confirm; key the confirm without the
   boot; skip the release signal.
3. **A rebuild after a wipe stays catching-up.** Exists today (see Verified facts). Added: the
   tail does not skip the rebuild, and a joiner wiped mid-fill is not reported complete.
4. **No key collision with old-server objects.** Exists today for the rebuild. Added: the tail
   adopts old-layout objects once, never re-adopts `cas/<self>/`, and ignores frozen
   old-layout prefixes.

Extra tests:

- Restart mid-fill: the joiner's `tail_epoch_done` and `complete` reset, it redoes the pass,
  and a stale report is rejected by fingerprint or freshness.
- Frontend swap mid-fill: another frontend finishes from store state; two frontends cannot
  double-promote (mark version); a concurrent drain request beats an auto-promotion beat.
- Previous owner down: a Down member inside its dwell blocks A to B; after the dwell the join
  proceeds with `degraded` set and auto-promotion refuses unless allowed; manual `active`
  works; a returning node flushes under P1 and the tail adopts its objects.
- r=1: promotion strands no staged blob; the fill check uses `owners_assuming`.
- Stale fingerprint: a map change mid-fill invalidates the report and restarts the fence.
- Sampled audit: a planted skipped blob is caught (mutant: audit disabled, promotion wrongly
  proceeds).
- Mixed frontend: dropped fields cause no auto-promotion; manual `active` works.
- Pin cap: with 4 pins open a join of a new instance and a drain of a non-draining member are refused and leave no identity, row or mark; a known member re-registering, a repeated drain, a lapsed pin and a forming farm are let through.
- Orphan flush: the expectation in `flush/tests.rs` that a Draining member stops flushing is
  rewritten (Draining and displaced nodes now flush); duplicate flush is harmless.
- Late lower-seq segment: a segment numbered below one already handled, uploaded after the
  first pass, is adopted by the next pass (mutant: `TailState::holds` true for any seq below a
  range end, which is what a cursor does).
- Fence boots liveness: an owner restarts (new boot) after the fence; the frontend re-bumps
  `tail_epoch` and re-records `fence_boots`, the owner acks the new epoch under its new boot,
  and the join completes (or the drain reaches C) instead of stalling. Mutant: never re-record
  `fence_boots` (the old-boot ack is rejected forever and the test times out red).
- Listing conformance: `list_sees_completed_put` on every backend that enables auto-promotion.

## 8. Risks

- Liveness over safety: a slow or Down member blocks promotion and drain. `blocked_on[]` and
  the manual override are the mitigation.
- A staged-only blob held by a node that dies before flushing is lost. The orphan flush
  narrows the window; replication still bounds it. Manual `active` with a Down member is
  dangerous and is documented as such.
- Duplicate flush bytes from P1; watch PUT cost in soak.
- The flusher gate is a core path; it needs the soak and the rewritten test. With no gate set
  every staged object flushes today, so a gate-less node is unaffected.
- Mixed-version rows stall old frontends; roll frontends first.
- Listing order and list-after-PUT visibility (section 3): a violation breaks the fence
  argument; mitigated by the conformance case and pass 2.
- The audit is probabilistic.
- A frontend that never reports `seen_gen` blocks pin release; the cap bounds the cost and an
  operator can force release.
- Promotion changes the primary: the promoted node can become rank 1 for some digests, so the
  flusher gate then picks it; tested.
- r=1 promotion needs its own test and soak before the default flips.
- No non-drain reclaim: disk grows on displaced owners until pressure eviction.
- Disk-pressure eviction outside a drain is a drop path without the per-blob confirm. Nothing
  is lost, because the cold store keeps the object, but a false miss is possible: if the owner
  has not adopted the object yet, a read that reaches it before the tail does can miss. The
  design does not change that path; the confirm applies only to the drain drop.
- The `file://` fallback adds scan load and large pulls if enabled; default off, throttled.

## 9. Pull request plan

| # | PR | Touches | Size | Proof | Unblocks |
|---|---|---|---|---|---|
| 1 | kbf-map additions | `kbf-map` | S | Unit tests: `owners_assuming` at r=1; fingerprint stable and changing; `read_order_with_history` dedup | 2, 4, 5 |
| 1b | Digest sampling reader | `kbf-meta` `MetaState::sample_digests` | S | Unit tests: order, bound, resume point | 5 |
| 2 | Durable previous-map pin | `members.rs`, `front_maps.rs`, `kbf-route` router, `kbf-mapsync` holder, `kbf-daemon` map, `map.proto` | M | Red first: two quick bumps plus a frontend restart gives a false miss; mutant: back to 1-deep; cap enforced. Fixes the hole independent of handoff | 5, 6 |
| 3 | Orphan flush and `flush_clear_gen` | `kbf-storage` flush, `kbf-front` cache flush, proto fields, staged metric | M | Red first: Draining and r=1 staged blobs never flush today | 5 |
| 4 | Tail adoption | `kbf-front` cache tail, `kbf-objstore` conformance case, wire fields | M | Red first: a segment flushed by another member after the rebuild is not readable today; a mutant that resumes past the last handled key (one cursor) | 5 |
| 5 | Automatic promotion | `members.rs` (`refresh`, `touch`), `join.rs`, mark phases, rules, audit, `--auto-promote` default off | L | Red first: spec test 1 end to end plus mutants | 6 |
| 6 | Drain completion and drop | node-to-node `FindMissingBlobs` client on owners (peer addresses and auth from the map; unavailable-owner and retry handling) with its tests, `draining/<id>` phases, `drain_confirmed`, `Cache::drop_confirmed`, `seen_gen`, `finish_drain`, flags; fixes the notes in `update/mod.rs`, `members.rs`, `operator-verbs.md`, `docs/api.md`, the "8.4" cross-reference in `architecture-v2.md`, P6 | L | Red first: spec test 2 with all mutants | 7 |
| 7 | Node-pull tier (only if the owner answers 1 below with a node-local store) | handoff engine for `file://` | M | default off | none |
| 8 | Operator surface and defaults | `GET /v1/members` fields, backlog metric, `blocked_on[]`; flip `--auto-promote` after soak | S | Soak with r=1 and r=3 | none |

Order: 1 first (1b beside it); 2, 3 and 4 in parallel after 1 (2 needs only 1); 5 needs 2, 3, 4
and 1b; 6 needs 5; 7 and 8 last.

## 10. Verified facts

All from `feature/arch-v2`.

**Cold-store keys and listing**
- Node object keys are `<root>cas/<member:016x>/<boot:016x>/<seq:016x>`
  (`crates/kbf-front/src/cache/write_group.rs:312`); epoch keys `<legacy>cas/<epoch>/<seq>` (`:307`);
  other-log keys `<root><log id>/cas/<epoch>/<seq>` (`:317`). Numbers are 16 lowercase hex digits
  (`write_group.rs:272-273`, `cache.rs:427-428`), so byte order is numeric order.
- Keys are ordered by seq only inside one `(member, boot)` or epoch; across the prefix the
  order is member, boot, seq. Legacy and node layouts do not interleave.
- `rebuild_index` (`crates/kbf-front/src/cache/rebuild.rs:~100-175`) resumes from a stored
  `Rebuild::Pending { after }` key with `ListToken::after(&ObjectKey)` and commits
  `RebuildProgress` every batch (`:~176-190`). Foreign keys are counted and skipped (`:~205`).
- `ObjectStore::list(prefix, after, max_keys) -> ListPage` returns keys ascending
  (`komira-ai/tierstore:crates/objstore-s3/src/lib.rs:~116-123`); token types at `komira-ai/tierstore:crates/objstore-s3/src/types.rs:~190-230`.
- S3: ListObjectsV2 with `start-after` and `continuation-token`
  (`komira-ai/tierstore:crates/objstore-s3/src/s3/mod.rs:671-690`; wire tests `komira-ai/tierstore:crates/objstore-s3/tests/s3_wire.rs:236`, `:275`).
  `file://`: `fs_store.rs:548-600`. Memory: `memory.rs:112`. Conformance:
  `conformance.rs:400-527`.

**List-after-PUT and sharing**
- The store contract says kbf needs no consistent listing and no read-after-write
  (`crates/kbf-objstore/src/lib.rs:6-8`); a listing may miss concurrent writes
  (`listing.rs:12-14`). A fresh `file://` listing re-reads every header (`fs_store.rs:36-42`).
  No code or doc states list-after-PUT for S3, MinIO or RustFS; the conformance suite does not
  check it.
- Only `file` and `s3` URIs are accepted (`crates/kbf-server/src/stores.rs:17`, `:104`).
- Farm modes use one `s3://` cold store (`docs/deploy/kbf-units.md`, "What each mode's unit carries";
  `docs/api.md:594-603`, one store registered for every storage node;
  `architecture-v2.md` section 8, a new owner rebuilds from segments an earlier owner flushed).
  Single-box modes use `file://` (`docs/deploy/kbf-units.md`, "all-in-one is for a trusted box"; `docs/deploy/storage-node.md:272`);
  `file://` is one process per directory: `FsStore::open` takes an exclusive `flock` on `LOCK` with `try_lock` (`fs_store.rs:294-302`, doc `:7-8`), and a second opener gets `FsStoreError::Locked` instead of sharing the directory. No doc describes a multi-node `file://` farm. The test-farm unit uses an `s3://` store
  that is loopback unless `MINIO_S3_ENDPOINT` is remote; the rehearsal harness defaults to a
  directory (`docs/design/kbf-cicd.md` section 4.3, the harness). No doc says "every node shares one store".

**Index and `put_unowned`**
- `MetaState.blobs` is private (`crates/kbf-meta/src/state.rs:320`); public readers are
  `blob_count`, `action_count`, `blob`, `touch_for`, `find_missing`, `action`,
  `action_closure`, `stored_objects`, `lacking`. The nearest ordered view is
  `StagedObject::blobs()` (`tier.rs:131`), one object only. The `Storage` trait
  (`crates/kbf-storage/src/lib.rs:71`) has no enumeration method.
- `put_unowned` defaults to `put` (`lib.rs:140`), forwards nothing, and is on the wire as the
  `kbf-unowned-copy` header (`service/mod.rs:91`; client `service/client.rs:73-88`, `:313`;
  server `bytestream.rs:74-90`). It is honoured only for the frontend's identity and a non-leg
  write (`service/generation.rs:39`, `admission.rs:157-169`).

**`map.proto`**
- `service Membership` has `Register` (`map.proto:86`) and `Heartbeat` (`:90`); the map is in
  `RegisterResponse.map` (field 4) and `HeartbeatResponse.map` (field 2). When this survey was
  made `HeartbeatRequest` had fields 1 to 10 (`member_id`, `boot`, `have_generation`,
  `catching_up`, `repair_debts`, `debts_known`, `applied_objstore_generation`,
  `apply_status`, `open_writes`, `writes_known`) and `HeartbeatResponse` had `map_generation`,
  `map`, `objstore_generation`; section 5 has the numbers as they are now.

**Rebuild, flusher, previous map**
- The rebuild is keyed on the meta log, not on the Joining state: `storage_node.rs:722-731`
  builds `CatchUp::rebuilding()` for a `--frontend` node whose log has no finished rebuild;
  `rebuild.rs:73-79` runs it and holds the start-up pass. The registered state is
  `catching-up`; a farm with data writes it `joining` (`tests/members_join.rs:173`).
- Tests that exist (no test uses the label "13a"):
  `crates/kbf-server/src/storage_rebuild_tests.rs` `wipe_rebuild_holds_blobs_in_a_bucket`,
  `wipe_rebuild_holds_blobs_in_a_directory`, `a_restart_mid_rebuild_resumes_and_stays_catching_up`,
  `a_finished_rebuild_is_not_run_again_by_a_restart`,
  `a_failed_rebuild_is_tried_again_and_the_node_stays_catching_up`,
  `a_node_without_a_frontend_owes_no_rebuild`; `startup_pass.rs`
  `the_start_pass_waits_for_the_rebuild`; `tests/storage_register.rs`
  `a_rebuilding_node_registers_catching_up_and_is_promoted_on_its_report`,
  `a_new_node_joining_an_old_farm_stays_joining_through_its_first_beats`;
  `tests/members_join.rs` `a_new_node_that_registers_rebuilding_joins_and_is_not_promoted_by_its_beat`,
  `a_node_joining_a_farm_with_data_is_never_primary`,
  `an_unfilled_joiner_stays_joining_through_a_restart_and_a_beat`.
- `OneFlusher::flushes` is true only with a map and an own id, a map at least as new as the
  newest generation heard of, and rank 1 of `write_owners(digest)`
  (`crates/kbf-storage/src/flush/mod.rs:64-76`); applied at `kbf-front/src/cache/flush.rs:335`.
  `MemberState::takes_writes` is `!Draining` (`kbf-map/src/map.rs:55-57`), so a Draining member
  is never rank 1; `flush/tests.rs:144-145` asserts it stops flushing. No other flush path for
  a displaced node was found. With no gate set (`cache.rs:202`) every staged object flushes.
- The previous map is one deep in `members.rs:573,624`, `kbf-mapsync/src/holder.rs:22,54`,
  `kbf-daemon/src/map.rs:114-122`, `front_maps.rs:38` and `kbf-route/src/router.rs:245-248`.

## 11. Open questions for the owner

1. **Is the cold store shared in every deployment?** Updated by the facts: farm modes
   (storage, storage-and-daemon) use one `s3://` bucket and the registration API sends one
   store to every node; the single-box modes use `file://`, which allows one opener per directory (`fs_store.rs:294-302`). So PR 7
   (node-pull for `file://`) is needed only if a multi-node farm is ever run with per-node
   `file://` stores, which no doc describes. Confirm that such a deployment does not exist or
   is unsupported.
2. **Pin-cap overflow policy.** The design refuses a join or drain while 4 pins are open (PR 2).
   The cap counts pins, so about two concurrent joins fill it. Is refusing acceptable to operators, versus deferring write ownership, and is 4 the right cap?
3. **Liveness versus safety on Down members.** Promotion waits while a Down member is inside
   its dwell, and a manual `active` overrides. Confirm that default.
4. **Is `seen_gen` reporting by frontends acceptable** as the pin release signal (a new field
   and a frontend heartbeat), versus a dwell only?
5. **Default for `--auto-promote`**, and who signs off on the flip (soak criteria at r=1 and
   r=3).
6. **Non-drain reclaim** stays out of scope. Confirm, because joins will never free space on
   the displaced owners.

Added by the verification, needing an owner answer: **which S3-compatible stores must auto-promotion
support?** The design requires the new conformance case to pass on each; today no source states
the property for any of them.
