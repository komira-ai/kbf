# Scheduler

The scheduler decides which worker runs which action, and makes sure that however
workers, networks and leaders fail, each operation is answered exactly once. This
document covers the scheduler core (`kbf-sched`), how `kbf-server` carries it out
(`kbf-server/src/farm.rs`), and what is **planned**. Lease handling on the daemon side
is in [daemon.md](daemon.md); the messages are in [worker-protocol.md](worker-protocol.md).

## Shape: a pure state machine

`kbf_sched::Scheduler` implements `kbf_types::StateMachine`. It is fed `Input`s, each
carrying the farm time, and returns `Effect`s for the caller to carry out in order:

| Input (`kbf_sched::Event`) | Meaning |
|---|---|
| `WorkerUp { worker, capacity, caps }` | a worker registered (the first `Hello` of a stream): a new session |
| `Capacity { worker, capacity, caps }` | a registered worker's report changed (a `Hello` resent on the same stream) |
| `Heartbeat { worker, running }` | a heartbeat on the worker's newest stream, with the leases it holds |
| `Submit { waiter, request }` | a caller asks for an action to run |
| `Committed(record)` | a record the scheduler asked to commit is committed (fed in log order) |
| `Started { operation, lease }` | the holder started the operation |
| `Report { operation, lease, outcome }` | the holder reports how it ended |
| `Tick` | time passed: expire silent workers' leases, run one placement round, note and refuse work no live worker can run |

| Effect (`kbf_types::Effect`) | The caller must |
|---|---|
| `Commit(record)` | append the record to the control log and feed it back as `Committed` once committed |
| `Start(start)` | send `Start` for a committed lease to its worker |
| `Answer(answer)` | answer every waiter of a finished operation |
| `Waiting(waiting)` | tell an operation's waiters why no live worker can run it, or that one can again |
| `Refuse(refusal)` | answer every waiter of a refused operation (`FAILED_PRECONDITION`) |

The core reads no clock and draws no random number, so the same inputs always give
the same decisions. That is what lets every replica of a log apply it identically
and lets the simulator replay a seed exactly.

In `kbf-server` the control log is the process itself: a `Commit` is treated as
committed the moment it is appended and fed straight back. Every input is followed by
a `Tick`, so placement happens as soon as something changes, and a timer ticks once a
second when nothing does. All of this runs under one lock in input order.

## Operations

An operation is one execution of one action that one or more callers wait on. Its
states:

```
Queued -> Leased -> Running -> Completed | Failed
  |          ^         |
  |          +---------+  (lease given up: back to Queued, granted again later)
  +-> Refused             (no live worker could run it for the unservable wait)
```

- `Leased { committed: false }`: placed, the grant proposed, no `Start` sent.
- `Leased { committed: true }`: the grant is committed and `Start` has been emitted.
- `Running`: the holder said it started. `kbf.worker.v1` has no message for this yet,
  so `kbf-server` never feeds `Started` and its operations stay `Leased` until their
  result; callers see `EXECUTING` from the moment the `Start` is sent.
