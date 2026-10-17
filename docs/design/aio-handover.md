# Updating a frontend or an all-in-one host behind Envoy, without failing a build

Status: design, not implemented. One update process for every shape: Envoy stays in front
of kbf on the host (also on a single box, also for `kbf all-in-one`); a new kbf process is
started on other ports; Envoy sends **new** connections and streams to it only once it is
ready and serving; the old process keeps answering what it already holds, then fails its
health check, finishes and exits. This replaces the first version of this note, which moved
the listening sockets between processes by fd passing, and an intermediate version that
parked requests while the new process caught up. **No request is parked or queued at any
point: section 5.3 states exactly what a call can wait for, and the one residual (the
switch time) is a measured gate, not a mode.**

File references are to `origin/feature/arch-v2` (symbols are the authority, line numbers
rot). "Inference" marks a claim read from code shape or from Envoy's documentation and not
yet from a test; section 12 lists every inference the spike must turn into a measurement.

## 1. Goal and contract

1. Builds already running finish on the old process.
2. New requests go to the new process, from the moment Envoy sees it ready.
3. The old process exits when it holds nothing.
4. Envoy updates by its own hot restart (section 7), under the same three rules.
5. No phase of an update holds, parks or queues a client request. (Section 5.3.)

No client build may fail because of an update. Buck2 sends one `Execute` per action,
retries only the opening of that stream (5 attempts, sleeps of about 0.1, 0.2, 0.4 and 0.8 s
plus up to 50 ms), never calls `WaitExecution` or `GetOperation`, and fails an action whose
stream breaks after it started (`remote_execution/oss/re_grpc/src/client.rs`, read on
`main`; upstream facebook/buck2#1569 is open; the farm's record is kbf #791). So the old
process must answer every `Execute` it accepted, to the end, and Envoy must never cut one.

What each party sees:

| party | during a kbf update |
|---|---|
| Buck2 | Its connection to Envoy never changes. Open `Execute` streams continue on the old process. A call that Envoy still sends to the old process after the successor took the roles is answered through the old process (section 5.2), not refused. No GOAWAY comes from kbf: Envoy keeps the downstream connection (section 8). |
| daemons (remote, and the in-process one of all-in-one) | The old process ends their streams. They redial the same worker address, which is Envoy's, and reach the successor. Leases are kept (the epoch is kept) and nothing is double-booked (section 6). A redial that reaches the old process in the short window before it goes not-ready is refused and retried by the daemon's own loop; a daemon is not a client call. |
| operator | `kbf handover` starts the other instance and, with `--wait`, polls the old one until it is gone. Operator reads (`/readyz`, `/v1/...`) are by instance on loopback; the public view is Envoy's. |

SQLite with the single-writer lock, `Store::demote` and the generation fence is enough for
the scheduler state; no Postgres. (Postgres has no `demote`, `kbf-state/src/store.rs`, and is
for two serving frontends.)

Storage nodes and daemon nodes of split mode are not part of this sequence: the frontend
updates each of them when it is idle or drained, in any order, under the N / N-1 rule of
section 9. This note covers the frontend, and the two halves an all-in-one process has in
addition (storage and daemon).

## 2. What was re-checked, and where the code disagrees with the brief

| claim | result | where |
|---|---|---|
| kbf sends nothing on a long Execute and has no HTTP/2 keepalive | **Not true on arch-v2.** Every open `Execute` and `WaitExecution` stream is re-sent its operation every `--progress-interval-secs` (default 30), and both listeners send an HTTP/2 PING every `--http2-keepalive-interval-secs` (default 30, timeout 20). Progress frames are data frames, so they reset Envoy's stream idle timer; a PING is connection-level and does not (inference). The Envoy overrides below stay, because both flags can be 0. | `kbf-server/src/config.rs` (`progress_interval_secs`, `http2_keepalive_*`), `kbf-front/src/execution.rs` (`progress`), `kbf-server/src/serve.rs` (`StreamTiming`, `keepalive`) |
| the longest action is 1 h | **Not a bound.** The container driver's default timeout is 1 h but an action's own `timeout` replaces it and is not capped. The envoy kit's 3900 s (1 h plus the 300 s unservable wait) is the default case only. | `kbf-driver-container/src/runtime.rs` (`timeout_of`, `default_timeout`), `crates/kbf-deploy/src/envoy/check.rs` (`SILENT`) |
| #857 is merged and stays needed | Yes: `Fenced`/`meta/generation`, `Store::demote` (SQLite), `done/<op>` rows with `DONE_FLOOR` (600 s); the old process answers its open streams from `Finished` rows it can read without knowing how the result was made. | `kbf-state/src/fence.rs`, `store.rs`, `sqlite/mod.rs`; `kbf-server/src/farm/durable.rs`, `kbf-front/src/finished_row.rs` |
| the handover halves of #879/#887 | **Not on arch-v2.** No `Farm::quiet/fence/follow/end_worker_streams`, no `handover/` module, no `open_after_handover`. `EpochPolicy::Keep` exists in `Durable::open`, but `StateStoreUri::epoch_policy` returns `Bump` for every SQLite start. Section 11 says what is reused. | `kbf-server/src/farm/durable.rs`, `kbf-server/src/state_store.rs`; branch `av2/frontend-handover-3` |
| `Drain::begin` is one step | Yes: it turns readiness off, refuses new Executes and sends GOAWAY in one call, once. Section 5.1 replaces it with a staged object. | `kbf-server/src/drain.rs` |
| **the frontend can answer a call for another process** | **Only storage.** `kbf-front/src/proxy` forwards CAS, ByteStream and action-cache calls to storage nodes; there is no Execute or WaitExecution forwarder. Section 5.2 needs one. | `kbf-front/src/proxy/mod.rs` (module doc: storage calls only, GetTree not yet; Execute is the scheduler's call and reaches storage through `Backend::routed`) |
| the meta log can be read by a second process | **No.** Single writer by flock, never compacted, a directory holding a snapshot is refused; every start replays the whole log. There is no follower open. | `kbf-server/src/raft_log/mod.rs` (`Locked`, module doc "Not yet: snapshots"), `docs/design/metadata-snapshots.md` |
| the hot tier can be opened by a second process | **No.** Open deletes every block file no staged object names, so a second opener removes the first's active blocks. | `kbf-tier/src/open.rs` |
| a write call is answered only when durable | Yes: a ByteStream `Write` or `BatchUpdateBlobs` buffers (admitted by bytes), is grouped into a segment, PUT, committed by one `PutBlobs`, and only then answered. | `kbf-front/src/cache/write_group.rs` |
| `/readyz` and the gRPC health service agree | Yes: both evaluate `Checks::failing`; `Readiness::stop` flips both; `Readiness::set_leader(false)` exists for a process that is up but must not be picked. | `kbf-server/src/health.rs`, `grpc_health.rs` |
| kbf-deploy renders an Envoy config | Yes, for a **Kubernetes client front**, not a box: one cleartext h2c listener behind a Tailscale ingress, one STRICT_DNS cluster of servers over TLS to the farm CA, route timeout 0, stream idle timeout 7200 s, no `max_stream_duration`, `healthy_panic_threshold` 0, gRPC health check (5 s interval), a checker that models those properties. It has no circuit breakers, no retry policy, no worker route, no hot restart, and refuses a route whose action has anything but a cluster and timeouts. Section 4 builds on it. | `crates/kbf-deploy/src/envoy/{render,check,config}.rs` |
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

**Clusters.** One cluster per listener, both instances in it, equal priority. Both are
healthy together for a few seconds during every update, **on purpose**: the old instance is
kept ready until Envoy has seen the successor healthy, so there is never a moment with no
ready backend and never a local 503 from Envoy; what the old instance cannot serve itself in
that overlap it answers through the successor (section 5.2).

**Health.** The signal is kbf's readiness, which is `/readyz`:

- `kbf_reapi`: the kit's existing `grpc.health.v1` check on the REAPI port, empty service
  name. It is `/readyz`'s evaluation (`grpc_health.rs`); reusing it keeps the checker and the
  kbf-it test that pins the service names (`crates/kbf-it/tests/envoy_check.rs`).
- `kbf_workers`: an HTTP check of `/readyz` on the instance's API port (`health_check_config.port_value`),
  plain loopback HTTP, since the worker cluster has no TLS of its own. `--api-listen` is already in
  the unit and is required here.
- Envoy's own view of the flip is read from its admin interface on loopback
  (`GET /clusters?format=json`, the `health_flags` of each host; inference, the spike checks
  the field names). The successor reads it to learn that Envoy routes to it (section 5); the
  settle timer of section 5.1 is the fallback when no admin address is configured. `--envoy-admin` is a flag
  of `kbf frontend` and `kbf all-in-one`, loopback only.
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
| route `retry_policy` | none | none; the checker refuses any extra action field | `retry_on` `connect-failure,refused-stream` only, on `Execute` and the read-only unary calls. It covers a connection that fails before a stream starts (for example an old instance that has just exited); it is not a hold and no part of the update depends on it | **new** (`retry-routes`) |
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
   `retry_policy`, but only on the routes named in 3.1, and only with `retry_on` in the closed set
   `connect-failure,refused-stream`.
   `check` also verifies that both instances appear in both clusters with disjoint port blocks.
5. The hot-restart unit and its start script (section 7) are rendered by `kbf-deploy linux`,
   next to the kbf units.

The Kubernetes front keeps its own rollout for Envoy (a Deployment rolls its pods); hot restart is
the box path. The settings it lacks (breakers; the retry policy is box only) are fixed
in step 4.


## 5. The update sequence

Two instances of one unit template, `kbf-frontend@a|b` or `kbf-all-in-one@a|b`, each with its
own port block (a drop-in that `install-units` writes per instance, since configuration is
flags). Instance letter `i`, predecessor `p` (the other letter).

### 5.1 Drain is staged, and readiness goes last

`Drain::begin` splits into two one-way stages, each idempotent, in `kbf-server/src/drain.rs`:

| stage | what changes | reversible | by |
|---|---|---|---|
| `hand-over` | `p` starts answering every **new** call through `i` (5.2); scheduler quiesced and fenced; worker streams ended; state store demoted; (all-in-one: the storage block of 6.3). **Readiness is not changed**: `p` stays ready. | no, after the fence | `i`, over the API |
| `unready` | `Readiness::stop`: `/readyz` and the gRPC health answer not-ready. Envoy stops choosing `p` for new streams and connections; open streams are untouched (section 3.1). | yes, until `p` exits | `i`, after Envoy shows `i` healthy (or `--handover-unready-after-secs` as the fallback); SIGTERM |

SIGTERM and `POST /v1/frontend/drain` without a handover body keep today's meaning: `Drain::begin`
as it is, readiness off, new `Execute` refused, GOAWAY to the listener (`api.rs`, `FRONTEND_DRAIN`).
A kbf that is stopped, not updated, still drains the old way; it is not an update path.
There is no `goaway` stage: `p` has no downstream connection to tell (Envoy owns it), and a
host Envoy no longer picks gets no new stream.

**The rule that removes every gap: `p` turns not-ready only after `i` is ready and Envoy routes
to it.** The two are healthy together for a few seconds. At every instant at least one backend
that can answer any call is healthy, so Envoy never has to answer with its local 503 and the
client never sees an update.

### 5.2 `p` answers for `i` while Envoy still sends calls to `p`

After `hand-over` and until Envoy flips, `p` still receives calls, because it is still ready.
It does not refuse them (a refusal is a request the client must retry) and does not hold them
(5.3). It answers them **through `i`**:

| call after the cut | what `p` does | code |
|---|---|---|
| `Execute`, `WaitExecution` (new) | opens the same call to `i`'s REAPI listener over the farm-CA TLS with the node's identity, forwards the request metadata, relays the response stream. `i` authorizes again. A duplicate `Execute` joins the same action (identical hermetic actions are joined in the scheduler), so a client that is later routed to `i` for the same action is safe. | **new**: an Execute relay beside `kbf-front/src/proxy`, which today forwards storage calls only |
| `FindMissingBlobs`, `GetActionResult`, `GetCapabilities` | `i` (authority questions are never answered from frozen state) | `HandoverStorage` (6.3) |
| `BatchReadBlobs`, `ByteStream.Read` | `p`'s frozen state first, then `i` | `HandoverStorage` |
| `ByteStream.Write`, `BatchUpdateBlobs`, `UpdateActionResult` | `i` (a write that is already open at the cut finishes on `p`, 6.3) | `HandoverStorage` |
| worker session of a daemon | refused UNAVAILABLE: `p` has no scheduler; the daemon's own redial loop reconnects, through Envoy, to `i` or to `p` again until it goes not-ready. This is the only refusal, and it is to a daemon, whose fence is 40 s against a window of a few seconds. | `kbf-server/src/worker.rs` |

Calls already open at the cut are not moved: an `Execute` or `WaitExecution` stream stays on
`p`, which answers it from the `done/<op>` rows `i` writes (5.4 step 5) until it ends; a read
in progress finishes from frozen state.

**Why a relay and not readiness-first or a refusal.** Turning `p` not-ready at the cut leaves
Envoy with no healthy backend until `i` is up and seen (about the takeover time plus one check
interval), and Envoy answers that with a local 503 it does not retry (inference); Buck2 retries
an `Execute` it could not open for only about 1.5 s. A trailers-only UNAVAILABLE from `p` that
Envoy retries onto `i` is the same wait with a retry timer in the middle (the client's call is
delayed until a retry lands) and needs a retry policy on `Execute` that must be correct for a
long stream. The relay needs one new component and no timer.

### 5.3 What a call can wait for (no request is held)

There is no queue mode, no park, no admission gate and no `--handover-park-secs`. A call is
only ever: served by `p` as usual (before the cut, or opened before it); relayed to `i` (5.2);
or served by `i` (once Envoy routes there). The residual wait is one thing:

> **Switch time `T_s`**: from the route flag at `p` to `i` holding every role and having
> caught up (components in 6.3: `p`'s in-flight segment PUTs and last batch, lock handoff, `i`'s
> catch-up and tier open, scheduler recovery). A relayed call that reaches `i` inside `T_s` is an open call on `i` whose
> handler has not started yet, exactly as any call is slow at a process that has just started
> serving. Nothing is stored, nothing is bounded by a flag, the call's own deadline governs.

`T_s` is made small by moving everything slow in front of the cut (`i` starts, loads and
warms before step 1; section 6.3 lists what each store can do in advance) and is **measured, not
assumed**: `i` records it per start (`kbf_handover_switch_ms`, and `handover/switch_ms` in the
state directory), `kbf handover` refuses to start an update when the last recorded value is above
`--handover-switch-budget-ms` (default 1000, a starting value for the spike) or absent
(`refused switch-too-long`, `refused unmeasured`), and the spike (section 12) must show a p99 under
the budget on a realistic host before any PR that cuts a store lands. If it does not, that store
is not handed over in place, and the answer for that host is split mode (section 15, Q1).

The honest limit: a single-copy store with an exclusive writer lock has a first instant at which
`p` no longer commits and a later one at which `i` does. Nothing in an update can make that interval
zero; this design aims at a few hundred milliseconds inside one call, gates on a measurement, and says so.

### 5.4 Sequence

0. `kbf handover` (or `systemctl start` of the idle instance). `i` does all its slow, lock-free
   work first (certificates, flags, runtime checks), binds its loopback ports, serves `/readyz`
   not ready (`Readiness::set_leader(false)`), and warms what it can (6.3). Its handlers for
   `Execute` and the storage calls wait on a roles-held latch. It cannot take the state store:
   the file lock is held.
1. `i` calls the predecessor's API (`POST /v1/frontend/drain`, `{"handover": {"peer": ...}}`,
   loopback peer, bearer token: the existing gates of `api.rs`; the port is `p`'s, from the port
   table). `p` runs the **prepare** checks, all reversible (6.3: disk, `T_s` budget, tier seal of
   the reversible part). A refusal (a different state path or address table, both instances
   active, a failed check) answers with a reason, `i` exits non-zero, `p` has not changed.
2. `p` runs `hand-over`: forwarding on, quiesce, fence, end worker streams, (storage block),
   demote the state store. Its answer is `released` with the generation it held.
3. `i` takes the next generation, `Fenced::take(store, Some(generation))`, with the epoch kept
   (`EpochPolicy::Keep`; today `StateStoreUri::epoch_policy` is `Bump` only and takes a
   parameter), recovers, takes the storage roles, opens the latch, `set_leader(true)` and says
   `READY=1`. From the latch opening `i` serves everything directly.
4. Envoy marks `i` healthy within one check interval. `i` reads that from the admin interface
   (both clusters), waits `--handover-settle-secs` (default 3), and calls `p`: `unready`. Envoy
   stops choosing `p` within `interval x unhealthy_threshold + timeout`; calls that raced the flip
   are relayed (5.2), not refused.
5. `p` follows its open `Execute` and `WaitExecution` streams from the `done/<op>` rows `i`
   writes, until each ends (`kbf_frontend_relay_lost_total` counts one it cannot find three times
   in a row: the only way this path fails a build, and it must be zero in every test but the
   planted one). `p` exits when no stream is open (its own and its relays), no `Result` is
   unacknowledged and, for all-in-one, no lease is held.

If `i` never becomes ready before step 2 nothing has changed. If `i` dies after `released`, `p`
still answers its own streams from the rows and relays fail UNAVAILABLE (retried by Buck2's
open-time retries); systemd restarts `i` as a cold start, which bumps the epoch, so the daemons
rerun their leases: slower. A stream that `p` was relaying for `i` breaks with `i` (the process that
held it died: the same class as a crash of `p`); a call that had not opened a stream is retried by Buck2.

**Why the same process for a frontend and all-in-one.** The sequence, ports, health flip, relay
and Envoy file are identical; all-in-one adds the storage and daemon halves of section 6.
`kbf frontend` and `kbf all-in-one` call one `handover::Driver` with a list of cut stages (section 11).

## 6. All-in-one: what the process still has to hand over

Envoy removes the listener transfer. It does not remove the problem that scheduler, storage and
daemon share a process. Everything below is kept from the first version, with the listener,
GOAWAY and park parts rewritten.

### 6.1 Blockers (re-checked on arch-v2)

| # | blocker | evidence |
|---|---|---|
| B1 | A running action dies with its daemon process: the keeper holds a pipe only the daemon holds. So the old process lives until its leases end. | `kbf-daemon/src/keeper.rs`, `kbf-driver-container/src/podman_killed.rs` |
| B2 | The meta log is single-writer by flock; there is no read-only open, no follower open and no `demote`; it is never compacted, so a start replays all of it. | `kbf-server/src/raft_log/mod.rs` (`RaftLogError::Locked`) |
| B3 | The cold `file://` store takes an exclusive `LOCK`. | `kbf-objstore/src/fs_store.rs` (`FsStoreError::Locked`) |
| B4 | The hot tier removes block files no staged object names at open. | `kbf-tier/src/open.rs` |
| B5 | The daemon half dies with the server half: `with_daemon` ends the daemon when `serving` ends. | `kbf-all-in-one/src/run.rs` |
| B6 | The in-process daemon's data plane is `LocalDataPlane` over the old process's cache. The control plane is a loopback mTLS worker session. | `kbf-all-in-one/src/plane.rs`, `kbf-node` |
| B7 | Two daemons on one host collide: the start sweep removes every container labelled with the node id and every lease directory; the `daemon-instance` claim; container names; the cache dir lock. | `kbf-driver-container/src/runtime.rs` (`sweep`), `kbf-node/src/instance.rs` |
| B8 | The scheduler books by worker (`free() = capacity - booked`); two workers on one box would double-book it. | `kbf-sched/src/scheduler.rs` |
| B9 | All-in-one starts with `Bump`; a bump makes every daemon kill its runs. | `kbf-server/src/state_store.rs` |
| B10 | `Drain` is one-way and `Farm::fence` has no unfence (on the handover branch). Section 5.1's staging puts the reversible work (prepare) in front of the fence. | `kbf-server/src/drain.rs` |
| B11 | The detached daemon fences itself after 40 s without an acknowledged heartbeat; the successor's `LEASE_GRACE` is 60 s. | `kbf-daemon/src/config.rs` (`FENCE_AFTER`), `kbf-sched/src/fence.rs` |
| B12 | The scheduler's recovery reads the state store, which `p` holds locked until `hand-over`; its time is part of `T_s`. | `kbf-state/src/sqlite/mod.rs`, `kbf-sched/src/scheduler/recover.rs` |

### 6.2 The detached daemon

- **Process shape.** After `released` the server half stops serving as scheduler and storage but the
  process does not exit; the daemon half runs on as a detached worker (B1, B5). The process exits when
  the conditions of 5.4 step 5 hold. A second SIGTERM ends open streams and abandons leases, which fails the
  builds whose streams `p` holds; it is the operator's explicit choice.
- **Control plane.** No new mechanism. The detached daemon's stream is ended by `hand-over`; it
  redials `--server`, which is Envoy's worker address, and reaches `i`. Between `hand-over` and
  `unready` a redial can reach `p` again (it is still ready); `p` refuses worker sessions in that window
  (5.2) and the daemon's redial loop (`--reconnect-ms`) converges. Same node id and instance id, so the
  scheduler sees the same process; it resends its Hello and unacknowledged Results before its first
  heartbeat, and keeps its runs only if the Welcome epoch equals its leases' epoch, hence `Keep` (B9).
- **Data plane.** At detach the daemon's data plane switches from `LocalDataPlane` over `p`'s cache to the
  remote plane every remote daemon uses, aimed at `i`'s REAPI listener (B6). An action that finishes after
  the cut uploads its outputs to `i` and reports its `Result` once; nothing writes into `p`'s stores
  after the demote.
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

### 6.3 Storage: who writes, who reads, and why no write waits

Alternative A (kept): **demote in place, early**. `p` gives up the writer role of the meta log,
the cold store and the hot tier at the cut, keeps what it needs to read, and `i` is the only
writer from then on. B (`p` stays the storage server until its last stream ends, `i` writes
through it) is **rejected**, with the reasons that now matter: it makes `i`'s every read and
write cross a second process for as long as `p` lives (hours); it still needs the same switch
later, at an instant nobody chooses, when `p` has gone idle and `i`'s callers are all that is
left, so the one residual wait of 5.3 would land on `i`'s own callers instead of on calls that
`p` is already relaying; and `p` could not release its locks, so the next update could not start.
C (`kbf frontend` plus separate daemon hosts) stays the answer for a site that will not accept
the measured `T_s` of A.

The ordered cut, in `p` at `hand-over`, each step a flag flip or a lock release; "advance" says what `i`
does before the cut so the step costs little:

| step in `p` | what | advance work in `i` (before step 1 of 5.4) |
|---|---|---|
| route flag | new storage calls go through `HandoverStorage` to `i` (5.2) | `i` binds its REAPI listener; its handlers wait on the roles latch |
| tier seal | flush the active block into cold; tier read-only and open so a `ByteStream.Read` in flight finishes (`Cache::seal`). The reversible part (seal the active block, drain the flusher) runs at prepare. | none: B4, `i` must not open the tier before the seal |
| writes already open | a write opened before the flag finishes on `p`: its segment's PUT completes through `p`'s open store handle (objects are immutable and renamed in atomically, keys carry a per-start epoch so two writers never collide, `kbf-front/src/cache/write_group.rs`), and its `PutBlobs` **commit is forwarded to `i`'s meta log** instead of `p`'s. The caller is answered when that commit is durable, as today. No write is cut and there is no write-drain bound. `p` releases the cold-store lock (below) only after its own in-flight segment PUTs have finished, so the longest of them (a blob large enough for a segment of its own is seconds) is a **component of `T_s`**: a write relayed to `i` meanwhile waits for `i` to hold the cold store. Measured by the spike; if it dominates, the cold store takes the PUT without the exclusive lock (an object-store cold tier has none; for `file://` a shared put lock and an exclusive sweep lock is the option to evaluate, Q8). | `i` accepts forwarded commits (an idempotent `PutBlobs` by object key); verified by T-4 |
| meta log | `RaftMetaLog::demote`: stop the tick task and committer after the in-flight batch, release `log.lock`, keep the applied `MetaState` queryable for frozen reads | `RaftMetaLog::open_follower` (new): read-only tail of the log, applying records as they are committed, so at the cut `i` has only the last batch to apply and then takes the lock. Without it `i` replays the whole log inside `T_s` (B2). The spike measures both. |
| cold store | `FsStore::demote`: release `LOCK`, keep reads | none: `i` blocks on the lock and takes it on release |
| state store | `store.demote()` last | `i` opens nothing; the rows are read at takeover (B12): measured inside `T_s` |
| daemon cache | release the cache dir lock (6.2) | none |

`HandoverStorage` is one `Arc<dyn Storage>` behind an atomic swap, used by the REAPI services and
by `LocalStore::over`. After the route flag: authority questions (`FindMissingBlobs`,
`GetActionResult`) go to `i`, never to frozen state; reads of a named digest try frozen state then
`i`; writes go to `i`. None of them is held in `p`: each is an open call on `i`, which answers when its
roles latch opens (5.3).

**Components of `T_s`**, each measured separately: `p`'s in-flight segment PUTs and the last meta batch; the lock releases; `i`'s meta-log catch-up (the tail after the follower open, or the whole replay without it); tier open on `i`; the scheduler's recovery from the state rows (B12).

**What `i` can and cannot do in advance, stated as claims to measure.** The follower open of the meta log and
the read-only open of the tier are the two pieces that decide `T_s`: without a follower open the meta log's
replay is in `T_s` and grows with the host's age (never compacted, no snapshots yet), and without a read-only
tier open (an open that does not run the orphan sweep) the tier open is too. If the spike measures a
`T_s` above the budget even with both, the log snapshots of `docs/design/metadata-snapshots.md` come first,
and all-in-one is not updated in place until they do (Q1).

**How `p` reaches `i`.** `i`'s REAPI listener is a known loopback port (the other block of the
table), reached over the farm-CA TLS with the node's identity. It is the same kind of hop as the proxy to a
storage node (`kbf-front/src/proxy`).

