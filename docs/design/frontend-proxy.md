# Frontend zero-drop update: bundled proxy, per-stream swap, Postgres side by side

Status: design for review, 2026-10-18, revised to the owner's answers of that date. Written against `origin/feature/arch-v2`. Claims about code that I did not re-read are listed in section 14 and must be checked by the unit that depends on them (unit U0 does the first ones).

## 1. Decisions

1. **One public port per frontend host** (above 1024), owned by `kbf-proxy`, shipped in the `kbf-installer` package as the `kbf-installer proxy` verb. It is a separate unprivileged service (user `kbf`), not part of the root helper.
2. **The proxy terminates TLS for everything on that port**: REAPI, operator API and worker sessions. It verifies a client certificate when one is presented (farm CA, role URI, node id in SAN, EKU), and forwards every stream to the ACTIVE child over loopback with the verified identity (node id, role) as metadata. Routing is per HTTP/2 stream and never sends GOAWAY to a client. No daemon or node release is a prerequisite.
3. **Children are plain `kbf --mode frontend` processes** that the proxy starts as its own processes and supervises, on loopback ports at fixed per-slot offsets. They trust identity metadata only from the proxy (section 3).
4. **Side by side on Postgres means one writer for scheduling.** The store-wide generation fence decides who schedules. The old child is read-only for scheduling, may write only its own metadata (section 8), and finishes its open streams from `done/` rows.
5. **Update order is frontend first.** The new frontend, once baked, updates the storage nodes and daemons. An N+1 frontend must serve N nodes (section 9).
6. **SQLite is dropped with no state migration.** The farm starts fresh on Postgres; cached blobs live on storage nodes and are unaffected (section 11).
7. **One update at a time** (the API rejects a second). **The proxy updates only with the package**, waiting for idle by default (section 10). **All-in-one** is stop then start, with the proxy holding new streams in between (section 5a).

## 2. Process model on a frontend host

| Process | User | Listens | Job |
|---|---|---|---|
| `kbf-installer run` (root helper) | root | nothing | Verifies and stages releases under `/var/lib/kbf-installer/releases/<commit>/`, reads `orders/`, spawns the unprivileged fetch child. Contract unchanged. |
| `kbf-installer proxy` (`kbf-proxy.service` + `.socket`) | `kbf` | the public port, from systemd socket activation (`O_CLOEXEC`) | TLS, client-certificate identity, routing, hold, update API, child supervision, open-stream accounting. |
| `kbf --mode frontend`, slot `a` or `b` | `kbf` | loopback ports at slot offsets; admin calls on a separate loopback port | The frontend. Plain h2, identity from the proxy only. |
| Postgres | `postgres` | Unix socket | Shared state (section 11). |

