# Starvation of a large booking by small actions: who owns the rule

Status: DESIGN, for review. Nothing here is built. Issue: komira-ai/kbf#169. Revised to the whole-node ruling: reserve the host with the least tail. Amended: the farm's live reservation engine is `js-registry`, not `js-sched` (2.4); where the rule is placed is an open choice (3.2).

Each claim is marked **[code]** (read from the named file and function, at the kbf
`main` and at the jobscheduler commit pinned below), **[doc]** (stated by a comment or
document, not checked against behaviour), **[proposed]**, or **[verify]** (believed, not
yet read or run).

**Pinned jobscheduler commit: `e23a2b4ccf14a3a557878f3992202b9f6a16b697`** (komira-ai/jobscheduler
`main`). Every `path:line` into jobscheduler below is at that commit; a line number is
the line of the named item, and moves when the file does.

Three crates are named. `js-sched` is the embedded scheduler (`crates/js-sched`) that kbf
links. `js-registry` is the registry the lease server runs (`crates/js-registry`). `js-whole` (`crates/js-whole`) is
the pure whole-node rules both depend on (`crates/js-sched/Cargo.toml:14`,
`crates/js-registry/Cargo.toml:19`). "js" with no suffix below meant `js-sched` in the
earlier text; where the text still says it, read `js-sched` and see 2.4.

## 1. The question

kbf#169: while a stream of one-core actions runs, an action booking 24 CPUs / 24 GiB is
passed over until the stream ends (34 s in the report; all 200 later small actions started
first). Bookings are never exceeded. Now that the farm can run with `--scheduler-core js`,
who owns the cure: the legacy core, js, or both?

## 2. Facts

### 2.1 There is one placement implementation, not two

- **[code]** `crates/kbf-server/src/farm.rs: Farm::new_with_core` builds the legacy core with
  `Scheduler::new(term).with_load_config(PlacementLoad::Off.config())`, where `Scheduler`
  is `kbf_sched::Scheduler`. `crates/kbf-sched/src/lib.rs` is `pub use js_sched::*`, at the
  revision pinned in the workspace `Cargo.toml`.
- **[code]** `crates/kbf-server/src/farm_js/open.rs` builds the js core's scheduler with
  `.with_load_config(placement.config())`, the same `PlacementLoad::config()`
  (`crates/kbf-server/src/placement_load.rs`).
- **[code]** `--placement-load` (`config.rs`, default `off`) is read under either
  `--scheduler-core`. `placement_load.rs` says kbf passes an explicit load config so
  that a library bump cannot change placement.

So the legacy core has no placement code of its own. Both cores place with js's
`Scheduler::place` (`crates/js-sched/src/scheduler.rs`) and `Servable::fit`
(`crates/js-sched/src/servable.rs`). They differ in the driver around it (`farm/driver.rs`:
`Driver::Legacy` vs `Driver::Js`), not in the rule that decides who waits.
The premise "load placement is js's default" holds for the library
(`crates/js-sched/src/admission/config.rs: PlacementLoad` defaults to `On`, `Off` is documented
as a rollback switch), **not for kbf**: kbf defaults to `Off` on both cores.

### 2.2 What happens to a large request behind small ones

`Scheduler::place` (`crates/js-sched/src/scheduler.rs`) walks the queue in order
`(Reverse(Qos), OperationId)` (field `queue`), so urgency first, then age. For each
operation it calls `Servable::fit` (`crates/js-sched/src/servable.rs`).

- **`--placement-load off` (kbf default today, either core). [code]** `fit` computes
  `by_load = adm.by_load() && kind == Action`, which is false; candidates are filtered by
  `Worker::has_room(request, false)` (free booked CPUs must hold the request), the least
  booked worker is taken, and `Fit.blocked` stays `None`. In `place`, the head hold is
  taken only `if blocked == Some(Blocked::Load)`, so nothing is held. A 24-CPU action
  that fits no worker now is skipped, and every later small action that fits is placed.
  This is the behaviour reported in #169. (That the report was taken with `off` is my
  inference from "no reservation for action leases"; **nobody has run #169 under `on`**.)
- **`--placement-load on`. [code]** `fit` returns `blocked: Some(Blocked::Load)` when
  candidates exist but the load predicate refuses all (`Servable::admitted`). `place`
  then calls `Servable::hold_for_head`: the request holds the worker with the smallest
  binding ratio (sticky within a tenth, `pick`), one hold per pool (`held_pools`), added
  to `reserved`, so operations after it in queue order are not placed there and the
  worker empties as its leases end. Work before it in queue order is still placed.
  The hold is taken the first round the request is blocked, recomputed every round, in
  memory, and counted in `kbf_sched_load_reservations_total`. Tests:
  `crates/js-sched/tests/admission_place.rs: a_blocked_head_holds_a_worker_and_starts_when_it_empties`,
  `work_ahead_of_the_head_is_never_held_back`,
  `one_hold_per_pool_and_the_second_head_waits_its_turn`. So the head hold is the
  anti-starvation rule for ordinary actions, and it exists, but only under `on`.
  Whether it already cures #169 is open: the load predicate allows a worker to be charged
  above its capacity (CPU slack, default 1.1x), so a 24-CPU request may be admitted by
  load without ever being blocked; that run has not been made.
