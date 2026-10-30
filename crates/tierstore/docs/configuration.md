# Configuration

tierstore is configured by flags, or by one TOML file given with `--config <path>`. The two make the same settings and run the same code: the file is read into the settings the flags make, and a key that is left out has the default of its flag. A file holds no secret, only the paths of key files.

> `--config` and `--print-config` are on main and not in the latest release (v0.0.38). Every flag below works in every release. This release means the storage-node shape described here: a hot tier and a cold tier.

## A file for a storage node

A hot tier on SSD in front of a cold tier on spinning disks, both directories:

```toml
[server]
listen            = "192.0.2.10:9000"
proxy_id          = "tierstore-1"
bucket            = "tierstore"
access_key_file   = "/etc/tierstore/access.key"
read_only_key_files = ["/etc/tierstore/daemons.key"]

[hot_store]
kind   = "dir"
path   = "/srv/tierstore/hot"
budget = "800GiB"

[cold_store]
kind = "dir"
path = "/srv/tierstore/cold"
```

The same with S3 buckets for the tiers, and every other key left at its default spelled out:

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
kind               = "s3"
bucket             = "tierstore-hot"
endpoint           = "http://s3-hot.example:9000"
region             = "us-east-1"
credentials_file   = "/etc/tierstore/hot.credentials"
budget             = "1TiB"
high_water         = 80
low_water          = 70
critical           = 95
copy_parallel      = 16
lease_ttl          = "30s"
reconcile_interval = "5s"
usage_stale_after  = "2m"
journal_linger     = "20ms"
journal_retention  = "24h"

[cold_store]
kind             = "s3"
bucket           = "tierstore-cold"
endpoint         = "http://s3-cold.example:9000"
credentials_file = "/etc/tierstore/cold.credentials"
skip_checks      = []
```

Start it with the file and nothing else but the per-run flags:

```sh
/usr/libexec/kbf/tierstore --config /etc/tierstore/tierstore.toml --drain-lb-wait 10s --drain-deadline 15m
```

## Types

- A duration is a string of a whole number and `ms`, `s`, `m` or `h`: `250ms`, `15m`.
- A size is an integer of bytes, or a string of a whole number and `B`, `KiB`, `MiB`, `GiB` or `TiB`: `"500GiB"`.
- A path is a string. A list of paths is an array, and the flag it stands for is given once for each element.
- A number above 9223372036854775807 cannot go in a file; as a flag it is accepted.

## `[server]`

| Key | Flag | Type, default | What |
|---|---|---|---|
| `server.listen` | `--listen` | address, required | where the S3 API, `/healthz`, `/readyz`, `/usagez` and `/metricz` are served |
| `server.proxy_id` | `--proxy-id` | string, required | names this process's journal and lease in the hot tier; keep it across restarts and give each process over the same tiers its own |
| `server.bucket` | `--bucket` | string, required | the one bucket clients see; not `healthz`, `readyz`, `usagez` or `metricz` |
| `server.region` | `--region` | string, `us-east-1` | the region in the credential scope of a request's signature (not the tiers' regions) |
| `server.access_key_file` | `--access-key-file` | path, required | the full key file |
| `server.read_only_key_files` | `--read-only-key-file` | paths, none | read-only key files |
| `server.presign_key_files` | `--presign-key-file` | paths, none | presigning key files |
| `server.gc_key_files` | `--gc-key-file` | paths, none | GC key files |
| `server.scoped_key_files` | `--scoped-key-file` | paths, none | scoped key files |
| `server.presign_max_expires` | `--presign-max-expires` | duration, `15m` | the longest expiry of any presigned URL |
| `server.max_request_bytes` | `--max-request-bytes` | bytes, 268435456 (256 MiB) | the longest body of a request: the longest object and the longest part. A body over it is refused with 413 and nothing is stored |
| `server.max_concurrent_puts` | `--max-concurrent-puts` | count, 32 | the most put bodies and parts taken at once; one more is 503 `SlowDown` with `Retry-After: 1` |
| `server.put_min_bytes_per_sec` | `--put-min-bytes-per-sec` | bytes a second, 1024 | the least average rate of a body, counted over the time spent waiting for the client; 0 turns it off |
| `server.put_rate_grace` | `--put-rate-grace` | duration, `30s` | the time spent waiting for the client before that rate is held |
| `server.body_idle_timeout` | `--body-idle-timeout` | duration, `30s` | how long a body or an answer may go without a byte; more than zero |
| `server.s3_request_timeout` | `--s3-request-timeout` | duration, `60s` | the limit on a request to an `s3` tier (both tiers); more than zero. Set it longer than the longest multipart completion your store takes |
| `server.backend_check_wait` | `--backend-check-wait` | duration, `5m` | how long the start-up self-test asks again of a tier that does not answer; `0s` asks once |

## `[hot_store]` and `[cold_store]`

Both tables have `kind` and the keys it needs. `kind` is `memory` (no other key), `dir` (needs `path`) or `s3` (needs `bucket`, `endpoint` and `credentials_file`; `region` is optional). As flags, a tier is `--hot` or `--cold` with `memory`, `dir:<path>` or `s3:<bucket>`, and the S3 keys are the flags below. A key the kind does not take is refused.

| Key | Flag | Type, default | What |
|---|---|---|---|
| `hot_store.kind`, `hot_store.path`, `hot_store.bucket` | `--hot` | required | the hot tier |
| `cold_store.kind`, `cold_store.path`, `cold_store.bucket` | `--cold` | required | the cold tier. Multipart uploads go straight to it, so it must do them (all three kinds do) |
| `hot_store.endpoint` | `--hot-endpoint` | `http://host[:port]`, required for `s3` | the hot tier's store. No path; TLS is not built yet |
| `hot_store.region` | `--hot-region` | string, `us-east-1` | the signing region of the hot tier's store |
| `hot_store.credentials_file` | `--hot-credentials-file` | path, required for `s3` | the key file of the hot tier's store |
| `cold_store.endpoint` | `--cold-endpoint` | `http://host[:port]`, required for `s3` | the cold tier's store |
| `cold_store.region` | `--cold-region` | string, `us-east-1` | the signing region of the cold tier's store |
| `cold_store.credentials_file` | `--cold-credentials-file` | path, required for `s3` | the key file of the cold tier's store |
| `cold_store.skip_checks` | `--skip-backend-check` | list of `CHECK=REASON`, none | the start-up check that is left out, with its reason. Only `multipart-conditional` can be skipped, and it needs a reason; it runs on the cold tier |

