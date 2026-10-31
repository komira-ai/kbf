# tierstore update strategy: port-forward hand-over

Status: DESIGN, for the maintainers. Nothing here is built. Checked against `main` at the commit this branch starts
from; each claim about today's code names the file and function.

This is the tierstore deep dive of the host update design. The host updater (one daemon per host; services register
with it) does fetch, sha256 and attestation verification, staging, reporting and rollback. The coordinator decides the
desired version per service, one host at a time, canary, health gate and rollback on a failed check. Both, and the port
forwarding, are designed in `host-updates.md`; this page says what tierstore must offer and do.

## 1. Shape

A new tierstore (v-next) runs beside the running one (v-current), each on its own private port (slot `a` 9301, slot `b`
9302); clients call 9300. v-next starts, finds the cache lock held, and serves read-only. The host updater swaps the rule
to it. When v-current has exited, the kernel frees the lock, and v-next takes it by itself and enables writes. Nothing
commands that last step: the updater observes it.

`crates/tierstore/src/server/handover.rs` is not a process hand-over: it hands a put's body from the request to the
backend's task (`hand_over`, `carry`, `received`). Nothing on `main` serves this.

Two shapes run from one binary (`Settings::Storage`, `Settings::Cache`; `server/config.rs`; started by
`serve_until_signalled` in `server/cli.rs` and `serve_cache_until_signalled` in `server/cache_cli.rs`):

- **Build node (cache)**: a `LocalCache` over a directory; no journal, no lease (`server/local_cache/mod.rs` header).
  The overlap hazard is the shared directory (section 4).
- **Storage node (proxy plus reconciler)**: the proxy is stateless; the reconciler runs under a lease (`lib.rs`,
  `reconciler.rs`, `lease.rs`). The overlap hazard is the lease and the journal (section 5).

## 2. Relies on host-updates.md: port forwarding, and what it asks of the listener

The nft rule, its atomic swap, the guard and the output chain are in `host-updates.md` and not repeated here. This
strategy needs these facts from it:

- an established flow stays on v-current while it drains (conntrack); new flows go to v-next after the swap;
- the redirect of a remote packet rewrites its destination to the address of the interface it arrived on, so the private
  ports listen on the **wildcard of both families**, not on loopback. A loopback-only listener would refuse every remote
  client. The guard table is what keeps a direct connection to a private port out.

**New tierstore work: the listener.** Today `--listen` is one `SocketAddr` (`server/cli.rs`, field `listen`; the build
node's file has one `listen` key, `server/config_cache.rs`), bound once by `TcpListener::bind` (`run_serving`,
`serve_cache`), and that call has no way to set `IPV6_V6ONLY`. So listening on `0.0.0.0` and `[::]` is new: a repeatable
`--listen` (and a list-valued key), two sockets, `IPV6_V6ONLY` set explicitly so the two do not collide on a host where
dual-stack is the default. The OCI front (`--oci-listen`, `oci::OciListener`) has the same single address and needs the
same change if it is handed over (question 4). A start that cannot bind both is refused.

## 3. What v-next must offer, and the steps

**Registration** (per instance, one health URL for each slot while two run):

| field | value | today |
|---|---|---|
| name | `tierstore` | new; nothing registers today |
| version | the running binary's version | **gap**: only the start line `tierstore <version> listen=...` (`serve_until_signalled`, `serve_cache_until_signalled`) and `--version`; the value is `CARGO_PKG_VERSION`, the crate's, not a commit. New: `tierstore_build_info{version,slot,mode}` on `/metricz` (section 7) |
| health endpoint | `GET /readyz` on the private port | **exists**: 200 until the drain begins, 503 after (`Server::respond`, `probe`, `server/mod.rs`); `/healthz` is 200 while the process runs; both unsigned |
| mode | `read_only` or `read_write` | new: the 200 body says `ready read-only` or `ready read-write` (today `ready`), and `tierstore_cache_mode{mode}` says it too; the status code is unchanged, so a probe that only checks 200 still works |
| strategy id | `port-forward-handover` | new |

