# Starvation of a large booking by small actions: who owns the rule

Status: DESIGN, for review. Nothing here is built. Issue: komira-ai/kbf#169.

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
  - **No bound and no give-up.** The hold lasts while the request is queued and the worker
    could run it. Work *before* the head in queue order (a higher QoS, `queue` order) is
    still placed on the held worker, so a steady stream of higher-QoS work refills it and
    a lower-QoS head can wait for as long as that stream lasts. Nothing gives the hold up
    at any bound: whole-node has `drain_after_ms`, `drain_deadline_ms` and
    `max_reserve_attempts`; the head hold has none. On a one-worker pool, small work
    behind a head waits up to the longest lease on that worker.
  - The comment on `Scheduler::place` (`scheduler.rs`, "No other operation holds a
    worker") is stale: the code holds a worker for a blocked head and for an
    `owns_worker` action as well as for whole-machine requests.
  - No simulation covers ordinary actions. `crates/js-sched/tests/sim_whole_machine.rs`
    asserts a wait bound (`CI_BOUND`) for whole-machine leases only. I found no kbf test of
    the #169 shape.

### 2.3 Is the whole-node reservation the anti-starvation rule?

- **[code]** `mark: reserved`, `reserved_for`, `drain_deadline_ms` are the fields of a
  host in the lease server's hosts list (`crates/js-lease-server/src/hosts.rs`,
  `crates/js-lease-server/src/api/whole_view_tests.rs`). They report `Scheduler`'s whole-node marks
  (`crates/js-sched/src/scheduler/whole.rs: WholeMark`, `Drain`), made in `place` only for
  `request.kind == LeaseKind::WholeMachine`; the `else` arm reserves via
  `Servable::reserve` for `owns_worker()` actions. kbf exposes it as `--whole-node-max`
  and the `--whole-node-*` flags; it is off unless `--whole-node-max` is given.
- **[code]** It does have the shape asked for in #169: a wait bound
  (`WholeNodeConfig.drain_after_ms`, default 0 so at once), a deadline
  (`drain_deadline_ms`, default the pool's max wall time), a give-up with back-off
  (`reserve_backoff_ms`, `max_reserve_attempts`), caps (`max_draining`, `max_whole_node`,
  `keep_for_ordinary`), and a seed-swept bound test (`sim_whole_machine.rs`).
- **[code]** But it is a different request class. It is for a request that wants the
  whole machine, and it is keyed on `LeaseKind::WholeMachine`. The #169 request books
  24 of 32 cores and is an ordinary `Action`. Sending it through whole-node would give it
  a whole node it does not need and cap it with `max_whole_node`. It is the pattern to
  copy, not the rule that applies.


## 3. Proposal

### 3.1 Owner [proposed]

**js owns the rule, once, in `Scheduler::place`.** Both kbf cores call that function. kbf
owns only the flag default and a test, run on both cores, that holds the pinned js to the
behaviour. No kbf scheduling code.

### 3.2 Steps, in order [proposed]

1. **kbf: make load placement the only mode, observe first.** In one kbf change:
   - flip `--placement-load` to `on` (`crates/kbf-server/src/config.rs`, default
     `PlacementLoad::Off`);
   - add the both-core #169 test through `Farm` (`SchedulerCore::Legacy` and `Js`, default
     flags): 60 small actions, then two booking 24 CPUs / 24 GiB, then 200 small and six
     booking 4 CPUs / 40 GiB on 32- and 28-core workers; assert every large action starts
     before the small queue is empty. It is red today because the default is `off`, and
     goes green with the flip if the existing hold is enough.
   - then remove `off`. Places that name it: `crates/kbf-server/src/farm.rs:272` (built
     `Off`, overridden by `with_placement_load` at `farm.rs:358` and
     `farm/driver.rs:192`), `farm/driver.rs:94` (`Settings` default),
     `crates/kbf-estimator-replay/src/replay/sim.rs:186`,
     `crates/kbf-server/src/farm/js/tests.rs:95,128`, and the other test and daemon
     fixtures that pass `PlacementLoad::Off` (`kbf-daemon/tests`, `kbf-integration-test`,
     `kbf-node/src/startup_tests.rs`, `farm/js/tests/ablation.rs`, `placement.rs`,
     `farm/placement_tests.rs`, `farm/node_load/tests.rs`). `Off` is documented in js as a
     rollback switch only; it can go in js later.
