# kbf-updater: rolling a version over a farm

`kbf-updater` rolls one version of the `kbf` package over the hosts of a farm, one host at a
time, behind a local socket. It runs on the frontend host. It installs through the
forced command of [`kbf-host-install`](host-install.md) over ssh, asks the frontend's loopback
operator API ([the API](../api.md)) whether a host may be stopped and whether it runs the target,
and writes its progress to `roll.json` in its state directory before each step, so a restart
of the updater finds a roll where it was.

## The inventory

`--inventory` names a root-owned file (and a root-owned directory), one host per line:

```text
# alias  address               role        host key (type and base64)                 [node id]
fe-a     fe.farm.example       frontend    ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample  instance=a base=18000
fe-b     fe.farm.example       frontend    ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample  instance=b base=18010
st1      st1.farm.example      storage     ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample
dm1      dm1.farm.example      daemon      ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample  dm1
```

The roles are `frontend`, `storage`, `daemon` and `mac-daemon`. A daemon host lists the node id
of its kbf configuration as the sixth word.

### A frontend that is a member of a pool

A `frontend` line with two more words after the key is one member of a pool of frontends that
sit behind a load balancer of the farm's own (anything that stops sending to a backend whose
`/readyz` answers 503):

| word | meaning |
|---|---|
| `instance=<name>` | the name of the member and the unit instance the host restarts for it: `kbf-frontend@<name>.service`. `-` is the plain unit `kbf-frontend.service`. Otherwise `[a-z][a-z0-9-]{0,31}` |
| `base=<port>` | the first of the member's five ports, 1024 to 65531. The member's operator API is at `base` plus 2 on loopback |

- There is one line per member, and every line lists the same address and host key: every
  member runs on the host the updater runs on, and the drain, the readiness and the commit go to
  the member on loopback while the install and the restart go over ssh to the address, so a
  member that lists another address or host key than an earlier one is refused.
- No two members share an `instance`, and their bases are at least 5 apart (a member uses
  `base` to `base` plus 4), or the file is refused.
- A pool of members and a `frontend` line without these words cannot be in one file, and a
  second `frontend` line without them is refused. A `frontend` line without them is not in a
  pool, nothing can move traffic away from it, and a roll that reaches it fails there
  (`swap_abandoned`) before any ssh call.
- `instance=` and `base=` are accepted on a `frontend` line only.

## The flags

All are flags; none is read from the environment.

