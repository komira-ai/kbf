# Updating a split-mode farm without failing a build

Status: design, nothing in this document is built. It is read against `feature/arch-v2` and these pull requests, fetched
when this was written: #857, #879 and #887 (the frontend handover, open, stacked), #803 (the rollout state machine, open), #886
(merged: a restarted node whose index is whole registers `Active`) and #860 (open: a wiped node rebuilds its index while
`CatchingUp`). Labels as in `architecture-v2.md`: **[F]** read in code or a PR, **[I]** inference, **[V]** verify before
building on it, **[D]** a design choice made here, **[U]** a claim about a program we do not own, proved only by the
end-to-end test named beside it. **[owner]** marks a decision that is the owner's.

## 0. The plan in one page

The owner's constraints: nothing restarts the frontend by itself (a restart fails every build in flight, buck2 never sends an
`Execute` again once its stream broke), and the farm is never idle, so there is no quiet moment. The plan is a queue-based
handover:

1. **Start the new frontend beside the old one.** From then on every new request goes into the shared queue, which is the
   state store, and is not dispatched by the old frontend.
2. **The old frontend finishes what it holds, then stops.** It answers its open `Execute` and `WaitExecution` streams and
   exits when the last one ends.
3. **Storage-and-daemon nodes and daemon-only hosts update one at a time, the moment each is idle**, driven by the new
   frontend: idle, update, rejoin taking queued work. Repeat until every host is new. Macs are daemon-only hosts and follow
   the daemon rule.
4. **Rollback is the same mechanism with the old release set as the target.**
5. Several frontends (Postgres behind a load balancer) are updated one at a time the same way.

What checking this against the handover code found (section 2.9 has the full list): **the queue model needs two frontends
writing the state store at once, and the SQLite backend allows exactly one writer, with a generation fence that locks the
old frontend out the moment the new one opens the store.** So the queue model is the several-frontends model, which
`architecture-v2.md` P9/P10 plans for Postgres and is not built. On SQLite, which is the farm today, the same plan runs with
the writer baton moving once, when the old frontend stands down; the queue is the state store from that instant, and between
the new frontend's start and that instant a request waits in the listener's backlog. Both are one state machine (section
2.2); the node step, the idle definitions, the queue alert, the schema rule and rollback are identical. Decisions: **D1
[owner]** which backend carries the overlap; **D2 [owner]** whether the new frontend dispatches to nodes not yet updated.

## 1. Scope

**Split mode** is a frontend host (REAPI, worker and operator API listeners, scheduler, state store, UI), N
`storage-and-daemon` nodes at R=2 (storage half and daemon half in one process, one cache), and daemon-only machines (the Macs,
a cloud daemon group). `architecture-v2.md` section 11 calls this "our farm".

Not here: **all-in-one** (one process holds the frontend, storage and daemon; its handover needs the storage log and the
in-process daemon handed over too, and has its own design); OS, Xcode, kernel and image updates (`fleet-updates.md`); first
install of a host. The pull updater (a signed manifest, an unprivileged client and a small root helper per host) is the
mechanism that delivers a release to a host and restarts a unit; it is specified elsewhere and unbuilt, and this document
says what it must do for this sequence.

## 2. The sequence

### 2.1 Rules the sequence keeps

- **Never two storage nodes at once, and never a second until the first is `Active` and repaired.** With 6 nodes at R=2 every
  pair of nodes shares about one blob in fifteen, so two nodes down at once loses blobs, not only availability. **[I]**
- **Drain, never kill.** A lease or an `Execute` stream is never ended by an update. A lease killed spends the operation's loss
  budget (`loss_requeues`; the third loss refuses the operation).
- **Every gate fails closed**: an absent metric is a failed gate.
- **The record is written before the action**, so a controller that restarts resumes at the host's observed phase.
- **A failure holds the run.** It does not retry by itself and does not skip the host. A person decides.
- **Capacity beats update**: if an update holds queued builds longer than its limit (2.8) the controller gives the host back.

### 2.2 One state machine, two backends

```text
Idle
 └─ Preflight ─────────── refused: Idle, nothing touched
     └─ Staging              every host fetches and verifies the release in parallel; nothing is installed
         └─ FrontendPhase    start the new frontend; the queue takes over        (2.5)
             └─ NodePhase    idle → update → rejoin, one host at a time          (2.6, 2.7)
                 └─ Settling the old frontend answers its last streams and exits (runs alongside NodePhase)
                     └─ Done
 any phase ── Held (a person acts) | Paused (between hosts) | Pending (a host or the capacity floor says later)
```

The overlap differs by backend, nothing else does:

| | A. SQLite (today) | B. Postgres (P9/P10, not built) |
|---|---|---|
| Writers during the overlap | one: the old frontend until `released`, the new one after (the baton moves once) | both, each fenced per daemon session (`session_gen`) |
| New frontend before it writes | holds connections in the listener backlog; accepts no `Execute` it cannot persist | accepts and queues at once |
| Old frontend after the new one serves | relays its open streams from `done/` rows; dispatches nothing, grants nothing | keeps its daemon streams and its leases until each host updates; claims nothing new (role `draining`) |
| Daemon streams | end at stand-down; daemons redial the new frontend | stay on the old frontend until the host restarts, then reconnect to the new one |
| If the new frontend dies after it serves | no frontend serves until systemd restarts it: a cold start (epoch bump, leases rerun) | the old frontend is still a writer and still serves: nothing is lost |

### 2.3 Who owns each step

