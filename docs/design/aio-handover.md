# Updating an all-in-one host without failing a build

Status: design, not implemented. It extends the frontend handover (#857 state, #879
protocol, #887 unit and command; `frontend-handover.md`) to `kbf all-in-one`, where the
scheduler, the storage and the daemon share one process. File references are to the
handover branch (#887) unless marked `[arch-v2]`. "Inference" marks a claim read from code
shape, not from a test. Nothing below is built; the handover branches are behind arch-v2
(`kbf-tls` landed after their base) and must be merged before anything is stacked on them.

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
| operator | The operator API and the readiness probes move to the successor; the old process has none. The successor reports the predecessor as draining and how many leases it holds, from the durable lease rows. |

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
(`frontend.rs:490-565, 788-818`) hard-code the REAPI+worker+API trio, a remote storage
backend, frontend flags and frontend probes. `kbf_server::start` (`start.rs:177-300`
`[arch-v2]`), the path all-in-one uses, has no handover socket, no spare dup, no stop latch
on the worker and API listeners, no `Holder`. Nobody has measured how large the refactor is.

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
| B9 | The all-in-one start opens the state store with an epoch bump (`cli.rs:34`); a bump makes every daemon kill its runs (`daemon.rs:763-797`). | `[arch-v2]` |

## 3. Design

### 3.1 Storage: who writes, who reads

Three alternatives.

**A. Demote in place (recommended).** Mirror `Store::demote` on every exclusive resource.
The old process gives up the *writer role* of the meta log, the cold store and the hot
tier, keeps what it needs to read, and the successor becomes the only writer. The old
process's reads then run on a `FollowerStorage`: frozen local state first, on a miss ask the
successor over the worker listener as an ordinary remote client. Its writes go only to the
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
   PR 2 is a spike that answers this first; if not, the fallback below applies.
2. `FsStore::demote`: release `LOCK`, keep reads. Objects are immutable and renamed in
   atomically, so a read needs no lock; a successor eviction makes the old read miss, and a
   miss goes to the successor. `tmp/` is not touched by the old process after demote.
3. `Cache::stand_down` (tier): seal the active block, drain the flusher so every staged
   block is in the cold store, then close the tier. The successor then opens a hot
   directory whose blocks are all named by the log it replays, so the orphan pass (B4)
   removes nothing the old process owns. The successor's epoch is newer than any block the
   old process wrote (`kbf-tier/src/error.rs:76-78`), and object keys are
   `cas/<epoch>/<seq>` with a fresh epoch per start (`kbf-front/src/cache/write_group.rs:304-307`),
   so two writers across the handover never collide.
4. `HandoverStorage`: one `Arc<dyn Storage>` behind an atomic swap, used by both the REAPI
   services and `LocalStore::over` (the in-process data plane). States `Local` then
   `Parked` (new write calls wait, bounded) then `Follower`. This is how the detached
   daemon's output upload reaches the successor without a second plane: its `LocalDataPlane`
   keeps calling the same trait object.

Fallback if the spike shows the log cannot be kept readable after `demote`: the old process
drops its `Cache` and `HandoverStorage` becomes a pure proxy to the successor
(`kbf-front/src/proxy` exists). Cost: a read stream in flight across the swap must finish
first, because the `Cache` (and its lock) drops only when the last `Arc` does; the quiesce
step waits for zero in-flight storage calls and refuses the handover (section 3.3, step 2)
rather than cutting a stream.

The segment-commit calls the daemon makes (`put_records` through Segments Begin, PutRange,
Commit; `plane.rs`) go through server handlers, not the `Storage` trait; they need the same
redirect. Inference: this is the largest part of PR 4.

### 3.2 The detached daemon

The in-process daemon must outlive its server half (B5, B1) and keep its leases (B9).

- **Process shape.** `kbf all-in-one` gains a stand-down result: after `released`, the
  server half stops serving (REAPI, worker, API) but the process does not exit. The daemon
  half keeps running as a detached worker. The process exits when all three hold: the REAPI
  connections have closed and `Farm::follow` reports nothing waiting (#879's exit rule),
  the daemon holds no lease, and no Result is unacknowledged. A second SIGTERM keeps its
  present meaning: it ends open streams and abandons leases (`serve.rs:429-440`).
- **Control plane.** No new mechanism: the worker listener is the same socket, passed by
  fd. The daemon's redial rotates the same resolved address after `--reconnect-ms`
  (`daemon.rs:507-549`). Same node id and same instance id, so the scheduler sees the same
  process (`scheduler.rs:786-838`). It resends its Hello and unacknowledged Results before
  its first heartbeat (`daemon.rs:15-19`). It keeps its runs only if the Welcome epoch
  equals its leases' epoch, which is why the successor opens with `EpochPolicy::Keep` and
  all-in-one's start path must stop bumping when it took over (B9).
- **Lease adoption, not double booking.** The successor's recovery stores each lease
  `Held{booked, session: u64::MAX}` with `awaited[worker] = now + LEASE_GRACE`, and
  `adopt` books it on that worker when it registers under the same worker id
  (`kbf-sched/src/scheduler/recover.rs:52-145`). So the detached daemon keeps its node id.
- **The successor's daemon is a different worker.** Node id gains the instance letter:
  `--node-id=<host>-<instance>`, one certificate per instance (`peers.admit` matches the
  certificate's node id with `Hello.node_id`, `worker.rs:142`). `kbf-all-in-one@a` and
  `@b` are the only two. Then the sweep (owner label = node id), the `daemon-instance`
  claim and the container names are per instance if the daemon's data dir is per instance
  (`<data-dir>/node-<instance>/`, leaving `state.db` and the meta, cold and hot dirs shared
  as storage). Container lease terms and cgroup names must also carry the instance (B7).
  Work, not a flag: audit `runtime.rs` for any name that is not owner-scoped.
- **Capacity.** The successor's daemon registers with `total - booked_by(predecessor)` and
  resends its Hello (`farm.resize`, `worker.rs:240-244`) as the predecessor's leases end,
  until it reports the full box. `booked_by` comes from the recovered leases, which the
  farm already knows. The predecessor's worker is cordoned on takeover (`cordon/<worker>` is
  a durable row and survives, `durable.rs:23-25`), so it gets no new grants. Unknown: the
  container driver's delegated `actions/` cgroup, `cpu.max` and `--actions-memory-max-gib`
  are per unit; whether two units together can exceed the box is not read.
- **Daemon cache.** With one cache (`one_cache.rs`), the daemon input cache is the hot
  store. The detached daemon stages no new inputs (cordoned), so it releases the cache at
  detach; the successor's daemon takes the cache dir lock (otherwise it runs uncached,
  `Disabled::Locked`).

### 3.3 Sequence

S is the successor, H the holder (old). Steps 0 to 7 name what is reused (R) or new (N).

0. **Knock** (R: `incoming::knock`). S sends `hello` with the REAPI, worker and API
   addresses and the state path; N: trailing tokens `kind=aio` plus the meta, cold and hot
   paths and the predecessor node id. H refuses if any differs from its own, as it does for
   a frontend (`outgoing.rs:180-210`). Both unit instances run the same command line, so
   this only trips on an edited unit. N: H also *prepares* on a valid hello, before the
   offer: it flushes the hot tier to cold and checks free space on the cold, meta and state
   directories for the final flush and for the successor's new term. On failure H answers
   `refused disk` and nothing has changed (this is how disk full is kept reversible,
   section 4). Nothing nonreversible has happened until `ready`.
1. **Offer and ready** (R). H sends the three fds and the generation; S adopts them and
   checks `local_addr`; S sends `ready`. S has opened no exclusive resource yet; N: S does
   its slow, lock-free work now (certificates, runtimes, flag checks) so the gap in step 5
   is only the locked opens.
2. **GOAWAY and quiesce** (R `drain.begin`, `Farm::quiet`; N). H sends GOAWAY on the REAPI
   listener, refuses new Executes `UNAVAILABLE`, then waits up to
   `--handover-quiesce-secs` for no Result being taken and no presubmit in flight. N: it
   also waits for zero in-flight storage writes. Nothing is cut yet, so a timeout here
   is still safe to honor; N: on timeout H answers `failed quiesce` and stays the server
   (it reopens readiness; its listener handle was a spare dup, S has not served). #879
   ignores the timeout and proceeds; for all-in-one it must not, because proceeding past
   an unfinished storage write loses it.
3. **Fence** (R `farm.fence`, sweeper abort). The scheduler takes no input: Results are
   refused, no grants.
4. **Cut the daemon streams and listeners** (R `worker_stop`, `end_worker_streams`,
   `api_stop`). The in-process daemon's stream ends with `UNAVAILABLE`; it starts its
   reconnect wait. Its contact with a server stops here, so this is where its 40 s fence
   clock starts (`kbf-daemon/src/config.rs:12`). The whole gap from here to step 6 must be
   well under that; it is not measured today, and the meta log (never compacted) is
   replayed in step 5, so the replay time is the number to measure first.
5. **Storage stand-down** (N, in this order, all after the daemon streams ended):
   `HandoverStorage` to `Parked`; wait for in-flight calls; `Cache::stand_down` (final
   flush); `RaftMetaLog::demote`; `FsStore::demote`; `HandoverStorage` to `Follower`;
   release the daemon cache. Then `store.demote()` (R). If any step fails, H can no
   longer simply continue; #879's behavior is `drain.close()`, which ends every stream.
   For all-in-one a failed demote after `Parked` instead returns `HandoverStorage` to
   `Local`, reopens what it released, and answers `failed`; this is possible for the log
   and cold store only before the `store.demote()` that frees the state store. Order:
   state-store demote last, because it is the only step the flock makes irreversible.
6. **Released** (R). H sends `released`. S now: `open_after_handover(generation)` (R; takes
   the flock at the next generation, `EpochPolicy::Keep`); N: `RaftMetaLog::open_dir` (new
   term, `AllocEpoch`), `FsStore::open`, tier open, `Cache::open`; `Farm::with_state`;
   cordon the predecessor's worker; start S's daemon with the reduced capacity; serve on
   the adopted sockets. Connections that arrived in the gap wait in the kernel backlog. S
   sends `READY=1` when its probes pass (R `ready.rs`: storage answers, state answers,
   map loaded), so `systemctl start` of the idle instance returns when S takes builds.
7. **Follow and drain** (R `Holder::run`, `Farm::follow`; N). H answers its open Execute
   and WaitExecution streams from the `done/<op>` rows S writes, every
   `--handover-follow-interval-ms`; an operation with no trace for three looks ends
   `UNAVAILABLE` and counts in `kbf_frontend_relay_lost_total` (`farm/handover.rs:19-30`).
   Meanwhile the detached daemon reconnects, S adopts its leases, its Results are accepted
   from the node that holds the lease (`worker.rs:261-277`), and its outputs go to S's
   storage through `FollowerStorage`. H exits when the three conditions in section 3.2
   hold.

The one window with no recovery is unchanged in kind from #879: between `released` and S
serving. If S dies there, H cannot take the stores back. Systemd restarts S as a cold start
(epoch bump; the daemons rerun their leases); H still answers its callers from the rows.
For all-in-one the window is longer than for a frontend (log replay) and a bump also makes
the *detached* daemon cancel its runs the moment it reaches S, so the builds survive and
the actions rerun. That is slower, not a failure, but it is the case the tests of section 5
must pin.

### 3.4 What changes in start-up code

- `frontend.rs`'s assembly becomes a `handover::Driver` that both modes call:
  `bind_or_adopt(trio)`, `Facts`, a list of `StandDownStage` objects (frontend: none beyond
  #879's; all-in-one: section 3.3 step 5), a list of `Ready` checks. Frontend behavior
  stays byte for byte; its tests stay unchanged and are the proof.
- `kbf all-in-one` gains `--handover-socket` (requires the sqlite `--state-store`, as for
  the frontend) and `--handover-quiesce-secs`, `--handover-follow-interval-ms`.
- `kbf-all-in-one/src/run.rs` replaces `start_interruptible` with
  `start_or_take_over` and keeps the daemon half alive on detach (`serve_until` gets a
  third reason to end: idle after detach).
- Configuration stays flags, no environment variable beyond `NOTIFY_SOCKET`.

## 4. Failures

Columns: what Buck2 sees, what the daemons and the operator see, recovery.

| point | Buck2 | daemon / operator | recovery |
|---|---|---|---|
| S dies or is refused before `ready` | nothing | operator: `systemctl start` fails (`Refused` or timeout, `TimeoutStartSec=120`) | H never changed (`outgoing.rs:153-155`); fix and retry the handover |
| prepare fails (disk full, flush error) | nothing | S exits non-zero with `refused disk` | H unchanged; free space; retry |
| quiesce times out | nothing | S exits; H reopens readiness | H stays server; retry |
| storage stand-down fails before the state-store demote | nothing (GOAWAY was sent; new connections reach S's backlog only, so they wait) | H answers `failed`, reopens its listeners and stores | H serves again; S exits; retry. Must be tested (section 5). |
| `store.demote()` fails | open streams end `UNAVAILABLE` (#879's `drain.close`) | operator alert | the one all-in-one case that fails builds; kept last so the earlier steps are the likely ones to fail |
| S dies after `released`, before serving | new requests wait in backlog until systemd restarts S | detached daemon cancels its runs on S's bumped epoch, reruns after grace | H still answers callers from rows; cold start of S; leftover actions rerun |
| S dies after serving, H still draining | new builds see the connection drop (the same as any server crash) | detached daemon fences after 40 s of no ack | systemd restarts S as a cold start; H's leftovers rerun; H's open callers are answered from rows S wrote |
| H dies mid-follow | its open streams break (Buck2 does not recover; unavoidable: the process holds the stream) | its daemon and keepers die, actions are killed (B1); S requeues after grace | a restart of H would start it as a cold start against S's state and fail on the flock; a marker written at detach stops the restart (open question 7); the next handover is explicit |
| detached daemon cannot reach S | none (callers still answered from rows if H alive) | daemon fences its leases after 40 s, reruns elsewhere | S's grace gives up the leases; a daemon that never registers inside `LEASE_GRACE` has its leases given up |
| disk full in the middle | a write fails as it would with no update | daemon uploads fail and retry | free space; handover still reversible before step 5's last stage |
| a Result in flight across the handover | none if the epoch is kept: Result is unacknowledged, resent after Welcome, before the first heartbeat | S accepts it from the lease holder | untested today (no test holds a Result across a handover); test T5 |
| two updates back to back | n/a | `kbf all-in-one handover` refuses when both instances are active (`@a` still draining) | wait for H to exit; this bounds the update rate by the longest action |

Reading of the table: the handover can fail builds only through the three rows already
true for #879 (an operation with no trace for three looks; a stream open on a connection
that hits a second-signal or shutdown escalation; a crash of the process that holds the
stream) plus the `store.demote()` failure.

## 5. systemd shape

`kbf-all-in-one@.service`, a template with two instances, mirroring `kbf-frontend@.service`
(#887):

- `Type=notify`, `NotifyAccess=main`, `TimeoutStartSec=120`; `READY=1` as in section 3.3.
- `RuntimeDirectory=kbf-all-in-one`, `RuntimeDirectoryMode=0700`,
  `RuntimeDirectoryPreserve=yes`; `--handover-socket=/run/kbf-all-in-one/control.sock`; both
  instances run as the same user.
- `KillSignal=SIGTERM`, `TimeoutStopSec=infinity`, `Restart=on-failure`,
  `RestartPreventExitStatus=2`, `DefaultInstance=a`, no `Conflicts=` between instances. See
  the failure table for `Restart` after detach: the process exits 0 after a handover, which
  `Restart=on-failure` does not restart, and a crash of a detached process must not
  restart it into a takeover; the plan is that a detached process sets a marker file the
  unit's `ExecCondition` reads. Open question 7.
- Same `Slice=kbf.slice`, `Delegate=yes` and `OOMScoreAdjust` as the single unit
  `[arch-v2] kbf-all-in-one.service`. `MemoryMin` sums over both instances while both
  are alive; the install plan must account for that.
- `--node-id=<host>-%i`, `--data-dir=<dir>` with per-instance daemon subdirectory, certificates
  per instance from the install step.
- The binary is swapped in by rename; the keeper starts from `/proc/self/exe`, so a swapped
  binary still spawns keepers for the old daemon (`kbf-daemon/src/keeper.rs:27-34`).

Command: generalize `kbf frontend handover` (`kbf-cli/src/handover.rs`) to `kbf handover
--unit kbf-frontend|kbf-all-in-one`, with the same rules (exactly one `@` instance active:
start the other; none: start `@a`; both active or any transitioning: refuse, exit 1; only
`show` and `start`, never `stop` or `restart`; `--wait` polls the old instance until
inactive). `install-units --mode all-in-one` writes the template, enables `@a` only, starts
`@a` if neither is up and never restarts a serving instance; check-units gets the same
`handover` rule, and the existing single unit becomes the legacy shape check-units warns
about. Existing hosts migrate by one cold restart into the template.

The pull updater (upd-5) drives the same command: stage the new binary beside the old,
rename it in, run `kbf handover --unit kbf-all-in-one --wait`. A refusal (both instances
active) is not an error: it means the previous old instance is still draining, so the
updater waits and retries, and reports "draining: N leases, oldest M minutes" from the
operator view. The updater never stops either instance and never forces: a forced
stop is the second SIGTERM, which the operator sends by hand and which abandons leases.

## 6. Tests

Each test names the defect it catches and the mutant that turns it red. Unit tests sit in
the library they cover; `kbf-it` tests use real processes.

| id | test | catches | mutant |
|---|---|---|---|
| T1 | `RaftMetaLog::demote` then a second `open_dir` succeeds while the first answers digest and action-cache queries | the lock not released, or state dropped | skip `file.unlock` in `demote`; or clear applied state |
| T2 | the demoted log refuses `commit` with `Unavailable`; the new log's term is greater | two writers | leave the committer running |
| T3 | `FsStore::demote` then second `open` succeeds; reads work; `tmp/` of the first untouched | B3 | `demote` keeps `LOCK` |
| T4 | `Cache::stand_down` leaves no staged block outside the cold store; the successor's orphan pass deletes no block the log names | B4, data loss on orphan pass | skip the final flush |
| T5 | `HandoverStorage`: calls started before the swap finish on the old backend, parked writes land on the successor, a miss on the frozen state asks the successor | lost write, stale read | swap without draining in-flight; return NOT_FOUND on miss |
| T6 | unit: stand-down step order with a fake stage that fails at each index; state-store demote last; before it, everything reopens | irreversible step too early | move `store.demote` before the log demote |
| T7 | unit: the successor with a kept epoch and a recovered lease books it on the predecessor's node id, and its own daemon registers with `total - booked` | double booking (B8), epoch bump (B9) | register with full capacity; `Bump` instead of `Keep` |
| T8 | unit: the per-instance daemon data dir means the successor's sweep removes nothing the predecessor owns | B7 | share the data dir |
| T9 | frontend tests unchanged after the `Driver` refactor | regression in `kbf frontend` | none (the existing suite is the check; run before and after) |
| T10 | `kbf-it` real processes: old all-in-one runs a long action; start the successor; the old process finishes the action; its output is readable through the successor; the Buck2-style Execute started before the handover completes with the right result | the whole contract | kill the old process at `ready`; the test must go red on the stream |
| T11 | `kbf-it`: a Result held in flight across the handover (the action finishes during the gap) is accepted once | the untested row in section 4 | drop unacknowledged Results on reconnect |
| T12 | `kbf-it`: the successor dies after `released`; a cold start serves, the callers of the old process get answers | the unrecoverable window | the old process exits on `released` |
| T13 | `kbf-it`: fault injection that fills the cold directory before `prepare`: handover refused, old keeps serving, no stream broken | disk full reversibility | skip the prepare check |
| T14 | `kbf-it`: a refused handover (different `--listen`) leaves the holder serving | #879's refusal path for the new tokens | accept without comparing the new tokens |
| T15 | `kbf-it`: the detached daemon cannot reach the successor for longer than 40 s: leases fence and rerun, no caller sees an error from the old process | the fence interaction | raise the daemon's fence beyond the gap |

A mutant is planted and the test is seen red on the farm before the PR goes up; the PR
description records which mutant. No test pins a wrong answer.

## 7. PR split

Stacked on #879 and #887 only where they touch `frontend.rs`, `handover/*` or `kbf-deploy`.
PRs 1 to 5 do not, so they can start from arch-v2 now. Sizes are changed lines, an estimate.

| PR | content | size | base |
|---|---|---|---|
| 1 | `FsStore::demote` + T3 | 150 | arch-v2 |
| 2 | spike: `RaftMetaLog::demote` and T1, T2; answers whether state stays readable; if not, the doc is amended with the proxy fallback before PR 3 | 300 | arch-v2 |
| 3 | `Cache::stand_down` (final flush, close tier) + T4 | 300 | PR 1 |
| 4 | `HandoverStorage`, `FollowerStorage`, the Segments redirect + T5 | 500 | PR 2, 3 |
| 5 | `kbf-node`: detached daemon (stay alive after server half, idle exit), per-instance data dir and container names, cache release + T8 | 450 | arch-v2 |
| 6 | `handover::Driver` out of `frontend.rs`, no behavior change + T9 | 500 | #879, #887 |
| 7 | all-in-one wiring: flags, `start_or_take_over`, step order, `Keep`, cordon, capacity + T6, T7 | 600 | 4, 5, 6 |
| 8 | `kbf-deploy`: `kbf-all-in-one@.service`, install and check rules, generalized `handover` command | 350 | 7, #887 |
| 9 | `kbf-it` T10 to T15 | 600 | 7, 8 |

Order: 1, 2 first (the spike decides the design), then 3 and 5 in parallel, 4, 6 after
#879/#887 land and are merged into arch-v2, 7, 8, 9. Each PR names what its tests prove and
the mutant it was seen red on.

## 8. Open questions

1. **Does `RaftMetaLog` keep its applied state readable after the Raft host stops?**
   Recommendation: PR 2 answers it before anything else; if no, take the proxy fallback
   (section 3.1), not a read-only open, which touches the Raft host's election.
2. **Replay time and the 40 s fence.** The log is never compacted; the gap includes its
   replay. Recommendation: measure on a real log before PR 7; if replay can exceed about
   10 s, add log snapshots (a separate design) or have the detached daemon raise its fence
   for a handover.
3. **Node id per instance.** `<host>-a`, `<host>-b` with two certificates, versus admit
   rules that accept a suffix. Recommendation: two certificates; it needs no change in
   `peers.admit`.
4. **Wire change.** Trailing tokens on `hello` versus `PROTOCOL=2`. Recommendation: trailing
   tokens, and check that a v1 holder tolerates unknown trailing tokens; if it does not,
   the first update to this version is a cold restart, once.
5. **Capacity split.** Resize-up by a periodic task in the successor, versus a shared box
   notion in the scheduler. Recommendation: the resize task; the scheduler change touches
   placement for every deployment.
6. **Cgroup and memory limits across two units.** Recommendation: one `actions/` parent
   shared by both instances so `cpu.max` and the memory cap apply to the sum; verify before
   PR 8.
7. **`Restart=` after detach.** Recommendation: a marker file written at detach and read by
   `ExecCondition`, so a crashed detached process is not restarted into a takeover.
8. **Cold store kind.** `file://` needs `FsStore::demote`; an object-store cold store has
   no lock. Recommendation: keep `file://` support (the all-in-one unit uses it), do not
   require an object store for this.
9. **Is the all-in-one worth this work?** Alternative C already updates a fleet without
   failing a build. Recommendation: build PR 1 to 5 anyway; they are small and independent,
   and the owner decides after PR 2 whether PR 6 to 9 are worth carrying.
10. **#743.** If the in-process control plane lands, the detached daemon's control plane is a
    new feature (dial the successor), not a redial. Recommendation: do not land #743
    before this design is built; it removes the mechanism section 3.2 reuses.
