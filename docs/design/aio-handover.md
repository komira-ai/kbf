# Updating a frontend or an all-in-one host behind Envoy, without failing a build

Status: design, not implemented. One update process for every shape: Envoy stays in front
of kbf on the host (also on a single box, also for `kbf all-in-one`); a new kbf process is
started on other ports, Envoy sends new work to it, and the old process drains: it stops
being ready, finishes what it holds and exits. This replaces the first version of this note,
which moved the listening sockets between processes by fd passing.

File references are to `origin/feature/arch-v2` (symbols are the authority, line numbers
rot). "Inference" marks a claim read from code shape or from Envoy's documentation and not
yet from a test; section 12 lists every inference the spike must turn into a measurement.

## 1. Goal and contract

1. Builds already running finish on the old process.
2. New requests go to the new process.
3. The old process exits when it holds nothing.
4. Envoy updates by its own hot restart (section 7), under the same three rules.

No client build may fail because of an update. Buck2 sends one `Execute` per action,
retries only the opening of that stream (5 attempts, sleeps of about 0.1, 0.2, 0.4 and 0.8 s
plus up to 50 ms), never calls `WaitExecution` or `GetOperation`, and fails an action whose
stream breaks after it started (`remote_execution/oss/re_grpc/src/client.rs`, read on
`main`; upstream facebook/buck2#1569 is open; the farm's record is kbf #791). So the old
process must answer every `Execute` it accepted, to the end, and Envoy must never cut one.

What each party sees:

| party | during a kbf update |
|---|---|
| Buck2 | Its connection to Envoy never changes. Open `Execute` streams continue on the old process. New streams are sent to the new one. No GOAWAY comes from kbf: Envoy keeps the downstream connection (section 8). |
| daemons (remote, and the in-process one of all-in-one) | The old process ends their streams. They redial the same worker address, which is Envoy's, and reach the new process. Leases are kept (the epoch is kept) and nothing is double-booked (section 6). |
| operator | `kbf handover` starts the other instance and, with `--wait`, polls the old one until it is gone. Operator reads (`/readyz`, `/v1/...`) are by instance on loopback; the public view is Envoy's. |

SQLite with the single-writer lock, `Store::demote` and the generation fence is enough for
the scheduler state; no Postgres. (Postgres has no `demote`, `kbf-state/src/store.rs`, and is
for two serving frontends.)

## 2. What was re-checked, and where the code disagrees with the brief

| claim | result | where |
|---|---|---|
| kbf sends nothing on a long Execute and has no HTTP/2 keepalive | **Not true on arch-v2.** Every open `Execute` and `WaitExecution` stream is re-sent its operation every `--progress-interval-secs` (default 30), and both listeners send an HTTP/2 PING every `--http2-keepalive-interval-secs` (default 30, timeout 20). Progress frames are data frames, so they reset Envoy's stream idle timer; a PING is connection-level and does not (inference). The Envoy overrides below stay, because both flags can be 0. | `kbf-server/src/config.rs` (`progress_interval_secs`, `http2_keepalive_*`), `kbf-front/src/execution.rs` (`progress`), `kbf-server/src/serve.rs` (`StreamTiming`, `keepalive`) |
| the longest action is 1 h | **Not a bound.** The container driver's default timeout is 1 h but an action's own `timeout` replaces it and is not capped. The envoy kit's 3900 s (1 h plus the 300 s unservable wait) is the default case only. | `kbf-driver-container/src/runtime.rs` (`timeout_of`, `default_timeout`), `crates/kbf-deploy/src/envoy/check.rs` (`SILENT`) |
| #857 is merged and stays needed | Yes: `Fenced`/`meta/generation`, `Store::demote` (SQLite), `done/<op>` rows with `DONE_FLOOR` (600 s). | `kbf-state/src/fence.rs`, `store.rs`, `sqlite/mod.rs`; `kbf-server/src/farm/durable.rs`, `kbf-front/src/finished_row.rs` |
| the handover halves of #879/#887 | **Not on arch-v2.** No `Farm::quiet/fence/follow/end_worker_streams`, no `handover/` module, no `open_after_handover`. `EpochPolicy::Keep` exists in `Durable::open`, but `StateStoreUri::epoch_policy` returns `Bump` for every SQLite start. Section 11 says what is reused from those branches and what is dropped. | `kbf-server/src/farm/durable.rs`, `kbf-server/src/state_store.rs`; branch `av2/frontend-handover-3` |
| `Drain::begin` is one step | Yes, and it cannot be staged today: it turns readiness off, refuses new Executes and sends GOAWAY in one call, once. Section 5 splits it. | `kbf-server/src/drain.rs` |
| a refused Execute is a trailers-only UNAVAILABLE | Yes: `execute` returns `Err(Status::unavailable(..))` as its first statement when `closing.draining()`, before any enqueue. That is the shape Envoy can retry (it reads the status in the response headers). | `kbf-front/src/execution.rs` |
| `/readyz` and the gRPC health service agree | Yes: both evaluate `Checks::failing`; `Readiness::stop` flips both. | `kbf-server/src/health.rs`, `grpc_health.rs` |
| kbf-deploy renders an Envoy config | Yes, for a **Kubernetes client front**, not a box: one cleartext h2c listener behind a Tailscale ingress, one STRICT_DNS cluster of servers over TLS to the farm CA, route timeout 0, stream idle timeout 7200 s, no `max_stream_duration`, `healthy_panic_threshold` 0, gRPC health check, a checker that models those properties. It has no circuit breakers, no retry policy, no worker route, no hot restart, and refuses a route whose action has anything but a cluster and timeouts. Section 4 builds on it. | `crates/kbf-deploy/src/envoy/{render,check,config}.rs` |
| a real Envoy is available to tests | Yes: CI fetches a pinned Envoy and validates the kit's render with `--mode validate`. | `.github/workflows/ci.yml`, `crates/kbf-deploy/tests/envoy_validate.rs` |

## 3. Topology on one box

One Envoy owns the public ports. kbf instances `a` and `b` listen on loopback port blocks.
Envoy's configuration is **static and lists both instances for ever**; a kbf update changes
no Envoy file and never reloads Envoy. Routing changes by health, nothing else.

```
 Buck2 --TLS--> Envoy :reapi --h2/TLS(CA)--> cluster kbf_reapi  { a:reapi, b:reapi }
 daemons --mTLS--> Envoy :worker --tcp-----> cluster kbf_workers { a:worker, b:worker }
 operators ------------------------------------> instance API on loopback (a or b)
 Envoy admin: loopback only
```

**Ports.** The public REAPI and worker ports are the ones clients and daemons dial today and
move to Envoy, so no client or remote daemon changes. Each instance gets three loopback ports
from one configured base: `a` base+0 (REAPI), +1 (worker), +2 (API); `b` base+10, +11, +12.
They are fixed at install time because the Envoy file is static; an ephemeral port would need
a dynamic endpoint source. `install-units` refuses a base whose six ports collide with each
other, with Envoy's, or with a listener already bound.

**REAPI listener (L7).** Envoy terminates the client-facing TLS (REAPI clients present no
certificate, `--reapi-tls-cert` doc in `kbf-server/src/config.rs`; the REAPI policy runs in
kbf and reads no peer certificate) and routes **per stream**. L4 would route per connection and
leave a client's long-lived connection, and every new stream on it, on the old process, which
is what the GOAWAY of the first design was for. Upstream is h2 over TLS to the farm CA, with the
kit's per-host SNI and SAN match. Routes are the kit's `ROUTES`.

**Worker listener (L4).** `tcp_proxy` to `kbf_workers`, no TLS in Envoy: the worker listener
is mutual TLS and `peers.admit` matches the client certificate's node id to `Hello.node_id`
(`kbf-server/src/worker.rs`); terminating it would lose the identity. Remote daemons keep
dialing the one address they dial today. The in-process daemon of all-in-one dials the same
public address (`--server=https://<host>:<worker port>` in
`templates/linux/modes/kbf-all-in-one.service`), so after it detaches it reaches the successor
the same way. Routing the worker port by instance number directly (no Envoy) was rejected: every
update would change the address a remote daemon must use.

**Clusters.** One cluster per listener, both instances in it, equal priority. Both can be
healthy for a few seconds during an update; a call the old instance can no longer serve is
refused in a retryable way (section 5), not dropped.

**Health.** The signal is kbf's readiness, which is `/readyz`:

- `kbf_reapi`: the kit's existing `grpc.health.v1` check on the REAPI port, empty service
  name. It is `/readyz`'s evaluation (`grpc_health.rs`); reusing it keeps the checker and the
  kbf-it test that pins the service names (`crates/kbf-it/tests/envoy_check.rs`).
- `kbf_workers`: an HTTP check of `/readyz` on the instance's API port (`health_check_config.port_value`),
  plain loopback HTTP, since the worker cluster has no TLS of its own. `--api-listen` is already in
  the unit and is required here.
- `interval` 1 s, `no_traffic_interval` 1 s, `timeout` 1 s, `unhealthy_threshold` 2, `healthy_threshold` 1.
  The kit's 5 s and 10 s are for a remote cluster; here they would be the update gap
  (section 5), and a loopback check is free. Starting values: the spike sets them.

### 3.1 The Envoy settings that keep a long Execute alive

Each row is a default that would cut a stream (or a limit that would refuse one), what the
kit sets today, and what the box render sets. "Check" is a rule `envoy check` must enforce, with a
mutant (section 10).

| setting | Envoy default | kit today | box render | check |
|---|---|---|---|---|
| route `timeout` | 15 s | `0s` on every route (`ROUTE_TIMEOUT`) | `0s`, REAPI routes | kept (`route-timeout`) |
| route `max_stream_duration` / `idle_timeout` | none / the HCM's | absent | absent | kept (`max-stream-duration`, `idle-timeout`) |
| HCM `stream_idle_timeout` | 300 s | `7200s` (`LONG_STREAM`) | `7200s`; the progress frame (30 s) keeps it reset, 7200 s covers `--progress-interval-secs=0` | kept (`idle-timeout`) |
| HCM `request_timeout` | disabled | absent | absent: a 64 MiB write streams | kept (`request-timeout`) |
| cluster `close_connections_on_host_health_failure` | false (and `tcp_proxy` only) | absent | false on both clusters: a failed check removes the host from selection and leaves its connections and open streams alone | **new** (`health-keeps-streams`) |
| cluster `healthy_panic_threshold` | 50 % | `0` | `0`: with both instances unready Envoy answers 503, it never sends to a fenced one | kept |
| circuit breakers `max_requests`, `max_connections`, `max_pending_requests`, `max_retries` | modest: 1024 connections per cluster is documented; the others, 1024 and 3, are from the cluster reference and are not re-checked here | **absent**: one open Execute is one request, so about 1000 concurrent actions would be refused with an overflow (the same gap exists in the Kubernetes front) | raised to at least the farm's concurrent-action ceiling (a config key, default 65536) | **new** (`breakers`) |
| `tcp_proxy` `idle_timeout` (worker) | 1 h | n/a | `0s`: a daemon's session stream is quiet between heartbeats and must not be cut | **new** (`worker-idle`) |
| upstream/downstream connection idle, `max_connection_duration` | 1 h with no streams / none | default | default (only idle connections close) | refuse a `max_connection_duration` |
| HTTP/2 upstream keepalive | none | none | `connection_keepalive` 30 s / 20 s, as kbf's own listeners | **new** |
| route `retry_policy` | none | none; the checker refuses any extra action field | `Execute` and the read-only unary calls only (section 5) | **new** (`retry-routes`) |
| HCM `drain_timeout` | 5 s | default | default (it times Envoy's own GOAWAY, section 7) | none |

## 4. Building on kbf-deploy's envoy kit

No parallel renderer. `envoy/render.rs` already holds the parts to reuse: `ROUTES`,
`ROUTE_TIMEOUT`, `LONG_STREAM`, `upstream_tls`, the cluster builder, the JSON writer that
makes equal settings equal bytes, and `check.rs` with its route model. Changes:

1. `render` gets a topology: `Front` (today's Kubernetes objects) and `Box`. `Box` writes
   only `envoy.json` (no manifests), with the two listeners, two clusters, admin on loopback.
2. `Server` is keyed by host **and port** (today a host named twice is `TwiceNamed`, and the
   TLS socket match is keyed by host); both instances share the box's name and differ in port.
3. New config keys for `Box`: the public ports, `KBF_BACKEND_PORT_BASE`, the client-facing
   certificate and key, the breaker ceiling and the longest-action bound (section 7).
   The closed-key-set rule of `config.rs` stays.
4. `check` gains the rows marked new in 3.1 and relaxes one rule: `RouteAction` may carry a
   `retry_policy`, but only on the routes of section 5, and only with `retry_on` in a closed set.
   `check` also verifies that both instances appear in both clusters with disjoint port blocks.
5. The hot-restart unit and its start script (section 7) are rendered by `kbf-deploy linux`,
   next to the kbf units.

The Kubernetes front keeps its own rollout for Envoy (a Deployment rolls its pods); hot restart is
the box path. The two settings it lacks (breakers, and a retry policy it does not need) are fixed
in step 4 for both.

## 5. The update sequence

Two instances of one unit template, `kbf-frontend@a|b` or `kbf-all-in-one@a|b`, each with its
own port block (a drop-in that `install-units` writes per instance, since configuration is
flags). Instance letter `i`, predecessor `p` (the other letter).

**Drain is staged.** `Drain::begin` splits into three one-way stages, each idempotent, in
`kbf-server/src/drain.rs`:

| stage | what changes | reversible | by |
|---|---|---|---|
| `stand-down` | new `Execute` refused UNAVAILABLE (trailers-only); worker sessions refused; quiesce; scheduler fenced; worker streams ended; state demoted; (all-in-one: storage demoted, section 6). **Readiness is not changed.** | no, after the fence | the successor, over the API |
| `unready` | `Readiness::stop`: `/readyz` and the gRPC health answer not-ready | yes until `goaway` | a timer after `released` (`--handover-unready-after-secs`) or SIGTERM |
| `goaway` | the REAPI listener's graceful shutdown (GOAWAY to Envoy's upstream connections, which close as their last stream ends), so the process can exit | no | `unready` plus a settle of `--handover-settle-secs` (default 5, at least `interval x unhealthy_threshold + timeout` of the check) |

SIGTERM and `POST /v1/frontend/drain` without a handover body keep today's meaning: all three at
once (`api.rs`, `FRONTEND_DRAIN`).

**Why stand-down leaves readiness alone.** The instant after the old process fences, nobody
holds the scheduler. If readiness went off first, Envoy would have no healthy backend until
the successor is ready and Envoy has seen it (about the takeover time plus one check interval),
and Envoy answers that with a local 503 it does not retry (inference). Buck2 retries an
`Execute` it could not open for only about 1.5 s. So the old instance stays routable through
the gap and refuses what it cannot serve in a form Envoy retries onto the other host. Only when
the successor serves does the old one go not-ready.

**Retry policy (the one Envoy feature this needs beyond health).** On `Execute` and the
read-only unary calls (`GetCapabilities`, `FindMissingBlobs`, `BatchReadBlobs`, `GetActionResult`):
`retry_on` `unavailable,connect-failure,refused-stream`; `num_retries` 8; back-off 100 ms to
1 s; `retry_host_predicate` `previous_hosts`. Safe because kbf refuses before any work, and
because a duplicate `Execute` joins the same action (the scheduler joins identical hermetic
actions, `kbf-front/src/execution.rs`). Envoy buffers the request of a retried call, so the
policy is never put on `ByteStream` or `BatchUpdateBlobs`; those rely on Buck2's own retries and,
for all-in-one, on the park of section 6. Envoy reads the status of a trailers-only reply from
the response headers, which is how kbf's refusal arrives (a status in trailers after a header
does not retry). A test asserts the refusal is trailers-only on the real tonic stack.

**Sequence** (frontend; section 6 adds the storage steps for all-in-one):

0. `kbf handover` (or `systemctl start` of the idle instance). The new process `i` does all
   its slow, lock-free work first (certificates, flags, runtime checks), binds its loopback
   ports and serves `/readyz` as not ready (`Readiness::set_leader(false)`, the hook that
   exists for this). It cannot take the state store: the file lock is held.
1. `i` calls the predecessor's API (`POST /v1/frontend/drain`, `{"handover": true}`, loopback
   peer, bearer token: the existing gates of `api.rs`; its port is `p`'s, from the port
   table). A refusal (a different state path or address table, both instances active,
   prepare checks of section 6) answers with a reason, `i` exits non-zero, `p` has not changed.
