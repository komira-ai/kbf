# Spike evidence (raw output; throwaway, never merged)

Pinned Envoy 1.39.3 (the CI pin, sha256 f61653fb...c200), box: 28-thread i7-14700K, 61 GB, ext4 on NVMe, load average 13 to 22 from other sessions during every run. Timestamps are seconds since the scenario started.

## spike-envoy silent

```
[t=  0.000s] scenario: == silent ==
[t=  0.073s] A-defaults: envoy started (admin 31000)
[t=  0.084s] B-timeout0-hcm-idle5s: envoy started (admin 31003)
[t=  0.084s] C-box-routeidle0-hcm-idle5s: envoy started (admin 31006)
[t=  0.084s] D-progress-2s-hcm-idle5s: envoy started (admin 31009)
[t=  0.085s] F-max_stream_duration-6s-control: envoy started (admin 31012)
[t= 15.112s] B-timeout0-hcm-idle5s: stream ENDED at 5.00s after open: error Internal: h2 protocol error: error reading a body from connection (frames after first: 0)
[t= 16.108s] F-max_stream_duration-6s-control: stream ENDED at 6.00s after open: error Internal: h2 protocol error: error reading a body from connection (frames after first: 0)
[t= 24.112s] D-progress-2s-hcm-idle5s: stream still OPEN after 14s (frames after first: 6); stub cancelled=0
[t= 24.112s] D-progress-2s-hcm-idle5s: then the stub ended it: clean end (OK trailers)
[t= 25.112s] A-defaults: stream ENDED at 15.00s after open: error Internal: h2 protocol error: error reading a body from connection (frames after first: 0)
[t= 35.109s] C-box-routeidle0-hcm-idle5s: stream still OPEN after 25s (frames after first: 0); stub cancelled=0
[t= 35.110s] C-box-routeidle0-hcm-idle5s: then the stub ended it: clean end (OK trailers)
```

## spike-envoy healthflip

```
[t=  0.000s] scenario: == healthflip ==
[t=  0.000s] healthflip: envoy started (admin 31000)
[t=  0.047s] flags: [(31002, []), (31003, [])]
[t=  0.047s] raw: {port:31002, health_status:{"eds_health_status":"HEALTHY"}} {port:31003, health_status:{"eds_health_status":"HEALTHY"}}
[t=  0.047s] raw-healthy: {port:31002, health_status:{"eds_health_status":"HEALTHY"}} {port:31003, health_status:{"eds_health_status":"HEALTHY"}}
[t=  0.048s] stream: silent stream is on stub a
[t=  3.049s] flip: stub a now answers NOT_SERVING to the health check
[t=  4.036s] flags: [(31002, ["failed_active_health_check"]), (31003, [])] (detected 0.99s after the flip)
[t=  4.036s] raw-failed: {port:31002, health_status:{"eds_health_status":"HEALTHY","failed_active_health_check":true}} {port:31003, health_status:{"eds_health_status":"HEALTHY"}}
[t= 29.039s] stream: 25s after the check failed: OPEN open frames=0; stub a open=1 cancelled=0
[t= 38.061s] load: 1603 unary calls, 0 errors []
[t= 38.061s] load: calls answered by the failed host a: 20 after the flip, last at +0.98s (detected at +0.99s); by the other: 1321
[t= 38.061s] load: after recovery, calls to a again: 119
[t= 38.061s] stream: stub ended the stream: ended clean end (OK trailers); cancelled=0
```

## spike-envoy breakers

```
[t=  0.000s] scenario: == breakers ==
[t=  0.000s] breakers-default: envoy started (admin 31000)
[t=  0.089s] breakers-default: 1100 silent streams requested: 1024 open, errors {"Unavailable: upstream connect error or disconnect/reset before headers. reset reason: overflow": 76}
[t=  0.090s] breakers-default: stats cluster.kbf_reapi.circuit_breakers.default.cx_open: 0; cluster.kbf_reapi.circuit_breakers.default.rq_open: 1; cluster.kbf_reapi.upstream_cx_active: 4; cluster.kbf_reapi.upstream_cx_overflow: 0; cluster.kbf_reapi.upstream_rq_503: 76; cluster.kbf_reapi.upstream_rq_active: 1024; cluster.kbf_reap
[t=  0.090s] breakers-default: clusters kbf_reapi::default_priority::max_connections::1024; kbf_reapi::default_priority::max_requests::1024; kbf_reapi::high_priority::max_connections::1024; kbf_reapi::high_priority::max_requests::1024
[t=  0.095s] breakers-box-5000: envoy started (admin 31004)
[t=  0.194s] breakers-box-5000: 1100 silent streams requested: 1100 open, errors {}
[t=  0.194s] breakers-box-5000: stats cluster.kbf_reapi.circuit_breakers.default.cx_open: 0; cluster.kbf_reapi.circuit_breakers.default.rq_open: 0; cluster.kbf_reapi.upstream_cx_active: 4; cluster.kbf_reapi.upstream_cx_overflow: 0; cluster.kbf_reapi.upstream_rq_active: 1100; cluster.kbf_reapi.upstream_rq_pending_overflow: 0; 
[t=  0.194s] breakers-box-5000: clusters kbf_reapi::default_priority::max_connections::5000; kbf_reapi::default_priority::max_requests::5000; kbf_reapi::high_priority::max_connections::1024; kbf_reapi::high_priority::max_requests::1024
```

## spike-envoy tcp

```
[t=  0.000s] scenario: == tcp ==
[t=  0.000s] tcp: envoy started (admin 31000)
[t=  3.055s] tcp: the TCP backend stops accepting (connect checks fail) at +3.0s
[t= 20.055s] tcp: session keep: None still open after 20s
[t= 20.055s] tcp: session close: Some(4.980607544) read -> Ok(0)
[t= 20.055s] tcp: session idle5: Some(5.000780825) read -> Ok(0)
[t= 20.055s] tcp: session defaultidle: None still open after 20s
[t= 20.055s] tcp: hosts: {port:31007, health_status:{"eds_health_status":"HEALTHY","failed_active_health_check":true}}
```

## spike-envoy hotrestart

```
[t=  0.000s] scenario: == hotrestart ==
[t=  0.000s] hr-epoch0: envoy started (admin 31000)
[t=  4.055s] hr-epoch1: envoy started (admin 31000)
[t=  4.055s] hotrestart: epoch 1 started at t=4.05s (parent shutdown 12s, drain 3s)
[t=  6.060s] admin: +2s /server_info Some("state=\"LIVE\" epoch=1"); epoch0 exit None; epoch1 exit None
[t=  8.063s] admin: +4s /server_info Some("state=\"LIVE\" epoch=1"); epoch0 exit None; epoch1 exit None
[t= 10.067s] admin: +6s /server_info Some("state=\"LIVE\" epoch=1"); epoch0 exit None; epoch1 exit None
[t= 12.071s] admin: +8s /server_info Some("state=\"LIVE\" epoch=1"); epoch0 exit None; epoch1 exit None
[t= 14.075s] admin: +10s /server_info Some("state=\"LIVE\" epoch=1"); epoch0 exit None; epoch1 exit None
[t= 16.079s] admin: +12s /server_info Some("state=\"LIVE\" epoch=1"); epoch0 exit None; epoch1 exit None
[t= 18.083s] admin: +14s /server_info Some("state=\"LIVE\" epoch=1"); epoch0 exit Some((16.094339683, "exit status: 0")); epoch1 exit None
[t= 20.086s] admin: +16s /server_info Some("state=\"LIVE\" epoch=1"); epoch0 exit Some((16.094339683, "exit status: 0")); epoch1 exit None
[t= 28.089s] hotrestart: held silent stream: ended at t=16.09s = restart +12.04s (Some(16.037048987)s after open): error Unknown: h2 protocol error: error reading a body from connection, frames=0
[t= 28.089s] hotrestart: quiet tcp_proxy session: read returned at t=16.09s = restart +12.04s: Ok(0)
[t= 28.089s] hotrestart: epoch0 exit Some((16.094339683, "exit status: 0")) (restart at t=4.05); epoch1 exit None
[t= 28.089s] hotrestart: old channel: 120 unary calls, 1 errors [("+1.2", "Cancelled: operation was canceled")]
[t= 28.089s] hotrestart: fresh channel per call: 120 unary calls, 0 errors []
```

## spike-envoy epochs

```
[t=  0.000s] scenario: == epochs ==
[t=  0.000s] epochs-0: envoy started (admin 31000)
[t=  2.055s] epochs: after epoch 0 start (+2.1s): process alive [(0, true)]
[t=  2.057s] epochs:   stream opened under epoch 0 (age 0.0s): OPEN open
[t=  2.057s] epochs-1: envoy started (admin 31000)
[t=  4.060s] epochs: after epoch 1 start (+4.1s): process alive [(0, true), (1, true)]
[t=  4.062s] epochs:   stream opened under epoch 0 (age 2.0s): OPEN open
[t=  4.064s] epochs:   stream opened under epoch 1 (age 0.0s): OPEN open
[t=  4.065s] epochs-2: envoy started (admin 31000)
[t=  6.068s] epochs: after epoch 2 start (+6.1s): process alive [(0, false), (1, true), (2, true)]
[t=  6.068s] epochs:   stream opened under epoch 0 (age 4.0s): ENDED error Unknown: h2 protocol error: error reading a body from connection
[t=  6.070s] epochs:   stream opened under epoch 1 (age 2.0s): OPEN open
[t=  6.072s] epochs:   stream opened under epoch 2 (age 0.0s): OPEN open
[t=  6.073s] epochs-3: envoy started (admin 31000)
[t=  8.075s] epochs: after epoch 3 start (+8.1s): process alive [(0, false), (1, false), (2, true), (3, true)]
[t=  8.076s] epochs:   stream opened under epoch 0 (age 6.0s): ENDED error Unknown: h2 protocol error: error reading a body from connection
[t=  8.076s] epochs:   stream opened under epoch 1 (age 4.0s): ENDED error Unknown: h2 protocol error: error reading a body from connection
[t=  8.078s] epochs:   stream opened under epoch 2 (age 2.0s): OPEN open
[t=  8.080s] epochs:   stream opened under epoch 3 (age 0.0s): OPEN open
[t= 53.081s] epochs: epoch 0 exit: Some((4.10596798, "exit status: 0"))
[t= 53.081s] epochs: epoch 1 exit: Some((6.112601612, "exit status: 0"))
[t= 53.081s] epochs: epoch 2 exit: Some((46.123185012, "exit status: 0"))
[t= 53.081s] epochs: epoch 3 exit: None
[t= 53.081s] epochs: final: stream of epoch 0 (age 51.0s): ENDED clean end (OK trailers)
[t= 53.081s] epochs: final: stream of epoch 1 (age 49.0s): ENDED error Unknown: h2 protocol error: error reading a body from connection
[t= 53.081s] epochs: final: stream of epoch 2 (age 47.0s): ENDED error Unknown: h2 protocol error: error reading a body from connection
[t= 53.083s] epochs: final: stream of epoch 3 (age 45.0s): OPEN open
```

