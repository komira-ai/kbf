# All-in-one vs distributed

`kbf` is one binary with five modes. A farm is either one process on one host
(`all-in-one`) or several processes on several hosts (`frontend`, `storage`, `daemon` and
`storage-and-daemon`). This page says what each mode runs, which ports it uses and who
dials whom, when to choose which, how to grow from one host to many, and how each mode
stops. Every flag and default below is the one in the code; settings the page does not
name are in the config reference at kbf.dev/docs/config/.

The units that start each mode, and the rules `kbf-deploy` checks them against, are in
[kbf-units.md](kbf-units.md). Installing a host is [kbf-installer-setup.md](kbf-installer-setup.md).

## The modes

A mode is chosen with `kbf --mode=<mode>`, or with `mode = "..."` in `/etc/kbf/kbf.toml`
(`kbf` alone reads the file). `kbf --mode=<mode> --help` lists the mode's flags, and a mode
refuses by name the flags that belong to another.

| mode | what the process runs | listens on | state it keeps |
|---|---|---|---|
| `all-in-one` | the frontend and a build daemon, in one process | REAPI, worker listener, optional operator API | the scheduler's state store; blobs in a cold store it opens itself |
| `frontend` | REAPI, the scheduler, the worker listener, the operator API; no blobs | REAPI, worker listener, optional operator API, optional enrollment listener | the scheduler's state store |
| `storage` | the storage service: the cache and the metadata log | the storage port | blobs and the metadata log |
| `daemon` | a build node: it runs actions | nothing | a data directory of leases and input caches |
| `storage-and-daemon` | a storage node and a build node in one process | the storage port | as `storage` and `daemon` |

**`all-in-one`.** The frontend runs as `kbf frontend` would, except that it keeps its own
blobs, and the daemon runs beside it with the in-process data plane: its blobs move
through calls into the server half, not over the network. The daemon's control plane is a
remote daemon's: it dials the server half's worker listener on loopback over mutual TLS.
Linux only, container driver only: any other `--driver` is refused, and so is `--cas`.

**`frontend`.** It serves REAPI (Execute, the scheduler, operations) and the worker
listener's work streams, and it routes each cache call to a storage node. It holds no blobs
and refuses every flag of a store. `--storage` is required: the URL of one storage node.
`--state-store` is required too, a Postgres URL (a `sqlite://` one still starts in this
release; see "Growing from one host to many"): a frontend given none, by the flag or by `state-store` in `kbf.toml`, exits 2 naming the flag
before it listens on anything, and `kbf --mode=frontend --check` exits 2 the same way. A
Postgres store that is not up yet is waited for, `--state-store-wait-secs` seconds (default
300; 0 does not wait; the window is the time spent waiting between attempts), and a start that runs out of the wait exits 1, not 2
([kbf-units.md](kbf-units.md#the-state-store-postgres-for-the-frontend)). With
the TLS flags the frontend keeps the farm's storage map and routes every call by it;
`--storage` is then only the bootstrap node.

`--pack-index` (off by default; the `pack-index` key of `[scheduler]` in `kbf.toml`) opens
the pack index at start: two tables of the Postgres state store (`pack` and `blob`) that
say which pack holds each blob and where. The worker listener then answers a daemon's
`kbf.worker.v1.PackIndex/Locate` from the index (daemon certificates only; without the flag
the service is not registered and the call is UNIMPLEMENTED). Unless the frontend is also
given the pack store flags below, nothing else reads or writes them. It needs a
`postgres://` state store whose schema
has the tables, which `kbf-installer setup` and the update step create, and a `sqlite://`
state store is refused with a line that says so (exit 2). With the flag off no pack table is
touched. The tables are an additive schema step: a kbf of the previous release keeps running
on the migrated schema.

**Packs** (off by default; the `[packs]` table of `kbf.toml`). With
`--pack-store-endpoint` (an `http://host:port` S3-compatible object store),
`--pack-store-bucket` and `--pack-store-credentials-file` (JSON
`{"access_key_id": "...", "secret_access_key": "..."}`, mode 0600 or 0400; the secret is never
a flag), and `--pack-index`, the frontend keeps the blobs clients upload in packs: many blobs
in one object, written to the store with a create-only request and recorded in the pack
index before the upload is acknowledged (a failed write answers UNAVAILABLE; packs finished
before the failure stay recorded, and the client sends the rest again). FindMissingBlobs asks the index and then, for the digests it lacks, the storage
nodes. A read, the client's and Execute's own, looks in the index first and fetches only the
blob's bytes from its pack with a ranged request, hashes them, and serves them (a pack that
holds other bytes than the digest names is never served); a blob the index lacks is read
from the storage nodes as before. A blob over 64 MiB and a compressed blob are not packed.
The closure check of the action cache counts a blob as held when the index has it: the check
of a result a daemon reports, and of every hit (Execute's and a client's `GetActionResult`),
works out what the result names (its output files, standard streams and the files of its
output trees) and asks the index first and the storage nodes for the rest, so an output that
is only in a pack satisfies it; the entry handed to the nodes names only the blobs they hold.
A result with an output that neither holds is refused on a write and is a miss on a hit. On a
routed farm a client's `GetActionResult` asks each blob's primary node only (as
`FindMissingBlobs` does), where Execute's check also tries the blob's other owners, so right
after a map change a client may see a miss for a result that Execute still hits, and runs the
action again. With no pack flags the nodes check the entry alone, as before.
`--pack-bytes` (default 32 MiB) is the length at which a pack is closed, `--pack-writer`
(default `frontend`) names the frontend in its pack keys, `--pack-store-region` (default
`us-east-1`) is the signing region, and `--pack-cache-dir` with `--pack-cache-bytes` keeps
the blobs read from packs on a local disk. A daemon that writes packs itself commits them
to the frontend (`kbf.worker.v1.PackIndex/CommitPack`, the daemon role only, and only a
frontend with the pack index and these flags takes it; `Welcome.pack_commit` tells the
daemon whether this one does): the frontend reads the pack's last byte (and refuses a pack
that is longer than the commit says), refuses a commit whose blobs add up to more bytes than
the pack, and re-hashes `--pack-commit-sample` blobs of it, reading each in ranges of at
most 8 MiB (default 2; at most 16384, which checks every blob; 0
checks none and trusts the daemon, whose bad blob is then caught only when it is first read)
before it records the pack, refuses a writer that is not the calling node, and counts the
refusals (`kbf_pack_commit_rejects_total`). At start the frontend checks that the store
refuses to replace an object, and does not serve if it does not. `GET /metrics` gains the
`kbf_pack_*` families. Left out, no object store is built or contacted and the frontend
serves exactly as before. A daemon reads a blob that is only in a pack with `--pack-read`
(below).

