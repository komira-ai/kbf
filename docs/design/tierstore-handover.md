# tierstore update hook: port-forward hand-over

Status: DESIGN, for the maintainers. Nothing here is built. Checked against `main` at the commit this branch starts
from; each claim about today's code names the file and function.

tierstore ships its update as a subcommand of its own binary, `tierstore update-hook <step>`, run by the generic host
updater under `host-updates.md` 3.2: steps `prepare`, `cutover`, `verify`, `rollback`, `cleanup`; exit `0`, `1`, `75`
deferred, `76` halt; the last stdout line is one JSON object; every step idempotent; the conformance suite in tierstore's
CI. The nftables mechanics are in `host-updates.md` 4.1.1 to 4.1.5 and are tierstore's, as steps of this hook. This page
is the hook, and what tierstore itself must change.

## 1. Shape

Two instances run side by side during an update, each on its own private port (slot units `tierstore@9301` and
`tierstore@9302`); clients call 9300, which one redirect rule sends to the current instance. v-next starts, finds the
cache lock held, and serves read-only. The hook swaps the rule to it. When v-current has exited, the kernel frees the
lock and v-next takes it by itself and enables writes. Nothing commands that: the hook observes it.

`komira-ai/tierstore:crates/tierstore/src/server/handover.rs` is not a process hand-over: it hands a put's body from the request to the
backend's task (`hand_over`, `carry`, `received`). Two shapes run from one binary (`Settings::Storage`,
`Settings::Cache`; `server/config.rs`; `serve_until_signalled` in `server/cli.rs`, `serve_cache_until_signalled` in
`server/cache_cli.rs`): a **build node** (a `LocalCache` over a directory, no journal or lease, hazard: the shared
directory, section 5) and a **storage node** (stateless proxy plus a reconciler under a lease, hazard: lease and journal,
section 6).

**What tierstore registers at runtime** (liveness only, `host-updates.md` 2): name `tierstore`, the version, health
`GET /readyz` on the instance's private port. **Exists**: `/readyz` is 200 until the drain begins, 503 after
(`Server::respond`, `probe`, `server/mod.rs`); `/healthz` is 200 while the process runs; both unsigned. **Gap**: the
version is only in the start line and `--version`, and is `CARGO_PKG_VERSION`, not a commit. New: `tierstore_build_info
{version,slot}` on `/metricz`; `/readyz`'s 200 body saying `ready read-only` or `ready read-write` (status unchanged);
`tierstore --capabilities`, one capability per line, where `handover-v1` means "has the lock, read-only mode, wildcard
listeners and the lingering drain".

## 2. The declaration (`/etc/updater.d/tierstore.toml`, installed with the release)

