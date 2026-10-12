# Metadata snapshots: the encoding, compaction and starting from a snapshot

The single-voter metadata log (`--meta-log=raft`) keeps every entry it has ever
committed. It writes no snapshot and never compacts, and a raft directory that holds a
snapshot is refused at start. This document designs the missing piece. It covers:

- a snapshot encoding for the replicated metadata state;
- how the single-voter log takes a snapshot and compacts;
- how a directory that holds a snapshot starts;
- how a reader refuses a version it does not know;
- what a snapshot costs (size and pause) at today's scale and at ten times that.

It follows the high-availability design (PR #303, open, `docs/design/ha.md` on that
branch), so that a later HA change inherits this work rather than replacing it.

**Everything here is planned** unless it says "today". Phase one is PRs 1 to 3 of
section 11. Every "today" claim was checked against `main` at `1ed3b18`. Measured
numbers say *measured* and estimates say *estimate*.

## 1. What exists today

- **The state machine.** `MetaMachine` (`crates/kbf-server/src/raft_log/machine.rs`)
  wraps a `MetaState` and adds the first `Bootstrap` applied, a count of `Bootstrap`
  entries, the index of the last entry applied, the last answer and the first decode
  failure. Its
  `Machine::snapshot` is `unreachable!`, and its `restore` returns `NoSnapshots`. The
  module docs (`raft_log/mod.rs`, "Not yet") say why: the metadata state has no
  snapshot encoding.
- **The Raft host can already snapshot.** `kbf_raft::Host::snapshot`
  (`crates/kbf-raft/src/host.rs`) asks the machine for its bytes, compacts the core
  through the applied index (`Raft::compact`), then calls `Storage::write_snapshot`.
  `Host::open` calls `Machine::restore` when the storage holds a snapshot, and the
  core then applies again starting after the snapshot's base. Nothing calls
  `Host::snapshot` outside the `kbf-raft` tests.
- **The disk store can already hold a snapshot.** `kbf-store` writes one file,
  `snapshot`. It holds a header record (base index, base term and state length) and
  then the state in CRC-32C records of at most 1 MiB (`SNAPSHOT_CHUNK`,
  `crates/kbf-store/src/record.rs`). The file is written to `snapshot.tmp`, synced,
  renamed and the directory synced. Only after that are the segments it covers removed
  (`Store::persist_snapshot`). `crates/kbf-store/src/snapshot_tests.rs` cuts power at
  every operation of that write.
- **What is missing** is the bytes in the middle: what `MetaMachine::snapshot` returns
  and what `restore` reads back. Without them the host cannot take a snapshot, and
  the log grows forever.
- **The state** (`crates/kbf-meta/src/state.rs`, `struct MetaState`) has these fields:
  - `retention`: configuration from flags, not replicated;
  - `now`: farm time, raised by `Tick`;
  - `blobs`: the CAS index, digest to location and last touch;
  - `actions`: digest to record (result digest, closure set) and last hit;
  - `unreachable`: object to loss mark (reason and generation);
  - `next_epoch`.

  All the maps are `BTreeMap`s, so iteration order is a function of the state.
- **Ticks.** `RaftMetaLog` commits `Tick(now)` every second (`raft_log::TICK`). A
  `Tick` command encodes to 11 bytes (measured). On disk that is 36 bytes per entry:
  an 8-byte record header, 17 bytes of index, term and kind, and the command. So an
  idle server writes 86,400 entries, about 3.1 MB, a day.
- **The open reads the directory twice.** `RaftMetaLog::open` calls `Store::open`,
  which scans every segment and the snapshot file, and throws away the result
  (`let (store, _) = Store::open(...)`). Then `Host::open` calls `Storage::load`, which
  scans everything again. Today this costs twice the log's replay read. Once a
  snapshot exists, it also costs reading the snapshot twice (section 6).
- **Pins** (the durable build-record roots of `docs/design/invocations.md`, 3.5) do
  not exist in code yet. They are planned as part of `MetaState`, added under a new log
  format (invocations PR 7). The snapshot has to carry them once they exist
  (section 3.4).

## 2. What a snapshot holds