The uploads into packs share a bound on the bytes they hold in flight,
`--pack-inflight-bytes` (default 1 GiB, at least 64 MiB, the largest blob that is packed, so
that no upload is refused for its size; at most 4294967295): an upload takes its blob's size as
soon as its resource name has arrived, before it reads on, and gives it back when it ends, and
one that finds the bound spent waits its turn, so a burst of uploads is slowed down and none is
failed for it (`kbf_pack_upload_waits_total` counts the waits, `kbf_pack_inflight_bytes` and
`kbf_pack_inflight_peak_bytes` the bytes taken). The bound counts blob bytes, not the memory of
the process. A `BatchUpdateBlobs` is decoded before its handler runs, so the bytes it holds
while it waits are not counted, and it has no idle timeout; reads are not counted either (a
read holds one blob, at most 64 MiB, or one batch). `--pack-upload-idle-timeout` (default 30
seconds, at most a day) ends a ByteStream upload that gets no byte of its body for that long
with `DEADLINE_EXCEEDED`: it stores and records nothing, gives its bytes back, and is logged
and counted (`kbf_pack_upload_idle_timeouts_total`). The time before the resource name has
arrived is not counted, so a client that never sends it is not cut by this timeout. An upload
that keeps sending, however slowly, is never cut and keeps its bytes until it ends, so
`--pack-inflight-bytes` is the only bound on that. A client that claims a blob and sends
nothing holds only its reservation, never a buffer of the claimed size. Both flags are
accepted and ignored without `--pack-store-endpoint`.

A daemon reads from the packs with `--pack-read` (off by default; the `[packs]` table of
`kbf.toml`), which needs `--pack-store-endpoint`, `--pack-store-bucket` and
`--pack-store-credentials-file` (a JSON key pair, mode 0600 or 0400, of a key that may read
`cas/` and write nothing; `--pack-store-region` defaults to `us-east-1`). Its reads ask the
frontend where a blob lies, read that range of the pack from the object store and check it
against the digest. A blob the frontend does not locate, and a frontend without
`--pack-index`, leave the read where it was (the storage nodes), as does a frontend of the
previous release, which answers PERMISSION_DENIED (logged once, then once more after a
read that was answered, and counted as `unasked`); `missing`, uploads and every
other call are unchanged, so without `--pack-write` the daemon still uploads its outputs to the storage members.
A pack that is gone or whose bytes are not the digest fails the read. Each read asks for one
blob: one `Locate` call and one ranged request, with at most 16 asking at once. A blob of
more than 64 MiB is read where it was before, as a read from a pack holds the whole blob in
memory, outside the transfer budget (`--transfer-bytes-in-flight`). `--pack-read-cache-dir` with
`--pack-read-cache-bytes` adds a cache of the blobs read from packs (the daemon's input
cache already keeps blobs by digest, so it is off by default). `kbf daemon` asks over its
connection to `--cas`. `kbf storage-and-daemon` with a `--frontend` reads packs too, over
its member plane: its daemon half asks over a connection to the first `--server`, the
frontend's worker listener that its session is made to, with the certificate the session
presents. That certificate has the `daemon` role, the only one the listener lets call
`Locate` (the node's storage half has a certificate of its own, with the `storage` role, and
does not call it), so no role table changes. The flags are the same, with the same fallbacks,
errors and bounds; reads of the local member's blobs and of the others' are alike packed
reads first, and the outputs still go to the members. `kbf all-in-one` and
`kbf storage-and-daemon` without a `--frontend` read blobs in-process from a store that is
the whole farm, with no frontend to ask and no pack index, and refuse the flag. Counts of
what was read from packs, elsewhere, lost and damaged are logged when the daemon stops
(`pack reads: ...`), and a lost or damaged blob is logged when it is found.

