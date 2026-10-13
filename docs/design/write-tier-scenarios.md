# The local write tier: failure scenarios (**planned**; the direct-path groundwork exists)

Every failure the local write tier of [write-path.md](write-path.md#6-the-local-write-tier)
must survive, the invariant each one checks and the outcome expected. It is the test
plan for rollout stage 5 (the tier in `kbf-sim-cell`), and the list stage 4's crash
tests are written against. Each scenario says where it is covered: **today** (a
`kbf-sim-cell` test that runs now, against the direct path with no tier), or the stage
it waits for: **stage 3** (tier metadata: `PutStaged`, `SealStaged`, `ObjectFlushed`,
`FlushConflict`, `AddTier`, the staged table, `BlobAnswer::Present { staged_on }`),
**stage 4** (the tier: block files, the flusher, recovery, the `Fs` extension), or
**stage 5** (the tier in the cell).

## 1. Invariants

Each scenario names the invariants it checks. The cell checks properties 1 to 7 of
[simulation.md](simulation.md#8-the-cell-kbf-sim-cell) after every step as well.

| | Invariant | Checked by |
|---|---|---|
| **A** | No false ack. An acknowledged blob is durable where the acknowledgement says: without the tier, in the object store and in the committed index; with it, in a block that was synced, under a name that was synced, with a committed `PutStaged`. From the ack on, the index answers it `Present` at every step, across restarts, unless a disk-loss fault was injected, and then only `Unavailable`, never `Absent` and never other bytes. | property 8 (today: the store half; stage 5: the tier half) |
| **B** | Nothing half-indexed. Every blob the index answers `Present` is held by the object its location names: in the store, or (while staged) in that object's block, whose file is at least the committed length. | property 9 (today: the store; stage 5: the block too) |
| **C** | Ids never move. A blob's `Location { store, object, offset }` is fixed by its first commit. A flush changes only the object's state, and a second flush of an object writes the same key with byte-identical content (the footer is built from the committed list, sorted by digest). | stage 4 tests; stage 5 property |
| **D** | Backpressure, not loss. When staged plus reserved bytes reach `--write-tier-bytes`, new writes take the direct path; when the store is down too, they are answered `UNAVAILABLE`. A write is never acknowledged without A. | stage 4 tests; stage 5 property |
| **E** | No leak. After any crash and the next start, every file in the tier directory is a staged object's `.blk`, a flushed object's `.seg`, or `TIER`; budget accounting equals the staged bytes plus open reservations. Store objects nothing names are counted, for the planned store sweep ([storage.md](storage.md#planned)). | today: the store-orphan count; stage 4: the directory |
| **F** | Liveness. Once the faults end, every upload is answered and every staged object is flushed. | property 7; stage 5 adds "no staged object at the end" |
| **G** | Exposure is measured. Unflushed bytes and the oldest unflushed block's age are reported; the unflushed-age alert fires while a block is older than its threshold. | stage 4 metrics test; stage 5 reads the metric |

## 2. Scenarios

The fault-injection points below exist in `kbf-sim-cell` today unless marked otherwise
(section 3 names them).

| # | Scenario | Fault | Invariants | Expected outcome | Coverage |
|---|---|---|---|---|---|
| 1 | Crash before `fdatasync` | power cut after a group's append, before its sync (a tier crash point; `FaultFs` keeps a seeded prefix of the unsynced tail) | A, B, E | nothing acknowledged; at the next start bytes past the committed length are truncated; a block with no `PutStaged` is an orphan and the sweep deletes it | stage 4 (`FaultFs`); today's analog: the server down at `StorePoint::PutSent` (`every_write_path_crash_point_leaves_nothing_half_indexed`) |
| 2 | Crash after `fdatasync`, before the `PutStaged` commit | power cut or kill at that point | A, B, E | not acknowledged; nothing indexed; the block (older epoch, not staged) is swept | stage 4; today's analog: `StorePoint::BeforeCommit` |
| 3 | Crash part-way through the `PutStaged` commit | power cut at the n-th filesystem operation of the commit | A, B, property 1 | either the entry is durable (indexed; the caller may never hear; its retry finds the blob present) or it is not (an orphan block); never applied without being durable | stage 4; today's analog: `StorePoint::InCommit` |
| 4 | Crash after the commit, before the reply | kill or power cut after `PutStaged` applies | A, F | indexed and durable; the client's retry is answered from the index (`FindMissingBlobs` says present, or the re-upload is a duplicate) | stage 4; today's analog: `StorePoint::BeforeReply` |
| 5 | Crash after the ack, before the flush | power cut while the block is staged and unsealed | A, B, C, F | the block survives (synced); at start `TIER` matches, the file is truncated to the committed length, the object sealed and queued; reads are served from the block meanwhile; the flush writes the object under its usual key | stage 4 + stage 5 |
| 6 | Store outage of minutes | the store hangs (every PUT held) or refuses every PUT, 2 to 10 min | A, B, D, F, G | with the tier: acks continue (local sync, one commit) while staged plus reserved bytes fit `--write-tier-bytes`; then writes fall back direct, which fails, so `UNAVAILABLE` (backpressure); the flusher backs up and drains once the store returns, oldest first; never data loss. Without the tier: nothing is acknowledged during the outage | today: `a_store_outage_of_minutes_never_acks_an_unstored_blob` (direct path); tier half stage 5 |
| 7 | Store outage of hours | as 6, 60 to 90 min | A, D, F, G | as 6; the unflushed-age alert fires during the outage; the tier reaches its budget and stays there; nothing evicted that is not flushed | today: `a_store_outage_of_hours_never_acks_an_unstored_blob` (direct path); tier half stage 5 |
| 8 | Slow store | every PUT answered 0.5 to 20 s late for a window, past the client's retry | A, B, D, F | without the tier: one blob has several PUTs in flight; the first answer indexes it, the rest are duplicates or orphans, every answer lands at its own object. With the tier: the flush is slower than ingest, the tier grows to its budget, then writes go direct | today: `a_slow_store_keeps_every_property`; tier half stage 5 |
| 9 | Store errors mid-flush | the next N PUTs fail, keeping nothing; or fail after keeping the object | A, B, C, E | without the tier: a failed PUT indexes nothing and is answered `UNAVAILABLE`; the retry is a new PUT under a new id; a kept-but-failed object is an orphan. With the tier: the flusher retries the **same** key with backoff; on `AlreadyExists` it reads the footer back and commits `ObjectFlushed` if it matches | today: `store_errors_index_nothing_they_failed`; flusher stage 4 |
| 10 | The store keeps an object but its acknowledgement is lost | PUT kept, no answer | A, B, C, E, F | without the tier: the PUT's deadline (`PUT_TIMEOUT`, the store client's request timeout) answers `UNAVAILABLE`; the retry stores again; the first object is an orphan. With the tier: the reflush meets `AlreadyExists`, the footer matches, `ObjectFlushed` | today: `a_lost_store_ack_leaves_an_orphan_not_a_hole`; flusher stage 4 |
| 11 | The flush succeeds but the `ObjectFlushed` commit is lost | the commit fails, or a crash between the PUT and the commit | B, C, E | the block stays staged; it is flushed again under the same key; footers equal, no `FlushConflict`; `.blk` is renamed `.seg` only after `ObjectFlushed` applies; a refused commit never lets eviction touch the block | stage 3 (commands) + stage 4 |
| 12 | Server restart mid-flush | kill or power cut during the flush PUT or the footer read-back | A, B, C, F | at start the sealed staged object is queued again; the reflush writes byte-identical content under the same key; no id changes; no double flush that differs | stage 4; today's analog: a restart never reuses object ids (mutant M9 below) |
| 13 | Local disk full | `ENOSPC` on create, append or sync below the budget (other tenants filled the disk) | A, D, E | the group or upload being written fails and is not acknowledged; its reservation is released and its file deleted; it falls back direct; staged blocks keep serving reads and flushing; a metric says so | stage 4; needs a `FaultFs` fault that returns `ENOSPC` (today `Fault::Error` is a generic error) |
| 14 | Local I/O error or a bad sector on a sealed block before its flush | a read error, or flipped bytes (`FaultFs::flip`) in a record | A, B | a record whose SHA-256 fails in the flusher's re-read is not uploaded; the object is marked `ObjectUnreachable { Corrupt }`; its blobs answer `Unavailable` and heal by re-upload; nothing serves the damaged bytes | stage 4 |
| 15 | Torn tail | power cut keeping part of an unsynced append (`FaultFs::crash(keep)`) | A, B | bytes past the committed length are truncated at start; every acknowledged range reads back intact | stage 4 |
| 16 | Eviction racing a read | a reader holds a location while eviction deletes the `.seg` | A | eviction deletes only `.seg` files of objects whose `ObjectFlushed` is applied; the reader's open fails `ENOENT` and falls back to the store, which holds the object; nothing is marked `Missing` | stage 4 |
| 17 | A read of a staged blob while its flush is in progress | read during the PUT, the commit and the rename | A, B | served from the block; a reader that finds neither `.blk` nor `.seg` after `ObjectFlushed` reads the store; a staged object is never committed `ObjectUnreachable { Missing }` by a reader | stage 4 + stage 5 |
| 18 | Node loss before the flush | the tier directory is replaced by an empty disk (a disk-loss fault) | A (its disk-loss clause), E, G | acknowledged data on that one disk is lost: the exposure is the seal delay (30 s) plus one PUT, longer while the store is down. The unflushed-age alert has fired already if the store was down. The server refuses to start on the wrong `TIER`; `--write-tier-abandon` marks every object staged there `Missing`; its blobs answer `Unavailable` (never `NOT_FOUND`, never other bytes); `FindMissingBlobs` reports them missing and a re-upload heals | stage 5 |
| 19 | Two servers: a Raft leader change with staged data on the old leader's disk | leadership moves; the old leader stays up, or goes down | A, B, F | `PutStaged` names the tier holding the block; reads of a blob staged elsewhere go to that server, or answer `Unavailable` while it is down; only the server holding a block flushes it. Until blocks are copied to two other tiers before the commit ([write-path.md](write-path.md#63-what-an-acknowledgement-means-now)), losing that server's disk is scenario 18 | after HA; not in stages 3 to 5 |
| 20 | Clock jumps | the wall clock steps forward or back | A, F, G, property 6 | the seal delay and the unflushed age are measured on a monotonic clock, so a step neither seals early nor hides an old block; farm time in the log never goes back; object ids do not depend on time | stage 4 (unit tests on a paused clock); stage 5 once `kbf-sim` models clock skew ([simulation.md](simulation.md#8-the-cell-kbf-sim-cell) lists it as not modeled) |
| 21 | The orphan sweep after each crash | every crash point of 1 to 5, 11 and 12, then a start | E | `.blk` files of older epochs not staged are deleted and the directory synced; `.seg` files stay; no budget is held by a file nothing names | today: store orphans counted (`RunReport::orphans`) after every crash point; directory sweep stage 4 |
| 22 | A wrong or missing `TIER` | the directory names another id, or none, while objects are staged | A | the server refuses to start; only `--write-tier-abandon=<id>` proceeds, as in 18 | stage 4 |
| 23 | Two groups on one block commit out of order, then a crash | the later group's `PutStaged` applies first | B, C | the committed length is the larger `end`; both groups read back; `PutStaged` after `SealStaged` is refused | stage 3 (state machine) + stage 4 |

## 3. What `kbf-sim-cell` has today

The groundwork for stage 5, none of it dependent on the tier:

- **Object store faults** (`StoreFault`, `StorePlan`): hang (`Pause`, every PUT held
  until a time), `Slow` (answers late for a window), `FailNext { n }` and `FailUntil`
  (refusals that keep nothing), `LoseAck { n }` (kept, never answered) and
  `KeepButFail { n }` (kept, answered failed). The store never loses an object it kept,
  and a PUT to an id it holds replaces it (no `conditional_put`: the worst case).
- **Crash at a point** (`CrashPlan<P>`, `Down`): the server is killed or loses power the
  n-th time it reaches a point. The direct path's points are `StorePoint::PutSent`,
  `BeforeCommit`, `InCommit { after }` (a power cut at a filesystem operation inside the
  `PutBlob` commit) and `BeforeReply`.
- **A PUT deadline** (`PUT_TIMEOUT`, 30 s) in the server, as the store client's request
  timeout: an unanswered PUT is answered `UNAVAILABLE`; a later answer is ignored.
- **Properties 8 and 9** (invariants A and B, store half), checked after every step
  against `BlobHomes`.
- **The seed sweep** (`sweep`): every seed runs even after one fails; the report lists
  each failing seed with its replay line. Kinds `StoreOutage`, `StoreOutageHours`,
  `StoreSlow`, `StoreErrors`, `StoreLostAck` and `StoreCrash` join the five earlier
  ones.

Tests (`crates/kbf-sim-cell/tests/write_path.rs`) and the planted defect each was shown
red on:

| Test | Catches (mutant) |
|---|---|
| `a_store_outage_of_minutes_never_acks_an_unstored_blob` (32 seeds) | M1 ack on `Upload`; M2 index a failed PUT; M3 `PutBlob` before the PUT; M5 a pause that holds nothing; M14 a refusal window ignored |
| `a_store_outage_of_hours_never_acks_an_unstored_blob` (2 seeds: one hang, one refusal) | M1, M2, M3, M5, M14 |
| `a_slow_store_keeps_every_property` (64 seeds) | M1, M3, M12 an index entry naming another object; M18 a slow window that answers at once |
| `store_errors_index_nothing_they_failed` (64 seeds) | M1, M2, M3, M12 |
| `a_lost_store_ack_leaves_an_orphan_not_a_hole` (64 seeds) | M1, M3, M12, M13 a lost ack that loses the object |
| `crashes_on_the_write_path_leave_nothing_half_indexed` (64 seeds) | M1, M3, M4 ack before the `PutBlob` commit, M9 a restart that reuses the first start's writer epoch, M12 |
| `every_write_path_crash_point_leaves_nothing_half_indexed` (every point, kill and two power cuts, 4 seeds each) | M4, M6 the in-commit crash never armed, M9 |
| `a_store_fault_sweep_replays` | M17 a slow answer that reads the wall clock |

Unit tests pin each fault, the crash plan (M7: `nth` off by one), the sweep (M8: stop at
the first failure; M20: a panic's `&str` message lost), the deadline (M10: no
deadline), the incremental property-1 replay (M11: a replica that goes on over a log it
did not read), each new property check fed a violation (M15: property 8 never asks the
index; M16: property 9 never asks the store), the orphan count (M19: objects the index
names counted) and the replay line's kind names (M21: the last kind not parsed). Property 1 now replays only the entries added since the
last check; that cut the five earlier sweeps from 4.2 s to 2.5 s and makes hour-long
runs affordable.

## 4. The seams stages 3 to 5 plug into

- **`BlobHomes`** (`crates/kbf-sim-cell/src/store.rs`): "does `object` hold `digest`
  now". `ObjectStore` implements it. Stage 5 adds the tier's block files (a staged
  object's block holds the blob if the file reaches its offset plus size) and checks
  properties 8 and 9 against the union.
- **`CrashPlan<P>`** (`src/hook.rs`): generic over the point enum. Stage 4/5 adds a
  `TierPoint` (after the append, after the sync, after `PutStaged`, after the flush
  PUT, after the read-back, after `ObjectFlushed`, before the rename) and arms it with
  the same `ServerFault`-style fault.
- **The store messages** (`Msg::Put`, `PutDone`, `PutFailed`): the flusher is one more
  sender of `Put` and must meet every `StoreFault` above.
- **`BlobAnswer` match sites**: the only places the cell reads the answer's shape are
  `no_false_ack`'s caller, `nothing_half_indexed` and `orphans` in `src/check.rs`, and
  one test helper in `src/server/write.rs`. Stage 3's `Present { location, staged_on }`
  changes those four and nothing else.
- **`Kind` and `sweep`**: stage 5 adds tier kinds (a crash at each `TierPoint`, a tier
  disk loss, a full tier) and sweeps them as the store kinds are swept.

## 5. Questions this catalogue raises for write-path.md

- **`ENOSPC` below the budget** (scenario 13) is not specified in section 6; this file
  states the expected outcome (fail the write, release, fall back direct).
- **A read error (not a hash mismatch) in the flusher's re-read** (scenario 14): retry,
  or mark `Corrupt` after some attempts? Section 6.4 covers only the hash mismatch.
- **Who flushes after a leader change** (scenario 19): section 6 assumes one server.
- **Clocks** (scenario 20): section 6 does not say the seal delay and the unflushed age
  use a monotonic clock; this file assumes they do.