A snapshot holds the machine's state as of its base entry. That is everything that
replaying the compacted entries would rebuild, and nothing else:

| Part | From | Why it must be there |
|---|---|---|
| CAS index: digest, location, last touch | `MetaState::blobs` | the index itself; `last_touch` decides `Collect` |
| Action cache: digest, result, closure, last hit | `MetaState::actions` | hits, and the closure check |
| Loss marks: object, reason, generation | `MetaState::unreachable` | without its generation, a late `ObjectReachable` would clear a newer mark (`ha.md` 4.6) |
| Farm time | `MetaState::now` | farm time must never go back across a restart (`RaftMetaLog` takes its base from it) |
| Next writer epoch | `MetaState::next_epoch` | without it, `AllocEpoch` would hand out an epoch again, and two writers would name the same objects |
| The first `Bootstrap`, and the count of `Bootstrap` entries | `MetaMachine` | the open checks the committed `Bootstrap` against the record and the prefix root. Without it, a compacted log would commit a second `Bootstrap` and accept a changed prefix root |
| Pins: id, current root, closure, hold-until | `MetaState` (planned) | `Collect` keeps every digest in a live pin's closure |
| Membership: voters and learners | the log record today; a config entry under HA | under HA the config entries are compacted away |
| Format and machine version | today the constant `Format::V1` | how a reader knows it can read the bytes (section 4) |

**Not in the snapshot:**

- **`retention`.** It is configuration. A restore takes it from the flags, the same
  way a replay of the log does today.
- **The last answer.** `MetaMachine::answer` is only for the caller of the entry being
  applied.
- **The decode failure.** A machine that has stopped on an entry never takes a snapshot
  (section 5).
- **The scheduler's state.** It is in memory today. Under HA it becomes the
  `FarmMachine` section (section 8).

## 3. The encoding

### 3.1 Frames

`Snapshot.state` (the bytes `kbf-store` already wraps in CRC records) is a sequence
of **frames**. Each frame is one prost message, `kbf.log.v1.SnapshotFrame`, written
with its varint length in front (`encode_length_delimited`). The messages live in
`crates/kbf-proto/proto/kbf/log/v1/snapshot.proto`, next to `log.proto`, which is where
`ha.md` 4.6 puts them. The codec lives in a `snapshot` module of `kbf-log`. That
module already converts every domain value involved (`Digest`, `Location`,
`ActionRecord`, `Touch`) and it keeps `kbf-meta` free of codec dependencies.

```
SnapshotFrame { oneof frame {
  Header    header     = 1;   // first, exactly once
  Blobs     blobs      = 2;   // then the sections, in this order, each 0..n frames
  Actions   actions    = 3;
  LossMarks loss_marks = 4;
  Pins      pins       = 5;   // reserved for log format 2 (section 3.4)
  Trailer   trailer    = 15;  // last, exactly once; nothing follows it
} }

Header  { uint32 format; uint32 machine_version; uint64 base_index; uint64 base_term;
          Membership membership; Bootstrap bootstrap; uint64 bootstraps;
          uint64 now_millis; uint64 next_epoch; }
Blobs   { repeated BlobEntry entries; }     // BlobEntry { Digest digest; Location location; uint64 last_touch_millis; }
Actions { repeated ActionEntry entries; }   // ActionEntry { Digest action; ActionRecord record; uint64 last_hit_millis; }
LossMarks { repeated LossMark entries; }    // LossMark { ObjectId object; UnreachableReason reason; uint64 generation; }
Trailer { uint64 blobs; uint64 actions; uint64 loss_marks; uint64 pins; bytes sha256; }
```

`Digest`, `Location`, `ObjectId`, `ActionRecord`, `UnreachableReason` and `Bootstrap`
are the existing `log.proto` messages, reused rather than copied. So a field means the
same thing in an entry and in a snapshot.

### 3.2 Rules that make the bytes a function of the state

- **No `map` fields.** Each section lists its entries in strictly increasing key
  order, across frame boundaries too, with the key order of the `BTreeMap` it came
  from. A frame holds at most 4,096 entries. Every frame is full except the last one of
  its section, and a section with no entries has no frame. So the same state always
  splits into frames at the same places, and two servers that take a snapshot at the
  same index write the same bytes (`ha.md` 4.6).