2. **Only if the test of step 1 is still red under `on`: change js.** Then the gap is a
   refusal that takes no hold. Fold `Blocked::ColdCap` and VM refusals into the head hold
   (give `fit` a reason for a VM refused for room, and hold on any `Some` reason). Nothing
   else changes: same hold, same one-per-pool, same queue order.
3. **Only if a run under `on` shows hold churn** (a worker held, then the request fitting a
   heartbeat later; visible as `kbf_sched_load_reservations_total` rising faster than
   starts): add a wait before the hold (`head_hold_after`, with a per-request
   `blocked_since`, in memory). Not proposed now: it adds state and a time knob over
   today's hold-at-once, and I recommend against it until a run shows the churn.

### 3.3 What the hold does and does not guarantee [code for current, proposed for tests]

The hold makes a head start once the leases on its worker end, provided nothing *ahead of
it in queue order* keeps refilling that worker. It is a mechanism for starvation inside
one QoS class, not a bound: it has no give-up at any time (2.2), and a steady stream of
higher-QoS work starves a lower-QoS large booking. Decision 4.3 accepts that.
So no wait bound is claimed. Instead of a bound assertion, a test documents the limit:

- in `crates/js-sched/tests/admission_place.rs`, a `batch` head blocked while `ci`
  actions keep arriving ahead of it is not started while the stream lasts, and starts when
  it stops. The test is named for the limit ("a lower-QoS head waits out a higher-QoS
  stream") so a later aging or give-up rule must change it on purpose.
- on a one-worker pool, small work behind a head waits for the longest lease on the worker
  (a unit test with one worker).

### 3.4 Tests, written first [proposed]

- kbf, step 1: the both-core #169 test above. Red now, green after the default flips (or
  red under `on` too, which selects step 2).
- js, step 2 only: a VM lease and a cold-cap refusal behind actions take the hold; each
  fails today.
- js, 3.3: the two limit-documenting tests; they pass today and pin the behaviour.
- A seed sweep of the #169 shape beside `sim_whole_machine.rs` is useful to measure the
  start times, but asserts no bound.

### 3.5 What kbf#169 becomes [proposed]

Keep kbf#169 as the tracking issue for step 1: it closes when the default flip and the
both-core test land and the test is green. If step 1's test stays red, open a jobscheduler
issue for step 2 and keep #169 open for the revision bump. It is not "superseded":
`--scheduler-core js` does not change the behaviour (2.1).

## 4. Decisions

1. **Load placement is the only mode (decided).** With the CPU slack a setting (default
   1.1x), kbf's `--placement-load` defaults to `on` and the `off` path is removed, not
   supported. The first kbf step is the flip plus the both-core test (3.2 step 1); the
   widening of `Fit.blocked` for `Off` is dropped with the path. Whether the existing hold
   already cures #169 is observed, not assumed.
2. **The held worker: by load ratio, as today (recommended).** The worker that empties
   soonest needs remaining-life data that js has only with learned profiles. Revisit when
   profiles give remaining life.
3. **No aging across QoS (recommended, accepted limit).** A lower-QoS large booking can
   wait as long as a higher-QoS stream lasts; aging would turn QoS from a priority into a
   weight. Documented by the test of 3.3, not hidden.
4. **One hold per pool, first (recommended).** Several large requests pay their drains in
   turn: slower, never stuck by each other. Add more holds per pool only if the #169 run
   shows the serial drains are unacceptable on a few nodes.
5. **VM leases (recommended).** Fold into the hold only if step 2 happens (one mechanism
   for every refusal); then the separate VM reservation in `macos-vms.md` is not built and
   that document points here. Otherwise leave as is.
6. **A wait before the hold (recommended: do not build).** `head_hold_after` and
   `blocked_since` only if a run under `on` shows hold churn (3.2 step 3).
