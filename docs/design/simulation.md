# Simulation: what is simulated and how

kbf schedules nothing: every Execute is one job of the job service, whose scheduler is
simulated in the jobscheduler repository. What kbf simulates is what it keeps itself: the
Raft log that holds the metadata, the object store writes, and the local write tier. Example
tests pin single interleavings; deterministic simulation explores many. This document lists
what exists, the conventions every simulation keeps, and the one-server cell in detail.

## 1. What exists today

| Harness | Where | What it drives | What it checks |
|---|---|---|---|
| `kbf-sim` kernel | `crates/kbf-sim` | `StateMachine` nodes on a virtual clock, a seeded ChaCha8 stream, a bus with delay, drop, duplicate, reorder and partitions; a SHA-256 trace hash | its own tests: bus faults (`tests/bus.rs`), one seed one trace (`tests/determinism.rs`), a toy counter that a seeded partition breaks (`tests/toy_counter.rs`) |
| Raft simulation | `crates/kbf-raft/tests/sim` | the core driven by its host loop (`kbf_raft::Host`) over an in-memory disk: 3 voters and 1 learner, partitions, crashes just before any of the host's writes, syncs, snapshot writes, sends or applies (the disk keeping what was synced and a seeded prefix of the rest), snapshots and log compaction through the applied index, and restarts from the latest snapshot | Raft safety after every step, no message promising unsynced state, entries applied in order, a snapshot holds exactly the applied entries through its base, liveness after the heal with no peer left behind a snapshot base; 500 seeds in CI, more in an ignored test, and in fault-free runs of 2 seeds a crash of each server at each boundary it passes before the heal, one per run |
| `kbf-sim-cell` | `crates/kbf-sim-cell` | one server as `kbf-server` assembles its metadata log (the `RaftMetaLog` over an in-memory fault-injecting disk), three clients, and an object store that can hang, be slow, fail PUTs or lose their answers; calm, restart, power-cut and I/O-error runs, 64 seeds each, the write path's store-fault and crash-point runs, and the local write tier's runs | properties 1 and 5 to 7 of ha.md section 15, 8 and 9 of the write path, and 10 to 13 of the tier, after every step ([section 3](#3-the-cell-kbf-sim-cell)); replay |

Measured on the dev box in a debug build: `kbf-sim-cell` about 2 s (the sweeps of
`tests/cell.rs`) and about 15 s (the write-path runs of `tests/write_path.rs`, the outages
minutes and hours long).

What is not simulated: the scheduling of actions (the job service's), more than one
server, and membership. The farm scheduler's state, which kbf no longer has, is not in the
cell; properties 2 to 4 of ha.md section 15, which were about it, are not checked and their
numbers are not reused.

## 2. Conventions every simulation keeps

- **A seed is the whole input.** Every random choice comes from one `SimRng` seeded
  by the test; nothing reads a clock or the OS's entropy; every collection is ordered.
  A run records a trace (inputs and effects, as text) and its SHA-256 hash.
- **Trace lines are hand-written, and the hashes are checked in.** Every trace line is
  written field by field (`kbf_sim::text`, never `Debug`), under a first line naming
  the format version, so renaming or moving a type cannot change a line. The hash of
  each pinned scenario and seed is kept in `crates/kbf-sim/tests/golden/trace-hashes.txt`,
  and an ordinary test compares them. A change that only moves code must leave every hash
  as it is. A change that alters a decision or the trace text moves them; regenerate on
  purpose with `cargo test -p kbf-sim -- --ignored regenerate_golden_hashes`, and say in the
  pull request why each changed line is expected.
- **Replayable from one line.** A failing check of the cell panics with the kind, the seed,
  the step, the violated property and the command:
  `KBF_SIM_KIND=<kind> KBF_SIM_SEED=<n> cargo test -p kbf-sim-cell --test cell -- --ignored replay`
  (with `KBF_SIM_CRASH=<point>/<down>` when the scenario pinned one crash).
- **Checked after every input.** The checks run after each step, not only at the end.
  End-of-run checks are for liveness and for what must have drained.
- **A check that cannot fail is not a check.** Each family counts the situations it
  means to reach (a PUT held, a crash that fired, a read during a flush) and asserts the
  sweep reached each, and each check has a unit test that feeds it a violation.
- **Budget.** Each file runs in under 10 s in a debug build on the dev box, so well
  under a minute on CI runners where it can. A large sweep is a bounded CI sweep plus an
  `#[ignore = "long: run with --ignored --release"]` test over many more seeds, with
  the command in the file's header, as the Raft simulation does.
- **A real bug is not simulated away.** A sim that finds one gets a public issue with
  the replay command; the scenario is fixed in its own PR with a test that failed
  before, or lands `#[ignore = "issue #N"]`.
- **Coverage.** A new test raises measured coverage of the crate it tests, and the gate
  holds every crate to 99% of lines and branches, or its floor in `coverage-floors`; a PR
  that raises a crate's coverage raises the floor with `kbf-coverage raise` (see
  CONTRIBUTING.md).

## 3. The cell (`kbf-sim-cell`)

`kbf-sim-cell` is the one-server skeleton of the high-availability design's
simulation (ha.md section 15). Its nodes:

- **The server** runs the code `kbf-server` runs for its metadata log. That log is a
  `RaftMetaLog` (`crates/kbf-server/src/raft_log`) over a `kbf_store::FaultFs`, opened
  as the server opens it, with the new log's id drawn from the seed; the log's futures
  never wait, so each is polled once. An upload goes to the object store first and is
  committed to the log once the store has it; a PUT the store refuses, or does not answer
  within 30 s, is answered `UNAVAILABLE` and indexes nothing. `FindMissingBlobs` is
  answered from the applied index. Farm time is the simulation clock: the server commits
  `Tick(now)` every second.
- **Clients** ask `FindMissingBlobs` for an action's input, upload it if missing, and ask
  again until it is present, resending their step every 8 s.
- **The object store** keeps every object it accepts and loses none. Its faults
  (`StoreFault`): it hangs until a time, answers late for a window, fails the next N
  PUTs or every PUT in a window (keeping nothing), keeps N objects but loses their
  answers, or keeps N and answers that they failed.

Each run has a kind. **Calm**: only the network's faults (1 % drops, 3 % duplicates,
3 % reordering, 1 to 50 ms delays). **Restart**: one or two process kills.
**Crash**: a power cut, at once or part-way through a later commit (the disk keeps
what was synced, a seeded prefix of each unsynced tail and a seeded subset of the
unsynced renames). **IoError**: a filesystem operation fails, the log stops and refuses
every commit while the server goes on answering reads, and the machine reboots. A
server that stopped or died starts again 3 s later over what its disk kept. Every server
fault begins by 60 s, and the run goes on to 210 s. The write path's kinds
([write-tier-scenarios.md](write-tier-scenarios.md#3-what-kbf-sim-cell-has-today)):
**StoreOutage** (the store hangs on even seeds and refuses on odd ones, 2 to 10 min),
**StoreOutageHours** (the same, 60 to 90 min), **StoreSlow**, **StoreErrors**,
**StoreLostAck**, and **StoreCrash** (the server killed or cut off the n-th time it
reaches a point of the write path, while the store is slow). Their runs go on 150 s
past the store's last fault, plus one client retry per counted failure. The local
write tier's kinds ([write-tier-scenarios.md](write-tier-scenarios.md#4-the-tier-in-the-cell-stage-5-as-built)),
whose server stages uploads on `kbf-tier` over a second fault-injecting disk and
flushes them: **TierCalm**, **TierCrash** (a crash at a point of the tier's write or
flush path), **TierStoreOutage**, **TierStoreOutageHours**, **TierDiskFull**,
**TierBadBlock** and **TierEviction**.

The properties, checked after every step (7 at the end). Their numbers are those of
ha.md section 15, whose 2 to 4 were about the farm scheduler and are not checked:

1. Replica equality: a replica replaying the durable log (what a power cut now would
   leave) through the server's applied index has the server's applied state and its
   `Bootstrap`.
5. No false present: every blob answered present is in the object store then (with
   the tier, or in its staged block).
6. Monotone time: farm time never goes back along the durable log or across a restart.
7. Liveness: every client's every blob is answered present by the end.
8. No false ack: every acknowledged upload is held by the object its PUT wrote, and
   present in the index at every later step the server is up.
9. Nothing half-indexed: every blob the index answers present is held by the object
   its location names.
10. Ids never move: a blob's location changes only after its object is marked
    unreachable, and no store object is replaced by other bytes.
11. to 13., in runs with the tier: its bytes stay within its budget; at each start its
    directory holds only what the committed state names; at the end nothing is left
    staged.

Each check also has a unit test that feeds it a violation (`src/check.rs`,
`src/replica.rs`), so none passes because it cannot fail. The defects the write path's
tests catch are listed in
[write-tier-scenarios.md](write-tier-scenarios.md#3-what-kbf-sim-cell-has-today).

Not modeled yet: more than one server, membership, snapshots (the metadata log takes
none), loss of an object the store kept and collection (property 5's later clauses),
clock-rate skew, and a process restart over the page cache after a failed write:
`FaultFs` makes a later sync durable for the bytes a failed one left, so the hazard
`--runtime-dir` is planned to close cannot show.
