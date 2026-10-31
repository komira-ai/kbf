# tierstore metrics and PromQL reference

This page lists every series tierstore exports, says exactly what increments each one and what it
does not count, and gives the PromQL queries used to measure tierstore. It is written for operators who scrape a node and for anyone who repeats those measurements.

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

- Every series except the gauges named as such is a counter since the process started. It never goes down while the process runs, and it restarts at 0 when the process does. Use `rate()` or `increase()` over a window at least four scrape intervals long; the queries on this page use `5m` for a 15 s scrape.
- Labels have a few fixed values each. None is a key, a bucket or a client, so the number of series stays small however busy the node is.
- Sum a ratio's numerator and denominator separately (`sum(rate(a)) / sum(rate(b))`), never average per-series ratios.

## The build-node cache (v1)

Every name in this section starts with `tierstore_cache_`, which the first column leaves out: `hits_total` is the series `tierstore_cache_` followed by `hits_total`. The text of the cache's series has no `# TYPE` line, so Prometheus stores them as untyped; `rate()` works on them anyway. An S3 GET reaches the cache as a HEAD (for the size and the conditions) and then a GET. These series count the GET, so a GET of a missing key, which the HEAD before it answers 404, is not in them.

### Cache reads and bytes

| name | type | labels | increments when |
|---|---|---|---|
| `hits_total` | counter | none | a GET of an object the cache holds is answered from disk, whole or ranged |
| `misses_total` | counter | none | a GET of a key the cache is set to keep is not answered from disk: the key is not held, or its file could not be read. |
| `bytes_served_local_total` | counter | none | bytes of the answers that counted as hits |
| `bytes_served_upstream_total` | counter | none | bytes of every answer that was not a hit: a pass-through read, and the read from disk that follows a fill |

What they do not count:

- A key the cache is not set to keep (outside every keep policy, or under a reserved prefix) is passed through to the upstream. It counts bytes under `bytes_served_upstream` and is neither a hit nor a miss.
- A miss counts once for each reader. Readers that share one fill each count a miss, and the fill counts once.
- A ranged GET of a key that is not held first asks the upstream for the size. If that HEAD fails (the key does not exist, the upstream is down) the GET counts no miss. A ranged GET of an object over the whole-object limit counts a miss and is passed through: it fills nothing and counts bytes under `bytes_served_upstream`. Any other GET of a key that is not held counts its miss before the fill starts, so a failed fill still shows as a miss. A file that could not be read from disk is a miss on the paths that count one.
- A range past the end of an object the cache holds is an error of the client: it is neither a hit nor a miss.
- A GET of a missing key never reaches the GET: the HEAD before it answers 404. So the hit and miss counters are rates of GETs that went on to read data.
- Bytes are counted when the cache hands them to the front, so a client that disconnects while they are sent still has them counted.
- `bytes_served_upstream` means "answered after going upstream", not "fetched". The read that follows a fill is served from the file the fill just wrote, and counts here although it came from disk. The bytes the cache fetched from the upstream are not exported yet.
- A ranged miss of an object at or under the whole-object limit fills the whole object, then answers the range. Its bytes served are the range; it fetches the object.

### Cache fills, failures and eviction

| name | type | labels | increments when |
|---|---|---|---|
| `fills_total` | counter | none | a fill wrote a new object into the cache |
| `fills_skipped_total` | counter | none | a fill was not started or not kept and nothing went wrong: every fill slot was running, or there was no room |
| `fills_skipped_busy_total` | counter | none | of the skipped fills, those that found every fill slot taken |
| `fill_failures_total` | counter | `reason`: `hash_mismatch`, `upstream_error`, `short_body`, `local_io`, `changed`, `aborted` | a fill ended without leaving an object in the cache |
| `evictions_total` | counter | `reason`: `budget`, `floor` | an object was evicted |
| `pinned_skips_total` | counter | none | an eviction pass passed over an object because it has more than one hard link |
| `discarded_at_start_total` | counter | none | a file was removed at start: a file the scan did not find whole, or what a fill left in the stage directory at the last stop |

The reasons of a failed fill:

- `hash_mismatch`: a key named by its content hashed to another name. The readers get an error and nothing is kept.
- `upstream_error`: the upstream failed with anything but "not found". A fill whose object does not exist counts nothing here.
- `short_body`: the upstream gave fewer bytes than the size it had said. The readers are served from the upstream.
- `local_io`: the cache directory could not be written.
- `changed`: the object was written over while it was read. Only keys not named by their content are checked for this.
- `aborted`: the fill task panicked or was cancelled before it ended. The readers that waited get a 503, and the failure counts once for the fill.