## spike-envoy newhost

```
[t=  0.000s] scenario: == newhost ==
[t=  0.003s] newhost-eds: envoy started (admin 31000)
[t=  4.186s] eds: host a flags: []
[t=  4.187s] eds: adding b (serving) to the file now
[t=  4.191s] eds: +0.004s host 31003: text health_flags="/failed_active_hc"  json port:31003, health_status:{"eds_health_status":"HEALTHY","failed_active_health_check":true}}
[t=  4.891s] eds: +0.705s host 31003: text health_flags="/failed_active_hc"  json port:31003, health_status:{"eds_health_status":"HEALTHY"}}
[t=  4.895s] eds: +0.708s host 31003: text health_flags="healthy"  json port:31003, health_status:{"eds_health_status":"HEALTHY"}}
[t=  4.895s] eds: b checks answered: 1
[t=  4.895s] newhost-static: envoy started (admin 31005)
[t=  4.947s] static: first flags with b not running: [(31007, []), (31008, ["failed_active_health_check"])]
[t=  7.948s] static: 3s later: [(31007, []), (31008, ["failed_active_health_check"])]
[t=  7.949s] static: starting b now
[t=  7.949s] static: +0.000s host 31008: text health_flags="/failed_active_hc"  json port:31008, health_status:{"eds_health_status":"HEALTHY","failed_active_health_check":true}}
[t=  8.932s] static: +0.983s host 31008: text health_flags="healthy"  json port:31008, health_status:{"eds_health_status":"HEALTHY"}}
```

## spike-switch meta

```
meta size=100000 cmds: built in 14.1s, applied index 100017, dir 179 MiB; second open while held: raft dir <work>/upd1 in 0.03ms; drop of the writer 44.75ms
meta size=100000: open_dir (lock + replay of 100017 entries + elect) n=9 p50=2197.2ms p99=3201.6ms max=3201.6ms
meta size=100000: replay rate ~45521 entries/s
meta size=100000: open without the lock over a copy 2524.4ms (applied 100027)
meta size=400000 cmds: built in 57.0s, applied index 400059, dir 726 MiB; second open while held: raft dir <work>/upd1 in 0.08ms; drop of the writer 254.64ms
meta size=400000: open_dir (lock + replay of 400059 entries + elect) n=9 p50=9996.1ms p99=10809.7ms max=10809.7ms
meta size=400000: replay rate ~40021 entries/s
meta size=400000: open without the lock over a copy 9737.2ms (applied 400069)
```

## spike-switch ticks

```
meta[ticks] size=100000 cmds: built in 5.3s, applied index 100008, dir 3 MiB; second open while held: raft dir <work>/upd1 in 0.04ms; drop of the writer 1.17ms
meta[ticks] size=100000: open_dir (lock + replay of 100008 entries + elect) n=7 p50=44.2ms p99=50.4ms max=50.4ms
meta[ticks] size=100000: replay rate ~2263894 entries/s
meta[ticks] size=100000: open without the lock over a copy 78.8ms (applied 100016)
meta[ticks] size=1000000 cmds: built in 55.0s, applied index 1000057, dir 33 MiB; second open while held: raft dir <work>/upd1 in 0.09ms; drop of the writer 20.54ms
meta[ticks] size=1000000: open_dir (lock + replay of 1000057 entries + elect) n=7 p50=332.1ms p99=387.4ms max=387.4ms
meta[ticks] size=1000000: replay rate ~3011755 entries/s
meta[ticks] size=1000000: open without the lock over a copy 339.9ms (applied 1000065)
```

## spike-switch state

```
state: built 300 done + 8000 unfinished (4 leased) in 157.9s; file 2 MiB (+wal 3 MiB); first open of a new file 1704.5ms
state: Store::demote, idle  n=6 p50=0.2ms p99=0.2ms max=0.2ms
state: Store::demote, under submit load n=5 p50=4.6ms p99=7.4ms max=7.4ms
state: successor SqliteStore::open after the demote n=11 p50=6.4ms p99=8.2ms max=8.2ms
state: scheduler recovery (Farm::with_state: epoch + load + recover) n=11 p50=13.2ms p99=14.7ms max=14.7ms
state: lease_epoch across the repeated handovers (Keep): [117457005402739672]
```

## Read from code, not run (each says where)

- `RaftMetaLog::query` reads only `Shared::state` (an `Arc<RwLock<MetaState>>`); `closed` guards `commit_batch` only; the directory lock is a separate `Option<File>` field. So a demote that stops the tick task, sets `closed` and drops the lock file leaves `query`, `leadership` and `applied_index` working. There is no `demote` on arch-v2, so this is by construction, not a run.
- `FsStore::open` empties `tmp/` and each open numbers its temporary files from 0 (`Inner::next`). A second store opened while the first has a PUT in flight would delete that PUT's temporary file (the link then fails) or collide on `tmp/0`. The split lock as designed is NOT safe for `file://` without a per-open `tmp/` subdirectory and a sweep that skips live owners.
- Daemon redial (`kbf-daemon/src/connect.rs`): wait after a welcomed session = `--reconnect-ms` (1000) jittered down by up to half; after the n-th failed round `--reconnect-ms * 2^(n-1)`, capped at 30 s, jittered the same way. A 100000-trial model of that schedule gives the time of the first attempt after the endpoint has been unreachable for D seconds: D=2s p99 3.8s; D=5s p99 12.3s; D=10s p99 24.5s; D=20s p99 47.1s with 4.3% past the 40 s fence; D=31s 89% past it.

## Conclusion (feeds P9/P13, kbf-installer)

Results (both pairs agree unless a range is given; fresh = new connection per call, 25 ms; one-channel = one tonic channel, 50 ms):

