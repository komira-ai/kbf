# One binary, a leader-elected cluster

Status: design, nothing built. Labels: **[F]** fact checked against main or a cited document, **[I]** inference, **[D]** owner decision, **[V]** verify before building on it.

## 1. Decisions

- **[D]** One `kbf` binary holds the server half and the daemon half. Every node runs it.
- **[D]** One flag, `--leader-candidates=node-a:port,node-b:port,node-c:port`, has the same value on every node. A node in the list runs its server half as a Raft member (leader or follower) plus its daemon. A node not in the list is daemon-only: its server half is never constructed and its daemon dials the candidates.
- **[D]** The leader schedules. The leader's own daemon uses the in-process data and control plane (no TLS between a node's own halves). Follower candidates, daemon-only nodes and Macs connect to the leader remotely.
- **[D]** Clients go through Envoy, which health-checks the candidates and sends all REAPI traffic to the current leader. This supersedes the earlier ruling that all-in-one serves only its own daemon; it keeps "no TLS between a node's own halves".
- **[D]** Hot store (SSD, `file://`) plus cold store (object store, `s3://`). Unflushed hot blobs that live on one node's SSD may be lost with that node. No per-action reservations. Configuration is flags. Tests never depend on timing.
- **[I]** Ship in stages. Stage 1 elects one unambiguous leader and gives it a fresh, strictly increasing epoch; the queue and leases stay leader-local, so a failover loses them (as a leader change already does today, `scheduler.md`). Stage 2 replicates the scheduler.
- **[I]** v1 membership is the static list. No membership change, no learner flow, no `--raft-join`.
- **[I]** All REAPI goes to the leader in v1 (section 4). This deviates from `deployment-topology.md` and from the two-cluster front in PR #303 and is recorded here as the decision.

## 2. Roles and flags

```
kbf --node-id=node-a --advertise=node-a:port \
    --leader-candidates=node-a:port,node-b:port,node-c:port \
    --raft-dir=... --hot-store=file://... --cold-store=s3://... --reserve-cpus=N
```

- Role resolution is a pure function of (`--advertise`, list, `--daemon-only`). A node is a candidate iff `--advertise` equals exactly one list entry.
- Refused at startup: empty list, duplicate entries, an `--advertise` matching more than one entry, and an `--advertise` that matches none unless `--daemon-only` is also given. A typo therefore cannot silently turn a candidate into a daemon-only node.
- The resolved role and list are logged at startup and shown in `/readyz`.
- A daemon-only node binds no listener, opens no Raft directory and holds no store. Today that is just a daemon with `--server` URLs [F]; no new code is needed for that role.
- **List guard.** Raft messages carry a hash of the candidate set (sorted, so order does not matter). A mismatch is rejected and reported in `/readyz`. Changing the list is a stop-all restart in v1; it is documented as such, not as a rolling procedure.
- **Wiped disk.** A candidate with an empty `--raft-dir` is not a member and must not vote or campaign until the cluster tells it the configuration. Initial bootstrap of three empty logs is an explicit one-shot act, not the default of an empty directory. Otherwise a replaced disk could vote twice in one term. [I] Raft's `currentTerm` and `votedFor` must be durable before any reply.
- A failed log append is fatal for that voter: it steps down and exits, never keeps leading on a log it cannot write.
- The registry of nodes never supplies Raft membership; the list does.
- Pilot server: becomes daemon-only. Its single-voter log is not imported; candidates start fresh.

## 3. Replication

| Replicated in stage 1 | Leader-local in stage 1 |
|---|---|
| The Raft log and elections | Queue, operations, waiters |
| One `Takeover` entry per leader term | Leases, worker sessions, held starts, memory floors |
| Meta-log records `RaftMetaLog` already commits (`Tick`, epochs) [F] | Stream senders and `watch` channels |

