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
| `--ssh-connect-timeout-secs`, `--ssh-command-timeout-secs`, `--drain-deadline-secs`, `--apply-poll-secs`, `--apply-deadline-secs` | optional bounds in seconds; they default to 10, 900, 600, 5 and 600 |
| `--peer-ready-secs`, `--frontend-drain-deadline-secs` | optional bounds in seconds, at least 1, for a frontend pool member: how long every other member must answer `/readyz` 200 in a row before a member is drained (default 30), and how long a member is given to exit once it is told to drain (default 1200, which must be longer than the longest execution a member may have open). |

The updater reaches each member's operator API on loopback at `127.0.0.1:<base + 2>`, with the
same token (`--token-file`) as `--operator-api`.

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
   to be still draining), for at most `--frontend-drain-deadline-secs`
   (`drain_failed`). A member that answers the status but says it is not draining fails the roll
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

**The bound.** `--frontend-drain-deadline-secs` (default 1200) must be longer than the longest
execution a member may have open, or the roll gives up on a drain that is still going as it
should. The frontend in this tree puts no bound of its own on a drain that its endpoint began
(`--shutdown-timeout-secs` bounds only a drain that a signal began), so an execution that runs
past the bound fails every roll at that member until it ends. A drain that outlasts the bound
fails the roll with `drain_failed` and `frontend member a (fe-a) is left draining`: the member
goes on draining and exits by itself, and a new roll finds it stopped (its connection is refused)
and goes on from the install.

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
                  [--operator-api http://IP:PORT --token-file FILE] [--helper-socket PATH]
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
- `--daemon-node`: for the agent of a **frontend host** that also runs a daemon: the daemon's node
  id. The agent drains that node around the frontend's update (with the operator token if the
  agent has one, else with its certificate, which names its own node only). Without it a frontend
  host's node is not drained.
- `--helper-socket`: the socket of `kbf-host-install --serve` (default
  `/run/kbf-host-install/local.sock`; [host-install.md](host-install.md#the-local-socket)).
- `--operator-api`, `--token-file`: optional, together: the full
  operator token over the frontend's loopback, an opt-in fallback used instead of the
  certificate; see the boundary below.
- Times, in seconds, 1 to 86400: `--heartbeat-secs` 15, `--drain-deadline-secs` 600,
  `--health-timeout-secs` 300, `--health-poll-secs` 5, `--retry-secs` 900.

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

The farm's frontend is down for the length of the switch (its `drain_secs`, the stop, and
`ready_secs`), so do it when nothing needs it. The switch answers `switched` only after `/readyz` answered 200 on the
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
   worker listener) and wait for its leases, for at most `--drain-deadline-secs`. A storage host
   drains the **daemon part** of its node too when it has one (a `storage-and-daemon` host): its
   restart would cut the daemon off from its leases, so the drain is asked, and a node the
   frontend does not know (404) is a storage host without a daemon, which has nothing to drain.
3. **Switch** (`switch`): the helper puts X on trial and restarts the units.
4. **Health**: the node's health check (`node_health`: a storage member is `active` on the build
   and no blob is known to be owed a copy, read from the frontend's
   `GET /v1/nodes/{id}/storage-health`; a daemon is connected on it) of X, held drained, for at
   most `--health-timeout-secs`, and never past the helper's deadline less ten seconds.
5. **Confirm** X if it is healthy. If it is not, **revert**: the helper does that by itself at its
   deadline whatever this process is doing; the agent asks for it at once.
6. **Undrain** only if the node is healthy: on X after a confirm, or on the release that ran
   after a revert. A node that is not healthy stays drained.

The result goes in the next heartbeat. A restart of the agent in the middle finds the helper's
pending marker (`query`) and finishes the trial from step 4; nothing is fetched, drained or
switched twice. A target that failed is not tried again before `--retry-secs`, however often
the coordinator names it.

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
(`KBF_REQUIRE_COORDINATOR_CURRENT=true` in the release workflow), `deploy-farm` reads that
commit before it sets anything and fails when it is not the release's:

```text
the coordinator runs 0123456789ab, the release is 89abcdef0123: update the coordinator first (docs/deploy/kbf-updater.md, Updating the coordinator)
```

A coordinator that reports no commit gets a notice and the deploy goes on, so a farm whose
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
   | `KBF_REQUIRE_COORDINATOR_CURRENT` | optional: `true` makes the step fail before it sets anything when the coordinator runs another commit than the release ([Updating the coordinator](#updating-the-coordinator)). Unset: the commits are printed and the step goes on |

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
"updating", "failed", "down"}]}`, where `state` is `converged`, `blocked` or `in_progress`. It
takes CI's OIDC token and refuses the operator token (the operator's read is `GET /v1/status`).
It is computed from the heartbeats and the desired state, and stores nothing. `converged` needs
the coordinator to be past its 30 second quiet start, at least one live host, every live host at
the version and none updating or failed, and `min_up` hosts of the storage and frontend roles up,
so a coordinator that has just restarted does not report a farm it has not heard from as
converged.

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