**What the host updater can ask of tierstore, and what exists:**

| need | today | proposal |
|---|---|---|
| listen on both families | one `SocketAddr` (`cli.rs`, `listen`) | repeatable `--listen`, `IPV6_V6ONLY` explicit (section 2) |
| start read-only beside a running instance | none | no flag: the instance that finds the lock held is read-only (section 4) |
| a proxy id per slot | `--proxy-id` | `tierstore-a`, `tierstore-b` (section 5) |
| ready, and which mode | `/readyz` | body and gauge carry the mode |
| drain | SIGTERM (`signals`, `cli.rs`), `--drain-lb-wait`, `--drain-deadline`, `Drained.abandoned` | P1, P2 (section 6) |
| take over | none | **nothing is asked**: v-next takes the lock itself when it frees (section 4); the updater observes the mode |

There is no promote verb, signal or route. `signals` handles SIGTERM and SIGINT only (`cli.rs`), so a release without a
handler would die on any other signal, the updater has no right to signal the service's process, and after a reboot no
one would send it. A lock needs none of that.

**Ordered steps** (the host updater runs them). For tierstore the **commit point is the SIGTERM to v-current**: it
cannot be rescinded, v-current stops accepting at the end of its settle wait and exits when drained (`Server::serve_from`).

| # | step | check | abort |
|---|---|---|---|
| 0 | plan: both releases must have the lock mode | `tierstore --capabilities` (new) prints `handover-v1` for the staged release and for v-current's (`host-updates.md`, the capability probe) | none: a plain restart with a gap, by rule |
| 1 | start v-next on the free slot | start line seen; `/readyz` 200 with `read-only` on its private port | stop v-next |
| 2 | swap the rule to v-next (`nft -f`, one transaction) | a request through 9300 is answered by v-next (build gauge); health gate held for a bound, v-current untouched | swap back, **then** stop v-next |
| 3 | **commit**: the executor's `retire-slot` job for v-current's slot (link to none, runtime mask, stop: the SIGTERM), `--drain-lb-wait` as the settle wait | v-current's `/readyz` is 503; in flight reaches 0 or the deadline passes | none: no swap back after this point |
| 4 | v-current exits; the kernel frees the lock | exit status 0 (a hung one is killed at `TimeoutStopSec=960`, `packaging/tierstore/tierstore.service`) | none: v-next serves; the coordinator's rollback is a new hand-over |
| 5 | finish: observe v-next | `/readyz` says `read-write` (and, on a storage node, the reconciler runs), waited for at most `promotion_bound` | none: past the bound the update is reported, never failed; see failure cases |
| 6 | report healthy | as step 5 | the coordinator's rollback is this strategy with the old version |

Rollback is a new hand-over with the old version as v-next. It works only if the old version has this mode; before the
first release that has it, it is a plain restart with a gap, and the updater says so in its result.

## 4. Cache read-only mode and the lock contract (build node)

**The lock decides the mode** (new; no `flock` or lock file exists under `crates/tierstore/src`). A start tries
`<path>/.lock` with `flock(LOCK_EX | LOCK_NB)` on a descriptor it keeps open: it gets it and is read-write, or it does not
and is **read-only until it does**. There is no flag, so a lone instance after a reboot, or a restart of a crashed
v-current, is read-write at once, and a second instance can never write beside the first.

- **Waiting**: a task retries the try on an interval (a setting, a second by default) on the server's clock. It blocks no
  thread and nothing waits on the lock inside a request. The interval is injectable, so a test counts retries and never
  sleeps.
- **Taking it**: on success, in this order: `layout::clear_stage`, `layout::scan` with deletes, replace the index,
  then enable fills, keeps and eviction (and, on a storage node, the reconciler, section 5), then publish `read-write`
  in `/readyz` and the gauge. v-current's last act is `flush_touches` at the end of `maintain`, so the rescan sees its
  recency.
