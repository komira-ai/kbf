# Updating the frontend without failing a build

`kbf frontend handover` starts a new frontend beside the serving one, behind Envoy. The new
one binds listeners of its own, takes the state store and the daemons over, and serves;
Envoy sends new streams to it once it is ready; the old one goes not ready, forwards the
calls Envoy still sends it, finishes the builds it holds and exits. This page says what
each party sees. The protocol between the two processes is `kbf_server::handover`
(`architecture-v2.md`, sections 11 and 12; the design is `aio-handover.md`); the unit and
the command are here.

## The two instances and their ports

Both instances run the same command line but for `--instance=a|b`. Instance a listens where
`--listen`, `--worker-listen` and `--api-listen` say; instance b listens ten ports above each
(port 0, which the system chooses, stays 0), so the two blocks are `base+0..2` and `base+10..12`,
the ones Envoy's configuration (`kbf-deploy envoy render --topology box`) names. A handover
goes to the other instance. The two REAPI listeners must be on loopback, since the old one
forwards the calls that still reach it to the new one. If the REAPI listener has a
certificate, `--handover-peer-name` (the DNS name the other instance's certificate carries) and
`--storage-ca` let the old one verify the new one; without them it refuses a handover.
A frontend without `--instance` refuses every handover.

Envoy's health-check settings are flags (`--envoy-check-interval-ms`, `--envoy-check-timeout-ms`,
`--envoy-unhealthy-threshold`, defaults 1000, 1000 and 2, the box render's), because the old
instance derives from them how long Envoy takes to see the new one healthy and to stop choosing
the old one: `interval x threshold + timeout + interval`, four seconds by default.

## The unit: `kbf-frontend@.service`

One template, two instances, `kbf-frontend@a` and `kbf-frontend@b`, with the same command
line. Only one serves at a time. `linux install-units --mode frontend` writes the template,
enables it (`DefaultInstance=a`: only `@a` starts at boot) and starts `@a` if neither
instance is up. A serving instance is never restarted by an install: the plan says to run
the handover instead. What the unit has, and why (check-units rule `handover`; rule `stop`
still asks for `TimeoutStopSec=infinity` and `KillSignal=SIGTERM`):

- `Type=notify`: kbf sends `READY=1` once it serves and its state store and map probes
  pass, so `systemctl start` of the idle instance returns when the new frontend can take
  builds, and fails when it could not. It does not wait for the `--storage` node: at a cold
  power-up the node may come up after the frontend, and a frontend that waited would be
  killed at `TimeoutStartSec` and restart with a new epoch, in a loop. The successor of a
  handover likewise tells its predecessor it serves without the storage probe. `/readyz`
  still fails with `storage` listed until the node answers: Envoy sends nothing to a
  frontend that would fail every blob call, and the frontend keeps its listeners and its
  state, so it is in rotation again as soon as the node is back (no restart, no new epoch).
- `TimeoutStartSec=120`, finite: a new instance that never says ready fails the start.
- `--handover-socket=/run/kbf-frontend/control.sock`, in `RuntimeDirectory=kbf-frontend`
  with `RuntimeDirectoryPreserve=yes`: the socket's directory outlives the instance that
  stops. Both instances run as the same user (the socket checks the peer's uid).
- `--instance=%i` and the per-instance port block are not in the template yet: the unit
  changes that carry them are separate from the frontend's behaviour described here, and
  until they land a handover is refused ("started without --instance").
- No `Conflicts=` between the instances: starting one must never stop the other, which is
  the one draining.

## The command

```
kbf frontend handover [--systemctl PATH] [--wait [--poll-ms N]]
```

1. It reads each instance's `ActiveState` (`systemctl show -p ActiveState --value`). Exactly
   one `active`: that one serves. None: nothing serves, and it starts `@a` (a first start,
   or a start after a crash). Both `active`: an earlier handover's old instance is still
   draining its builds, and the command refuses (exit 1) and starts nothing; run it again
   when the old one has exited. An instance `activating` or `deactivating` is refused the
   same way.
2. It runs `systemctl start` on the idle instance and waits for it. The new binary must be
   in place first, put there by rename (a running frontend keeps its old file); the updater
   does not restart what it replaces.
3. On success it prints that the new instance serves and the old one drains and exits by
   itself, and exits 0. With `--wait` it then looks at the old instance every `--poll-ms`
   (1000) until it is no longer active: exit 0 for a clean exit, 1 if it ended in failure.
4. If the start fails it exits 1 and says whether the old instance is still active. A
   successor that fails before it said `ready` changed nothing: the old frontend keeps
   serving. The only other end is the one in "When it goes wrong" below.

The only verbs it runs are `show` and `start`: it never stops, restarts or kills.

## What each party sees

**Buck2 builds in flight (open `Execute` and `WaitExecution` streams).** They stay open and
are not told to go away: no GOAWAY is sent in an update, and Envoy keeps the connection. The
old frontend answers each from the `done/` row the new one writes when the action finishes, so
Buck2 receives the result of its action as it would have from one process. While the action
is leased the stream shows it executing; while it is queued it shows what it showed. If the
store has no trace of an operation (neither a `done/`, an `op/` nor a queue row) for three
looks in a row, which is the new frontend having lost it, the stream ends UNAVAILABLE and
`kbf_frontend_relay_lost_total` counts it: a stream is never left waiting for ever. Buck2
does not recover a stream that breaks, so that count is the number of builds that failed
because of a handover; it is zero unless the new frontend died between two writes.

**New Buck2 calls.** Envoy sends them to the old frontend until it sees the new one healthy
and the old one goes not ready. A call that reaches the old one after it has handed over is
forwarded to the new one (`Execute` and `WaitExecution`: the same message and metadata, which
the new one authorizes again) and answered from there to its end; it waits inside its own
open call while the new one takes the store, and nothing is queued or refused. Each is
counted: `kbf_handover_relayed_total` and `kbf_handover_relays_open`. Storage calls (CAS,
ByteStream, action cache) go to the storage nodes from either frontend. A call that raced the
fence itself (between its request and its queueing) gets UNAVAILABLE at open, which Buck2
asks again.

**Daemons.** The old frontend ends their streams and stops its worker listener; each dials
the same worker address again, which is Envoy's, and reaches the new frontend. The epoch is
kept, so a daemon keeps the runs of its leases and sends again the results the old frontend
did not acknowledge. No scheduling happens between the old frontend's stand-down and the new
one serving (normally well under a second; the wait for calls between their request and their
queueing is bounded by `--handover-quiesce-secs`, 10 s by default).

**The operator API (`kbf cluster`).** Each instance's API listener is on its own port. The old
one keeps answering `/readyz` (and its reads), but its farm is fenced: write calls (cordon,
drain) go to the new instance. A cordon or a drain an operator set is a row in the state store,
so the new frontend still holds it (a drain's deadline is wall-clock time).

**`/readyz`, the health service and Envoy.** The old frontend stays ready through the stand-down
(its readiness stops asking the stores it gave up). The new one says ready once its probes pass,
and tells the old one it serves. The old one then waits Envoy's settle and goes not ready: Envoy
has seen the new one and sends the old one nothing new. At every moment at least one frontend
that can answer a call is ready. If the new one dies after the old one let go, or is alive and
never serves within `--handover-watchdog-secs` (15 s), the old one goes not ready by itself, so
Envoy answers with a clean error instead of choosing a frontend that can start nothing.
The old one exits when it holds no stream, no relay and no caller and has been out of Envoy's
rotation for the settle.

## What a handover carries and what it does not

**Carried, as rows of the state store.** The operations and their queue order (`op/`,
`queue/`), the leases (`lease/`), the loss count of an operation (`loss/`), the result of a
finished one (`done/`), the daemons' sessions, the cordons and drains, and the epoch. The
old frontend writes the leases it has given up before it gives up the writer role: it
retries, for at most `--handover-quiesce-secs`, the requeues the store refused, and stops
at once if the store says a newer writer exists. If the store still refuses at the
deadline, the handover completes and the old frontend logs an ERROR naming the operations
it could not write; the new frontend then recovers each of them with the lease the store
still holds, and the daemon's heartbeat or the lease grace settles it.

Rows for the state that is in memory today and in the stack of changes #897, #911, #915 and
#916 (`fate/` for the memory ladder and the fault budgets of an operation, `floor/` for the
memory floors, `subject/` for the usage line of an operation) are carried only once those
changes are merged; until then a successor starts those from nothing.

**Not carried.**

- An invocation record is split between the two processes: an invocation in flight, or a
  recent one, looked up through `/v1` or the UI on the new frontend is not found. Build
  results are not affected, because callers are answered from `done/`.
- Counters restart at 0 in the new process (the `pressure` counter included), so a sum
  across processes scraped one by one sees a drop. Use `rate()` or `increase()`.
- `action shape:` is logged again, once per key, by the new process.
- The unservable timer restarts: a refusal can be delayed by up to `unservable_wait`.
- `kbf_front_open_executions` stays above zero in the old process for as long as it
  answers callers from the store.
- A kill whose store write was not acknowledged before a crash costs at most one extra run
  or one extra budget step.
- The rule "seen before: the last measured peak plus a buffer" for the memory a key is
  booked is not implemented; the floors are only the highest rung an operation was raised
  to.

**Known gap.** Two other refused writes are not retried, before or at a handover. When
the store refuses the rows of a committed record (an `Effect::Commit`), the farm logs it
and drops the effects that depended on it; when it refuses the `done/` row of a finished
operation, the farm logs it and answers its callers from memory. Neither is written again
by the old frontend, so the new one recovers such an operation from the rows it finds: the
lease it was granted, or no result. An issue is to be filed for it.

## When it goes wrong

| What happens | What clients see | What the operator does |
|---|---|---|
| The new instance fails flags, certificates or probes, or is stopped, before `ready` | Nothing: the old frontend serves, ready | Read `journalctl -u kbf-frontend@<new>`, fix, run the command again |
| The new instance dies after the old one released the store | The old one goes not ready as soon as the control connection closes; callers of the old one still get their results from the rows; the new instance is started again by systemd (`Restart=on-failure`) as a cold start, which bumps the epoch: the daemons run their leases again | Check `journalctl`; builds are not failed, actions rerun |
| The new instance is alive and never serves | The old one goes not ready after `--handover-watchdog-secs` | Check `journalctl -u kbf-frontend@<new>` |
| The old frontend dies mid-handover | Its open streams end, as for any crash | The new one proceeds (the kernel released the file lock) |
| The same instance, another state file, or a REAPI address that is not loopback in the new unit | The handover is refused with the reason and the old frontend serves | Fix the unit; a change of state file is a cold restart: `systemctl stop` the serving instance (it drains) and start the other |
| Both instances active | The command refuses | Wait for the old one to exit |

A handover moves a process, not a machine: the two instances run on one host, on one
state file. `--handover-socket` needs `--state-store`, and a start with the flag but
without a state store is refused.
