# Frontends behind kbf-proxy: a pool on one shared state store

This page says what the code does now. A host's public port belongs to `kbf-proxy`, which
balances over frontends that run on their own; the frontends share one Postgres state store;
a frontend is updated by taking it out of the pool, retiring it, restarting it and putting it
back, one at a time. The crate's API and the decisions behind it are in
[kbf-proxy-api.md](kbf-proxy-api.md).

The recommended way to update a frontend without failing a build is a load balancer in front of several frontends: [frontends-behind-a-load-balancer.md](../deploy/frontends-behind-a-load-balancer.md).

Removed, and not described here any more: the proxy's two-slot supervision of frontend children
(starting, adopting, swapping them, the hold of new streams during a swap, the routing of a
`WaitExecution` by the old child's term, the update API at `/v1/update/frontend` and the
`kbf-installer update` client), the handover between two frontends of one host (the control
socket, the takeover and the stand-down, the writer generation and its fence, the relay of calls
to the successor, the staged readiness, the wait for Envoy's health check) and the admin port a
frontend served for them. The flags that turned them on are still accepted and do nothing: each
one given logs a warning (`docs/api.md`, "The admin calls (removed)"). The sections are numbered
as they were, so that a citation of section 3, 9 or 11 still finds its topic; the numbers of the
sections that described the removed machinery are not reused.

## 1. Decisions

1. **One public port per frontend host** (above 1024), owned by `kbf-proxy`, shipped in the
   `kbf-installer` package as the `kbf-installer proxy` verb. It is a separate unprivileged
   service (user `kbf-server`, the frontend's own), not part of the root helper.
2. **The proxy terminates TLS for everything on that port**: REAPI, the operator API and worker
   sessions. It verifies a client certificate when one is presented (farm CA, role URI, node id
   in the SAN, EKU), and forwards every stream over loopback to a frontend of its pool with the
   verified identity (node id, role) as metadata. Routing is per HTTP/2 stream and never sends
   GOAWAY to a client in a roll or a drain (the proxy's own GOAWAY comes only on its shutdown
   and on a flood-limit trip: `kbf-proxy-api.md` section 10).
3. **Frontends are plain `kbf --mode=frontend` processes that run on their own**, each under a
   unit of its own, on loopback ports at fixed offsets from the `base_port` of its
   `[[proxy.frontend]]` table. The proxy starts, adopts, restarts and swaps none of them. A
   frontend trusts identity metadata only from the proxy (section 3).
4. **Frontends share one Postgres state store and none fences another.** A frontend that opens
   the store takes no writer generation (section 11); its scheduler writes are conditional on
   the rows it read, so any number of frontends serve at once, and each takes up the work the
   store shows it (`docs/design/scheduler.md`, "Frontends that share work").
5. **An update is a roll, one frontend at a time**, driven by `kbf-updater` through each
   member's own operator API, not through the proxy: the member drains itself, is installed,
   restarted and waited for (section 4). The updater does not call the proxy's admin API.
   Nothing is held, queued or swapped.
6. **An `all-in-one` process is not a member the updater rolls**: it serves its own storage and
   daemon, and the pool is of `kbf --mode=frontend` processes. A proxy in front of it is the
   operator's.
7. **The state store is Postgres** (section 11).

## 2. Process model on a frontend host

| Process | User | Listens | Job |
|---|---|---|---|
| `kbf-installer run` (root helper) | root | nothing | Acts on the orders of the pull update path. It starts, stops and updates no frontend: a frontend's binary is `/usr/bin/kbf`, which the package manager replaces. |
| `kbf-installer proxy` (`kbf-proxy.service` and `.socket`) | `kbf-server` | the public port, from systemd socket activation | TLS, client-certificate identity, routing, the pool and its admin API, open-stream accounting. |
| `kbf --mode=frontend`, one unit each | `kbf-server` | loopback ports at its `base_port` offsets | A frontend. Plain h2, identity from the proxy only. |
| Postgres | `postgres` | Unix socket | The shared state (section 11). |

- **The proxy runs as the unprivileged `kbf-server` user**, the frontends' own, so that they can
  read the secret it writes. The root helper keeps its "no socket, no network" property, and the
  proxy and the helper exchange nothing.
- **Public address.** The packaged `kbf-proxy.socket` listens on port 8980. `[proxy] listen` in
  `installer.toml` moved it: `kbf-installer setup` wrote the drop-in
  `kbf-proxy.socket.d/listen.conf` (empty `ListenStream=`, then the address) and never restarted
  the socket. No release ships the socket now, and `setup` writes no drop-in. The proxy itself ignores the key: it takes the socket systemd hands it.
- **The identity secret.** The proxy writes a 32-byte file under `/run/kbf-proxy/` (tmpfs, so per
  boot; `RuntimeDirectoryPreserve=yes`, so it survives a restart of the proxy). A frontend is
  given the file with `--proxy-auth-file` and reads it when it starts.
- **Order of units.** The proxy needs no order against the frontends: it polls each one's
  `/readyz` once a second and sends nothing to a frontend that is not ready while another is.
- Hardening: `OOMScoreAdjust=-900` for the proxy, a high `LimitNOFILE`, bounded buffers,
  header-list caps, h2 rapid-reset and CONTINUATION-flood limits, a `catch_unwind` per
  connection.

## 3. TLS, identity and per-stream routing

**One TLS terminator, one port, the server certificate chosen by SNI.** There is no ALPN demux
and no passthrough: every connection is terminated here. The proxy holds a list of server
certificates, each with the DNS names it is served for, and presents the one whose name is in
the ClientHello's server name (names compare case-insensitively). A ClientHello with no server
name, or one nobody configured, gets the certificate the configuration marks `default`; the
default is never implied by position. So a Buck2 client that asks for the public name is served
the public certificate and verifies it against the public roots, while a farm node that asks for
the farm-internal name is served the certificate of the farm CA that it verifies. Which
certificates, names and default: section 9 of [kbf-proxy-api.md](kbf-proxy-api.md). ALPN is
`h2`, `http/1.1`; 0-RTT is off. The proxy requests a client certificate that is optional at the
handshake; this does not depend on the name the client used.

**Certificate reload.** `systemctl reload kbf-proxy` runs `/bin/kill -HUP $MAINPID`
(`ExecReload=` in the unit). The proxy re-reads every server certificate, key and the client CA
together, builds the whole configuration, and swaps it in only if all three make a working one
(a certificate already expired is refused). Handshakes that start afterwards use the new one; an
open connection and its streams keep the configuration they were accepted under and are not cut.
The work runs on the blocking pool; signals that arrive during a reload coalesce. SIGHUP never
stops the proxy. A rejected file keeps the old configuration, adds one to
`kbf_proxy_tls_reload_failed_total` and writes one line to the journal that names the failure;
`systemctl reload` still exits 0, because the signal is asynchronous, so a renewal hook that must
know checks the counter or the served certificate. There is no retry: a certificate and key
renewed in two writes fail as a mismatch until the signal that follows the second write, so the
hook signals after both files are in place.

- A presented certificate is verified against the farm CA with the existing checks: role URI,
  node id in the SAN, EKU (client auth). A certificate that fails is a handshake failure, not
  anonymous.
- **Per path class:** worker and storage-node paths (`/kbf.worker.*`, `Membership`) require a
  verified certificate with a worker or storage role, else `UNAUTHENTICATED`. REAPI and Buck2
  auth is configurable: `buck2_auth = "none"` (network-level, for example an overlay network),
  `"bearer"` (an API-key header, constant-time compare), or `"mtls"` (verified certificate
  required). Operator paths take the operator token.
- **The web UI (`/ui`, `/ui/...`) is open**: no certificate and no token are asked for, and only
  `GET` and `HEAD` pass (any other method is a 405 at the proxy). It is forwarded to a
  frontend's operator listener like an operator call, a path that is not plain segments under
  it (`..`, `.`, an empty segment, an encoded dot or slash) is a 404, and the `[proxy]` key
  `ui = false` makes `/ui` a 404. The class is its own: every `/v1` path, `/metrics`, the worker
  and the REAPI paths keep the class they had. The proxy adds no security header to a UI answer;
  the server's pass through unchanged.
- mTLS therefore authenticates who (which node, which role), not only "trusted".

**Identity to the child.** The proxy strips every inbound `x-kbf-*` header, then sets
`x-kbf-node-id`, `x-kbf-role` (from the verified certificate; absent when none),
`x-kbf-node-ips`, `x-kbf-client-addr` and `x-kbf-proxy-auth`, a per-boot secret.

- **`x-kbf-node-ips`** (additive; the farm's storage nodes register as `<ip>:8983`, so the child
  needs the address the certificate names, not only its DNS name). The IP subjectAltNames of the
  **verified leaf certificate** (never an intermediate), as canonical text (IPv6 compressed),
  sorted, each once, comma separated with no space: `192.0.2.1` or `192.0.2.1,fd00::1`. At most
  16 addresses; a leaf that names more is refused at the proxy like one with two DNS names. Set
  only with a verified certificate and only when the leaf names at least one address; absent
  otherwise; always stripped from the inbound request like every `x-kbf-*`. The child believes
  it under the same conditions as `x-kbf-node-id` (loopback peer, secret) and refuses the call
  `PERMISSION_DENIED` if it is empty, repeated, oversized, or has any element that is not a
  canonical address, never reading it in part. `Membership` covers an IP host by exact address
  equality with those addresses (an IPv4-mapped IPv6 address is not its IPv4 address). **Skew
  fails closed:** an old proxy sends no header, so a new child sees no addresses and refuses a
  registration by IP as before; a new proxy's header meets an old child, which strips the
  unknown `x-kbf-*` and registers nothing by IP as before.
- **`x-kbf-client-addr`** (additive). The address the client connected to the proxy from, as
  canonical IP text (an IPv4-mapped IPv6 address unmapped), no port, set on every forwarded
  request, with or without a certificate. It is the proxy's own reading of the connection, never
  a client's header (the inbound one is stripped like every `x-kbf-*`). It exists so a child's
  per-caller bound can tell browsers apart: an open `/ui` request carries no node id, and every
  browser otherwise connects, as far as the child sees, from the proxy. The child believes it
  only under the conditions of `x-kbf-node-id` (loopback peer, secret). **Skew:** an old proxy
  sends none (the child falls back to the peer address); a new proxy's header meets an old
  child, which strips the unknown `x-kbf-*`.
- The secret is 32 random bytes in a `0600` file under `/run/kbf-proxy/`, read by the child at
  start. Not argv, not env.
- A child accepts identity headers only from a loopback peer that presents the secret
  (constant-time compare). Otherwise it ignores them and treats the caller as anonymous, so a
  local process without the secret cannot claim a node id. The child's own `Hello.node_id` check
  compares against `x-kbf-node-id` instead of the peer certificate.
- The child's side of this contract (the flag, the gates, what is refused, and the limits) is
  `docs/api.md`, ["Who may write"](../api.md#who-may-write), section "Behind the farm's proxy".
- **Operator-API trust.** A "Who may write" gate that relied on "peer is loopback" would be open
  to the public once a proxy forwards. Behind the proxy a write is refused unless it arrives with
  the proxy's secret and passes every other gate, the operator token included.
- **Pre-stream limits.** "No timeouts" applies to streams, not to the front door. There are
  handshake, preface and header timeouts (about 10 s), connection and per-IP caps, and rate
  limiting of wrong tokens before any hashing.

**Routing.** The six REAPI paths, ByteStream and `google.longrunning.Operations` (GetOperation)
go to the REAPI port of a frontend, worker and `Membership` paths to its worker port, the
enrollment service to its enrollment port, and every other `/v1/*` and `/ui` path to its
operator port. A path that matches nothing is answered by the proxy and never forwarded.
`/v1/update/frontend*` is a path of the proxy's own: once the operator token is checked, the
built-in handler answers 503, because the proxy has no update API. `/metrics` is the proxy's own
series (the pool's members among them); `/healthz` and `/readyz` are the proxy's view, `/readyz`
being 200 while some frontend is up and ready.

**Placement.** A stream is placed on one frontend of the pool when its headers arrive, by one
pick under the pool's lock: the frontend with the fewest streams open among those that are up
(not draining) and ready. It stays there until it closes, so one Buck2 connection carries streams
to several frontends. A `WaitExecution` is routed like any call: an operation's name over a
state store is `operations/<id>`, which every frontend answers from the store's rows
(`docs/design/scheduler.md`, "Results across frontends"). The retry rules, the keepalive, the
flow control and what is done with a child's GOAWAY and resets are section 10 of
[kbf-proxy-api.md](kbf-proxy-api.md).

## 4. Updating a frontend

`kbf-updater` rolls the members of a pool one at a time, before every other host, through each
member's operator API on loopback at `127.0.0.1:<base + 2>`, and never when the member is the
last one serving. The proxy is not asked to drain, retire or undrain anything; it stops sending
to a member whose `/readyz` answers 503 and sends to it again when it answers 200. The steps are
those of [../deploy/kbf-updater.md](../deploy/kbf-updater.md), "What a roll does with a frontend
pool member", which is the authority; in short:

1. **Peers ready.** Every other member answers `/readyz` 200 for a window in a row.
2. **Drain.** `POST /v1/frontend/drain` on the member (`docs/api.md`): `/readyz` 503, no new
   work, GOAWAY on its listeners, a wait for the open executions, and its exit. The updater
   waits for the exit, or for the drain status to report the phase `closing`.
3. **Install and restart.** The package is installed on the host (once per host), and the
   frontend's unit, `kbf-frontend@<name>.service` (`kbf-frontend.service` for the member named
   `-`), is restarted (`restart frontend=<name>` of `kbf-host-install`). A stop drains: the
   unit has `TimeoutStopSec=infinity` ([../deploy/kbf-units.md](../deploy/kbf-units.md)).
4. **Ready.** The updater waits until the member's `/readyz` answers 200 and its `/healthz`
   reports the commit of the target.

An earlier build of the updater drained a member at the proxy, waited for its streams, called
`POST /v1/frontend/retire` and undrained it at the end. That roll is superseded by the one
above; the frontend still serves the retire route (`docs/api.md`), and the updater no longer
calls it.

**What each party sees.** Buck2 builds in flight keep their `Execute` and `WaitExecution`
streams: the member's drain refuses new calls and lets the open executions finish, and the
calls it still holds when it exits are answered from the store. New calls go to the other
frontends. Daemons redial and
keep the runs of their leases: the epoch is the store's, and a frontend that starts keeps it.
An operator's cordon or drain is a row in the store, so every frontend holds it.

**What a roll does not carry.** Counters restart at 0 in the restarted process (use `rate()` or
`increase()` over a sum of processes). The views kept in memory (`GET /v1/nodes`, `/v1/leases`,
`/v1/queue`) list what is connected to that process, and a restarted frontend lists a daemon
only after it reconnects. An invocation record is built by a process's own recorder from the
events its own farm sent, and no row of the state store carries it: a build that one frontend
recorded is not found through `/v1` or the UI on another, and the callers of that build are
answered from the `done/` rows all the same (the test
`a_successors_recorder_has_no_record_of_a_build_but_its_caller_is_answered` pins it).
`action shape:` is logged again, once per key, by the restarted process, and the timer of the
unservable wait starts again. Two refused writes are not retried: when the store refuses the
rows of a committed record (an `Effect::Commit`) the farm logs it and drops the effects that
depended on it, and when it refuses the `done/` row of a finished operation the farm logs it
and answers its callers from memory; the other frontend recovers such an operation from the
rows it finds (the lease it was granted, or no result). Only a refused requeue is written
again (`flush_unsettled`). A kill whose store write was not acknowledged before a crash costs
at most one extra run or one extra budget step. The memory an operation is booked is the
highest rung it was raised to; a booking from the last measured peak plus a buffer is not
built (learned sizes are off by default, `--learned-sizes off`; in `shadow` they only log and
change no booking).

**When a step fails**, the roll stops and the frontend is named as draining or not serving in
the updater's detail; the updater does not start another until an operator has fixed it and
started a new roll. What the
operator does is in [../deploy/kbf-updater.md](../deploy/kbf-updater.md).

## 5. Failure behaviour

| What fails | Result |
|---|---|
| A frontend dies | Its open streams end, as for any crash. The proxy's poll finds it not ready within a second and sends nothing to it while another frontend is ready. The queue and the leases are in the store, so the others take them up; daemons that were connected to it redial. systemd restarts the unit (`Restart=on-failure`). |
| The state store is unreachable | A frontend's `/readyz` fails on the store and the proxy prefers a frontend that is ready. A pool whose probes all fail still serves. |
| The proxy dies | Every client stream through it is cut, including streams to frontends that go on running. systemd restarts it, and socket activation keeps the port, so new connections wait in the accept queue. The frontends keep running and their secret survives. This is the main residual risk of a single front door; it is the reason the proxy changes only with a package. |
| The root helper dies | Not in the data path. |

## 9. Compatibility between releases

A roll updates the frontends first, and the nodes take their release from their hosts'
packages, so a frontend of release N+1 serves nodes of release N.

| Rule | Statement | Held by |
|---|---|---|
| R1 | The new frontend accepts every protocol from `min_protocol` up. A release never raises `min_protocol` in the release that introduces new behavior. | The worker handshake (`negotiate`, `crates/kbf-server/src/worker.rs`) and the cross-version test below. The server reports the inputs (`oldest_protocol`, `protocol_unknown`), and no frontend compares them before a roll. |
| R4 | A release offered to move to must not be older than the running build, by build `seq` (`kbf --version --json`, never by the text of a commit); a rollback skips the check. | `release/age.rs` in `kbf-server`, a pure decision on two builds' seqs. No route calls it now. |
| C1 | Shipped daemons check `Welcome.protocol_version == PROTOCOL_VERSION`, which cannot be fixed in old daemons. | The N+1 frontend sends `Welcome.protocol_version = min(hello protocol, own)` (`negotiate`). The cross-version test runs a daemon built from the pinned commit of release N against the tip's frontend. |

**R1's input.** `GET /v1/nodes` and `GET /v1/members` each carry `oldest_protocol`, the least
`protocol` among the entries that name one, and `protocol_unknown`, how many name none; the
unknown are never folded into the oldest (`docs/api.md`). A daemon's `protocol` is the version
the `Welcome` of its newest stream named (the lesser of its first `Hello`'s and the newest the
server speaks) and stays after the stream closes, so `/v1/nodes` counts every daemon the process
has registered, connected or not. A member's is the version its registration named; a `down`
member's row has expired with its heartbeats and a row written before the field existed names
none, so both are unknown. `/v1/nodes` lists no daemon that has not connected to this process
(an empty list right after a restart is not "no nodes"; `/v1/members` is read from the store and
has no such gap).

Stated limits, written in the docs rather than enforced: the member-map floor is not set by an
operator. A fresh map starts at the oldest protocol the frontend accepts (`OLDEST_PROTOCOL`, not
its own newest `PROTOCOL`, which would lock N nodes out) and every later map carries
`max(stored floor, OLDEST_PROTOCOL)`; `register` judges a node against that floor, read from the
store, and not against a constant (tests: `a_build_as_shipped_floors_its_first_map_at_its_oldest_protocol`,
`members_floor`). Storage nodes do not enforce `Map::min_protocol` today. The floor never falls:
a frontend rolled back to a build with a lower oldest keeps the stored floor, which is the flag
day named next. Rollback across a protocol bump after nodes updated is a flag day (stop nodes,
downgrade, start).

**Cross-version test.** `kbf-cell cross-version` (`crates/kbf-integration-test/src/cross_version.rs`)
runs a frontend (`kbf-server`) from one directory of binaries and a daemon (`kbf-daemon --driver
fake`) from another over the real worker protocol (handshake, registration and heartbeats; no work
is offered), on loopback over mutual TLS. `--server-dir` and `--daemon-dir` each hold `kbf` and
their binary; `--installer-dir` names the release whose installer is on the hosts. The
`cross-version` job of `beta.yml` (a push to main, not the pull-request gate) builds the tip and
release N, the commit `OLD_COMMIT` names, and runs both directions: an N+1 frontend with N
daemons, and an N frontend with N+1 daemons, the installer of N+1 in both. A published release is
not needed to have an N: a build of a pinned commit is one. Its steps, in order, each bounded by
`--wait`:

1. Each directory's `kbf --version --json` states `min_protocol`, `protocol` and `state_schema`,
   and the `kbf-server` or `kbf-daemon` beside it names the same commit (a directory that mixes
   two builds fails). The builds must be compatible, as the handshake makes them: the server
   refuses a `Hello` below its `min_protocol` and answers any other with the lesser of the
   `Hello`'s version and its own `protocol`, and a daemon takes a `Welcome` from the oldest
   version it speaks up to the one it sent (`negotiate`, and `welcome::acceptable` in
   `kbf-daemon`). So the daemon's `protocol` must be at least the server's `min_protocol` (R1: a
   release that raises `min_protocol` past a shipped daemon fails here), and the server's
   `protocol` at least the daemon's `min_protocol`. A daemon newer than its frontend is
   compatible, and is welcomed at the frontend's newest. A server from before `negotiate` clamped
   a newer `Hello` refuses such a daemon: release N must be a commit that has it.
2. The installer goes first. The installer of `--installer-dir`'s release must be able to stage
   both builds: `kbf_installer::stage::understood`, the check the installer's staging applies,
   refuses a release that declares a `protocol`, `min_protocol` or `state_schema` above what the
   installer understands (`installer_too_old`). An installer never updates itself, so a kbf that
   raises one of the three can only follow its installer; the reverse order fails. What an
   installer understands is what the `kbf` of its release declares (the installer's `supported()`
   is raised with the release; `the_installer_understands_what_kbf_declares` pins that it is
   never below it). Without `--installer-dir` the order is not checked and the run says so.
3. A probe `Hello` over the same mutual TLS as a daemon: at the daemon's protocol it is welcomed
   as the lesser of that and the server's `protocol`; one above the server's `protocol` is
   welcomed as the server's `protocol`; one below its `min_protocol` is refused
   FAILED_PRECONDITION naming the version.
4. `kbf-daemon` logs `welcomed` (when it does not, the failure carries the `reason=` of the
   daemon's own log: the facts come from `kbf --version --json`, which reports the server's
   constants, so a daemon whose own range differs is told apart only there). The session is then
   held for eight heartbeat intervals (the server runs with `--heartbeat-interval-ms 250`): the
   daemon must stay up, and neither log may say a heartbeat went unacknowledged, the session
   ended, or a message did not parse (a heartbeat whose shape changed without a version bump
   passes the handshake and fails here).
5. `GET /v1/nodes` lists the node connected, at the session's protocol when the entry carries
   one.

While every build declares the same numbers (all 1 today) a pair cannot be incompatible, the
order cannot fail, and the lower edge of the window is protocol 0, which any server refuses: the
run says each is not exercised. The cell's own checks run in the pull-request gate against fakes
(`crates/kbf-integration-test/tests/cross_version.rs`). Not covered yet: any message after the
heartbeat (`Start`, `Result`, `NodeStatus`, `UpdateStatus`: the cell offers no work, so the fake
driver never runs an action), the real `kbf-installer` binary (an installer's numbers are
modelled by its release's `kbf --version --json`, and an old installer's `supported()` is assumed
equal to that), and the first-crossing case of a daemon whose certificate carries no role
(re-enrolment).

**First crossing.** The first update to a release with this layout cannot be a roll. It is a
cold cutover: stop every frontend of an earlier release, then start this one
([../deploy/state-store-cutover.md](../deploy/state-store-cutover.md), "Moving to frontends
that share the store"; the database is kept). A SQLite or fresh store is section 11.

## 11. Postgres and the shared state store

**Placement.** One Postgres from the OS package per frontend host, on a Unix socket only,
database `kbf` and role `kbf-server`, peer authentication (no password in argv or env), URL
`postgres:///kbf?host=/run/postgresql&user=kbf-server`. `synchronous_commit` stays on and the
store checks it at open. `kbf-installer` has `Suggests: postgresql`, not `Depends`: the package is
shared by node hosts, and apt installs `Recommends` by default; `setup --mode=frontend|all-in-one`
refuses clearly when no server answers. `setup --mode=frontend|all-in-one` with no `--database`
creates that role and database as the server's superuser over the socket (peer, no password;
`--create-local-database` does the same for a URL given); it never edits `pg_hba.conf` or
`pg_ident.conf`, prints the exact lines to add and refuses, naming the missing rule, when the
server does not admit a login (`docs/deploy/kbf-installer-setup.md`, "The state database").
Off-host Postgres (client TLS) is out of scope. A `sqlite://` URI still starts in this release
and logs a warning that SQLite is removed in the next; a SQLite file is held by one process, so
a second frontend over it is refused.

**Shared writers.** A frontend that opens a Postgres store calls `kbf_state::share`, which leaves
the row `meta/generation` holding the marker `shared-writers` (`kbf_state::SHARED_WRITERS`): an
absent-only write for a store with no row, a write conditional on the version read for a row that
holds a generation, nothing for a row that holds the marker, so frontends that open the store
together agree. The marker is not eight bytes, so a build that takes generations reads the row as
corrupt and refuses to start on the store. No frontend takes or checks a generation, and the
writes of a scheduler are kept apart by the rows they read. The move from generations to the
marker is a cold cutover: every frontend of an earlier release is stopped before the first one of
this release is started ([../deploy/state-store-cutover.md](../deploy/state-store-cutover.md#moving-to-frontends-that-share-the-store)).

**Epoch.** A frontend that starts keeps the stored epoch, and so does a second frontend over
the same store; only `POST /v1/epoch/bump` raises it (`docs/api.md`). Ids are reserved in blocks
in the store, so a restarted or second frontend never hands out an id another took.

**Rows.** The rows are documented where they are written: the scheduler's (`op/`, `queue/`,
`lease/`, `done/`, `cordon/` and the `meta/` counters among them) in
`crates/kbf-server/src/farm/durable.rs`, membership in `crates/kbf-server/src/members.rs`,
enrollment in `crates/kbf-enroll-registry`, and the history index (`ui/`) in `crates/kbf-history`.

**Schema.** `kbf boot-check`, the first `ExecStartPre=` of the frontend and all-in-one units,
brings the state database to the schema the build embeds (`state_schema` of `kbf --version
--json`) with expand-only migrations, each in its own transaction under one advisory lock, and
refuses the start when it cannot. A database already at that schema, or past it, is left alone.
Frontends of two builds run side by side during a roll, so release N+1 may only expand the
schema while N runs. Every lock wait ends after 30 seconds. The connection URL is never logged.

**Backup.** Postgres is a runtime dependency the operator provides, so its backup and restore are
the operator's: kbf ships no backup timer and no backup or restore verb. A host that an earlier
build set up with `kbf-backup.timer` has it disabled by `setup`.

**No state migration.** The state of a SQLite frontend is moved once, in a cold cutover with the
frontend stopped, by `kbf-state-copy` ([../deploy/state-store-cutover.md](../deploy/state-store-cutover.md)),
or the farm starts fresh on Postgres: cached blobs live on storage nodes and are unaffected. A
fresh store loses the member map and enrollment (nodes enroll again), cordons, the run history and
the `done/` rows; in-flight work is dropped, so a cutover is drain, then stop.
