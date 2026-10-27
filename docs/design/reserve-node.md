# Reserving a whole node for a quiet run

Status: design, revision 3, not built. Target path: `docs/design/reserve-node.md`.

Vocabulary: "exists" means origin/main has the code today (checked by grep for this revision). "new" means this note proposes it. Defaults are proposals and live in configuration.

## 1. Problem and goal

Performance work needs a machine on which nothing else of the farm's runs, whose CPU frequency policy is known, and whose remaining noise is measured. A shared farm gives none of that, and a cached result measures nothing.

Goal: an action asks for an entire node and gets it. For the duration no other lease is on the node; the node was drained of other work first (nothing is killed); the result states governor, frequency, boost, SMT and the CPU time that was not the action's, and says plainly which noise sources kbf controls, which it only measures and which it does not cover (section 8.3). The ask is capped so a stream of requests cannot starve the farm. The surface stays compatible with the whole-node lease class of the jobscheduler design (long-term home): same state names, cap names, defaults and escalation rule; the elastic-machine part of that design is out of scope here (section 12).

First release scope: per-action reservation, Linux. Per-build holders are cut from this design (section 14, appendix B).

Non-goals: setting governor or frequency (the daemon is unprivileged; the host image sets them and the report states them); preempting running work (not built, so a reservation waits); a general quota system; making a converged storage neighbour quiet (section 7.2).

## 2. What exists and what is new

| Piece | Exists | New |
|---|---|---|
| `kbf-lease=whole_machine` parsed by the front, `LeaseKind::WholeMachine`, booking the worker's whole capacity | yes | none |
| Driver for it | Mac only (`native-whole-machine`) | Linux container driver serves it on daemons that opt in |
| Placement: `Servable::fit` takes the least loaded worker, ties by name (not first-fit); `Servable::reserve` holds a worker for a queued whole-machine request, recomputed every round from the queue walk | yes, in memory, stateless, uncapped, held from round one | replaced for whole-machine requests by a stored, capped, deadline-bearing hold that begins after `drain_after` (4.2) |
| Undo of a committed grant when the daemon refuses Start (`unplace`), two per-request attempt counters (`max_reserve_attempts`, `max_replace`) | **no** (`git grep` finds neither) | new scheduler and durable-store work, its own PR (4.2) |
| Cordon / drain (operator, updates); cordons are `cordon/<worker>` rows applied at the intake poll | yes | untouched; the hold is a separate scheduler-owned state with its own verdict |
| `do_not_cache` on the request | yes, but copied from the client's decoded request (`kbf-front/src/execution.rs:817`) | front forces it from the property (3.3) |
| `Request::joinable()` (kbf-sched `input.rs`) false for a `do_not_cache` request | yes (front test `execution/platform.rs:62`) | relied on and tested: reserving requests never coalesce |
| Result carrier `ExecutedActionMetadata.auxiliary_metadata` with the `ResourceUsage` `Any` | yes | a second `Any` for the node report; a third for server-side queue/drain timing |
| Cgroup layout (`supervisor/`, `actions/`, `--reserve-cpus` as a CFS quota, cpu.stat throttle counters read) | yes | cpuset partition for the duration, when cpuset is delegated |
| Governor, cpufreq, boost, SMT, loadavg, per-CPU stat, interrupts, thermal counters | nothing reads them | read-only readers in the daemon |
| Node phases `Preparing` / `Restoring` | yes, built for the Mac GUI session switch: the daemon names a lease the scheduler already holds | Linux does not reuse them (4.2) |

Two inconsistencies to fix in a separate docs-only commit before the design PR:
- the `kbf-sched` cordon module header (`cordon.rs`) says a cordon is gone after a server restart; `farm/durable.rs` stores `cordon/<worker>` rows and the intake poll applies them. The code is right (confirmed by reading `durable.rs`); the header is stale.
- the comment on `CgroupUsage` in `worker.proto` says kbf sets no `cpu.max`; `--reserve-cpus` does.

## 3. Request

### 3.1 Property surface

