# Updating an installed host

This page is about a host that is already set up with [`kbf-installer setup`](kbf-installer-setup.md)
(adoption of an older host is installing the kbf and kbf-proxy packages). There are three ways to move it to a new release:

| way | state in this build |
|---|---|
| by hand, with `kbf-deploy host install` and a restart | only a host of the earlier layout, whose units run `releases/current/kbf`; the `kbf` package's units run `/usr/bin/kbf` |
| `kbf-installer update --frontend`, through the proxy's update API | the host fetches from its `url` source; a refused order is reported late (see below) |
| the operating-system package, applied by `kbf-updater` | the way a host normally takes a release (see (c)) |

The first two use the same layout: each release in `releases/<commit>/` of the installer tree, the links
`releases/current` and `releases/previous`, and `state.json`. See
[host releases](host-releases.md) for the layout and what is trusted.

A farm whose hosts run the packages is rolled by [kbf-updater](kbf-updater.md), which also
drains, retires and restarts the frontends of a kbf-proxy pool one at a time.

## Upgrade the installer first (seq)

A kbf built from main at or after commit 0c650e54 prints `seq` in `--version --json`, and every
`kbf-installer` built before that commit refuses that output (it refused any field it did not know),
so it cannot stage such a kbf.

On each host, upgrade the `kbf-installer` package before staging such a kbf. After the change that
makes the installer ignore fields it does not know, a field a newer kbf adds no longer strands an
installer that has that change. An installer without it still refuses.

## (a) By hand

Put the release in a directory that holds `kbf` (and `kbf-daemon` if the release has one, and
`units/` if it has units), then, as root:

```text
sudo kbf-deploy host status
sudo kbf-deploy host install --commit <40 hex> --from <dir>            # dry run
sudo kbf-deploy host install --commit <40 hex> --from <dir> --apply
sudo systemctl restart <unit>
sudo kbf-deploy host status
```

- `host install` copies the files into `releases/<commit>/`, refuses a release directory that is
  there with other content, checks the sha256 of each copied binary against its source, points
  `previous` at the old `current` and `current` at the new release, and writes the state. Every
  `host` verb prints its plan and changes nothing without `--apply`, and `--apply` needs root.
  Installing the current release again changes no link. It refuses while a commit-confirm deploy is
  pending in the state.
- **No verb restarts a service.** After `install --apply` the units still run the old binary until
  you restart them. Restart the units that run `releases/current/kbf` (the mode units of the
  `kbf` package run `/usr/bin/kbf`, which `host install` does not change).
- `host status` prints the state and whether the `current` link matches it. It exits 0 when it does
  (or when there is neither), 1 when it does not, and 2 when the state cannot be read
  (`REFUSED: ...`). `--json` prints one object.
- `host rollback [--apply]` points `current` at the previous release and removes `previous`. It
  refuses when there is no previous release or its binary is gone. Restart the units again.
- `host gc [--keep N] [--apply]` removes release directories. It never removes the current, previous
  or staged release, or one that a running process executes from; `--keep N` also keeps the N newest
  others (default 0). If it cannot tell which releases run, it removes nothing and fails.

`kbf-installer` has the same verbs for a host it manages: `kbf-installer rollback --unit <unit>`,
`kbf-installer gc [--keep N]` (it keeps the newest other release by default) and `kbf-installer
status`. Like `host`, they change nothing without `--apply`. `rollback` takes `--unit` (or
`--launchd-label` on a Mac), because the installer restarts that service itself.

### Restarting a storage member

A storage node holds blobs, and a restart takes it out of reach for as long as it is down. Take it
out of routing first, so that writes move to its other owner at once and not after a heartbeat
times out. For a node that also runs actions (`storage-and-daemon`), drain it first:

```text
kbf cluster drain <node> --deadline-secs 600 --token-file /etc/kbf/api-token
```

That is `POST /v1/nodes/{node}:drain` of [the operator API](../api.md); the node is `drained` once
its leases have ended, and `draining` or `drain_paused` until then. Then plan the restart of the
member, with the id from `GET /v1/members`:

```text
curl -sS -X POST http://127.0.0.1:8982/v1/members/<id> \
    -H "Authorization: Bearer $(sudo cat /etc/kbf/api-token)" \
    -H 'Content-Type: application/json' \
    -d '{"state":"planned_down","for_secs":900}'
```

Run it on the frontend's host, where the API listens on `127.0.0.1:8982` unless `--api-listen` says
otherwise (a write passes only from a loopback peer with the token and a JSON content type). The
answer is `{"id": N, "state": "...", "generation": G}`. `planned_down` works only on an `active`
member (any other state is `409`) and `for_secs` is 1 to 86400, default 900. The member is `down`
in the map from that call, whatever its heartbeat does, until `for_secs` has passed. Then restart
its unit, and wait for it to register again: it is `active` when its index is whole, `catching_up`
while it rebuilds. `kbf cluster member` takes only `active` and `draining`, so use the request above
for `planned_down`. `{"state":"active"}` undoes a plan for a restart that did not happen.

