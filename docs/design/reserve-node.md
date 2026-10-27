# Reserving a whole node for a quiet run

Status: design, revision 2, not built. Target path: `docs/design/reserve-node.md`.

Vocabulary: "exists" means origin/main has the code today (checked by grep for this revision). "new" means this note proposes it. Defaults are proposals and live in configuration.

## 1. Problem and goal

Performance work needs a machine on which nothing else of the farm's runs, whose CPU frequency policy is known, and whose remaining noise is measured. A shared farm gives none of that, and a cached result measures nothing.

Goal: an action asks for an entire node and gets it. For the duration no other lease is on the node; the node was drained of other work first (nothing is killed); the result states governor, frequency, boost, SMT and the CPU time that was not the action's, and says plainly which noise sources kbf controls, which it only measures and which it does not cover (section 8.3). The ask is capped so a stream of requests cannot starve the farm. The surface stays compatible with the whole-node lease class of the jobscheduler design (long-term home).

First release scope: per-action reservation, Linux. Per-build holders are cut from this design (section 14, appendix B).

Non-goals: setting governor or frequency (the daemon is unprivileged; the host image sets them and the report states them); preempting running work (not built, so a reservation waits); a general quota system; making a converged storage neighbour quiet (section 7.2).

## 2. What exists and what is new

| Piece | Exists | New |
|---|---|---|
| `kbf-lease=whole_machine` parsed by the front, `LeaseKind::WholeMachine`, booking the worker's whole capacity | yes | none |
| Driver for it | Mac only (`native-whole-machine`) | Linux container driver serves it on daemons that opt in |
| Placement: `Servable::fit` takes the least loaded worker, ties by name (not first-fit); `Servable::reserve` holds a worker for a queued whole-machine request, recomputed every round from the queue walk | yes, in memory, stateless, uncapped, held from round one | replaced for whole-machine requests by a stored, capped, deadline-bearing hold that begins after `drain_after` (4.2) |
| Undo of a committed grant when the daemon refuses Start (`unplace`), a per-request attempt counter (`max_replace`) | **no** (`git grep` finds neither) | new scheduler and durable-store work, its own PR (4.2) |
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
- FAILED_PRECONDITION, `reserve-no-capacity`: no node of the pool lists the driver, or `reservable` is 0 by the table in 4.3 at admission. A one-node pool cannot reserve.

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

A node is in at most one reservation. Feasible means: it lists the Linux whole-machine driver; it is serving (not cordoned, draining, drained, paused, held); it is in no reservation; and it is connected to the frontend that owns the operation. The pick is the existing `reserve` rule: the worker held last round, else the fewest leases, then least booked, then first by name; after a given-up hold the next attempt avoids the previous node (`op.avoid` exists; tested).

### 4.2 Drain, hold, and the Start protocol

States, in jobscheduler's words:

```
queued -> draining -> exclusive_ready -> running -> restoring -> released
```