A daemon writes its outputs into packs with `--pack-write` (off by default; the `[packs]`
table of `kbf.toml`), which needs `--pack-read` and `--pack-store-write-credentials-file`, a
key pair file of the same form as `--pack-store-credentials-file` but of a key that may
create objects under `cas/` (the read key stays read-only; the endpoint, the bucket and the
region are the ones of `--pack-read`). Refused at parse time without either, naming the flag
that is missing. At start the daemon checks that the store refuses to replace an object and
does not start if it does not or if the store cannot be reached for the check; its packs are
named `cas/d/<--node-id>/<boot>/<n>`, where the boot is new on every start. Packs are written
only while the frontend says it takes `CommitPack` (its `Welcome` carries `pack_commit`: it
has `--pack-index` and the pack store flags); until then, and after a frontend of the
previous release answers UNIMPLEMENTED, the outputs go to the storage members as before. A put
is acknowledged only after its pack is stored and the frontend has recorded it; a blob that
cannot be packed, or a pack that cannot be committed, is written to the storage members
instead. The counts are logged when the daemon stops (`pack writes: ...`); they are not yet
on the heartbeat. `kbf all-in-one` and `kbf storage-and-daemon` without a `--frontend` refuse
`--pack-read`, and with it `--pack-write`.

The same counts ride every heartbeat of a daemon that has `--pack-read` (since the daemon
process started; a daemon without the flag sends none), and the frontend its session is on
exports the newest of each node on `GET /metrics` as `kbf_pack_daemon_served_total{node}`,
`kbf_pack_daemon_served_bytes_total{node}`, `kbf_pack_daemon_elsewhere_total{node}`,
`kbf_pack_daemon_unasked_total{node}`, `kbf_pack_daemon_lost_total{node}` and
`kbf_pack_daemon_damaged_total{node}` ([api.md](../api.md#get-metrics) has what each counts
and what a gone node shows). They are the gate for moving a farm to packs: the legacy reads
are the frontend's `kbf_pack_read_fallbacks_total` plus the sum over nodes of
`kbf_pack_daemon_elsewhere_total`, which must have stopped rising, and
`kbf_pack_daemon_unasked_total` must stay flat (a rising one is the packs being skipped), and
`kbf_pack_daemon_lost_total` and `kbf_pack_daemon_damaged_total` must stay 0. Reads of blobs
over 64 MiB and of digests that are not SHA-256 are never asked of the packs and always count in
`kbf_pack_daemon_elsewhere_total`, so the sum stops rising apart from those. A daemon is counted
by the frontend its session is on, so the sums are taken over every frontend of the farm.

A daemon that also has `--pack-write` puts the counts of its pack writes in every heartbeat too,
and the frontend exports them as `kbf_pack_daemon_packs_written_total{node}`,
`kbf_pack_daemon_blobs_packed_total{node}`, `kbf_pack_daemon_packed_bytes_total{node}`,
`kbf_pack_daemon_write_fallbacks_total{node,reason}` (`reason` is one of `off`, `unfit`,
`unsupported`, `refused`, `store`, `record`, `index`), `kbf_pack_daemon_missing_indexed_total{node}`
and `kbf_pack_daemon_missing_unasked_total{node}` ([api.md](../api.md#get-metrics)). A rising
`store`, `record` or `index` fallback is outputs going to the storage nodes because packs are
failing; `off` is a frontend that does not take pack commits.

The blobs the storage members flushed before the frontends wrote packs are in no pack the
index knows, so a read of one still goes to the members. `kbf-pack-adopt`, an operator tool and
a binary of its own, indexes those segments where they lie, with no byte copied and nothing
written to the object store: [pack-adopt.md](pack-adopt.md) gives the order to run it in.

**The job-facing read listener** (off by default; the `[job-read]` table of `kbf.toml`). A task
that runs an action on a worker fetches the action's input files from the frontend, with a read
grant (a token the frontend gives the run, valid until its expiry, for one Action: it reads that
Action, its Command and its input tree, and no other blob). The task is untrusted code that holds
no client certificate, so the REAPI listener is not open to it; `--job-listen <addr>` opens
another, over HTTPS, that serves `GET /<hex>/<size>` with `Authorization: Bearer <read grant>` and
nothing else (any other path is 404, any other method 405). It needs three more flags, all four
given together:

- `--job-tls-cert` and `--job-tls-key`: the certificate chain and key of the listener (PEM), issued
  by the farm CA that the task is told to trust. The TLS is the server's alone: no client
  certificate is asked for. The task dials the frontend by IP address, and a client that dials an
  IP address accepts only a certificate that names it as an IP subjectAltName, so the certificate
  must carry one for each address a job dials the frontend at (for a farm of frontends, one
  certificate per frontend with its own address, or one naming them all). The start checks this
  and exits 2 when the certificate has none, or when `--job-listen` is on one address and the
  certificate does not name it; on `0.0.0.0` or `::` it needs some IP name. The files are read
  again every `--tls-reload-interval-secs` like the other listeners' (the names are checked at
  start only).
- `--job-grant-secret-file`: the farm secret the grants are signed with (at least 32 bytes, mode
  0600 or 0400, owned by the server's user; surrounding whitespace is dropped). Every frontend
  of the farm, and whatever issues the grants, reads the same secret, so a grant is verified by
  any frontend. It is never a flag.

The answers are 200 with the blob and its `Content-Length`; 401 for a missing, malformed, forged,
expired or revoked grant (with `WWW-Authenticate: Bearer`); 403 for a digest outside the grant's
scope, or a grant whose Action has no readable inputs in the store; 404 for a blob that is not
held, an unknown path, and 405 (with `Allow: GET`) for another method; 413 for a blob over 64 MiB,
without reading it; and 503 when the store cannot answer. A body carries a reason code
(`missing_token`, `invalid_token`, `expired`, `revoked`, `out_of_scope`, `no_scope`, `too_large`,
`not_found`, `method_not_allowed`, `store_unavailable`) and nothing else. A blob is read as the
REAPI listener's reads are (the pack index first, then the storage nodes, checked against its
digest) and sent whole, so a response never holds more than 64 MiB; at most 32 reads are open at
once, at most 256 connections are open, a client has 10 seconds to send a request's headers, and a
connection lives at most 10 minutes. The listener's socket is closed the moment a drain begins; the
connections open finish their request. `GET /metrics` of the operator API gains
`kbf_job_read_requests_total` (by `outcome`) and `kbf_job_read_bytes_total`, and
`kbf_tls_reloads_total` a `listener="job"` series. Where the jobs run must be allowed to reach
this address and port (the egress allowlist of the worker's network), and the listener should be
on an interface the jobs, and nothing outside the farm, can reach.

**`storage`.** It serves the storage service over gRPC and nothing else: no REAPI, no
scheduler, no operator API (only `GET /metrics`, on `--metrics-listen`, off unless given).
It has no state store; it reaches one only through a frontend. Without `--frontend` it is a
farm of one member. With `--frontend <url>` it registers with that frontend, heartbeats to
it, and takes the storage map from it.

**`daemon`.** The build node. `--server` (required, repeatable) names the frontend's
worker listener. It holds no store and no credential. `--driver` is required: `container`
on Linux, `native` on a Mac.

**`storage-and-daemon`.** The `storage` and `daemon` flags in one command line, in one
process: the daemon half reads and writes the node's own storage through calls, with
no socket between. Linux only, container driver only. With `--frontend` the node registers
and the daemon routes each blob by the frontend's map: a blob this node owns goes in-process,
any other goes over mutual TLS to its owner. Without `--frontend` the node is the whole
farm. With `--frontend`, `--pack-read` makes the daemon half read blobs from the packs first
(see "Packs" above).

### The one-port proxy

`kbf-proxy` is the public front door of a frontend host. It terminates TLS on one port,
checks the client certificate when one is presented, and routes each HTTP/2 stream (not
each connection) to a frontend of its pool on loopback, telling the frontend who the
caller is. REAPI, worker sessions, the operator API and enrollment all arrive on that one
port; a path the proxy does not know is answered by the proxy and never forwarded. The
proxy exists so that a frontend can be updated without failing a build: the frontends of
its pool (`[[proxy.frontend]]` tables of the `[proxy]` table) run on their own, and an
update drains one at a time through the proxy's admin API. The proxy starts, adopts and
swaps no frontend: the two-slot supervision it once had was removed, and a `[proxy]` table
with the `upstream` table is refused. It fronts `kbf --mode=frontend` only; an `all-in-one`
process cannot sit behind it. The update of a pool is described in
[kbf-updater.md](kbf-updater.md).

To run several frontends behind a load balancer of your own instead (Envoy, with a
compose example), see [frontends-behind-a-load-balancer.md](frontends-behind-a-load-balancer.md).

### The installer's role

`kbf-installer setup --mode=<mode>` makes a host ready for one mode: the accounts and
directories, `/etc/kbf/installer.toml` and `/etc/kbf/kbf.toml`, the first operator token on
a frontend or all-in-one host, the state database's schema, and the mode's units. A
`storage`, `daemon` or `storage-and-daemon` host is given the frontend's URL and an
enrollment token and enrolls itself at its first start. A `frontend` or `all-in-one` host
does not enroll; its certificates and addresses are the operator's, and `setup` takes them as
flags. A host from the packages can run the same verb as `kbf setup --mode=<mode>` (without the
proxy's flags). Details: [kbf-installer-setup.md](kbf-installer-setup.md). A frontend can also run as
a container image, with no installer: [containers.md](containers.md).

## Ports and who dials whom

The defaults, from the flags themselves. A listener whose default is loopback is for one
host; a farm sets routable addresses.

| listener | flag | default | modes |
|---|---|---|---|
| REAPI | `--listen` | `127.0.0.1:8980` | `frontend`, `all-in-one` |
| worker listener | `--worker-listen` | `127.0.0.1:8981` | `frontend`, `all-in-one` |
| operator API | `--api-listen` | off unless given | `frontend`, `all-in-one` |
| enrollment | `--enroll-listen` | off unless given | `frontend` |
| job-facing read | `--job-listen` | off unless given | `frontend` |
| storage service | `--storage-listen` | `127.0.0.1:8982` | `storage`, `storage-and-daemon` |
| storage metrics | `--metrics-listen` | off unless given | `storage`, `storage-and-daemon` |
| the proxy's public port | `ListenStream=` of `kbf-proxy.socket` | `8980` | proxy hosts |
| the proxy's frontends | `base_port` of each `[[proxy.frontend]]` table | none | proxy hosts |

Some defaults come from tools around the binary, not from `kbf`:

- `kbf-installer setup` writes `api-listen = 127.0.0.1:8982` into `kbf.toml` of a frontend
  or all-in-one host when it is not given. `kbf cluster --api` defaults to the same address.
- The units that `kbf-deploy` renders give a storage node port 8983 (`KBF_STORAGE_PORT`),
  because 8982 is the operator API's on a frontend host; the enrolled node takes its storage
  port from the frontend's configuration. The renderer's REAPI, worker and operator
  ports are 8980, 8981 and 8982.
- Behind the proxy, a frontend of the pool listens on its `base_port` (REAPI),
  `base_port + 1` (worker), `+ 2` (operator API), `+ 3` (admin calls, which a frontend no
  longer serves) and `+ 4` (enrollment); the `base_port`s of two frontends are at least five
  apart. `--instance` no longer moves a frontend's listeners: the flag is accepted and
  ignored.
- A daemon opens no listening port.

A listener serves plain text unless its TLS flags are given. The REAPI and worker listeners
accept plain text on any address, and off loopback the start line carries a warning. A
storage node refuses plain text off loopback.

### Who dials whom

| caller | callee | port | how it is secured |
|---|---|---|---|
| a REAPI client (Buck2) | the frontend, or the proxy | `--listen`, or the proxy's public port | TLS with the frontend's REAPI certificate; the REAPI authentication policy (`--reapi-auth-policy`) decides who may call |
| a daemon | the frontend's worker listener (`--server`) | `--worker-listen` | mutual TLS; the daemon's certificate names the role `daemon` |
| a daemon | the storage nodes the map names | each member's registered address | mutual TLS |
| a storage node | the frontend's worker listener (`--frontend`) | `--worker-listen` | mutual TLS; the certificate names the role `storage`, with names that cover the host of `--advertise-addr` |
| the frontend | the storage nodes | each member's registered address | mutual TLS; the frontend's certificate names the role `frontend` |
| a storage node | its peers | each peer's registered address | mutual TLS; copies a write to the blob's second owner |
| an operator or `kbf cluster` | the operator API | `--api-listen` | a write must come from a loopback peer (or through the proxy, with its secret) and carry the operator token as a bearer token; reads are open to whoever reaches the address |
| a new node | the enrollment listener | `--enroll-listen` | TLS that authenticates the server only; the node pins the CA from its token |
| a storage node | the cold store | the object store's endpoint | the S3 key pair, below |

Four things follow.

- Clients never talk to storage or to daemons; the frontend does that for them. Daemons
  and storage nodes never talk to clients.
- A storage node dials the frontend, and the frontend dials the node back at the address it
  registered. That address is `--advertise-addr`, or the address the node bound, which must
  then not be a wildcard.
- A daemon's blob traffic goes to storage directly once the frontend has sent it the map
  (in `Welcome`, and in each `MapUpdate`). A daemon-only node starts on its `--cas` leg to the
  frontend's worker endpoint until the map reaches it.
- The operator API is plain HTTP/JSON under `/v1`. With a proxy it is reached through the
  public port. Without one, bind `--api-listen` where only operators can reach it, and run
  `kbf cluster` on the frontend's host: a write from another host is refused (403).

### The cold store and its keys

The cold store is the durable home of every blob: a `file://` directory on one host, or an
`s3://` bucket for a farm.

- **Registration.** The operator registers the farm's store with the frontend
  (`/v1/object-stores/...`, or `kbf cluster object-store`). The routes exist when the
  frontend runs with `--object-store-secrets-dir`, which needs `--api-listen`,
  `--api-token-file` and `--state-store`. The endpoint, bucket and prefix are kept in the state
  store. The secret key is kept only in that directory, one file of mode 0600 per
  connection, and it never enters the state store, a backup of it, or `kbf.toml`. The
  endpoint must be `http://`: an `https://` endpoint is refused, because the S3 client does
  not speak TLS yet.
- **A node gets the keys by asking.** An enrolled storage node's `NodeConfig` carries the
  store's URI (`cold_store`) and never a key. After it registers with the frontend, the node
  calls `ColdStore.GetStoreConfig` on the worker listener, with its storage certificate, and
  opens its cache on the answer. It holds the key in memory only, with core dumps off. The
  registered store must be the store the node's `--cold-store` names (endpoint, bucket and
  prefix), or the node keeps its own and reports `identity_mismatch`. The keys are probed
  before the cache opens on them.
- **A rotation reaches a node at its next start.** There is no hot swap; until then the
  frontend shows the node as `pending`.
- **A fallback.** A node that has an `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` of its
  own gives up on the frontend after four tries and starts on those. A node without them
  waits for the frontend, as it waits for the registration.
- **Who never holds a key.** A daemon holds no store and no credential. A frontend serves no
  blob itself. In `all-in-one` the cold store is a directory in the units, so there is no key.

The units that `kbf-deploy` renders for `storage` and `storage-and-daemon` still give the key
pair through `EnvironmentFile=<KBF_ETC>/s3.env`; see [kbf-units.md](kbf-units.md).

## When to choose which

### All-in-one: one host

Choose `all-in-one` for a development box, a trial, or a small team that has one build host.
It is the least to run: one process, one unit, no enrollment, no registration. What it
cannot do:

- **No replication.** There is one storage member, so there is one copy of each blob; the
  replication factor is 2 but is clamped to the number of members. Losing the host's disk
  loses the cache and the action cache.
- **No high availability.** The host is the frontend, the storage and every builder. A
  restart of the process restarts all three.
- **It stops all at once.** A stop drains the frontend half, as below, and the daemon half
  goes with it; a deploy of a new `kbf` is a restart of the whole farm.
- **Builds run beside the farm's state.** The container driver runs each action as a
  subordinate id of the process's user, but no test yet proves an action cannot read what
  that user holds. Use it on a trusted host only, as
  [kbf-units.md](kbf-units.md) says.

### Distributed: a farm

Choose a farm for more than one build host, for any need to survive the loss of a host, or
to keep the scheduler's state on a host that runs no builds. A farm is one frontend (alone or behind
the proxy), storage nodes, and daemons: `storage-and-daemon` on the Linux hosts that have
disks and CPUs, and `daemon` on hosts that only build.

- **Replication.** The map places each blob on `r` owners, and `r` is 2. It is not a flag.
  A write goes to the primary and is copied to the second owner. With one storage member the
  map has one owner.
- **A member that is down.** A member's row lives `--member-ttl-secs` (15 by default, at least
  3) after its last heartbeat; nodes heartbeat at a third of that. Then the member is
  `down` in the map and writes go to the other owner. It stays in the map for
  `--member-dwell-secs` (300 by default); a member that registers again in that time takes its
  place back. After the dwell it **leaves** the map and its digests go to other owners.
  Both flags need `--state-store`.
- **A member that is restarted on purpose.** Waiting for the TTL costs a connection attempt
  per write and refuses action-cache commits. Instead, before the restart, run
  `kbf cluster member <id> --state planned-down [--for-secs N] --token-file F`
  (`POST /v1/members/{id}` with `{"state": "planned_down"}`). The member goes `down` at once,
  one new map generation, and the dwell is counted from that call. `--for-secs` defaults to
  900 and is at most 86400; a heartbeat after it has passed returns the member to `active`, so
  a restart that never happens strands nothing. The member registering again after the
  restart, or `--state active`, undoes it. `GET /v1/storage` shows `"planned_down": true`.
- **A draining member.** `--state draining` takes a member out of the write path without
  taking it `down`; it is read first. The state of every member is in `GET /v1/members`.
- **Macs.** A Mac is a node only: `daemon` with the `native` driver, enrolled like any other
  node. Macs build what Linux cannot (Xcode, iOS), and hold no blobs; `all-in-one` and
  `storage-and-daemon` refuse any driver but `container`, so a Mac is never one of those. Its
  `kbf.toml` is rendered by `kbf-mac-provision`. See
  [kbf-installer-setup.md](kbf-installer-setup.md) for the verbs.
- **A frontend is one process; several may share a store.** Frontends over one Postgres
  store share it and none fences another (see
  [state-store-cutover.md](state-store-cutover.md)); a SQLite file is held by one process, so
  only one frontend can use it. A farm with one frontend schedules nothing while it is down;
  with several behind the proxy, the others carry on, and a frontend is updated by a roll
  ([kbf-updater.md](kbf-updater.md)).
- **A `storage-and-daemon` host is for a trusted network.** Its unit runs as the build user,
  which can read the cold store's key from the process's environment, as `all-in-one`'s user
  can read the farm's state. Hosts that must not share a user with a key run `daemon` and
  `storage` apart.

Linux build hosts need `crun`, subordinate ids and a delegated cgroup; see
[linux-build-host.md](linux-build-host.md). Storage hosts, their disks and memory are in
[storage-node.md](storage-node.md).

## Growing from one host to many

There is no verb that converts an all-in-one host into a farm. You add the farm's parts and
move the host's role, in this order. What the code supports is below; what it does not is in
the `Planned:` paragraph.

1. **The state store.** `all-in-one` defaults to `postgres:///kbf?host=/run/postgresql`,
   the database `kbf` on a local socket, reached as the process's own user. It also accepts
   `sqlite:///absolute/path` for this release only: a start on it works and logs a warning
   that SQLite is removed in the next release (see the upgrade notes). A frontend has no
   default and refuses to start without `--state-store`. A farm's frontend uses Postgres:
   `kbf-installer setup` with no `--database` provisions the role and database in one run
   (the database `kbf` and the role `kbf-server` on the local socket; the schema follows once
   the server admits the logins, which on a stock server means a second run after the printed
   lines are added), prints the `pg_hba.conf` and `pg_ident.conf` lines the logins need,
   refuses with the command that installs Postgres when `psql` or the server is missing, and
   never edits either file; a role you made by hand is passed with `--database`. Postgres must
   flush commits before it acknowledges them (`synchronous_commit` on) or the start is
   refused. A URL with a password is refused: the command line is readable by every user on
   the host, so the role is reached by the socket and peer authentication. The state store
   also holds the storage members, the enrollment registry and the object-store registration.
2. **The CA and certificates.** `all-in-one` already needs mutual TLS between its two halves.
   A farm needs one CA and one certificate per node, each with exactly one role (`frontend`,
   `storage` or `daemon`, as `urn:kbf:role:<role>`). The frontend runs the enrollment
   listener: `--enroll-listen`, `--ca-dir`, `--frontend-name` (repeatable) and `--ca-signer`
   (the socket of the `kbf-signer` process that holds the CA key; `--ca-dev-file-signer` is
   for development; a frontend that runs as another account than the signer needs the
   signer's `--client-group`, see the signer's socket in [api.md](../api.md)). The operator makes a token with `POST /v1/enroll/tokens`, and a node
   enrolls with it at its first start and renews at half of its certificate's life (7 days by
   default, `--enroll-cert-lifetime-secs`). A node host needs no hand-issued certificate. The
   table of what each certificate must contain is in [kbf-units.md](kbf-units.md).