- **Not covered under `on`. [code]**
  - `Blocked::ColdCap` (only the cold-start cap refused) takes no hold.
  - A VM lease is never given `blocked` (the `vm_by_load` branch of `fit` only filters
    candidates), so a VM waits behind actions; `Worker::has_room` says "nothing reserves a
    node for a queued VM". `macos-vms.md` (reservation for VM leases) describes it as
    planned.
  - **The hold picks by load ratio, not by remaining time, and admits no backfill.**
    `hold_for_head` takes the worker with the smallest binding ratio (sticky within a
    tenth), not the one that empties first, and every worker in `reserved` is closed to
    all later operations, short ones included. Work *before* the head in queue order (a
    higher QoS) is still placed there, so a steady higher-QoS stream refills the worker
    and a lower-QoS head waits as long as the stream lasts. The hold has no timer; the
    whole-node path has `drain_after_ms`, `drain_deadline_ms` and `max_reserve_attempts`.
  - The comment above `Scheduler::place` (`crates/js-sched/src/scheduler.rs:607`, "No other
    operation holds a worker"; `place` is at `scheduler.rs:629`) is stale: the code holds a
    worker for a blocked head (`head_holds`, `scheduler.rs:774`) and for an `owns_worker`
    action (`scheduler.rs:707`) as well as for whole-machine requests.
  - No simulation covers ordinary actions. `crates/js-sched/tests/sim_whole_machine.rs`
    asserts a wait bound (`CI_BOUND`) for whole-machine leases only. I found no kbf test of
    the #169 shape.

### 2.3 What the whole-node path already does (it is most of the rule)

- **[code]** A `LeaseKind::WholeMachine` request that fits nowhere holds a worker
  (`crates/js-sched/src/scheduler/whole.rs`: `Scheduler::hold_whole`, `WholeMark`,
  `Drain`; called from `place`). With whole-node settings on, the worker is the first of
  `Scheduler::drain_order` (`crates/js-sched/src/scheduler/whole.rs:286`) that could run
  it. `drain_order` builds one `HostLoad` per worker whose `end_bounds` are each held
  lease's `started_at` (else `granted_at`) plus the lease's `max_wall` (`whole.rs:287-310`),
  and `js_whole::cost::drain_cost` (`crates/js-whole/src/cost.rs:30-45`) sorts hosts by
  `wait_ms = max(end_bounds) - now`, then fewest leases, then name. A lease with no
  `max_wall` counts as ending at `u64::MAX` (`whole.rs:289`; `set_max_wall`, `whole.rs:280`,
  says so). That is the host with the least **upper bound** on its remaining time, not
  the least estimated tail: it ranks by the declared bound, and every host holding a
  lease with no bound ties at the maximum.
  Without whole-node settings, `Servable::reserve` (`crates/js-sched/src/servable.rs:567`) takes fewest leases, then least booked.
- **[code]** Today's drain mark is a reservation of the whole worker: its name is added to
  `reserved`, so nothing after it in queue order is placed there. It is the right
  object, but it has a clock: `drain_after_ms` before it is made, a `deadline`
  (`reserve_deadline`) after which it is given up and the worker barred for
  `reserve_backoff_ms`, up to `max_reserve_attempts`. It is in memory only (so is the
  registry's, 2.4).
- **[code]** The #169 request (an `Action` booking 24 of 32 cores) never reaches this
  path: `place` calls `hold_whole` only for `WholeMachine`, `Servable::reserve` only for
  `owns_worker()` actions, and `hold_for_head` (load ratio) for a `Blocked::Load` head.
  Three holds, three pick rules.

### 2.4 Two engines place a whole-node reservation; the farm runs only one

- **[code]** The lease server does not call `js-sched`. `crates/js-lease-server/src/sched/mod.rs:3-4`
  says only the pure conversions exist and "Nothing in the server calls them yet"; the
  shell that would call them is not written. `crates/js-lease-server/Cargo.toml:21`
  depends on `js-sched`, but no placement is made through it. `Scheduler::place` runs
  in kbf's embedded core (2.1) and in sims.
- **[code]** The live whole-node reservation is `js-registry`:
  `crates/js-registry/src/whole/reserve.rs` (`advance` at `:189`, `place` at `:381`,
  `keep_or_move` at `:351`), the wait list `whole/queue.rs` and request state
  `whole/requests.rs`, and the start gate `reserve::may_start` (`reserve.rs:245`) called from
  `crates/js-registry/src/registry/start.rs:179`. Its host table is read into `HostFacts`
  by `Registry::host_facts` (`start.rs:24`).
- **[code]** Both engines rank hosts with one function, `js_whole::cost::drain_cost`
  (`js-sched`: `scheduler/whole.rs:316`; `js-registry`: `reserve.rs:403`; the registry
  reaches it through `pub(crate) use js_whole::{caps, cost}`, `whole/mod.rs:18`). Its
  input, `HostLoad.end_bounds` (`cost.rs:14`), is each lease's start plus its declared
  `max_wall`. In the registry the bound is `record.bound` (`start.rs:24-33`) and a host
  holds one booked lease (`Registry::booked` returns host to one lease,
  `crates/js-registry/src/provider.rs:60`).
- **[code]** The rank is an upper bound, not an estimate. `drain_cost` takes
  `max(end_bounds)` (`cost.rs:38`) and the js-sched side feeds `u64::MAX` for a lease with no
  `max_wall` (`whole.rs:289`). A host whose lease has the longer declared wall but would
  end sooner in fact is ranked behind one that would end later.
- **[code]** Neither engine persists its marks. The registry's say so
  (`reserve.rs:40-49`: "held in memory only", with the lease file recording only the
  whole-node reservation of a lease it holds); `js-sched`'s `Drain` is in memory (2.3).
  Persisting the registry's marks is follow-on work, an additive record in the lease
  file, outside this note.
- **[code]** The estimates exist on the lease server's side. `NodeUsage`
  (`crates/js-lease-server/src/usage_js.rs:118`) owns a `ProfileStore` (`usage_js.rs:90`,
  `js_usage::ProfileStore`, `crates/js-usage/src/profile_store.rs:92`); `ProfileStore::profile(&ProfileKey)`
  (`profile_store.rs:262`) returns a `KeyProfile` whose `fit` is `None` below three
  runs and otherwise carries `wall_minutes_p90` (`crates/js-usage/src/fit.rs:60`), "how long
  a new lease of the key is expected to last": the P90 of the runs' wall minutes, rounded
  up per run, at least 1. The registry maps a run to its key by `Registry::profile_of`
  (`crates/js-registry/src/registry.rs:862`). `js-registry` does not depend on `js-usage`
  (`Cargo.toml`), so an estimate reaches it as a value passed in, not a call.
- **[verify]** A lease on a registry host is reachable to its profile key at the moment
  `host_facts` is built (the lease's run is `profile_of`'s argument).

So "js owns the rule in `Scheduler::place`" (earlier text) was wrong for the farm: a rule
placed only in `js-sched` would change the embedded kbf core and nothing the lease server
does.

## 3. The rule [proposed]

### 3.1 Statement

A request is **large** when it is queued, fits no worker now, and some worker could hold
it once its leases end: a `WholeMachine` lease, an `owns_worker` action, or a
`Blocked::Load` head such as the 24-of-32-core #169 request.

1. **Start time.** For a worker, the start time of a large request is the earliest time at
   which, with every held lease released at its estimated end, the free capacity holds
   the request's demand. For a whole-machine or owner-worker request the demand is the
   whole worker, so this is the latest lease end; for the #169 request it is the time the
   first leases free 24 cores, not the time the worker empties. The **tail** is the start
   time minus now. One function (`start_time(worker, demand)`) serves every kind. For a
   `Blocked::Load` head "holds the demand" is read against the same charge model as
   `Servable::admitted` (`load_state.left` and the lease charges over minutes, the 1.1x
   slack, curve charges), not against bookings: `Worker::has_room(request, by_load)`
   zeroes the CPU check, so a worker can have free booked capacity and still refuse the
   request. After kbf step 1 the load predicate is the only admission. The start time is the
   first minute at which `admitted` would accept, with held leases dropping out at their
   remaining time (3.3). `admitted` also reads the node's reported (measured) load and the
   cold-start cap, which are not functions of the held leases. Recommendation: hold the
   measured part constant at its latest reading and let only the charged part decay with
   the leases; a measured load that is the binding term then gives a long tail (the
   request waits for the load to fall, and the pick prefers a worker where it does not
   bind), and the cold-start cap is projected to lift at its own end. Decaying measured
   load by the lease charge would promise a start the node's real load does not support. A worker that already admits it has tail zero and the request is
   not blocked there. **Scope of the shared function:** `js_whole::cost` knows whole hosts
   (a lease holds the host), so it covers the whole-machine and owner-worker kinds, with
   the demand the whole worker. The partial-capacity start time of a `Blocked::Load` head
   reads the admission charge model, which only `js-sched` has; that part stays in
   `js-sched` (3.4) and takes the clamp and the overdue floor from the same `tail()` rule.
2. **Reserve** the worker with the least tail among those that could hold the request.
   The pick is made again every round with no stickiness: ties go to the lower name, so a
   steady state does not flap. A reservation moves when another worker's tail becomes the
   least, including when the reserved worker's tail grows. The reservation keeps its
   original `made` time across moves, so the queue-age order of the caps does not reset
   (`Drain::mark` drops `made` on a worker change today; it must carry it).
3. **Backfill.** An operation *after the large request in queue order* is placed on the
   reserved worker only if its estimated end is no later than that worker's start time.
   Otherwise it goes elsewhere or waits. Operations *ahead* of the large request in queue
   order are not held back (3.3).
4. **Start.** The large request starts when the start time comes, not when a timer fires.
5. **No timer.** No `head_hold_after`, no `blocked_since`, no fixed hold, and no give-up
   deadline for this hold.

### 3.2 Owner: where the rule lives is an open choice

What is common to both choices (and is the first step, 3.6): **the tail rule is written
once, in `js_whole::cost`**, as pure functions, and both engines' ranking reads it.

- `HostLoad` gains the estimated end of each lease beside its declared end bound.
- `tail()` of a host is the latest remaining time over its leases, each as
  `max(min(estimated end, enforced end) - now, margin)` (3.3): an estimate is clamped to
  the lease's max-wall bound, an unknown estimate falls back to the bound, and a lease
  with no bound is unknown (3.5).
- `drain_cost` sorts by tail, then fewest leases, then name.
- `fits_in_tail(candidate estimated end, host tail, now)` is the backfill test (rule 3 of
  3.1): true only when the candidate's estimated end is no later than the host's start
  time.

Who calls it, and who feeds the estimates, is the CEO's decision. Neither choice changes the rule.

- **Path R (rule in `js-whole`, the registry is the live engine).** The tail is read
  by `js-registry`'s `reserve.rs` (the pick at `:403`) and `reserve::may_start`
  (`:245`), whose refuse-every-start arm for a marked host is replaced by
  `fits_in_tail`. The lease server feeds estimates from its `ProfileStore` (2.4) into
  `HostFacts`, behind a flag. `js-sched` is the second user of the same function
  (`drain_order`, `whole.rs:286`). One implementation of the rule, no second shell,
  no mirrored state. The cost: the registry keeps its sticky mark, deadline and give-up
  (`reserve.rs`) until its own change; one lease per host means a draining registry host has
  no room to backfill into (4.14); and `js-sched` keeps three picks (3.4) for the
  ordinary large request, which `js-whole` does not cover (3.1).
- **Path S (`js-sched` becomes the lease server's placement engine).** Wire up
  `crates/js-lease-server/src/sched/map.rs` so the server builds a `Scheduler` and places
  runs through it, which makes `js-sched` place ordinary runs too, and the whole rule
  of 3.1 to 3.4 (start time for a partial request, backfill in `fit`, one hold per
  pool) applies to the farm as written. The cost: a placement shell, a rewrite of the
  registry's start and wait path onto `Scheduler`, and two persistence and
  hosts-API contracts (D55, `drain_deadline_ms`) moving at once. It is a larger,
  separate decision than starvation.
- **Engineering's lean:** R now, S later if `js-sched` should place ordinary runs.
  R reaches the farm's actual reservation with one new function and no new
  deployment shape; S is justified by what it would do for ordinary runs, not by
  this issue.

kbf owns only the flag default, the both-core test, and the wiring of kbf step 0 (passing the
submission's `max_wall` to the js core). No kbf scheduling code, under either path.

### 3.3 Limits, accepted and tested

- **Work ahead in queue order.** `reserved` is built during the queue walk and is empty at
  the start of each round, so rule 3 binds only operations after the large request.
  Older work ahead of it (and any higher-QoS work) still lands on the reserved worker.
  For the same QoS this is a finite set: every later arrival has a higher operation id
  and is behind the large request, so the tail cannot be extended forever by older
  same-QoS work. A steady higher-QoS stream still starves it; that is decision 4.3. We
  accept both and test both (3.7 e and f). Applying rule 3 to work ahead needs last
  round's reservation as an input to the walk; not built (decision 4.9).
- **Overdue leases.** `drain_cost` reads a lease past its end as zero remaining
  (`saturating_sub`, `cost.rs:39`), so a still-busy worker would look empty, become the least-tail pick,
  and close backfill. A lease's **remaining time** is
  `max(min(estimated end, enforced end) - now, margin)`: the cap at the enforced end never
  takes it below `margin`, because a lease past its enforced end still holds the worker
  until the next lease tick releases it. `margin` is a js setting (default one minute). Per core: js-lease releases a lease
  past its wall at its next tick; the embedded core has no tick, and there the floor
  holds until the kbf action timeout ends the lease. A learned estimate that proves too short
  therefore keeps the worker from looking free. This rule is part of `tail()` in
  `js_whole::cost` (3.2), so both engines have it.

### 3.4 Mapping onto the code that exists

The bullets below describe `js-sched` (the embedded scheduler; under Path R its second
user, under Path S the farm's engine). The `js-whole` and `js-registry` parts are in 3.2
and 3.6.

- **Which worker.** One pick for the three kinds, from `drain_cost`'s order generalised
  by `start_time`. Today only `WholeMachine` is chosen that way, and only with whole-node
  settings on; `drain_order` must stop requiring them. `Servable::reserve` returns its
  `kept` worker before it looks at the drain order, so for rule 2 it is called with
  `kept = None`. `hold_for_head`'s binding-ratio pick goes away.
- **Is the drain mark the reservation?** For `WholeMachine`, yes: `reserved` plus
  `WholeMark`'s worker and `made`. What goes for this rule: `deadline`, the give-up count,
  `reserve_backoff_ms` and `drain_after_ms` as a wait before holding. For an ordinary
  large action the reservation stays in `head_holds`, **not** in `drain.marks` (below).
- **Cap check on a re-pick.** `hold_whole` runs `may_reserve` only when the pick differs
  from the kept worker, because the holder's own mark is already in `whole_counts`. With
  `kept = None` the check would run every round against its own mark, and at
  `max_draining = 1` the holder would lose its hold and flap. So: pick with
  `kept = None`; skip the cap checks when the pick is the previous mark's worker; when
  it moves, exclude the request's own mark from the counts. Red test: a request at the
  cap keeps its hold across rounds.
- **Three stores today, and what each keeps.** (1) `drain.marks`: `WholeMachine` with
  whole-node settings; capped. (2) `self.reservations`, via `Servable::reserve`:
  `owns_worker` actions, and `WholeMachine` without settings; uncapped, so four queued
  owner-worker actions can each reserve a worker. (3) `head_holds`: `Blocked::Load`
  heads; one per pool by `held_pools`. The rule changes the pick in all three and
  merges none of them. For (2) the bound is one reserved worker per pool per round, the
  same `held_pools` rule as (3), applied to owner-worker actions and to settings-less
  `WholeMachine`. **This changes shipped behaviour, deliberately:** settings-less
  `WholeMachine` is the kbf default (`--whole-node-max` unset) and holds today with no
  per-pool bound, so a second queued request in the same pool will no longer hold a
  worker in the same round. I read the tests by name: `whole_node_{caps,drain,order,
  quiet,view}.rs` run with whole-node settings, use store (1), and are unaffected;
  `whole_worker.rs` holds one request at a time, and the one beside a whole-node pool
  (`a_whole_worker_request_holds_a_worker_beside_a_whole_node_pool`) is a single hold.
  I found none that asserts two concurrent settings-less holds; if the red run finds
  one, it is changed to assert the new one-per-pool order, not kept. An owner-worker
  action and a `Blocked::Load` head share the pool key (same `needs` and kind); the one
  earlier in queue order takes the pool's hold. Test (q).
- **Caps.** `whole_counts` counts every entry of `drain.marks` toward `max_whole_node` and
  `max_draining`, and `whole_round` drops the newest marks beyond `ceiling_excess` at the
  start of each round. An ordinary large action in `drain.marks` would be counted as a
  whole node and could be dropped silently by the ceiling; `last_capable` also assumes
  only whole marks take a worker from an ordinary request. So ordinary large actions
  keep `head_holds` (one per pool, decision 4.5) and are outside the whole-node caps;
  the caps apply to `WholeMachine` requests as today. Different stores, on purpose: one
  capped by the whole-node flags, the others not. What bounds the ordinary one is `held_pools`: one held
  worker per pool per round, so four queued 24-of-32 requests in one pool take one
  worker, never the pool. Several pools (platform and kind classes) can each hold one.
- **Backfill.** In `place`, a later operation that `fit` would put on a reserved worker
  is allowed if `now + estimate(op) <= start_time`. This needs the start time carried
  from the pick into `Servable::fit` as a per-worker limit beside `reserved`, and a
  lease's estimated end (`started_at`, else `granted_at`, plus its estimated duration)
  for held leases and the candidate. `drain_order` already counts this round's
  placed leases, and backfill must too. Today only the declared `max_wall` is known,
  and on the js core not even that (step 0).

### 3.5 Estimates, bounds and the cold start

Estimates come from historical tracking: a duration per key learned from recorded runs
(jobscheduler#245, jobscheduler#446; kbf#1950 `--usage-profiles`). **[code]** On the lease
server the source is the existing `ProfileStore` (2.4): `Fit::wall_minutes_p90`
(`crates/js-usage/src/fit.rs:60`) is the P90 of the key's wall minutes, rounded up per
run and at least 1 minute, and is `None` below three runs. That answers the earlier
open question for the lease server: no new duration per key is needed there, and the
estimate is a P90 in whole minutes, so it is deliberately not an optimistic one.
**[verify]** For the embedded `js-sched`, `Profile::timed` reads `life`
(`crates/js-sched/src/admission/profile.rs:246`); whether `life` is the same P90 or only
how long the curve is charged is not read; the js-sched step reads it first.

- **Fallback [proposed, and it is a fallback]: the declared bound.** The estimate of a
  lease is its declared `max_wall`. A bound above `ceiling` is **refused for backfill**
  (treated as unknown; a clamped bound would not be enforced, and its overrun would run
  to the real timeout), and a lease with no declared bound is unknown. For the pick,
  unknown counts as `ceiling`; for backfill, unknown is refused. In `js_whole::cost` an
  estimate is also clamped to its lease's bound (3.2). `ceiling` is **new
  configuration in js-sched**, fed by kbf (js-sched does not depend on js-lease, and
  the embedded farm has no lease table). On the lease server the wall ceiling already
  exists (`server.max_lease_wall`, refused above, `crates/js-lease-server/src/lib.rs:94`) and the
  same value is passed to the cost function. It should carry the same value as the wall
  ceiling js-lease refuses above (`Settings::with_max_wall`, `WallAboveCeiling`), so one
  concept has one value; it refuses, it does not clamp. The scheduler has no pools of
  its own, so one ceiling is passed to it. Learned history replaces the
  fallback per key once a key has enough clean runs (the threshold in jobscheduler#245).
- **With the fallback alone, backfill admits only a job whose bound is shorter than the
  remaining tail.** A candidate ends at `now + bound`; the reserved lease ends at
  `start + its bound`. A 1 h lease 50 minutes in has a 10 minute tail, and a job declaring
  5 minutes backfills. A job with a bound as long as the held leases' never does, so the
  reserved worker idles up to the tail; on the kbf js core today, where no bound is
  passed, nothing backfills until step 0. Learned estimates widen the set. A test uses
  different bounds on the two sides.
- **What bounds an overrun is an enforced `max_wall`, defined operationally:** a lease
  past its wall is ended by something that runs without the scheduler's help. A
  `set_max_wall` call proves a declared bound, not enforcement. The scheduler learns
  which it has from **one per-core setting**, `bounds_enforced`, which kbf sets once
  step 0 is green (recommended over a per-operation bit: enforcement is a property of the
  executor, not of a job, and a bit per job can be set by a client that is not enforced).
  Without it every bound is treated as unknown for backfill. In js-lease it exists:
  `acquire` expires a lease at the earlier of its ttl and its wall, `renew` never
  extends past it, and `tick` stops an expired one; this serves js-lease-server and
  js-registry. In the embedded farm core (`js-server-core` and `js-sched`) there is none:
  `max_wall` only feeds `set_max_wall`. On the kbf farm the enforcer would be the kbf
  action timeout, which I did not verify. `js-server-core` already takes the wall
  (`SubmitOptions::max_wall`, persisted as the `opwall/` row, applied with
  `set_max_wall`); `crates/kbf-server/src/farm/js/hot.rs` says it cannot, and that
  comment is stale. The gap is kbf only: it passes no `SubmitOptions`. So **step 0 is kbf
  wiring**, plus a test that an action past its wall is ended; backfill is allowed only
  for a job whose bound is enforced.

### 3.6 Steps, in order

**The first step is common to both paths and is the first implementation step.**

1. **`js-whole`: the tail rule, alone.** `crates/js-whole/src/cost.rs`: estimated ends on
   `HostLoad`, `tail()` with the max-wall clamp and the unknown fallback and the overdue
   floor (3.2, 3.3), `drain_cost` sorted by tail, and `fits_in_tail`. Pure functions with
   unit tests, red first: two hosts, A with the longer max wall and the shorter estimated
   tail, B the reverse, and `drain_cost` must put A first (red today: `cost.rs:38` ranks by
   the bound). With no estimates every result equals today's, and the existing goldens
   (the hosts-API field and the D55 record rows) are unchanged: nothing is written
   or exported by this step. Neither engine reads the new fields yet.

**The steps after this depend on the CEO's choice of path (3.2).**

*If Path R:*

2. **The lease server feeds estimates, behind a flag.** From `NodeUsage`'s `ProfileStore`
   (2.4) into `HostFacts.load` in `Registry::host_facts` (`start.rs:24`), the declared
   bound as the fallback; flag off keeps today's pick. Red first: A has the longer max wall
   and the shorter estimated tail, and the mark must land on A.
3. **The backfill gate.** `reserve::may_start` (`reserve.rs:245`) replaces the arm that refuses
   every start on a marked host (`(Some(_), _, _) => false`) with `fits_in_tail`, behind the
   same flag. 4.14 says what this can and cannot do on a one-lease-per-host pool.
4. **Flip the flag.**
5. **`js-sched`, the second user** (jobscheduler#582): `drain_order` reads the same
   `tail()`, with kbf steps 0 and 1 below as its prerequisites. Its three picks, `Blocked::Load` start
   time and the rest of 3.4 are this step.

*If Path S:* the placement shell (`sched/map.rs`) comes next, then 5 as the farm's rule and
the registry's start and wait path moves onto it; 2 to 4 are not built.

*kbf, either path:*

- **kbf step 0: carry and enforce `max_wall` on the js core.** kbf passes the
  submission's `max_wall` in `SubmitOptions` (js-server-core already applies it), fixes the
  stale `hot.rs` comment, and a lease past its wall is ended by the action timeout. Test
  (red today): a js-core job whose `max_wall` passes is ended and its worker's tail ends
  with it. Backfill on the js core is not enabled before this. The lease-server path already enforces its bound in
  js-lease (3.5) and needs no kbf step 0.
- **kbf step 1: make load placement the only mode, then observe.**
  - flip `--placement-load` to `on` (`crates/kbf-server/src/config.rs`);
  - add the both-core #169 test through `Farm` (`SchedulerCore::Legacy` and `Js`, default
    flags): 60 small actions, then two booking 24 CPUs / 24 GiB, then 200 small and six
    booking 4 CPUs / 40 GiB on 32- and 28-core workers; assert every large action starts
    before the small queue is empty. Red today under the `off` default;
  - then remove `off`; this is plumbing through about thirty files, not a literal swap:
    `crates/kbf-server/src/farm.rs:272`, `farm.rs:358`,
    `farm/driver.rs:192` and `:94`, `crates/kbf-estimator-replay/src/replay/sim.rs:186`,
    `crates/kbf-server/src/farm/js/tests.rs:95,128`, and the daemon, integration and
    placement fixtures that pass `PlacementLoad::Off`.
- **kbf: bump the js revision** after the js-sched step. The both-core test stays green.
- **Remove the give-up** in an engine (the hold deadline, back-off and attempt count, and
  `drain_after_ms` as a wait), only after the enforced bound (M1), the re-pick (M2) and
  the overdue rule (M4) are green in that engine: the give-up was also the escape from a
  wrong pick. Compatibility in 4.11. The registry's give-up stays in this work (4.12).
- **Follow-on, not in this note:** persisting the registry's marks as an additive lease-file record.

### 3.7 Tests, written first [proposed]

The lettered tests are the 22-item plan of jobscheduler#582 (the 20 below plus kbf steps 0 and 1);
the letters and the numbering are unchanged. Under Path R some of them live below
`js-sched` instead, and the list says so in this table; the rest stay in `js-sched`
(`crates/js-sched/tests`).

| tests | live in |
|---|---|
| (a) pick by least tail, (d) ceiling and unknown, (g) overdue, (p) past the enforced end, (c) the backfill predicate | unit tests in `crates/js-whole/src/cost.rs`, on `tail()`, `drain_cost`, `fits_in_tail`; (a) and (g) also once through `js-registry` `reserve.rs` (the A/B red test of step 2) |
| (c) a started backfill, (s) `bounds_enforced` | `js-registry` for the gate (step 3, 4.14); `js-sched` for the embedded core |
| (h) moves keep `made`, (m) cap at `max_draining = 1`, (j)-(k) give-up and compatibility | `js-sched` (`Drain`, `hold_whole`). The registry is sticky and keeps its own tests (4.12) |
| (b), (e), (f), (i), (l), (n), (o), (q), (r), (t) | `js-sched` only: partial request, queue order, QoS, whole-node caps, ordinary large actions, recording |
| kbf step 0 and step 1 | kbf |

A test that now lives in `js-whole` is a unit test on the pure function; it does not
replace the `js-sched` test of the same letter, which proves the scheduler reads it.

- kbf step 0: as above. kbf step 1: the both-core #169 test. Red now.
- js: (a) two workers, one short tail, one long: the large request reserves the short
  tail and starts when it ends; red today (the pick is by load ratio). (b) The #169
  shape: the start time is when 24 cores free up, not when the worker empties; the
  pick follows it. (c) Backfill: a job whose estimate ends inside the start time is
  placed on the reserved worker, one ending after it is not, and the large job starts at
  the original start time. (d) The ceiling: a declared bound over `ceiling` is refused for
  backfill, an unknown estimate is refused, and the cases on each side of the ceiling
  behave as stated; with fallback-only estimates a job with a shorter bound than the
  remaining tail backfills and one with an equal or longer bound does not. (e) Older same-QoS
  work ahead of the large request lands on the reserved worker, and the large job starts
  after it but before any later arrival. (f) A lower-QoS large request waits out a
  higher-QoS stream and starts when it stops. (g) Overdue: a worker whose lease is past
  its estimate is not preferred over a worker with a real shorter tail, and admits no
  backfill beyond `margin`. (h) The reservation moves when the reserved worker's tail
  grows past another's, keeps its `made` time, and does not move on a tie. (i) An
  ordinary large action is not counted by `max_whole_node` and is not dropped by
  `ceiling_excess`. (j) At the give-up removal the deadline and give-up tests are removed with the code, and a
  presented request is never given up. (k) Compatibility (js-sched only; the registry is untouched, 4.12): the old flags are
  accepted and inert, and nothing in js-sched expires by `deadline`; an unclaimed
  registry record is still dropped by `regain_grace` as today. (l) Recording: a request
  that starts and one that is cancelled each emit one record (a move only increments its
  counter) with the estimated start, blocked-at and backfilled core-seconds; a released
  lease reports its placement estimate. The sink is an event the scheduler emits to
  its existing observer path (the js change names it), so (l) asserts on that event, not
  on a scrape. (m) A request at `max_draining = 1` keeps its
  hold across rounds. (n) A `Blocked::Load` head whose booked capacity is free but is
  refused by load: its start time follows `admitted`, not bookings; red first. (o) Four
  queued owner-worker actions in one pool reserve one worker. (p) A lease past its
  enforced end does not give tail zero. (q) An owner-worker action and a `Blocked::Load`
  head in one pool: the earlier in queue order holds, the other does not. (r) A
  `Blocked::Load` head where measured load, not lease charge, is the binding term: its
  start time stays at the measured reading and follows only the charged part. (s) With
  `bounds_enforced` unset, no backfill; set, backfill as in (c). (t) The hosts view and
  kbf's whole-mark metric still read `deadline` (4.11).

### 3.8 What is recorded for section 5

Per reservation, in memory and emitted as one event record when the reservation ends
(start, cancel, move or cap drop), not as counters alone: request id, blocked-at (the
first round the request was blocked; `blocked_since` is not built as a decision input,
this is a record field), reserved-at, the estimated start at reservation and the last
estimated start before it began, actual start, number of moves, and backfilled
core-seconds. Per lease: the estimate it was placed with, kept until release, and a
`backfilled` flag on the held lease, so actual over estimated duration is computed at
release. Histograms (wait versus tail estimate, estimate miss) are built from these
records offline; the scheduler exports only the totals. Nothing is persisted (D55
unchanged beyond 4.11); both engines keep their marks in memory only (2.4).

## 4. Decisions

1. **Load placement is the only mode (decided).** kbf's `--placement-load` defaults to
   `on` and the `off` path is removed; the CPU slack stays a setting (default 1.1x).
2. **One pick rule: the least tail (decided).** It is written once in `js_whole::cost`
   (3.2) and replaces the upper-bound rank of `drain_cost` in both engines, the
   binding-ratio pick of `hold_for_head` and the fewest-leases pick of `Servable::reserve`. The earlier
   recommendation to hold by load ratio is withdrawn.
3. **No aging across QoS (recommended, accepted limit).** A lower-QoS large request can
   wait as long as a higher-QoS stream lasts; aging would turn QoS from a priority into a
   weight. Tested by 3.7(f).
4. **No hold timer (decided; conditional on M1, M2 and M4).** `head_hold_after`,
   `blocked_since` and the wait before a hold are not built. The give-up of a presented
   request (`deadline`, the back-off, `max_reserve_attempts`, `drain_after_ms`) is
   removed in the give-up removal step (3.6), after the enforced `max_wall` (kbf step 0), the non-sticky re-pick
   (4.9) and the overdue rule (3.3) are tested: they replace what the give-up did, which
   was to bound a delay and to move a request off a wrong pick. Keep the caps.
5. **One hold per pool, first (recommended).** Several large requests take their tails in
   turn: slower, never stuck by each other. Allow more only if the #169 run shows the
   serial waits are unacceptable on a few nodes.
6. **Cold start: the declared bound, refused above the ceiling (recommended).** A fallback,
   replaced per key by history. Unknown is refused for backfill, never infinite.
7. **VM leases (recommended).** A VM refused for room is a large request: give it the same
   hold, so the VM reservation planned in `macos-vms.md` is not built. If that is not a
   small widening of the same call, leave it for its own issue.
8. **Start time for a partial request (recommended: EASY-style).** Define the start as the
   earliest time free capacity holds the demand (3.1.1) for every kind, rather than
   restricting the rule to whole-worker requests. It is one function, whole-worker
   requests are its special case, and the alternative leaves #169 itself outside the
   rule. Backfill uses the CEO's test only (ends inside the start time); the extra case
   of a job that fits in the capacity the large request will not need is not built, and
   is a later change if the backfill share (section 5) is low.
9. **Sticky or not: not sticky (recommended).** Re-pick every round (3.1.2). Hysteresis
   is added only if the reservation-move counter shows flapping. Work ahead in queue
   order is not held back (3.3); revisit only with a run that shows it extends a tail.
10. **Estimate source (recommended).** The lease server's `ProfileStore` (2.4, 3.5), the declared
    bound as the fallback. For `js-sched`, `estimate(op)` with the fallback; learned estimates
    arrive through jobscheduler#245, jobscheduler#446 and kbf#1950 without changing the rule.

11. **Compatibility of the removal (recommended: keep every name, change the behaviour).**
    - The three `--whole-node-*` flags of the deadline, back-off and attempts stay
      accepted, log a deprecation, and have no effect, so old unit files still start.
    - After the give-up removal `deadline` has no use in js-sched: it gave up a hold, and that is gone.
      The orphan case (a reservation whose request never comes back) is `regain_grace` /
      `claim_by` in the registry, which exists today and is independent of `deadline`;
      nothing else expires by `deadline`.
    - The registry keeps its `deadline` field in the D55 record, the hosts API
      `drain_deadline_ms` (with its golden) and its own timer (4.12), so D55, the wire
      and the golden do not change in this work.
    - `WholeMark.deadline` is a public field and a view column (`whole_view.rs`, read by
      kbf's `whole_mark` metric and `docs/api.md`). It is kept, always absent after step
      4, so the view and the metric do not break; removing it is a later, separate
      change.
    - Docs amended with the give-up removal: `whole-node.md` and `configuration.md` where they describe
      the embedded scheduler's give-up; W6/W14 are about the registry and stay.
12. **The registry keeps its sticky mark, deadline and give-up in this work (recommended).**
    `crates/js-registry/src/whole/reserve.rs` has its own marks, `keep_or_move`
    (`:351`), caps and give-up, and the D55 record, the hosts API field and the
    claim/regain handling live there. Under Path R it takes the *tail rule* (the ranking
    and the backfill test) by calling `js_whole::cost`; it does not take the removal of
    its timer, which carries the persistence and wire changes this work should not.
    This replaces the earlier plan of a separate registry issue after `js-sched`, and the earlier
    "deployment split" reading: the farm's lease server runs `js-registry` and not `js-sched`
    (2.4), so a rule that lives only in `js-sched` reaches the embedded kbf core alone. **[proposed]**
    Whether the registry's timer then conflicts with "no hold timer" (rule 5) is a question for
    the CEO: the rule applies to the pick and the backfill now, and to the timer when
    the registry's own change removes it.
13. **Metrics are recorded, not inferred (recommended).** 3.8.
14. **Backfill on the registry is nearly vacuous today (verify before the backfill gate, step 3).** A registry
    host holds one booked lease (2.4), so a marked host that is draining has no room for
    another start, and a marked host that is quiet has tail zero, where `fits_in_tail` is
    false for any candidate of positive length. Step 3 therefore changes the registry's
    behaviour only where a pool books several leases per host, or where the arm is reached
    with a zero-length candidate. **[verify]** by reading the booking in `book.start_where`
    and by a test; if it holds, step 3 is the gate for a future shape and the value of
    Path R on the registry is the ranking (steps 1 and 2 of 3.6), and the backfill share (section 5) is measured on
    `js-sched`. Say so in the PR rather than count a number the gate cannot move.
15. **Where the rule is placed: Path R or Path S (open; the CEO decides).** 3.2.
    Engineering's lean is R now, S later.

## 5. What it measures (for the paper)

- **Whole-node wait versus the tail estimate:** per large request, time from blocked to
  start against the tail estimated when it reserved (and the last estimate before it
  started). Report p50 and p99.
- **Backfill share:** the fraction of the reserved worker's core-seconds used by
  backfilled work during the wait (utilization of the reserved host while it drains).
  With fallback-only estimates this is limited to jobs with a bound shorter than the remaining tail (3.5);
  report it split by fallback and learned.
- **Estimate miss:** actual start minus estimated start at reservation, and actual over
  estimated duration per backfilled job; fallback versus learned. Also the count of
  reservation moves and of overdue leases.
- The scheduler records per reservation: reserved-at, estimated start, actual start,
  backfilled core-seconds, moves. They are exported as counters and a per-request
  record; no test asserts a time bound.