**Prepare (reversible).** At step 1 of 5.4, before the cut, `p` seals the reversible part of the tier, checks free
space on the cold, meta and state directories, and evaluates the `T_s` budget of 5.3. It refuses
(`refused disk`, `refused switch-too-long`, `refused unmeasured`) and nothing has changed. Each successor start
records its measured `T_s`. The first start into the template is a cold restart that records it.

**Committed cut block**, in the order of the table above. A failure after the route flag is committed: the
process fails closed (`drain.close()`, non-zero exit, no marker), systemd restarts it as a cold start, its builds
fail and their actions rerun, and `kbf_handover_demote_failed_total` alerts. Each stage has a failpoint test.


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
(the split-mode update note, `docs/design/split-mode-updates.md` on branch `docs/split-mode-updates`, not yet on arch-v2, section 4.1, U1) and now concerns Envoy updates only, which are rarer than kbf updates. The
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
| t0 | `i` starts, not ready, warms | nothing |
| t1 | `p` runs `hand-over` and stays ready | open `Execute` streams continue on `p` (answered from `done/` rows once `i` has the scheduler); a new call that Envoy still sends to `p` is answered through `i` (5.2), slower by at most `T_s` (5.3); the connection to Envoy is unchanged |
| t2 | `i` holds every role, is ready; Envoy sees it healthy in at most one interval | new streams go to `i` |
| t3 | `i` sees itself healthy in Envoy, settles, tells `p` `unready`; Envoy stops choosing `p` | nothing: no frame reaches Buck2, and no stream is cut (3.1) |
| t4 | `p` exits when its last stream ends | nothing |