`fills_skipped_total` and `fill_failures_total` do not overlap: a short body, a local write error
and a changed object are failures, not skips. `evictions_total` counts objects, not bytes, and
`budget` is the share of an eviction pass that brings the cache under its low water mark while
`floor` is the share that restores the free space of the file system. `pinned_skips_total` goes
up once for each pinned object that a pass reaches. A pass walks the objects oldest first and
stops once it has freed enough, so it does not reach every pinned object, and it keeps rising
while an object that the passes reach stays pinned.
`discarded_at_start_total` is a counter that is set once, when the cache is opened, and stays
the same for the rest of the process.

### Cache gauges

These four are gauges, measured when `/metricz` is read.

| name | type | what it is |
|---|---|---|
| `bytes` | gauge | bytes of objects in the cache |
| `objects` | gauge | objects in the cache |
| `fs_free_bytes` | gauge | free bytes of the file system of the cache directory |
| `staging_files` | gauge | files in the stage directory now: fills that are running |

## The hot and cold tiers (v2)

The text of these series has no `# TYPE` line, so Prometheus stores them as untyped; `rate()` works on them anyway.

One labelled family per measure. `tier` is `hot` or `cold`, and `op` is `get` or `head`. The
value `local` of `tier` is reserved for the build node's cache.

| name | type | labels | increments when |
|---|---|---|---|
| `tierstore_hits_total` | counter | `tier`, `op` | the tier had the object |
| `tierstore_misses_total` | counter | `tier`, `op` | the tier answered "not found" (a hot miss goes on to the cold tier) |
| `tierstore_read_errors_total` | counter | `tier`, `op` | the tier failed with anything but "not found": it is down, refused, or timed out |
| `tierstore_reads_total` | counter | `tier`, `op` | a read reached the tier: its hits, misses and errors added up. For `tier="cold"`, the reads that had to go to the cold store |
| `tierstore_bytes_served_total` | counter | `tier` | bytes of object data that gets answered, by the tier that gave them, after a range is applied |
| `tierstore_bytes_read_total` | counter | `tier` | bytes of object data that gets read from the tier. For `cold`, the whole object, whatever range was asked, except a ranged read made while the hot tier is at its critical mark, which reads only the range |
| `tierstore_promotions_total` | counter | none | a get copied an object from the cold tier into the hot tier |
| `tierstore_promoted_bytes_total` | counter | none | bytes of those objects: the whole object, whatever range the get asked |
| `tierstore_promotions_skipped_total` | counter | `reason`: `critical`, `full`, `raced`, `failed` | a get found the object in the cold tier and did not copy it into the hot tier |

What they do not count, and how to read them:

- An S3 GET reaches the proxy as a HEAD and then a GET. A GET that is answered at the HEAD shows only under `op="head"`: a key that exists in neither tier, a conditional request that is answered 304 or 412, and a range that cannot be satisfied. So `op="head"` is a lookup rate: it mixes the clients' HEADs with the HEAD that starts every GET. `op="get"` counts only the reads that went on to fetch data.
- An error is neither a hit nor a miss. A tier that is down does not lower its hit rate; it shows in `read_errors_total`. A hot-tier error ends the read: the cold tier is not asked, so a hot outage does not show as cold reads.
- Bytes are counted when the proxy hands them to the front, so a client that disconnects while they are sent still has them counted. A range that cannot be satisfied after a cold read has counted the cold hit and the cold bytes read, and no bytes served.
- A ranged GET that misses the hot tier, while the hot tier is at its critical mark, asks the cold tier for the range alone and copies nothing: it counts a `critical` skip, and its `bytes_read` and `bytes_served` are both the range.
- Otherwise a ranged GET that misses the hot tier reads the whole object from the cold tier, promotes the whole object, and serves the range. Its `bytes_read` and `promoted_bytes` are the object's size, and its `bytes_served` is the range.
- A HEAD promotes nothing.
- `critical` is a skip because the hot tier is at or above its critical mark by the usage the reconciler last published, and also while the proxy takes the hot tier to be full because it answered an earlier copy with "full" a moment ago. `full` is the hot tier answering this copy with "full". `raced` is another read having copied the object first: two concurrent cold GETs of one object read it twice and promote it once, and the second counts `raced`. `failed` is any other error of the copy.
- `promotions_total` counts a copy when the hot tier has taken it, before the entry for it is appended to the journal. That append can fail, and then the object can later leave the hot tier with no trace in these counters.
- A GET that is dropped while it copies has counted its cold hit and its bytes, and neither a promotion nor a skip. So promotions plus skips can be fewer than cold hits.

### Tier queries

All of these use a `5m` window. Every `sum` is outside the `rate`. Shorten the window to see a
change sooner and lengthen it to smooth a quiet node; keep it at least four scrape intervals.

### Tier hit rates