- `Completed` / `Failed`: a result from the current lease was committed.
- `Refused`: a refusal was committed while the operation was queued (see
  [Placement](#placement)).

**Retention.** A finished operation (`Completed`, `Failed` or `Refused`) is kept for
the finished retention after its waiters are answered (`FINISHED_RETENTION` = 60 s,
the server's `--finished-retention-secs`), then dropped at the first input at or
after its end (issue #165). While it is kept, `kbf-server` keeps its callers too, and
WaitExecution on its name streams the done operation; once it is dropped the name is
NOT_FOUND. A late input naming a dropped operation (a stale record, report or start)
is ignored, as for any unknown one, so the scheduler holds only the unfinished
operations and those finished within the retention. Over a state store a name outlives
the scheduler's copy: any server answers it from the store's `done/` row, which is kept at
least ten minutes ([Results across frontends](#results-across-frontends)).

**In-flight dedup.** Operations are keyed by a `WorkKey`, a class and a dedup text the
scheduler treats as opaque. For a build the class is the REAPI instance name and the
dedup text the action digest (`ActionKey` converts to and from it). A new request whose key matches an unfinished operation joins it
as another waiter instead of queueing a second run, if the request is *joinable*:
hermetic and not `do_not_cache`. The same digest under another instance name is
different work and never joins. A more urgent caller that joins raises the
operation's QoS level.

## Leases

A lease is a numbered permission to run one operation on one worker. Its id is
`(term, seq)`: the term of the leader that granted it, and a sequence number within
that term. Ids order by term first, so every lease a newer leader grants is newer than
every lease an older leader granted. A single server's leases live and die with its
process, so each process picks its own term at start: its start time in milliseconds
on the wall clock, times 2^16, plus 16 random bits (`kbf_server::farm::process_term`).
A restarted server therefore never grants a lease id its predecessor granted (issue
[#137](https://github.com/komira-ai/kbf/issues/137)), and its terms
order after its predecessor's while the wall clock does not step back across the
restart. The replicated log's term replaces this once it is wired.

Two rules carry the scheduler's safety. Each holds whatever order inputs arrive in.

**Commit before Start.** Placing an operation emits only `Commit` of the grant. The
`Start` is emitted when the grant comes back as `Committed`, and never for a grant
that stopped being current in the meantime. A lease the log does not know can
therefore never run, and a new leader replaying the log can never send a `Start`
twice for work it does not know about.

**One accepted result per operation.** A report is turned into a proposed result only
if it comes from the lease the operation currently holds under a committed grant, and
only once per holding. A committed result is accepted only if its lease is the
operation's newest committed grant, in log order, and the operation is not finished.
Results from given-up leases, duplicates and late arrivals are dropped.

### Fencing: G and T

A worker can lose touch with the scheduler while still running work. Two clocks keep
the copies apart:

- **G = 60 s** (`fence::LEASE_GRACE`): the scheduler gives up every lease on a worker
  it has not heard from for G, and requeues the operations.
- **T = 40 s** (`fence::SELF_FENCE`): a worker running *self-fenced* work stops it once
  the newest heartbeat the scheduler acknowledged was **sent** more than T ago.

A heartbeat's send time is no later than the moment the scheduler heard it, so a
worker stops at most T after the scheduler last heard it, and the scheduler
re-dispatches no sooner than G after that. A margin of 5 s
(`LEADER_LEASE_MARGIN`) covers a change of leader; `T + 5 s < G` is checked at
compile time, so changing a constant to break it does not build.

Each lease carries a fence policy (`kbf_types::FencePolicy`):

- `RunOn` for hermetic work: it runs to its timeout and keeps its result. A duplicate
  run after re-dispatch costs only compute, and the second result loses in the log.
- `SelfFence` for networked work, which must never run twice at once.

The scheduler derives the policy from the request (`hermetic` gives `RunOn`). The
`Start` message does not carry it yet, so today's daemon self-fences every lease.

### Reconciling with what workers say they run

Every heartbeat lists the leases the worker holds (running, or finished with a result
not yet acknowledged). A committed lease that the scheduler holds on that worker and
the heartbeat leaves out is requeued:

- at once, if its `Start` went to an *earlier session of the same daemon process* (the
  daemon either received it before registering again, and then lists it, or never
  will);
- at once, as a farm fault ([Retries](#retries-of-farm-faults)), if its `Start` went to
  the daemon process the current one was *proven to replace* (issue #22; below);
- if its `Start` went to *another daemon process* that registered as the worker, once
  `HANDOVER_GRACE` (T + 5 s = 45 s) has passed since the scheduler last heard the
  worker before the newest change of process (issue #140);
- otherwise once its `Start` has been out for `START_GRACE` (equal to G), because until
  then the `Start` may still be on its way.

A lease whose result has already been reported is kept either way: that result is on
its way to the log.

The second rule assumes that a `Start` arriving after `START_GRACE` is never run. The
daemon makes this hold: it acts on a `Start` only within `START_VALIDITY` (W = 14 s) of
sending the heartbeat the `Start` names, and `W + T + 5 s < G` is checked at compile
time beside the other fence condition (see [worker-protocol.md](worker-protocol.md)).
The other way round, `Scheduler::not_held` names the leases a heartbeat lists that the
scheduler granted and no longer holds on that worker, and the server sends a `Cancel`
for each.

Sessions matter here. Only the first `Hello` on a stream registers a worker and opens
a new session. The server feeds heartbeats only from a worker's newest stream; a
heartbeat that arrives on a replaced stream is dropped and not acknowledged, so a
daemon still talking on an old stream fences on time.

A registration names the daemon process that made it: `Hello.instance_id`, drawn at
random when the daemon starts and the same on each of its streams. Only that process can say it no longer runs a lease, so the first rule above
holds only within one process. Two processes can register as one node: the node's
certificate is on a cloned machine, a second daemon was started with it, or a node was
replaced while the old one still ran (node identity is the certificate, not the
process). Then the newer process's heartbeats say nothing about what the older one runs,
and the older one, whose stream is no longer acknowledged, keeps running self-fenced
work until it fences, T after the scheduler last heard it. Its leases are therefore kept
for `HANDOVER_GRACE` from then, and given up by the first heartbeat after that which
leaves them out. A restarted daemon, or a rebooted node, is another process too: the
scheduler cannot tell it from a second daemon, so what it lost is requeued after the
handover grace rather than on its first heartbeat, unless it proves its predecessor
stopped. The server logs every change of process on a node. A registration without an
instance id (a daemon that predates the field) is taken as another process's.

**A proven restart** (issue #22). The daemon binary keeps its instance id in its
scratch directory (`<--data-dir>/leases/daemon-instance`), written only once every runtime has
been built, so once every driver's start-up sweep has killed the runs the daemon
before it left there. The next daemon reads the old id before it writes its own, and
names it in `Hello.predecessor_instance_id`. The server takes the claim when it names
the process the node's previous stream came from and that stream has closed, and
feeds the registration with `predecessor_stopped`. The scheduler then records that
process as stopped (`Worker.stopped_process`), and the first heartbeat of the new
process gives up at once every lease whose `Start` went to it and which it does not
list. It does not do so at registration: the phase, taken right after, sends a
switching whole-machine lease's `Start` to the new process, which then restores the
node. Leases of any earlier process keep the handover grace. The claim adds no
assumption: the sweep already needs one daemon per scratch directory. What it does
not cover: a copy of the scratch directory on another machine (a cloned disk) whose
daemon is cut off from the server, so that its stream has closed while it still runs;
its self-fenced work then stops within T, but a hermetic run runs on beside the rerun.
A daemon without `--data-dir` (the fake driver) names no predecessor.

This puts one duty on a daemon: it must list everything it holds in its first heartbeat
on each new stream, or what it leaves out is requeued at once. Re-adopting work across a
daemon restart is **planned**. Today a restarted daemon lists nothing of its
predecessor's, and before its `Hello` its driver ends every run that predecessor left on
the node (issue #155), so what is requeued (at once after a proven restart, else after
the handover grace) no longer runs there.
The grace rests on the older process fencing T after it was last heard; a process that
was killed fences nothing. For its runs, each run's keeper (a process that ends the run
when the daemon dies) is the guarantee whether or not a daemon is started again, and the
restarted daemon's sweep is the second line, when a daemon is started again on the same
node with the same scratch directory. What neither reaches is listed in
[daemon.md](daemon.md#when-the-daemon-is-killed).

### A daemon moving between frontends

Several frontends can share one state store, each with the daemons connected to it. A
daemon is connected to one frontend at a time, and the frontend it last said `Hello` to
owns its node: only that frontend may grant the node a lease, give one up, or take a
result for one. What the code does today:

- **The owner row.** `owner/<node>` holds the id of a frontend and a counter (eight bytes
  each, big-endian). A `Hello` makes the frontend it is said to the owner, with one
  compare-and-set write that adds one to the counter. The frontend remembers the counter
  it wrote. Each write of a lease of that node (a grant, a lease given up, a result) reads
  the row, and its transaction carries a check that the row is still the one read, so the
  write fails if the daemon said `Hello` elsewhere in between. A write that finds another
  name or counter writes nothing and fails with a "stale owner" error for that node. That
  error drops only what the frontend holds for that node (the node's held grants and
  results, and its later ones as they meet the same refusal): the frontend's work for its
  other nodes goes on, and the frontend is not marked as superseded, as a newer writer of
  the whole store would mark it (the writer generation, which is unchanged). A held `Start`
  reads the counter when it is written, not when it was proposed, so a daemon that
  reconnects to the same frontend does not turn its held grants stale. A node whose
  daemon has not yet said `Hello` to the frontend has no counter to check, and its
  writes are conditional on their lease rows alone, as they were.
- **Acknowledging heartbeats.** The frontend still hears every heartbeat it takes, but
  acknowledges one only while it owns the node and has written the node's session row
  lately. It looks at the owner row again when its last look is `OWNER_CACHE_TTL` (2 s)
  old; a frontend whose look says the daemon has gone elsewhere stops acknowledging and no
  longer renews the node's session row, so the daemon fences on time (T after the last
  acknowledged heartbeat, as above). The session row is the sign other frontends have that
  the daemon is alive, so a frontend that cannot write it (the store refuses writes, or a
  registration or renewal failed) stops acknowledging when the last write is `ACK_WINDOW`
  (30 s) old. The window is the session row's life (`LEASE_GRACE` from its last write) and
  one more renewal interval (`SESSION_RENEWAL`), less T and twice the 5 s margin of the
  fence. That is longer than a renewal interval and a heartbeat interval together (checked
  at compile time), so a frontend that renews on schedule never withholds an acknowledgement.
  By construction it is also short enough that a daemon's work ends before a reader that
  waited one renewal interval beyond the row's expiry would give its leases up; the orphan
  sweep below is that reader, and a compile-time check asserts it.
- **Claiming and the store.** A claim the store refuses because it cannot be reached is
  made again, while the daemon's stream is open, by the first feed that finds the store
  reachable; the heartbeats are not acknowledged until it is made. A claim that loses
  every try to others writing the row gives the node up (nothing is written for it until
  it says `Hello` here again), and a row that cannot be read is not claimed or tried
  again until the next `Hello`. A node whose claim failed for any reason, or whose
  arrival (the leases and session row the claim takes over) could not be read after the
  claim was written, is lost to the frontend in the same way until a claim succeeds, so
  no grant for it is written meanwhile. A claim made again after an arrival that could not
  be read finds the frontend's own row, so the frontend keeps the owner the first claim
  replaced and reports that one.
- **Finding a lease by its operation.** `lease-by-op/<operation>` holds the id of the
  lease the operation holds (term and sequence, eight bytes each, big-endian). The grant
  writes it in its own transaction, and the end of the lease (a give-up, a result, a
  refusal) deletes it. A request for an action that is running on another frontend's
  daemon joins the operation and needs to know which node holds its lease (whether the
  frontend owns the node decides what the join does, "Joining" below); it reads the
  operation's index row and that lease's row instead of every lease row of the farm. An
  operation in the queue
  holds no lease and is not looked up at all. A row that is missing, or names a lease that
  is not the operation's, is left by a release that predates the index, and the lookup
  then reads every lease row, as it always did.
- **A daemon that moves.** When a daemon says `Hello` to a frontend, that frontend takes
  up the unfinished operations whose lease rows name the node and that its scheduler does
  not hold (it started before the grant, or did not run it). Besides a request that joins
  an operation leased on a node the frontend owns already ("Joining" below), this is the
  only way a frontend takes up a lease it did not grant. The daemon's first heartbeat lists
  what it runs, and a lease it lists is kept. A lease it leaves out is given up as above: at once
  if the daemon is the process the node's session row names (the same process moved, as a
  reconnect), and after `HANDOVER_GRACE` and `OWNER_CACHE_TTL` (47 s) from the `Hello` if
  the row names another
  process or is gone and the node's owner was another frontend. In that case the old
  process may still be running the lease, acknowledged by the frontend it left until that
  frontend looks at the owner row again, and it fences T after its last acknowledged
  heartbeat; the wait covers both. The hold applies whether or not this frontend's
  scheduler has registered the node before (a daemon that went to another frontend and
  came back is registered here already; the leases taken in for it are booked on it at
  once, and kept for the wait). A node with no earlier owner, and a daemon that says
  `Hello` again to the frontend that owns it (its owner row names this frontend), keep the
  rules above. The `Hello` reads every `lease/` row to find the node's leases, while the
  frontend holds its lock: a cost of the order of the leases of the farm for each
  reconnecting daemon, accepted until a `lease-by-worker/` index is worth its writes.

### Frontends that share work

A frontend schedules onto the daemons connected to it, and the store decides, by the
conditional writes above, which frontend runs an operation. Each frontend also looks at the
store every `INTAKE_POLL` (200 ms) to learn what the others did. A farm with no state store,
one a retire fenced (`POST /v1/frontend/retire`), and one whose store cannot be read do
nothing at a poll.

- **A start.** A starting frontend takes up the operations that wait for a lease and none
  that holds one: the lease belongs to the frontend its node is connected to, and is taken
  up when the node says `Hello` here (above). A lease whose node never says `Hello` again
  is requeued by the orphan sweep.
- **Taking in.** An operation another frontend queued (a `queue/` row the scheduler does
  not hold) is taken in with the `loss/`, `fate/`, `subject/` and size-key rows the store
  holds of it, and the ids this frontend numbers with go past it. One that holds a lease
  by the time the poll reads its rows (another frontend granted it meanwhile) is not taken
  in: its lease is taken up only as above.
- **Forgetting.** An operation the scheduler holds queued whose `queue/` row is gone was
  granted, finished or refused by another frontend: it leaves the scheduler (no give-up, no
  loss, no fault is recorded) and the callers it had are kept. An operation whose `queue/`
  row is a newer write than the one the poll read last was granted elsewhere and queued
  again: it is taken in again with the rows of now. An operation whose lease the scheduler
  gave up and whose requeue the store has not taken yet has no `queue/` row for that reason
  and stays.
- **Callers.** The kept callers of an operation another frontend runs are shown it
  executing, and given its result when the `done/` row is there; they stay known by name
  for the finished retention, as the callers of an operation this frontend finished do.
  Callers who all cancelled are not followed. An operation the store has no trace of (its
  `done/` row outlived the retention) leaves its callers waiting and is reported as an
  untraced operation, as for the callers of a retired frontend.
- **Joining.** A request for an action the store holds an operation of (its `inflight/`
  row) joins that operation. If it is queued, or holds a lease on a node this frontend owns,
  the frontend takes it up and the request is a caller of its own scheduler. If it holds a
  lease on a node another frontend owns, or one no frontend has heard from since the lease
  was written, the frontend takes up nothing, the lease included: its scheduler would hold
  a lease it does not own, expire it when the node did not register here, and delete the
  owner's `lease/` row in the give-up. The request is shown the operation executing at once
  and followed through the store as the callers above are, until the `done/` row is there. If
  the node never comes back, the orphan sweep requeues the lease, the poll takes the
  operation in as a queued one, and the joiner is followed on.
- **A lost race.** A grant this frontend wrote that the store refuses as stale (the queue
  row is gone, or is another write) lost to another frontend. The scheduler undoes the
  placement (`Scheduler::unplace`: the booking is released and no give-up is recorded),
  and the operation follows the store as above, at once: the frontend starts nothing and
  keeps no lease for it. A result or a refusal the store refuses as stale, or for a node
  that moved to another frontend, does not leave its callers waiting either: they are
  kept, and answered from the store's `done/` row.
- **Operator state.** A cordon, drain or uncordon an operator sets through any frontend
  (the node need only have been claimed by one, `owner/` row) is written to `cordon/`; the
  other frontends apply a row that differs from the one the previous poll found, and
  return a node to placement whose row is gone. A drain's deadline is written on the
  store's clock, not the frontend's, and a frontend that applies it ends the drain as far
  from now as the deadline is from the store's time, so frontends whose clocks differ
  agree. A floor (the least memory a fresh submission books) a frontend raises is written
  with the larger ask of its own and the row's, at the version read, so no frontend lowers
  another's; the other frontends raise their floors to the stored ones, and floors that
  frontends stamped alike load in key order with distinct stamps.
- **The epoch.** A frontend serves the epoch it started under. The poll reads the stored
  epoch, and a frontend whose own is behind (an operator raised it) reports that
  (`Farm::epoch_ahead`) until it is restarted.

The orphan sweep looks, at the same poll, for leases whose node has no `session/` row
anywhere and is not connected to this frontend. The first look remembers the lease, with
the store's clock reading, its row's version and its node's `owner/` row's version. A later
look that finds the lease as it was, and the node still without a session,
`ORPHAN_WAIT` (`SESSION_RENEWAL` plus the 5 s margin of the fence, 25 s) after the first on
the store's clock (never the frontend's own), requeues it in one transaction that checks the
rows read (the lease, the operation and the owner row), deletes the lease and its index row,
queues the operation and counts the loss as a silent node's lease is counted. A lease
that changed, or whose node came back, starts the wait over, and so does one whose requeue
lost to another write between the look and the transaction (the pass goes on to the other
leases). The wait is long enough that
the daemon has stopped the work: it runs at most `ACK_WINDOW` and T (and the margin of the
fence) past the last write of its session row, which was `LEASE_GRACE` before the first
look at the latest, and a compile-time check asserts that this is under `LEASE_GRACE` plus
the wait.

### Results across frontends

A client that submitted through one frontend can ask any other for the result.

- **Names.** A farm over a state store names an operation `operations/<id>`: its id in
  decimal. The ids come from the blocks each frontend reserves in the store, so an id names
  one operation on every frontend and across restarts, which the term in the older form
  `operations/{term}-{waiter}` was there to guarantee (issue #154). Every caller of an
  operation has the same name. A farm with no state store (tests only) numbers from 0 in every
  process, and keeps the older form; it answers no name of the new one. A farm over a store
  still answers a name of the older form, for a caller its process holds.
- **Answers.** `WaitExecution` and `GetOperation` of any frontend answer a name. A frontend
  whose scheduler holds the operation answers through the channel of its first caller there.
  Any other answers from the store: the `op/` and `queue/` rows say it is queued or running
  (executing, once a lease is held), and the `done/` row holds its result. The channel the
  answer is made on is looked at again at every intake poll (`Progress`, the look
  `Farm::follow` makes for the callers of an operation it did not run), until it shows the result, and is dropped when no stream or call holds it. A
  store that cannot be read, or has no trace of the operation, answers NOT_FOUND.
- **Whose result it is.** A call is authorized against the instance name the operation was
  submitted under, and a `done/` row holds only the response. The transaction that finishes
  an operation therefore puts a `doneof/<operation>` row beside the `done/` row, from the
  request in the `op/` row it removes: the instance name and the action's digest, with the
  same life. A result with no such row (written by an earlier release) is not found by name
  once its `op/` row is gone.
- **How long.** A `done/` row lasts the finished retention or `DONE_FLOOR` (600 s), whichever
  is longer, so a client that reconnects to another frontend after the finish still gets
  its result. After that the name is NOT_FOUND; the finish removed the operation's
  `inflight/` row already, so the client's next `Execute` of the action is answered from the
  action cache, or runs afresh.
- **GetOperation.** It reads the stage the operation is at, once, from the same answer as
  `WaitExecution`. It is the only call of the `Operations` service the server serves; the
  others are UNIMPLEMENTED.

**No frontend takes the writer generation.** A frontend that opens a Postgres store
(`StateStoreUri::open`) writes the marker `kbf_state::SHARED_WRITERS` to `meta/generation`
instead (an absent-only write for a store with no row, a write conditional on the version read
for one that holds a generation, nothing for one that holds the marker), so any number of
frontends open the store and none fences another. The marker is not eight bytes, so a build
that takes generations reads the row as corrupt and refuses to start on the store; the writes
of such a build that is still running are refused by the marker. The scheduler's writes are
kept apart by the rows they read, as the sections above say. The move from generations to the
marker is a cold cutover: every frontend that takes generations is stopped before the first one
that does not is started (see
[the state-store cutover](../deploy/state-store-cutover.md#moving-to-frontends-that-share-the-store)).
A SQLite file is held by one process, so only one frontend can open it, and it takes no generation either.

### Node phases

A worker's `Hello` says what it is doing besides running leases
([worker-protocol.md](worker-protocol.md#node-phases); `kbf_types::NodePhase`), and the
server feeds it to the scheduler (`Event::Phase`) after every `Hello`, the
registration's included, before any placement round:

- `serving`: placement may offer it leases.
- `preparing <lease>`: it switches a Mac's GUI session to the user of that
  whole-machine lease, which it holds, then runs its action; `restoring <lease>`: it
  switches back after the lease's action ended, and cleans up. The switch is a
  userspace restart that ends the daemon, so the worker is silent for minutes. While
  the worker is silent, that lease (and only that one) is kept for
  `SESSION_SWITCH_GRACE` (10 minutes, more than G) counted from the first report of the
  phase, instead of G; past it the lease goes back to the queue with the reason
  `SessionSwitch`. Nothing of the lease runs during a switch (the action has not
  started, or has ended with its result kept on the node), so keeping it past G cannot
  run it twice. A daemon that dies while the action runs is no switch: the action's
  processes live on until the restarted daemon kills them before its `Hello`; that
  daemon says `restoring <lease>` and fails the lease (its result is lost) rather than
  run it again. Until it is back those processes live on (the helper runs them), beside
  a retry elsewhere once the lease is given up. The same phase reported again (after
  the restart) does not extend the grace; `preparing` then `restoring` gives the switch
  back a grace of its own.
- When a phase names a lease the scheduler holds on that worker, committed, whose
  `Start` went to an earlier session (the restarted daemon is a new session, and a new
  process), the scheduler sends the `Start` again to the new session: that is how the
  daemon takes the lease up after the restart. A phase that names a lease of this term
  the scheduler does not hold there is answered with a `Cancel` by the server
  (`Scheduler::not_held`).
- `quarantined <why>`: a cleanup failed. The worker takes no lease.
- `memory_pressure`: the daemon holds a lease and its node's memory stall time is above
  `--memory-pressure-pause-percent` (default 10; [daemon.md](daemon.md#cgroups-and-limits),
  "The memory backstop"). The worker takes no new lease until the daemon says
  `serving` again, and keeps the ones it runs. It is no fault of the node's, and
  counts as none.

A worker whose phase is not `serving` is held back from placement as a cordoned one
is: work only it could run waits, naming it and its phase (`mac-1 (quarantined)`),
and is not refused for it. Of the plan's rule "a node that misses the preparation
timeout is quarantined" ([fleet-updates.md](fleet-updates.md#102-isolation-layers)),
only the requeue is built: a worker silent past the grace is not quarantined by the
server; its daemon, if it returns, finds the lease cancelled and cleans up.

## QoS

A QoS level (`kbf_types::Qos`) says how urgent work is. It is chosen by the user and is
never a platform property, because platform properties are part of the action digest
and urgency must not split the cache.

| Level | Urgency | Meant for | Preemptible |
|---|---|---|---|
| `interactive` | 300 | a person is waiting | no |
| `ci` | 200 | CI and automated clients | no |
| `batch` | 100 | jobs, background work | yes |

A deployment may define more levels with `Qos::custom(name, urgency)`. A custom
urgency places the level among the built-in ones; it may not reuse a built-in name or
urgency. Every level at or below `batch` is preemptible.

The queue is ordered by urgency, most urgent first, then by submission order.

### Choosing a level

A client names its level with the request header `x-kbf-qos: <level>` on Execute (Buck2
sends one for each `http_headers` entry of its `[buck2_re_client]` buckconfig section).
What the front end does with it:

- No header: `ci`. A client that sends none is unchanged.
- The header names a level on the front end's list (`--qos-allowed`, `ci,batch` by
  default; `interactive` is accepted only when the operator adds it): that level. The
  value is read without regard to case or surrounding spaces.
- Anything else (a level not on the list, a value that is no level, two `x-kbf-qos`
  headers): `ci`. The call is **not** refused. The front end logs a warning, at most a few
  a minute, naming the value that was ignored.

The level is on the operation, not in the action: it is not a platform property, so an
action sent at `ci` and at `batch` has one digest and one action-cache entry. Queued work
orders by urgency, so a queued `batch` action starts after every queued `ci` action; a
caller that joins a queued twin at a more urgent level promotes it (see the tests in
`crates/js-sched/tests/scheduler.rs` in the jobscheduler repository, where the scheduler code and its tests now live).

Not built yet: reading REAPI's `ExecutionPolicy.priority`, **preemption** of `batch` work
(a running `batch` action keeps its slot while `ci` work waits; only what is queued is
ordered), and fair turns between invocations within one level.

## Placement

What the code does today:

- Each worker's capacity is read from its node report at registration: `cpus` x 1000
  millicores, `mem_gib` GiB and `gpu` GPUs. The whole machine is offered. So are its
  capabilities (`kbf_caps::NodeCaps`, see [capabilities.md](capabilities.md)).
- Every action requests one core and 4 GiB, the first rung of the memory ladder
  (`kbf_front::DEFAULT_RESOURCES`; the server's `--memory-ladder-gib` sets the rung),
  or the whole cores and GiB its `kbf-book-cpus` and `kbf-book-mem-gib` properties name (see
  [platform-properties.md](../platform-properties.md#kbf-book-cpus-and-kbf-book-mem-gib)),
  its `gpu` count, and what its platform asks of a worker (`Request::needs`). The
  scheduler raises the memory of a new submission to its action key's memory floor,
  if it has one (see "Memory kills" below).
- With `--exclusive-actions` on, the front sets `Request.whole_worker` on every `action`
  request (not on a `whole_machine` or `vm` one). The request still carries the booking
  above, which the front computes as before and which stays the record of what the
  platform named, but the scheduler resolves it per worker: the request books the
  worker's full capacity (all its CPUs, memory and GPUs), has room only on a worker that
  holds no lease, and places no other lease beside it, as a whole-host action does. The
  `Start` carries that capacity. It is placed by the same round, and while it waits the
  scheduler holds a reservation for it on a worker. The action digest is untouched. A hint
  (`kbf-book-cpus`, `kbf-book-mem-gib`) is read and recorded but not booked. An own-limit
  memory kill has no larger rung (see "Memory kills"): the operation finishes
  `Failed(OutOfMemory)` and leaves no memory floor. The control log has no field for the
  request's `whole_worker`, so recovery reads every operation back with it false. A queued
  operation is then an ordinary action. A running one is recovered by booking its request
  as stored (`recover.rs`: `book` with `worker.booking(request)`, the request's own
  resources, and a load charge from the recorded grant cost, which is the admission
  charge of that booking; the worker-wide charge is made only in the scheduler's memory at
  grant), so it holds the front's small booking and ordinary actions can be placed
  beside it, and if it is lost or requeued it reruns as an ordinary action, up the memory
  ladder, without owning the worker. A run that depends on one action per worker must not
  restart the server. The server logs `exclusive-actions: on` at start when the flag is on. Both
  scheduler cores take the request unchanged.
- Each request carries its lease kind (`kbf-lease`: `action` or `whole_machine`). A
  worker is feasible for it only if its node report lists a driver that serves the
  kind (`container`, `native`, `fake` or `local` for `action`, `native-whole-machine` for
  `whole_machine`; see [capabilities.md](capabilities.md#driver-entries)).
- A placement round walks the queue in order and gives each operation to the least
  loaded live, feasible worker whose capabilities satisfy its platform and that has
  room for it: the one with the least booked as a share of its capacity (the larger of
  its CPU share and its memory share, each booked over capacity, compared exactly in
  integers), then the fewest leases, then the first in name order. Work therefore
  spreads over the farm instead of filling the first worker. The platform and the room
  are decided first and the load only ranks the workers that pass, so a Mac is never the
  least loaded worker for Linux work. The memory share counts what leases booked, though
  admission does not (below), and is not capped at 1: a worker booked past its memory is
  still more loaded than one booked less. A whole-machine lease, and an action at a
  worker's whole memory, go only to a worker with no lease, so their candidates tie and
  the first by name is taken. An action has room where no whole-machine lease is held,
  its free CPUs and GPUs (capacity minus bookings) hold the request's, and the request's
  memory is at most the worker's memory capacity (`Worker::has_room`). CPUs and GPUs are
  booked; memory is only checked against capacity: what other leases hold of the node's
  memory never keeps an action out. The node bounds how many actions run at once by their
  CPUs, and its memory-pressure backstop pauses new starts. An action at the worker's
  whole memory runs alone. A
  `whole_machine` lease has room only on a worker that holds no lease and is at least
  its request large; it books the worker's whole capacity, which its `Start` carries.
  A worker is live if it was heard from within G. Matches are memoised per platform
  request and kind in a round.
- A `whole_machine` lease that fits nowhere holds one feasible worker it could run on:
  the one it held at the last round while that one still could, else the one with the
  fewest leases, then the least booked. No operation after it in queue order (less
  urgent, or as urgent and younger) is placed there, so that worker empties as its
  leases end; operations before it still are. The hold lasts while the operation is
  queued and the worker could run it (live, not cordoned, feasible). No lease is ever
  stopped for it. An `action` holds no worker: one at the whole memory of a worker (the
  last rung of the memory ladder, which runs alone there) waits until some worker's
  free memory meets its request, that is, until a worker has no lease. Under sustained
  spread load that wait can be long, and nothing reserves a worker for it; callers
  wait. A large `action` request has no such hold either (issue #169), and spreading
  makes it likelier to wait than first fit did: with eight one-core leases on two
  workers of eight cores, first fit leaves one worker empty and a six-core action runs
  at once, while spreading leaves four on each and it waits for cores to free.
- With `--whole-node-max N` the hold above is replaced by the scheduler library's whole-node
  drain and caps (off unless the flag is given; the settings are the farm's, since kbf has
  one scheduler and no pools). A `whole_machine` request that fits nowhere waits
  `--whole-node-drain-after-secs` (0), then holds the worker that will empty soonest: the
  one whose leases' longest wall times end first, then the fewest leases, then the lowest
  name. A wall time is the `timeout` of an action; one that names none (or zero) is taken
  to end last. The hold is given up at its deadline,
  `--whole-node-drain-deadline-secs` or, left out, the request's own wall time (an action
  with no wall time and no such flag is never given up, so set the flag when clients may
  send no timeout); the worker is then barred to the
  request for `--whole-node-reserve-backoff-secs` (300) and another is held. After
  `--whole-node-max-reserve-attempts` (2) give-ups the deadline is the wall time. No lease
  is stopped. The caps: at most N workers whole (held or reserved) at once, at most
  `--whole-node-max-draining` (1) reserved while still running leases, at least
  `--whole-node-keep-for-ordinary` (1) workers left with neither, and no reservation on the
  last worker an older ordinary request could run on unless
  `--whole-node-allow-last-capable`; a request a cap stops holds nothing and queues.
  An operation's wall time is kept with its rows in the state store (an `opwall/` row, written
  in the transaction that queues it, only for an operation that has one, and removed when the
  operation ends): a farm that takes the operation up again, after a restart or from another
  frontend, gives it to the scheduler before the first placement round, so the hold keeps its
  deadline and the leases their place in the drain order. A store written before the row
  existed recovers its operations with no wall time.
  The tuning flags need `--whole-node-max`. `Scheduler::whole_state` and `whole_summary`
  say why a request waits; `GET /v1/queue` (a `whole_node` section) and the
  `kbf_sched_whole_*` gauges show them, and the Queue page of the UI.
- A request with `ios.device=1` has room only where a device the worker reports
  satisfies every `ios.device.*` attribute of the request and no lease holds it, and
  where the worker holds no other device lease (`DEVICE_LEASES_PER_WORKER` = 1, until
  leases are shown not to reach each other's devices). It books the first such device
  by id with its CPU and memory, and its `Start` names it (`device_id`). A worker
  whose report lists no device is never given one, so a daemon that predates devices
  is not. See [ios-devices.md](ios-devices.md#54-booking).
- At most `PLACEMENT_ROUND` = 256 grants are made per round (one log flush per round).
- A booking (and its device) is released when the operation finishes or its lease is
  given up; a new registration or report of the worker does not release it.
- Every queued operation is also checked, past the grant limit too, against what live
  workers could give it once their bookings end. If no live worker satisfies its
  platform, or none that does is large enough, it is *unservable*: the scheduler emits
  `Waiting` with a reason (no worker connected; the closest worker and the requirements
  it lacks; or the request that is too large) whenever the reason changes, and
  `Waiting` with none once a live worker could run it again.
- An operation unservable for the unservable wait (`UNSERVABLE_WAIT` = 300 s, the
  server's `--unservable-wait-secs`) is refused: the scheduler takes it out of the
  queue, so nothing places it meanwhile, and commits a `Refusal` record. Once that is
  committed the operation is `Refused` and its waiters are answered (`Refuse`). The wait
  counts from the first round at which the operation was unservable, through changes
  of its reason, and the refusal is proposed at the first round at or after that time
  plus the wait. It restarts whenever the operation is servable again. It is not zero
  because a worker that can run the work is often a moment away: daemons reconnect
  within seconds of a server restart, and a Mac that reboots is gone for minutes. Work
  that no kbf daemon could ever run is refused by the front before it is queued (see
  [capabilities.md](capabilities.md#what-the-code-enforces-today)). When several
  frontends share one state store, each knows only the daemons connected to it, so
  before such a refusal is committed the frontend reads the `session/` rows of the store,
  which hold the capabilities and capacity of every frontend's daemons. If a daemon of
  another frontend could run the operation, the refusal is taken back (the scheduler
  puts the operation in the queue again, `Scheduler::hold_refusal`) and comes due again
  after another wait; a store whose session rows cannot be read holds it too. A refusal
  that is committed is written only while the operation is still queued in the store, so
  it never removes an operation another frontend has granted. The frontend that loses
  such a race drops its write as stale, and the callers waiting on it at that frontend
  are not answered by it.

**Planned**, in roughly the order they are needed:

- **Learned sizes** (`kbf-estimator`). Requests are sized from what earlier runs of
  similar actions used, with a margin that shrinks as samples grow, raised quickly
  after an overshoot and lowered slowly. Cold actions get a cautious prior.
- **Scoring.** Among feasible workers: alignment of the request with free room (so
  CPU-heavy work goes where CPU is free and memory-heavy work where memory is free),
  time saved by inputs already on the worker, a penalty for pressure, and best fit to
  keep large holes free for large work.
- **Protected floors.** Capacity a node reserves for other services is subtracted
  before it is offered.
- **Reclaimed room.** `batch` work may use memory that is booked but unused, and is
  preempted at once when more urgent work needs it.
- **Retries.** Every infrastructure failure retried on another worker a bounded
  number of times before callers see `INTERNAL`. Today the farm faults of
  [Retries of farm faults](#retries-of-farm-faults) are, and so is `Failed(Infra)`, within
  the same `FARM_RERUNS` budget. A
  missing input (`Failed(MissingInput)`, [Outcomes](#outcomes)) is not an
  infrastructure failure and will not count against that budget: rerunning it fails
  the same way until the client uploads the blob.

## Retries of farm faults

What the code does today (issue #22; the policy is
[failure-classes.md](failure-classes.md), 6.5). Four kinds of run end as the farm's
fault, and each is recorded on its operation as a `FarmRun` (the node, and why):

- a busy node's memory kill (`Failed(NodeMemoryPressure)`, [Memory kills](#memory-kills));
- a run the daemon reports lost (`Failed(Lost)`, from `Result.lost`): fenced (no
  acknowledged heartbeat within T), or a whole-machine action whose result a daemon
  restart took;
- a lease given up because its daemon was proven stopped (the requeue reason
  `Restarted`, [above](#reconciling-with-what-workers-say-they-run));
- a run the farm could not do (`Failed(Infra)`): the daemon's driver failed around the
  action, or its outputs were not all stored.

They share one budget: the operation runs again while at most `FARM_RERUNS` = 2 runs
have ended so, so at most 3 runs end as farm faults, whatever mix of the four. A
rerun prefers a node none of its farm runs ended on: placement skips those nodes while
a live, serving node outside them could run the request once its bookings end, and
waits for it; when none could (a farm of one node), it takes one of them. The run past
the budget finishes the operation with its failure; a restart past it has no result of
its own, so the scheduler proposes `Failed(Lost(DaemonRestarted))` for that lease and
the committed record finishes the operation, as every result does. Callers then get
`INTERNAL`: "kbf farm fault on <last node>: <what ended its run>, and its reruns are
used up. Operator fix: <fix>. Runs: 3 (node-a, fenced; node-b, daemon restarted; ...)."

Not charged to that budget: leases given up for reconnection or a session switch (they
never ran) and leases given up as not started (the node heartbeats but never listed the
lease within the start grace). A not-started give-up has a budget of its own,
`NOT_STARTED_RERUNS` = 3 per operation: each one adds the node to those placement avoids
and runs the operation again; the give-up past the budget finishes the operation with
`Failed(Infra)` (the farm never started the action), and its callers get `INTERNAL`
"kbf farm fault: the farm never started the action; 4 leases were not listed by their
nodes within the start grace" (`kbf-server/src/fault.rs`). That result is the
scheduler's own, so it is not run again as an `Infra` farm fault. A lease given up for
silence (G) or replacement without proof (the handover grace) is charged as a lost run
(fenced) once it had been sent its `Start`. Nothing
a lost run produced reaches the action cache: a killed run sends no `Result`, a lost
`Result` never completes an operation, and a late `Result` of a lease given up is
refused once the operation holds a new lease.

## Memory kills

What the code does today (`kbf_sched::MemoryRun`; the policy is
[failure-classes.md](failure-classes.md), 6.1). A daemon's `Result` says which memory
ran out when a lease was killed for it (`Result.memory_kill`,
[worker-protocol.md](worker-protocol.md#result-and-resultack)); the native and container
drivers set it. The server
turns it into one of two outcomes, and the scheduler decides on the committed result:

- **`Failed(OutOfMemory)`: the action passed its own memory limit.** The operation is
  queued again one rung up a **fixed ladder**, the server's `--memory-ladder-gib`
  (default `4,16,32`), and after the last rung on a **whole host**. The first booking
  is the front's: the first rung (4 GiB) for an action that names no memory size, else
  the GiB its `kbf-book-mem-gib` names, from which the ladder climbs (a 6 GiB ask
  that is killed books 16 GiB). A kill books the first rung above what the killed run
  booked, unless that rung is not below the **cap**: the memory of the largest node
  that could run it when the kill is committed, one that is live (cordoned or not),
  serves its lease kind, satisfies its platform and holds its CPU and GPU request; the
  node of the killed run always counts. Then it books the cap, the whole host: with all
  its memory booked no other lease fits there, so the run has the node to itself while
  its CPU booking stays what it was. On a farm whose largest node has 24 GiB the rungs
  are 4, 16 and then 24 GiB (the 32 GiB rung is skipped). A whole-machine lease raises
  what it booked, the node's whole memory, the same way. A kill of a run that booked
  the cap finishes the operation `Failed(OutOfMemory)`: the action needs more memory
  than any node offers. The flag is a list of GiB that ascends strictly and whose
  rungs are each at least 1; the server refuses another at start. One rung (`8`) is a
  first booking of 8 GiB and then a whole host. The first booking is not clamped to
  the farm: where no live node has the first rung's memory, an action that names no
  size is unservable, as one that names that size would be, and is refused after the
  unservable wait; a farm of small nodes sets a lower first rung.
- **With `--exclusive-actions` on**, an action already books the whole worker, so it has
  no rung above: an own-limit kill finishes the operation `Failed(OutOfMemory)` and raises
  no floor for its key, the same as a kill of a run that booked the cap.
- **`--no-memory-climb`: no rung above the first booking.** With the flag, set on the
  server (default off; off is the behaviour above), an own-limit kill does not queue the
  operation again. The scheduler records the kill as a memory run, as it does a kill at
  the cap, and finishes the operation `Failed(OutOfMemory)` at the booking it ran with:
  the first rung, or the GiB its `kbf-book-mem-gib` names. Its callers are told
  FAILED_PRECONDITION with reason `ACTION_OUT_OF_MEMORY`, the message saying the ladder is
  off, not that no node is large enough (it passed its memory limit with 4 GiB booked on
  node-a; the memory ladder is off (--no-memory-climb), so it is not run again with more
  memory. Runs: 1 (4 GiB on node-a)). The server tells the two apart by the last run: a
  kill below its cap can only end an operation when the climb is off, and its attempt in the invocation record carries
  `memory_kill` `own_limit`. No booking, floor or key floor is raised, so every
  submission of the action starts where the first did. A kill the node made under memory
  pressure is not the action's: it runs again with the same booking and counts against
  the farm-fault reruns, with the flag as without it. The ladder's rungs still matter
  for the first booking only (the first rung). `--learned-sizes on` is allowed with the
  flag: the booking floor the estimator adds comes from a kill that climbs, which the
  flag never lets happen; the estimator still takes the kill in as a sample of the
  action, as it takes any kill, but the scheduler holds no floor from it. The flag is
  held by the legacy core's scheduler, which a restart builds again from the flag. The js
  core builds its scheduler inside `js-server-core`, whose configuration has no setting
  for it yet, so a server given `--no-memory-climb` with `--scheduler-core js` refuses to
  start (exit status 2). A start logs `memory climb: on` or `memory climb: off`. With
  `--exclusive-actions` as well, an action books the whole worker, so its booking is its
  cap: an own-limit kill reads as a kill at the cap, with the original message, not the
  ladder-off one.
- **The memory floor.** Each raised booking is kept as the memory floor of the
  action's key (instance name and action digest), and a later submission of that key
  books at least the floor, so a known-big action starts at the rung that ran it
  instead of climbing from the first again; a floor never falls. A server with a
  state store writes each raise down with the requeue that follows it and restores the
  floors at start; one without a store forgets them at a restart (one more killed run
  per key). At most `MEMORY_FLOORS` = 65 536 are kept, the one raised longest ago forgotten
  first.
- **`Failed(NodeMemoryPressure)`: the node killed it while it was under its own
  limit** (a node-wide out-of-memory kill, or the node's backstop). The busy node's
  fault: the node's memory-pressure count (`Scheduler::memory_pressure`) goes up, and
  the operation is queued again with the same booking as a farm fault
  ([Retries of farm faults](#retries-of-farm-faults)): preferring another node, from
  the budget it shares with lost runs. It never raises the booking or the floor. Past
  the budget it finishes `Failed(NodeMemoryPressure)`.
- **The two budgets do not mix.** A rung of the ladder is not a farm rerun and a farm
  rerun is not a rung, so a long ladder never takes a farm fault's reruns, and farm
  faults never stop the ladder short of the cap. An operation runs at most
  `1 + rungs above its first booking + 1 (the whole host) + FARM_RERUNS` times:
  `1 + 3 + FARM_RERUNS` for the default ladder. Leases given up for
  reconnection or a session switch count against neither; not starting has its own
  budget, `NOT_STARTED_RERUNS`; silence and replacement without proof after `Start`
  was sent are charged as a lost run.
- A memory kill committed for a lease the operation no longer holds (it was given up
  and is queued again) changes nothing: no run is recorded and the booking stays.
- Each rerun is a requeue the server logs with its reason ("passed its memory limit
  with 4 GiB booked; it runs again with 16 GiB booked"); a busy node's kill is also
  logged as a warning on the `kbf_server::attention` target with the node's count.

## Outcomes

A daemon reports `OK` with an `ActionResult`, or a failure status. The server maps it
to the scheduler's outcome and to what callers see:

| Report | Scheduler outcome | Callers get | Action cache |
|---|---|---|---|
| `OK`, all outputs stored | `Completed` | the `ActionResult` | written if exit code 0 and not `do_not_cache` |
| `OK`, an output not stored | `Failed(Infra)`: run again from the farm budget | past the budget, `INTERNAL`: "kbf farm fault on <node>: ..." | not written |
| `DEADLINE_EXCEEDED` | `Failed(Timeout)` | `DEADLINE_EXCEEDED`, with the partial stdout and stderr digests the CAS holds, if any | not written |
| `INVALID_ARGUMENT` | `Failed(Invalid)` | `INVALID_ARGUMENT`, with the daemon's reason | not written |
| not `OK`, `memory_kill` own limit | `Failed(OutOfMemory)`: run again one rung up (4, 16, 32 GiB, a whole host) below the cap | at the cap, `FAILED_PRECONDITION`: "kbf: the action needs more memory than any node offers: ...", with an `ErrorInfo` of reason `ACTION_OUT_OF_MEMORY` | not written |
| not `OK`, `memory_kill` node pressure | `Failed(NodeMemoryPressure)`: run again with the same memory, from the farm budget | past the budget, `INTERNAL`: "kbf farm fault on <node>: ..." | not written |
| not `OK`, `lost` set (fenced, or daemon restarted) | `Failed(Lost)`: run again from the farm budget, preferring another node | past the budget, `INTERNAL`: "kbf farm fault on <node>: ...", naming every farm run | not written |
| `FAILED_PRECONDITION`, every `PreconditionFailure` violation `MISSING` with a `blobs/<hash>/<size>` subject | `Failed(MissingInput)`: answered at once, never run again, no farm rerun spent | `FAILED_PRECONDITION`: "N input blob(s) are not in the CAS", with the `PreconditionFailure` listing each blob as its only detail, as the front answers before queueing; Bazel re-uploads and executes again | not written |
| anything else | `Failed(Infra)`: run again from the farm budget | past the budget, `INTERNAL`: "kbf farm fault on <node>: ..."; the daemon's reason goes only to the server's log, a WARN `lease failed` line with the lease, the node, the code and the reason | not written |
| (none: refused unrun) | `Refused` | `FAILED_PRECONDITION`, with the reason | not written |

A run that exits non-zero (a failing test) is still `Completed`: callers get its
result, but it is not cached. The action-cache entry is committed before the callers
are answered, so a caller that asks again at once gets a hit. Today an infrastructure
failure is rerun within the farm budget ([Retries of farm faults](#retries-of-farm-faults));
past the budget it is answered `INTERNAL` naming its runs.

## Accounting

What exists today: a runtime may attach a `kbf.worker.v1.ResourceUsage` to the
`ActionResult` it returns, inside `execution_metadata.auxiliary_metadata`. It carries
user and system CPU time, peak memory, and wall time, as the kernel measured them. The
test-only local runtime fills it, with the peak resident memory of any one process
(from `wait4`). The container driver fills it too, with the lease cgroup's
`memory.peak` (the most memory the whole cgroup held at once) and the cgroup's own
counters beside it. The two peaks are different quantities in the same field, so
read `peak_memory_bytes` knowing which runtime wrote it. Because it travels inside the result, it
reaches the server and the action cache with it.

**Planned**: one usage record per lease (CPU-seconds, peak memory, wall phases, queue
time, and whether the work ran, hit the cache or joined a twin), kept off the dispatch
path and stored where any query engine can read it. Records feed the size estimator.
Infrastructure retries and preemptions are charged to the farm, not to the user.

## More than one server

The scheduler runs on one server today, and its queue lives only in that process.
In the planned shape ([deployment-topology.md](deployment-topology.md)) the control
log is a Raft log on local disk, with one voter first and three voters on three
hosts later, and the scheduler runs on its leader:

- `Commit` effects become log proposals; `Committed` inputs come from the log's apply
  path, on every replica in the same order.
- A new leader starts a new term, so its lease ids order after every earlier lease,
  and the log decides which result of an operation wins. A failover keeps the leases
  committed before it: `Welcome.epoch` names the log, not the leader, and daemons
  resend their results to the new leader.
- Submissions are committed too, so a new leader inherits the queue (not yet: today a
  leader change would lose queued work).
- Only the leader schedules. At three servers any server serves the read path and
  accepts upload bytes, but the client front routes `Execute` and `WaitExecution` to
  the leader, the one server whose `/readyz` passes its `leader` check, and a follower
  sends every metadata commit to the leader rather than applying it. Every daemon holds its one worker
  stream to the leader, and a follower answers a daemon's session with a redirect
  naming the leader. None of this is built: `/readyz` exists, but its `leader` check
  always passes on the one server, and there is no second readiness path and no
  redirect yet.

## The js scheduler core (`--scheduler-core`)

Every build has two scheduler cores, and `--scheduler-core` (`[scheduler] scheduler-core`)
picks the one a server runs: `legacy`, the default, is the farm described above; `js` runs
the farm on the js scheduler core (`kbf-server/src/farm_js` and `src/farm/js`), which
decides placement, leases and retries itself while the farm carries out what it decides.
The server logs the core at start (`scheduler core: js`) and exports it as
`kbf_scheduler_core{core}`.

To switch, change the flag and restart the frontend; to switch back, set `legacy` and
restart. Both cores keep their state in the same rows of the state store, and each reads
what the other wrote (the lease record's cost field is additive and the legacy core ignores
it), so nothing is converted either way.

What an operator sees with `js`:

- **Learned sizes.** `--learned-sizes shadow` and `off` work: the estimator is shown every
  attempt, the `size estimate:` line, the `kbf_estimator_shadow_total` counts and the
  size summaries (`est/` rows) are as on `legacy`, and no booking changes. `--learned-sizes on`
  is refused at start (exit status 2): this core sets no size keys, so `on` would book as
  `off` does.
- **Operation names.** Only `operations/<id>` names are answered. A name of the older form,
  `operations/<term>-<waiter>`, is not found, also on a farm with no state store (see
  "Results across frontends": `legacy` still answers it for a caller its own process holds). After a rollback to `legacy` a client that holds an
  `operations/<id>` name from `js` still finds it on a farm over a state store: `legacy`
  answers that form from the same `op/`, `queue/` and `done/` rows. A farm with no state
  store (`kbf-server` without `--state-store`, which keeps the scheduler's state in memory and
  loses it on a restart) answers only the `operations/<id>` names of operations the core
  holds, and nothing from a store.
- **Node load.** `--placement-load` is read on this core as on `legacy`. The farm accepts
  the load a daemon reports on its heartbeat by the same rule, keeps it in memory, shows it in
  the node's detail, and hands each accepted reading to the scheduler, which was built with
  the load configuration of `placement_load.rs`. With `off`, the default, the scheduler books
  by the CPUs a lease asks for and reads no load. With `on`, the js scheduler's placement by
  load is switched on as a whole, not only the reading. `placement_load.rs` pins these
  settings as constants, and its tests hold them:
  - *Load predicate.* A node is admitted by its measured load and the load a lease is
    predicted to add, not by its booked CPUs.
  - *Profile charging* (`PROFILE`, `PROFILE_MEMORY`, floor 750 thousandths). A key with a
    learned time profile is charged its cores per minute, checked over the next half hour, and
    its remaining maximum memory. This core gives the scheduler no profile, so the setting
    is idle today.
  - *Flat-memory guard* (`FLAT_MEMORY_SLACK_PERMILLE` = 0). The flat memory charged to the
    leases of a node, the new lease's own included, stays within the node's memory. With no
    prediction a lease is charged no memory, so the guard counts only the leases that carry
    one (a retry up the memory ladder, a VM) unless `--placement-predictions on` gives leases
    a memory; in `--placement-load on` alone it admits nearly everything.
  - *Expected-life delta* (`EXPECTED_LIFE_DELTA_MINUTES` = 2). A new lease is charged until
    the P90 of its key's wall time plus two minutes, not for the whole horizon. The delta
    needs a prediction for the operation and a curve source that knows the key's wall time.
    With `--placement-predictions on` this core gives the scheduler the first, but the farm
    never gives it a curve source, so the setting is idle today.
  - *Balance scoring.* Nodes are ranked with the scheduler's own balance term, which
    `placement_load.rs` leaves at the scheduler library's value.

  A node with no usable reading is admitted on the sum of its leases' predicted costs, never
  by the booked rule. A node that has never sent an accepted reading (a daemon or runtime
  that takes none, or an agent that reports none) is charged at least 1000 m for each lease
  (the node's whole CPU capacity, if that is less), whatever the prediction, so a prediction
  under one core changes nothing there, and `--placement-predictions on` would do nothing for
  small actions on such nodes; a node that has measured and gone stale is not floored. On a
  farm of such nodes `--placement-load on` therefore charges close to the booked rule, and a change
  in placement is not to be expected from it. `--placement-predictions` is read under
  `--scheduler-core js` too (see the flag's help): `on` hands the scheduler the prediction at
  submit; an operation read back from the state store is charged as with `off`.
- **Node of another frontend.** The detail of a node that another frontend holds is not
  answered from the store's rows.
- **Whole-node drain.** A submission's `max_wall` is not given to the scheduler, so a
  whole-node drain sees no per-operation deadline. Keep whole-node placement off
  (`--whole-node-max` unset) with `js` until the core takes `max_wall`.
- **Logs.** The core's "lease finished" line has no `exit_code=` field and comes before
  the `action usage:` line. The `size estimate:` line of a new operation is written after
  the submit is queued, not before.
- **Fenced frontend.** A frontend the store has fenced still writes size summaries (the
  `est/` rows); on `legacy` it writes none.

## Where to look

- `crates/kbf-sched/src/scheduler.rs`: the state machine.
- `crates/kbf-sched/src/fence.rs`: G, T and the worker-side `SelfFence` clock.
- `crates/kbf-server/src/farm.rs`: carrying out effects, the result path, the
  action-cache write.
- `crates/kbf-sched/tests/sim_cell.rs`: the scheduler in a simulated cell, over a seed
  sweep.
- [simulation.md](simulation.md): every scheduler simulation, the invariants they
  check, and the scenarios still to build.
