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

This release reads the storage-node shape: `[server]`, `[hot_store]` and `[cold_store]`, and an
optional `[[policy]]`. A file for a storage node:

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

### Which key each flag has

Every flag that describes the data path has the key or keys below, and no other has one. A flag
that takes a list (`--read-only-key-file`, given once for each key) is a list in the file. A tier
flag is a `kind` and the key its kind needs: `--hot dir:<path>` is `kind = "dir"` and `path`,
`--hot s3:<bucket>` is `kind = "s3"` and `bucket`. The keys of the reconciler, the journal and the
lease are in `[hot_store]`, since only a process with a hot tier has them. The crate's tests walk
the flags of the binary against this table, so a flag added without a key fails them.

| flag | key | what |
|---|---|---|
| `--listen` | `server.listen` | the address the S3 API, `/healthz`, `/readyz` and `/usagez` are served on |
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
| `--usage-stale-after` | `hot_store.usage_stale_after` | how old the published usage may be before `/usagez` says 503 |
| `--max-request-bytes` | `server.max_request_bytes` | the longest body of a request |
| `--max-concurrent-puts` | `server.max_concurrent_puts` | the most put bodies and parts taken at once |
| `--put-min-bytes-per-sec` | `server.put_min_bytes_per_sec` | the least average rate of a body (0 turns it off) |
| `--put-rate-grace` | `server.put_rate_grace` | the time waiting for the client before that rate is held |
| `--body-idle-timeout` | `server.body_idle_timeout` | how long a body or an answer may go without a byte |
| `--skip-backend-check` | `cold_store.skip_checks` | a list of `CHECK=REASON` texts: the start-up check that is left out, with its reason (only `multipart-conditional`; it runs on the cold tier) |
| `--backend-check-wait` | `server.backend_check_wait` | how long the start-up self-test asks again of a tier that does not answer |

These stay flags, since they describe one run and not the data path: `--config`,
`--print-config`, `--drain-lb-wait` and `--drain-deadline`. A start that gives `--config` and any
other flag is refused with status 2 and a message that names the flags (`--drain-lb-wait` and
`--drain-deadline` may go with it). A refusal that the checks of the start make (water marks out of
order, a key file others can read) names the keys of the file where the flag-mode message names the
flags.

### What the loader refuses

Each of these exits with status 2 and names the file and the key:

- a key or a table the format does not have (the file is a closed set), and a value of the wrong
  type;
- a key that holds a secret: keys and credentials are paths of files, whose names end in `_file`
  or `_files`; `secret_access_key`, `access_key_id`, `password`, a name with `token` in it and the
  like are refused wherever they are;
- `[local_cache]`, which is reserved for a later release and refused as not supported yet, and a
  file without both `[hot_store]` and `[cold_store]`;
- a key of a tier that its `kind` does not take (`path` for a `memory` tier, `endpoint` for a `dir`
  tier), a `dir` tier without `path`, an `s3` tier without `bucket`, `endpoint` or
  `credentials_file`, and a key only one tier has in the other (`budget` and the reconciler's keys
  are `[hot_store]`'s; `skip_checks` is `[cold_store]`'s, since the check it skips runs there);
- a water mark outside 1 to 100, and a budget, a count or a limit of zero;
- a `[[policy]]` prefix that is listed twice, that lies inside another listed prefix (or holds
  one), that does not end in `/`, or that is under `_journal/` or `_tiering/`, which tierstore
  keeps for itself.

`[[policy]]` has a `prefix` (empty for all the rest) and `cache`. In this release a policy is read
and checked and has no effect, since nothing is cached yet; `cache = true` is refused as not
supported yet. The empty prefix may be listed along with others (it is the rest); two non-empty
prefixes may not nest.

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
| `--listen` | the address the S3 API, `/healthz` and `/readyz` are served on |
| `--proxy-id` | names this process's journal and its lease in the hot tier; keep it across restarts, and give each process over the same tiers one of its own |
| `--bucket` | the name of the one bucket clients see |
| `--access-key-file`, `--read-only-key-file` | the full key, and (given once for each) the read-only keys; see above |
| `--presign-key-file`, `--presign-max-expires` | a presigning key with its ceiling (given once for each key), and the longest expiry of any URL; see Presigned PUT URLs |
| `--gc-key-file` | a GC key (given once for each key); see GC keys |
| `--scoped-key-file` | a scoped key (given once for each key); see Scoped keys |
| `--hot`, `--cold` | the tiers: `s3:<bucket>` for a bucket of an S3-compatible store (also `dir:<path>` and `memory`; a `dir:` path must also be writable under `ProtectSystem=strict`, so add it to `ReadWritePaths=`) |
| `--hot-endpoint`, `--cold-endpoint` | where the tier's store is, `http://host[:port]` (TLS is not built yet); `--hot-region` and `--cold-region` set the signing region of a tier, `us-east-1` if not given |
| `--hot-bytes` | the bytes the hot tier is meant to hold; the water marks are shares of it (`--high-water`, `--low-water`, `--critical`) |
| `--drain-lb-wait`, `--drain-deadline` | the drain on SIGTERM: how long `/readyz` says 503 while the server goes on serving, and how long it then waits for requests in flight |
| `--usage-stale-after` | how old the usage that the reconciler publishes may be (2m by default, longer than `--lease-ttl`) before `/usagez` says 503 with the reason (200 and the age while fresh). Puts ignore a usage older than 30 s, so a stopped reconciler means a hot tier that is never taken to be full; puts are not refused. A load balancer probes `/readyz`, which does not look at the usage (one stopped reconciler would otherwise take every node out); alerting probes `/usagez` |

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

systemd sends SIGTERM. The process answers `/readyz` 503, serves for `--drain-lb-wait` so that a
load balancer sees it, refuses new requests, waits for those in flight for at most
`--drain-deadline`, gives up the reconciler's lease and exits 0. `TimeoutStopSec=` is set above
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