- **The kbf-log decoding rules apply as they are.** An enum value that is zero or
  unknown is an error. An unset `oneof` is an error. A required field that is absent is
  an error. A set (a closure) must be strictly increasing. And each frame must be the
  bytes `encode` writes for the decoded value: the frame is re-encoded and compared, so
  a field this binary does not know is refused rather than dropped, and a non-minimal
  varint is refused rather than accepted.
- **The trailer counts and hashes.** The trailer gives the number of entries in each
  section and the SHA-256 of every byte before the trailer frame. A reader recounts and
  rehashes, and refuses on any difference. The CRC records of `kbf-store` already
  catch disk damage. The digest exists for the copy that HA puts in the object store
  (section 8) and for any later reader that gets the bytes without the CRC framing.
- **The restored state must satisfy the apply rules.** `MetaState` gains a
  constructor from parts (planned name `MetaState::from_parts(retention, parts)`), and
  it refuses any state the commands could not have produced:
  - a location or loss mark that names an epoch at or above `next_epoch`;
  - `next_epoch` of 0;
  - a closure that does not list strictly increasing digests.

  A snapshot whose CRCs and digest are right but whose content is wrong (a bug in an
  older binary, say) refuses the start. It is not served.

### 3.3 Why frames, and not one message

- **Bounded memory per frame.** A frame is at most 4,096 entries, so a reader decodes
  one frame at a time and never holds a second copy of the whole state as protobuf
  values. A single message of millions of entries would have to be decoded whole.
- **Streaming later without a format change.** `ha.md` 4.9 says a snapshot is "never
  built as one buffer". Phase one does build one buffer (section 5.3). With frames, a
  later streaming writer emits the same bytes, so moving to it changes a trait and not
  the format.

### 3.4 Pins

Pins arrive with log format 2 (the pin command, `invocations.md` PR 7). The same
format number also turns on the `Pins` frame:

```
Pins { repeated Pin entries; }   // Pin { string id; Digest root; repeated Digest closure; uint64 hold_until_millis; }
```

Pins are ordered by `id`, and each closure is strictly increasing. A format-1 header
with a `Pins` frame is refused, and so is a format-2 snapshot read by a format-1
binary (section 4). The pins work therefore needs no second snapshot version of its
own. It ships the frame and the command together.

## 4. Versions, and how a reader refuses

There is one format number for entries and snapshots: `kbf_log::Format`, which is V1
today. That is the "format version" of `ha.md` 4.7, which governs "which entry and
snapshot encodings may appear". The header names it, along with `machine_version`
(1 today: the apply behavior that `main` runs). The open goes like this:

1. The first frame must be a `Header`. If it is not, the error is `NotAHeader`.
2. `format` must be a format this binary reads. If it is not, the error is
   `UnknownFormat(n)`, and the message names `n` and the formats this binary reads.
   `machine_version` is checked the same way, as `UnknownMachineVersion(n)`.
3. `base_index` and `base_term` must equal the base that `kbf-store` recorded in the
   file header. Otherwise a state has been attached to the wrong base (`BaseMismatch`).
4. `membership` must equal the membership in the log record (`voters 1` today).
   Otherwise the error is `MembershipMismatch`.
5. The sections come next, in order. An unknown frame kind is never skipped: it is an
   unset `oneof`, which is a decode error.
6. The trailer must be last, with the right counts and the right SHA-256, and no bytes
   may follow it.

Every refusal becomes `MetaMachine::Error` (it replaces `NoSnapshots`). It surfaces as
`RaftLogError::Open(OpenError::Machine(..))`, so the server refuses to start and the
message says what it found. **A refusal is fail-stop, and the reader never falls back.**
Two fallbacks are wrong:

- Starting empty would answer "absent" for every blob the farm holds.
- Replaying the log alone is not possible: the entries through the base are gone.

The operator fixes the problem by running a binary that reads that format.