The hot tier, over the reads that fetch data:

```promql
sum(rate(tierstore_hits_total{tier="hot",op="get"}[5m]))
  /
(sum(rate(tierstore_hits_total{tier="hot",op="get"}[5m]))
  + sum(rate(tierstore_misses_total{tier="hot",op="get"}[5m])))
```

The cold tier is asked only after a hot miss, so its hit rate is the share of those that the
cold store had:

```promql
sum(rate(tierstore_hits_total{tier="cold",op="get"}[5m]))
  /
(sum(rate(tierstore_hits_total{tier="cold",op="get"}[5m]))
  + sum(rate(tierstore_misses_total{tier="cold",op="get"}[5m])))
```

To count errors as non-hits, divide by `tierstore_reads_total` for the tier instead.

### Cold-read share of GETs

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

### Tier read amplification

Bytes read from the tier divided by bytes served.

For the cold tier, and then for all gets:

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

### Promotions per cold hit

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
ignoring (reason) group_left
sum(rate(tierstore_hits_total{tier="cold",op="get"}[5m]))
```

Bytes promoted per cold byte read, which is 1 when every cold read is promoted:

```promql
sum(rate(tierstore_promoted_bytes_total[5m]))
  /
sum(rate(tierstore_bytes_read_total{tier="cold"}[5m]))
```

## On `/metricz` today

`/metricz` shows the blocks of the sources a process has, one after the other: the proxy's (the
hot and cold tier series above), then the OCI read front's, then the cache's. A source a process
does not have shows nothing. When it has none, the whole text is one line, "this server has no
proxy, no OCI read front and no local cache". The OCI read front, when it is enabled, exports
these series:

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

### The drain

On main, not in v0.0.41. One series of the server itself, with a `# TYPE` line, shown on every
node whatever sources it has (it comes after the blocks of the proxy, the OCI front and the
cache):

| name | type | labels | increments when |
|---|---|---|---|
| `tierstore_drain_late_requests_total` | counter | none | in a drain, the server had shut its end of a connection for writing and its peer wrote after that: counted once for each such connection, not for each read |

It is 0 until a drain. After a restart it is the number of connections whose client sent a
request within one round trip of the end of the stream: the request was read, dropped and not
answered (see "Drain and restart" in [operations](operations.md)). A request that was on the wire
before the server shut its end is served and is not counted.

### v1: changes in the next release

The series of "The build-node cache (v1)" above change as follows, and the cache gains the series
of the sections after this one. They all start with `tierstore_cache_`.

- Each family gets one `# TYPE` line: a counter, or a gauge for `tierstore_cache_bytes`, `tierstore_cache_objects`, `tierstore_cache_fs_free_bytes` and `tierstore_cache_staging_files`. `tierstore_cache_discarded_at_start_total` is declared a counter that is set once, when the cache is opened.
- A read of a kept key that the cache does not serve is one miss on every path, counted right after the lookup and before any call to the upstream: a ranged GET whose size HEAD to the upstream then fails, a ranged GET of an object over the whole-object limit, and a fill that then fails. So a ranged GET whose HEAD fails now counts a miss. A range past the end of a held object is still neither a hit nor a miss.
- The bytes the cache fetched from the upstream are `tierstore_cache_upstream_bytes_read_total`, in the next section.

### v1: the cache's calls to the upstream

| name | type | labels | increments when |
|---|---|---|---|
| `tierstore_cache_upstream_reads_total` | counter | `op`: `get`, `head`, `list`, `list_uploads` | the cache sends one such call to the upstream |
| `tierstore_cache_upstream_bytes_read_total` | counter | none | bytes that the cache's upstream GETs brought back |
| `tierstore_cache_head_hits_total` | counter | none | a HEAD the cache is asked for is answered from its index: a client's HEAD, or the S3 front's HEAD before a GET |
| `tierstore_cache_head_misses_total` | counter | none | the same HEADs, sent to the upstream |

What they do not count:

- A fill reads the upstream in ranged chunks, so `op="get"` counts calls, not objects: an object of 350 bytes filled in chunks of 100 is four GET calls. It also counts every pass-through read.
- `op="head"` counts the cache's own HEADs as well as the clients': the size before a ranged fill, the two HEADs of a fill, the HEAD before a put, and the HEAD after one is acknowledged. `head_hits_total` and `head_misses_total` count only the HEADs the S3 front sends to the cache, which includes the HEAD that the front sends before each GET. So `head_hits_total + head_misses_total` is a lookup rate, not a rate of HEAD requests from clients.
- A listing counts as one `list` call, and each page of a paged listing counts as one more. Each call of `list_uploads` counts one.
- Not counted at all: deletes, creating and aborting uploads, sending a part, the start-up probe, and retries made inside a backend wrapper.
- `upstream_bytes_read_total` counts a GET that returned. The bytes of a GET that failed are not counted, and the bytes of a GET that returned are counted even if the fill then fails (a short body, a hash mismatch, a changed object) or is dropped.

