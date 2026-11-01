# One host, all containers: the control side of a small farm

A small farm can run everything its control side needs on one machine: the state database, two
frontends, a load balancer in front of them, and the update coordinator, each as a container.
This page is that layout and the checks that show it is right. It does not install anything;
the images and how they are verified are in [containers.md](containers.md), the settings of a
frontend are in [kbf-modes.md](kbf-modes.md). Addresses below are documentation addresses
(`192.0.2.10` is the address the farm's nodes dial; replace it).

```text
nodes / Buck2 clients ──TLS──> 192.0.2.10:8980 REAPI    ┐
                               192.0.2.10:8981 worker   ├ Envoy (TCP passthrough, host network)
                               192.0.2.10:8444 enroll   ┘     │ each frontend in or out by /readyz (REAPI: /readyz/reapi)
                                       ┌──────────────────────┴───────────────┐
                              fe-a  127.0.0.1:1898x, 18444           fe-b  127.0.0.1:2898x, 28444
                                       └──── unix socket dir ───┬──────────────┘
                                                      Postgres (--network none)
```

## Postgres: no network, a socket directory

Run the official Postgres image, **pinned by digest** (a tag moves), as the user id 65532, with
no network at all:

```text
docker run --name kbf-postgres --network none --user 65532:65532 \
  -e PGDATA=/var/lib/postgresql/data/pgdata \
  -v /srv/kbf/postgres/data:/var/lib/postgresql/data \
  -v /srv/kbf/postgres/run:/var/run/postgresql \
  postgres@sha256:<digest> \
  -c listen_addresses= -c unix_socket_directories=/var/run/postgresql \
  -c synchronous_commit=on
```

`/srv/kbf/postgres/data` and `/srv/kbf/postgres/run` are owned by 65532 and mode 0700. The
`run` directory is where Postgres puts its socket; it is also mounted into each frontend at
`/run/postgresql`. With `listen_addresses` empty and no network, the socket is the only way in,
and only root and 65532 can enter the directory.

**That directory's mode is the access control.** kbf never sends a password to Postgres: a
`--state-store` (or `state-store =`) value that carries one is refused at the flag
(`crates/kbf-server/src/state_store.rs:102-108` the message, `:165-166` the refusal, `:182-184`
the test for a password in user information or a `password` parameter), the accepted form is a
socket URL reached by the process's own user (`:21-23`, `:175-176`), and the frontend has no
password file to read. The container's Postgres therefore has to let the socket's callers in
without a password; the official image does that when it is started with
`-e POSTGRES_HOST_AUTH_METHOD=trust`, which is safe here only because of the two facts above
(no network, a 0700 directory). Do not add either a published port or a looser mode.

Create the role and the database the frontends name, once, with `docker exec` as 65532. The
frontends' URL, in each copy of `kbf.toml`:

```toml
[scheduler]
state-store = "postgres:///kbf?host=/run/postgresql&user=kbf-server"
```

`user=` names the role (`crates/js-state/src/postgres/mod.rs` in the jobscheduler repository, at the revision `Cargo.toml` pins). A store that does not
flush commits is refused at the connection, so keep `synchronous_commit` on
(`crates/js-state/src/postgres/schema.rs`, the `synchronous_commit` check). The schema is brought up to date
by `kbf boot-check --mode=frontend`, which the units run before the start; in a container run it once per
image tag as a job ([containers.md](containers.md#the-frontend)).

## Two frontends

`fe-a` and `fe-b` are two containers of the same attested frontend image
([containers.md](containers.md#verify-and-import): verify and import it first). Each has:

- `--network host` and every listener on loopback, with ports of its own. A frontend binds
  `--listen` (REAPI), `--worker-listen`, `--api-listen` (the operator API; off unless given) and
  `--enroll-listen` (off unless given): `crates/kbf-cli/src/config.rs:206-216` lists the flags,
  [kbf-modes.md](kbf-modes.md) (lines 103-105) their defaults. A scheme that keeps the two apart:

  | | REAPI | worker | operator API | enroll |
  |---|---|---|---|---|
  | fe-a | `127.0.0.1:18980` | `127.0.0.1:18981` | `127.0.0.1:18982` | `127.0.0.1:18444` |
  | fe-b | `127.0.0.1:28980` | `127.0.0.1:28981` | `127.0.0.1:28982` | `127.0.0.1:28444` |

  On the host network the operator API at a loopback address accepts writes from a loopback
  peer (`docs/api.md:45`, [host-install.md](host-install.md#a-frontend-container)).
- **its own state directory**, mounted at `/var/lib/kbf-server` (65532, mode 0750), and its own
  copy of `kbf.toml`. The copies are the same except `state-store`, which is the socket URL
  above. The file must not be writable by group or other, or the start is refused
  (`crates/kbf-cli/src/config.rs:346-368`).
- **its own history objects directory**, if `--history-objects` is a `file://` directory. The
  file store takes an exclusive lock on `LOCK` in that directory when it opens
  (`crates/kbf-objstore/src/fs_store.rs:294-304`), so a second frontend pointed at the same
  directory fails to start. Give each frontend its own directory, or an object store that
  several processes may share.
- **65532-owned copies of the certificates, keys and the operator token** that `kbf.toml`
  names. The operator token (`api-token-file`) is also what the update drain presents. A token
  file must be owned by the server's user and be mode 0600 or 0400
  (`crates/kbf-server/src/token.rs:5-8`).
  Give the copies to 65532 with mode 0400; a shared certificate may be read by both frontends.
- **enrollment through the signer**, which holds the CA key outside the frontends. Run
  `kbf-signer` on the host with `--frontend-uid 65532 --client-group <group>`
  (`crates/kbf-signer/src/cli.rs:28-40`): the signer's socket directory becomes mode 0750, the
  socket 0660, the CA directory 0750 and `ca.pem` 0640, all of that group, and `ca.key` stays
  0600 (`crates/kbf-signer/src/lib.rs:13-17`, [api.md](../api.md) "The signer's socket"). The
  signer's account must be a member of the group, and a group that does not exist stops the
  start with nothing written (`crates/kbf-signer/src/cli.rs:49-56`). Each frontend
  container mounts the signer's socket directory and the CA directory read-only and runs with
  that group's numeric id as a supplementary group (`--group-add <gid>`), which is how a
  container reaches the socket (`docs/api.md` "A container reaches the socket by mounting the
  socket's directory and being run with that group's id as a supplementary group"). The
  frontend flags are `--enroll-listen`, `--ca-dir`, `--ca-signer=<socket>` and
  `--frontend-name=<the DNS name the nodes dial>` (`docs/api.md:847-855`); the name is in the
  enrollment listener's certificate, so it must be the name the load balancer answers to.

A frontend's command, for `fe-a`:

```text
docker run --name kbf-fe-a --network host --user 65532:65532 --group-add <signer-group-gid> \
  -v /srv/kbf/fe/etc:/etc/kbf:ro \
  -v /srv/kbf/fe/a:/var/lib/kbf-server \
  -v /srv/kbf/postgres/run:/run/postgresql \
  -v /run/kbf-signer:/run/kbf-signer:ro \
  -v /srv/kbf/ca:/etc/kbf-frontend-ca:ro \
  kbf-frontend:<commit> --config=/etc/kbf/kbf.toml \
  --listen=127.0.0.1:18980 --worker-listen=127.0.0.1:18981 \
  --api-listen=127.0.0.1:18982 --enroll-listen=127.0.0.1:18444 \
  --ca-dir=/etc/kbf-frontend-ca --ca-signer=/run/kbf-signer/signer.sock \
  --frontend-name=kbf.example.invalid --enroll-allow-unenforced-deny
```

`fe-b` is the same with its own state directory and ports. Both frontends open the one
database and write to it, which is what the Postgres store is for
(`crates/kbf-server/src/state_store.rs:21-25`). `--enroll-allow-unenforced-deny` is there because `--enroll-listen` is refused without it while
storage does not enforce the deny rows a node revocation writes (`DENY_ENFORCED` is false,
`crates/kbf-server/src/enroll/gate.rs:19-30`): a revoked node is still served by storage until
its certificate expires. Leave it out only when a build enforces them.

## Waiting for Postgres

The commands above can be run by hand, or by systemd as one unit per container. On a reboot
systemd starts Postgres and the frontends together, and a frontend that reaches the socket
before Postgres has made it cannot open its state store. Two things cover that, and both are
wanted.

- **The frontend waits.** For `--state-store-wait-secs` (default 300, 0 to 3600; key
  `state-store-wait-secs` in `[scheduler]`) it tries the store again after 250 ms, then after
  twice as long each time up to 10 s, and logs each attempt. It waits for connection refused or
  reset, a socket that is not there yet, a connect that times out and SQLSTATE 57P03 (starting
  up). A URL that does not parse, a refused login, a database or role that does not exist and
  a schema the build does not accept are not waited for: they exit 2 at once. A store that is
  still not up when the window ends exits **1**. The window is the time spent waiting between
  attempts, not the time inside each attempt, and the log gives the window and what remains of
  it at every attempt.
- **The unit waits too, and says when to stop.** An `ExecStartPre=` polls `pg_isready` inside
  the Postgres container, under `timeout` so that a Postgres that never comes up cannot hold the
  start for ever. `pg_isready` needs `-U` here: the container runs as uid 65532, which has no
  name for it to look up.

A unit for the Postgres container, and one for `fe-a` (`fe-b` is the same with its own name,
state directory and ports):

```text
# /etc/systemd/system/kbf-postgres.service (a sketch: the docker run arguments are the ones above)
[Unit]
Description=kbf state database (Postgres container)
After=docker.service
Requires=docker.service

[Service]
ExecStartPre=-/usr/bin/docker rm -f kbf-postgres
# The arguments of the docker run command above, with --rm.
ExecStart=/usr/bin/docker run --rm --name kbf-postgres --network none --user 65532:65532 ...
ExecStop=/usr/bin/docker stop kbf-postgres
Restart=always

[Install]
WantedBy=multi-user.target
```

```text
# /etc/systemd/system/kbf-fe-a.service (a sketch: the docker run arguments are the ones above)
[Unit]
Description=kbf frontend a
After=docker.service kbf-postgres.service kbf-signer.service
Requires=docker.service kbf-signer.service
Wants=kbf-postgres.service
StartLimitIntervalSec=0

[Service]
# Counts the ExecStartPre= below: systemd's default of 90 s would cut a 240 s wait short.
TimeoutStartSec=300
ExecStartPre=/usr/bin/timeout 240 /bin/sh -c 'until /usr/bin/docker exec kbf-postgres pg_isready -q -h /var/run/postgresql -U kbf-server -d kbf; do sleep 2; done'
ExecStartPre=-/usr/bin/docker rm -f kbf-fe-a
# The arguments of the docker run command for fe-a above, with --rm.
ExecStart=/usr/bin/docker run --rm --name kbf-fe-a --network host --user 65532:65532 ...
# A stop drains: the time must exceed --drain-lb-wait plus the longest action.
ExecStop=/usr/bin/docker stop --time 3600 kbf-fe-a
TimeoutStopSec=infinity
Restart=on-failure
RestartSec=5
# Exit 2 is a start the frontend refused (a flag, a certificate, a URL, a login, a schema):
# starting it again reads the same bytes. A store that stays down past --state-store-wait-secs
# exits 1, and is started again.
RestartPreventExitStatus=2

[Install]
WantedBy=multi-user.target
```

During the wait the listeners are bound but nothing answers on them, so health-check a frontend
with HTTP `/readyz`, not with a TCP connect. A stop signal sent during the wait is acted on when
the wait ends, so with `TimeoutStopSec=infinity` a stop can take up to `--state-store-wait-secs`.
A wrong socket path or a wrong local port in `--state-store` looks exactly like a Postgres that
is not up yet: the frontend waits the window, exits 1, is restarted, and the log names the
error. A Postgres that never comes back makes the frontend restart every window, visible as
exit 1 in the journal.

If the `timeout` ends the wait, the `ExecStartPre=` fails with status 124 and the unit is
restarted after `RestartSec`, as for any failure but exit 2. `RestartPreventExitStatus=2` stays:
it holds the refusals, which a restart cannot cure. `kbf-postgres.service` is pulled in with
`Wants=` and not `Requires=`, so that a restart of Postgres does not also restart both
frontends at once; each frontend finds its store gone through `/readyz` and Envoy takes it out.

**Order the signer before the frontend.** A frontend that serves enrollment through the signer
imports the CA root against the signer's public key at its start, through the socket of
`--ca-signer` and the `ca.pem` the signer made. A start that finds no signer is a refused start
(exit 2), which `RestartPreventExitStatus=2` does not retry, so `kbf-signer.service` (the
host's unit for the signer above) is in `After=` and `Requires=` of both frontends. **Envoy needs no ordering
against Postgres or the frontends:** it health-checks each frontend with `GET /readyz` and puts
it into the rotation when it answers 200, whenever that is.

## The load balancer: TCP passthrough

Envoy, **pinned by digest**, on the host network, listening on the address the farm dials. It
copies bytes: TLS ends at the frontends, so the certificates the nodes see are the frontends'
own and the worker listener's mutual TLS is untouched. It publishes three listeners, one
cluster each:

| Envoy listener | to the frontends' |
|---|---|
| `192.0.2.10:8980` | REAPI |
| `192.0.2.10:8981` | worker |
| `192.0.2.10:8444` | enroll |

Each cluster lists both frontends and health-checks each endpoint on **that
frontend's operator API port**: the worker and enroll clusters with `GET /readyz`, the REAPI
cluster with `GET /readyz/reapi`, which is `/readyz` and also needs a connected worker. The REAPI
cluster needs the stricter check because a frontend that restarted while all the nodes stayed
on the other one is ready but cannot place an action: its Executes would wait for as long as
it is up. The worker cluster cannot use it: a frontend with no worker gets its first one through
that cluster. The check is per frontend and counts workers of any platform, so a frontend with
only Linux workers still holds a macOS action; the cluster-wide answer to that is that every
frontend has a worker of each platform you build for. The port is the check's own
`health_check_config.port_value`, because the endpoint is the REAPI, worker or enroll port. A frontend is in the rotation while its check
answers 200 and out while it answers 503 (`docs/api.md:1273`). A drain turns `/readyz` to 503
first and serves on for `--drain-lb-wait` (`crates/kbf-cli/src/config.rs:68`,
`docs/api.md:590-632`), so a balancer that checks every second sees it before a stream is cut.
Set `close_connections_on_host_health_failure: false` so that a stream already open on a
draining frontend is left to finish, and an `idle_timeout` of `0s` on the proxies, since a
build's streams are long. A fragment, for the REAPI cluster (the others use `path: /readyz`):

```yaml
clusters:
  - name: reapi
    type: STATIC
    connect_timeout: 3s
    lb_policy: LEAST_REQUEST
    health_checks:
      - { timeout: 1s, interval: 1s, unhealthy_threshold: 2, healthy_threshold: 2,
          http_health_check: { path: /readyz/reapi } }
    close_connections_on_host_health_failure: false
    load_assignment:
      cluster_name: reapi
      endpoints:
        - lb_endpoints:
            - endpoint: { address: { socket_address: { address: 127.0.0.1, port_value: 18980 } },
                          health_check_config: { port_value: 18982 } }
            - endpoint: { address: { socket_address: { address: 127.0.0.1, port_value: 28980 } },
                          health_check_config: { port_value: 28982 } }
```

Check the file before it is used: `envoy --mode validate -c envoy.yaml` in the same image.

## The coordinator

The coordinator is one more container on the same host, from its own image: see
[containers.md](containers.md), "Run the coordinator on one host". Its heartbeat listener is
8443 and its control listener 8444; Envoy's enroll listener above is also 8444, so publish them
on different addresses or give the coordinator other ports through its flags.

## Checks, after the first start

1. `curl -s -o /dev/null -w '%{http_code}\n' http://127.0.0.1:18982/readyz` is 200, and the same
   for `28982`. A 503 names the failing check in its body (`docs/api.md:1273-1288`).
2. Envoy's admin port (loopback) lists both endpoints of every cluster as healthy.
3. `openssl s_client -connect 192.0.2.10:8980 -servername kbf.example.invalid -CAfile ca.pem`
   ends in `Verify return code: 0 (ok)` for 8980, 8981 and 8444: the certificates are the
   frontends'.
4. `GET /v1/enroll/tokens` on a frontend's operator API with the operator token answers 200,
   which shows enrollment is on and the state store is reachable.
5. Stop `fe-a`: `/readyz` of `fe-b` stays 200 and a build through `192.0.2.10:8980` goes on. Start
   it, and wait for Envoy to take it back before stopping the other.

## Updating a frontend

A frontend is updated by the host's helper:
`kbf-host-install --serve --frontend-container`, described in
[host-install.md](host-install.md#a-frontend-container). It fetches and checks the release's
image, drains the container through its operator API, and makes the container again from the new
image; a failed switch is put back at the deadline. **One frontend at a time, and never the last
one serving.** On this host:

- list the other frontend's operator API in `peers` (for `fe-a`, `127.0.0.1:28982`): a switch
  refuses with `peer_not_ready` unless every peer answers `/readyz` with 200
  (`crates/kbf-host-install/src/local/frontend/config.rs:97-99`, host-install.md step 1). That
  is the only guard on the host itself;
- set `network = "host"`, give no `ports`, and `api` is the frontend's loopback operator API
  (host-install.md, "On the host network");
- the coordinator orders frontends one at a time and never the last one that is up
  ([updates.md](../design/updates.md), "Never the last frontend"). It counts hosts that
  heartbeat, so two containers behind one agent are one frontend to it, and it will not order
  that host to update; use the helper by hand (`peers` is the guard) or run a frontend on a
  second host.

**What the helper can and cannot make.** `/etc/kbf/host-install-frontend.toml` describes one
container (`[frontend]`, `crates/kbf-host-install/src/local/frontend/config.rs:35,101-127`), and
the container it makes has only `--name`, `--restart`, `--network`, `--user`, `--publish`,
`--volume` and the command (`config.rs:65-86`). It has no `--group-add`: a container it makes
cannot reach a signer socket whose directory is 0750 for a group, so a frontend that serves
enrollment through the signer is not one the helper can remake as written above. The second
frontend on the host is not described by this file at all. Until that changes, the update of
these two is the operator's, one at a time, with the checks above between.
