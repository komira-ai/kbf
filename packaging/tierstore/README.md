# tierstore: an example unit

`tierstore` (`crates/tierstore`) is an S3 server over a hot and a cold tier of S3-compatible
stores: it writes new objects to the hot tier, copies them to the cold tier in the background, and
frees the hot tier when it fills. It is a release asset for Linux only, as a tarball with its
SBOM and its provenance bundle (`docs/artifacts.md`); there is no package and no container image.
These files go beside the binary.

| file | what |
|---|---|
| `tierstore.service` | the unit: the binary as `tierstore`, an account that is not root, a two-tier S3 setup, a stop that waits for the drain, and the hardening of the other example units |
| `tierstore.conf` | the sysusers.d entry of the `tierstore` account (no login, no home) |

The values in the unit are examples (`192.0.2.10`, `s3-hot.example`, the bucket names, the
sizes): edit them to the setup's. `crates/tierstore/tests/packaging.rs` reads these files and
checks that the command line parses as the binary parses it, that no secret is on it (every key
and credential flag names a file), that the stop timeout covers the drain, and that nothing
starts the unit by itself.

## Placing the binary

The release carries `tierstore-<commit>-linux-x86_64.tar.gz` and
`tierstore-<commit>-linux-arm64.tar.gz`, each with `<tarball>.sigstore.json` and listed in
`SHA256SUMS`. Take the tarball of the host's architecture by its digest, and verify it as every
tarball is verified (`docs/artifacts.md`, "Provenance bundles: verifying a tarball offline").
The tarball holds one member, the binary under its own name:

```sh
tar -xzf tierstore-<commit>-linux-x86_64.tar.gz tierstore
install -d -m 0755 /usr/libexec/kbf
install -m 0755 -o root -g root tierstore /usr/libexec/kbf/tierstore
```

Nothing updates it afterwards. The host updater (`kbf-updater`, `kbf-host-install`) does not
manage `tierstore` yet: whoever runs the setup places the next binary the same way and restarts
the unit.

## Placing the unit

```sh
install -m 0644 tierstore.conf /etc/sysusers.d/tierstore.conf
systemd-sysusers /etc/sysusers.d/tierstore.conf
install -d -m 0750 -o root -g tierstore /etc/tierstore
install -m 0644 tierstore.service /etc/systemd/system/tierstore.service
systemctl daemon-reload
```

Then write the four key files, each owned by `tierstore` with mode 0600 or 0400 (the process
refuses a key file that its group or others can read):

- `/etc/tierstore/access.key`: the full key, the one clients that write sign their requests to
  `tierstore` with (`--access-key-file`). It may do everything the server serves: put, get, head,
  list, delete, and the calls of a multipart upload;
- `/etc/tierstore/daemons.key`: a read-only key (`--read-only-key-file`), for the daemons that
  read blobs and must not write or delete them. A request signed with it may get and head an
  object, list the bucket (objects, and the open multipart uploads) and head the bucket; a put, a
  delete, CreateBucket or any call that starts, feeds, completes or aborts a multipart upload is
  answered 403 `AccessDenied`, before its body is read, and changes nothing.
  Give the flag once for each read-only key, to give each reader a key of its own. The access key
  ids of all the keys of one server must differ: a request names its key by its id, so a start
  with two keys that have one id is refused with exit status 2;
- `/etc/tierstore/hot.credentials` and `/etc/tierstore/cold.credentials`: the keys `tierstore`
  signs its requests to the two stores with (`--hot-credentials-file`,
  `--cold-credentials-file`). The two tiers may be two stores, and have a key each.

Every one of these files holds its key in one of two forms, and the process tells them apart by
the first byte of the file that is not white space: `{` is JSON, anything else is two lines.

```json
{"access_key_id": "AKIDEXAMPLE", "secret_access_key": "the secret"}
```

or, as two lines, the access key id and then the secret access key:

```text
AKIDEXAMPLE
the secret
```

The JSON form is the one that the pack flags of the frontend and of the daemon read
(`--pack-store-credentials-file`), but `tierstore` is stricter than they are: it needs exactly
the two fields, both non-empty strings, and refuses a file with another field, with a field
misspelled or with an empty value. So one file holds both the key that a daemon signs with and
the key that `tierstore` checks only if it has exactly those two non-empty fields; a pack-flags
file that has another field must be trimmed to be shared. `tierstore` takes the values of the
JSON form as they are, without trimming. No message of the process carries a secret.

A role belongs to a key, not to a prefix: a read-only key may read every object of the bucket. A key limited to prefixes is a GC key or a scoped key (below).
A write limited to one object under a prefix is a presigned PUT URL (below).

## Presigned PUT URLs

A client that holds a presigned URL writes one object with a plain `PUT` of the URL: the server
checks SigV4 query-string authentication as the AWS documentation has it (`X-Amz-Algorithm`,
`X-Amz-Credential`, `X-Amz-Date`, `X-Amz-Expires`, `X-Amz-SignedHeaders`, `X-Amz-Signature`;
payload `UNSIGNED-PAYLOAD`; `host` signed), so any SDK presigner or `tierstore::server::presign_put`
makes one. Only a presigning key (`--presign-key-file`, once per key, mode 0600) can sign a URL; the
full and read-only keys cannot, and a presigning key is refused for any request signed in the
header. The file holds the key and its ceiling, all required (a file without one is refused at
start):

```json
{"access_key_id": "pack-presign", "secret_access_key": "the secret",
 "ceiling": {"prefix": "cas/b/", "max_expires_secs": 600, "max_object_bytes": 134217728}}
```

A URL is good for one `PUT` of a key that starts with the prefix (after the path is decoded: no
`..`, no `.` segment, no control character, never `_journal/` or `_tiering/`), with an
`X-Amz-Expires` of at most the smaller of `max_expires_secs` and `--presign-max-expires` (default
15m), and a body of at most `max_object_bytes` (5 GiB at most, the S3 single-PUT limit; each key file chooses its own), checked on
`Content-Length` and on the bytes received (and held to `--max-request-bytes`, 256 MiB by default, too: a key with a larger
cap needs that raised, and the smaller of the two applies; the body is streamed to the tier and
never held whole). It is refused from `X-Amz-Date` plus `X-Amz-Expires` on, with no grace. The
object appears only after a complete body, a second `PUT` of the key is `412`, and no multipart
call, `If-Match`, read, list or delete is allowed. The signature does not cover the body, so the
reader of the object must check its content. The query string is never logged.

A secret is never on the command line or in the environment: the process reads no environment
variable, and the unit sets none.

## GC keys

A GC key (`--gc-key-file`, once per key; JSON of `access_key_id`, `secret_access_key` and `prefix`,
mode 0600, not a symbolic link, owned by the user the server runs as) is for the process that
collects the objects that presigned PUTs wrote. It signs in the header only (a presigned URL signed
with it is refused) and may do these, only for a key that starts with its prefix (which ends in `/`)
and is longer than it:

| call | allowed |
|---|---|
| ListObjectsV2 | with a `prefix` inside the key's; a list with no prefix or another is 403, not filtered |
| ListMultipartUploads | with a `prefix` inside the key's, the open uploads under it; no prefix or another is 403, not filtered |
| HeadObject | yes |
| DeleteObject | yes: from both tiers, journaled as for any delete |
| AbortMultipartUpload | of an upload of such a key |
| GetObject | no: a GC key never reads a body |
| put, part, start or completion of an upload, copy, CreateBucket, HeadBucket | no |
| `_journal/`, `_tiering/`, any key outside the prefix, the prefix itself | no |

A call the key may not do is refused with 403 `AccessDenied` without its body being read. The
refusals are made in this order: the signature and key id, the method, the decoding of the path and
query (400, or 404 for an unknown bucket), the checksum headers (400 or 501), and last the key or
list prefix; so a call that is both refused and malformed may get the malformed-request error. A delete by a GC key is not
create-only; it is the only way an object leaves the prefix that presigned PUTs write.

## Scoped keys

A scoped key (`--scoped-key-file`, once per key; mode 0600, not a symbolic link, owned by the user
the server runs as) reads, creates and deletes under prefixes that its file lists, and does nothing
else. It is for a process that must touch some prefixes of the bucket and not the rest, and that
neither replaces nor deletes what it did not write. It signs in the header only (a presigned URL
signed with it is refused). The file is JSON of exactly these five fields, all required:

```json
{"access_key_id": "frontend-packs", "secret_access_key": "the secret",
 "read_prefixes": ["cas/"],
 "write_prefixes": ["cas/f/frontend/"],
 "delete_prefixes": []}
```