### v1: writes through the cache

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
- `bad_digest`: the body did not hash to the name of the object. When the upstream had already taken the put, it counts as a put and as a `bad_digest` failure.
- `other`: any other failure.

What they do not count:

- Before sending a put, the cache sends the upstream a HEAD. A key the HEAD finds is answered "already exists" without a put reaching the upstream: it counts in `write_exists_total` only, not as `put_failures` with reason `exists`. That reason counts the races the HEAD did not see.
- A HEAD before a put that fails with anything but "not found" fails the put although no put was sent. It counts under `put_failures` with the reason its error maps to (`unavailable`, `full`, `other`), and in `upstream_reads_total{op="head"}`.
- A part that fails, a delete, an abort and a part's retry are not failures here. A part that is accepted counts its bytes even if the upload is never completed, and a client's retry of an accepted part counts them twice.
- A completion counts one put and no bytes of its own: the bytes are those of its parts.
- `writes_through_total` counts whole-body puts only, `upstream_puts_total` counts those and completions, and `write_exists_total` counts the HEAD's answer and the upstream's together.

### The queries

All of these use a `5m` window. Every `sum` is outside the `rate`. Shorten the window to see a
change sooner and lengthen it to smooth a quiet node; keep it at least four scrape intervals.

### Query: cache hit rate (v1)

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

### Query: one dashboard for both, the v1 cache as tier="local"

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

Only these three map to `tier="local"`. The cache's `upstream` series are not local: they
measure the cache's calls to the cold store, so on a build node they are the cold-side numbers
and map to `tier="cold"`:

| shared series | the cache's series | how they differ |
|---|---|---|
| `tierstore_bytes_read_total{tier="cold"}` | `tierstore_cache_upstream_bytes_read_total` | the cache's counts the chunks of fills and pass-through reads that returned, whole objects or not |
| `tierstore_reads_total{tier="cold",op="get"}` | `tierstore_cache_upstream_reads_total{op="get"}` | the cache's counts calls, and a fill makes one per chunk, so it is not a count of objects |
| `tierstore_reads_total{tier="cold",op="head"}` | `tierstore_cache_upstream_reads_total{op="head"}` | the cache's includes its own HEADs (fills, puts) |

Cold-side bytes read, one query for both deployments:

```promql
sum by (tier) (rate(tierstore_bytes_read_total{tier="cold"}[5m]))
or
label_replace(sum(rate(tierstore_cache_upstream_bytes_read_total[5m])), "tier", "cold", "", "")
```

The write-through counters (`upstream_puts`, `upstream_put_bytes`, `upstream_put_failures`) are
also toward the cold store and have no shared counterpart. `tierstore_cache_bytes_served_upstream_total`,
the fills and the evictions have none either, and `tierstore_cache_head_hits_total` counts
clients' HEADs, not `op="head"` of the shared family. The `local` values count a GET once per reader at the cache, and the `hot` and `cold`
values once per GET at the proxy, so compare the rates, not the totals.

### Query: cache read amplification (v1)

Bytes fetched from the upstream divided by bytes served. For the v1 cache, over all answers, and over the
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

### Query: write-through put failure rate by reason (v1)

Failures per second, by reason:

```promql
sum by (reason) (rate(tierstore_cache_upstream_put_failures_total[5m]))
```

As a share of the puts attempted (accepted, or failed at the upstream or at the HEAD before the
send):

```promql
sum by (reason) (rate(tierstore_cache_upstream_put_failures_total[5m]))
  /
ignoring (reason) group_left
(sum(rate(tierstore_cache_upstream_puts_total[5m]))
  + sum(rate(tierstore_cache_upstream_put_failures_total[5m])))
```

The denominator counts a bad-digest put that the upstream took twice, once as a put and once as
a failure, and does not count puts refused because the HEAD before the send found the key (see
"v1: writes through the cache"). A completion that fails with "exists" counts in `put_failures` with reason `exists` and not in
`write_exists_total`, which only whole-body puts feed. The share above counts such a completion as
a failure, and the share below leaves it out, so they differ slightly when completions race. For the share of puts refused because the key exists, add
`write_exists_total`:

```promql
sum(rate(tierstore_cache_write_exists_total[5m]))
  /
(sum(rate(tierstore_cache_upstream_puts_total[5m]))
  + sum(rate(tierstore_cache_write_exists_total[5m]))
  + sum(rate(tierstore_cache_upstream_put_failures_total{reason!="exists"}[5m])))
```