3. **The cold store.** `file://` stays on one host only. Choose an `s3://` store every
   storage node can reach, register it with the frontend as above, and give each node the
   same `--cold-store` URI. A fresh prefix is a fresh cold store.
4. **The frontend.** Run `kbf --mode=frontend` on its own host, or behind the proxy (a pool of
   frontends given as `[[proxy.frontend]]` tables, written by hand: `kbf-installer setup` sets up
   no proxy). Bind `--listen` and `--worker-listen`
   to addresses the nodes reach, give them the TLS flags, and set `--storage` (required) to
   the first storage node, with `--storage-client-cert`, `--storage-client-key` and
   `--storage-ca` for the mutual TLS to it. Add `--api-listen` and `--api-token-file` for
   operators.
5. **The nodes.** On each host: `kbf-installer setup --mode=storage-and-daemon` (or `daemon`)
   with `--frontend-url` and `--token-file`. The node takes its flags from `kbf.toml` and the
   frontend's configuration: `--frontend` (the worker endpoint), `--advertise-addr`,
   `--storage-listen` on all addresses at the storage port, the three certificate flags,
   `--hot-store`, `--meta-dir` and, for a daemon, `--server`. `kbf --mode=<mode> --check`
   checks the host and exits.
6. **Promote the new members.** In a farm formed more than a minute ago, a node it has not
   seen before registers as `joining` and stays so until `kbf cluster member <id> --state
   active`, unless the frontend runs with `--auto-promote`. Promoting does not copy any blob
   to the member. Read the risk in the operator API's section on `POST /v1/members/{id}` in
   `docs/api.md` before you promote a joiner whose slice is not empty.
