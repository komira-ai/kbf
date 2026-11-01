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
  past this check can read one. The proxy sets `x-kbf-node-id`, `x-kbf-role` and `x-kbf-node-ips` for a call
  whose client certificate it verified; the worker listener takes those (and no TLS flag)
  in place of a certificate: the role table, the `Hello.node_id` binding, the blob calls
  and `Membership`'s role check use them. A `Hello` whose node id is not the proxy's
  `x-kbf-node-id` is refused `PERMISSION_DENIED`. A `Membership` registration
  or heartbeat has no certificate behind the proxy: its names are the proxied node id and
  the IP addresses in `x-kbf-node-ips` (those of the verified leaf; see
  `docs/design/frontend-proxy.md` section 3), so the host of its instance id and its address
  must be that name (up to ASCII case) or one of those addresses (exact equality). A
  malformed, repeated or oversized `x-kbf-node-ips` is refused `PERMISSION_DENIED`; without
  one, an IP host is refused. The proxied role must be `storage`. The proxy also sets
  `x-kbf-client-addr`, the address the client connected from (canonical IP text, no
  port), on every call; only the per-caller read bound uses it, and a value that is not one
  IP address is refused `PERMISSION_DENIED` (HTTP 403).

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
| `protocol` | the `kbf.worker.v1` version the `Welcome` that answered the `Hello` opening its newest stream named: the lesser of the `Hello`'s version and the newest the server speaks, so a daemon whose `Hello` names 5 against a server speaking 3 shows 3 ([worker-protocol.md](design/worker-protocol.md)); `null` if no version is known (a `Hello` naming 0, which is not a version, from a server that accepts it). A `Hello` resent on the stream does not change it, and a node that returns on another version shows the new one. It stays after the stream closes: it is the version the node last spoke |
| `blocked_on` | only on a `joining` member, only when the frontend runs with `--auto-promote`: a list of sentences saying what holds its automatic promotion back (a member that has not flushed, a tail epoch not acknowledged, a member down, the settle time). Absent otherwise; an empty answer for a joiner means only its next beat or the next sweep is owed |
| `oldest_protocol` | the least `protocol` among the listed nodes whose `protocol` is not `null`, connected or not (a node whose stream closed may return on the version it last spoke); `null` if no listed node names one. Nodes that name none are not in it |
| `protocol_unknown` | how many listed nodes have a `null` `protocol`. They are never counted in `oldest_protocol`, so `oldest_protocol` says "every node but these speaks at least this": a reader that needs every node reads it with `protocol_unknown` equal to 0 |
| `software` | the newest `NodeStatus` it sent ([worker-protocol.md](design/worker-protocol.md#nodestatus)); `null` from a daemon that predates it. An empty string or list is a value the node could not read |
| `software.received_at_unix_ms` | when the server received it, by the server's clock |
| `software.xcode_builds` | the Xcode builds actions can use on it: only these route work |
| `software.xcodes` | every installed Xcode, ready or not: `app`, `build` (empty if not known), `state` (`ready`, `license_not_accepted`, `first_launch_not_run`, `metal_toolchain_missing`, `failed`, `not_surveyed` from the daemon's start until its first survey of its Xcodes ends (never advertised; the daemon sends its status again when the survey ends), or `unknown` for a state this server does not know), `reason` (the check that failed and its answer, from the daemon's last survey that changed this Xcode's `DEVELOPER_DIR`, build or state; a survey that differs only in its reason is not sent, so when another check later fails with the same build and state, this is still the earlier check's text) and `fix` (the command an administrator runs on the node, when one is known). Empty from a daemon that predates it |
| `software.container_images` | the container images the node was pinned to (`kbf-daemon --image`, container driver), as `docker://<repo>@sha256:<digest>`, sorted: each was in its image store, as one architecture's manifest, when the daemon started (a daemon that finds one missing does not start). Status only: no work is placed by image. Empty when none is pinned, on a Mac, and from a daemon that predates it |
| `needs_attention` | what an operator must do on the node, one line per item: today each installed Xcode that is not ready, as `Xcode <build> (<app>) installed but not ready: <reason>; fix: <command>`. An Xcode `not_surveyed` keeps the item the same app had in the node's previous status (as when a restarted daemon sends its first status), until a survey reports it; one with no previous item has none. The server also logs each item once at `WARN` (target `kbf_server::attention`) when it appears, and at `INFO` when a status reports it gone (its Xcode ready, changed or removed); a status that repeats an item, or lists its Xcode `not_surveyed`, logs nothing. An item is the same while its Xcode's app, build, state and fix are, so a reason that changes alone is shown here but not logged again. The previous status is kept in the server's memory: after a server restart, each item is logged again when its node reports it; kbf has no alert delivery yet (issue #189) |
| `lease_count` | how many leases it holds now (granted or running), the number of entries of `GET /v1/nodes/{id}`'s `leases`. In memory like the rest: after a server restart it is 0 until the daemon's leases are granted again |
| `booked` | what those leases book of its capacity: `{"cpu_millis", "memory_bytes", "gpus", "vms"}`, CPU in thousandths of a core, memory in bytes. A record of the leases' limits, not a measurement of use |
| `placement.state` | `serving`; `cordoned` (no new lease, its leases run on); `draining` (cordoned, waiting for its leases until `deadline_unix_ms`, which is left out for a drain with no deadline); `drained` (cordoned, no lease left: either its leases ended, or the node disconnected and, after the lease grace, its leases were given up and requeued to run elsewhere; check `connected`); `drain_paused` (the deadline passed with leases still running: they run on, and nothing proceeds until an operator acts) |
| `placement.leases` | while draining or paused: the leases it still holds, as `term.seq` (each server process has its own term, the Raft term under the Raft metadata log: [worker-protocol.md](design/worker-protocol.md#server-restarts-and-the-lease-epoch)) |

The list is the nodes this server process has seen, so it says nothing of a daemon that has
not connected to it yet: a server that has just started, or one of several frontends that
share a state store ([design/frontend-proxy.md](design/frontend-proxy.md)), lists only
those that have registered with it. `oldest_protocol` and `protocol_unknown` describe that list and
no more. The check that a release may stop accepting the oldest version
([design/frontend-proxy.md](design/frontend-proxy.md) section 9, rule R1) reads them; it is
not made by this server.

## `GET /v1/nodes/{id}`

One node: every field of its entry in `GET /v1/nodes`, three more, a `load` when its daemon
has sent readings, and an `input_cache` when it has sent input cache counts, a read open like the
list:

```json
{ "node_id": "linux-1", "connected": true, "protocol": 1, "software": null,
  "needs_attention": [], "placement": { "state": "serving" }, "phase": { "state": "serving" },
  "lease_count": 1,
  "booked": { "cpu_millis": 1000, "memory_bytes": 1073741824, "gpus": 0, "vms": 0 },
  "capacity": { "cpu_millis": 8000, "memory_bytes": 17179869184, "gpus": 0, "vms": 0 },
  "free": { "cpu_millis": 7000, "memory_bytes": 16106127360, "gpus": 0, "vms": 0 },
  "leases": ["117399224320012061.42"] }
```

| Field | Meaning |
|---|---|
| `capacity` | what the node's newest `Hello` said it can run at once |
| `free` | `capacity` less `booked` on each axis, never below zero |
| `leases` | the leases it holds, as `term.seq`, in lease order |
| `owner` | who placed the node out of placement: `operator` or `updater:<unit>`. Present while the node is cordoned, draining or drained, left out while it serves. A cordon with no recorded owner is `operator`, and so is one whose recorded owner cannot be read. No writer records an owner yet, so every cordon reads `operator` |

### Node load

`load` is what the node's daemon reports of the CPU and memory its actions use, from the
`node_load` of its heartbeats. The server also hands each accepted reading to the scheduler, which
places by it only under `--placement-load on`. It is left out when the server has accepted no
reading of the node (a daemon or runtime that takes none, a failed read, a node another frontend
holds), and it is in memory: a server restart forgets it.

```json
"load": { "measured_cpu_millicores": 2500, "memory_working_set_bytes": 1000,
          "memory_active_file_bytes": 200, "admission_working_set_bytes": 1000,
          "covered": ["117399224320012061.40"], "actions_cpu_limited": false,
          "cpu_throttled_usec": 7, "cpu_pressure_some_total_usec": 12345,
          "lists": { "clocks": 12, "minutes": 40,
                     "dropped": { "invalid": 0, "duplicate": 0, "over_cap": 0 },
                     "refused_minutes": 0 },
          "stale": false }
```

| Field | Meaning |
|---|---|
| `measured_cpu_millicores` | the CPU the node's actions used, in thousandths of a CPU: the larger of the last two windows between accepted readings, each the cgroup's CPU time over the daemon's clock time. `null` before two windows are known, and when `stale` |
| `memory_working_set_bytes` | the actions cgroup's `memory.current` less its inactive file pages, at the last accepted reading |
| `memory_active_file_bytes` | the cgroup's active file pages at the last accepted reading |
| `admission_working_set_bytes` | `memory_working_set_bytes` less the active file pages beyond a quarter of the node's `capacity` memory, never below zero: active page cache the kernel has not yet marked reclaimable must not make an idle node look full |
| `covered` | the leases, as `term.seq`, that ran for the whole window before the last reading |
| `actions_cpu_limited` | whether the actions cgroup has a CPU quota |
| `cpu_throttled_usec`, `cpu_pressure_some_total_usec` | cumulative CPU throttled time and CPU pressure (`some` total), in microseconds; telemetry. Left out when the daemon could not read them |
| `lists` | what the `lease_clocks` and `minutes` lists of the node's readings have come to since its daemon process began (a restarted daemon counts again from 0), so that a dropped record, or an empty feed, can be seen: `clocks` and `minutes` are the records kept, summed over the accepted readings; `dropped` counts the records left out of the two lists and of `covered`, by reason (`invalid`: a lease missing, a minute above 63, a working set above 2^45 bytes, a mean above its peak, a start after the reading; `duplicate`: a repeat of an earlier kept record; `over_cap`: a record past the cap of its list, 96 for the clocks and `covered`, 64 for the minutes); `refused_minutes` counts the minutes carried by readings that were not accepted |
| `stale` | no reading was accepted for three windows (a window being the time between the last two accepted readings) |

A reading is accepted only if its `seq` is above the last accepted one of the same daemon process;
a restarted daemon starts again, and its first reading is the new baseline. A reading is dropped
(it counts as absent, and the last accepted one stays the baseline) when its clock or CPU counter
went back or its clock did not move, when its CPU since the last accepted reading is more than
four cores per hardware thread of the node's `capacity`, or when its working set is more than four
times the node's memory. A lease's busiest window is not here: it is the usage record's
`peak_window_millicores`. The reading's `lease_clocks` and `minutes` lists
([worker-protocol.md](design/worker-protocol.md#heartbeat-and-heartbeatack)) are vetted and counted
(`lists`) and handed to the scheduler with the reading; their records are not shown, and the server
keeps no series of them. `covered` is cut to 96 leases, each once, before it is kept. A heartbeat is
never refused, nor its acknowledgement changed, for what its load carries.

### Input cache counts

`input_cache` is what the node's input cache has done, from the `input_cache` of its heartbeats
(`Heartbeat.input_cache`, [worker-protocol.md](design/worker-protocol.md)). It is left out when the
node has sent none (a daemon whose cache is off, a runtime with no cache, a daemon built before the
field, a node another frontend holds): absent is not zero. It is in memory: a server restart
forgets it until the next heartbeat. The counts are cumulative since the daemon process started,
so a restarted daemon counts again from 0 (a Prometheus counter handles the reset; nothing here
keeps them across it).

```json
"input_cache": { "fills": 120, "fill_bytes": 734003200, "hits": 480, "hit_bytes": 3145728000,
                 "evictions": 3, "evicted_bytes": 41943040,
                 "trees": { "builds": 1, "hits": 9, "evictions": 0 } }
```

| Field | Meaning |
|---|---|
| `fills`, `fill_bytes` | entries written into the cache from the content store, and their bytes. An entry copied from its executable or plain sibling, or adopted from disk, is not a fill |
| `hits`, `hit_bytes` | entries a lease found in the cache and used, and their bytes. The lease that filled an entry does not count it; a lease that waited for another's fill does |
| `evictions`, `evicted_bytes` | entries removed to keep the byte budget or the free-space floor, and their bytes. An entry removed because it failed its check is not one |
| `trees` | the shared tree store's own counts: `builds` (trees built from cache entries), `hits` (acquires that found the tree built) and `evictions`. Left out when the node has no tree store |

The unit is the cache entry (one blob by digest and executable bit), not the file name a lease
places it under: two names of one blob in a lease are one entry, and the per-lease `inputs:` log
line counts names.

`{id}` is the node id as `POST /v1/nodes/{node}:cordon` takes it, percent-encoded as one
path segment. `404` with `{"error": "no_node"}` for a node that has not registered since
this server started and that no frontend of its state store holds, and for an id that ends in
`:cordon`, `:drain` or `:uncordon`: that
is a write's path, and a read of it is never a node (an id that holds another colon,
`host:8080`, is read as it is). A path below the node (`/v1/nodes/{id}/...`) is `404`.
The node's recent failed runs are not here: they are `GET /v1/runs?node=` with
`--history-objects`.

With a state store, a cordon or drain written through another frontend is read at once,
wherever the read lands: the server takes up the store's `cordon/` rows before it answers.
A node connected to another frontend of the store is answered from the store: `placement` is
its `cordon/` row (a drain with no lease left reads as `drained`; the store does not say
whether a drain paused), `leases` and `lease_count` are the leases the store records on it,
`connected` is `false`, and `protocol`, `software`, `booked`, `capacity` and `free` are empty
(they are the other frontend's to say). `503` when that store cannot be read, and when the
store cannot say who holds a node that is not connected to the frontend answering: never "no
such node", and never the node with no lease. A node connected to the frontend answering is
answered from that frontend while its store cannot be read.

## Own-node calls of a host's updater

A host's updater ([deploy/kbf-updater.md](deploy/kbf-updater.md#the-boundary)) drains and reads its
own node without a token. The calls are served by the frontend on its **worker listener**
(`--worker-listen`, mutual TLS against `--worker-client-ca`, the listener daemons dial), not on the
operator API, and only to a certificate with the role `updater` whose one DNS name is the `{id}` of
the call:

| Call | Answer |
|---|---|
| `POST /v1/nodes/{id}:drain` | as the operator API's drain: cordons the node and waits for its leases; the body is `{"deadline_secs": N}` or none (no deadline: the drain waits for the leases however long they run) |
| `POST /v1/nodes/{id}:uncordon` | as the operator API's uncordon |
| `GET /v1/nodes/{id}` | as `GET /v1/nodes/{id}` above |
| `GET /v1/nodes/{id}/storage-health` | below |

Everything else is `403`: another node's id, `:cordon`, any other path (the whole operator API
included), and every call of a certificate that is not an updater's (a daemon's, a storage node's, a
frontend's), whatever the path; a call with no client certificate does not get past the handshake.
The id is compared as written: it is 1 to 63 of `a-z`, `0-9` and `-` (not first or last), nothing
is decoded, and a query string makes the call none of the four. A node on the listener's deny list
(`--worker-deny-list`, or the `deny/` rows) is refused. `404` for a node the frontend has not seen since it started
(a storage host without a daemon part has none). The listener offers `h2` and `http/1.1`; the
updater's client speaks HTTP/1.1 with one request per connection.

**`GET /v1/nodes/{id}/storage-health`** is a storage host's own storage health and nothing else of
the storage:

```json
{ "node": "linux-1", "half": "member",
  "member": { "id": 1, "state": "active", "build_commit": "0123456789ab" },
  "storage": { "members_active": 3, "under_replicated_blobs": 0 } }
```

| Field | Meaning |
|---|---|
| `half` | how the node stands to the storage members: `member` (its storage half is the member whose address it enrolled with, joined as safe-to-stop joins them), `no_storage` (it enrolled with no storage address), `unknown_node` (the enrollment registry does not know it; [adopt](#v1enrolltokens-and-v1enrollnodes) a node provisioned before enrollment), `unlinked` (no member, or more than one, has its address) |
| `why` | for `unknown_node` and `unlinked` only: a sentence saying which; it may repeat the address the node itself enrolled with |
| `member` | for `member` only: that member's own `id`, `state` and `build_commit` (`null` if it said none). No other member is named |
| `storage.members_active` | the storage members that are `active`, as `GET /v1/storage` counts them |
| `storage.under_replicated_blobs` | blobs known to be owed a copy; `null` while that is unknown, never zero for unknown |

A read of the members and the registry: it changes nothing, and a frontend with no state store
answers `404`.

## `GET /v1/members`

The storage members of the current map, as the `Membership` service keeps them
(`kbf.map.v1.Membership`; [architecture-v2.md](design/architecture-v2.md) section 6),
in the map's order. A read, open like `GET /v1/nodes`; it takes no write. Storage
members are not the daemon nodes of `/v1/nodes`: a storage node registers with a
frontend, a daemon opens a worker stream.

```json
{ "generation": 7,
  "members": [
    { "id": 1, "addr": "192.0.2.11:8982", "state": "active", "weight": 1, "protocol": 1,
      "runs_log": 5, "log_version": 4, "build_commit": "0123456789abcdef0123456789abcdef01234567" },
    { "id": 2, "addr": "192.0.2.12:8982", "state": "draining", "weight": 1, "protocol": 2,
      "runs_log": null, "log_version": null, "build_commit": null },
    { "id": 3, "addr": "192.0.2.13:8982", "state": "down", "weight": 2, "protocol": null,
      "runs_log": null, "log_version": null, "build_commit": null }
],
  "oldest_protocol": 1, "protocol_unknown": 1 }
```

| Field | Meaning |
|---|---|
| `generation` | the map's generation; `0` with an empty `members` before the first member has registered |
| `id` | the member id the store assigned at registration (the `member_id` of `RegisterResponse`) |
| `addr` | the address callers dial |
| `state` | `joining` (registered, a write owner, read last), `catching_up`, `active` (a full owner), `draining` (takes no new writes), `down` (its lease lapsed; kept in the map for the dwell, then it leaves the list) or `unknown` (a state a later build wrote that this one does not name; the member stays listed, takes no writes, is never read from and is not counted as `active`) |
| `weight` | its share of the digests relative to the others |
| `protocol` | the protocol version in the `RegisterRequest` that wrote the member's row (a heartbeat and a state change keep it; a new registration of the member replaces it). `null` when the row names none, as a row written before this field existed, and for a `down` member: its row expires with its heartbeats, and the version went with it |
| `runs_log` | the newest metadata-log machine version the node's build runs, as the `RegisterRequest` that wrote the row said (a heartbeat never replaces it; it fills it only when the row holds none). Informational: it is frozen at 5 and gates nothing |
| `log_version` | the machine version the node's metadata log was at in its newest heartbeat (a registration says it too). Informational: no build raises a log any more, so it is the version an earlier build left, and 1 for a new log |
| `build_commit` | the commit the node's binary was built from, as its registration said (a heartbeat never replaces it; it fills it only when the row holds none): 40 hex digits, or the 12-digit commit when the build did not know the full one, or the text `unknown` when the build had no git checkout; at most 64 bytes are kept |
| `oldest_protocol` | the least `protocol` among the members whose `protocol` is not `null`; `null` if none is |
| `protocol_unknown` | how many members have a `null` `protocol`. Never counted in `oldest_protocol`: read the two together, as for [`GET /v1/nodes`](#get-v1nodes) |

The registration is judged against the map's floor once, when it is made
([design/frontend-proxy.md](design/frontend-proxy.md) section 9); `protocol` is what it
named, and nothing judges it again.

`runs_log`, `log_version` and `build_commit` are information only: nothing in the frontend
decides on them yet. Each is `null`, never `0` or `""`, when the node did not say it (a node
of an older build sends none of them), and for a `down` member, whose row went with its
heartbeats. A frontend built before these fields drops them when it rewrites a member's row;
they read `null` until the node's next heartbeat to a frontend that knows them brings them back
(while an older frontend still receives the node's heartbeats, they stay `null`).

A node that registers for the first time in a farm formed more than a minute ago is
`joining` until the operator promotes it with `POST /v1/members/{id}`; unless the frontend runs
with `--auto-promote` (off by default) the server does not promote on its own, so a script that
waits for `active` waits for that. With it, the joiner's own heartbeat promotes it once it is
filled (`docs/design/rebalance-handoff.md`), and `POST` stays the override. A server without a state store keeps no members and answers `404`; a state
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
  `{"deadline_secs": N}` (seconds, any value, 0 included); without a body there is **no
  deadline**: the node stays `draining` until its leases end, then is `drained`, and is never
  `drain_paused`. With no lease left the node is `drained`. If a deadline that was given comes
  first, the drain **pauses**: the
  leases run on, nothing is killed or given up, and the node stays `drain_paused`
  even after they end, until an operator drains it again (a new deadline) or
  uncordons it.
- `:uncordon` (`POST /v1/nodes/{node}:uncordon`): returns the node to placement and ends any drain.

The answer is the node, as `GET /v1/nodes` lists it, after the action. `404` for a
node that no frontend of the state store has held or an unknown verb, `400` for a drain body of
another shape, and `401`, `403` or `415` from the gates of [Who may write](#who-may-write).

With a state store, a node connected to another frontend is placed through the row the
action writes under `cordon/`, which the node's own frontend applies. `503` when the store
cannot say whether another frontend holds a node that is not connected to the frontend
answering (never `404`), and when the store does not take the row. In both the action is not
applied: the row is written first, and the frontend's own scheduler follows it, so a node
connected to the answering frontend keeps the placement it had. A `503` that follows a write
the store did take despite the error can leave the row; read the node to see where it stands.
The same holds for the updater's own-node `:drain` and `:uncordon` below, which a host's
updater takes as a failed call, not as "no daemon here".

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

An operator sets a storage member's state. The body is `{"state": "active"}`,
`{"state": "draining"}` or `{"state": "planned_down"}` (which may add `"for_secs": N`, 1 to
86400, default 900); the answer is `{"id": N, "state": "...", "generation": G}`,
the map's generation after the change. `down` is the store's verdict, `catching_up` is what a registration says and `joining` is
written by a registration (a new node in a farm with data), so none of them is accepted (`400`, as is
any other body). `404` for an id that has no live row (it expired, or never
registered) or is not a number, or on a server without a state store; `503` when the
state store cannot be written; `401`, `403` or `415` from the gates of
[Who may write](#who-may-write), the same gates as the node writes. Nothing is changed
by a refused request.

When a member becomes `active` the cluster also records that it has formed (see below),
in the same transaction.

**A planned restart.** `planned_down` takes an `active` member out of the write path at
once, for a restart an operator is about to make; any other state answers `409`. A member
is `down` in the map only a TTL (15 s by default) after its last heartbeat. Until then a write whose
primary is the stopped node dials it first and then moves on to the next owner, which costs
a connection attempt per write, and an action-cache commit does not move on at all and is
refused `UNAVAILABLE`, which a client retries for about a second and a half. After this call the
map (one new generation, in the answer) has the member `down`, so every router, chain
primary and repair worker that holds the map sends writes to the other owner and reads
the member's blobs from it, and a node or daemon that holds an older map reads it like
any `down` member (the wire has no new state). The member keeps serving what reaches it
and keeps heartbeating until it stops; a heartbeat does not undo this, until `for_secs` has
passed: the first heartbeat after that returns it to `active` (a controller that died, or a
restart that never came, does not strand a healthy node out of service). The `down/<id>`
mark that keeps its slot for the dwell (the same time as for a member that fails) is
written in the same transaction, with a value that says an operator planned it:
[`GET /v1/storage`](#get-v1storage) shows `"state": "down", "planned_down": true` for it.
The end of the plan is kept in the member's row, which a frontend built before it drops when it
rewrites the row, so nothing is planned down while a frontend of an older build serves the same
state store. Asking again for a member already planned down is the same plan with its end moved to
`for_secs` from now: `200`, the same answer, no new generation. It is undone by
`{"state": "active"}` (a restart that did not happen), or by the member
registering again after the restart: `active` if its index is whole, `catching_up` while
it rebuilds, the mark deleted. If it never returns, it leaves the map at the end of the
dwell counted from this call, as any `down` member does. It survives a restart of the
frontend: it is in the store.

**When a member is `active` without this call.** A registration that says the node's
index is whole enters `active`, in a fresh cluster and in a formed one alike: it is the
first start of a cluster, or a restart of a node whose meta log already has its rebuild,
so a rolling restart leaves every member `active`. The registration also writes the
`meta/members_formed` mark. A registration that says the node is rebuilding its index
enters `catching_up` and becomes `active` by its own first beat that says it is done.
The exception is a node the farm has not seen before (a new instance id) that registers
more than a minute after the first `active` member: its store is empty, so it enters
`joining` and an `unfilled/<id>` mark is written with it. While the mark is there every
registration of that member is `joining` whatever it says, and no beat promotes it; this call
(which removes the mark) does. A member that was `active` and restarts, and a
returning instance, are `active` again. Nothing copies the blobs that existed before the
node joined: promoting it says the operator knows it holds what it owns. A replacement host at the
address of a node the farm has seen is a known instance, so it registers `active` with an
empty store (the instance id is the address). A farm whose nodes start more than a minute
apart leaves the later ones `joining` until promoted; `down` members cannot be promoted
until they register again.

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

## `GET /v1/members/{id}/safe-to-stop`

Whether the host of a storage member may be stopped now without leaving a blob with one
copy. The updater of a host asks it before it stops the host's process. It is a read that
changes no member, row or placement, and it passes the same gates as the member write above
([Who may write](#who-may-write): a loopback peer, no `Origin`, the token, a JSON content
type), because its answer decides whether to stop a host. `kbf cluster safe-to-stop <id>
--token-file F` calls it.

```json
{ "safe": false,
  "reasons": ["member m2 owes 5 blobs"],
  "planned_down": false }
```

| Field | Meaning |
|---|---|
| `safe` | `true` if the member can go out without a blob being left with one copy |
| `reasons` | why `safe` is `false`: the worst thing found, one line. Empty when `safe` is `true` for a storage member; for a daemon, one line that says only the daemon was judged |
| `planned_down` | whether the member is already planned down (`POST /v1/members/{id}` with `planned_down`). It does not make the stop unsafe: the member is already out of the write path |

`{id}` is a storage member's number (`GET /v1/members` lists them; `2` for member `m2`)
or the id of the node that runs it. The answer is the storage
gate of the update controller, asked with this member out: every **other** member must be
`active`, heard within the membership's time to live (15 s by default), not planned down,
and report a repair debt that is zero (a debt that is not reported is not zero, and a
report older than two heartbeat intervals is not a report); and at
least one other member must exist to hold the second copy. The member's own state is not
asked, so a member that is already planned down, or already down, is safe to stop; its own
repair debt is. Each way the gate is shut is its own line in `reasons`:

| `reasons` line | Meaning |
|---|---|
| `member mN is Down`, `is Draining` or `is Joining` | a second member is not `active` |
| `member mN is catching up` | a second member is rebuilding its index |
| `member mN is planned down` | a second member is already out for a planned restart |
| `member mN was not heard within the time to live` | a second member counts as down |
| `member mN does not report its repair debt` | its debt is unknown |
| `member mN owes B blobs` | a member, the one asked about included, owes copies |
| `no other member holds the second copy` | the member is the only one |

`mN` is the member's id as the server writes it (`m2`). When several things are wrong, the
line is the worst one: a second member down before a debt.

A node id is judged by the storage member that is the node's storage half, joined as the
update controller joins a host to its member: the address the node advertised when it
enrolled (`GET /v1/enroll/nodes`) is the address one member registered. That member's
answer is the node's, the same body as asking by the member's number. A node that has no
storage half is a daemon, which drains in its own stop and needs no permit: a Mac, a Linux
node enrolled with no storage address, and any node of a server without a state store
(such a server keeps no storage members) answer `200` with `"safe": true` and a `reasons`
line that says only the daemon was judged. A Linux node that cannot be told from one with a
member answers `409` with `{"error": ...}` saying why, never safe: the enrollment registry
does not know it or cannot be read, or its address is no member's or two members'. A
number is a member's number first, then a node id. `404` with `{"error": ...}` for an
`{id}` that is neither a member nor a node that has registered with this server
(`GET /v1/nodes`). `503` when the state store cannot be read, `500` when a stored row does
not decode; `401`, `403` or `415` from the gates.

The answer is about the members as the server sees them now: a heartbeat is up to a time
to live old, so the updater asks again just before it stops, and a `false` is retried
rather than overridden.

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

**What it cuts.** The entry is applied to the storage nodes, the membership service and the
worker listener: a daemon whose certificate or node is denied here is refused on its stream
(`Hello`, `Heartbeat`, `Result`, ...), its open stream ends at its next message, and its blob
calls on that listener are refused `PERMISSION_DENIED`. The listener checks against the membership's copy of these rows (refreshed by this call before
it answers, and every beat) beside `--worker-deny-list`; when the copy is more than three sweeps
old, every check is refused `UNAVAILABLE` (see Revocation, above). The
file stays what it was: a list the listener alone reads, which takes effect without the state
store. `GET /v1/denied` shows the list; `GET /v1/members` does not.

**Locking yourself out.** An entry that names the frontend's own certificate (by serial or
key, or a `node` that is the name in it) makes every storage node refuse the frontend. The
way back is `DELETE /v1/denied`, which goes through this API and not through a certificate.

**Upgrading.** Upgrade every storage node and every frontend before relying on an entry. A
storage node that predates the list ignores it, and the checks at `Register` and `Heartbeat`
are the frontend's, so a denied storage node can still register and beat through a frontend
that predates them. A frontend that predates the list copies it forward in the map
unchanged.

## `GET /v1/leases` and `GET /v1/queue`

The leases the scheduler holds and the operations waiting for one: open reads, in memory,
unredacted. Written up in [api-scheduler.md](api-scheduler.md).

## `GET /v1/frontend/drain` and `POST /v1/frontend/drain`

Draining the frontend itself, for a rollout that restarts it (the nodes are drained
with `:drain` above). Buck2 does not recover an Execute stream that breaks after it
started, so a restart that cuts a stream fails its action: the frontend is drained, and
restarted when it has no stream left. The drain works behind any load balancer (Envoy,
HAProxy, a Kubernetes Service) with nothing of kbf in front: it is a sequence that the
load balancer's own health check follows, and `POST` and SIGTERM (a container's stop, a
pod's deletion) begin the same one.

**`GET /v1/frontend/drain`**: a read, open like `GET /v1/nodes`:

```json
{ "draining": false, "phase": "serving", "open_executions": 3, "queued_operations": 1 }
```

Once a configured `--drain-deadline` has passed with executions open, the answer also holds
`"deadline_passed": true` (the field is left out until then).

- `draining`: a drain has begun, by this endpoint or by SIGTERM or SIGINT: the phase is any
  but `serving`. It is not undone.
- `phase`: where the drain is. `serving`: none has begun. `lb_wait`: `/readyz` answers `503`
  and everything is still served. `draining`: new Execute calls are refused and every
  listener has been sent GOAWAY; the open executions run on, however long that takes.
  `closing`: the streams that are left are being closed, and the process exits within a few
  seconds.
- `open_executions`: Execute and WaitExecution operation streams open now
  (`kbf_front_open_executions`: a stream counts from the moment it is made, before its
  first operation is read, until it ends or its client leaves).
- `queued_operations`: operations in the scheduler's queue, with no lease yet. With
  `open_executions` it tells idle (both 0) from busy, and a stream that waits for a
  worker (`open_executions` above 0, `queued_operations` above 0, no lease running)
  from one that runs. Once a drain has begun, the streams of operations that were never
  started and that no worker can run are ended `UNAVAILABLE`, so `queued_operations` can
  stay above 0 with fewer streams open. `kbf_front_executes_started_total` (see the metrics below) tells a
  client that has just come from one that is still there.

**`POST /v1/frontend/drain`**: begins the drain, a write that passes every gate of
[Who may write](#who-may-write) and takes no body (or `{}`), and answers the same status
with `draining` true and the phase `lb_wait`. Calling it again changes nothing; SIGTERM or
SIGINT after it changes nothing either, and the other way round. The drain of a frontend
over a state store (`kbf frontend` requires one):

1. **Not ready, serving on.** `/readyz` turns to `503` (`stopping`) and the health service to
   `NOT_SERVING` before the request is answered, and everything is still served: unary
   calls (FindMissingBlobs, BatchUpdateBlobs, the action cache), ByteStream, the streams
   that are open and new Execute calls. Phase `lb_wait`.
2. **The load balancer's wait.** For `--drain-lb-wait` seconds (default 10, 0 allowed) the
   frontend does nothing more, so that the load balancer's health check sees the `503`: set
   it to at least its check interval times its unhealthy threshold, plus the time its routes
   take to change (for a Kubernetes pod, the readiness probe's period times its failure
   threshold, plus the endpoints' propagation). Unary calls carry no client retry, which is
   why they are served here.
3. **Refuse, stop taking work, GOAWAY.** New Execute calls are refused `UNAVAILABLE`, which
   Buck2 retries when it opens a call. The frontend stops taking queued work from the
   state store and stops placing work on its nodes (they read `cordoned` in `GET /v1/nodes`
   of this frontend; nothing is written to the store), so the other frontends take what it
   holds queued; what a node of this frontend is running goes on. The REAPI listener, the
   worker listener and the operator API are sent HTTP/2 GOAWAY, so a load balancer
   drains their connections: calls in flight on a connection finish and new ones open
   elsewhere. The REAPI and worker listeners stop accepting; this API goes on answering, so
   the drain can be read. Phase `draining`.
4. **Wait for the executions.** The wait lasts until `open_executions` is 0, however long
   that takes: there is no default deadline, and the drain never ends a stream because
   time has passed. `--drain-deadline` (seconds, at least 1, no upper bound, absent by
   default) only reports: when it passes with executions open, the frontend logs a WARN
   with the count, sets `deadline_passed` in `GET /v1/frontend/drain`, and goes on waiting,
   serving the open streams, with the farm not stood down and the process up, whether a
   signal or the endpoint began the drain. Buck2 retries only the opening of an Execute
   call, not a stream that breaks after it, so an ended stream fails that Buck2 action:
   the frontend ends none except one whose operation can never start. That is an operation that has never been started
   and that no live worker can run: no worker on this frontend, and no other frontend to
   take it (`WAITING`: there is no worker that could run it, or at least one that could is
   cordoned by an operator, or not serving, and no daemon connected to another frontend
   of the state store could run it; the stream says so in its metadata). Its stream ends
   `UNAVAILABLE` at the end of the load balancer wait, when step 3 begins, naming why. The
   action could never run, so this costs nothing and frees the stop. An
   operation that is only queued (a worker can take it), and one that waits only for the
   nodes this drain cordoned in step 3, and one that a daemon of another frontend could
   run (the other frontends take it), keep their streams,
   as does any operation that ever had a lease, even one queued again after a lost lease:
   they stay until the operation is done. Ending a stream does not cancel
   the operation: it stays queued for the other frontends. An execution running on a node
   of this frontend finishes here: the node's result is taken and the caller is answered.
   An execution another frontend took is answered from the result it records in the state
   store.
5. **Close and exit.** The frontend stands down as a retire does (below): the farm is
   fenced, and the worker streams that are left end `UNAVAILABLE`, so each daemon dials
   another frontend through the load balancer, which keeps its leases (the state store's
   owner rows are authoritative). No Execute or WaitExecution stream is open by now unless a
   second signal or `--shutdown-timeout-secs` forced the stop; the ones that are end
   `UNAVAILABLE` (an action that was still running fails, as Buck2 does not recover a
   stream). Phase `closing`, a few seconds long, and the process exits 0. For a worker
   listener behind a load balancer that passes TCP through, GOAWAY reaches the daemon's
   connection and nothing else: this is the step that closes it.

The drain is not undone: the process exits, and what starts in its place is a new
process. A frontend that has nothing open exits as soon as the wait is over. A second
SIGTERM or SIGINT skips the waits of steps 2 and 4 and goes on to step 5, and
`--shutdown-timeout-secs` (default 0, none), when a signal began the drain, does the
same when it runs out. These two are the operator's hard stops and the only ways a drain
ends open streams. A drain the endpoint began is bounded the same way once a stop signal
arrives. A server with no state store (`kbf-server`; a frontend started by a test) drains at once instead: `/readyz` `503`, new Execute calls refused, the REAPI
listener sent GOAWAY, the open streams kept until their action is done except the
streams of operations that can never start (there is no other frontend), which end
`UNAVAILABLE` when the drain begins (as in step 4),
and the server exits when the REAPI connections have closed, bounded by
`--shutdown-timeout-secs` (it has no `--drain-deadline`).

The stop timeout of what runs the frontend must exceed `--drain-lb-wait` plus the longest
execution it may hold, and is never the bound of the drain: under systemd that means
`TimeoutStopSec=infinity` in the unit, and the unit templates in `kbf-deploy` set it; for
a container, `stop_grace_period` (Docker) or `terminationGracePeriodSeconds`
(Kubernetes) is set to the sum plus five minutes, and the `preStop` hook of a pod needs no
sleep, since the wait is the frontend's own. See
[kbf-units.md](deploy/kbf-units.md#draining-a-frontend-behind-a-load-balancer).

`kbf cluster` (on the frontend's host) calls this API: `kbf cluster status [--frontend]`,
`kbf cluster drain <node> [--deadline-secs N] --token-file F`, `kbf cluster drain
--frontend --token-file F` and `kbf cluster undrain <node> --token-file F` (the API's
`:uncordon`). It prints the answer's JSON, exits 0 on a 2xx and 1 otherwise, and takes
`--api` for the listener (default `127.0.0.1:8982`). A frontend drain has no undrain:
restart it. `kbf cluster member <id> --state active|draining|planned-down [--for-secs N] --token-file F` calls
`POST /v1/members/{id}`, and `kbf cluster denied --token-file F` calls `GET /v1/denied`.
`kbf cluster object-store ...` calls the `/v1/object-stores/...` routes (below).
`kbf cluster epoch-bump --token-file F` calls `POST /v1/epoch/bump`.

## `GET /v1/frontend/retire` and `POST /v1/frontend/retire`

Taking one frontend out of service while other frontends share its state store, for a
rollout that restarts it. A drain (above) serves on while the load balancer sees it go,
waits for the open executions and then exits, the worker streams and the queue moving to the
others at its end (its last step is a retire's); a retire moves them at once and keeps the
process answering the callers it holds, which is what an updater uses.

**`GET /v1/frontend/retire`**: a read, open like `GET /v1/frontend/drain`:

```json
{ "retired": false, "open_streams": 3, "open_sessions": 2 }
```

- `retired`: the retire has completed. It is not undone.
- `open_streams`: Execute and WaitExecution operation streams open now, the callers this
  frontend still answers (`kbf_front_open_executions`).
- `open_sessions`: the worker sessions the frontend holds.

**`POST /v1/frontend/retire`**: begins the retire, a write that passes every gate of
[Who may write](#who-may-write) and takes no body (or `{}`). It answers the same status
once the retire has completed, with `retired` true. Calling it again changes nothing and
answers the same status. The retire:

- refuses every new Execute `UNAVAILABLE`, which Buck2 retries when it opens a call;
- waits, for at most ten seconds, for the calls between their request and their queueing,
  then stops taking work: the frontend reads no more of the state store's queue, grants nothing,
  claims nothing and writes the requeues the store had refused (for at most the same
  time again);
- stops the worker listener and ends every worker stream `UNAVAILABLE`, so each daemon
  dials another frontend, which takes up the leases the daemon lists;
- turns `/readyz` to `503` (`stopping`) and the health service to `NOT_SERVING`;
- keeps every open Execute and WaitExecution stream, and answers it from the result another
  frontend records in the state store, for as long as the process lives.

The REAPI listener is not sent GOAWAY and the process does not exit: the operator restarts
the unit once `open_streams` is 0. A server that is not a frontend over a state store
answers both routes `409` (`this server is not a frontend over a state store`).

## `POST /v1/epoch/bump`

Raises the epoch the state store holds by one, for the day the store was restored from a
backup and the daemons must drop the leases the lost history granted (the steps are in
[kbf-units.md](deploy/kbf-units.md#the-state-store-postgres-for-the-frontend)). A write that
passes every gate of [Who may write](#who-may-write) and takes no body (or `{}`). The epoch
is the one `Welcome` names: a daemon that sees an epoch other than the one it knew kills the
runs of its leases and lists none. A frontend that starts keeps the stored epoch, so a start
no longer changes it; this call is the one way it changes.

```json
{ "previous": 41, "epoch": 42, "serving": 41 }
```

- `previous`: the epoch the store held before the call.
- `epoch`: the epoch it holds now. The next frontend that starts serves it.
- `serving`: the epoch the frontend that answered serves, the one it started under. The
  call does not change it: restart the frontend for the new epoch to apply.

`404` from a server with no `--state-store`, which keeps no epoch; `503` when the store
cannot be reached or other writers kept changing the epoch for the whole of the call's tries
(it changed nothing; call again); `409` when the store refuses the write as fenced
(`Fenced`; no current store returns it, so a live store does not answer it; it changed
nothing).

## `/v1/enroll/tokens` and `/v1/enroll/nodes`

The operator's side of enrollment (`design/install-and-enrollment.md`):
the tokens a node enrolls with, and the registry of nodes that did. They live in the state
store, so a frontend that has no enrollment (one started without `--enroll-listen`)
answers every one of these routes with `404` once the call passes the gates.

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
| `POST /v1/enroll/nodes/{id}:adopt` | records a node provisioned before enrollment | the node, as `GET /v1/enroll/nodes` lists it |

**Create.** The body is `{"roles": ["daemon"], "host_role": null, "max_uses": 1,
"expires_in": 86400, "node_id": "n1", "node_prefix": "rack-", "names": [".example"],
"replace": false, "label": "rack 4"}`; only `roles` is required (`storage` and `daemon`, one to
two, no repeat; or `updater` alone, below). `max_uses` is 1 to 1000 (default 1); `expires_in` is seconds, at least 1 and at
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

**A host's updater** ([deploy/kbf-updater.md](deploy/kbf-updater.md#enrolling-a-host)) enrolls with
a token whose `roles` is `["updater"]` and nothing else, with `host_role` (`frontend`, `storage`,
`daemon` or `mac-daemon`: what its certificate says the host runs), the one `node_id` it is for
(no `node_prefix`) and one use; each of those is a `400` if wrong, and `host_role` is a `400` on a
token that does not grant `updater`. Its certificate has the role `updater`; the updater has a
registry row of its own, beside any node of the same id, and the node listing does not show it.

`info` and each element of `tokens` are `{id, roles, host_role, max_uses, uses, expires_at,
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

**Adopting a node.** A node provisioned by hand, with certificates the operator issued, was
never enrolled, so the registry does not know it: its storage-health answer is `unknown_node`
and the safe-to-stop check cannot join it to its storage member, both of which go through the
address the node advertised when it enrolled. `POST /v1/enroll/nodes/{id}:adopt` writes that
registry row without enrolling. The body is `{"roles": ["storage"], "advertise":
"host:7000"}`; `roles` is required, and `advertise` is required for a node with the `storage` role
and must be absent or empty for any other. The roles are `storage` and `daemon` (not `updater`), and `advertise`
follows enrollment's rules (a DNS name or IP address, read in lower case, and a port from 1 to
65535, not loopback, unspecified, link-local or multicast, and not a name this frontend
serves). An unknown field, a malformed body or id, or a value enrollment refuses is a
`400`; an id the registry holds (revoked ones included) or an address another node holds is
a `409`. The answer is the node as the listing shows it: `active`, with `token_id` and
`token_label` both `adopted`, `not_after` `0` (it holds no certificate) and no
`last_renewed_at`. The call issues no certificate and writes no serial or key. An adopted
node never renews (a renewal of it is refused as `unknown_node`); revoking it marks it
`revoked` and writes one deny row, for the node id (which refuses the node's existing operator-issued certificates, as for an enrolled node). A token with `replace` can enroll over
an adopted id, which then becomes an enrolled node.

**Failures.** `400` for a body or value the registry refuses, `404` (above), `409` when
the store refuses the write as fenced (`Fenced`, which no current store returns: nothing
was written; the enroll wire's `NotSigner`), `503` when the state store failed or
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

## `kbf.enroll.v1`: `Enroll` and `Renew` on `--enroll-listen`

The node's side of enrollment: a gRPC service (`kbf.enroll.v1.Enroll`, two methods) on a
listener of its own, **off unless `--enroll-listen=<addr>` is given**. It is served over
TLS that authenticates the server only (a node with no certificate yet cannot present
one). The server certificate is the frontend's own, issued by the farm CA for the
`--frontend-name` list and held in memory (never written), replaced at half its life; the
chain it sends is the leaf and then the root. A node learns the CA from its token, which
carries the SHA-256 of the root, and pins it.

**Flags** (all are the frontend's alone, and every one but `--enroll-listen` needs it):

| Flag | Meaning |
|---|---|
| `--enroll-listen=ADDR` | serve `Enroll` and `Renew` here; needs `--state-store`, `--ca-dir`, `--frontend-name` and a signer flag |
| `--ca-dir=DIR` | the CA directory (`ca.key` mode `0600`, `ca.pem`). Empty or missing: a new CA is made once. Holding a CA: it is loaded and **nothing is overwritten**. Holding anything else, or half a CA: refused, untouched |
| `--ca-signer=SOCKET` | **production.** The CA key lives in a `kbf-signer` process; this process holds none. `--ca-dir` must hold the `ca.pem` the signer made (readable by this account: see "The signer's socket" below), and the signer's public key must match it |
| `--ca-dev-file-signer` | development and tests only: the key is a file in this process. A start with neither signer flag fails, so production cannot end up holding the key by default |
| `--frontend-name=NAME` | a DNS name or address this frontend serves (repeat). The listener's certificate carries them; no node may advertise one, or a host under one |
| `--enroll-cert-lifetime-secs=N` | certificate life, 3600 to 2592000 (default 604800); nodes renew at half. The signer's own `--lifetime-secs` must be at least this |
| `--enroll-rate-per-minute=N`, `--enroll-rate-burst=N`, `--enroll-max-in-flight=N` | the limits below (defaults 60, 30, 64) |
| `--enroll-worker-endpoint`, `--enroll-cas-endpoint`, `--enroll-storage-port` | what every node is told (`NodeConfig`) |
| `--enroll-allow-unenforced-deny` | does nothing (see the start gate); still accepted so existing units start, and removed in a later release |

**The signer's socket.** `kbf-signer` runs as its own account and, alone, serves its Unix
socket to `--frontend-uid` (the kernel's `SO_PEERCRED` user id of each connection; any other
peer is closed on without a byte). By default the socket is mode `0600` in a directory of
mode `0700` and `--ca-dir` is `0700`, so a frontend running as another account (another
user, or a container) can neither connect nor read `ca.pem`. `--client-group=NAME` shares
both with a group:

| What | Without the flag | With `--client-group=NAME` |
|---|---|---|
| socket directory | `0700` | `0750`, group `NAME` |
| socket | `0600` | `0660`, group `NAME` |
| `--ca-dir` | `0700` | `0750`, group `NAME` |
| `ca.pem` | `0644` | `0640`, group `NAME` |
| `ca.key` | `0600` | `0600`, not given to the group |
| peers served | the user id of `--frontend-uid` | that user id, and any peer whose effective group is `NAME` or whose account has `NAME` as its primary group or is listed as one of its members (`/etc/passwd` and `/etc/group` only, not NSS, read at every connection) |

The signer must itself be a member of `NAME` (a process can only give a file to a group it
belongs to), and the frontend's account a member too; the package's sysusers file makes
group `kbf-signer-clients` with `kbf-signer` and `kbf-server` in it. The start is refused,
with nothing written, when `NAME` is not in `/etc/group`; an existing socket directory that
others can enter, or the group can write, is refused, as is a `--ca-dir` that others can
enter or the group can write, or whose `ca.key` is readable by group or others (it is never
repaired). A container reaches the socket by mounting the socket's directory and being run
with that group's id as a supplementary group.


**Order of an `Enroll`.** The shape (ids, role names, the size of each CSR) is checked
first and reads nothing. Then, in one registry transaction: the token **secret** (an
unknown id and a wrong secret are the same refusal after the same work, byte for byte);
**idempotency** (a request already served is answered from the stored node); the token's
**policy** (revoked, expired, used up, roles, node id, advertise names; the roles asked
must be a subset of the token's); and only then the CA: each CSR is parsed
(`kbf_ca::parse_csr`: ECDSA P-256, `ecdsa-with-SHA256`, at most 2 KiB, a signature that
verifies) and a certificate is issued for it. A CSR is therefore parsed only for a caller
who proved the secret. The answer carries one certificate per role, the CA bundle (PEM),
the frontend's clock (`server_time`), `renew_after` (half-way through the shortest life)
and the `NodeConfig`. A node granted the `storage` role is told the farm's cold store in
`NodeConfig.cold_store`: the store registered with `kbf cluster object-store` (the `cold`
binding), as `s3://bucket/prefix/?endpoint=...&region=...`, with `conditional-put` and
`min-put-rate` when they are not the defaults. The URI is built from the registered
endpoint, region, bucket, prefix and settings only: **never** the access key id, the secret
or any other query key (the key pair reaches a node through `ColdStore.GetStoreConfig`, over
its storage certificate). A farm with nothing registered sends an empty `cold_store`, and a
daemon-only node is sent none. A storage node that holds none asks once at its next start. A
registry that cannot be read refuses the call (`StoreUnavailable`) before the token is
spent. The node uses the `cold_store` it is sent with its own process credentials, so the
frontend decides which endpoint receives the node's signed requests: the same trust as the
worker and CAS endpoints the frontend already sends in `NodeConfig`. A storage node must send `advertise` (`host:port`) and no other node
may; the host may not be a frontend name, `localhost`, or a loopback, unspecified,
link-local or multicast address. Set `node.advertise` in `kbf.toml` on a storage node.

**`Renew`** needs no token: the node signs a CSR per role with the key the registry holds
and puts `issued_at` among the signed attributes. Checked: the node exists and is not
revoked; it asks for exactly the roles it holds, each with the **same key**; the CSR's
`issued_at` attribute **equals** `RenewRequest.issued_at` (the field is not signed, so a
captured CSR replayed under a new field is refused `MalformedRequest`); `issued_at` is
within five minutes of the frontend's clock; the same CSR was not seen in the last ten
minutes and the node did not renew in the last minute. Certificates are issued for the
same keys. The answer carries `config` only when the node's `config_generation` differs.

**When a node gets a new config.** `NodeConfig.generation` is a digest of the rest of the
config as that node receives it (roles, `worker_endpoint`, `cas_endpoint`, `storage_port`,
`cold_store`, and the other fields), with the low bit set so it is never 0. It changes
exactly when something the node is sent changes: a frontend restarted with another
`--enroll-worker-endpoint`, `--enroll-cas-endpoint` or `--enroll-storage-port`, or a cold
store registered, changed or taken back (a daemon-only node is not sent the store, so
that does not change its generation). A restart with the same flags changes nothing. The
number is **not ordered**: the node keeps it and `Renew` compares it for equality, so a
frontend put back to an earlier endpoint sends that configuration again. A node stores
the new config in `node.json` at the renewal that carries it, but the running process
does not re-dial: it uses the endpoint at its next start. Moving a farm's endpoints is
therefore "renew, then restart each node within the dwell", and no re-enrollment.

**Refusals** are exactly a `RefusalReason`: the gRPC status code is `reason.code()` and
the message is `reason.message()`, byte for byte, with no detail. The node client
recognises a refusal only by both. `NotSigner` is a store that refuses the write as fenced
(`Fenced`, which no current store returns), or a frontend whose CA cannot sign now (its signer is down, the clock is before the build's floor);
any other state-store failure is `StoreUnavailable`.

**Limits.** The request body is at most **16 KiB** (a longer message is `MalformedRequest`).
A token bucket per **TCP peer address** (an IPv6 address counts as its /64) allows the
burst, then the per-minute rate, and at most `--enroll-max-in-flight` calls run at once; the rest are `RateLimited`.
The table of sources is bounded (4096). Behind the bundled kbf-proxy (which routes
`kbf.enroll.v1` to the child's `--enroll-listen`) every peer is loopback, so these limits are
in effect one global bucket for the listener; the per-client-address limit is the proxy's, on
its accept path. **The counts are in memory: they start again at a restart**, so a new
process gives every source a full bucket. A call is
ended after 30 seconds and a TLS handshake after its own deadline.

**Revocation is enforced (design section 8.3).** Revoking a node refuses its renewal at
once and writes its deny rows (`POST /v1/enroll/nodes/{id}/revoke`, above). Storage reads them
through the membership map, and the **worker listener checks every call against the
membership's in-memory copy of them** (no store read per call), beside `--worker-deny-list`:
a revoked node's `Hello`, and its `Heartbeat`, `Result`, `NodeStatus`, `MapRequest` and
`UpdateStatus` on an open stream, its REAPI worker calls and its blob calls are refused
`PERMISSION_DENIED`, and its open stream ends at its next message (a heartbeat at the latest).
The revoke refreshes that copy before it answers, so the worker listener and storage follow at
once, and the membership sweeper refreshes it every beat (a third of `ttl`), so a row written
by another frontend is seen within one sweep.

**When the rows cannot be read.** A refresh that fails (the store down for a moment) leaves the
last copy in use for three sweeps (`DENY_SNAPSHOT_SWEEPS` beats): callers it denied stay denied
and the others are still admitted. After that, and before the first refresh after a start, every
worker-listener check is refused `UNAVAILABLE`: nobody is admitted on a list that may be out of
date. A corrupt `deny/` row (one whose key is not `deny/<serial|spki-sha256|node>/<value>`)
makes every refresh fail, so three sweeps after it appears the worker listener refuses everyone,
and the log says `membership refresh` failed. Repair: read the key from the log or list the
`deny/` rows in the state store, delete the bad row (or re-add the entry with
`POST /v1/denied`, after deleting it), and the next sweep readmits everyone not denied. If the refresh that follows a revoke fails, the failure is logged and the revoke still
answers; the worker listener then enforces it from the next successful sweep, within one beat.
A deny written by another frontend is seen within one sweep. A frontend refreshes once before
it serves, so a restart does not refuse daemons until the first sweep; if that refresh fails
it logs an error and starts, and the rule above applies.
A revoked node no longer keeps working until its certificate expires. Taking its rows off the
deny list (`DELETE /v1/denied?entry=...`, one call per entry the revoke listed) readmits the
node, but a revoked node's id is burned for enrollment. Behind `--proxy-auth-file` only the
`node` row can name a caller (the proxy forwards no serial or key).

`--enroll-listen` therefore needs no acceptance, and `--enroll-allow-unenforced-deny` does
nothing: a unit that still passes it starts as before.

### Operator steps

1. Start the frontend with the listener (development shown; production replaces the signer
   flag with `--ca-signer=/run/kbf-signer/signer.sock`):

   ```sh
   kbf --mode=frontend ... --state-store=<uri> --api-token-file /etc/kbf/api.token \
     --enroll-listen=0.0.0.0:7444 --frontend-name=kbf.example.com \
     --ca-dir=/var/lib/kbf/ca --ca-dev-file-signer
   ```

2. Make a token, on the frontend's host (the answer holds the token string once):

   ```sh
   T=$(cat /etc/kbf/api.token); API=http://127.0.0.1:<api port>
   curl -H "Authorization: Bearer $T" -H 'Content-Type: application/json' \
     -d '{"roles": ["storage","daemon"], "max_uses": 1, "expires_in": 3600, "node_id": "node-07", "label": "rack 4"}' \
     $API/v1/enroll/tokens
   # {"token": "kbft1.<id>.<secret>.<pin>", ...}
   ```

3. On the node, place `/etc/kbf/kbf.toml` and the token file (mode `0600`, owned by the
   service user, one line), then start it:

   ```toml
   [frontend]
   url = "https://kbf.example.com:7444"
   [node]
   id = "node-07"
   advertise = "node-07.nodes.example.com:8983"   # storage nodes only
   [enroll]
   token_file = "/var/lib/kbf/enroll.token"
   ```

   The node enrolls, installs its keys and certificates, removes the token file, and
   renews at half life. The token limits the roles; the mode the node runs is the
   operator's choice.

4. Revoke a node or a token with the routes above. A revoked node is refused `NodeRevoked`
   on its next renewal, and from the moment the revoke answers its worker streams end and its
   calls on the worker listener are refused (see above).

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
  "reasons": ["3 blobs owed"],
  "nodes": [ { "id": 1, "state": "active", "planned_down": false, "repair_debts": 3, "open_writes": 0, "generation": 12,
               "unflushed_bytes": 1048576, "unflushed_blocks": 2, "oldest_unflushed_ms": 4200,
               "budget_bytes": 107374182400, "used_bytes": 2097152, "flush_failures": 0, "flush_conflicts": 0 },
             { "id": 2, "state": "active", "planned_down": false, "repair_debts": 0, "open_writes": null, "generation": 12,
               "unflushed_bytes": null, "unflushed_blocks": null, "oldest_unflushed_ms": null,
               "budget_bytes": null, "used_bytes": null, "flush_failures": null, "flush_conflicts": null } ] }
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
- `nodes`: every registered member in any state (`planned_down` is true for a `down`
  member an operator planned, [above](#post-v1membersid)), with what its newest report says:
  `repair_debts` (the copies it owes), `open_writes` (the `Write` calls, action-cache
  commits and daemon segment uploads it has open now, an upload from `Begin` until its
  commit ends, as a primary or a replica) and `generation` (the map it held when
  it counted). A value is `null` when the member's report is older than the freshness
  window, when it has not reported, or when it cannot say (an older node does not count its
  writes). `null` is unknown, never zero: whoever reads it fails closed. A node is idle for a
  rollout only when every other member is `ACTIVE` and fresh, no copy is owed anywhere, and
  the node itself reports no open write from a map at least as new as the one that stopped
  routing writes to it (`Members::node_idle`, in-process, with the reasons).
- The write-tier health of each node, from its heartbeat (read-only; no decision depends on
  it): `unflushed_bytes` and `unflushed_blocks` (acknowledged data on the node's local disk
  not yet in the cold store: what losing the disk would take), `oldest_unflushed_ms` (the
  age of the oldest such block, 0 with none), `budget_bytes` and `used_bytes` (the tier's
  budget and the bytes counted against it), and `flush_failures` and `flush_conflicts`
  (counts since the node started). Each is `null` when the report is stale, when the node
  has not heartbeated since it registered, when it has no write tier, or when it is an older
  build that does not send it; `null` is unknown, never zero, and a real `0` is shown as
  `0`. A value above 2^53 - 1 is not a count: it is shown as `null`.

The number is as old as the newest heartbeat. A copy owed since then is not in it, so
whoever acts on a zero keeps new writes away from the node first (drains it), then reads
again. A member that left the map (a `DOWN` member leaves after the dwell) is not counted.
`503` with `{"error": ...}` when the membership cannot be read.

## The admin calls (removed)

A `kbf --mode frontend` no longer serves `GET /v1/admin/role` and
`POST /v1/admin/{quiesce,resume,takeover,stand-down}`, and has no candidate that waits for
a takeover: frontends that share a Postgres state store each take work from it, and a
frontend leaves service by [the retire](#get-v1frontendretire-and-post-v1frontendretire) or a stop. The flags that
turned the admin port on (`--admin-listen`, `--await-takeover`) and the flags of the
handover between two frontends of one host (`--handover-socket`, `--instance`,
`--handover-quiesce-secs`, `--handover-follow-interval-ms`, `--handover-watchdog-secs`,
`--handover-peer-name`, `--envoy-check-interval-ms`, `--envoy-check-timeout-ms`,
`--envoy-unhealthy-threshold`), and their keys in the `[handover]` table of `kbf.toml`, are
still accepted so that a unit or file written before the removal starts: they are hidden
from `--help`, do nothing, and each one given logs a warning naming it. They are removed in
the release after. `admin_protocol` in `kbf --version --json` (`docs/artifacts.md`) stays 1
for a proxy that still reads it.

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
0600 per connection, `<name>.<version>.json`, in a directory made 0700; every frontend
that serves at once must be able to read it, and it must stay out of any backup of the host
(the state store, and so `state-backup`, never holds a secret key). Without the flag every
route answers `404` `not_enabled` to a caller who passed the gate.

**Every route, a GET included, passes the whole gate of [Who may write](#who-may-write)**
(loopback peer, no `Origin`, the bearer token, `Content-Type: application/json`), because
the routes list and change the farm's store and the key to it. One token, no roles. A TLS
proxy on the host reaches them from loopback and passes the gate: whether the proxy carries
these routes is the operator's decision. Every answer, errors and `405` included, carries
`Cache-Control: no-store`. A body is at most 16 KiB (`413 payload_too_large`) and refuses
unknown fields. No route is fenced: two frontends on one state store both serve them,
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
  The reports are the storage nodes' heartbeats: each accepted heartbeat writes a row in the
  state store with a time to live of 15 seconds, so every frontend reads the same reports, and a report
  that is not renewed lapses and reads as `pending`. A farm restarted with an empty store shows
  `pending` for every node until heartbeats arrive.

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
| `stopping` | the server has received SIGTERM or SIGINT, or `POST /v1/frontend/drain` has begun a drain, or `POST /v1/frontend/retire` has retired the frontend. It answers `503` from that moment, and for the rest of the drain; open Execute streams are kept until their action is done, and only the shutdown timeout (`--shutdown-timeout-secs`) or a second signal ends them (`--drain-deadline`, on a frontend over a state store, only reports); the one exception is a stream whose operation was never started, that no worker on this frontend can run and that no other frontend can take, which ends `UNAVAILABLE` when the drain begins |
| `leader` | the server does not hold the scheduler role. A single server runs every role and always holds it; the check is there for a replicated control log, whose followers fail it. **Planned**: a second readiness path, without this check, for the servers that may serve reads ([deployment-topology.md](design/deployment-topology.md#build-clients-one-name-routed-by-method)) |
| `store` | the object store does not answer a one-byte read of the key `<prefix>readyz-probe` within `--readyz-store-timeout-ms` (default 2000; it bounds the gRPC health service's probe too), or answers with an error. The probe only reads: no poll writes, deletes or lists. Nothing writes that key, so "not found" is the expected answer and passes, as does the object's bytes or a range past its end |

`version` and `commit` are those of `GET /v1/nodes`' `server` field. Each `/readyz`
makes one read of the store, so poll it at the interval the front needs, not faster.

**`GET /readyz/reapi`**: the readiness a load balancer asks for the REAPI cluster. It is
`/readyz` with one more check, and has the same body, status codes and (no) token. A
frontend that answers `/readyz` with `200` but has no worker connected answers `503`:

```json
{ "ready": false, "version": "0.1.0+0123456789ab", "commit": "0123456789ab",
  "failing": [ { "check": "no_worker", "reason": "no worker is connected to this server" } ] }
```

| Check | Fails when |
|---|---|
| `no_worker` | no worker has an open stream to this server, as `connected` in `GET /v1/nodes` (a node of any platform and any placement counts; a node whose stream has closed does not). Listed after the checks of `/readyz`, which this route also runs |

Why it is a route of its own: an operation that none of a frontend's own workers can run
waits on that frontend, and another frontend takes it only once this one stands down. After
a frontend restarts, its workers may all be on the other frontend, and it would take Execute
calls it cannot place for as long as it is up. `/readyz` itself cannot fail for this, because
workers reach a frontend through the worker listener, which a load balancer routes by
`/readyz` too: a frontend with no worker would never get its first. So the REAPI cluster asks
`/readyz/reapi`, and the worker and enrollment clusters ask `/readyz`. The route does not
look at platforms: a frontend whose workers are all Linux is in the REAPI rotation, and an
action that needs another platform still waits there.

## `GET /v1/health`

The one-screen answer to "is the farm healthy": the values other routes report, composed
so a reader does not add them up. A read, open like `GET /v1/nodes`; it names no secret.
It answers `200` whether or not the server is ready (`/readyz` stays the probe), and
says which in `ready`.

```json
{ "ready": true, "failing": [],
  "nodes": { "total": 5, "serving": 2, "cordoned": 1, "draining": 1, "drained": 0,
             "quarantined": 0, "disconnected": 1 },
  "queue": { "depth": 3, "oldest_age_ms": 41200 },
  "leases": 7,
  "storage": { "members_active": 3, "members_total": 3, "under_replicated_blobs": 0 },
  "started_unix_ms": 1792584000000, "uptime_ms": 86400000 }
```

| Field | Same value as |
|---|---|
| `ready`, `failing` | `GET /readyz`: the same evaluation, `failing` as `[{"check", "reason"}]` |
| `nodes` | the list of `GET /v1/nodes`, each node counted once, in the first of these that holds: `disconnected` (`connected` is false), `quarantined` (`phase.state`), then `placement.state`: `cordoned`, `draining` (a drain running, or paused at its deadline), `drained`, or `serving`. `total` is the number of nodes listed and the sum of the six |
| `queue.depth`, `queue.oldest_age_ms` | `depth` and `oldest_age_ms` of `GET /v1/queue`, both read from one scheduler snapshot (`depth` is also `queued_operations` of `GET /v1/frontend/drain`). `oldest_age_ms` is how long the longest-waiting operation has waited, in milliseconds, and `null` only when nothing waits |
| `leases` | the sum of `lease_count` of `GET /v1/nodes` |
| `storage` | `members_active`, `members_total` and `under_replicated_blobs` of `GET /v1/storage` (`null` while unknown, never zero); `storage` is `null` when this server keeps no storage members, and when the membership cannot be read, with `storage_unavailable` saying why |
| `started_unix_ms`, `uptime_ms` | when the server started, as milliseconds since the Unix epoch on its wall clock, and the milliseconds since then by its monotonic clock (so a clock step does not change `uptime_ms`). A retire and restart of the frontend starts a new process, and with it a new `started_unix_ms`: it is when the in-memory state below began to fill |

In memory like `GET /v1/nodes`: after a restart or a retire and restart of the frontend `nodes`, `queue` and
`leases` count only what has reconnected since. The update state and the history store's
counters are not in this answer.

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
| `kbf_execute_requests_total` | counter | Execute calls ended |
| `kbf_execute_errors_total{cause}` | counter | Execute calls that ended in an error status or were dropped before the done operation (an operation that finished with a failed `ExecuteResponse` is not one), by `cause`: `status` (a gRPC error status), `client_abort` (the client reset the stream and its connection stayed), `dropped` (the stream was dropped and the client did not say so: a lost connection, or no connection to ask). All three are always exposed; the harness gate `execute_errors` reads their sum |
| `kbf_execute_failed_actions_total{class}` | counter | operations this frontend finished as failed, by the failure class of `docs/design/failure-classes.md` (section 1), counted once when the farm settles the operation (not per caller, not per rerun), by what its callers are told: `action` (the action exited non-zero, timed out, or was killed at the largest node's memory), `request` (the action cannot run as written: INVALID_ARGUMENT), `farm` (the farm could not run it: a farm fault past its reruns, an input blob lost since the front checked it, an operation no live worker could run), `ambiguous` (never a final answer, so it stays 0 until a path ends an operation with it). All four series are written from the start of a farm, so an absent series means the metric is gone. Only a frontend with a farm writes it |
| `kbf_execute_in_flight` | gauge | Execute and WaitExecution streams open |
| `kbf_under_replicated_blobs` | gauge | the sum over all registered storage members of the blobs each owes a second copy, exported **only while it is known** (every registered member `ACTIVE` and reporting within the window); absent otherwise, never 0. Frontends with a state store. [`GET /v1/storage`](#get-v1storage) says why it is unknown |
| `kbf_member_repair_debts{member}`, `kbf_member_open_writes{member}` | gauge | per storage member, the copies it owes and the writes it has open now, one sample per member, **only for a value that is known** (fresh report, and the node says it counts); a member with no sample is unknown, never 0. Frontends with a state store. [`GET /v1/storage`](#get-v1storage) lists the same per node |
| `kbf_storage_health_known` | gauge | 1 when `kbf_under_replicated_blobs` is exported, 0 when it is unknown (including a membership that cannot be read). Frontends with a state store |
| `kbf_member_state{member,state}` | gauge | per storage member, one sample for each of `joining`, `catching_up`, `active`, `draining`, `down` and `unknown` (a state this build does not name): 1 for the state the member is in, 0 for the others. Absent while the membership cannot be read. Frontends with a state store |
| `kbf_members_not_active` | gauge | registered storage members that are not `ACTIVE`; exported only while the membership can be read, absent is unknown and never 0. The B1 and B4 gate allows at most 1. Frontends with a state store |
| `kbf_member_boot{member}` | gauge | per storage member, the boot counter of its current registration: larger at every restart, unchanged by heartbeats. The restart gate reads its growth over the scrapes. Frontends with a state store |
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
| `kbf_execute_walk_dir_read_seconds` | histogram | one Directory read from the store by an Execute's input-tree check over a store (the farm's frontends): the store's latency per read, failed reads included; Directories taken from the content cache and reads shared with another check are not observed. |
| `kbf_execute_dir_cache_hits_total`, `kbf_execute_dir_cache_misses_total`, `kbf_execute_dir_cache_merged_total` | counters | the content cache of decoded Directories behind those checks (128 MiB, least recently used out first, shared by every Execute; content is addressed by digest so it never goes stale). Hits: Directories a check took without reading, from the cache or from a read another check had open. Misses: reads a check led. Merged: of the hits, those another check's read supplied. A cached Directory is never taken for held: its digest goes to FindMissing with the check's files (`kbf_execute_dir_cache_present_checked_total` counts them), and one the store reports missing is an absent input, as for a Directory that cannot be read |
| `kbf_execute_dir_cache_present_checked_total` | counter | cached Directories whose digests a check put to FindMissing to confirm the store still holds them (each costs a digest in a call the files already make, not a read) |
| `kbf_execute_dir_cache_evictions_total`, `kbf_execute_dir_cache_bytes` | counter, gauge | decoded Directories pushed out of the content cache to stay within its byte bound, and the bytes it held at the end of the last check (about 1 MB for a 1000-Directory, 15k-file tree). Evictions climbing with the same trees walked again means the bound is too small for the working set |
| `kbf_scheduler_core{core}` | gauge | the scheduler core the server runs (`--scheduler-core`): one series per core, 1 for the one running and 0 for the other. Both cores are in every binary; `legacy` is the default |
| `kbf_usage_profiles_mode{mode}` | gauge | the `--usage-profiles` mode the server runs: one series per mode (`off`, `record`, `on`), 1 for the one running and 0 for the others. `off` is the default. In this build the mode changes nothing else the farm does |
| `kbf_execute_submit_seconds`, `kbf_sched_lock_wait_seconds` | histogram | the scheduler's submit called from an Execute, and the part of it spent waiting for the scheduler's lock |
| `kbf_sched_queued`, `kbf_sched_leases` | gauge | operations waiting for a lease, and leases granted or running on all nodes, as of the last submit or scheduler tick (at most a tick old) |
| `kbf_sched_whole_held`, `kbf_sched_whole_marks`, `kbf_sched_whole_draining`, `kbf_sched_whole_ceiling` | gauge | workers held by a whole-machine lease, held by a queued whole-machine request while they drain, marked workers that still have leases, and the most workers that may be whole now. Printed only with `--whole-node-max`, as of the last submit or scheduler tick |
| `kbf_sched_whole_waiting{reason}` | gauge | queued whole-machine requests by why each waits (`early`, `holding`, `barred`, `cap_ceiling`, `cap_keep_for_ordinary`, `cap_max_draining`, `cap_per_consumer`, `last_capable`, `queued`, `unservable`), every reason always printed, 0 when none. Printed only with `--whole-node-max` |
| `kbf_prediction_made_total` | counter | new operations the estimator gave a prediction (`--placement-predictions shadow` or `on`): the learned cores, memory and life of the operation's key |
| `kbf_prediction_unset_total{reason}` | counter | new operations that got none, by why: the estimator's fallbacks (`missing`, `stale`, `cold`, `corrupt`, `over_cap`) and what the farm does not ask it about (`kind`, `no_os`, `no_subject`); such an operation is charged as it is with `--placement-predictions off` |
| `kbf_prediction_vs_run_total{axis,bucket}` | counter | with `shadow`: finished runs compared with the prediction made for their operation, once per operation, for a run that succeeded and ran a second or more. `axis` is `cpu` (the cores the run used on average), `mem` (its peak memory) or `life` (its life in whole minutes); `bucket` is `under` (the run used more than predicted), `fit` (at most the prediction and at least half of it) or `over` (less than half of it) |
| `kbf_placement_cpu_limited_nodes` | gauge | nodes whose latest load reading (not stale) says the actions cgroup has a CPU quota (`actions_cpu_limited`) while `--placement-load` is on: placement charges them by prediction alone, which is a misconfiguration; the server also warns when a node starts to report one (0 with `--placement-load off`) |
| `kbf_bytes_in_flight`, `kbf_bytes_in_flight_peak` | gauge, gauge | bytes this process holds in memory for requests in flight now (a ByteStream Write's receive buffer or the message in hand, a Read's window, a BatchUpdateBlobs's blobs, a Segments upload's buffer, and the same on a storage node's Write), and the most it has held at once since the process started (never reset). Counted from the allocation of a buffer until the last reference to it is dropped, however the request ended. Not counted: the segment writer's own buffer, the object store's and write tier's internals, and gRPC's copies of a message. Always printed. The daemon does not serve `/metrics` yet |
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

**Packs.** On a frontend given `--pack-store-endpoint` ([kbf-modes.md](deploy/kbf-modes.md)) the
blobs clients upload are kept in packs, and these counters follow; without the flag they are absent.

| Family | Type | What it measures |
|---|---|---|
| `kbf_pack_writes_total`, `kbf_pack_written_bytes_total`, `kbf_pack_written_blobs_total` | counter | packs stored and recorded in the pack index, their bytes (footers included), and the blobs in them |
| `kbf_pack_skipped_blobs_total` | counter | blobs of a write the pack index had already, so not packed again |
| `kbf_pack_write_failures_total` | counter | writes that failed because the store or the pack index could not take a pack (UNAVAILABLE to the client): the blobs not yet recorded are missing and the client sends them again, and packs finished before the failure stay recorded. A blob that is not its digest is INVALID_ARGUMENT and is not counted |
| `kbf_pack_find_missing_asked_total`, `kbf_pack_find_missing_found_total` | counter | digests put to the pack index by FindMissingBlobs, by Execute's check of an action's inputs and by the check of an action result's blobs (written, or hit), and the ones it had (the rest go on to the storage nodes) |
| `kbf_pack_reads_total`, `kbf_pack_read_bytes_total` | counter | blobs served from a pack (a client's read or Execute's own), and their bytes |
| `kbf_pack_read_fallbacks_total` | counter | reads of a blob the pack index lacks, answered by the storage nodes: a blob uploaded before packs were on. Falling to 0 over time is how a farm sees that nothing is left on the nodes to find |
| `kbf_pack_read_lost_total` | counter | reads of a blob the index has and whose pack the store lacks, answered by the storage nodes (alert on any) |
| `kbf_pack_read_errors_total` | counter | reads that failed because the pack index or the pack store could not answer |
| `kbf_pack_digest_mismatches_total` | counter | reads of a blob whose pack held other bytes than its digest: never served, UNAVAILABLE (alert on any) |
| `kbf_pack_inflight_bytes`, `kbf_pack_inflight_peak_bytes` | gauge | bytes the uploads into packs have taken of `--pack-inflight-bytes` now, and the most at once since the start (never above the flag) |
| `kbf_pack_upload_waits_total` | counter | uploads that found `--pack-inflight-bytes` spent and waited their turn: a burst slowed down, none failed. Rising steadily means the bound is below what clients send |
| `kbf_pack_upload_refusals_total` | counter | uploads refused RESOURCE_EXHAUSTED because their blobs alone are over `--pack-inflight-bytes` |
| `kbf_pack_upload_idle_timeouts_total` | counter | ByteStream uploads ended DEADLINE_EXCEEDED because, after their resource name had arrived, no byte of their body arrived for `--pack-upload-idle-timeout`; nothing was stored and their bytes were given back |
| `kbf_pack_commits_total` | counter | packs recorded in the pack index, whether a daemon stored one (`kbf.worker.v1.PackIndex/CommitPack`) or a build wrote it through a presigned write (also counted in `kbf_pack_untrusted_commits_total`) (a retry of a commit already recorded is not counted again) |
| `kbf_pack_commit_rejects_total` | counter | commits of a pack refused with nothing recorded, from a daemon or, for a build's pack, any refusal counted in `kbf_pack_untrusted_commit_rejects_total`. For a daemon: the writer was not the caller, the name or the list of blobs was not one a daemon may commit, the pack was not in the store, a sampled blob was not its digest (`--pack-commit-sample`; alert on any), or the pack was recorded before with other contents. A store or an index that failed is UNAVAILABLE to the daemon and is not counted |
| `kbf_pack_untrusted_commits_total` | counter | packs a build wrote through a presigned write that the frontend read, hashed to the last byte and recorded in the pack index (a replay of a commit already recorded is not counted again) |
| `kbf_pack_untrusted_commit_rejects_total` | counter | commits of such a pack refused with nothing recorded, labelled `reason`: `forbidden` (the key was not the one issued), `shape` (the manifest cannot be a pack's: a cap, an order, a gap, an overlap or a record of no bytes), `footer` (the footer is not the manifest), `mismatch` (a record is not its digest or CRC), `missing` (the pack is not in the store as long as said) or `conflict` (the key was recorded with other contents). A store or an index that failed is not counted. Each refusal also moves `kbf_pack_commit_rejects_total` |
| `kbf_pack_ingests_total` | counter | packs a task wrote through a presigned write that `ingest` turned into an action result: the action ran (any exit code), whether or not the result was written to the action cache |
| `kbf_pack_ingest_rejects_total` | counter | packs `ingest` refused, labelled `reason`: `missing` (no object under the key of pack 0), `wrong_key` (a key was not minted for this lease and action or is not in the place its slot names, the manifest is for another lease or action, or a key was recorded with another pack), `too_large` (the pack, its record count, a record or the manifest record is over its cap), `malformed` (a damaged footer or trailer, records out of order, overlapping, with a gap, one of no bytes, or a digest in two places), `hash_mismatch` (a record is not its digest, or a large object is not the blob its digest names: another length, or bytes that do not hash to it), `bad_manifest` (the record at offset 0 is not a valid manifest, it names more packs or other large slots than the keys issued, or an output tree is not a `Tree`) or `incomplete` (a pack beyond the first or a large object is not in the store, or a digest the manifest or a tree names is in no pack and no large object) |
| `kbf_pack_ingest_unavailable_total` | counter | ingests the object store, the pack index or the action cache could not answer; the pack is not judged and the lease is run again |
| `kbf_pack_ingest_packs_total` | counter | packs of the runs `ingest` turned into an action result (a run of one pack counts 1): divided by `kbf_pack_ingests_total`, the packs per run |
| `kbf_pack_ingest_large_bytes_total` | counter | bytes of large objects (raw objects over the record cap that a task wrote under `L<i>` keys) that `ingest` streamed into the CAS, for the runs it turned into an action result |
| `kbf_pack_ingest_large_writethrough_total` | counter | large objects that `ingest` also put to `cas/l/<sha256 hex>` in the pack object store, labelled `result`: `ok`; `exists` (the key was already held at the digest's length, so nothing was uploaded, or the store refused a second write of it: the key names the bytes); `failed` (the store failed the write, or holds an object of another length under the key, which is not trusted or replaced); `aborted` (the ingest gave the write up before its end: the CAS write stopped, the bytes were not the blob, a retry started the pass over, or the store did not take a window in 30 s; nothing is kept under the key). Best effort: no result fails the ingest |
| `kbf_executor_leases_held` | gauge | host leases the executor holds now (only with `--executor js-agent`; the `kbf_executor_*` families are absent otherwise) |
| `kbf_executor_runs_total` | counter | runs the executor ended, labelled `outcome` by what the farm was told: `ran` (the frontend rebuilt the action's result), `infra` (an ending the farm runs again), `timeout` (the action's own), `out_of_memory`, `lost` (the host or the lease server) or `invalid` (an action that cannot run here) |
| `kbf_executor_ingests_total` | counter | what the ingest of a task's pack came to, labelled `result`: `ok`, `unavailable`, `stopped` (the ingest panicked or was cancelled) or `rejected_<reason>` for each reason of `kbf_pack_ingest_rejects_total` |
| `kbf_executor_late_starts_total` | counter | `Start`s dropped because their window ended before the run was posted; the farm runs the action again |
| `kbf_executor_stray_leases_released_total` | counter | leases released because none of the executor's slots held them (a lost acquire answer, an undelivered release, a lease beyond the pool's size at a restart) |
| `kbf_executor_phase_seconds` | histogram | how long the phases of an executor run took, labelled `phase`: `prepare`, `start`, `queue`, `task`, `run` and `ingest`; stamped by the executor once per pass, so `queue`, `task` and `run` are good to a pass plus the agent's heartbeat; an unobserved phase is not counted. `docs/deploy/kbf-modes.md` says how to read them |

**What ingest of a run holds and refuses.** A digest is in one place: a record of one pack or one
large object. A digest that is a record twice, in two packs or twice in one, is `malformed` (a task builds each pack from distinct blobs, so an honest pack has none; the check now runs across packs and before anything is stored). Large objects are streamed into the CAS before the
packs are recorded and the result is cached, so a run that fails after that point leaves them
there: valid blobs, each under the digest its own bytes hash to, that no result names. A hostile
task can leave up to 8 large objects of the large cap (512 MiB) each, 4 GiB, of such bytes per run,
until a janitor removes them (komira-ai/kbf#1980).

**Legacy reads, on a storage node's `--metrics-listen`.** `kbf_storage_legacy_read_total{kind,source}`
(counter) counts the reads a storage node (`kbf storage`, `kbf storage-and-daemon`) serves from its
own copy of the data. Its five rows are always printed, 0 included: `kind="cas",source="bytestream"`
(a ByteStream `Read` that found its blob, one per call), `kind="cas",source="segments"` (a `Segments`
`ReadRange` that returned bytes, one per call), `kind="ac",source="action_store"` (an `ActionStore`
`Get` that found an entry), and `kind="cas",source="in_process"` and `kind="ac",source="in_process"`
(the same blob and entry reads made by the daemon half of a `storage-and-daemon` process, which reaches
its own node without a connection). A read that finds nothing, a read of the empty blob,
`FindMissingBlobs`, `QueryWriteStatus`, `Segments` `Locate`, every write, and the node's own reads for
flushing and repair are not counted; a read a frontend or a daemon serves from a pack never reaches a
node. The sum over all rows staying at 0 is how an operator sees that nothing reads the storage nodes
any more.

**Job-facing read.** On a frontend given `--job-listen` ([kbf-modes.md](deploy/kbf-modes.md)) the
listener a task fetches its action's inputs from counts requests and bytes; without the flag these
are absent. The same scrape has a `listener="job"` series in the `kbf_tls_reloads_total` family, except
under `--job-plain-http`, where the listener has no TLS files.

| Family | Type | What it measures |
|---|---|---|
| `kbf_job_read_requests_total` | counter | requests answered, labelled `outcome`: `ok` (200), `unauthorized` (401: no grant, a malformed, forged, expired or revoked one), `forbidden` (403: a digest outside the grant's scope, or an Action with no readable inputs), `not_found` (404: a blob that is not held, or a path that is not `/<hex>/<size>`), `too_large` (413: a blob over 64 MiB, not read), `method_not_allowed` (405) and `unavailable` (503: the store could not answer) |
| `kbf_job_read_bytes_total` | counter | blob bytes served to jobs |

**Pack reads by daemons.** A daemon started with `--pack-read` ([kbf-modes.md](deploy/kbf-modes.md))
puts the counts of its pack reads in every heartbeat, and the frontend its worker stream is on
keeps the newest of each node and exports them, one sample per node, labelled `node`. They
are the counts of the daemon's process: they start again from 0 when it restarts, as any counter
may. A node that sent none (a daemon without `--pack-read`, or one built before the field) has
no sample, and with no node sending any the families are absent. A node whose stream has closed
keeps the counts of its last heartbeat until the frontend restarts (a node's other facts are
kept the same way); a node whose next heartbeat carries none, such as a daemon restarted without
`--pack-read`, loses its samples. Each read of a blob counts in exactly one of served, elsewhere,
unasked, lost and damaged.

| Family | Type | What it measures |
|---|---|---|
| `kbf_pack_daemon_served_total{node}`, `kbf_pack_daemon_served_bytes_total{node}` | counter | blobs the node's daemon read from a pack, and their bytes |
| `kbf_pack_daemon_elsewhere_total{node}` | counter | reads of a blob no pack holds (or that is not asked of the packs, or whose frontend serves no locations): answered by the storage nodes, as before packs |
| `kbf_pack_daemon_unasked_total{node}` | counter | reads answered by the storage nodes because the packs could not be asked (the pack index or the pack store failed): the daemon is reading around the packs |
| `kbf_pack_daemon_lost_total{node}` | counter | reads that failed because the pack the index names is gone or too short (UNAVAILABLE to the lease; alert on any) |
| `kbf_pack_daemon_damaged_total{node}` | counter | reads that failed because the bytes in the pack are not the digest (never served; alert on any) |

**Pack writes by daemons.** A daemon started with `--pack-write` (which needs `--pack-read`;
[kbf-modes.md](deploy/kbf-modes.md)) also puts the counts of its pack writes in every heartbeat,
and the frontend keeps and exports them as it does the reads: one sample per node, labelled
`node`, the counts of the daemon's process (they start again from 0 when it restarts). A node that
sent none (a daemon without `--pack-write`, or one built before the field) has no sample of these
families, whether or not it sent read counts, and a node whose next heartbeat carries none loses
them. The unit is the blob: each blob of the outputs a daemon puts is in a committed pack or was
written to the storage nodes instead, and counts in exactly one fallback reason then.

| Family | Type | What it measures |
|---|---|---|
| `kbf_pack_daemon_packs_written_total{node}`, `kbf_pack_daemon_blobs_packed_total{node}`, `kbf_pack_daemon_packed_bytes_total{node}` | counter | packs the node's daemon stored and had the frontend commit, the blobs in them, and their bytes (footers included) |
| `kbf_pack_daemon_write_fallbacks_total{node,reason}` | counter | blobs written to the storage nodes instead of into a pack, by `reason`: `off` (the frontend does not take pack commits, or answered UNIMPLEMENTED earlier), `unfit` (over the largest blob a pack holds, not a SHA-256 digest, or over the in-flight budget), `unsupported` (the commit was answered UNIMPLEMENTED), `refused` (the commit was refused for a reason a repeat does not change), `store` (the object store failed the put of the pack), `record` (the commit failed for a reason a repeat may pass), `index` (the pack index failed before the pack was written) |
| `kbf_pack_daemon_missing_indexed_total{node}`, `kbf_pack_daemon_missing_unasked_total{node}` | counter | digests of a find-missing question the pack index answered, and digests that went to the storage nodes whole because the index could not be asked |

A farm that writes packs has `kbf_pack_daemon_write_fallbacks_total` flat at 0 apart from `off`
and `unfit`; a rising `store`, `record` or `index` says the packs are failing and outputs are going
to the storage nodes (nothing is lost: the pack object of a failed commit is unindexed garbage
under the daemon's own prefix). A pack that was stored and not committed leaves that garbage, and
its blobs are counted in `record`, not in `blobs_packed`.

**The gate for moving a farm to packs.** The legacy reads are the reads that still go to the storage
nodes for a blob a pack could hold: the frontend's `kbf_pack_read_fallbacks_total` plus the sum
over nodes of `kbf_pack_daemon_elsewhere_total`. A farm has moved when that sum has stopped
rising (`increase()` over a window longer than a build is 0), and `kbf_pack_daemon_unasked_total`,
`kbf_pack_daemon_lost_total` and `kbf_pack_daemon_damaged_total` have stayed flat throughout
(the last two at 0). A rising `kbf_pack_daemon_unasked_total` says the packs are being skipped,
not that nothing is left on the nodes. A read of a blob over 64 MiB, or of one whose digest is
not SHA-256, is never asked of the packs and always counts in `kbf_pack_daemon_elsewhere_total`,
so a farm that reads such blobs has a sum that keeps rising: the criterion is that it has stopped
rising apart from those reads.

**Input cache counts.** A frontend exports what each daemon says its input cache did, from the
`input_cache` of its heartbeats ([`GET /v1/nodes/{id}`](#input-cache-counts) shows the same), as the
counters of the daemon's process, one sample per node. They start again from 0 when the daemon
restarts, as a counter may. A node that sent none (its cache is off, or its daemon predates the
field) has no sample, and with no node sending any the families are absent; a node whose next
heartbeat carries none loses its samples. A node whose stream has closed keeps the counts of its
last heartbeat until the frontend restarts. The unit is the cache entry, not the file name.

| Family | Type | What it measures |
|---|---|---|
| `kbf_node_input_cache_fills_total{node}`, `kbf_node_input_cache_fill_bytes_total{node}` | counter | entries the node's daemon wrote into its input cache from the content store (cold fills), and their bytes |
| `kbf_node_input_cache_hits_total{node}`, `kbf_node_input_cache_hit_bytes_total{node}` | counter | entries a lease found in the cache and used, and their bytes |
| `kbf_node_input_cache_evictions_total{node}`, `kbf_node_input_cache_evicted_bytes_total{node}` | counter | entries evicted for the byte budget or the free-space floor, and their bytes |
| `kbf_node_input_cache_tree_builds_total{node}`, `kbf_node_input_cache_tree_hits_total{node}`, `kbf_node_input_cache_tree_evictions_total{node}` | counter | the shared tree store's builds, acquires that found the tree built, and evictions; samples only for nodes that have a tree store |

**Measured load.** A frontend with a farm hands the scheduler each node load reading it accepts
([Node load](#node-load)), in every `--placement-load` mode, and exports what the scheduler has
counted of them and of placement by load, and each node's load as its view shows it, so that
placement by measured load can be watched before and after `--placement-load on`. The same families
are written by the farm's own scheduler core and by the js core.

The scheduler's counters are written on every scrape of a frontend with a farm, 0 included, and
count since the frontend started (a restart starts them again from 0):

| Family | Type | What it measures |
|---|---|---|
| `kbf_sched_load_readings_accepted_total` | counter | readings the scheduler accepted |
| `kbf_sched_load_readings_dropped_total` | counter | readings the scheduler dropped: out of order, a counter or the clock going back, or implausible (more than four times the node's cores or memory). A reading the server itself refuses (a repeat, an older one) is not handed to the scheduler: it counts only in `kbf_node_load_refused_minutes_total` of its node, by the minutes it carried |
| `kbf_sched_load_floor_windows_total`, `kbf_sched_load_floor_binds_total` | counter | accepted readings that gave a window, and of those the windows in which the floor from the covered leases' predictions was above the measurement |
| `kbf_sched_load_misfit_cpu_minutes_total`, `kbf_sched_load_misfit_memory_minutes_total` | counter | lease-minutes whose mean cores were over twice the charge (and 250 millicores over it), and whose working set was above the curve's remaining maximum |
| `kbf_sched_load_demotions_cpu_total`, `kbf_sched_load_demotions_memory_total` | counter | times the shape guard charged a key's CPU, or memory, flat for its next runs |
| `kbf_sched_load_admitted_total` | counter | leases the load predicate admitted (`--placement-load on`) |
| `kbf_sched_load_admitted_fallback_total`, `kbf_sched_load_admitted_cold_total`, `kbf_sched_load_curve_flat_total`, `kbf_sched_load_profile_cpu_total`, `kbf_sched_load_profile_memory_total` | counter | of those admitted: on a node charged by prediction alone; cold (a key with no usable estimate); whose key has a curve that was not used (charged flat in CPU and memory); charged by curve in CPU; charged by curve in memory |
| `kbf_sched_load_blocked_total`, `kbf_sched_load_cold_deferred_total` | counter | queued requests, once per request per round, that had a node with room apart from the predicate and found none it admitted, and of those the ones whose only obstacle was the cold cap |
| `kbf_sched_load_reservations_total` | counter | head reservations taken or moved |
| `kbf_sched_load_score_evaluations_total`, `kbf_sched_load_scored_total` | counter | slot-resource evaluations of the placement score, and placements the score chose among two or more nodes |
| `kbf_sched_load_chose_slack_total`, `kbf_sched_load_chose_distrusted_total`, `kbf_sched_load_chose_concentrated_total`, `kbf_sched_load_chose_by_tie_break_total` | counter | of the scored placements: on a node predicted above its capacity; on a distrusted node; on a node concentrated on the lease's key; decided by the last tie-break |
| `kbf_sched_load_fallback_cpu_limited_nodes` | gauge | live nodes charged by prediction because their newest reading says the actions cgroup has a CPU limit, while `--placement-load` is on: not a running total, and above 0 means a node whose cgroup limit and pool do not match |

The js core also writes the `kbf_ablation_*` families, which split the counts above and a few
more into the cases a run of the placement ablation is compared on (js alone, `--placement-load
on`, `--placement-predictions` shadow or on). All are written from the start of a frontend on the
js core and absent on the farm's own scheduler core:

| Family | Type | Labels | What it measures |
|---|---|---|---|
| `kbf_ablation_placements_total` | counter | `decision` | `by_load`: leases admitted by the load rule on a node with a usable reading; `fallback_charge`: leases admitted on a node charged by prediction alone; `refused_over_capacity`: queued requests, once per request per round, that had a node with room apart from the rule and found none it admitted. All 0 while placement does not read the load. The nodes charged by prediction because of a CPU quota are `kbf_placement_cpu_limited_nodes` |
| `kbf_ablation_charges_total` | counter | `source` | of the leases admitted by load: `profile_cpu` and `profile_memory` charged by their key's learned curve, `curve_flat` whose key has a curve that was not used |
| `kbf_ablation_bookings_total` | counter | `source` | leases started, by what admission charged them: `prediction` (a learned estimate), `cold` (the cold costs of a key with no estimate), `retry` (a rerun up the memory ladder, charged at its rung), `default` (the request's cores and no memory) |
| `kbf_ablation_oom_retries_total` | counter | `rung_gib` | leases given up after passing their own memory limit, by the whole GiB they run again at; a series for a rung only once something was retried at it |

Each node that has a load in its view ([Node load](#node-load); `GET /v1/nodes/{id}`) has one
sample of each of these, labelled `node` (the node id) as the pack families are: at most one series
per node the frontend has accepted a reading of. A node with no accepted reading has none. A node
whose stream has closed keeps its samples, going stale, until the frontend restarts (as `load` is
kept in the node view and the pack counts are kept); with no node having a load no family is
written. A value the view leaves out has no sample and is never written as 0, and a family that no
node has a sample of is not written at all.

| Family | Type | Labels | What it measures, and when it has no sample |
|---|---|---|---|
| `kbf_node_load_measured_cpu_millicores` | gauge | `node` | `measured_cpu_millicores`: the CPU the node's actions used, in thousandths of a CPU, the larger of the last two windows. No sample with fewer than two windows, and when `stale` |
| `kbf_node_load_memory_working_set_bytes` | gauge | `node` | `memory_working_set_bytes` at the last accepted reading |
| `kbf_node_load_memory_active_file_bytes` | gauge | `node` | `memory_active_file_bytes` at the last accepted reading |
| `kbf_node_load_admission_working_set_bytes` | gauge | `node` | `admission_working_set_bytes`: the working set less the active file pages beyond a quarter of the node's memory |
| `kbf_node_load_stale` | gauge | `node` | 1 when no reading was accepted for three windows, else 0 |
| `kbf_node_load_actions_cpu_limited` | gauge | `node` | 1 when the actions cgroup has a CPU quota, else 0 |
| `kbf_node_load_cpu_throttled_seconds_total`, `kbf_node_load_cpu_pressure_some_seconds_total` | counter | `node` | `cpu_throttled_usec` and `cpu_pressure_some_total_usec` in seconds (the view's microseconds over a million, written with six decimals): the cumulative CPU throttled time and CPU pressure (`some` total) of the actions cgroup. They pass through the daemon's cgroup counters, so they can fall if the cgroup is recreated within a session, which Prometheus reads as a counter reset. No sample when the daemon could not read them |
| `kbf_node_load_clocks_total`, `kbf_node_load_minutes_total` | counter | `node` | `lists.clocks` and `lists.minutes`: the clock and minute records kept, summed over the accepted readings of the daemon's process |
| `kbf_node_load_refused_minutes_total` | counter | `node` | `lists.refused_minutes`: the minutes carried by readings that were not accepted |
| `kbf_node_load_dropped_total` | counter | `node`, `reason` | `lists.dropped`, one series per reason: `invalid`, `duplicate`, `over_cap` (always all three for a node that has a load) |

The counters of a node are those of its daemon's process: they start again from 0 when it restarts.
The `covered` list of the view is not exported.

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
shape `key`, its `cfg`, the `outcome` and the `lease`, then `booked_millicpus` and
`booked_memory_bytes`, the lease's grant, to set against what it used, and names the `key` on each
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
| `GET /v1/invocations/{id}/failures` | the invocation, and a page of the actions that failed (an OK status with a non-zero exit, a failed status, a timeout, a refusal), each with its reason, attempts, command and the end of its logs ([below](#the-failures-view)) |
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
booking, memory used and limit of a killed run, `booked_millicpus` and
`booked_memory_bytes` (what the run's lease was granted, for every run, with the
meaning and the gaps given for the build pages' attempts above; the two routes show the
same grant), the REAPI times and cgroup `usage` the
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
  `node_pressure`, read from how it ended); `booked_millicpus` and
  `booked_memory_bytes`, what the lease was granted (its `Start`'s booking, after the
  memory ladder and the floors, not the client's request), thousandths of a CPU and
  bytes, for every attempt whose report the scheduler took; each is left out for a
  lease taken up from the state store, which the server never started, and for a record
  written before the grant was kept (its `booked_memory_bytes` is then only a
  memory kill's); `cap_bytes` for an own-limit kill
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

### The failures view

`GET /v1/invocations/{id}/failures` answers `{"invocation": {...}, "failures": [...]}` and,
unless it is the last page, `next_page_token` and `next_page`. `invocation` is the
invocation's own answer, its `links` including `failures`. `failures` lists, in the order
the farm first saw them (never grouped by kind), every action row whose outcome is not OK
with exit code 0; an action still in flight is not listed, and one that passed after a
rerun is not a failure (its reruns are in its `attempts`). It takes `?page_size=`
(20 by default, 1 to 100) and `?page_token=`, and no filter. Each failure holds:

| Field | Meaning |
|---|---|
| `action`, `href`, `instance`, `target`, `mnemonic` | as on `/actions` |
| `outcome` | as on `/actions`: `{"ok": {"exit_code": 1}}`, `{"failed": {"code", "message"}}`, `{"timed_out": {}}` or `{"refused": {"reason"}}` |
| `failure` | the last attempt's failure class (`timeout`, `infra`, `invalid`, `out_of_memory`, `node_memory_pressure`, `missing_input`, `lost`) with the daemon's reason text, `{"class", "reason"}`; `null` when the last attempt did not end in a failure (a run that exited non-zero, a refusal) |
| `attempts` | every lease, as on `/actions` |
| `command` | `{"arguments", "environment_names", "working_directory", "platform", "timeout_micros", "output_paths", "truncated"}`, read when the page is answered from the Action and Command blobs. **Unredacted**: arguments are shown as the build sent them. Environment variables are names only, never values. `platform` is the Action's properties, or the Command's when the Action names none; `timeout_micros` is `null` when the Action sets none; `truncated` says strings were dropped from the end (output paths, then names, then arguments) to keep them within 64 KiB. `null` when a blob is not held, was never uploaded or is over 1 MiB |
| `command_available` | whether `command` is there |
| `stdout`, `stderr` | `null` when the result has none; else the blob object of `/actions` (`digest`, `available`, `href`) with `bytes` (the log's size), `tail` (the last 16 KiB, invalid UTF-8 replaced, a cut inside a character dropped), `tail_bytes` (the bytes the tail shows) and `truncated` (the log is longer than the tail). When there is no tail, `tail` is `null` and `tail_omitted` says why: `not_held` (the CAS does not hold the blob), `too_large` (over 64 MiB), `unreadable` (the read failed or took over 10 s) or `blobs_off` |
| `outputs`, `outputs_truncated`, `usage` | as on `/actions/{digest}` |

The command and the log tails are read from the CAS only when the policy names a
`blobs.getAuthorizer` (the start line says ` blobs=on`): without it they are not read
and each `tail_omitted` is `blobs_off`, so the page shows nothing the blob route would
refuse. The body writes `<`, `>` and `&` as `<`, `>` and `&` (a JSON
parser reads the same text back), and is sent as `application/json` with
`X-Content-Type-Options: nosniff`, because arguments, targets and logs are text the
build chose.

| Code | When |
|---|---|
| `200` | the answer |
| `400` | a parameter the route does not take, one given twice, a `page_size` outside 1 to 1000 (1 to 100 on `/failures`), a filter value not listed above, or a page token that does not decode, was made for another route or another filter, or names a position past the rows |
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

## `GET /ui`: the web UI

Read-only pages for people with a browser, server-rendered HTML from the `kbf-ui` crate
([design](design/kbf-ui-v1.md), [operator guide](deploy/kbf-ui.md)). On the operator API listener, **on by default**:
`--ui=false` turns it off and every `/ui` path is then a `404`. Without `--api-listen`
there is nothing to mount it on.

- **Open.** No login, no certificate, no token, whatever the REAPI policy says: the pages
  are served to whoever reaches the port, and show everything unredacted, including the
  logs of builds and the arguments of failed commands. The routes the pages read that the
  policy gates on the network (the build pages, run logs, blobs)
  are asked in process with every caller allowed; asked over the network they keep their
  gates. Keep the operator API on loopback, or behind the proxy and a network you trust.
- **Read-only.** `GET` and `HEAD`; every other method is `405`. The write routes and their
  token are unchanged, and the pages mount none of them.
- **Pages:** `GET /ui` and `GET /ui/` (Overview: readiness, nodes by state, queue, leases and
  storage from `GET /v1/health`, the ten latest builds from `GET /v1/invocations`),
  `GET /ui/nodes` (`GET /v1/nodes`), `/ui/nodes/{id}` (`GET /v1/nodes/{id}`),
  `GET /ui/builds` (the recent builds of
  `GET /v1/invocations`, filtered by `?state=open|closed` and `?instance=`, paged by
  `?page_token=`; `?id=` looks one build up by id; any other parameter, one given twice or
  another state is `400`), `/ui/builds/{id}` (the summary, the failed actions of
  `/failures` first, then the actions; `?failures_token=` and `?page_token=` page the two
  lists), `/ui/builds/{id}/actions/{digest}` (the action; for one that failed, its failure
  class, command arguments and environment names, exit code and the end of its stdout and
  stderr from `/failures`, each with a link that downloads the whole log, which the blob
  route answers as an attachment), `/ui/leases` (`GET /v1/leases`;
  `?node=`, `?page_size=` and `?page_token=` are handed to the route, and the next page is
  the route's own link on the page's path), `/ui/queue` (`GET /v1/queue`), and the
  stylesheet at `/ui/static/kbf.css`. Each page lists, at its foot, the `/v1` routes it read, each a link
  to the JSON. The Action page looks a failed action up in the first 5 pages of
  `/failures`, or in the one page `?failures_token=` names (the Build page's links name it).
  The Overview, Nodes and Node pages reload themselves when asked:
  `?refresh=N` with N one of 5, 10, 30 or 60 (`<meta http-equiv="refresh">` with only the
  integer); any other value, a repeated `refresh` or another parameter is `400`, and a page
  whose read failed does not reload.
- **Headers on every `/ui` answer**, errors included: `Content-Security-Policy:
  default-src 'none'; style-src 'self'; img-src 'self'; frame-ancestors 'none'; base-uri
  'none'; form-action 'self'` (no script of any kind; the forms are the Builds id lookup and
  the Leases node filter, both `GET`), `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`,
  `Cache-Control: no-store`, `Cross-Origin-Resource-Policy: same-origin`,
  `Cross-Origin-Opener-Policy: same-origin` and a `Permissions-Policy` denying every
  powerful feature (camera, microphone, geolocation, usb, serial, hid, bluetooth,
  payment, display capture, clipboard, midi, xr, and the rest of its list) with an
  empty allowlist. No `Strict-Transport-Security`, deliberately: a farm often serves a
  private-CA or self-signed certificate, and HSTS would stop a browser from proceeding
  past its warning. The proxy's own answers (503, 404, 405, 401) carry a
  stricter set; see `docs/deploy/kbf-ui.md`.
- **Bounds.** Each caller has at most 4 reads open at once on `/ui`, and 4 on the read
  routes the pages use (`/v1/blobs`, `/v1/invocations` and below, `/v1/runs` and the history
  routes): the fifth is `429` with `Retry-After: 1`, before its handler
  runs, and a slot is held until the answer's body has been sent or dropped. A caller is
  the node the proxy named, else the client address the proxy forwarded in
  `x-kbf-client-addr` (believed only of a call that presented the proxy secret; the header
  on any other call is stripped unread), else the peer address. So behind the proxy each
  browser has its own bound; behind a proxy that sends no such header, browsers share the
  proxy's address and so share one bound. A path longer than 2048 bytes, or a query longer
  than 4096, is `414`; a path segment longer than 256 bytes, or one that decodes to a
  control character, is `400`.

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

## `PUT /v1/releases/{commit}/{platform}/{name}`, `GET /v1/releases` and `GET /v1/releases/{commit}` (removed)

`kbf-server` no longer keeps a release store: the three routes answer `404`, and the
gRPC `kbf.update.v1.ReleaseService` (`Fetch`, `Describe`) is not served, so a call to it
is refused as a method the worker listener does not serve. A release reaches a host as a
package. `--releases-dir` is accepted, hidden from `--help` and ignored for one release, and
logs a warning naming it; a directory left there is never opened, changed or deleted.

## `GET /v1/update` and `POST /v1/update/{desired,pause,resume}` (removed)

`kbf-server` no longer serves these routes: they answer `404`. It keeps no desired release,
no update policy and no proposals. Updates are
OS packages installed by `kbf-updater` on each host. The `update/*` rows that older builds
left in a state store are not read and not changed; a store that holds them opens and serves
as before.

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
