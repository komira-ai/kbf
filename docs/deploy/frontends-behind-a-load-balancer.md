# Running several frontends behind a load balancer

The recommended deployment of a kbf farm is a load balancer in front of several
`frontend` containers, build nodes on bare metal, and one object store as the cold
store. This page describes that shape, how a frontend is replaced without failing a
build, and the two example files that go with it:
[examples/deploy/envoy/envoy.yaml](../../examples/deploy/envoy/envoy.yaml) and
[examples/deploy/compose/docker-compose.yml](../../examples/deploy/compose/docker-compose.yml).

The page says what the code does today and marks what is a target. Where it names a
flag, a port or an endpoint, `kbf --mode=frontend --help` and
[kbf-modes.md](kbf-modes.md) are the references.

The frontend image is a release asset (see [docs/artifacts.md](../artifacts.md) for the
frontend image release asset): load it with `docker image load`, and it is tagged
`kbf-frontend:<commit>`. The compose example names it by `KBF_COMMIT`. Until a release
carries it, build the image yourself from the `kbf` binary with
`packaging/container/frontend/Dockerfile`.

## The topology

```text
   Buck2 clients                        build nodes (bare metal)
        |                               daemon, or storage-and-daemon
        | REAPI over TLS   (8980)             |
        v                                      | worker session, mutual TLS (8981)
 +-------------------------------+            |
 |        load balancer          |<-----------+   storage nodes register here too
 |   TCP passthrough, health     |
 |   checks on /readyz           |
 +---------+-----------+---------+
           |           |
           v           v
     +-----------+ +-----------+
     | frontend  | | frontend  |   `kbf --mode=frontend`, one container each
     |    A      | |    B      |   (operator API :8982, reached by the LB for health only)
     +-----+-----+ +-----+-----+
           |             |
           +------+------+
                  |
        +---------+----------+                   +----------------------+
        |  Postgres          |                   |  object store (S3    |
        |  the state store   |                   |  or compatible):     |
        |  (--state-store)   |                   |  the cold store      |
        +--------------------+                   +----------+-----------+
                                                            ^
   frontends -- mutual TLS --> storage nodes ---------------+
                               (blobs: hot store on local disk, flush to the cold store)
```

What runs where:

- **Frontends** run `kbf --mode=frontend`: REAPI, the scheduler, the worker listener and
  the operator API. A frontend holds no blobs and refuses every store flag. It keeps its
  scheduler state in the Postgres state store, and it passes cache calls on to the
  storage nodes (`--storage` is required; with the storage client flags it dials them
  over mutual TLS).
- **Build nodes** run `kbf --mode=daemon` or `--mode=storage-and-daemon` on bare metal.
  Each dials the load balancer's address, never a frontend's.
- **Storage nodes** (`storage`, or `storage-and-daemon`) hold the blobs on local disk and
  flush to the cold store, which is one object store the whole farm shares. The cold
  store is configured on the storage nodes with `--cold-store`
  ([storage-node.md](storage-node.md)); the frontend does not set it.
- **Postgres** holds the scheduler's state. All frontends open the same database.

## Why several frontends

- **Live update.** A frontend that stops drains first (below), but a single frontend
  that is restarted is not listening for a while, and a client that connects meanwhile
  is refused. With several frontends the load balancer stops sending new connections to
  the one being replaced, and the others keep serving.
- **Availability.** A frontend that crashes or is replaced takes only the connections
  it held. The queue and the leases are in Postgres, so the others carry on, and a
  node that was talking to the lost frontend reconnects to the load balancer and lands
  on another.

Buck2 does not recover an Execute stream that breaks after it started, so a frontend is
drained and left to finish its open streams before it exits. Several frontends make that
wait invisible to new clients; they do not make a cut stream recoverable.

There is no in-process proxy or swap in this shape. The load balancer and the frontend's
own drain do the work.

## What the frontends share

**One Postgres state store.** Give every frontend the same `--state-store` (the
`state-store` key of the `[scheduler]` table of `kbf.toml`), a `postgres://` URL.
Several frontends over one store are supported on `main` today:

- A frontend that opens a Postgres store takes no writer generation. It makes sure the
  store's `meta/generation` row holds the marker `shared-writers`
  (`crates/js-state/src/shared.rs` in the jobscheduler repository, `share`; called from `StateStore::open` in
  `crates/kbf-server/src/state_store.rs`), and any number of frontends can then open the
  database and serve at once; none fences another. Writes are conditional on the rows
  read.
- A frontend started with a SQLite state file is held by one process: a second frontend
  on the same file is refused. Postgres is what several frontends need.