**Take one storage member out at a time.** Blobs are kept on R owners, and R is 2 by default
([the architecture](../design/architecture-v2.md)). With two members out at once, a blob whose two
owners are exactly those two has no reachable copy, and reads of it fail until one returns. Wait
until the first member is `active` again before the next. The frontend has its own drain, `kbf
cluster drain --frontend`, for a restart that must not cut a build's stream
(`POST /v1/frontend/drain`, see [the operator API](../api.md)).

## (b) `kbf-installer update --frontend`

On a frontend host that runs behind the bundled proxy (`setup --proxy`), the proxy serves an update
API at `/v1/update/frontend`. `kbf-installer update` is its client. It swaps the host's frontend
and waits for the update to end.

```text
kbf-installer update --frontend --commit <40 hex> \
    --api 127.0.0.1:8980 --api-ca /etc/kbf/ca.pem --api-server-name frontend.example.invalid \
    [--token-file /etc/kbf/api-token] [--nodes] [--source fetch|staged] [--request-id ID] \
    [--handover-timeout-s 3600]
```

| flag | meaning |
|---|---|
| `--commit` | the release to update to |
| `--frontend` | required: update this host's frontend |
| `--api` | the proxy's public address as `host:port`; it must be a loopback address |
| `--api-ca`, `--api-server-name` | the CA the API's certificate is verified against, and the name it carries (the address's own IP when none is given) |
| `--token-file` | the operator token, a file of mode 0600 or 0400 (default `/etc/kbf/api-token`, which `setup` writes) |
| `--nodes` | recorded in the update; the swap sends the new frontend no order for the nodes either way, and the nodes take their release from the host's packages |
| `--source` | where the release comes from: `fetch` (the default) is the source the installer is configured with; `staged` is the release the host's package carries (`available` in `status.json`). A host whose `release_source` is `bundled` fetches nothing and refuses `fetch` (exit 3, `source_refused`); the proxy refuses `staged` with `--nodes` or a rollback (`source_refused`) and for a commit the package does not carry (`not_staged`), both exit 3 |
| `--request-id` | names this update, so a run repeated after a lost connection is the same update; random when not given |
| `--handover-timeout-s` | how long to wait for the update to end, the new frontend's bake included (default 3600) |

It needs no root and is refused when it comes through `sudo` (exit 2): run it as the account that
may read the token file. It prints one JSON summary line. The exit codes are 0 for done (and for a
frontend that already runs the commit), 1 for failed, rolled back, a timeout or an API that does not
answer, 2 for a usage error (`--source staged` with `--nodes` is one), 3 when the release is
incompatible or incomplete or when the host will not take the source (`source_refused`,
`not_staged`), and 4 when an update is open, an old child is still draining, or the installer is
busy.

The swap sends the new frontend no order for the nodes, so an update does not fail there. An
update record an earlier build of the proxy wrote can still name `nodes` as the failed step: the
frontend was swapped and serves, and the nodes were not told. `kbf-installer deploy` is refused on a
host whose `release_source` is `bundled` (exit 3, `source_refused`, before the bundle is read): it
can only fetch.

The API refuses a new update with `503` (`handover_unavailable`) when `update.json` is unreadable,
when the host has no `releases/` directory, when no frontend child was adopted (the proxy's start-up
hold is in force), or when the other slot is still running a process.

**What works, and what does not yet.** The API writes a `frontend_update` order, and the root half
(`kbf-installer run`) fetches, verifies and stages the release. The root half now has a release
fetcher: it reads `installer.toml` and `installer.conf` at each fetch and fetches from the `url`
source, the only source a frontend or all-in-one host can be set up with. The root half names the
proxy's order it refused (`status.json` carries `refused_order_id` beside the `refusal`), so an order that is
refused (a failed fetch, a failed check) ends the update failed at once, with the refusal's own
word as the reason. An installer from before that field names no order, and then the update waits out
the API's staging deadline (15 minutes by default) and ends failed with reason `timeout`; the cause
is in the `refusal` of `status.json`. One thing is missing:

- No test drives a fetch through the fetch child from `installer.toml`, so the first update on a
  host is the first proof that its `url` source, its trust files and its fetch user work. Keep the
  by-hand path (a) as the way back.

## (c) By package, with `kbf-updater`

A release reaches a host as an operating-system package. `kbf-updater` drains the host, installs the
package, restarts the units, waits until the host is healthy and undrains it; on a Mac it installs the
signed tarball instead. The frontend keeps no release store and no desired release: the routes
`GET /v1/update` and `POST /v1/update/{desired,pause,resume}` and the release routes answer `404`, and
`--releases-dir` is accepted, ignored and warned about (see [the operator API](../api.md)).

The floor on each host is unchanged: `installer.toml` carries `min_generation`, a host refuses an
order below it, and `setup --min-generation N` raises it (it never lowers it).