2. `p` runs `stand-down`. Its answer to the first call that arrives after the demote is
   `released` with the generation it held. `p` is still ready: Envoy may still pick it, and
   it refuses `Execute` and worker sessions with UNAVAILABLE.
3. `i` takes the next generation, `Fenced::take(store, Some(generation))`, with the epoch
   kept (`EpochPolicy::Keep`; today `StateStoreUri::epoch_policy` is `Bump` only and takes a
   parameter), recovers, serves and says `READY=1`. Envoy marks `i` healthy within one check interval
   and the retries of step 2 land there.
4. After `--handover-unready-after-secs` (default 10, and above the recorded takeover time plus
   two check intervals, enforced at step 1) `p` runs `unready`; after the settle, `goaway`.
5. `p` follows its open `Execute` and `WaitExecution` streams from the `done/<op>` rows `i`
   writes, until each ends (`kbf_frontend_relay_lost_total` counts one it cannot find three times
   in a row: the only way this path fails a build, and it must be zero in every test but the
   planted one). `p` exits when no stream is open, no `Result` is unacknowledged and, for
   all-in-one, no lease is held.

If `i` never becomes ready (before step 2) nothing has changed. If `i` dies after `released`,
`p` still answers its callers from the rows and systemd restarts `i` as a cold start, which
bumps the epoch, so the daemons rerun their leases: slower, no failed build.