**Upgrades.** A binary reads every format up to the one it writes, and it writes the
committed format. So a rolling upgrade under HA works exactly as `ha.md` 4.7 has it for
entries. A downgrade below a committed format is refused at start. That is the same
rule entries already follow, because `kbf_log::decode` refuses a format it does not
know.

## 5. Taking a snapshot and compacting

### 5.1 When

The tick task already runs every second under the log's lock (`tick_every`). After
its tick, it takes a snapshot when **either** of these holds:

- 100,000 entries have been applied since the last snapshot (the `ha.md` 4.9 lean); or
- the bytes in the log segments since the last snapshot are at least the larger of
  64 MiB and the size of the last snapshot file.

The second rule keeps snapshot writes in proportion to log writes. If the state is
4 GB, taking a snapshot every 64 MiB of log would write 60 times more snapshot than
log. With the rule, snapshot writes are at most about the log's own bytes, and the
directory holds at most two snapshots' worth (the file and its `.tmp` during a write)
plus that much log.

There is no time trigger. `ha.md` 4.9 has a 10-minute trigger to keep the object-store
copy fresh, and a single voter has no copy yet. With ticks alone, the entry rule fires
about every 28 hours on an idle server (86,400 entries a day, section 1). The
thresholds are flags (`--raft-snapshot-entries` and `--raft-snapshot-log-bytes`, the
floor), so tests can set them to a few entries.

### 5.2 How

`RaftMetaLog` locks the host and calls `Host::snapshot`, the existing path:

1. `MetaMachine::snapshot` encodes the state as of the applied index.
2. `Raft::compact(applied)` drops the entries from the core's memory.
3. `Store::persist_snapshot` writes `snapshot.tmp`, syncs it, renames it over
   `snapshot`, syncs the directory, and only then removes the segments that hold no
   entry after the base.

A segment that holds the base and entries after it is kept whole, and the base hides
its earlier entries. So after compaction the log keeps at most one partial segment
(64 MiB by default) plus the active one.

Two changes to the trait are needed:

- `Machine::snapshot` becomes `fn snapshot(&self) -> Result<Vec<u8>, Self::Error>`.
  Encoding cannot fail for a state that arrived through V1 entries, but
  `EncodeError::DigestFunction` exists (`DigestFunction` is non-exhaustive). An encoder
  that panics would take the server down with the lock held, so the error is returned
  instead. The host returns it before it compacts anything, and the log goes on without
  a snapshot. A failed snapshot is logged, and the next tick tries again.
- **A machine that has stopped on an entry** (`MetaMachine::failed`) never takes a
  snapshot. The tick task checks this, and `MetaMachine::snapshot` returns an error.
  Such a state skipped entries that every other replica applied, and a snapshot would
  make that permanent. Today such a log refuses to open anyway; the check keeps it so.

### 5.3 Crashes and errors

| Event | What happens |
|---|---|
| Crash before the rename | `snapshot.tmp` is a leftover. The next `Store::open` removes it and replays the full log. Nothing is lost |
| Crash after the rename, before the segments go | The base hides the covered entries. `Store::open` removes segments it finds wholly covered. Nothing is lost and nothing is applied twice |
| A write or sync error during the snapshot | The store stops (`StoreError` is fail-stop) and the host stops. Every later commit is `Unavailable`, as for any storage error today. The process must restart |
| The snapshot file damaged on disk | Its CRC fails, and `Store::open` refuses the directory. With one voter, that loses the metadata: the entries through the base are gone. This is the new exposure that compaction adds. Before compaction, damage in a segment was equally fatal. Recovery is a store copy (HA, section 8) or rebuilding the index from segment footers, which is not built |

### 5.4 Memory during a snapshot (phase one)

`Machine::snapshot` returns a `Vec<u8>` of size S, and today `encode_snapshot`
(`kbf-store`) copies it into a second `Vec` with the record headers before it writes
the file. Phase one removes that copy: the store writes the header record and then
each chunk record straight from the state slice. That leaves one transient buffer of
S on top of the state's heap. At the sizes of section 7 this is 0.1 GB (measured
point A) to 0.9 GB (point B). Removing that buffer too is the streaming step that
`ha.md` 4.9 asks for, and frames make it a trait change (section 3.3).

## 6. Starting from a snapshot