| item | value | note |
|---|---|---|
| hook | `tierstore update-hook <step>`, `hook.user` and `hook.group` `tierstore-update` | the staged release's own binary; Rust, no shell. A **hook-only account** that differs from the service's `uid = "tierstore"` (`host-updates.md` 3.4.1): the service must not be able to write what the hook trusts. The polkit rule for the units attaches to this account |
| `CAP_NET_ADMIN` | steps that touch nft: `prepare`, `cutover`, `rollback` | the updater's root part launches the step as a transient unit with the capability ambient (`host-updates.md` 3.4); the hook is never root. Declared as `hook.step_capabilities.prepare`, `.cutover` and `.rollback`; `hook.capabilities`, the fallback for `verify` and `cleanup`, is empty. (`prepare` needs it to read and load the guard table: nftables netlink requests need it even to list) |
| units, families | `tierstore@9301.service`, `tierstore@9302.service` (one template, `%i` is the port, below); `AF_UNIX AF_INET AF_INET6 AF_NETLINK` | the only units it may start, stop or mask; netlink for nft |
| record directory | `state_dir = "/var/lib/updater/hooks/tierstore"`, created by the updater, owned by `tierstore-update` (`host-updates.md` 2.4, 3.4.1) | holds `state.json` (temporary file, fsync, rename), an exclusive lock for the hook's own steps, and `slots/<port>`. **Outside every path the service may write**; never under `/run` |
| slot links | `<state_dir>/slots/<port>` points at a staged release (`/var/lib/updater/releases/tierstore/<digest>/`, root-owned, read-only), or is absent | a retired slot has none; the service executes it and cannot change it. **Conflict to settle**: the service must traverse `state_dir`, which 3.4.1 allows "through group membership" (mode 0750, the service in the group, read only) and the 2.4 example's `0700` does not |
| lock files, nft tables | build node `<cache>/.lock`, storage node `/var/lib/tierstore/instance.lock`; `kbf_forward_tierstore`, `kbf_guard_tierstore` | the lock is the service's (section 5), no step reads a value from it; tables: `host-updates.md` 4.1.1 |
| read / write paths | read `/usr/sbin/nft` and `/etc/updater.d/tierstore.toml` (root-owned, 0644); `write_paths = []`, `state_dir` is implicit | no step touches the cache directory or the service's state. Asks of the 2.4 example: add the declaration to `read_paths` (the validator below reads `health_ports` and `units` from it); `/etc/tierstore` and `/usr/libexec/kbf` are not read by any step |
| timeouts (s), deadline | `prepare` 600, `cutover` 300, `verify` 1000, `rollback` 600, `cleanup` 1300; `verify_deadline_secs` 1600 | `verify`: `promotion_bound_secs` 900 + margin; `prepare`: self-test of the tiers (`--backend-check-wait`); `cleanup`: settle 70 + `--drain-deadline` 900 + lease 5 + margin, and the unit's `TimeoutStopSec` becomes 1100 (960 in `komira-ai/tierstore:packaging/tierstore/tierstore.service`, which assumes a 10 s wait); the deadline is at least `verify` plus `rollback` |
| applied by the updater | `promotion_ceiling_secs` 3600, `cleanup_retries` 3; `[hook.params]` `promotion_bound_secs = 900` | the one ceiling and the forward re-runs of `cleanup`; params passed as `--params <file>` with the reserved key `updater.promotion_ceiling_secs` added (section 3, `verify`) |

Slot unit template (flags only, no environment; the rest as today's `komira-ai/tierstore:packaging/tierstore/tierstore.service`):
`ExecStart=<state_dir>/slots/%i/tierstore --listen 0.0.0.0:%i --listen [::]:%i --proxy-id tierstore-%i
--drain-lb-wait 70s --drain-deadline 15m ...` and `ConditionPathExists=` the same path, so a slot with no link does not
start. No `[Install]`, as today's unit has none. One template starts both slots, so the settle (`--drain-lb-wait`,
host-updates.md Question 13) is the same for either. `--listen` repeated and the proxy id per slot are new (sections 4, 6).

**What starts the active slot at boot: the declaration's `boot = "restore"`, not an enabled unit** (recommended). The
updater's root part runs `tierstore update-hook restore` at its own start, before any unit, with the hook's rights and
step record (`host-updates.md` 2.4, 3.5). `restore` is not one of the five steps: it validates `state.json` (section 3),
applies the redirect and the guard for the slot it names, and runs `systemctl start tierstore@<that slot>`; the other slot
is not started. Which slot it names: with no update in flight, `serving`; with an update in flight **and no `stopping`
marker**, the **previous** slot, so a reboot before the commit is an abort (the new slot is not started and `previous` is
the rule target); with `stopping` set, `serving` (the new slot). The updater then follows 3.2 for the step the record
shows pending: a pending `cutover` is rolled back (the old slot is already running and the rule names it, so `rollback`
only has to stop what is not running), a pending `verify` fails and rolls back, a pending `cleanup` is re-run forward.
Why not `WantedBy=` on the slot units, with `enable` and `disable` done by `cutover` and `cleanup`: it needs the right to
manage unit files (a polkit action beyond start and stop), it makes two sources of truth (the symlinks and the record)
that a crash can split, and a reboot between the two would start both. With `restore` there is one record and one rule
target, and running it again repairs a rule lost to a `flush ruleset`. The units have no `[Install]`.

## 3. The steps

Every step reads the record first and sets state, never "does a thing once": that is what makes a re-run after a crash at
any point converge. The record, `state.json` in `state_dir`, has `schema`, `serving` (the port the rule names), `previous`
(the port of v-current while an update is in flight, else absent), `mode`, and markers.

