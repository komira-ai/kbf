# Frontend zero-drop update: bundled proxy, per-stream swap, Postgres side by side

Status: design for review, 2026-10-18, revised after a correctness review and a scope review. Written against `origin/feature/arch-v2`. Claims about code that I did not re-read are listed in section 14 and must be checked by the unit that depends on them (unit U0 does the first ones).

## 1. Decisions

1. **One public port per frontend host**, owned by `kbf-proxy`, shipped in the `kbf-installer` package as the `kbf-installer proxy` verb. It is a separate unprivileged service, not part of the root helper.
2. **Routing is per HTTP/2 stream.** The proxy terminates TLS for REAPI and the operator API, opens its own upstream streams, and never sends GOAWAY to a REAPI or operator client. Worker sessions are not terminated; they are routed per connection and passed through.
3. **Children are plain `kbf --mode frontend` processes** that the proxy starts and supervises. In v1 they listen on loopback ports at fixed per-slot offsets. Unix sockets are a later change (section 13).
4. **Side by side on Postgres means one writer at a time.** The store-wide generation fence decides who writes. The old child becomes a read-only follower and finishes its open streams from `done/` rows. Concurrent writers are a later track (section 8).
5. **Update order is frontend first.** The new frontend, once baked, updates the storage nodes and daemons. An N+1 frontend must serve N nodes (section 9).
6. **SQLite is dropped. Postgres is a runtime dependency everywhere**, including all-in-one. The state move happens first and alone, under the existing front door (section 11).
7. **The proxy updates only with the package** (apt or brew). The restart is operator-timed and drops open client streams (section 10).

## 2. Process model on a frontend host

| Process | User | Listens | Job |
|---|---|---|---|
| `kbf-installer run` (root helper) | root | nothing | Verifies and installs releases under `/var/lib/kbf-installer/releases/<commit>/`, reads `orders/`, spawns the unprivileged fetch child. Contract unchanged. |
| `kbf-installer proxy` (`kbf-proxy.service` + `.socket`) | `kbf` | the public port, from systemd socket activation (`O_CLOEXEC`) | Demux, TLS, routing, update API, child supervision, open-stream accounting. |
| `kbf --mode frontend`, slot `a` or `b` | `kbf` | loopback ports at slot offsets; admin calls on a separate loopback port | The frontend. Plain h2. |
| Postgres | `postgres` | Unix socket | Shared state (section 11). |

- **The proxy runs as `kbf`.** A process that terminates all REAPI TLS can already read what matters, and a user split would need a privileged spawn path. The root helper keeps its "no socket, no network" property. The proxy talks to it only through the `kbf`-owned `orders/` directory and `status.json` (the existing spool contract). No new control socket.
- **Doc change:** `kbf-installer.md` "no installer network listener" becomes "the root helper has no listener; the `proxy` verb is a separate unprivileged service". Hardening applies per service.
- The proxy can ask only "install commit X". It cannot name a path or URL. The root half accepts a commit only if it is complete for this platform in the configured release source and passes attestation, so a stolen token can deploy only a signed release. A downgrade needs `rollback: true` and is recorded.
- **Supervision.**
  - Children are not killed when the proxy dies (`KillMode=process`; verify with a real `systemctl restart kbf-proxy`). A restarted proxy adopts them using a `pidfd`, the recorded start time and `/proc/<pid>/exe`, never the bare pid.
  - Liveness of the generation holder is a Postgres session advisory lock on its dedicated connection. It is released on death, so "the holder is dead" is not a guess from a pid.
  - The proxy pins the release directory of each live slot and of the rollback target against GC.
  - After a reboot both slots are dead. The proxy starts the last active slot recorded in `status.json`, which is a cold start with an epoch bump (section 8), and fails safe if that release is missing.
  - Hardening: `OOMScoreAdjust=-900` for the proxy, high `LimitNOFILE`, bounded buffers, header-list caps, h2 rapid-reset and CONTINUATION-flood limits, `catch_unwind` per connection, fuzzed demux.

## 3. Demux and per-stream routing