`RaftMetaLog::open`:

1. Reads the record (unchanged).
2. Opens the store, and **passes the `Recovered` that `Store::open` returns to the
   host**. Today it drops that result and `Host::open` scans the directory again. With
   a snapshot, the second scan would read and copy S bytes twice more. The planned
   form is a `Host::open_with(config, storage, stored, ..)` that takes the `Stored` it
   was given and does not call `load`. `load` stays for callers that have nothing.
3. `Host::open` calls `MetaMachine::restore`. The machine decodes and checks the bytes
   (section 4) and sets `meta`, `bootstrap`, `bootstraps` and `applied = base.index`.
4. The election runs as today. The blank entry commits and applies every entry after
   the base, once each (`Raft::restore` sets `applied` to the base).
5. The `Bootstrap` check runs as today against `machine.bootstrap()`, which now comes
   from the snapshot. The log does not commit a second `Bootstrap`, and a changed
   prefix root is refused.
6. Farm time takes its base from `meta().now()`, as today.

**The test that fails today** is
`crates/kbf-server/tests/raft_log.rs::an_entry_or_snapshot_this_version_cannot_read_refuses_the_open`.
It plants a snapshot of no bytes and expects the open to be refused. That stays true:
empty bytes have no header (`NotAHeader`). The test is split so that the snapshot
half plants a header with an unknown `format` and checks that the error names it.

## 7. Size and pause

### 7.1 Per-entry cost (measured)

**Method.** A throwaway test in `kbf-log` (PR 1 commits it as an `#[ignore]` test):

- It fills a `MetaState` through `execute` with N blobs (`PutBlobs` of 1,000) and M
  actions with five-digest closures.
- It counts live heap bytes with a counting global allocator.
- It times `MetaState::clone` and the `kbf_log::encode` of every blob and action as
  log commands.

The log encoding of a `PutBlob` and a `PutAction` uses the same messages a snapshot
frame uses, so it stands in for the snapshot bytes. The snapshot entry adds a
`last_touch` or `last_hit` field (7 bytes, estimate) and drops `role`.

Run: release build, one thread, a 20-core 2023 desktop CPU (Intel i7-14700K). A server
of an older generation will be slower; an estimate of 1.5x to 2.5x has not been
measured.

| | per blob | per action (closure of 5) |
|---|---|---|
| heap (measured) | 116 to 123 B | 619 to 627 B |
| log encoding (measured) | 56.0 B | 298 B |
| snapshot encoding (estimate: log + 7 B) | about 63 B | about 300 B, plus about 42 B per extra closure digest |
| encode time (measured) | 82 ns | at most 375 ns (this includes building the closure set) |

| Point | blobs | actions | heap (measured) | log-encoded bytes (measured) | encode (measured) | clone (measured) |
|---|---|---|---|---|---|---|
| A | 1,000,000 | 100,000 | 185 MB | 88 MB | 0.12 s | 0.08 s |
| B (10x A) | 10,000,000 | 1,000,000 | 1.79 GB | 0.88 GB | 1.19 s | 0.81 s |

Cost grows linearly: time and bytes per entry are the same at A and B.

### 7.2 Today's scale and 10x (estimates)

There is no production index to measure yet. The sizing therefore starts from the
pilot rate in the code: about 4 operations a second (the comment on
`FINISHED_RETENTION`, `crates/kbf-sched/src/scheduler.rs`). It takes that rate as a
**ceiling**: held around the clock, every operation an executed, cacheable action
that writes one action entry (closure of 5) and 3 new blobs. The 3 is an estimate:
the Action and Command blobs, about 1.9 outputs, with stdout and stderr mostly
duplicates. That gives 345,600 actions and 1.04 million blobs a day.

| Case | blobs | actions | snapshot (estimate) | heap (estimate) | encode (estimate) |
|---|---|---|---|---|---|
| One day at the ceiling | 1.0 M | 0.35 M | 0.17 GB | 0.34 GB | 0.2 s |
| A week at the ceiling, no `Collect` (today: nothing runs `Collect`) | 7.3 M | 2.4 M | 1.2 GB | 2.4 GB | 1.5 s |
| Steady state with `Collect` at the default retention (blobs held about 8 days after the last touch, actions 31 days after the last hit) | 8.3 M | 10.7 M | 3.7 GB | 7.6 GB | 4.7 s |
| 10x the steady state | 83 M | 107 M | 37 GB | 76 GB | 47 s |

