# How kbf stores your build's data

This guide is for an operator who is choosing hardware for a kbf farm and wants to know
what survives which failure. It describes what the code does as of the kbf version this
page ships with. Anything designed and not built is in the closing paragraph that starts
"Planned:". For depth, read [the storage design](../design/storage.md) and
[the write path design](../design/write-path.md).

## What kbf keeps

kbf keeps two kinds of data for your build tool.

- The **content-addressable store (CAS)** holds blobs: source files, outputs, stdout and
  stderr, and the `Directory` and `Tree` messages that describe input and output trees. A
  blob is named by its SHA-256 digest and size. kbf checks every blob against its digest
  when it is written and when it is read, so it does not serve wrong bytes.
- The **action cache (AC)** maps an action's digest to the result of running it. A cached
  result is served only if every blob the result names is present and readable. If one is
  not, the lookup is a miss and the action runs again.

Clients never write the action cache. `UpdateActionResult` is refused with
`PERMISSION_DENIED`. Only the daemon that ran an action stores its result, and only after
all its outputs are stored.

Two facts follow for hardware. Blob bytes can be rebuilt from the cold store (below), but
action cache entries live only in the node's metadata log. A node that loses its log
loses its cached results, and those actions run again.

## Where the bytes live

Each storage node has up to three places that hold data. Each has its own flag or
directory.

| Place | What it holds | Flag | Put it on |
|---|---|---|---|
| Cold store | Every blob, packed into large segment objects. The durable home. | `--cold-store` | One shared S3-compatible bucket for a farm of several nodes; a directory on a large disk for a single box |
| Hot store (the write tier) | Recent uploads, until they are flushed to the cold store. Optional. | `--hot-store` | A local SSD or NVMe, as a `file://` directory |
| Metadata log | The index (digest to segment and offset) and the action cache. | `--meta-dir`, or `<hot store>/meta` | A local disk that nothing else fills |

A store URI is `file:///absolute/path` or
`s3://bucket/prefix?endpoint=http://host:port`. The hot store must be a `file://` directory.
A `file://` directory on a tmpfs is a store in RAM; kbf has no `memory:` store.

The hot store's size is not a flag. Its budget is 80% of its file system's free space after
the floor below is kept free (the floor is never more than half of what is free, so a nearly
full disk gets a small budget, not none), plus the bytes the tier already holds, and lowered
by `--hot-store-max-bytes` if you set it. When the
hot store is at its budget, writes skip it and go straight to the cold store. If the cold
store is down too, those writes fail with `UNAVAILABLE`.

Without a hot store, uploads go straight to the cold store and a farm on one disk can name
that disk as its cold store.

A farm of more than one storage node uses one shared S3 bucket as its cold store. A
`file://` cold store is for a single box: a directory is locked to one process, so two
nodes cannot share it.

## Storage members and their states

A storage node started with `--frontend` registers with a frontend and becomes a **member**
of the storage map. The frontend assigns the member an id and a boot counter, and keeps a
map: a generation number, and the members with address, zone, weight and state, and the
replication factor. `GET /v1/members` lists the generation and, for each member, its id,
address, state, weight and protocol. A node started without `--frontend` is a
one-member farm of itself.

| State | Takes writes | Holds the data it owns | Read |
|---|---|---|---|
| `joining` | yes, but not as the first owner while a settled member can be | no | last |
| `catching_up` | yes, but not as the first owner while a settled member can be | no | last |
| `active` | yes | yes | in rank order |
| `draining` | no; its successor takes its place at once (see the note below) | yes | in rank order, before its successor |
| `down` | it keeps its place for the dwell; writers skip it and record a repair debt | yes | never listed |
| `unknown` | no | not known | never listed |

A state a later build wrote that this one does not name shows as `unknown`. The member
stays in the map and is left out of placement. A heartbeat rewrites its row with the state
it had; registering again, or setting a state by hand, replaces it.

A joiner is first owner only when the farm has no settled member, as in a farm that is
just forming, and then the best-scored joiner is the primary. With a replication factor of
1 a joiner owns nothing.

A drain needs at least three members. A chain with fewer write owners than the
replication factor is refused, and a draining member is not a write owner. With two
members, draining one leaves one write owner for a factor of 2, so every write answers
`UNAVAILABLE` until the drain ends.

Which state a node gets:

- Nodes that start within a minute of each other when a farm is formed are `active` at
  once.
- A node that registers later, in a farm that already holds data, is `joining`. It stays
  `joining` until an operator runs `kbf cluster member <id> --state active --token-file F`,
  or until the frontend runs with `--auto-promote` and the joiner has filled. Nothing copies
  blobs that existed before the node joined to it by default, so promote it when that is
  acceptable. With `--auto-promote`, the frontend waits until the joiner has indexed every
  object in the cold store and every member has flushed what it staged before the join,
  and then the joiner's own heartbeat promotes it. The joiner is filled from the shared cold
  store, not from other nodes. `--handoff-settle-secs` (30 by default) is how long all the
  conditions must keep holding, and `--handoff-allow-degraded` lets a promotion go ahead
  although a member was down during the fill. While `--auto-promote` holds a joiner,
  `GET /v1/members` shows a `blocked_on` list for it that says what it waits for.
- A node whose metadata log has not finished rebuilding its index registers as
  `catching_up`, unless it is a node the farm has not seen before, joining a farm that holds
  data: that node is `joining` whatever it says about its index. A `catching_up` node
  serves from the start, rebuilds in the background, and becomes `active` on its first
  heartbeat after the rebuild is done.
- A member whose heartbeat lapses for `--member-ttl-secs` (15 by default) becomes `down`.
  It stays in the map for `--member-dwell-secs` (300 by default). After the dwell it leaves
  the map and its share of the digests moves to the other members.

## Placement and replication

kbf places a blob with weighted rendezvous hashing. Every member gets a score for the
blob's digest that does not depend on the other members, and the owners are the members
with the highest scores. Adding or removing a member moves only the digests that member
wins. A member's share is set by `--storage-weight` (default 1). `--storage-zone` records a
zone label; placement does not use it yet.

The **replication factor** is 2: each blob has two owners, the first called the primary.
The factor is clamped to the number of members, so a one-node farm keeps one copy. It is
not a flag in this build.

A write is a **chain**. The writer sends the blob once, to the primary. The primary streams
it on to the second owner. The second owner checks the digest and stores it first, and only
then does the primary's own copy count as present. The write is acknowledged after both
hold it.