- **Epoch.** Today `Welcome.epoch` is derived from wall-clock time (`process_term()`) [F]. Stage 1 replaces it with the committed log index of the leader's `Takeover` entry: non-zero and strictly increasing across leaders and restarts. This lands before any second voter exists, because with a wall-clock epoch a node that loses and regains leadership could reuse its epoch with stale in-memory state.
- The daemon already kills leases of a different epoch [F], so a leader change cleanly aborts running work with no new daemon logic. The daemon additionally refuses a `Welcome` with a lower epoch than it has seen, so a stale leader cannot make it kill good leases. [V] Persist the highest epoch across daemon restart, or document that a restarted daemon has no leases to protect.
- **Leader-ready** means: this node is Raft leader, has heard from a majority within the election timeout (CheckQuorum), and its `Takeover` has committed. Only then does it accept sessions or REAPI traffic, and only then does `/readyz` report `leader`. `set_leader` has no caller today [F]; this wires it.
- **Step-down drops all leader-local scheduler state**, so a node that later regains leadership cannot resurrect old leases or queue.
- **At-least-once window.** Between a leader losing its quorum and noticing (bounded by the election timeout), daemons still attached to it can finish work. Their results are fenced by the epoch on reconnect and duplicate outputs are harmless because the CAS is content-addressed.
- Heartbeats and liveness stay out of the log. Liveness is soft leader state rebuilt from daemon `Hello` reports after `Takeover`. Stage 2 replicates grants, completions and the epoch only.
- Lease deadlines are leader-clock values; followers never compare a peer's clock to their own. Election timers use a local monotonic clock.
- **Stage 2** adopts the replicated scheduler of PR #303 (`FarmMachine`, `LeaseId.term` = Raft term, epoch = log id, `Takeover{term, now}`, workers unplaceable until `WorkerUp`, a grace extension of lease deadlines at `Takeover`). That document remains the reference for it.
- **Prerequisites in `kbf-raft`** (not in scope today [F]): PreVote, CheckQuorum, InstallSnapshot, a network transport. Until InstallSnapshot exists, compaction must not pass the slowest voter's progress. A rolling-upgrade version gate (a voter that meets an unknown entry type halts; new entry types are proposed only when every voter reports the version) comes with stage 2.

## 4. Leader discovery and routing

**Daemons.** Redirect, not proxy.
- Stage 1 uses no new protocol. A node that is not leader-ready ends a `Session` promptly with `UNAVAILABLE` (it never accepts the stream and idles). The daemon connect loop already re-resolves every name, tries every address and backs off up to 30 s [F]; with three candidates it finds the leader within one round.
- Later, the `kbf-leader: <worker_addr>` trailer of PR #303 section 9 shortens the search. The daemon follows it only to an address it was configured with or resolved, only if it verifies under the CA, at most once per round, and caps backoff at 2 s while it holds leases. This is an optimization, not a correctness need.
- A daemon-only node that loses all candidates keeps running actions, holds finished results and retries. A flag sets a local ceiling after which it kills actions, so an isolated node does not work forever for nobody.
- Peer Raft traffic uses its own mutual-TLS listener and is never behind Envoy.