**Why the same process for a frontend and all-in-one.** The sequence, ports, health flip, retry
policy and Envoy file are identical; all-in-one adds the storage and daemon halves of section 6.
`kbf frontend` and `kbf all-in-one` call one `handover::Driver` with a list of stand-down stages
(section 11).

## 6. All-in-one: what the process still has to hand over

Envoy removes the listener transfer. It does not remove the problem that scheduler, storage and
daemon share a process. Everything below is kept from the first version, with the listener and
GOAWAY parts rewritten.

### 6.1 Blockers (re-checked on arch-v2)

| # | blocker | evidence |
|---|---|---|
| B1 | A running action dies with its daemon process: the keeper holds a pipe only the daemon holds. So the old process lives until its leases end. | `kbf-daemon/src/keeper.rs`, `kbf-driver-container/src/podman_killed.rs` |
| B2 | The meta log is single-writer by flock; there is no read-only open and no `demote`. | `kbf-server/src/raft_log/mod.rs` (`RaftLogError::Locked`) |
| B3 | The cold `file://` store takes an exclusive `LOCK`. | `kbf-objstore/src/fs_store.rs` (`FsStoreError::Locked`) |
| B4 | The hot tier removes block files no staged object names at open. | `kbf-tier/src/open.rs` |
| B5 | The daemon half dies with the server half: `with_daemon` ends the daemon when `serving` ends. | `kbf-all-in-one/src/run.rs` |
| B6 | The in-process daemon's data plane is `LocalDataPlane` over the old process's cache. The control plane is a loopback mTLS worker session. | `kbf-all-in-one/src/plane.rs`, `kbf-node` |
| B7 | Two daemons on one host collide: the start sweep removes every container labelled with the node id and every lease directory; the `daemon-instance` claim; container names; the cache dir lock. | `kbf-driver-container/src/runtime.rs` (`sweep`), `kbf-node/src/instance.rs` |
| B8 | The scheduler books by worker (`free() = capacity - booked`); two workers on one box would double-book it. | `kbf-sched/src/scheduler.rs` |
| B9 | All-in-one starts with `Bump`; a bump makes every daemon kill its runs. | `kbf-server/src/state_store.rs` |
| B10 | `Drain` is one-way and `Farm::fence` has no unfence (on the handover branch). Section 5's staging puts the reversible work in front of the fence. | `kbf-server/src/drain.rs` |
| B11 | The detached daemon fences itself after 40 s without an acknowledged heartbeat; the successor's `LEASE_GRACE` is 60 s. | `kbf-daemon/src/config.rs` (`FENCE_AFTER`), `kbf-sched/src/fence.rs` |