With `--ram-ack-bytes` over 0 (off by default) the acknowledgement comes earlier for a blob
of at most 4 MiB: both owners hold it verified in RAM, the second owner first, and each
writes it to its disk afterwards ([What is durable when](#the-write-tier-what-is-durable-when)).

If the second owner cannot be reached, the primary writes a **repair debt** to its journal
before it acknowledges, so the write succeeds with one copy and a durable record that a
second copy is owed. That write is acknowledged from the primary's disk, never from RAM. A repair worker makes the copy when the owner returns. A blob present
on a primary therefore has either a second copy or a durable debt. If the primary crashes
in between, the blob is absent and the writer writes it again.

Copying between nodes needs mutual TLS. A node served over plain text, which is allowed on
loopback only, judges calls by the map but copies nothing.

Reads try the owners in rank order, then the previous generation's owners, so a blob
written before a join or a drain is found where it was. A read is `NOT_FOUND` only if every
owner says it does not hold the blob. If an owner could not answer, the read is
`UNAVAILABLE`. A read that finds a blob on only one owner does not copy it to the other;
only a repair debt does.

`GET /v1/storage` reports `under_replicated_blobs`, the number of blobs that are owed a
second copy. It is `null` (unknown) unless every member is `active` and has reported
recently. A gate must treat `null` as not zero.

## The write tier: what is durable when

Without a hot store, a blob is acknowledged after its segment is written to the cold store,
its footer is read back, and its location is committed to the metadata log. The blob is as
durable as the cold store.

With a hot store, the write path changes:

1. The upload is written to a block file on the hot store, which is synced, together with
   its directory entry.
2. Its location is committed to the metadata log, which is synced. The blob is marked
   **staged** and the client is answered.
3. A block is **sealed** after 30 seconds, or when it reaches the segment limit of 128
   MiB. The flusher runs once a second. It re-reads every record, checks its SHA-256, writes the
   segment to the cold store, reads the footer back, and commits that the object is
   flushed. Only then is the blob in the cold store.
4. A flushed block stays on the hot store as a read cache until space is needed. Only
   flushed copies are evicted. (Under `kbf all-in-one` the one cache keeps no flushed copies
   on the hot store, so there this step does not hold.)

| Moment | What survives |
|---|---|
| After the answer, before the flush | A crash of kbf, a restart, and a power loss of the host. The block and the log are synced. |
| After the answer, before the flush | **Not** the loss of the hot store's disk. With replication 2 and a second member, the second owner holds a copy; on one node, the blob is lost. |
| After the flush | Loss of the hot store. The blob is in the cold store, and the cold store's own durability applies. |
| After the answer, before either owner has written the blob (`--ram-ack-bytes` over 0 only, a blob of at most 4 MiB) | The loss of **one** owner, or of its process: the other holds the blob in RAM, serves it, writes it to its disk, and owes the lost one a copy. **Not** the loss of both owners' RAM (both processes, or both hosts losing power) before either has written it: the blob is gone, a read is `NOT_FOUND`, the writer uploads it again, and no action-cache entry names it. |

With `--ram-ack-bytes` over 0, a node holds the blobs it acknowledged from RAM until its
drain has synced them, which takes milliseconds when the disk keeps up:
`kbf_storage_ram_unwritten_bytes` is the bytes still in RAM and
`kbf_storage_ram_unwritten_oldest_seconds` the age of the oldest. A full budget does not
fail a write: it is written to the disk before it is acknowledged, counted in
`kbf_storage_ram_writethrough_total{reason="budget"}`. A node that stops drains what it
holds within `--ram-drain-deadline`; `SIGKILL` and a power loss do not.

The flush starts at most about 31 seconds after the block is opened (the 30-second seal
delay plus the next once-a-second flusher round; a block that reaches 128 MiB is sealed
sooner), then takes one put, and longer while the cold store is down. No flag changes these
timings. `kbf storage` and `kbf storage-and-daemon` flush the tier when they stop, after
their calls end, within `--ram-drain-deadline`; what the deadline leaves is flushed by the
next start. `kbf-server` and `kbf-all-in-one` do not flush it at shutdown. A block left on
the hot store holds acknowledged bytes the cold store does not have yet, so do not delete a
hot store you have not drained.

A hot store carries a `TIER` file. If it is missing or names another tier while objects are
staged on this node, the node refuses to start, so that a wrong mount does not destroy the
index. `--hot-store-abandon=<id>` (the id is in the refusal) is the way out when the disk
is really gone. See [Loss marks](#loss-marks-and-unreachable-blobs).

Every node flushes what it staged. At replication factor 2 both owners of a blob put it
into the cold store, as two objects, so a shared bucket holds each blob twice and you
should size it, and its cost, for twice the data.

## The cold store

The cold store is where every blob ends up. kbf names every object key and records every
location in its own metadata, so it needs no listing, conditional write or read-after-write
guarantee from the store.

**Filesystem (`file://`).** One file per object. A write goes to a temporary file, is
synced, is linked into place, and the directory is synced before the write is answered. The
directory is locked while a store is open on it, so a second server on it is refused. It is
the store of record: nothing sits behind it, so an object it evicts is gone.

**S3-compatible (`s3://`).** MinIO and RustFS are the stores kbf's CI runs against. kbf
signs requests with SigV4 and uses path-style addressing. Put the endpoint in the URI query
(`endpoint=` is required). Other query keys are `region` (default `us-east-1`),
`conditional-put=true` for a store that refuses to overwrite a key, and
`min-put-rate=BYTES_PER_SEC`. The key pair comes from the JSON file of
`--cold-store-credentials-file` (mode 0600 or 0400), never from the URI; `AWS_ACCESS_KEY_ID` and
`AWS_SECRET_ACCESS_KEY` still work when no file is named, deprecated. An object over 16 MiB is uploaded as a
multipart upload and aborted on any failure. The store must be reached over `http`;
`https://` endpoints are refused in this build.

**Registering the store through the operator API.** An operator can give the farm's keys to
the storage nodes through the frontend instead of through each node's environment. The
frontend must be started with `--object-store-secrets-dir DIR`, `--api-listen`,
`--api-token-file` and `--state-store`; without them every route answers `404`.

1. `PUT /v1/object-stores/connections/{name}` creates a connection: an endpoint and a key
   pair. The secret is kept in a file under `DIR`, mode 0600, and never in the state store.
2. `PUT /v1/object-stores/bindings/cold` binds the role `cold` to the connection, a bucket
   and a prefix.
3. `GET /v1/object-stores` shows the desired state and which nodes have applied it.

`kbf cluster object-store ...` calls the same routes. Every route needs the bearer token,
reads included, and the calls must come from the frontend host (see Planned restarts).
The registration only supplies keys: a node still names its store with
`--cold-store`, and the registered store must be the same endpoint, bucket and prefix, or
the node keeps its flag and reports `identity_mismatch`. A node with a `file://` cold store
never takes the registration. A node reads the registration at its start, so rotating a key
reaches a running node at its next restart. [api.md](../api.md) has the
bodies, the error codes and the rotation steps.

## Free-space floors

kbf keeps free space on each disk it writes to. The floor is 5% of the file system, and at
least 1 GiB. The floor is not configurable.

- A write to a `file://` cold store that would leave less free than the floor is refused,
  and the upload is answered `UNAVAILABLE`.
- The refused write also wakes the evictor, which looks every 10 seconds as well. While
  less than twice the floor is free, it forgets the least recently used objects and deletes
  their files until twice the floor is free. It never takes an object used in the last 60
  seconds or an object still staged on a hot store. The client finds out as it would for
  any lost blob: `FindMissingBlobs` reports it missing and the client uploads it again, or
  an action cache entry that needed it is a miss and the action runs again.
- An S3 cold store is never evicted from.
- The hot store's budget is computed from its free space after the same floor (capped at
  half of what is free, as above).

Size a `file://` cold store for the cache you want to keep. Eviction cuts the seven days a
blob is promised after a hit down to 60 seconds, so a download that comes later than that
after a hit can fail with `NOT_FOUND`.

## Loss marks and unreachable blobs

A blob can be one of three things to the metadata: present, **unavailable** (the index
holds it but its object cannot be read), or absent. Only absent becomes `NOT_FOUND`. kbf
never turns "I could not read it" into "it does not exist". `FindMissingBlobs` lists an
unavailable blob as missing so the client uploads it again, and the new upload repairs the
index.

A **loss mark** is made when a staged object's only copy was on a hot store that is now
gone or wrong: the tier was abandoned with `--hot-store-abandon`, the block was missing or
short at a start, or a record failed its digest before the flush. The blobs in that object
are counted as lost. They answer as unavailable, an action cache entry that names one is a
miss, and a re-upload clears the mark. A mark on an object that was already flushed is not
a loss, because the cold store can answer again. `kbf_cache_lost_blobs` shows the blobs
that are lost now, and `kbf_cache_blob_losses_total` counts every blob ever marked.

**When every owner of a blob is down,** the router has nobody to ask. The read fails with
`UNAVAILABLE` with the message "every owner of `<digest>` is down", where `<digest>` is
the blob's hex digest, and the client sees a failure for that
blob. kbf does not answer `NOT_FOUND` and does not invent a copy. The blob is unreachable
until an owner returns and heartbeats. A build tool that retries on `UNAVAILABLE` waits; one
that does not fails the action. A member that has been down for longer than the dwell
leaves the map and its digests move to other members. A blob that was flushed is in the
cold store; a blob that was staged and not yet flushed, and whose owners are gone, is not
recoverable from it.

## Per-node metadata logs

Each node keeps its metadata in a Raft log on its local disk, in `--meta-dir` or in
`<hot store>/meta`. The log has **one voter**: the node itself. Every commit is synced
before it is answered. There are no snapshots yet, so the log is never compacted and it
grows with every commit. A second server started on the same directory is refused.

The log has a **machine version**, which an earlier build used to say which commands it
accepted. This build never raises it: a new log is at version 1 and stays there, and every
command this build knows commits to a log at any version. Formats change additively, so a
new command is written as it is, and a build that does not know it skips it. A log an
earlier build raised keeps the version it reached, and this build applies it as before:

| Version | Added | Raised, by an earlier build, at |
|---|---|---|
| 2 | The write tier's commands | A start with a hot store |
| 3 | `EvictObjects` | The start-up pass or the evictor on a `file://` cold store, on a single-voter log |
| 4 | Node writers: objects named by member and boot | A node registered with a frontend |
| 5 | `ReseatStaged`, which moves a conflicting staged object | A start with a hot store |

A binary that does not run the version of a raise entry in a log skips the entry and keeps
the version the log had reached before it. Decoding does not depend on the version, so it
applies every command it knows and skips any command it does not know (one that a later
build added). An entry that is not a well-formed message still refuses the log. What a
skipped entry did is lost to that binary, so **rolling back to an older build loses nothing
only if the older build knows every command the log holds.** A build that predates skipping
refuses the log at the first entry it does not know, so it cannot be rolled back to once
the log holds one. `kbf --version --json` prints `meta_log`, the highest version a build
runs. It is informational: it is frozen at 5 and no longer changes with a release.

To roll back to a build that predates skipping, or past a raise an earlier build wrote, restore
a copy of the metadata directory that was taken before it. A byte copy of every file
in the directory, taken while the server is stopped, is a valid backup.
`kbf-deploy linux meta-backup` takes one and `meta-verify` checks it; see
[the storage node guide](../deploy/storage-node.md#metadata-backup-and-restore). A backup is
refused unless the write tier has nothing unflushed. Never start a restored copy against a
live server's object store: it shares the log's id and its object-key epochs.

Losing the log of a node that has a frontend does not lose the blobs. The node registers as
`catching_up` and rebuilds its index by listing the cold store and reading each segment's
footer. It answers `UNAVAILABLE`, never `NOT_FOUND`, for a blob it has not indexed yet. The
action cache is not rebuilt. On a single server with no frontend there is no rebuild: lose
the log and the blobs are unreachable.

## Planned restarts

To restart a storage node without making writers wait on it, set it `planned_down` first.
The call needs the operator token and goes to the frontend's operator API. Run it on the
frontend host: the write gate accepts only a loopback peer with no `Origin` header, the
bearer token and `Content-Type: application/json`.

```sh
curl -X POST \
  -H "Authorization: Bearer $(cat /etc/kbf/api-token)" \
  -H 'Content-Type: application/json' \
  --data '{"state": "planned_down", "for_secs": 900}' \
  http://<api address>/v1/members/3
```

`kbf cluster member 3 --state planned-down --for-secs 900 --token-file F` does the same.
`for_secs` is 1 to 86400 and defaults to 900.

- Without it, a stopped node is `down` in the map only `--member-ttl-secs` (15 by default)
  after its last heartbeat.
  Until then, a write whose primary is the stopped node dials it first, and an action cache
  commit is refused `UNAVAILABLE`.
- With it, the member is `down` in a new map generation at once. Routers, chain primaries
  and repair workers that hold the map send writes to the other owner and read the member's
  blobs from it.
- It works only on an `active` member; any other state answers `409`.
- The member keeps serving while it runs. After `for_secs` its next heartbeat returns it to
  `active`, so a restart that never came does not leave a healthy node out of service.
- When the node registers again after the restart it is `active` if its index is whole, or
  `catching_up` while it rebuilds. `{"state": "active"}` undoes the plan.
- If it never returns, it leaves the map at the end of the dwell, like any `down` member.

After `planned_down` the node is no longer `active`, so `under_replicated_blobs` stays
`null` for the whole restart. Read `GET /v1/storage` instead, and stop the node only when
all of these hold:

- `not_active` lists only this node, and `silent` and `unreported` are empty.
- `debts_total` is `0`. It sums the debts of the `active` members, so it does not include
  this node.
- In `nodes`, this node's `repair_debts` and `open_writes` are both `0`, and its
  `generation` is at least the generation that stopped routing writes to it. A `null` is
  unknown, not zero.

## Planned

Planned: more than one voter per metadata log, with snapshots, so a lost disk does not lose
the index. Planned: scheduled garbage collection, so blobs expire by `min_ttl` (7 days) and
the action cache by `action_ttl` (30 days); no collection runs today, so nothing expires
except by eviction on a `file://` cold store. Planned: a drain that finishes by itself, where
a draining node drops a blob only after its new owner confirms it, described in
[rebalance-handoff.md](../design/rebalance-handoff.md); today a draining member keeps its
data until an operator removes it. Planned: only the first owner of a blob flushes it, and
the second owner drops its copy once the flush is confirmed. Planned: zone-aware
placement. Planned: a replication factor that is a flag. Planned: `https://` endpoints for
the S3 cold store. Planned: zstd compression, and serving large blobs as content-defined
chunks.
