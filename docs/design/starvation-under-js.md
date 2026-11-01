# Starvation of a large booking by small actions: who owns the rule

Status: DESIGN, for review. Nothing here is built. Issue: komira-ai/kbf#169. Revised to the whole-node ruling: reserve the host with the least tail. Amended: the lease server's live reservation engine is `js-registry`, not `js-sched` (2.4); where the rule is placed is decided: Path S, approved 2026-11-01 (3.2).

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
  and `js_whole::cost::drain_cost` (`crates/js-whole/src/cost.rs:30-45`) sorts hosts at `e23a2b4c` by
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

### 2.4 Two engines place a whole-node reservation; the lease server runs one of them

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
  `max_wall`. In the registry the bound is `record.bound` (`start.rs:38`, set at `start.rs:196` to the start time plus the
  lease's `max_wall`; field at `registry/record.rs:56`) and a host
  holds one booked lease: the table is `state.book.booked()` (`start.rs:25`), a map from host to one
  lease, and `host_facts` builds `leases: usize::from(lease.is_some())` and at most one end bound
  (`start.rs:44-47`). Nothing in `HostLoad` or a `Mark` (`reserve.rs:69`) records the tail
  at the time a mark is made, so no backfill share or estimate error can be read off the registry today.
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
  (`crates/js-lease-server/src/usage_js.rs:108`, impl at `:118`) owns a `ProfileStore` (`usage_js.rs:90`,
  `js_usage::ProfileStore`, `crates/js-usage/src/profile_store.rs:92`); `ProfileStore::profile(&ProfileKey)`
  (`profile_store.rs:262`) returns a `KeyProfile` whose `fit` is `None` below three
  runs and otherwise carries `wall_minutes_p90` (`crates/js-usage/src/fit.rs:60`), "how long
  a new lease of the key is expected to last": the P90 of the runs' wall minutes, rounded
  up per run, at least 1. The registry maps a run to its key by `Registry::profile_of`
  (`crates/js-registry/src/registry.rs:862`). `js-registry` depends on `js-usage` for the key type
  only (`Cargo.toml:18`; `ProfileKey` at `registry.rs:12`, `runs.rs:23`); it knows the key but
  not the store, so the estimate is fed in from the lease server's `NodeUsage`.
- **[code]** A lease carries no profile key; a run does (`runs.rs:475`, from `queue_keyed_run`,
  `registry.rs:795`, called by the lease server at `run_api.rs:337`). A lease's estimated end is
  therefore its start plus the largest `wall_minutes_p90` among the keyed runs queued on it, and it is
  unknown until a keyed run exists. The registry stores the end (`record.bound`), not the start, so
  the feed needs a started-at on the host record beside `bound` **[proposed]**.

So "js owns the rule in `Scheduler::place`" (earlier text) was wrong for the lease server: a rule
placed only in `js-sched` would change the embedded kbf core, which is where kbf#169 happens (2.1),
and nothing the lease server does. The registry decides for `WholeMachine` only.

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
   `js-sched` (3.4) and takes the clamp and the unknown-last rule from the same `tail()`.
2. **Reserve** the worker with the least tail among those that could hold the request.
   The pick is made again every round with no stickiness: ties go to the lower name, so a
   steady state does not flap. A reservation moves when another worker's tail becomes the
   least, including when the reserved worker's tail grows. The reservation keeps its
   original `made` time across moves, so the queue-age order of the caps does not reset
   (`Drain::mark` drops `made` on a worker change today; it must carry it).
3. **Backfill.** An operation *after the large request in queue order* is placed on the
   reserved worker only if its estimated end, plus a `margin` kept spare, is no later than
   that worker's start time (`fits_in_tail`, `cost.rs:97-109`). The spare margin is a change from the
   rule as first written ("no later than the start time"). Otherwise it goes elsewhere or waits. **[code]** (jobscheduler#613, `admission/head.rs` `start_time`) The start time holds the measured load constant at its latest reading, so when the real measured load falls the real start can come earlier than the start time; backfill admitted against the start time can then delay the large request by at most that gap, and never past the start time itself. Operations *ahead* of the large request in queue
   order are not held back (3.3).
4. **Start.** The large request starts when the start time comes, not when a timer fires.
5. **No timer.** No `head_hold_after`, no `blocked_since`, no fixed hold, and no give-up
   deadline for this hold.

### 3.2 Owner: Decision: Path S (approved 2026-11-01)

What is common to every path (and is the first step, 3.6): **the tail rule is written
once, in `js_whole::cost`**, as pure functions, and the engines' ranking reads it.

The shape below is the first implementation step as built (jobscheduler PR #597, head
`6b7ba4641c65ff4ebd20d26aa922066b2317045b`; line numbers are `crates/js-whole/src/cost.rs` there).

- **[code]** `HostLoad` gains `est_ends: Vec<Option<u64>>`, in the order of `end_bounds`
  (`cost.rs:18`); an empty list, `None`, or an entry past the end is a lease with no estimate.
  `HostLoad` is a `pub` struct that derives `PartialEq, Eq` and is built by struct literal at
  `crates/js-registry/src/registry/start.rs:44` and `crates/js-sched/src/scheduler/whole.rs:310`
  (at `e23a2b4c`), so step 1 edits both call sites to pass `Vec::new()`; neither engine reads it yet.
- **[code]** `tail(load: &HostLoad) -> Option<u64>` (`cost.rs:83`) is the host's tail as an absolute
  time: the latest over its leases of each lease's end, where a lease's end is its estimate cut to
  its max-wall bound, else the bound, else unknown (`lease_end`, `cost.rs:67`). A host with no
  lease has tail zero; `None` when some lease has neither an estimate nor a bound. It takes no
  `now`, no `margin` and no `ceiling`.
- **[code]** `DrainCost.tail_end: Option<u64>` (`cost.rs:32`) carries it, and
  `wait_ms = tail_end.unwrap_or(u64::MAX).saturating_sub(now)` (`cost.rs:48`).
  `drain_cost` sorts by (tail unknown last, `wait_ms`, leases, id) (`cost.rs:54-57`).
- **[code]** `fits_in_tail(now, estimated_duration: Option<u64>, tail_end: Option<u64>, margin: u64)`
  (`cost.rs:97-109`) is `now + duration + margin <= tail_end` with saturating sums, and fails
  closed when the duration or the tail is unknown. The margin is applied only here;
  `DEFAULT_BACKFILL_MARGIN_MS` is one minute (`cost.rs:63`).
- **[proposed]** A reservation takes the tail once, when it sets the mark, and keeps that value
  (the doc comment at `cost.rs:81-82`): work admitted later cannot extend it. The mark stores
  `tail_end` (the tail-record item, 3.6).

**Decision: Path S (approved 2026-11-01).** `js-sched` becomes the lease server's
placement engine; `js-registry` stays the registry of hosts. Concretely:

- The lease server drives `js-sched` for placement. It wires up
  `crates/js-lease-server/src/sched/map.rs`, which is unused today (2.4): the server builds a
  `Scheduler` and places runs through it, so `js-sched` places ordinary runs and the whole
  rule of 3.1 to 3.4 (start time for a partial request, backfill in `fit`, one hold per pool)
  applies to the farm as written.
- `js-registry` stays the host registry (hosts, their facts, claim and regain handling,
  the D55 record and the hosts API). Its whole-node reservation (`whole/reserve.rs`) and
  its start and wait path are replaced by the `Scheduler` (3.6, steps 5 to 11).
- The booking model changes: the registry books each host as one whole shape with at most one
  lease (`start.rs:44-47`); `Scheduler` workers hold many leases.
- The costs, each a step in 3.6: the booking-model change (step 5), the placement shell that
  consumes `map.rs` (step 6), the flag (steps 6 and 12), and moving the registry's start and wait
  path onto `Scheduler` while the D55 and `drain_deadline_ms` contracts hold (step 11).

**Path R, the considered alternative, not chosen.** Path R kept the rule in `js-whole` with
`js-registry` as the live engine: the registry's pick (`reserve.rs:403`) and start gate
(`reserve::may_start`, `:245`) would read the tail, with estimates fed from the `ProfileStore`
behind a flag, and `js-sched` as a second user. It was not chosen for three reasons from this
note: R gives the farm the ranking only (the backfill gate is vacuous on the registry's
one-lease-per-host booking, 4.14); kbf#169 is not fixed under R until the `js-sched` change
anyway, because the 24-of-32-core action places through the embedded core (2.1); and the
embedded core's #169 case needs `js-sched` under either path, so R leaves two placement
engines with two reservation rules, where S leaves one.

kbf owns only the flag default, the both-core test, and the wiring of kbf step 0 (passing the
submission's `max_wall` to the js core). No kbf scheduling code.
### 3.3 Limits, accepted and tested

- **Work ahead in queue order.** `reserved` is built during the queue walk and is empty at
  the start of each round, so rule 3 binds only operations after the large request.
  Older work ahead of it (and any higher-QoS work) still lands on the reserved worker.
  For the same QoS this is a finite set: every later arrival has a higher operation id
  and is behind the large request, so the tail cannot be extended forever by older
  same-QoS work. A steady higher-QoS stream still starves it; that is decision 4.3. We
  accept both and test both (3.7 e and f). Applying rule 3 to work ahead needs last
  round's reservation as an input to the walk; not built (decision 4.9).
- **Overdue leases.** `wait_ms` saturates at zero (`cost.rs:48`, as `cost.rs:39` did), so a lease
  past its end reads zero remaining and a host whose only lease is overdue ranks as free. That is
  how step 1 is built, and it is a weaker rule than the one this note first proposed (a
  floor of `margin` on each lease's remaining time, so a still-busy worker never looks free). What
  stays: `fits_in_tail` keeps `margin` between backfilled work and the tail's end (`cost.rs:106`), and
  the tail is taken at the mark and kept. Whether the pick also needs the floor is **[verify]**: an
  overdue lease past its enforced end still holds the worker until the next lease tick
  (js-lease) or, in the embedded core, until the kbf action timeout ends it. If the red run
  (tests (g), (p)) shows a mark landing on a host that is not in fact free, the floor is added to the
  pick as its own change, in the engine that has the tick. The kind of the enforced end (per core)
  is as in 3.5.

### 3.4 Mapping onto the code that exists

The bullets below describe `js-sched` (the embedded scheduler, and under Path S the farm's
placement engine). The `js-whole` and `js-registry` parts are in 3.2
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
  an unknown tail ranks last (`cost.rs:55`); for backfill, unknown is refused (`fits_in_tail`
  fails closed, `cost.rs:103-106`). In `js_whole::cost` an estimate is also cut to its lease's
  bound (`cost.rs:67-73`). The wall `ceiling` above which a bound is refused for backfill is not a
  parameter of `tail()`; it is applied by the caller, which treats such a bound as no bound. In js-sched it is **new
  configuration**, fed by kbf (js-sched does not depend on js-lease, and
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

1. **`js-whole`: the tail rule, alone** (as built, 3.2). `crates/js-whole/src/cost.rs`:
   `HostLoad.est_ends`, `tail()`, `DrainCost.tail_end`, `drain_cost` sorted by tail, and
   `fits_in_tail`; both `HostLoad` call sites pass `Vec::new()`. Pure functions with unit tests,
   red first: two hosts, A with the longer max wall and the shorter estimated tail, B the
   reverse, and `drain_cost` must put A first (red before the change: `cost.rs:38` at
   `e23a2b4c` ranks by the bound). **This step keeps today's order for input with no
   estimate**, and the other tests are:
   - an unbounded case (the existing tests lack one): host A with a `u64::MAX` end bound and host
     B with a finite one; B ranks first and A's `wait_ms` is `u64::MAX - now`; two unbounded
     hosts order by leases, then id;
   - an estimate past the bound is cut to the bound (`an_estimate_past_the_max_wall_is_cut_to_it`);
     `tail()` of an empty host is zero (`once_the_running_leases_are_gone_the_tail_is_over`,
     `cost.rs:411`);
   - `fits_in_tail` fits up to the tail's end less the margin and not a millisecond more, never
     fits an unknown duration or an unknown tail, and its sum saturates
     (`cost.rs:424-442`);
   - a **characterization test** against the pre-change `drain_cost` as an oracle **[code]**:
     `with_no_estimates_every_old_case_gives_the_old_order_and_waits` (`cost.rs:238`) runs the pre-tail
     cases at several clocks through `drain_cost_before_tail` (`cost.rs:213`, the old function kept
     verbatim, returning `(id, wait_ms, leases)`) and through the new `drain_cost`, and requires the same
     id, `wait_ms`, leases and order; `tail_end` is checked on its own. It is green on the old logic once it
     compiles, so its red is the compile failure until the new fields exist; afterwards it catches any
     change to the order or the wait of input with no estimate.
   The existing goldens (the hosts-API field and the D55 record rows; D55, W6 and W14 are
   decisions in jobscheduler `docs/design/whole-node.md`) are unchanged: nothing is written or
   exported by this step. Neither engine reads the new field yet, but both call sites are
   edited. The weaker overdue rule (3.3) and the `ceiling` handling arrive with the step that passes real
   values (step 7 for the lease server's feed, step 9 for the embedded core), tied to tests (g)
   and (d); `ceiling` handling is the caller's.

**Steps after the first are Path S (3.2).** Every step is **[proposed]**. Each names its
repo, what it changes, the test that proves it (written first, red before the change), and
what it depends on. Test letters are those of 3.7.

2. **[proposed] kbf: carry and enforce `max_wall` on the js core (kbf step 0).** kbf passes
   the submission's `max_wall` in `SubmitOptions` (js-server-core already applies it), fixes
   the stale `hot.rs` comment, and a lease past its wall is ended by the action timeout.
   Test (red today): a js-core job whose `max_wall` passes is ended and its worker's tail ends
   with it. Depends on: nothing. Backfill on the js core is not enabled before this. The
   lease-server path already enforces its bound in js-lease (3.5) and needs no such step.
3. **[proposed] kbf: make load placement the only mode (kbf step 1).** Flip `--placement-load`
   to `on` (`crates/kbf-server/src/config.rs`), add the both-core #169 test through `Farm`
   (`SchedulerCore::Legacy` and `Js`, default flags): 60 small actions, then two booking 24 CPUs /
   24 GiB, then 200 small and six booking 4 CPUs / 40 GiB on 32- and 28-core workers; assert
   every large action starts before the small queue is empty. Red under the `off` default
   **[verify]** whether it is already green under `on` (nobody has run it, 2.2); then remove
   `off`, which is plumbing through about thirty files (`farm.rs:272`, `farm.rs:358`,
   `farm/driver.rs:192` and `:94`, `crates/kbf-estimator-replay/src/replay/sim.rs:186`,
   `crates/kbf-server/src/farm/js/tests.rs:95,128`, and the fixtures that pass
   `PlacementLoad::Off`). Depends on: nothing.
4. **[proposed] jobscheduler: `js-sched` takes the tail rule and one pick** (jobscheduler#582).
   `drain_order` reads `tail()`; the three picks (`hold_whole`, `Servable::reserve`,
   `hold_for_head`) become one, by least tail, with the start time of a partial request
   read against `Servable::admitted` (3.1, 3.4); `drain_order` stops requiring whole-node
   settings; the pick is non-sticky and keeps `made`; ordinary large actions stay in
   `head_holds`, one per pool. Tests, red first: (a) two workers, short and long tail, the
   request reserves the short one; (b) the #169 start time is when 24 cores free up; (h), (m),
   (i), (n), (o), (q), (r). Depends on: step 1.
5. **[proposed] jobscheduler: the booking model, one shape with at most one lease becomes many
   leases per host.** The registry's booked table (`state.book.booked()`, `start.rs:25`) and
   `host_facts` (`start.rs:44-47`) stop assuming one lease per host, so a host maps to a
   `Scheduler` worker with many leases through `sched/map.rs`. Tests, red first: a host with
   room starts a second lease (refused today); the hosts-API and D55 goldens are unchanged;
   **[verify]** by reading `book.start_where` what else assumes one lease. Depends on: step 4.
6. **[proposed] jobscheduler: the placement wiring.** The lease server drives `js-sched` for
   placement through `crates/js-lease-server/src/sched/map.rs`: it builds a `Scheduler` from
   the registry's hosts and places runs through it, behind a flag that defaults off; flag off
   keeps the registry's path. Tests, red first: with the flag on a run is placed by
   `Scheduler` (today nothing in the server calls `sched/map.rs`); the #169 shape (a
   24-of-32-core request among small ones) holds a worker and starts through the lease
   server; flag off is byte-identical to today. Depends on: steps 4, 5.
7. **[proposed] jobscheduler: the estimate feed, behind a flag; runs in parallel with step 6.**
   A lease record gets a started-at beside `bound`; a lease's estimate is its start plus the
   largest `wall_minutes_p90` of its keyed runs from `NodeUsage`'s `ProfileStore` (2.4), unknown
   until a keyed run exists, the declared bound as the fallback. Flag off passes no estimates,
   as step 1. Tests, red first: the record round-trips started-at; A has the longer max wall
   and the shorter estimated tail, and with the flag on the mark lands on A; a lease with no
   keyed run is unknown. Depends on: steps 1, 5.
8. **[proposed] jobscheduler: the evaluation counters; runs in parallel with step 6.** The mark
   stores `DrainCost.tail_end` with the estimated end that produced it and the request's later
   start; the scheduler counts backfill share and estimate error (3.8, section 5). Test, red
   first: (l) a request that starts and one that is cancelled each emit one record with the
   estimated start and backfilled core-seconds (a move only increments its counter). Depends
   on: steps 1, 4.
9. **[proposed] jobscheduler: the `fits_in_tail` backfill gate where `js-sched` places work on
   a marked host.** A later operation is placed on the reserved worker only if `fits_in_tail`;
   the start time is carried from the pick into `Servable::fit`; `bounds_enforced` is one
   per-core setting and `ceiling` is new configuration (3.5). With multi-lease hosts (step 5)
   behind the lease server (step 6) the gate bites, which it does not on the registry's
   one-lease booking (4.14). Tests, red first: (c) a job ending inside the tail backfills, one
   ending after does not, and the large job starts at the original time; (d), (e), (f), (g),
   (p) overdue, (s) `bounds_enforced` unset means no backfill. **[verify]** first that
   `Profile::timed`'s `life` is the P90 (3.5). Depends on: steps 2, 4, 6; and step 7 for learned
   estimates (the declared bound alone works without it).
10. **[proposed] kbf: bump the js revision** to the step 9 commit. Test: the both-core test of
    step 3 stays green, and the js-core `max_wall` test of step 2 enables backfill there.
    Depends on: steps 2, 3, 9.
11. **[proposed] jobscheduler: move the registry's start and wait path onto `Scheduler`.**
    `whole/reserve.rs`, `whole/queue.rs`, `whole/requests.rs` and `reserve::may_start` are
    replaced by the `Scheduler`'s holds; `js-registry` keeps the host table, claim/regain
    and the D55 record. Tests: the registry's whole-node tests are ported to the `Scheduler`
    and must still pass; the D55 and `drain_deadline_ms` goldens are unchanged (4.11).
    **[verify]** what `drain_deadline_ms` reports once the registry has no timer (4.12).
    Depends on: steps 6, 9, and a canary on the flag.
12. **[proposed] jobscheduler: flip the flag** to on, once the canary shows (section 5) wait
    versus tail estimate and no regression. Test: the step 6 tests with the default flipped.
    Depends on: steps 8, 11.
13. **[proposed] jobscheduler: remove the give-up** (the hold deadline, back-off, attempt
    count, and `drain_after_ms` as a wait) from the one engine, after the enforced bound (step 2),
    the re-pick (step 4) and the overdue case (4.4) are green. Tests: (j) a presented request is
    never given up, (k) the old flags are accepted and inert, (t) the hosts view and `whole_mark`
    metric still read `deadline` (4.11). Depends on: steps 2, 4, 9, 11.
- **Follow-on, not in this note:** persisting the scheduler's marks as an additive lease-file
  record.

### 3.7 Tests, written first [proposed]

The lettered tests are the 22-item plan of jobscheduler#582 (the 20 below plus kbf steps 0 and 1);
the letters and the numbering are unchanged. Some of them are unit tests below
`js-sched`, in `js-whole`, and the table says so; the rest live in `js-sched`
(`crates/js-sched/tests`), and a few once more through the lease server.

| tests | live in |
|---|---|
| (a) pick by least tail, (d) the unknown half (`a_host_with_an_unknown_lease_ranks_after_every_known_tail`, `cost.rs:341`), (g) and (p) the fits-in-tail half, (c) the backfill predicate | unit tests in `crates/js-whole/src/cost.rs`, on `tail()`, `drain_cost`, `fits_in_tail`; (a) and (g) also once through the lease server (the A/B red test of step 7) |
| (d) the ceiling half: a bound over `ceiling` is treated as no bound | the caller's tests (lease server, `js-sched`); `tail()` takes no ceiling (3.5) |
| (c) a started backfill, (s) `bounds_enforced` | `js-sched` (step 9); once more through the lease server after the wiring (step 6) |
| (h) moves keep `made`, (m) cap at `max_draining = 1`, (j)-(k) give-up and compatibility | `js-sched` (`Drain`, `hold_whole`). The registry's whole-node tests are ported with step 11 (4.12) |
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
  the original start time (with the measured load constant). (d) The ceiling: a declared bound over `ceiling` is refused for
  backfill, an unknown estimate is refused, and the cases on each side of the ceiling
  behave as stated; with fallback-only estimates a job with a shorter bound than the
  remaining tail backfills and one with an equal or longer bound does not. (e) Older same-QoS
  work ahead of the large request lands on the reserved worker, and the large job starts
  after it but before any later arrival. (f) A lower-QoS large request waits out a
  higher-QoS stream and starts when it stops. (g) Overdue: `fits_in_tail` admits no backfill
  on a tail that is past (`now + duration + margin <= past end` is false, `cost.rs:106`), which holds
  today; and, if the pick-side floor is built (3.3), a worker whose lease is past its estimate is not
  preferred over a worker with a real shorter tail. (h) The reservation moves when the reserved worker's tail
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
   (3.2) and replaces the upper-bound rank of `drain_cost`, the
   binding-ratio pick of `hold_for_head` and the fewest-leases pick of `Servable::reserve`. The earlier
   recommendation to hold by load ratio is withdrawn.
3. **No aging across QoS (recommended, accepted limit).** A lower-QoS large request can
   wait as long as a higher-QoS stream lasts; aging would turn QoS from a priority into a
   weight. Tested by 3.7(f).
4. **No hold timer (decided, conditional on M1, M2 and M4).** `head_hold_after`,
   `blocked_since` and the wait before a hold are not built. The give-up of a presented
   request (`deadline`, the back-off, `max_reserve_attempts`, `drain_after_ms`) is
   removed in the give-up removal step (3.6), after the enforced `max_wall` (kbf step 0), the non-sticky re-pick
   (4.9) are tested and the overdue case is closed: either the pick-side floor of 3.3 is built, or the
   red run of tests (g) and (p) shows that no mark lands on a host that is not free.
   `fits_in_tail` already refuses backfill on an overdue tail (`cost.rs:106`), and that part holds
   today. These replace what the give-up did, which
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
   rule. Backfill uses the maintainers' test only (ends inside the start time); the extra case
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
      `drain_deadline_ms` (with its golden) and its own timer until step 11 (4.12), so D55, the wire
      and the golden do not change in this work.
    - `WholeMark.deadline` is a public field and a view column (`whole_view.rs`, read by
      kbf's `whole_mark` metric and `docs/api.md`). It is kept, always absent after the give-up
      removal, so the view and the metric do not break; removing it is a later, separate
      change.
    - Docs amended with the give-up removal: `whole-node.md` and `configuration.md` where they describe
      the embedded scheduler's give-up; W6/W14 are about the registry and stay.
12. **The registry stays the host registry; its whole-node reservation moves into `js-sched` (decided with Path S).**
    `crates/js-registry/src/whole/reserve.rs` has its own marks, `keep_or_move` (`:351`),
    caps and give-up. Under Path S step 11 replaces them with the `Scheduler`'s holds, so
    the tail rule, the backfill test and the removal of the give-up (step 13) apply in one engine.
    The registry keeps the host table, claim/regain handling, the D55 record and the hosts
    API field (4.11). **Open [verify]:** what `drain_deadline_ms` and the D55 `deadline` carry once
    the registry has no timer; the answer must leave the goldens unchanged (step 11 reads the
    code first). Persisting the scheduler's marks stays follow-on work.
13. **Metrics are recorded, not inferred (recommended).** 3.8.
14. **Backfill on the registry is vacuous today; under Path S it bites (decided; verify the booking first).** A registry
    host holds one booked lease (2.4), so a marked host that is draining has no room for
    another start, and a marked host that is quiet has tail zero, where `fits_in_tail` is
    false for any candidate of positive length. With the booking model changed to many leases per
    host (step 5) and `js-sched` placing (step 6), the gate of step 9 bites. **[verify]** by
    reading the booking in `book.start_where` and by the step 5 test.
15. **Where the rule is placed: Path S (decided, approved 2026-11-01).** 3.2.

## 5. What it measures (for the evaluation)

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
