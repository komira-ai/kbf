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

- **[code]** `kbf-server/src/farm.rs: Farm::new_with_core` builds the legacy core with
  `Scheduler::new(term).with_load_config(PlacementLoad::Off.config())`, where `Scheduler`
  is `kbf_sched::Scheduler`. `kbf-sched/src/lib.rs` is `pub use js_sched::*`, at the
  revision pinned in the workspace `Cargo.toml`.
- **[code]** `kbf-server/src/farm_js/open.rs` builds the js core's scheduler with
  `.with_load_config(placement.config())`, the same `PlacementLoad::config()`
  (`kbf-server/src/placement_load.rs`).
- **[code]** `--placement-load` (`config.rs`, default `off`) is read under either
  `--scheduler-core`. `placement_load.rs` says kbf passes an explicit load config so
  that a library bump cannot change placement.

So the legacy core has no placement code of its own. Both cores place with js's
`Scheduler::place` (`js-sched/src/scheduler.rs`) and `Servable::fit`
(`js-sched/src/servable.rs`). They differ in the driver around it (`farm/driver.rs`:
`Driver::Legacy` vs `Driver::Js`), not in the rule that decides who waits.
The premise "load placement is js's default" holds for the library
(`js-sched/src/admission/config.rs: PlacementLoad` defaults to `On`, `Off` is documented
as a rollback switch), **not for kbf**: kbf defaults to `Off` on both cores.

### 2.2 What happens to a large request behind small ones

`Scheduler::place` walks the queue in order `(Reverse(Qos), OperationId)` (field `queue`),
so urgency first, then age. For each operation it calls `Servable::fit`.

- **`--placement-load off` (kbf default, either core). [code]** `fit` computes
  `by_load = adm.by_load() && kind == Action`, which is false; candidates are filtered by
  `Worker::has_room(request, false)` (free booked CPUs must hold the request), the least
  booked worker is taken, and `Fit.blocked` stays `None`. In `place`, the head hold is
  taken only `if blocked == Some(Blocked::Load)`, so nothing is held. A 24-CPU action
  that fits no worker now is skipped, and every later small action that fits is placed.
  Because the stream refills a node as fast as leases end, the large request starts only
  when the stream thins. This is the behaviour reported in #169. (That the report was
  taken with `off` is my inference from "no reservation for action leases"; nobody has
  re-run it for this note.)
- **`--placement-load on`. [code]** `fit` returns `blocked: Some(Blocked::Load)` when
  candidates exist but the load predicate refuses all (`Servable::admitted`). `place`
  then calls `Servable::hold_for_head`: the request holds the worker with the smallest
  binding ratio (sticky within a tenth, `pick`), one hold per pool (`held_pools`), added
  to `reserved`, so operations after it in queue order are not placed there and the
  worker empties as its leases end. Work before it in queue order is still placed.
  The hold is recomputed every round, in memory, and counted in
  `kbf_sched_load_reservations_total`. Tests:
  `js-sched/tests/admission_place.rs: a_blocked_head_holds_a_worker_and_starts_when_it_empties`,
  `work_ahead_of_the_head_is_never_held_back`,
  `one_hold_per_pool_and_the_second_head_waits_its_turn`. So the head hold is the
  anti-starvation rule for ordinary actions, and it exists, but only under `On`.
- **Not covered under either mode. [code]**
  - `Blocked::ColdCap` (only the cold-start cap refused) takes no hold; a request that
    is blocked on a worker that merely has no room by the booked rule (`Off`) has no
    `Blocked` value at all.
  - A VM lease is never given `blocked` (the `vm_by_load` branch of `fit` only filters
    candidates), so a VM waits behind actions too; `Worker::has_room` says "nothing
    reserves a node for a queued VM". `macos-vms.md` section on reservation for VM
    leases describes this as planned.
  - There is no wait bound: under `On` a request holds a worker the first round it is
    blocked, even if it would have fitted a heartbeat later (idle capacity for nothing),
    and no bound on how long a hold may last (it ends when the leases on the held worker
    end; the worker is chosen by load ratio, not by soonest to empty).
  - The doc comment on `Scheduler::place` says "No other operation holds a worker" besides
    whole-machine requests, which the code contradicts (`owns_worker` actions and blocked
    heads hold). The comment is stale.
  - No simulation covers ordinary actions. `js-sched/tests/sim_whole_machine.rs` asserts a
    wait bound (`CI_BOUND`) for whole-machine leases only; the head hold has unit tests,
    no seed sweep. I found no kbf test of the #169 shape (a grep of `kbf-server` for the
    hold and for head reservations finds only the metric).

### 2.3 Is the whole-node reservation the anti-starvation rule?

- **[code]** `mark: reserved`, `reserved_for`, `drain_deadline_ms` are the fields of a
  host in the lease server's hosts list (`js-lease-server/src/hosts.rs`,
  `api/whole_view_tests.rs`). They report `Scheduler`'s whole-node marks
  (`scheduler/whole.rs: WholeMark`, `Drain`), made in `place` only for
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