1. queued: nothing is held. If a feasible node has no lease, `has_room` grants at once.
2. After `drain_after` (30 s) in the queue with no grant, the request claims a slot (4.3) and takes a hold on one node: `draining`.
   - **The per-round `reservations` derivation is replaced for whole-machine requests.** The slot row is the authority; the in-memory map is loaded from rows at intake; a round only decides whether to claim one. A queued whole-machine request without a row holds nothing, so younger work lands on feasible nodes (on Macs too, where today's hold from round one changes; see 4.6).
   - Placement on the held node skips operations queued after the request (equal or lower urgency and younger). More urgent work is still placed there (today's rule; QoS is `ci` for everything today, so that branch is tested with synthetic QoS). Running leases are never stopped.
3. When the node's last lease ends the grant is committed, then Start is sent (as today). **The daemon runs its prepare checks before it accepts Start** and refuses with a typed reason when they fail. It does not report `Preparing`: that phase names a lease the scheduler already holds and carries the session-switch grace. Prepare is: lease table empty and no lease cgroup; static snapshot; pre-run probe (4.7); cpuset partition (7.1) applied before accept; sampler started. Accept is the `exclusive_ready` event. The refusal is a new field on the Start response in `worker.proto`. An old daemon never sends it (and never lists the driver); an old server never reads it; skew is safe in both directions and tested.
4. On refusal the scheduler undoes the grant. **This is new work, not reuse**: `unplace` and the attempt counter do not exist. Design: the refusal carries "no process was started", so the scheduler may free the node without waiting `LEASE_GRACE` (60 s); a lost or silent Start still follows today's START_GRACE / lease-lost path with the fence. The attempt count is stored in the operation's row, counts against `max_replace`, and the node is flagged for the operator. A refusal is retriable (`exclusive-not-ready`).
5. running, then `restoring`: leases removed, cpuset and quota restored (7.1), node back to serving. A node that fails restore stays out of service and is visible to the operator. Whether this reuses the existing post-lease phase is decided by reading the phase code at implementation; it is not a reason to add a protocol field.

The hold is a scheduler state, not a cordon. A cordon is skipped by every request including the reserving one, and belongs to the update machinery.

### 4.3 Caps

All farm-wide, enforced with store rows (section 6).

- Slots: `reservable = min(max_nodes, floor(max_fraction * N))`, N the count of serving nodes of the pool that list the driver, **read from store-visible node rows** (the node look the updater reads), not from one frontend's connections. Defaults `max_nodes = 1`, `max_fraction = 0.25`. A table is a test (11): (N, max_nodes, max_fraction) -> reservable, including N = 0, 1, 3, 4, 6.
  - The `floor` makes a pool of 1 to 3 nodes unable to reserve under the default fraction; that is intended and reported as `reserve-no-capacity`. A transient dip of N after admission never refuses a queued request: it waits (the front refuses only at admission).
  - A node in `draining`, `exclusive_ready`, `running` or `restoring` occupies a slot.
  - A request that cannot take a slot stays queued without a hold.
- One outstanding hold per owner. With the owner key the instance name this is one hold per instance; stated, because it means one long perf job blocks other reservers of that instance. Intended: the slot is the scarce thing.
- `drain_deadline` (20 min): a hold with no grant is given up; node serving; request backs off (doubling from 1 min); attempt counted; after `max_replace` (3) the request fails `exclusive-not-ready`. Nothing is killed to finish a drain.
- `max_wait` (4 h from submission): RESOURCE_EXHAUSTED, `reserve-wait-exceeded`. The arithmetic, explicit: with one slot and `max_hold` 2 h, a second reserver can legitimately wait out `max_wait` behind two healthy holds. Intended: refuse and let the client retry, not queue unboundedly.
- `max_hold` (2 h from `exclusive_ready`): the backstop. The action timeout is required to be below it, so the lease ends first.
- Slots are claimed by conditional create (6). Lowering a cap never revokes a hold in progress.

### 4.4 Fairness and starvation, both ways

Farm starved by reservers: bounded by slot count, one hold per owner, deadline and back-off. Work that only the held node can serve must wait, not be refused. Today a cordoned worker gives `Verdict::Cordoned` (a waiting verdict in `servable.rs`, text "every live worker that can run it is cordoned"). A hold needs its own waiting verdict class, `Held`, with its own reason text, added to the verdict enumerations in the fleet property tests (`tests/f3_fleet/check.rs`, `tests/f4/reference.rs`, `f4/check/round.rs`), so the 300 s unservable wait never turns a hold into a refusal. Tested at 299 s and 301 s of injected time.

Reserver starved by normal work: `fit` takes the least loaded worker (ties by name), so an idle node is picked first, which suits a reserver; the hold then stops younger work landing and the node empties as its leases end, bounded by the longest lease on it. Not solved: a pool where every node always carries a long lease; `max_replace` and the explicit refusal handle that. The real fix is preemption of `batch` work (planned, not built).

Among reservers: first in, first out within QoS (ties by submission sequence, tested).

### 4.5 Comparability across runs

The scheduler picks the node, and nodes may differ (stepping, microcode, kernel, memory layout). Two QUIET results from two nodes are not comparable by that fact alone. So the report carries a `hardware_fingerprint` (8.1); `kbf build show` warns when node id or fingerprint differs between runs it shows; the docs say cross-node comparison is unsafe. Pinning a node or class is `kbf-node` (planned/refused today): a follow-up with its own design.

### 4.6 Mac pool

Capping Mac holds (one per platform pool) changes today's uncapped behaviour on the 2-Mac pool. It is a separate PR behind a separate flag (`reserve_cap_mac`), default off first, so the Linux work changes no Mac behaviour. (The replaced per-round derivation applies to Linux-driver requests only until that PR lands.)

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

Row family `reserve/slot-<i>`, `i` in `0..reservable`: `{ operation_id, owner, node, state (draining|held), claimed_ms, deadline_ms, frontend_term }`.

- Claim: conditional create-if-absent on one slot row, **only after a node is chosen** (a frontend with no feasible node never claims a slot; release if the node vanishes). Free slots and the cap are the same fact. A frontend that loses the create stays queued.
- Ownership: only the frontend that owns the operation creates and updates its row, on a node it serves. A request whose owner serves no opted-in node is "unservable" there, not "unplaced", and ownership today moves only on "cannot place". New: for whole-machine requests unservable on the owner, the owner releases the operation so the store can give it to a frontend that serves a reservable node. Tested with a two-frontend case where the only opted-in node is on the non-owner.
- Renewal: the owner refreshes `deadline_ms` while the hold is live. At expiry plus one `LEASE_GRACE` any frontend deletes with a version-conditional delete, so a dead frontend's draining hold frees its slot unaided. Renew-vs-expire: exactly one wins, both orders tested.
- A stale-expired row whose node still runs a reservation's leases frees the slot, not the node's placement; leases run to their end.
- `reservable` is read from configuration by each frontend. On a shrink, rows at index >= reservable keep occupying until they end; on a grow, new indices are claimable at once. Nothing is revoked.
- The node look gains a `reserve` field (state, owner, since, deadline).
- Invariant tested at every store-write kill point (slot create, grant commit, Start sent, slot delete): after recovery a slot row exists iff a hold or reservation lease exists.

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

Existing registry (names indicative): `kbf_reserve_requests_total{outcome}` (granted, refused by reason, given_up, not_ready, wait_exceeded, lost); `kbf_reserve_wait_seconds`, `_drain_seconds`, `_hold_seconds`; `kbf_reserve_slots{state}` and `_limit`; `kbf_reserve_verdict_total{verdict}`. Operator: the node listing shows `reserve`; a log line at every transition (reservation id, node); one action `release-hold` on a `draining` or idle-held node: drops hold and slot, never kills a lease.

## 10. Interaction with updates and the coordinator

- A node in a reservation is not picked for an update drain; the updater waits, bounded by `max_hold`, and sees it in the node look.
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
| S | (N, max_nodes, max_fraction) -> reservable incl. 0, 1-node pool, mid-hold shrink; transient N dip does not refuse a queued request | per-frontend N; off by one |
| R | Slot race: two frontends, one slot; renew-vs-expire both orders; expiry at grace boundary (-1, 0); shrink with live rows beyond index; no claim before node chosen | read-then-write; unconditional delete; claim before pick |
| R | Same owner twice: one hold | owner key ignored |
| S | `drain_deadline`: node serving, back-off doubling, attempt counted, next attempt avoids previous node; `max_replace` -> `exclusive-not-ready` | deadline never fires; counter reset |
| S | Start refused with "no process started": grant undone at once, no 60 s fence, attempt counted; Start lost: START_GRACE path unchanged | undo skipped; undo without ack |
| S | Cancel in draining, between commit and Start, running: slot and hold freed, no attempt counted | slot leaked |
| S | Cordon while draining frees slot, no attempt counted | slot leaked |
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

The jobscheduler `whole_machine` class: a host with no lease is picked; after `drain_after` one host is marked draining and nothing new is placed on it; at most one draining host per pool or `max_draining_fraction`; a drain past `drain_deadline` is given up and backs off; `max_replace` bounds retries then `exclusive-not-ready`; the agent reports `exclusive_ready` and the lease token is minted only after it.

| Here | There |
|---|---|
| `kbf-lease=whole_machine` | lease class `whole_machine` plus selectors from the consumer's config |
| `drain_after`, `drain_deadline`, `max_replace`, slots | same-named pool parameters; slot rows become the pool's drain cap |
| `draining`, `exclusive_ready`, `restoring` | same words |
| Start after grant commit and daemon accept (typed refusal) | token minted after `exclusive_ready`; refusal is `exclusive-not-ready` |
| `exclusive-not-ready`, queued, refused | the three outcomes stay distinct |
| `NodeState` `Any` | consumer-owned; the daemon is unchanged by the move |
| Allow-list, per-owner limit, `max_wait`, `reserve-*` reasons | stay in kbf: jobscheduler has no tenant caps |

Divergence to remove, not carry: kbf's drain is an operator cordon; the reservation hold is scheduler-driven and stored, as jobscheduler's is. Property, outcome and state names are kbf's and do not change on the move.

## 13. PR plan

1. Docs-only: fix the `cordon.rs` header and the `CgroupUsage` comment.
2. Prerequisite: coordinator zero-free-slots test (10); read the front's identity source (3.2).
3. kbf-sched: pure hold/slot state, `Held` verdict, undo-grant with attempt counter, tests and mutants (S).
4. Slot rows and two-frontend ownership (R).
5. Front: refusals, forced `do_not_cache`, no-coalescing and daemon-path cache tests.
6. Daemon: `--whole-node`, lists, prepare, typed Start refusal in `worker.proto`, cpuset partition (D).
7. Readers, sampler, `NodeState`, verdict, `Reservation` stamp, recorder, `kbf build show`.
8. Farm test arms (N); host docs on storage confinement.
9. Separate: Mac cap (4.6). Later, own designs: `kbf-node` pinning; per-build holder.

## 14. Open questions for the owner (with a recommendation)

1. Name: reuse `kbf-lease=whole_machine`; no `kbf-reserve=node`. Recommend: reuse.
2. Per-build holder: cut from this release. Recommend: separate design after per-action ships (needs a scheduler non-lease hold, frontend affinity by reservation id, an (instance, id) key, and jobscheduler's answer on consumer-decided multi-operation leases).
3. Cache stance: never store or coalesce reserving results; the client changes the action for repeats. Recommend: as written.
4. Converged neighbour: report plus operator-set confinement of the storage unit, only confined nodes reservable (perhaps 1 of 6). Recommend: report and document; a daemon-enforced partition is out of reach today.
5. Governor and boost: state, never set; node opt-in with accepted lists. Recommend: as written.
6. Defaults (one node, 20 min drain deadline, 3 attempts, 4 h wait, 2 h hold, 1% noise placeholder). Recommend: configuration, tune after a month.
7. Allow-list identity: instance name first (one instance is one owner), client identity when the front exposes one.
8. Mac cap: separate flagged PR, default off first. Recommend: yes.
9. VM kind refused with reservation until the VM driver has a whole-node story.
10. A pool where every node always carries a long lease ends in `exclusive-not-ready`. Recommend: accept until preemption exists; revisit by metric.
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