Only `[hot_store]` has these, since only a process with a hot tier has them:

| Key | Flag | Type, default | What |
|---|---|---|---|
| `hot_store.budget` | `--hot-bytes` | size, required | the bytes the hot tier is meant to hold. The water marks are shares of it |
| `hot_store.high_water` | `--high-water` | percent, 80 | above this share of the budget, eviction begins |
| `hot_store.low_water` | `--low-water` | percent, 70 | eviction goes on down to this share |
| `hot_store.critical` | `--critical` | percent, 95 | above this share a put waits a little first and a read of a cold object is not copied back |
| `hot_store.journal_linger` | `--journal-linger` | duration, `20ms` | how long a batch of journal entries waits for more |
| `hot_store.journal_retention` | `--journal-retention` | duration, `24h` | how long an applied journal object is kept |
| `hot_store.copy_parallel` | `--copy-parallel` | count, 16 | the most objects the reconciler copies at once |
| `hot_store.lease_ttl` | `--lease-ttl` | duration, `30s` | how long the reconciler's lease lasts without a renewal |
| `hot_store.reconcile_interval` | `--reconcile-interval` | duration, `5s` | how often the reconciler works; shorter than the lease |
| `hot_store.usage_stale_after` | `--usage-stale-after` | duration, `2m` | how old the published usage may be before `/usagez` says 503; longer than the lease |

The water marks are percents from 1 to 100. The start refuses `low_water` at or above `high_water`, `high_water` above `critical`, a `reconcile_interval` at or above `lease_ttl`, and a `usage_stale_after` at or below `lease_ttl`.

## `[oci]`

The read-only OCI registry front. The table is the front: leave it out for a server with none. `listen` is needed in it and the other keys are refused without it.

```toml fragment
[oci]
listen              = "192.0.2.10:5000"
tls_cert_file       = "/etc/tierstore/oci.crt"
tls_key_file        = "/etc/tierstore/oci.key"
prefix              = "oci/"
max_connections     = 64
connection_lifetime = "1h"
```