**The record is read through a validator, before any other call** (`host-updates.md` 3.4.1; a file planted by the service
cannot even be written there, and the hook still does not trust it). The valid ports are the declaration's `health_ports`
(9301, 9302) and the valid units are its `units`, both read from `/etc/updater.d/tierstore.toml`; `serving` and `previous`
must each be one of the two ports and differ; `mode` is `handover` or `restart`; each marker is one of `swapped`,
`verified`, `stopping`; `schema` is one the binary knows. A unit name is built from a validated port, never read. Anything
else is refused with exit `1`, class `environment`, reason `state_invalid`, **before any `nft`, `systemctl` or file call**.
The same goes for the params file (numbers only, within sane ranges) and the environment (empty). The updater then runs
`rollback`, which reads the same record, refuses the same way, and ends in `halt` (`host-updates.md` 3.2): a record that
cannot be trusted is an operator's. The idle port is the other one of `serving`, recomputed every time.

**`prepare <artifact>`** (before anything visible). Check `tierstore --capabilities` of the staged release and of the one
the `serving` slot links to: both must have `handover-v1`, else the hook records `mode=restart` and `cutover` does a plain
restart with a gap (the first release with the lock, section 5; host-updates.md Question 15). Refuse with `75` (host not
ready, nothing done) if the guard table cannot be loaded, `nf_conntrack_tcp_timeout_established` is not longer than the
service's idle bound, or `nf_conntrack_count` is near `nf_conntrack_max` (`host-updates.md` 4.1.3). Point the idle slot's link at the artifact,
start `tierstore@<idle>`, and poll `/readyz` on `127.0.0.1:<idle>` until it answers 200 with `read-only` (a counted
number of attempts; the unit starts only after its self-test). It records `previous = serving`. *Idempotent*: the link is
replaced by rename, `systemctl start` of a started unit does nothing, the guard table is loaded by replacing it, the idle
port comes from the validated record. *Fails*: v-next never ready: `1`, class `release`.

**`cutover`** (the visible step). First check that the idle instance still answers `/readyz` with `read-only` (after a
reboot it would not): if not, `1`, nothing changed. Then write intent to the record, apply the swap file of 4.1.2 for the
idle port in one `nft -f`, then probe through 9300 until a request is answered by v-next (its build gauge names the slot
and the version), a counted number of times. A probe that never succeeds swaps back and exits `1`. Records `swapped` and
sets `serving` to the new port. *Idempotent*: the file replaces the table and names the target port, so a second apply
changes nothing; a crash before `swapped` is re-run. *Restart mode*: stop v-current, start v-next, wait for readiness,
then the same swap.

**`verify`** (the gate; run again and again). Before `cleanup`: v-next is ready, a counted series of probes through 9300
is answered by v-next with the staged version, the redirect names v-next, and v-current still serves. It exits `0` with
`"promotion_pending":true`: v-current holds the lock, so v-next is read-only by design (`host-updates.md` 3.8). After
`cleanup` has stopped v-current (the `stopping` marker): wait for `/readyz` to say `read-write` for
`promotion_bound_secs` (900, from the params file); if it does, `0` with no pending flag; if the bound passes, `0` with
`"promotion_pending":true` and the holder of the lock in `detail`. **There is no ceiling in the hook**: the updater keeps
running `verify` as its post-success watcher, outside the step table, and applies `promotion_ceiling_secs` (3600) itself,
ending the watch as `Failed(after_commit)` for a storage node and a build node alike (`host-updates.md` 3.2); the hook
receives the value as `updater.promotion_ceiling_secs` in the params file and must not exit `1` for it. After the commit
`verify` exits `1` only for a fault of its own observation (v-next crashed, answers wrongly), class `release`, which the
updater also reports as `Failed(after_commit)` and never rolls back. **Exit `75` is not used**: nothing here is "not
ready, try later with nothing done". *Idempotent*: it only observes, and sets the `verified` marker, which records the boot id read at that moment.