- **The proxy runs as the unprivileged `kbf` user** and is installed by the kbf-installer package. Only the root part of kbf-installer stages verified releases. The root helper keeps its "no socket, no network" property; the proxy talks to it only through the `kbf`-owned `orders/` directory and `status.json` (the existing spool contract). No new control socket.
- **Doc change:** `kbf-installer.md` "no installer network listener" becomes "the root helper has no listener; the `proxy` verb is a separate unprivileged service". Hardening applies per service.
- **Order to the root half (decided).** The proxy writes a `frontend_update` record into `orders/` carrying only `{order_id, epoch, generation, verb, commit, window_s}` (well under the 4096-byte limit). It carries no components, no attestation and no expected facts: the proxy has no release store. The root half fetches that commit from the `release_source` in `installer.toml` (a `url` source on a frontend host), takes the attestation from the source, runs the existing verify pipeline and the `--version --json` probe, and **stages without flipping**. Facts (`protocol`, `min_protocol`, `state_schema`, `profile`) appear in `status.json` under `staged{}`; the proxy runs the R1/R4 gate on them. The record cannot name a path or URL, a stolen token can deploy only a signed release, and a downgrade needs `rollback: true` and is recorded.
- **Supervision.**
  - Children are not killed when the proxy dies (`KillMode=process`; verify with a real `systemctl restart kbf-proxy`). A restarted proxy adopts them using a `pidfd`, the recorded start time and `/proc/<pid>/exe`, never the bare pid.
  - Liveness of the generation holder is a Postgres session advisory lock on its dedicated connection. It is released on death, so "the holder is dead" is not a guess from a pid.
  - **GC pin (decided).** The proxy writes nothing to pin. The root half's `gc` keeps `current`, `previous`, the staged release, and any release directory that a live process's `/proc/<pid>/exe` resolves into (scanned at each `gc`). A running child and the rollback target (`previous`, or the old child's release before confirm) are therefore safe without cooperation. Frontend swaps are Linux-only in this design.
  - **Boot and `current` (decided).** The proxy has no slots and never writes `current`, `slots/` or `status.json`. Children run `releases/<sha>/kbf` directly (0755). After a reboot the proxy starts `installed_commit` read from `status.json` (0644) as a cold start with an epoch bump (section 8), and fails safe if that release is missing.
  - Hardening: `OOMScoreAdjust=-900` for the proxy, high `LimitNOFILE`, bounded buffers, header-list caps, h2 rapid-reset and CONTINUATION-flood limits, `catch_unwind` per connection, fuzzed h2 and TLS paths.

## 3. TLS, identity and per-stream routing

**One TLS terminator.** There is no ALPN or SNI demux and no passthrough. The proxy accepts TLS with the server certificate (ALPN `h2`, `http/1.1`; reloaded on SIGHUP; 0-RTT off) and requests a client certificate that is optional at the handshake.
- A presented certificate is verified against the farm CA with the existing checks: role URI, node id in SAN, EKU (client auth). A certificate that fails is a handshake failure, not anonymous.
- **Per path class (installer.toml):** worker and storage-node paths (`/kbf.worker.*`, `Membership`) require a verified certificate with a worker or storage role, else `UNAUTHENTICATED`. REAPI and Buck2 auth is configurable: `buck2_auth = "none"` (network-level, for example Tailscale), `"bearer"` (an API-key header, constant-time compare), or `"mtls"` (verified certificate required). Operator paths take the operator token, and `/v1/update/frontend*` takes the update credential (section 4).
- mTLS therefore authenticates who (which node, which role), not only "trusted".

**Identity to the child.** The proxy strips every inbound `x-kbf-*` header, then sets `x-kbf-node-id`, `x-kbf-role` (from the verified certificate; absent when none) and `x-kbf-proxy-auth`, a per-boot secret.
- The secret is 32 random bytes in a `0600` file under `/run/kbf-proxy/` (tmpfs, so per boot, and it survives a proxy restart so adopted children keep working). It is read at child start. Not argv, not env.
- A child accepts identity headers only from a loopback peer that presents the secret (constant-time compare). Otherwise it ignores them and treats the caller as anonymous, so a local process without the secret cannot claim a node id. The child's own `Hello.node_id` check compares against `x-kbf-node-id` instead of the peer certificate.
- Later alternative (U6): children on a Unix socket in a `kbf`-only directory, trusting `SO_PEERCRED`. It removes the secret, not the headers.
- **Operator-API trust.** Any "Who may write" gate in `docs/api.md` that relies on "peer is loopback" is open to the public once a proxy forwards. Making those gates identity-header- or token-based is a **hard prerequisite of the proxy merging** (U4a).
- **Pre-stream limits.** "No timeouts" applies to streams, not to the front door. There are handshake, preface and header timeouts (about 10 s), connection and per-IP caps, and rate limiting before any token check.

**Routing.** The six REAPI paths and ByteStream go to the REAPI port, worker and `Membership` paths to the worker port, and `/v1/update/frontend*` is served by the proxy. Every other `/v1/*` path (including the forwarded `/v1/update/desired`, `/pause`, `/resume`, `GET /v1/update`) goes to the operator port. `/healthz` and `/readyz` are the proxy's view: 200 only if a child is active and ready.

**Binding rule.** A client stream binds to the slot that was current when its headers arrived, until the stream closes. The bind and the count are one step: the proxy takes a permit together with the slot load. Retire needs all permits of the old slot and "not current". One Buck2 connection therefore carries pre-switch streams to the old child and post-switch streams to the new one. Needs a stress test, and a loom test of the permit.

**Properties kept from the Envoy spec** (the 45 checks in `envoy/failure.rs` become the proxy test list):
- No route timeout, no max stream duration. Idle timeout 0 for Execute and WaitExecution, 7200 s elsewhere.
- **Dead peers.** Downstream PING keepalive (30 s interval, 20 s ack). It closes only dead peers, never a live stream, so it fits the no-timeout ruling. It is what keeps a half-open client from holding the drain open for ever.
- Retry only before the first upstream response, only on connect failure or REFUSED_STREAM, and only while a bounded replay buffer (the first 64 KB window) still holds the request. Never retry ByteStream.
- Flow control is per stream. Windows are capped per connection and stream. A client RST is propagated upstream. An upstream reset mid-stream becomes a gRPC `UNAVAILABLE` trailer, not a bare RST.
- A child GOAWAY (the drain endpoint) stops new streams on that upstream connection. It is never forwarded to a client.
- Upstream pools open more connections instead of queueing behind long streams. A stream queued on a pool of slot A still counts as bound to A. Test with 5000 held streams plus unary latency.
- A child health failure does not touch open streams.

**Fenced-not-committed.** A request whose headers went to the old child just before the flip can reach an enqueue that returns `Fenced`. The child returns a distinct trailers-only "fenced, nothing committed" status only when it is certain nothing committed. The proxy then re-dispatches the buffered request to the active slot. Enqueue is idempotent by request id (section 8), because the proxy cannot always know.

## 4. Update API (served by the proxy)
```
POST /v1/update/frontend  {commit, request_id, nodes: true, rollback: false}
  202 {update_id, phase}; same request_id returns the same update_id
  409 another update is open, an old child is still draining, or commit == running
  409 incompatible {rule}; 422 commit not complete for this platform
GET  /v1/update/frontend
  {update_id, from, to, phase, since, active_slot, restart_pending,
   rollback_allowed, rollback_blocked_reason,
   old: {open_streams, oldest_stream_age_s (proxy counts, worker sessions included), open_executions, queued_operations (child reports)}, new: {open_streams},
   failure: {step, reason} | null}
POST /v1/update/frontend/abort        before "switching"
POST /v1/update/frontend/rollback     after "switched" while the old child lives and no node updated
POST /v1/update/frontend/force-retire explicit, needs a reason, counts dropped streams, never automatic
```


Phases: `accepted, fetching, verifying, staged, preflight, starting, ready, holding, quiescing, switching, switched, baking, nodes, draining, done`, plus `failed` and `rolled_back`.

- **One update at a time (decided).** A second `POST` while one is open, or while an old child drains, gets 409. `force-retire` is the escape.
- **One state machine, one actor.** All API calls go through a single mutex or actor. Test: fire `POST`, `abort` and `rollback` at once.
- **State of record (decided).** Facts come from two sources: the installer boundary (`orders/`, `status.json`) for fetch, verify and `installed_commit`, and the Postgres generation holder plus `GET role` for who is active. The store wins any disagreement. Intent comes from one small file, `update.json` in the proxy state dir (`/var/lib/kbf-proxy/`, `kbf:kbf` 0700), written by temp, fsync, rename on every phase change. It holds `update_id`, `request_id`, `from`, `to`, `nodes`, `rollback`, `phase`, `phase_started_wall`, `bake_started_wall`, `nodes_posted`, `old_slot`, `new_slot` and the last 64 `request_id -> update_id` pairs. On restart the proxy reads it, checks it against the facts, and resumes at its phase. If it is missing or corrupt the proxy refuses new updates (`409 state_lost`), leaves both children as the generation holder says, and shows the status red. Test a kill between fsync and the 202.
- **Auth.** Now: the **operator token**, the same one that guards the operator API (`Authorization: Bearer`, constant-time compare). `setup` creates it as `/etc/kbf/operator.token` (32 random bytes hex, root:kbf 0640, only on frontend and all-in-one hosts, never overwritten); `installer.toml` key `operator_token_file` points at it and the proxy reads it as group `kbf`. Worker and storage paths use mTLS (section 3); Buck2 auth is separate (`buck2_auth`), rate limited before any hashing, never in a query string or a log. Remote trigger: GitHub OIDC (#1088) replaces it for non-local callers. The check sits behind one internal function so that swap is local. I have not read #1088, so nothing more is fixed here.

## 5. Swap sequence (frontend host)

Each step records its phase, then acts, and every action is idempotent, so a crash resumes.

1. **Accept and preflight.** No update in flight, commit not running, `orders/` writable, active child answers. Answer 202.
2. **Stage** through a `frontend_update` order (section 2): the root half fetches, verifies and probes, then stages without flipping. A failure changes nothing. The proxy starts the candidate from `releases/<sha>/kbf`. The flip comes at step 8, as a `confirm` order.
3. **Compatibility gate** (section 9): run `releases/<commit>/kbf --version --json`, apply R1 and R4. Refusal is `409 incompatible` naming the rule, before any process starts.
4. **Start candidate** in the idle slot with `--await-takeover`, as a child of the proxy. It opens Postgres and runs expand-only migrations under an advisory lock with `lock_timeout` (about 2 s, with retry). It never bumps the epoch, never takes the generation, and runs no reconciler, scheduler or worker listener. If it exits in 30 s or is not ready in 120 s, kill it, `failed`. The old child is untouched.
5. **Readiness.** `/readyz`, a `grpc.health.v1` check, and one synthetic `GetCapabilities` through the candidate.
6. **Hold, quiesce, takeover, swap** (`holding`, `quiescing`, `switching`). The only step with a hold.
   1. **Hold new streams.** The proxy accepts them and reads their headers but opens nothing to a backend. It does not grant more flow-control window (the per-stream initial window is capped at 64 KB, so at most that is buffered per stream), and the queue is bounded. The limit is `hold_max` (frontend default 15 s, longer than the takeover limit of 10 s). Held streams are released to a slot only after takeover has resolved either way.
      - **Bypass.** `/v1/update/frontend*`, `/healthz`, `/readyz` and `/metrics` are never held (otherwise `abort` would deadlock); the proxy reads the request head to decide. On HTTP/1.1 there is no multiplexing: the proxy stops reading further requests on that connection until released and, at `hold_max`, answers `503` with `Retry-After`.
   2. **Quiesce the old child:** it stops scheduling, assigning and requeueing and flushes. It does not wait for running work (that is followed from `done/`). Budget: **3 s** to quiesce, 10 s to take over, 2 s slack, inside the 15 s `hold_max`. A quiesce over 3 s counts as a failed switch (Failure below). An `Assign` is sent only after its lease write commits (section 7). This closes the window in which A assigns while B already holds the generation.
   3. `takeover` on the candidate: it takes the generation (atomically, section 8), reloads all state from the store, starts reconciler, scheduler and worker listener.
   4. The proxy swaps the active slot atomically and releases the hold into the new child. Journal `switched`.
   - **Hold expiry.** After `hold_max` the proxy fails each still-held stream with gRPC `UNAVAILABLE` (retryable) and increments `kbf_proxy_hold_expired_total`. Metrics: `kbf_proxy_held_streams`, `kbf_proxy_hold_seconds`. Expiry does not abort the update.
   - **Failure.** If takeover fails or exceeds 10 s, the proxy first kills the candidate (it never served; `pidfd`), then reads the generation row and roles, and only then tells the old child to resume. The old child resumes from the store, not from warm memory. Phase `failed`, step `switch`.
7. **Stand down the old child.** It ends its worker streams with `UNAVAILABLE "connect again"`; the redial is a new stream that the proxy routes to the new child (no connection to close, since nothing is passed through). It keeps serving open Execute and WaitExecution streams from `done/` rows through the follower, with no GOAWAY and no timeout. ByteStream and CAS data-path calls continue (section 8). The relay-to-successor in `relay.rs` is turned off, because the proxy routes new streams: one mechanism only. The `done/` follower stays.
8. **Bake** (default 5 min, from `bake_started_wall`). The proxy watches the new child. v1 has **no automatic rollback while the proxy runs** (a proxy restart that finds a failed switch before the confirm rolls back, section 6; the confirm has landed when the root's `current` names the new commit, which the restarted proxy reads from `status.json` `installed_commit`); the status shows a red child and the operator runs `rollback`: the proxy sends `stand-down` to the new child and `resume` to the old one, which fully reloads and re-takes the generation. It is refused once any node updated. When bake passes, the proxy sends a `confirm` order naming the staged commit; the root half flips `previous` then `current` (state first). On rollback or abort it sends `revert`, which drops the staged release; `current` was never touched. For this verb the root half does not check `/proc/<MainPID>/exe` (there is no unit); it checks that the named commit is the staged one and that no node-activation swap is pending. Self-confirm by a compromised release is the residual KI section 10 already accepts.
9. **Nodes.** If `nodes: true`, the proxy asks the new child for `POST /v1/update/desired {commit}`. The child's node controller (#970, routes #976) updates storage nodes and daemons. A failure here does not roll the frontend back. Node cordon and drain count open streams from either frontend (test: drain a node that has a stream via the old child).
10. **Drain and retire.** When the proxy's count on the old slot is 0 and the old child reports `open_streams == 0` and `open_sessions == 0`, the proxy sends SIGTERM and never SIGKILL. No timeout. A stuck stream stays visible in status. `releases/` keeps the previous release.

**Hard limit.** A follower that cannot read the store for `DONE_FLOOR` (600 s) can lose a result. The old child fails that stream with `UNAVAILABLE`. Add the metric `kbf_follow_oldest_age_s`.

### 5a. All-in-one swap

One process owns both frontend and storage, so two cannot run side by side. The same proxy and hold step apply, with stop then start:
1. Stage, verify and compatibility-check as above (steps 1 to 3). The proxy holds new streams (6.1, `hold_max` default 300 s).
2. The old all-in-one `quiesce`s: it stops taking new actions, finishes its running ones, and releases its storage and its Postgres generation.
3. The old process exits. The proxy starts the new child, waits for ready, and feeds it the held streams.
4. A stream still held at `hold_max` gets `UNAVAILABLE` and a metric tick; the update keeps going, because running actions may legitimately outlast the hold. `abort` before the old process exits resumes it.
Open streams of the old process end when it exits; Execute clients retry and attach to the same op row (section 7).

## 6. Crash behavior

| Crash point | Result |
|---|---|
| Steps 1-3 | No child started. Proxy restarts, reads `update.json` and `status.json`, resumes or fails the order. |
| Steps 4-5, candidate dies | One respawn, then `failed`. On proxy restart the old child is adopted as active and the candidate (never took the generation) is killed. |
| Step 6 before takeover returns | On restart ask both children `GET role`; the generation holder is active. The hold is in memory and vanishes. |
| Step 6 after takeover, before the swap | The generation holder is active. The other child gets stand-down. |
| Old child dies while draining | Its open streams are lost. Unavoidable. Results in `done/` stay. The update continues. |
| New child dies during bake (proxy up) | If the old child is alive, the operator rolls back; the proxy does not. |
| Proxy crash during bake | New proxy reads `update.json`, adopts both children, continues bake from `bake_started_wall` (an expired bake passes at once). The restart also checks the store: if the new child, the recorded-active one, is dead or does not hold the generation while the old child lives, the switch failed behind the proxy's back. This is **automatic rollback**, before the confirm only: the proxy records `rolling_back` and `failure{step: switch}` first, then holds, resumes the old child, drops the staged release, and retires the new child (nothing to signal if it is dead; SIGTERM once idle, never SIGKILL). The update ends `failed` with step `switch`, not `rolled_back`, which stays the operator's answer. |
| Proxy crash, both children dead (after the switch) | Nobody serves. The proxy starts the recorded-active release (the new one) again in its slot, records the process before it waits, waits for ready, and makes it take the generation: a cold start with an epoch bump (section 8). The gate then releases into that slot. A crash in between repeats this: the recorded process is adopted. If the new release cannot start or take over, the update ends `failed`, step `restart`. Before the switch the old release is the one started, and the update ends `failed`, step `restart`. |
| Proxy crash after the confirm landed (the root's `current` names the new commit) | The boundary is the root, not `update.json`: a restarted proxy at `baking` or later reads `installed_commit` from `status.json`, and the confirm landed iff it equals the update's `to`. That holds for a crash after the root flipped and before `update.json` recorded it (phase still `baking`, with `nodes` true or false). The store wins: the restart never rolls back and neither bakes nor confirms again. A dead new child is started again as above while a live old child is told to stand down and drains; a live new child that does not hold the generation takes over after the old child stands down. The old child is then retired like any other (SIGTERM once idle) and, with `nodes` true, the nodes are sent. The update ends `done`. If that start or takeover fails, `failed`, step `restart`, and the staged release stays. If `update.json` and the root disagree the root wins and the restart notes it: a `failure{step: confirm}` left by an earlier try is cleared when the root holds the new commit, and a phase of `nodes` or later with `current` still naming the old commit is treated as before the confirm (the rollback above). If `status.json` cannot be read, the proxy guesses nothing: the update ends `failed`, step `restart`, with both children as they are, no rollback and no signal. |
| Proxy crash during nodes | `nodes_posted` is advisory: the proxy re-posts `desired{commit}`, which the controller treats as idempotent. |
| Proxy crash during draining | The proxy keeps waiting on the old child's `open_executions`; the old child's proxied streams were already cut by the crash. |
| Proxy crash | systemd restarts it, socket activation queues new connections. **All client streams through the proxy are cut**, including streams to the old child. A hold in progress is lost (clients retry). Main residual risk of a single front door. Measure it with systemd restart counts and child-side "peer vanished" counters; the proxy cannot count its own crash. |
| Root installer crash | Not in the data path. Resumes from `orders/`. |
| Postgres restarts | See ambiguous commits, section 8. Pin `postgresql` restarts out of the unattended-upgrade window or order them with the update. |

## 7. Daemon and storage-node sessions across the swap

- A `Worker.Session` is one h2 stream that the proxy routes to the active child. At the flip the old child ends its worker streams with `UNAVAILABLE`, and the daemon's redial is routed to the new child like any new stream. The verified node id is re-sent with each stream, so the new child needs no shared session state. Test: the daemon's second Session is served by the new child over the same TCP connection.
- **Assign after commit.** A child sends an assignment only after its lease write committed.
- **Completions.** A child never acknowledges a completion before its commit. The daemon keeps unacked completions and resends them on the new session. The new child accepts a completion for a lease granted under the previous session, keyed by (lease id, attempt), first valid writer wins, and rejects a stale attempt. Test: kill the session between send and ack; the result is neither lost nor duplicated.
- **Reconnect grace.** After takeover the new child does not expire leases of daemons that have not yet redialed. The grace is tied to lease expiry and node heartbeat, not a bare constant (initial value 30 s, configurable). Attempt-fenced results make a long partition safe: the late result is rejected, not run twice. A daemon with no session and no lease abandons its work.
- A redial of a known instance is a continuation (`predecessor_instance_id`), not a stale-stream drop. Needs a test, not an assumption.
- **Execute retry after a cut stream.** Buck2 re-sends Execute. In-flight dedup is a store op row keyed by (instance, action digest), so a new Execute attaches to the existing op, on either child. Test: Execute, kill the proxy, Execute again, assert exactly one run.
- Storage-node `Membership` and register calls take the same route. Both children may dial nodes during a drain, so a test checks that storage nodes assume no singleton frontend.

## 8. Postgres side by side

**Fencing must be atomic with the write.** A separate SELECT of the generation before a write lets A pass, B take, and A still commit. Each fenced write does the check inside its transaction (`SELECT gen ... FOR SHARE`, or `UPDATE ... WHERE gen = $1`). After the first `Fenced` the child sets a sticky flag: it stops scheduling, assigning and requeueing, never retries, and does not exit. Test: hold A's transaction open, take over on B, assert A's commit fails or B's takeover waits.

**Who may write which keys during a swap.** Key families below are the names I expect; U0 confirms them against the store and U2 pins them in a table in the code.

| Key family | Old child (after takeover) | New child | Why no conflict |
|---|---|---|---|
| `lease/`, `op/` (queue, claim, requeue, assignment state), `epoch`, generation row | none (fenced) | writes | Every write checks the generation in its transaction; the old child's sticky flag stops it after the first `Fenced`. |
| Member map, cordons, `update/*`, `desired`, release index, reconciler and controller state | none (fenced) | writes | Same fence. |
| Run history and log rows for the streams the old child still serves (`history/<instance_id>/...`, `log/<instance_id>/...`) | appends | never writes under the old instance id | Keys are prefixed by the writing child's own instance id (a UUID per process). The new child never owns an old instance id, so the key sets are disjoint and each row has one writer. Appends are insert-only with a unique key, so a retry cannot clash. |
| Its own heartbeat row (`frontend/<instance_id>`) | writes | writes its own | Keyed by instance id, as above. |
| `done/` rows | reads only | writes | The old child follows them to finish its streams. |
| CAS blob ownership, ByteStream metadata | idempotent inserts | idempotent inserts | Content-keyed, the same value from either writer. |

The rule in code: a child after `Fenced` or `stand-down` may write only keys carrying its own instance id, plus the content-keyed data path. A test asserts this by running a write-recording store wrapper through a swap and failing on any other key from the old child.

**Fence scope.** Scheduler, lease, requeue, reconcile, controller, `update/*` and `desired` writes are fenced. The rows above marked "appends"/"idempotent inserts" are exempt so streams on A do not fail mid-flight after takeover. A `QueryWriteStatus` for an upload that B does not know returns `committed_size = 0` so the client restarts cleanly.

**Idempotent state writes.** Every state write has a unique key and is safe to retry, because a connection that dies after COMMIT leaves the outcome unknown. Test: restart Postgres under 200 streams; no errors, no duplicate runs.

**Epoch rule.**
- Takeover from a live holder keeps the epoch.
- A cold start (no live holder) bumps it. This includes the restart that starts the new release again because its child died (section 6); the old child, stood down, holds nothing and does not count as a live holder.
- A candidate's open and migrations never bump.
- A rollback takeover is allowed only when the holder is alive and agrees, or demonstrably dead (advisory lock gone). The proxy never asks two children to take the generation at once.
- Takeover is a full state reload, so a rolled-back child sees the member map, cordons, `desired` and release index that the other child changed. Test: B mutates, roll back, A sees it.

**Schema.** Release N+1 may only expand (additive) while N runs; N+2 may contract. In v1 this is a review rule plus a cross-version CI test, not a `min_reader` table. Rolling back past a contract is not supported and the docs say so. Size `max_connections` for two children plus migrations; use `CREATE INDEX CONCURRENTLY`, avoid table rewrites.

**Not provided.** Full concurrency (per-session `session_gen`, batch pull-claim, a `draining` role, per-host generations) is sized L to XL in `split-mode-updates.md` and is deferred. `Store::demote` has no Postgres implementation and none is written, because the fence refuses the old child's writes. **Mandatory test:** a result committed through frontend B reaches a waiter on frontend A.
## 9. Frontend-first order and compatibility

The frontend host updates via section 5. After bake, the new child writes `desired` and its controller updates the nodes. `kbf-installer deploy` waits on `GET /v1/update/frontend` reaching `done` and no longer posts node updates itself.

| Rule | Statement | Enforced by |
|---|---|---|
| R1 | The new frontend accepts every protocol from `min_protocol` up. A release never raises `min_protocol` in the release that introduces new behavior. | Preflight: `new.min_protocol <= min(connected node.protocol)` from the active child's `/v1/nodes`. |
| R4 | The node controller runs only on the generation holder. `desired.commit` must not be older than that child's own commit; `POST /v1/update/desired` returns 409 otherwise. | The store's generation fence refuses a superseded controller's writes. Beyond it, the controller makes no look while its farm is quiesced or handed over, its transport refuses a cordon or an order then, and its task ends when its store handle is superseded (`update/runner.rs`, `update/session.rs`). The `desired.commit` check is not in place yet. |
| C1 | Shipped daemons check `Welcome.protocol_version == PROTOCOL_VERSION`, which cannot be fixed in old daemons. | The N+1 frontend sends `Welcome.protocol_version = min(hello protocol, own)`. The cross-version test uses a real released N daemon binary. |

Stated limits, written in the docs rather than enforced in v1: the member-map floor is not set by an operator. A fresh map starts at the oldest protocol the frontend accepts (`OLDEST_PROTOCOL`, not its own newest `PROTOCOL`, which would lock N nodes out) and every later map carries `max(stored floor, OLDEST_PROTOCOL)`; `register` judges a node against that floor, read from the store, and not against a constant (tests: `a_fresh_stores_first_map_floor_is_the_oldest_protocol_the_build_accepts`, `members_floor`). Storage nodes do not enforce `Map::min_protocol` today. The floor never falls: a frontend rolled back to a build with a lower oldest keeps the stored floor, which is the flag day named next. An operator-set floor waits for R1 to read the members' protocols. Rollback across a protocol bump after nodes updated is a flag day (stop nodes, downgrade, start); several frontend hosts need a minimum-over-hosts rule, deferred with multi-host.

**Cross-version test.** CI fetches release N and runs N nodes against an N+1 frontend and the reverse over the real worker protocol. A planted `min_protocol` raise must go red.

**First crossing.** The first update to a release with this layout cannot be a swap. It is a stop and start with a fresh Postgres (section 11), done by `kbf-installer migrate` (#1083) without any state copy.

## 10. The proxy's own update

- `kbf-installer` updates only by apt or brew. The postinst does not restart the running proxy; it writes a marker and `GET /v1/update/frontend` shows `restart_pending: true`.
- **Decided:** `kbf-installer restart-proxy` waits for zero open streams by default (`--now` overrides). Socket activation keeps the listening socket, so new connections wait in the accept queue, and children keep running and are adopted.
- **What is dropped with `--now`:** every open client stream through the old proxy, including worker sessions (daemons redial in seconds). Buck2 does not recover a broken stream, so a build in flight fails. Acceptable because the proxy changes with package releases, not kbf releases. A hot restart with `SO_REUSEPORT` is possible and is not planned.
- **Frozen contract** between proxy and children: ports, `/readyz`, the identity headers, and the admin calls `takeover`, `quiesce`, `stand-down`, `resume`, `role`, versioned as `admin_protocol` in `kbf --version --json` (replacing `handover_protocol` from #1054). `resume` (built in U2) is the inverse of `quiesce`/`stand-down` while the process lives: full reload from the store, then re-take of the generation if the holder is dead or agrees. The proxy sends it on takeover failure, `abort` and rollback. The three crash points the U7 proof names: before takeover returns, after takeover before the swap, and the old child dying while draining.

## 11. Postgres, SQLite removal, fresh start

**Placement.** One Postgres from the OS package per frontend host, on a Unix socket only, role and database `kbf`, peer authentication (no password in argv or env), URL `postgres:///kbf?host=/run/postgresql`. `synchronous_commit` stays on and the store checks it at open. `kbf-installer` declares `Depends: postgresql` and `setup` creates the role and database. Off-host Postgres (client TLS) is out of scope until a second frontend host is planned. All-in-one gets the same dependency; the SQLite default and `STATE_FILE` go away.

**Backup.** A systemd timer running `pg_dump` replaces `VACUUM INTO`. The first backup must succeed and be restore-tested.

**No migration (decided).** The current state is dropped and the farm starts fresh on Postgres. There is no `kbf state migrate`. Cached blobs live on storage nodes and are unaffected.
- **What the owner loses:** the member map and enrollment (nodes enroll again), cordons, `desired`, the release index, run history and `done/` rows. In-flight work is dropped, so the cutover is drain-then-stop: no running builds. U0 checks that a storage node re-registers its existing blobs without re-upload, and that the member-map floor of a fresh store does not lock out N nodes (section 9).
- **Epoch:** a fresh store starts a new epoch; daemons discard old-epoch work, and nothing is running.
- **Order.** U0 verifies. U1/U2 wire Postgres (at the tip `kbf-modes.md` says Postgres is "not wired yet", the `state-store` rule refuses `postgres://`, and there is no `Store::demote`) and run the whole test matrix on both backends. The farm cutover is a held script (dry run by default, never forcing) that needs the owner's go per host: stop, install Postgres, start the new layout, re-enroll nodes. Rollback before cutover is restarting the SQLite unit; the file is untouched, and a Postgres row is never copied back.
- **Delete SQLite last (U3b)**, once the farm runs on Postgres and the owner confirms there is no second host. `Store::demote` and the baton go in that unit. `rusqlite` leaves `cargo tree`.

## 12. PR plan

Owners are left blank. Order: U0, then U1/U2, U3a and the farm cutover, then U4a, U4b, U5, U7a, U8a, U7b, U8b, U9, then merge #1091, then U7c, U3b. U7a and U8a build against traits and fakes and can start now.

**Installer PRs these units need** (KI section 9): P1 skeleton and golden fixtures (#1048), P2 layout and state schema 2 (#1053), P4 `orders/` reader and status writer (#1037), P5 `run` with gc (#1051), P8 verify pipeline (#1050), P7 fetch child and URL source (#1084). U7b and U8b need all six merged; U7a and U8a need none.

| Unit | Content (crate/files) | Depends on | Proof |
|---|---|---|---|
| U0 | Verify first (report plus a failing test per false assumption): `Fenced` survivable in reconciler and requeue; Unix-socket URI parses; `Store` key families as in section 8; atomic fence; blobs re-register after a fresh store. | none | Everything below depends on it. |
| U1 | Postgres in `kbf-server` and `kbf-all-in-one`: `StateStoreUri::Postgres`, `kbf-deploy` unit rule, flags, epoch rule, `synchronous_commit` check. | U0 | `frontend`, `state_store`, `history` tests on both backends; a CI job and image exist or are added. |
| U2 | `kbf-server`: `--await-takeover`, `takeover`, `quiesce`, `stand-down`, `role`; atomic fence and sticky flag; own-instance-key write rule; full reload; idempotent writes. | U1 | Two children on real Postgres: 50 Executes on A, takeover on B, all finish through A. Mutants: skip the generation take; assign before commit; non-atomic fence; old child writes a foreign key. Each must go red. |
| U3a | `kbf-installer` / `kbf-deploy`: held fresh-start cutover script (no state copy). | U1 | Dry run on a copy of the layout; node re-enroll check. |
| U4a | `kbf-server`: accept `x-kbf-node-id`/`role` only from loopback with the per-boot secret; bind `Hello.node_id` to it; identity- or token-based write gates. | U0 | Header without secret ignored; forged node id refused; direct non-loopback call refused. |
| U4b | `kbf-proxy` library and `proxy` verb, static upstream table: TLS termination, optional client cert and role checks, Buck2 auth modes, per-stream routing, permits, hold gate, keepalive, replay buffer. | U4a | Fake upstreams: 200 streams on A, flip to B, every A stream completes with no GOAWAY or RST. Idle 3 h under paused time not closed. ByteStream not retried after the first frame. Hold: 1000 held streams buffer at most the window, expire to `UNAVAILABLE`. Mutants: GOAWAY on flip, per-connection routing, trusting an inbound `x-kbf-node-id`, a window with no active child. |
| U5 | `kbf-daemon`, storage node, `kbf-server`: resend unacked completions; attempt-fenced results; `Welcome` echoes min protocol. | U0 | Real released-N fixture against N+1; kill the session between send and ack. |
| U6 (later) | Children on Unix sockets with `SO_PEERCRED` instead of loopback ports and secret. | U4b | Socket-path and peer-uid tests. |
| U7a | `kbf-installer`: supervisor and swap core behind `ChildControl`, `OrderWriter` and `Clock` traits, `update.json`, hold/quiesce/takeover/resume logic, rollback. | U2 | Fake-child tests for every phase and the three crash points; mutants: swap before takeover resolves, resume without reload, lost `update.json`. |
| U7b | `kbf-installer`: real child control and adoption, units, socket, postinst marker, all-in-one stop/hold/start, `frontend_update`/`confirm`/`revert` orders. | U7a, U4b, P1, P2, P4, P5, P7, P8 | Soak (below) and an all-in-one swap with 200 held streams fed to the new child. |
| U7c | Deletions: `EnvoyCheck`, the `Type=notify` handshake, `kbf-cli/src/handover.rs`, relay-to-successor in `kbf-server`, leftovers in `kbf-front`. Rewrites `frontend/handover.rs` and `frontend_drain.rs`. | U7b, #1091 | Grep and `cargo tree` show none remain; server tests pass. |
| U8a | `kbf-installer`: update API actor, operator-token auth, idempotency, status, rate limits, routes behind traits. | U4b (types only) | Auth matrix; 409 and 422; idempotent `request_id`; concurrent call test. |
| U8b | `kbf-installer`: wire U8a to `orders/`, `status.json` and the proxy; `force-retire`; OIDC seam (#1088 later). | U8a, U7b, P1, P2, P4 | End to end on fake children and a fixture spool. |
| U9 | R1 and R4 gates, controller generation gate, member-map floor fix, `deploy` default, cross-version test. | U2, U8 | Planted `min_protocol` raise red; two controllers without the gate red. |
| U3b | Delete SQLite, `STATE_FILE`, baton, `Store::demote`. | farm on Postgres | `rusqlite` gone from `cargo tree`. |
| U12 | Docs: `frontend-handover.md`, `aio-handover.md`, `kbf-installer.md`, `split-mode-updates.md`, `architecture-v2.md`, `kbf-modes.md`. | all | Wording matches code. |

**Cut from v1 (seams only):** off-host Postgres and client TLS, `min_reader` table, auto-rollback, a crash-point harness at every phase, hot proxy restart, multi-host rule.

**Soak** (in `kbf-it`, on the farm, real child processes; run by U4b and U7b): 200 concurrent streams on one h2 connection (held Execute and WaitExecution, ByteStream with checksums, unary) plus worker sessions with client certificates. Mid-run, the full swap. Assert: every pre-flip stream ends OK with the right payload; every post-flip stream is served by the new child (child id in a response header); the node id seen by the child equals the certificate's; no GOAWAY seen by the client; client errors and dropped-stream counters are 0; an Execute outliving stand-down by 11 minutes completes; daemons reconnect with the same epoch and keep leases; the old child exits only after its last stream; exactly one run per action. Mutants that must go red: route per connection; retire on a 5 s timer; GOAWAY at flip; promote or retire with a window that has no active child; drop the reconnect grace.

## 13. Existing PRs

| PR | Verdict |
|---|---|
| #970 controller (merged) | Keep. Gains the generation and desired-commit gate (U9). |
| #976 update routes (draft) | Keep. The proxy forwards them. |
| #1037 order spool | Keep and land first: `orders/` is the proxy-to-root channel and gates U7b. |
| #1062 `POST /v1/update/frontend` in kbf-server | Obsolete. Close after confirming nothing is stacked on its branch. |
| #1054 slots and `frontend_update` | Change. Slots and `pending_confirm` for frontends go away; confirm and undo become the `confirm`/`revert` orders sent by the proxy supervisor; `handover_protocol` becomes `admin_protocol`. Do not merge as is. Name the salvageable tests first. |
| #1047 gamma `deploy` | Change. Targets the proxy route, frontend-first, no `--order`. Stacked on #1039: read it first. |
| #1083 `migrate` | Change. Becomes the first-crossing stop and start on a fresh Postgres; no state copy. |
| #1091 drop Envoy unit | Keep, but blocked until U4b to U7b land, or a host has no front door. Mark it so nobody merges it. |
| #1088 GitHub OIDC | Keep. Becomes the remote trigger credential for the update API (U8 seam). |
| Merged #937, #930, #984, #1042 | Drain endpoint and `done/` follower stay. Baton, `EnvoyCheck`, the +10 block and `kbf-cli frontend handover` are deleted (U7, U3b). |

## 14. Open questions

None blocking. Decided by the owner on 2026-10-18: proxy terminates all TLS (no ALPN/SNI prerequisite), unprivileged proxy, all-in-one hold, old child scheduling-read-only, SQLite dropped with a fresh start, one update at a time, proxy restarts only with package upgrades (idle wait by default), frontend-first order.

**Not verified (U0 and the owning unit check these):** the atomicity and survivability of `Fenced`; the Postgres Unix-socket URI; the key family names in section 8 and a `Store` key scan; `predecessor_instance_id` continuation; hyper http2 features in the lockfile; the existing "Who may write" model and #1088; the `daemon.rs` `Welcome` check and `members.rs` floor behavior; that storage nodes re-register blobs on a fresh store; the CI Postgres job.
