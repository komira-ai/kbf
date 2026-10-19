# The operator API

`kbf-server` serves an HTTP/JSON API under `/v1` for the Fleet UI, scripts and
clients ([fleet-updates.md](design/fleet-updates.md) section 11.4), and
[`/healthz` and `/readyz`](#get-healthz-and-get-readyz) for a front's health check (the REAPI
listener answers the same readiness over [`grpc.health.v1`](#grpchealthv1-on-the-reapi-listener)), and
[`/metrics`](#get-metrics), the cache's upload-path metrics in the Prometheus text format. It listens on its
own address, given with `--api-listen`; without the flag there is no API. The start
line then ends with ` api=<addr>`. The listener speaks HTTP/1.1, and HTTP/2 in clear
text (h2c, with prior knowledge); every rule below holds for both.

Reads answer anyone who reaches the address: bind it where only operators do. (Behind the farm's proxy, `--proxy-auth-file`, only a loopback peer reaches it at all: [Who may write](#behind-the-farms-proxy).) The
exceptions are the [enrollment routes](#v1enrolltokens-and-v1enrollnodes), which pass every
gate of a write even to be read, and the build pages, [`GET /v1/invocations/{id}`](#get-v1invocationsid-and-below)
and below, and [`GET /v1/blobs`](#get-v1blobsdigest), which run the REAPI policy and are
off without it. Writes need a token (see [Who may write](#who-may-write)). The roles of
[fleet-updates-security.md](design/fleet-updates-security.md) section S9 (`admin` for
operators, a short-lived `rollout` for CI) are **planned**; the token is the one
operator credential until they land.

Every body is `application/json`, except a blob's bytes and `/metrics`. What the server keeps here is
**in memory**, like the scheduler's state: a restart forgets it, and each daemon
reports again after its next `Welcome`.

## Who may write

A write (cordon, drain, uncordon, a member's state, a deny list entry) can take every node in the fleet out of service, so
it must come from an operator. Where a request comes from does not show that:

- **Every server host is also a worker.** Build actions run on it, and they can reach
  loopback: the native driver on Linux enforces no network isolation, and the macOS
  sandbox allows loopback.
- **A reverse proxy on the host** (`tailscale serve`, nginx) makes every caller it
  forwards a loopback peer.
- **A browser on the host** can POST to a loopback address from any page it opens,
  without a preflight, if the API accepts a "simple" request (`text/plain` or a form
  body, or no body).

So a write must pass every one of these gates, in this order (the loopback gate is first
because the token must not cross a network, and it is not an identity: it never lets a
write through without the token):

| Gate | Refused with |
|---|---|
| the peer is loopback, so the token never crosses a network in clear; with `--proxy-auth-file`, the request must also present the proxy's secret (see [Behind the farm's proxy](#behind-the-farms-proxy)) | `403` |
| the request has no `Origin` header (browsers send one with a cross-origin POST) | `403` |
| the server was started with `--api-token-file` (without it, writes are off) | `403` |
| `Authorization: Bearer <token>` with the file's token (the SHA-256 digests of both are compared in constant time, so neither the token's bytes nor its length show through timing) | `401`, with `WWW-Authenticate: Bearer` |
| `Content-Type: application/json` (parameters allowed), which a page cannot send cross-origin without a preflight the API never answers | `415` |

**The token file.** `--api-token-file <path>` names a file holding one token of at
least 32 visible ASCII characters (surrounding whitespace, such as the trailing
newline, is dropped), in at most 4096 bytes; `openssl rand -hex 32` makes one. The
file must be a regular file (a FIFO or device is refused at once, without waiting on
it) owned by the server's user with mode exactly `0600` or `0400`; otherwise the
server refuses to start. The token is read once at start: it is never on the command
line, in the start line or in a log, and a change to the file takes effect at the
next start.

**Run `kbf-server` as its own user.** Keep the token out of the build actions'
reach: a build action that can read the file can write. On a host that is also a
worker, `kbf-server` must run as a different user from `kbf-daemon` and from every
user leases run as. The native driver runs actions as the daemon's user, so a `0600`
token file owned by a user the server shares with the daemon is readable by every
build on that host.

**Through a reverse proxy.** A remote operator reaches the API through a proxy on the
server's host that terminates TLS (`tailscale serve`, nginx). The proxy is what makes
the loopback gate pass, so the token is what stands between the network and the
writes: forward the `Authorization` header unchanged, and expose only what the
operators' network should reach. A proxy that adds an `Origin` header to every
request turns writes off. A server started without `--proxy-auth-file` cannot tell such
a proxy from a local process: both are loopback peers. The farm's own proxy (below)
is told apart by a secret.

### Behind the farm's proxy

`--proxy-auth-file <path>` says the server sits behind the farm's TLS-terminating proxy
(`docs/design/frontend-proxy.md`, section 3). The file holds a per-boot secret of exactly
32 bytes, owned by the server's user with mode `0600` or `0400` (the token file's checks);
the server refuses to start otherwise. It is read at start, and never taken from the
command line or the environment. The proxy presents the 32 bytes as 64 lower-case hex
digits in `x-kbf-proxy-auth`. Then, on the operator API, the worker listener and the REAPI
listener:

- A request from an address that is not loopback is refused before anything reads it
  (`403` on this API; `UNAUTHENTICATED` on gRPC).
- A loopback request that does not present the secret, or presents a wrong one (compared in
  constant time), is a local caller: its `x-kbf-*` headers mean nothing. On this API it may
  read, as before, but **a write is refused `403` even with the token**: a write must come
  through the proxy.
- A loopback request that presents the secret is the proxy's. A write still needs every
  other gate, the token included: the secret says who forwarded the request, not that the
  caller may write.
- Every `x-kbf-*` header is removed from every request, whatever the result, so nothing
  past this check can read one. The proxy sets `x-kbf-node-id` and `x-kbf-role` for a call
  whose client certificate it verified; the worker listener takes those (and no TLS flag)
  in place of a certificate: the role table, the `Hello.node_id` binding, the blob calls
  and `Membership`'s role check use them. A `Hello` whose node id is not the proxy's
  `x-kbf-node-id` is refused `PERMISSION_DENIED`. A `Membership` registration
  or heartbeat has no certificate names behind the proxy: the one name is the proxied node
  id, so the host of its instance id and its address must be that name (up to ASCII case; an IP
  address is refused). The proxied role must be `storage`.

What the flag does not do, and what the proxy must do:

- **Deny-list entries by `serial` or `spki-sha256` do nothing behind the proxy.** The child
  sees no certificate, only the node id and role the proxy forwarded, so only a `node`
  entry in `--worker-deny-list` can name a proxied caller. To revoke a certificate, list
  its node, or have the proxy refuse the certificate. A server started with
  `--proxy-auth-file` and a deny list holding a `serial` or `spki-sha256` entry logs a
  warning at start naming the count. (Enforcing them would need the proxy to forward a
  verified serial and key in a new header; that is the proxy's work, not this flag's.)
- The storage listener (`kbf storage`, and the in-process one of all-in-one) is not behind
  the proxy: it keeps its own mutual TLS and its own admission.
- `--reapi-auth-policy mtls` finds no certificate on a proxied REAPI call and refuses every
  call; use another policy behind the proxy until the proxy forwards the identity to it.
- With the flag, `/metrics` and `/healthz` answer a loopback peer only, and a local `curl`
  write is refused (see above).
- The server cannot tell the proxy's `x-kbf-node-id` from a client's if the proxy forwards
  a client's headers: the proxy must remove every client-supplied `x-kbf-*` header before
  it adds its own. A call from a Unix-socket peer has no address and is refused. The
  secret file must be owned by the user the server runs as, so the proxy and the server run
  as one user.

Without the flag, nothing changes: the headers do nothing (they are removed all the same),
and the gates are the ones in the table.

## `GET /v1/nodes`

The server that answers, then every node registered since it started, in node-id
order, with the protocol version it speaks, its software and where it is in
placement, and, after the list, the oldest protocol version among them:

```json
{ "server": { "version": "0.1.0+0123456789ab", "commit": "0123456789ab" },
  "nodes": [
  { "node_id": "mac-1", "connected": true, "protocol": 1, "software": {
      "os_name": "macOS", "os_version": "15.1", "os_build": "24B83", "kernel": "",
      "daemon_version": "0.1.0", "xcode_builds": ["15F31d", "16C5032a"],
      "xcodes": [
        { "app": "/Applications/Xcode_15.4.app", "build": "15F31d", "state": "ready",
          "reason": "", "fix": "" },
        { "app": "/Applications/Xcode_16.1.app", "build": "16B40",
          "state": "license_not_accepted",
          "reason": "/usr/bin/xcodebuild -license check exited with exit status: 69: You have not agreed to the Xcode license agreements.",
          "fix": "sudo /Applications/Xcode_16.1.app/Contents/Developer/usr/bin/xcodebuild -license accept" },
        { "app": "/Applications/Xcode_16.2.app", "build": "16C5032a", "state": "ready",
          "reason": "", "fix": "" } ],
      "container_images": [],
      "received_at_unix_ms": 1791370000000 },
    "needs_attention": [
      "Xcode 16B40 (/Applications/Xcode_16.1.app) installed but not ready: /usr/bin/xcodebuild -license check exited with exit status: 69: You have not agreed to the Xcode license agreements.; fix: sudo /Applications/Xcode_16.1.app/Contents/Developer/usr/bin/xcodebuild -license accept" ],
    "placement": { "state": "draining", "deadline_unix_ms": 1791371800000,
                   "leases": ["117399224320012061.42"] } },
  { "node_id": "old-1", "connected": false, "protocol": null, "software": null,
    "needs_attention": [], "placement": { "state": "serving" } }
],
  "oldest_protocol": 1, "protocol_unknown": 1 }
```

| Field | Meaning |
|---|---|
| `server.version` | the `kbf-server` build that answers, as `--version` and the start line print it: the package version, `+`, and the commit |
| `server.commit` | the commit it was built from, 12 hex digits (`git rev-parse --short=12`), or `unknown` for a build without a git checkout |
| `node_id` | the id its daemon registered with |
| `connected` | whether its newest stream is still open |
| `protocol` | the `kbf.worker.v1` version the `Hello` that opened its newest stream named, which the server answered in `Welcome` ([worker-protocol.md](design/worker-protocol.md)); `null` if that `Hello` named none (the field is 0, which is not a version). A `Hello` resent on the stream does not change it, and a node that returns on another version shows the new one. It stays after the stream closes: it is the version the node last spoke |
| `oldest_protocol` | the least `protocol` among the listed nodes whose `protocol` is not `null`, connected or not (a node whose stream closed may return on the version it last spoke); `null` if no listed node names one. Nodes that name none are not in it |
| `protocol_unknown` | how many listed nodes have a `null` `protocol`. They are never counted in `oldest_protocol`, so `oldest_protocol` says "every node but these speaks at least this": a reader that needs every node reads it with `protocol_unknown` equal to 0 |
| `software` | the newest `NodeStatus` it sent ([worker-protocol.md](design/worker-protocol.md#nodestatus)); `null` from a daemon that predates it. An empty string or list is a value the node could not read |
| `software.received_at_unix_ms` | when the server received it, by the server's clock |
| `software.xcode_builds` | the Xcode builds actions can use on it: only these route work |
| `software.xcodes` | every installed Xcode, ready or not: `app`, `build` (empty if not known), `state` (`ready`, `license_not_accepted`, `first_launch_not_run`, `metal_toolchain_missing`, `failed`, `not_surveyed` from the daemon's start until its first survey of its Xcodes ends (never advertised; the daemon sends its status again when the survey ends), or `unknown` for a state this server does not know), `reason` (the check that failed and its answer, from the daemon's last survey that changed this Xcode's `DEVELOPER_DIR`, build or state; a survey that differs only in its reason is not sent, so when another check later fails with the same build and state, this is still the earlier check's text) and `fix` (the command an administrator runs on the node, when one is known). Empty from a daemon that predates it |
| `software.container_images` | the container images the node was pinned to (`kbf-daemon --image`, container driver), as `docker://<repo>@sha256:<digest>`, sorted: each was in its image store, as one architecture's manifest, when the daemon started (a daemon that finds one missing does not start). Status only: no work is placed by image. Empty when none is pinned, on a Mac, and from a daemon that predates it |
| `update` | where its update is: `{"state", "up_to_date", "availability"}` and nothing more (the commits and the refusal text are on [`GET /v1/update/hosts`](#get-v1updatehosts)). `state` is the host's step state from its newest `UpdateStatus` (`idle`, `fetching`, `staged`, `activated_unconfirmed`, `confirmed`, `reverted`, `refused`, `manual`, or `unknown` for a state this server does not know), `null` unless `availability` is `ok`. `availability`: `unsupported` if the node never sent an `UpdateStatus` (never shown as idle), `unavailable` if it sent one but its newest stream is closed or has not sent its own yet (a restarted daemon is not judged by its previous process), `ok` otherwise. `up_to_date` is `true` or `false` against the desired commit of `GET /v1/update`, and `null` whenever the server cannot judge: no desired release (or no `--state-store`, or a row that cannot be read), a host that is not `ok`, or a running commit that is not a hex prefix of 12 digits or more |
| `needs_attention` | what an operator must do on the node, one line per item: today each installed Xcode that is not ready, as `Xcode <build> (<app>) installed but not ready: <reason>; fix: <command>`. An Xcode `not_surveyed` keeps the item the same app had in the node's previous status (as when a restarted daemon sends its first status), until a survey reports it; one with no previous item has none. The server also logs each item once at `WARN` (target `kbf_server::attention`) when it appears, and at `INFO` when a status reports it gone (its Xcode ready, changed or removed); a status that repeats an item, or lists its Xcode `not_surveyed`, logs nothing. An item is the same while its Xcode's app, build, state and fix are, so a reason that changes alone is shown here but not logged again. The previous status is kept in the server's memory: after a server restart, each item is logged again when its node reports it; kbf has no alert delivery yet (issue #189) |
| `placement.state` | `serving`; `cordoned` (no new lease, its leases run on); `draining` (cordoned, waiting for its leases until `deadline_unix_ms`); `drained` (cordoned, no lease left: either its leases ended, or the node disconnected and, after the lease grace, its leases were given up and requeued to run elsewhere; check `connected`); `drain_paused` (the deadline passed with leases still running: they run on, and nothing proceeds until an operator acts) |
| `placement.leases` | while draining or paused: the leases it still holds, as `term.seq` (each server process has its own term, the Raft term under the Raft metadata log: [worker-protocol.md](design/worker-protocol.md#server-restarts-and-the-lease-epoch)) |

The list is the nodes this server process has seen, so it says nothing of a daemon that has
not connected to it yet: a server that has just started, or that took over from another
(the frontend swap, [design/frontend-proxy.md](design/frontend-proxy.md)), lists only those
that have registered since. `oldest_protocol` and `protocol_unknown` describe that list and
no more. The check that a release may stop accepting the oldest version
([design/frontend-proxy.md](design/frontend-proxy.md) section 9, rule R1) reads them; it is
not made by this server.

## `GET /v1/members`

The storage members of the current map, as the `Membership` service keeps them
(`kbf.map.v1.Membership`; [architecture-v2.md](design/architecture-v2.md) section 6),
in the map's order. A read, open like `GET /v1/nodes`; it takes no write. Storage
members are not the daemon nodes of `/v1/nodes`: a storage node registers with a
frontend, a daemon opens a worker stream.

```json
{ "generation": 7,
  "members": [
    { "id": 1, "addr": "192.0.2.11:8982", "state": "active", "weight": 1, "protocol": 1 },
    { "id": 2, "addr": "192.0.2.12:8982", "state": "draining", "weight": 1, "protocol": 2 },
    { "id": 3, "addr": "192.0.2.13:8982", "state": "down", "weight": 2, "protocol": null }
],
  "oldest_protocol": 1, "protocol_unknown": 1 }
```

| Field | Meaning |
|---|---|
| `generation` | the map's generation; `0` with an empty `members` before the first member has registered |
| `id` | the member id the store assigned at registration (the `member_id` of `RegisterResponse`) |
| `addr` | the address callers dial |
| `state` | `joining` (registered, a write owner, read last), `catching_up`, `active` (a full owner), `draining` (takes no new writes) or `down` (its lease lapsed; kept in the map for the dwell, then it leaves the list) |
| `weight` | its share of the digests relative to the others |
| `protocol` | the protocol version in the `RegisterRequest` that wrote the member's row (a heartbeat and a state change keep it; a new registration of the member replaces it). `null` when the row names none, as a row written before this field existed, and for a `down` member: its row expires with its heartbeats, and the version went with it |
| `oldest_protocol` | the least `protocol` among the members whose `protocol` is not `null`; `null` if none is |
| `protocol_unknown` | how many members have a `null` `protocol`. Never counted in `oldest_protocol`: read the two together, as for [`GET /v1/nodes`](#get-v1nodes) |

The registration is judged against the map's floor once, when it is made
([design/frontend-proxy.md](design/frontend-proxy.md) section 9); `protocol` is what it
named, and nothing judges it again.

A registered member is `joining` until something promotes it with `Members::set_state`;
the server does not promote on its own yet, so a script that waits for `active` waits
for that. A server without a state store keeps no members and answers `404`; a state
store that cannot be read answers `503`. The view reads the map and then the member rows
(for `protocol`), so a stored map or a stored member row that does not decode answers
`500` for the whole view, not a list without that member's version: the store is up and
its content is wrong, as it is for the other readers of those rows.

## `POST /v1/nodes/{node}:cordon`, `:drain`, `:uncordon`

An operator takes a node out of placement, drains it, or returns it
([fleet-updates.md](design/fleet-updates.md) sections 3.3 and 4.2):

- `:cordon` (`POST /v1/nodes/{node}:cordon`): the scheduler offers the node no new lease; its leases run on. A drain
  already under way is kept.
- `:drain` (`POST /v1/nodes/{node}:drain`): cordons the node and waits for its leases to end. The body may be
  `{"deadline_secs": N}`; without a body the deadline is 30 minutes. With no lease
  left the node is `drained`. If the deadline comes first the drain **pauses**: the
  leases run on, nothing is killed or given up, and the node stays `drain_paused`
  even after they end, until an operator drains it again (a new deadline) or
  uncordons it.
- `:uncordon` (`POST /v1/nodes/{node}:uncordon`): returns the node to placement and ends any drain.

The answer is the node, as `GET /v1/nodes` lists it, after the action. `404` for a
node that never registered or an unknown verb, `400` for a drain body of another
shape, and `401`, `403` or `415` from the gates of [Who may write](#who-may-write).

A cordon names the node, not its stream: a node that reconnects (say, after a reboot)
is still cordoned. Work that only cordoned nodes could run waits, and its callers are
told so (`every live worker that can run it is cordoned: <nodes>`). It is **not**
refused for it, however long the cordon lasts: a cordon is temporary by intent, and
refusing would fail builds. An uncordon places it at once. The scheduler's unservable
wait (`--unservable-wait-secs`) still refuses work that no connected node, cordoned
or not, could run, and the time spent waiting for a cordon does not count toward it.
A server restart forgets every cordon, as it forgets the rest of the scheduler's
state, and every start logs a warning that says so. The protocol has no drain
message yet: the daemon is not told, and the server simply sends it no new `Start`.

## `POST /v1/members/{id}`

An operator sets a storage member's state. The body is `{"state": "active"}` or
`{"state": "draining"}`; the answer is `{"id": N, "state": "...", "generation": G}`,
the map's generation after the change. `down` is the store's verdict, `catching_up` is what a registration says and `joining` is
the join handoff's, so none of them is accepted (`400`, as is
any other body). `404` for an id that has no live row (it expired, or never
registered) or is not a number, or on a server without a state store; `503` when the
state store cannot be written; `401`, `403` or `415` from the gates of
[Who may write](#who-may-write), the same gates as the node writes. Nothing is changed
by a refused request.

When a member becomes `active` the cluster also records that it has formed (see below),
in the same transaction.

**When a member is `active` without this call.** A registration that says the node's
index is whole enters `active`, in a fresh cluster and in a formed one alike: it is the
first start of a cluster, or a restart of a node whose meta log already has its rebuild,
so a rolling restart leaves every member `active`. The registration also writes the
`meta/members_formed` mark. A registration that says the node is rebuilding its index
enters `catching_up` and becomes `active` by its own first beat that says it is done. No
registration writes `joining`; that state is set by the join handoff in code
(`Members::set_state`), and a `joining` member is promoted by this call.

**The risk of promoting a joiner.** `active` makes the member a settled owner: it
takes the rank-1 slots of the digests it wins in the rendezvous order, and it is read
in rank order. Nothing here copies the blobs of that slice to it (the join handoff,
`architecture-v2.md` section 8.4, is not built). What the router does about it, from
`kbf-route`'s read walk and `kbf-map`'s `read_order_with_previous`:

- A read that gets `NotFound` from an owner goes on to the next owner in the read
  order, and the answer is `NotFound` only if every owner said so. So a promoted
  joiner that lacks a blob costs one extra round trip per read of it, not a failed
  read.
- That holds only while an owner that has the blob is still in the list. The list is
  this generation's top `r` settled owners, then the write owners not yet listed, then
  the previous generation's list: one generation back, no more. A joiner that wins a
  digest's top `r` pushes an old copy-holder out of it, and once the map moves on
  again (any member change bumps the generation) that holder is in no list: the blob
  is read as missing, and the action is rebuilt or the upload repeated.
- Writes of that slice go to the joiner as primary (the chain head and the
  `FindMissing` target): it reports every blob it has not seen as missing, so
  uploads that were deduplicated are sent again until the slice fills.

So promote a joiner when its slice is expected to be empty or its blobs are cheap to
rebuild (a new cluster, a cache), or have it hold the slice first; and promote one
member at a time. `draining` is the reverse: the member takes no new writes, its
successor takes its slot at once, and it is read first.

## `GET /v1/denied`, `POST /v1/denied` and `DELETE /v1/denied`

An operator adds an entry to the storage deny list, or takes one off. A denied
certificate is refused by every storage node on every method, and by the membership
service ([architecture-v2.md](design/architecture-v2.md) section 9 and section 6,
Admission). An entry names:

| Entry | Names |
|---|---|
| `serial <hex>` | the certificate with that serial (colons, case and leading zeros ignored) |
| `spki-sha256 <64 hex>` | every certificate with that public key, whatever its serial (`openssl x509 -noout -pubkey \| openssl pkey -pubin -outform DER \| sha256sum`) |
| `node <id>` | every certificate that carries `<id>` as a DNS name, compared up to ASCII case |

A `node` entry names DNS names only. A certificate's IP names are never compared with it,
nor is a name with a trailing dot or a port, so a `node` entry that holds an IP address or a
colon (an IPv6 address, or a member's `host:port`) is `400`: list that certificate by
`serial` or `spki-sha256`, which also name a certificate that has no DNS name.

`POST /v1/denied` takes `{"entry": "node mac-07", "reason": "retired"}`. Both fields are
required (an empty reason is `400`: a deny is never anonymous), and any other field is `400`. The
entry is spelled as a line of the deny list file is; it is stored in its canonical form (a
serial in lower case without colons or leading zeros, a node id in lower case), so one entry
is one row whatever it was typed as (`node Mac-07` and `node mac-07` are one entry, and
either spelling takes it off). The reason is at most 512 bytes and the entry's value at most
253, with no control character in it; the list
holds at most 4096 entries (`400` past any of them: take one off first). The answer is
`{"entry": "serial a1b", "generation": G}`: the entry as stored, and the generation of the
map that holds it, which the call builds before it answers (`0` before any member has
registered; the entry is in the first map then). An entry that is listed already answers
`200` and keeps its first reason.

`GET /v1/denied` lists the entries with the reasons they were added with:
`{"denied": [{"entry": "serial a1b", "reason": "a lost laptop", "at_ms": 1791371800000}, ...]}`,
serials first, then key hashes, then node ids, each in order, as the store's rows hold them
now (the map the nodes hold lags the rows by up to a beat, and holds no reasons). `at_ms` is
the store clock's milliseconds, Unix time on a running frontend. The list is empty
(`{"denied": []}`) when nothing is denied. It passes the same gates as a write, the `GET`
included, so the reasons reach only an operator, and its answer is `Cache-Control: no-store`.
It assumes no single writer: every frontend over the store answers from the same rows, a
frontend that is not the newest included. `404` on a server without a state store.

`DELETE /v1/denied?entry=<entry>` takes the entry off (percent-encode it, or use `+` for the
space): `200` with the same answer, `404` if it is not listed, `400` with no `entry` or one
that is not an entry.

Both need a token, a loopback peer (one that presents the proxy's secret, with `--proxy-auth-file`), no `Origin` and `Content-Type: application/json` (a
`DELETE` sends the header and no body), as every write does ([Who may
write](#who-may-write)); `404` on a server without a state store; `503` when the state
store cannot be written. A `503` that comes after the row was written leaves the row, and
the map follows at the next beat.

**When it takes effect.** On the storage nodes, from the first call after a node holds the
generation the answer names: a node learns a new generation with its next heartbeat
(a third of the member time to live, 5 s by default). A node that has no map enforces
nothing: one that takes a map from a frontend (`--frontend`) serves nothing until it has one,
but a standalone storage node, which has no map source, serves every call and enforces no
entry. At the membership service, a `Register` counts the entry at once, and a
`Heartbeat` from the first call after the map holds it; a denied node's row is not renewed,
so the member is `down` after the time to live and leaves the map after the dwell. Until
then the frontends still route to it, and the other nodes already refuse its copies, so a
write it is the primary for fails in that window. The entries are in the map every node
receives, which `GET /v1/members` does not show.

**What it does not cut.** The entry is applied to the storage nodes and to the membership
service, not to the worker listener: a daemon whose certificate is denied here loses its
storage access but keeps its work stream, and goes on receiving actions (their commands,
environments and input digests), and its blob calls on that listener are served, until it
is listed in `--worker-deny-list`, the file that listener reads
([worker-protocol.md](design/worker-protocol.md)). To take a daemon off the farm, list it in
both. `GET /v1/denied` shows the list; `GET /v1/members` does not.

**Locking yourself out.** An entry that names the frontend's own certificate (by serial or
key, or a `node` that is the name in it) makes every storage node refuse the frontend. The
way back is `DELETE /v1/denied`, which goes through this API and not through a certificate.

**Upgrading.** Upgrade every storage node and every frontend before relying on an entry. A
storage node that predates the list ignores it, and the checks at `Register` and `Heartbeat`
are the frontend's, so a denied storage node can still register and beat through a frontend
that predates them. A frontend that predates the list copies it forward in the map
unchanged.

## `GET /v1/frontend/drain` and `POST /v1/frontend/drain`

Draining the frontend itself, for a rollout that restarts it (the nodes are drained
with `:drain` above). Buck2 does not recover an Execute stream that breaks after it
started, so a restart that cuts a stream fails its action: the frontend is drained, and
restarted when it has no stream left.

**`GET /v1/frontend/drain`**: a read, open like `GET /v1/nodes`:

```json
{ "draining": false, "open_executions": 3, "queued_operations": 1 }
```

- `draining`: a drain has begun, by this endpoint or by SIGTERM or SIGINT. It is not
  undone.
- `open_executions`: Execute and WaitExecution operation streams open now
  (`kbf_front_open_executions`: a stream counts from the moment it is made, before its
  first operation is read, until it ends or its client leaves).
- `queued_operations`: operations in the scheduler's queue, with no lease yet. With
  `open_executions` it tells idle (both 0) from busy, and a stream that waits for a
  worker (`open_executions` above 0, `queued_operations` above 0, no lease running)
  from one that runs. `kbf_front_executes_started_total` (see the metrics below) tells a
  client that has just come from one that is still there.

**`POST /v1/frontend/drain`**: begins the drain, a write that passes every gate of
[Who may write](#who-may-write) and takes no body (or `{}`), and answers the same status
with `draining` true. Calling it again changes nothing; SIGTERM or SIGINT after it
changes nothing either. The drain:

- turns `/readyz` to `503` (`stopping`) and the health service to `NOT_SERVING`;
- refuses every new Execute `UNAVAILABLE`, which Buck2 retries when it opens a call;
- sends the REAPI listener GOAWAY, so it takes no new connection;
- keeps every open Execute and WaitExecution stream until its action is done;
- ends the server, exit 0, when the REAPI connections have closed. The worker listener
  and this API stay up until then, so daemons go on reporting results.

A frontend that has nothing open exits at once, after answering the request. What ends
a drain that waits too long: `--shutdown-timeout-secs` when a signal began it (the
default waits for ever), or a second SIGTERM or SIGINT, which ends every open stream
`UNAVAILABLE` (the action fails) and stops the server within a few seconds. Under
systemd that means `TimeoutStopSec=infinity` in the unit: a stop timeout would SIGKILL
the drain, and the unit templates in `kbf-deploy` set it.

`kbf cluster` (on the frontend's host) calls this API: `kbf cluster status [--frontend]`,
`kbf cluster drain <node> [--deadline-secs N] --token-file F`, `kbf cluster drain
--frontend --token-file F` and `kbf cluster undrain <node> --token-file F` (the API's
`:uncordon`). It prints the answer's JSON, exits 0 on a 2xx and 1 otherwise, and takes
`--api` for the listener (default `127.0.0.1:8982`). A frontend drain has no undrain:
restart it. `kbf cluster member <id> --state active|draining --token-file F` calls
`POST /v1/members/{id}`, and `kbf cluster denied --token-file F` calls `GET /v1/denied`.
`kbf cluster object-store ...` calls the `/v1/object-stores/...` routes (below).

## `/v1/enroll/tokens` and `/v1/enroll/nodes`

The operator's side of enrollment (`design/install-and-enrollment.md`):
the tokens a node enrolls with, and the registry of nodes that did. They live in the state
store, so a frontend that has no enrollment (until the enroll service lands, every
frontend) answers every one of these routes with `404` once the call passes the gates.

**Every route here, a GET included, passes every gate of [Who may write](#who-may-write):**
a loopback peer (one that presents the proxy's secret, with `--proxy-auth-file`), no `Origin` header, the bearer token, and `Content-Type: application/json`
(send it on a GET and a DELETE too, with no body). The list of tokens and nodes is the farm's
door, so unlike `GET /v1/nodes` it is not open to whoever reaches the address. A refused
call changes nothing. Every answer carries `Cache-Control: no-store`.

| Route | Does | Answer |
|---|---|---|
| `POST /v1/enroll/tokens` | makes a token | the token, **with its secret and string, once** |
| `GET /v1/enroll/tokens` | lists the tokens | `{"tokens": [...]}`, never a secret, hash or string |
| `DELETE /v1/enroll/tokens/{id}` | revokes a token | `{"id": "..."}` |
| `GET /v1/enroll/nodes` | lists the nodes | `{"nodes": [...]}` |
| `POST /v1/enroll/nodes/{id}/revoke` | revokes a node | `{"id": "...", "denied": [...]}` |

**Create.** The body is `{"roles": ["daemon"], "max_uses": 1, "expires_in": 86400,
"node_id": "n1", "node_prefix": "rack-", "names": [".example"], "replace": false,
"label": "rack 4"}`; only `roles` is required (`storage` and `daemon`, one to two, no
repeat). `max_uses` is 1 to 1000 (default 1); `expires_in` is seconds, at least 1 and at
most 30 days (default a day); `node_id` and `node_prefix` restrict the node ids the token
enrolls; `names` restricts the hosts a storage node may advertise (a host, or a suffix that
starts with `.`; empty allows any); `replace` lets it re-enroll an id that exists. An
unknown field is a `400`. The answer is

```json
{"token": "kbft1.<id>.<secret>.<pin>", "secret": "<base64url>", "info": {"id": "...", ...}}
```

`token` is what a node is given (the pin is the SHA-256 of the root CA certificate, which
the node checks the frontend against at first contact). **This is the only answer that
holds `token` or `secret`:** the state store keeps a hash of the secret, and no list
returns the hash, the secret or the string. Lose it and revoke the token. A token whose
string cannot be built (the frontend has no CA pin, `503`; the random source fails, `500`)
is not stored.

`info` and each element of `tokens` are `{id, roles, max_uses, uses, expires_at,
created_at, revoked_at, node_id, node_prefix, names, replace, label, created_by}`; times
are seconds on the state store's clock. **Revoke** stops new enrollments with the token;
nodes it enrolled stay enrolled, and revoking a revoked token is a `200`. `404` for an
unknown or malformed id.

Each element of `nodes` is `{id, roles, state, advertise, zone, token_id, token_label,
created_at, last_renewed_at, not_after, revoked_at}`, with `state` `active` or `revoked`
and no certificate or key. **Revoking a node** takes no body (or `{}`), marks it `revoked`
(its id is burned: no token enrolls that id again) and writes the deny rows for the node,
each unexpired serial it holds or held and each of its keys, in one transaction; `denied`
lists them as the deny list spells them (`node n1`, `serial ...`, `spki-sha256 ...`). `400`
for a node already revoked, `404` for an unknown or malformed id.

**Failures.** `400` for a body or value the registry refuses, `404` (above), `409` when
this frontend is not the signer (it lost the writer role: nothing was written, call the
current frontend; the enroll wire's `NotSigner`), `503` when the state store failed or
other writers kept winning (try again), `500` for a stored row that does not decode.
`401`, `403` and `415` are the gates'.

There is no `kbf` verb for this: `kbf` only runs, and operator actions are calls to this
API. From the frontend's host, with the file `--api-token-file` names:

```sh
T=$(cat <token file>); API=http://127.0.0.1:<port>
curl -H "Authorization: Bearer $T" -H 'Content-Type: application/json' \
  -d '{"roles": ["daemon"], "max_uses": 3, "expires_in": 86400, "label": "rack 4"}' \
  $API/v1/enroll/tokens                       # the token string is in this answer only
curl -H "Authorization: Bearer $T" -H 'Content-Type: application/json' $API/v1/enroll/tokens
curl -X DELETE -H "Authorization: Bearer $T" -H 'Content-Type: application/json' \
  $API/v1/enroll/tokens/<id>
```

A UI comes later.

## `GET /v1/storage`

Whether every blob is known to have its copies elsewhere: the number a rollout asks
before it takes a storage node down. A read, open like `GET /v1/nodes`, served by a
frontend with a state store (without one there are no storage members, and the route is
`404`). The definition, which is also the help text of the metric below:

`under_replicated_blobs` is the sum, over all registered storage members, of the blobs
each owes a second copy (the open records of the member's repair journal, which each
storage heartbeat reports). It is **known** only when every registered member is `ACTIVE`
and has reported within the freshness window (two heartbeat intervals); otherwise it is
**unknown**, because a member that is down, joining, catching up, draining or silent may
hold blobs that nobody counts. A member that has not read its journal since it started, or
has none, reports that it cannot say; that is unknown too, never zero. So is a farm with
no registered member.

```json
{ "members_total": 3, "members_active": 3, "not_active": [], "silent": [],
  "unreported": [], "debts_total": 3, "under_replicated_blobs": 3,
  "reasons": ["3 blobs owed"] }
```

- `under_replicated_blobs`: the number, or `null` while unknown. A gate treats `null` as
  not zero.
- `debts_total`: the debts of the `ACTIVE` members that reported within the window and
  said, summed even when the total is unknown.
- `not_active`, `silent`, `unreported`: member ids that make the number unknown: members
  that are not `ACTIVE`; `ACTIVE` members with no report within the window; `ACTIVE`
  members whose report says they cannot say.
- `reasons`: why the number is not zero, in that order, as a rollout would say it
  (`member 3 not active`, `member 2 silent`, `5 blobs owed`); empty when it is known to be
  zero.

The number is as old as the newest heartbeat. A copy owed since then is not in it, so
whoever acts on a zero keeps new writes away from the node first (drains it), then reads
again. A member that left the map (a `DOWN` member leaves after the dwell) is not counted.
`503` with `{"error": ...}` when the membership cannot be read.

## The admin calls: `GET /v1/admin/role` and `POST /v1/admin/{quiesce,resume,takeover,stand-down}`

A `kbf --mode frontend` started with `--admin-listen <addr>` serves these on a port of
their own, separate from the operator API. The address must be a loopback address (any
other is refused at start, in the flags and again when the port is bound), and the calls
carry no credential: they move the scheduler's role, so nothing but the host's own
processes may reach them. The port needs `--state-store`. The calls and the `role` answer
are versioned by `admin_protocol` in `kbf --version --json` (`docs/artifacts.md`).

**`GET /v1/admin/role`**:

```json
{
  "role": "active",
  "instance_id": "5d1c4a52-8b0e-4f64-9a27-3c0e4f8d9a10",
  "admin_protocol": 1,
  "generation": 7,
  "held_generation": 7,
  "superseded": false,
  "open_streams": 3,
  "open_sessions": 2,
  "reapi": "127.0.0.1:8980",
  "worker": "127.0.0.1:8981",
  "api": "127.0.0.1:8982"
}
```

- `role`: `candidate` (started with `--await-takeover`: holds no generation and schedules
  nothing), `active` (holds the generation and schedules), `quiesced` (holds it and has
  stopped scheduling, assigning and requeueing) or `stood-down` (another process holds the
  generation).
- `instance_id`: a UUID, new for each process.
- `generation`: the generation the state store is at now (`null` if it cannot be read);
  `held_generation`: the one this process took (`null` for a candidate). A takeover passes
  the holder's `generation` as `expect`.
- `superseded`: the store has refused a write of this process because another writer holds
  the generation.
- `open_streams`: Execute and WaitExecution streams open; `open_sessions`: worker streams
  open.
- `reapi`, `worker`, `api`: the listeners. A candidate has no worker listener, and a
  stood-down process has none again.

Every call below answers `200` with this object after the call. A call that does not apply
answers `409` with the object and an `error` naming why (`role`, or a generation); a body
that is not what the call takes is `400`.

**`POST /v1/admin/quiesce`** (active or quiesced): waits, for at most 10 seconds, for the
Execute calls between their request and their queueing and the results being taken, then
fences the farm: it feeds the scheduler nothing, so nothing is granted, assigned or
requeued, a call that would queue is refused `UNAVAILABLE`, and a worker's result or
heartbeat is not applied. It writes the requeues the store refused. Idempotent. Open
streams and worker sessions stay open. While quiesced, a worker's heartbeat or result is not
applied, so a quiesce is meant to last seconds (a takeover's limit is 10): one longer than
the leases' expiry lets the scheduler expire the leases of work still running and requeue it
when the process resumes. Nothing bounds it.

**`POST /v1/admin/resume`** (quiesced or active): undoes a quiesce, for a takeover that did
not happen. Refused with `409` when the store is no longer at `held_generation` or the
store has refused a write of this process (another writer holds it: only `stand-down`
applies). The farm fed the scheduler nothing while it was quiesced, so its memory is the
store's as of the quiesce; nothing is reloaded.

**`POST /v1/admin/takeover`**, body `{"expect": N}` (candidate only): takes the generation,
which must be exactly `N` (a store at another generation is refused with `409` and nothing
is written), keeps the epoch (daemons that redial keep the runs of their leases), reads all
state from the store, and starts the scheduler, the reconciler and the worker listener on
the sockets the candidate already bound. `500` if the generation was taken and the
frontend could not start; the process then ends.

**`POST /v1/admin/stand-down`** (active, quiesced or stood-down): the old holder's step
after a takeover elsewhere. It ends the worker streams with `UNAVAILABLE` ("connect
again"), stops the worker listener, goes not ready (`/readyz`, the health service) and
answers the Execute and WaitExecution streams it kept from the `done/` rows the new
holder writes, with no GOAWAY and no timeout. It does not give up the state store. It is
not undone. A process that is never told to stand down but finds it is fenced (the store
refused a write of its own: `superseded` turns true) fences its farm and answers its kept
calls from the `done/` rows the same way. The process exits only when told to (`open_streams` and `open_sessions` say
when it holds nothing).

**A candidate** (`--await-takeover`, which needs `--admin-listen`) opens the state store
without taking the generation, touches no epoch and writes nothing; it serves `/readyz`,
`grpc.health.v1` and `GetCapabilities`, and no other REAPI call (`UNIMPLEMENTED`). It runs
no scheduler or reconciler and binds no worker listener. It makes and changes no schema.
A SQLite state file is held by one process (an exclusive file lock), so a candidate cannot
run beside the process that holds the file: side by side needs the Postgres store.
A start without `--await-takeover` takes the next generation and bumps the epoch, as any
start does.

## `/v1/object-stores/...`

Registers the farm's object store with the frontend, which keeps it as durable desired state
with a generation. A
**connection** is a named endpoint plus a key pair; a **binding** maps a role to a
connection, a bucket and a prefix. The only role is `cold`, today's `--cold-store`; `history`
and `backup` answer `422 role_not_supported` until something consumes them. A role here is
neither an IAM role nor an enrollment role.
Every verb of the API is in [the verbs table](design/operator-verbs.md); the bodies, the error
codes and the rotation runbook are in [the design](design/object-store-registration.md).

The routes exist when the frontend is started with `--object-store-secrets-dir DIR` (which
needs `--api-listen`, `--api-token-file` and `--state-store`). `DIR` holds one file of mode
0600 per connection, `<name>.<version>.json`, in a directory made 0700; every frontend child
that serves at once must be able to read it, and it must stay out of any backup of the host
(the state store, and so `state-backup`, never holds a secret key). Without the flag every
route answers `404` `not_enabled` to a caller who passed the gate.

**Every route, a GET included, passes the whole gate of [Who may write](#who-may-write)**
(loopback peer, no `Origin`, the bearer token, `Content-Type: application/json`), because
the routes list and change the farm's store and the key to it. One token, no roles. A TLS
proxy on the host reaches them from loopback and passes the gate: whether the proxy carries
these routes is the operator's decision. Every answer, errors and `405` included, carries
`Cache-Control: no-store`. A body is at most 16 KiB (`413 payload_too_large`) and refuses
unknown fields. No route is fenced: two frontend children on one state store both serve them,
and a lost race is `409 conflict`, never a lost write.

| Route | Does |
|---|---|
| `PUT /v1/object-stores/connections/{name}` | create or replace; `201` created, `200` changed or unchanged |
| `GET /v1/object-stores/connections` | `{connections: [...]}`, never a secret |
| `GET /v1/object-stores/connections/{name}` | one connection, never a secret |
| `DELETE /v1/object-stores/connections/{name}` | `{generation}`; `409 in_use` while a binding names it |
| `POST /v1/object-stores/connections/{name}:verify` | probe the store, change nothing |
| `PUT /v1/object-stores/bindings/{role}` | bind `cold`; `201` created, `200` changed or unchanged |
| `DELETE /v1/object-stores/bindings/{role}` | `{generation}`; `409 in_use` once a node has applied it |
| `GET /v1/object-stores` | the desired state and the nodes' convergence |
| `GET /v1/object-stores/audit?limit=N` | the newest `N` audit rows (default 50, at most 500) |

The design ([object-store-registration](design/object-store-registration.md)) has the rotation
runbook; every verb of the API is in [the verbs table](design/operator-verbs.md).

**Names.** `{name}` is `[a-z0-9][a-z0-9_-]{0,62}`, else `400 bad_name`: it becomes a file name
and a URL segment, so `/`, `.`, `:` and NUL never reach the file system. `{role}` is `cold`.

**`PUT .../connections/{name}`** takes `{endpoint, region?, access_key_id,
secret_access_key, conditional_put?, min_put_rate?, expect_generation?, verify?}`.

- `endpoint` is `http://host[:port]`: `https` answers `422 tls_unsupported` (the S3 client
  speaks no TLS yet, so a registration would be accepted and fail on every node); anything
  else that is not that answers `422 invalid_endpoint`. `access_key_id` and
  `secret_access_key` are visible ASCII (at most 256 and 1024 characters), `region` is
  `[a-z0-9-]` (at most 64): else `422 invalid_field`.
- Changing `endpoint` of a bound connection answers `409 identity_change`; credentials,
  region, `conditional_put` and `min_put_rate` change freely. The same body again changes
  nothing (`200`, `changed: false`, the generation stays).
- `expect_generation` that is not the registry's answers `409 generation_mismatch` and writes
  nothing; without it the last writer wins and both are in the audit log.
- `verify` (default `true`): when the connection is the one the `cold` binding names, the
  frontend probes that bucket and prefix with the key being registered before it writes
  anything, and a failed probe answers `422 probe_failed` with no row and no file. When no
  binding names the connection there is no bucket to probe: `verified` is `null`, and the
  connection is probed when it is bound and by `:verify`.
- Answer: `{name, generation, changed, access_key_id, secret_set, verified}`, never the
  secret or a digest of it.

**`PUT .../bindings/{role}`** takes `{connection, bucket, prefix?, expect_generation?,
verify?}`; `verify` (default `true`) probes the bucket with the connection's stored key
first. `422 unknown_connection`, `invalid_bucket`, `invalid_prefix` and `probe_failed` write
nothing. The store identity `(endpoint, bucket, prefix)` is immutable once bound
(`409 identity_change`): moving to another store is a migration this API has no verb for.
Answer: `{role, generation, changed, verified}`.

**`POST .../connections/{name}:verify`** takes nothing, or `{bucket?, prefix?}`; without a
bucket it uses the `cold` binding's, else `422 no_bucket`. It puts one small object under
`<prefix>.kbf-probe/`, reads it back, checks that a second write is refused when the
connection says `conditional_put`, and deletes it. `200 {name, verified}`, or `422
probe_failed` with `verified` (`{ok, put, get, delete, conditional_put}`): the steps, never
the store's words. The probe proves that *this frontend* can reach the store with this key;
the frontend connects to an endpoint an operator typed, so it is bounded (20 seconds in all)
and operator-only behind the token.

**`GET /v1/object-stores`** answers `{generation, connections, bindings, convergence}`;
`convergence` is `{converged, expected, applied, unsupported, nodes: [{id, state,
applied_generation, status, last_seen_secs}]}`.

- `status` is one of `ok`, `pending`, `probe_failed`, `identity_mismatch`, `unsupported`: a
  closed code, never a node's own text. A node that reports `ok` at another generation is
  shown `pending`.
- `expected` counts active nodes that report the registration; draining and denied nodes
  are listed but not counted. A node whose build does not report it at all is `unsupported`,
  counted in `unsupported` and not in `expected`, and named in `nodes`, so version skew is not
  mistaken for "not fetched yet".
- `converged` is true only when at least one node is expected, every expected node has
  applied the current generation with status `ok`, and none is `unsupported`. It never claims
  more than the frontend knows: a frontend that has heard from no node says `false`.
  **This build wires no source of node reports yet: `nodes` is empty and `converged` is `false`.**

**`GET /v1/object-stores/audit`** answers `{entries: [{seq, at, action, subject, outcome,
generation}]}`, newest first. A row names what was touched and what happened, never a body
field; the actor is always "the API token", the gate knows no finer identity. Only writes
leave rows (a refused call, a failed probe and `:verify` leave none), an unchanged repeat leaves
one with `outcome` `unchanged`.

**Errors** are `{"error": "...", "code": "..."}` (a `probe_failed` also has `verified`). The
text names no value of the request: a body that does not parse says where, not what.

| Status | `code` |
|---|---|
| 400 | `bad_name`, `bad_body`, `bad_query` |
| 401, 403, 415 | `unauthorized`, `forbidden`, `unsupported_media_type` (the gate's) |
| 404 | `not_enabled`, `no_such_connection`, `no_such_binding`, `unknown_action` |
| 409 | `identity_change`, `in_use`, `generation_mismatch`, `conflict` |
| 413 | `payload_too_large` |
| 422 | `invalid_field`, `tls_unsupported`, `invalid_endpoint`, `invalid_bucket`, `invalid_prefix`, `role_not_supported`, `unknown_connection`, `no_bucket`, `probe_failed` |
| 500 | `secret_missing` (the file a row names is missing or not 0600: fail closed, PUT again to repair), `secret_io`, `corrupt_rows`, `internal` |
| 503 | `state_unavailable` |

`DELETE .../bindings/{role}` answers `409 in_use` when a node reports `ok` at a generation
at or after the binding's `updated_generation`: it runs on that store, and the API cannot take
a store back from a running node. (A binding re-PUT with another connection of the same
endpoint moves `updated_generation`; a node that applied only the earlier version is not
counted.)

**Rotating the key** (the API never revokes a key at the store):

1. Add the new key at the store.
2. `PUT` the connection with the new secret. The frontend probes the bound bucket with it
   first.
3. Restart the storage nodes through the installer: a running node does not swap
   credentials in v1, and picks the registration up at its next start.
4. Poll `GET /v1/object-stores` until `converged` is `true` (a node still on the old key is
   `pending`).
5. Only then revoke the old key at the store.

```sh
T=$(cat <token file>); API=http://127.0.0.1:<port>
# the body is a file, never an inline secret; the content type is required on every call
curl -X PUT -H "Authorization: Bearer $T" -H 'Content-Type: application/json' \
  --data @connection.json $API/v1/object-stores/connections/cold
curl -X PUT -H "Authorization: Bearer $T" -H 'Content-Type: application/json' \
  --data '{"connection":"cold","bucket":"farm-cold","prefix":"cold/"}' \
  $API/v1/object-stores/bindings/cold
curl -H "Authorization: Bearer $T" -H 'Content-Type: application/json' $API/v1/object-stores
```

**`kbf cluster object-store`** is a client of these routes on the frontend's host, one call a
verb, printing the answer's JSON (exit 0 on a 2xx, 1 on anything else, 2 for a command line
that does not parse). Every verb needs `--token-file F`, a read included, because every route
passes the whole gate; `--api ADDR` names the listener (default `127.0.0.1:8982`).

| Verb | Call |
|---|---|
| `object-store status` | `GET /v1/object-stores` |
| `object-store audit [--limit N]` | `GET /v1/object-stores/audit[?limit=N]` |
| `object-store connection list` | `GET /v1/object-stores/connections` |
| `object-store connection show NAME` | `GET /v1/object-stores/connections/NAME` |
| `object-store connection put NAME --endpoint URL --access-key-id ID (--secret-file F \| --secret-stdin) [--region R] [--conditional-put] [--min-put-rate BYTES] [--expect-generation N] [--no-verify]` | `PUT /v1/object-stores/connections/NAME` |
| `object-store connection delete NAME` | `DELETE /v1/object-stores/connections/NAME` |
| `object-store bind --connection NAME --bucket B [--role cold] [--prefix P] [--expect-generation N] [--no-verify]` | `PUT /v1/object-stores/bindings/cold` |
| `object-store unbind [--role cold]` | `DELETE /v1/object-stores/bindings/cold` |
| `object-store verify NAME [--bucket B] [--prefix P]` | `POST /v1/object-stores/connections/NAME:verify` |

The secret key is never a flag or an environment variable: `connection put` reads it from
standard input (`--secret-stdin`) or from a file (`--secret-file F`) that must be a regular
file whose mode gives its group and others no access (0600 or 0400), else it exits 1 and
calls nothing. Only the final line ending is cut. Names and the role are checked before any
call (`bind --role history` exits 2). The key is in none of the command's output or errors.

```sh
kbf cluster object-store connection put cold --endpoint http://127.0.0.1:9000 \
  --access-key-id AKID --secret-file key.txt --token-file T   # key.txt is mode 0600
kbf cluster object-store bind --connection cold --bucket farm-cold --prefix cold/ --token-file T
kbf cluster object-store status --token-file T
```

The request body is read into memory and parsed; the buffers are not zeroized, so the
secret key sits in the frontend's memory for as long as the allocator keeps those pages.
The key is never logged, never in a state row and never in an answer.

## `GET /healthz` and `GET /readyz`

For the health check of a front or proxy (a TLS-terminating ingress, Envoy) in front
of the REAPI listener. Both are reads, open like `GET /v1/nodes`, and are served only
when `--api-listen` is given. The routes are served by the same task as the REAPI
and worker listeners, which are bound before it starts and stop when it ends, so any
answer means both are bound.

**`GET /healthz`**: the process is alive. It reads nothing, and answers `200` for as
long as the server runs, during a stop too:

```json
{ "status": "alive", "version": "0.1.0+0123456789ab", "commit": "0123456789ab" }
```

**`GET /readyz`**: the server is ready to serve. `200` when every check passes, `503`
otherwise; the body lists each failing check and why:

```json
{ "ready": false, "version": "0.1.0+0123456789ab", "commit": "0123456789ab",
  "failing": [ { "check": "store",
                 "reason": "a read of kbf/1791370000000000000/readyz-probe had no answer within 2000 ms" } ] }
```

| Check | Fails when |
|---|---|
| `stopping` | the server has received SIGTERM or SIGINT, or `POST /v1/frontend/drain` has begun a drain. It answers `503` from that moment, and for the rest of the drain; open Execute streams are kept until their action is done, and only the shutdown timeout (`--shutdown-timeout-secs`) or a second signal ends them |
| `leader` | the server does not hold the scheduler role. A single server runs every role and always holds it; the check is there for a replicated control log, whose followers fail it. **Planned**: a second readiness path, without this check, for the servers that may serve reads ([deployment-topology.md](design/deployment-topology.md#build-clients-one-name-routed-by-method)) |
| `store` | the object store does not answer a one-byte read of the key `<prefix>readyz-probe` within `--readyz-store-timeout-ms` (default 2000; it bounds the gRPC health service's probe too), or answers with an error. The probe only reads: no poll writes, deletes or lists. Nothing writes that key, so "not found" is the expected answer and passes, as does the object's bytes or a range past its end |

`version` and `commit` are those of `GET /v1/nodes`' `server` field. Each `/readyz`
makes one read of the store, so poll it at the interval the front needs, not faster.

## `grpc.health.v1` on the REAPI listener

For a proxy that health-checks its backends over gRPC (Envoy's `grpc_health_check`, a
gRPC load balancer), the REAPI listener (`--listen`) serves the standard
[gRPC health checking protocol](https://github.com/grpc/grpc-proto/blob/master/grpc/health/v1/health.proto),
`grpc.health.v1.Health`, whether or not `--api-listen` is given, and over the
listener's TLS when `--reapi-tls-cert` and `--reapi-tls-key` are given. Its answer is
`/readyz`'s: `SERVING` when every check of the [table above](#get-healthz-and-get-readyz)
passes, `NOT_SERVING` when one fails. It gives no reason; `/readyz` names the failing
checks.

It answers for these service names, all with the same status: `""` (the server as a
whole), `build.bazel.remote.execution.v2.Capabilities`,
`build.bazel.remote.execution.v2.ContentAddressableStorage`,
`google.bytestream.ByteStream`, `build.bazel.remote.execution.v2.ActionCache` and
`build.bazel.remote.execution.v2.Execution`.

| Method | Answer |
|---|---|
| `Check` | the status, after one store probe (as one `/readyz`). Another service name is `NOT_FOUND` |
| `List` | every name above with the status, after one store probe |
| `Watch` | the status at once, then each time it changes. It evaluates again as soon as a stop signal arrives or the scheduler role changes, and probes the store every 5 s. Once the server is stopping it sends `NOT_SERVING` (if that was not its last message) and ends the stream `UNAVAILABLE`, so an open Watch does not hold up the drain. Another service name is sent `SERVICE_UNKNOWN`, and the stream stays open until the server stops |

On SIGTERM or SIGINT the status turns `NOT_SERVING` before the REAPI streams are
ended and the listener stops accepting. After that the listener takes no new calls,
so a `Check` during the drain fails or times out instead of answering; either way the
proxy marks the server down.

The REAPI authentication policy (`--reapi-auth-policy`, [reapi-auth.md](reapi-auth.md))
does not run on these calls: a health check needs no credentials, whatever the policy.
Every other call on the listener, a path it does not serve included, still runs it.
The store probe waits at most `--readyz-store-timeout-ms`, like `/readyz`'s.

**Planned:** with several servers, the answers follow the two readiness paths, ready
to serve reads and leader; which service name reports which is not chosen
([deployment-topology.md](design/deployment-topology.md#build-clients-one-name-routed-by-method)).

## `GET /metrics`

The cache's upload path in the Prometheus text exposition format (version 0.0.4,
`Content-Type: text/plain; version=0.0.4; charset=utf-8`): what the CAS services did,
and how long each stage of a store took. A read, open like `GET /v1/nodes`; it names
no blob, build or node. Every listener that serves the cache counts into the same
families: the REAPI listener (clients) and the worker listener (daemons' uploads of
outputs and reads of inputs). The counts are in memory and start at 0 with the
process.

A storage node (`kbf storage`, and the storage half of `kbf storage-and-daemon`) has no
operator API and serves this one route, plain HTTP, on `--metrics-listen <addr>`
(off unless given; the same open read as above, so bind it where only operators do). It
answers the families of its own cache, among them `kbf_cache_lost_blobs` and
`kbf_cache_blob_losses_total`, the node's TLS reloads, and, in
`kbf storage-and-daemon`, the one cache's fill.

Each request family counts from the start of its handler to its answer. A call whose
caller goes away before the answer counts as an error.

| Family | Type | What it measures |
|---|---|---|
| `kbf_bytestream_write_requests_total`, `_errors_total` | counter | ByteStream Write calls answered (or dropped), and those that failed |
| `kbf_bytestream_write_bytes_total` | counter | blob bytes received, counted as each message arrives, so a slow Write shows progress before it ends |
| `kbf_bytestream_write_in_flight` | gauge | Writes begun and not yet answered |
| `kbf_bytestream_write_seconds` | histogram | Write latency, end to end |
| `kbf_bytestream_write_present_total` | counter | Writes answered at once because the blob was already durable |
| `kbf_batch_update_requests_total`, `_errors_total`, `_bytes_total`, `_in_flight`, `_seconds` | as above | BatchUpdateBlobs calls; `kbf_batch_update_blobs_total` counts the blobs they sent |
| `kbf_find_missing_requests_total`, `_errors_total`, `_in_flight`, `_seconds` | as above | FindMissingBlobs calls; `kbf_find_missing_digests_total` the digests asked about, `kbf_find_missing_missing_total` those answered missing |
| `kbf_execute_requests_total`, `_errors_total` | counter | Execute calls ended, and those that ended in an error status or were dropped by their caller before the done operation (an operation that finished with a failed `ExecuteResponse` is not one) |
| `kbf_execute_in_flight` | gauge | Execute and WaitExecution streams open |
| `kbf_under_replicated_blobs` | gauge | the sum over all registered storage members of the blobs each owes a second copy, exported **only while it is known** (every registered member `ACTIVE` and reporting within the window); absent otherwise, never 0. Frontends with a state store. [`GET /v1/storage`](#get-v1storage) says why it is unknown |
| `kbf_storage_health_known` | gauge | 1 when `kbf_under_replicated_blobs` is exported, 0 when it is unknown (including a membership that cannot be read). Frontends with a state store |
| `kbf_front_open_executions` | gauge | Execute and WaitExecution operation streams alive: raised when the stream is made (before its first item is read), lowered when it ends or its caller drops it. A call still being authorized or prepared, and one answered from the action cache, is not in it. `GET /v1/frontend/drain` reports it |
| `kbf_front_executes_started_total` | counter | Execute calls the front took (not refused because it is draining), cache hits included |
| `kbf_execute_queued` | gauge | Execute streams the scheduler accepted whose operation has not left `QUEUED` |
| `kbf_execute_prepare_seconds` | histogram | an Execute from its arrival to the call of the scheduler: authorization, action cache lookup, reading the action and its inputs, the held-inputs check, the booking; observed only for an Execute that reaches the scheduler |
| `kbf_execute_dispatch_seconds` | histogram | the scheduler's submit call, from the call to its return |
| `kbf_execute_queued_seconds` | histogram | from the scheduler's acceptance to the stream's first update that is not `QUEUED` |
| `kbf_execute_total_seconds` | histogram | an Execute from its arrival to its last response, its error or the departure of its caller; cache hits included |
| `kbf_store_blobs_seconds` | histogram | one store of verified blobs (a Write's, or a batch's): the query for blobs already held, their touch, and the wait until every segment holding its blobs is written: the linger, the wait for a PUT slot, each PUT attempt with the backoff between attempts, the footer read, and the commit with any wait behind commits queued ahead of it |
| `kbf_segment_put_seconds` | histogram | one PUT attempt of a segment's object; a PUT tried again is observed once per attempt |
| `kbf_segment_footer_read_seconds` | histogram | one segment's footer read back after its PUT: two range GETs, the trailer and then the footer |
| `kbf_tls_reloads_total{listener,outcome}` | counter | looks at a TLS listener's files (`reapi`, `worker`; [renewal](deploy/tls-renewal.md)): `applied` (new files now serve new handshakes), `rejected` (changed but unusable: the previous files keep serving; the reason is in the log) and `unchanged`; only for a listener with TLS |
| `kbf_tls_leaf_not_after_seconds{listener}` | gauge | when the certificate the listener serves stops being valid, in seconds since the epoch; alert on it well before it passes |
| `kbf_segment_commit_seconds` | histogram | one segment's `PutBlobs` commit to the metadata log, including any wait behind commits queued ahead of it |
| `kbf_segment_blobs`, `kbf_segment_bytes` | histogram | blobs, and bytes (footer included), per segment written |
| `kbf_object_get_seconds` | histogram | one range GET of a blob being read (ByteStream Read, GetActionResult, a BatchReadBlobs run); footer reads are not in it |
| `kbf_meta_commit_seconds{command}` | histogram | every metadata-log commit the cache makes, by command (`put_blobs`, `touch`, `put_action`, `object_unreachable`, `tick`, `collect`, ...): with the Raft metadata log, the Raft log's commit latency |
| `kbf_meta_query_seconds` | histogram | every metadata-log query the cache makes |
| `kbf_execute_presubmit_seconds` | histogram | an Execute from the handler's start to its first stream item (the first `QUEUED` operation cannot be sent sooner): authorization, the action-cache lookup, the action and command reads, the input-tree walk and the scheduler's submit. Failed and abandoned calls are observed too |
| `kbf_execute_presubmit_in_flight` | gauge | Executes inside that window now: during a stall with idle slots, this is the number of requests the scheduler has not seen |
| `kbf_execute_walk_seconds`, `kbf_execute_dir_reads_total` | histogram, counter | the input-tree check of one Execute, whole (directories read concurrently or taken from memory, subtrees skipped, presence checks, any wait for a check another request is running), and the Directory blobs read from the store, counted as each read starts |
| `kbf_execute_submit_seconds`, `kbf_sched_lock_wait_seconds` | histogram | the scheduler's submit called from an Execute, and the part of it spent waiting for the scheduler's lock |
| `kbf_sched_queued`, `kbf_sched_leases` | gauge | operations waiting for a lease, and leases granted or running on all nodes, as of the last submit or scheduler tick (at most a tick old) |
| `kbf_tier_in_flight`, `kbf_tier_wait_seconds`, `kbf_tier_call_seconds` | gauge, histogram | write-tier file calls queued on or running on the blocking pool; the wait of one for a pool thread; and one whole, hand-over to answer. Always printed, 0 without a write tier; a wait near the call time says the pool is full |
| `kbf_storage_open_write_fds`, `kbf_storage_write_pool_queue`, `kbf_storage_write_pool_direct_total` | gauge, gauge, counter | block files the write tier holds open for writing, as of the last write-pool step (at most `--write-pool-writers` plus one, for the blobs the pool writes); blobs waiting for a pool writer; and blobs the pool sent straight to the object store because its queue (`--write-pool-queue`) was full or the tier did not take them. Always printed, 0 without a write tier |
| `kbf_input_directories_cached_total` | counter | Directories the input check took from the decoded-directory cache: the blob is touched and its presence checked, nothing is read |
| `kbf_input_directories_skipped_total` | counter | Directories skipped with everything under them: an earlier check verified them and the retention guarantee of that check still runs |
| `kbf_input_directories_joined_total` | counter | Directories found being checked for another request, and waited for |
| `kbf_cache_lost_blobs`, `kbf_cache_blob_losses_total` | gauge, counter | blobs whose only copy was in a block on a write tier that is gone before the flusher put it in the store (the tier abandoned with `--hot-store-abandon`, the block missing or short at a start, or its bytes failing their digest): lost now, and ever. A lost blob is `UNAVAILABLE`, FindMissingBlobs lists it, an action-cache entry naming it is a miss, and a re-upload takes it off the gauge ([architecture-v2.md](design/architecture-v2.md#8-storage), Loss marks). Read from the metadata state at the scrape; left out of a scrape the metadata log cannot answer |

Latency buckets run from 1 ms to 60 s; size buckets from 1 KiB to 1 GiB by fours;
the blobs-per-segment buckets from 1 to 16384 by fours.

With a local write tier (`--hot-store`, [write-path.md](design/write-path.md#6-the-local-write-tier)),
these families follow; without one they are absent. An acknowledged blob on the
tier is on that server's disk and not yet in the object store, so the unflushed
families are what a disk loss would take, and the oldest age is what an alert
watches.

| Family | Type | What it measures |
|---|---|---|
| `kbf_write_tier_staged_total` | counter | segments staged: synced to a block file, then committed with `PutStaged` |
| `kbf_write_tier_full_total`, `kbf_write_tier_errors_total` | counter | segments written to the store instead, because the tier was at its budget, or its disk failed before the commit |
| `kbf_write_tier_flushes_total`, `kbf_write_tier_flush_failures_total` | counter | staged objects flushed (PUT, footer read back, `ObjectFlushed` committed), and flush attempts that failed and are retried after a wait (1 s doubling to 60 s) |
| `kbf_write_tier_flush_conflicts_total` | counter | staged objects whose key in the store held another object: they stay staged and are read locally until an operator acts (alert on any) |
| `kbf_write_tier_corrupt_total` | counter | staged objects with a record that failed its digest on the tier's disk: not uploaded, marked unreachable, healed by a re-upload |
| `kbf_write_tier_local_reads_total` | counter | blobs read from the tier's disk (a block, or a flushed copy) |
| `kbf_write_tier_evicted_total` | counter | flushed copies evicted, least recently read first |
| `kbf_write_tier_budget_bytes`, `kbf_write_tier_used_bytes` | gauge | the hot store's budget, and the bytes counted against it: blocks not yet flushed, reserved ranges included |
| `kbf_write_tier_flushed_copy_bytes` | gauge | flushed copies kept for local reads, in what the budget leaves over |
| `kbf_write_tier_unflushed_bytes`, `kbf_write_tier_unflushed_blocks` | gauge | acknowledged bytes, and the blocks holding them, not yet in the object store |
| `kbf_write_tier_oldest_unflushed_seconds` | gauge | the age of the oldest of those blocks (since it was created, or since the restart that recovered it); 0 when there is none |

In `kbf all-in-one`, when the hot store is a directory on the filesystem of `--data-dir`,
the daemon half's input cache and the server half's hot store are one cache
([architecture-v2.md](design/architecture-v2.md#10-locality)): the input cache's
directory is the node's only local copy of a blob, the tier deletes a block once it is
flushed (so `kbf_write_tier_flushed_copy_bytes` stays 0), and the input cache evicts its
entries until they and the tier's blocks fit its budget: after each input it fills, and
within about a second of the tier growing past what the budget leaves it (uploads alone).
Entries a running lease pins are never evicted, so a hot store budgeted at or above the
input cache's budget leaves room for those alone (the daemon warns once). These families
follow; in every other process they are absent.

| Family | Type | What it measures |
|---|---|---|
| `kbf_local_cache_input_bytes` | gauge | the input cache's entries |
| `kbf_local_cache_storage_bytes` | gauge | what local storage holds on that disk: the write tier's blocks |
| `kbf_local_cache_fill_bytes` | gauge | both, each byte once: what the one budget counts |

**Build history.** With `--history-objects` the server records each attempt and cache hit of
an invocation a client named, after the call is answered and off its path, and counts what it
could not keep. Without the flag these families are absent and nothing is recorded.

| Family | Type | What it measures |
|---|---|---|
| `kbf_history_dropped_total{stage}` | counter | records dropped instead of kept, by where: `queue` (the writer's queue was full), `buffer` (chunks waiting to be written passed their bound), `put` (an object write failed), `index` (an index row could not be applied), `invocations` (the writer held its bound of invocations) |
| `kbf_history_queue_events`, `kbf_history_queue_bytes` | gauge | events waiting for the writer, and their bytes |
| `kbf_history_pending_rows` | gauge | index rows the writer has not applied yet |
| `kbf_history_open_invocations` | gauge | invocations the writer holds state for |

**Reading a stall with idle slots.** `kbf_sched_queued` high and `kbf_sched_leases` below the
nodes' slots means work is waiting that placement cannot use (look at the node's booking and
`GET /v1/nodes`). `kbf_sched_queued` low while buck2 has many Executes open means they have not
reached the scheduler: `kbf_execute_presubmit_in_flight` counts them, `kbf_execute_walk_seconds`
and `kbf_execute_submit_seconds` (with `kbf_sched_lock_wait_seconds`) split where they wait, and
`kbf_tier_wait_seconds` near `kbf_tier_call_seconds` says the blocking pool is the queue. An
Execute that spends 10 s or more before its first operation is also logged at `WARN` with its
action, instance, invocation id and those phase times.

**Reading a slow upload.** Each ByteStream Write sends one blob, so a daemon's output
tree of N files costs N Writes, but not N segments: the cache groups blobs from
concurrent Writes and batches into one segment, sealed when it is full or
`--write-linger` after its first blob, and a blob of `--write-own-segment-bytes` or
more gets a segment of its own. Over a window, with `rate()`:

- Writes per second is `rate(kbf_bytestream_write_requests_total)`; with
  `kbf_bytestream_write_in_flight` near the daemon's upload limit, it is that limit
  over the mean Write latency
  (`rate(kbf_bytestream_write_seconds_sum) / rate(kbf_bytestream_write_seconds_count)`).
- The mean of each stage (`_sum` over `_count` of `kbf_segment_put_seconds`,
  `kbf_segment_footer_read_seconds`, `kbf_segment_commit_seconds`) says which one the
  Write waits on: the object store's PUT, its GETs, or the metadata log. A PUT tried
  again counts once per attempt, so `kbf_segment_put_seconds` can count more than
  the segments written (`kbf_segment_blobs`' count). What `kbf_store_blobs_seconds`
  holds beyond the three is the held-blob query and touch, the linger, the wait for
  one of `--write-puts-in-flight` PUT slots, and the backoff between PUT attempts.
  What a Write holds beyond its store is the wait for admission under
  `--write-bytes-in-flight`, receiving and hashing the bytes and authorizing the
  caller (none timed on its own), and the check whether the blob is already held: its query and any touch also show in `kbf_meta_query_seconds` and
  `kbf_meta_commit_seconds{command="touch"}`.
- `kbf_segment_blobs` with a mean of 1 says no grouping happened: uploads arrived
  more than a linger apart, or were large enough for segments of their own, so each
  paid a PUT, two GETs and a commit of its own.
- `kbf_bytestream_write_present_total` against the requests says how many uploads
  were of blobs already held.

The daemon logs the other side, per lease: its phase times (`lease phases:`) and,
from its transfer engine, what it read and uploaded (`transfers:`), joined on `lease=`
([daemon.md](design/daemon.md#where-a-leases-time-went)). The server logs, per lease
report that carries usage (an OK run or any failed one the daemon measured, for
example an out-of-memory kill, a timeout or a lost run), `action usage:` with the same cgroup fields, the action's
shape `key`, its `cfg`, the `outcome` and the `lease`, and names the `key` on each
`lease granted` line ([daemon.md](design/daemon.md#the-servers-action-usage-line)).

## `GET /v1/invocations/{id}` and below

The build pages: what the farm did for one build, read from the invocation records
([invocations.md](design/invocations.md#4-the-read-api)). The records are kept **in
memory** within `--invocation-memory` (section 3.5 there): a restart forgets them, and
at the bound idle records are removed. A record gone, or never made, is `404` on every
route.

| Route | Answers |
|---|---|
| `GET /v1/invocations/{id}` | the invocation: its header fields and counters, links to the routes below, its slowest actions and its phase totals |
| `GET /v1/invocations/{id}/actions` | a page of its action rows, in the order first seen; `?outcome=ok\|failed\|timed_out\|refused`, `?how=cache_hit\|cache_hit_execute\|joined\|ran`, `?requeue_reason=<reason>`, `?memory_kill=none\|own_limit\|node_pressure`; `?sort=wall_ms\|execution_ms\|queue_ms\|input_fetch_ms\|output_upload_ms`, longest first |
| `GET /v1/invocations/{id}/actions/{digest}` | one action's row, with the outputs, stdout, stderr and usage copied into it, and whether each blob is still held |
| `GET /v1/invocations/{id}/artifacts` | a page of the outputs of its actions, in row order |
| `GET /v1/invocations/{id}/logs` | a page of the stdout and stderr of its actions, failed actions first |
| `GET /v1/invocations/{id}/metrics` | its aggregates |

**Off unless the REAPI policy names it.** Pages hold build outputs and logs. The routes
run the policy of `--reapi-auth-policy` ([reapi-auth.md](reapi-auth.md)): its
`authenticationPolicy` on the request, then its `invocations.getAuthorizer` with every
instance name the invocation's calls used; all must allow (a refusal is `403`).
Without that key, or without a policy file, every request is `404` and the start line
says ` invocations=off` (before ` blobs=`); with it, ` invocations=on`.
`--invocations-open` turns the routes on for every caller the policy accepts (and the
blob route with them), for a listener only trusted people reach.

**`{id}`** is an invocation id as a client sent it (1 to 128 bytes of
`[A-Za-z0-9._-]`, starting with a letter or digit) or `<id>~<n>` (`n` from 2), the
record of another caller of the same id ([invocations.md](design/invocations.md#24-ids-are-claims-not-secrets)).
It is matched as the bytes of the path: a percent-encoded character (`%3C`, `%6F`) is
`404`, as is anything else.

**With `--history-objects`** a record the in-memory records do not hold (the server
restarted, or the record was removed at the bound) is read back from build history
(`docs/design/ui-data.md` 5.5), and every route above answers from it as it does from a
live record: filters, `sort=` and page tokens included. Without the flag nothing below
applies and the routes are as described above.

- **Authorized on every instance, in two steps.** The invocation's instance names are read
  from its header and each is asked of `invocations.getAuthorizer`; the records are then
  read and checked against exactly those names. An instance added in between is `503`
  (send the request again). A header whose instance list is empty or was cut at its bound
  cannot be checked whole, so the invocation answers `no_record` to every caller.
- **What a restored record says.** `"restored": true` on the invocation, and, when history
  has a hole (the invocation was never closed, or records were dropped), `"complete":
  false` and `"gap_from"`: the time the last chunk that did land was written, from which
  on records may be missing. `first_seen` and `last_seen` are those of the first and last
  record, not of the first and last call.
- **What it does not keep.** `tool`, `tool_version`, `correlated_invocations_id` and
  `collisions` are empty; the counters are those the rows show (`ac_miss`, `blobs_*` and
  `bytes_*` are 0); a row has no `target`, `mnemonic`, `configuration`, `action_id` or
  `operation` of its own (the `shape` has the target the server read), no `usage` in its
  action answer, an output's `executable` is `false`, and a cache hit has no listing. An
  attempt's end and the row's outcome are read from the run's class: a failed run's gRPC
  status code and a lost run's cause are not kept (see `restore.rs`). A cgroup counter the
  daemon could not read is 0.
- **Pages.** A page token made for a record the recorder held answers `no_record` once
  the record is read back instead, and so does one made for a restored record that has
  since left the 64 MiB cache of restored records. At most 50,000 records are read for one
  invocation; past that the record is `rows_capped` and not complete.

**`GET /v1/runs/{run_id}`** (with `--history-objects`; otherwise `404` as any unknown
route) is one attempt: the instance, `lease` (`term.seq`), `worker`, `queued`, `granted`,
`started` and `ended`, `class` (`ok`, `failed`, `timed_out`, `memory_killed`, `infra`,
`lost`), `exit_code` for a run that exited, the memory kill and the booking, cap, raised
booking, memory used and limit of a killed run, the REAPI times and cgroup `usage` the
daemon reported, the digests of the action, command, result and logs, and the `shape`
(its key, `cfg`, target and whether it is a test). A key the record has no value for is
left out. The run is read from the in-memory record while it is held and from history
after, in one rendering: the answer is the same before and after a restart. The run's one
instance is asked of `invocations.getAuthorizer` before anything is answered. `{run_id}`
is 24 lowercase hex digits, which the attempts of the build pages give as `run_id` and
`run_href` (only with `--history-objects`). Not here yet: the `envelope`, `end` and
transfer counters.

A run that has logs also has `logs`: for each of `stdout`, `stderr` and `result` that it
has, the `digest`, whether the blob is still held (`available`), an `href`, and for
`stdout` and `stderr` of a failed run the `tail` history kept (the last 16 KiB, as text),
its `total_bytes`, `tail: null` with `tail_omitted` (`too_large` over 64 MiB, `unreadable`,
`busy` when the read budget was not free in time) and `tail_redacted: false`: no
redaction exists, the tail is as the run wrote it. The `href` is the blob route while the
blob is held and the log route below once it is not and history holds a copy; `null`
otherwise. A failed run's `command_rendering` (argv, environment variable names only,
working directory, output paths, at most 4 KiB, `redacted: false`) is shown beside them.

**`GET /v1/runs/{run_id}/logs/{stdout|stderr|result}`** (with `--history-objects`) is the
copy of a failed run's log that history kept, which outlives the blob it was copied from
(30 days, `r30`). It is raw bytes, `application/octet-stream` as an attachment, with
`X-Content-Type-Options: nosniff` and `Content-Security-Policy: sandbox` on every answer.
The run's instance is asked of `invocations.getAuthorizer` before the copy is read, and the
copy is read for that instance only. One `Range: bytes=...` is honoured (`206`, or `416`),
and only the blocks that hold the range are read; several ranges are ignored. The bytes of
an answer count against the same read budget as `/v1/blobs`, and a read waits for it at
most 10 seconds, then is `503` with `Retry-After` and `"busy": true`. A run with no such
copy (it passed, the log was over 16 MiB, or it had none) is `404` `no_log`; an unknown run
`404` `no_run`; a query string is `400`.

**Across builds** (with `--history-objects`; otherwise these routes do not exist, `404` as
any unknown path). The invocation list and the shape and test routes are read from the index
rows the history writer keeps, never from an object, so they answer the same after a
restart and cost no object read; the grid reads the run records of the chunks of the builds
it names (`docs/design/ui-data.md` 4.3 and 6.2). They run the same policy as the build pages:
`authenticationPolicy`, then `invocations.getAuthorizer` (`404` with `"off"` when it is not
named). The authorizer is asked once per request about each instance the request would
touch, and every answer holds only rows of allowed instances; an invocation is listed only
when every instance it used is allowed. `?instance=` names one instance; one the caller
may not read is `403`. Lists are paged as above (`page_size` 1 to 200, 100 by default;
`next_page` is the link to follow; a token names its route and filters and stays valid
across restarts). An answer over more instances than the index reads at once (256) says
`"partial": true`. Times are RFC 3339 and a `+` in an offset is `%2B` in a query.

| Route | Answers |
|---|---|
| `GET /v1/invocations` | `{"invocations": [...]}`, newest first by the time of the first record; `?instance=`, `?since=`, `?until=`, `?state=open\|closed`. Each item: `id`, `first_seen`, `last_seen`, `state`, `complete`, `counts` (`ran`, `hits`, `failed`), `instances`, `href`. `?commit=` is `400` `needs_report` (no commit is kept before the client report), `?tool=` is `400` `unsupported_filter` |
| `GET /v1/shapes` | `{"shapes": [...]}`, by instance then shape key; `?kind=test\|other`, `?status=passing\|failing\|flaky\|oom_killed`, `?instance=`. `?status=slower` is `400` `unsupported_status` (it needs the day-close fold) and `?target=`, `?program=` are `400` `unsupported_filter` (the catalog keeps no body yet) |
| `GET /v1/tests` | the same with `kind=test` |
| `GET /v1/shapes/{key}?instance=&cfg=` | one `(instance, cfg)` of the key, with `previous_keys: null`; with either absent, `{"shapes": [...]}` of the key's rows (paged) |
| `GET /v1/shapes/{key}/runs?instance=&cfg=` | the last 16 runs of a test, newest first, each `{run_id, run_href, class, execution_ms, source: "run", digest16}` (`run_href` is `GET /v1/runs/{run_id}`), with `complete: false`, `covers: "recent_16"` (`"none"` for a shape that is not a test) and `needs: "day_index"`: older runs are not answered yet. Both parameters are required |
| `GET /v1/grid?shapes=<key>,<key>&invocations=<id>,<id>` (or `last=<n>`) | the outcome of shapes in builds: `{"columns": [...], "rows": [...], "shapes": [...]}`. `shapes` is 1 to 50 keys; the builds are named by `invocations` (1 to 50 different ids) or are the newest `last` (1 to 50, of the instances the caller may read, `?instance=` narrows them). A column is `{id, href, first_seen, complete}` (`complete: false` when the build has more than 20,000 records and the read stopped); a row is one `(instance, key, cfg)` some build ran, `{instance, key, cfg, href, cells}`, with one cell for each column: `{status, run_id, run_href}` with `status` `pass`, `fail` (a failure or a timeout), `oom_killed` or `infra`, from the row's last run in that build that is a result, or `{status: "absent"}` where the build has no run of the row. A build that is not held, or that a caller may not read (any of its instances), is `404` `no_record`. `columns=commit` is `400` `needs_report`; there are no `cached_pass` cells yet |
| `GET /v1/actions/{digest}/runs` | the runs of an action digest (`<64 hex>-<size>`) of today, newest first by end time, `?instance=`, `?class=`; each `{run_id, run_href, instance, worker, class, started, ended, shape}` |
| `GET /v1/runs?node=&from=&to=&class=&instance=` | the runs of today that overlapped `[from, to]` on `node` (`node` is required; both times are optional), newest first by end time |

The last two read the writer's index of today's runs, in memory (64 MiB, the oldest runs
forgotten first, empty after a restart), not the day index: their answers always say
`"partial": true` and `covers_from`, the time from which the index has every run it was
sent. A run of an instance the caller may not read is never listed.

Both list by end time, newest first; two runs that ended in the same millisecond are
listed latest arrival first. Runs reach the index in the order the writer receives them,
and a failed run can arrive after runs that ended later, so the order is the index's own,
not arrival. `next_page` is a cursor, the end time and arrival number of the last run
listed (`<ended_ms>.<seq>` in the token); the next page lists the runs that sort before it (older by that key).
A run that arrives between two pages with an end time older than the cursor is listed on a
later page; one with a newer end time is not listed by the pages that follow (read the
first page again to see it). No run is listed twice or skipped by paging.

A shape row is `{instance, key, cfg, kind, target, first_seen, last_seen, href, runs_href}`
and, for a test, `status` (`passing`, `failing`, `flaky`, `oom_killed`, or `null` while the
runs seen were all infrastructure), `flaky` (`{proven, proof_digest, since}`: the first 16
hex digits of the action digest seen both passing and failing, and the run that completed
the proof), `last_pass` and `first_fail` (`{at, run_id, run_href}`, `null` for none). `key`
is 16 lowercase hex digits. A key no instance the caller may read has is `404`
`{"error": "no_shape"}`.

**The invocation:**

```json
{
  "id": "0b8c...", "url": null,
  "tool": "buck2", "tool_version": "0.1.0", "correlated_invocations_id": null,
  "instances": [""],
  "first_seen": "2026-10-11T09:00:00.000000Z", "last_seen": "2026-10-11T09:04:12.031008Z",
  "state": "open", "complete": true, "rows_capped": false, "actions": 812,
  "counts": {"execute": 812, "cache_hit_execute": 40, "joined": 3, "ran": 769, "failed": 2,
             "ac_hit": 5120, "ac_miss": 812, "blobs_asked": 9000, "blobs_missing": 300,
             "bytes_uploaded": 1234567, "bytes_downloaded": 89012345},
  "collisions": ["0b8c...~2"],
  "links": {"actions": "/v1/invocations/0b8c.../actions", "artifacts": "...", "logs": "...",
            "metrics": "..."},
  "phase_totals": {"execution_ms": {"sum": 912345, "rows": 769},
                   "queue_ms": {"sum": 20431, "rows": 769}},
  "top_slowest": {
    "wall_ms": [{"action": "<digest>", "wall_ms": 61234, "href": "/v1/invocations/0b8c.../actions/<digest>"}],
    "execution_ms": [{"action": "<digest>", "execution_ms": 60011, "href": "..."}]}
}
```

Times are RFC 3339 in UTC to the microsecond; durations are integer microseconds;
digests are `<sha256 hex>-<size>`. `state` is `idle` after 10 minutes with no event and
no operation in flight, else `open`. `url` is `null` (the link template,
`--invocation-url-template`, is planned).

**`phase_totals`** sums, over the rows the farm ran for this invocation (`how` =
`ran`), each phase's milliseconds (see `durations` below; `wall_ms` is not summed):
`sum`, over the rows that measured the phase, and `rows`, how many did (a mean is
`sum / rows`); a phase no such row measured is left out. **`top_slowest`** lists, for `wall_ms` and for
`execution_ms`, the 20 such rows that took longest, longest first (ties in row order),
each with its `action`, its milliseconds and its `href`. Both are computed from the
rows at each request.

**An action row** (on `/actions`, and as the start of `/actions/{digest}`):
`action`, `instance`, `target`, `mnemonic`, `configuration` and `action_id` (each
`{"value", "truncated"}` or `null`), `how`, `operation`, `attempts` (each with `lease`,
`worker`, `queued`, `granted`, `started`, `ended` and `end`: `{"result": "ok"}`,
`{"requeue": "<reason>", "reason": "<text>"}` or `{"failure": "<class>", "reason":
"<text>"}`, `null` while held), `outcome` (`{"ok": {"exit_code": n}}`, `{"failed":
{"code": n, "message": "..."}}`, `{"timed_out": {}}`, `{"refused": {"reason": "..."}}`,
or `null` in flight), `result` and `command` (digests or `null`) and `href`, the row's
own route; then the keys added since (none of the keys above changed):

- `shape`: what the server read from the action's `Command` when it was queued, or
  `null` (a cache hit, or a Command not read): `key` (the shape key, 16 hex digits, as
  the `action usage:` line writes it), `cfg` (the configuration hash an output path
  named, or `null`) and `target`, the target the action's own `buck-out` paths name
  (`{"label": "cell//pkg:name", "truncated", "kind": "target"|"anon"|"bxl"}`), or
  `null` when they name none (a remotely run test whose outputs sit under
  `buck-out/v2/test/execution/` and whose program is not a `buck-out` path): it is
  read, never guessed. The row's `target` above is what the client's
  `RequestMetadata` said, a hint only (buck2 sends none).
- `durations`, in whole milliseconds, each left out when not measured: `wall_ms`, from
  the first lease's queueing to the end of the lease that ended the operation
  (requeues included); `queue_ms`, queued to granted summed over its leases so far;
  `execution_ms`, `input_fetch_ms` and `output_upload_ms`, each `start` to `completed`
  of the REAPI times of the run of the lease that ended the operation. All but
  `queue_ms` are left out until a lease ends the operation (accepted or failed; a
  requeued lease's end is not the last), and for one that ended with no lease ending
  it, so once given they never change; `queue_ms` only grows, by each lease granted
  after a requeue.
- on each attempt: `memory_kill` once it ended (`none`, `own_limit` or
  `node_pressure`, read from how it ended); `booked_memory_bytes` for a lease killed
  for memory (the one that ended the operation too), `cap_bytes` for an own-limit kill
  (the largest node that could run it), `raised_memory_bytes` for an own-limit
  requeue (the next booking); and, for a lease whose report the scheduler took,
  `reapi_times` (each of REAPI's nine `ExecutedActionMetadata` times it has, by name
  without `_timestamp`, as its stamped result would carry them: `queued` is the
  operation's first submission, and the worker start and completion are the server's
  when the daemon sent none) and `usage` (`wall_us`, each cgroup counter the daemon
  read: `cpu_usage_us`, `cpu_user_us`, `cpu_system_us`, `cpu_periods`,
  `cpu_throttled`, `throttled_us`, `memory_peak_bytes`; `parallelism`, CPU time over
  wall time; `numa_bytes`, `[{"node", "bytes"}]`). A key not measured is left out,
  never zero.

A timed-out run's stdout and stderr (those the CAS holds) are its row's `stdout` and
`stderr`; its `result` stays `null`, since a partial result is never stored. `/actions/{digest}` adds `outputs` (each `path`, `digest`, `executable`,
`directory`, `available` and `href`), `outputs_truncated`, `stdout` and `stderr` (each
`{"digest", "available", "href"}` or `null`), `usage` (or `null`) and `blobs`
(`action` and `command` links). An output directory is listed as its Tree digest.

**`available`** says whether the cache still holds the blob, read from its index
without touching it (a page keeps no blob); `null` if the index does not answer.
**`href`** links the blob route, `/v1/blobs/{digest}?instance=<the row's
instance>&name=<file name>`, with `&inline=text` for a log of at most 4 MiB; every
`href` is `null` while the blob route is off.

**`/artifacts`** lists each output as an output of `/actions/{digest}` is, with its
`action`. **`/logs`** lists each action whose result has a stdout or a stderr:
`action`, `href`, `failed` (anything but OK with exit code 0), `outcome`, `stdout` and
`stderr`. The failed ones come first, in row order, then the others.

**`/metrics`**: `counts` as above; `queue_micros` (queued to granted) and `run_micros`
(granted to ended) over the leases of the rows the farm ran for this invocation
(`how` = `ran`; a joined row's leases count where its twin ran), each with `count`,
`min`, `p50`, `p90`, `max` and `sum` (`null` when empty); `reruns` by requeue reason;
`usage_actions`, the rows whose result carries usage, and over them `cpu_micros`
(user plus system) and `peak_memory_bytes`, each `{"sum", "max"}`, `null` when no row
does. They are computed from the rows at each request. `p50` and `p90` are approximate:
never below the true value and at most 1/16 above it (exact below 16 µs); the rest are
exact.

**Filters and order of `/actions`.** `?requeue_reason=` keeps the rows with a lease
given up for that reason (`silent`, `replaced`, `reconnected`, `not_started`,
`session_switch`, `out_of_memory`, `node_memory_pressure`, `restarted`, `lost`,
`infra`), whatever the action's end: an action killed for its own limit, run again and
then OK is kept by `out_of_memory`. `?memory_kill=own_limit` (or `node_pressure`) keeps
the rows with a lease killed that way; `none`, the rows with no lease killed for
memory, cache-hit rows (which have no leases) included. `?sort=<phase>` lists the rows that measured that phase (`durations` above),
longest first, ties in row order; its pages resume after the last row listed. No row
is listed twice; a row whose `queue_ms` grows past that point after it was passed (it
was requeued and granted again) is not listed again, and a row that measures the
phase only after the pages passed its place (its operation ended since) is not listed.
Filters combine, and `next_page` carries them all.

**Pages.** `/actions`, `/artifacts` and `/logs` take `?page_size=` (100 by default,
1 to 1000) and `?page_token=`, the `next_page_token` of the page before, with the same
filter; the answer is `{"actions": [...]}` (or `artifacts`, `logs`) and, unless it is
the last page, `next_page_token` and `next_page`, the route's path with the same filter,
page size and that token: the link to follow. A token is opaque: it names the record it
was made for, its filter and a position. Rows are only appended, so a build still
writing never makes a page skip or repeat a row. Outputs and logs are listed as each
row held them when a page passed it: an action still running then is not listed when
it ends (read its row, or start again). On `/logs`, rows added after the failed ones
ran out are listed with the rest, failed or not.

| Code | When |
|---|---|
| `200` | the answer |
| `400` | a parameter the route does not take, one given twice, a `page_size` outside 1 to 1000, a filter value not listed above, or a page token that does not decode, was made for another route or another filter, or names a position past the rows |
| `401` | the policy's `authenticationPolicy` refuses the request |
| `403` | `invocations.getAuthorizer` refuses one of the invocation's instance names: each one its calls used and each one a row holds, on every route |
| `404` | the routes are off: `{"error": "<why>", "off": "invocations"}`; no such record is held (never made, removed at the memory bound, or lost at a restart; with `--history-objects`, and not in history either), a page token's included even if a record has been made since under the same key: `{"error": "no_record", "id": "<id>", "records_since": "<time>"}` (below); no such route, `{id}` is not an id or `<id>~<n>`, or no row for the action: `{"error": "<why>"}` |
| `503` | the record gained an instance name while it was read: send the request again; with `--history-objects`, history could not be read (`{"error": "build history cannot be read right now"}`) |

**A record that is not held** answers `{"error": "no_record", "id": "<id>",
"records_since": "<time>"}` ([invocations-api.md](design/invocations-api.md#410-a-record-that-is-gone)).
`records_since` is the later of the server's start and the `last_seen` of the newest
record of a client's id removed at the memory bound (removed minted records do not
count): a build whose calls went on after it would still be held, so its `404` means
the server saw no call with that id; a build that ended before it may have been lost.
The `off` body of a route that is switched off is never this one.

## `GET /v1/blobs/{digest}`

A CAS blob's bytes over HTTP, for a browser: the outputs, stdout and stderr a build's
page links to ([invocations.md](design/invocations.md#46-blobs-over-http)).

```
GET /v1/blobs/<sha256 hex>-<size>?instance=<instance name>&name=<file name>&inline=text
```

**Off unless the REAPI policy names it.** Unlike the rest of this API, the route does
not answer whoever reaches the address: blobs hold build outputs and logs, which can
hold secrets. It runs the policy of `--reapi-auth-policy`
([reapi-auth.md](reapi-auth.md)): its `authenticationPolicy` on the request, then its
`blobs.getAuthorizer` with the `instance` parameter. Without that key, without the
build pages' `invocations.getAuthorizer` (blobs are what the pages link to, and are off
with them), or without a policy file, every request is `404` and the start line says
` blobs=off` (before ` api=`), with the body `{"error": "<why>", "off": "blobs"}`;
with both, ` blobs=on`. `--invocations-open` fills in `allow` for either key the file
leaves out.

| Parameter | Meaning |
|---|---|
| `{digest}` | the blob: 64 lowercase hex digits, `-`, the size in bytes |
| `instance` | the instance name the authorizer is asked about; absent is the empty name. The cache does not separate blobs by instance ([reapi-auth.md](reapi-auth.md#authorizers)), so it gates the call only |
| `name` | the file name a browser saves the bytes under; absent or empty sends none |
| `inline=text` | show the bytes in the browser as text instead of downloading them; `text` is the one value |

A parameter not in this table, one given twice, or another `inline` value is `400`.

**The bytes.** The blob is read through the cache's read path, the one ByteStream Read
takes: the whole blob is checked against its digest before a byte is sent (a stored
object whose bytes are wrong is `503` and is never sent), and the read counts as a use
of the blob for its retention, as a ByteStream Read does. The answer is
`application/octet-stream` with `Content-Disposition: attachment`, and with `name`,
`attachment; filename*=UTF-8''<name>`: the name percent-encoded as RFC 8187 says, so a
quote, a `;` or a line break in it can never end the value or add a parameter. With
`inline=text` it is `text/plain; charset=utf-8` and `inline` (with the same
`filename*`), for a blob of at most 4 MiB (4194304 bytes); a larger one is `400`, and
still downloads without `inline`.

**Memory.** Like a ByteStream Read, a read holds one window of its blob at a time
(8 MiB, `READ_WINDOW_BYTES`; a smaller blob, its size). Every byte of the blob is
hashed, the bytes before a range and after it too, and the last window is sent only
once the digest checked: a blob of one window is checked whole before a byte is sent,
and one that fails is an error answer; a larger blob whose bytes fail ends its body
early (the connection is cut), and the client must not take what it got for the blob.
The route keeps a budget: the windows of all the answers it is reading or still sending
hold at most 1 GiB (1073741824 bytes) at once. An answer gives its share back when its
body has been sent or dropped (the client went away), and a failed read gives it back at
once.

- **Waiting.** A read whose blob does not fit in what is left waits in line, first come
  first served, before a byte is read: a large read at the head of the line is not
  passed by smaller ones behind it, so a stream of small reads cannot starve it (and
  they wait while it does). A read still waiting after 10 seconds is `503` with
  `Retry-After: 1` and the body `{"error": "<why>", "busy": true}`; a blob that cannot
  be read is `503` without either, so a client can tell busy from broken.
- **Any size.** No blob is too large to read. A blob not held is `404` before it waits,
  whatever size its digest claims.
- **Slow clients.** A client holds its blob's share until its answer is sent or it goes
  away; the listener has no write timeout.

**Never a page.** Every answer of the route, the errors and `404` included, carries
`X-Content-Type-Options: nosniff` and `Content-Security-Policy: sandbox`, so stdout that
holds HTML is shown as text and never runs as a page on the API's origin.

**Ranges.** One range of `Range: bytes=...` (`a-b`, `a-`, `-n`) is answered `206` with
`Content-Range: bytes a-b/<size>`; a last byte past the end is read as the end. A range
that starts at or past the end (or any range of an empty blob) is `416` with
`Content-Range: bytes */<size>`. Several ranges, another unit or a malformed value are
ignored, as RFC 9110 allows, and the whole blob is sent. Every `200` and `206` says
`Accept-Ranges: bytes`.

| Answer | When |
|---|---|
| `200`, `206` | the bytes, as above |
| `400` | a malformed digest or query, or `inline=text` for a blob over 4 MiB |
| `401` | the authentication policy refuses the request |
| `403` | `blobs.getAuthorizer` refuses the instance name |
| `404` | the route is off, or the blob is not held |
| `416` | the range starts past the end |
| `503` | busy: the blob's share of the memory budget was not free within 10 seconds (`Retry-After: 1`, `"busy": true`); or the blob is held but its bytes cannot be read (missing from the store, or failing their digest), or the store or the metadata log does not answer |
| `500` | the cache's own records disagree |

Errors are JSON, `{"error": "<why>"}`, as everywhere in this API.

## `STALE_MAP` on a storage node's gRPC calls

Not part of the HTTP API: the answer a storage node (`kbf storage`) gives over gRPC to a
caller that routed a call by an older storage map than the node holds
([architecture-v2](design/architecture-v2.md) section 6). It is listed here because
callers (the frontend, the daemons) and tools read it from the wire.

**The caller's generation** is on every call: field 1 (`map_generation`) of every
`kbf.storage.v1` request (`ActionStore.Get`, `Prepare`, `Commit`), and the ASCII metadata
entry `kbf-map-generation` (the decimal generation) on every other call:
`ContentAddressableStorage.FindMissingBlobs`, `ByteStream.Read`, `Write` and
`QueryWriteStatus`, and each `kbf.worker.v1.Segments` call. A call with no entry is
generation 0, "no map seen". An entry that is not a decimal `u64` is `INVALID_ARGUMENT`.

**The node's judgement**, made before the call reads anything (so no answer, not even
"already stored", is given under a placement the node no longer holds). A node that is
one of several (it is given a source of maps) and has no map yet fetches one the same way
and serves no call, whatever the caller's generation, until it has one: `UNAVAILABLE`. A
node that is not part of a multi-member farm has no such source and judges nothing.

| the caller's generation | the node |
|---|---|
| equal to the node's | serves the call |
| older than the node's (0 included) | answers `STALE_MAP` |
| newer than the node's, or the node has no map yet | fetches the map, waiting at most 2 seconds, then judges again: serves it if the node is now at the caller's generation, `STALE_MAP` if past it; `UNAVAILABLE` if no map came |

**The encoding of `STALE_MAP`:** the gRPC status `FAILED_PRECONDITION` with the trailing
metadata entry `kbf-map-generation: <n>` (decimal), the node's generation. The message
is words for a log and is never matched. `FAILED_PRECONDITION` alone is not `STALE_MAP`:
an action-cache write refused for a closure blob the node does not hold answers that code
too, and carries no such entry. A reader takes the status for `STALE_MAP` only with both
the code and the entry, and takes the entry for a number only if it is decimal.

**What the caller does:** fetches a map of generation `<n>` or newer (the next `MapUpdate`
on its session, or a `MapRequest`: [worker-protocol.md](design/worker-protocol.md)) and
runs the call again, once. A second `STALE_MAP`, or a fetch that brings no newer map, is
treated as `UNAVAILABLE`. `UNAVAILABLE` for a caller ahead of the node is the node's
lost push: the caller's map may be the right one, so the caller tries again, and the
node's next push makes the call succeed.

## `PUT /v1/releases/{commit}/{platform}/{name}` and `GET /v1/releases`

The frontend's release store (`docs/design/pull-updates.md` section 6.3): the tarballs of a
release and their build attestations, kept as uploaded. Mounted by `--releases-dir`; without a store both answer
`503` naming `--releases-dir`.

- `commit` is 40 lowercase hex digits, `platform` is `linux-x86_64`, `linux-arm64` or
  `darwin-arm64`, `name` is `kbf` or `kbf-daemon` (the tarball) or either with the suffix
  `.attestation` (its build attestation bundle). Anything else is `400`; no segment is ever joined
  into a path as it arrived.
- `PUT` takes the file as the body, with the write gates of the section "Who may write" except
  the JSON content type (loopback peer, and the proxy's secret with `--proxy-auth-file`, no `Origin`, the bearer token). A tarball is at most
  512 MiB (`413` past it), a bundle at most 256 KiB; an empty body is `400`. The body is written to
  a `.partial` file, synced, and renamed into place, so an interrupted upload leaves nothing
  visible.
- `201` `{"sha256", "size"}` for a new key. A release is immutable: the same bytes again are
  `200` with the same answer, other bytes are `409` and change nothing.
- `GET` is open and lists `{"releases": [{"commit", "platforms": [{"platform", "complete",
  "components": [{"name", "sha256", "size", "attested"}]}]}]}`: commits sorted, platforms in the
  order above, a platform `complete` when it holds, with their attestations, the tarballs its
  hosts need: `kbf` for a Linux platform, `kbf-daemon` for `darwin-arm64`. A component is
  listed when its tarball is stored.
- `GET /v1/releases` and the gRPC `ReleaseService.Fetch` are unauthenticated at the router and
  the service. `Fetch` is mounted on the worker listener, behind its mutual TLS and the role
  table (the daemon role alone may call it) when TLS is configured; in plain-text worker mode
  it is open, like every worker method. `GET /v1/releases` is open to whoever reaches the
  operator API, like `GET /v1/nodes`: it lists commits, platforms and digests, no bytes. The
  store is mounted by `--releases-dir`, which needs `--api-listen`; without the flag every
  update endpoint answers `503`.

## `GET /v1/update` and `POST /v1/update/{desired,pause,resume}`

The desired release (`docs/design/pull-updates.md` section 6.3), kept in the `update/desired`
row of the state store. Without `--state-store` every answer is `503` naming it; without
`--releases-dir`, `GET /v1/update` and `POST /v1/update/desired` are `503` naming that flag
(`pause` and `resume` need only the state store). A write that other writers kept racing
for the whole of its retries is `503` with `Retry-After: 1`: send it again.

- `GET /v1/update` is open: `{"desired": null | {"generation", "commit", "set_at_ms"},
  "paused": bool, "policy": null | {...}, "releases": <the body of GET /v1/releases>}`. `policy`
  is the `update/policy` row when the operator wrote one (`paused`, `auto_nodes`,
  `auto_frontend`, `new_work_only_to_updated`, `accept_failing_builds`, `max_hosts_out`,
  `min_serving` and the limits in seconds), else `null`; `paused` is its flag (false without
  a row).
- `POST /v1/update/desired` with `{"commit": "<40 lowercase hex>"}` and, optionally,
  `"expect_generation": N` (JSON content type, the write gates) offers the commit. With
  `expect_generation` it is a compare-and-set: it writes only if the desired release is at
  generation N (0 before any offer), else `409` naming both and nothing is written. `409` unless the release store holds some platform of it complete
  (see above); `400` for a body that is not exactly that. The answer is the desired release.
  The generation moves only when the commit differs from the desired one, so a repeated click is
  not a new run; a rollback is the older commit offered the same way and gets a higher
  generation. Generations increase, but may skip numbers: each write that loses a race has
  already drawn a counter value.
- `POST /v1/update/pause` and `POST /v1/update/resume` (the write gates, no body) set `paused` of the policy
  row, by compare-and-set of that field alone, and answer `{"paused": bool}`. No run is
  needed, and an offer leaves the flag as it is: while set, no host is started.

## `GET /v1/update/hosts`

What each host last said about its update, for the update page and for agents
(`docs/design/update-ui.md` section 5). **Off unless the policy names `farm.getAuthorizer`**
([reapi-auth.md](reapi-auth.md)): the request runs the policy's authenticator and then that
authorizer (asked with the empty instance name), and without the key or a policy file every
request is `404` with `"off": "farm"`. It carries what the open `GET /v1/nodes` must not: the
commits a host runs, staged and installed, and its refusal text.

```json
{ "desired": { "generation": 3, "commit": "<40 hex>", "set_at_ms": 1791370000000 },
  "paused": false,
  "hosts": [
    { "node_id": "linux-1", "connected": true, "availability": "ok", "state": "refused",
      "up_to_date": false, "desired_commit": "<40 hex>", "running_commit": "0123456789ab",
      "installed_commit": "<40 hex>", "staged_commit": "", "refusal": "no helper",
      "offered_generation": 3, "helper_present": false, "reported_at_unix_ms": 1791370000000 } ],
  "next_page_token": null }
```

- `desired` is the row of `GET /v1/update` (`null` before any offer, and without `--state-store`).
  `paused` is the policy row's `paused` (the pause lives there alone), `false` without a policy
  row (as on `GET /v1/update`), and `null` (unknown, never `false`) if the policy object has no
  boolean `paused`.
- Each host has `availability`, `state` and `up_to_date` as on `GET /v1/nodes`. The rest is as
  the host last reported, even if it is no longer `ok` (`null` for a host that never sent an
  `UpdateStatus`): `running_commit` (12 hex digits or `unknown`), `installed_commit` and
  `staged_commit` (`""` if none or not known), `refusal`, `offered_generation`,
  `helper_present` and `reported_at_unix_ms` (the server's clock).
- **Text from a host is data.** Each string is kept cut to 64 bytes (a commit) or 512 bytes
  (`refusal`) on a character boundary, with control characters replaced by U+FFFD. A reader
  escapes it as it would any string from outside.
- Hosts are in node-id order. `?page_size=` (100 by default, 1 to 1000) and `?page_token=` (the
  `next_page_token` of the page before, null on the last page); any other query key or a bad
  page size is `400`. A state store that fails is `500`, not hosts without a desired commit.

## `UNAVAILABLE` from a storage node that is catching up

Also gRPC, not HTTP. A storage node that is rebuilding its index (it registered with
`catching_up` set in `kbf.map.v1.RegisterRequest`, and is `CATCHING_UP` in the map) may
hold a blob it has not indexed yet, so it never answers that something is missing:

| call | while catching up |
|---|---|
| `ByteStream.Read` of a blob it does not find | `UNAVAILABLE`, not `NOT_FOUND` |
| `FindMissingBlobs` with any digest it does not find | the whole call `UNAVAILABLE` (an answer naming fewer digests would say the rest are held) |
| `ActionStore.Get` with no entry | `UNAVAILABLE`, not an empty answer |
| `ActionStore.Prepare` or `Commit` refused for a closure blob it does not find | `UNAVAILABLE`, not the `ABSENT` refusal |

Every other answer is the usual one: hits are served, writes are taken, and the other
refusals stand. The message begins `catching up:`; it is words for a log and is never
matched. A caller treats the node as an owner that could not say, and asks the next owner
(a router answers `NOT_FOUND` only if every owner did). The node stops at the first
heartbeat after its rebuild that sends `catching_up` false: the frontend makes it `ACTIVE`
in the next map generation.

## Planned

`/v1/software`, `/v1/rollouts` and the per-node update state of
[fleet-updates.md](design/fleet-updates.md) section 11.4.