7. **Move the old host.** Stop `all-in-one` (see below), then start the same host as
   `storage-and-daemon`, or as `frontend`. Do not run `all-in-one` and a frontend on one host
   at once: both bind `--listen` and `--worker-listen` at the same defaults.

What changes in the flags: the frontend gains `--storage`, `--storage-ca` and its client
pair, `--enroll-listen` and its companions, and `--object-store-secrets-dir`, and loses
every flag of a store. The daemon moves to its own process and takes `--server` for the
worker listener and no `--cas` when it is in a `storage-and-daemon` process. The storage
half gains `--frontend`, `--advertise-addr`, `--storage-zone` and `--storage-weight`.

**Planned:** a conversion of the blobs, the action cache and the queue of an `all-in-one`
host into a farm. Today the farm starts with an empty cache and its own scheduler state;
in-flight work does not move, and the farm's new state store is given an epoch of its own,
so the daemons that reconnect see an epoch they did not know and kill the runs of the
leases they held before. Copying a joining member's slice to it
(the join handoff, architecture-v2.md section 8.4) is not built either.

## Stopping and draining

| mode | on SIGTERM or SIGINT | what bounds it |
|---|---|---|
| `frontend` | drains, by the sequence below | `--drain-lb-wait` (10 s by default); nothing else by default: the drain waits for every open execution, and `--drain-deadline` (none by default) only reports; to force a stop, a second signal or `--shutdown-timeout-secs` (0 by default: none) |
| `all-in-one` | drains both halves, in order: the server half refuses new work and finishes what it accepted, then the daemon half drains, and the server half stops after it | nothing by default: it waits for every execution and every lease, however long; a second signal, or `--shutdown-timeout-secs` (none by default), stops both at once; `--drain-deadline-secs` only logs a warning |
| `storage` | stops | none of its own |
| `daemon` | drains | nothing but the leases: it waits until all have ended (`--drain-deadline-secs`, off by default, only logs a warning); a second signal |
| `storage-and-daemon` | drains the daemon half, then stops storage | as `daemon` |