**Envoy.** One cluster, all candidates, health-checked on the leader-ready check (gRPC health service `kbf.leader`, backed by `/readyz`'s `leader` check).
- `panic_threshold: 0`. With one healthy host of three, Envoy's default panic mode would route to followers.
- Short check interval, low unhealthy threshold. Failover is election timeout plus interval times threshold: expect seconds of `UNAVAILABLE`, which clients must retry.
- Retries by method class: idempotent reads (`FindMissingBlobs`, `GetActionResult`, `BatchReadBlobs`, ByteStream `Read`, `GetCapabilities`) retry on `unavailable`, `connect-failure`, `reset`; `WaitExecution` retries on `unavailable`, `connect-failure` (it resumes by operation name, which needs stage 2's finished-operation window to be useful); `Execute` and all writes retry only on `connect-failure` or `refused-stream`, i.e. only when nothing was sent.
- Envoy runs on the one client box today; CI runners reach it over the private network. It is a single point of failure for clients, not for the farm.
- `SIGTERM` on the leader should transfer leadership (stage 2) so planned restarts are not an outage.

## 5. In-process versus remote daemon on role change

- The daemon half is one object whose control-plane transport has two implementations behind one trait: in-process and remote. One conformance suite runs against both.
- Main still uses a mutual-TLS loopback worker listener for the control plane; only the data plane is in-process [F]. Until the in-process control plane exists, the leader's own daemon keeps the loopback path. That is correct but does not yet meet "no TLS between a node's own halves"; the gap is stated, not hidden.
- Leadership arrives: the node becomes leader-ready first (`Takeover` committed). Its daemon then opens an in-process session. The scheduler treats workers as unplaceable until they report, so there is no need to gate readiness on the local daemon.
- Leadership leaves: the server half steps down and drops streams; the in-process session ends; the same daemon redials the new leader remotely.
- Stage 1: the new epoch makes every daemon kill and forget its leases, including the in-process one. Stage 2: running actions survive, are listed in `Hello` and adopted by the new leader.
- The leader now serves other nodes' daemons through its worker listener, so the single-node "no worker listener, accepts no remote daemon" text of the all-in-one design must be amended when that design lands.

## 6. Data: hot and cold

- Each candidate has a hot SSD store; the cold store is shared. Only the current leader receives uploads, so its hot store is the one coherent view. Followers serve no reads in v1. This avoids the trap in which an upload accepted by follower X sits only on X's SSD while the leader schedules the action elsewhere.
- Cost: every upload and input fetch crosses the leader's NIC. That is the likely first bottleneck, well before scheduling. Fix when measured: write-through to the cold store before ack and let daemons fetch from the cold store, or a blob-location index. Follower reads are deferred until one of those exists.
- After failover the new leader has its own (possibly stale) hot store plus the cold store, not the old leader's unflushed blobs (accepted [D]). Two rules keep that safe:
  1. `FindMissingBlobs` reports from the real hot-plus-cold view, so clients re-upload.
  2. `GetActionResult` treats an entry whose output blob is absent as a miss. [V] Where the action cache lives and whether `GetActionResult` already checks outputs has not been traced. If it does not, this is a correctness fix that lands before stage 1 ships.
- Write side: v1 relies on verify-on-read. Ack-after-flush for `UpdateActionResult` is added if the miss rate is high.

## 7. Failure modes

| Failure | Behavior | Test (simulator, logical time) |
|---|---|---|
| Leader crash | Election; new `Takeover`; stage 1 loses queue and leases, stage 2 keeps them | epochs strictly increase; stage 2: leases survive leader kill |
| Partitioned or isolated leader | CheckQuorum steps it down, `kbf.leader` goes false | minority leader steps down and is not leader-ready |
| Rejoining partitioned node | PreVote stops it from deposing a healthy leader | term unchanged after rejoin |
| Two nodes believe they lead | Stale one cannot commit `Takeover`, so it never becomes ready; daemons reject lower epochs | at most one leader-ready node per term; none before its `Takeover` commits |
| Flapping | PreVote, stickiness, 2 to 4 s election timeout, Envoy thresholds | no term churn under message delay |
| Slow follower | Majority commit does not wait; behind the base it is not served until InstallSnapshot | follower behind base catches up |
| Voter disk full or append fails | Fatal: step down and exit | injected append error stops the voter |
| Wiped voter disk | Empty log means not a member; no vote | empty-log node never votes before configured |
| List mismatch | Raft messages rejected, shown in `/readyz` | differing hashes never form a group; order-only difference is accepted |
| Daemon-only node loses all candidates | Holds results, retries, local kill ceiling | ceiling flag kills after the bound |
| Envoy picks stale leader | Not-ready node answers `UNAVAILABLE`; client retries | fake-backend routing test |
| Mixed versions | Stage 2 version gate; halt on unknown entry | no new entry applied before the gate |

Surviving one node loss needs 2 of 3 voters; two losses stop the farm, with no loss in the cold store.

Client view on failover in stage 1: queued work is lost, running work is killed, the `Execute` stream breaks, and `WaitExecution` for an operation the new leader does not know most likely fails. **[V]** How the build client reacts to a broken stream or `NOT_FOUND` has not been traced; it decides whether stage 1 is usable and is checked first (PR 0 below).

## 8. Phased plan

Each PR's first test must fail before the change. No test depends on wall time; the simulator (ADR 0001) supplies logical time, and farm drills assert on end state, not duration.

0. **Spike (before design work is built on):** drive a real build client against a single leader, break the stream, and record how it recovers. Also read where the action cache lives (section 6). Outcome edits this document.
1. **Role resolver.** Pure function, `--leader-candidates`, `--advertise`, `--daemon-only`. Red first: a daemon-only node binds a listener today; duplicates, empty list and unmatched advertise are accepted today.
2. **One binary.** Server and daemon libraries behind one entry point. Red first: daemon-only node reaches a leader listed behind a non-leader.
3. **Raft safety in `kbf-raft`** (separate small PRs): CheckQuorum, then PreVote, then wiped-disk rule and fatal append. Red first: minority leader keeps leading; rejoiner deposes leader; empty log votes.
4. **Network transport and static three-voter group** with the list hash.
5. **Epoch and readiness.** `Takeover` index as `Welcome.epoch`, `set_leader` wired, step-down clears scheduler state, daemon rejects lower epochs. Red first: wall-clock epochs are not ordered; `leader` is true on a follower.
6. **Follower refusal and daemon rotation.** Red first: followers accept sessions today.
7. **Envoy config** checked in with a golden test and a fake-backend routing test, plus the retry table.
8. **InstallSnapshot**, then compaction allowed.
9. **Control-plane transport trait**, in-process implementation, role-change switching.
10. **Hot-store rules** from section 6 with failover tests.
11. **Farm drill:** kill the leader under load with a real build.
12. **Stage 2:** replicated scheduler from PR #303, trailer redirect, version gate, leadership transfer, then follower reads once the blob-location question is decided.

## 9. Open questions for the owner

1. The pilot's existing log and unflushed hot blobs are abandoned. Acceptable?
2. Stage 1 loses queued and running work on every failover. Acceptable as an interim, subject to PR 0 showing the client recovers by resubmitting?
3. Leader-only reads put all blob traffic through the leader. Acceptable while there is one client box?
4. Is one Envoy on the client box enough while CI runners also depend on it?
5. Wiped-disk rule: an empty `--raft-dir` is not a member until configured, and first bootstrap is an explicit act. Adopt?
6. Write side: verify-on-read first, ack-after-flush only if misses are frequent. Adopt?
7. Is a stop-all restart for changing the candidate list acceptable until membership change exists?

## Appendix: review points not adopted

- *Operator's list as a seed with leader-driven learners and `--bootstrap` on one node:* rejected. Two sources of truth, needs unbuilt membership change and InstallSnapshot, and `--bootstrap` differs per node and is dangerous on a wiped disk. The list is the static membership.
- *Robust's follower reads (`kbf-reads`) and AC reads without ReadIndex:* rejected for v1; incoherent with per-node hot stores.
- *Robust's gate "leader-ready only after its own daemon is attached in-process":* rejected; unplaceable-until-`WorkerUp` already covers it and the gate could stall an election.
- *Robust's Phase A with wall-clock epoch:* rejected; the `Takeover` epoch comes first.
- *Operator's `/v1/nodes`, metric names and registry section:* dropped as invented. Only the `/readyz` role and list fields are kept; metrics are left to the PR that adds the code.
- *Minimal's "rolling restart unsupported" for the list hash:* kept but stated honestly as stop-all, with the hash order-insensitive and reported.
- *Judge point that minimal gave no rollback story:* partly accepted. Once the epoch is in the log an older binary cannot read it; stage 1 PR 5 is flag-gated until the farm drill passes. [I]
- *Judge point that the hash guard blocks all upgrades:* accepted as the stop-all note; not worth a membership protocol in v1.