### 6.2 The detached daemon

- **Process shape.** After `released` the server half stops serving but the process does not
  exit; the daemon half runs on as a detached worker (B1, B5). The process exits when the
  conditions of step 5 hold. A second SIGTERM ends open streams and abandons leases, which fails the
  builds whose streams `p` holds; it is the operator's explicit choice.
- **Control plane.** No new mechanism. The detached daemon's stream is ended by `stand-down`; it
  redials `--server`, which is Envoy's worker address, and reaches the successor. Between
  `stand-down` and the successor serving, a redial can reach the old instance again (it is still
  ready); the old instance refuses worker sessions in that window (UNAVAILABLE), and the daemon's
  redial loop (`--reconnect-ms`) converges. Same node id and instance id, so the scheduler sees the
  same process; it resends its Hello and unacknowledged Results before its first heartbeat, and keeps
  its runs only if the Welcome epoch equals its leases' epoch, hence `Keep` (B9).
- **Lease adoption, not double booking.** The successor's recovery stores each lease
  `Held{booked, session: u64::MAX}` with `awaited[worker] = now + LEASE_GRACE`, and adopts it when
  the worker registers under the same id (`kbf-sched/src/scheduler/recover.rs`, from the first
  version; re-read before the PR).
- **The successor's daemon is a different worker.** `--node-id=<host>-<instance>`, one certificate
  per instance, per-instance daemon directory `<data-dir>/node-<instance>/` only when `--instance` is
  given (existing hosts keep their layout); container lease terms and cgroup names carry the
  instance (B7). Work, not a flag: audit `runtime.rs` for any name that is not owner-scoped.
- **Cordon, and its reset.** The predecessor's worker is cordoned by a durable `cordon/<worker>`
  row. Node ids alternate (`-a`, `-b`, `-a`), so in the same transaction as the takeover the
  successor **deletes the cordon row of its own node id** and writes the predecessor's; when the
  predecessor exits its worker and cordon rows are deleted. A takeover clears a manual cordon on that
  node id; the operator cordons again.
- **Capacity.** The successor's daemon registers `total - booked_by(predecessor)` and resends its
  Hello as the predecessor's leases end, driven by lease-end events it observes, never a timer, and
  never above `total`. Open: whether two units together can exceed the box through the per-unit
  cgroup caps (Q4); the capacity PR does not ship before it is answered.
- **Daemon cache.** The detached daemon stages no new inputs, so it releases the cache dir lock at
  detach; the successor's daemon takes it.
- **Remote daemons** of an all-in-one host get the same cut and redial through Envoy, are not cordoned,
  and are placed on at once.

### 6.3 Storage: who writes, who reads

Alternative A (kept): **demote in place**. The old process gives up the writer role of the meta
log, the cold store and the hot tier, keeps what it needs to read, and the successor is the only
writer. B (the old process stays the storage server) and C (use `kbf frontend` plus separate
daemon hosts) are as in the first version: B rejected (a hop for every build for hours, a second
handover later), C stays the answer for a site that will not afford A.

1. `RaftMetaLog::demote`: stop the tick task and committer after the in-flight batch, refuse later
   commits, release `log.lock`, keep the applied `MetaState` queryable. Unknown whether the applied
   state outlives the Raft host stopping; the spike answers it first, and the fallback is a pure proxy
   to the successor. Not reversible: lives after the fence.