- **Holder**: the one process that may mutate the directory (fill, publish, evict, touch, `clear_stage`, `scan` with
  deletes). A read-only process holds nothing, opens files under `obj/` for reading, and calls the upstream.
- **v-current crashes or hangs**: the kernel frees the lock when the process dies. A hung v-current is killed by the
  unit's stop timeout (`TimeoutStopSec=960`), so v-next gets the lock after at most the drain deadline plus the margin;
  until then it serves read-only. `clear_stage` and `scan` clean `.stage/` and a half-published sidecar.
- **A lock does not protect against a v-current that predates it**: such a process holds nothing, so a new instance
  would be granted the lock at once while it still writes. Hence step 0: no overlap onto a v-current that reports no
  mode. The first release with the lock goes in by a plain restart; every hand-over after it has a v-current that holds it.
- **A replaced `.lock`** is a lock on nothing: after taking it, the holder compares `fstat` of its descriptor with `stat`
  of the path and, on a mismatch, releases and retries.
- Old binaries ignore `.lock` (`scan` reads `obj/` and `.stage/` only): the file is additive.

**Read-only behaviour.** The instance serves hits from the existing directory; every miss and write goes to the
upstream. Nothing like it exists: `CacheConfig::write_through` is about client writes, and a read cache already refuses
client writes at the front (`Server::respond`, `read_cache`). Serving a hit beside v-current is safe because the data file
is a create-only hard link of a complete file (`fill.rs: publish` renames the sidecar, then links the data file last), so
a name that exists is whole, and an eviction unlinks a name without touching bytes a reader has open. v-next's index is a
snapshot from its start: an object v-current fills later is a miss (served from the upstream); one it evicted is a failed
read and a miss. The five writes to the shared directory today, and how each is disabled:

| write today | how read-only disables it |
|---|---|
| `LocalCache::open` runs `layout::clear_stage` (empties `.stage/`, where v-current's fills are) and `layout::scan` (deletes a sidecar with no data file: the middle of v-current's `publish`) | a read-only open reads and skips; deletes nothing, empties nothing |
| `check_local` writes a probe under `.stage/` (`local_cache/selfcheck.rs`) | probe a directory of its own on the same file system, or skip |
| `layout::make_dirs` chmods directories (in `open`, `scan`, `publish`) | not called on the shared tree |
| `Inner::lookup` calls `drop_entry` on `NotFound`, deleting files v-current may just have refilled | a read error is a miss and touches nothing |
| `flush_touches` stamps file times (`utimensat`) in `maintain` and `evict_pass` | `maintain` does not run |

It also starts no fills, keeps, eviction or scrub (`scrub_rate` is accepted and ignored today, `serve_cache`).

## 5. Journal, lease and proxy id (storage node)

The reconciler takes the lease, in `Reconciler::pass` through `Lease::ensure` (`lease.rs`), on `_tiering/lease` in the
hot tier: create-only, renewed or taken over by `put_if_match` after `lease_ttl`. The proxy writes
`_journal/<proxy_id>/<seq>` create-only and skips a taken number (`Inner::write_batch`, `tiered.rs`).

- **Two proxies are safe**: any number work over the same tiers (`lib.rs`), and `Proxy::open` goes on after the highest
  number it finds.
- **One proxy id for both is wrong.** Two writers on one journal interleave, and the reconciler treats it as one stream
  (oldest first by journal order within a proxy). The unit already says to give each process its own id
  (`tierstore.service`, above `[Unit]`). Proposal: the slot is the id, `tierstore-a` and `tierstore-b`, stable across
  upgrades, so ids do not pile up under `_tiering/proxies/`.
- **One lease holder name is a small real bug.** `ReconcilerConfig::holder` is the proxy id and the body is
  `holder\n<generation>` counting from 1 in each process (`Lease::body`); the content tag hashes the body. A restarted
  process can write a body equal to an older one, and a stale renew with that old tag then succeeds. It needs a long
  pause, so it is narrow; slot-named holders and a start nonce in the body remove it.
