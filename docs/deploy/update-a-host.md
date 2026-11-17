# Updating an installed host

This page is about a host that is already set up with [`kbf-installer setup`](kbf-installer-setup.md)
(adoption of an older host is installing the kbf and kbf-proxy packages). There are three ways to move it to a new release:

| way | state in this build |
|---|---|
| by hand, with `kbf-deploy host install` and a restart | only a host of the earlier layout, whose units run `releases/current/kbf`; the `kbf` package's units run `/usr/bin/kbf` |
| `kbf-installer update --frontend`, through the proxy's update API | removed, with the proxy's supervision of frontend children (see (b)) |
| the operating-system package, applied by `kbf-updater` | the way a host normally takes a release (see (c)) |

The by-hand way uses this layout: each release in `releases/<commit>/` of the installer tree, the links
`releases/current` and `releases/previous`, and `state.json`. See
[host releases](host-releases.md) for the layout and what is trusted.

A farm whose hosts run the packages is rolled by [kbf-updater](kbf-updater.md), which also
drains, installs and restarts the frontend members of a pool one at a time, through each
member's own operator API.

Frontends in containers behind a load balancer are updated by draining one at a time:
[frontends-behind-a-load-balancer.md](frontends-behind-a-load-balancer.md).

## Upgrade the installer first (seq)

A kbf built from main at or after commit 0c650e54 prints `seq` in `--version --json`, and every
`kbf-installer` built before that commit refuses that output (it refused any field it did not know),
so it cannot stage such a kbf.

On each host, upgrade the `kbf-installer` package before staging such a kbf. After the change that
makes the installer ignore fields it does not know, a field a newer kbf adds no longer strands an
installer that has that change. An installer without it still refuses.

## (a) By hand

Put the release in a directory that holds `kbf` (and `units/` if it has units), then, as root:

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
  (`REFUSED: ...`). `--json` prints one object. Both list the releases the host keeps, each with
  its commit, its version and why it stays.
- `host rollback [--apply]` points `current` at the previous release and removes `previous`. It
  refuses when there is no previous release or its binary is gone. Restart the units again.
- `host gc [--keep N] [--apply]` removes release directories. It never removes the current, previous
  or staged release, one that a running process executes from, or one that `retain.json` pins or
  names as the coordinator's last good; `--keep N` also keeps the N newest others (2 or more, default
  4). It removes nothing while a swap is on trial. If it cannot tell which releases run, it removes
  nothing and fails.

`kbf-installer` has the same verbs for a host it manages: `kbf-installer rollback --unit <unit>`,
`kbf-installer gc [--keep N]` (the same rule: 2 or more, default 4) and `kbf-installer
status`. Like `host`, they change nothing without `--apply`. `rollback` takes `--unit`, because the installer restarts that service itself.

### Restarting a storage member

A storage node holds blobs, and a restart takes it out of reach for as long as it is down. Take it
out of routing first, so that writes move to its other owner at once and not after a heartbeat
times out. Ask first whether the host may stop, and plan the restart of the member, with the
id from `GET /v1/members`:

```text
kbf cluster safe-to-stop <member id> --token-file /etc/kbf/api-token
kbf cluster member <member id> --state planned-down --for-secs 900 --token-file /etc/kbf/api-token
```

Run them on the frontend's host, where the API listens on `127.0.0.1:8982` unless `--api` says
otherwise (a write passes only from a loopback peer with the token and a JSON content type).
`safe-to-stop` exits 0 when the host may stop and 3, with the reasons, when not. The second is
`POST /v1/members/{id}` of [the operator API](../api.md) with `planned_down`, which works only on
an `active` member (any other state is `409`); `--for-secs` is 1 to 86400, default 900. The member
is `down` in the map from that call, whatever its heartbeat does, until `--for-secs` has passed.
Then restart its unit, and wait for it to register again: it is `active` when its index is whole,
`catching_up` while it rebuilds. `kbf cluster member <member id> --state active` undoes a plan for a
restart that did not happen.

**Take one storage member out at a time.** Blobs are kept on R owners, and R is 2 by default
([the architecture](../design/architecture-v2.md)). With two members out at once, a blob whose two
owners are exactly those two has no reachable copy, and reads of it fail until one returns. Wait
until the first member is `active` again before the next. The frontend has its own drain, `kbf
cluster drain --frontend`, for a restart that must not cut a build's stream
(`POST /v1/frontend/drain`, see [the operator API](../api.md)).

## (b) `kbf-installer update --frontend` (removed)

The proxy no longer serves an update API: `/v1/update/frontend` answers `503`, and there is no
`kbf-installer update` client. The two-slot supervision it belonged to was removed with it: the
proxy starts, adopts and swaps no frontend child, it writes no `update.json`, `boot.json` or
`releases.json`, and a `[proxy]` table with the `upstream` table is refused at start (exit 2).
`kbf-installer proxy` keeps `--kbf-binary`, `--update-dir` and `--orders-dir` as flags that are
accepted, ignored and warned about, so that a unit written before the removal still starts. A
frontend host behind the proxy runs a pool of frontends that run on their own
(`[[proxy.frontend]]` tables), and a pool is rolled as in (c).

## (c) By package, with `kbf-updater`

A release reaches a host as an operating-system package. `kbf-updater` installs the
package, restarts the units and waits until the host is healthy. On a Linux storage host the install is done by `kbf-host-install --serve --layout deb` (or `rpm`),
which fetches the release's attested package, checks it offline, installs it with the package manager,
restarts the units, and puts the previous package back if the new one is not confirmed by its deadline
([host-install.md](host-install.md#on-a-linux-storage-host-that-runs-the-package)). A host set up by
hand moves to the package once ([native-package-migration.md](native-package-migration.md)). The frontend keeps no release store and no desired release: the routes
`GET /v1/update` and `POST /v1/update/{desired,pause,resume}` and the release routes answer `404`, and
`--releases-dir` is accepted, ignored and warned about (see [the operator API](../api.md)).

The floor on each host is unchanged: `installer.toml` carries `min_generation`, a host refuses an
order below it, and `setup --min-generation N` raises it (it never lowers it).
