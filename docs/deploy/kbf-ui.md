# The web UI: operator guide

The kbf server serves a read-only web UI under `/ui`. This page says what it is, how to
reach it, how to turn it off, what it exposes, and what each page shows. The design is
[kbf-ui-v1.md](../design/kbf-ui-v1.md); the route and header reference is "`GET /ui`: the
web UI" in [api.md](../api.md).

## What it is

- **Read-only.** Pages are server-rendered HTML with no script and no client toolchain. `GET`
  and `HEAD` are served; every other method is `405`. The UI has no button, form that
  writes, cookie or login. The operator API's write routes and their token are unchanged.
- **Open.** It asks for no certificate, no token and no login. Anyone who can reach the port
  gets every page.
- **On by default.** There is nothing to install or enable.
- **The same data as the API.** Every page reads only the `/v1` routes it declares, in
  process, and lists those routes (each a link to the JSON) at its foot. A number on a page
  is a number an agent can fetch from the API.

## Reaching it

- **Behind the proxy** (a `frontend` host set up with `--proxy`): open
  `https://<proxy address>:8980/ui`. The proxy's one public port (`8980` unless
  `kbf-proxy.socket` says otherwise) forwards `/ui` and `/ui/...` to the active frontend's
  operator API, so there is no second port to open. The proxy lets `GET` and `HEAD`
  through to the UI and answers any other method itself with `405`.
- **Without the proxy** (`frontend` or `all-in-one` mode): the UI is on the operator API's
  address, `--api-listen`. That flag is off unless given, and with it off there is
  nothing to serve the UI on. Bind it where only operators can reach it.
- **Some pages need history.** Targets and Target read the cross-build routes
  (`/v1/tests`, `/v1/grid`), and the Log page reads a run's log from history, and those
  routes do not exist unless the farm runs with `--history-objects=<URI>` (which needs
  `--state-store`); without it those pages answer `200` with a "History is off on this
  farm" page that names the flag. A record that is not held is still a `404`. The build pages
  work without it from the server's in-memory records, which a restart empties.

## Turning it off

- **At the proxy:** set `ui = false` in the `[proxy]` table of `/etc/kbf/installer.toml`, then
  restart `kbf-proxy.service` (the table is read once at start; SIGHUP reloads certificates
  only). `/ui` is then a `404` at the proxy. `kbf-installer setup` writes `ui = true` on a
  new proxy host and keeps a `ui = false` you set when it is run again.
- **At the server:** start `kbf` with `--ui=false`. Every `/ui` path is then a `404`.
  A plain `--ui` means true.

Turn it off at both places if the farm should serve no UI at all: the proxy setting stops the
route at the public port, the flag stops it at the server itself.

## What it exposes

Everything the pages read is shown **unredacted** to whoever reaches the port:

- the build list, each build's actions, targets, and failed-action command arguments and
  environment names;
- the end of each failed action's stdout and stderr, each attempt's logs on the Log page
  (the last 1 MiB, or the first with `?from=start`, with a note when it is cut), and a link
  that downloads the whole log;
- node names, software versions, capacity, leases and the queue, and storage members and
  their health.

This holds whatever the REAPI authentication policy says: the pages ask the policy-gated
build, log and blob routes in process with every caller allowed. The same routes asked over
the network keep their gates. Put the port behind your network boundary (a VPN, a private
subnet or a firewall rule), as you would for any page that shows build logs. If a build
prints a secret into a log, the UI shows it.

Every `/ui` answer carries a `Content-Security-Policy` of `default-src 'none'` with styles
and images from the page's own origin only (no page uses an image), `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`,
`Cache-Control: no-store`, `Cross-Origin-Resource-Policy: same-origin`,
`Cross-Origin-Opener-Policy: same-origin` and a `Permissions-Policy` that denies the powerful
browser features. The proxy leaves a forwarded answer's headers as the server set them. The answers the proxy
makes itself (the 503 handover page during a frontend swap, 404, 405, 401, the probes) carry
a stricter set: `Content-Security-Policy: default-src 'none'; frame-ancestors 'none';
base-uri 'none'; form-action 'none'`, `X-Content-Type-Options: nosniff`,
`Referrer-Policy: no-referrer`, `Cache-Control: no-store` and
`Cross-Origin-Resource-Policy: same-origin`. Nothing sets HSTS: a farm often serves a
private-CA or self-signed certificate, and HSTS would stop a browser from proceeding past
the certificate warning to the UI.

## The per-client read bound

Each caller may have at most 4 reads open at once on `/ui`, and 4 on the `/v1` read routes
the pages use. The fifth is `429` with `Retry-After: 1`, before its handler runs. A slot is
held until the answer has been sent, so a slow reader cannot hold more than 4. A path
longer than 2048 bytes or a query longer than 4096 is `414`.

A caller is identified by the client address the proxy forwards, so behind the proxy each
browser has its own bound. A server reached by a proxy that sends no client address, or
directly, bounds by the peer address: browsers behind one such address share one bound.

## The pages

Every path is under `/ui`. An opt-in `?refresh=N` (N is 5, 10, 30 or 60 seconds) reloads
Overview, Nodes and Node; any other value or parameter is a `400`.

| Page | Path | Shows |
|---|---|---|
| Overview | `/ui` | Readiness and failing checks, nodes by state, queue depth, leases held, storage summary, the ten latest builds |
| Builds | `/ui/builds` | Recent builds, filtered by state and instance, with a lookup of one build by id |
| Build | `/ui/builds/{id}` | One build's summary, its failed actions first, then its actions |
| Action | `/ui/builds/{id}/actions/{digest}` | One action; for a failed one, its failure class, command, exit code and log tails with links to the whole logs |
| Targets | `/ui/targets` | The tests the farm has run, each with a strip of its last 16 builds' outcomes |
| Target | `/ui/targets/{key}` | One test's recent builds as a pass/fail strip, each cell a link to its build |
| Nodes | `/ui/nodes` | Every node: connection, placement, phase, software, update state, leases, booked resources |
| Node | `/ui/nodes/{id}` | One node's capacity, booked and free resources, and leases |
| Leases | `/ui/leases` | What the scheduler holds now: node, phase, age, operation, platform and QoS; filter `?node=` |
| Queue | `/ui/queue` | Depth, oldest age, the queue per platform and QoS, and why operations wait or are kept off nodes |
| Storage | `/ui/storage` | Under-replicated blobs, each member's state, repair debts and open writes, and its write-tier flush backlog |
| Log | `/ui/runs/{run_id}/logs/{stream}` | One run's log (`stdout`, `stderr` or `result`) as escaped text, the last 1 MiB (`?from=start` for the first), with a note that names the end shown when it is cut, a link to the other end and a link that downloads the whole log; reached from the Action page |
| Updates | `/ui/updates` | The desired release, the releases, and each host's update state; the command that sets the release is shown as text only |

Nodes, leases and the queue live in the server's memory: after a restart or a frontend
swap they are empty until the daemons reconnect, and the pages say so, with the time the
server started and how long it has run (`started_unix_ms` and `uptime_ms` of
`GET /v1/health`; a page says the time is not known when the server does not send it).
A value the server
does not know is shown as "unknown" or "not measured", never as zero.
