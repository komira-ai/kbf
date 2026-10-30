# tierstore

An S3 front over a hot and a cold tier. It serves one bucket, path-style, and checks SigV4 in the
`Authorization` header (and presigned PUT URLs). The packaging notes, the flags and the key files
are in `packaging/tierstore/README.md`; the roles of a key are in `src/server/keys.rs`.

## Flags or a file

The settings that describe the data path are flags, or one TOML file given with `--config <path>`
(`[server]`, `[hot_store]`, `[cold_store]` and `[[policy]]`; `src/server/config.rs`). The two make
the same settings and run the same code. `--print-config`, given the flags of a start, prints the
file that is equivalent to them. A file holds no secret, only the paths of key files, and refuses
a key or a table it does not know. The format, the table of the key of each flag and the rules are
in "The configuration file" in `packaging/tierstore/README.md`; the flags stay accepted for one
release along with the file.

## Metrics

`GET /metricz` on the S3 listener (not signed; a bucket cannot be called `metricz`) is the
Prometheus text of the counters of the server. All are counters since the process started. A
storage node with a hot and a cold tier shows the block below; a build node that runs a local
cache shows the cache's `tierstore_cache_*` block (see "Local cache" below). A server that has
both shows both blocks, one after the other.

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
| `tierstore_bytes_read_total` | `tier` | bytes of object data gets read from the tier: for `cold`, whole objects, whatever range was asked, so the ratio to `bytes_served` is the read amplification of ranged gets |
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

When the build-node cache (`local_cache`) is configured, its block of `tierstore_cache_*` series
also renders, after the proxy's. Until those series take the shared names, a query over every tier
reads them as follows:

| shared series | `tierstore_cache_*` series |
|---|---|
| `tierstore_hits_total{tier="local",op="get"}` | `tierstore_cache_hits_total` |
| `tierstore_misses_total{tier="local",op="get"}` | `tierstore_cache_misses_total` |
| `tierstore_hits_total{tier="local",op="head"}`, `tierstore_misses_total{tier="local",op="head"}` | `tierstore_cache_head_hits_total`, `tierstore_cache_head_misses_total` |
| `tierstore_bytes_served_total{tier="local"}` | `tierstore_cache_bytes_served_local_total` |
| `tierstore_bytes_served_total{tier="cold"}` (the cache's upstream is the cold store) | `tierstore_cache_bytes_served_upstream_total` |
| `tierstore_reads_total{tier="cold",op}` | `tierstore_cache_upstream_reads_total{op}` |
| `tierstore_bytes_read_total{tier="cold"}` | `tierstore_cache_upstream_bytes_read_total` |

Its hit rate is `rate(tierstore_cache_hits_total[5m]) / (rate(tierstore_cache_hits_total[5m]) +
rate(tierstore_cache_misses_total[5m]))`. `tierstore_cache_upstream_puts_total`, `_put_bytes_total`
and `_put_failures_total` are writes to the cold side, which the proxy's block does not count (it
counts reads), so they have no counterpart here; neither do the cache's fills, skips and evictions.

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
