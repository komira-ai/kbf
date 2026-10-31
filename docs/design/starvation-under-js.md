# Starvation of a large booking by small actions: who owns the rule

Status: DESIGN, for review. Nothing here is built. Issue: komira-ai/kbf#169. Revised to the whole-node ruling: reserve the host with the least tail.

Each claim is marked **[code]** (read from the named file and function, at the kbf
`main` and the jobscheduler `main` that this note was written against) or **[proposed]**.
"js" below means `crates/js-sched` in komira-ai/jobscheduler.

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
  - The comment on `Scheduler::place` (`scheduler.rs`, "No other operation holds a
    worker") is stale: the code holds a worker for a blocked head and for an
    `owns_worker` action as well as for whole-machine requests.
  - No simulation covers ordinary actions. `crates/js-sched/tests/sim_whole_machine.rs`
    asserts a wait bound (`CI_BOUND`) for whole-machine leases only. I found no kbf test of
    the #169 shape.

### 2.3 What the whole-node path already does (it is most of the rule)

- **[code]** A `LeaseKind::WholeMachine` request that fits nowhere holds a worker
  (`crates/js-sched/src/scheduler/whole.rs`: `Scheduler::hold_whole`, `WholeMark`,
  `Drain`; called from `place`). With whole-node settings on, the worker is the first of
  `Scheduler::drain_order` that could run it. `drain_order` builds one `HostLoad` per
  worker whose `end_bounds` are each held lease's `started_at` (else `granted_at`) plus
  the lease's `max_wall`, and `js_whole::cost::drain_cost` sorts hosts by
  `wait_ms = max(end_bounds) - now`, then fewest leases, then name. A lease with no
  `max_wall` counts as ending at `u64::MAX` (`set_max_wall` says so). That is already
  "the host with the least remaining time", with the declared bound as the estimate.
  Without whole-node settings, `Servable::reserve` takes fewest leases, then least booked.
- **[code]** Today's drain mark is a reservation of the whole worker: its name is added to
  `reserved`, so nothing after it in queue order is placed there. It is the right
  object, but it has a clock: `drain_after_ms` before it is made, a `deadline`
  (`reserve_deadline`) after which it is given up and the worker barred for
  `reserve_backoff_ms`, up to `max_reserve_attempts`. It is in memory only.
- **[code]** The #169 request (an `Action` booking 24 of 32 cores) never reaches this
  path: `place` calls `hold_whole` only for `WholeMachine`, `Servable::reserve` only for
  `owns_worker()` actions, and `hold_for_head` (load ratio) for a `Blocked::Load` head.
  Three holds, three pick rules.

## 3. The rule [proposed]

### 3.1 Statement

A request is **large** when it is queued, fits no worker now, and some worker could hold
it once its leases end. The `WholeMachine` case and the #169 case are alike.

1. Among the workers that could hold a large request, reserve the one with the least
   **tail**: the estimated time until its current leases have all ended (the latest
   estimated end among its leases, minus now).
2. Work that would land on the reserved worker is placed there only if its estimated end
   is no later than the reserved worker's tail end. Work that would extend the tail goes
   elsewhere or waits.
3. The large request starts when the tail ends (the worker has room), not when a timer
   fires.
4. No head-hold timer and no fixed hold: `head_hold_after` and `blocked_since` are not
   built, and the large-booking hold has no give-up. The reservation is recomputed each
   round (ties go to the worker already reserved, then name), so it follows the
   estimates when they change.

### 3.2 Owner

**js owns the rule, once, in `Scheduler::place`.** kbf owns only the flag default and the
both-core test. No kbf scheduling code.

### 3.3 Mapping onto the code that exists

- **Which worker.** One pick rule for `WholeMachine`, `owns_worker` and `Blocked::Load`
  heads: `drain_cost`'s order (`wait_ms`, leases, name) over the workers that could run
  the request. Today only the first is chosen that way, and only with whole-node settings
  on; `drain_order` must stop requiring them. `hold_for_head`'s binding-ratio pick and
  `Servable::reserve`'s fewest-leases pick go away (decision 4.2).
- **Is the drain mark the reservation?** Yes: `reserved` plus the per-request record
  (`WholeMark`'s worker and `made`). What goes for this rule: `deadline`, the give-up
  count, `reserve_backoff_ms`, and `drain_after_ms` as a wait before holding (a hold is
  taken the round the request is blocked). What stays and is not this rule: the caps
  (`max_whole_node`, `max_draining`, `keep_for_ordinary`) on whole-machine requests.
- **Backfill inside the tail.** The reserved worker is no longer closed outright. In
  `place`, a later operation that `fit` would put on a reserved worker is allowed if
  `now + estimate(op) <= tail_end(worker)`, else that worker is skipped for it as now.
  This needs `drain_order`'s per-worker maximum end (it exists) carried into
  `Servable::fit` as a per-worker limit beside `reserved`, and one thing that does not
  exist: a lease's **estimated end**, `started_at` (else `granted_at`) plus an estimated
  duration, for the candidate as well as for the leases held. Today only the declared
  `max_wall` is known.
- **Cold start.** `drain_order` treats a lease with no bound as ending at `u64::MAX`, so
  every tail is infinite, the pick falls to fewest leases then name, and any candidate
  would "fit inside" an infinite tail. That is wrong for backfill; 3.4 gives every lease
  a finite estimate.

### 3.4 Estimates, and the cold start

Estimates come from historical tracking: a duration per key learned from recorded runs.
That is the work of jobscheduler#245 and jobscheduler#446 (profiles that survive the
store round trip, and a `predicted` that sees the parcel) and kbf#1950
(`--usage-profiles`, a profile source for the embedded js core). Whether a profile
already exposes a total duration was not verified; if not, a duration per key is part of
that work. Until a key has history:

- **Fallback [proposed, and it is a fallback]: the job's own declared bound.** The
  estimated duration of a lease is its `max_wall` (what `set_max_wall` already carries
  and `drain_order` already uses); with none declared, the pool's longest allowed wall
  time. Learned history replaces the fallback per key once the key has enough clean runs
  (the threshold jobscheduler#245 is about).
- One function `estimate(op)` serves tail and backfill, so the rule is correct with the
  fallback and sharper with history. With the fallback the tail is an upper bound: the
  reserved worker waits at most its longest bound, and backfill is conservative.
- An estimate too small lets a backfilled job run past the tail. That delays the large
  job; it never fails or kills a lease. It is measured (section 5).

### 3.5 Steps, in order

1. **kbf: make load placement the only mode, then observe.** One change:
   - flip `--placement-load` to `on` (`crates/kbf-server/src/config.rs`);
   - add the both-core #169 test through `Farm` (`SchedulerCore::Legacy` and `Js`, default
     flags): 60 small actions, then two booking 24 CPUs / 24 GiB, then 200 small and six
     booking 4 CPUs / 40 GiB on 32- and 28-core workers; assert every large action starts
     before the small queue is empty. Red today under the `off` default;
   - then remove `off`: `crates/kbf-server/src/farm.rs:272`, `farm.rs:358`,
     `farm/driver.rs:192` and `:94`, `crates/kbf-estimator-replay/src/replay/sim.rs:186`,
     `crates/kbf-server/src/farm/js/tests.rs:95,128`, and the daemon, integration and
     placement fixtures that pass `PlacementLoad::Off`.
2. **js (the jobscheduler issue linked from this PR): the tail rule** of 3.1 to 3.4, tests
   first (3.6).
3. **kbf: bump the js revision.** The both-core test stays green with the bump.

### 3.6 Tests, written first [proposed]

- kbf, step 1: the both-core #169 test. Red now.
- js: (a) two workers, one with a short tail and one long: the large request reserves the
  short tail and starts when it ends (the #169 shape; red today, the pick is by load
  ratio); (b) tail backfill: a job estimated to end inside the tail is placed on the
  reserved worker, one that would end after it is not, and the large job starts at the
  original tail end; (c) cold start: with no history the declared bound is the estimate,
  with none the pool maximum, and backfill never sees an infinite tail; (d) a lower-QoS
  large request waits out a higher-QoS stream and starts when it stops (pins decision
  4.3); (e) the tests of the deadline and give-up are removed with the code.

## 4. Decisions

1. **Load placement is the only mode (decided).** kbf's `--placement-load` defaults to
   `on` and the `off` path is removed; the CPU slack stays a setting (default 1.1x).
2. **One pick rule: the least tail (decided).** It replaces the binding-ratio pick of
   `hold_for_head` and the fewest-leases pick of `Servable::reserve`. The earlier
   recommendation to hold by load ratio is withdrawn.
3. **No aging across QoS (recommended, accepted limit).** A lower-QoS large request can
   wait as long as a higher-QoS stream lasts; aging would turn QoS from a priority into a
   weight. Tested by 3.6(d), not hidden.
4. **No hold timer (decided).** `head_hold_after`, `blocked_since` and the wait before a
   hold are out. Recommendation: delete `drain_deadline_ms`, `reserve_backoff_ms` and
   `max_reserve_attempts` for `WholeMachine` leases in the same js change rather than
   keep a second hold with a timer; keep the caps. The give-up was the only protection
   against a wrong estimate; the loss is bounded by the fallback (a hold waits at most
   the longest declared bound).
5. **One hold per pool, first (recommended).** Several large requests take their tails in
   turn: slower, never stuck by each other. Allow more only if the #169 run shows the
   serial waits are unacceptable on a few nodes.
6. **Cold start: the declared bound (recommended).** A fallback, replaced per key by
   history (3.4).
7. **VM leases (recommended).** A VM refused for room is a large request: give it the same
   hold, so the VM reservation planned in `macos-vms.md` is not built. If that is not a
   small widening of the same call, leave it for its own issue.
8. **Estimate source (recommended): not built here.** The js step uses `estimate(op)`
   with the fallback; learned estimates arrive through jobscheduler#245,
   jobscheduler#446 and kbf#1950 without changing the rule.

## 5. What it measures (for the paper)

- **Whole-node wait versus the tail estimate:** per large request, time from blocked to
  start against the tail estimated when it reserved. Report p50 and p99.
- **Backfill share:** the fraction of the reserved worker's core-seconds used by
  backfilled work during the wait (utilization of the reserved host while it drains).
- **Estimate miss:** actual tail end minus estimated tail end at reservation, and actual
  over estimated duration per backfilled job; cold-start fallback versus learned.
- The scheduler records per reservation: reserved-at, estimated tail end, actual start,
  backfilled core-seconds. They are exported as counters and a per-request record; no
  test asserts a time bound.
