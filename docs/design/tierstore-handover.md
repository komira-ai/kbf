# tierstore update hook: port-forward hand-over

Status: DESIGN, for the maintainers. Nothing here is built. Checked against `main` at the commit this branch starts
from; each claim about today's code names the file and function.

tierstore ships its update as a subcommand of its own binary, `tierstore update-hook <step>`, run by the generic host
updater under the contract of `host-updates.md` section 3.2: steps `prepare`, `cutover`, `verify`, `rollback`,
`cleanup`; exit `0` ok, `1` failure, `75` deferred, `76` halt; the last stdout line is one JSON object; every step is
idempotent; the hook passes the conformance suite in tierstore's CI. The updater fetches, verifies and stages the
release, keeps the step record, and rolls back; the coordinator orders hosts. The nftables mechanics (rules, swap,
conntrack, guard) are in `host-updates.md` 4.1.1 to 4.1.5 and are tierstore's, as steps of this hook. This page is the
hook, and what tierstore itself must change.

## 1. Shape

Two instances run side by side during an update, each on its own private port (slot units `tierstore@9301` and
`tierstore@9302`); clients call 9300, which one redirect rule sends to the current instance. v-next starts, finds the
cache lock held, and serves read-only. The hook swaps the rule to it. When v-current has exited, the kernel frees the
lock and v-next takes it by itself and enables writes. Nothing commands that: the hook observes it.

`crates/tierstore/src/server/handover.rs` is not a process hand-over: it hands a put's body from the request to the
backend's task (`hand_over`, `carry`, `received`). Two shapes run from one binary (`Settings::Storage`,
`Settings::Cache`; `server/config.rs`; `serve_until_signalled` in `server/cli.rs`, `serve_cache_until_signalled` in
`server/cache_cli.rs`): a **build node** (a `LocalCache` over a directory, no journal or lease, hazard: the shared
directory, section 5) and a **storage node** (stateless proxy plus a reconciler under a lease, hazard: lease and journal,
section 6).

**What tierstore registers at runtime** (liveness only, `host-updates.md` 2): name `tierstore`, the version, health
`GET /readyz` on the instance's private port. **Exists**: `/readyz` is 200 until the drain begins, 503 after
(`Server::respond`, `probe`, `server/mod.rs`); `/healthz` is 200 while the process runs; both unsigned. **Gap**: the
version is in the start line `tierstore <version> listen=...` and `--version` only, and is `CARGO_PKG_VERSION`, the
crate's, not a commit. New: `tierstore_build_info{version,slot}` on `/metricz`, and `/readyz`'s 200 body saying
`ready read-only` or `ready read-write` (today `ready`; the status code does not change). New: `tierstore --capabilities`
prints the capabilities of the binary, one per line; `handover-v1` means "has the lock, read-only mode, wildcard
listeners and the lingering drain".

## 2. The declaration (`/etc/updater.d/tierstore.toml`, installed with the release)

| item | value | note |
|---|---|---|
| hook | `tierstore update-hook <step>`, user `tierstore` | the artifact's own binary; Rust, no shell |
| `CAP_NET_ADMIN` | steps that touch nft: `prepare`, `cutover`, `rollback` | the updater's root part launches the step as a transient unit with the capability ambient (`host-updates.md` 3.4); the hook runs as `tierstore`, never as root. **Needs from the updater**: a per-step capability list. With one list per hook, `verify` and `cleanup` would hold it as well: acceptable, not wanted |
| address families | `AF_UNIX AF_INET AF_INET6 AF_NETLINK` | netlink for nft |
| units | `tierstore@9301.service`, `tierstore@9302.service` | the only units it may start, stop or mask |
| slot unit template | `tierstore@.service`, `%i` is the port | below |
| state directory | `/var/lib/tierstore/update/` (`state.json`: active port, mode, step markers; temporary file, fsync, rename) | `StateDirectory=`, never `RuntimeDirectory=` |
| slot links | `/var/lib/tierstore/slots/<port>` points at a release directory, or is absent | a retired slot has none |
| lock files | build node `<cache>/.lock`; storage node `/var/lib/tierstore/instance.lock` | section 5 |
| nft tables | `kbf_forward_tierstore` (redirect), `kbf_guard_tierstore` (guard) | `host-updates.md` 4.1.1 |
| read / write paths | read `/usr/libexec/kbf`, `/usr/sbin/nft`, `/etc/tierstore`; write `/var/lib/tierstore` | the cache directory is not touched by any step |
| timeouts (s) | `prepare` 600, `cutover` 300, `verify` 600, `rollback` 600, `cleanup` 1300 | `prepare`: self-test of the tiers (`--backend-check-wait`); `cleanup`: settle 70 + `--drain-deadline` 900 + lease 5 + margin, and the unit's `TimeoutStopSec` becomes 1100 (it is 960 in `packaging/tierstore/tierstore.service`, which assumes a 10 s wait) |
| `verify_deadline_secs` | 1200 | at least `verify` plus `rollback` (`host-updates.md` 2.4) |
| `promotion_bound_secs`, `promotion_ceiling_secs` | 300, 3600 | proposed fields, not in the 2.4 example; section 3, `verify` |