No GOAWAY is needed from kbf and none reaches Buck2: Envoy owns the downstream connection, and `p` is
never sent a GOAWAY in an update. Streams open on `p`'s connection continue because a failed health check
removes a host from selection and leaves its connections alone (3.1; inference, T-E4 measures it on the
pinned Envoy). There is no window with no healthy backend, so there is no Envoy-generated 503 and no
reliance on Buck2's 1.5 s open-time retries. The one added latency, `T_s`, is a few hundred milliseconds
at most by the gate, and only for a call that arrives between t1 and t2 or is still sent to `p` between t2 and t3.

For the in-process daemon the cut is `hand-over` ending its stream, then a redial through Envoy, as in 6.2.

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
- **N / N-1.** The windows of the split-mode update note (`docs/design/split-mode-updates.md` on branch `docs/split-mode-updates`) section 3 apply unchanged: the old instance reads, for
  three looks each, the `done/`, `op/`, `lease/` and queue rows the new one writes, so row encodings are frozen across one
  step and the SQLite schema follows expand/contract. Envoy's config is version-independent, so a kbf update is not
  also an Envoy compatibility question.

## 10. Failures

"Class": reversible (before the cut) or committed.

| point | class | Buck2 | operator / daemon | recovery |
|---|---|---|---|---|
| `i` fails or is refused before it calls `p` | reversible | nothing | `systemctl start` fails | `p` unchanged; fix, retry |
| prepare refuses (disk, flush, `T_s` budget, unmeasured) | reversible | nothing | `i` exits with the reason | retry |
| `i` dies after `released`, before serving | committed | streams `p` relays for `i` break; new calls are answered by `p` with UNAVAILABLE (a refusal only because `i` is gone) and retried by Buck2 until systemd restarts `i` | cold start bumps the epoch; the detached daemon reruns its leases | callers of `p` are answered from rows |
| `T_s` above its budget | committed | calls in the window are slower; none is dropped | `kbf_handover_switch_ms`; the next update is refused (`refused switch-too-long`) | fix the slow component; see 6.3 |
| `i` never shows healthy in Envoy (admin API or check broken) | committed | none: `p` stays ready and keeps relaying; no capacity is lost | the fallback timer (`--handover-unready-after-secs`) runs; alert on `p` still relaying | repair Envoy's check; `p` has no way back, so `i` keeps serving |
| a demote stage fails | committed | open streams on `p` end UNAVAILABLE | `p` exits non-zero, cold start | actions rerun |
| `p` dies mid-follow | committed | its open streams and relays break (the process holds them) | its daemon and keepers die (B1); marker stays | `--acknowledge-crash` |
| **Envoy crashes or is killed** | n/a, **new** | every open `Execute` breaks | systemd restarts it; kbf unaffected; worker sessions redial | the cost of the uniform topology: a process in front of every build. Mitigation: low `OOMScoreAdjust`, a memory limit, no `restart`, a watchdog |
| **Envoy hot restart: parent closed at `--parent-shutdown-time-s`** | n/a, **new** | streams still open then break | `kbf_envoy_parent_cut` | set the time above the longest action (Q2) |
| a health check flaps ready on a fenced instance | committed | relayed calls | `kbf_handover_relayed_total` rising after `unready` | the check includes the stop flag |
| two updates back to back | n/a | n/a | `kbf handover` refuses when both instances are active | bounded by the longest action |