| flag | |
|---|---|
| `--state-dir`, `--inventory`, `--proxy-uid`, `--token-file`, `--ssh-key-file`, `--ssh-user`, `--channel` | required |
| `--operator-api http://<loopback ip>:<port>` | required: the frontend's operator API, for safe-to-stop and the health of storage and daemon hosts. Not a loopback address is refused |
| `--socket` | the roll API's socket, for tests and development; in production systemd passes it |
| `--ssh-connect-timeout-secs`, `--ssh-command-timeout-secs`, `--apply-poll-secs`, `--apply-deadline-secs` | optional bounds in seconds; they default to 10, 900, 5 and 600 |
| `--drain-deadline-secs` | optional, 1 or more with no upper bound, absent by default. A daemon host's node is drained before its install and its leases are waited for however long they take. With the flag, a deadline that passes stops the waiting only: nothing is installed, the roll stops at the host with `drain_failed` and the node left drained (see [A daemon host's drain](#a-daemon-hosts-drain)). 0 is refused |
| `--peer-ready-secs`, `--frontend-drain-deadline-secs` | `--peer-ready-secs`: optional, at least 1 (default 30), how long every other member must answer `/readyz` 200 in a row before a member is drained. `--frontend-drain-deadline-secs`: optional, 1 or more with no upper bound, absent by default; a member that was told to drain is waited for until it has exited, however long its executions run. With the flag, a deadline that passes fails the roll with the member left draining. 0 is refused |

The updater reaches each member's operator API on loopback at `127.0.0.1:<base + 2>`, with the
same token (`--token-file`) as `--operator-api`.

## A daemon host's drain

A daemon host's node is drained at the frontend (`POST /v1/nodes/{id}:drain`) before its install,
and the roll reads the node's leases every poll until there are none, however long that takes: the
install restarts the daemon, which would cut the leases it still holds, so it waits. Without
`--drain-deadline-secs` the drain asks the frontend for no deadline of its own. With the flag, a
deadline that passes with a lease still held stops the waiting and nothing else: the host is not
installed, the roll stops at it with `drain_failed` ("`dm still holds N leases after the drain
deadline of S s; nothing was installed and the leases run on; ...; node dm-node (dm) is left
drained`"), the node stays drained (cordoned) and its leases run on. To try again, start another
roll: it drains the node again (a drain of a drained node does nothing) and waits again. To give
the node back without installing, uncordon it (`POST /v1/nodes/{id}:uncordon`, or `kbf cluster`).
(A host run by the [agent](#the-agent-mode) keeps a mark of the cordons the agent makes; a cordon
given back by hand is noticed by the agent, see `--cordon-dir`.)
The node drain at the frontend is the frontend's cordon and is not changed by this.

The SSH roll gives back only a node whose cordon carries the roll's mark, and the frontend's cordon
has no owner, so two limits follow; kbf#2166 (the cordon-owner issue) tracks the fix:

- An operator who drains a node during the roll, after the roll's mark is written, has that drain
  lifted at the give-back: the mark says the cordon is the roll's, and the frontend cannot tell two
  drains apart.
- When the frontend does not know the node yet at the drain, the roll drains it with no mark, and
  the node is left out after the roll. The roll logs one warn line for it (`<node>: left out after
  the roll: its cordon is not marked as this roll's ...; uncordon it by hand if it should serve`),
  as it does for a node an operator drained before the roll; there is no other alert.

## What a roll does with a frontend pool member

A farm whose frontends sit behind a load balancer of its own (Envoy, a cloud balancer, anything
that stops sending to a backend whose `/readyz` answers 503) is rolled by the member's own drain.
The load balancer is never asked: the member says it is not ready, and the balancer, whatever it
is, stops sending to it. A roll takes the members before every other host, one at a time, and the
turn of a member is:

1. **Check** (`pending`). Every other member of the inventory is asked `GET /readyz` on its own
   operator port (`base` plus 2) every 5 seconds. Each must answer 200 every time for
   `--peer-ready-secs` (30) in a row; an answer of 503, and a member that cannot be asked, start
   the window again. If the window has not been had `--peer-ready-secs` plus 300 seconds after the
   first ask, the roll fails with `unsafe_to_stop` and nothing has been changed. A member that is
   the only one in the inventory has no other to serve and is refused the same way. The member
   itself is not asked.
2. **Drain** (`draining`). `POST /v1/frontend/drain` on the member's operator API. The frontend
   then runs its own drain, which does not depend on the balancer: `/readyz` 503, no new work and
   GOAWAY on its listeners, a wait for the executions open on it, and its exit. The roll waits
   until the member's operator API refuses the connection (it has exited) or its
   `GET /v1/frontend/drain` reports `"phase": "closing"` (the streams that are left are being
   closed and the process exits within seconds; a status with another phase, or none, is taken
   to be still draining), however long that takes, or until `--frontend-drain-deadline-secs`,
   if it was given, is out (`drain_failed`). A member that answers the status but says it is not draining fails the roll
   at once (`drain_failed`). A member that is not running refuses the connection of the drain
   itself, has nothing to drain, and the turn goes on; a drain that gets no answer (a timeout)
   fails the roll with `frontend member a (fe-a) may be draining`.
3. **Install** (`installing`). The package is installed on the host through `kbf-host-install`,
   unless the host already has the target. An install the host reports as in progress is waited
   for, for at most 900 seconds (`install_failed`), and not run a second time beside it. The
   package covers every member of the host, so this happens once per host, at the first member.
4. **Restart** (`restarting`). `restart frontend=<instance>` through `kbf-host-install`, which
   starts the new version (`restart_failed`).
5. **Ready** (`waiting_healthy`). The roll waits until the member's own `GET /readyz` answers 200
   and its `GET /healthz` reports a `commit` that the target's version names, for at most 300
   seconds (`health_timeout`; `swap_abandoned` if the version names no commit).

Nothing is put back at the end: a drained frontend has exited, and the restart made a new process,
which is ready or is not. The next member is rolled only when this one is ready, which is also
what the next check asks of it.

**The bound.** There is none unless `--frontend-drain-deadline-secs` is given: the roll waits for
the member to exit however long the executions open on it run, and cuts none of them. The
frontend in this tree puts no bound of its own on a drain that its endpoint began
(`--shutdown-timeout-secs` bounds only a drain that a signal began). With the flag, a drain that
outlasts the deadline fails the roll with `drain_failed` and `frontend member a (fe-a) is left
draining`: the member goes on draining and exits by itself, and a new roll finds it stopped (its
connection is refused) and goes on from the install. A deadline shorter than the longest
execution a member may have open fails every roll at that member until the execution ends.

**A member that is stopped, or a wrong `base`.** A member whose operator API refuses the
connection of the first drain is taken for one that an earlier roll left stopped, and is installed
on and restarted. A `base` in the inventory that is not the member's has the same look: the unit is
restarted though it was never drained, which ends any execution open on it (a Buck2 build does not
reattach, so each is a failed action), and the wait for readiness fails, since it asks at the same
wrong address. Check the `base` of a new line before the first roll.

Every failure after the drain stops the roll and names the member as not serving (`frontend member
a (fe-a) is not serving`): the install, the restart and the readiness wait leave a member that is
down or not ready, and the load balancer sends nothing to it. The other members serve. An operator
fixes it and starts another roll, which begins at the check again.

A restart of the updater finds the phase in `roll.json`. In `draining` the member's drain is read
first: a member that says it is not draining was never told to (the record is written before the
drain is sent), and is checked against the other members as in the check. The member is then told
to drain again (a drain does nothing to a member that is draining) and its exit is waited for; in
`installing` the host is asked what it has and installs only if it does not have the target; in
`restarting` and `waiting_healthy` the member is **not** told to drain, which would take down the process the restart started. The
phases are `pending`, `draining`, `installing`, `restarting`, `waiting_healthy` and `done`. A
`roll.json` written by an earlier build, which has none of `draining` and `restarting`, is read as
it was: no field was added, and the format is still 1. A `roll.json` in `retiring` or
`undraining`, which an earlier build wrote for a member it rolled through a proxy, fails the
member with `swap_abandoned`: abort that roll.

## The agent mode

`kbf-updater agent` is the other mode of this program, and the one the maintainers' design
([../design/updates.md](../design/updates.md)) ends on: an unprivileged updater on every host that
dials the coordinator and owns that host's update. The roll mode above is unchanged and keeps
working (until the roll and its ssh path are deleted, in the last step of that design).

The program is a release asset for each platform that runs it: `kbf-updater-<commit>-linux-x86_64.tar.gz`,
`kbf-updater-<commit>-linux-arm64.tar.gz` and `kbf-updater-<commit>-darwin-arm64.tar.gz`, each with one
member, `kbf-updater`, attested like every tarball ([../artifacts.md](../artifacts.md)). The macOS one
is signed ad hoc with the hardened runtime, as the other macOS binaries are; a Mac runs the agent as
`_kbf-updater` from the property list in `packaging/host-agent/macos`.
The account `_kbf-updater` and the two directories the agent writes, `/var/db/kbf-updater` (its
state directory, the account's, mode 700) and `/Library/Logs/kbf-updater` (its logs, the account's,
mode 755), come from the `updater_user` item of `kbf-mac-provision` (a release asset, `kbf-mac-provision-<commit>-darwin-arm64.tar.gz`, attested like every tarball ([../artifacts.md](../artifacts.md))): put `updater_user=_kbf-updater`
(and, to fix the id, `updater_id=<200 to 499>`) in the Mac's profile and run `apply`
([the item](../../crates/kbf-mac-provision/README.md)). Nothing else creates the account.

```text
kbf-updater agent --coordinator <dns name>:<port> --channel NAME
                  (--state-dir DIR | --client-cert FILE --client-key FILE --ca FILE)
                  [--frontend <dns name>:<port>] [--enroll-frontend <dns name>:<port>]
                  [--operator-api http://IP:PORT --token-file FILE] [--cordon-dir DIR]
                  [--helper-socket PATH]
                  [--service NAME,helper-socket=PATH[,SETTING=VALUE...]]...
                  [--heartbeat-secs N] [--drain-deadline-secs N] [--health-timeout-secs N]
                  [--health-poll-secs N] [--retry-secs N]
```

- `--coordinator`: a DNS name and a port. The coordinator's certificate must chain to the farm CA
  for that name and have the role `coordinator`; that is checked after the handshake and before
  anything is sent.
- `--state-dir`: the directory `kbf-updater enroll` filled ([below](#enrolling-a-host)): the key,
  the certificate and the CA are its files. The agent renews the certificate at half its life
  and writes the new one there; `--enroll-frontend` is where. The certificate is the farm CA's `updater`
  certificate: its one DNS name is this node's id and its `urn:kbf:host-role:` is what the host
  runs (`frontend`, `storage`, `daemon` or `mac-daemon`; a `frontend` host's helper runs the
  frontend as a container, see [A frontend host](#a-frontend-host)).
  The agent reads both from it. The files are read for every heartbeat and every call to the
  frontend, so a renewed pair is used from the next one, if it names the same node and role as the
  pair the agent started with (a pair for another node is refused until the agent is restarted).
  The key file must be a regular file of the agent's user with mode 0600 or 0400, as the token
  file must.
- `--client-cert`, `--client-key`, `--ca`: instead of `--state-dir`, files an operator placed.
  Nothing renews them. Not both.
- `--frontend`: the frontend's worker listener (its `--worker-listen`, the address daemons dial), a
  DNS name and a port. The node's drain, undrain and status, and a storage host's own storage
  health, are calls there, made over mutual TLS with the updater's certificate and **no token**;
  see [the boundary](#the-boundary). The name must be one the frontend's worker certificate (the
  farm CA's `frontend` certificate) covers.
- `--enroll-frontend`: the frontend's enroll listener (`--enroll-listen`), a DNS name and a port:
  the address `kbf-updater enroll --frontend` was given. The certificate is renewed there. It is
  required with `--state-dir`.
- `--channel`: the channel of the helper's allowlist. The version the coordinator names is looked
  up in it by the helper, never fetched by URL.
- `--cordon-dir`: where the agent records that a cordon on the node is its own: one small file
  per node and per unit it updates (`cordon-<node>.<unit>`, the unit being `kbf` or the
  `--service` name), synced before the drain call and removed after the undrain worked. A retry
  of an update that failed with the node left drained, and an agent restarted in the middle of an
  update, give such a node back; a node an operator drained is left out. A host the frontend
  knows no node for (a storage host with no daemon) gets no mark. A node counts as the agent's
  while any unit's mark is on it, and an undrain by one unit gives the node back only when no
  other unit's mark remains. Default: the `--state-dir`, else the directory of `--client-key`.
  The agent checks at start that it can write there and does not start otherwise; with placed
  files in a root-owned directory under the shipped unit's `ProtectSystem=strict`, name a writable
  one (the unit's `StateDirectory`, `/var/lib/kbf-updater`, or a path in `ReadWritePaths=`).
  **A cordon given back by hand** (`:uncordon`, `kbf cluster`) leaves the mark; the agent drops
  the marks of a node whenever it reads that node serving: at start, at each heartbeat it spends
  idle, and before an update's drain. The window in which a stale mark still means something is
  therefore from the hand uncordon to the agent's next look: at most one heartbeat interval while
  the agent is idle, up to the end of the update when it is in the middle of one, and for as long
  as the agent is stopped, or the frontend cannot be reached. In that window an operator's new
  drain of the node reads as the agent's own and is given back by the update that finds it. If
  the agent is stopped, remove the file by hand: `<cordon dir>/cordon-<node>.<unit>`. The
  frontend's node read names no cordon (no generation, no time), so nothing finer is possible
  without changing it.
- `--daemon-node`: for the agent of a **frontend host** that also runs a daemon: the daemon's node
  id. The agent drains that node around the frontend's update (with the operator token if the
  agent has one, else with its certificate, which names its own node only). Without it a frontend
  host's node is not drained.
- `--helper-socket`: the socket of `kbf-host-install --serve` (default
  `/run/kbf-host-install/local.sock`; [host-install.md](host-install.md#the-local-socket)).
- `--operator-api`, `--token-file`: optional, together: the full
  operator token over the frontend's loopback, an opt-in fallback used instead of the
  certificate; see the boundary below.
- `--service NAME,helper-socket=PATH[,SETTING=VALUE...]`: a service the host runs besides `kbf`, given once for
  each; see [Services besides kbf](#services-besides-kbf). Without it the agent is a
  `kbf`-only agent.
- Times, in seconds, 1 to 86400: `--heartbeat-secs` 15, `--health-timeout-secs` 300,
  `--health-poll-secs` 5, `--retry-secs` 900. `--drain-deadline-secs` has no default and no
  upper bound (1 or more): without it the node's leases are waited for however long they take.

### A frontend host

A host whose certificate says `frontend` runs its frontend as a container, and its helper is
`kbf-host-install --serve --frontend-container` ([host-install.md](host-install.md#a-frontend-container)).
The agent runs the same A/B as for any host (install, switch, health, confirm, or revert) and
reports it in the next heartbeat. What differs is who does the frontend's part: the helper drains
the container through the frontend's own operator API, recreates it from the new attested image
with the same run configuration, waits for `/readyz` and `/healthz` on the target commit, and puts
the old image back on any failure; the agent's health check is the helper's word that the release
runs. The coordinator orders frontends one at a time and never the last one that is ready; the
helper adds its own refusal when a `peers` entry of its configuration is not ready.

**A lone frontend is never updated by the coordinator.** A frontend is ordered only when at least
`max(KBF_MIN_UP_FRONTEND, 1)` *other* frontends are up (`required_up` in
`crates/kbf-coordinator/src/decide.rs`), so with one frontend there is nobody else and no order is
ever sent, whatever `KBF_MIN_UP_FRONTEND` says; the gamma job then fails when the stall or the
deadline passes. A storage host is in the same case when `KBF_MIN_UP_STORAGE` is 1 or more (the
default) and it is the only one. For a farm the coordinator deploys, run two or more frontends. With
one, update it by hand in a maintenance window, through its helper's socket
([host-install.md](host-install.md#the-five-requests)), as the socket's account:

```text
install kbf=<version> from <channel>
switch kbf=<version>
confirm kbf=<version>
```

The farm's frontend is down only from the moment the old container has ended, for the new one's start and
`ready_secs`; the drain before that (no deadline unless `drain_secs` is set, and never cutting work) lets the old
frontend finish what it has. The switch answers `switched` only after `/readyz` answered 200 on the
new commit; send `confirm` then, or the helper reverts at the window's deadline.

### The heartbeat

A POST of `{"platform", "version", "health", "last_update", "updating"}` to `/v1/heartbeat`, at
start and every `--heartbeat-secs`, on a thread of its own, so that it goes on through a whole A/B
(a host that is silent for 45 seconds counts as down). `version` is the version text the host's
helper knows for the release that runs; for a release placed by hand it is
`0.1.0+g<12 hex digits>`, which names the commit and nothing else, and an update to the same commit
teaches the helper the text. `health` is the node's health check against the running version.
`updating` is the target while an A/B runs; `last_update` is how the last one ended. **The node id
and the role are in no field**: the coordinator reads them from the certificate.

### One A/B

On a reply of `update to X` (and only X is used from it), in this order, each step only if the one
before it worked:

1. **Fetch and verify** through the helper (`install`): the release is fetched on the host and
   checked there. An unattested build, one of another commit, or one the channel does not have is
   refused here, and the node has not been touched.
2. **Drain** the node at the frontend (`:drain`, with the updater's certificate on the frontend's
   worker listener) and wait until it holds no lease, however long that takes. With
   `--drain-deadline-secs`, a deadline that passes before that fails the update with the node
   left drained, nothing switched, and the leases running on; the agent tries the same order
   again after `--retry-secs`, which drains the node again and waits again. A storage host
   drains the **daemon part** of its node too when it has one (a `storage-and-daemon` host): its
   restart would cut the daemon off from its leases, so the drain is asked, and a node the
   frontend does not know (404) is a storage host without a daemon, which has nothing to drain.
3. **Switch** (`switch`): the helper puts X on trial and restarts the units. The agent's call
   has no time limit: a frontend host's helper drains the container for as long as it takes
   (`drain_secs` unset), and it serves one request at a time, so it cannot be asked anything
   meanwhile. The agent waits on the open call; a helper that dies closes the socket, which ends
   the wait at once as `unreachable`. The other calls keep their limits (an install 20 minutes, a
   revert 10 minutes, the rest 1 minute). A helper that is alive and silent is waited for, bounded
   by the helper's own deadlines (the drain deadline it is configured with, the stop, the readiness
   wait).
4. **Health**: the node's health check (`node_health`: a storage member is `active` on the build
   and no blob is known to be owed a copy, read from the frontend's
   `GET /v1/nodes/{id}/storage-health`; a daemon is connected on it) of X, held drained, for at
   most `--health-timeout-secs`, and never past the helper's deadline less ten seconds.
   A daemon is connected to one frontend only, and `--frontend` is the load balancer's name,
   which picks a frontend per connection: a frontend that does not hold the node answers
   "not connected" (or 404). The agent therefore asks up to 8 times, each on a new connection,
   and takes the first answer that says connected; it reports "is not connected" only when
   none does. A client with `--operator-api` reaches one frontend and asks once.
5. **Confirm** X if it is healthy. If it is not, **revert**: the helper does that by itself at its
   deadline whatever this process is doing; the agent asks for it at once.
6. **Undrain** only if the node is healthy: on X after a confirm, or on the release that ran
   after a revert. A node that is not healthy stays drained.

The result goes in the next heartbeat. A restart of the agent in the middle finds the helper's
pending marker (`query`) and finishes the trial from step 4; nothing is fetched, drained or
switched twice. A target that failed is not tried again before `--retry-secs`, however often
the coordinator names it.

### Services besides kbf

An agent given `--service` also updates those services, with the same A/B (install, drain, switch,
health, confirm or revert) run with the service's own ports. `NAME` is a service name (lowercase
letters, digits and `-`, at most 64 bytes; not `kbf`), given once. The settings follow it, separated
by commas, each at most once:

| setting | meaning | default |
|---|---|---|
| `channel=C` | the channel of the helper's allowlist that holds the service's releases | `NAME` (it must be a channel name) |
| `helper-socket=PATH` | the socket of the helper that installs it (an absolute path with no `..`); required, one per service, and never the agent's `--helper-socket`, whose helper installs `kbf` (an install request names no service, so it would install `kbf` at the service's version) | none: required |
| `drain=node` or `drain=none` | `node`: the host's node is drained and given back through the frontend, as for `kbf`; `none`: nothing is drained, and no drain call is made | `node` |
| `ready-url=http://IP:PORT/PATH` | health is `GET PATH` on that loopback address answering 2xx (the path is `/` if there is none) | the node's health through the frontend, as for `kbf` |

A service's helper is spoken to exactly as `kbf`'s is: `query`, and `install kbf=<version> from
<channel>`, with the service's version and channel. The request names no service, so each service
needs a helper of its own that installs that service when asked so; the agent refuses to start with a
service whose socket is its own `--helper-socket`.

A value holds no comma. `drain=node` and node health need `--frontend` (or the operator token), as
`kbf` does; a service with `drain=none` and a `ready-url` needs neither. A readiness URL is checked
for the service's word that it serves; it does not say which release answered.

With no `--service`, the heartbeat is the one above with no `services` field and the reply is read
as before. With any, the heartbeat also carries `services`: for each, its version, health, last
update and the target of the update running now (`kbf` stays in the fields it always had), and the
reply may name the service to update. The agent runs one A/B at a time on the host, whichever
service it is for: an order that arrives meanwhile waits for the one running. A target that failed
is left alone for `--retry-secs` per service: a failure of one does not hold back another. The
agent's log lines for a service are led by its name. A restart in the middle of a service's trial
finishes that trial first, as for `kbf`.

### Enrolling a host

A host gets its updater certificate from the farm CA with a one-time token, with `kbf-updater enroll`:

```text
kbf-updater enroll --frontend <dns name>:<port> --node-id ID --token-file FILE --state-dir DIR
```

1. On the frontend the operator makes the token (`POST /v1/enroll/tokens`, [../api.md](../api.md)):
   `{"roles": ["updater"], "node_id": "host-1", "host_role": "daemon"}`, one use. The token fixes
   the node id and the host role; a token for a node's roles (`storage`, `daemon`) is refused for
   an updater, and an updater token is refused for any other role, for another node id, a second
   time and after it expired.
   A frontend host is enrolled the same way, with `"host_role": "frontend"` in the token. The word
   `frontend` is refused only in the token's `roles` list (`Role::from_str`), never as the `host_role`.
2. On the host, as the agent's account, the token goes in a file of that account's with mode 0600
   and the state directory is that account's with mode 0700 (the command refuses otherwise,
   before it reads the token). The command makes the key on the host (mode 0600; only a request
   signed by it is sent), checks the frontend against the CA pin the token carries and the answer
   (role, node id, key, chain) before it writes anything, installs `pki/updater.key`,
   `pki/updater.pem` and `pki/ca.pem`, and removes the token file. Run again after a success it
   finds the host enrolled and changes nothing; after a failure it leaves the token file for a retry.
3. The agent started with `--state-dir DIR` renews the certificate at half its life (spread by up
   to 10%), by a request signed by the key the registry holds, retrying every five minutes
   doubling to an hour, and logs an error when less than a day is left. A host whose certificate
   lapsed during an outage recovers by itself, for as long as the host is not replaced.

Revoking the host by its id (`POST /v1/enroll/nodes/{id}/revoke`) revokes its updater row too, so the
renewal is refused and the certificate runs out at its end. The worker listener's deny list
(`--worker-deny-list`, a `node` entry) does apply to an updater's node calls, as it does to a
daemon's, and takes effect at once; without an entry a certificate already issued keeps its
own-node calls until it expires: seven days by default, thirty at most. A host re-enrolled with
a `replace` token stops the old key from renewing in the same way and does not cut off a
certificate already issued. The `deadline_secs` of a self-drain is whatever the host sends; it
only moves that host's own drain deadline.

The CA's key is the frontend's alone; the coordinator holds none. The units in
`packaging/host-agent` use this layout ([its README](../../packaging/host-agent/README.md)).

### The boundary

The design says the full operator token must not be on hosts, and the agent does not need it. The
frontend itself serves a host's updater four calls about its own node on the worker listener, the
listener it already authenticates node certificates on (mutual TLS against `--worker-client-ca`,
reached by every host that runs a daemon or a storage node):

| call | |
|---|---|
| `POST /v1/nodes/{id}:drain` | cordon the node and wait for its leases (body `{"deadline_secs": N}`, optional) |
| `POST /v1/nodes/{id}:uncordon` | return it to placement |
| `GET /v1/nodes/{id}` | the node, as the operator API has it |
| `GET /v1/nodes/{id}/storage-health` | the node's own storage member and two storage-wide numbers |

**The frontend serves these for a certificate with the role `updater` whose one DNS name is `{id}`,
and nothing else**: another node's id, the cordon, a listing, the members and enrollment routes,
the update API and every other path are `403`, and so is every call of a daemon, storage, frontend
or coordinator certificate on these paths. No operator token is involved on the host or the
frontend, and the worker listener's deny list applies to the node. The shipped units give the agent
no token. The frontend's certificate for the name in `--frontend` must be the farm CA's `frontend`
certificate (the agent checks the chain to the farm CA, the name and the role before it sends a
byte), not a public one.

The worker listener offers `h2` and `http/1.1` in its handshake; the agent speaks HTTP/1.1, one
request per connection, and the daemons' gRPC is unchanged beside it. A frontend that is older than
this change offers `h2` only and refuses the agent's handshake: update the frontends first.

What remains of the token: `--operator-api` and `--token-file` are an opt-in fallback for a host
whose operator accepts the token there (anything running as the agent's account can then read it);
given, they are used instead of the certificate. Without `--frontend` and without the token an
agent heartbeats and refuses every update order before it fetches anything, for any host.

## Updating the coordinator

The coordinator is not one of the hosts it orders: it does not update itself. It is updated the
way it was deployed ([containers.md](containers.md#run-the-coordinator-on-one-host): a new image,
the tag changed in the unit, a restart; to go back, name the old tag). Hosts that were updating
finish or revert on their own while it is down, and the restarted coordinator orders nothing for
its first 30 seconds.

Update it first when a release changes the coordinator or the messages between it and the
hosts (`crates/kbf-coordinator`, `crates/kbf-updater/src/wire.rs`): a newer agent's heartbeat
reaches an older coordinator, but a coordinator that does not know a newer rule would order the
farm by the old one.

`GET /v1/status` and `GET /v1/convergence` say which build answers, as `commit`: the 12 digits
`kbf-coordinator --version` shows after the `+`. A coordinator built before this field, or
without a git checkout (`unknown`), says none. With `--require-coordinator-current`
(`KBF_REQUIRE_COORDINATOR_CURRENT=true` in the release workflow, passed as the flag's value), `deploy-farm` reads that
commit before it sets anything and fails when it is not the release's:

```text
the coordinator runs 0123456789ab, the release is 89abcdef0123: update the coordinator first (docs/deploy/kbf-updater.md, Updating the coordinator)
```

Without the flag, a different commit is a `coordinator not current` warning annotation on the run
summary (the same text) and the deploy goes on. A coordinator that reports no commit gets a notice and the deploy goes on, so a farm whose
coordinator is older than this field keeps working. The rule is equality, not order: a
coordinator newer than the release (a rollback of the hosts) also fails; leave the variable unset
for such a deploy. The variable is the operator's to set for the releases that need it.

## Turn on the gamma stage

The last job of `release.yml`, `gamma`, tells the update coordinator "desired = this release" once
`publish` has made the release, and waits until the farm converges. It is the end-to-end check of
a release on a real farm. It does nothing until an operator has done the three things below, once;
until then the job prints `::notice::the deploy step is off` and passes, so a fork or a repository
that runs no farm is unaffected. Nothing in this repository does any of them.

1. **Deploy the coordinator** (`kbf-coordinator`, [the design](../design/updates.md); as a pod
   under an orchestrator such as k3s, or as one container on one host under Docker, see
   [containers.md](containers.md#run-the-coordinator-on-one-host)) with a
   certificate the farm CA issued with the role `coordinator`, and an allow-list for CI's OIDC
   token in the file `--oidc-config` names. The job's token is the run's, so the allow-list must
   say exactly this, and nothing wider:

   ```toml
   [remote_operator]
   issuer = "https://token.actions.githubusercontent.com"
   audience = "https://<the coordinator's control address, as written in the variable below>"
   repository_id = "<the numeric id of komira-ai/kbf>"
   repository = "komira-ai/kbf"
   job_workflow_ref = ["komira-ai/kbf/.github/workflows/release.yml@refs/heads/main"]
   refs = ["refs/heads/main"]
   environments = ["release"]
   event_names = ["workflow_run"]
   ```

   The coordinator loads the file with `kbf_oidc::AllowList`, so it also reads several
   `[[remote_operator]]` entries (never mixed with the single table), one per repository and
   workflow that may call. An entry's `services = [...]` (default `["kbf"]`) lists the services a
   token it admits may set, and a token never gets another entry's services: setting a service the
   admitting entry does not list is a 403 `forbidden` that changes nothing. The single table above
   is one entry that lets CI set `kbf` only. The services must be defined (`kbf` is shipped with the coordinator; others
   come from `--services-file`, which no longer takes `ci_services`: such a file is refused).
   Entries must name the same issuer, and two entries may not share a `repository_id` and a
   `job_workflow_ref`; such a file is refused at load.

   `event_names` must name `workflow_run`: the release workflow is started by the completion of
   the pipeline, and its tokens say so (the default list is `push` and `workflow_dispatch`).
   The `sha` of the token is the head of `main`, not the commit released. A token is good for one
   call. The control listener (`--control-listen`) must be reachable from GitHub-hosted runners;
   the heartbeat listener need not be. A control listener on a Tailscale network only is
   reachable too: the job joins that network first when `TS_CLIENT_ID` and `KBF_COORDINATOR_URL` are set (below).
2. **Enroll the hosts**, so that their updaters heartbeat to the coordinator
   ([The agent mode](#the-agent-mode)). A host that does not heartbeat is `down`, and the
   coordinator does not wait for it: it says `converged` when every host that does heartbeat runs
   the release, none is updating or failed, and at least the minimums (below) are up. The step
   does not take that alone: it remembers how many hosts of each role were live when it started
   (see below) and fails if fewer run the release at the end, so a host that the release made
   stop heartbeating fails the deploy. A host that was already down when the step started is not
   expected; raise the minimums to the number of hosts the farm must keep, for the roles whose
   count matters beyond that.
3. **Set the repository variables** (Settings, Secrets and variables, Actions, Variables):

   | variable | value |
   |---|---|
   | `KBF_COORDINATOR_URL` | `https://<name>:<port>` of the control listener, no path. Unset or empty: the step is off |
   | `KBF_COORDINATOR_CA` | the farm CA's certificate (PEM, public). The coordinator's certificate is the farm CA's, which no system store holds; empty trusts the system store instead |
   | `TS_CLIENT_ID`, `TS_AUDIENCE` | optional, for a coordinator reachable only on a Tailscale network: the client id and audience of the Tailscale trust credential (workload identity federation) that CI jobs join the network with, as `tag:ci`. It must trust this repository's `release` environment (the token's subject is `repo:<owner>/<repo>:environment:release`), and the network's policy must let `tag:ci` reach the control listener. With `TS_CLIENT_ID` and `KBF_COORDINATOR_URL` set, the job joins as an ephemeral node with the run's OIDC token, after the build and before the verb; no secret is involved. `TS_CLIENT_ID` or `KBF_COORDINATOR_URL` unset: the step is skipped (`TS_AUDIENCE` must be set with `TS_CLIENT_ID`) |
   | `FARM_ADDRESS`, `FARM_INSTANCE`, `FARM_TLS`, `FARM_LINUX_X86_64_PROPERTIES` | optional, for the proof below: the farm's REAPI address as `grpc://<host>:<port>` (also when TLS is on), its instance name (empty: the empty instance), `true` or `false` for TLS, and the platform properties of the Linux x86-64 workers as a comma-separated `key=value` list, such as `OSFamily=linux,ISA=x86-64`. `FARM_ADDRESS` unset or empty: the proof is off |
   | `KBF_MIN_UP_STORAGE`, `KBF_MIN_UP_FRONTEND` | optional: the storage hosts and frontends that must stay up while others update (default 1 each). The operator picks the storage number so that every blob stays readable |
   | `KBF_REQUIRE_COORDINATOR_CURRENT` | optional: `true` makes the step fail before it sets anything when the coordinator runs another commit than the release ([Updating the coordinator](#updating-the-coordinator)). Unset: a different commit is a warning annotation on the run and the step goes on |

### What the step does

The job starts only after `rehearse` and `rehearse-agent` pass, on hosted runners with systemd. In
`rehearse`, a host laid out by hand from the release before this one (its `kbf` through its own
`kbf-deploy host install`, its `kbf-host-install`, its package's trust files and accounts, this
release's helper unit) is updated to this release by the real helper through its local socket, as
the agent asks for it (`install`, `switch`, `confirm`), with the release fetched from GitHub by
the helper itself (`crates/kbf-host-install/tests/update_rehearsal.rs`). In `rehearse-agent` the
same host is updated through the control loop of this page: this commit's coordinator, with a
throwaway farm CA and an allow-list as above; `cargo xtask deploy-farm`, given the version that
`cargo xtask release farm-version` prints, as the next step is; the release before this one's
agent, helper, frontend and `kbf`, the agent heartbeating to the coordinator and reading its
node's storage health from the frontend with its certificate alone
(`crates/kbf-host-install/tests/update_rehearsal_agent.rs`). A release that host cannot take, or
a version the hosts never report, is never set as the desired version. The rehearsals do not
cover a drain of a node with leases (the host has no daemon half), the farm's Tailscale network
or its real issuer (the issuer of the OIDC token is a stand-in on loopback, and the minimum of
storage hosts that must stay up is 0 because the rehearsal's farm has one).

The version it sets is not the release's tag. A host heartbeats `<semver>+g<12 lower-case hex
digits of the commit>` (for example `0.1.0+g0123456789ab`); the coordinator calls a host done
when its heartbeat equals the desired version, and the host's installer finds the commit to
install in that text. The step before it, `cargo xtask release farm-version` (one `run:` line,
`id: farm-version`), derives that text for the release being deployed: it downloads the release's
`SHA256SUMS` and `kbf-<commit>-linux-x86_64.version.json` (what the build printed for
`kbf --version --json`), requires the facts to be the file `SHA256SUMS` lists and the release build
of this commit, and prints `version=<its version>+g<its first 12 commit digits>`. The job fails
there when it cannot. `deploy-farm` refuses a `--version` of any other form with an error that
says which form it needs, before it calls the coordinator. Anything else that sets the
coordinator's desired version (a `POST /v1/desired` by hand) needs the same form, or no host
will converge on it.

`--services-file` is read once at start, under the rule of `--operator-token-file`: a regular file (not a symlink) owned by the coordinator's user with mode `0600` or `0400`, at most 1 MiB; a file that fails it stops the start. Treat it as secret-grade: a file whose first entry is named `kbf` replaces the shipped kbf definition, and can set `max_updating` up to 10000, `min_up = 0`, drop `never_last` and every `after`, so whoever controls the file controls kbf's rollout safety. A replaced kbf must be the first entry, wait for no service and have a role, and its desired state still names `min_up` for storage and frontend.

A service entry may also set two rollout fields, both off by default (a coordinator before the change refuses a file that names them, so set them only after every coordinator runs the new build):

- `bake_beats = N` (default 0, at most 100; a larger number is refused with `bake_beats is over 100`). A host at the desired version keeps the update slot it used until it has sent `N` healthy beats in a row at that version, not updating and not waiting for a promotion. An unhealthy or updating beat, a pending promotion, a different version, or a silence of 45 s or more between two beats starts the count again. The count is kept in the coordinator's memory only: a restart forgets it, and a host at the desired version bakes again. A host that is silent for 45 s or more stops holding the slot for baking. `GET /v1/convergence` is `in_progress`, not `converged`, while a live host is still baking.
- `canary = { count = n }` (`n` from 1 to 10000; otherwise refused with `canary count is not 1 to 10000`). Until `n` hosts at the desired version have baked (`bake_beats` healthy beats, 0 meaning up at the version), the coordinator orders at most `n` hosts of the service: hosts that are updating, under an order, failed at the version, or at it all count as started, silent ones included. The hosts that go first are the live ones not yet started, hosts of roles that wait for no other role before the rest, then by node id. Every other host is held with `canary_wait`; a canary that cannot go yet (no free slot, too few hosts up, a role it waits for) holds them all, by design. A canary that fails leaves the service `blocked` and orders nobody else. The roles-and-labels form of the canary (`canary = { roles = [...], labels = {...} }`) is refused as an unknown field until host selectors land.

A host whose heartbeat carries `promotion_pending` holds its update slot and does not count as up, whatever the definition says. The coordinator puts no bound of its own on a pending promotion. The design gives that bound to the updater (the service declaration's `promotion_ceiling_secs`, which turns an overdue promotion into a failed update, `after_commit`, while the agent keeps beating); no build implements it yet. A host that dies mid-promotion holds its slot until it is fixed, as an updating host does.

The coordinator keeps its desired state per service name, and `kbf` is the only service it accepts unless `--services-file` defines more ([the design](../design/updates.md)): `POST /v1/desired` takes an optional `service` (absent means `kbf`; a name no file defines is a 400), and `GET /v1/status` adds `services` beside the unchanged `desired`, `services_dropped`, the number of heartbeats whose `services` list was bad and was dropped (the host is answered as a `kbf`-only host), and `per_service` (below). While `kbf` is the only service, the `--state-file` holds the single state in the form earlier coordinators wrote, so a downgrade to one of them starts on the same file; a file in the map form (`{"services": {...}}`), which a coordinator with a second service writes, is refused by an earlier build, and the operator then restores the file from before or writes `kbf`'s state alone in it.

`--ops-file` (default: the `--state-file` name with `.ops` added) holds what an operator decides, apart from CI's desired state: the hosts excluded from a service, the labels on a host, and a ring of the last 64 operator changes (it also has the places for the per-service marks later releases fill: a halt, the last good version, the bad versions). The coordinator creates the file at the first change and replaces it whole (a temporary file beside it, fsync, rename, directory fsync), owner-only; with no file nothing is excluded or labelled and the coordinator behaves as before. The reader is strict: a key it does not know, or a `schema` newer than the build's, stops the start with the flag and the path in the message, so a coordinator rolled back below the build that wrote the file does not ignore what it holds. To start without the file anyway, move it aside on purpose. The directory must be writable by the coordinator. `GET /v1/status` shows the file's content as `ops` once anything has been changed (before that the key is absent).

`POST /v1/operator` on the control listener takes the operator token as the bearer (CI's OIDC token is refused with 401, as is no token) and one action as JSON, at most 4096 bytes:

- `{"action":"exclude","service":"<name>","node_id":"<id>","reason":"<text>"}`: takes a host out of a service. The service must be one the coordinator manages; the reason is 1 to 256 bytes with no control character; a second exclusion of the same host and service replaces the first.
- `{"action":"include","service":"<name>","node_id":"<id>"}`: removes the exclusion (no error when there is none).
- `{"action":"set_labels","node_id":"<id>","labels":{"<key>":"<value>"}}`: replaces the host's labels; at most 8, each key and value 1 to 32 bytes of `[a-z0-9-]`; an empty object removes them. Only this call sets labels: a heartbeat that carries them changes nothing.
- `{"action":"clear_halt","service":"<name>","reason":"<text>"}`: lifts the halt of a service. A host whose last update failed in the `halt` phase halts its service: nothing is ordered for any host of it, a new desired version does not lift it, `GET /v1/convergence` says `halted` and names the host, the check and the version (`halt`), `deploy-farm` fails on it, and the coordinator logs it at error with `alert="halt"`. The clear is written to the ops file before it takes effect, acknowledges the report that raised the halt and every other halt report received while it stood (a host keeps reporting until it updates again; the acknowledgement ends when the host reports a different `last_update` (another reason, class or target, or none), so a new failure of the same version halts again) and adds one to the service's clear generation, which an agent that announced `halt-v1` is told as `halts` in every answer. **Rollout gate:** do not roll a coordinator with this change until the agents run kbf#2158 with `halts` support. An agent that announces `halt-v1` but does not know the `halts` reply field refuses the reply (its reply reader denies unknown fields). A service that is not halted is refused with 400.

The token is checked before the body is read (no token is a 401 whatever the body). The file holds at most 4096 hosts with labels and 4096 exclusions (`MAX_HOSTS` and `MAX_EXCLUDED` in `ops.rs`, the one place to raise them); an action that would add one more is a 400 and the log line names the cap. A label key given twice in one body is a 400.

Each accepted action is written to the file before it takes effect (a failed write is a 500 and the old state stands), numbered, appended to the ring, and logged at info on the target `kbf_coordinator::audit` with `by`, `action`, `service`, `node_id` and `reason`; the answer is that entry. A bad body is a 400. In this release the coordinator records these changes and shows them in the status; nothing yet reads them when it decides what a host is told.

`cargo xtask deploy-farm` (one `run:` line):

1. reads `GET /v1/convergence` once, before anything is changed, and keeps the hosts live then
   (at the version, updating or failed, so not `down`) per role as the baseline, and prints it
   (`baseline: hosts live before the deploy: storage 3, frontend 2`). A 503 on this read is tried
   again like the POST. A coordinator that has just restarted (the table says so) may not have
   heard from every host yet; a host it hears from later counts as one that joined;
2. posts `{"version": <the version text above>, "min_up": {...}}` to `/v1/desired` (the per-role
   limits are the coordinator's defaults), with an OIDC token for the audience that is the URL as
   written in the variable (the audience keeps a trailing `/` the variable has, which the URL
   the calls go to does not, so write the variable and the allow-list's `audience` alike). A 503
   on the POST is tried again, up to four times;
3. reads `GET /v1/convergence` every 30 seconds, with a fresh token each time, and prints a
   table of the counts per role (`at_desired`, `updating`, `failed`, `down`);
4. ends when the coordinator says `converged` and every role has at least as many hosts at the
   version as it had live at the baseline (exit 0; a host that joined during the deploy only
   adds); when it says `converged` and a role has fewer, which fails with the role, the count
   expected, the count reached and how many went down; `blocked` (a live host failed its update to
   this version and is still behind it); when the counts have not changed for 20 minutes
   (`--stall-min`; a host whose update takes longer than that leaves the counts as they are, so
   raise the flag past the longest update); when the coordinator's desired version is no longer
   this release's (another writer replaced it); when 60 minutes are up (`--deadline-min`); or when the coordinator refuses
   the token (401 or 403, at once: the allow-list does not match this job). Any of those but
   convergence fails the job; the release stays published whatever it says. Another failed read
   is reported and tried again, and counts as no change.

`GET /v1/convergence` answers `{"desired", "quiet", "state", "roles": [{"role", "at_desired",
"updating", "failed", "down"}]}`, where `state` is `converged`, `blocked`, `halted` or `in_progress` (`halted` adds `halt`: `{"node_id","check","target"}`). It
takes CI's OIDC token and refuses the operator token (the operator's read is `GET /v1/status`).
It is computed from the heartbeats and the desired state, and stores nothing. `converged` needs
the coordinator to be past its 30 second quiet start, at least one live host, every live host at
the version and none updating or failed, and `min_up` hosts of the storage and frontend roles up,
so a coordinator that has just restarted does not report a farm it has not heard from as
converged.

`GET /v1/convergence?service=NAME` answers the same shape for another service the coordinator
manages: the hosts counted are those whose heartbeats carry that service in `services`, the
`desired` is that service's, and `converged` also needs, for each role the service runs on, as
many hosts up as the service requires of the role (its role's `min_up`, at least 1 for a
`never_last` role, else the desired state's `min_up` for that role). Without the query, or with
`?service=kbf`, the answer is the one described above, byte for byte. A name no definition
declares is a 404 `not_found`; a query that is not exactly `service=NAME` is a 400. Any token the
allow-list accepts may read any service's convergence; the read changes nothing. `GET /v1/status` adds, after its
existing keys, `per_service`: for every managed service (`kbf` included) by name,
`{"desired": <its desired state or null>, "roles": [{"role", "at_desired", "updating",
"failed", "down"}], "hosts": [...], "up": N, "slots_held": N, "convergence": {...}}`, the counts
being those of `GET /v1/convergence` for the service. `hosts` is every host that heartbeated the
service, by node id: the fields of the top-level `hosts` entries, with `up` and `holds_slot`
counted for this service (a host ordered to update `other` holds `other`'s slot and is not up for
it, while it holds nothing under `kbf`), and `hold`: the rule that keeps the coordinator from
ordering the host now (`quiet`, `updating`, `busy_elsewhere`, `up_to_date`, `failed_here`,
`deferred`, `no_slot`, `baking`, `canary_wait`, `waiting_for_role`, `waiting_for_service`, `min_up`, `not_a_member`, `no_desired`, `no_definition`; these spellings are public), or
`null` when it would be ordered at its next heartbeat. `up` and `slots_held` count those hosts,
and `convergence` is the answer of `GET /v1/convergence?service=NAME`. The top-level keys, and the
top-level `hosts`, which are `kbf`'s, are unchanged. Two holds are for the rollout fields above: `baking` means every slot of the host's role is held and at least one holder is a host at the desired version that has not finished baking, and `canary_wait` means the service's canary gate is closed and the host is not one of the canaries. `deferred` means the host declined the update to the desired version and its wait (`defer_backoff`, never shorter than the host's own `retry_after`) has not ended.

`cargo xtask deploy-farm --service NAME` (default `kbf`) deploys another service the same way.
For `kbf` the requests are the ones listed above. For another service the POST body also has
`"service": NAME`, the baseline read and every poll are `GET /v1/convergence?service=NAME`, and
`--version` is not checked against `kbf`'s form: it is that service's own version text, which
must equal what its hosts report in their heartbeat (the coordinator compares the text and no
more). The name is 1 to 64 bytes of lowercase letters, digits and `-`. `--service` takes one
name, so a deploy of several services is one call per service. A coordinator that does not know
services (one from before the services wire) ignores the query, so the baseline read is `kbf`'s,
and then refuses the POST, whose `service` key it does not know: the run fails with that refusal
and nothing is set; it never deploys `kbf` in the service's place. `cargo xtask release farm-version` stays
the `kbf` form (`<semver>+g<12 hex digits>`, from `kbf`'s release asset). A second repository
computes its own: the version string its binary reports and its updater puts in the service's
heartbeat entry (`services[].version`), from that repository's own release, and passes that text
to `--service NAME --version TEXT`.

### The proof

After `deploy-farm` the job builds a small Buck2 project against the farm it has just
deployed (`cargo xtask prove-farm`; the project is `crates/kbf-integration-test/gamma/buck2`,
four `/bin/sh -c` actions that depend on each other). The Buck2 is the pinned one the beta
smoke uses. The verb writes the project's `.buckconfig.local` from the four `FARM_*` variables:
the REAPI address for the engine, the action cache and the CAS, TLS, the instance name, and an
execution platform with remote execution on, local execution off and the properties. The run id
and attempt are an argument of every action, so no action of a run has been run or cached
before. It then reads the `Commands: N (cached: C, remote: R, local: L)` line Buck2 prints at
the end of the build, and fails the job unless `R` is above 0 and `L` is 0 (a build that fails
or prints no such line fails too). With `FARM_ADDRESS` unset it prints
`::notice::the gamma proof is off` and passes. The job needs no secret for it; a farm that
requires a credential for its REAPI address is not covered.

The `gamma` job runs in the `release` environment, like `publish`: if the environment has
required reviewers, a release is approved a second time before the job starts.

One release at a time holds across the deploy: the workflow's group does not cancel, so a second
release waits for the first release's deploy (at most 90 minutes) to end.