What this says:

- **Actions dominate.** That is because actions are retained for 31 days and blobs for
  8. A hit touches the result and every closure blob (`MetaState`'s action query), so
  an action with no hit for about 8 days has usually lost its blobs to `Collect`. From
  then on it can only miss, yet it stays in the index until day 31. Collecting an
  action once its result blob is collected would bound the action map at about 8 days
  (2.8 M at the ceiling, a snapshot of about 1.4 GB). This is a retention change for
  `storage.md`, and it is asked as an open question (section 10).
- **At 10x the heap, not the snapshot, is the limit.** `MetaState` holds the whole
  index in memory on every server (`ha.md` 4.9 says the same). At 76 GB, the
  state does not fit a typical server, whatever the snapshot does. This document does
  not solve that. It records it for the storage design.

### 7.3 The pause

`Host::snapshot` runs under the log's lock. `MetaLog::query` takes the same lock, so
while a snapshot runs, **every commit and every metadata read waits**: `FindMissing`,
`GetActionResult`, uploads. Calls are delayed, not failed. The pause is the encode time
plus the write and sync of S bytes. Disk time has not been measured; at 1 GB/s a 1 GB
write takes about 1 s (estimate).

The budget is `ha.md`'s: 1 s at the target index size.

- **Point A, and the first weeks** (the server starts on a fresh object key prefix,
  and the state starts small): about 0.12 s of encode plus about 0.1 s of write. This
  is within budget, and phase one ships it as is.
- **Steady state at the ceiling:** about 4.7 s of encode plus about 4 s of write. That
  is far over budget. Ways to cut it, cheapest first:
  1. **Write and sync outside the lock.** Split `Host::snapshot` into a step under the
     lock (encode, and note the base) and a step after it (write the file, then lock
     again and compact). This removes the disk time. The store has to allow segment
     appends while it writes the file; that works, because the `.tmp` file shares no
     bytes with a segment.
  2. **Encode outside the lock from a copy-on-write state.** Move the large maps to
     persistent (structurally shared) maps, so taking a snapshot copies a root under
     the lock and the encode runs on the copy. This is `ha.md` 4.9's follow-up.
     `kbf-meta` takes no dependency beyond `kbf-types` and `thiserror` (`ha.md` 4.6),
     so this needs either a decision to allow one or a map written in the crate.
  3. **Shrink the state** (the action retention question of 7.2): about 2.7x fewer
     bytes at the ceiling.

  A plain `clone` under the lock is rejected. At point B it takes 0.81 s against
  1.19 s for the encode, it doubles the heap, and it still pauses.
- **What decides.** PR 3 logs the pause of every snapshot as `snapshot_pause_seconds`
  (an exported metric once the metrics endpoint exists) and the snapshot size. Lever 1
  is the first follow-up when a measured pause exceeds 1 s.

### 7.4 Replay

Today a start replays the whole log, and the log grows by at least 3.1 MB a day from
ticks alone. After compaction, a start decodes one snapshot and replays at most about
the threshold of section 5.1. Start time after a snapshot is about the decode of S
(not measured; it should be of the order of the encode) plus that replay. PR 3 has an
`#[ignore]` test that measures both at points A and B.

## 8. How HA inherits this

`ha.md` 4.9 and its PR 14 ("Snapshot codec for both machines; local snapshots;
compaction; store copy with the manifest last; S1") cover more than this does. This
design is the `MetaState` half of PR 14 plus local compaction for one voter. What HA
keeps, what it adds and where this differs:

| `ha.md` | Here | HA's change |
|---|---|---|
| 4.6: prost messages in `kbf-proto`, `snapshot.proto` beside `log.proto`; no maps; key order; deterministic bytes; strict decode | as written (section 3) | none |
| 4.7: one format version for entries and snapshots, plus a machine version | the header carries both; V1 and 1 | `MachineVersion` raises them; the reader rules of section 4 are the ones 4.7 needs |
| 4.9: a snapshot holds base, membership, both versions, the `MetaState` section, the `FarmMachine` section and its SHA-256 | all but the `FarmMachine` section | adds `FarmMachine` frames under a format bump; a format-2 reader reads a format-1 snapshot as "`FarmMachine` initial" |
| 4.9: encoding streams, never one buffer | phase one builds one buffer (section 5.4); frames allow streaming without a format change | changes the `Machine` and `Storage` traits to a writer and a reader |
| 4.9: every replica snapshots on its own | the same `Host::snapshot` path | runs on every replica; the deterministic bytes let two servers upload the same object |
| 4.9 S1: compact no further than the newest store copy | a single voter compacts against its local snapshot only | adds the store copy and the S1 bound. **Before 1 to 3** (HA PR 32 starts "from a single voter that has compacted its log"), the voter must upload a copy, because a learner catches up by InstallSnapshot by reference. With one voter, a lost disk loses the metadata both today and after this change (`storage.md`, Planned) |
| 4.9 trigger lean: 100,000 entries, 64 MiB or 10 minutes | 100,000 entries, or log bytes of at least max(64 MiB, last snapshot) | open decision 8 of `ha.md` adopts the size-proportional rule or keeps its own; the 10-minute rule exists for the store copy |
| 4.9: `Checkpoint { index, digest }` every 10,000 entries | not here | the trailer's SHA-256 is one possible digest, but encoding the full state every 10,000 entries costs the full encode time; 4.9 should choose a cheaper running digest |
| PR 14 mutant: "drop `next_epoch` from the snapshot" | in PR 1's tests | inherited |

The code that phase one adds stays under HA: the frames, the codec, `from_parts`, the
`MetaMachine` restore and the open from a snapshot. HA adds frames, a store copy and a
streaming transport. It replaces nothing.

## 9. Alternatives rejected

- **Serialize `MetaState` with serde (bincode or postcard).** That would give a second
  encoding of the same values beside `log.proto`, with no strict decoding, no
  canonical-bytes check and nothing tying it to the log's version. `ha.md` 4.6 chose
  prost.
- **One message per section.** Decoding it would need the whole section in memory as
  prost values (section 3.3).
- **Replaying the log in a background thread to produce the snapshot.** No pause, but
  it holds a second full state in memory, needs a reader of live segments and gains
  nothing over lever 2 of 7.3.
- **Rebuilding the index from segment footers instead of snapshots.** The footers hold
  locations, but not touch times, action entries, loss marks, epochs or pins.

## 10. Open questions

1. **Action retention** (7.2): should `Collect` remove an action entry whose result or
   closure blob it is collecting? At the ceiling that cuts the resident state about
   2.7x. **Lean: yes**, decided in `storage.md`, not here.
2. **The pause budget at the target size.** Lever 1 of 7.3 is cheap. Lever 2 needs a
   dependency decision for `kbf-meta`. **Lean:** lever 1 as soon as a measured pause
   exceeds 1 s, and lever 2 only if one is still over after lever 1.
3. **Snapshot trigger** (section 8): should `ha.md` adopt the size-proportional
   rule? **Lean: yes.**

## 11. Pull request plan

Each test names the defect it catches. "Plant" names a mutant that is applied locally
to see the test go red, and is never pushed.

**PR 1 (M): the snapshot codec.** It adds `snapshot.proto`, the `kbf-log` `snapshot`
module (`encode_snapshot`, `decode_snapshot`), and `MetaState::parts` and
`MetaState::from_parts`. No server change. Tests:

- Round trip over generated states (blobs, actions with closures, loss marks of both
  reasons and several generations, `now` and `next_epoch`):
  `decode(encode(s)) == s`, and `encode(decode(b)) == b`. Plant: drop `next_epoch`
  from the header; red.
- The bytes are a function of the state: the same set of commands applied in two
  orders that reach equal states gives equal bytes, and a section of exactly 4,096
  entries, and one of 4,097, splits at the frame size. Plant: let frames split on a
  byte budget instead of a count; red.
- Golden bytes for a small fixed state (two blobs, one action, one loss mark) are
  committed. Plant: renumber a field; red.
- Refusals, each a separate case that names the error:
  - an unknown `format`. Plant: read any format as V1; red;
  - an unknown `machine_version`;
  - a frame before the header;
  - an unknown frame kind. Plant: skip it; red;
  - a section out of order;
  - a key repeated across a frame boundary. Plant: check order only inside a frame;
    red;
  - a field this binary does not know. Plant: drop the re-encode check; red;
  - a trailer count or SHA-256 that differs. Plant: skip the trailer check; red;
  - bytes after the trailer;
  - a location that names an epoch at or above `next_epoch`. Plant: skip `from_parts`
    validation; red.
- Restore then replay equals replay, over generated command sequences split at every
  k: `from_parts(snapshot after k)` plus `execute(k+1..n)` equals `execute(1..n)`.
  The sequences include a late `ObjectReachable` and a `Collect` at a later farm
  time. Plant: drop the generation; red. Plant: drop `last_touch`; red.
- `#[ignore]` measurement test: the numbers of 7.1.

**PR 2 (M): snapshot and restore in the single-voter log.** It covers
`MetaMachine::snapshot` and `restore` and the trait change
(`Machine::snapshot -> Result`). It replaces `NoSnapshots` with the snapshot error. It
adds `Host::open_with`, so the directory is read once. It makes `kbf-store` write the
snapshot file without a second buffer. It adds `RaftMetaLog::snapshot()`, which tests
call directly; nothing triggers it yet. Tests:

- After a snapshot at k, more commits and a restart: the state equals that of a log
  that never took a snapshot, `applied_index` continues, and farm time does not go
  back. Plant: restore with `applied = 0`; red.
- After a snapshot, a restart and a start with another prefix root: refused
  `Mismatch`; with the same root, the open commits no second `Bootstrap`: it commits
  only its blank entry, so the applied index rises by exactly one.
  Plant: leave the `Bootstrap` out of the snapshot; red (a second one is committed
  and the changed root is accepted).
- A snapshot whose header base differs from the file's: refused. Plant: ignore the
  header base; red. A membership other than the record's: refused.
- The test in section 6, split: empty bytes give `NotAHeader`, and an unknown format
  gives an error that names it.
- A power cut at every `FaultFs` operation of `RaftMetaLog::snapshot`: the restart
  holds every answered commit, applied once. Plant: compact the store's segments
  before the rename; red.
- A storage error during a snapshot: every later commit is `Unavailable`. A machine
  stopped on an entry: `snapshot()` is refused and the directory holds no snapshot.
  Plant: snapshot a failed machine; red.
- The open reads the snapshot once: a `FaultFs` read count. Plant: call `load` again;
  red.

**PR 3 (S): compaction on a policy.** It adds the triggers of 5.1 in the tick task, the
two flags, a log line and the `snapshot_pause_seconds` value, and turns snapshots on
in `kbf-sim-cell` with small thresholds. Tests:

- With a threshold of 10 entries, after 25 commits the directory holds a snapshot, no
  segment wholly before the base, and a restart that equals. Plant: the trigger never
  fires; red.
- The log stays bounded: after 1,000 commits with a 4 KiB log floor, the segment bytes
  are under the bound. Plant: never remove segments; red.
- `kbf-sim-cell` crash seeds with a snapshot every few entries: the durability checker
  holds. Plant: restore drops the action map; red.
- The CI test of metadata surviving a snapshot, a restart and a replay is the first two
  tests above run through `kbf-server`'s binary (`tests/cold_restart.rs`): blobs,
  action-cache hits and the log id survive a compacted restart.
- `#[ignore]` measurement test: pause and replay at points A and B (7.3, 7.4).

**PR 4 (S, with invocations PR 7): pins in the snapshot.** It adds the `Pins` frame
under log format 2. Tests:

- A pin, its accumulated closure and its hold-until survive a snapshot and a restart,
  and a `Collect` after the restore keeps every digest of the live pin's closure.
  Plant: leave out the `Pins` frame; red.
- A format-1 reader refuses a format-2 snapshot. Plant: accept it; red.