Reading of the table: the update fails a build only through the first-version rows (an operation with no trace
for three looks, a second-signal escalation, a crash of the process that holds the stream, a committed demote failure,
a successor that dies with relays open) plus the two Envoy rows. The earlier write-drain expiry row is gone: no write is
cut by a timer. Envoy is a single process in front of every `Execute`; that is the price of one update path
everywhere and the project has taken it.

## 11. Reuse from the fd-passing branches

- **Kept**: `Farm::quiet/fence/follow/end_worker_streams` and the durable `done/` rows
  (`farm/handover.rs`), `Drain` as a staged object (5.1), `Fenced`/`Store::demote`, `READY=1`
  (`handover/notify.rs`, `ready.rs`), the daemon-side redial, the `Stop`/`Latch` pair.
- **Dropped**: the control socket and `SCM_RIGHTS` wire (`handover/wire.rs`, `incoming.rs`, `outgoing.rs`), `listen.rs`'
  adopt and spare dup, the address check against the holder's listeners, GOAWAY as the client's switch, and the
  "connections wait in the backlog" step. The coordination the wire carried (refusal reasons, the generation) is the API
  call of 5.4 and the state store's own generation.
- **New**: the Execute relay (5.2); `HandoverStorage` (6.3); `RaftMetaLog::open_follower` and a read-only tier open (6.3).
- **Open**: whether the fd-passing path stays as a no-Envoy shape (Q12 of section 15). Recommendation: trim it. A second way
  to do one thing, tested against none of the Envoy topology, and an unconditional source of the question "which
  update path is this host on".