**`rollback`** (before the stop of v-current only). Apply the swap file for the other port (the one `previous` names once
`swapped`, else the one `serving` names), then stop the instance the restored rule does not name, and clear `previous` and
the markers. *Idempotent*: stopping a stopped unit and re-applying a swap are no-ops. **It is valid only before `cleanup`
has written its `stopping` marker.** After it, v-current is exiting or gone and a swap back would send new connections
to a closed port, so `rollback` exits `1` with `"reversible":false`, which the updater reads as `Failed(after_commit)`,
not `halt` (`host-updates.md` 3.2).

**`cleanup`** (the commit point of tierstore). The updater calls it only after a successful `verify`, and the hook
refuses anyway: **unless the record says `verified`, `cleanup` does nothing and exits `1` with `"reversible":true`** (`host-updates.md` 3.2: a
`cleanup` must not commit on its own authority, and a step that commits re-observes first). If `verified` is set, and
`stopping` is not yet written, `cleanup` first re-observes in its own run: v-next serves through 9300 (the build gauge
names the slot and version) and the boot id equals the one the marker holds. A failed check, or a stale marker (another
boot id), commits nothing and exits `1` with `"reversible":true`; the updater runs `rollback`. Then write `stopping` and from here print
`"reversible":false` on every exit, so the updater records the commit (a re-run after `stopping` does not re-observe); then `systemctl stop tierstore@<previous>`. The
process sees SIGTERM, keeps accepting for the settle (`--drain-lb-wait`, 70 s, the last SYN retransmit of a default
client, `host-updates.md` 4.1.3 and Question 13), applies P1 and P2 (section 7, Question 14), waits for requests in flight
at most `--drain-deadline`, gives up the lease, and exits; the unit's stop timeout kills a hung one and the kernel frees
the lock. Then retire the slot: remove its link, `systemctl mask --runtime`, clear `previous`. *Idempotent*: every part
is "set to state": stop of a stopped unit, link absent, mask present, `previous` cleared. A `cleanup` that fails after
`stopping` is re-run forward by the updater up to `cleanup_retries` (3) times, then `Failed(after_commit)`; it is never
rolled back.

## 4. What tierstore changes in the listener

The redirect rewrites a remote packet's destination to the address of the interface it arrived on, so the private ports
listen on the **wildcard of both families**, not loopback (`host-updates.md` 4.1.1, Question 6); the guard table keeps a
direct connection to a private port out. Today `--listen` is one `SocketAddr` (`server/cli.rs`, field `listen`; one
`listen` key in `server/config_cache.rs`), bound by one `TcpListener::bind` (`run_serving`, `serve_cache`), which cannot
set `IPV6_V6ONLY`. New: a repeatable `--listen`, two sockets, `IPV6_V6ONLY` explicit; a start that cannot bind both is
refused. `--oci-listen` (`oci::OciListener`) has the same single address (question 4 below).

## 5. Cache lock and read-only mode (build node)

**The lock decides the mode** (new; no `flock` or lock file exists under `komira-ai/tierstore:crates/tierstore/src`). A start tries
`<path>/.lock` with `flock(LOCK_EX | LOCK_NB)` on a descriptor it keeps open (close-on-exec, no follow, a local file
system): it gets it and is read-write, or it does not and is **read-only until it does**. No flag: a lone instance after a
reboot, or a restarted v-current, is read-write at once; a second instance can never write beside the first.

- **Waiting**: a task retries on an interval on the server's clock (a second by default); it blocks no thread, nothing
  waits on the lock inside a request, and a test counts retries and never sleeps.
- **Taking it**: `layout::clear_stage`, `layout::scan` with deletes, replace the index, enable fills, keeps and eviction
  (a storage node: the reconciler), then publish `read-write`. v-current's last act is `flush_touches` at the end of
  `maintain`, so the rescan sees its recency. Only the holder mutates the directory.
- **v-current crashes or hangs**: the kernel frees the lock; a hung one is killed by the unit's stop timeout, so v-next
  takes it after at most the drain deadline plus margin and serves read-only until then.
- **A v-current that predates the lock** holds nothing, so v-next would get the lock at once while it still writes:
  hence `prepare`'s capability check and `mode=restart`. The first release with the lock goes in by a plain restart.
- **A replaced `.lock`** is a lock on nothing: the holder compares `fstat` of its descriptor with `stat` of the path when
  it takes the lock and periodically, and releases and retries on a mismatch. A storage node's lock file is under
  `/var/lib/tierstore/`, not a `RuntimeDirectory=` (systemd removes it with the unit).

