# tierstore metrics and PromQL reference

This page lists every series tierstore exports, says exactly what increments each one and what it
does not count, and gives the PromQL queries that the measurements in the tierstore white paper
use. It is written for operators who scrape a node and for anyone who repeats those measurements.

Every series is on `GET /metricz`, on the S3 listener, in the Prometheus text format. The path is
not signed, like `/healthz`, and a bucket cannot be called `metricz`. Point a scrape job at it:

```yaml
scrape_configs:
  - job_name: tierstore
    metrics_path: /metricz
    static_configs:
      - targets: ["localhost:9000"]
```

## The two deployments

**v1: a build-node cache in front of the cold store.** One tierstore process runs on a build node
with a local directory as a read-through cache over one upstream tierstore. It has no hot tier, no
journal and no reconciler. A read the cache holds is answered from disk; any other read goes to
the upstream, and a key the cache is set to keep is filled into the directory on the way. This is
the only tierstore hop in v1, and its series describe the cache's own work and its calls to the
upstream.

**v2: storage nodes with hot and cold tiers.** A storage node runs a proxy over a hot store and a
cold store. A read asks the hot tier first, and on a miss the cold tier. A get that finds the
object only in the cold tier reads the whole object and copies it into the hot tier (a promotion)
when the hot tier has room. Its series are one labelled family per measure, with the tier as a
label, so one query serves every tier.

A process runs one of the two shapes, so the series of one shape are absent from the other's
`/metricz`. A series that is absent has not been counted at zero: a query over it returns
nothing, not 0.

## Reading the series

- Every series except the gauges named as such is a counter since the process started. It never
  goes down while the process runs, and it restarts at 0 when the process does. Use `rate()` or
  `increase()` over a window at least four scrape intervals long; the queries on this page use
  `5m` for a 15 s scrape.
- Labels have a few fixed values each. None is a key, a bucket or a client, so the number of
  series stays small however busy the node is.
- Sum a ratio's numerator and denominator separately (`sum(rate(a)) / sum(rate(b))`), never
  average per-series ratios.

## On `/metricz` today

The OCI read front, when it is enabled, exports these series. Without it, `/metricz` has one
comment line that says the front is not enabled.

| name | type | labels | what it counts |
|---|---|---|---|
| `tierstore_oci_requests_total` | counter | `route`: `ping`, `manifest`, `blob`, `other`; `outcome`: `ok`, `partial`, `not_found`, `invalid`, `not_allowed`, `range_not_satisfiable`, `failed` | requests, by the status they were answered with (`ok` 200, `partial` 206, `not_found` 404, `invalid` 400, `not_allowed` 405, `range_not_satisfiable` 416, `failed` 5xx) |
| `tierstore_oci_bytes_served_total` | counter | none | bytes of blobs read for responses |
| `tierstore_oci_digest_mismatches_total` | counter | none | objects whose bytes were not the digest they are named by |
| `tierstore_oci_front_info` | gauge | `mode`: `https`, `http` | always 1; the label is the mode the front runs in |

## Not in a release yet

Everything below describes series that are being added and are not in a released tierstore. The
names and labels are those of the changes under review and can still change; this page is
updated when they land. Do not build alerts on them until they are in a release.

The text of these series carries no `# TYPE` line, so Prometheus stores them as untyped.
`rate()` works on them anyway.

### v1: the build-node cache

The cache's series start with `tierstore_cache_`. An S3 GET reaches the cache as a HEAD (for the
size and the conditions) and then a GET, so some series count the HEAD and some the GET; the
tables say which.

#### Reads and bytes

| name | type | labels | increments when |
|---|---|---|---|
| `tierstore_cache_hits_total` | counter | none | a GET of an object the cache holds is answered from disk, whole or ranged |
| `tierstore_cache_misses_total` | counter | none | a GET of a key the cache is set to keep is not answered from disk: the key is not held, or its file could not be read |
| `tierstore_cache_bytes_served_local_total` | counter | none | bytes of the answers that counted as hits |
| `tierstore_cache_bytes_served_upstream_total` | counter | none | bytes of every answer that was not a hit: a pass-through read, and the read from disk that follows a fill |

What they do not count:

- A key the cache is not set to keep (outside every keep policy, or under a reserved prefix) is
  passed through to the upstream. It counts bytes under `bytes_served_upstream` and is neither a
  hit nor a miss.
- A miss counts once for each reader. Readers that share one fill each count a miss, and the
  fill counts once.