Every prefix ends in `/` and is not empty. None may be `_journal/` or `_tiering/`, be under one, be
a prefix of one, or have one as a segment (`cas/_journal/`); a file that has one is refused at
start. A list may be empty, but a key whose three lists are all empty is refused, since it could do
nothing. The lists may overlap: a prefix in `read_prefixes` and in `write_prefixes` is one the key
reads and creates under. A key file that is refused (bad JSON, a missing or unknown field, a bad
prefix, a mode other than 0600) exits 1 at start with a message that names the flag and the file and
never the secret, as a bad GC key file does. The access key id differs from those of all other
keys: two keys with one id are refused with exit status 2 (the command line does this; a
`ServerConfig` built in code, as a library user does, is not checked for distinct ids by the server).

A call is allowed only for a key (or, for a listing, a `prefix` parameter) that starts with a prefix
of the list the call needs, after the path is percent-decoded and with no `..`, no `.` segment and
no control character in it. A key must be longer than the prefix: the prefix itself is not an object.
A key or a listing `prefix` that has `..` anywhere in it, even inside a prefix, is refused.

| call | list | allowed |
|---|---|---|
| GetObject, HeadObject | `read_prefixes` | yes, whole or by range |
| ListObjectsV2 | `read_prefixes` | with a `prefix` parameter inside a read prefix; no prefix or another is 403, not filtered |
| ListMultipartUploads | `read_prefixes` | the same rule: a `prefix` inside a read prefix |
| PutObject | `write_prefixes` | create-only: no `If-Match`, and `If-None-Match` absent or `*`; a key that exists is `412 PreconditionFailed` |
| CreateMultipartUpload, UploadPart, CompleteMultipartUpload | `write_prefixes` | of a key inside a write prefix, each call checked |
| AbortMultipartUpload | `write_prefixes` | of an upload of such a key (a delete prefix does not allow an abort) |
| DeleteObject | `delete_prefixes` | from both tiers, journaled as for any delete |
| a put with `If-Match` or any other `If-None-Match`, a copy (`x-amz-copy-source`, also on a part) | none | no: a scoped key never replaces or copies |
| the calls on the bucket (CreateBucket, HeadBucket, delete) and the root, any other method | none | no |
| `_journal/`, `_tiering/`, any key outside the list the call needs | none | no |

The lists are not interchangeable: a write prefix does not let the key read what it wrote (add the
prefix to `read_prefixes` too), and a read prefix does not let it write. A key with a write
prefix and no read prefix can still learn that a key exists, by the `412` of a put over it. A
call the key may not do is refused with 403 `AccessDenied` without its body being read. The refusals are made in the order
of the GC key's (see above): the signature and key id, the method, the decoding of the path and
query, the checksum headers, and last the key, the listing prefix and the conditions of the put; so
a call that is both refused and malformed may get the malformed-request error.

Two uses, for the pieces of kbf that keep their objects in a tierstore bucket:

- **The frontend's pack store** (`--pack-store-endpoint`, with `--pack-store-credentials-file` set
  to a file in the JSON form of the key above). A frontend reads packs that daemons and tasks
  wrote under `cas/` and writes its own packs under `cas/f/<writer>/`, where `<writer>` is its
  `--pack-writer` (`frontend` if left out), one key `cas/f/<writer>/<boot>/<sequence>` for each
  pack. It writes with a plain `PUT` with `If-None-Match: *`, one request for a pack, and reads
  with ranged `GET`s. So the key is `read_prefixes: ["cas/"]`, `write_prefixes:
  ["cas/f/<writer>/"]`, `delete_prefixes: []`. With no delete prefix, two best-effort deletes the
  frontend makes are refused with a logged warning and change nothing: the removal of its
  create-only self-test object (`cas/f/<writer>/<boot>/selftest`, one small object left for each
  start of the frontend) and of the object of a pack whose write failed. To have both removed,
  give `delete_prefixes: ["cas/f/<writer>/"]`: the key then deletes only what the frontend itself
  wrote.
- **Build history** (`kbf-server --history-objects
  's3://<bucket>/kbf-history?endpoint=...&conditional-put=true'`, the key
  pair in `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY`). The history objects are under
  `kbf-history/history/`. The writer makes only create-only single `PUT`s with `If-None-Match: *`
  (without `conditional-put=true` it is a plain `PUT`, and tierstore answers a retry of a chunk
  whose first reply was lost with `412`, which the writer does not take for "already there", so it
  retries the chunk for good: keep `conditional-put=true`) and the reader makes only ranged
  `GET`s; neither lists, deletes, overwrites or starts a multipart upload (the code makes no list or
  delete call at all, so the expiry of old objects is for a lifecycle rule of the bucket, not the
  server, and a sweeper that deletes would need `delete_prefixes: ["kbf-history/"]`). So the key is
  `read_prefixes: ["kbf-history/"]`, `write_prefixes: ["kbf-history/"]`, `delete_prefixes: []`.

## The configuration file