**Read-only behaviour.** Hits come from the existing directory; misses and writes go to the upstream. Nothing like it
exists (`CacheConfig::write_through` is about client writes; a read cache already refuses them at the front,
`Server::respond`, `read_cache`). A hit beside v-current is safe: the data file is a create-only hard link of a complete
file (`fill.rs: publish` renames the sidecar, then links the data file last), and an eviction unlinks a name without
touching bytes a reader has open. v-next's index is a snapshot: an object v-current fills later is a miss, one it evicted
is a failed read and a miss. The five writes to the shared directory today and how each is disabled:

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

- **Any number of proxies work over the same tiers** (`lib.rs`; `Proxy::open` goes on after the highest number it finds),
  but **one proxy id for both is wrong**: two writers on one journal interleave, and the reconciler treats it as one
  stream (oldest first by journal order within a proxy). The unit already says to give each process its own id; the slot
  template gives `tierstore-9301` and `tierstore-9302`, stable across updates.
- **One lease holder name is safe**: `ReconcilerConfig::holder` is the proxy id and the body is
  `holder\n<nonce>\n<generation>` (`Lease::body`), the nonce 128 random bits as hex drawn in `Lease::new` and the
  generation counting from 1 in each process; the content tag hashes the body. A restarted process with the same name
  writes other bodies than the process before it, so a stale renew or release with the old tag fails. (Before the
  nonce, the body was `holder\n<generation>` and the two could collide.)
- **A `dir:` hot tier cannot have two holders**: `DirBackend::put_if_match` is atomic among the calls of one backend only
  (`dir.rs`). It keeps the plain restart (`mode=restart`).

**The same lock decides the reconciler.** A storage node takes `--lock-file` (new) with the same try loop. v-next serves
reads and writes through its own proxy at once and does not run the reconcile loop until it holds the lock: v-current
holds the lease through the whole drain (`reconcile` ends only when `serve` returns, `run_serving`); a second runner
takes it only by expiry or makes v-current lose it silently, which `Lease` documents; and v-current gives it up on exit
(`Reconciler::stop`, at most `STOP_WAIT` = 5 s), so v-next's first `ensure` after taking the lock finds it free.

## 7. The drain of v-current: P1 and P2

Today, when told to stop (`Server::serve_from`, `respond`, `finish`): `/readyz` 503 at once; for `drain_lb_wait` (10 s) it
serves everything; then the listener is dropped and every request **on a connection still open** is answered 503; then it
waits for the requests in flight, at most `drain_deadline` (15 min), and closes the connections. After the swap each
established keep-alive connection still reaches v-current and would get a 503 while one long transfer holds the drain
(host-updates.md Question 14). Two changes, independent of how the port moves:

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
| `cache_bytes`, `cache_objects` (a snapshot in v-next), `cache_fs_free_bytes` | start snapshot / live | **double-count** one directory and one file system |
| `hits_total`, `misses_total`, `read_errors_total`, `reads_total`, `bytes_served_total`, `bytes_read_total` (`tier`, `op`), `promotions_*` (storage), `oci_requests_total`, `oci_bytes_served_total`, `oci_digest_mismatches_total`, `oci_front_info` | count | per process; the sum is right |

Counters restart at 0 in a new process; a rate handles it; read the directory gauges from the lock holder. No family names
a build, slot or mode today, and a scrape of 9300 does not say which instance answered. New: `tierstore_build_info
{version,slot}`, `tierstore_cache_mode{mode}`, `tierstore_drain_late_requests_total`. A scraper that wants both instances
scrapes the two private ports from the node (the guard blocks off-host); one is down between updates, so an alert is on
"no instance up", not on `up == 0`.

## 9. Failure cases, per step