**Demux on the one port.** The proxy reads the ClientHello (`LazyConfigAcceptor`).
- ALPN `kbf-worker/1`, or SNI equal to `worker_sni` (an `installer.toml` key, also a SAN on the server certificate): do not terminate. Dial the active child's worker port and `copy_bidirectional`. mTLS stays end to end, so the `Hello.node_id` to client-certificate binding is unchanged. ALPN is the primary signal. The no-SNI IP dial is a tested case.
- Anything else is terminated with the server certificate, no client certificate, ALPN `h2` and `http/1.1`. Certificate and key are reloaded on SIGHUP. 0-RTT is off.
- **No second public port.** Daemons and storage nodes must send the ALPN or SNI. That is a hard block for frontend-first and is Q2, not a detail.
- **Pre-stream limits.** "No timeouts" applies to streams, not to the front door. There are handshake, preface and header timeouts (about 10 s), connection and per-IP caps, and rate limiting before any token check.

**Routing after termination.** The six REAPI paths and ByteStream go to the REAPI port. `/v1/update/frontend*` is served by the proxy. Every other `/v1/*` path (including the forwarded `/v1/update/desired`, `/pause`, `/resume`, `GET /v1/update`) goes to the operator port. `/healthz` and `/readyz` are the proxy's view: 200 only if a child is active and ready. The proxy strips inbound `x-kbf-*` headers.

**Binding rule.** A client stream binds to the slot that was current when its headers arrived, until the stream closes. The bind and the count are one step: the proxy takes a permit together with the slot load. Retire needs all permits of the old slot and "not current". The same rule covers passthrough connections that are mid-ClientHello. One Buck2 connection therefore carries pre-switch streams to the old child and post-switch streams to the new one. Needs a stress test, and a loom test of the permit.

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
   old: {open_streams, open_sessions, oldest_stream_age_s}, new: {open_streams},
   failure: {step, reason} | null}