`tierstore --config <path>` reads one TOML file in place of the flags that describe the data path
(the listen address, the tiers, the key files, the limits and the reconciler's settings). A server
started from a file runs the code of one started from the same settings as flags: the file is read
into the settings the flags make, and the defaults are the flags' defaults. The unit above keeps
its flags; a file is for a setup that would rather keep its settings in one place.

A file holds no secret, and a message about it repeats no value except a `[[policy]]` prefix (a prefix is
a key name, not a secret): a syntax refusal names the line, the column and the key; any other refusal names
the key. A number above 9223372036854775807 (the largest TOML integer) cannot go in a file; as a flag it
is accepted, and `--print-config` refuses it with exit 2. A `--print-config` that cannot write its output
(a closed pipe, a full disk) exits 1; a refusal of the file or the flags exits 2.

A file has one of two shapes, told apart by its tables: a storage node (`[server]`, `[hot_store]`
and `[cold_store]`, as the flags are, and an optional `[oci]` (the read-only registry front) and
`[[policy]]`) and a build node (`[server]`, `[local_cache]` and `[cold_store]` with no
`[hot_store]`, an optional `[oci]`, and `[[policy]]`), which is a read cache and has no flag form (see
"A build node" below). Both shapes take the same `[oci]` table. A file for a storage node:

```toml
[server]
listen              = "192.0.2.10:9000"
proxy_id            = "tierstore-1"
bucket              = "tierstore"
region              = "us-east-1"
max_request_bytes   = 268435456
access_key_file     = "/etc/tierstore/access.key"
read_only_key_files = ["/etc/tierstore/daemons.key"]
presign_key_files   = ["/etc/tierstore/pack-presign.key"]
gc_key_files        = ["/etc/tierstore/pack-gc.key"]

[hot_store]
kind              = "s3"
bucket            = "tierstore-hot"
endpoint          = "http://s3-hot.example:9000"
region            = "us-east-1"
credentials_file  = "/etc/tierstore/hot.credentials"
budget            = "1TiB"
high_water        = 80
low_water         = 70
critical          = 95
copy_parallel     = 16
lease_ttl         = "30s"
reconcile_interval = "5s"
usage_stale_after = "2m"
journal_linger    = "20ms"
journal_retention = "24h"

[cold_store]
kind             = "s3"
bucket           = "tierstore-cold"
endpoint         = "http://s3-cold.example:9000"
credentials_file = "/etc/tierstore/cold.credentials"
skip_checks      = []
```

A key that is left out has the default of its flag (`tierstore --help` lists them); `listen`,
`proxy_id`, `bucket` and `access_key_file` of `[server]`, `kind` of each tier and `budget` of
`[hot_store]` are needed. Durations are a whole number and `ms`, `s`, `m` or `h`, as in the flags.
The water marks are percents, as in the flags (1 to 100).

### A build node

A file with `[local_cache]` and `[cold_store]` and no `[hot_store]` runs a read cache in front of
one upstream: typically an S3 endpoint that is the tierstore of a storage node, read with a
read-only key. It has no journal, no lease and no reconciler; the only things that run beside the
front are the fills that a miss starts and the eviction pass. It is not a tier that is written to:
every call but GET and HEAD (a put, a part, the start or completion of an upload, a delete, a copy,
CreateBucket) is refused 403 `AccessDenied` with a message that says this is a read cache, before
the body is read. The key files of `[server]` are those of the front, as for a storage node.

With `write_through = true` in `[local_cache]` (default `false`) the node takes the writes its keys
may do and sends each to the upstream; the answer is given only after the upstream has the object,
and a create-only put under a `cache = true` policy is also kept on the node, so the next read of
it is a hit (a multipart upload is not kept: the first read fills it; see "What a put keeps"
below). Create-only
puts (with or without `If-None-Match: *`), presigned PUTs, multipart uploads (start, part,
completion, abort) and deletes go to the upstream; the roles of the full, read-only, scoped,
presigning and GC keys apply as on a storage node, and a put that replaces (`If-Match`) and a copy
are still refused. A create-only put makes one HEAD of the key at the upstream before the body is
read, and answers 412 at once if the object is there; the upstream's conditional put is the
arbiter of two writers. A full upstream (507) is 503 `SlowDown` with `Retry-After`; an upstream that
does not answer is 503. A delete drops the cache's copy and then deletes at the upstream, and a fill
that was reading the object does not keep it; the completion of an upload drops the cache's copy of
the key. Listings leave out the keys under `_journal/` and `_tiering/`, always. The upstream key
must now be one that writes: with `write_through` the start runs the self-test of the storage node
against the upstream, under `_tiering/selftest/` (a create-only put that refuses a key that exists,
a replace on a tag, ranged reads, paging, and a multipart completion that refuses a key that
exists), and a store that answers one wrongly, a key that may not write included, is exit status 3
before the address is bound; one that cannot be reached is status 1. The upstream must therefore be
a store that takes writes under `_tiering/` (an S3 bucket, not the front of a storage node). The
upstream's key is also the cache's: only the front's roles keep one client from another's objects.
The start line then says `mode=write-through-cache`.

What a put keeps. The body of a create-only put under a `cache = true` policy is copied into
`.stage/` as it streams to the upstream. When the upstream has acknowledged the object, the node
asks it for the object's tag and time (one HEAD, so the tag in the cache is the one the upstream
gave, which for a large body is not an MD5), puts the copy in the cache, and then answers 200. The
copy is best effort and never fails the put: if every keep slot is busy (`max_concurrent_keeps`),
the object does not fit the budget, the stage directory cannot be written, the HEAD fails, the disk
falls behind the body, or the object was deleted meanwhile, the copy is dropped, the put is still
200 with the object at the upstream, and the skip is counted. The body is written to the stage file
by a task of its own through a queue of at most 16 chunks and 4 MiB (the bytes bound the memory,
whatever the size of the chunks; at most `max_concurrent_keeps` times 4 MiB in all), so a slow or
hung disk never makes the upload wait: the copy is dropped instead. Once the upstream has
acknowledged the object, the rest of the keep runs on its own task, so a client that goes away at
that point does not leave a half-made copy. The put waits for that task for `keep_wait` and is then
answered; the task ends within `keep_deadline` with the object kept or abandoned. A read of a key
whose copy is still being made never waits for it: it is answered from the upstream. The eviction
pass that a kept object may call for runs on its own task, and the put does not wait for it. A link
that is under way when a copy is abandoned cannot be called back and is undone when it ends; if its
thread never returns (a disk that hangs for good) the staged file and a reference to the cache stay
until the process ends, and the undoing may remove the entry of a fill of the same key made after
the abandonment, which costs one more trip to the upstream and never serves stale data.
A key named by its content (`cas/l/<sha256>`) is hashed as the body streams, copied or not;
a body that does not hash to the name is answered 400 `BadDigest`, the upstream stores nothing, and
nothing is kept. A body that is cut leaves nothing at the upstream or in the cache.
`/metricz` has `tierstore_cache_writes_through_total` (puts the upstream took),
`tierstore_cache_write_exists_total` (puts answered 412) and
`tierstore_cache_write_keep_skipped_total{reason}` with the reasons `budget` (also a body of no
declared length), `io`, `busy`, `head`, `deleted`, `slow` (the disk did not keep up), `deadline` (the copy was abandoned) and `read_only` (on main, not in v0.0.41: the cache does not hold the lock of its directory, so it copies nothing);
`tierstore_cache_keeps_expired_total` counts the puts answered before their copy was done.

```toml
[server]
listen          = "127.0.0.1:9400"
proxy_id        = "tierstore-cache-1"
bucket          = "tierstore"
access_key_file = "/etc/tierstore-cache/access.key"

[local_cache]
path       = "/var/lib/tierstore-cache"
layout     = "digest-v1"
budget     = "40GiB"
high_water = 90
low_water  = 75
critical   = 95
free_floor = 10
scrub_rate = "50MiB/s"

[cold_store]
kind             = "s3"
bucket           = "tierstore"
endpoint         = "http://s3-upstream.example:9300"
credentials_file = "/etc/tierstore-cache/upstream-read.key"

[[policy]]
prefix = "cas/b/"
cache  = true

[[policy]]
prefix = "cas/d/"
cache  = true

[[policy]]
prefix = ""
cache  = false
```

What it keeps: a key is kept if the longest `[[policy]]` prefix that holds it says `cache = true`
(and it is not under `_journal/` or `_tiering/`). Every other key, and every listing, goes to the
upstream as it is and is never kept. A HEAD, GET or ranged GET of a kept object is answered from the
disk and makes no call to the upstream. A GET of an object that is not there reads it from the
upstream in chunks of 8 MiB into `<path>/.stage/`, hashing the bytes as they pass, and links it into
`<path>/obj/<aa>/<bb>/<hex>` (raw bytes, mode 0444; `<hex>` is the SHA-256 of the key, or, for a
key `cas/l/<sha256>`, the SHA-256 in the key, which the bytes must hash to or the fill is
discarded and counted, and the read is answered with an error, since the upstream's bytes are known
to be wrong) only when the object is whole; a sidecar `<hex>.meta` holds the key, the
size, the hash of the bytes and the upstream's tag. The misses of one key share one fill. A ranged
GET of an object that is not there fills the whole object if it is at most `whole_fill_limit` (64
MiB by default), and otherwise passes the range through and keeps nothing; each such ranged read
costs the upstream a HEAD and a GET. A node that serves container layers through the registry front
(see The registry front) should raise the limit to its largest layer: a layer over it is never kept. A fill that fails (a short body, an object written over
while it was read, an upstream error, a directory that cannot be written) never fails the reader for
the cache's reasons: the reader is answered from the upstream. A kept object is never checked
against the upstream again, and a hit is not hashed again (the scrub below does that, later): put
`cache = true` only on prefixes whose objects are never rewritten (the content-addressed ones), and
note that the start reads the sizes of the files, not their bytes. Anyone who may read through the front may read everything the upstream's
read key can read, so give build clients scoped keys. The directories the cache makes are mode 0755
whatever the umask.

Memory. A fill holds one chunk (8 MiB) at a time, so the memory of the fills is at most
`max_concurrent_fills` chunks (4 by default: 32 MiB). A miss of a key with no fill running, when
that many fills are running, does not wait for one: it is served from the upstream as a
pass-through, keeps nothing, and is counted in `fills_skipped_total` and `fills_skipped_busy_total`.
The misses of a key whose fill is running share it and need no slot. A pass-through is not retried:
the object is filled by the next miss of its key that finds a free slot, so with every slot busy
for long, as under a sustained cold load, new keys are warmed more slowly (watch
`fills_skipped_busy_total`; raise `max_concurrent_fills` if it climbs). That bound is only for the
fills. The front holds a response whole, as it does for any backend, so a GET of a large object
(a whole GET, or a range) holds the bytes asked in memory for the request, hit, miss or
pass-through, whether or not the object is kept, and a whole GET of a miss holds the object once
the fill has kept it. A fill that panics or is cancelled frees its slot and its key, and the
readers that waited for it get a 503 (`fill_failures_total{reason="aborted"}`).

Scrub. A hit is not hashed again, so a background scrub reads the objects at `scrub_rate` (16 MiB a
second by default) and hashes each against the digest it must have: the name for a key
`cas/l/<sha256>`, and for any other key the `sha256` that the sidecar recorded at the fill (every
sidecar has it, so every object is checked). An object whose bytes do not match, or whose sidecar
can no longer be read, is removed from the index and the disk, counted in
`scrub_mismatches_total` and named on stderr; the next read of its key fills it again from the
upstream. The scrub walks the objects in name order, takes the names at the start of a round, and
starts over when it is done; it reads an object to its end, so one object larger than the rate is read
in one go and the next reads wait until the rate has paid for it. It never drops an object whose
fill is still ending, and never one whose file was replaced after it was read. A file or a sidecar that
cannot be read (a failing disk, out of descriptors) is left alone and counted in
`scrub_errors_total`; a sidecar that is missing or does not parse drops the object. The bytes are
hashed in chunks of 1 MiB and a shutdown ends the pass at the next chunk; inside one object the
read is not paced. A round (every object once) starts at most once in `scrub_min_round`. An
object whose check panics is counted in `scrub_errors_total` and skipped for the round. `scrub_rate = "off"`
turns it off; a restart still checks sizes only. A hard link that a job holds to a dropped object
keeps the old bytes for that job: the scrub removes the cache's name only.