- A ranged GET of a key that is not held first asks the upstream for the size. If that HEAD
  fails (the key does not exist, the upstream is down) the GET counts no miss. A GET without a
  range counts its miss before the fill starts, so a failed fill still shows as a miss.
- A ranged GET of an object over the whole-object limit is a miss that is passed through: it
  counts a miss and bytes under `bytes_served_upstream`, and fills nothing.
- A GET of a missing key never reaches the GET: the HEAD before it answers 404. It shows only in
  the HEAD series below, so the hit and miss counters are rates of GETs that went on to read
  data.
- Bytes are counted when the cache hands them to the front, so a client that disconnects while
  they are sent still has them counted.
- `bytes_served_upstream` means "answered after going upstream", not "fetched". The read that
  follows a fill is served from the file the fill just wrote, and counts here although it came
  from disk. What the cache fetched from the upstream is `tierstore_cache_upstream_bytes_read_total`
  below.
- A ranged miss of an object at or under the whole-object limit fills the whole object, then
  answers the range. Its bytes served are the range; its bytes fetched are the object.

#### Fills, failures and eviction

| name | type | labels | increments when |
|---|---|---|---|
| `tierstore_cache_fills_total` | counter | none | a fill wrote a new object into the cache |
| `tierstore_cache_fills_skipped_total` | counter | none | a fill was not started or not kept and nothing went wrong: every fill slot was running, or there was no room |
| `tierstore_cache_fills_skipped_busy_total` | counter | none | of the skipped fills, those that found every fill slot taken |
| `tierstore_cache_fill_failures_total` | counter | `reason`: `hash_mismatch`, `upstream_error`, `short_body`, `local_io`, `changed`, `aborted` | a fill ended without leaving an object in the cache |
| `tierstore_cache_evictions_total` | counter | `reason`: `budget`, `floor` | an object was evicted |
| `tierstore_cache_pinned_skips_total` | counter | none | an eviction pass passed over an object because it has more than one hard link |
| `tierstore_cache_discarded_at_start_total` | counter | none | a file was removed at start: a file the scan did not find whole, or what a fill left in the stage directory at the last stop |

The reasons of a failed fill:

- `hash_mismatch`: a key named by its content hashed to another name. The readers get an error
  and nothing is kept.
- `upstream_error`: the upstream failed with anything but "not found". A fill whose object does
  not exist counts nothing here.
- `short_body`: the upstream gave fewer bytes than the size it had said. The readers are served
  from the upstream.
- `local_io`: the cache directory could not be written.
- `changed`: the object was written over while it was read. Only keys not named by their content
  are checked for this.
- `aborted`: the fill task panicked or was cancelled before it ended. The readers that waited
  get a 503, and the failure counts once for the fill.

`fills_skipped_total` and `fill_failures_total` do not overlap: a short body, a local write error
and a changed object are failures, not skips. `evictions_total` counts objects, not bytes, and
`budget` is the share of an eviction pass that brings the cache under its low water mark while
`floor` is the share that restores the free space of the file system. `pinned_skips_total` goes
up once for each pinned object on every pass, so it keeps rising while an object stays pinned.
`discarded_at_start_total` is fixed once the process has started.

#### Gauges

These four are gauges, measured when `/metricz` is read.

| name | type | what it is |
|---|---|---|
| `tierstore_cache_bytes` | gauge | bytes of objects in the cache |
| `tierstore_cache_objects` | gauge | objects in the cache |
| `tierstore_cache_fs_free_bytes` | gauge | free bytes of the file system of the cache directory |
| `tierstore_cache_staging_files` | gauge | files in the stage directory now: fills that are running |

#### The cache's calls to the upstream

| name | type | labels | increments when |
|---|---|---|---|
| `tierstore_cache_upstream_reads_total` | counter | `op`: `get`, `head`, `list`, `list_uploads` | the cache sends one such call to the upstream |
| `tierstore_cache_upstream_bytes_read_total` | counter | none | bytes that the cache's upstream GETs brought back |
| `tierstore_cache_head_hits_total` | counter | none | a HEAD is answered from the cache's index |
| `tierstore_cache_head_misses_total` | counter | none | a HEAD is sent to the upstream |

What they do not count:

- A fill reads the upstream in ranged chunks, so `op="get"` counts calls, not objects: an object
  of 350 bytes filled in chunks of 100 is four GET calls. It also counts every pass-through read.
- `op="head"` counts the cache's own HEADs as well as the clients': the size before a ranged
  fill, the two HEADs of a fill, the HEAD before a put, and the HEAD after one is acknowledged.
  `head_hits_total` and `head_misses_total` count only the HEADs that clients (the S3 front)
  send to the cache, which includes the HEAD that the front sends before each GET. So
  `head_hits_total + head_misses_total` is a lookup rate, not a rate of HEAD requests from
  clients.