The assembly of `frontend.rs` (`Prepared`, `Trio`, `hello`, `take_over`, `start_or_take_over`) becomes a
`handover::Driver` both modes call, with a golden of its call sequence for the frontend taken before the refactor.
`kbf all-in-one` gains `--instance`, `--envoy-admin`, `--handover-unready-after-secs`, `--handover-settle-secs`,
`--handover-switch-budget-ms`, `--handover-follow-interval-ms`, all off without `--instance`.
Configuration is flags; the only environment variable is `NOTIFY_SOCKET`.

## 12. Spike gating the design (PR 0, throwaway, not merged)

Answers, with a real pinned Envoy and stub or real kbf, before any merged PR:

1. **Envoy**: a silent stream longer than the default 15 s route timeout survives with the box settings; the default
   300 s idle timeout is not reached with progress frames; a failed health check leaves an open stream alone and stops
   new streams within the settle; with both hosts healthy and one turning not-ready there is no moment with a local 503;
   `GET /clusters?format=json` shows each host's health flag as the design reads it; more than 1024
   concurrent silent streams through default breakers fail and through the box settings do not;
   `close_connections_on_host_health_failure` on the worker cluster cuts a daemon's session (so it stays false); a hot restart
   with a 60 s drain time leaves a silent stream open past it and closes it at the parent shutdown time; what a second hot
   restart inside the first's window does.