Eviction keeps the cache under `budget` bytes: above `high_water` percent of it, and when the free
space of the file system (`statvfs`) is under `free_floor` percent, a pass removes the least
recently used objects until the cache is at `low_water` percent and the free space is back at the
floor. The order of use is in memory, seeded at start from the times of the files and written back
to them (once for each object each pass); an object with a link count over one is in use by another
tree, so it is skipped, counted and stays charged to the budget. A fill is skipped, and counted,
when the object is larger than the budget or would take the cache, with the fills that are running,
above `critical`. The free-space floor is a trigger of the eviction and nothing else: it does not
stop a fill, so on a disk that the jobs fill, the cache may fill the file system below the floor
until the next pass (every 10 seconds) frees it. The start refuses (exit status 2) a `budget` larger
than the size of the file system less `free_floor` percent of it; a file system whose size cannot be
read, or that reports 0 bytes, is not judged: the start warns on stderr and goes on. The files of
`.stage/` are never evicted and are removed at start. `put_permit_wait`, like `max_concurrent_puts` and `put_min_bytes_per_sec`, is accepted in
`[server]` of a build node and does nothing there without write-through, since the node answers every
put 403.

| key | what |
|---|---|
| `local_cache.path` | the directory of the cache, made if it is not there; put it on the file system that jobs' scratch is on, outside every job's mount |
| `local_cache.layout` | `"digest-v1"`, the only layout: raw bytes, hard-linkable |
| `local_cache.budget` | the bytes the cache is meant to hold: a number, or a string such as `"40GiB"`; needed |
| `local_cache.high_water` | percent of the budget above which a pass evicts (default 90) |
| `local_cache.low_water` | percent of the budget a pass evicts down to (default 75, below `high_water`) |
| `local_cache.critical` | percent of the budget above which a fill is skipped (default 95, not below `high_water`) |
| `local_cache.free_floor` | percent of the file system that is kept free (0 to 99, default 10): below it a pass evicts, even under the budget; it does not stop a fill, and `budget` may not be above the file system's size less the floor (exit status 2), unless its size cannot be read or is 0, which the start warns of and does not judge |
| `local_cache.max_concurrent_fills` | the most fills that run at once (1 to 64, default 4); a miss that finds them all running is served from the upstream and kept nowhere |
| `local_cache.max_concurrent_keeps` | the most create-only puts that are copied into the cache at once (1 to 64, default 8), apart from the fills; a put that finds them all running succeeds and is not kept |
| `local_cache.keep_wait` | the longest a put waits, once the upstream has the object, for its copy to be put in the cache (default `5s`, above 0). Past it the put is answered 200 and the copy is finished in the background; counted in `tierstore_cache_keeps_expired_total`. Safe because the upstream already holds the object: a read of it that misses costs only the trip, so read-your-write on the node is best effort past this wait |
| `local_cache.keep_deadline` | the longest a copy takes in all, from the upstream's acknowledgement (default `60s`, above 0 and not below `keep_wait`); a copy that is not done by then (a hung disk or upstream) is abandoned: its stage file is removed, its slot and room are given back, and it is counted as `deadline` |
| `local_cache.write_through` | `false` by default: a read cache that refuses every write with 403 before the role of the key is looked at. `true`: the node takes the writes its keys may do and sends each to the upstream, which must be written with a key that may write; the start runs the upstream's self-test (create-only and multipart-conditional included) and exits with status 3 if the upstream answers one wrongly |
| `local_cache.whole_fill_limit` | the most an object may be for a ranged read of it to fill the whole of it: a number or a size such as `"512MiB"`, from 1 byte to the `budget` (default 64 MiB); a larger object is passed through and kept nowhere. A fill reads 8 MiB at a time whatever the limit, so raising it does not raise the memory of the fills |
| `local_cache.cache` | `true`, the default; `false` is refused (leave `[local_cache]` out to run without a cache) |
| `local_cache.scrub_min_round` | the least time between the starts of two rounds of the scrub (default `"1h"`; at least `"1s"`, or `"0s"`: a round starts as soon as the last is done): a small cache is not read again and again at the rate |
| `local_cache.scrub_rate` | the bytes a second the scrub reads, a size and `/s` such as `"50MiB/s"` (default `"16MiB/s"`); `"off"` turns the scrub off |
| `cold_store.endpoints` | a list of exactly one endpoint, in place of `endpoint`: several are refused as not supported yet, since the cache reads from one upstream |

`[cold_store]` is the upstream: any `kind` (an `s3` bucket, with `endpoint` and a `credentials_file`
of a read-only key, in practice, or of a key that writes with `write_through`), and without `skip_checks` and the keys of the reconciler. `[server]`
is as for a storage node, without `backend_check_wait`. A `[[policy]]` with `cache = true` is needed.
The three tables `[hot_store]`, `[local_cache]` and `[cold_store]` together are refused: the hot
store is already local disk.

At start the process makes the directory, removes what a fill that was running left in `.stage/`,
reads `obj/` back (an object whose sidecar does not say what the file holds is deleted), and asks
the file system under the cache four questions with a probe object: a second link to a name that
exists is refused (`create-only`), a name made by a link shows the whole file (`atomic-visibility`),
the bytes read back are the bytes written (`raw-bytes`), and the link count reports the second
name (`st_nlink`). A file system that answers one wrongly is not served over: the process says which
check and exits with status 3, as for a tier that fails its self-test. It then asks the upstream a
HEAD of a key that is not there: 404 or any object is an answer, and anything else (it is down, it
refuses the key) exits with status 1. The write checks of the storage-node self-test are not run
against the upstream, whose key is read-only. On SIGTERM the process drains as a storage node does.

`GET /metricz` is not signed, and a bucket cannot be called `metricz`. It answers in the Prometheus
text format, `tierstore_cache_<name>`, with: `hits_total`, `misses_total`, `bytes_served_local_total`,
`bytes_served_upstream_total` (the bytes of every read that was not a hit, the read that follows a
fill included, so the byte hit ratio is the first over the sum of the two), `fills_total`,
`fills_skipped_total`, `fills_skipped_busy_total` (those of the skipped fills that found every fill
slot taken), `fill_failures_total{reason}` (`hash_mismatch`, `upstream_error`,
`short_body`, `local_io`, `changed`, `aborted`), `evictions_total{reason}` (`budget`, `floor`), `pinned_skips_total`, `scrubbed_objects_total`, `scrubbed_bytes_total`, `scrub_mismatches_total`, `scrub_errors_total`,
`discarded_at_start_total`, and the gauges `bytes`, `objects`, `fs_free_bytes` and `staging_files`.
A storage node has the proxy's counters instead, the same path on a node with the OCI front (a
storage node, or a build node with an `[oci]` table) also has the OCI counters (see Counters), and
a build node with the front shows the OCI block and then the cache block; a process with none of
the three has one line that says so; the proxy's series are in "Metrics" in
`crates/tierstore/README.md`.

### The registry front

The registry front (see The OCI read front) is the optional `[oci]` table of a storage node's file
or of a build node's, left out for a server with no front. The two shapes have the same keys and
the same rules. On a storage node the front reads through the proxy; on a build node it reads
through the local cache, so a blob or a manifest under a `[[policy]]` prefix with `cache = true`
is served from the disk when it is there and filled from the upstream when it is not, as an S3 GET
is. The blobs the front reads are the keys under `<prefix>blobs/sha256/` (`oci/` by default), so the
file is refused at load, naming `oci.prefix`, unless the longest `[[policy]]` prefix that covers
them has `cache = true`: a front whose blobs are kept nowhere would send every pull to the upstream,
and there is no use of that to keep (a registry that passes through is a storage node's front). A
blob over `local_cache.whole_fill_limit` is passed through, still checked against its digest, and
not kept.

A kept object is not checked again on a hit, so a blob that rotted on the cache's disk is detected
by the front when it is read whole (the response ends in an error or a cut connection, and
`tierstore_oci_digest_mismatches_total` counts it) but it is not evicted: it stays until the budget
evicts it, and every pull of it fails until then. To refill it, remove its data file and its
`.meta` sidecar under `local_cache.path`: both are named by the SHA-256 of the object's key (for
`oci/blobs/sha256/<digest>` that is the hash of that text, not the digest) and sit in
`obj/<first two hex digits>/<next two>/`. The next
read finds the file gone, drops the entry and fills the object again from the upstream.