| Step | Owner (component) | Needs |
|---|---|---|
| Poll for a release | frontend: update poller (6.2) | read-only access to the release source |
| Preflight | frontend: planner (2.4) | `kbf --version --json` of each tarball; build facts of every running host |
| Staging | each host: update client (`kbf-node`) and root helper, offline verification | session to the frontend |
| Slot, plan, ordering, resume | the **new** frontend: rollout controller (6.1) | `update/*` rows |
| Start new frontend, stand down | frontend host: the helper starts the idle instance; the old process stands down (#879) | units `kbf-frontend@{a,b}` (#887) |
| Cordon, lease count, queue | scheduler (`Farm`), `cordon/<node>` rows (#879) | none new |
| Planned-down, member states | `Members` | operator-settable `down` (2.6, step 3) |
| Open write count, repair debt | storage node: heartbeat fields | not built |
| Activate, confirm, revert | the host's root helper | a signed order in its inbox |
| Rejoin | scheduler: first claim by the node | uncordon |

### 2.4 Preflight

Reads, changes nothing. Any failure ends the run `Idle` with the reasons.

Inputs: the signed manifest per pool and platform, whose `meta` is `kbf --version --json` of the tarball's binary (`commit`,
`protocol`, `min_protocol`, `state_schema`, `profile`, `version`; **[F]** `crates/kbf-cli/src/version.rs`) plus the fields of
3.6; and the same facts of every running host (each reports them in registration). Checks:

1. **Signature and freshness**: chain to the pinned root, pool, platform, expiry, serial above the installed one and the floor.
2. **Every host is known and connected.** A host that reports no build halts the plan with its name; it is never skipped.
3. **Order** (rule in 3.3): the frontend moves first, except a downgrade across a protocol difference, where the nodes move first.
   Refused as a flag day if no order keeps every host inside the frontend's accepted range at every instant.
4. **Schema**: the rule of 3.4. A release whose schema step is not expand-only is refused here.
5. **Handover wire** equal to the running frontend's, else the frontend phase is a cold restart (3.5).
6. **`meta_log`**: if the release raises the nodes' log machine version, node rollback is "wipe and rebuild" (5.2); the UI says so
   before the click.
7. **`unit_rev`**: unit files and flags are installed by a held script, never by the updater. A release that needs a newer
   unit than a host has is refused (the handover refuses a changed listen address or state path, but only after the
   successor started).
8. **Storage gate** G_S (2.7). The metrics it reads do not exist yet; until they do the plan cannot start a node.
9. **Capacity**: with one node out, the rest meet `min_serving` (policy), or the operator accepts the slowdown.

### 2.5 Frontend phase: start the new frontend, move the queue

**[F]** The mechanics are those of `docs/design/frontend-handover.md` (#887). In order:

| # | Step | Owner | Detail |
|---|---|---|---|
| F0 | Gate | controller | no other handover running (the idle unit is `inactive`); the state file is backed up (`VACUUM INTO`); preflight clean |
| F1 | Flip | frontend helper | install the release; flip `current` by atomic symlink rename; **do not restart the serving instance** |
| F2 | Start | frontend helper | `systemctl start kbf-frontend@<other>`; the successor asks the control socket, receives the three listener sockets and the store generation, builds everything that needs no store, answers `ready` |
| F3 | Stand down | old process | GOAWAY on its REAPI connections and close its copy of that listener; wait up to `--handover-quiesce-secs` (10) for calls between request and queueing; fence the farm; end the worker streams; `Store::demote` (stop the writer, release the lock, keep reading); answer `released` |
| F4 | Serve | new process | open the store at the told generation, keep the epoch, serve from the inherited sockets: from here every `Execute` is a queue row |
| F5 | Settle | controller | every daemon and storage member connected at F2 is connected again; the map generation did not move; `kbf_frontend_relay_lost_total` did not rise; a probe cycle passed |
| F6 | Confirm | helper | cancel the revert timer; record `last_good` |

After F6 the run is in `NodePhase` and `Settling` at once. The old process answers its streams from the `done/` rows the new one
writes (and a stream whose operation the store has no trace of for three looks ends `UNAVAILABLE`, counted), then exits. It may
live as long as the longest build. **Nothing may stop it** (not the update, not a package manager); the next frontend handover
waits for it (the command refuses with both instances active).

**What "the queue" is at F3 to F4 on SQLite.** The new process cannot persist anything before `released`, so it does not
accept an `Execute` before then: the connection waits in the backlog, which is the kernel's queue and holds for the gap (normally
under a second, bounded by the 10 s quiesce). Accepting a request into memory would acknowledge what a crash loses, and buck2
does not send it again. On Postgres (backend B) the new frontend accepts and queues from its first serving moment.

### 2.6 The node step: idle, update, rejoin

This is what the node-rolling unit implements. One storage-and-daemon node at a time; the new frontend's controller drives it.
(A daemon-only host runs the same steps minus 3, 4 and 7.)

**Selection.** Candidates are hosts at the old release, not `Held`, with no other host in a step. The controller looks every
heartbeat. A candidate is picked when it is **idle** (2.7) and the gates hold. If none is idle after `idle_wait` (default 2
minutes from F6 or from the last node's uncordon), it picks the candidate with the fewest leases and cordons it, so a farm that
is never idle still drains one node at a time (**D3**, below). Idle nodes are never skipped for a busier one.

| # | Step | What happens | Leaves the step when | If it fails |
|---|---|---|---|---|
| 1 | Claim | CAS `update/slot` to this host, write `update/host/<id>` = `Selected`. Re-check G_S. | slot held | nothing changed |
| 2 | Cordon | `POST /v1/nodes/{node}:cordon`, atomic in the scheduler (and the row persists). A node already at zero leases is cordoned in the same step it is picked, so no lease can arrive between "idle" and "cordoned". | placement `cordoned` | nothing changed |
| 3 | Leases | wait for the node's leases to end by themselves. Idle for the daemon half is zero leases and no claim granted but undelivered. | zero leases | after the limit (15 min Linux): uncordon, `Pending` (capacity beats update) |
| 4 | Planned-down | `POST /v1/members/{id}` `{"state":"down","reason":"restart"}`: writes the `down/<id>` mark with the dwell and bumps the map in one transaction; every router stops choosing this node as a write primary. Wait for the node's open write sessions and the frontend router's writes in flight to it to reach zero. | writes in flight zero | undo (member returns by its next heartbeat), uncordon, `Pending` |
| 5 | Gate | re-evaluate G_S with this node out | true | undo as above; `Held` if a second member is `Down` |
| 6 | Activate | order to the host: the helper writes `pending_confirm`, flips `current`, restarts the unit; the revert timer arms when the new process starts | helper `ACTIVATED_UNCONFIRMED` | the helper reverts itself |
| 7 | Rejoin storage | the new process registers (boot counter larger): `Active` at once if its meta log has `RebuildDone` (#886), `CatchingUp` if it rebuilds (#860; reads answer `UNAVAILABLE`, never `NOT_FOUND`) | `Active` | intact node not `Active` in the health limit (120 s): revert. Rebuilding node: wait while it makes progress, never revert |
| 8 | Repaired | the debts of the downtime are settled | G_S true with this node in | wait up to 5 min, then `Held`, node stays cordoned, no revert (the binary is healthy) |
| 9 | Confirm | order `CONFIRM`; helper cancels its revert timer | `CONFIRMED` | the armed timer fires at its deadline |
| 10 | Rejoin work | uncordon; the scheduler's next claim gives the node queued work | first lease granted to the node, or the queue empty for one heartbeat | node stays cordoned, alert |

Restart deadline: the `down/<id>` mark lives for the dwell (300 s default **[F]**), and a node not back registered by then leaves
the map and its slice is re-replicated. The controller holds the run and alerts at dwell minus two TTLs instead of waiting for
the dwell to decide. **[V]** how long a start takes on a full node; no measurement exists.

**Step 4 is a gap in the code and decides whether builds fail.** **[F]** `kbf-route` sends a write "to the first write owner
that is not `Down` in the map, and to that one only". A member goes `Down` after its row's TTL lapses (15 s after the last
heartbeat). So for up to 15 s after the process stops, writes whose primary is this node fail `UNAVAILABLE`; buck2 retries
an upload 5 times with 100 ms to 5 s back-off (about 1.5 s) and then fails the action. Reads are fine: a refused connection
fails over to the next owner at once. Today `Members::set_state` refuses `Down` (`NotSettable`). **[D]** An operator-settable
`down` with a reason is the smaller change; the alternative, letting the router fail a write over on a refused connection,
changes the write path's "that one only" guarantee and the repair accounting. It must not be `Draining`: that starts the
handoff of the node's slice, a movement of the whole shard for a one-minute restart.

**Storage graceful stop. [V]** The storage half should stop accepting, finish open streams to a bound, then exit (the frontend does
this with `--shutdown-timeout-secs`). Whether the node does on SIGTERM is not established in code I read. Step 4's wait for
zero write sessions makes the stop safe for writes either way; reads that break mid-stream resume on the next owner
(**[F]** `kbf-route`: a read that breaks after its first byte "goes on from the bytes already passed").

**D3 [owner]: pure opportunism or forced drain of one node.** Waiting only for a node to be idle can starve when the farm never
is. Lean: the two-stage rule above (opportunistic for `idle_wait`, then cordon the least-loaded node); it never has more than
one node cordoned, so capacity loss is bounded at 1/N, and it falls back to `Pending` at the limit.

### 2.7 Idle, defined

| Half | Idle means | Source |
|---|---|---|
| Daemon (a daemon-only host, or the daemon half) | zero leases; no claim granted and not yet delivered; for a Mac, no whole-machine hold | scheduler state |
| Storage | **no in-flight writes** to the node, and **the other R=2 copy is serving** | below |

Reads never make a storage node idle and are not waited for: a read in flight finishes or resumes on the next owner.

**G_S, the storage gate** (all true, evaluated with the node marked out):

1. every other member is `Active` and heartbeated within a TTL (a silent member counts as down);
2. the aggregate repair debt is zero, so the other copy of everything this node owns is complete;
3. no member is `CatchingUp` or planned-down;
4. after step 4: the node reports zero open write sessions and the frontend router has zero writes in flight to it.

**[F]** None of these metrics exist: `under_replicated` is only planned in `architecture-v2.md`, repair debts are journalled in
each primary and the frontend is told through a `NoLedger` today, and a node reports no write-session count. So the gate fails
closed and node updates by this path cannot start before the node reports `open_writes` and `repair_debt` in its heartbeat and
the frontend sums them (unit U3).

### 2.8 How long the queue may hold, and the alert

Two holds exist and have separate limits.

- **The handover gap** (SQLite, F3 to F4): connections wait in the backlog. Limit: the quiesce bound, 10 s. The metric
  `kbf_handover_gap_seconds` is set at F4; above 10 s it is an alert, because a client's connect timeout is the next thing to fail.
- **The update hold**: the age of the oldest queued operation while a run is active. Metric
  `kbf_update_queue_hold_seconds{platform}`; the controller reads it every heartbeat.

Policy values (defaults): `hold_warn` 2 minutes, `hold_limit` 10 minutes, and neither is smaller than twice the day's p99 claim
wait. At `hold_warn`: the UI banner and a warning alert. At `hold_limit`: the controller **uncordons the host it is draining**
(capacity beats update), the run goes `Pending`, and a page-level alert fires. A host in step 6 onward is never uncordoned by
this rule (its restart is already running); the rule applies only to steps 2 to 5.

The alert says:

> Update to `<commit>` is holding queued builds: the oldest queued action has waited `<age>` (platform `<p>`, `<n>` queued,
> serving capacity `<x>` of `<y>` nodes). Cause: `<host>` is `<cordoned, waiting for k leases, the longest running d of limit>`
> or `<waiting for n open write sessions>` or `<waiting for repair debt of n blobs on node m>` or `<restarting, s of deadline>`.
> At `<hold_limit>` the update uncordons `<host>` and goes Pending. To act: `kbf update pause`, or `kbf update skip <host>`.

### 2.9 Where the queue model conflicts with #857, #879, #887 and #803

Every conflict found, with what it needs. **K1 and K2 are decisions for the owner (D1).**

| # | Conflict | Detail | Resolution |
|---|---|---|---|
| K1 | **Two writers** | The plan has the new frontend queueing while the old one still dispatches its in-flight work. **[F]** The SQLite backend takes an exclusive `flock` for the life of the process on purpose (`architecture-v2.md` section 4.3: a second frontend on the file refuses to start). `Store::demote` (#857) is the only way the lock moves, and #879 calls it at stand-down. Who is the writer during the overlap: with SQLite there is no overlap, the old one until `released`. | Backend A: the baton moves once; the new frontend does not accept before `released`. Backend B (Postgres): real overlap. See D1. |
| K2 | **The generation fence** | **[F]** #857 appends a check on `meta/generation` to every transaction, and `open_after_handover` takes `g+1`. The old frontend's writes fail `Fenced` the instant the new one opens the store. That is the single-writer guarantee and is incompatible with two live writers, even on Postgres. | In B, replace the generation fence with `session_gen` per daemon session (`architecture-v2.md` section 5: a frontend writes lease changes for a daemon only while its `session_gen` matches). In A the fence is right and stays. |
| K3 | **Daemon streams** | #879 ends all worker streams at stand-down. The plan lets a host stay on the old frontend until it updates. | A: daemons redial the new frontend at stand-down (their leases are kept, epoch kept); the next restart of each is just the update. B: streams stay; the old frontend's worker listener is closed to new connections, existing ones stay. |
| K4 | **The old frontend stops dispatching** | #879 fences the whole farm (`Farm::fence`: queues nothing, accepts no result, grants nothing). The plan wants a frontend that still serves its leases but takes no new work. | A: fence is right. B: a `draining` role: no claim, existing leases and results served. |
| K5 | **Accept and queue before the baton** | The plan: "new requests go straight into the shared queue". **[F]** #879 hands the REAPI listener to the successor at the offer but it serves only after `released`. | A: no accept before `released` (2.5); the backlog is the queue for the gap. B: accept at once. |
| K6 | **`done/` rows and the follower** | Reusable as built. In A they are how the old frontend answers its streams. In B they are how a waiter on one frontend learns of a result committed on the other (`architecture-v2.md` P9). | keep |
| K7 | **Cordon rows** | `cordon/<node>` persists (#879). The node step needs a cordon that survives a reconnect and a cold start. | reuse; test T3 |
| K8 | **The strand** | **[F]** After `released` in A the holder cannot take the store back; a successor that dies there means no frontend until systemd restarts it (cold start, epoch bump). B has no such window. | A: documented, tested (C3); this is the strongest argument for B |
| K9 | **Both instances active** | `kbf frontend handover` refuses when both instances are active (#887). In this plan NodePhase starts at F6, while the old instance is still draining. | node steps do not need the old instance gone; only the next frontend handover waits for it |
| K10 | **Order and the frontend step in #803** | **[F]** #803 plans storage-and-daemon, frontend, daemon, daemon-mac, and its frontend step drains, waits for open streams (6 h) and swaps, a restart. | order: frontend first; the frontend step is F0 to F6; the engine's per-host steps become 2.6; the ssh transport is replaced by the session, the operator API and the helper's inbox; the engine resumes from `update/host/<id>` instead of the top |
| K11 | **Two rollout records** | The in-memory `kbf_types::Rollout` and its `RolloutDriver` in `kbf-server` (made for OS updates) and #803's coexist. | one record: durable `update/*` rows; the old driver's phase names survive as display names only |
| K12 | **Handover wire is an exact match** | **[F]** `wire.rs`: `hello <version>`, holder refuses `version != PROTOCOL`. The one conversation where two releases meet by design. | frozen at 1; growth through optional trailing tokens; a bump is a cold restart (3.5) |
| K13 | **What is not carried over** | **[F]** #879 known limits: memory-ladder history, usage line, recorder state of operations in flight. | accepted; never fails a build; unit U9 carries it |
| K14 | **`released` is irreversible but the helper's revert is not** | If the successor never confirms, the helper reverts `current` and a restart runs the old release as a cold start, which is wrong if the schema moved. | schema rule 3.4: automatic revert only across an expand-only change |

**D1 [owner]. Which backend carries the overlap?**
- **A. SQLite baton.** Built (#857, #879, #887 once merged). The queue is the state store from `released`; before it, the
  listener backlog. Cost: a small gap (seconds at worst), the strand window K8, one writer at a time, so the node phase
  starts only after F6.
- **B. Postgres, true overlap.** The plan exactly as stated: queue first, old drains, nodes migrate one by one, several
  frontends. Needs P9 (pull-claim, `session_gen`, a waiter on any frontend) and P10 (the backend and its two-frontend
  test) first, and replaces the fence (K2). Removes the strand and the gap.
- **Lean: A for the first release, designed so B drops in**: the state machine, the node step, idle, schema rule, alert and
  rollback are the same; B changes only F3 to F5 and the fence. The farm runs one SQLite frontend today, and B's
  prerequisites are weeks away; A is days from merge. Revisit when Postgres lands.

**D2 [owner]. Does the new frontend dispatch to nodes not yet updated?** The plan says updated nodes "start taking work from
the queue". Reading it literally, old nodes get no new work, so capacity falls to the updated set and the queue grows until
nodes update. Lean: **yes, dispatch to old nodes**, because the protocol rule (3.3) already makes it safe and it keeps full
capacity during an update that may last hours. A knob `new_work_only_to_updated` gives the literal behaviour; with it the
queue alert of 2.8 is the safety. In B a node on the old frontend gets only that frontend's work in either reading.

### 2.10 Daemon-only hosts and the Macs

Same steps minus 3-storage details: cordon, wait for zero leases (idle: no leases, no whole-machine hold), activate (a
LaunchDaemon restart through the helper), register `active` at the commit, confirm, uncordon, first claim. A Mac holds no
blob; its loss is capacity only.

- **Opportunistic only by default**: a Mac is updated when it goes idle; the controller does not force-drain one (a Mac lease may
  last an hour: 60 minutes by default). After `mac_force_after` (policy, default off) it cordons the least-loaded Mac as D3 does.
- **A busy Mac is deferred**, the run continues, and "done with N deferred" shows in the UI. This is safe in both orders: a
  Mac at the old release is inside the new frontend's accepted range (3.3).
- **The last Mac of a platform class is updated only when no queued operation needs that class** (`fleet-updates.md`: a node
  that is the last of its class). Otherwise its queue would stall for the restart; the alert of 2.8 names it.
- Quarantine, code signing and TCC grants are the pull updater's concern (a hand-copied binary is refused; ad-hoc signed
  binaries change identity each build, which may reset a grant; **[V]** whether any Mac uses one).

### 2.11 Several frontends

With Postgres behind a load balancer (backend B), each frontend is updated in turn: start the new instance, wait for
`/readyz`, drain the old one (503 and GOAWAY, `POST /v1/frontend/drain`, which is not undone), wait for its streams, stop it.
The load balancer is the listener; nothing is passed. The rollout slot is held by a new-release frontend only: a controller of
the old release checks `update/slot.epoch` and does nothing. Node steps begin once one frontend is new. Frontends of both
releases serve nodes of both releases in the window, so the contract of 3.2 holds in all directions.

### 2.12 Rollback

**Set the old release as the target; the same mechanism walks back.** The controller offers a new release naming the previous
artifacts with a higher serial (the old directories are still on the hosts: running, previous, one more), preflight runs, and:

- frontend: a handover back (the old release is started as the successor); this works only if the contract of 3.2 holds in the
  reverse direction, which is why every row of it says both ways;
- nodes: the node step, in the same order, idle by idle;
- across a protocol difference the order reverses (3.3): nodes first, so no node ever dials a frontend that does not accept it.

Not reversible, and said so before the click: a release that changed the schema beyond expand (3.4), and a node log raise.
**A node whose log was raised** cannot be reopened by an older binary (**[F]** `MachineVersion::format()` is `None` for a version
it does not know); the walk-back wipes that node's meta directory and rebuilds from segment footers (#860). **[V]** that the
action-cache entries that only that node's log held are on the other owner (#860: the action cache "is not rebuilt").

## 3. Mixed versions

### 3.1 Windows

| Window | Frontend | Nodes | Lasts |
|---|---|---|---|
| W1 (handover) | holder N and successor N+1, one store (A) | N | seconds, then the holder serves leftover streams from rows |
| W2 (node phase) | N+1 | k at N+1, the rest at N | hours |
| W3 (deferred hosts) | N+1 | a busy Mac at N | until it updates |
| W4 (rollback) | each of the above reversed | | |
| W5 (B only) | N and N+1 both writing | N | the whole run |

### 3.2 The compatibility contract

Between a release and the one before it, **in both directions**, because rollback and the handover back run the windows
backwards.

| Surface | Between | Rule across one step | Now |
|---|---|---|---|
| Worker stream (`kbf.worker.v1`) | daemon and frontend | The server accepts `min_protocol..protocol` and answers `Welcome` with the daemon's own version (**[F]** `worker.rs`); the daemon accepts only `Welcome.protocol_version == PROTOCOL_VERSION`. A new frontend serves old daemons; a new daemon is refused by an old frontend. Changes are field and oneof-arm additions only. **[V]** an old daemon ignores a `ServerMessage` arm it does not know. | holds for additive changes; no test sends an unknown arm |
| Membership (`kbf.map.v1`) | node and frontend | `Register` refuses a protocol below the frontend's floor (**[F]** `members.rs`) and has no ceiling | one-sided |
| Storage RPCs and REAPI through the map | frontend and node, daemon and node, node and node (chain replication, repair, read repair) | every storage RPC works between N and N+1 in both roles, because an R=2 pair spans the window | no cross-version test |
| Operator API, UI JSON | tools and frontend | additive | by convention |
| State-store **row encodings** | frontend N+1 and holder N (and, in B, both writers) | the holder, demoted to a reader, reads `done/`, `op/`, `lease/` and queue rows the successor writes, for three looks each. In B each reads every row the other writes. Frozen across one step. | not stated anywhere |
| SQLite **schema** | frontends only | rule 3.4 | strict equality, no migration |
| Handover control wire | holder and successor | frozen (K12) | exact match |
| Node meta log (`MachineVersion` V1 to V4) | one node, across its restart | one-way raise at start | no declared field |
| Cold-store objects and footers | any nodes | a node adopts older segments (#860); **[V]** an older node tolerates newer objects | unknown |
| Unit files, flags, certificates | host and release | not updated by the pull path; a release declares `unit_rev` | no declaration |

### 3.3 The order rule

At every instant each node's `protocol` must lie in the frontend's accepted range `[min_protocol, protocol]`. So:

- **Upgrade** (default): frontend first. The new frontend accepts old nodes iff `new.min_protocol <= oldest running protocol`.
  A new `protocol` is fine: old nodes are inside the range.
- **Downgrade across a protocol difference**: nodes first, then the frontend: otherwise the old frontend would refuse the
  nodes that are still new.
- **No order satisfies it**: a flag day. The planner refuses and prints the numbers.

This is simpler than the earlier "nodes first" plan: nodes can never be ahead of the frontend on an upgrade.
**[D]** `architecture-v2.md` section 11 says "storage one at a time, frontends, daemons"; one sentence of it changes when this is accepted.

### 3.4 The schema rule: expand then contract

**Chosen: expand/contract** (as `architecture-v2.md` section 11 already states), not a version gate. **[F]** Today
`schema.rs` accepts a file only when `user_version` equals the binary's, so the new frontend refuses the old file, and a
demoted holder or a rolled-back binary refuses a migrated one. Design:

- The file records `rev` (the newest expand applied) and `floor` (the oldest binary revision that may still open it). A binary
  has its own `REV`. It opens a file iff `floor <= REV`; if `rev < REV` it applies its expand steps in one transaction and
  sets `rev`. A binary older than `rev` opens the file unchanged because expand steps only add (columns, keys, tables with
  defaults) and a binary ignores what it does not know.
- **Contract** (dropping or reinterpreting) raises `floor`. It runs only by an explicit command, never on open, and only in a release
  after every frontend and every holder is at or above the new floor; preflight refuses a contract release while a running
  frontend reports a lower `REV`.
- Row encodings follow the same rule: a new field is read by the old release as absent; an encoding change is a contract.
- Rollback is therefore always safe until the contract step runs, and the UI marks a contract release "not reversible".

Why not a version gate (new behaviour off until every host reports the new version): a gate does not solve the first
open, when the new binary must change the file while the old one still reads it; it needs every host online and known to
raise (a silent Mac blocks it); and once raised it cannot be lowered, which removes rollback. Expand/contract needs none
of that. The rare release that changes behaviour (not shape) ships the behaviour dormant behind a policy row and a later release
flips it: the same "expand first, activate a release later" rule, not a second mechanism.

### 3.5 What is not a handover

Refused by preflight unless the operator accepts failing builds (`--accept-failing-builds`, typed reason, `admin`): a
contract schema change; a handover-wire change; a change of listen, worker or API address or state path (**[F]** the holder
refuses). The cold path is `systemctl stop` of the serving instance (it drains: refuses new `Execute`, keeps open streams,
no deadline) and a start of the other.

### 3.6 What `kbf --version --json` adds

Today: `commit`, `min_protocol`, `profile`, `protocol`, `state_schema`, `version`. Add: `handover` (wire version), `meta_log`
(the highest log machine version the binary raises to), `unit_rev`, and the schema pair `rev` and `floor` in place of
`state_schema`. A test pins that each is reported.

## 4. What a client sees

### 4.1 Buck2 and the frontend

**Facts about buck2 [F]** (`remote_execution/oss/re_grpc/src/client.rs` on `main`, read for this document; #791 here read the same file
earlier):

- `execute_with_progress` retries only opening the stream: `MAX_ATTEMPTS` 5, 100 ms doubling to 5 s, with jitter. A stream that breaks
  after it started is returned as an error and the action fails (`remote_call_error`).
- `is_retryable` is true for `tonic::Status` `Unavailable`, `ResourceExhausted`, `Aborted`, `Cancelled`, for transient `io::Error`
  kinds (`BrokenPipe`, `ConnectionReset`, `ConnectionAborted`, `UnexpectedEof`, `TimedOut`) and for an `h2::Error` wrapping one.
- It never calls `WaitExecution`. A broken `Execute` is not re-attached.
- Blob uploads and downloads (ByteStream write and read, batch update and read, `GetActionResult`, `FindMissingBlobs`) are retried by
  the same helper; a ByteStream retry runs the whole blob again.
- It has no GOAWAY-specific handling.

**HTTP/2 and tonic [F]:** after GOAWAY, streams at or below `last-stream-id` run to completion (RFC 9113). The h2 server's
`graceful_shutdown` documentation says "after about 1 RTT, no new requests should be accepted", "it's possible to receive more
requests after calling this method", and the connection closes once all active streams complete. tonic maps `REFUSED_STREAM` to
`Unavailable`, `CANCEL` to `Cancelled`, `NO_ERROR` to `Internal`, anything else to `Unknown`; hyper turns an RST with `NO_ERROR` or
`CANCEL` into a clean end of stream.

**[U] The race is where buck2 can fail a build.** A request sent just as the GOAWAY lands is refused with `REFUSED_STREAM` (tonic
`Unavailable`: retried), or surfaces `NO_ERROR` from the GOAWAY (tonic `Internal`: not in buck2's list, the action fails), or is
cancelled before it was sent (`Cancelled`: retried). I could not establish which one the pinned buck2 produces from the sources.
#887's claim that such a call "gets UNAVAILABLE at open, which Buck2 asks again" has no test. **T6 is the proof.** If red, the
fallback is a two-step GOAWAY (first with the maximum stream id, then a PING round trip, then the final one: the sequence RFC 9113
describes) in `drain.rs`/`handover/outgoing.rs`; no change to buck2.

### 4.2 An `Execute` in flight

**[F]** It stays open and is not told to go away. The old frontend answers it from `done/<op>` rows the new one writes (the follower),
shows "executing" while the operation holds a lease, and ends `UNAVAILABLE` only when the store has no trace of the operation
for three looks (`kbf_frontend_relay_lost_total`). That counter is the count of builds a handover failed and must be zero in every
test but the planted one. A `WaitExecution` is answered the same way. Operation names carry the store generation (**[F]** #857),
so a name is never reused. An operation queued but not leased shows what it showed; it is dispatched by the new frontend.
**Lost, accepted:** memory-ladder history, usage line, recorder state of operations in flight (K13).

### 4.3 A new call

Before `released`: served by the old frontend. After GOAWAY: the client opens a connection; it waits in the backlog until the new
frontend serves (A) or is accepted at once (B). Whether the call that raced the GOAWAY is retried is U1/T6.

### 4.4 Daemon worker streams and storage nodes

**[F]** In A, at `released` the old frontend ends the worker streams; each daemon redials the same address and reaches the new
frontend. The epoch is kept: a daemon keeps its leases and runs and sends again the results the old frontend did not
acknowledge. A `Result` taken at the moment of stand-down is waited for (`Farm::quiet`); no test holds one in flight (T5 does).
In B the streams stay until the host restarts.

Storage nodes hold no stream: `Register` and `Heartbeat` are unary and membership lives in the store with a 15 s TTL, so a
reconnect to the new frontend loses nothing. A gap past the TTL flaps members `Down` and re-routes writes; T5 asserts the map
generation is unchanged across a handover.

### 4.5 A node update as buck2 sees it

Invisible except through storage calls to that node: reads fail over to the other owner; writes avoid the node from step 4;
an upload mid-stream on it at the stop is retried whole by buck2 (**[F]**, 5 attempts). **[U]** that it completes within the
attempts; T1 measures it. An `Execute` queued because capacity is short keeps its stream open and gets progress every
`--progress-interval-secs`.

## 5. Failure handling and rollback

### 5.1 Principles

- A host reverts **itself** from local state only: a binary that never becomes healthy is put back by the helper's timer or boot
  counter (the window starts when the new process starts, not at the flip, and is capped by the helper's own config).
- The controller **holds** on any failure and touches nothing else. It never retries a revert by itself (a revert from a frontend
  outage and one from a bad build look alike from the host).
- An error from the activate order counts as "swapped, maybe": revert, never retry.
- Rollback is 2.12.

### 5.2 Per step

| Step and fault | What the system does | Operator |
|---|---|---|
| Preflight refuses | `Idle`, reasons, nothing touched | fix the release or take the cold path |
| Staging fails on a host | that host `REFUSED`; the run cannot start | free disk or re-offer |
| F2 successor fails before `ready` (flags, certificates, probes, stopped) | the old frontend serves as it was; `current` reverts | journal, fix, re-offer |
| F3/F4 successor dies after `released` (A) | no frontend until systemd restarts it: cold start, epoch bump, leases rerun; the old process still answers its streams from rows; repeated failure reverts `current` and the cold start runs the old release, **only if** the schema change was expand-only | journal; do not revert if a contract ran |
| Old frontend dies mid-handover | its open streams end as for any crash; the successor proceeds (the kernel freed the lock) | rerun those builds |
| F5 daemons or members do not return | no confirm; the timer fires; the revert is a handover back | |
| Old frontend dies while Settling | the builds on its streams fail; the new frontend is unaffected | rerun; unit U8 |
| Node, step 3 leases do not end | uncordon at the limit, `Pending` | |
| Step 4 or 5 gate fails | undo planned-down, uncordon, `Pending` or `Held` | |
| Step 6 binary exits at once | boot counter reverts before the Nth start; `Held` | journal |
| Step 6/7 starts, never registers | window expires, helper reverts, `REVERTED`, `Held` | |
| Step 7 stays `CatchingUp` | wait while rebuild progresses; a failed rebuild retries with back-off (**[F]** #860); past the rebuild limit `Held`, node stays cordoned, the other owner serves | decide |
| Step 8 debt does not settle | `Held`, cordoned, no revert | look at the other owner |
| A second member goes `Down` unplanned | the controller stops at once, restarts nothing; blobs owned by both are `UNAVAILABLE` until one returns | bring it back first |
| Restart deadline passes | `Held`, alert; the dwell may start re-replication | |
| Confirm lost | the timer fires; node returns to N; `Held` | |
| Revert fails (previous missing) | node stays as is, cordoned, `REFUSED`, page-level alert | manual path |
| Controller (frontend) dies mid-run | on start it reads `update/host/<id>` and resumes; the cordon row persists | |
| Frontend host loses power | everything on it fails; cold start; running actions are re-sent after the epoch | accepted: one frontend is a scheduling single point of failure |

### 5.3 The crash matrix for split mode

For the unit that builds the real-process crash matrix. A case is a SIGKILL, stop, full disk or power cut at the point named,
with real processes. IDs are stable so tests and PRs cite them.

**Frontend:**
- C1 kill the old frontend before `ready`: the successor takes the lock when the kernel frees it and starts cold; builds on the old streams fail.
- C2 kill the successor before `ready`: the old frontend serves; `current` reverts.
- C3 kill the successor after `released`, before it serves (K8): systemd restarts, epoch bump, actions rerun, no operation name reused, the old process answers its streams.
- C4 kill the old frontend after `released` while its follower holds streams: those streams end, the new frontend serves the rest.
- C5 a `Result` in flight at the stand-down: neither lost nor applied twice.
- C6 a new `Execute` every few milliseconds across the handover: zero failures, retries counted (T6).
- C7 the old frontend stuck in quiesce for the full bound: stands down anyway.
- C8 `Store::demote` fails: `failed`, the successor aborts, the old keeps serving.
- C9 control socket missing or another user: refused while something serves.
- C10 both instances active, a second handover: refused.
- C11 flags differ: refused with the reason.
- C12 power loss at each of F1 to F4.
- C13 helper dies between the flip and the start of the other instance: reconciles on restart.
- C14 handover N to N+1 then back with a build in flight.
- C15 **queue gap**: the backlog holds a connect longer than the client's connect timeout: counted, alert fires at 10 s.
- C16 the frontend is killed with queued, undispatched operations: the rows survive; after the cold start they are dispatched; their original streams are gone (those builds fail as for any crash).

**Node:**
- C20 the controller dies after cordon, after planned-down, after activate, after confirm, before uncordon: resumes at the host's phase; no host activated twice.
- C21 the helper dies at each boundary of activate: converges to old or new, never a mix.
- C22 the node's process dies repeatedly after the restart (boot counter), and once after `Active` before confirm.
- C23 power loss of the node during the restart; the dwell expires: slice re-replicated; run held; nothing else restarts.
- C24 a second node killed while the first is planned-down: reads `UNAVAILABLE` not `NOT_FOUND`; controller stops; recovers; no blob lost.
- C25 a lease arrives between "idle" and cordon: impossible by construction (cordon is the pick); the daemon also refuses an activate order with a lease.
- C26 disk full while staging: installed untouched, `REFUSED`.
- C27 intact index: `Active` at once; wiped: `CatchingUp`, reads `UNAVAILABLE`, controller waits for the right one.
- C28 the frontend restarts cold during the node phase: cordon restored from rows, member row survives, heartbeats continue.
- C29 debt metric absent or a member silent: the gate refuses.
- C30 a long log replay on first start: the restart deadline is declared from history.
- C31 write sessions never reach zero (a stuck upload): `Pending` at the limit, node returned.
- C32 the queue hold passes `hold_limit`: the host is uncordoned, the alert says why.

**Daemon-only:**
- C40 a Mac lease outlives the wait: deferred, not killed, the run goes on.
- C41 the last Mac of a class with queued work for it: not picked.
- C42 helper dies mid-activate: converges; cordon stays until the controller uncordons.

**Mixed:** C50 each window of 3.1, both directions of 3.2, with a build running through it.

### 5.4 The residual risk, stated

Still able to fail a build: the old frontend crashing while it holds streams (C4); a successor that dies between `released` and
serving and a client that sent its `Execute` meanwhile (A only; it queues and runs, its stream does not survive a cold start);
a GOAWAY race buck2 does not retry (U1); a host power loss. Each has a metric the tests assert on.

## 6. Who drives it

### 6.1 The controller

**[D]** One driver: a rollout controller in the frontend process (the crate that holds #803's engine), working through the
operator API and the hosts' sessions. The CLI and the UI are its clients. The manual path (a human runs the installer verbs and
`kbf frontend handover`) stays as the fallback; it calls the same installer code.

State in `kbf-state` (one CAS row each; works on SQLite and Postgres):
- `update/desired {serial, track, paused}`;
- `update/slot {holder, order_id, epoch, state}`: no TTL; if the holder dies the slot stays held and the run is `Held`;
- `update/host/<id> {phase, order_id, serial, since}`: what the controller resumes from;
- `update/policy {auto_nodes, auto_frontend, idle_wait, hold_warn, hold_limit, min_serving, new_work_only_to_updated, mac_force_after, accept_failing_builds}`;
- `update/available/<commit>` from the poller.

The controller that runs is the new release's. In A the old frontend's controller is fenced with the rest of the farm; the new
one starts at the host phase it reads and writes `Settling` at F6, so it never repeats the handover it is the result of.

Orders to a host are files in its helper's inbox, written by its unprivileged `kbf` after an order over its session; the helper
verifies offline. **[F]** The messages (`UpdateOffer`, `UpdateOrder`, `UpdateStatus`) are not in `worker.proto` yet. Requirements
added here: an order carries the store epoch and a monotonic id (a restored store cannot replay one; a host refuses a lower id),
a host refuses an activate order while it holds a lease, and the frontend host's `activate` is "flip and start the other
instance" (one fixed argument named by the host's own config), not a restart of the serving one.

### 6.2 The poller

**[D]** A task in the frontend that finds a new attested release and only ever proposes it:

- every few minutes (a flag, default 5), one read-only request per cycle with a conditional request (`ETag`), so a quiet cycle is a `304`;
  the credential, if any, is a read-only token read from a file (a flag, not an environment variable), never logged or stored in a row;
- it looks for the newest successful run of the artifacts workflow on the main branch, and for each platform the farm needs, the
  tarball, `SHA256SUMS` and the attestation bundle;
- it verifies the attestation in-process against a pinned repository and workflow identity, and the tarball's hash, before anything
  is stored; a failure is never offered and an alert names it;
- it reads `kbf --version --json` from the binary (service user, empty environment, no network, a timeout) and rejects one whose commit differs from the run's;
- it runs preflight against the live farm and stores the result, so the UI shows blockers without a click;
- it stores `update/available/<commit>` (hashes, `meta`, attestation time, preflight result, the merge commit subject) and keeps the last few;
- **it never applies.** `update/policy` decides. `auto_nodes: manual` (default) offers; `auto` runs the whole sequence's node steps when preflight is
  clean. `auto_frontend` is separate, defaults to `manual`, and cannot be set to `auto` unless `auto_nodes` is `auto` and the last two runs finished
  without `Held`. A frontend handover is never started by a timer unless an authenticated operator wrote that row;
- it refuses a downgrade (by serial), a commit not on main, and a run with a failed or missing attestation;
- failures it survives: the source unreachable (the offer stays; the UI shows the last good poll; an alert after an hour), a rate limit
  (back off), a malformed answer (log once an hour), a restart (state in rows), a handover (the new frontend starts its own poller; a poll in flight is dropped).

### 6.3 The update page and button

A server-rendered `kbf-ui` page (**[F]** today read-only, no script, over the read API) with a new write path.

**Shows** (from `GET /v1/update`, the JSON twin):
- per host: running release (commit, serial, protocol, since), the available release (commit, attested at, polled at), a "mixed" badge;
- the plan and preflight verdict: order chosen and from which numbers, next host, and **blockers** each with a reason and a fix:
  `schema change needs the cold path`, `release needs node rollback by rebuild`, `repair debt N blobs on <node>`, `a host reports no
  build`, `old frontend still draining 3 streams`, `a member is Down`, `last Mac of its class has queued work`;
- the run: one row per host with its step (`selected`, `cordoned, waiting for 2 leases`, `waiting for 1 write session`, `restarting`,
  `rebuilding 61%`, `waiting for repair, debt 12`, `confirmed`, `held: reason`), time per step, the frontend phase and the old
  process's open stream count (Settling), and `queue hold`, `relay_lost`, `failed builds during the run`, map generation;
- the policy rows, who changed them last and when.

**Does** (a form with a confirm step that names the plan):

| Button | Triggers | Role | Disabled when (reason shown) |
|---|---|---|---|
| Update | `update/desired` + the whole sequence per policy | `update` | blockers; run active; nothing available |
| Update frontend only | F0 to F6 | `update` | blockers; old instance still draining |
| Update nodes only | node steps | `update` | frontend not new; blockers |
| Pause, Resume | `update/desired.paused` | `update` | no run |
| Skip host | marks a `Held` host failed, frees the slot | `admin` | not `Held`; says the host stays cordoned |
| Roll back | a release naming the previous one | `update` | not reversible (2.12), with the reason |
| Accept failing builds | the cold path | `admin` | never a default; a typed reason |
| Policy | `update/policy` | `admin` | |

The confirm page shows the plan **by digest**; the POST names it; the controller refuses if the plan changed since the page was drawn.

**Auth.** **[F]** The operator API's gates reject a browser by design (loopback peer, no `Origin`, bearer token, JSON content type), and
`fleet-updates-security.md` plans `admin` and a narrow CI `rollout` role (the code has only `Daemon` and `Client`). A button needs a
UI write path first: a session the UI mount makes from the operator's credential (**[V]** which one the fleet uses), a per-session CSRF
token in every form, `SameSite=Strict`, a server `Origin` check, and an audit record naming the actor on every write. **[D]** Roles:
`read` (the page), `update` (the buttons above except skip and accept), `admin` (everything). A CI `rollout` role may offer a release and
may not start the frontend phase or change policy.

## 7. Test plan

End-to-end tests in `kbf-it` (a new cell, `kbf-cell upd`, beside `m1` and `m2`): real `kbf` processes, the pinned buck2,
`//:pause`-style actions, a `file://` cold store, 3 storage-and-daemon nodes at R=2, one daemon-only process, the frontend as
`@a`/`@b` processes with the control socket (the unit text is covered by the existing unit lints). A second cell runs the base
binary and a test-only "N+1" built from the next commit for the mixed-version tests. Each test names the defect and the mutant to plant.

| # | Test | Proves | Defect (mutant) |
|---|---|---|---|
| T1 | `rolling_nodes_under_build_load` | all 3 nodes updated idle by idle while buck2 builds and uploads: zero failed builds, zero `NOT_FOUND`, boot counter +1 per node, never two members non-`Active` at a sampled instant, `lost_blobs == 0` | writes failing in the TTL window (skip planned-down); two nodes at once (drop the slot CAS); `NOT_FOUND` during rebuild |
| T2 | `idle_is_defined_per_half` | a daemon is picked at zero leases and not before; a storage node is not picked with an open write session or with debt; with the other copy's node silent it is not picked | picking on lease count alone (storage); picking with debt |
| T3 | `cordon_survives_restart_and_rejoin_takes_work` | the node comes back cordoned, takes no work until uncordon, then takes queued work (first lease observed) | reconnect clears the cordon; uncordon without a claim |
| T4 | `node_that_never_becomes_active_holds` | exits at once, never registers, stays `CatchingUp`: boot-counter revert, window revert, hold; other nodes' pids unchanged; builds still pass | missing revert timer; continuing after a failure |
| T5 | `frontend_handover_with_builds_in_flight` | 20 concurrent 30 s builds, a handover with a `Result` held in flight at the stand-down: all succeed, `relay_lost == 0`, epoch and map generation unchanged, no `loss_requeues` | fence after demote; result applied twice; follower look count not reset |
| T6 | `new_calls_across_the_handover` | the pinned buck2 starts a target every 50 ms across the handover: zero failures, retries recorded. **U1's proof** | GOAWAY semantics buck2 does not retry (close instead of GOAWAY → red); successor not accepting from the backlog |
| T7 | `queue_gap_and_queue_rows` | the gap metric is set at F4 and stays under the bound; queued, undispatched operations survive a cold start (C16) | accepting before `released`; queue rows not durable |
| T8 | `queue_hold_alert` | saturate the farm during a node cordon: warn at `hold_warn` with the text of 2.8, uncordon and `Pending` at `hold_limit` | no limit; alert text missing the cause |
| T9 | `frontend_crash_matrix` | C1 to C16, one test each | each case's own defect |
| T10 | `mixed_versions_both_ways` | base and N+1 in every window of 3.1, both directions: N+1 frontend with N nodes, N with N+1 (downgrade order), handover N to N+1 and back, with a build in flight | made-required field; repurposed field; `done/` row tag change; handover wire bump; unknown `ServerMessage` arm crashing an old daemon |
| T11 | `schema_expand_contract` | an N+1 that expands: the N holder still reads, rollback opens the file, the contract release is refused while an N frontend runs | strict-equality open; contract on open |
| T12 | `preflight_refuses` | table: unsigned, older serial, expired, wrong platform, contract schema, protocol flag day, handover mismatch, absent debt metric, silent member, host with no build: each refused and **no process touched** | gate that fails open (absent metric as zero) |
| T13 | `controller_restart_resumes` | kill the frontend cold after cordon, after planned-down, after activate, after confirm: resume at the host's phase, nothing activated twice | engine starting from the top |
| T14 | `second_loss_stops_the_run` | an unplanned node killed while another is planned-down: stop, restart nothing, `UNAVAILABLE`, recover | continuing; counting only planned members |
| T15 | `mac_and_last_of_class` | a busy daemon-only host is deferred; the last of a class with queued work is not picked | killing a lease; stalling a class |
| T16 | `rollback_walks_back` | offer the previous release: frontend by handover back, nodes idle by idle, builds pass; across a protocol difference nodes go first; a contract or log raise says not reversible / rebuilds | rollback restarting the frontend; wrong order |
| T17 | `poller` | a fake source: newer attested release offered not applied; bad attestation/hash not offered; unreachable shows last good and alerts after the hour; `304` is free; `auto_nodes` never starts the frontend phase; downgrade refused; restart keeps state | applying without policy; unverified artifact stored |
| T18 | `update_page` | every state of 6.3 renders; a blocker disables the button with its reason; POST needs CSRF, same-origin, role and plan digest; a stale digest is refused; every write audited | CSRF missing; enabled with blockers; a changed plan applied |
| T19 | `cold_path_refusals` | contract change or wire change refused unless `--accept-failing-builds`; with it, stop-drain-start leaves open streams to finish | silent cold restart |

T5 and T6 prove the model's one unproven step: if either cannot be made reliable the model is wrong and that is reported, not worked around.

## 8. Open questions

**[owner]** marks the owner's call; the rest are decided here and open to correction.

- **D1 [owner]** backend for the overlap: A now, B when Postgres lands (2.9). **Lean A.**
- **D2 [owner]** dispatch to not-yet-updated nodes (2.9). **Lean yes**, knob for the literal behaviour.
- **D3 [owner]** forced drain of the least-loaded node after `idle_wait` (2.6). **Lean yes**, one node at a time, `Pending` at the limit; without it a never-idle farm never updates.
- **Q1 [owner] Who signs.** The pull updater has an operator sign each release offline and hosts verify against a pinned root, so a compromised frontend cannot install anything unsigned. The direction here has the frontend poll and apply on a click, which needs a signer: (a) the frontend verifies the attestation and signs with a key it holds (one click; a frontend compromise is a fleet compromise); (b) the operator signs on their machine (strongest; "available" means attested, not signed, one command); (c) a separate signer. **Lean: (b) ships first; (a) opt-in per deployment.** A signed binary runs as the service user, which on a storage-and-daemon node holds the cold-store credential.
- **Q2 [owner]** `auto_frontend`. **Lean: manual until T5, T6 and C1 to C16 have been green nightly for a month**; the poller never changes it.
- **Q3 [owner]** Macs deferred when busy so one long UI test cannot hold the run (2.10). **Lean yes.**
- **Q4** State-schema migration (3.4) is a `kbf-state` change of a few days and is on the critical path of the first release that changes a column. Until then the planner refuses such a release.
- **Q5** Planned-down member state (2.6, step 4): **lean add it**; the router-failover alternative changes the write guarantee.
- **Q6** Node graceful stop and a `Leave` call. **Lean yes.**
- **Q7 [owner]** `auto_nodes` default `manual` until two click-through runs are clean.
- **Q8** One rollout record (K11). **Lean durable `update/*` rows.**
- **Q9** A nightly mixed-version job against the release before the last tag (T10), a precondition for auto.
- **Q10** The handover wire frozen at 1 (K12); say so in the doc that owns `handover.rs`.
- **Q11 [owner]** `--accept-failing-builds`: **lean keep**; without it a contract release has no way out.

## 9. Units, in dependency order

Each lands a test that fails first. Sizes (S under 1 day, M 1 to 3, L 3 to 5 days of agent time) are estimates.

| Unit | What | Size | Depends on |
|---|---|---|---|
| U0 | Land #857, #879, #887, #860; bring #803's engine into the controller's crate | merge | review |
| U1 | The probes first: T6 (buck2 and GOAWAY), then T5 and T7 | S-M | U0 |
| U2 | `--version --json` fields (3.6); manifest `meta`; planner (order rule, refusals) | M | U0 |
| U3 | `open_writes` and `repair_debt` in the heartbeat; frontend aggregate and G_S; operator `down` (step 4); node graceful stop | M-L | U0 |
| U4 | `update/*` rows, the controller with resume, the real transport, the node step 2.6 and idle 2.7 (**the node-rolling unit**) | L | U2, U3 |
| U5 | Frontend phase on the helper: `activate` for two instances, F0 to F6, Settling | M | U4, pull-updater helper |
| U6 | Queue hold metric, alert and the uncordon rule (2.8) | S | U4 |
| U7 | The crash matrix of 5.3 with real processes | L | U4, U5 |
| U8 | State-schema expand/contract (3.4) and the row-encoding freeze test | M | none |
| U9 | Reduce the old frontend's exposure while Settling; carry per-operation memory across the handover or record the loss | M | U5 |
| U10 | Poller and policy | M | U2 |
| U11 | UI write path, page and buttons | L | U4, U10 |
| U12 | Mixed-version nightly and rollback test | M | U5 |
| U13 | Backend B: replace the generation fence with `session_gen`, a `draining` role, accept-and-queue in the new frontend | L | P9, P10 of `architecture-v2.md`, D1 |

What this document does not claim: that any of it is built; that buck2 retries a request that races a GOAWAY (T6); that a node's
stop is graceful (Q6); that the repair gate has a metric to read (U3); that a restart fits inside the dwell (C30); or that the
pull updater exists.
