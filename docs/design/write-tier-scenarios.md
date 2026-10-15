# The local write tier: failure scenarios (**built in part**: the cell runs the tier's scenarios of section 4)

Every failure the local write tier of [write-path.md](write-path.md#6-the-local-write-tier)
must survive, the invariant each one checks and the outcome expected. It is the test
plan for rollout stage 5 (the tier in `kbf-sim-cell`), and the list stage 4's crash
tests are written against. Each scenario says where it is covered: **today** (a
`kbf-sim-cell` test that runs now, against the direct path with no tier), or the stage
it waits for: **stage 3** (tier metadata: `PutStaged`, `SealStaged`, `ObjectFlushed`,
`FlushConflict`, `AddTier`, the staged table, `BlobAnswer::Present { staged_on }`),
**stage 4** (the tier: block files, the flusher, recovery, the `Fs` extension), or
**stage 5** (the tier in the cell). Where stage 5 is built, the row names its test in
`crates/kbf-sim-cell/tests/write_tier.rs` (section 4).

## 1. Invariants

Each scenario names the invariants it checks. The cell checks properties 1 to 7 of
[simulation.md](simulation.md#8-the-cell-kbf-sim-cell) after every step as well.

| | Invariant | Checked by |
|---|---|---|
| **A** | No false ack. An acknowledged blob is durable where the acknowledgement says: without the tier, in the object store and in the committed index; with it, in a block that was synced, under a name that was synced, with a committed `PutStaged`. From the ack on, the index answers it `Present` at every step, across restarts, unless a disk-loss fault was injected, and then only `Unavailable`, never `Absent` and never other bytes. | property 8 (today: the store half; stage 5: the tier half) |
| **B** | Nothing half-indexed. Every blob the index answers `Present` is held by the object its location names: in the store, or (while staged) in that object's block, whose file is at least the committed length. | property 9 (today: the store; stage 5: the block too) |
| **C** | Ids never move. A blob's `Location { store, object, offset }` is fixed by its first commit. A flush changes only the object's state, and a second flush of an object writes the same key with byte-identical content (the footer is built from the committed list, sorted by digest). | stage 4 tests; stage 5 property |
| **D** | Backpressure, not loss. When staged plus reserved bytes reach the hot store's budget, new writes take the direct path; when the store is down too, they are answered `UNAVAILABLE`. A write is never acknowledged without A. | stage 4 tests; stage 5 property |
| **E** | No leak. After any crash and the next start, every file in the tier directory is a staged object's `.blk`, a flushed object's `.seg`, or `TIER`; budget accounting equals the staged bytes plus open reservations. Store objects nothing names are counted, for the planned store sweep ([storage.md](storage.md#planned)). | today: the store-orphan count; stage 4: the directory |
| **F** | Liveness. Once the faults end, every upload is answered and every staged object is flushed. | property 7; stage 5 adds "no staged object at the end" |
| **G** | Exposure is measured. Unflushed bytes and the oldest unflushed block's age are reported; the unflushed-age alert fires while a block is older than its threshold. | stage 4 metrics test; stage 5 reads the metric |

## 2. Scenarios

The fault-injection points below exist in `kbf-sim-cell` today unless marked otherwise
(section 3 names them).

| # | Scenario | Fault | Invariants | Expected outcome | Coverage |
|---|---|---|---|---|---|
| 1 | Crash before `fdatasync` | power cut after a group's append, before its sync (a tier crash point; `FaultFs` keeps a seeded prefix of the unsynced tail) | A, B, E | nothing acknowledged; at the next start bytes past the committed length are truncated; a block with no `PutStaged` is an orphan and the sweep deletes it | stage 4 (`FaultFs`); today's analog: the server down at `StorePoint::PutSent` (`every_write_path_crash_point_leaves_nothing_half_indexed`); **stage 5: `every_tier_crash_point_keeps_every_property` (`tier-after-append`)** |
| 2 | Crash after `fdatasync`, before the `PutStaged` commit | power cut or kill at that point | A, B, E | not acknowledged; nothing indexed; the block (older epoch, not staged) is swept | stage 4; today's analog: `StorePoint::BeforeCommit`; **stage 5: `every_tier_crash_point_keeps_every_property` (`tier-after-sync`)** |
| 3 | Crash part-way through the `PutStaged` commit | power cut at the n-th filesystem operation of the commit | A, B, property 1 | either the entry is durable (indexed; the caller may never hear; its retry finds the blob present) or it is not (an orphan block); never applied without being durable | stage 4; today's analog: `StorePoint::InCommit` at the `PutBlob` commit's append (lost) and its sync (kept by a kill or a power cut keeping the whole record, lost by one keeping none or part of it), checked at the next start (`every_write_path_crash_point_leaves_nothing_half_indexed`); **stage 5: `every_tier_crash_point_keeps_every_property` (`tier-in-put-staged-0`, `-1`)** |
| 4 | Crash after the commit, before the reply | kill or power cut after `PutStaged` applies | A, F | indexed and durable; the client's retry is answered from the index (`FindMissingBlobs` says present, or the re-upload is a duplicate) | stage 4; today's analog: `StorePoint::BeforeReply`; **stage 5: `every_tier_crash_point_keeps_every_property` (`tier-before-reply`)** |
| 5 | Crash after the ack, before the flush | power cut while the block is staged and unsealed | A, B, C, F | the block survives (synced); at start `TIER` matches, the file is truncated to the committed length, the object sealed and queued; reads are served from the block meanwhile; the flush writes the object under its usual key | stage 4 + stage 5; **stage 5: `every_tier_crash_point_keeps_every_property` (`tier-acked`)** |
| 6 | Store outage of minutes | the store hangs (every PUT held) or refuses every PUT, 2 to 10 min | A, B, D, F, G | with the tier: acks continue (local sync, one commit) while staged plus reserved bytes fit the hot store's budget; then writes fall back direct, which fails, so `UNAVAILABLE` (backpressure); the flusher backs up and drains once the store returns, oldest first; never data loss. Without the tier: nothing is acknowledged during the outage | today: `a_store_outage_of_minutes_never_acks_an_unstored_blob` (direct path); tier half stage 5; **stage 5: `a_store_outage_fills_the_tier_then_pushes_back`** |
| 7 | Store outage of hours | as 6, 60 to 90 min | A, D, F, G | as 6; the unflushed-age alert fires during the outage; the tier reaches its budget and stays there; nothing evicted that is not flushed | today: `a_store_outage_of_hours_never_acks_an_unstored_blob` (direct path); tier half stage 5; **stage 5: `a_store_outage_fills_the_tier_then_pushes_back` (2 seeds of 60 to 90 min; the alert of G is not read)** |
| 8 | Slow store | every PUT answered 0.5 to 20 s late for a window, past the client's retry | A, B, D, F | without the tier: one blob has several PUTs in flight; the first answer indexes it, the rest are duplicates or orphans, every answer lands at its own object. With the tier: the flush is slower than ingest, the tier grows to its budget, then writes go direct | today: `a_slow_store_keeps_every_property`; tier half stage 5 |
| 9 | Store errors mid-flush | the next N PUTs fail, keeping nothing; or fail after keeping the object | A, B, C, E | without the tier: a failed PUT indexes nothing and is answered `UNAVAILABLE`; the retry is a new PUT under a new id; a kept-but-failed object is an orphan. With the tier: the flusher retries the **same** key with backoff; on `AlreadyExists` it reads the footer back and commits `ObjectFlushed` if it matches | today: `store_errors_index_nothing_they_failed`; flusher stage 4 |
| 10 | The store keeps an object but its acknowledgement is lost | PUT kept, no answer | A, B, C, E, F | without the tier: the PUT's deadline (`PUT_TIMEOUT`, the store client's request timeout) answers `UNAVAILABLE`; the retry stores again; the first object is an orphan. With the tier: the reflush meets `AlreadyExists`, the footer matches, `ObjectFlushed` | today: `a_lost_store_ack_times_out_and_leaves_an_orphan_not_a_hole`; flusher stage 4 |
| 11 | The flush succeeds but the `ObjectFlushed` commit is lost | the commit fails, or a crash between the PUT and the commit | B, C, E | the block stays staged; it is flushed again under the same key; footers equal, no `FlushConflict`; `.blk` is renamed `.seg` only after `ObjectFlushed` applies; a refused commit never lets eviction touch the block | stage 3 (commands) + stage 4; **stage 5: `every_tier_crash_point_keeps_every_property` (`tier-after-flush-put`, `tier-in-flush-commit-0`, `-1`)** |
| 12 | Server restart mid-flush | kill or power cut during the flush PUT or the footer read-back | A, B, C, F | at start the sealed staged object is queued again; the reflush writes byte-identical content under the same key; no id changes; no double flush that differs | stage 4; today's analog: a restart never reuses object ids (mutant M9 below); **stage 5: `every_tier_crash_point_keeps_every_property` (`tier-after-flush-put`: the reflush writes the same bytes, property 10)** |
| 13 | Local disk full | `ENOSPC` on create, append or sync below the budget (other tenants filled the disk) | A, D, E | the group or upload being written fails and is not acknowledged; its reservation is released and its file deleted; it falls back direct; staged blocks keep serving reads and flushing; a metric says so | stage 4; needs a `FaultFs` fault that returns `ENOSPC` (today `Fault::Error` is a generic error); **stage 5: `a_full_disk_sends_writes_direct_and_loses_nothing` (`FaultFs`'s `Fault::NoSpace`)** |
| 14 | Local I/O error or a bad sector on a sealed block before its flush | a read error, or flipped bytes (`FaultFs::flip`) in a record | A, B | a record whose SHA-256 fails in the flusher's re-read is not uploaded; the object is marked `ObjectUnreachable { Corrupt }`; its blobs answer `Unavailable` and heal by re-upload; nothing serves the damaged bytes | stage 4; **stage 5: `a_bad_block_is_marked_corrupt_and_never_flushed` (a flipped byte); a read error in the flusher's re-read: `a_full_disk_sends_writes_direct_and_loses_nothing` (retried with backoff, never marked)** |
| 15 | Torn tail | power cut keeping part of an unsynced append (`FaultFs::crash(keep)`) | A, B | bytes past the committed length are truncated at start; every acknowledged range reads back intact | stage 4; today's analog: a power cut keeping 1 byte of the `PutBlob` record's unsynced bytes: the log's torn tail is truncated, the blob is not indexed, property 1 holds |
| 16 | Eviction racing a read | a reader holds a location while eviction deletes the `.seg` | A | eviction deletes only `.seg` files of objects whose `ObjectFlushed` is applied; the reader's open fails `ENOENT` and falls back to the store, which holds the object; nothing is marked `Missing` | stage 4; **stage 5: `eviction_racing_a_read_falls_back_to_the_store`** |
| 17 | A read of a staged blob while its flush is in progress | read during the PUT, the commit and the rename | A, B | served from the block; a reader that finds neither `.blk` nor `.seg` after `ObjectFlushed` reads the store; a staged object is never committed `ObjectUnreachable { Missing }` by a reader | stage 4 + stage 5; **stage 5: every tier run: each blob answered present is read 2 s later; `the_tier_stages_flushes_and_refuses_a_rollback` wants reads that found their object's flush PUT outstanding (the cell commits `ObjectFlushed` and renames in one step, so no read lands between the commit and the rename)** |
| 18 | Node loss before the flush | the tier directory is replaced by an empty disk (a disk-loss fault) | A (its disk-loss clause), E, G | acknowledged data on that one disk is lost: the exposure is the seal delay (30 s) plus one PUT, longer while the store is down. The unflushed-age alert has fired already if the store was down. The server refuses to start on the wrong `TIER`; `--hot-store-abandon` marks every object staged there `Missing`; its blobs answer `Unavailable` (never `NOT_FOUND`, never other bytes); `FindMissingBlobs` reports them missing and a re-upload heals | stage 5 |
| 19 | Two servers: a Raft leader change with staged data on the old leader's disk | leadership moves; the old leader stays up, or goes down | A, B, F | `PutStaged` names the tier holding the block; reads of a blob staged elsewhere go to that server, or answer `Unavailable` while it is down; only the server holding a block flushes it. Until blocks are copied to two other tiers before the commit ([write-path.md](write-path.md#63-what-an-acknowledgement-means-now)), losing that server's disk is scenario 18 | after HA; not in stages 3 to 5 |
| 20 | Clock jumps | the wall clock steps forward or back | A, F, G, property 6 | the seal delay and the unflushed age are measured on a monotonic clock, so a step neither seals early nor hides an old block; farm time in the log never goes back; object ids do not depend on time | stage 4 (unit tests on a paused clock); stage 5 once `kbf-sim` models clock skew ([simulation.md](simulation.md#8-the-cell-kbf-sim-cell) lists it as not modeled) |
| 21 | The orphan sweep after each crash | every crash point of 1 to 5, 11, 12 and 27, then a start (replaying the log, or from a snapshot as in 31) | E | `.blk` files of older epochs not staged are deleted and the directory synced: a block synced before its `PutStaged` committed, an upload open at the restart (its own block, never committed), and a flushed object's `.blk` left by a crash after `ObjectFlushed` and before the rename (the store holds the object). `.seg` files stay; no budget is held by a file nothing names | today: store orphans counted (`RunReport::orphans`) after every crash point; directory sweep stage 4; **stage 5: `every_tier_crash_point_keeps_every_property` (the `tier-after-sync` and `tier-before-rename` rows) and property 12 at every start** |
| 22 | A wrong or missing `TIER` | the directory names another id, or none, while objects are staged | A | the server refuses to start; only `--hot-store-abandon=<id>` proceeds, as in 18 | stage 4 |
| 23 | Two groups on one block commit out of order, then a crash | the later group's `PutStaged` applies first | B, C | the committed length is the larger `end`; both groups read back; `PutStaged` after `SealStaged` is refused | stage 3 (state machine) + stage 4 |
| 24 | Crash after a block or `TIER` is created and synced, before `sync_dir` (section 6.1.1) | power cut keeping any subset of the name changes since the last directory sync (`FaultFs::crash_reordered`'s `names`) | A, E | nothing in the block was acknowledged (the ack waits for the directory sync); at the next start the block's name is gone (nothing to do) or present (an orphan, scenario 21). `TIER` is synced before `AddTier` commits, so a committed tier always has its `TIER` file | stage 4 |
| 25 | A group's commit fails below a later group's end (section 6.1.2) | group 1 reserves and writes its range, group 2 the next; group 1's `PutStaged` fails (an I/O error or a refused commit), group 2's applies | B, C | group 1 is answered `UNAVAILABLE` and indexes nothing; its bytes stay as dead bytes below group 2's `end`; the committed length is group 2's `end`; the flushed object keeps the gap, and `Footer::parse` accepts it (it checks only that each entry ends at or before `index_offset`) | stage 3 (state machine) + stage 4 |
| 26 | The seal while ranges are reserved (section 6.1.4) | the seal timer (or the segment limit) fires while a group has reserved its range and not yet committed | B, C, F | the seal stops new reservations, waits until every reserved range is committed or failed, then commits `SealStaged`; no group is acknowledged and then refused by a `PutStaged` after the seal; new writes open the next block | stage 4; stage 5 under random timing |
| 27 | A crash mid-stream on a large blob's own block (section 6.2) | kill or power cut while a ByteStream `Write` streams into its own block, before its sync or its commit | A, E | not acknowledged; nothing indexed; the block (an older epoch, not staged) is an orphan, deleted at the next start (scenario 21); the client's retry writes a new block | stage 4; stage 5 |
| 28 | `FlushConflict`: the store holds the key with a footer that does not match (section 6.4) | the flush meets `AlreadyExists` and the footer read back differs | B, C, G | `FlushConflict` commits; the object stays staged and its blobs `Present`, read from the block; it is not marked `Corrupt`; the flusher stops retrying it; the block is never evicted; an alert fires | stage 3 (command) + stage 4 (a store fake holding other bytes under the key); stage 5 needs keyed objects and `conditional_put` in the cell's store (today a PUT replaces the object) |
| 29 | One staged block missing or short at start, `TIER` matching (section 6.4) | one `.blk` removed, or truncated below its committed length, while the server is down | A (its disk-loss clause), B, E, G | only that object is committed `ObjectUnreachable { Missing }`, with an alert; its blobs answer `Unavailable` and heal by re-upload; every other staged object is truncated to its committed length, sealed and queued as usual; the server starts | stage 4; stage 5 with a "lose one file" disk fault |
| 30 | The binary rolled back after the tier has run (section 6.5) | a log holding `AddTier`, `PutStaged`, `SealStaged`, `ObjectFlushed` or `FlushConflict` (log format 2) opened by a binary that reads only format 1 | A, B | the open is refused, never skipped: the entry decodes as `DecodeError::UnknownCommand`, the Raft machine stops on it, and opening the log returns `RaftLogError::Decode` naming that entry's index; the server does not start. A binary that skipped the entry would answer staged blobs `Absent` (breaking A) or index a location whose object it no longer tracks (breaking B). The staged blocks stay on disk untouched, so rolling forward again loses nothing | stage 3: `a_write_tier_command_in_a_format_1_log_refuses_the_open` (one run per command at the log's tail) and the format-1 codec refusal; stage 5: a rollback-and-start step in the cell; **stage 5: `the_tier_stages_flushes_and_refuses_a_rollback` (the durable log at the end of each run opened as a version-1 binary)** |
| 31 | Starting from a metadata snapshot taken while objects are staged (section 6.5) | a snapshot whose base lies after a `PutStaged` and before that object's `ObjectFlushed`, the log compacted through it, then a crash and a start from the snapshot | A, B, E | the staged table (each object's tier, committed length, state and record list) and the record of objects that left are in the snapshot, so the start rebuilds the same state as replaying the log: each staged object is truncated to its committed length, sealed and queued, and its blobs answer `Present { staged_on }`. A snapshot without the staged table makes every staged block look like an older-epoch block nothing names, and the orphan sweep (21) deletes acknowledged data: a false ack (A) and an index naming a deleted block (B) | stage 3 with the snapshot encoding ([metadata-snapshots.md](metadata-snapshots.md); not on `main` yet): a round-trip of a state holding staged objects in every state; stage 5: the crash table crossed with a snapshot taken at each point |
| 32 | A store-down backlog on a host that also runs a daemon (section 6.6) | as 7, with a build running on the same host's daemon, its input cache near its budget | A, D | the tier fills to the hot store's budget and stops there (D); the daemon's input cache is a separate store with its own budget and eviction, so the tier's growth never evicts a running build's inputs and the build keeps its hard links. The two budgets plus the free-space floor fit the disk, so the backlog does not push the host into `ENOSPC` (13) | stage 5, needs a daemon input cache in the cell (not modelled today) |
| 33 | A daemon miss served from its own host's tier while the block is flushed or evicted (section 6.6) | the same host's daemon reads a staged blob over loopback during the flush PUT, the `ObjectFlushed` commit, the rename to `.seg`, or the eviction of the `.seg` | A, B | as 16 and 17, for a loopback reader: the read is served from the open `.blk` or `.seg`, or, if the reader finds neither after `ObjectFlushed`, from the store; the daemon's cache file gets the blob's verified bytes, never a short or mixed read | stage 5, as 32 |

## 3. What `kbf-sim-cell` has today

The groundwork for stage 5, none of it dependent on the tier:

- **Object store faults** (`StoreFault`, `StorePlan`): hang (`Pause`, every PUT held
  until a time), `Slow` (answers late for a window), `FailNext { n }` and `FailUntil`
  (refusals that keep nothing), `LoseAck { n }` (kept, never answered) and
  `KeepButFail { n }` (kept, answered failed). The store never loses an object it kept,
  and a PUT to an id it holds replaces it (no `conditional_put`: the worst case).
- **Crash at a point** (`CrashPlan<P>`, `Down`): the server is killed or loses power the
  n-th time it reaches a point. The direct path's points are `StorePoint::PutSent`,
  `BeforeCommit`, `InCommit { after }` and `BeforeReply`. `InCommit` plants the crash at
  the `after`-th filesystem operation of the `PutBlob` commit, which has two (0, the
  record's append; 1, its sync). It is recorded (`Seen::crashed_at`, `Seen::cuts`) only
  when it fires inside that commit; one planted past it lands in a later commit and is
  not counted as this point. The next start records whether the cut blob is indexed
  (`Cut::survived`).
- **A PUT deadline** (`PUT_TIMEOUT`, 30 s) in the server, as the store client's request
  timeout: an unanswered PUT is answered `UNAVAILABLE`; a later answer is ignored.
- **Properties 8 and 9** (invariants A and B, store half), checked after every step
  against `BlobHomes`.
- **The seed sweep** (`sweep`): every seed runs even after one fails; the report lists
  each failing seed with its replay line. A scenario with one pinned crash
  (`Scenario::crash_at`) puts it in the line (`KBF_SIM_CRASH=<point>/<down>`, for
  example `in-commit-1/cut-1-0`), so the replay runs that crash, not the seed's.
- **What each fault met** (`ObjectStore::met`, `ObjectStore::repeated`, `Seen::timed_out`,
  `Seen::put_failed`): the PUTs a refusal, a lost ack or a false failure met, and the
  server's own record of each. Network duplicates of a PUT or an upload leave orphans
  too, so a test ties an outcome to the PUT the fault met, not to a count. Kinds `StoreOutage`, `StoreOutageHours`,
  `StoreSlow`, `StoreErrors`, `StoreLostAck` and `StoreCrash` join the five earlier
  ones.

Tests (`crates/kbf-sim-cell/tests/write_path.rs`) and the planted defect each was shown
red on:

| Test | Catches (mutant) |
|---|---|
| `a_store_outage_of_minutes_never_acks_an_unstored_blob` (32 seeds) | M1 ack on `Upload`; M2 index a failed PUT; M3 `PutBlob` before the PUT; M5 a pause that holds nothing; M14 a refusal window ignored |
| `a_store_outage_of_hours_never_acks_an_unstored_blob` (2 seeds: one hang, one refusal) | M1, M2, M3, M5, M14 |
| `a_slow_store_keeps_every_property` (64 seeds) | M1, M3, M12 an index entry naming another object; M18 a slow window that answers at once |
| `store_errors_index_nothing_they_failed` (64 seeds): each refused PUT the server was told of leaves nothing and its blob is acknowledged under another object; each false failure it was told of leaves an orphan; every seed that met a false failure has one the server was told of | M1, M2, M3, M12; M22 a kept-but-failed PUT answers `PutDone`; M23 the server's refusals not recorded |
| `a_lost_store_ack_times_out_and_leaves_an_orphan_not_a_hole` (64 seeds): every lost ack times out unless a network duplicate of its PUT was answered, at least 90 % time out, and each timed-out object is an orphan whose blob is acknowledged under another object | M1, M3, M12, M13 a lost ack that loses the object; M24 a lost ack still answers `PutDone`; M25 deadlines not recorded |
| `crashes_on_the_write_path_leave_nothing_half_indexed` (64 seeds) | M1, M3, M4 ack before the `PutBlob` commit, M9 a restart that reuses the first start's writer epoch, M12 |
| `every_write_path_crash_point_leaves_nothing_half_indexed` (5 points: put sent, before the commit, at the commit's append, at its sync, before the reply; each under a kill and power cuts keeping none, 1 byte (a torn record) and all of the unsynced bytes; 4 seeds each): one crash, one restart, an orphan; an in-commit crash is one cut of `PutBlob`, indexed at the next start exactly when its record was whole | M4, M6 the in-commit crash never armed, M9; M26 the in-commit crash planted 40 operations on (in a later commit); M27 a power cut handled as a kill; M28 `kbf-store`'s open keeps a torn tail (property 1) |
| `an_in_commit_crash_past_the_commit_is_no_cut_of_it` | M29 an in-commit crash recorded when planted, not when it fires |
| `a_store_fault_sweep_replays` (`StoreCrash`, `StoreSlow`, `StoreErrors`, `StoreLostAck`) | M17 a slow answer that reads the wall clock |

Unit tests pin each fault, the crash plan (M7: `nth` off by one), the sweep (M8: stop at
the first failure; M20: a panic's `&str` message lost), the deadline (M10: no
deadline), the incremental property-1 replay (M11: a replica that goes on over a log it
did not read), each new property check fed a violation (M15: property 8 never asks the
index; M16: property 9 never asks the store), the orphan count (M19: objects the index
names counted) and the replay line's kind names (M21: the last kind not parsed), the pinned crash in the
replay line and its read-back (M30: the line drops `KBF_SIM_CRASH`), and the store's
record of repeated PUTs (M31: a repeat not recorded). Property 1 now replays only the entries added since the
last check; that cut the five earlier sweeps from 4.2 s to 2.5 s and makes hour-long
runs affordable.

## 4. The tier in the cell (stage 5, as built)

A run whose kind starts with `Tier` gives the server `kbf-tier`'s `Tier` on a second
`FaultFs`, over the Raft log raised to machine version 2 (write-path.md section 6.8).
The server attaches it at each start, stages each upload (write, sync, `PutStaged`, then
the ack), runs the flusher every tick (`SealStaged`, the re-read and check, one PUT of
records and footer under the object's own id, `ObjectFlushed`, the rename), and reads
each blob a `FindMissingBlobs` answered present 2 s later from the tier or the store.
A crash or power cut takes both disks down together. The store keeps a flushed segment
as its bytes and blob list, and records a PUT that replaces an object with other bytes.

**Kinds.** `TierCalm` (budget 160 bytes, seal after 5 s, blocks of up to 48 bytes;
clients upload 1 to 30 byte blobs). `TierCrash`: one crash at a `TierPoint`, killed or
cut off, while the store is slow. `TierStoreOutage` and `TierStoreOutageHours` (budget
96): as the store outages, with the tier. `TierDiskFull`: one to three `ENOSPC`s
(`Fault::NoSpace`, which fails only operations that take space) on even seeds, I/O
errors on any operation of the tier's disk on odd ones. `TierBadBlock` (seal
after 20 s): one byte of the oldest staged record flipped. `TierEviction` (budget 64):
flushed copies evicted while reads are in flight. **Tier points** (`TierPoint`):
`tier-after-append`, `tier-after-sync`, `tier-in-put-staged-<op>`, `tier-before-reply`,
`tier-acked`, `tier-after-flush-put`, `tier-in-flush-commit-<op>`, `tier-before-rename`;
a pinned one replays with `KBF_SIM_CRASH=<point>/<down>` and `KBF_SIM_KIND=TierCrash`.

**Properties.** In tier runs 5, 8 and 9 count a staged object's block (as a power cut
now would leave it, at the committed offset, with the blob's bytes) as a home beside the
store; a block a start finds lost, or a present blob a read finds nowhere or with other
bytes, breaks 8 (a local read that failed with an I/O error a planted fault caused, of a
blob the store does not hold yet, is unavailable, not lost; a local read error no
planted fault caused breaks 8 at once, since `kbf-front` would answer an acknowledged
blob unavailable). A read of a damaged staged record marks its object `Corrupt` and
closes and discards its block, as `kbf-front`'s `fetch_answer` does; a damaged flushed
copy is read from the store. Acknowledgements and entries in an object a bad sector damaged are held
to A's disk-loss clause: never `Absent`. Four more: **10, ids never move** (C; a blob's
location changes only after its object is marked unreachable, and no store object is
replaced by other bytes; checked in every run), **11, within budget** (D), **12, no leak**
(E, at each start: only `TIER`, staged blocks at their committed length and flushed
copies, and the budget counts exactly the staged bytes), **13, drained** (F, at the end).
Each has a unit test fed a violation (`src/check/tier.rs`).

| Test (`tests/write_tier.rs`) | Scenarios | Catches (planted, each red) |
|---|---|---|
| `the_tier_stages_flushes_and_refuses_a_rollback` (16 `TierCalm` seeds): staged acks, flushes and reads in every seed, no read error (none is planted), reads during a flush PUT in at least 2 seeds; a version-1 binary refuses the durable log at the raise, which comes before every tier command, and this binary opens it again to the same staged table | 17, 30 | T1 the metadata machine keeps format 1 after the raise (7: the tier's first commit stops the log, every start); T2 a machine version past what the binary runs skipped (the rollback opens); T3 the tier's `fdatasync` skipped (8); T4 its `sync_dir` skipped (8); a partial read of a staged block that answers an I/O error (8, a read error no fault caused) |
| `every_tier_crash_point_keeps_every_property` (each of the 10 points, under a kill and power cuts keeping none and all; 4 seeds each): most runs crash, and each next start did what the point implies: after the sync the block is swept or cut back, before the rename it is swept, before the reply and after the ack it is kept whole, after the flush PUT the object is PUT again under its id and flushed | 1 to 5, 11, 12, 21 | T3, T4; T5 the orphan sweep skipped (12); T6 the rename to a flushed copy before `ObjectFlushed` commits (8) |
| `tier_crash_seeds_keep_every_property` (32 `TierCrash` seeds) | 1 to 5, 11, 12, 21 | T5; T6; T7 bytes past the committed length kept at a start (12); T8 recovered unsealed blocks never named for their seal (13) |
| `a_store_outage_fills_the_tier_then_pushes_back` (16 + 2 seeds): every ack inside the outage is a staged one, the budget pushed back in every seed, and acks resume after | 6, 7 | T9 the budget not enforced (11); T10 eviction that deletes a staged block (8, 13) |
| `a_full_disk_sends_writes_direct_and_loses_nothing` (32 seeds): at least 4 of the 16 even seeds met `ENOSPC` and at least 4 of the 16 odd seeds an I/O error, each told apart, and some flush met a failed re-read and was retried | 13, 14 (read error) | T11 a write error answered `Ok` (8: synced and acknowledged without its bytes); `Fault::NoSpace` that fails nothing; `FaultFs::faults_met` that counts nothing (8: an injected read error taken for a bug) |
| `a_bad_block_is_marked_corrupt_and_never_flushed` (32 seeds): each damaged object is marked corrupt, by the flusher's re-read or by a read, and never flushed; both happen | 14 | T12 the flusher's check passes every record (the damaged object is PUT; 10); a read that marks an object without closing its open block (9: the next grouped `PutStaged` is refused) |
| `eviction_racing_a_read_falls_back_to_the_store` (32 seeds): copies evicted in most seeds, and reads that lost their local copy served by the store | 16 | T10 |

T1 and T2 are in `kbf-server`'s metadata machine, T3 to T5 and T7 to T11 in `kbf-tier`,
T6 and T12 in the cell's copy of `kbf-front`'s steps. `kbf-server/tests/raft_log.rs`
pins the raise itself (T1, T2, a raise committed on every call).

Not covered yet: the unflushed-age metric and alert (G), a disk loss and
`--hot-store-abandon` (18, 22, 29), `FlushConflict` (28; the cell's store has no
`conditional_put`), segment uploads and streamed own blocks (27), snapshots (31), a daemon input cache (32, 33), two
servers (19) and clock jumps (20).

## 5. The seams stages 3 to 5 plug into

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
  `read_back_cuts` and one test helper in `src/server/write.rs`. Stage 3's
  `Present { location, staged_on }` changes those five and nothing else.
- **`Kind` and `sweep`**: stage 5 adds tier kinds (a crash at each `TierPoint`, a tier
  disk loss, a full tier) and sweeps them as the store kinds are swept.
- **What the cell does not model yet**: a metadata snapshot (scenario 31; the cell's
  server opens its log with `RaftMetaLog::open`, which writes no snapshot and refuses a
  raft directory that holds one) and an input cache in the fake daemon (`src/daemon.rs` runs leases and fetches
  nothing; scenarios 32 and 33). Each is a new step or a new part of an actor, not a
  new fault on an existing one.

## 6. Questions this catalogue raises for write-path.md

- **`ENOSPC` below the budget** (scenario 13): answered by write-path.md section 6.7
  (any tier error before the commit goes direct); stage 5 tests it.
- **A read error (not a hash mismatch) in the flusher's re-read** (scenario 14):
  answered by write-path.md section 6.7 (retried with the flush backoff); stage 5 tests
  it.
- **Who flushes after a leader change** (scenario 19): section 6 assumes one server.
- **`TIER` before `AddTier`** (scenario 24): section 6.1 says `TIER` is written and the
  directory synced before any record is acknowledged, not that this happens before
  `AddTier` commits; this file assumes it does, so a committed tier id always has its
  file and a crash between them is a fresh directory, not scenario 22.
- **Clocks** (scenario 20): section 6 does not say the seal delay and the unflushed age
  use a monotonic clock; this file assumes they do.
- **The staged table in a snapshot** (scenario 31): section 6.5 says the snapshot
  encoding gains the staged table, but [metadata-snapshots.md](metadata-snapshots.md)
  does not list it yet. This file assumes the record of objects that left (kept for
  the blob retention) is in the snapshot too, so a start from a snapshot refuses the
  same late `PutStaged` a replay refuses.
- **Raising the log to format 2** (scenario 30): built in stage 5 (write-path.md
  section 6.8): one `MachineVersion` entry naming version 2, committed when the tier
  is attached, before its first `AddTier`.