The front is another listener of the same process: it starts with the node, drains with it,
and its counters are on the `/metricz` of the S3 listener beside the cache's. A key of the table
that is left out has the default of its flag:

```toml
[oci]
listen              = "192.0.2.10:5000"
tls_cert_file       = "/etc/tierstore/oci.crt"
tls_key_file        = "/etc/tierstore/oci.key"
prefix              = "oci/"
max_connections     = 64
connection_lifetime = "1h"
```

The table has the rules of the `--oci-*` flags: `listen` is needed in it, and the other keys are
refused without it; `tls_cert_file` and `tls_key_file` go together; `max_connections` is at least
one; `connection_lifetime` is a duration as elsewhere. The certificate and the key are named by
path, as every key file is, so the names end in `_file`.

### Which key each flag has

Every flag that describes the data path has the key or keys below, and no other has one. A flag
that takes a list (`--read-only-key-file`, given once for each key) is a list in the file. A tier
flag is a `kind` and the key its kind needs: `--hot dir:<path>` is `kind = "dir"` and `path`,
`--hot s3:<bucket>` is `kind = "s3"` and `bucket`. The keys of the reconciler, the journal and the
lease are in `[hot_store]`, since only a process with a hot tier has them. The crate's tests walk
the flags of the binary against this table, so a flag added without a key fails them.

| flag | key | what |
|---|---|---|
| `--listen` | `server.listen` | the address the S3 API, `/healthz`, `/readyz` and `/usagez` are served on. On main, not in v0.0.41: the flag may be given again, and the key may be a list of addresses, served by one server (see Filling in the flags) |
| `--proxy-id` | `server.proxy_id` | names the journal and the lease of this process |
| `--hot` | `hot_store.kind`, `hot_store.path`, `hot_store.bucket` | the hot tier: `kind` is `memory`, `dir` (with `path`) or `s3` (with `bucket`, `endpoint` and `credentials_file`); in a flag, `memory`, `dir:<path>` or `s3:<bucket>` |
| `--cold` | `cold_store.kind`, `cold_store.path`, `cold_store.bucket` | the cold tier, in the same three kinds |
| `--hot-endpoint` | `hot_store.endpoint` | `http://host[:port]` of an `s3` hot tier |
| `--hot-region` | `hot_store.region` | the signing region of an `s3` hot tier |
| `--hot-credentials-file` | `hot_store.credentials_file` | the key file of an `s3` hot tier |
| `--cold-endpoint` | `cold_store.endpoint` | `http://host[:port]` of an `s3` cold tier |
| `--cold-region` | `cold_store.region` | the signing region of an `s3` cold tier |
| `--cold-credentials-file` | `cold_store.credentials_file` | the key file of an `s3` cold tier |
| `--s3-request-timeout` | `server.s3_request_timeout` | the limit on a request to an `s3` tier (both tiers) |
| `--hot-bytes` | `hot_store.budget` | the bytes the hot tier is meant to hold: a number, or a string such as `"500GiB"` |
| `--high-water` | `hot_store.high_water` | percent of the budget above which eviction begins |
| `--low-water` | `hot_store.low_water` | percent of the budget eviction goes down to |
| `--critical` | `hot_store.critical` | percent of the budget above which a put waits and a cold read is not copied back |
| `--bucket` | `server.bucket` | the name of the bucket served |
| `--region` | `server.region` | the region of a request's signature |
| `--access-key-file` | `server.access_key_file` | the full key file |
| `--read-only-key-file` | `server.read_only_key_files` | a list of read-only key files |
| `--presign-key-file` | `server.presign_key_files` | a list of presigning key files |
| `--gc-key-file` | `server.gc_key_files` | a list of GC key files |
| `--scoped-key-file` | `server.scoped_key_files` | a list of scoped key files |
| `--presign-max-expires` | `server.presign_max_expires` | the longest expiry of any presigned URL |
| `--journal-linger` | `hot_store.journal_linger` | how long a batch of journal entries waits for more |
| `--journal-retention` | `hot_store.journal_retention` | how long an applied journal object is kept |
| `--copy-parallel` | `hot_store.copy_parallel` | the most objects the reconciler copies at once |
| `--lease-ttl` | `hot_store.lease_ttl` | how long the reconciler's lease lasts without a renewal |
| `--reconcile-interval` | `hot_store.reconcile_interval` | how often the reconciler works |
| `--lock-file` | `hot_store.lock_file` | the lock file the reconciler works under: only the process that holds it reconciles (on main, not in v0.0.41) |
| `--usage-stale-after` | `hot_store.usage_stale_after` | how old the published usage may be before `/usagez` says 503 |
| `--max-request-bytes` | `server.max_request_bytes` | the longest body of a request |
| `--max-concurrent-puts` | `server.max_concurrent_puts` | the most put bodies and parts taken at once |
| `--put-permit-wait` | `server.put_permit_wait` | how long a put that arrives with all of those taken waits for one to end (15s by default; 0 answers 503 at once). No byte of its body is read while it waits. If none ends in that time it is answered 503 `SlowDown` with `Retry-After: 1`. A waiter holds a connection and no permit or body memory, is not bounded except by the connections, counts as in flight in a drain (so a drain may take up to this much longer, never past `--drain-deadline`), and does not notice its client leaving until its body is first read |
| `--put-min-bytes-per-sec` | `server.put_min_bytes_per_sec` | the least average rate of a body (0 turns it off) |
| `--put-rate-grace` | `server.put_rate_grace` | the time waiting for the client before that rate is held |
| `--body-idle-timeout` | `server.body_idle_timeout` | how long a body or an answer may go without a byte |
| `--skip-backend-check` | `cold_store.skip_checks` | a list of `CHECK=REASON` texts: the start-up check that is left out, with its reason (only `multipart-conditional`; it runs on the cold tier) |
| `--backend-check-wait` | `server.backend_check_wait` | how long the start-up self-test asks again of a tier that does not answer |
| `--oci-listen` | `oci.listen` | where the registry front listens; the front is off without the `[oci]` table. On main, not in v0.0.41: the flag may be given again, and the key may be a list of addresses |
| `--oci-tls-cert` | `oci.tls_cert_file` | the certificate chain (PEM) of the front; with `oci.tls_key_file`, or neither |
| `--oci-tls-key` | `oci.tls_key_file` | the private key (PEM) of the front |
| `--oci-prefix` | `oci.prefix` | the key prefix the front reads, ending in `/` (`oci/` if left out) |
| `--oci-max-connections` | `oci.max_connections` | the most connections the front has open at once (64 if left out; at least 1) |
| `--oci-connection-lifetime` | `oci.connection_lifetime` | the longest a connection of the front lives (`1h` if left out) |

These stay flags, since they describe one run and not the data path: `--config`,
`--print-config`, `--check-config`, `--capabilities`, `--drain-lb-wait` and `--drain-deadline`. A
start that gives `--config` and any other flag is refused with status 2 and a message that names
the flags (`--drain-lb-wait`, `--drain-deadline` and `--check-config` may go with it). A refusal
that the checks of the start make (water marks out of order, a key file others can read) names the
keys of the file where the flag-mode message names the flags.

`tierstore --capabilities` (on main, not in v0.0.41) prints what this build can do, one
capability to a line, and exits with status 0; a build with none prints nothing. It takes no other
flag and needs none of those a start requires: with any other argument it prints nothing on stdout,
says on stderr `tierstore: --capabilities takes no other flag, got <argument>`, and exits with
status 2. A host updater reads it from a release before it hands over to it. `tierstore --version`
prints `tierstore <version>+<commit>`, the end of the start line is ` commit=<commit>`, and
`/metricz` has `tierstore_build_info{version="<version>",commit="<commit>"} 1`. The commit is the
one the binary was built from, twelve hex digits: git's, or the value of
`TIERSTORE_BUILD_COMMIT_OVERRIDE` at build time for a build without a `.git` (a build refuses a
value that is not twelve lowercase hex digits), or `unknown` where there is neither.