2. **kbf, the switch**: `T_s` and each component of it (6.3) on a realistic host, p99 over repeated cuts: with and without a
   meta-log follower open; with and without a read-only tier open; the longest in-flight segment PUT with large blobs; the
   scheduler's recovery on a busy state file. `RaftMetaLog::demote` keeps its applied state readable. Whether a segment PUT through
   a handle that has released the cold-store lock is safe for `file://`. The result is a number against
   `--handover-switch-budget-ms`, and the go or no-go for A in 6.3.
3. **kbf, the relay**: a relayed `Execute` through `i`'s listener before its roles latch opens completes after it, with the
   client's own deadline honoured and nothing queued outside the open call.

## 13. Tests

Each test names the defect it catches and the line that turns it red; a mutant is planted and seen red on the farm
before a PR goes up. Unit tests sit in the library they cover; real Envoy tests use the pinned binary (CI fetches
it); container tests extend the `podman-x86` job, everything else runs in `test-x86` and `test-arm`.

| id | test | catches | mutant |
|---|---|---|---|
| E1 | the box render is golden, passes `envoy --mode validate`, and `envoy check` passes it | a render Envoy refuses or the checker misreads | add an unknown field |
| E2 | `envoy check` fails each defect of 3.1 alone: route timeout default, `max_stream_duration`, idle timeout default, default breakers, `close_connections_on_host_health_failure` true, worker idle default, a retry policy on `ByteStream` or with `unavailable`, one instance missing, overlapping ports, a parent-shutdown below the longest action | a render that silently cuts or refuses a stream | each setting returned to Envoy's default |
| E3 | real Envoy and two stub servers: a silent `Execute`-shaped stream longer than the (scaled) timeouts completes | the 15 s and idle defaults | route timeout left at default |
| E4 | stub `a` turns not-ready while `b` is ready: no new stream reaches `a` within the settle, the open one on `a` finishes, no 503 is ever returned, for the REAPI (L7) and worker (L4) clusters | health failure cutting a stream; a daemon session cut; a gap | `close_connections_on_host_health_failure` true on the worker cluster; `a` turned not-ready before `b` is ready |
| E5 | more than the default breaker's concurrent silent streams are held open through Envoy | the 1024 cap | breakers left default |
| E6 | `kbf frontend` a/b with Envoy: an `Execute` open across the handover completes; calls issued continuously through it see zero errors and zero Envoy 503; `kbf_handover_relayed_total` is non-zero (the relay is exercised, not skipped) | the update gap | `unready` sent at the cut instead of after `i` is healthy; relay replaced by a refusal |
| E7 | real hot restart: a silent stream outlives `--drain-time-s`, new streams reach the new epoch, the parent is gone at the parent-shutdown time; with a parent shutdown below the stream's length the stream is cut and the test goes red | the default 900 s | parent shutdown at 900 s |
| E8 | before the last PR: one real Buck2 build through a kbf handover, and one through an Envoy hot restart | Buck2's behavior at GOAWAY and at a relayed open | skip the relay |
| T-R | the relay: `Execute` and `WaitExecution` after the cut reach `i`, metadata forwarded, `i` authorizes again, the response stream is relayed to the end; a call that reaches `i` before its latch opens completes after it with no queue object outside the call; the client's deadline ends a call that never starts | a lost call, a hold bounded by a flag, a bypassed policy | answer from `p`'s own scheduler; drop the metadata |
| T-D | `Drain`: `hand-over` alone leaves `/readyz` ready and the gRPC health SERVING; `unready` flips both; no handover body keeps today's `begin` (GOAWAY included); each stage idempotent | a split that loses an effect, readiness off too early | flip readiness in `hand-over` |
| T-A | the API call: refusal reasons; a different state path refused; the answer carries the generation; no handover body keeps today's meaning | a refusal that proceeds | accept without comparing |
| T-1..T-3 | `RaftMetaLog::demote` and `open_follower` (the follower's state equals the writer's at the cut, a torn tail is ignored), `FsStore::demote`; the second open succeeds while reads work | B2, B3 | skip the unlock; apply the torn tail |
| T-4, T-5 | `Cache::seal` and the orphan pass; `HandoverStorage` (`FindMissing` and action-cache lookups to `i`, named reads frozen then `i`, a write open at the cut finishes and its commit lands once on `i`, a write after the cut lands on `i`) | B4, a stale hit, a lost or doubled write | answer from frozen state; commit on `p` after the demote |
| T-6 | real stages with a failpoint at each cut-block index: fail-closed outcome, no marker, cold start serves; state demote last | a partial demote | move `store.demote` earlier |
| T-7 | `Keep` on the real start path; recovered lease books on the predecessor's node id; successor registers `total - booked`; two cycles a, b, a, the second gets grants | B8, B9, the stale cordon | `Bump`; skip the cordon delete |
| T-8 | the real start sweep against a fake container runtime holding two owner labels | B7 | share the data dir |
| T-9 | golden of the Driver's call sequence for the frontend, plus the existing suite | a refactor changing both sides | reorder two stages |
| T-10 | all-in-one with a remote daemon: a long action finishes on the old process; its output readable through the successor; an `Execute` longer than every keepalive completes (container, x86) | the whole contract | kill the old process at `released` |
| T-11 | an action finishes after the cut, local and remote: its outputs upload to `i`, its `Result` is accepted once, nothing is written into `p`'s stores | unacknowledged results, a write into a demoted store | keep the local data plane |
| T-12 | `i` dies after `released`: a cold start serves, callers of the old one get answers | the unrecoverable window | the old exits at `released` |
| T-13..15 | injected ENOSPC at prepare refuses and the old keeps serving; `T_s` over the budget refuses the next update; the short-fence flag: under budget zero reruns, over it the leases fence and rerun and no caller sees an error from the old one | disk-full reversibility, the budget, the fence interaction | raise the fence; record no `T_s` |
| T-16 | SIGKILL a detached old: the successor is unaffected, `precheck` refuses the restart, `--acknowledge-crash` clears | restart into takeover | write the marker after `released` |
| T-17 | a load of writes, reads and `Execute` calls running across the whole cut: every call completes, none is refused by `p`, the longest call is within `T_s` plus its normal latency | any hold, park or refusal in the sequence | add a sleep in the cut; refuse writes at the route flag |