2. `FsStore::demote`: release `LOCK`, keep reads. Objects are immutable and renamed in atomically.
3. `Cache::seal`: reversible part at prepare (seal the active block, drain the flusher into cold);
   committed part at stand-down (final flush, tier read-only and open, so a `ByteStream.Read` in
   flight finishes). Object keys carry a per-start epoch, so two writers never collide
   (`kbf-front/src/cache/write_group.rs`).
4. `HandoverStorage`: one `Arc<dyn Storage>` behind an atomic swap, used by the REAPI services and
   by `LocalStore::over`. In `Follower`: authority questions (`FindMissingBlobs`, `GetActionResult`)
   go to the successor, never to frozen state; reads of a named digest try frozen state then the
   successor; writes go to the successor and, while it is not serving, **park**, bounded by
   `--handover-park-secs`. The segment-commit calls the daemon makes need the same redirect: their
   own PR.

**How the old process reaches the successor.** In the first version both processes shared one
address; now the successor's REAPI listener is a known loopback port (the other block of the
table), reached over the farm-CA TLS with the node's identity. This is the same hop the proxy fallback
uses (`kbf-front/src/proxy`).

**Prepare (reversible).** At step 1 of section 5, before the fence, the old process seals the tier and flushes it, checks
free space on the cold, meta and state directories, and evaluates the **gap budget**: it refuses
(`refused disk`, `refused replay-too-long`, `refused unmeasured`) and nothing has changed. The gap is
the successor's replay of the meta log (never compacted, so it grows with the host's age), tier open and
farm recover, plus the daemon's redial wait. Each successor start records its measured time
(`handover/replay_ms` in the state directory). The budget is: recorded time + `--reconnect-ms` + heartbeat
interval below half of `FENCE_AFTER` (20 s) and below `--handover-unready-after-secs` minus two check
intervals. The first start into the template is a cold restart that records it. If replay can exceed the budget on a
realistic host, log snapshots (a separate design) come first.

**Write drain.** After the REAPI listener refuses new streams the old process counts client writes
(`ByteStream.Write`, `BatchUpdateBlobs`, segment commits) and waits for zero, bounded by
`--handover-write-drain-secs` (default 120), before it demotes the stores. In the Envoy model new
streams still arrive at the old instance until it is not ready, so the old instance **refuses new
writes at once** with UNAVAILABLE (which Buck2 retries onto the successor through Envoy) and waits only for
the writes already open. On expiry the remaining writes are cut and counted
(`kbf_handover_writes_cut_total`): the one deliberate build-failure risk of the sequence, needing a client
that writes one blob longer than the bound.

**Committed demote block**, in this order, each a lock release or flag flip: final flush and tier
read-only; `RaftMetaLog::demote`; `FsStore::demote`; `HandoverStorage` to `Follower`; release the daemon
cache; write the detached marker (section 9); `store.demote()` last. A failure here is committed: the
process fails closed (`drain.close()`, non-zero exit, no marker), systemd restarts it as a cold start,
builds on it fail and its actions rerun, and `kbf_handover_demote_failed_total` alerts. Each stage has a
failpoint test.

## 7. Envoy's own update: hot restart

Envoy is updated, and its configuration changed, by **hot restart**, never by `systemctl restart`
(which cuts every open stream). The unit runs Envoy's restarter script (the supported wrapper:
SIGHUP re-runs the start script with the next `--restart-epoch`). `ExecReload` sends that SIGHUP.

What Envoy does (Envoy's hot restart documentation): the new process initializes, runs its first
health checks, asks the old one for its listen sockets (reuse-port sockets are passed by worker
index), listens, and tells the old process to drain. Existing connections are never transferred. The
old process sends its HTTP/2 connections a GOAWAY, lets open streams finish, and is shut down after
`--parent-shutdown-time-s`; whatever is open then is closed.

Settings, and why each is not the default:

| flag | default | value | reason |
|---|---|---|---|
| `--parent-shutdown-time-s` | 900 | the longest action plus 15 minutes | at expiry the old process is closed with every stream still open; 900 s would fail every action over 15 minutes |
| `--drain-time-s` | 600 | 60 | below the parent shutdown time as Envoy requires; it only schedules how soon connections are told to go, not when streams are cut (open ordinary streams are left to complete, per the draining documentation) |
| `--drain-strategy` | `gradual` | `immediate` | clients get the GOAWAY at once and move their new streams to the new process early |
| `--base-id` | 0 | fixed, unique on the host | the epochs' shared memory |
| `--skip-hot-restart-on-no-parent` | off | set | a start after a crash of the first process is a normal start |

**The longest action is not a bound** (section 2): an action names its own timeout. Either the farm caps it
(a `--max-action-timeout` on the server, refused at Execute above the cap, which makes the Envoy value a
derived number and a check) or Envoy's hot restart can cut an action longer than the parent shutdown time
(Q2). The box config carries `KBF_LONGEST_ACTION_SECS`; the kit's check refuses a render whose
parent-shutdown value is below it.

**What a client sees at an Envoy update.** This is the one place a client sees a GOAWAY. Envoy's HTTP
connection manager announces a shutdown to a drained HTTP/2 connection and sends the final GOAWAY after
`drain_timeout` (5 s default; inference: the two-step announcement is the sequence RFC 9113 describes and
the one the split-mode note names as the fix for the GOAWAY race). Streams already open run to their end on
the old process; new streams on the old connection before the final GOAWAY are accepted by it; Buck2 has no GOAWAY
handling of its own, so what it does with a refused-at-GOAWAY stream is the same open question as before
(split-mode note, section 4.1, U1) and now concerns Envoy updates only, which are rarer than kbf updates. The
real-Buck2 build through an Envoy hot restart is a test (T-E8).

**Rules.** An Envoy hot restart and a kbf handover are serialized by one lock file the updater and
`kbf handover` both take, so neither starts while the other is mid-way. A second Envoy hot restart while the
first parent is alive is refused by the updater (it reads `server.hot_restart_epoch` and the parent's
liveness from the admin interface; Envoy documents no limit on concurrent epochs, so the refusal is ours).
Envoy's binary is versioned and staged like kbf's (section 9). The new Envoy epoch needs no change on the kbf side: its static
config lists `a` and `b`.

**Certificates.** Envoy holds the client-facing certificate. A renewal needs Envoy to read it again: a hot
restart (rare, cheap, and the path above) or file-watched secrets (an xDS source from a file, which the owner
excluded). Recommendation: hot restart (Q3). The backends keep their own live reload (`docs/deploy/tls-renewal.md`).

## 8. What a client sees, end to end

A kbf update, Buck2 first:

| time | event | Buck2 |
|---|---|---|
| t0 | `i` starts, not ready | nothing |
| t1 | `p` stands down: refuses new `Execute`, fences, demotes | open `Execute` streams continue (relayed from `done/` rows after the successor takes over); a new `Execute` that lands on `p` gets UNAVAILABLE in headers and Envoy retries it, the connection to Envoy unchanged |
| t2 | `i` serves; Envoy sees it healthy in at most one interval | new streams go to `i` |
| t3 | `p` not ready, settle, GOAWAY on Envoy's upstream connection only | nothing: no frame reaches Buck2 |
| t4 | `p` exits when its last stream ends | nothing |

No GOAWAY is needed from kbf and none reaches Buck2: Envoy owns the downstream connection and an upstream GOAWAY
only closes Envoy's connection to `p`, which Envoy replaces for new streams. Streams open on `p`'s
connection continue (Envoy finishes the streams of a draining upstream connection; inference, T-E4 measures it). A health
check failure never cuts an open stream (section 3.1). Budget for the gap, the one window where only retries
cover a call: takeover time plus one check interval, which must stay below the retries' total (about 5 s with the
policy above) and, with Envoy's retries absent, below Buck2's own 1.5 s; T-E6 plants a slow check interval and a missing
policy and asserts the errors appear.