`tierstore --config <path> --check-config` loads the file as a start does (the same loader and the
same refusals, the build-node checks and `write_through` included) and reads the key files it
names; it binds nothing, makes no directory and calls no store or network. It prints nothing and
exits 0 if the file would start; if the file is refused it prints the message a start prints and
exits 2. `--check-config` without `--config` is a usage error (status 2). It does not reach the
tiers, so a tier that is down, or a key that the store would refuse, is found by a start (its
self-test) and not by the check. It does not look at the disk either: the start's refusal of a
`budget` over the size of the file system less `free_floor` (exit status 2), the cache directory
and the credentials file of a tier are checked by a start only. For a storage node with an `[oci]`
table the check does validate the OCI front as a start does (the prefix, the connection lifetime,
and the TLS certificate and key files, which it reads and parses), without binding the address
and without the warning that a start gives for plain HTTP off loopback. A TLS file that is not
there or cannot be used is exit status 2 under `--check-config`, where a start exits 1.

### What the loader refuses

Each of these exits with status 2 and names the file and the key:

- a key or a table the format does not have (the file is a closed set), and a value of the wrong
  type;
- a key that holds a secret: keys and credentials are paths of files, whose names end in `_file`
  or `_files`; `secret_access_key`, `access_key_id`, `password`, a name with `token` in it and the
  like are refused wherever they are;
- `[local_cache]` along with `[hot_store]`, and a file with neither shape's tables (it needs both
  `[hot_store]` and `[cold_store]`, or `[local_cache]` and `[cold_store]`); the refusals of a build
  node are with its table above;
- a key of a tier that its `kind` does not take (`path` for a `memory` tier, `endpoint` for a `dir`
  tier), a `dir` tier without `path`, an `s3` tier without `bucket`, `endpoint` or
  `credentials_file`, and a key only one tier has in the other (`budget` and the reconciler's keys
  are `[hot_store]`'s; `skip_checks` is `[cold_store]`'s, since the check it skips runs there);
- a water mark outside 1 to 100, and a budget, a count or a limit of zero;
- an `[oci]` table without `listen`, with a key but no `listen` (the message names that key), with
  only one of `tls_cert_file` and `tls_key_file`, or with `max_connections = 0`;
- a `[[policy]]` prefix that is listed twice, that lies inside another listed prefix (or holds
  one), that does not end in `/`, or that is under `_journal/` or `_tiering/`, which tierstore
  keeps for itself.

`[[policy]]` has a `prefix` (empty for all the rest) and `cache`. On a build node `cache = true`
keeps the keys under the prefix; on a storage node a policy is read and checked and has no effect,
and `cache = true` is refused, since a storage node keeps no cache. The empty prefix may be listed
along with others (it is the rest); two non-empty prefixes may not nest.

### Moving from flags to a file

`tierstore --print-config` with the flags of a unit (without `--config`) prints the file that is
equivalent to them on stdout and exits 0 without starting anything; it prints the key files by
path, never what is in them, and the file it prints is loaded back before it is printed, so it
loads. Take the flags out of `ExecStart=`:

```sh
/usr/libexec/kbf/tierstore --print-config --listen 192.0.2.10:9000 ... > /etc/tierstore/tierstore.toml
```

and switch the unit by changing `ExecStart=` to

```
ExecStart=/usr/libexec/kbf/tierstore --config /etc/tierstore/tierstore.toml --drain-lb-wait 10s --drain-deadline 15m
```

The flags and the file are both accepted in this release, so a unit may stay as it is and move
when it is convenient; the flags are to go in the release after, and `--config` is then the way to
configure the data path. The shipped `tierstore.service` keeps its flags, and
`crates/tierstore/tests/packaging.rs` parses them as the binary parses them. `--print-config` does
not convert anything else in a unit (a drop-in, the firewall rules).

## Filling in the flags

The unit's `ExecStart=` is a complete command line. The flags to edit:

| flag | what |
|---|---|
| `--listen` | the address the S3 API, `/healthz` and `/readyz` are served on. On main, not in v0.0.41: give it again for each further address (for the wildcard of both families, `0.0.0.0:<port>` and `[::]:<port>`; an IPv6 address is bound for IPv6 only, so they do not clash); one server, one drain and one limit on connections serve them all, an address given twice is refused (exit 2), and the start line shows them as `listen=<a>,<b>`. A wildcard and a specific address on one port, or one address on both `--listen` and `--oci-listen`, are refused by the system at bind on Linux (exit 1, naming the address); on macOS and the BSDs `SO_REUSEADDR` can let them share the port |
| `--proxy-id` | names this process's journal and its lease in the hot tier; keep it across restarts, and give each process over the same tiers one of its own |
| `--bucket` | the name of the one bucket clients see |
| `--access-key-file`, `--read-only-key-file` | the full key, and (given once for each) the read-only keys; see above |
| `--presign-key-file`, `--presign-max-expires` | a presigning key with its ceiling (given once for each key), and the longest expiry of any URL; see Presigned PUT URLs |
| `--gc-key-file` | a GC key (given once for each key); see GC keys |
| `--scoped-key-file` | a scoped key (given once for each key); see Scoped keys |
| `--hot`, `--cold` | the tiers: `s3:<bucket>` for a bucket of an S3-compatible store (also `dir:<path>` and `memory`; a `dir:` path must also be writable under `ProtectSystem=strict`, so add it to `ReadWritePaths=`) |
| `--hot-endpoint`, `--cold-endpoint` | where the tier's store is, `http://host[:port]` (TLS is not built yet); `--hot-region` and `--cold-region` set the signing region of a tier, `us-east-1` if not given |
| `--hot-bytes` | the bytes the hot tier is meant to hold; the water marks are shares of it (`--high-water`, `--low-water`, `--critical`) |
| `--drain-lb-wait`, `--drain-deadline` | the drain on SIGTERM: how long `/readyz` says 503 while the server goes on accepting and serving (the settle), and how long it then waits for requests in flight and for the connections it closes (see "Stopping it") |
| `--usage-stale-after` | how old the usage that the reconciler publishes may be (2m by default, longer than `--lease-ttl`) before `/usagez` says 503 with the reason (200 and the age while fresh). Puts ignore a usage older than 30 s, so a stopped reconciler means a hot tier that is never taken to be full; puts are not refused. A load balancer probes `/readyz`, which does not look at the usage (one stopped reconciler would otherwise take every node out); alerting probes `/usagez` |
| `--oci-listen`, `--oci-tls-cert`, `--oci-tls-key`, `--oci-prefix` | the read-only registry front, off unless `--oci-listen` is given; see The OCI read front |

`tierstore --help` lists them all, with their defaults.

## Using a stock SDK

The front serves one bucket, path-style, over plain HTTP, and checks SigV4 in the `Authorization`
header only. An SDK with its defaults sends some things the front refuses, so a client needs these
settings. `crates/tierstore/tests/s3_front/sdk.rs` drives the front with aws-sdk-rust (the
`aws-sdk-s3` crate, version 1.96.0, in this repository's tests only) set this way, and that test is
what "tested" means below.

| setting | value | why |
|---|---|---|
| endpoint | `http://<host>:<port>` of `--listen` | the front has no TLS; an `https://` endpoint fails |
| addressing | path-style (`force_path_style`) | the bucket is in the path, `/<bucket>/<key>`; virtual-host addressing (`<bucket>.<host>`) is not served |
| bucket | the `--bucket` name | there is one; other names are not served |
| region | tierstore's own `--region` (not `--hot-region` or `--cold-region`, which are for the tiers' stores), `us-east-1` by default | the region is in the credential scope of the signature, and a client with another is refused 403 `InvalidAccessKeyId` |
| credentials | a static key id and secret: the full key, or a read-only key | the front has no session tokens, no STS and no presigned URLs |
| request checksums | only when an operation requires one (`request_checksum_calculation = when_required`) | the SDK default sends a checksum with every upload, and for a streamed body sends it as a trailer in an `aws-chunked` body, which the front refuses with 501 |
| response checksums | only when required (`response_checksum_validation = when_required`) | the front sends no checksum on a download, so there is nothing to validate; this keeps the SDK from asking |
| checksum algorithm | none, or CRC-32, CRC-32C, SHA-1 or SHA-256 | read from the front's code, not tested with an SDK: CRC-64/NVMe, `aws-chunked` and trailing checksums are refused with 501; `Content-MD5` is checked |
| `x-id` | nothing to set | the AWS SDKs add `x-id=<Operation>` to the query of the calls of an object, and no setting stops it; S3 ignores it, and so does the front |
| listing | no `encoding-type` | `encoding-type=url` is refused with 501 (aws-sdk-rust sends it only when asked to) |

