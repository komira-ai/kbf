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
st1      st1.farm.example      storage     ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample
dm1      dm1.farm.example      daemon      ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIExample  dm1
```

The roles are `storage`, `daemon` and `mac-daemon`, and `frontend`, which the roll refuses. A
daemon host lists the node id of its kbf configuration as the sixth word.

### Frontends are not rolled

A frontend is a container that [`kbf-host-install`](host-install.md) drains and recreates, and the
updater does not roll one. A `frontend` line still parses (at most one), and a roll that reaches it
fails there (`swap_abandoned`) before any ssh call, so list storage and daemon hosts only.

The words `instance=` and `base=`, which listed a frontend as a member of a pool that the proxy
balanced over, are refused with the line number: that turn of a roll (drain at the proxy, retire,
restart, undrain) is gone, and with it `--proxy-admin`, the phases `draining`, `retiring`,
`restarting` and `undraining` and the failure codes `retire_failed` and `restart_failed`. A
`roll.json` that still holds one of those phases is not read, so the updater does not start on it
(fail closed): finish or abort that roll with the build that wrote it, or remove the file, before
the update.

## The flags

All are flags; none is read from the environment.

| flag | |
|---|---|
| `--state-dir`, `--inventory`, `--proxy-uid`, `--token-file`, `--ssh-key-file`, `--ssh-user`, `--channel` | required |
| `--operator-api http://<loopback ip>:<port>` | required: the frontend's operator API, for safe-to-stop and the health of storage and daemon hosts. Not a loopback address is refused |
| `--socket` | the roll API's socket, for tests and development; in production systemd passes it |
| `--ssh-connect-timeout-secs`, `--ssh-command-timeout-secs`, `--drain-deadline-secs`, `--apply-poll-secs`, `--apply-deadline-secs` | optional bounds in seconds; they default to 10, 900, 600, 5 and 600 |

## The agent mode

`kbf-updater agent` is the other mode of this program, and the one the maintainers' design
([../design/updates.md](../design/updates.md)) ends on: an unprivileged updater on every host that
dials the coordinator and owns that host's update. The roll mode above is unchanged and keeps
working (until the roll and its ssh path are deleted, in the last step of that design).

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
  runs (`storage`, `daemon` or `mac-daemon`; a `frontend` is refused, the roll updates frontends).
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
- `--helper-socket`: the socket of `kbf-host-install --serve` (default
  `/run/kbf-host-install/local.sock`; [host-install.md](host-install.md#the-local-socket)).
- `--operator-api`, `--token-file`: optional, together: the full
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