For the in-process daemon the cut is `stand-down` ending its stream, then a redial through Envoy, as in
section 6.2.

## 9. Units, command, marker

Mirrors `kbf-frontend@.service` on the handover branch, for both modes.

- Template `kbf-frontend@.service` and `kbf-all-in-one@.service`, `Type=notify` (`READY=1` when the instance
  holds the store and its probes pass), `TimeoutStartSec` finite (all-in-one: `max(120, 3 x recorded replay)`),
  `KillSignal=SIGTERM`, `TimeoutStopSec=infinity`, `Restart=on-failure`, `DefaultInstance=a`, no `Conflicts=`
  between instances, `RestartPreventExitStatus=2`. A per-instance drop-in carries the port block, `--node-id`,
  `--instance` (the rendered drop-in is the only per-instance difference). `install-units` refuses when the
  free memory is below both instances' `MemoryMin` (an idle process and a draining one must fit).
- `kbf-envoy.service`: the restarter with `ExecReload`, `OOMScoreAdjust` below kbf's (an Envoy killed is every open
  stream cut), versioned binary path. `systemctl stop kbf-envoy` is documented as the outage it is.
- `kbf handover --unit kbf-frontend|kbf-all-in-one [--wait]`: reads `ActiveState` of both instances; exactly one active:
  start the other; none: start `@a` unless a marker blocks it; both active or transitioning: refuse, exit 1. It
  runs only `show` and `start`, never `stop` or `restart`. With the Envoy lock (section 7).
- **Binary path.** The updater stages `/opt/kbf/<sha>/kbf`, `ExecStart` runs the `current` symlink, so a running
  process's `/proc/self/exe` (which keepers re-execute) stays its versioned file; garbage collection keeps every
  version a running process maps plus the newest two. Order: stage, check both instances are not active, flip
  the symlink, then `kbf handover --wait`. A refusal before the flip leaves the symlink alone and reports
  "draining: N leases, oldest M minutes".
- **Detached marker (all-in-one).** `/run/kbf-all-in-one/detached-%i`, written in the demote block before
  `released`, removed on a clean exit. `ExecCondition=kbf all-in-one precheck --instance=%i` exits non-zero when it exists, so a
  crashed detached process is not restarted into a takeover of the live instance (which would be an update
  backwards); `start_or_take_over` repeats the check; `kbf handover --acknowledge-crash <instance>` is the only way
  back. While detached the unit reads `active (running)` and the process keeps `STATUS=` and a read-only status file current.
- **Long drains.** The update rate is bounded by the longest action on the old instance; `--max-detached-hours`
  (default 12) alerts and never forces. A second SIGTERM to the old instance fails the builds it holds; the doc says so.
- **N / N-1.** The windows of `split-mode-updates.md` section 3 apply unchanged: the old instance reads, for
  three looks each, the `done/`, `op/`, `lease/` and queue rows the new one writes, so row encodings are frozen across one
  step and the SQLite schema follows expand/contract. Envoy's config is version-independent, so a kbf update is not
  also an Envoy compatibility question.

## 10. Failures

"Class": reversible (before the fence) or committed.

| point | class | Buck2 | operator / daemon | recovery |
|---|---|---|---|---|
| `i` fails or is refused before it calls `p` | reversible | nothing | `systemctl start` fails | `p` unchanged; fix, retry |
| prepare refuses (disk, flush, replay, unmeasured) | reversible | nothing | `i` exits with the reason | retry |
| `i` dies after `released`, before serving | committed | new calls retried by Envoy, then Buck2, until systemd restarts `i` | cold start bumps the epoch; the detached daemon reruns its leases | callers of `p` are answered from rows |
| `i` slower than `--handover-unready-after-secs` | committed | a window of 503 from Envoy (both unready) then retries by Buck2 | alert on the gap | raise the bound; it should have been refused at prepare |
| write drain expires | committed | that write is cut; the build fails | `writes_cut_total` | the one designed failure; raise the bound |
| a demote stage fails | committed | open streams on `p` end UNAVAILABLE | `p` exits non-zero, cold start | actions rerun |
| `p` dies mid-follow | committed | its open streams break (the process holds them) | its daemon and keepers die (B1); marker stays | `--acknowledge-crash` |
| **Envoy crashes or is killed** | n/a, **new** | every open `Execute` breaks | systemd restarts it; kbf unaffected; worker sessions redial | the cost of the uniform topology: a process in front of every build. Mitigation: low `OOMScoreAdjust`, a memory limit, no `restart`, a watchdog |
| **Envoy hot restart: parent closed at `--parent-shutdown-time-s`** | n/a, **new** | streams still open then break | `kbf_envoy_parent_cut` | set the time above the longest action (Q2) |
| a health check flaps ready on a fenced instance | committed | retried calls | `upstream_rq_retry` alert | the check includes the stop flag; the settle covers it |
| Envoy retry budget exceeded | n/a, **new** | UNAVAILABLE passes through | `upstream_rq_retry_overflow` | `max_retries` raised in 3.1 |
| two updates back to back | n/a | n/a | `kbf handover` refuses when both instances are active | bounded by the longest action |