Payload signing, read from the front's code and not tested as such (not checked: whether the Rust
SDK signs the hex hash or `UNSIGNED-PAYLOAD` for an in-memory body): a body whose signed `x-amz-content-sha256` is its hex SHA-256 is checked against it;
`UNSIGNED-PAYLOAD` with no checksum header is accepted unchecked; a `STREAMING-` payload is
refused with 501.

aws-sdk-rust, as the test sets it (the test also passes the SDK's HTTP client built without TLS,
`aws_smithy_http_client::Builder::new().build_http()`, which the lines below leave out):

```rust
use aws_sdk_s3::config::{Credentials, Region, RequestChecksumCalculation, ResponseChecksumValidation};

let config = aws_sdk_s3::Config::builder()
    .behavior_version_latest()
    .endpoint_url("http://tierstore.example:9000")
    .force_path_style(true)
    .region(Region::new("us-east-1"))
    .credentials_provider(Credentials::new(KEY_ID, SECRET, None, None, "static"))
    .request_checksum_calculation(RequestChecksumCalculation::WhenRequired)
    .response_checksum_validation(ResponseChecksumValidation::WhenRequired)
    .build();
let client = aws_sdk_s3::Client::from_conf(config);
```

With these, the test puts an object, gets it whole and by byte ranges, heads it, lists with a prefix
and a continuation token, deletes it, uploads in parts (create, two parts, complete) and aborts an
upload. A second put of a key, with `If-None-Match: *` or without, comes back as an error with
status 412 and code `PreconditionFailed`. A read-only key gets, heads and lists, and its put and
delete come back as 403 `AccessDenied`.
The aws CLI and boto3 are **not tested**: the lines below are from their documentation, and
whether they are enough is not shown. They need a version that has the checksum settings (aws CLI
2.23 or later, boto3 1.36 or later).

```ini
# ~/.aws/config
[default]
region = us-east-1
endpoint_url = http://tierstore.example:9000
request_checksum_calculation = when_required
response_checksum_validation = when_required
s3 =
    addressing_style = path
```

```python
# boto3
import boto3
from botocore.config import Config

s3 = boto3.client(
    "s3",
    endpoint_url="http://tierstore.example:9000",
    region_name="us-east-1",
    aws_access_key_id=KEY_ID,
    aws_secret_access_key=SECRET,
    config=Config(
        signature_version="s3v4",
        s3={"addressing_style": "path"},
        request_checksum_calculation="when_required",
        response_checksum_validation="when_required",
    ),
)
```

One thing is known from the front and not from those clients: boto3 and the aws CLI are said to
add `encoding-type=url` to a ListObjects or ListObjectsV2 call on their own, with no setting to
stop it, and the front answers that parameter with 501 `NotImplemented`. If that is so, `aws s3
ls` and `list_objects_v2` fail against the front, and the calls that name a key (`get_object`,
`put_object`, `head_object`, `delete_object`, multipart) are not affected. Check it with a real
client before relying on either.

## The OCI read front

With `--oci-listen`, the process also serves the objects of the store as a read-only container
registry, over HTTPS or over plain HTTP (see Two modes), so that podman (and the
other clients of the OCI distribution API) pulls an image by digest from the same hot and cold tiers the S3 front writes to. It reads through the
proxy in the process: no S3 credential and no hop through the S3 listener. It is off unless
`--oci-listen` is given, and then nothing else about the process changes.

With `--config` the front is the `[oci]` table of the file and the `--oci-*` flags are refused
(see The configuration file, which lists the key of each flag below). `--print-config` writes
the front's flags as that table. A build node's file takes the same `[oci]` table (it has no flags):
the front then reads through the node's local cache, not a proxy (see The registry front).

| flag | what |
|---|---|
| `--oci-listen ADDR` | where the registry front listens. With `--oci-tls-cert` and `--oci-tls-key` it is HTTPS; with neither it is plain HTTP, at any address; off loopback (127/8 and `::1` are loopback; `0.0.0.0` and `::` are not) the start prints one warning line, and a certificate and key are advised. On main, not in v0.0.41: give the flag again for each further address; one front, one limit on connections and one drain serve them all, and an address given twice is refused (exit 2); the start line shows them as `oci-listen=<a>,<b> (https)` or `(http)`, and a plain front prints one warning line for each address off loopback |
| `--oci-tls-cert PATH`, `--oci-tls-key PATH` | both or neither (one without the other is refused at start, exit 2). The certificate chain (leaf first) and the private key, PEM. Clients dial an IP address, so the leaf must carry an IP subjectAltName for `--oci-listen` (for each of them, when it is given more than once; any IP subjectAltName, if the address is `0.0.0.0` or `::`); a certificate without one, one that is expired or not yet valid, a key file any user can read (the "other" mode bits; a group may), or an unreadable file, is refused at start. The files are read once: a new certificate takes a restart, so renew it before it expires |
| `--oci-prefix PREFIX` | the key prefix, ending in `/`, `oci/` if not given. A prefix inside `_journal/` or `_tiering/`, or one that is not plain path segments, is refused at start (exit 2) |
| `--oci-max-connections N` | connections open at once, 64 if not given; the others wait in the kernel's backlog. Memory is bounded by connections times the buffer a connection holds (up to 4 MiB), about 256 MiB at the default |
| `--oci-connection-lifetime D` | the longest a connection lives, 1h if not given; a request being answered is finished first |

A client has 10 seconds to finish the TLS handshake and then the S3 front's header timeout (a
minute) to send the head of a request, which is also how long an idle connection may wait for its
next request. A connection holds one of the `--oci-max-connections` slots from accept to close and
there is no limit per client address, so a host that opens that many connections and says nothing
can keep the others waiting: the host's firewall rules are the control for that. A client that takes no byte of an answer for `--body-idle-timeout`
is cut. A connection that reaches its lifetime finishes the answer it is sending for at most
`--drain-deadline`, and is cut then. On the drain the front goes on serving through `--drain-lb-wait`, takes no new connection,
lets the requests being answered finish, and closes what is left at `--drain-deadline`. The start
line on stdout gets ` oci-listen=<address> (https)` or ` (http)` after `listen=<address>`, and `/metricz`
has `tierstore_oci_front_info{mode="https"}` or `{mode="http"}`. In plain mode there is no handshake,
so the handshake time does not apply; the other limits are the same.

### Two modes

- **HTTPS**: with `--oci-tls-cert` and `--oci-tls-key`, at any address.
- **Plain HTTP**: with neither, at any address. A node's own container engine can pull from the
  front of its own tierstore over loopback, so the pull is a connection from the host to itself and
  the traffic never leaves it. A connection to any address beyond loopback can be read and changed on
  its way: the process starts, prints one warning line on stderr, and a certificate and key are
  advised there. Loopback is reachable by every local process, and by containers that
  share the host's network namespace; the front is anonymous in either mode.

A container engine that pulls over plain HTTP has to be told it may:

- podman: mark the registry insecure for that location in `registries.conf`, e.g.

  ```toml
  [[registry]]
  location = "127.0.0.1:9500"
  insecure = true
  ```

  or pull with `--tls-verify=false`. No `certs.d` directory is needed.
- js-oci needs to be allowed plain HTTP to a loopback registry address for such a pull; that is a
  setting of js-oci, not of tierstore.

What it serves, and nothing else (any other path is 404 and any method but GET and HEAD is 405
`UNSUPPORTED` with `Allow: GET, HEAD`, both with the OCI error document):

| request | answer |
|---|---|
| `/v2/` | 200, `{}`, `Docker-Distribution-API-Version: registry/2.0` |
| `/v2/<name>/manifests/sha256:<hex>` | the manifest (at most 4 MiB), with `Content-Type` from its `mediaType`, `Content-Length`, `Docker-Content-Digest` and an `ETag` of the digest. A tag is 404 `MANIFEST_UNKNOWN`: tags are not served |
| `/v2/<name>/blobs/sha256:<hex>` | the blob, streamed, `application/octet-stream`, with `Accept-Ranges: bytes`; one `Range` is 206 with `Content-Range`, one past the end 416 |

The `Content-Type` of a manifest is its `mediaType` when that is the OCI image manifest or index or
the Docker schema 2 manifest or manifest list. A manifest with none, or another, is served as an OCI
image manifest if it is `schemaVersion` 2 with a `config` object and a `layers` array, as an OCI
image index if it is `schemaVersion` 2 with a `manifests` array (an index may leave `mediaType`
out), and is 500 `MANIFEST_INVALID` with the reason otherwise.

