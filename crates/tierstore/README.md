# tierstore

A generic hot/cold object store over S3-compatible backends. It knows nothing of kbf: its normal
dependencies include no other crate of this workspace unless the `s3` feature is on
(`crates/kbf-integration-test/tests/layering.rs` keeps it that way), so it can move to a repository
of its own.

- The library: the `Backend` trait (create-only put, `If-Match` put, ranged get, head, delete, list,
  multipart), an in-memory and a directory backend, `Proxy` (a journal per proxy, a get that copies a
  cold hit back, a full hot tier sending puts to the cold tier), `Reconciler` (copies puts to the
  cold tier, applies deletes, evicts what the cold tier holds, publishes usage) and `CachedStore`.
- Feature `s3`: `S3Backend`, a bucket of an S3-compatible store as a `Backend`.
- Feature `server` (turns `s3` on): `server`, the S3 API front of a proxy, and the `tierstore`
  binary that runs a proxy, that front and a reconciler in one process.
  `packaging/tierstore/README.md` has the flags, the keys and the unit.

An S3 front over a hot and a cold tier. It serves one bucket, path-style, and checks SigV4 in the
`Authorization` header (and presigned PUT URLs). The packaging notes, the flags and the key files
are in `packaging/tierstore/README.md`; the roles of a key are in `src/server/keys.rs`.

## Flags or a file

The settings that describe the data path are flags, or one TOML file given with `--config <path>`
(`[server]`, `[hot_store]`, `[cold_store]`, `[oci]` and `[[policy]]`; `src/server/config.rs`). The two make
the same settings and run the same code. `--print-config`, given the flags of a start, prints the
file that is equivalent to them. A file holds no secret, only the paths of key files, and refuses
a key or a table it does not know. The format, the table of the key of each flag and the rules are
in "The configuration file" in `packaging/tierstore/README.md`; the flags stay accepted for one
release along with the file.

## A build node: a read cache

A file with `[local_cache]` and `[cold_store]` and no `[hot_store]` runs the same binary as a read
cache in front of one upstream (`src/server/local_cache/`, the file's tables in
`src/server/config_cache.rs`): GET and HEAD of the keys a `[[policy]]` with `cache = true` holds are
served from a directory of raw files (`<path>/obj/<aa>/<bb>/<hex>`), filled from the upstream on a
miss, hashed as they are filled and evicted oldest-use first under a byte budget and a free-space
floor; every other call is refused 403. It has no journal and no lease. The keys of the table and
the rules are in "A build node" in `packaging/tierstore/README.md`; `GET /metricz` shows its
counters.

With `write_through = true` in `[local_cache]` (version 0.2.0 on; `tierstore --version` shows it)
the node also takes create-only puts, presigned PUTs, multipart uploads and deletes, and sends
each to the upstream (`src/server/local_cache/write.rs`) before it answers; it keeps no copy of
what is written. The roles of the keys apply as on a storage node. The start tests the upstream's
create-only and multipart-conditional answers with the self-test, so the upstream key must be one
that writes (exit status 3 if not). `tierstore --config <path> --check-config` checks a file as a
start does, without touching the network, and exits 0 and prints nothing, or prints the refusal and
exits 2.

## Metrics

`GET /metricz` on the S3 listener (not signed; a bucket cannot be called `metricz`) is the
Prometheus text of the counters of the process. All are counters since the process started.
`/metricz` renders whichever blocks the process has: the proxy's (a storage node, with a hot and
a cold tier; the block below), the OCI read front's, and the local cache's `tierstore_cache_*`
block (a build node; see "Local cache" below). A cache node and a storage node are different
processes: a file with `[local_cache]` is refused together with `[hot_store]`, so one process does
not have both the proxy's and the cache's block.