| scenario | result |
|---|---|
| (a) reuse_port + `POST /drain_listeners?graceful` on A, stop A when the drain period is over and HTTP is idle | PROVEN SAFE. `short`, `long`, and `over` (28 s, past D and P) streams all ended clean on A (trailers, 0 RST/GOAWAY errors, stub saw 0 cancelled). Fresh calls: 0 errors in 1624 and 1630. One-channel: 1 `Cancelled` in 835 (see surprises). New connections: after the drain began ~1 in 3 (362 of 1124) still landed on A until exactly D = 20 s (last at +19.998 s, +19.968 s), all served; 0 landed on A after D. A never exits by itself. Daemon session stayed on A until A's SIGTERM (+29.3 to +30.1 s), redialed to B 1.0 s later. |
| (a') same with `&skip_exit` or `?inboundonly` + INBOUND listeners | UNSAFE for the risk asked about: the callback skips `stopListeners`, so new connections keep landing on A for A's whole life (556 to 619 of ~1100 until the SIGTERM at +29.6 s). Never use `skip_exit`. `?inboundonly` on listeners without `traffic_direction` does nothing at all (no GOAWAY, A keeps serving 66 s, 1211 new connections). |
| (a'') non-graceful `POST /drain_listeners` | listeners closed at once: 0 new connections on A, 0 errors, but no GOAWAY: the one-channel client stayed on A for 66 s (1288 calls) and A never exits. New streams from existing connections keep landing on A. Not a swap by itself. |
| (a''') stop rule without waiting for D (SIGTERM at +2.4 to +4.0 s, HTTP idle) | 0 errors in 603 and 651 calls, but A was still accepting (41 and 79 calls landed on it, last 2 ms before the stop) and a stream starting in that window would be killed. The rule must wait for D. |
| (b) same-version hot restart (1.39.3 -> 1.39.3, and cross-release 1.38.4 -> 1.39.3: same `--hot-restart-version`) | WORKS. `short` stream (ends +8 s < P) ended clean; `over` stream and the worker tcp_proxy session were cut at P exactly (+25.29 s, +25.20 s after spawn, P = 25 s); fresh calls 0 errors in 1365 and 1345; one-channel 1 `Cancelled` in ~700. |
| (b2) second restart 4.1 s after the first (inside the window) | epoch 0's streams (short and over) and its tcp session died 64 to 118 ms after epoch 2 was spawned, 17 to 31 ms BEFORE epoch 2 reported LIVE; epoch 1's stream lived P after the second restart. Envoy did not refuse it (epoch 1 exit status 0 at +25 s). The installer must. |
| (c) hot restart into a different `--hot-restart-version` | No published release differs (all 11.104 from 1.24 to 1.40), so a real attempt is impossible. Simulated by writing the shared-memory header a different build would have written (`size_` or `version_`): the new epoch dies with SIGABRT 48 to 87 ms after spawn (`assert failure: shmem->version_ == HOT_RESTART_VERSION ... Hot restart version mismatch!`, or `size_ == sizeof(SharedMemory)`), exit by signal 6 (134 from a shell). Epoch 0 is untouched (LIVE, served calls, held stream ended clean). |
| (d) fallback: stop A, start B, no overlap | new connections fail in a window of 26 ms and 53 ms (2 to 3 fresh calls, 1 to 2 one-channel calls: `Unavailable: tcp connect error`), B answers at +56 to +101 ms; B ready 107 ms after spawn. This is a trivial config (no TLS, local stubs, load average 35+); the real number is B's start-to-ready on the box. Needed only when (a) is unavailable (a second set of ports is not possible, or reuse_port is off). |
| second Envoy on the same ports, `enable_reuse_port: false` | refused: B exits 1 in 60 to 70 ms, `cannot bind ... Address already in use`. `enable_reuse_port` unset: Linux default is on (B started; 362 vs 355 calls split between A and B). |

### The installer's Envoy swap step

1. Own step, never in the same step as the frontend swap. Before it: the new binary passes `envoy --mode validate -c <config>` on the config it will run.
2. Choose the path from `<current> --hot-restart-version` versus `<new> --hot-restart-version` (string compare):
   - equal: hot restart is allowed (proven here including 1.38.4 -> 1.39.3). Epoch = last epoch + 1, same `--base-id`, `--parent-shutdown-time-s` at least the longest action plus margin, as the kit renders it. Streams outlive the restart only until the parent shutdown time; the installer must therefore not start it while a longer action may run.
   - different (never seen on any release since 1.12; the check is a guard): do NOT attempt hot restart (SIGABRT on the new epoch, harmless to the old but pointless); take the reuse_port path.
   - A binary that cannot print `--hot-restart-version`, or a current process the installer cannot identify: reuse_port path, or fallback.
3. reuse_port path: listeners keep `enable_reuse_port: true` (explicit in the kit, even though it is the Linux default); B gets its own admin port and its own `--base-id`; start B, wait `/ready` 200 (the first health check is done by then, B serves), then `POST /drain_listeners?graceful` on A (plain `graceful`: never `skip_exit`, never `inboundonly` without INBOUND listeners, never non-graceful as the only call). `--drain-time-s` D is the window in which A still accepts (about a third of new connections); it is not a stream deadline. Stop A with SIGTERM only when both hold: D plus 1 s have passed since the drain call (the listeners are closed) and `http.<stat_prefix>.downstream_cx_active` on A has been 0 for 1 s; or a deadline of the longest action plus D, after which in-flight streams are cut and said so. SIGTERM exits in under 100 ms and cuts any open tcp_proxy session (the daemon redials in its reconnect interval).
4. Refusals: refuse a second hot restart while the first epoch's `--parent-shutdown-time-s` window is open (record the start time and epoch; the window ends when the previous epoch's process has exited); refuse a swap while A still serves a previous drain; refuse an install whose new binary reports a different hot restart version on the hot-restart path; refuse to start B when its listener would use `enable_reuse_port: false` while A is up.
5. Fallback (no overlap possible): drain the kbf frontend first so no client stream is open, then SIGTERM A and start B; new connections fail for about B's start-to-ready time (107 ms on the spike's config).

### Surprises

- No Envoy release since 1.12 has a different `--hot-restart-version`; the "cross-version cannot hot restart" case does not arise on any pair of current releases, and 1.38.4 -> 1.39.3 hot restart works.
- `/drain_listeners?graceful&skip_exit` does the opposite of its docs for this purpose: with `skip_exit` the listeners are never stopped (source: `listeners_handler.cc`, the callback that stops them is skipped), so A keeps taking ~half of new connections.
- Plain `graceful` keeps A accepting for the whole drain time (about a third of new connections), all served; A does not exit afterwards and its streams are NOT cut at D (a 28 s stream survived D = 20 s and P = 25 s): drain time is not a stream deadline outside hot restart, the installer's stop is.
- One call in roughly 800 over a long-lived tonic channel fails with `Cancelled: operation was canceled` when A sends GOAWAY (1 in every graceful-drain and hot-restart run; 0 in the early-stop runs, which had no GOAWAY race). Fresh-connection calls never failed. Idempotent unary clients must retry.
- Hot restart's second-restart kill lands before the new epoch reports LIVE (17 to 31 ms), not after.

# Envoy cross-version upgrade spike (spike/envoy-upgrade)

Harness: `spike-upgrade` (crates/kbf-spike-aio/src/bin/spike-upgrade.rs). Two pairs of real Envoy releases, sha256 checked against each release's `checksums.txt.asc`:

| release | sha256 | `--hot-restart-version` |
|---|---|---|
| 1.39.3 (CI pin, 2026-10-06) | f61653fb5f7645129d3092a82f21ce3e6f70d0a75fe2e861b0e7417790f2c200 | 11.104 |
| 1.38.4 (2026-08-26) | c994c452de131f59c9ec9f4a2fffcc65039f250a38b6279870bb95dac21db0fa | 11.104 |

Pairs run: same (1.39.3 -> 1.39.3) and cross-release (1.38.4 -> 1.39.3). **No published release differs in `--hot-restart-version`**: also 11.104 on 1.40.0, 1.37.6, 1.36.10, 1.35.11, 1.34.12, 1.33.14, 1.32.2, 1.31.5, 1.27.7, 1.24.12 (older x86_64 release binaries are not published). The string is `"{HOT_RESTART_VERSION}.{sizeof(SharedMemory)}"` and has been 11.104 since Envoy 1.12 (1.10 had a 10.x form). So (c) is simulated, see its section.

Box: 28 threads, load average 35 to 50 from other sessions during every run. Settings of #925 in every run: route timeout 0 and idle_timeout 0s, HCM stream_idle_timeout 7200s, breakers 5000, tcp_proxy idle_timeout 0s, `close_connections_on_host_health_failure` false, listeners `enable_reuse_port: true`, `--drain-strategy gradual`, active health checks 1 s.

Time scaling: the box's longest action (hours) is L = 12 s, `--drain-time-s` D = 20 s (box 60 s), `--parent-shutdown-time-s` P = 25 s (box L + 900 s). Only the order L < D < P and the fixed Envoy timers matter; nothing here depends on wall-clock magnitudes. Streams: `short` ends at +6 s, `long` at +12 s (= L), `over` at +28 s (> D, > P).

Load during every run: a unary call over a NEW connection every 25 ms ("fresh"), a unary call every 50 ms over one long-lived tonic channel opened on the old Envoy ("one-channel"), three silent server-streaming calls, and a daemon-like TCP session on the worker listener (redials 1 s after a close). Calls are answered by stub `a` (behind Envoy A) or stub `b` (behind B), so the answering Envoy is exact. Raw output below is trimmed of the 2 s status polls.

## same-1.39.3: upgrade-graceful

```
[t=  0.000s] scenario: == upgrade-graceful (old envoy-1.39.3, new envoy-1.39.3) ==
[t=  3.205s] upgrade[graceful]: B ready=true 0.10s after spawn; both listen on port 31002 (reuse_port)
[t=  7.207s] upgrade[graceful]: POST /drain_listeners?graceful on A -> Some((200, "OK\n"))
[t= 36.888s] upgrade[graceful]: A has no HTTP connection for 1s at +29.6s: SIGTERM
[t= 37.141s] upgrade[graceful]: A exit: Some(("+29.69s", "exit status: 0")) (drain began at t=7.21s; SIGTERM sent at Some(29.580617953))
[t= 47.144s] upgrade[graceful]: A http connections accepted: before drain Some(182), at exit None; B: before Some(92), now Some(847)
[t= 47.144s] upgrade[graceful]: stream short: opened t=0.05s, ended drain+6.00s (13.15s after open): clean end (OK trailers)
[t= 47.144s] upgrade[graceful]: stream long: opened t=0.06s, ended drain+12.00s (19.16s after open): clean end (OK trailers)
[t= 47.144s] upgrade[graceful]: stream over: opened t=0.06s, ended drain+28.01s (35.16s after open): clean end (OK trailers)
[t= 47.144s] upgrade[graceful]: stub a: watches 3, cancelled by the peer 0, open now 0
[t= 47.144s] upgrade[graceful]: stub b: watches 0
[t= 47.144s] upgrade[graceful]: marks (t): B-ready 3.20, drain 7.21, A-exit 36.89
[t= 47.144s] upgrade[graceful] fresh-connection calls: 1648 calls in all, 0 errors
[t= 47.144s] upgrade[graceful] fresh-connection calls:   [start..B-ready] 121 calls: on A 121, on B 0, errors 0
[t= 47.144s] upgrade[graceful] fresh-connection calls:   [B-ready..drain] 152 calls: on A 60, on B 92, errors 0
[t= 47.144s] upgrade[graceful] fresh-connection calls:   [drain..A-exit] 1135 calls: on A 390, on B 745, errors 0
[t= 47.144s] upgrade[graceful] fresh-connection calls:   [A-exit..end] 240 calls: on A 0, on B 240, errors 0
[t= 47.148s] upgrade[graceful] fresh-connection calls: calls answered by a at or after the mark: 390 (first Some(0.03), last Some(19.976)); failed calls after the mark: 0
[t= 47.148s] upgrade[graceful] one-channel calls: 843 calls in all, 1 errors
[t= 47.148s] upgrade[graceful] one-channel calls:   [start..B-ready] 62 calls: on A 62, on B 0, errors 0
[t= 47.148s] upgrade[graceful] one-channel calls:   [B-ready..drain] 78 calls: on A 78, on B 0, errors 0
[t= 47.148s] upgrade[graceful] one-channel calls:   [drain..A-exit] 581 calls: on A 19, on B 561, errors 1
[t= 47.148s] upgrade[graceful] one-channel calls:   [A-exit..end] 122 calls: on A 0, on B 122, errors 0
[t= 47.148s] upgrade[graceful] one-channel calls:   error at +0.99s: Cancelled: operation was canceled
[t= 47.148s] upgrade[graceful] one-channel calls: calls answered by a at or after the mark: 19 (first Some(0.017), last Some(0.935)); failed calls after the mark: 1
[t= 47.149s] upgrade[graceful]: daemon session: connected drain-7.15s on A, ended drain+29.68s
[t= 47.149s] upgrade[graceful]: daemon session: connected drain+30.68s on B, ended still open at the end
exit 0
```

## same-1.39.3: upgrade-early-stop

```
[t=  0.000s] scenario: == upgrade-early-stop (old envoy-1.39.3, new envoy-1.39.3) ==
[t=  3.127s] upgrade[early-stop]: B ready=true 0.05s after spawn; both listen on port 31102 (reuse_port)
[t=  7.129s] upgrade[early-stop]: POST /drain_listeners?graceful on A -> Some((200, "OK\n"))
[t=  9.603s] upgrade[early-stop]: A has no HTTP connection for 1s at +2.4s: SIGTERM
[t=  9.855s] upgrade[early-stop]: A exit: Some(("+2.48s", "exit status: 0")) (drain began at t=7.13s; SIGTERM sent at Some(2.371437652))
[t= 19.858s] upgrade[early-stop]: A http connections accepted: before drain Some(186), at exit None; B: before Some(85), now Some(149)
[t= 19.858s] upgrade[early-stop]: stub a: watches 0, cancelled by the peer 0, open now 0
[t= 19.858s] upgrade[early-stop]: stub b: watches 0
[t= 19.858s] upgrade[early-stop]: marks (t): B-ready 3.13, drain 7.13, A-exit 9.61
[t= 19.859s] upgrade[early-stop] fresh-connection calls: 603 calls in all, 0 errors
[t= 19.859s] upgrade[early-stop] fresh-connection calls:   [start..B-ready] 117 calls: on A 117, on B 0, errors 0
[t= 19.859s] upgrade[early-stop] fresh-connection calls:   [B-ready..drain] 153 calls: on A 68, on B 85, errors 0
[t= 19.859s] upgrade[early-stop] fresh-connection calls:   [drain..A-exit] 95 calls: on A 41, on B 54, errors 0
[t= 19.859s] upgrade[early-stop] fresh-connection calls:   [A-exit..end] 238 calls: on A 0, on B 238, errors 0
[t= 19.859s] upgrade[early-stop] fresh-connection calls: calls answered by a at or after the mark: 41 (first Some(0.042), last Some(2.427)); failed calls after the mark: 0
[t= 19.859s] upgrade[early-stop] one-channel calls: 309 calls in all, 0 errors
[t= 19.859s] upgrade[early-stop] one-channel calls:   [start..B-ready] 60 calls: on A 60, on B 0, errors 0
[t= 19.859s] upgrade[early-stop] one-channel calls:   [B-ready..drain] 79 calls: on A 79, on B 0, errors 0
[t= 19.859s] upgrade[early-stop] one-channel calls:   [drain..A-exit] 48 calls: on A 21, on B 27, errors 0
[t= 19.859s] upgrade[early-stop] one-channel calls:   [A-exit..end] 122 calls: on A 0, on B 122, errors 0
[t= 19.859s] upgrade[early-stop] one-channel calls: calls answered by a at or after the mark: 21 (first Some(0.046), last Some(1.072)); failed calls after the mark: 0
[t= 19.859s] upgrade[early-stop]: daemon session: connected drain-7.05s on A, ended drain+2.47s
[t= 19.859s] upgrade[early-stop]: daemon session: connected drain+3.48s on B, ended still open at the end
exit 0
```

## same-1.39.3: upgrade-graceful-skip-exit

```
[t=  0.000s] scenario: == upgrade-graceful-skip-exit (old envoy-1.39.3, new envoy-1.39.3) ==
[t=  3.121s] upgrade[graceful-skip-exit]: B ready=true 0.05s after spawn; both listen on port 31202 (reuse_port)
[t=  7.125s] upgrade[graceful-skip-exit]: POST /drain_listeners?graceful&skip_exit on A -> Some((200, "OK\n"))
[t= 37.934s] upgrade[graceful-skip-exit]: A has no HTTP connection for 1s at +30.7s: SIGTERM
[t= 38.187s] upgrade[graceful-skip-exit]: A exit: Some(("+30.81s", "exit status: 0")) (drain began at t=7.12s; SIGTERM sent at Some(30.708073133))
[t= 48.190s] upgrade[graceful-skip-exit]: A http connections accepted: before drain Some(189), at exit None; B: before Some(79), now Some(646)
[t= 48.190s] upgrade[graceful-skip-exit]: stream short: opened t=0.06s, ended drain+6.00s (13.06s after open): clean end (OK trailers)
[t= 48.190s] upgrade[graceful-skip-exit]: stream long: opened t=0.07s, ended drain+12.00s (19.06s after open): clean end (OK trailers)
[t= 48.190s] upgrade[graceful-skip-exit]: stream over: opened t=0.07s, ended drain+28.01s (35.06s after open): clean end (OK trailers)
[t= 48.190s] upgrade[graceful-skip-exit]: stub a: watches 3, cancelled by the peer 0, open now 0
[t= 48.190s] upgrade[graceful-skip-exit]: stub b: watches 0
[t= 48.190s] upgrade[graceful-skip-exit]: marks (t): B-ready 3.12, drain 7.12, A-exit 37.94
[t= 48.190s] upgrade[graceful-skip-exit] fresh-connection calls: 1682 calls in all, 0 errors
[t= 48.190s] upgrade[graceful-skip-exit] fresh-connection calls:   [start..B-ready] 115 calls: on A 115, on B 0, errors 0
[t= 48.190s] upgrade[graceful-skip-exit] fresh-connection calls:   [B-ready..drain] 152 calls: on A 73, on B 79, errors 0
[t= 48.190s] upgrade[graceful-skip-exit] fresh-connection calls:   [drain..A-exit] 1175 calls: on A 619, on B 556, errors 0
[t= 48.190s] upgrade[graceful-skip-exit] fresh-connection calls:   [A-exit..end] 240 calls: on A 0, on B 240, errors 0
[t= 48.190s] upgrade[graceful-skip-exit] fresh-connection calls: calls answered by a at or after the mark: 619 (first Some(0.035), last Some(30.71)); failed calls after the mark: 0
[t= 48.190s] upgrade[graceful-skip-exit] one-channel calls: 862 calls in all, 1 errors
[t= 48.190s] upgrade[graceful-skip-exit] one-channel calls:   [start..B-ready] 60 calls: on A 60, on B 0, errors 0
[t= 48.190s] upgrade[graceful-skip-exit] one-channel calls:   [B-ready..drain] 78 calls: on A 78, on B 0, errors 0
[t= 48.191s] upgrade[graceful-skip-exit] one-channel calls:   [drain..A-exit] 602 calls: on A 30, on B 571, errors 1
[t= 48.191s] upgrade[graceful-skip-exit] one-channel calls:   [A-exit..end] 122 calls: on A 0, on B 122, errors 0
[t= 48.191s] upgrade[graceful-skip-exit] one-channel calls:   error at +0.07s: Cancelled: operation was canceled
[t= 48.191s] upgrade[graceful-skip-exit] one-channel calls: calls answered by a at or after the mark: 30 (first Some(0.024), last Some(1.558)); failed calls after the mark: 1
[t= 48.191s] upgrade[graceful-skip-exit]: daemon session: connected drain-7.06s on A, ended drain+30.81s
[t= 48.191s] upgrade[graceful-skip-exit]: daemon session: connected drain+31.81s on B, ended still open at the end
exit 0
```

## same-1.39.3: upgrade-immediate

```
[t=  0.000s] scenario: == upgrade-immediate (old envoy-1.39.3, new envoy-1.39.3) ==
[t=  3.269s] upgrade[immediate]: B ready=true 0.16s after spawn; both listen on port 31302 (reuse_port)
[t=  7.273s] upgrade[immediate]: POST /drain_listeners on A -> Some((200, "OK\n"))
[t= 67.440s] upgrade[immediate]: A exit: None (drain began at t=7.27s; SIGTERM sent at None)
[t= 77.492s] upgrade[immediate]: A http connections accepted: before drain Some(196), at exit Some(196); B: before Some(68), now Some(2319)
[t= 77.492s] upgrade[immediate]: stream short: opened t=0.11s, ended drain+6.00s (13.17s after open): clean end (OK trailers)
[t= 77.492s] upgrade[immediate]: stream long: opened t=0.11s, ended drain+12.00s (19.17s after open): clean end (OK trailers)
[t= 77.492s] upgrade[immediate]: stream over: opened t=0.11s, ended drain+28.00s (35.17s after open): clean end (OK trailers)
[t= 77.492s] upgrade[immediate]: stub a: watches 3, cancelled by the peer 0, open now 0
[t= 77.492s] upgrade[immediate]: stub b: watches 0
[t= 77.492s] upgrade[immediate]: marks (t): B-ready 3.27, drain 7.27, A-exit (A never exited)
[t= 77.492s] upgrade[immediate] fresh-connection calls: 2739 calls in all, 0 errors
[t= 77.492s] upgrade[immediate] fresh-connection calls:   [start..B-ready] 121 calls: on A 120, on B 1, errors 0
[t= 77.492s] upgrade[immediate] fresh-connection calls:   [B-ready..drain] 142 calls: on A 75, on B 67, errors 0
[t= 77.492s] upgrade[immediate] fresh-connection calls:   [drain..A-exit] 2476 calls: on A 0, on B 2476, errors 0
[t= 77.492s] upgrade[immediate] fresh-connection calls:   [A-exit..end] 0 calls: on A 0, on B 0, errors 0
[t= 77.493s] upgrade[immediate] fresh-connection calls: calls answered by a at or after the mark: 0 (first None, last None); failed calls after the mark: 0
[t= 77.493s] upgrade[immediate] one-channel calls: 1413 calls in all, 0 errors
[t= 77.493s] upgrade[immediate] one-channel calls:   [start..B-ready] 62 calls: on A 62, on B 0, errors 0
[t= 77.493s] upgrade[immediate] one-channel calls:   [B-ready..drain] 74 calls: on A 74, on B 0, errors 0
[t= 77.493s] upgrade[immediate] one-channel calls:   [drain..A-exit] 1277 calls: on A 1277, on B 0, errors 0
[t= 77.493s] upgrade[immediate] one-channel calls:   [A-exit..end] 0 calls: on A 0, on B 0, errors 0
[t= 77.493s] upgrade[immediate] one-channel calls: calls answered by a at or after the mark: 1277 (first Some(0.018), last Some(66.2)); failed calls after the mark: 0
[t= 77.493s] upgrade[immediate]: daemon session: connected drain-7.16s on A, ended still open at the end
exit 0
```

## same-1.39.3: upgrade-inbound

```
[t=  0.000s] scenario: == upgrade-inbound (old envoy-1.39.3, new envoy-1.39.3) ==
[t=  3.195s] upgrade[inbound]: B ready=true 0.06s after spawn; both listen on port 31402 (reuse_port)
[t=  7.198s] upgrade[inbound]: POST /drain_listeners?graceful&inboundonly&skip_exit on A -> Some((200, "OK\n"))
[t= 36.353s] upgrade[inbound]: A has no HTTP connection for 1s at +29.2s: SIGTERM
[t= 36.605s] upgrade[inbound]: A exit: Some(("+29.21s", "exit status: 0")) (drain began at t=7.20s; SIGTERM sent at Some(29.155314408))
[t= 46.610s] upgrade[inbound]: A http connections accepted: before drain Some(179), at exit None; B: before Some(91), now Some(682)
[t= 46.610s] upgrade[inbound]: stream short: opened t=0.14s, ended drain+6.02s (13.08s after open): clean end (OK trailers)
[t= 46.610s] upgrade[inbound]: stream long: opened t=0.14s, ended drain+12.06s (19.12s after open): clean end (OK trailers)
[t= 46.610s] upgrade[inbound]: stream over: opened t=0.14s, ended drain+28.06s (35.12s after open): clean end (OK trailers)
[t= 46.610s] upgrade[inbound]: stub a: watches 3, cancelled by the peer 0, open now 0
[t= 46.610s] upgrade[inbound]: stub b: watches 0
[t= 46.610s] upgrade[inbound]: marks (t): B-ready 3.20, drain 7.20, A-exit 36.41
[t= 46.610s] upgrade[inbound] fresh-connection calls: 1606 calls in all, 0 errors
[t= 46.610s] upgrade[inbound] fresh-connection calls:   [start..B-ready] 116 calls: on A 116, on B 0, errors 0
[t= 46.610s] upgrade[inbound] fresh-connection calls:   [B-ready..drain] 153 calls: on A 62, on B 91, errors 0
[t= 46.610s] upgrade[inbound] fresh-connection calls:   [drain..A-exit] 1102 calls: on A 519, on B 583, errors 0
[t= 46.610s] upgrade[inbound] fresh-connection calls:   [A-exit..end] 235 calls: on A 0, on B 235, errors 0
[t= 46.610s] upgrade[inbound] fresh-connection calls: calls answered by a at or after the mark: 519 (first Some(0.098), last Some(29.133)); failed calls after the mark: 0
[t= 46.610s] upgrade[inbound] one-channel calls: 825 calls in all, 1 errors
[t= 46.610s] upgrade[inbound] one-channel calls:   [start..B-ready] 60 calls: on A 60, on B 0, errors 0
[t= 46.610s] upgrade[inbound] one-channel calls:   [B-ready..drain] 78 calls: on A 78, on B 0, errors 0
[t= 46.610s] upgrade[inbound] one-channel calls:   [drain..A-exit] 567 calls: on A 19, on B 547, errors 1
[t= 46.610s] upgrade[inbound] one-channel calls:   [A-exit..end] 120 calls: on A 0, on B 120, errors 0
[t= 46.610s] upgrade[inbound] one-channel calls:   error at +0.50s: Cancelled: operation was canceled
[t= 46.611s] upgrade[inbound] one-channel calls: calls answered by a at or after the mark: 19 (first Some(0.027), last Some(1.016)); failed calls after the mark: 1
[t= 46.611s] upgrade[inbound]: daemon session: connected drain-7.06s on A, ended drain+29.16s
[t= 46.611s] upgrade[inbound]: daemon session: connected drain+30.16s on B, ended still open at the end
exit 0
```

## same-1.39.3: upgrade-inbound-undirected

```
[t=  0.000s] scenario: == upgrade-inbound-undirected (old envoy-1.39.3, new envoy-1.39.3) ==
[t=  3.296s] upgrade[inbound-undirected]: B ready=true 0.18s after spawn; both listen on port 31502 (reuse_port)
[t=  7.298s] upgrade[inbound-undirected]: POST /drain_listeners?graceful&inboundonly&skip_exit on A -> Some((200, "OK\n"))
[t= 67.397s] upgrade[inbound-undirected]: A exit: None (drain began at t=7.30s; SIGTERM sent at None)
[t= 77.403s] upgrade[inbound-undirected]: A http connections accepted: before drain Some(191), at exit Some(1296); B: before Some(73), now Some(1240)
[t= 77.403s] upgrade[inbound-undirected]: stream short: opened t=0.11s, ended drain+6.00s (13.19s after open): clean end (OK trailers)
[t= 77.403s] upgrade[inbound-undirected]: stream long: opened t=0.12s, ended drain+12.00s (19.19s after open): clean end (OK trailers)
[t= 77.403s] upgrade[inbound-undirected]: stream over: opened t=0.12s, ended drain+28.00s (35.19s after open): clean end (OK trailers)
[t= 77.403s] upgrade[inbound-undirected]: stub a: watches 3, cancelled by the peer 0, open now 0
[t= 77.403s] upgrade[inbound-undirected]: stub b: watches 0
[t= 77.403s] upgrade[inbound-undirected]: marks (t): B-ready 3.30, drain 7.30, A-exit (A never exited)
[t= 77.403s] upgrade[inbound-undirected] fresh-connection calls: 2760 calls in all, 0 errors
[t= 77.403s] upgrade[inbound-undirected] fresh-connection calls:   [start..B-ready] 113 calls: on A 111, on B 2, errors 0
[t= 77.403s] upgrade[inbound-undirected] fresh-connection calls:   [B-ready..drain] 150 calls: on A 79, on B 71, errors 0
[t= 77.403s] upgrade[inbound-undirected] fresh-connection calls:   [drain..A-exit] 2497 calls: on A 1211, on B 1286, errors 0
[t= 77.403s] upgrade[inbound-undirected] fresh-connection calls:   [A-exit..end] 0 calls: on A 0, on B 0, errors 0
[t= 77.404s] upgrade[inbound-undirected] fresh-connection calls: calls answered by a at or after the mark: 1211 (first Some(0.007), last Some(66.085)); failed calls after the mark: 0
[t= 77.404s] upgrade[inbound-undirected] one-channel calls: 1416 calls in all, 0 errors
[t= 77.404s] upgrade[inbound-undirected] one-channel calls:   [start..B-ready] 59 calls: on A 59, on B 0, errors 0
[t= 77.404s] upgrade[inbound-undirected] one-channel calls:   [B-ready..drain] 77 calls: on A 77, on B 0, errors 0
[t= 77.404s] upgrade[inbound-undirected] one-channel calls:   [drain..A-exit] 1280 calls: on A 1280, on B 0, errors 0
[t= 77.404s] upgrade[inbound-undirected] one-channel calls:   [A-exit..end] 0 calls: on A 0, on B 0, errors 0
[t= 77.406s] upgrade[inbound-undirected] one-channel calls: calls answered by a at or after the mark: 1280 (first Some(0.023), last Some(66.053)); failed calls after the mark: 0
[t= 77.406s] upgrade[inbound-undirected]: daemon session: connected drain-7.18s on A, ended still open at the end
exit 0
```

## same-1.39.3: hrsame

```
[t=  0.000s] scenario: == hrsame (old envoy-1.39.3, new envoy-1.39.3) ==
[t=  3.425s] hrsame: restart 1 (epoch 1) spawned t=3.12s, LIVE as epoch 1: true at +0.30s
[t= 39.343s] hrsame: marks: restart1 spawn t=3.123, LIVE t=3.425; restart2 spawn None, LIVE None
[t= 39.343s] hrsame: stream short (opened on epoch 0): ended restart1 spawn+8.31s, restart1 LIVE+8.00s: clean end (OK trailers)
[t= 39.343s] hrsame: stream over (opened on epoch 0): ended restart1 spawn+25.29s, restart1 LIVE+24.99s: error Unknown: h2 protocol error: error reading a body from connection
[t= 39.343s] hrsame: worker tcp_proxy session (opened on epoch 0): ended restart1 spawn+25.29s: Ok(0)
[t= 39.343s] hrsame: exits: e0 Some((28.43456301, "exit status: 0")), e1 None, e2 None
[t= 39.344s] hrsame fresh-connection calls: 1365 calls in all, 0 errors
[t= 39.344s] hrsame fresh-connection calls:   [start..restart1] 111 calls: on A 111, on B 0, errors 0
[t= 39.344s] hrsame fresh-connection calls:   [restart1..end] 1254 calls: on A 1254, on B 0, errors 0
[t= 39.344s] hrsame one-channel calls: 702 calls in all, 1 errors
[t= 39.344s] hrsame one-channel calls:   [start..restart1] 58 calls: on A 58, on B 0, errors 0
[t= 39.344s] hrsame one-channel calls:   [restart1..end] 644 calls: on A 643, on B 0, errors 1
[t= 39.344s] hrsame one-channel calls:   error at +1.53s: Cancelled: operation was canceled
exit 0
```

## same-1.39.3: hrsecond

```
[t=  0.000s] scenario: == hrsecond (old envoy-1.39.3, new envoy-1.39.3) ==
[t=  3.168s] hrsecond: restart 1 (epoch 1) spawned t=3.06s, LIVE as epoch 1: true at +0.11s
[t=  7.309s] hrsecond: restart 2 (epoch 2) spawned +4.11s after restart 1 (inside the 25s window), LIVE as epoch 2: true at +0.135s
[t= 43.396s] hrsecond: marks: restart1 spawn t=3.062, LIVE t=3.168; restart2 spawn Some(7.173809645), LIVE Some(7.308781112)
[t= 43.396s] hrsecond: stream short (opened on epoch 0): ended restart1 spawn+4.23s, restart1 LIVE+4.12s, restart2 spawn+0.118s, restart2 LIVE-0.017s: error Unknown: h2 protocol error: error reading a body from connection
[t= 43.396s] hrsecond: stream over (opened on epoch 0): ended restart1 spawn+4.23s, restart1 LIVE+4.12s, restart2 spawn+0.118s, restart2 LIVE-0.017s: error Unknown: h2 protocol error: error reading a body from connection
[t= 43.396s] hrsecond: stream epoch1-stream (opened on epoch 1): ended restart1 spawn+29.24s, restart1 LIVE+29.13s, restart2 spawn+25.127s, restart2 LIVE+24.992s: error Unknown: h2 protocol error: error reading a body from connection
[t= 43.396s] hrsecond: worker tcp_proxy session (opened on epoch 0): ended restart1 spawn+4.23s: Ok(0)
[t= 43.396s] hrsecond: exits: e0 Some((7.297032912, "exit status: 0")), e1 Some((32.310736786, "exit status: 0")), e2 None
[t= 43.396s] hrsecond fresh-connection calls: 1527 calls in all, 0 errors
[t= 43.396s] hrsecond fresh-connection calls:   [start..restart1] 115 calls: on A 115, on B 0, errors 0
[t= 43.396s] hrsecond fresh-connection calls:   [restart1..restart2] 154 calls: on A 154, on B 0, errors 0
[t= 43.396s] hrsecond fresh-connection calls:   [restart2..end] 1258 calls: on A 1258, on B 0, errors 0
[t= 43.396s] hrsecond one-channel calls: 781 calls in all, 1 errors
[t= 43.396s] hrsecond one-channel calls:   [start..restart1] 59 calls: on A 59, on B 0, errors 0
[t= 43.396s] hrsecond one-channel calls:   [restart1..restart2] 79 calls: on A 78, on B 0, errors 1
[t= 43.396s] hrsecond one-channel calls:   [restart2..end] 643 calls: on A 643, on B 0, errors 0
[t= 43.396s] hrsecond one-channel calls:   error at +0.45s: Cancelled: operation was canceled
exit 0
```

## same-1.39.3: fallback

```
[t=  0.000s] scenario: == fallback (old envoy-1.39.3, new envoy-1.39.3) ==
[t=  3.244s] fallback: SIGTERM to A at t=3.14s; A exit Some(("+0.014s", "exit status: 0")); B spawned +0.004s, /ready=true at +0.107s
[t= 12.271s] fallback: A exit Some(("+0.014s", "exit status: 0"))
[t= 12.271s] fallback fresh-connection calls: 283 calls in all, 3 errors
[t= 12.271s] fallback fresh-connection calls:   [start..SIGTERM] 116 calls: on A 116, on B 0, errors 0
[t= 12.271s] fallback fresh-connection calls:   [SIGTERM..B-ready] 4 calls: on A 0, on B 1, errors 3
[t= 12.271s] fallback fresh-connection calls:   [B-ready..end] 163 calls: on A 0, on B 163, errors 0
[t= 12.271s] fallback fresh-connection calls:   error at +0.02s: connect: transport error
[t= 12.271s] fallback fresh-connection calls:   error at +0.05s: connect: transport error
[t= 12.271s] fallback fresh-connection calls:   error at +0.07s: connect: transport error
[t= 12.271s] fallback: new connections: first failed call at +0.022s, last failed call at +0.075s, first call answered by B at +0.101s
[t= 12.271s] fallback one-channel calls: 145 calls in all, 2 errors
[t= 12.271s] fallback one-channel calls:   [start..SIGTERM] 59 calls: on A 59, on B 0, errors 0
[t= 12.271s] fallback one-channel calls:   [SIGTERM..B-ready] 2 calls: on A 0, on B 0, errors 2
[t= 12.271s] fallback one-channel calls:   [B-ready..end] 84 calls: on A 0, on B 84, errors 0
[t= 12.271s] fallback one-channel calls:   error at +0.02s: Unavailable: tcp connect error
[t= 12.271s] fallback one-channel calls:   error at +0.07s: Unavailable: tcp connect error
exit 0
```

## same-1.39.3: overlap-false

```
[t=  0.000s] scenario: == overlap-false (old envoy-1.39.3, new envoy-1.39.3) ==
[t=  9.125s] overlap[Some(false)]: B /ready=false, exit Some(("+0.07s", "exit status: 1"))
[t=  9.125s] overlap[Some(false)]: B log: cannot bind '127.0.0.1:32102': Address already in use
exit 0
```

## same-1.39.3: overlap-default

```
[t=  0.000s] scenario: == overlap-default (old envoy-1.39.3, new envoy-1.39.3) ==
[t=  1.217s] overlap[None]: B /ready=true, exit None
[t= 12.219s] overlap[None] fresh-connection calls: 717 calls in all, 0 errors
[t= 12.219s] overlap[None] fresh-connection calls:   [start..end] 717 calls: on A 362, on B 355, errors 0
exit 0
```

## same-1.39.3: hrmismatch-version (simulated)

```
[t=  0.000s] scenario: == hrmismatch-version (old envoy-1.39.3, new envoy-1.39.3) ==
[t=  0.139s] hrmismatch[version]: shared memory /dev/shm/envoy_shared_memory_8000: size_ 104, version_ 11 (the --hot-restart-version of this binary is 11.104)
[t=  0.139s] hrmismatch[version]: poked version_ to 12: epoch 0's shared memory now says what an Envoy of another hot restart version would have written
[t=  0.203s] hrmismatch[version]: epoch 1 exit: Some(("+0.063s after spawn", "signal: 6 (SIGABRT)"))
[t=  0.203s] hrmismatch[version]: epoch 1 log: [2026-10-17 22:29:38.581][631553][critical][assert] [source/server/hot_restart_impl.cc:70] assert failure: shmem->version_ == HOT_RESTART_VERSION. Details: Hot restart version mismatch! You must have hot restarted into a
[t=  1.220s] hrmismatch[version]: epoch 0 after the failed attempt: admin Some("state=\"LIVE\" epoch=0"); fresh call Ok("a"); exit None; held stream finished: false
[t=  1.220s] hrmismatch[version]: held stream (opened on epoch 0): clean end (OK trailers)
exit 0
```

## same-1.39.3: hrmismatch-size (simulated)

```
[t=  0.000s] scenario: == hrmismatch-size (old envoy-1.39.3, new envoy-1.39.3) ==
[t= 20.097s] hrmismatch[size]: epoch 0 never served
exit 0
```

## cross-1.38.4-1.39.3: upgrade-graceful

```
[t=  0.000s] scenario: == upgrade-graceful (old envoy-1.38.4, new envoy-1.39.3) ==
[t=  3.241s] upgrade[graceful]: B ready=true 0.06s after spawn; both listen on port 24002 (reuse_port)
[t=  7.246s] upgrade[graceful]: POST /drain_listeners?graceful on A -> Some((200, "OK\n"))
[t= 36.878s] upgrade[graceful]: A has no HTTP connection for 1s at +29.5s: SIGTERM
[t= 37.569s] upgrade[graceful]: A exit: Some(("+30.10s", "exit status: 0")) (drain began at t=7.25s; SIGTERM sent at Some(29.531965902000003))
[t= 47.571s] upgrade[graceful]: A http connections accepted: before drain Some(185), at exit None; B: before Some(82), now Some(862)
[t= 47.571s] upgrade[graceful]: stream short: opened t=0.18s, ended drain+6.00s (13.07s after open): clean end (OK trailers)
[t= 47.571s] upgrade[graceful]: stream long: opened t=0.18s, ended drain+12.00s (19.07s after open): clean end (OK trailers)
[t= 47.571s] upgrade[graceful]: stream over: opened t=0.18s, ended drain+28.00s (35.07s after open): clean end (OK trailers)
[t= 47.571s] upgrade[graceful]: stub a: watches 3, cancelled by the peer 0, open now 0
[t= 47.571s] upgrade[graceful]: stub b: watches 0
[t= 47.571s] upgrade[graceful]: marks (t): B-ready 3.24, drain 7.25, A-exit 37.34
[t= 47.572s] upgrade[graceful] fresh-connection calls: 1624 calls in all, 0 errors
[t= 47.572s] upgrade[graceful] fresh-connection calls:   [start..B-ready] 113 calls: on A 113, on B 0, errors 0
[t= 47.572s] upgrade[graceful] fresh-connection calls:   [B-ready..drain] 153 calls: on A 71, on B 82, errors 0
[t= 47.572s] upgrade[graceful] fresh-connection calls:   [drain..A-exit] 1133 calls: on A 362, on B 771, errors 0
[t= 47.572s] upgrade[graceful] fresh-connection calls:   [A-exit..end] 225 calls: on A 0, on B 225, errors 0
[t= 47.572s] upgrade[graceful] fresh-connection calls: calls answered by a at or after the mark: 362 (first Some(0.018), last Some(19.998)); failed calls after the mark: 0
[t= 47.572s] upgrade[graceful] one-channel calls: 835 calls in all, 1 errors
[t= 47.572s] upgrade[graceful] one-channel calls:   [start..B-ready] 58 calls: on A 58, on B 0, errors 0
[t= 47.572s] upgrade[graceful] one-channel calls:   [B-ready..drain] 78 calls: on A 78, on B 0, errors 0
[t= 47.572s] upgrade[graceful] one-channel calls:   [drain..A-exit] 582 calls: on A 67, on B 514, errors 1
[t= 47.572s] upgrade[graceful] one-channel calls:   [A-exit..end] 117 calls: on A 0, on B 117, errors 0
[t= 47.572s] upgrade[graceful] one-channel calls:   error at +1.55s: Cancelled: operation was canceled
[t= 47.572s] upgrade[graceful] one-channel calls: calls answered by a at or after the mark: 67 (first Some(0.009), last Some(3.518)); failed calls after the mark: 1
[t= 47.572s] upgrade[graceful]: daemon session: connected drain-7.07s on A, ended drain+30.08s
[t= 47.572s] upgrade[graceful]: daemon session: connected drain+31.08s on B, ended still open at the end
exit 0
```

## cross-1.38.4-1.39.3: upgrade-early-stop

```
[t=  0.000s] scenario: == upgrade-early-stop (old envoy-1.38.4, new envoy-1.39.3) ==
[t=  3.259s] upgrade[early-stop]: B ready=true 0.11s after spawn; both listen on port 24102 (reuse_port)
[t=  7.266s] upgrade[early-stop]: POST /drain_listeners?graceful on A -> Some((200, "OK\n"))
[t= 11.277s] upgrade[early-stop]: A has no HTTP connection for 1s at +4.0s: SIGTERM
[t= 11.530s] upgrade[early-stop]: A exit: Some(("+4.02s", "exit status: 0")) (drain began at t=7.27s; SIGTERM sent at Some(4.011608856))
[t= 21.534s] upgrade[early-stop]: A http connections accepted: before drain Some(201), at exit None; B: before Some(71), now Some(152)
[t= 21.534s] upgrade[early-stop]: stub a: watches 0, cancelled by the peer 0, open now 0
[t= 21.534s] upgrade[early-stop]: stub b: watches 0
[t= 21.534s] upgrade[early-stop]: marks (t): B-ready 3.26, drain 7.27, A-exit 11.29
[t= 21.535s] upgrade[early-stop] fresh-connection calls: 651 calls in all, 0 errors
[t= 21.535s] upgrade[early-stop] fresh-connection calls:   [start..B-ready] 119 calls: on A 118, on B 1, errors 0
[t= 21.535s] upgrade[early-stop] fresh-connection calls:   [B-ready..drain] 152 calls: on A 82, on B 70, errors 0
[t= 21.535s] upgrade[early-stop] fresh-connection calls:   [drain..A-exit] 150 calls: on A 79, on B 71, errors 0
[t= 21.535s] upgrade[early-stop] fresh-connection calls:   [A-exit..end] 230 calls: on A 0, on B 230, errors 0
[t= 21.535s] upgrade[early-stop] fresh-connection calls: calls answered by a at or after the mark: 79 (first Some(0.015), last Some(3.991)); failed calls after the mark: 0
[t= 21.535s] upgrade[early-stop] one-channel calls: 336 calls in all, 0 errors
[t= 21.535s] upgrade[early-stop] one-channel calls:   [start..B-ready] 61 calls: on A 61, on B 0, errors 0
[t= 21.535s] upgrade[early-stop] one-channel calls:   [B-ready..drain] 78 calls: on A 78, on B 0, errors 0
[t= 21.535s] upgrade[early-stop] one-channel calls:   [drain..A-exit] 77 calls: on A 26, on B 51, errors 0
[t= 21.535s] upgrade[early-stop] one-channel calls:   [A-exit..end] 120 calls: on A 0, on B 120, errors 0
[t= 21.535s] upgrade[early-stop] one-channel calls: calls answered by a at or after the mark: 26 (first Some(0.014), last Some(1.325)); failed calls after the mark: 0
[t= 21.536s] upgrade[early-stop]: daemon session: connected drain-7.11s on A, ended drain+4.01s
[t= 21.536s] upgrade[early-stop]: daemon session: connected drain+5.07s on B, ended still open at the end
exit 0
```

## cross-1.38.4-1.39.3: upgrade-graceful-skip-exit

```
[t=  0.000s] scenario: == upgrade-graceful-skip-exit (old envoy-1.38.4, new envoy-1.39.3) ==
[t=  3.665s] upgrade[graceful-skip-exit]: B ready=true 0.11s after spawn; both listen on port 24202 (reuse_port)
[t=  7.670s] upgrade[graceful-skip-exit]: POST /drain_listeners?graceful&skip_exit on A -> Some((200, "OK\n"))
[t= 37.296s] upgrade[graceful-skip-exit]: A has no HTTP connection for 1s at +29.5s: SIGTERM
[t= 37.548s] upgrade[graceful-skip-exit]: A exit: Some(("+29.64s", "exit status: 0")) (drain began at t=7.67s; SIGTERM sent at Some(29.525040955))
[t= 47.553s] upgrade[graceful-skip-exit]: A http connections accepted: before drain Some(193), at exit None; B: before Some(68), now Some(641)
[t= 47.554s] upgrade[graceful-skip-exit]: stream short: opened t=0.55s, ended drain+6.00s (13.12s after open): clean end (OK trailers)
[t= 47.554s] upgrade[graceful-skip-exit]: stream long: opened t=0.55s, ended drain+12.00s (19.12s after open): clean end (OK trailers)
[t= 47.554s] upgrade[graceful-skip-exit]: stream over: opened t=0.55s, ended drain+28.01s (35.12s after open): clean end (OK trailers)
[t= 47.554s] upgrade[graceful-skip-exit]: stub a: watches 3, cancelled by the peer 0, open now 0
[t= 47.554s] upgrade[graceful-skip-exit]: stub b: watches 0
[t= 47.554s] upgrade[graceful-skip-exit]: marks (t): B-ready 3.67, drain 7.67, A-exit 37.31
[t= 47.554s] upgrade[graceful-skip-exit] fresh-connection calls: 1608 calls in all, 0 errors
[t= 47.554s] upgrade[graceful-skip-exit] fresh-connection calls:   [start..B-ready] 117 calls: on A 116, on B 1, errors 0
[t= 47.554s] upgrade[graceful-skip-exit] fresh-connection calls:   [B-ready..drain] 144 calls: on A 76, on B 68, errors 0
[t= 47.554s] upgrade[graceful-skip-exit] fresh-connection calls:   [drain..A-exit] 1118 calls: on A 556, on B 562, errors 0
[t= 47.554s] upgrade[graceful-skip-exit] fresh-connection calls:   [A-exit..end] 229 calls: on A 0, on B 229, errors 0
[t= 47.554s] upgrade[graceful-skip-exit] fresh-connection calls: calls answered by a at or after the mark: 556 (first Some(0.025), last Some(29.612)); failed calls after the mark: 0
[t= 47.554s] upgrade[graceful-skip-exit] one-channel calls: 828 calls in all, 1 errors
[t= 47.554s] upgrade[graceful-skip-exit] one-channel calls:   [start..B-ready] 61 calls: on A 61, on B 0, errors 0
[t= 47.554s] upgrade[graceful-skip-exit] one-channel calls:   [B-ready..drain] 74 calls: on A 74, on B 0, errors 0
[t= 47.554s] upgrade[graceful-skip-exit] one-channel calls:   [drain..A-exit] 574 calls: on A 43, on B 530, errors 1
[t= 47.554s] upgrade[graceful-skip-exit] one-channel calls:   [A-exit..end] 119 calls: on A 0, on B 119, errors 0
[t= 47.554s] upgrade[graceful-skip-exit] one-channel calls:   error at +1.16s: Cancelled: operation was canceled
[t= 47.554s] upgrade[graceful-skip-exit] one-channel calls: calls answered by a at or after the mark: 43 (first Some(0.035), last Some(2.239)); failed calls after the mark: 1
[t= 47.555s] upgrade[graceful-skip-exit]: daemon session: connected drain-7.11s on A, ended drain+29.63s
[t= 47.555s] upgrade[graceful-skip-exit]: daemon session: connected drain+30.63s on B, ended still open at the end
exit 0
```

## cross-1.38.4-1.39.3: upgrade-immediate

```
[t=  0.000s] scenario: == upgrade-immediate (old envoy-1.38.4, new envoy-1.39.3) ==
[t=  3.162s] upgrade[immediate]: B ready=true 0.11s after spawn; both listen on port 22452 (reuse_port)
[t=  7.165s] upgrade[immediate]: POST /drain_listeners on A -> Some((200, "OK\n"))
[t= 67.187s] upgrade[immediate]: A exit: None (drain began at t=7.16s; SIGTERM sent at None)
[t= 77.211s] upgrade[immediate]: A http connections accepted: before drain Some(194), at exit Some(194); B: before Some(78), now Some(2363)
[t= 77.211s] upgrade[immediate]: stream short: opened t=0.05s, ended drain+6.00s (13.11s after open): clean end (OK trailers)
[t= 77.211s] upgrade[immediate]: stream long: opened t=0.05s, ended drain+12.00s (19.11s after open): clean end (OK trailers)
[t= 77.211s] upgrade[immediate]: stream over: opened t=0.06s, ended drain+28.00s (35.11s after open): clean end (OK trailers)
[t= 77.211s] upgrade[immediate]: stub a: watches 3, cancelled by the peer 0, open now 0
[t= 77.211s] upgrade[immediate]: stub b: watches 0
[t= 77.211s] upgrade[immediate]: marks (t): B-ready 3.16, drain 7.16, A-exit (A never exited)
[t= 77.211s] upgrade[immediate] fresh-connection calls: 2784 calls in all, 0 errors
[t= 77.211s] upgrade[immediate] fresh-connection calls:   [start..B-ready] 119 calls: on A 118, on B 1, errors 0
[t= 77.211s] upgrade[immediate] fresh-connection calls:   [B-ready..drain] 152 calls: on A 75, on B 77, errors 0
[t= 77.211s] upgrade[immediate] fresh-connection calls:   [drain..A-exit] 2513 calls: on A 0, on B 2513, errors 0
[t= 77.211s] upgrade[immediate] fresh-connection calls:   [A-exit..end] 0 calls: on A 0, on B 0, errors 0
[t= 77.212s] upgrade[immediate] fresh-connection calls: calls answered by a at or after the mark: 0 (first None, last None); failed calls after the mark: 0
[t= 77.212s] upgrade[immediate] one-channel calls: 1427 calls in all, 0 errors
[t= 77.212s] upgrade[immediate] one-channel calls:   [start..B-ready] 61 calls: on A 61, on B 0, errors 0
[t= 77.212s] upgrade[immediate] one-channel calls:   [B-ready..drain] 78 calls: on A 78, on B 0, errors 0
[t= 77.212s] upgrade[immediate] one-channel calls:   [drain..A-exit] 1288 calls: on A 1288, on B 0, errors 0
[t= 77.212s] upgrade[immediate] one-channel calls:   [A-exit..end] 0 calls: on A 0, on B 0, errors 0
[t= 77.212s] upgrade[immediate] one-channel calls: calls answered by a at or after the mark: 1288 (first Some(0.011), last Some(66.014)); failed calls after the mark: 0
[t= 77.212s] upgrade[immediate]: daemon session: connected drain-7.11s on A, ended still open at the end
exit 0
```

## cross-1.38.4-1.39.3: upgrade-inbound

```
[t=  0.000s] scenario: == upgrade-inbound (old envoy-1.38.4, new envoy-1.39.3) ==
[t=  3.364s] upgrade[inbound]: B ready=true 0.30s after spawn; both listen on port 22552 (reuse_port)
[t=  7.366s] upgrade[inbound]: POST /drain_listeners?graceful&inboundonly&skip_exit on A -> Some((200, "OK\n"))
[t= 36.434s] upgrade[inbound]: A has no HTTP connection for 1s at +29.1s: SIGTERM
[t= 36.685s] upgrade[inbound]: A exit: Some(("+29.07s", "exit status: 0")) (drain began at t=7.37s; SIGTERM sent at Some(29.066596538999995))
[t= 46.692s] upgrade[inbound]: A http connections accepted: before drain Some(194), at exit None; B: before Some(85), now Some(657)
[t= 46.692s] upgrade[inbound]: stream short: opened t=0.06s, ended drain+6.00s (13.30s after open): clean end (OK trailers)
[t= 46.692s] upgrade[inbound]: stream long: opened t=0.07s, ended drain+12.00s (19.31s after open): clean end (OK trailers)
[t= 46.692s] upgrade[inbound]: stream over: opened t=0.07s, ended drain+28.01s (35.31s after open): clean end (OK trailers)
[t= 46.692s] upgrade[inbound]: stub a: watches 3, cancelled by the peer 0, open now 0
[t= 46.692s] upgrade[inbound]: stub b: watches 0
[t= 46.692s] upgrade[inbound]: marks (t): B-ready 3.36, drain 7.37, A-exit 36.44
[t= 46.692s] upgrade[inbound] fresh-connection calls: 1625 calls in all, 0 errors
[t= 46.692s] upgrade[inbound] fresh-connection calls:   [start..B-ready] 126 calls: on A 119, on B 7, errors 0
[t= 46.692s] upgrade[inbound] fresh-connection calls:   [B-ready..drain] 152 calls: on A 74, on B 78, errors 0
[t= 46.692s] upgrade[inbound] fresh-connection calls:   [drain..A-exit] 1108 calls: on A 547, on B 561, errors 0
[t= 46.692s] upgrade[inbound] fresh-connection calls:   [A-exit..end] 239 calls: on A 0, on B 239, errors 0
[t= 46.692s] upgrade[inbound] fresh-connection calls: calls answered by a at or after the mark: 547 (first Some(0.036), last Some(29.052)); failed calls after the mark: 0
[t= 46.692s] upgrade[inbound] one-channel calls: 832 calls in all, 1 errors
[t= 46.692s] upgrade[inbound] one-channel calls:   [start..B-ready] 65 calls: on A 65, on B 0, errors 0
[t= 46.692s] upgrade[inbound] one-channel calls:   [B-ready..drain] 78 calls: on A 78, on B 0, errors 0
[t= 46.692s] upgrade[inbound] one-channel calls:   [drain..A-exit] 567 calls: on A 12, on B 554, errors 1
[t= 46.692s] upgrade[inbound] one-channel calls:   [A-exit..end] 122 calls: on A 0, on B 122, errors 0
[t= 46.692s] upgrade[inbound] one-channel calls:   error at +0.34s: Cancelled: operation was canceled
[t= 46.692s] upgrade[inbound] one-channel calls: calls answered by a at or after the mark: 12 (first Some(0.034), last Some(0.647)); failed calls after the mark: 1
[t= 46.693s] upgrade[inbound]: daemon session: connected drain-7.30s on A, ended drain+29.07s
[t= 46.693s] upgrade[inbound]: daemon session: connected drain+30.07s on B, ended still open at the end
exit 0
```

## cross-1.38.4-1.39.3: upgrade-inbound-undirected

```
[t=  0.000s] scenario: == upgrade-inbound-undirected (old envoy-1.38.4, new envoy-1.39.3) ==
[t=  3.344s] upgrade[inbound-undirected]: B ready=true 0.22s after spawn; both listen on port 22652 (reuse_port)
[t=  7.348s] upgrade[inbound-undirected]: POST /drain_listeners?graceful&inboundonly&skip_exit on A -> Some((200, "OK\n"))
[t= 67.383s] upgrade[inbound-undirected]: A exit: None (drain began at t=7.35s; SIGTERM sent at None)
[t= 77.392s] upgrade[inbound-undirected]: A http connections accepted: before drain Some(212), at exit Some(1308); B: before Some(64), now Some(1209)
[t= 77.392s] upgrade[inbound-undirected]: stream short: opened t=0.12s, ended drain+6.00s (13.23s after open): clean end (OK trailers)
[t= 77.392s] upgrade[inbound-undirected]: stream long: opened t=0.12s, ended drain+12.00s (19.23s after open): clean end (OK trailers)
[t= 77.392s] upgrade[inbound-undirected]: stream over: opened t=0.12s, ended drain+28.00s (35.23s after open): clean end (OK trailers)
[t= 77.392s] upgrade[inbound-undirected]: stub a: watches 3, cancelled by the peer 0, open now 0
[t= 77.392s] upgrade[inbound-undirected]: stub b: watches 0
[t= 77.392s] upgrade[inbound-undirected]: marks (t): B-ready 3.34, drain 7.35, A-exit (A never exited)
[t= 77.392s] upgrade[inbound-undirected] fresh-connection calls: 2743 calls in all, 0 errors
[t= 77.392s] upgrade[inbound-undirected] fresh-connection calls:   [start..B-ready] 122 calls: on A 120, on B 2, errors 0
[t= 77.392s] upgrade[inbound-undirected] fresh-connection calls:   [B-ready..drain] 153 calls: on A 91, on B 62, errors 0
[t= 77.392s] upgrade[inbound-undirected] fresh-connection calls:   [drain..A-exit] 2468 calls: on A 1200, on B 1268, errors 0
[t= 77.392s] upgrade[inbound-undirected] fresh-connection calls:   [A-exit..end] 0 calls: on A 0, on B 0, errors 0
[t= 77.393s] upgrade[inbound-undirected] fresh-connection calls: calls answered by a at or after the mark: 1200 (first Some(0.004), last Some(66.012)); failed calls after the mark: 0
[t= 77.393s] upgrade[inbound-undirected] one-channel calls: 1414 calls in all, 0 errors
[t= 77.393s] upgrade[inbound-undirected] one-channel calls:   [start..B-ready] 63 calls: on A 63, on B 0, errors 0
[t= 77.393s] upgrade[inbound-undirected] one-channel calls:   [B-ready..drain] 78 calls: on A 78, on B 0, errors 0
[t= 77.393s] upgrade[inbound-undirected] one-channel calls:   [drain..A-exit] 1273 calls: on A 1273, on B 0, errors 0
[t= 77.393s] upgrade[inbound-undirected] one-channel calls:   [A-exit..end] 0 calls: on A 0, on B 0, errors 0
[t= 77.393s] upgrade[inbound-undirected] one-channel calls: calls answered by a at or after the mark: 1273 (first Some(0.017), last Some(66.003)); failed calls after the mark: 0
[t= 77.397s] upgrade[inbound-undirected]: daemon session: connected drain-7.22s on A, ended still open at the end
exit 0
```

## cross-1.38.4-1.39.3: hrsame

```
[t=  0.000s] scenario: == hrsame (old envoy-1.38.4, new envoy-1.39.3) ==
[t=  3.536s] hrsame: restart 1 (epoch 1) spawned t=3.23s, LIVE as epoch 1: true at +0.30s
[t= 39.236s] hrsame: marks: restart1 spawn t=3.232, LIVE t=3.536; restart2 spawn None, LIVE None
[t= 39.236s] hrsame: stream short (opened on epoch 0): ended restart1 spawn+8.39s, restart1 LIVE+8.08s: clean end (OK trailers)
[t= 39.236s] hrsame: stream over (opened on epoch 0): ended restart1 spawn+25.20s, restart1 LIVE+24.90s: error Unknown: h2 protocol error: error reading a body from connection
[t= 39.236s] hrsame: worker tcp_proxy session (opened on epoch 0): ended restart1 spawn+25.20s: Ok(0)
[t= 39.236s] hrsame: exits: e0 Some((28.44588083, "exit status: 0")), e1 None, e2 None
[t= 39.236s] hrsame fresh-connection calls: 1345 calls in all, 0 errors
[t= 39.236s] hrsame fresh-connection calls:   [start..restart1] 106 calls: on A 106, on B 0, errors 0
[t= 39.236s] hrsame fresh-connection calls:   [restart1..end] 1239 calls: on A 1239, on B 0, errors 0
[t= 39.236s] hrsame one-channel calls: 693 calls in all, 1 errors
[t= 39.236s] hrsame one-channel calls:   [start..restart1] 56 calls: on A 56, on B 0, errors 0
[t= 39.236s] hrsame one-channel calls:   [restart1..end] 637 calls: on A 636, on B 0, errors 1
[t= 39.236s] hrsame one-channel calls:   error at +0.89s: Cancelled: operation was canceled
exit 0
```

## cross-1.38.4-1.39.3: hrsecond

```
[t=  0.000s] scenario: == hrsecond (old envoy-1.38.4, new envoy-1.39.3) ==
[t=  3.135s] hrsecond: restart 1 (epoch 1) spawned t=3.06s, LIVE as epoch 1: true at +0.08s
[t=  7.216s] hrsecond: restart 2 (epoch 2) spawned +4.08s after restart 1 (inside the 25s window), LIVE as epoch 2: true at +0.075s
[t= 43.223s] hrsecond: marks: restart1 spawn t=3.059, LIVE t=3.135; restart2 spawn Some(7.141385388), LIVE Some(7.21631557)
[t= 43.223s] hrsecond: stream short (opened on epoch 0): ended restart1 spawn+4.14s, restart1 LIVE+4.06s, restart2 spawn+0.057s, restart2 LIVE-0.018s: error Unknown: h2 protocol error: error reading a body from connection
[t= 43.223s] hrsecond: stream over (opened on epoch 0): ended restart1 spawn+4.14s, restart1 LIVE+4.06s, restart2 spawn+0.057s, restart2 LIVE-0.018s: error Unknown: h2 protocol error: error reading a body from connection
[t= 43.223s] hrsecond: stream epoch1-stream (opened on epoch 1): ended restart1 spawn+29.15s, restart1 LIVE+29.07s, restart2 spawn+25.068s, restart2 LIVE+24.993s: error Unknown: h2 protocol error: error reading a body from connection
[t= 43.223s] hrsecond: worker tcp_proxy session (opened on epoch 0): ended restart1 spawn+4.14s: Ok(0)
[t= 43.223s] hrsecond: exits: e0 Some((7.203161662, "exit status: 0")), e1 Some((32.236018981, "exit status: 0")), e2 None
[t= 43.223s] hrsecond fresh-connection calls: 1506 calls in all, 0 errors
[t= 43.223s] hrsecond fresh-connection calls:   [start..restart1] 115 calls: on A 115, on B 0, errors 0
[t= 43.223s] hrsecond fresh-connection calls:   [restart1..restart2] 155 calls: on A 155, on B 0, errors 0
[t= 43.223s] hrsecond fresh-connection calls:   [restart2..end] 1236 calls: on A 1236, on B 0, errors 0
[t= 43.223s] hrsecond one-channel calls: 777 calls in all, 1 errors
[t= 43.223s] hrsecond one-channel calls:   [start..restart1] 59 calls: on A 59, on B 0, errors 0
[t= 43.223s] hrsecond one-channel calls:   [restart1..restart2] 79 calls: on A 78, on B 0, errors 1
[t= 43.223s] hrsecond one-channel calls:   [restart2..end] 639 calls: on A 639, on B 0, errors 0
[t= 43.223s] hrsecond one-channel calls:   error at +0.33s: Cancelled: operation was canceled
exit 0
```

## cross-1.38.4-1.39.3: fallback

```
[t=  0.000s] scenario: == fallback (old envoy-1.38.4, new envoy-1.39.3) ==
[t=  3.191s] fallback: SIGTERM to A at t=3.08s; A exit Some(("+0.017s", "exit status: 0")); B spawned +0.001s, /ready=true at +0.107s
[t= 12.200s] fallback: A exit Some(("+0.017s", "exit status: 0"))
[t= 12.200s] fallback fresh-connection calls: 310 calls in all, 2 errors
[t= 12.200s] fallback fresh-connection calls:   [start..SIGTERM] 115 calls: on A 115, on B 0, errors 0
[t= 12.200s] fallback fresh-connection calls:   [SIGTERM..B-ready] 4 calls: on A 0, on B 2, errors 2
[t= 12.200s] fallback fresh-connection calls:   [B-ready..end] 191 calls: on A 0, on B 191, errors 0
[t= 12.200s] fallback fresh-connection calls:   error at +0.00s: connect: transport error
[t= 12.200s] fallback fresh-connection calls:   error at +0.03s: connect: transport error
[t= 12.200s] fallback: new connections: first failed call at +0.004s, last failed call at +0.030s, first call answered by B at +0.056s
[t= 12.200s] fallback one-channel calls: 159 calls in all, 1 errors
[t= 12.200s] fallback one-channel calls:   [start..SIGTERM] 59 calls: on A 59, on B 0, errors 0
[t= 12.200s] fallback one-channel calls:   [SIGTERM..B-ready] 2 calls: on A 0, on B 1, errors 1
[t= 12.200s] fallback one-channel calls:   [B-ready..end] 98 calls: on A 0, on B 98, errors 0
[t= 12.200s] fallback one-channel calls:   error at +0.02s: Unavailable: tcp connect error
exit 0
```

## cross-1.38.4-1.39.3: overlap-false

```
[t=  0.000s] scenario: == overlap-false (old envoy-1.38.4, new envoy-1.39.3) ==
[t=  9.092s] overlap[Some(false)]: B /ready=false, exit Some(("+0.06s", "exit status: 1"))
[t=  9.092s] overlap[Some(false)]: B log: cannot bind '127.0.0.1:22352': Address already in use
exit 0
```

## cross-1.38.4-1.39.3: overlap-default

```
[t=  0.000s] scenario: == overlap-default (old envoy-1.38.4, new envoy-1.39.3) ==
[t=  1.107s] overlap[None]: B /ready=true, exit None
[t= 12.110s] overlap[None] fresh-connection calls: 708 calls in all, 0 errors
[t= 12.110s] overlap[None] fresh-connection calls:   [start..end] 708 calls: on A 353, on B 355, errors 0
exit 0
```

## cross-1.38.4-1.39.3: hrmismatch-version (simulated)

```
[t=  0.000s] scenario: == hrmismatch-version (old envoy-1.38.4, new envoy-1.39.3) ==
[t=  0.120s] hrmismatch[version]: shared memory /dev/shm/envoy_shared_memory_8000: size_ 104, version_ 11 (the --hot-restart-version of this binary is 11.104)
[t=  0.120s] hrmismatch[version]: poked version_ to 12: epoch 0's shared memory now says what an Envoy of another hot restart version would have written
[t=  0.207s] hrmismatch[version]: epoch 1 exit: Some(("+0.087s after spawn", "signal: 6 (SIGABRT)"))
[t=  0.207s] hrmismatch[version]: epoch 1 log: [2026-10-17 22:30:05.443][639521][critical][assert] [source/server/hot_restart_impl.cc:70] assert failure: shmem->version_ == HOT_RESTART_VERSION. Details: Hot restart version mismatch! You must have hot restarted into a
[t=  1.233s] hrmismatch[version]: epoch 0 after the failed attempt: admin Some("state=\"LIVE\" epoch=0"); fresh call Ok("a"); exit None; held stream finished: false
[t=  1.233s] hrmismatch[version]: held stream (opened on epoch 0): clean end (OK trailers)
exit 0
```

## cross-1.38.4-1.39.3: hrmismatch-size (simulated)

```
[t=  0.000s] scenario: == hrmismatch-size (old envoy-1.38.4, new envoy-1.39.3) ==
[t=  0.151s] hrmismatch[size]: shared memory /dev/shm/envoy_shared_memory_7000: size_ 104, version_ 11 (the --hot-restart-version of this binary is 11.104)
[t=  0.151s] hrmismatch[size]: poked size_ to 112: epoch 0's shared memory now says what an Envoy of another hot restart version would have written
[t=  0.247s] hrmismatch[size]: epoch 1 exit: Some(("+0.077s after spawn", "signal: 6 (SIGABRT)"))
[t=  0.247s] hrmismatch[size]: epoch 1 log: [2026-10-17 22:30:06.788][639821][critical][assert] [source/server/hot_restart_impl.cc:67] assert failure: shmem->size_ == sizeof(SharedMemory). Details: Hot restart SharedMemory size mismatch! You must have hot restarte
[t=  1.311s] hrmismatch[size]: epoch 0 after the failed attempt: admin Some("state=\"LIVE\" epoch=0"); fresh call Ok("a"); exit None; held stream finished: false
[t=  1.311s] hrmismatch[size]: held stream (opened on epoch 0): clean end (OK trailers)
exit 0
```