| step | case | what happens | state after |
|---|---|---|---|
| any | `state.json` invalid or planted (a port outside the declared two, a bad mode or marker, an unknown schema) | `1`, `state_invalid`, **no `nft`, `systemctl` or file call**; the updater's `rollback` refuses the same way and ends in `halt` | host as it was; an operator looks |
| `prepare` | v-next never ready; guard cannot load; conntrack bounds fail | `1` or `75`; the updater runs `rollback`, which stops v-next | v-current serves, rule untouched |
| `cutover` | `nft -f` fails | one transaction: the old rule stays; `1` | v-current serves |
| `cutover` | probe never answered by v-next | swap back inside the step; `1` | v-current serves |
| `verify` (before cleanup) | v-next crashes or answers wrongly | `1`; the updater runs `rollback`: swap back, stop v-next; v-current still accepts | v-current serves |
| `cleanup` | no `verified`, a boot id that changed since it, or v-next not serving through 9300 | commits nothing; `1`, `reversible:true`; the updater runs `rollback` | v-current serves |
| `cleanup` | killed after `stopping`, or fails after it | re-run forward by the updater up to `cleanup_retries` (3): stop, retire; no swap back; then `Failed(after_commit)` | v-next serves |
| `cleanup` | v-next crashes after `stopping` | no swap back (v-current has stopped accepting); systemd restarts v-next, read-only while v-current exits, read-write at once if gone; the coordinator rolls back by a new hand-over | a gap until v-next is up; failed |
| `cleanup` | v-current hangs past the drain deadline | `Drained.abandoned` counts what was cut; the unit is killed at its stop timeout; the kernel frees the lock | v-next takes it |
| `verify` (after the commit) | lock not taken within `promotion_bound_secs` | `0` with `promotion_pending`; the coordinator holds the slot; the updater's watcher keeps running `verify` | at `promotion_ceiling_secs` the updater reports `Failed(after_commit)`; the hook exits `1` for nothing of the kind |
| `rollback` | asked after `stopping` | `1`, `reversible:false` | `Failed(after_commit)`, not `halt` |
| any | v-current crashes during the overlap | the kernel frees the lock; v-next takes it; if the swap has not happened the next `cutover` finds v-next ready and applies it | v-next serves, read-write |
| any | host reboots mid-update | `restore` (boot entry point): before `stopping` it starts the previous slot and points the rule at it (an abort, the pending step is rolled back); with `stopping` it starts the new slot, which finds the lock free | one instance; no overlap |
| any | the disk fills while both run | v-current's fills and eviction handle it as today (`critical`, `free_floor`); v-next writes nothing; stale entries are misses | no corruption |

## 10. Test plan (tests first)

House rules: nothing asserts timing, no real sleeps, injected clocks and counted events. Each test names the defect it
catches and is written red before the code. **Conformance, one line per step** (the suite of `host-updates.md` 9, with the
real hook binary against a fake service and a fake `nft` and `systemctl`):

- **C1 `prepare`**: twice leaves one started idle unit, one link, one guard table; killed at each boundary and re-run
  reaches the same state; no `handover-v1` yields `mode=restart`; a bad conntrack bound is `75`.
- **C2 `cutover`**: twice leaves one rule naming the idle port; killed between apply and `swapped` converges; a failing
  probe swaps back and exits `1`; the swap is one `nft -f` (a recording fake sees one call).
- **C3 `verify`**: twice changes nothing; the marker holds the boot id; with the lock held it exits `0` with `promotion_pending`; with `read-write` it
  clears the flag; a wrong version or slot through 9300 is `1`; with `updater.promotion_ceiling_secs` in the params file
  and the lock never taken it still exits `0` (red: a hook that exits `1` at the ceiling).
- **C4 `rollback`**: after `cutover` it returns the fake to its earlier state; after `cleanup`'s `stopping` marker it
  exits `1` with `reversible:false` and changes nothing.
- **C5 `cleanup`**: without `verified`, with a `verified` marker holding another boot id, or with v-next not serving
  through 9300, it exits `1` with `reversible:true` and stops nothing (red: a hook that stops v-current); with a fresh one, twice and
  killed at each boundary, it ends with the unit stopped, link absent, mask present, `previous` cleared, and every exit
  after `stopping` carries `reversible:false`.
- **C6 `restore`** (boot): before `stopping` it starts only the previous slot and names it in the rule; with `stopping`
  only the new one; twice changes nothing.
