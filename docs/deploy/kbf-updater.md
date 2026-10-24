# kbf-updater: rolling a version over a farm

`kbf-updater` rolls one version of the `kbf` package over the hosts of a farm, one host at a
time, behind kbf-proxy's local socket. It runs on the frontend host. It installs through the
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

### A frontend that is a member of the kbf-proxy pool

A `frontend` line with two more words after the key is one member of a pool that kbf-proxy
balances over (pool mode, the `[[proxy.frontend]]` tables of [the proxy's API](../design/kbf-proxy-api.md)):

| word | meaning |
|---|---|
| `instance=<name>` | the name the proxy knows the member by (`name` of its `[[proxy.frontend]]` table) and the unit instance the host restarts for it: `kbf-frontend@<name>.service`. `-` is the plain unit `kbf-frontend.service`. Otherwise `[a-z][a-z0-9-]{0,31}` |
| `base=<port>` | the first of the member's five ports (`base_port` of the table), 1024 to 65531. The member's operator API is at `base` plus 2 on loopback |

- There is one line per member, and every line lists the same address and host key: every
  member runs on the host the updater runs on, and the retire and the commit go to the member
  on loopback while the install and the restart go over ssh to the address, so a member that
  lists another address or host key than an earlier one is refused.
- No two members share an `instance`, and their bases are at least 5 apart (a member uses
  `base` to `base` plus 4), or the file is refused.
- A pool of members and a `frontend` line without these words cannot be in one file, and a
  second `frontend` line without them is refused as before. A `frontend` line without them is
  not in a pool, nothing can move traffic away from it, and a roll that reaches it fails
  there (`swap_abandoned`) before any ssh call.
- `instance=` and `base=` are accepted on a `frontend` line only.

## The flags

All are flags; none is read from the environment.

| flag | |
|---|---|
| `--state-dir`, `--inventory`, `--proxy-uid`, `--token-file`, `--ssh-key-file`, `--ssh-user`, `--channel` | required |
| `--operator-api http://<loopback ip>:<port>` | required: the frontend's operator API, for safe-to-stop and the health of storage and daemon hosts. Not a loopback address is refused |
| `--operator-proxy-auth-file` | the proxy's per-boot secret, for a frontend that runs behind kbf-proxy |
| `--proxy-admin http://<loopback ip>:<port>` | the admin API of the kbf-proxy in pool mode (`admin_listen`). Required when the inventory lists pool members; refused unless it is a loopback address. It asks for no credential, and none is sent |
| `--socket` | the roll API's socket, for tests and development; in production systemd passes it |
| `--ssh-connect-timeout-secs`, `--ssh-command-timeout-secs`, `--drain-deadline-secs`, `--apply-poll-secs`, `--apply-deadline-secs` | optional bounds in seconds; they default to 10, 900, 600, 5 and 600 |

The updater reaches each member's operator API on loopback at `127.0.0.1:<base + 2>`, with the
same token (`--token-file`) and proxy secret as `--operator-api`.

## What a roll does with a frontend pool member

A roll takes the members before every other host, one member at a time, and does not begin one
until the one before it is back in the pool. For a member the turn is:

1. **Check.** The proxy's list (`GET /v1/proxy/upstreams`) must show some other member up and
   ready. If none is, the roll fails with `unsafe_to_stop` and nothing has been changed: the last
   member that is up is never taken out. The member itself need not be up: one that is drained
   or not ready is taken out of nothing, and is rolled while one other member serves.
   (`unknown_member` if the proxy does not list the member, `safe_to_stop_unavailable` if the
   list cannot be read.)
2. **Drain** (`draining`). `POST /v1/proxy/upstreams/<instance>:drain`: the proxy gives the member
   no new stream. The roll then waits until the REAPI and operator streams the proxy shows open
   on the member are 0, for at most 900 seconds (`drain_failed` if they are not).
3. **Retire** (`retiring`). `POST /v1/frontend/retire` on the member's operator API: the member
   hands its work to the other frontends, and the roll waits until the member reports no stream
   open, for at most 900 seconds (`retire_failed`). The call is answered when the retire has
   ended, so it is given 120 seconds to be answered.
4. **Install** (`installing`). The package is installed on the host through `kbf-host-install`,
   unless the host already has the target. An install the host reports as in progress is waited
   for, for at most 900 seconds (`install_failed`), and not run a second time beside it. The
   package covers every member of the host, so this happens once per host, at the first member.
5. **Restart** (`restarting`). `restart frontend=<instance>` through `kbf-host-install`
   (`restart_failed`).
6. **Ready** (`waiting_healthy`). The roll waits until the proxy lists the member ready and the
   member's `GET /healthz` reports a `commit` that the target's version names, for at most the
   frontend health bound, 300 seconds (`health_timeout`; `swap_abandoned` if the version names no
   commit or the proxy drops the member).
7. **Undrain** (`undraining`). `POST /v1/proxy/upstreams/<instance>:undrain` (`undrain_failed`).

Every failure after the check stops the roll. One after the proxy has drained the member **leaves
it drained**: the detail names it (`frontend member a (fe-a) is left drained`). A drain the proxy
refuses says only that, and the member may not be drained. The updater never undrains a member it
could not show to be healthy. An operator looks at the member, and either fixes it and starts
another roll (a member found drained is rolled again as long as another member is up) or puts it
back by hand:

- A failure before the retire (the check, the drain, the wait for streams) leaves a member that
  still serves its work as before, and `POST /v1/proxy/upstreams/<instance>:undrain` puts it back.
- A failure at the retire or after it may leave a member that is **retired**: it refuses new work
  and is not ready until its unit is restarted, and undraining it would put a member that does not
  serve back in the pool. Restart the unit (`restart frontend=<instance>` of `kbf-host-install`)
  or start another roll, which finishes the member. Undrain by hand only once the member is ready
  and runs the target.

### A restart of the updater

Each phase above is written to `roll.json` before its work begins. A roll found `running` after
a restart is resumed with `drive`: the member in hand is drained again (a drain does nothing to a
member that is already out), and the turn goes on from the phase the record holds. A member found
in `installing` asks the host what it has and installs only if it does not have the target
(waiting out an install it reports as in progress); one found in `undraining` is drained again
and put back.

A `roll.json` written by an earlier build, which has none of the phases `draining`, `retiring`,
`restarting` and `undraining`, is read as it was: no field was added, and the format is still 1.
The other way does not hold: an earlier build does not know these phases and refuses to start on
a `roll.json` that holds one, so finish or abort a roll that is in a member's turn before
downgrading the updater.

## The agent mode

`kbf-updater agent` is the other mode of this program, and the one the maintainers' design
([../design/updates.md](../design/updates.md)) ends on: an unprivileged updater on every host that
dials the coordinator and owns that host's update. The roll mode above is unchanged and keeps
working (until the roll and its ssh path are deleted, in the last step of that design).

```text
kbf-updater agent --coordinator <dns name>:<port> --channel NAME
                  (--state-dir DIR | --client-cert FILE --client-key FILE --ca FILE)
                  [--frontend <dns name>:<port>]
                  [--operator-api http://IP:PORT --token-file FILE
                  [--operator-proxy-auth-file FILE]] [--helper-socket PATH]
                  [--heartbeat-secs N] [--drain-deadline-secs N] [--health-timeout-secs N]
                  [--health-poll-secs N] [--retry-secs N]
```

- `--coordinator`: a DNS name and a port. The coordinator's certificate must chain to the farm CA
  for that name and have the role `coordinator`; that is checked after the handshake and before
  anything is sent.
- `--state-dir`: the directory `kbf-updater enroll` filled ([below](#enrolling-a-host)): the key,
  the certificate and the CA are its files. The agent renews the certificate at half its life
  and writes the new one there; `--frontend` is where. The certificate is the farm CA's `updater`
  certificate: its one DNS name is this node's id and its `urn:kbf:host-role:` is what the host
  runs (`storage`, `daemon` or `mac-daemon`; a `frontend` is refused, the roll updates frontends).
  The agent reads both from it. The files are read for every heartbeat and every call to the
  frontend, so a renewed pair is used from the next one, if it names the same node and role as the
  pair the agent started with (a pair for another node is refused until the agent is restarted).
  The key file must be a regular file of the agent's user with mode 0600 or 0400, as the token
  file must.
- `--client-cert`, `--client-key`, `--ca`: instead of `--state-dir`, files an operator placed.
  Nothing renews them. Not both.
- `--frontend`: the frontend's public address, a DNS name and a port (the kbf-proxy). The node's
  drain, undrain and status are calls there, made with the updater's certificate and **no token**;
  see [the boundary](#the-boundary). It is required with `--state-dir`.
- `--channel`: the channel of the helper's allowlist. The version the coordinator names is looked
  up in it by the helper, never fetched by URL.
- `--helper-socket`: the socket of `kbf-host-install --serve` (default
  `/run/kbf-host-install/local.sock`; [host-install.md](host-install.md#the-local-socket)).
- `--operator-api`, `--token-file`, `--operator-proxy-auth-file`: optional, together: the full
  operator token over the frontend's loopback, an opt-in fallback used instead of the
  certificate; see the boundary below.
- Times, in seconds, 1 to 86400: `--heartbeat-secs` 15, `--drain-deadline-secs` 600,
  `--health-timeout-secs` 300, `--health-poll-secs` 5, `--retry-secs` 900.

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
2. **Drain** the node at the frontend (`:drain` of the operator API, as the roll mode does for a
   daemon host, with the updater's certificate through the proxy; a storage host is drained too, which does nothing when it runs no daemon) and wait for its leases, for at most
   `--drain-deadline-secs`.
3. **Switch** (`switch`): the helper puts X on trial and restarts the units.
4. **Health**: the node's health check (`node_health`: a storage member is `active` on the build and no blob is known to be owed a copy, a daemon is connected on it) of X, held drained, for
   at most `--health-timeout-secs`, and never past the helper's deadline less ten seconds.
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
renewal is refused and the certificate runs out at its end. The proxy does not read the deny list,
so a certificate already issued keeps its four own-node calls (drain, uncordon, status, storage health) until it
expires: seven days by default, thirty at most. A host re-enrolled with a `replace` token stops
the old key from renewing in the same way and does not cut off a certificate already issued.
The `deadline_secs` of a self-drain is whatever the host sends; it only moves that host's own
drain deadline.

The CA's key is the frontend's alone; the coordinator holds none. The units in
`packaging/host-agent` use this layout ([its README](../../packaging/host-agent/README.md)).

### The boundary

The design says the full operator token must not be on hosts, and the agent does not need it. The
drain of a node (`NodeDrain`: cordon, drain, uncordon) and the node's status for its health are
calls to the operator API, which is on the frontend's loopback. The agent makes them through the
frontend's public proxy with its updater certificate: **the proxy admits, for that certificate,
`POST /v1/nodes/{id}:drain`, `POST /v1/nodes/{id}:uncordon`, `GET /v1/nodes/{id}` and
`GET /v1/nodes/{id}/storage-health` for the certificate's own node id, and nothing else** (another node's id, the cordon, a listing, the update
API and everything else is `403`; so are the worker and build-cache paths whatever `buck2_auth`
says), and puts the operator token in itself. The shipped units give the agent no token. The proxy's certificate for the name in `--frontend` must be the farm CA's `frontend` certificate (the agent checks the chain to the farm CA, the name and the role before it sends a byte), not a public one.

What remains of the token: `--operator-api` and `--token-file` (and the proxy secret file) are an
opt-in fallback for a host whose operator accepts the token there (anything running as the agent's
account can then read it); given, they are used instead of the certificate. A **storage** host
needs no token: its health is read from `GET /v1/nodes/{id}/storage-health`
([../api.md](../api.md#get-v1nodesidstorage-health)), the node's own member (state and build
commit) and two numbers about the storage as a whole, which the proxy lets its certificate read for
its own node id only. The host is healthy when its member is `active` on the target's commit, at
least one member is active, and no blob is known to be owed a copy (`under_replicated_blobs` is
zero, or `null` while it cannot be told, which is not this host's to wait for). A storage host is
drained first like a daemon host: it may run a daemon (`storage-and-daemon`), whose leases a restart
would cut off, and a node the frontend does not know has no daemon, which is nothing to drain.
Without `--frontend` and without the token an agent refuses every update order before it fetches
anything. The frontend must be of the release that has the route (an older proxy answers `403`
and the host's health stays unread, so the update is reverted): update the frontends first for that
release, or let the roll update the storage hosts.