- The first start of a frontend of this release on a database that an older frontend
  used replaces the generation row with the marker. That is a cold cutover; the order
  to do it in is in [state-store-cutover.md](state-store-cutover.md#moving-to-frontends-that-share-the-store).
  A new database needs nothing.
- Frontends keep the farm's epoch across a start, and reserve blocks of operation ids
  and lease sequences in the store, so a second frontend never hands out an id another
  took ([kbf-units.md](kbf-units.md), "The epoch, and a restore from a backup").

The Postgres URL carries no password (kbf refuses one) and the client does not speak
TLS to Postgres (an `sslmode=require` URL is refused). Reach the database by a local
socket, or over a network on which only the frontends can reach it, with the server's
role authorized by address.

**One object store.** The cold store is the storage nodes' (`--cold-store`). The
frontends do not open it for blobs. If you run with build history, `--history-objects`
(`s3://bucket/prefix?...`, [api.md](../api.md)) is an object store a frontend writes to;
give every frontend the same one.

**The same certificates' names, not the same keys.** Every frontend serves the REAPI and
worker listeners under the farm's one DNS name (the load balancer's), so the certificate
each loads must cover that name. Frontends may share a certificate and key or have one
each; the listeners reread their files every `--tls-reload-interval-secs` (default 30).

## Ports and what the load balancer does with them

| port (default) | listener | flag | through the load balancer |
|---|---|---|---|
| 8980 | REAPI (Buck2) | `--listen` | yes, TCP passthrough in the example |
| 8981 | worker listener (daemons, storage nodes) | `--worker-listen` | yes, TCP passthrough: mutual TLS must reach the frontend |
| 8982 | operator API, `/healthz`, `/readyz` | `--api-listen` (off unless given) | health checks only; do not publish it |

The defaults of `--listen` and `--worker-listen` are loopback; in a container set them to
`0.0.0.0:8980` and `0.0.0.0:8981`, and set `--api-listen` (the installer's own default is
`127.0.0.1:8982`, but a health check from another container needs a reachable address).