**The frontend.** Buck2 does not recover an Execute stream that breaks after it started, so
a stop that cuts a stream fails its action. On the first signal, or on
`POST /v1/frontend/drain` (`kbf cluster drain --frontend --token-file F`), the frontend
(which has a state store) takes itself out of any load balancer in five steps
([kbf-units.md](kbf-units.md#draining-a-frontend-behind-a-load-balancer)): `/readyz` turns to
503 while everything is still served; after `--drain-lb-wait` it refuses each new Execute with
`UNAVAILABLE` (which Buck2 retries), takes no more work from the state store, and sends
GOAWAY on the REAPI, worker and operator API listeners; it keeps every open Execute and
WaitExecution stream until its action is done, however long that takes, except a stream
whose operation was never started, that no worker on this frontend can run and that no
other frontend can take, which ends `UNAVAILABLE` at the end of the load balancer wait;
then it ends the worker streams and exits 0. There is no default deadline. A
`--drain-deadline` that is set (no upper bound) only reports when it passes with executions
open: a WARN with the count, and `deadline_passed` in `GET /v1/frontend/drain`; no stream is
ended, the process stays up, and the drain keeps waiting, whether a signal or the endpoint
began it. `GET /v1/frontend/drain` reports the phase. The way to force a stop is a second
signal, which skips the waits, or `--shutdown-timeout-secs` running out on a drain a
signal began; either ends the streams that are left `UNAVAILABLE`, and their builds fail. A frontend with no state store (a test setup) drains at once: `/readyz`
503, new Execute calls refused, GOAWAY on the REAPI listener, and exit 0 once the REAPI
connections have closed. Under systemd that needs `TimeoutStopSec=infinity`; the units that
`kbf-deploy` renders set it.

**`all-in-one`.** A stop drains both halves and abandons no lease. On the first signal the
server half does the plain drain of `kbf-server` (not the load balancer sequence of `kbf
frontend`: `--drain-lb-wait` and `--drain-deadline` are not flags of this mode): `/readyz`
turns to 503, new Execute calls are refused `UNAVAILABLE`, the REAPI listener is sent GOAWAY,
and the executions that are open run until their action is done. The daemon half keeps
taking leases meanwhile, since an operation the server accepted before the signal and has
not placed still has to run, and its leases report through the server half. When no
execution is open any more, the daemon half drains as `daemon` does: no new lease, the
running ones finish, report and are acknowledged, however long that takes. The server half
keeps serving the worker listener (the daemon's `Result`s and their acknowledgements, and
its blobs) until the daemon half has returned, and then stops; the process exits 0 when both
have. A drain begun with `POST /v1/frontend/drain` goes the same way.

There is no default deadline. A second signal is the hard stop of both halves: the server
half ends its open streams `UNAVAILABLE` and the daemon half abandons its running leases,
which the scheduler places again. `--shutdown-timeout-secs` (0, none, by default) is the
server half's own hard stop and ends the process the same way: when it runs out the server
half ends its streams and stops, and the daemon half stops with it, abandoning its leases.
`--drain-deadline-secs` is the daemon half's and, as on `daemon`, only logs one warning with
the number of leases still running (0 warns at once) and keeps waiting. Under systemd the
unit needs `TimeoutStopSec=infinity`, as `frontend`'s and `daemon`'s do.

**`storage-and-daemon`.** The signal handlers are installed before anything starts. On the
first signal the daemon half takes no new lease, lets the running ones finish and report, and
waits for their acknowledgements, however long that takes. `--drain-deadline-secs` is not
set by default; given, it only logs one warning with the number of leases still running
when that many seconds have passed (any value, no upper bound; 0 warns at once), and the
wait goes on. A second signal is the hard stop: it ends the drain at once and abandons the
leases still running. Only then does the storage half
stop, because a running action reads and writes its blobs through it, and the process exits
0 (kbf#1087). A signal while the storage half is still starting, for instance waiting for
a frontend that is down, abandons the start and exits 0.

**`daemon` and `storage`.** A daemon drains as above and then exits 0. A storage node serves
until the signal and stops.

A stop of a storage member that is not planned makes the frontend wait for the TTL before it
marks the member `down`; use `planned-down` first, as above. To empty a build node before
maintenance without stopping it, `kbf cluster drain <node>`.

## Related pages

- [kbf-units.md](kbf-units.md): each mode's packaged systemd unit and how it is checked.
- [kbf-installer-setup.md](kbf-installer-setup.md): `kbf-installer setup`, the state database,
  the proxy host and backup.
- [update-a-host.md](update-a-host.md): updating a host.
- [storage-node.md](storage-node.md): storage hosts, disks, memory and the cold store on a
  single host.
- [linux-build-host.md](linux-build-host.md): what a Linux build host installs and gives
  the daemon.
- [pack-adopt.md](pack-adopt.md): indexing the segments the storage members already hold into
  the pack index, in place.
- The operator API, with its gates, in [api.md](../api.md).