Slot unit template (flags only, no environment; the rest as today's `packaging/tierstore/tierstore.service`):
`ExecStart=/var/lib/tierstore/slots/%i/tierstore --listen 0.0.0.0:%i --listen [::]:%i --proxy-id tierstore-%i
--drain-lb-wait 70s --drain-deadline 15m ...` and `ConditionPathExists=/var/lib/tierstore/slots/%i/tierstore`, so a
slot with no link does not start. The same template starts both slots, so the settle is the same for either. `--listen`
repeated and the proxy id per slot are new (sections 4 and 6).

A boot unit runs `tierstore update-hook restore` (not a step; the updater never calls it): it re-applies the redirect
for the active port from `state.json` and the guard, so after a reboot or a `flush ruleset` the rule names the instance
that starts (`host-updates.md` 3.5). It is idempotent for the same reason the swap is.

## 3. The steps

Every step reads `state.json` first and sets state, never "does a thing once": that is what makes a re-run after a crash
at any point converge. The state names the active port; the **idle port is the other one**, recomputed every time.

**`prepare <artifact>`** (before anything visible). Check `tierstore --capabilities` of the staged release and of the one
the active slot links to: both must have `handover-v1`, else the hook records `mode=restart` and `cutover` does a plain
restart with a gap (the first release with the lock, section 5). Refuse with `75` (host not ready, nothing done) if the
guard table cannot be loaded, `nf_conntrack_tcp_timeout_established` is not longer than the service's idle bound, or
`nf_conntrack_count` is near `nf_conntrack_max` (`host-updates.md` 4.1.3). Point the idle slot's link at the artifact,
start `tierstore@<idle>`, and poll `/readyz` on `127.0.0.1:<idle>` until it answers 200 with `read-only` (a counted
number of attempts; the unit starts only after its self-test). *Idempotent*: the link is replaced by rename, `systemctl
start` of a started unit does nothing, the guard table is loaded by replacing it, the idle port comes from the state.
*Fails*: v-next never ready: `1`, class `release`.

**`cutover`** (the visible step). Write intent to `state.json`, then apply the swap file of 4.1.2 for the idle port in one
`nft -f`, then probe through 9300 until a request is answered by v-next (its build gauge names the slot and the
version), a counted number of times. A probe that never succeeds swaps back and exits `1`. Records `swapped`. *Idempotent*:
the file replaces the table and names the target port, so a second apply changes nothing; a crash before `swapped` is
re-run. *Restart mode*: stop v-current, start v-next, wait for readiness, then the same swap.

**`verify`** (the gate; run again and again). Before `cleanup`: v-next is ready, a counted series of probes through 9300
is answered by v-next with the staged version, the redirect names v-next, and v-current still serves. It exits `0` with
`"promotion_pending":true`: v-current holds the lock, so v-next is read-only by design (`host-updates.md` 3.8). After
`cleanup` has stopped v-current (a marker in `state.json`): wait for `/readyz` to say `read-write` for `promotion_bound`;
if it does, `0` with no pending flag; if the bound passes, `0` with `"promotion_pending":true` and the holder of the lock
in `detail`, so the updater keeps watching. At `promotion_ceiling`: `1` on a storage node (a v-next that never promotes
copies nothing to the cold tier: the reconciler is off), and an alert on a build node. **Exit `75` is not used**: nothing
here is "not ready, try later with nothing done". *Idempotent*: it only observes, and sets the `verified` marker.

**`rollback`** (before the stop of v-current only). Apply the swap file for the old port, then stop `tierstore@<idle>`
and clear the markers. *Idempotent*: stopping a stopped unit and re-applying a swap are no-ops. **It is valid only before
`cleanup` has written its `stopping` marker.** After it, v-current is exiting or gone and a swap back would send new
connections to a closed port, so `rollback` exits `1` with `"reversible":false`.

**`cleanup`** (the commit point of tierstore). **If `verified` is not set, `cleanup` does what `rollback` does** and
stops nothing else: the updater runs `cleanup` after a failed `prepare` or `cutover` (`host-updates.md` 3.2), and for
tierstore that must never stop v-current. If `verified` is set: write `stopping`, then `systemctl stop
tierstore@<active>`. The process sees SIGTERM, keeps accepting for the settle (`--drain-lb-wait`, 70 s, the last SYN
retransmit of a default client, `host-updates.md` 4.1.3), applies P1 and P2 (section 7), waits for requests in flight at
most `--drain-deadline`, gives up the lease, and exits; the unit's stop timeout kills a hung one and the kernel frees the
lock. Then retire the slot: remove its link, `systemctl mask --runtime`, record the new active port. *Idempotent*: every
part is "set to state": stop of a stopped unit, link absent, mask present, the active port written. After `stopping` is
written a failed `cleanup` is re-run forward, never rolled back (question 1).

## 4. What tierstore changes in the listener

The redirect rewrites a remote packet's destination to the address of the interface it arrived on, so the private ports
listen on the **wildcard of both families**, not loopback; a loopback-only listener would refuse every remote client
(`host-updates.md` 4.1.1). Today `--listen` is one `SocketAddr` (`server/cli.rs`, field `listen`; one `listen` key in
`server/config_cache.rs`), bound by one `TcpListener::bind` (`run_serving`, `serve_cache`), which cannot set
`IPV6_V6ONLY`. New: a repeatable `--listen`, two sockets, `IPV6_V6ONLY` explicit. A start that cannot bind both is
refused. `--oci-listen` (`oci::OciListener`) has the same single address and needs the same change if it is handed over
(question 4). The guard table keeps a direct connection to a private port out.

## 5. Cache lock and read-only mode (build node)

**The lock decides the mode** (new; no `flock` or lock file exists under `crates/tierstore/src`). A start tries
`<path>/.lock` with `flock(LOCK_EX | LOCK_NB)` on a descriptor it keeps open (close-on-exec, no follow, a local file
system): it gets it and is read-write, or it does not and is **read-only until it does**. No flag: a lone instance after a
reboot, or a restarted v-current, is read-write at once; a second instance can never write beside the first.

- **Waiting**: a task retries on an interval on the server's clock (a second by default); it blocks no thread, nothing
  waits on the lock inside a request, and a test counts retries and never sleeps.
- **Taking it**: in this order `layout::clear_stage`, `layout::scan` with deletes, replace the index, enable fills, keeps
  and eviction (and, on a storage node, the reconciler), then publish `read-write`. v-current's last act is
  `flush_touches` at the end of `maintain`, so the rescan sees its recency.
- **Holder**: the one process that may mutate the directory. A read-only process holds nothing, reads `obj/`, and calls
  the upstream.
- **v-current crashes or hangs**: the kernel frees the lock; a hung one is killed by the unit's stop timeout, so v-next
  takes it after at most the drain deadline plus margin and serves read-only until then.
- **A v-current that predates the lock** holds nothing, so v-next would get the lock at once while it still writes:
  hence `prepare`'s capability check and `mode=restart`. The first release with the lock goes in by a plain restart.
- **A replaced `.lock`** is a lock on nothing: the holder compares `fstat` of its descriptor with `stat` of the path when
  it takes the lock and again periodically, and releases and retries on a mismatch.
- **The storage node's lock file** is under `/var/lib/tierstore/`, not a `RuntimeDirectory=`, which systemd removes when
  the unit stops and would unlink the file under v-next.

**Read-only behaviour.** Hits come from the existing directory; misses and writes go to the upstream. Nothing like it
exists: `CacheConfig::write_through` is about client writes, and a read cache already refuses client writes at the front
(`Server::respond`, `read_cache`). A hit beside v-current is safe because the data file is a create-only hard link of a
complete file (`fill.rs: publish` renames the sidecar, then links the data file last), and an eviction unlinks a name
without touching bytes a reader has open. v-next's index is a snapshot: an object v-current fills later is a miss, one it
evicted is a failed read and a miss. The five writes to the shared directory today and how each is disabled:

| write today | how read-only disables it |
|---|---|
| `LocalCache::open` runs `layout::clear_stage` (empties `.stage/`, where v-current's fills are) and `layout::scan` (deletes a sidecar with no data file: the middle of v-current's `publish`) | a read-only open reads and skips; deletes and empties nothing |
| `check_local` writes a probe under `.stage/` (`local_cache/selfcheck.rs`) | probe a directory of its own on the same file system, or skip |
| `layout::make_dirs` chmods directories (in `open`, `scan`, `publish`) | not called on the shared tree |
| `Inner::lookup` calls `drop_entry` on `NotFound`, deleting files v-current may just have refilled | a read error is a miss and touches nothing |
| `flush_touches` stamps file times (`utimensat`) in `maintain` and `evict_pass` | `maintain` does not run |

It also starts no fills, keeps, eviction or scrub (`scrub_rate` is accepted and ignored today, `serve_cache`).

## 6. Journal, lease and proxy id (storage node)

The reconciler takes the lease, in `Reconciler::pass` through `Lease::ensure` (`lease.rs`), on `_tiering/lease` in the hot
tier: create-only, renewed or taken over by `put_if_match` after `lease_ttl`. The proxy writes `_journal/<proxy_id>/<seq>`
create-only and skips a taken number (`Inner::write_batch`, `tiered.rs`).

- **Two proxies are safe**: any number work over the same tiers (`lib.rs`), and `Proxy::open` goes on after the highest
  number it finds.
- **One proxy id for both is wrong**: two writers on one journal interleave, and the reconciler treats it as one stream
  (oldest first by journal order within a proxy). The unit already says to give each process its own id. The slot
  template gives `tierstore-9301` and `tierstore-9302`: stable across updates, so ids do not pile up under
  `_tiering/proxies/`.
- **One lease holder name is a small real bug**: `ReconcilerConfig::holder` is the proxy id and the body is
  `holder\n<generation>` counting from 1 in each process (`Lease::body`); the content tag hashes the body. A restarted
  process can write a body equal to an older one, and a stale renew with that old tag then succeeds. A start nonce in the
  body removes it.
- **A `dir:` hot tier cannot have two holders**: `DirBackend::put_if_match` is atomic among the calls of one backend only
  (`dir.rs`). It keeps the plain restart (`mode=restart`).

**The same lock decides the reconciler.** A storage node has no cache directory, so it takes `--lock-file` (new) with the
same try loop. v-next serves reads and writes through its own proxy at once and does not run the reconcile loop until it
holds the lock: v-current holds the lease through the whole drain (`reconcile` ends only when `serve` returns,
`run_serving`); a second runner takes the lease only by expiry or makes v-current lose it silently, which `Lease`
documents; and v-current gives the lease up on exit (`Reconciler::stop`, at most `STOP_WAIT` = 5 s), so v-next's first
`ensure` after taking the lock finds it free.

## 7. The drain of v-current: P1 and P2

Today, when told to stop: `/readyz` 503 at once; for `drain_lb_wait` (default 10 s) it serves everything; then the phase
is REFUSING, the listener is dropped, and every request **on a connection still open** is answered 503 with
`Connection: close`; then it waits for the requests in flight, at most `drain_deadline` (default 15 min); then it closes
the connections (`Server::serve_from`, `respond`, `finish`). After the swap each established keep-alive connection still
reaches v-current, and each would get a 503 while one long transfer holds the drain. Two changes, independent of how the
port moves:

- **P1** from the moment the drain begins, every answer carries `Connection: close`.
- **P2** a **lingering close** for idle connections at the end of the settle: `shutdown(SHUT_WR)` on each idle keep-alive
  connection (the client reads a clean end of stream), keep reading until the client closes, then close. A plain
  `close()` with unread request bytes sends a reset that can destroy an answer. A request already on the wire is served.
  Bytes arriving after the FIN are read, dropped and counted (`tierstore_drain_late_requests_total`): the remaining race
  is a request within one round trip of the FIN, which no server can answer.

**The client policy is no retry**: a retried create-only put is ambiguous (the retry's 412 cannot tell its own earlier
success from another writer's). A client that follows P1 never meets the race: a connection is idle at the end of the
settle only if its client sent nothing since before the drain began. The settle is `--drain-lb-wait`, 70 s in the slot
template, never `0s`: a connection whose SYN was mapped to v-current just before the swap is served, not reset
(`host-updates.md` 4.1.3). A keep-alive connection that outlives `drain_deadline` is closed (`finish_by_deadline`) and
the cut requests are counted in `Drained.abandoned` (`cut_note`).

## 8. Metrics

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

Counters restart at 0 in a new process; a rate handles it. The directory gauges are read from the lock holder. No family
names a build, slot or mode today (the only process-level label is `oci_front_info{mode}`), and a scrape of 9300 does
not say which instance answered. New: `tierstore_build_info{version,slot}`, `tierstore_cache_mode{mode}` and
`tierstore_drain_late_requests_total`. A scraper that wants both instances scrapes the two private ports from the node
(the guard blocks off-host); one is down between updates, so an alert is on "no instance up", not on `up == 0`.

## 9. Failure cases, per step

| step | case | what happens | state after |
|---|---|---|---|
| `prepare` | v-next never ready; guard cannot load; conntrack bounds fail | `1` or `75`; `cleanup` (abort path, no `verified`) stops v-next | v-current serves, rule untouched |
| `cutover` | `nft -f` fails | one transaction: the old rule stays; `1` | v-current serves |
| `cutover` | probe never answered by v-next | swap back inside the step; `1` | v-current serves |
| `cutover` | hook killed between apply and `swapped` | re-run applies again (no-op) and goes on | rule names v-next |
| `verify` (before cleanup) | v-next crashes or answers wrongly | `1`; the updater runs `rollback`: swap back, stop v-next; v-current still accepts | v-current serves |
| `verify` | verify deadline passes | the updater's dead-man runs `rollback` | as above |
| `cleanup` | killed before `stopping` | re-run; nothing was stopped | rollback still valid |
| `cleanup` | killed after `stopping` | re-run forward: stop, retire; no swap back | v-next serves |
| `cleanup` | v-next crashes after `stopping` | no swap back (v-current has stopped accepting); systemd restarts v-next, read-only while v-current exits, read-write at once if gone; the coordinator rolls back by a new hand-over | a gap until v-next is up; failed |
| `cleanup` | v-current hangs past the drain deadline | `Drained.abandoned` counts what was cut; the unit is killed at its stop timeout; the kernel frees the lock | v-next takes it |
| `verify` (after) | lock not taken within `promotion_bound` | `0` with `promotion_pending`; the coordinator holds the slot; the updater keeps watching | at the ceiling: `1` (storage) or an alert |
| any | v-current crashes during the overlap | the kernel frees the lock; v-next takes it; if the swap has not happened the next `cutover` or `restore` applies it | v-next serves, read-write |
| any | host reboots mid-update | `restore` applies the rule for the active port; one instance starts, finds the lock free, read-write | no overlap; the step record resumes |
| any | the disk fills while both run | v-current's fills and eviction handle it as today (`critical`, `free_floor`); v-next writes nothing; stale entries are misses | no corruption |

## 10. Test plan (tests first)

House rules: nothing asserts timing, no real sleeps, injected clocks and counted events. Each test names the defect it
catches and is written red before the code. **Conformance, one line per step** (the suite of `host-updates.md` 9, with the
real hook binary against a fake service and a fake `nft` and `systemctl`):

- **C1 `prepare`**: twice in a row leaves one started idle unit, one link, one guard table; killed at each boundary and
  re-run reaches the same state; a staged release without `handover-v1` yields `mode=restart`; a bad conntrack bound is `75`.
- **C2 `cutover`**: twice leaves one rule naming the idle port; killed between apply and `swapped` converges; a probe that
  fails swaps back and exits `1`; the swap is one `nft -f` (a fake that records calls sees one).
- **C3 `verify`**: twice changes nothing; with the lock held it exits `0` with `promotion_pending`; with `read-write` it
  clears the flag; a wrong version or slot through 9300 is `1`.
- **C4 `rollback`**: after `cutover` it returns the fake to its earlier state; after `cleanup`'s `stopping` marker it
  exits `1` with `reversible:false` and changes nothing.
- **C5 `cleanup`**: without `verified` it behaves as `rollback` and never stops v-current (red: a hook that stops it);
  with it, twice and killed at each boundary, it ends with the unit stopped, link absent, mask present, port recorded.

nft tests: see `host-updates.md` 9. The service's own tests:

- **T1 hand-over under load, no retry.** N clients put (create-only) and get, keep-alive and new, each sending its next
  request when it has the previous answer, no retry of any request, while all five steps run end to end (remote clients
  through a veth peer as well as local ones). Zero failed requests; every object a client was told was written reads
  back with its bytes and hash; both instances answered (build gauge); `tierstore_drain_late_requests_total` is 0; the
  directory passes `scan` with nothing discarded. Without P1 and P2 it fails.
- **T2 lock contract.** Two processes on one directory: the second is read-only and its retry count rises while the
  first holds the lock (an injected clock ticks the retries); after the first is killed the second takes it, its `scan`
  cleans a half-published sidecar, and its mode says `read-write`; a replaced `.lock` is released and retaken.
- **T3 read-only writes nothing.** Start read-only on a directory with a half-published sidecar and files in `.stage/`;
  serve hits and misses; compare the whole tree (names, sizes, modes, mtimes) before and after; one case per row of the
  table in section 5.
- **T4 stale index.** v-current evicts then refills an object while v-next holds the old entry: v-next serves it and
  deletes nothing.
- **T5 rollback.** Update, then `rollback` before `cleanup`, under T1's load: zero failures, objects intact.
- **T6 lease and journal.** Two proxies with slot ids over one hot tier write separate journals; v-next's reconciler
  makes no `ensure` call before it holds the lock file (a counted event); after it does, it takes the lease v-current
  released; two starts never write the same lease body.
- **T7 metrics.** Read-only v-next shows 0 for the fill and eviction families after a miss-heavy run; the build and mode
  gauges differ between instances.
- **T8 drain (P1).** On a paused clock over in-memory pipes (the pattern of `drain_tests.rs`): every answer after the
  drain begins says `Connection: close`; no request gets 503.
- **T9 lingering close (P2), deterministic.** Over a pair of in-memory streams (tokio `duplex`), events counted, no clock,
  an idle keep-alive connection when accepting stops. In order: the server shut down its write half (the client's read
  returns 0 bytes); the connection is not dropped (the open count is still 1); the client closes and the count goes to
  0. Second case: the client writes a request after the server's FIN; the server reads it, drops it, counts it
  (`late_requests` is 1), and closes only after the client closes. Third case over real loopback TCP, ordered by events:
  a client holding unsent request bytes when the server closes reads a clean end of stream, never `ECONNRESET`; a plain
  `close()` fails it.
- **T10 self-take.** A read-only instance with the lock free takes it on the next retry, runs `clear_stage` and `scan`,
  then serves a counted fill and reports `read-write`; with the lock held it changes nothing; a lone instance at start is
  read-write before it answers `/readyz`.

## 11. Questions for the maintainers

1. **`cleanup` is the commit point and cannot be rolled back** (the stop of v-current). `host-updates.md` 3.2 says a
   failure of `cleanup` is reported and that `rollback` undoes `cutover`; it does not say what the updater does when
   `cleanup` fails after the irreversible part. Recommendation: the updater re-runs `cleanup` (idempotent) and, if it keeps
   failing, reports `Failed` with phase `after_commit` and does not run `rollback`; the hook answers `rollback` after
   `stopping` with `1`, `reversible:false`.
2. **`cleanup` after a failed `prepare` or `cutover`.** 3.2 runs it; for tierstore it must not stop v-current, so the hook
   makes it state-aware (abort path unless `verified`). Recommendation: say in 3.2 that `cleanup` may be called at any
   point and must act from the hook's own record.
3. **Per-step capabilities** in the declaration, so that only `prepare`, `cutover` and `rollback` hold `CAP_NET_ADMIN`.
   Recommendation: add `hook.step_capabilities`; otherwise accept the wider grant.
4. **Listeners.** A repeatable `--listen` with explicit `IPV6_V6ONLY`, and the same for `--oci-listen`. Recommendation:
   both in the first tierstore change, or the OCI front's clients see a gap at every update.
5. **P1 and P2** change the plain restart's drain too (a connection is closed, never answered 503). Recommendation: the
   only behaviour, not a flag.
6. **The residual race** (a request within one round trip of the FIN on a connection idle since before the drain).
   Recommendation: accept it as the stated limit, counted, and do not ask clients to retry a create-only put.
7. **First lock release** goes in by `mode=restart`, and so does a `dir:` hot tier. Recommendation: accept.
8. **Version source.** Recommendation: the build gauge, with the commit baked into the binary rather than the crate version.
