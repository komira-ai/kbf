# Operations

## Starting

Start the unit by hand (see [install](install.md)). Before it opens the proxy or binds the address, the process runs the start-up self-test (below). When it is ready it prints one line on stdout:

```text
tierstore <version> listen=192.0.2.10:9000
```

With the OCI front on, the line gets ` oci-listen=<address> (https)` or ` (http)` after the listen address. The self-test's progress and every other message go to stderr, and so to the journal.

## Probes

Three paths of the S3 listener answer a GET or HEAD without a signature, so a bucket cannot have any of these names.

| Path | Answer | Use it for |
|---|---|---|
| `/healthz` | 200 `ok` while the process runs | liveness |
| `/readyz` | 200 `ready` until the drain begins, then 503 `draining` | a load balancer's health check |
| `/usagez` | 200 with the age of the hot tier's published usage, or 503 with the reason | alerting |

`/readyz` does not look at the usage: one stopped reconciler would otherwise make every node unready. `/usagez` is for alerting, not for a load balancer.

## Usage

The reconciler publishes the hot tier's usage every pass. `/usagez` says:

```text
usage published <N>s ago
no usage yet, <N>s since start
usage of the hot tier is stale: published <N>s ago, limit <M>s
usage of the hot tier is missing: none published in <N>s, limit <M>s
```

The first two are 200 and the last two 503. The limit is `--usage-stale-after` (`hot_store.usage_stale_after`, 2m by default). A server that starts before the reconciler's first pass is 200 for that long. Puts ignore a usage older than 30 seconds, as if the hot tier were not full, so a stopped reconciler means a hot tier that is never taken to be full. Puts are not refused; alert on `/usagez`.

## Eviction and the water marks

The hot tier's size is measured against `--hot-bytes` (`hot_store.budget`). Three shares of it matter:

- Above `--high-water` (80% by default) the reconciler evicts the oldest objects that the cold tier already holds, until the hot tier is down to `--low-water` (70%). It never evicts an object it has not copied.
- Above `--critical` (95%) a put waits a short time before it is written, and a read of an object that is only in the cold tier is served but not copied back into the hot tier.
- At the budget, or when the hot tier says it is full, new objects are written straight to the cold tier.

Eviction frees the hot tier only as fast as the cold tier takes copies, which `--copy-parallel` bounds. If the cold tier is slow or down, copies stall, nothing becomes evictable, and the hot tier climbs to the budget.

## What a full disk does

A `dir` hot tier whose disk is out of space, or an `s3` hot tier that answers 507, is a full hot tier: the put that met it goes to the cold tier where nothing of its body was taken yet, and the tier is taken to be full for a moment. A put that had already streamed some of its body into the hot tier fails, and the client's retry goes to the cold tier. Reads use both tiers throughout. The hot tier is used again once the usage falls. Objects written straight to the cold tier are not journaled and not copied; the reconciler has nothing to do for them.

If the cold tier is a full disk, puts that reach it fail. A tier that is full at start still starts; the start-up checks that need a write are skipped, with a line that says so. Size `--hot-bytes` below the real capacity of the SSD: the budget is what the process compares with, not what the file system has.

## Drain and restart

On SIGTERM or SIGINT (what `systemctl stop` sends) the process drains:

1. `/readyz` answers 503 at once, and for `--drain-lb-wait` (10s) the server goes on serving everything, so that a load balancer sees the 503;
2. it stops accepting connections and answers new requests, but for the probes, with 503 `ServiceUnavailable` and `Connection: close`;
3. it waits for requests in flight, at most `--drain-deadline` (15m) in all;
4. it gives up the reconciler's lease and exits 0.

A request cut at the deadline is counted on stderr: `tierstore: <N> requests were cut at the drain deadline`. Another process over the same tiers takes the reconciler's lease once it is given up or has expired (`--lease-ttl`). State is in the tiers, not the process, so a restart needs nothing but the same `--proxy-id`.

To restart for a new binary or a new key file, replace the file, then `systemctl restart tierstore`; run more than one process, with ids of their own, if clients must not see the pause.

## The start-up self-test

Before it serves, the process asks each tier, under a probe key in `_tiering/selftest/` that is new for each start and deleted afterwards, the questions its guarantees rest on: a create-only put refuses a key that exists, an `If-Match` put replaces only on the right content tag, a range read gives the range, a head has `Last-Modified`, a listing honors `start-after`, and on the cold tier a multipart completion is create-only. A tier that answers one wrongly exits the process with status 3, naming the tier, the check and the answer. A tier that cannot be reached is asked again, with pauses from one second doubling to thirty, for `--backend-check-wait` (5m), and then the start exits 1.

`--skip-backend-check multipart-conditional=<reason>` leaves out the last check, for a store you have shown to be safe another way; the reason is written to stderr at every start. No other check can be skipped.

## A build node

A build node (the file has `[local_cache]`, see [configuration](configuration.md#a-file-for-a-build-node)) serves the S3 front as a read cache. What it keeps: a key is kept if the longest `[[policy]]` prefix that holds it says `cache = true` (and it is not under `_journal/` or `_tiering/`). A GET or HEAD of a kept object is answered from the disk with no call to the upstream. A GET of one that is not there is read from the upstream in chunks of 8 MiB into a staging directory, hashed as it passes, and linked into place only when it is whole; the misses of one key share one fill. A ranged GET of a miss fills the whole object if it is at most 64 MiB, and otherwise passes the range through and keeps nothing. A fill that fails never fails the reader for the cache's reasons: the reader is answered from the upstream. Every call but GET and HEAD is refused 403 `AccessDenied`.

Put `cache = true` only on prefixes whose objects are never rewritten, the content-addressed ones: a kept object is never checked against the upstream again. Anyone who may read through the front may read what the upstream's key can read, so give clients scoped keys.

Memory is bounded for the fills: at most `local_cache.max_concurrent_fills` chunks of 8 MiB are held. A miss that finds every slot taken is served from the upstream and kept nowhere; if the count of such skips climbs, raise the key. A response is held whole in memory, as for any backend.

Eviction keeps the cache under `local_cache.budget`: above `high_water` percent of it, or when the free space of the file system is under `free_floor` percent, a pass (every 10 seconds) removes the least recently used objects down to `low_water` percent and until the free space is back at the floor. The free-space floor only triggers a pass; it does not stop a fill. An object that another tree has linked is skipped, counted and stays charged to the budget. A fill is skipped when the object is larger than the budget or would take the cache above `critical`.

At start the process makes the directory, removes what a fill left half done, reads the kept objects back (sizes only; an object whose sidecar does not say what the file holds is deleted), and asks the file system four questions with a probe object. A file system that answers one wrongly exits the process with status 3, naming the check. It then asks the upstream for a key that is not there: 404 or any object is an answer, and anything else exits 1. On SIGTERM it drains as a storage node does.

## Metrics

`GET /metricz` on the S3 listener (unsigned) answers 200 in the Prometheus text format. On main it has the hit, miss, byte and cold-read counters of the hot and cold tiers, the counters of the OCI read front, such as `tierstore_oci_requests_total`, and the counters of the cache of a build node, all named `tierstore_` and a word; without `--oci-listen` it says the front is not enabled, and a node with no cache says so on a line of its own. The tier counters are not in the latest release (v0.0.38). The full list of metric names, their labels and queries to alert on is in [metrics](metrics.md).

## Not in a release yet

- **Waiting for a put slot.** Today a put over `--max-concurrent-puts` is answered 503 `SlowDown` at once; the server will instead wait briefly for a slot first, and clients will retry a 503 with jittered waits.