- **C7 planted record** (`host-updates.md` 3.4.1): a `state.json` planted anywhere the service can write changes nothing;
  one in `state_dir` with a port outside the two, an unknown mode or marker, equal `serving` and `previous`, or a newer
  schema makes every step and `restore` exit `1` with the fake `nft` and `systemctl` recording **no call** (red: a hook
  that takes the port from the record). nft tests: see `host-updates.md` 9. The service's own tests follow.
- **T1 hand-over under load, no retry.** N clients put (create-only) and get, keep-alive and new, each sending its next
  request when it has the previous answer, no retry of any request, while all five steps run end to end (remote clients
  through a veth peer as well as local ones). Zero failed requests; every object a client was told was written reads
  back with its bytes and hash; both instances answered (build gauge); `tierstore_drain_late_requests_total` is 0; the
  directory passes `scan` with nothing discarded. Without P1 and P2 it fails; the same run ending in `rollback` before
  `cleanup` has zero failures too.
- **T2 lock and self-take.** Two processes on one directory: the second is read-only and its retry count rises while the
  first holds the lock (an injected clock ticks the retries); after the first is killed it takes the lock, runs
  `clear_stage` and `scan` (a half-published sidecar is cleaned), serves a counted fill and says `read-write`; a replaced
  `.lock` is released and retaken; a lone instance is `read-write` before it answers `/readyz`.
- **T3 read-only writes nothing.** Start read-only on a directory with a half-published sidecar and files in `.stage/`;
  serve hits and misses (also an object v-current evicted and refilled: served, nothing deleted); compare the whole tree
  (names, sizes, modes, mtimes) before and after; one case per row of the table in section 5; the fill and eviction
  families stay 0 and the build and mode gauges differ between instances.
- **T4 lease and journal.** Two proxies with slot ids over one hot tier write separate journals; v-next's reconciler
  makes no `ensure` call before it holds the lock file (a counted event), then takes the lease v-current released; two
  starts never write the same lease body.
- **T5 drain.** P1 on a paused clock over in-memory pipes (the pattern of `drain_tests.rs`): every answer after the drain
  begins says `Connection: close`, none is 503. P2, deterministic, over a pair of `duplex` streams, events counted, no
  clock: an idle keep-alive connection when accepting stops; in order, the server shut down its write half (the client's
  read returns 0 bytes), the connection is not dropped (open count 1), the client closes and the count goes to 0. A
  request the client writes after the FIN is read, dropped and counted (`late_requests` 1), and the close still waits for
  the client. Over real loopback TCP, ordered by events: a client holding unsent request bytes when the server closes
  reads a clean end of stream, never `ECONNRESET`; a plain `close()` fails it.

## 11. Questions for the maintainers

1-3. *Settled in `host-updates.md` 3.2 and 3.4*: a failed `cleanup` after the stop is re-run forward up to
   `cleanup_retries`, then `Failed(after_commit)`, never rolled back; after a failed `prepare` or `cutover` the updater
   runs `rollback`, and `cleanup` runs only after a successful `verify` (the hook still refuses without `verified`);
   capabilities are per step.
4. **Listeners** (the wildcard decision is host-updates.md Question 6). A repeatable `--listen` with explicit
   `IPV6_V6ONLY`, and the same for `--oci-listen`. Recommendation: both in the first tierstore change, or the OCI front's
   clients see a gap at every update.
5. **P1 and P2** (host-updates.md Question 14) change the plain restart's drain too (a connection is closed, never
   answered 503). Recommendation: the only behaviour, not a flag.
6. **The residual race** (a request within one round trip of the FIN on a connection idle since before the drain).
   Recommendation: accept it as the stated limit, counted, and do not ask clients to retry a create-only put.
7. **First lock release** goes in by `mode=restart` (host-updates.md Question 15), and so does a `dir:` hot tier.
   Recommendation: accept.
8. **Version source.** Recommendation: the build gauge, with the commit baked into the binary rather than the crate version.
9. **`state_dir` mode.** The slot links must be executable by the service, so `state_dir` cannot be `0700`
   (host-updates.md 3.4.1 says read access "through group membership", the 2.4 example says `0700`). Recommendation: mode
   `0750`, group `tierstore-update`, the service account a member, read only; the record stays unwritable by the service.