- **A `dir:` hot tier cannot have two holders**: `DirBackend::put_if_match` is atomic among the calls of one backend only
  (`dir.rs`, the doc of `DirBackend`). An S3 hot tier does not have this problem.

**The same lock decides the reconciler.** A storage node has no cache directory, so it takes a host-local lock file
(`--lock-file`, new; a path both slots share in the units' `StateDirectory`, not a `RuntimeDirectory`, which systemd removes when a unit stops; the holder re-checks `fstat` of its descriptor against `stat` of the path while it holds the lock, so a replaced file is noticed) with the same try-loop as section 4. v-next
serves reads and writes through its own proxy at once and does not run the reconcile loop until it holds the lock.
Reasons: v-current already holds the lease and runs the loop through the whole drain (`reconcile` ends only when `serve`
returns, `run_serving`); a second runner takes the lease only by expiry or makes v-current lose it silently, which
`Lease` documents as a pass that can run beside another's; and a `dir:` hot tier is unsafe. v-current gives the lease up
on exit (`Reconciler::stop`, at most `STOP_WAIT` = 5 s), so v-next's first `ensure` after taking the lock finds it free.
If v-next does not have the lock, v-current is alive and keeps reconciling; when v-current is gone the kernel has freed the
lock, so there is no interval with no reconciler beyond the retry interval.

## 6. The drain of v-current: P1 and P2

Today, when told to stop: `/readyz` 503 at once; for `drain_lb_wait` (default 10 s) it serves everything; then the
phase is REFUSING, the listener is dropped, and every request **on a connection still open** is answered 503
`ServiceUnavailable` with `Connection: close`; then it waits for the requests in flight, at most `drain_deadline`
(default 15 min); then it closes the connections (`Server::serve_from`, `respond`, `finish`).

With port forwarding that gives a 503 window: after the swap no new flow reaches v-current, but each established
keep-alive connection does, and after `drain_lb_wait` its next request gets a 503 while one long transfer holds the
drain. Two changes in tierstore (new, independent of how the port moves):

- **P1** from the moment the drain begins, every answer carries `Connection: close`. A client that was busy reconnects
  after its next answer and lands on v-next.
- **P2** a **lingering close** for idle connections, at the end of the settle wait: the server calls `shutdown(SHUT_WR)`
  on each idle keep-alive connection (the client reads a clean end of stream), keeps reading until the client closes its
  side, and only then closes. A plain `close()` with unread request bytes sends a reset, which can destroy an answer the
  client has not read; the lingering close never does. A request that was already on the wire is served as usual: only
  a connection with no request in progress is treated this way. Bytes that arrive after the `FIN` are read and
  dropped, and counted (`tierstore_drain_late_requests_total`). They are the remaining race: a request that left the client
  within one round trip of the `FIN` meets a connection that is closing, and no server can answer it.

**The client policy is no retry.** A retried create-only put is ambiguous: if the first attempt was applied, the retry
gets 412 and the client cannot tell its own object from another's. So the strategy's goal is that a client that follows
P1 never meets the race. A connection is idle at the end of the settle only if its client has sent nothing since before
the drain began; every client that did send something got `Connection: close` (P1). The counter makes the residual
visible, and the test below asserts it is zero for clients that are never idle.

`--drain-lb-wait` is the **settle wait** in a hand-over and is never `0s`: v-current keeps accepting and serving through
it, so a connection whose SYN was mapped to v-current just before the swap (in its accept queue, or a retransmitted SYN)
is served, not reset (`host-updates.md`, 4.1.3). The updater passes the strategy's `settle` (default about **70 s** from the stop of v-current,
which covers the last SYN retransmit of a default client, about 63 s after its first SYN, 4.1.3) as `--drain-lb-wait`; tierstore's own default stays 10 s for a plain
restart. The longer settle costs only update time: v-current serves everything throughout it. A keep-alive connection that outlives `drain_deadline` is closed by
v-current (`finish_by_deadline`, `finish`) and the cut requests are counted in `Drained.abandoned` and printed by
`cut_note`: an idle one costs the client a reconnect; a request still running fails, as in a plain restart.