| Key | Flag | Type, default | What |
|---|---|---|---|
| `oci.listen` | `--oci-listen` | address | where the front listens. With a certificate and key it is HTTPS; with neither it is plain HTTP, and off loopback the start prints one warning line |
| `oci.tls_cert_file` | `--oci-tls-cert` | path | the certificate chain (PEM), leaf first. It must carry an IP subjectAltName for the listen address. Both this and the key, or neither |
| `oci.tls_key_file` | `--oci-tls-key` | path | the private key (PEM); a file any user can read is refused |
| `oci.prefix` | `--oci-prefix` | string, `oci/` | the key prefix, ending in `/`; not inside `_journal/` or `_tiering/` |
| `oci.max_connections` | `--oci-max-connections` | count, 64 (at least 1) | connections open at once |
| `oci.connection_lifetime` | `--oci-connection-lifetime` | duration, `1h` | the longest a connection lives |

The front reads objects at `<prefix>blobs/sha256/<hex>` and serves GET and HEAD of `/v2/`, of manifests by digest and of blobs by digest. It is anonymous: anyone who can connect can read any digest under the prefix, so the network decides who may. Tags are not served. The certificate and key are read once; a new certificate takes a restart.

## `[[policy]]`

`[[policy]]` has a `prefix` (empty for all the rest, or ending in `/`) and `cache`. A policy is read and checked and has no effect: `policy.cache = true` is refused, because there is no cache to ask for yet. Two non-empty prefixes may not nest, a prefix may not be listed twice, and none may lie under `_journal/` or `_tiering/`.

## The flags that stay flags

These describe one run, not the data path, and have no key:

| Flag | Default | What |
|---|---|---|
| `--config` | none | the file |
| `--print-config` | off | print the file for the flags given, and exit |
| `--drain-lb-wait` | `10s` | on SIGTERM, how long `/readyz` says 503 while the server goes on serving |
| `--drain-deadline` | `15m` | then how long it waits for requests in flight |

`--help` and `--version` are the usual. A start that gives `--config` and any other flag but `--drain-lb-wait` and `--drain-deadline` is refused with exit status 2.

## What the loader refuses

Each of these exits with status 2 and names the file and the key (a syntax error names the line and the column, never the value):

- a key or a table the format does not have, and a value of the wrong type;
- a key that holds a secret: keys and credentials are paths of files whose names end in `_file` or `_files`; `secret_access_key`, `access_key_id`, `password`, a name with `token` in it and the like are refused anywhere;
- a file without both `[hot_store]` and `[cold_store]`, and the table `[local_cache]`;
- a key a tier's `kind` does not take, a `dir` tier without `path`, an `s3` tier without `bucket`, `endpoint` or `credentials_file`, and a key only one tier has in the other (`budget` and the reconciler's keys belong to `[hot_store]`, `skip_checks` to `[cold_store]`);
- a water mark outside 1 to 100, and a budget, count or limit of zero;
- an `[oci]` table without `listen`, with only one of the certificate and the key, or with `max_connections = 0`;
- a `[[policy]]` that breaks the rules above.

## Moving from flags to a file

`tierstore --print-config`, given the flags of a unit and not `--config`, prints on stdout the file that is equivalent to them and exits 0 without starting anything. It names key files by path and never prints what is in them, and the file it prints is loaded back before it is printed, so it loads. Take the flags out of `ExecStart=`:

```sh
/usr/libexec/kbf/tierstore --print-config --listen 192.0.2.10:9000 ... > /etc/tierstore/tierstore.toml
```

Then change `ExecStart=` to the `--config` line above, keeping `--drain-lb-wait` and `--drain-deadline`. It does not convert anything else in a unit. If `--print-config` cannot write its output (a closed pipe, a full disk) it exits 1; a refusal of the flags exits 2.

## Not in a release yet

- **The node-local cache table.** The file will have a table for a read-through cache on local disk in front of an upstream, and a write-through setting for it. On main the table name `[local_cache]` is reserved and refused as not supported yet, and the file must have a hot tier and a cold tier.
- **Several upstream endpoints.** A cache node listing more than one upstream, with failover between them.