Failpoints and the short-fence flag live behind a cargo feature with a gate test that fails when a release build enables
it. No test pins a wrong answer.

## 14. PR split

PRs that touch `frontend.rs`, `kbf-deploy` or `kbf-cli` stack on the trimmed #879/#887 or on a re-implementation of
their farm half (Q12); the rest stack on arch-v2. Sizes are estimated changed lines.

| PR | content | size |
|---|---|---|
| 0 | spike, section 12, not merged | 500 |
| | decision (Q5) after PR 0 | |
| E1 | envoy kit `Box` topology: render, config keys, `Server` by host and port, new checks, E1/E2 | 700 |
| E2 | real-Envoy tests E3-E5, E7 against the render | 500 |
| 1 | `Drain` staged, the API body and status, T-D, T-A | 400 |
| R | the Execute relay beside `kbf-front/src/proxy`, T-R | 600 |
| 2 | `handover::Driver` out of `frontend.rs`, T-9; the farm half (`fence`, `follow`, `end_worker_streams`) and `Keep` through `StateStoreUri` | 700 |
| 3 | frontend wiring: `--instance`, `--envoy-admin`, predecessor call, ports, settle and unready | 400 |
| 4 | `kbf-deploy`: unit templates and per-instance drop-ins, `kbf-envoy.service` with the restarter, binary path, `kbf handover`, lock file | 600 |
| 5 | E6, E8 and T-17 for the frontend | 400 |
| 6-9 | all-in-one: `FsStore::demote`, `RaftMetaLog::demote` and `open_follower` (T-1..3), `Cache::seal`, read-only tier open and `HandoverStorage` (T-4, T-5, forwarded commits), the detached daemon with the remote data plane and per-instance data dir (T-8, T-11), the wiring, cordon reset and capacity (T-6, T-7, T-12..16) | about 3500 |
| 10 | all-in-one `kbf-it` T-10, T-12, T-17, container CI placement | 500 |