Reading of the table: the update fails a build only through the first-version rows (an operation with no trace
for three looks, a second-signal escalation, a crash of the process that holds the stream, the write-drain
expiry, a committed demote failure) plus the two new Envoy rows. Envoy is a single process in front of every
`Execute`; that is the price of one update path everywhere and the owner has taken it.

## 11. Reuse from the fd-passing branches

- **Kept**: `Farm::quiet/fence/follow/end_worker_streams` and the durable `done/` rows
  (`farm/handover.rs`), `Drain` as a staged object (section 5), `Fenced`/`Store::demote`, `READY=1`
  (`handover/notify.rs`, `ready.rs`), the daemon-side redial, the `Stop`/`Latch` pair.
- **Dropped**: the control socket and `SCM_RIGHTS` wire (`handover/wire.rs`, `incoming.rs`, `outgoing.rs`), `listen.rs`'
  adopt and spare dup, the address check against the holder's listeners, GOAWAY as the client's switch.
  The coordination the wire carried (refusal reasons, the generation) is the API call of section 5 and the
  state store's own generation.
- **Open**: whether the fd-passing path stays as a no-Envoy shape (Q1). Recommendation: trim it. A second way
  to do one thing, tested against none of the Envoy topology, and an unconditional source of the question "which
  update path is this host on".

The assembly of `frontend.rs` (`Prepared`, `Trio`, `hello`, `take_over`, `start_or_take_over`) becomes a
`handover::Driver` both modes call, with a golden of its call sequence for the frontend taken before the refactor.
`kbf all-in-one` gains `--instance`, `--handover-unready-after-secs`, `--handover-settle-secs`,
`--handover-write-drain-secs`, `--handover-park-secs`, `--handover-follow-interval-ms`, all off without `--instance`.
Configuration is flags; the only environment variable is `NOTIFY_SOCKET`.

## 12. Spike gating the design (PR 0, throwaway, not merged)

Answers, with a real pinned Envoy and stub or real kbf, before any merged PR:

1. **Envoy**: a silent stream longer than the default 15 s route timeout survives with the box settings; the default
   300 s idle timeout is not reached with progress frames; a failed health check leaves an open stream alone and stops
   new streams within the settle; an upstream GOAWAY lets open streams finish and new streams use the other host; more than 1024
   concurrent silent streams through default breakers fail and through the box settings do not; a retried trailers-only
   UNAVAILABLE reaches the other host and a local no-healthy-upstream 503 is not retried; `close_connections_on_host_health_failure` on the
   worker cluster cuts a daemon's session (so it stays false); a hot restart with a 60 s drain time leaves a silent stream open past it
   and closes it at the parent shutdown time; what a second hot restart inside the first's window does.
2. **kbf**: `RaftMetaLog::demote` keeps its applied state readable; replay time of a real aged log against the budget.

## 13. Tests

Each test names the defect it catches and the line that turns it red; a mutant is planted and seen red on the farm
before a PR goes up. Unit tests sit in the library they cover; real Envoy tests use the pinned binary (CI fetches
it); container tests extend the `podman-x86` job, everything else runs in `test-x86` and `test-arm`.

| id | test | catches | mutant |
|---|---|---|---|
| E1 | the box render is golden, passes `envoy --mode validate`, and `envoy check` passes it | a render Envoy refuses or the checker misreads | add an unknown field |
| E2 | `envoy check` fails each defect of 3.1 alone: route timeout default, `max_stream_duration`, idle timeout default, default breakers, `close_connections_on_host_health_failure` true, worker idle default, a retry policy on `ByteStream`, one instance missing, overlapping ports, a parent-shutdown below the longest action | a render that silently cuts or refuses a stream | each setting returned to Envoy's default |
| E3 | real Envoy and two stub servers: a silent `Execute`-shaped stream longer than the (scaled) timeouts completes | the 15 s and idle defaults | route timeout left at default |
| E4 | stub `a` turns not-ready: no new stream reaches `a` within the settle, the open one on `a` finishes, for the REAPI (L7) and worker (L4) clusters | health failure cutting a stream; a daemon session cut | `close_connections_on_host_health_failure` true on the worker cluster |
| E5 | more than the default breaker's concurrent silent streams are held open through Envoy | the 1024 cap | breakers left default |
| E6 | `kbf frontend` a/b with Envoy: an `Execute` open across the handover completes; calls issued continuously through it see zero errors; `upstream_rq_retry` is non-zero | the update gap, the missing retry | no retry policy; a 5 s check interval |
| E7 | real hot restart: a silent stream outlives `--drain-time-s`, new streams reach the new epoch, the parent is gone at the parent-shutdown time; with a parent shutdown below the stream's length the stream is cut and the test goes red | the default 900 s | parent shutdown at 900 s |
| E8 | before the last PR: one real Buck2 build through a kbf handover, and one through an Envoy hot restart | Buck2's behavior at GOAWAY and open-time refusal | omit the settle |
| T-R | the refusal of a draining `Execute` is trailers-only on the tonic stack | Envoy's retry reading headers | send a header before the error |
| T-D | `Drain`: each stage alone, idempotent; `stand-down` leaves `/readyz` ready; `unready` flips it and the gRPC health; `goaway` completes the listener's shutdown; SIGTERM does all three | a split that loses an effect | drop one effect |
| T-A | the API call: refusal reasons; a different state path refused; the answer carries the generation; no handover body keeps today's meaning | a refusal that proceeds | accept without comparing |
| T-1..T-3 | `RaftMetaLog::demote`, `FsStore::demote`, the second open succeeds while reads work (first version T1, T2, T3) | B2, B3 | skip the unlock |
| T-4, T-5 | `Cache::seal` and the orphan pass; `HandoverStorage` (`FindMissing` and action-cache lookups to the successor, named reads frozen then successor, parked writes land) (first version T4, T5, T18) | B4, a stale hit, a lost write | answer from frozen state |
| T-6 | real stages with a failpoint at each demote-block index: fail-closed outcome, no marker, cold start serves; state demote last | a partial demote | move `store.demote` earlier |
| T-7 | `Keep` on the real start path; recovered lease books on the predecessor's node id; successor registers `total - booked`; two cycles a, b, a, the second gets grants | B8, B9, the stale cordon | `Bump`; skip the cordon delete |
| T-8 | the real start sweep against a fake container runtime holding two owner labels | B7 | share the data dir |
| T-9 | golden of the Driver's call sequence for the frontend, plus the existing suite | a refactor changing both sides | reorder two stages |
| T-10 | all-in-one with a remote daemon: a long action finishes on the old process; its output readable through the successor; an `Execute` longer than every keepalive completes (container, x86) | the whole contract | kill the old process at `released` |
| T-11 | an action finishes during the gap, local and remote: its upload parks and lands, its `Result` is accepted once | unacknowledged results | give up the upload early |
| T-12 | the successor dies after `released`: a cold start serves, callers of the old one get answers | the unrecoverable window | the old exits at `released` |
| T-13..15 | injected ENOSPC at prepare refuses and the old keeps serving; the short-fence flag: under budget zero reruns, over it the leases fence and rerun and no caller sees an error from the old one | disk-full reversibility, the fence interaction | raise the fence |
| T-16 | SIGKILL a detached old: the successor is unaffected, `precheck` refuses the restart, `--acknowledge-crash` clears | restart into takeover | write the marker after `released` |