- A listing counts as one `list` call, and each page of a paged listing counts as one more.
  Each call of `list_uploads` counts one.
- Not counted at all: deletes, creating and aborting uploads, sending a part, the start-up probe,
  and retries made inside a backend wrapper.
- `upstream_bytes_read_total` counts a GET that returned. The bytes of a GET that failed are not
  counted, and the bytes of a GET that returned are counted even if the fill then fails (a short
  body, a hash mismatch, a changed object) or is dropped.

#### Writes through the cache

These series exist when the cache node is set to send writes to the upstream (write-through).
Without it, the cache is read-only and they stay 0.

| name | type | labels | increments when |
|---|---|---|---|
| `tierstore_cache_upstream_puts_total` | counter | none | the upstream accepted a create-only put, or the completion of a multipart upload |
| `tierstore_cache_upstream_put_bytes_total` | counter | none | bytes of the puts the upstream accepted, and of each part it accepted |
| `tierstore_cache_upstream_put_failures_total` | counter | `reason`: `exists`, `unavailable`, `full`, `bad_digest`, `other` | a put or a completion reached the upstream and failed |
| `tierstore_cache_writes_through_total` | counter | none | the upstream took a whole-body create-only put (not a completion) |
| `tierstore_cache_write_exists_total` | counter | none | a put was answered "already exists" |
| `tierstore_cache_keeps_expired_total` | counter | none | a put was answered before the copy kept on the node finished, because the copy took longer than the wait |
| `tierstore_cache_write_keep_skipped_total` | counter | `reason`: `budget`, `io`, `busy`, `head`, `deleted`, `slow`, `deadline` | the upstream took a put and the node did not keep a copy |

The reasons of a failed put:

- `exists`: the upstream's create-only put or completion found the object (412).
- `unavailable`: the upstream did not answer.
- `full`: the upstream had no room (507).
- `bad_digest`: the body did not hash to the name of the object. When the upstream had already
  taken the put, it counts as a put and as a `bad_digest` failure.
- `other`: any other failure.

What they do not count:

- Before sending a put, the cache sends the upstream a HEAD. A key the HEAD finds is answered
  "already exists" without a put reaching the upstream: it counts in `write_exists_total` only,
  not as `put_failures` with reason `exists`. That reason counts the races the HEAD did not see.
- A HEAD before a put that fails with anything but "not found" fails the put and is counted in
  no put series. It shows in `upstream_reads_total{op="head"}`.
- A part that fails, a delete, an abort and a part's retry are not failures here. A part that is
  accepted counts its bytes even if the upload is never completed, and a client's retry of an
  accepted part counts them twice.
- A completion counts one put and no bytes of its own: the bytes are those of its parts.
- `writes_through_total` counts whole-body puts only, `upstream_puts_total` counts those and
  completions, and `write_exists_total` counts the HEAD's answer and the upstream's together.

### v2: the hot and cold tiers

One labelled family per measure. `tier` is `hot` or `cold`, and `op` is `get` or `head`. The
value `local` of `tier` is reserved for the build node's cache, whose series are the ones above;
see "One dashboard for both" below.

| name | type | labels | increments when |
|---|---|---|---|
| `tierstore_hits_total` | counter | `tier`, `op` | the tier had the object |
| `tierstore_misses_total` | counter | `tier`, `op` | the tier answered "not found" (a hot miss goes on to the cold tier) |
| `tierstore_read_errors_total` | counter | `tier`, `op` | the tier failed with anything but "not found": it is down, refused, or timed out |
| `tierstore_reads_total` | counter | `tier`, `op` | a read reached the tier: its hits, misses and errors added up. For `tier="cold"`, the reads that had to go to the cold store |
| `tierstore_bytes_served_total` | counter | `tier` | bytes of object data that gets answered, by the tier that gave them, after a range is applied |
| `tierstore_bytes_read_total` | counter | `tier` | bytes of object data that gets read from the tier. For `cold`, the whole object, whatever range was asked |
| `tierstore_promotions_total` | counter | none | a get copied an object from the cold tier into the hot tier |
| `tierstore_promoted_bytes_total` | counter | none | bytes of those objects: the whole object, whatever range the get asked |
| `tierstore_promotions_skipped_total` | counter | `reason`: `critical`, `full`, `raced`, `failed` | a get found the object in the cold tier and did not copy it into the hot tier |

What they do not count, and how to read them:

- An S3 GET reaches the proxy as a HEAD and then a GET. A GET that is answered at the HEAD shows
  only under `op="head"`: a key that exists in neither tier, a conditional request that is
  answered 304 or 412, and a range that cannot be satisfied. So `op="head"` is a lookup rate: it
  mixes the clients' HEADs with the HEAD that starts every GET. `op="get"` counts only the reads
  that went on to fetch data.
- An error is neither a hit nor a miss. A tier that is down does not lower its hit rate; it
  shows in `read_errors_total`. A hot-tier error ends the read: the cold tier is not asked, so
  a hot outage does not show as cold reads.
- Bytes are counted when the proxy hands them to the front, so a client that disconnects while
  they are sent still has them counted. A range that cannot be satisfied after a cold read has
  counted the cold hit and the cold bytes read, and no bytes served.
- A ranged GET that misses the hot tier reads the whole object from the cold tier, promotes the
  whole object, and serves the range. Its `bytes_read` and `promoted_bytes` are the object's
  size, and its `bytes_served` is the range.
- A HEAD promotes nothing.
- `critical` is a skip because the hot tier is at or above its critical mark by the usage the
  reconciler last published, and also while the proxy takes the hot tier to be full because it
  answered an earlier copy with "full" a moment ago. `full` is the hot tier answering this copy
  with "full". `raced` is another read having copied the object first: two concurrent cold GETs
  of one object read it twice and promote it once, and the second counts `raced`. `failed` is
  any other error of the copy.
- `promotions_total` counts a copy when the hot tier has taken it, before the entry for it is
  appended to the journal. That append can fail, and then the object can later leave the hot
  tier with no trace in these counters.
- A GET that is dropped while it copies has counted its cold hit and its bytes, and neither a
  promotion nor a skip. So promotions plus skips can be fewer than cold hits.

### The queries

All of these use a `5m` window. Every `sum` is outside the `rate`. Shorten the window to see a
change sooner and lengthen it to smooth a quiet node; keep it at least four scrape intervals.

#### Hit rate per tier

The v1 local cache, from the cache's own series. Errors are not in either term, so a failing
upstream does not lower the rate:

```promql
sum(rate(tierstore_cache_hits_total[5m]))
  /
(sum(rate(tierstore_cache_hits_total[5m])) + sum(rate(tierstore_cache_misses_total[5m])))
```

The same by bytes, the share of answered bytes that came from the local directory:

```promql
sum(rate(tierstore_cache_bytes_served_local_total[5m]))
  /
(sum(rate(tierstore_cache_bytes_served_local_total[5m]))
  + sum(rate(tierstore_cache_bytes_served_upstream_total[5m])))
```

The v2 hot tier, over the reads that fetch data:

```promql
sum(rate(tierstore_hits_total{tier="hot",op="get"}[5m]))
  /
(sum(rate(tierstore_hits_total{tier="hot",op="get"}[5m]))
  + sum(rate(tierstore_misses_total{tier="hot",op="get"}[5m])))
```

The v2 cold tier is asked only after a hot miss, so its hit rate is the share of those that the
cold store had:

```promql
sum(rate(tierstore_hits_total{tier="cold",op="get"}[5m]))
  /
(sum(rate(tierstore_hits_total{tier="cold",op="get"}[5m]))
  + sum(rate(tierstore_misses_total{tier="cold",op="get"}[5m])))
```

To count errors as non-hits, divide by `tierstore_reads_total` for the tier instead.

#### One dashboard for both: the v1 cache as tier="local"

The shared series have `tier` and `op` labels and the cache's do not. `label_replace` adds the
label, and `or` joins the two shapes, so one panel, `sum by (tier)`, reads either deployment.
Hit rate by tier:

```promql
(
  sum by (tier) (rate(tierstore_hits_total{op="get"}[5m]))
  or
  label_replace(sum(rate(tierstore_cache_hits_total[5m])), "tier", "local", "", "")
)
/
(
  sum by (tier) (rate(tierstore_hits_total{op="get"}[5m]) + rate(tierstore_misses_total{op="get"}[5m]))
  or
  label_replace(
    sum(rate(tierstore_cache_hits_total[5m]) + rate(tierstore_cache_misses_total[5m])),
    "tier", "local", "", ""
  )
)
```

Bytes served by tier:

```promql
sum by (tier) (rate(tierstore_bytes_served_total[5m]))
or
label_replace(sum(rate(tierstore_cache_bytes_served_local_total[5m])), "tier", "local", "", "")
```

The mapping, series by series:

| shared series | the cache's series |
|---|---|
| `tierstore_hits_total{tier="local",op="get"}` | `tierstore_cache_hits_total` |
| `tierstore_misses_total{tier="local",op="get"}` | `tierstore_cache_misses_total` |
| `tierstore_bytes_served_total{tier="local"}` | `tierstore_cache_bytes_served_local_total` |

Only these three map. `tierstore_cache_bytes_served_upstream_total`, the fills, the evictions and
the upstream calls have no counterpart, and `tierstore_cache_head_hits_total` counts clients'
HEADs, not `op="head"` of the shared family. The `local` values count a GET once per reader at the cache, and the `hot` and `cold`
values once per GET at the proxy, so compare the rates, not the totals.

#### Cold-read share of GETs (v2)

Every GET asks the hot tier first, so the share of GETs that had to read the cold tier is the
cold reads over the hot reads:

```promql
sum(rate(tierstore_reads_total{tier="cold",op="get"}[5m]))
  /
sum(rate(tierstore_reads_total{tier="hot",op="get"}[5m]))
```

A hot error ends the read before the cold tier, so it is in the denominator and not the
numerator. For the share of GETs that the cold tier answered, use its hits:

```promql
sum(rate(tierstore_hits_total{tier="cold",op="get"}[5m]))
  /
sum(rate(tierstore_hits_total{op="get"}[5m]))
```

#### Read amplification

Bytes read from below divided by bytes served. For the v1 cache, over all answers, and over the
answers that went upstream:

```promql
sum(rate(tierstore_cache_upstream_bytes_read_total[5m]))
  /
(sum(rate(tierstore_cache_bytes_served_local_total[5m]))
  + sum(rate(tierstore_cache_bytes_served_upstream_total[5m])))
```

```promql
sum(rate(tierstore_cache_upstream_bytes_read_total[5m]))
  /
sum(rate(tierstore_cache_bytes_served_upstream_total[5m]))
```

The second is the fill amplification: it is above 1 when ranged misses fill whole objects, and
close to 1 when clients read whole objects. It is not exactly 1 for whole reads, because the read
that follows a fill counts its bytes under `bytes_served_upstream` and the fill's chunks under
`upstream_bytes_read`, and a fill that fails counts bytes fetched and none served.

For v2, the cold tier, and then all gets:

```promql
sum(rate(tierstore_bytes_read_total{tier="cold"}[5m]))
  /
sum(rate(tierstore_bytes_served_total{tier="cold"}[5m]))
```

```promql
sum(rate(tierstore_bytes_read_total[5m]))
  /
sum(rate(tierstore_bytes_served_total[5m]))
```

On the hot tier the two byte counters are equal, so the second query is 1 plus the extra bytes
that whole-object cold reads cost.

#### Write-through put failure rate by reason (v1)

Failures per second, by reason:

```promql
sum by (reason) (rate(tierstore_cache_upstream_put_failures_total[5m]))
```

As a share of the puts that reached the upstream (accepted or failed):

```promql
sum by (reason) (rate(tierstore_cache_upstream_put_failures_total[5m]))
  /
ignoring (reason) group_left
(sum(rate(tierstore_cache_upstream_puts_total[5m]))
  + sum(rate(tierstore_cache_upstream_put_failures_total[5m])))
```

The denominator counts a bad-digest put that the upstream took twice, once as a put and once as
a failure, and does not count puts refused by the HEAD before the send (see "Writes through the
cache"). For the share of puts refused because the key exists, add `write_exists_total`:

```promql
sum(rate(tierstore_cache_write_exists_total[5m]))
  /
(sum(rate(tierstore_cache_upstream_puts_total[5m]))
  + sum(rate(tierstore_cache_write_exists_total[5m]))
  + sum(rate(tierstore_cache_upstream_put_failures_total{reason!="exists"}[5m])))
```

#### Promotions per cold hit (v2)

```promql
sum(rate(tierstore_promotions_total[5m]))
  /
sum(rate(tierstore_hits_total{tier="cold",op="get"}[5m]))
```

This is at most 1. What keeps it under 1 is the skips, which add up to the rest, plus the GETs
that were dropped while they copied:

```promql
sum by (reason) (rate(tierstore_promotions_skipped_total[5m]))
  /
sum(rate(tierstore_hits_total{tier="cold",op="get"}[5m]))
```

Bytes promoted per cold byte read, which is 1 when every cold read is promoted:

```promql
sum(rate(tierstore_promoted_bytes_total[5m]))
  /
sum(rate(tierstore_bytes_read_total{tier="cold"}[5m]))
```