**js owns it, once, in `Scheduler::place`.** Both kbf cores call that function, so a rule
there reaches both with no kbf scheduling code. A kbf-side rule would be a second way to
decide who waits,. kbf owns only (a) the
flag default and wiring and (b) a test, run on both cores, that holds the pinned js to
the behaviour.

### 3.2 The rule [proposed]

A request is *blocked* when it is servable (some live worker could run it once its
bookings end) but `fit` placed it nowhere this round, for any reason: booked CPUs under
`Off`, load under `On`, cold cap, VM room. (Today `Fit.blocked` is `None` under `Off`
and for VMs; widen it so every refusal reports a reason, with a booked/VM reason added to
the enum.)

1. The scheduler records the first round at which each queued request was blocked
   (`blocked_since`, in memory like the holds; recovered as "now" after a restart, which
   only delays a hold).
2. Once `now - blocked_since >= head_hold_after`, the request takes the head hold that
   `hold_for_head` takes today: one worker per pool, later operations in queue order
   kept off it, earlier ones not. Same stickiness.
3. The hold lasts while the request is queued and the worker could run it, as today.
   For choosing the worker, under `Off` use the most free booked CPUs (see question 2).
4. Queue order stays `(QoS, age)`. A `batch` request still waits behind `ci` work: that
   is priority, not starvation. No aging across QoS classes (question 3).

This is the #169 expectation ("once the oldest waiting operation has been passed over for
a set limit, hold node capacity for it, ordered by QoS") stated on the existing mechanism,
not a second one. `head_hold_after` replaces today's implicit zero under `On`.

### 3.3 Parameters [proposed, to be set by the simulation, not by taste]

- `head_hold_after`: start at 10 s (two heartbeat windows of the default 5 s) so a
  momentary block does not idle a worker; it must be at most the wait budget an operator
  would accept for a large action.
- Bound the sweep must demonstrate: a held request starts within `head_hold_after` plus
  the longest lease on the held worker at the time of the hold, plus one placement round.
- Exposed as a js `Scheduler` setting; in kbf a flag only if a deployment needs another
  value (configuration is flags), default the js default.

### 3.4 Tests, written first [proposed]

In jobscheduler (red before the change):

1. `admission_place.rs`: the `Blocked` fixture of
   `a_blocked_head_holds_a_worker_and_starts_when_it_empties` run with
   `PlacementLoad::Off`. Fails today: no hold is taken, small work lands on the worker.
2. A request blocked for less than `head_hold_after` holds nothing and is placed if room
   appears; at the bound it holds. Fails today under `On` (holds at once).
3. A VM lease behind actions takes the same hold.
4. A seed sweep next to `sim_whole_machine.rs`: two workers (32 and 28 cores), the
   shape of #169 (60 small, 2 large at 24 CPUs, 200 small and 6 more large), asserting
   every large start is within the bound in 3.3, no booking is exceeded, and the trace
   replays per seed. The sweep fails today under `Off`.

In kbf (after the js revision is bumped), in `kbf-server` next to
`farm/js/tests/exclusive.rs` (which already loops `CORES_UNDER_TEST`): the #169 shape
through `Farm` on both `SchedulerCore::Legacy` and `SchedulerCore::Js` with default flags,
asserting the large actions start before the small queue is empty. This is the test that
cannot be satisfied by a kbf-only workaround.

### 3.5 What kbf#169 becomes [proposed]

Retarget the rule to jobscheduler: open an issue there with section 3.2 and the sweep of
3.4. Keep kbf#169 open as the kbf tracking issue, narrowed to "bump the js revision, add
the both-core farm test, document `--placement-load` interplay", and close it when that
lands. Do not close it as superseded: `--scheduler-core js` does not change the behaviour
(2.1), so the report is still true on every kbf configuration with the default flags.
Until then the supported workaround is `--placement-load on`, which has the hold (2.2).

## 4. Open questions for the maintainers

1. Is `--placement-load off` still a supported kbf default? In js it is a rollback-only
   switch, and it is the reason #169 reproduces. Flipping the default to `On` would give
   the hold today without a js change, but is a change in where work lands for every farm.
2. Hold the worker chosen by load ratio (today) or the one that empties soonest (needs
   remaining-life data js has only with learned profiles)? Ratio is simpler; the bound in
   3.3 then depends on the longest lease there.
3. Should a long-waiting low-QoS request ever pass a stream of higher-QoS work (aging
   across classes)? This note says no; it would turn QoS from a priority into a weight.
4. One hold per pool (today) serialises several large requests, each paying a drain.
   Acceptable for the report (two 24-CPU, six 40 GiB), or does a farm of few nodes need
   more than one?
5. VM leases: fold into this rule (3.2 does) or keep the separate VM reservation that
   `macos-vms.md` describes?