## 7. Metrics

`/metricz` is one block per source (`Server::metrics_probe`). Read-only and overlapping:

| family (`tierstore_`) | read-only v-next | with two processes |
|---|---|---|
| `cache_hits_total`, `cache_misses_total`, `cache_bytes_served_local_total`, `cache_bytes_served_upstream_total` | count | per process; the sum is right |
| `cache_fills_total`, `cache_fills_skipped_total`, `cache_fills_skipped_busy_total`, `cache_fill_failures_total{reason}`, `cache_evictions_total{reason}`, `cache_pinned_skips_total`, `cache_staging_files` | stay 0 | v-current only |
| `cache_discarded_at_start_total` | 0 (read-only open deletes nothing) | v-current's |
| `cache_bytes`, `cache_objects` | start snapshot, never updated | **double-count** one directory |
| `cache_fs_free_bytes` | live | **double-count** one file system |
| `hits_total`, `misses_total`, `read_errors_total`, `reads_total`, `bytes_served_total`, `bytes_read_total` (`tier`, `op`), `promotions_total`, `promoted_bytes_total`, `promotions_skipped_total{reason}` (storage) | count | per process; the sum is right |
| `oci_requests_total`, `oci_bytes_served_total`, `oci_digest_mismatches_total`, `oci_front_info` | count | per process |

Counters restart at 0 in a new process; a rate handles it. The directory gauges are read from the process that holds
the lock. No family names a build, slot or mode today (the only process-level label is `oci_front_info{mode}`), and a
scrape of 9300 reaches whichever instance the rule points at without saying which. Proposal: scrape the private ports and
add `tierstore_build_info{version,slot}`, `tierstore_cache_mode{mode="read_only|read_write"}` and
`tierstore_drain_late_requests_total`. One private port is down between upgrades, so an alert is on "no instance up", not
on `up == 0`. `/readyz` on v-next is 200 before it holds the lock: it means "serves requests"; the mode says whether it
writes.

## 8. Failure cases

| case | what happens | state after |
|---|---|---|
| v-next not ready (step 1) | abort: stop it; nothing moved | v-current serves; failed |
| the swap fails (step 2) | the updater's swap is one transaction: the old rule stays | v-current serves; v-next stopped |
| v-next ready but wrong through 9300, or crashes, before the commit | abort: swap back, then stop v-next; v-current has not been signalled, so it still accepts | v-current serves |
| v-next crashes after the commit | no swap back: v-current has stopped or is stopping accepting, so a swap back would send new connections to a closed port. systemd restarts v-next: it is read-only while v-current still holds the lock, read-write at once if v-current is gone. The coordinator rolls back by a new hand-over | a gap until v-next is up again; failed |
| v-current hangs past the drain deadline | `Drained.abandoned` counts what was cut; connections close; the unit is killed at `TimeoutStopSec`; the kernel frees the lock and v-next takes it | v-next read-write; update succeeds late |
| v-current crashes during the overlap | the kernel frees the lock; v-next takes it. If the swap has not happened, the updater swaps (it holds the rule) | v-next serves, read-write |
| v-next never gets the lock | only while v-current is alive (it holds it, so it is serving and reconciling); v-next keeps serving read-only | the finish check times out; the updater reports; the coordinator decides |
| the host reboots mid-overlap | the executor starts one unit, the slot the rule names; it finds the lock free and is read-write at once | no overlap |
| the disk fills while both run | v-current's fills and eviction handle it as today (`critical`, `free_floor`); v-next writes nothing; stale index entries are misses | no corruption |

## 9. Test plan (tests first)

House rules: nothing asserts timing, no real sleeps, injected clocks and counted events. Each test names the defect it
catches and is written red before the code. nft tests: see `host-updates.md`.