Failpoints and the short-fence flag live behind a cargo feature with a gate test that fails when a release build enables
it. No test pins a wrong answer.

## 14. PR split

PRs that touch `frontend.rs`, `kbf-deploy` or `kbf-cli` stack on the trimmed #879/#887 or on a re-implementation of
their farm half (Q1); the rest stack on arch-v2. Sizes are estimated changed lines.

| PR | content | size |
|---|---|---|
| 0 | spike, section 12, not merged | 400 |
| | owner decision (Q5) after PR 0 | |
| E1 | envoy kit `Box` topology: render, config keys, `Server` by host and port, new checks, E1/E2 | 700 |
| E2 | real-Envoy tests E3-E5, E7 against the render | 500 |
| 1 | `Drain` staged, the API body and status, T-D, T-A, T-R | 400 |
| 2 | `handover::Driver` out of `frontend.rs`, T-9; the farm half (`fence`, `follow`, `end_worker_streams`) and `Keep` through `StateStoreUri` | 700 |
| 3 | frontend wiring: `--instance`, predecessor call, ports, settle and unready timers | 400 |
| 4 | `kbf-deploy`: unit templates and per-instance drop-ins, `kbf-envoy.service` with the restarter, binary path, `kbf handover`, lock file | 600 |
| 5 | E6 and E8 for the frontend | 400 |
| 6-9 | all-in-one: `FsStore::demote` and `RaftMetaLog::demote` (T-1..3), `Cache::seal` and `HandoverStorage` (T-4, T-5, segments redirect), the detached daemon and per-instance data dir (T-8), the wiring, cordon reset and capacity (T-6, T-7, T-11..16) | about 3000 |
| 10 | all-in-one `kbf-it` T-10, T-12, container CI placement | 500 |

PRs 6 to 9 are inert until their flags are given. Everything from PR 6 is gated on the spike's kbf answer.

## 15. Open questions

1. **Keep the fd-passing path (#879/#887) as a no-Envoy shape, or trim it?** Recommendation: trim to the farm half
   (section 11); a second update path is a second thing to test and to explain.
2. **The longest action.** Cap an action's timeout farm-wide (`--max-action-timeout`) so Envoy's parent-shutdown time is a
   derived number, or accept that an action longer than it is cut by an Envoy hot restart. Recommendation: cap, default 6 h.
3. **Certificate renewal at Envoy**: hot restart or file-watched secrets. Recommendation: hot restart; it avoids an
   xDS source and the path is already exercised.
4. **Cgroup caps across two all-in-one units** (`actions/` `cpu.max`, `--actions-memory-max-gib` are per unit).
   Recommendation: one `actions/` parent shared by both instances; verify before the capacity PR.
5. **Is all-in-one worth the storage work?** Alternative C (`kbf frontend` plus separate daemon hosts) already updates a
   fleet with this note's frontend half. Recommendation: the owner decides after PR 0; build the frontend half first either way.
6. **Write-drain bound.** 120 s is a guess at "no client writes one blob longer"; measure the largest Buck2 upload before PR 8.
7. **Node id per instance**: two certificates (no change to `peers.admit`) against a suffix rule. Recommendation: two certificates.
8. **Cold store kind**: `file://` needs `FsStore::demote`; an object store has no lock. Recommendation: keep `file://` support.
9. **#743** (control plane as calls, paused): if it lands the detached daemon's control plane becomes a feature, not a redial.
   Recommendation: do not land it before this is built.
10. **Health check kind for `kbf_reapi`**: the gRPC check (kit today; on the REAPI port, over TLS) against an HTTP
    `/readyz` check on the API port. Recommendation: the gRPC check, it is already specified and checked and is the same evaluation.

Reviewed and not taken: (a) readiness off before the fence with Envoy's health check as the only gap cover (section 5: the
gap is detection time on both sides, longer than Buck2's retry window); (b) routing the worker port by instance number without
Envoy (every update would change a remote daemon's address); (c) L4 for REAPI (new streams on an existing connection would
stay on the old process); (d) a Welcome-carried extended daemon fence as the default (it weakens the only partition
safeguard); (e) retries on `ByteStream` and `BatchUpdateBlobs` (Envoy buffers the request).