POST /v1/update/frontend/abort        before "switching"
POST /v1/update/frontend/rollback     after "switched" while the old child lives and no node updated
POST /v1/update/frontend/force-retire explicit, needs a reason, counts dropped streams, never automatic
```

Phases: `accepted, fetching, verifying, staged, preflight, starting, ready, quiescing, switching, switched, baking, nodes, draining, done`, plus `failed` and `rolled_back`.

- **One state machine, one actor.** All API calls go through a single mutex or actor. Test: fire `POST`, `abort` and `rollback` at once.
- **State of record: two sources, no third.** The installer boundary (`orders/`, `status.json`) records fetch and verify. The Postgres generation holder, plus `GET role` on each child, decides who is active. The proxy re-derives the phase from those on restart. There is no `update-state.json` and no store row (the #1062 row is dropped). The `request_id` map is kept in a small fsynced file in the proxy state dir; that is a cache for idempotency, not a source of truth. Test a kill between fsync and the 202.
- **Auth, v1.** `Authorization: Bearer <token>` compared in constant time against a token in `installer.toml`. Failures are rate limited before any hashing. The token is never in a query string or a log. The auth check sits behind a small internal function so the GitHub OIDC work (#1088) can replace it. I have not read #1088, so nothing more is fixed here.
- **Operator-API trust.** Any "Who may write" gate in `docs/api.md` that relies on "peer is loopback" is open to the public once a proxy forwards. Making those gates token-only is a **hard prerequisite of the proxy merging** (U4), not a later audit.

## 5. Swap sequence

Each step records its phase, then acts, and every action is idempotent, so a crash resumes.

1. **Accept and preflight.** No update in flight, commit not running, `orders/` writable, active child answers. Answer 202.
2. **Fetch and verify** through an `install_release` order and the existing root path (fetch child, attestation, sha256, staging). A failure changes nothing.
3. **Compatibility gate** (section 9): run `releases/<commit>/kbf --version --json`, apply R1 and R4. Refusal is `409 incompatible` naming the rule, before any process starts.
4. **Start candidate** in the idle slot with `--await-takeover`. It opens Postgres and runs expand-only migrations under an advisory lock with `lock_timeout` (about 2 s, with retry). It never bumps the epoch, never takes the generation, and runs no reconciler, scheduler or worker listener. If it exits in 30 s or is not ready in 120 s, kill it, `failed`. The old child is untouched.
5. **Readiness.** `/readyz`, a `grpc.health.v1` check, and one synthetic `GetCapabilities` through the candidate.
6. **Quiesce, takeover, swap** (`quiescing`, `switching`). The only step with a hold.
   1. Admission hold on new request headers. Held requests are not read (flow control holds the sender), and the queue is bounded. The hold limit (15 s) is longer than the takeover limit (10 s), and held streams are released to a slot only after takeover has resolved either way.
   2. **Quiesce the old child:** it stops scheduling, assigning and requeueing and flushes. An `Assign` is sent only after its lease write commits (section 7). This closes the window in which A assigns while B already holds the generation.
   3. `takeover` on the candidate: it takes the generation (atomically, section 8), reloads all state from the store, starts reconciler, scheduler and worker listener.
   4. The proxy swaps the active slot atomically, closes the passthrough worker connections that are bound to A (section 7), and releases the hold. Journal `switched`.
   - **Failure.** If takeover fails or exceeds 10 s, the proxy first kills the candidate (it never served; `pidfd`), then reads the generation row and roles, and only then tells the old child to resume. The old child resumes from the store, not from warm memory. Phase `failed`, step `switch`.
7. **Stand down the old child.** It ends its worker streams with `UNAVAILABLE "connect again"`. It keeps serving open Execute and WaitExecution streams from `done/` rows through the follower, with no GOAWAY and no timeout. ByteStream and CAS data-path calls continue (section 8). The relay-to-successor in `relay.rs` is turned off, because the proxy routes new streams: one mechanism only. The `done/` follower stays.
8. **Bake** (default 5 min). The proxy watches the new child. v1 has **no automatic rollback**; the status shows a red child and the operator runs `rollback`. Rollback means the old child re-takes the generation (after a full reload) and the new one is stood down. It is refused once any node updated.
9. **Nodes.** If `nodes: true`, the proxy asks the new child for `POST /v1/update/desired {commit}`. The child's node controller (#970, routes #976) updates storage nodes and daemons. A failure here does not roll the frontend back. Node cordon and drain count open streams from either frontend (test: drain a node that has a stream via the old child).
10. **Drain and retire.** When the proxy's count on the old slot is 0 and the old child reports `open_streams == 0` and `open_sessions == 0`, the proxy sends SIGTERM and never SIGKILL. No timeout. A stuck stream stays visible in status. `force-retire` is the explicit escape. A new update is refused while a slot drains (Q5). `releases/` keeps the previous release.

**Hard limit.** A follower that cannot read the store for `DONE_FLOOR` (600 s) can lose a result. The old child fails that stream with `UNAVAILABLE`. Add the metric `kbf_follow_oldest_age_s`.

## 6. Crash behavior

| Crash point | Result |
|---|---|
| Steps 1-3 | No child started. Proxy restarts, re-reads `status.json`, resumes or fails the order. |
| Steps 4-5, candidate dies | One respawn, then `failed`. On proxy restart the old child is adopted as active and the candidate (never took the generation) is killed. |
| Step 6 before takeover returns | On restart ask both children `GET role`; the generation holder is active. The hold is in memory and vanishes. |
| Step 6 after takeover, before the swap | The generation holder is active. The other child gets stand-down. |
| Old child dies while draining | Its open streams are lost. Unavoidable. Results in `done/` stay. The update continues. |
| New child dies during bake | If the old child is alive, the operator rolls back. Otherwise systemd restarts it on the same slot, and that start is a **cold start with an epoch bump**, which invalidates leases and runs (section 8). |
| Proxy crash | systemd restarts it, socket activation queues new connections. **All client streams through the proxy are cut**, including streams to the old child. Main residual risk of a single front door. Measure it with systemd restart counts and child-side "peer vanished" counters; the proxy cannot count its own crash. |
| Root installer crash | Not in the data path. Resumes from `orders/`. |
| Postgres restarts | See ambiguous commits, section 8. Pin `postgresql` restarts out of the unattended-upgrade window or order them with the update. |

## 7. Daemon and storage-node sessions across the swap

- A `Worker.Session` is passed through to the active child at connect time. At the flip the old child ends its worker streams with `UNAVAILABLE`, **and the proxy closes the passthrough TCP connections bound to it.** Otherwise a pooled h2 connection would carry the redial back to the old child for ever. This is a deliberate exception to "never GOAWAY", which covers REAPI and operator clients only. Test: the daemon's second Session is served by the new child.
- **Assign after commit.** A child sends an assignment only after its lease write committed.
- **Completions.** A child never acknowledges a completion before its commit. The daemon keeps unacked completions and resends them on the new session. The new child accepts a completion for a lease granted under the previous session, keyed by (lease id, attempt), first valid writer wins, and rejects a stale attempt. Test: kill the session between send and ack; the result is neither lost nor duplicated.
- **Reconnect grace.** After takeover the new child does not expire leases of daemons that have not yet redialed. The grace is tied to lease expiry and node heartbeat, not a bare constant (initial value 30 s, configurable). Attempt-fenced results make a long partition safe: the late result is rejected, not run twice. A daemon with no session and no lease abandons its work.
- A redial of a known instance is a continuation (`predecessor_instance_id`), not a stale-stream drop. Needs a test, not an assumption.
- **Execute retry after a cut stream.** Buck2 re-sends Execute. In-flight dedup is a store op row keyed by (instance, action digest), so a new Execute attaches to the existing op, on either child. Test: Execute, kill the proxy, Execute again, assert exactly one run.
- Storage-node `Membership` and register calls take the same route. Both children may dial nodes during a drain, so a test checks that storage nodes assume no singleton frontend.

## 8. Postgres side by side

**Fencing must be atomic with the write.** A separate SELECT of the generation before a write lets A pass, B take, and A still commit. Each fenced write does the check inside its transaction (`SELECT gen ... FOR SHARE`, or `UPDATE ... WHERE gen = $1`). After the first `Fenced` the child sets a sticky flag: it stops scheduling, assigning and requeueing, never retries, and does not exit. Test: hold A's transaction open, take over on B, assert A's commit fails or B's takeover waits.

**Fence scope.** Only scheduler, lease, requeue, reconcile, controller, `update/*` and `desired` writes are fenced. The data path (CAS blob ownership, ByteStream metadata) is exempt: idempotent inserts keyed by content, so uploads on A do not fail mid-stream after takeover. A `QueryWriteStatus` for an upload that B does not know returns `committed_size = 0` so the client restarts cleanly.

**Idempotent state writes.** Every state write has a unique key and is safe to retry, because a connection that dies after COMMIT leaves the outcome unknown. Test: restart Postgres under 200 streams; no errors, no duplicate runs.

**Epoch rule.**
- Takeover from a live holder keeps the epoch.
- A cold start (no live holder) bumps it. This includes a crash of the new child during bake.
- A candidate's open and migrations never bump.
- A rollback takeover is allowed only when the holder is alive and agrees, or demonstrably dead (advisory lock gone). The proxy never asks two children to take the generation at once.
- Takeover is a full state reload, so a rolled-back child sees the member map, cordons, `desired` and release index that the other child changed. Test: B mutates, roll back, A sees it.

**Schema.** Release N+1 may only expand (additive) while N runs; N+2 may contract. In v1 this is a review rule plus a cross-version CI test, not a `min_reader` table. Rolling back past a contract is not supported and the docs say so. Size `max_connections` for two children plus migrations; use `CREATE INDEX CONCURRENTLY`, avoid table rewrites.

**Not provided.** Full concurrency (per-session `session_gen`, batch pull-claim, a `draining` role, per-host generations) is sized L to XL in `split-mode-updates.md` and is deferred. `Store::demote` has no Postgres implementation and none is written, because the fence refuses the old child's writes. **Mandatory test:** a result committed through frontend B reaches a waiter on frontend A.

## 9. Frontend-first order and compatibility

The frontend host updates via section 5. After bake, the new child writes `desired` and its controller updates the nodes. `kbf-installer deploy` waits on `GET /v1/update/frontend` reaching `done` and no longer posts node updates itself.

| Rule | Statement | Enforced by |
|---|---|---|
| R1 | The new frontend accepts every protocol from `oldest_protocol` up. A release never raises `oldest_protocol` in the release that introduces new behavior. | Preflight: `new.oldest_protocol <= min(connected node.protocol)` from the active child's `/v1/nodes`. |
| R4 | The node controller runs only on the generation holder. `desired.commit` must not be older than that child's own commit; `POST /v1/update/desired` returns 409 otherwise. | Controller tick checks the generation. |
| C1 | Shipped daemons check `Welcome.protocol_version == PROTOCOL_VERSION`, which cannot be fixed in old daemons. | The N+1 frontend sends `Welcome.protocol_version = min(hello protocol, own)`. The cross-version test uses a real released N daemon binary. |

Stated limits, written in the docs rather than enforced in v1: the member-map floor must be set by an explicit operator action (today a fresh map starts at the frontend's own `PROTOCOL`, which can lock N nodes out; fix it in U9 with a restored-store test); rollback across a protocol bump after nodes updated is a flag day (stop nodes, downgrade, start); several frontend hosts need a minimum-over-hosts rule, deferred with multi-host.

**Cross-version test.** CI fetches release N and runs N nodes against an N+1 frontend and the reverse over the real worker protocol. A planted `oldest_protocol` raise must go red.

**First crossing.** The first update to a release with this layout cannot be a swap. It is a stop and start done by `kbf-installer migrate` (#1083).

## 10. The proxy's own update

- `kbf-installer` updates only by apt or brew. The postinst does not restart the running proxy; it writes a marker and `GET /v1/update/frontend` shows `restart_pending: true`.
- `kbf-installer restart-proxy` waits for zero open streams by default (`--now` overrides). Socket activation keeps the listening socket, so new connections wait in the accept queue, and children keep running and are adopted.
- **What is dropped:** every open client stream through the old proxy, including worker sessions (daemons redial in seconds). Buck2 does not recover a broken stream, so a build in flight fails. Acceptable because the proxy changes with package releases, not kbf releases. A hot restart with `SO_REUSEPORT` is possible and is not planned (Q4).
- **Frozen contract** between proxy and children: ports, `/readyz`, and the admin calls `takeover`, `quiesce`, `stand-down`, `role`, versioned as `admin_protocol` in `kbf --version --json` (replacing `handover_protocol` from #1054).

## 11. Postgres, SQLite removal, farm migration

**Placement.** One Postgres from the OS package per frontend host, on a Unix socket only, role and database `kbf`, peer authentication (no password in argv or env), URL `postgres:///kbf?host=/run/postgresql`. `synchronous_commit` stays on and the store checks it at open. `kbf-installer` declares `Depends: postgresql` and `setup` creates the role and database. Off-host Postgres (client TLS) is out of scope until a second frontend host is planned. All-in-one gets the same dependency; the SQLite default and `STATE_FILE` go away. An all-in-one update is stop and start (Q3).

**Backup.** A systemd timer running `pg_dump` replaces `VACUUM INTO`. The first backup must succeed and be restore-tested before the SQLite file is retired.

**Order: the state move comes first and alone, under the existing Envoy and handover.** It is the one irreversible step and must not wait for the proxy.
1. U0 verifies the assumptions. U1 and U2 are real feature work: at the tip `kbf-modes.md` says Postgres is "not wired yet", the `state-store` rule refuses `postgres://`, and there is no `Store::demote`.
2. U1/U2 wire Postgres and run the whole test matrix on both backends. SQLite stays compiled.
3. `kbf state migrate --from sqlite://... --to postgres://...` copies the durable keys (member map, release index, `update/desired`, epoch, enroll and history rows) and drops `op/`, `done/` and `lease/`. Because in-flight work is dropped, **the cutover is drain-then-stop: no running builds.** The source is opened read-only and the tool refuses while a frontend holds the lock. A dry run on a copy of the live file (taken while stopped) is compared key by key before the real cutover, not only the golden `state/v1.db`.
4. **Epoch:** the first start on migrated state is a cold start. Decision: carry the old epoch value forward and bump it once, deliberately, in the migration, so daemons discard old-epoch work; the cutover has no running work, so nothing is lost.
5. The farm cutover is a held script (dry run by default, never forcing) that needs the owner's go for each host. Rollback before cut-over is restarting the SQLite unit; the file is untouched. A Postgres row cannot be copied back. Re-registering nodes through the control API instead of copying is possible and loses lease history and `done/` rows.
6. Delete SQLite last (U3b), once the farm is migrated and the owner confirms there is no second host. `migrate`, `Store::demote` and the baton go in that unit, not earlier. `rusqlite` leaves `cargo tree`.

## 12. PR plan

Order: U0, then U1/U2, U3a and the farm cutover, then U5 (daemon ALPN release), then U4 with loopback children, U7, U8, U9, then merge #1091, then U3b.

| Unit | Content | Proof |
|---|---|---|
| U0 | Verify first: `Fenced` is survivable in reconciler and requeue; the Postgres Unix-socket URI parses; `Store` can scan every durable key; atomic fence; whether CAS/ByteStream touch fenced writes. | A short report and a failing test for each assumption that is false. Everything below depends on it. |
| U1 | Postgres in `kbf-server` and `kbf-all-in-one`: `StateStoreUri::Postgres`, the `kbf-deploy` unit rule, flags, epoch rule, `synchronous_commit` check. | Server `frontend`, `state_store` and `history` tests on both backends; confirm a CI job and image exist, else add them. |
| U2 | `--await-takeover`, `takeover`, `quiesce`, `stand-down`, `role`; atomic fence and sticky flag; `Fenced` read-only follower; full reload on takeover; idempotent writes; fence-exempt data path. | Two children on real Postgres: 50 Executes started on A, takeover on B, all finish through A while B accepts new ones. Rollback takeover works. Mutants: skip the generation take; assign before commit; non-atomic fence. Each must go red. |
| U3a | `kbf state migrate`, held cutover script, live-file dry run. | Round trip on golden and on a copy of the live file, key-by-key compare, refusal while a frontend holds the lock. |
| U5 | Daemon and storage node send ALPN `kbf-worker/1`/SNI; daemon resends unacked completions; attempt-fenced results; `Welcome` echoes min protocol. | Real released-N fixture against N+1; kill the session between send and ack. |
| U4 | `kbf-proxy` library and `proxy` verb with a static upstream table: TLS, ALPN split, per-stream routing, permit binding, hold gate, keepalive, replay buffer, close-at-flip. Token-only write gates merge first. | Fake upstreams: 200 streams on A, flip to B, every A stream completes with no GOAWAY or RST, new streams hit B. Idle 3 h under paused time not closed. ByteStream not retried after the first frame. Mutants: GOAWAY on flip, per-connection routing, terminating on the worker ALPN, a window with no active child. |
| U6 (later) | Children on Unix sockets instead of loopback ports. | Socket-path tests; the +10 offset removed. |
| U7 | Swap orchestrator, supervisor, adoption, units, socket, postinst marker. Removes `EnvoyCheck`, the `Type=notify` handshake, `kbf-cli/src/handover.rs`, and relay-to-successor; rewrites `frontend/handover.rs` and `frontend_drain.rs`. | Soak (below) plus three crash points: before takeover, after takeover, old child dying while draining. |
| U8 | Update API: routes, token auth, status, rate limits, `force-retire`. | Auth matrix; 409 and 422 cases; idempotent `request_id`; concurrent call test. |
| U9 | R1 and R4 gates, controller generation gate, member-map floor fix, `deploy` default, cross-version test. | Planted `oldest_protocol` raise red; two controllers without the gate red; `sim_rollout` N+1 frontend with N nodes. |
| U12 | Docs: `frontend-handover.md`, `aio-handover.md`, `kbf-installer.md`, `split-mode-updates.md`, `architecture-v2.md`, `kbf-modes.md`. | Wording matches code. |

**Cut from v1 (kept as seams only):** off-host Postgres and client TLS, Argon2 and an authenticator trait, `min_reader` table, auto-rollback, a crash-point harness at every phase, the all-in-one swap, hot proxy restart, multi-host rule. Add them when a second host or a real need exists.

**Soak** (in `kbf-it`, on the farm, real child processes; run by U4 and U7): 200 concurrent streams on one h2 connection (held Execute and WaitExecution, ByteStream with checksums, unary). Mid-run, the full swap. Assert: every pre-flip stream ends OK with the right payload; every post-flip stream is served by the new child (child id in a response header); no GOAWAY seen by the client; client errors and dropped-stream counters are 0; an Execute outliving stand-down by 11 minutes completes; daemons reconnect with the same epoch and keep leases; the old child exits only after its last stream; exactly one run per action. Mutants that must go red: route per connection; retire on a 5 s timer; GOAWAY at flip; promote or retire with a window that has no active child; drop the reconnect grace.

## 13. Existing PRs

| PR | Verdict |
|---|---|
| #970 controller (merged) | Keep. Gains the generation and desired-commit gate (U9). |
| #976 update routes (draft) | Keep. The proxy forwards them. |
| #1037 order spool | Keep and land first: `orders/` is the proxy-to-root channel and gates U7. |
| #1062 `POST /v1/update/frontend` in kbf-server | Obsolete. Close after confirming nothing is stacked on its branch. |
| #1054 slots and `frontend_update` | Change. Slot, confirm and undo logic moves into the proxy supervisor; `handover_protocol` becomes `admin_protocol`. Do not merge as is; tell the owner now so no more is added. Name the salvageable tests first. |
| #1047 gamma `deploy` | Change. Targets the proxy route, frontend-first, no `--order`. Stacked on #1039: read it first. |
| #1083 `migrate` | Change. Adopt the new units and slots; gains the Postgres step. It does not block the state move (section 11 runs under the old layout), so split the Postgres step out. |
| #1091 drop Envoy unit | Keep, but blocked until U4 to U7 land, or a host has no front door. Mark it so nobody merges it. |
| #1088 GitHub OIDC | Keep. Replaces the v1 token check. |
| Merged #937, #930, #984, #1042 | Drain endpoint and `done/` follower stay. Baton, `EnvoyCheck`, the +10 block and `kbf-cli frontend handover` are deleted (U7, U3b). |

## 14. Open questions, with recommendations

- **Q1. Proxy in the package, as a separate unprivileged process.** Yes: a public parsing process should not be root, and the root half keeps its no-listener property.
- **Q2. Old daemons send no ALPN or SNI, and frontend-first means the first proxy frontend is older than its nodes' next release.** This is a hard block. (a) Ship a daemon and node release (U5) before the proxy package; a one-time node-first exception on the farm. (b) A second worker port: contradicts the ruling. (c) The proxy terminates worker TLS with an optional client certificate, routes `/kbf.worker.*` and `Membership` per stream, and passes the verified node id in a header trusted only from the proxy; this moves the `Hello.node_id` binding into the proxy, a trust change. Recommendation: (a).
- **Q3. All-in-one updates by stop and start.** Accept.
- **Q4. Proxy restart drops open streams; default waits for idle.** Accept, track the counters.
- **Q5. No second update while an old child drains.** Accept; `force-retire` is the escape.
- **Q6. Preserve the farm's state through `kbf state migrate`.** Yes, with the owner's go per host.
- **Q7. One writer at a time during the drain.** Accept; add concurrent writers when a second host is planned.

**Not verified (U0 and the owning unit check these):** the atomicity and survivability of `Fenced`; the Postgres Unix-socket URI; `Store` key scan; `predecessor_instance_id` continuation; hyper http2 features in the lockfile; whether tonic sets SNI independent of the dialed address; the existing "Who may write" model and #1088; the `daemon.rs` `Welcome` check and `members.rs` floor behavior; the CI Postgres job.