The operator API answers reads (including `/readyz`) to anyone who can reach it, and a
write must come from a loopback peer and carry the operator token
([api.md](../api.md#who-may-write)). So keep it on the frontends' private network, give
the load balancer a route to it for health checks only, and run operator verbs
(`kbf cluster ...`) inside the frontend's own container or host, where the peer is
loopback.

## Health checks

Use `GET /readyz` on the operator API (`--api-listen`). It answers `200` when the
frontend is ready and `503` when it is not, with a body that lists each failing check
([api.md](../api.md#get-healthz-and-get-readyz)). The checks are `stopping`, `leader`
and `store`.

The REAPI cluster asks `GET /readyz/reapi` instead: `/readyz` and, in addition, at least one
connected worker (check `no_worker`). A frontend that restarted while every worker stayed on
the other one would otherwise take Execute calls it cannot place, and they would wait for as
long as it is up. The worker and enrollment clusters keep `/readyz`, since a frontend gets its
first worker through them.

**Readiness drops during a drain.** From the moment a frontend receives SIGTERM or
SIGINT, or `POST /v1/frontend/drain` is called, `/readyz` answers `503` (`stopping`) and
stays so until the process exits. At the same time the frontend refuses new Execute
calls, sends GOAWAY on the REAPI listener, and keeps every open Execute and
WaitExecution stream until its action is done. The load balancer's active health check
sees the `503` and stops sending it new connections; connections already open are not
cut by the check.

`/healthz` answers `200` for as long as the process runs, a stop included. It tells a
liveness probe that the process is up, not that it should receive traffic; do not use it
as the load balancer's check. kbf has no `/livez`.

Each `/readyz` makes one read of the store, so poll at an interval of a second or more,
not faster.

## TLS

Where TLS ends is set per listener.

- **REAPI (8980).** Without `--reapi-tls-cert` and `--reapi-tls-key` the listener serves
  plain text. With them it serves TLS with the server certificate only; clients present
  no certificate. Two ways to run it behind a load balancer:
  - *Passthrough* (the example): the load balancer proxies TCP and the frontend ends TLS
    with a certificate for the farm's name. Buck2 then verifies the frontend's
    certificate (`tls = true`, `tls_ca_certs`, [guides/examples.md](../guides/examples.md)).
  - *Terminate at the load balancer*: the load balancer ends TLS for the farm's name
    and speaks HTTP/2 to the frontends (plain text on a private network, or TLS with
    the frontends' own certificate). It must pass gRPC streams without a total timeout,
    since an Execute stream stays open as long as its action runs. The example does not
    do this.
- **Worker listener (8981).** With `--worker-tls-cert`, `--worker-tls-key` and
  `--worker-client-ca` the listener demands a client certificate that chains to the
  CA, and reads the role (`urn:kbf:role:daemon` or `urn:kbf:role:storage`) from it. A
  load balancer that terminated this TLS would hide the client certificate from the
  frontend, and every call would be refused. **The load balancer must pass this port
  through as TCP.** The daemon verifies the frontend's certificate against `--ca-cert`
  and the host of the `--server` URL (or `--tls-server-name`).
- **Operator API (8982).** Plain HTTP. Keep it off the public side.

A passthrough balancer balances connections, not streams: all of one Buck2 connection's
actions go to the frontend it landed on. Daemons hold one persistent session each, so
they spread by connection too. If connections stay skewed, the REAPI port is the one to
move to an HTTP/2 balancer.

A frontend sees the load balancer's address as the peer of every call, not the client's.

## A live update, step by step

For frontends A and B behind the load balancer. Update them one at a time.

1. **Check the other frontend is ready.** `GET /readyz` on B answers `200`. Never start
   when the one that stays up is not ready.
2. **Drain A.** Either send SIGTERM to the process (what a container runtime's stop
   does), or call the drain from inside A's container:
   `kbf cluster drain --frontend --token-file <file>`
   (`POST /v1/frontend/drain`, [api.md](../api.md)). A's `/readyz` turns `503`.
3. **The load balancer takes A out.** Its health check fails A (the example marks it
   unhealthy after two failed checks, one second apart), and new connections go to B.
   A sends GOAWAY on REAPI, so it takes no new connection from a client.
4. **A waits for its streams.** It keeps every open Execute stream until the action is
   done and exits 0 when the REAPI connections have closed. The worker listener and the
   operator API stay up until then, so daemons go on reporting results. With
   `--shutdown-timeout-secs` at its default of 0 there is no deadline. Give the
   container a stop grace period at least as long as the longest action: a runtime that
   kills the container after a few seconds (the compose default is 10) ends the streams
   and fails the actions. A second SIGTERM also ends them.
5. **Replace A.** Start A from the new image with the same `kbf.toml`. It opens the
   shared Postgres store, finds the `shared-writers` marker, and serves.
6. **Wait for ready.** A's `/readyz` answers `200`, and the load balancer's check
   passes it back in.
7. **Next.** Repeat from step 1 for B.

While the old and the new frontend run side by side they read one store, so a release
changes the store's schema in two steps (expand first, contract a release later); the
upgrade order (store, storage one at a time, frontends, daemons) is in
[architecture-v2.md](../design/architecture-v2.md). A node that dropped its session
to A reconnects through the load balancer and keeps the runs of the leases it holds,
because a start does not change the epoch.

**Target, not built yet:** a rollout tool that runs these steps (drain, wait for exit,
replace, wait for ready, next) for the container deployment. Today the steps are the
orchestrator's, or an operator's.

## The single frontend

One frontend works: run it with `--state-store` and without a load balancer, or with
one in front. It has no live update. A restart is a stop, a gap, and a start; the stop
drains, so open streams finish, but from the first SIGTERM the frontend refuses new
Execute calls (Buck2 retries those when it opens a call), and once it exits nothing
listens until the new process is up: a client that connects in that gap is refused.
Nodes reconnect (`--reconnect-ms`,
`--reconnect-max-ms`) and lose no state, because it is in Postgres. Pick a quiet time,
drain first, and keep the gap short. A single frontend is also a scheduling single point
of failure: running actions finish, but nothing new is scheduled while it is down.

Adding the second frontend later changes no state: start it with the same `--state-store`
and put both behind the load balancer.

## Bare-metal nodes dial the load balancer

Point every node at the farm's one name:

- a daemon: `--server https://<farm name>:8981` (repeatable), with `--ca-cert`,
  `--cert`, `--key`; `--tls-server-name` when the name in the URL is not the one in the
  frontend's certificate;
- a storage node: `--frontend https://<farm name>:8981`, and `--advertise-addr` for its
  own address, which the frontends and the other nodes dial directly;
- Buck2: `engine_address`, `action_cache_address` and `cas_address` all
  `grpc://<farm name>:8980` with `tls = true` ([guides/examples.md](../guides/examples.md)).

Storage nodes are not behind the load balancer: a frontend and a daemon dial each
storage node at the address it registered, over mutual TLS. The frontend reaches the
first one by `--storage`, the bootstrap node, and takes the rest from the farm's
storage map.

A daemon opens a persistent session to whichever frontend the balancer picks. When that
frontend drains, the session ends after its work is reported and the daemon dials the
balancer again.

## The examples

- [examples/deploy/envoy/envoy.yaml](../../examples/deploy/envoy/envoy.yaml): Envoy with
  two frontend upstreams, TCP passthrough for REAPI and the worker port, an active
  `/readyz` health check, and outlier detection.
- [examples/deploy/compose/docker-compose.yml](../../examples/deploy/compose/docker-compose.yml):
  Envoy, two frontends and a Postgres. The object store, the storage nodes and the
  daemons are outside it. Each frontend mounts its own `kbf.toml`
  (`frontend-a.toml`, `frontend-b.toml`) with placeholder values.

Neither is a production configuration. They show the wiring; the certificates, the
tokens and the addresses are yours.