The block of the proxy is one labelled family per measure, so one query serves every tier. The
labels have a few fixed values, never a key or a client: `tier` is `hot` or `cold` here (`local`
is the tier of the build node's cache), `op` is `get` or `head`.

| name | labels | meaning |
|---|---|---|
| `tierstore_hits_total` | `tier`, `op` | reads the tier had the object for |
| `tierstore_misses_total` | `tier`, `op` | reads the tier did not have it for (a hot miss goes on to the cold tier) |
| `tierstore_read_errors_total` | `tier`, `op` | reads the tier failed with anything but "not found" (the tier is down, refused, timed out) |
| `tierstore_reads_total` | `tier`, `op` | reads that reached the tier: hits, misses and errors. For `tier="cold"` these are the reads that had to go to the cold store |
| `tierstore_bytes_served_total` | `tier` | bytes of object data that gets answered, by the tier that gave them (the range, if one was asked) |
| `tierstore_bytes_read_total` | `tier` | bytes of object data gets read from the tier: for `cold`, whole objects whatever range was asked (the range alone when the hot tier is critical), so the ratio to `bytes_served` is the read amplification of ranged gets |
| `tierstore_promotions_total` | none | objects a get copied from the cold tier into the hot tier |
| `tierstore_promoted_bytes_total` | none | bytes of those objects: the whole object, whatever range the get asked |
| `tierstore_promotions_skipped_total` | `reason` = `critical`, `full`, `raced`, `failed` | copies not made, by reason |

What the counts mean:

- An S3 GET reaches the proxy as a head (for the size and the conditions) and then a get. A GET
  that is answered at the head shows only under `op="head"`: a key that exists nowhere, a
  conditional that answers 304 or 412, a range that cannot be satisfied. So `op="head"` is a
  lookup rate: it mixes the client's HEADs with the heads that start every GET. `op="get"`
  counts only the reads that go on to fetch data.
- A hit or a miss is counted when the tier answers, an error when it fails with anything but
  "not found"; an error is neither a hit nor a miss, so a tier that is down does not lower its
  hit rate and shows in `read_errors_total`.
- Bytes served are counted when the proxy hands them to the front, so a client that
  disconnects while they are sent still has them counted.
- `critical` is a skip because the hot tier is at or above its critical mark by the usage the
  reconciler published (the budget included), and also while the proxy takes the hot tier to be
  full because it answered an earlier copy with "full" a moment ago. `full` is the hot tier
  answering this copy with "full". `raced` is another read having copied the object first: two
  concurrent cold gets of one object read it twice and promote it once, the second counting
  `raced`. `failed` is any other error of the copy.
- `promotions_total` counts a copy when the hot tier has taken it, before its entry is appended
  to the journal, which can fail; the reconciler then never learns of the copy and the object
  can be deleted from the hot tier without a trace in these counters.
- A get that is dropped while it copies has counted its cold hit and bytes, and neither a
  promotion nor a skip, so promotions plus skips can be less than cold hits.
- A ranged get while the hot tier is at or above its critical mark reads only the range from the
  cold tier: `bytes_read` is the range, nothing is promoted, and a `critical` skip is counted.
- A head promotes nothing.

PromQL, over the reads that fetch data (`op="get"`):

```
# hot hit rate
sum(rate(tierstore_hits_total{tier="hot",op="get"}[5m]))
  / sum(rate(tierstore_reads_total{tier="hot",op="get"}[5m]))
# the same with errors left out of the denominator
sum(rate(tierstore_hits_total{tier="hot",op="get"}[5m]))
  / (sum(rate(tierstore_hits_total{tier="hot",op="get"}[5m])) + sum(rate(tierstore_misses_total{tier="hot",op="get"}[5m])))
# share of gets answered by the cold tier
sum(rate(tierstore_hits_total{tier="cold",op="get"}[5m]))
  / sum(rate(tierstore_hits_total{op="get"}[5m]))
# cold reads per second
sum(rate(tierstore_reads_total{tier="cold",op="get"}[5m]))
# errors per second of a tier
sum by (tier) (rate(tierstore_read_errors_total[5m]))
# promotions per cold hit
sum(rate(tierstore_promotions_total[5m])) / sum(rate(tierstore_hits_total{tier="cold",op="get"}[5m]))
# bytes read from the cold tier per byte answered from it
rate(tierstore_bytes_read_total{tier="cold"}[5m]) / rate(tierstore_bytes_served_total{tier="cold"}[5m])
```

### Local cache

When the build-node cache (`local_cache`, see "A build node: a read cache" above) is configured,
its block of `tierstore_cache_*` series renders, after the proxy's and the OCI front's. Until those
series take the shared names, a query over every tier reads them as follows:

| shared series | `tierstore_cache_*` series |
|---|---|
| `tierstore_hits_total{tier="local",op="get"}` | `tierstore_cache_hits_total` |
| `tierstore_misses_total{tier="local",op="get"}` | `tierstore_cache_misses_total` |
| `tierstore_bytes_served_total{tier="local"}` | `tierstore_cache_bytes_served_local_total` |
| `tierstore_bytes_served_total{tier="cold"}` (the cache's upstream is the cold store) | `tierstore_cache_bytes_served_upstream_total` |

Its hit rate is `rate(tierstore_cache_hits_total[5m]) / (rate(tierstore_cache_hits_total[5m]) +
rate(tierstore_cache_misses_total[5m]))`. The cache's fills, skips and evictions have no
counterpart here.

## Roles of a key

| role | flag | may do |
|---|---|---|
| full | `--access-key-file` | everything the front serves |
| read-only | `--read-only-key-file` | get, head, list |
| presigning | `--presign-key-file` | sign presigned PUT URLs for one object each under a prefix |
| GC | `--gc-key-file` | list, head, delete and abort uploads under one prefix |
| scoped | `--scoped-key-file` | read, create and delete under the prefixes of three lists |

### Scoped keys

A scoped key's file is JSON of exactly `access_key_id`, `secret_access_key`, `read_prefixes`,
`write_prefixes` and `delete_prefixes`; the lists may be empty, but not all three, and every prefix
ends in `/`, is not, or under, or over `_journal/` or `_tiering/`, and has neither as a segment.

A call whose key or listing `prefix` has `..` anywhere in it is refused, even inside a prefix.
The server library does not itself force the access key ids of its keys to differ (`ServerConfig`
is plain data); the command line does, and exits 2.

- `read_prefixes`: GetObject, HeadObject, ListObjectsV2 and ListMultipartUploads (a listing's
  `prefix` parameter must be inside one);
- `write_prefixes`: create-only PutObject (`If-None-Match: *` or no condition; `412` if the key
  exists) and the calls of a multipart upload (create, part, complete, abort);
- `delete_prefixes`: DeleteObject.

A scoped key never replaces (`If-Match`) and never copies (`x-amz-copy-source`). Everything else is
`403 AccessDenied`. Two uses:

- the frontend's pack store: `read_prefixes: ["cas/"]`, `write_prefixes: ["cas/f/<writer>/"]`,
  `delete_prefixes: []` (add the writer prefix to the delete list to let the frontend remove its
  self-test object and the object of a failed write);
- build history, `--history-objects 's3://<bucket>/kbf-history?endpoint=...&conditional-put=true'`
  (without the last parameter a retried chunk gets `412` for good): `read_prefixes`
  and `write_prefixes` of `["kbf-history/"]`, `delete_prefixes: []`.

See "Scoped keys" in `packaging/tierstore/README.md` for the table of calls.

## The OCI read front

With `--oci-listen ADDR` the binary also serves the objects of the store as a read-only container
registry (`server::oci`), so that podman and other registry clients pull an image by digest from the
same tiers. It runs in one of two modes:

- **HTTPS**, with `--oci-tls-cert PATH --oci-tls-key PATH`, at any address.
- **Plain HTTP**, with neither flag, at any address. A node's own container engine pulls from the
  front of its own store over loopback (127/8 or `::1`), so the traffic never leaves the host.
  Off loopback (`0.0.0.0` and `::` included) the front still starts, and prints one warning line
  on stderr saying it serves plain HTTP on a non-loopback address, since a plain connection can be
  read and changed on its path: give the certificate and key there. One of the two flags without
  the other is refused (exit 2).

The start line and `/metricz` say which mode runs.

In a file given with `--config`, the front is the optional `[oci]` table (`listen`, `tls_cert_file`,
`tls_key_file`, `prefix`, `max_connections`, `connection_lifetime`), with the rules of the flags
(`listen` is needed in it, the certificate and the key go together, `max_connections` is at least
1); the `--oci-*` flags are refused with the file, and `--print-config` writes them as the table.

- `GET` and `HEAD` of `/v2/`, `/v2/<name>/manifests/sha256:<hex>` and
  `/v2/<name>/blobs/sha256:<hex>`; anything else is 404, and any other method 405.
- An object is at `<--oci-prefix>blobs/sha256/<hex>` (`oci/` by default), manifests included. The
  repository name in a URL is checked against the distribution grammar and then ignored: one
  content-addressed namespace.
- Anonymous: no token is asked for and `Authorization` is ignored. Only digest-named objects under
  the prefix can be read.
- A manifest is checked against its digest before it is sent; a blob is hashed as it is read and cut
  short, not delivered whole, if it is not its digest. Tags are not served. A manifest is measured
  and then read in two store calls, so one replaced between them can be answered 500
  `DIGEST_INVALID` once; a retry succeeds.
- Memory is bounded by `--oci-max-connections` (64 by default) times the buffer a connection holds,
  up to 4 MiB (a blob chunk or a manifest).
- In HTTPS mode the certificate must carry an IP subjectAltName for the listen address (clients dial
  an IP), checked at start. The files are read once: a new certificate takes a restart. The
  connection limits, timeouts and drain are the same in both modes (a plain connection has no
  handshake to time).
- `GET /metricz` on the S3 listener has the counters.

Putting an image in, and pulling it with podman and js-oci, are in `packaging/tierstore/README.md`
("The OCI read front"); the behaviour is specified by the tests in `tests/oci_front` (over real TLS and over plain HTTP, both
on loopback) and `src/server/oci`.