- **T1 hand-over under load, no retry.** N clients put (create-only) and get, keep-alive and new connections, each
  issuing its next request as soon as it has the previous answer, with **no retry of any request**, while the steps run
  end to end (remote clients through a veth peer as well as local ones, the setup of `host-updates.md`). Zero failed
  requests; every object a client was told was written reads back with its bytes and hash; both instances answered (build
  gauge), so it is not vacuous; `tierstore_drain_late_requests_total` is 0; the directory passes `scan` with nothing
  discarded. Without P1 and P2 it fails.
- **T2 lock contract.** Two processes on one directory: the second is read-only and its retry count rises while the
  first holds the lock (an injected clock ticks the retries); after the first is killed the second takes it, its `scan`
  cleans a half-published sidecar, and its mode says `read-write`; a replaced `.lock` is released and retaken.
- **T3 read-only writes nothing.** Start read-only on a directory with a half-published sidecar and files in `.stage/`;
  serve hits and misses; compare the whole tree (names, sizes, modes, mtimes) before and after. One case per row of the
  table in section 4: remove its fix and the tree differs.
- **T4 stale index.** v-current evicts then refills an object while v-next holds the old entry and is asked for it:
  v-next serves it and deletes nothing.
- **T5 rollback.** Hand over, then back, under T1's load: zero failures, objects intact.
- **T6 lease and journal.** Two proxies with slot ids over one hot tier write separate journals; v-next's reconciler makes
  no `ensure` call before it holds the lock file (a counted event); after it does, it takes the lease v-current released;
  two starts never write the same lease body.
- **T7 metrics.** Read-only v-next shows 0 for the fill and eviction families after a miss-heavy run; the build and mode
  gauges differ between instances.
- **T8 drain (P1).** On a paused clock over in-memory pipes (the pattern of `drain_tests.rs`): every answer after the
  drain begins says `Connection: close`; no request gets 503.
- **T9 lingering close (P2), deterministic.** A server over a pair of in-memory streams (tokio `duplex`), events counted,
  no clock: an idle keep-alive connection when accepting stops. Assert in order: the server shut down its write half (the
  client's read returns 0 bytes); the server has **not** dropped the connection (an open-connection count is still 1);
  the client then closes and the count goes to 0. A second case: the client writes a request after the server's `FIN`;
  the server reads it, drops it, counts it (`late_requests` is 1), and the connection still closes only after the client
  closes. A third case over real loopback TCP, ordered by events (a read that returns end of stream, not a reset): a
  client holding unsent request bytes at the moment of the server's close reads a clean end of stream, never
  `ECONNRESET`. A plain `close()` fails the third case.
- **T10 self-take.** A read-only instance with the lock free takes it on the next retry, runs `clear_stage` and `scan`,
  then serves a counted fill and reports `read-write`; with the lock held it changes nothing; a lone instance at start is
  read-write before it answers `/readyz`.

## 10. Questions for the maintainers

1. **Self-take instead of a promote command.** Recommendation: as written; the lock is the whole protocol, so a reboot,
   a crashed v-current, and a hung one all resolve without the updater. The cost is that an instance that finds the lock
   held is read-only whether or not it is a hand-over.
2. **P1 and P2** change the plain restart's drain too (a connection is closed, never answered 503). Recommendation:
   the only behaviour, not a flag, so there is one drain.
3. **The residual race** (a request within one round trip of the `FIN` on a connection idle since before the drain).
   Recommendation: accept it as the stated limit, with the counter, and do not ask clients to retry a create-only put.
4. **Listeners.** A repeatable `--listen` with explicit `IPV6_V6ONLY`, and the same for `--oci-listen`. Recommendation:
   do both in the first tierstore change, or the OCI front's clients see a gap at every upgrade.
5. **First lock release** goes in by a plain restart, and rollback past it is a restart with a gap. Recommendation: accept.
6. **A `dir:` hot tier** cannot have two lease holders. Recommendation: the hand-over is for S3 hot tiers; `dir:` keeps
   the plain restart.
7. **Version source.** The registered version has no source today. Recommendation: the build gauge, with the commit baked
   into the binary rather than the crate version.
8. **Scraping private ports** is blocked by the guard from off the host. Recommendation: a scraper on the node reads
   loopback.