A malformed digest (not `sha256:` and 64 lowercase hex digits) is 400 `DIGEST_INVALID`, a repository
name that is not valid by the distribution grammar is 400 `NAME_INVALID`, an object that is not in
the store is 404 `BLOB_UNKNOWN` or `MANIFEST_UNKNOWN`, a store that cannot be reached is 503, and
what the store said stays in the log.

### Keys, and one namespace

An object is at `<prefix>blobs/sha256/<64 lowercase hex digits>`, the same for a manifest, an index,
a layer and a config: manifests are blobs. The `<name>` in a URL is checked against the grammar and
then ignored, so there is one content-addressed namespace and any name pulls any digest. A key is
built only from a validated digest, so nothing outside the prefix can be named.

### Integrity

A manifest is read whole and its SHA-256 is checked against its digest before a byte is sent; one
that does not match is 500 `DIGEST_INVALID`, is never served and is counted. The size is asked
before the read, so a manifest replaced by an object of another size between the two can be
answered 500 `DIGEST_INVALID` once; a retry succeeds. A blob is hashed as it is
read, in chunks of 4 MiB, and its last chunk is held back until the hash is right: a blob that does
not match its digest ends the connection before the response is complete, so no client receives a
whole wrong body, and the mismatch is counted. That holds for a read of the whole blob, and for a
`Range` that covers all of it (which is still 206). A blob `HEAD` and a `Range` of part of a blob
report what the store holds and verify nothing; the client verifies the digest of what it
assembles. (A manifest `HEAD` reads and verifies the manifest.) A client that sends several ranges
(as a partial pull of a `zstd:chunked` or `estargz` layer does) is sent the whole blob with 200,
which the specification allows, and falls back to splitting it. `If-Range`, `If-None-Match` and
`Accept` are ignored.

### Anonymous reads

The front never asks for a token: a request with an `Authorization` header is served as one without.
Anyone who can connect can read any digest under the prefix, and nothing else. That is the intent:
the images in it are public (a public registry's images are the first thing put there), and the
network decides who can connect (the host's firewall rules). Do not put an image in
the prefix that is not meant to be read by whoever can reach the port.

### Putting an image in the store

Take the blobs of an OCI layout (the files under `blobs/sha256/` of a layout made with
`skopeo copy ... oci:<dir>` or `podman save --format oci-dir`) and put each, create-only, at
`<prefix>blobs/sha256/<hex>` through the S3 front with the full key: the layers, the config, the
manifest, and the index of a multi-platform image, all by the file's own name, which is its digest.
`index.json` of the layout is not put; the digest of the manifest (or index) to pull is in it, and
is what a client is given. A layer larger than `--max-request-bytes` (256 MiB if not given) needs the
flag raised, or a multipart upload, or a presigned PUT whose key has a `max_object_bytes` of up to
5 GiB (see Presigned PUT URLs; the ceiling prefix must cover `oci/`). An object that is already
there is refused (412), which is a retry that is safe.

Check a put with `HEAD` on the front: the `Docker-Content-Digest` of a manifest is the digest of the
bytes it read, after they were checked.

### Pulling

Over plain HTTP on loopback there is no CA to trust; see Two modes for the client settings.

podman, from a host that trusts the CA of the certificate:

```sh
# the CA that issued --oci-tls-cert, in the directory named like the registry in the reference
# (the address, with :port when the reference has one)
install -D -m 0644 farm-ca.crt /etc/containers/certs.d/192.0.2.10/ca.crt
podman pull 192.0.2.10/library/app@sha256:<manifest digest>
```

The reference is by digest: a tag is refused. The repository name (`library/app`) is any name that
fits the grammar and means nothing to the store. A registry on another port than 443 is written
`192.0.2.10:8443/library/app@sha256:...` and its directory is `certs.d/192.0.2.10:8443`.

A client that allows only `https` registries on port 443 (or the port in the reference), and
registries at a private address only when told to, pulls the same references; allow the address in
that client's own settings.

### Counters

`GET /metricz` on the S3 listener (unsigned, like `/healthz`; a bucket cannot be called `metricz`)
shows, in the Prometheus text format, `tierstore_oci_requests_total{route,outcome}` (routes `ping`,
`manifest`, `blob`, `other`; outcomes `ok`, `partial`, `not_found`, `invalid`, `not_allowed`,
`range_not_satisfiable`, `failed`), `tierstore_oci_bytes_served_total` (bytes of blobs read for
responses), `tierstore_oci_digest_mismatches_total` and `tierstore_oci_front_info{mode="https"|"http"}` (1; the mode the front runs in). They are not on the registry listener,
which serves nothing else. Without `--oci-listen` the path says the front is not enabled.

## Starting it

The unit does not start at install and does not start at boot: it has no `[Install]` section,
so `systemctl enable` has nothing to link, and it is skipped while `/etc/tierstore/access.key`
does not exist. Start it by hand when the files are in place:

```sh
systemctl start tierstore
```

To start it at boot, add the section in a drop-in (`systemctl edit tierstore` takes
`[Install]` and `WantedBy=multi-user.target`) and run `systemctl enable tierstore`; that is
the operator's choice, and the shipped file does not make it.

## Stopping it

systemd sends SIGTERM. The process answers `/readyz` 503 and puts `Connection: close` on every
answer from then on, serves for `--drain-lb-wait` (the settle) so that a load balancer sees it,
stops accepting, and closes each idle connection with a lingering close (it shuts its end for
writing, so that the client reads a clean end of stream, and reads until the client closes, for 60
seconds at most). A request already on the wire is served, with `Connection: close`, never
answered 503; one that arrives within a round trip of the end of the stream cannot be answered
and is counted in `tierstore_drain_late_requests_total`. It waits for the requests in flight and
the connections for at most `--drain-deadline`, gives up the reconciler's lease and exits 0. This
is on main, not in v0.0.41, which answers a request on a connection that is open after the settle
503 `ServiceUnavailable`. `TimeoutStopSec=` is set above
the sum of the two flags and the lease's wait, so that systemd does not send SIGKILL to a drain
in progress; if `--drain-deadline` is raised, raise it too. A start that fails is tried again
after 30 seconds; exit status 2 (a command line the argument parser refused: an unknown flag, a
missing or malformed value; or two keys with one access key id) is not, since a restart cannot
mend it. Other flag combinations and key files refused after parsing exit 1, and are tried again
every 30 seconds.

## The start-up self-test

Before it binds its address the process asks each tier the questions its guarantees rest on: a
create-only put of a key that exists must be refused, a replace on a wrong ETag must be refused
and one on the right ETag must work, a ranged read must give exactly the range, a HEAD must give
the size and the ETag of what was written, a listing after a key must page right, and on the cold
tier the completion of a multipart upload over a key that exists must be refused. (Only the
create-only completion is probed, not the refusal of a bad part list.) It uses keys under
`_tiering/selftest/`, which clients never see; they are deleted afterwards, and keys a killed
start left more than an hour ago (by the store's clock, not the machine's) are swept; only keys
under that prefix are ever deleted. A tier that answers one of them wrongly exits 3
with the tier, the check and the answer on stderr, and is not restarted
(`RestartPreventExitStatus=2 3`): fix or replace the store. That includes answers that are a
configuration error, such as 403 `AccessDenied` or `NoSuchBucket`, which are not retried either.
A tier that cannot be reached (down, a timeout, 408, 429, any 5xx but 501 and 507) is asked again,
after pauses from one second doubling to thirty, for `--backend-check-wait` (five minutes by
default, for each tier; `0s` asks once), so that a store that is slow to come up after a reboot
does not fail the start. Then the start exits 1 and the unit tries it again after 30 seconds, up to
its start limit (5 starts in an hour, so about half an hour of real failures, then systemd gives up
until `systemctl reset-failed tierstore`). An
error that persists on a conditional step (a create-only put, `If-Match`, the completion of a
multipart upload) while the store is up, a 500 for instance, may be a gap in the store and not an
outage; the message says so.
A tier that is full (a full hot tier is a designed state) starts: the checks that need a write are
skipped, with a line on stderr that says so. For a full cold tier the line is a warning that writes
to the cold tier will fail.

`--skip-backend-check multipart-conditional=<reason>` leaves out the multipart check, for a store
that lacks a conditional completion; the reason is written to stderr at every start. No other check
can be skipped (exit 2). Without that check a multipart upload to the cold tier may replace an
object that exists.
