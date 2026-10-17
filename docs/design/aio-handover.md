# Updating an all-in-one host without failing a build

Status: design, not implemented. It extends the frontend handover (#857 state, #879
protocol, #887 unit and command; `frontend-handover.md`) to `kbf all-in-one`, where the
scheduler, the storage and the daemon share one process. File references are to the
handover branch (#887) unless marked `[arch-v2]`. Line numbers are a snapshot of branches
that move; the symbol is the authority, and `frontend.rs` is cited by symbol only.
"Inference" marks a claim read from code shape, not from a test. Nothing below is built;
the handover branches are behind arch-v2 (`kbf-tls` landed after their base) and must be
merged before anything is stacked on them.

## 1. Goal and contract

Update the binary of an all-in-one host by starting a second all-in-one process beside the
serving one:

1. builds already running finish on the old process;
2. new build requests go to the new process;
3. the old process exits when it holds nothing.

No client build may fail because of the update. Buck2 never re-sends an Execute whose stream
broke (a stream cut after it started is not recovered), so every Execute the old process has
accepted is answered, by the old process, to the end.

What each party sees:

| party | during the update |
|---|---|
| Buck2 | Open Execute and WaitExecution streams continue on the old process. The old process sends GOAWAY on its REAPI connections; Buck2 opens a new connection for new work and reaches the successor, which owns the same listening socket (`handover/wire.rs`, `drain.rs:80-96`). Inference: after GOAWAY only streams already open continue on a connection, so the old process serves almost no new CAS reads. |
| in-process daemon | Its control stream ends (`UNAVAILABLE`, "connect again"), it redials the same worker address and reaches the successor. Its leases keep running because the epoch is kept (`kbf-daemon/src/daemon.rs:763-797`). It stays in the old process, now as a *detached* worker. |
| successor's daemon | A second worker with its own node id, registered with the capacity the detached one is not using. |
| operator | The operator API and the readiness probes move to the successor; the old process serves none (it publishes a read-only status file and `sd_notify` STATUS, section 6). The successor reports the predecessor as draining and how many leases it holds, from the durable lease rows. |

The model needs no Postgres. SQLite with the single-writer flock, `Store::demote` (the old
process keeps reads and frees the write lock) and the generation fence is enough for the
scheduler state, as it is for `kbf frontend`. Postgres has no `demote`
(`kbf-state/src/store.rs:101-105` returns `Err`) and is only for two serving frontends.

## 2. What exists, what blocks

Reused unchanged from #857/#879 (all in `kbf-server`, `kbf-state`):

- the wire (`handover/wire.rs`: `hello`/`offer`/`ready`/`released`/`failed`/`refused`, up to
  three fds by `SCM_RIGHTS`), `incoming::knock` and `Offer::ready`, `outgoing::Holder`
  (control socket 0600, peer-uid check, refusal list), `handover/notify.rs` and `ready.rs`
  (`READY=1`);
- `listen.rs` (`Socket::bind`/`adopt`, the spare `dup`, the address check) and the
  `Stop`/`Latch` pair;
- `Drain::begin`, `Farm::quiet/fence/follow/end_worker_streams`, the durable `done/` rows
  (`farm/handover.rs`, `farm/durable.rs`);
- `kbf-state`: `Fenced`/`meta/generation`, `Store::demote`, `open_after_handover(generation)`,
  `EpochPolicy::Keep`.

Needs generalizing out of `frontend.rs`: the assembly. `Prepared`, `Trio`, `hello()`,
`take_over()`, `start_or_take_over` and the construction of `Holder`, `Stand` and `Facts`
hard-code the REAPI+worker+API trio, a remote storage
backend, frontend flags and frontend probes. `kbf_server::start` (`start.rs:177-300`
`[arch-v2]`), the path all-in-one uses, has no handover socket, no spare dup, no stop latch
on the worker and API listeners, no `Holder`. Nobody has measured how large the refactor is; section 3.4 gives it a golden so it cannot pass by changing both sides.

Blockers found in code, each handled in section 3:

| # | blocker | evidence |
|---|---|---|
| B1 | A running action dies with its daemon process. The keeper holds a pipe only the daemon holds; EOF kills the run (`kbf-daemon/src/keeper.rs:1-25`, `podman_killed.rs:42-44`). No code lets a new process adopt a run. So the old process must stay alive until its leases end. | `[arch-v2]` |
| B2 | The meta log is single-writer by flock (`raft_log/mod.rs:94-97, 239-255`, `RaftLogError::Locked`). There is no read-only open, no `demote`, no snapshot form (`mod.rs:58-60`). Even a reopen writes (term blank entry, ticks). | `[arch-v2]` |
| B3 | The cold `file://` store takes an exclusive `LOCK` (`kbf-objstore/src/fs_store.rs:239-249`, `FsStoreError::Locked`). | `[arch-v2]` |
| B4 | The hot tier has no lock of its own (grep found none; inference), but at open it deletes block files no staged object names (`kbf-tier/src/open.rs:87,107`). A second opener must not see blocks the first still owns. | `[arch-v2]` |
| B5 | The daemon half dies with the server half (`run.rs:132-145`): when the server stops, `serve_until` ends and running leases are abandoned. | `[arch-v2]` |
| B6 | The in-process daemon's data plane is `LocalDataPlane` over the old process's cache, fixed at construction (`plane.rs`, `kbf-node/src/lib.rs:430`). The control plane is *not* in process: `#743` (control plane as calls) is open and paused; the daemon is an ordinary worker over loopback mTLS. | `[arch-v2]` |
| B7 | Two daemons on one host collide: the start sweep removes every container labelled with the node id and every `leases/kbf-lease-*` directory (`kbf-driver-container/src/runtime.rs:310, 897-924`); the `leases/daemon-instance` claim names the old instance as predecessor, so the server gives up its leases at once (`kbf-node/src/instance.rs:1-11`); container names from lease terms starting at 1 clash (`podman_killed.rs:29-31`); the daemon cache dir lock degrades the second to uncached (`kbf-daemon/src/cache/layout.rs:131-147`). | `[arch-v2]` |
| B8 | The scheduler books by worker: `free() = capacity - booked` (`kbf-sched/src/scheduler.rs:158`). Two workers on one box each report the box's capacity and would double-book it. | handover-2 |
| B9 | The all-in-one start opens a sqlite state store with `EpochPolicy::Bump` (`kbf-server/src/state_store.rs:86`); a bump makes every daemon kill its runs (`daemon.rs:763-797`). The Keep path is `open_after_handover(generation)` (`state_store.rs:121`); all-in-one's start must call it, not merely stop bumping. | `[arch-v2]` |
| B10 | `Drain::begin` is one-way: the `began` swap, `Readiness::stop`, `closer.drain` and the GOAWAY oneshot (`kbf-server/src/drain.rs`); the REAPI server's graceful shutdown completes on that GOAWAY and the listener stops accepting; `Farm::fence` has no unfence; `Stand::run` aborts the sweeper and fires the `Stop` latches. A failed handover after `begin` cannot be undone by any code that exists. | handover-3 |
| B11 | The detached daemon's 40 s fence (`kbf-daemon/src/config.rs`, `FENCE_AFTER`) starts at its last acknowledged heartbeat and cancels its runs when it trips; the successor's `LEASE_GRACE` is 60 s (`kbf-sched` `fence.rs`). The handover gap (log replay, tier open, farm recover, redial backoff) is unmeasured and grows with the age of the log. | `[arch-v2]` |

## 3. Design

Two rules shape everything below.

- **One commit point.** Everything before the GOAWAY (section 3.3, step 3) is reversible and
  has code or a stated new PR behind its reversal. Everything from the GOAWAY on is one
  committed, irreversible block, and the doc says so instead of promising a recovery that B10
  rules out. All fallible or slow work (flush, disk, replay estimate, S's certificates and
  runtimes) is moved in front of the commit point.
- **A budget for the gap.** From the cut of the daemon streams (step 6) to S serving is the
  window the detached daemon's fence must outlast (B11). It is a measured, enforced budget,
  not an open question (section 3.3, "Gap budget").

### 3.1 Storage: who writes, who reads

Three alternatives.

**A. Demote in place (recommended).** Mirror `Store::demote` on every exclusive resource.
The old process gives up the *writer role* of the meta log, the cold store and the hot
tier, keeps what it needs to read, and the successor becomes the only writer. The old
process's reads then run on a `FollowerStorage` (rules below). Its writes go only to the
successor.

**B. Invert the roles.** The old process stays the storage server (`storage_half.rs`
exists) and the successor runs as frontend plus daemon against it. Rejected: the old
process lives as long as its longest action (hours), every build in that time pays an extra
hop, and when the old process finally exits the successor needs a second, unplanned
handover of exactly the resources A moves once.

**C. Do not update all-in-one this way.** Run `kbf frontend` (handover exists, #887) with
`storage-and-daemon` or `storage` and `daemon` hosts, whose daemon is a separate process
that a frontend handover does not touch. Updating storage and daemon one at a time is the
existing fleet path. This stays the answer for sites that cannot afford the work below; A is
the work needed so the small deployment gets the same property.

A needs three new `demote` operations and one new storage wrapper:

1. `RaftMetaLog::demote`: stop the tick task and the committer after the in-flight batch,
   refuse later commits (`Unavailable`), release `log.lock`, keep the applied `MetaState`
   queryable (digest to segment index, action cache). Unknown: whether the Raft host's
   applied state outlives the host stopping (`raft_log/mod.rs:463-470` shows only `Drop`).
   The spike (PR 0) answers this first; if not, the fallback below applies. **Not
   reversible**: a reopen is not a restore (a new term, a blank entry, ticks), so this stage
   lives inside the committed block of section 3.3 and is never "undone".
2. `FsStore::demote`: release `LOCK`, keep reads. Objects are immutable and renamed in
   atomically, so a read needs no lock; a successor eviction makes the old read miss, and a
   miss goes to the successor. `tmp/` is not touched by the old process after demote. Also
   inside the committed block.
3. `Cache::seal` (tier): reversible part, run at prepare (step 0): seal the active block and
   drain the flusher so every staged block is in the cold store. Committed part (step 7):
   a final flush of the delta, then the tier turns **read-only and stays open**. It is not
   closed, so a `ByteStream.Read` open at the GOAWAY finishes from frozen state with no wait
   and no deadline. The successor then opens a hot directory whose blocks are all named by
   the log it replays, so the orphan pass (B4) removes nothing the old process still needs
   to name (a block file unlinked under an open fd stays readable; a later open by path
   misses and goes to the successor). The successor's epoch is newer than any block the old
   process wrote (`kbf-tier/src/error.rs:76-78`), and object keys are `cas/<epoch>/<seq>`
   with a fresh epoch per start (`kbf-front/src/cache/write_group.rs:304-307`), so two
   writers across the handover never collide.
4. `HandoverStorage`: one `Arc<dyn Storage>` behind an atomic swap, used by both the REAPI
   services and `LocalStore::over` (the in-process data plane). States `Local`, `Sealing`
   (counts client writes, section 3.3 step 4), `Follower`. The rules in `Follower`:
   - **Authority questions go to S, never to frozen state.** `FindMissingBlobs` and
     `GetActionResult` (and any lookup whose answer a client acts on by skipping work) are
     answered by S. A frozen "present" can be stale: S may have evicted the blob, the client
     skips the upload and its Execute then fails on a missing input. If S is not yet serving
     the call is parked (below), never answered from frozen state.
   - **Reads of a digest the client names** (`ByteStream.Read`, `BatchReadBlobs`) try frozen
     state first and ask S on a miss. A stale hit here is harmless: the bytes are the bytes.
   - **Writes** go only to S. While S is not serving they are *parked*: they wait until S
     serves, bounded by `--handover-park-secs` (default: the replay budget of the gap
     plus 60 s). Expiry returns `UNAVAILABLE` and is named in section 4 as the one place a
     storage call can fail a build. After the GOAWAY no new client write can start on H
     (h2 graceful shutdown refuses streams above the last-stream id; inference, asserted by
     a test), so `Parked` is in practice the detached daemon's output uploads.

Fallback if the spike shows the log cannot be kept readable after `demote`: the old process
drops its `Cache` and `HandoverStorage` becomes a pure proxy to the successor
(`kbf-front/src/proxy` exists). Cost: the `Cache` (and its lock) drops only when the last
`Arc` does, so reads in flight must end first; the read-only seal above does not apply, and
the pre-commit quiesce (step 2) must also count reads and refuse the handover on timeout.
That fallback is worse for the contract, which is why the spike comes first.

The segment-commit calls the daemon makes (`put_records` through Segments Begin, PutRange,
Commit; the data plane in `kbf-node`) go through server handlers, not the `Storage` trait;
they need the same redirect. Inference: this is the largest part of the storage PR, so it
is its own PR (4b).

### 3.2 The detached daemon

The in-process daemon must outlive its server half (B5, B1) and keep its leases (B9).

- **Process shape.** `kbf all-in-one` gains a stand-down result: after `released`, the
  server half stops serving (REAPI, worker, API) but the process does not exit. The daemon
  half keeps running as a detached worker. The process exits when all three hold: the REAPI
  connections have closed and `Farm::follow` reports nothing waiting (#879's exit rule),
  the daemon holds no lease, and no Result is unacknowledged. A second SIGTERM keeps its
  present meaning: it ends open streams and abandons leases (`serve.rs:429-440`); the cost is
  stated in section 5 (it ends the callers' streams that H holds, which fails those
  builds).
- **Control plane.** No new mechanism: the worker listener is the same socket, passed by
  fd. The daemon's redial rotates the same resolved address after `--reconnect-ms`
  (`daemon.rs:507-549`). Same node id and same instance id, so the scheduler sees the same
  process (`scheduler.rs:786-838`). It resends its Hello and unacknowledged Results before
  its first heartbeat (`daemon.rs:15-19`). It keeps its runs only if the Welcome epoch
  equals its leases' epoch, which is why the successor opens with `EpochPolicy::Keep` through
  `open_after_handover` (B9), and why all-in-one's start path with a takeover must pass that
  policy (T7 asserts the argument on the real start path).
- **Lease adoption, not double booking.** The successor's recovery stores each lease
  `Held{booked, session: u64::MAX}` with `awaited[worker] = now + LEASE_GRACE`, and
  `adopt` books it on that worker when it registers under the same worker id
  (`kbf-sched/src/scheduler/recover.rs:52-145`). So the detached daemon keeps its node id.
  `LEASE_GRACE` starts at recover: it also ends the leases of a daemon not registered by
  then, so the gap budget (below) is also bounded by it.
- **The successor's daemon is a different worker.** Node id gains the instance letter:
  `--node-id=<host>-<instance>`, one certificate per instance (`peers.admit` matches the
  certificate's node id with `Hello.node_id`, `worker.rs:142`). `kbf-all-in-one@a` and
  `@b` are the only two. Then the sweep (owner label = node id), the `daemon-instance`
  claim and the container names are per instance if the daemon's data dir is per instance
  (`<data-dir>/node-<instance>/`, leaving `state.db` and the meta, cold and hot dirs shared
  as storage). Container lease terms and cgroup names must also carry the instance (B7).
  Work, not a flag: audit `runtime.rs` for any name that is not owner-scoped. The per-instance
  dir applies only when `--instance` is given, so existing single-unit hosts keep their
  layout (no data migration) until they take the template.
- **Cordon, and its reset.** The predecessor's worker is cordoned by a durable
  `cordon/<worker>` row (`farm/durable.rs`), restored at recovery. Node ids alternate
  (`-a`, `-b`, `-a`), so the next successor's own node id equals the id cordoned one cycle
  earlier, and the restored cordon would leave its daemon with no grants. Rule: in the same
  transaction as the takeover, S **deletes the cordon row of its own node id** and writes the
  predecessor's. When H exits (its worker deregistered with no leases), S deletes H's worker
  row and cordon row. Consequence, accepted and documented: a takeover clears a manual
  cordon an operator put on that node id; the operator cordons again after.
- **Capacity.** The successor's daemon registers with `total - booked_by(predecessor)` and
  resends its Hello (`farm.resize`, `worker.rs:240-244`) as the predecessor's leases end,
  until it reports the full box. `booked_by` comes from the recovered leases, which the
  farm already knows. Direction guarantee: capacity is derived from *bookings*
  (`request.resources`), which an action can exceed, so the successor can only be wrong by
  over-granting if actions overrun; to keep the error one-sided, resize-up runs only from
  lease-end events S observes (never from a timer), and S never reports more than `total`.
  Unknown (Q6): the container driver's delegated `actions/` cgroup, `cpu.max` and
  `--actions-memory-max-gib` are per unit; whether two units together can exceed the box is
  not read. The capacity PR does not ship before Q6 is answered.
- **Daemon cache.** With one cache (`one_cache.rs`), the daemon input cache is the hot
  store. The detached daemon stages no new inputs (cordoned), so it releases the cache at
  detach; the successor's daemon takes the cache dir lock (otherwise it runs uncached,
  `Disabled::Locked`).
- **Remote daemons.** An all-in-one worker listener may also have remote daemons. They get
  the same cut, redial and 40 s fence, so the gap budget applies to them; they are not
  cordoned, so S places new work on them at once; their output uploads go to the same
  listener, whose clients retry on their own. T10 and T11 include a remote daemon.
- **Upload during the gap.** An action of the detached daemon can finish while S is not
  serving. Its output upload goes through `LocalDataPlane`, `HandoverStorage` (`Parked`)
  and waits for S, bounded by `--handover-park-secs`. Required property: an upload that
  waits on a not-yet-serving successor (and on S dying and being restarted) is retried for
  at least that bound and is never reported as an action failure. Not read yet: the daemon's
  upload retry and the scheduler's treatment of an upload failure. PR 5 reads both and
  records what it found; if the daemon gives up sooner, the Result is held unacknowledged
  until the upload lands.

### 3.3 Sequence

S is the successor, H the holder (old). R reused, N new.

**Phase 1, prepare (reversible, invisible to clients).**

0. **Knock and prepare** (R `incoming::knock`; N). S sends `hello` with the REAPI, worker and
   API addresses and the state path; N: trailing tokens `kind=aio` plus the meta, cold and hot
   paths and the predecessor node id. H refuses if any differs from its own, as it does for
   a frontend (`outgoing.rs:180-210`). N: before the offer H also *prepares*: it seals the
   hot tier and flushes it to cold (`Cache::seal`), checks free space on the cold, meta and
   state directories for the final delta and for S's new term, and evaluates the **gap
   budget**. Any failure answers `refused disk`, `refused replay-too-long` or
   `refused unmeasured`; nothing has changed (this is how disk full is kept reversible,
   section 4). H also refuses if its own detached marker exists (section 5).
1. **Offer and ready** (R). H sends the three fds and the generation; S adopts them and
   checks `local_addr`; S sends `ready`. S has opened no exclusive resource yet; N: S does
   its slow, lock-free work now (certificates, runtimes, flag checks) so the gap is only
   the locked opens.

**Phase 2, refuse and quiesce (reversible).**

2. **Refuse** (N: `Drain::refuse`, `Drain::unrefuse`). Split `Drain::begin` in two. `refuse`
   turns readiness off and makes `closer.drain` refuse new Executes `UNAVAILABLE`, which
   Buck2 retries (the #887 help text; inference, tested by T17). It sends nothing one-way.
   H then waits up to `--handover-quiesce-secs` for `Farm::quiet` (no Result being taken, no
   presubmit in flight). On timeout, or if any phase-2 step fails, H calls `unrefuse`
   (readiness back, executes accepted again), answers `failed quiesce` and keeps serving
   exactly as before; its listeners were never touched and S has not served. #879 ignores
   the quiesce timeout and proceeds; for all-in-one it must not. Both halves of `Drain` are
   unit-tested, and the reversal has a real-process test (T6b: force `failed quiesce`, then
   open a new REAPI connection and run an Execute on H).

**Commit point.** Nothing after this line can be undone.

3. **GOAWAY** (R `Drain::goaway`, the one-way remainder of `begin`). The REAPI listener's
   graceful shutdown completes; the accept loop ends; new connections wait in the kernel
   backlog of the shared socket for S. The remaining `Drain::begin` behavior is exactly
   today's.
4. **Write drain** (N). `HandoverStorage` to `Sealing`: H counts REAPI client writes
   (`ByteStream.Write`, `BatchUpdateBlobs`, Segments commits) and waits for zero, bounded by
   `--handover-write-drain-secs` (default 120). Everything else is still live: the scheduler
   is not fenced, the daemon streams are up, so the 40 s clock has not started, and Results
   and daemon uploads continue. After the GOAWAY no new client stream starts, so the count
   only falls. Reads are not counted (the tier stays readable). On expiry the remaining
   writes are cut `UNAVAILABLE` and counted (`kbf_handover_writes_cut_total`); that is the
   one deliberate build-failure risk of the sequence and it needs a client writing one
   blob for longer than the bound (section 4). New builds stall in the backlog for at most
   this long.
5. **Fence** (R `farm.fence`, sweeper abort). The scheduler takes no input: Results are
   refused, no grants.
6. **Cut the daemon streams and listeners** (R `worker_stop`, `end_worker_streams`,
   `api_stop`). The in-process daemon's stream ends with `UNAVAILABLE`; it starts its
   reconnect wait. Its 40 s fence clock started at its last acknowledged heartbeat, up to one
   heartbeat interval before this step; this is where its server contact stops.
7. **Demote block** (N, in this order, no waits, each a lock release or a flag flip):
   final flush of the delta and tier read-only; `RaftMetaLog::demote`; `FsStore::demote`;
   `HandoverStorage` to `Follower`; release the daemon cache; write the detached marker
   (section 5); `store.demote()` (R) last. Any failure here is a *committed* failure:
   H fails closed. It calls `drain.close()` (as #879 does), answers `failed`, exits non-zero
   without the marker, and systemd restarts it as a cold start. Builds on H fail and its
   actions rerun; operator alert (`kbf_handover_demote_failed_total`). The stages are kept
   few and lock-only so this is the unlikely class, and each has a failpoint test (T6).
8. **Released** (R). H sends `released`. S now: `open_after_handover(generation)` (R; takes
   the flock at the next generation, `EpochPolicy::Keep`); N: `RaftMetaLog::open_dir` (new
   term, `AllocEpoch`), `FsStore::open`, tier open, `Cache::open`; `Farm::with_state`;
   in one transaction delete the cordon row of its own node id and cordon the predecessor's;
   start S's daemon with the reduced capacity; record the measured replay time
   (`handover/replay_ms` in the state dir, for the next prepare); serve on the adopted
   sockets. Connections that arrived in the gap wait in the kernel backlog. S sends
   `READY=1` when its probes pass (R `ready.rs`), so `systemctl start` of the idle instance
   returns when S takes builds.
9. **Follow and drain** (R `Holder::run`, `Farm::follow`; N). H answers its open Execute
   and WaitExecution streams from the `done/<op>` rows S writes, every
   `--handover-follow-interval-ms`; an operation with no trace for three looks ends
   `UNAVAILABLE` and counts in `kbf_frontend_relay_lost_total` (`farm/handover.rs:19-30`).
   Meanwhile the detached daemon reconnects, S adopts its leases, its Results are accepted
   from the node that holds the lease (`worker.rs:261-277`), and its outputs go to S's
   storage through `FollowerStorage`. H exits when the three conditions in section 3.2
   hold.

**Gap budget.** The gap is step 6 to S serving: replay of the meta log (never compacted, so it
grows with the age of the host), tier open, farm recover, and the daemon's redial wait
(`--reconnect-ms`). It is enforced, not hoped for:

- every S start records its measured replay and open time in `handover/replay_ms`;
- at step 0 H refuses (`refused replay-too-long`) when `recorded_replay + reconnect_ms +
  heartbeat_interval` exceeds half of `FENCE_AFTER` (20 s today), and refuses
  (`refused unmeasured`) when nothing is recorded; the first start into the template is a
  cold restart that records it (section 5);
- S reports the real gap (`kbf_handover_gap_seconds`) and an alert fires above 50 percent
  of the fence;
- the replay measurement on a real log is part of the spike (PR 0) and gates the whole
  design: if it can exceed the budget on a realistic host, log snapshots (a separate
  design) come first.

A fence extension in the Welcome (so a handover-aware daemon fences later) is rejected as
the default: the fence is the only thing that stops a partitioned daemon running work the
scheduler has given up on, and it needs `T + 5 < G` to keep holding. It stays the fallback
if replay cannot be bounded.

The one window with no recovery is unchanged in kind from #879: between `released` and S
serving. If S dies there, H cannot take the stores back. Systemd restarts S as a cold start
(epoch bump; the daemons rerun their leases); H still answers its callers from the rows.
The bump makes the *detached* daemon cancel its runs when it reaches S, so the builds
survive and the actions rerun. Slower, not a failure; T12 pins it.

### 3.4 What changes in start-up code

- `frontend.rs`'s assembly (`Prepared`, `Trio`, `hello`, `take_over`, `start_or_take_over`
  and the construction of `Holder`, `Stand` and `Facts`) becomes a `handover::Driver` that
  both modes call: `bind_or_adopt(trio)`, `Facts`, a list of `StandDownStage` objects
  (frontend: none beyond #879's; all-in-one: section 3.3), a list of `Ready` checks.
  Frontend behavior stays byte for byte; proof is the existing tests plus a golden of the
  Driver's call sequence for frontend taken before the refactor (stage list, fd count, hello
  bytes), so the refactor cannot pass by changing both sides.
- `kbf all-in-one` gains `--handover-socket` (requires the sqlite `--state-store`, as for
  the frontend), `--handover-quiesce-secs`, `--handover-write-drain-secs`,
  `--handover-park-secs`, `--handover-follow-interval-ms`. All default off: without
  `--handover-socket` the process behaves as today.
- `kbf-all-in-one/src/run.rs` replaces `start_interruptible` with `start_or_take_over` and
  keeps the daemon half alive on detach (`serve_until` gets a third reason to end: idle after
  detach). `start_or_take_over` itself refuses a takeover when the instance's detached marker
  exists; the guard is in the process, not only the CLI.
- Configuration stays flags, no environment variable beyond `NOTIFY_SOCKET`.

## 4. Failures

Columns: what Buck2 sees, what the daemons and the operator see, recovery. "Class" is
reversible (before the commit point) or committed (after).

| point | class | Buck2 | daemon / operator | recovery |
|---|---|---|---|---|
| S dies or is refused before `ready` | reversible | nothing | operator: `systemctl start` fails (`Refused` or timeout, `TimeoutStartSec`) | H never changed (`outgoing.rs:153-155`); fix and retry |
| prepare refuses (disk full, flush error, replay-too-long, unmeasured) | reversible | nothing | S exits non-zero with the reason | H unchanged; fix and retry |
| quiesce times out, or a phase-2 step fails | reversible | new Executes got `UNAVAILABLE` for up to the quiesce bound, retried | S exits; H `unrefuse`s | H keeps serving, same listeners (never GOAWAYed); retry. T6b. |
| write drain expires (a client write longer than the bound) | committed | that write ends `UNAVAILABLE`; the build fails | counter `kbf_handover_writes_cut_total` | the only designed client-visible failure; raise the bound or update at a quiet time. Bound default 120 s |
| a demote-block stage fails, including `store.demote()` | committed | open streams end `UNAVAILABLE` (`drain.close`); builds on H fail | H exits non-zero without a marker; systemd cold-starts it; alert | actions rerun; the unlikely class (lock releases only), failpoint-tested |
| client storage call on H in the gap | committed | `FindMissing` and action-cache lookups go to S and wait (parked); reads of named digests served from frozen state or S | parked wait is bounded by `--handover-park-secs`; expiry `UNAVAILABLE` | expiry is a build-failure risk, sized by the replay budget; metric `kbf_handover_parked_wait_seconds`. T18 |
| detached daemon's output upload during the gap | committed | none if the upload retries through the gap | parked then lands on S; Result sent after the upload | property required in section 3.2; PR 5 reads the retry path; T11 |
| S dies after `released`, before serving | committed | new requests wait in backlog until systemd restarts S | detached daemon cancels its runs on S's bumped epoch, reruns after grace | H still answers callers from rows; leftover actions rerun. T12 |
| S dies after serving, H still draining | committed | new builds see the connection drop (as any server crash) | detached daemon fences after 40 s of no ack | systemd restarts S cold; H's leftovers rerun; H's callers are answered from rows S wrote |
| gap exceeds the daemon fence (should be refused at prepare) | committed | H's callers see no error (streams are H's) | detached daemon cancels its runs, reruns; alert on `kbf_handover_gap_seconds` | actions rerun, slower; if it occurs the budget is wrong: lower it. T15 asserts zero reruns under budget |
| H dies mid-follow | committed | its open streams break (unavoidable: the process holds the stream) | its daemon and keepers die (B1); S requeues after grace; the marker remains | systemd's `ExecCondition` refuses to restart H into a takeover (section 5); the operator acknowledges, then the next handover is explicit. T16 |
| detached daemon cannot reach S | committed | none while H lives | daemon fences its leases after 40 s, reruns elsewhere | S's grace gives up the leases |
| disk full in the middle | committed | a write fails as it would with no update | daemon uploads retry | free space |
| a Result in flight across the handover | committed | none if the epoch is kept: unacknowledged, resent after Welcome, before the first heartbeat | S accepts it from the lease holder | untested today; T11 |
| two updates back to back | n/a | n/a | `kbf handover` refuses when both instances are active | bounded by the longest action; the hotfix path and its cost are in section 5 |

Reading of the table: the handover fails a build only through the rows already true for #879
(an operation with no trace for three looks; a second-signal escalation; a crash of the
process that holds the stream) plus two designed ones, the write-drain expiry and a parked
call that outlasts its bound, plus a committed demote failure. The first and the second are
bounded by flags and measured; the last is kept small by construction.

## 5. systemd shape

`kbf-all-in-one@.service`, a template with two instances, mirroring `kbf-frontend@.service`
(#887):

- `Type=notify`, `NotifyAccess=main`; `TimeoutStartSec` is written by `install-units` as
  `max(120, 3 x recorded replay)`; `READY=1` as in section 3.3.
- `RuntimeDirectory=kbf-all-in-one`, `RuntimeDirectoryMode=0700`,
  `RuntimeDirectoryPreserve=yes`; `--handover-socket=/run/kbf-all-in-one/control.sock`; both
  instances run as the same user. A stale `control.sock` after a crash is unlinked by the
  next starter only after a peer-uid check and a failed connect; a live one means handover.
- `KillSignal=SIGTERM`, `TimeoutStopSec=infinity`, `Restart=on-failure`,
  `DefaultInstance=a`, no `Conflicts=` between instances.
- Same `Slice=kbf.slice`, `Delegate=yes` and `OOMScoreAdjust` as the single unit
  `[arch-v2] kbf-all-in-one.service`. `install-units` refuses when free memory is below the
  sum of both instances' `MemoryMin` (two idle-plus-draining processes must fit); open
  question 6 covers the cgroup caps.
- `--node-id=<host>-%i`, `--instance=%i`, `--data-dir=<dir>` with per-instance daemon
  subdirectory, certificates per instance from the install step.

**Detached lifecycle (decided).**

- Marker `/run/kbf-all-in-one/detached-%i`, per instance, in the `RuntimeDirectory`. It is
  written in step 7 before `released`, removed by the process on its clean exit after its
  keepers are gone, and cleared by a reboot (correct: a reboot kills the daemon and the
  actions anyway).
- The unit has `ExecCondition=/usr/bin/kbf all-in-one precheck --instance=%i`, a subcommand
  (so the logic is testable without systemd) that exits non-zero when the marker exists. A
  marker present when the unit is *inactive* means the detached process died: start is
  skipped, not failed, so `Restart=on-failure` cannot restart it into a takeover of a live
  successor. `start_or_take_over` repeats the check in the process.
- `kbf handover` reports a skipped instance with a marker as "crashed while detached: its
  actions are gone", and `kbf handover --acknowledge-crash <instance>` clears the marker; that
  is the only way back, never automatic.
- While detached the unit reads `active (running)`. The process keeps `STATUS=detached: N
  leases, oldest M min, serving <S instance>` current through `sd_notify` and writes the same
  as a read-only status file `/run/kbf-all-in-one/status-%i`, so H stays observable when S is
  down. `kbf handover status` reads both instances' files and S's operator API.

Command: generalize `kbf frontend handover` (`kbf-cli/src/handover.rs`) to `kbf handover
--unit kbf-frontend|kbf-all-in-one`, with the same rules (exactly one `@` instance active:
start the other; none: start `@a` unless a marker blocks it; both active or any
transitioning: refuse, exit 1; only `show` and `start`, never `stop` or `restart`; `--wait`
polls the old instance until inactive). `install-units --mode all-in-one` writes the
template, enables `@a` only, starts `@a` if neither is up and never restarts a serving
instance; check-units gets the same `handover` rule, and the existing single unit becomes the
legacy shape check-units warns about. Existing hosts migrate by one cold restart into the
template, which also records the first replay measurement.

**Binary path and the updater.** The binary is not swapped by rename. The updater stages
`/opt/kbf/<sha>/kbf` (versioned, immutable), and `ExecStart` runs `/opt/kbf/current/kbf`, a
symlink. `/proc/self/exe` of a running process (which keepers re-execute, `keeper.rs`)
therefore stays the versioned file; the updater's garbage collection keeps every version a
running process still maps (found through `/proc/*/exe`) plus the newest two. Order: stage,
then **check that both instances are not active, then flip the symlink, then** run `kbf
handover --unit kbf-all-in-one --wait`. A refusal (both active) before the flip means the
previous instance is still draining: the updater leaves the symlink alone, waits and retries,
and reports "draining: N leases, oldest M minutes" read from the status files. So a staged
binary never sits renamed behind a waiting handover.

**Long drains and a hotfix.** The update rate is bounded by the longest action on H. The
updater has `--max-detached-hours` (default 12): beyond it, it raises an alert with the lease
list; it never forces. The operator's escape is a second SIGTERM to H, which ends the open
streams H holds and abandons its leases: callers on H see `UNAVAILABLE` and Buck2 does not
recover an Execute whose stream broke, so **this fails those builds and loses the work**; it is
documented as such and is not a no-cost option. A security fix that cannot wait for a
multi-hour action pays that, by explicit operator decision.

## 6. Observability

Metrics (`kbf_handover_*`, on S's operator API, and on H's status file while detached):
`phase` gauge (prepare, refuse, committed, released, serving), `gap_seconds` histogram,
`parked_writes_total` and `parked_wait_seconds`, `refused_total{reason}`,
`writes_cut_total`, `demote_failed_total`, `detached_leases` and `detached_oldest_seconds`,
plus `kbf_frontend_relay_lost_total` (exists). Each step logs a structured line with its step
index and elapsed time. Alerts: gap above 50 percent of the fence; detached age above
`--max-detached-hours`; any `demote_failed_total` or `writes_cut_total`; S's capacity below
total for longer than the longest known action.

## 7. Tests

Each test names the defect it catches and the line that turns it red. Unit tests sit in the
library they cover; `kbf-it` tests use real processes. A mutant is planted and seen red on
the farm before a PR goes up; the PR description records which mutant. No test pins a wrong
answer.

| id | test | catches | mutant |
|---|---|---|---|
| T1 | `RaftMetaLog::demote` then a second `open_dir` succeeds while the first answers digest and action-cache queries | lock not released, or state dropped | skip `file.unlock` in `demote`; or clear applied state |
| T2 | the demoted log refuses `commit` with `Unavailable`; the new log's term is greater | two writers | leave the committer running |
| T3 | `FsStore::demote` then second `open` succeeds; reads work; `tmp/` of the first untouched | B3 | `demote` keeps `LOCK` |
| T4 | the test builds the staged blocks itself (not in setup) and calls `Cache::seal` then the final flush: no staged block outside the cold store; the successor's orphan pass deletes no block the log names; the sealed tier still serves a read opened before the seal | B4, data loss, a read dropped by a close | skip the final flush; close the tier instead of read-only |
| T5 | `HandoverStorage`: `FindMissing` and action-cache lookups in `Follower` go to S even when frozen state says present; named reads hit frozen state, miss to S; parked writes land on S and wait for it; parked expiry is `UNAVAILABLE` | stale hit, lost write | answer `FindMissing` from frozen state; return NOT_FOUND on miss |
| T6 | real stages (real `RaftMetaLog`, `FsStore`, tier on temp dirs) with a failpoint at each stage index of the demote block: assert the specified fail-closed outcome (streams end, non-zero exit, no marker), and that a cold start then serves; plus an ordering test (state-store demote last) | an undetected partial demote; irreversible step in the wrong place | move `store.demote` earlier |
| T6b | unit and `kbf-it`: `Drain::refuse` then `unrefuse`; force `failed quiesce`, then a new REAPI connection runs an Execute on H and `/readyz` is ready again | the recovery that B10 said had no code | make `unrefuse` a no-op |
| T7 | the all-in-one start path with a takeover passes `EpochPolicy::Keep` (assert on the policy argument); a recovered lease books on the predecessor's node id; S's daemon registers with `total - booked`; a **two-cycle** test (a to b to a): the second successor receives grants because it deleted its own cordon row | B8, B9, the stale cordon | register full capacity; `Bump`; skip the cordon-row delete |
| T8 | run the real start sweep against a fake container runtime holding two owner labels: the survivor's containers and lease dirs remain | B7 (a path test does not cover the label match) | share the data dir; sweep by prefix |
| T9 | a golden of the Driver's call sequence for frontend taken before the refactor, plus the existing frontend suite | regression in `kbf frontend`, a refactor that changes both sides | reorder two stages in the Driver |
| T10 | `kbf-it`, real processes, with a remote daemon too: old all-in-one runs a long action; start the successor; H finishes it; its output is readable through S; an Execute streamed longer than any keepalive interval, started before the handover, completes | the whole contract | kill H at `ready`; test red on the stream |
| T11 | `kbf-it`: an action finishes during the gap (local and remote daemon): its upload parks and lands, its Result is accepted once | unacknowledged Result and upload rows | drop unacknowledged Results; give up the upload early |
| T12 | `kbf-it`: S dies after `released` (failpoint); a cold start serves, callers of H get answers | the unrecoverable window | H exits on `released` |
| T13 | an injected `FreeSpace` and a test-only `Storage` wrapper return ENOSPC at prepare: handover refused, H keeps serving, no stream broken (no real disk is filled) | disk full reversibility | skip the prepare check |
| T14 | refused handover (different `--listen`) leaves the holder serving | #879's refusal path for the new tokens | accept without comparing the new tokens |
| T15 | the fence is a test-only flag (seconds, not 40): below budget the detached daemon reconnects and the **rerun count is zero**; above it the leases fence and rerun and no caller sees an error from H | the fence interaction | raise the daemon's fence beyond the gap; remove the budget refusal |
| T16 | `kbf-it`: SIGKILL a detached H; S is unaffected; `kbf all-in-one precheck` exits non-zero so the unit would not restart into a takeover; `--acknowledge-crash` clears; a clean H exit removes the marker | the restart-into-takeover | write the marker after `released` instead of before |
| T17 | `kbf-it` with the tonic/h2 stack Buck2 uses: many iterations of unary `FindMissing` and Execute issued continuously through the handover, a `ByteStream.Write` and `Read` open across the GOAWAY, a slow `Read` held open through the whole handover, an Execute longer than the keepalive; assert zero errors reach the caller. Before the last PR: one real Buck2 build through a handover | the two client behaviors assumed (Buck2 retries a refused Execute; a channel reconnects after GOAWAY) | stop `refuse` from refusing; close the tier |
| T18 | `kbf-it`: concurrent CAS traffic and an open Execute across the handover; S evicts a blob after the handover and then a `FindMissing` reaches H: answer comes from S | stale hit; parked bound | answer from frozen state |

CI placement. Real processes with a long action need a real container: they extend the
existing `podman-x86` job (`cargo xtask podman-tests`, Delegate=yes system unit, x86_64
only; the harness is `crates/xtask/src/podman_tests.rs`); T10, T11 and T15's container leg run
there and are x86-only. Everything that needs no container (T12, T13, T14, T16, T17 against
the fake driver, T6, T6b) runs in `test-x86` and `test-arm`; the arm job asserts the
fake-driver forms and says so. The failpoints (after-`released`, each demote stage) and the
short-fence flag live behind a cargo feature `handover-failpoints` with a gate test that
fails when a release build enables it. T13 injects ENOSPC rather than filling a disk (no
root or loop mount on a hosted runner).

## 8. PR split

PRs stack on #879 and #887 only where they touch `frontend.rs`, `handover/*`, `kbf-deploy`
or `kbf-cli`. Conflict check, to be run at each PR: `git diff --name-only <base>...origin/feature/arch-v2`
against the files the PR touches. Today the handover-3 branch touches `kbf-server`
(`drain.rs`, `farm/*`, `frontend.rs`), `kbf-front/src/metrics.rs`, `kbf-sched/.../view.rs`,
`kbf-deploy`, `kbf-cli` (`lib.rs`, `handover.rs`); it does not touch `kbf-all-in-one`
(`run.rs`, `cli.rs`), `kbf-node` or `kbf-daemon/src`, so PRs 5 and 7 are disjoint from it at
the file level (recheck after rebase; the `kbf-all-in-one` help golden is shared). Sizes are
changed lines, an estimate.

| PR | content | size | base |
|---|---|---|---|
| 0 | **throwaway spike branch, not merged**: `RaftMetaLog::demote` prototype; measure replay on a real aged log; answer whether state stays readable and the real gap; output is an amendment to this note | 300 | arch-v2 |
| | owner decision (Q9) on whether to proceed, after PR 0 | | |
| 1 | `FsStore::demote` + T3 | 150 | arch-v2 |
| 2 | `RaftMetaLog::demote` (final form) + T1, T2 | 300 | arch-v2 |
| 3 | `Cache::seal`, read-only tier + T4 | 300 | 1 |
| 4a | `HandoverStorage`, `FollowerStorage` + T5 | 350 | 2, 3 |
| 4b | Segments redirect (daemon commit calls) + test | 250 | 4a |
| 5 | `kbf-node`: detached daemon (stay alive, idle exit) behind `--handover-socket`, per-instance data dir behind `--instance` (default off: no behavior or layout change for existing hosts), cache release; reads the daemon upload retry path + T8 | 450 | arch-v2 |
| 6a | `Drain::refuse`/`unrefuse` split + T6b unit | 200 | #879, #887 |
| 6b | `handover::Driver` out of `frontend.rs`, no behavior change + T9 | 500 | 6a |
| 7a | all-in-one wiring: flags, `start_or_take_over`, sequence, marker, `Keep` + T6, T7 (wiring), T16 unit | 450 | 4b, 5, 6b |
| 7b | cordon reset, capacity resize, worker-row cleanup + T7 (two-cycle) | 250 | 7a |
| 8 | `kbf-deploy`: `kbf-all-in-one@.service`, install and check rules, `precheck`, generalized `handover`, versioned binary path, updater order | 450 | 7a, #887 |
| 9 | `kbf-it` T10 to T18, failpoint feature, CI placement | 700 | 7b, 8 |

Order: PR 0, then the owner decision, then 1, 2 in parallel; 3, 5, 6a in parallel; 4a, 4b,
6b; 7a; 7b and 8; 9. Everything from PR 1 on is gated on the spike result (not only PR 4):
if PR 0 says the state is not readable after demote, or the gap cannot be bounded, the note is
amended (proxy fallback, snapshots) before any of them. PRs 4 and 5 are inert until 7a (flags
default off), so they ship through the pull updater without changing a host. Each PR names
what its tests prove and the mutant it was seen red on.

## 9. Open questions

1. **Does `RaftMetaLog` keep its applied state readable after the Raft host stops?**
   Recommendation: PR 0 answers it before anything else; if no, take the proxy fallback
   (section 3.1), not a read-only open, which touches the Raft host's election.
2. **Node id per instance.** `<host>-a`, `<host>-b` with two certificates, versus admit
   rules that accept a suffix. Recommendation: two certificates; it needs no change in
   `peers.admit`.
3. **Wire change.** Trailing tokens on `hello` versus `PROTOCOL=2`. Recommendation: trailing
   tokens, and check that a v1 holder tolerates unknown trailing tokens; if it does not,
   the first update to this version is a cold restart, once.
4. **Capacity split.** Resize-up by a lease-end-driven task in the successor, versus a shared
   box notion in the scheduler. Recommendation: the resize task; the scheduler change touches
   placement for every deployment.
5. **Write-drain bound.** 120 s default is a guess at "no client writes one blob longer".
   Recommendation: measure the largest Buck2 upload on a real build before PR 7a, and set
   the default above it.
6. **Cgroup and memory limits across two units.** Recommendation: one `actions/` parent
   shared by both instances so `cpu.max` and the memory cap apply to the sum; verify before
   PR 7b.
7. **Cold store kind.** `file://` needs `FsStore::demote`; an object-store cold store has
   no lock. Recommendation: keep `file://` support (the all-in-one unit uses it), do not
   require an object store for this.
8. **Is the all-in-one worth this work?** Alternative C already updates a fleet without
   failing a build. Recommendation: the owner decides after PR 0 (cheap, throwaway), before
   any merged PR; build only if yes.
9. **#743.** If the in-process control plane lands, the detached daemon's control plane is a
   new feature (dial the successor), not a redial. Recommendation: do not land #743
   before this design is built; it removes the mechanism section 3.2 reuses.

Reviewed and not taken: (a) a Welcome-carried extended fence as the default (section 3.3,
rejected; it weakens the only partition safeguard). (b) The claim that a second SIGTERM
"fails nothing client-side": H holds its callers' streams, so ending them fails those builds;
section 5 says so. (c) A single `Parked` bound tied to measured replay: taken as a flag
(`--handover-park-secs`) whose default is derived from the replay budget, because the budget
is enforced rather than known in advance.