PRs 6 to 9 are inert until their flags are given. Everything from PR 6 is gated on the spike's `T_s` answer.

## 15. Open questions

1. **Is a bounded in-call switch time acceptable as "no request is held"?** The design has no park, queue,
   admission gate or refusal; a call that lands in the switch is slower by `T_s` (gate: p99 under 1 s). A single-copy store
   with an exclusive writer lock cannot do better, so literal zero wait exists only in split mode (storage nodes routed and
   replicated, frontend relay only). Recommendation: accept `T_s` with the gate; if the spike misses it, all-in-one is
   not updated in place and the supported no-wait shape is `kbf frontend` plus storage and daemon nodes (Alternative C).
2. **The longest action.** Cap an action's timeout farm-wide (`--max-action-timeout`) so Envoy's parent-shutdown time is a
   derived number, or accept that an action longer than it is cut by an Envoy hot restart. Recommendation: cap, default 6 h.
3. **Certificate renewal at Envoy**: hot restart or file-watched secrets. Recommendation: hot restart; it avoids an
   xDS source and the path is already exercised. (The split-mode note leans to a file-based secret source; one of the two
   notes changes.)
4. **Cgroup caps across two all-in-one units** (`actions/` `cpu.max`, `--actions-memory-max-gib` are per unit).
   Recommendation: one `actions/` parent shared by both instances; verify before the capacity PR.
5. **Is all-in-one worth the storage work?** Alternative C already updates a fleet with the frontend half and a no-wait
   storage update. Recommendation: decide after PR 0; build the frontend half first either way.
6. **Cold store and the PUT lock** (6.3): `file://` needs `FsStore::demote` and the longest in-flight PUT is in `T_s`; an
   object store has no lock. Recommendation: spike a shared put lock; keep `file://` support.
7. **Node id per instance**: two certificates (no change to `peers.admit`) against a suffix rule. Recommendation: two certificates.
8. **The split-mode update note's E4 to E6** (successor "accepts and holds"; old goes NOT_SERVING at the offer) contradicts 5.1 and
   5.2. This note's version is the later decision; that note needs the same relay and the same readiness order.
9. **#743** (control plane as calls, paused): if it lands the detached daemon's control plane becomes a feature, not a redial.
   Recommendation: do not land it before this is built.
10. **Health check kind for `kbf_reapi`**: the gRPC check (kit today; on the REAPI port, over TLS) against an HTTP
    `/readyz` check on the API port. Recommendation: the gRPC check, it is already specified and checked and is the same evaluation.
11. **Forwarded caller identity in the relay**: forward request metadata and let `i` authorize again (this note), or forward a signed
    assertion from `p`. Recommendation: forward and re-authorize; `i` trusts nothing `p` says.
12. **Keep the fd-passing path (#879/#887) as a no-Envoy shape, or trim it?** Recommendation: trim to the farm half
    (section 11); a second update path is a second thing to test and to explain.

Reviewed and not taken: (a) readiness off before the cut with Envoy's health check as the only gap cover (5.2: a window with
no healthy backend, a local 503, longer than Buck2's retry window); (b) `p` stays the storage server and `i` writes through it
(6.3: a second hop for hours and the same switch later); (c) a refusal of calls after the cut that Envoy retries (a hold under
another name, and needs a retry policy on `Execute`); (d) routing the worker port by instance number without Envoy (every
update would change a remote daemon's address); (e) L4 for REAPI (new streams on an existing connection would stay on the
old process); (f) a Welcome-carried extended daemon fence as the default (it weakens the only partition safeguard); (g) retries
on `ByteStream` and `BatchUpdateBlobs` (Envoy buffers the request); (h) queue or park modes of any kind (the project's decision).