One key, no second spelling: `kbf-lease=whole_machine` (exists). On Linux it now resolves to the new driver instead of "no worker serves it". `kbf-reserve=node` (the ask's spelling) is not added: it would be a second name for the same thing. `kbf-cpu`, `kbf-mac-admin`, `kbf-node` are unchanged; `kbf-node` (planned/refused) is the future way to pin a node or hardware class (4.5).

### 3.2 Refusal rules

Refused at the front, before queueing, with a `google.rpc.ErrorInfo` reason in the partial execution metadata (the carrier used for "no worker can run it"):

- INVALID_ARGUMENT: `kbf-book-cpus` / `kbf-book-mem-gib` with `whole_machine` (exists); a key in two spellings (exists); `whole_machine` with a GPU or VM lease kind.
- INVALID_ARGUMENT, `reserve-timeout-required`: the action carries no timeout. A hold must be able to end by itself.
- INVALID_ARGUMENT, `reserve-timeout-too-long`: timeout above `max_hold`.
- PERMISSION_DENIED, `reserve-not-allowed`: the caller is not allow-listed. The owner key is the REAPI instance name. Whether the front has an authenticated client identity is to be read in `kbf-front` before implementation; it is used only if it exists. Stated plainly: CI uses one instance, so the allow-list then means "everyone on that instance"; the cap, not the allow-list, is the protection. An empty allow-list means nobody.
- FAILED_PRECONDITION, `no-matching-pool`: no pool lists the driver, or the pool that would serve it does not offer the class (`max_whole_node` unset or 0, 4.3).
- FAILED_PRECONDITION, `no-capable-host`: a pool matches but no registered node of it could ever be given whole (none lists the driver, or every one is tainted or denied). This is the only refusal for lack of a host. A busy, cordoned or lost node is capable; a ceiling that is 0 at this moment is not a refusal: the request queues and `max_wait` bounds it (4.3).

`reserve-not-allowed` and the `reserve-timeout-*` reasons are kbf's tenant rules and stay; the two reasons above are the jobscheduler names and replace `reserve-no-capacity`.

Node side: only daemons started with `--whole-node` (config `[daemon] whole_node = true`) list the Linux whole-machine driver. A daemon may also list `whole_node_governors = ["performance"]` and `whole_node_boost = ["off"]`; outside either list the driver is not listed. A misconfigured node is unservable (it waits, then is refused after the unservable wait), not silently noisy. The existing test asserting the container runtime never serves `whole_machine` (`kbf-daemon/tests/local.rs:469`) changes to "only with the opt-in".

### 3.3 Cache and coalescing

Every platform property is in the action digest, so a reserving action never shares an entry with the plain action. That is not enough, and the first draft's claim that the property "sets" `do_not_cache` was wrong: the front copies the flag from the client's request. New rules:

1. The front forces `do_not_cache = true` whenever `kbf-lease=whole_machine` is present, overriding the client.
2. `joinable()` is false for `do_not_cache`, so two identical concurrent reserving Executes never share one run (and one measurement). Tested, not assumed (section 11).
3. The write side has two paths: the server's `cacheable` decision in `farm.rs` (from `waiters.first()`, harmless once no waiter can attach) and the daemon's `Cache::write_action_result`. The daemon path must be verified to honour the same flag; if it does not, that is a fix in the same PR with a mutant test on that path.
4. The front already rejects client AC writes. No skip-cache-lookup flag is needed; the standing rule against skipping the remote cache is untouched.

Limit: the client's own local reuse of an unchanged action is outside kbf. A repeated measurement must change the action (documented convention: a `RUN_ID` environment variable in the action) or be an action the client does not reuse. kbf adds no `kbf-run-id` property in the first cut.

## 4. Admission and queueing

### 4.1 Feasibility and node choice

A node is in at most one reservation. Feasible means: it lists the Linux whole-machine driver; it is serving (not cordoned, draining, drained, paused, held); it is in no reservation; it is not excluded for this request (below); and it is connected to the frontend that owns the operation.

Choice, in this order:
1. A quiet node (no lease) is taken at once, subject to the caps; it never waits for `drain_after`.
2. Otherwise, after `drain_after`, the node with the smallest worst-case drain: the latest lease end on it (for each lease, its start plus its action timeout, minus now; zero for none), then the fewest leases, then the lowest node id. Every lease ends by its timeout, so this is a bound, not an estimate.
3. Once reserved, the request does not switch to a merely cheaper node. A different node that has become quiet is taken at once and the hold moves to it (the owner updates its row).

Host exclusion: a given-up node is excluded for that request for `reserve_backoff` (default 5 min, constant). `op.avoid` exists and is tested but has no expiry; the timed exclusion is new. While every feasible node is excluded the request waits unreserved. This replaces back-off of the request itself.

### 4.2 Drain, hold, and the Start protocol

States, in jobscheduler's words:

```
queued -> draining -> exclusive_ready -> running -> restoring -> released
```

1. queued: nothing is held. If a feasible node has no lease, `has_room` grants at once.
2. After `drain_after` (30 s; a pool parameter) in the queue with no grant and no quiet node, the request claims a slot and a drain place (4.3) and takes a hold on one node by the choice in 4.1: `draining`.
   - **The per-round `reservations` derivation is replaced for whole-machine requests.** The slot row is the authority; the in-memory map is loaded from rows at intake; a round only decides whether to claim one. A queued whole-machine request without a row holds nothing, so younger work lands on feasible nodes (on Macs too, where today's hold from round one changes; see 4.6).
   - Placement on the held node skips operations queued after the request (equal or lower urgency and younger). More urgent work is still placed there (today's rule; QoS is `ci` for everything today, so that branch is tested with synthetic QoS). Running leases are never stopped.
3. When the node's last lease ends the grant is committed, then Start is sent (as today). **The daemon runs its prepare checks before it accepts Start** and refuses with a typed reason when they fail. It does not report `Preparing`: that phase names a lease the scheduler already holds and carries the session-switch grace. Prepare is: lease table empty and no lease cgroup; static snapshot; pre-run probe (4.7); cpuset partition (7.1) applied before accept; sampler started. Accept is the `exclusive_ready` event. **The lease clock (TTL and action timeout bookkeeping) and `max_hold` start at `exclusive_ready`**, not at the grant commit: the requester is not charged for drain or prepare. This is the token time of the jobscheduler design (there the token is minted at ready; kbf has no token and the accept is the same instant). The refusal is a new field on the Start response in `worker.proto`. An old daemon never sends it (and never lists the driver); an old server never reads it; skew is safe in both directions and tested.
   - `prepare_timeout` (new, default 2 min): a daemon that never answers Start (neither accept nor refusal) within it is treated as not ready, exactly like a refusal. The typed refusal stays the fast path: a refusal frees the node at once without waiting for the timeout.
4. On refusal or `prepare_timeout` the scheduler undoes the grant. **This is new work, not reuse**: `unplace` and the attempt counters do not exist. Design: a typed refusal carries "no process was started", so the scheduler may free the node without waiting `LEASE_GRACE` (60 s); a lost or silent Start still follows today's START_GRACE / lease-lost path with the fence, and `prepare_timeout` ends the wait for readiness, not the fence. The count is stored in the operation's row, counts against `max_replace` (prepare failures only; drain give-ups count against `max_reserve_attempts`, 4.3), and the node is flagged for the operator. A refusal is retriable (`exclusive-not-ready`) with `reserve_backoff` exclusion of that node; after `max_replace` (3) the request fails `exclusive-not-ready`.
5. Other leases on a reserved node: from the first moment of the hold nothing new is placed, and the answer to a renewal of any lease already on that node carries `host_reserved` (new, additive field in the renewal response; old clients ignore it). It asks the holder to place no more work there and to release early when its work is done. A holder that ignores it keeps the lease to its timeout; **a renewal is never refused for it** and nothing is killed. A node that is not reserved never sends it. Tested (11).
6. running, then `restoring`: leases removed, cpuset and quota restored (7.1), node back to serving. A node that fails restore stays out of service and is visible to the operator. Whether this reuses the existing post-lease phase is decided by reading the phase code at implementation; it is not a reason to add a protocol field.

The hold is a scheduler state, not a cordon. A cordon is skipped by every request including the reserving one, and belongs to the update machinery.

### 4.3 Caps

Names and defaults are those of the whole-node class of the jobscheduler design; all are pool configuration, farm-wide, enforced with store rows (section 6). A node is *whole* from the hold until restore completes: `draining`, `exclusive_ready`, `running`, `restoring`. A request waiting unreserved counts toward nothing.

| Key | Meaning | Default |
|---|---|---|
| `max_whole_node` | most nodes of the pool whole at once | none: the pool does not offer the class (`no-matching-pool`); the perf pool's configuration sets 1 |
| `max_draining` | most nodes `draining` (held, not yet granted) at once | 1 |
| `max_whole_node_per_consumer` | most nodes one consumer has whole at once | 1. The consumer is the REAPI instance name until the front has a client identity (3.2). CI uses one instance, so this then means one reservation at a time for all of CI; stated because one long perf job blocks other reservers of that instance. |
| `keep_for_ordinary` | fewest capable nodes of the pool that are neither held nor reserved, so ordinary work always has them | 1 |
| `allow_last_capable` | whether a hold may start on the only unreserved capable node of an ordinary request queued ahead of it | false |

- **Ceiling** = `min(max_whole_node, capable_nodes - keep_for_ordinary)`, `capable_nodes` the serving nodes of the pool that list the driver (tainted, denied and cordoned-for-good excluded; lost ones counted), **read from store-visible node rows** (the node look the updater reads), not from one frontend's connections. A hold starts only while whole nodes are below the ceiling, `draining` nodes below `max_draining`, and the consumer below its limit; otherwise the request waits in queue order, unheld. A table is a test (11): (capable, max_whole_node, keep_for_ordinary) -> ceiling, including 0, 1, 2, 3 capable nodes. With the default floor of 1 a one-node pool cannot reserve (ceiling 0): the request queues and ends in `reserve-wait-exceeded`; an operator who wants a one-node pool sets `keep_for_ordinary = 0` knowingly.
- A fall of the ceiling (a node lost or tainted) never revokes a held reservation; holds beyond the ceiling in `draining` are dropped newest first at once, none starts until the count is below it. A transient dip of N after admission never refuses a queued request.
- **Last capable node.** Unless `allow_last_capable`, a hold does not start on node H when H is the only unreserved capable node (by the ordinary request's platform properties) of an ordinary request queued ahead of the reserving request. Only older work is protected, so a stream of newer requests cannot hold a reserver off.
- **Drain deadline and attempts.** `drain_deadline` (20 min; a kbf proposal; the jobscheduler default is the pool's max wall time): a hold with no grant at the deadline is *given up*: node serving, nothing on it touched, the node excluded for this request for `reserve_backoff` (4.1), the request re-picks another node by 4.1 or waits unreserved. Give-ups are counted per request (`max_reserve_attempts`, default 2, stored in the operation's row). **Escalation (the jobscheduler rule): after `max_reserve_attempts` give-ups the deadline of the next hold is the pool's max wall time, whatever `drain_deadline` says, and that hold is never given up for a non-quiet node**: nothing new is placed, so every lease that was on the node has ended by then (plus `LEASE_GRACE` for a lost one). The attempt counter alone makes the bound hold for every setting. The max wall time here is the longest lease end the front admits for the pool; the bound relies on every ordinary lease having one, which is read in the code before implementation (PR 2) and is a prerequisite if some lease has none. Nothing is killed to finish a drain.
- **Counters are separate:** `max_reserve_attempts` counts drain give-ups; `max_replace` (default 3) counts prepare failures (typed refusal or `prepare_timeout`, 4.2). There is no `exclusive-not-ready` for a drain that is slow: a drain either ends or is waited out.
- **Bound.** A request holding a hold is granted within `max_reserve_attempts * drain_deadline` + the max wall time, then `prepare_timeout` for each of at most `max_replace` tries. `max_wait` (4 h from submission; kbf's tenant rule, not in the jobscheduler design) remains the **outer bound** and wins where smaller: RESOURCE_EXHAUSTED, `reserve-wait-exceeded`. Explicit arithmetic: with `max_whole_node` 1 and `max_hold` 2 h, a second reserver can legitimately wait out `max_wait` behind two healthy holds. Intended: refuse and let the client retry, not queue unboundedly.
- `max_hold` (2 h from `exclusive_ready`): the backstop. The action timeout is required to be below it, so the lease ends first.
- Slots are claimed by conditional create (6). Lowering a cap never revokes a hold in progress.

### 4.4 Fairness and starvation, both ways

Farm starved by reservers: bounded by the ceiling, `max_draining`, the per-consumer limit, the keep-for-ordinary floor, deadline and host exclusion. Work that only the held node can serve must wait, not be refused. Today a cordoned worker gives `Verdict::Cordoned` (a waiting verdict in `servable.rs`, text "every live worker that can run it is cordoned"). A hold needs its own waiting verdict class, `Held`, with its own reason text, added to the verdict enumerations in the fleet property tests (`tests/f3_fleet/check.rs`, `tests/f4/reference.rs`, `f4/check/round.rs`), so the 300 s unservable wait never turns a hold into a refusal. Tested at 299 s and 301 s of injected time.

Reserver starved by normal work: `fit` takes the least loaded worker (ties by name), so an idle node is picked first, which suits a reserver; the hold then stops younger work landing and the node empties as its leases end, bounded by the longest lease on it. A pool where every node always carries a long lease is bounded, not solved: after `max_reserve_attempts` the hold waits out the max wall time and cannot be given up (4.3), and `max_wait` bounds the request from outside. The real fix for the wait is preemption of `batch` work (planned, not built).

Among reservers: first in, first out within QoS (ties by submission sequence, tested).

### 4.5 Comparability across runs

The scheduler picks the node, and nodes may differ (stepping, microcode, kernel, memory layout). Two QUIET results from two nodes are not comparable by that fact alone. So the report carries a `hardware_fingerprint` (8.1); `kbf build show` warns when node id or fingerprint differs between runs it shows; the docs say cross-node comparison is unsafe. Pinning a node or class is `kbf-node` (planned/refused today): a follow-up with its own design.

### 4.6 Mac pool

Capping Mac holds (one per platform pool; on the 2-Mac pool the default floor of 1 means a ceiling of 1) changes today's uncapped behaviour on the 2-Mac pool. It is a separate PR behind a separate flag (`reserve_cap_mac`), default off first, so the Linux work changes no Mac behaviour. (The replaced per-round derivation applies to Linux-driver requests only until that PR lands.)

### 4.7 Pre-run probe

Noise first appears after the benchmark has run and been wasted, so prepare includes a bounded probe: after a sync and a settle (default 3 s), sample per-CPU stat, PSI and writeback on the lease CPUs. If noise exceeds the limit the daemon refuses (`exclusive-not-ready`, retriable). Pre-run and in-run figures are reported separately. Input staging (CAS fetch onto the node) finishes before `exclusive_ready`; at sample start the daemon records Dirty/Writeback and a page-cache indicator (`Cached`, file-backed bytes of the lease cgroup from `memory.stat`). Cold or warm cache is the action's responsibility; an unprivileged daemon cannot drop caches.

## 5. Lifecycle

Request -> queued -> (slot + hold after `drain_after`) -> grant -> prepare -> accept -> run -> result -> release.

Release also on: action timeout; client cancel or disconnect (queued operation removed or lease ended; hold and slot dropped when no queued operation needs it; tested in draining, between grant commit and Start, and running: slot freed, node serving, no attempt counted); `max_hold`.

Daemon loses its frontend, or the frontend dies: a `draining` hold is a row and is rebuilt. A granted lease follows existing fencing: hermetic work runs to its timeout; networked work self-fences after `SELF_FENCE` (40 s); the scheduler waits `LEASE_GRACE` (60 s) before reuse. The slot row is deleted by whichever frontend sees the lease end.

Frontend restart: the hold is a row, reloaded at the next intake poll by either frontend.

Node lost mid-reservation: the operation fails `reservation-lost` and carries no report; it is never re-run elsewhere (a number from another node looks like the measurement and is not). Daemon restart restores cpuset and quota from configuration at startup: start is restore.

## 6. Store rows and two frontends

Facts relied on: the store decides which frontend runs an operation (conditional writes); each frontend places only onto daemons connected to it; operator state is shared by rows read at the intake poll; deadlines are in store-clock milliseconds.

Row families, all conditional create-if-absent:
- `reserve/slot-<i>`, `i` in `0..ceiling`: `{ operation_id, owner, node, state (draining|held), claimed_ms, deadline_ms, frontend_term, reserve_attempts, replace_attempts }`. Free slots and `max_whole_node`/ceiling are the same fact.
- `reserve/drain-<j>`, `j` in `0..max_draining`: taken with the slot while `draining`, deleted at grant commit (the cap on idle capacity).
- `reserve/owner-<owner>-<k>`, `k` in `0..max_whole_node_per_consumer`: the per-consumer limit.
A claim creates owner, then slot, then drain row; losing any create releases the earlier ones and the request stays queued. These are kbf's mechanism for what the jobscheduler design checks under one table lock, because kbf has two frontends and no shared lock.

- Claim: the creates above, **only after a node is chosen** (a frontend with no feasible node never claims a slot; release if the node vanishes). Free slots and the cap are the same fact. A frontend that loses the create stays queued.
- Ownership: only the frontend that owns the operation creates and updates its row, on a node it serves. A request whose owner serves no opted-in node is "unservable" there, not "unplaced", and ownership today moves only on "cannot place". New: for whole-machine requests unservable on the owner, the owner releases the operation so the store can give it to a frontend that serves a reservable node. Tested with a two-frontend case where the only opted-in node is on the non-owner.
- Renewal: the owner refreshes `deadline_ms` while the hold is live. At expiry plus one `LEASE_GRACE` any frontend deletes with a version-conditional delete, so a dead frontend's draining hold frees its slot unaided. Renew-vs-expire: exactly one wins, both orders tested.
- A stale-expired row whose node still runs a reservation's leases frees the slot, not the node's placement; leases run to their end.
- The ceiling is computed by each frontend from store-visible node rows and configuration. On a shrink, rows at index >= ceiling keep occupying until they end (held ones are never revoked; `draining` ones beyond it are deleted by their owner); on a grow, new indices are claimable at once.
- The node look gains a `reserve` field (state, owner, since, deadline).
- Invariant tested at every store-write kill point (slot create, grant commit, Start sent, slot delete): after recovery a slot row exists iff a hold or reservation lease exists, and a drain row exists iff its slot is `draining`.

Layering: `kbf-sched` gets pure hold/slot state with explicit events (`claim result`, `slot expired`, `start refused`); `farm.rs` executes the store I/O. Section 11 marks each test's layer.

## 7. What the action sees

### 7.1 Linux whole_machine

The lease books the worker's whole capacity (CPU and GPUs; memory checked, not booked, as today); its memory limit is the node's reported memory (already "whole host"). Today `--reserve-cpus` is a CFS quota on `actions/`; a quota on a whole-node action causes bandwidth throttling, the opposite of quiet. So, when `+cpuset` is delegated (the numa placement preflight exists):

1. The reserve is whole physical cores: all SMT siblings of `ceil(reserve_cpus / threads_per_core)` cores, the highest-numbered. `actions/` is never emptied: if reserve covers all cores, at least one core stays for the action or prepare refuses. A pure function, tested over fixture topologies (1/2/4 threads per core, offline CPUs).
2. Write order, all before Start is accepted: (a) `supervisor/cpuset.cpus` = reserve cores; (b) `actions/cpuset.cpus` = the rest; (c) `actions/cpu.max` = `max`. Restore is the reverse and runs on every exit path (lease end, refusal, panic hook where possible) and at daemon start from configuration. A partial restore failure keeps the node out of service and flags it.
3. Dropping the quota removes the guard against a runaway action starving the daemon; the cpuset is the guard now, which is why (a) precedes (c). The sampler runs on the reserve cores.

Without cpuset delegation the quota stays, the report says `cpuset_applied = false`, and the verdict is never `QUIET`. A delegated cpuset's effect needs one real-node test (read back `cpuset.cpus.effective` for supervisor and lease; a child of `actions/` runs only on the action set); a fake tree proves only the strings written.

### 7.2 The converged neighbour

All six Linux nodes run storage beside the daemon. That process keeps running; the lease does not own it, and it serves CAS reads for other builds during the run (disk, page cache, network, softirq: more than its `cpu.stat` shows).
- Bounded (host image): the storage unit is confined to the reserve cores (`AllowedCPUs=`-style); documented in the host docs; the daemon cannot do it (not in its delegated subtree). First cut: only nodes whose confinement is verified are opted in; cost stated: perhaps 1 of 6 nodes.
- Stated: `--noise-watch=<cgroup>` (repeatable): CPU time delta, `io.stat` delta, `cpuset.cpus.effective`, overlap with lease CPUs. `supervisor/` is always watched.
- Verdict consequence: where `noise_watch` is configured, QUIET requires watched cgroups not overlapping lease CPUs and watched I/O below a limit. A node with no watch configured and a storage neighbour present is capped at `UNKNOWN`, reason `neighbour-unconfined`.

## 8. The node report

### 8.1 Content

A new message `NodeState` in `worker.proto`, as its own `Any` (`type.googleapis.com/kbf.worker.v1.NodeState`) in `auxiliary_metadata` beside `ResourceUsage`; readers find entries by type URL and skip others. Every field is optional: absent means not read, absent is not zero. The report lists `missing_inputs`.

- Static, at prepare: schema version; CPU model; `hardware_fingerprint` (model, cores, memory, kernel, image id); kernel release and cmdline (isolcpus, nohz_full, mitigations); microcode; cpufreq driver and pstate/epp; governor (plus `governor_uniform`); min/max scaling frequency; boost (`no_turbo` or `boost`, with `boost_source`); SMT active; cpuidle governor and max C-state; logical CPUs online; NUMA node count, per-node memory, `numa_balancing`; THP enabled/defrag; swap used; hypervisor flag and instance type; vulnerabilities summary; machine id; CPUs and cores given to the lease; `cpuset_applied`; reserve cores; `partition_root`.
- Sampled: 1 s sampler, read-only, pinned to the reserve cores, driven by an explicit `tick(now)` over a `Sysfs` root path so tests step it (no sleeps); missed ticks counted. Per lease CPU: effective frequency (APERF/MPERF where readable, else `scaling_cur_freq` marked coarse), min/mean/max, spread (max-min)/mean; per-CPU `/proc/stat` deltas (user, system, irq, softirq, steal, iowait) **on the lease cpuset only**; IRQ counts landed on lease CPUs from `/proc/interrupts`; loadavg at start and end; PSI CPU some-total delta; thermal and throttle counter deltas where exposed; `lease_cpu_micros` from the lease cgroup; sibling-busy fraction; watched-cgroup deltas; Dirty/Writeback and page-cache indicator. Summarised, never listed per CPU beyond a bounded top-N (size tested with 256 CPUs).
- Noise metric: per lease CPU, non-lease busy time (user, system, irq, softirq, steal, iowait not attributable to the lease cgroup) over run wall time (not over `lease_cpu_micros`). Report the worst CPU and the total, in integer microseconds. Guards: a counter that goes backwards makes its field absent; lease cpu above busy by more than the tick slack (USER_HZ, 10 ms) is flagged `anomaly` and UNKNOWN; zero wall time is UNKNOWN. Never a panic or QUIET.
- Verdict: `QUIET`, `NOISY`, `UNKNOWN` (`SHARED` reserved, unused). QUIET only if all hold: every **required** input was read (required: governor, boost, `cpuset_applied`, per-CPU stat, lease cgroup stat, PSI; required if the host exposes it: thermal, frequency); governor and boost in the daemon's accepted lists (or the lists are empty and the report only states them); `cpuset_applied`; throttle and thermal deltas zero; worst-CPU noise below `--quiet-max-noise-centi` (integer centi-percent, strict `<`, default 100 = 1.00%, a placeholder tuned from the planted-noise run) and below an absolute microsecond floor; frequency spread below its limit; pre-run probe clean; the neighbour rule of 7.2; not virtualized (a hypervisor caps the verdict at UNKNOWN: steal hides host neighbours); missed sampler ticks below a limit. The numbers are always present when read.

### 8.2 Where it goes

The `ActionResult` from Execute (never cached, so never replayed); a `Reservation` `Any` stamped by the server beside node id and times (reservation id, node id, queued, drain and hold durations, attempts); the invocation recorder keeps both, since whether the build tool shows unknown auxiliary metadata is unestablished; `kbf build show` prints them and warns on node or fingerprint differences. A failed reservation carries the `ErrorInfo` reason, never a fabricated report. An old daemon's result with no `NodeState` is accepted, the reservation is released, the verdict UNKNOWN. Both `Any`s survive durable round trip and handover (tested).

### 8.3 Controlled, measured, not covered

| Noise source | Status |
|---|---|
| Daemon and its hashing pool CPU | controlled by kbf (reserve cores, cpuset) |
| Other leases | controlled by kbf (no lease on node, hold) |
| Storage neighbour CPU | controlled by host image; verified and measured; unconfined = `neighbour-unconfined` |
| Governor, boost, C-state policy | controlled by host image; checked against accepted lists; stated |
| Hardware IRQ affinity (irqbalance onto lease cores) | checked at prepare, counts reported; host image controls |
| Kernel threads, unbound workqueues, other slices | measured only; `cpuset.cpus.effective` and overlap reported; a root cpuset partition used where delegation allows (`partition_root`) |
| Frequency stability, thermal | measured only |
| Memory bandwidth, LLC, disk, network sharing with storage | **not covered, not measured** (io.stat and net bytes of watched cgroups are reported as proxies) |
| Page cache warm/cold, NUMA, THP | stated; the action's responsibility |
| Hypervisor neighbours | not covered; verdict UNKNOWN |

### 8.4 macOS

The Mac whole-machine lease exists and is unchanged. No cpuset, cgroup, frequency control or PSI. The Mac fills what it can read (brand, load average, lease user's CPU time, other users' CPU time); verdict UNKNOWN; P/E cores noted, no governor claimed.

## 9. Metrics and operator visibility

Existing registry (names indicative): `kbf_reserve_requests_total{outcome}` (granted, refused by reason, given_up, escalated, not_ready, wait_exceeded, lost); `kbf_reserve_wait_seconds`, `_drain_seconds`, `_hold_seconds`; `kbf_reserve_slots{state}` and `_limit`; `kbf_reserve_verdict_total{verdict}`. Operator: the node listing shows `reserve`; a log line at every transition (reservation id, node); one action `release-hold` on a `draining` or idle-held node: drops hold and slot, never kills a lease.

## 10. Interaction with updates and the coordinator

- A node in a reservation is not picked for an update drain (renewals on it carry `host_reserved`, 4.2); the updater waits, bounded by `max_hold`, and sees it in the node look.
- A node cordoned or draining for an update is not eligible; a `draining` hold on a node that is then cordoned is dropped, the slot frees, the request re-picks, the attempt does not count.
- A cordon on a `running` reservation behaves as a drain: the lease runs on.
- Cordon `Paused` is not stored (only Cordoned and Draining). After a restart a paused drain resumes as a drain with a past deadline; reservation code does not assume paused is durable.
- **Coordinator slots: a hard prerequisite.** The coordinator's slot accounting must report zero free slots on a node in a reservation and grant none during it, or "no other leases" is false. The first PR reads it and adds a test that a reserved node reports zero free slots. If the coordinator cannot be taught, opted-in nodes are excluded from it. (Not read for this note.)

## 11. Testing plan

Injected clocks and fakes; no test sleeps (the sampler takes `tick(now)`). Every row's mutant is planted at the named layer on the farm and recorded red in the PR. Layers: F front, S kbf-sched (pure), R farm/store (conditional-write fake, scripted clock), D daemon (fake sysfs/cgroup tree), N real node.

| Layer | Test | Mutant |
|---|---|---|
| F | Parse: value, conflict with `kbf-book-*`, two spellings, GPU/VM kind, no timeout, timeout over `max_hold` | allow `kbf-book-cpus`; drop timeout checks |
| F | Table-driven: every refusal in 3.2 has status and exact ErrorInfo reason in partial metadata; empty allow-list refuses; per-owner is per instance | status-only assertion; empty list allows |
| S | Allow-list verdict ignored below the front cannot grant | check only at front |
| F | Client sends `do_not_cache=false` with the property: still forced true | force removed |
| F+S | Two concurrent identical reserving Executes run twice, results distinct; a plain waiter cannot join a reserving operation | coalescing allowed |
| R | Daemon-side `write_action_result` honours the flag; no AC entry after a reserving run; second identical Execute runs again | gate skipped on the daemon path |
| S | Hold blocks younger placement, not more urgent (synthetic QoS); hold owner placed first; same-QoS by sequence | hold ignored in `fit`; urgent blocked |
| S | A request with no slot never blocks placement (Linux, and Mac) | hold without slot |
| S | `Held` verdict waits, never refused, at 299 s and 301 s injected | held treated as absent |
| S | (capable, max_whole_node, keep_for_ordinary) -> ceiling incl. 0, 1, 2, 3 capable nodes, 1-node pool queues (not refused), mid-hold shrink; transient dip does not refuse a queued request | per-frontend N; off by one; floor ignored |
| S | `max_draining`: a second hold does not start while one is `draining`; `allow_last_capable` false: no hold on the only unreserved capable node of an older ordinary request, true: hold starts; a newer ordinary request does not block | cap ignored; protects newer work |
| S | Host choice: quiet node first with no `drain_after`; else smallest latest-lease-end, then fewest leases, then lowest id (three fixtures, each decided by one key); reserved request does not switch to a cheaper busy node, does move to a node that became quiet | key order swapped; switches on cost |
| R | Slot race: two frontends, one slot (and one drain place, one consumer place); renew-vs-expire both orders; expiry at grace boundary (-1, 0); shrink with live rows beyond index; no claim before node chosen | read-then-write; unconditional delete; claim before pick |
| R | Same consumer twice: one hold with the default limit, two with limit 2; losing the drain-row create releases the slot | owner key ignored; slot leaked on a lost create |
| S | `drain_deadline`: node serving, given-up node excluded for `reserve_backoff` (re-eligible after it, on the injected clock), `reserve_attempts` counted, next hold on another node or waits unreserved | deadline never fires; exclusion without expiry; counter reset |
| S | Escalation: after `max_reserve_attempts` give-ups the next hold's deadline is the max wall time even when `drain_deadline` is shorter, and it is not given up for a non-quiet node (grant when the last lease ends); never `exclusive-not-ready` for a slow drain; `max_wait` smaller than the bound refuses `reserve-wait-exceeded` | counter not stored; escalated hold given up; slow drain refused |
| S | Start refused with "no process started": grant undone at once, no 60 s fence, `replace_attempts` counted (not `reserve_attempts`); after `max_replace` -> `exclusive-not-ready`; Start lost: START_GRACE path unchanged | undo skipped; undo without ack; counters swapped |
| S | `prepare_timeout`: a daemon that answers neither accept nor refusal is treated as refused (counted, node excluded); a refusal before the timeout frees the node at once; the lease clock and `max_hold` start at accept, not at commit (an accept after a long prepare leaves the full TTL) | clock starts at commit; timeout missing |
| S+D | `host_reserved`: renewal of another lease on a reserved node carries it, on an unreserved node does not; the renewal is still granted; no lease is stopped; an old client ignores the field (skew both ways) | renewal refused; field sent always |
| S | Cancel in draining, between commit and Start, running: slot and hold freed, no attempt counted | slot leaked |
| S | Cordon while draining frees slot and drain place, no attempt counted | slot leaked |
| R | Frontend restart: rows reloaded, same node and place; owner dead: slot freed by the other via store clock; kill-point invariant at each store write | rows ignored; slot delete before lease end |
| R | Two frontends, only opted-in node on the non-owner: operation moves and reserves | owner keeps it forever |
| D | `serves("whole_machine")` only with `--whole-node`; governor/boost outside lists: not listed | served unconditionally; list ignored |
| D | Prepare finds a stray lease cgroup or a noisy pre-probe: refuses | check skipped |
| D | Fake sysfs/procfs: governor, boost (both sources), SMT, frequency, stat deltas, interrupts, thermal; sampler tick count and min/mean/max exact for a scripted sequence | wrong file; absolute instead of delta |
| D | Unreadable inputs: field absent, verdict UNKNOWN, never zero; field-presence test generated from the .proto | defaulted to 0 |
| D | Verdict: exactly 1.00% in integer centi units; zero wall time; counter backwards; lease > busy; CPU offline mid-run; throttle; spread; virtualized; neighbour-unconfined | `<` vs `<=`; throttle ignored |
| D | Core selection over fixture topologies; `actions/` never empty | empty set allowed |
| D | cpuset partition: written in order before accept; restored at lease end and at startup from a half-applied tree; partial restore failure keeps node unserving; no cpuset: quota stays, `cpuset_applied=false`, not QUIET | restore skipped; flag always true |
| R | Result metadata: both `Any`s after durable round trip and handover; unknown URL skipped; old-daemon result accepted as UNKNOWN; size bound at 256 CPUs | type URL changed; reader errors |
| S | Node lost mid-run: `reservation-lost`, not re-run | retry elsewhere |
| D | Mac: report subset, UNKNOWN | claims QUIET |
| N | Committed farm test, not a manual run: (1) busy loop in a watched cgroup -> NOISY naming it; (2) loop outside every watched cgroup -> NOISY by worst-CPU noise; (3) clean, on a node with confined storage -> QUIET (an unconfined converged node's NOISY is acceptable and recorded); (4) long lease first, then a reservation: it waits and the lease is not killed; (5) real-cpuset read-back of 7.1 | each arm red against its mutant |

## 12. Migration to jobscheduler's whole-node class

Read against the whole-node design at its current head (decisions D46 to D52). The class there: a quiet host is taken at once; otherwise, after `drain_after`, one host is reserved, chosen by the smallest worst-case drain (latest lease end, then fewest leases, then lowest id), and nothing new is placed on it; caps are `max_whole_node`, `max_draining`, `max_whole_node_per_consumer`, `keep_for_ordinary` and `allow_last_capable`; a drain past `drain_deadline` is given up, the host excluded for the request for `reserve_backoff`, and after `max_reserve_attempts` give-ups the next reservation's deadline is the pool's max wall time and cannot be given up; the agent reports `exclusive_ready` and the lease token is minted only then, the lease clock counting from the token; `prepare_timeout` and `max_replace` bound a host that is not ready, after which the request fails `exclusive-not-ready`; the renewal answer of a lease on a reserved host carries `host_reserved`; refusals for lack of a host are `no-matching-pool` and `no-capable-host`.

| Here | There |
|---|---|
| `kbf-lease=whole_machine` | lease class `whole_machine` plus selectors from the consumer's config |
| `drain_after`, `drain_deadline`, `reserve_backoff`, `max_reserve_attempts`, `prepare_timeout`, `max_replace` | same-named pool parameters, same meaning (`max_replace` counts prepare failures only) |
| `max_whole_node`, `max_draining`, `max_whole_node_per_consumer`, `keep_for_ordinary`, `allow_last_capable` | same names and defaults; kbf enforces them with store rows because it has two frontends and no shared table lock; they become table-lock checks on the move |
| `draining`, `exclusive_ready`, `restoring` | same words |
| Start after grant commit and daemon accept (typed refusal, `prepare_timeout`) | token minted after `exclusive_ready`; lease clock and `max_hold` count from the accept, the token time |
| `host_reserved` on renewal answers | same additive field |
| `no-capable-host`, `no-matching-pool` | same reasons and same meaning |
| `exclusive-not-ready`, queued, refused | the three outcomes stay distinct |
| `NodeState` `Any` | consumer-owned; the daemon is unchanged by the move |
| Allow-list, `max_wait`, `reserve-not-allowed`, `reserve-timeout-*`, `reserve-wait-exceeded` | stay in kbf: the jobscheduler design has no tenant caps or request wait bound |

Out of scope here: the elastic-machine part of that design (D50: a whole-node request acquires a fresh machine instead of draining a busy one). kbf has no elastic provider for this farm; a quiet node, then a drain, is the whole order. If one arrives it is a later design.

Differences kept on purpose: the per-consumer key is the instance name until a client identity exists (the jobscheduler default for an API without per-consumer identity is no limit); kbf's `drain_deadline` default is 20 min where the jobscheduler default is the pool's max wall time, and `drain_after` is 30 s where its default is zero; kbf's outer `max_wait` has no counterpart. Divergence to remove, not carry: kbf's drain is an operator cordon; the reservation hold is scheduler-driven and stored, as jobscheduler's is. Property, outcome and state names are kbf's and do not change on the move.

## 13. PR plan

1. Docs-only: fix the `cordon.rs` header and the `CgroupUsage` comment.
2. Prerequisite: coordinator zero-free-slots test (10); read the front's identity source (3.2).
3. kbf-sched: pure hold/slot state, ceiling and caps, host choice and timed exclusion, escalation, `Held` verdict, undo-grant with the two attempt counters, `prepare_timeout`, tests and mutants (S).
4. Slot, drain and consumer rows and two-frontend ownership (R).
5. Front: refusals, forced `do_not_cache`, no-coalescing and daemon-path cache tests.
6. Daemon: `--whole-node`, lists, prepare, typed Start refusal and `host_reserved` in `worker.proto`, cpuset partition (D).
7. Readers, sampler, `NodeState`, verdict, `Reservation` stamp, recorder, `kbf build show`.
8. Farm test arms (N); host docs on storage confinement.
9. Separate: Mac cap (4.6). Later, own designs: `kbf-node` pinning; per-build holder.

## 14. Open questions for the owner (with a recommendation)

1. Name: reuse `kbf-lease=whole_machine`; no `kbf-reserve=node`. Recommend: reuse.
2. Per-build holder: cut from this release. Recommend: separate design after per-action ships (needs a scheduler non-lease hold, frontend affinity by reservation id, an (instance, id) key, and jobscheduler's answer on consumer-decided multi-operation leases).
3. Cache stance: never store or coalesce reserving results; the client changes the action for repeats. Recommend: as written.
4. Converged neighbour: report plus operator-set confinement of the storage unit, only confined nodes reservable (perhaps 1 of 6). Recommend: report and document; a daemon-enforced partition is out of reach today.
5. Governor and boost: state, never set; node opt-in with accepted lists. Recommend: as written.
6. Defaults (`max_whole_node` 1, `max_draining` 1, floor 1, 20 min drain deadline, 2 reserve attempts, 3 replace attempts, 5 min reserve back-off, 2 min prepare timeout, 4 h wait, 2 h hold, 1% noise placeholder). Recommend: configuration, tune after a month.
7. Allow-list identity: instance name first (one instance is one owner), client identity when the front exposes one.
8. Mac cap: separate flagged PR, default off first. Recommend: yes.
9. VM kind refused with reservation until the VM driver has a whole-node story.
10. A pool where every node always carries a long lease is a bounded wait: after `max_reserve_attempts` the hold waits out the max wall time and cannot be given up, and kbf's `max_wait` (4 h) stays the outer bound (`reserve-wait-exceeded`). It no longer ends in `exclusive-not-ready`. Recommend: accept until preemption exists; revisit by the `escalated` metric. Prerequisite: every ordinary lease has a max wall time (4.3).
11. Node or hardware-class pinning (`kbf-node`) for benchmark series. Recommend: yes, soon; without it cross-run comparison is unsafe.

## Appendix A. Review points taken

All, except those in B: per-CPU lease-cpuset noise metric with wall-time denominator; controlled-vs-measured table; IRQ check; neighbour-unconfined rule and io.stat; frequency/boost/SMT/C-state/kernel/NUMA/THP/virtualization/fingerprint fields; node comparability warning; pre-run probe and staging; required-input set and sampler pinning; `unplace` and `max_replace` moved to "new"; per-round derivation replaced; store-visible denominator and ownership move; per-build cut; Start protocol decided; owner key and timeout requirement; forced `do_not_cache` and no-coalescing; `Held` verdict; coordinator as prerequisite; Mac cap separated; cordon header confirmed by code; cpuset write order and restore; clock/tick design; layer column and mutant rule; the added store, crash, cancel and boundary tests.

## Appendix B. Review points rejected or deferred

- "`fit` is first-fit by name" (a recalled earlier finding): not true of current `fit`, which takes the least loaded and breaks ties by name (read in `servable.rs`); 4.4 keeps the draft's reasoning.
- One-thread-per-core lease mode: deferred; sibling-busy fraction is reported instead.
- `SHARED` verdict and refusing `action`+`build` for benchmarks: moot, per-build is cut; the verdict value is reserved.
- A `kbf-run-id` property: rejected for the first cut; the documented `RUN_ID` environment convention changes the digest with no new surface.
- Thermal and frequency hard-required for QUIET: rejected; required only where the host exposes them, with `missing_inputs` listed, or most of the farm would be UNKNOWN.
- Requiring a cpuset root partition: not required; used where possible and reported, as unprivileged delegation may not permit it.
- Storage I/O and network deltas as a hard threshold everywhere: partly; the limit applies where `noise_watch` is configured; memory bandwidth and LLC are stated as not covered.

## Changes in revision 3

- Caps renamed to the whole-node class's: `max_whole_node`, `max_draining`, `max_whole_node_per_consumer`, `keep_for_ordinary`, `allow_last_capable`, with its defaults and ceiling formula; `max_nodes`, `max_fraction` and `floor(0.25*N)` removed. Store rows kept as the mechanism, now slot, drain and consumer rows.
- `max_replace` split: `max_reserve_attempts` counts drain give-ups, `max_replace` counts prepare failures. Escalation adopted (after the attempts the deadline is the pool's max wall time and is never given up); host exclusion with `reserve_backoff` replaces request back-off; the `exclusive-not-ready` for drain give-ups is gone and open question 10 is now a bounded wait with `max_wait` as the outer bound.
- Host choice: quiet node at once, else smallest latest lease end, fewest leases, lowest id; no switching once reserved.
- Admission refusals for lack of a host are `no-capable-host` and `no-matching-pool`; `reserve-no-capacity` removed.
- Added `host_reserved` on renewal answers and `prepare_timeout`; the typed refusal is the fast path.
- The accept is named `exclusive_ready`; the lease clock and `max_hold` start there. Section 12 rewritten against the current whole-node design, including the correct meaning of its `max_replace`; the elastic-machine part is out of scope.
- Tests and PR plan updated for escalation, host exclusion, host choice, `host_reserved`, `prepare_timeout` and the new caps.
