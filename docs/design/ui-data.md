# kbf UI data: what the UI and its read API present

Status: design, nothing built beyond what is marked [F]. Data before visuals: this document says what the UI and its machine-readable read API must present, where each item comes from, how long it is kept and what it costs. It says nothing about layout. Labels as in `architecture-v2.md`: **[F]** fact checked in the code at this branch's merge-base with `feature/arch-v2`, `364da4ae` (this branch's commits above it touch only this document, `invocations-api.md` 4.7 and one line of `architecture-v2.md`, so code cites are the same at this branch's head), **[D]** owner decision, **[I]** inference, **[V]** verify before building on it. Sizes are arithmetic from stated rates, not measurements.

## Summary

The UI is a set of read-only pages over one JSON read API; an agent calls the same API and sees everything a page shows. What it answers, and what is missing today:

| User question | What answers it | Today / needs |
|---|---|---|
| Did my build pass; which actions failed and why; full logs; the exact command (D1 to D4) | invocation summary, `/actions`, `/failures`, decoded command, log tails | summary and `/actions` built, in memory only, lost on restart; `/failures`, which carries the decoded command, needs U1; logs last only as long as storage keeps the blob, so they need failure pins (U3) |
| Where did the time go; which actions were slowest; why did it wait (D5, D11) | all nine REAPI timestamps, phase totals, `sort=`, `top_slowest`, `wait_reason` | four times per attempt are kept; the rest needs U1 |
| Why did it OOM, what limit, what next time (D6) | memory fields on every memory-killed attempt, memory floor per digest | only a requeue keeps booked/raised; the final kill keeps none: U1, U2 |
| Is this test flaky, since when; first failing commit; is this action slower (D8, D9) | outcome rows and daily rollups per shape and branch | nothing today: history store (U3), commits from a client report (U4) |
| What is on the critical path (D10) | client report; else slowest actions and phase totals | client report (U4) |
| Nodes, queue, leases (O1 to O3) | `/v1/nodes`, `/v1/nodes/{id}`, `/v1/queue`, `/v1/leases` | `/v1/nodes` built (open, operator listener only); the rest needs U2 |
| Which node fails what passes elsewhere (O5) | per-node failure counts, runs by node and time | history store (U3) |
| Storage health (O4) | storage map, repair and loss counts | arrives with the v2 storage phases P3 to P6 |
| Farm health now and over weeks (W1, W2) | `/v1/health`, built-in series | `/metrics` exists but resets at start: U5 |
| Remove a leaked secret (O6) | purge as a tombstone | U5 on one store, U8 across replicas |
| What idle capacity does (W3) | idle-host test reruns | U7, after preemption is built |

The biggest things to add:

- **A durable history store** beside the state store, written after commit and never on the acknowledgement path, following the state store's backend (SQLite or a separate Postgres database). It holds runs, cache hits, shapes, test outcomes and rollups, and replaces the Raft-based stage 2 of `invocations.md`.
- **Per-action fields kept in the row**: shape and `cfg`, the nine REAPI timestamps, the full cgroup block, memory-kill fields, wait reason, farm reruns, a timed-out run's logs. Today most exist only in log lines, blobs or scheduler memory, not in the row.
- **A client report** (one upload route, owner only, off without per-caller authentication) for what REAPI cannot carry: commit and branch, target names, critical path, test cases.
- **Pins for failure logs**: a failed run's logs and result kept 30 days within a byte budget shared per instance, and client reports in a budget of their own. Nothing in storage can pin a blob today.

**Size.** At the assumed rate (about 4 operations a second, 9 cache hits per execution; assumptions, not measurements), about 36 GB of history (about 30 GB at a 67% hit rate), plus the two pin budgets: 20 GiB for failures, 10 GiB for reports (5.3).

**Phases.** U1 (per-action data) and U2 (farm-state reads) need no state store and ship on today's single frontend. U3 (history store and pins), U4 (client report), U5 (series, health, purge on one store, raw Command gate) and U6 (UI pages) need the SQLite state store (P1) and still run on the single frontend; U7 (idle-host tests) also waits for preemption; U8 (purge across replicas) waits for P5 and P6 (section 9).

**Decisions.** Nothing here is decided. Section 10 lists 19 questions for the owner, each with a recommendation.

It builds on `invocations.md` and `invocations-api.md` (the per-build record and its read API) and on `architecture-v2.md` (frontend serves REAPI, scheduler and UI; SQLite/Postgres state store; sharded storage). It does not redo them; it lists what the UI adds.

## 0. Branch caveat and stale text

- **[F]** kbf#765 is on this branch: `feature/arch-v2` merged `main` in `364da4ae`, and this branch merged `feature/arch-v2`. The `action usage:` line now carries the stable command-shape `key`, `cfg`, `outcome`, `lease` and `node`, is written for every report whose result carries a `ResourceUsage` (failed runs the daemon measured included), and `lease granted` names the key (`crates/kbf-server/src/action_usage.rs:1-45`; the key on `lease granted` at `crates/kbf-server/src/farm.rs:1028`). The key and its shape exist only in log lines: no record row and no API answer carries them (3).
- **[F]** `invocations.md` section 9 says only PRs 1, 2, 3 and 5 have landed. The read API (PR 4) is in the code: `crates/kbf-server/src/http_invocations.rs:101-102`, routes at `crates/kbf-server/src/http_invocations/pages.rs:171-181`.
- **[F]** `invocations.md` section 1 and the `scheduler.md` Accounting text say no driver fills `ResourceUsage`. The container driver does (`crates/kbf-driver-container/src/cgroup_usage.rs:140-175`); the native driver does not (no `ResourceUsage` in `crates/kbf-driver-native/src`).
- **[F]** `failure-classes.md` section 9 says no metrics endpoint exists; `GET /metrics` exists (`crates/kbf-server/src/metrics.rs:24-40`).
- **[F]** `failure-classes.md` 6.3 (`:719-721`) says flakes are recorded in "a bounded table on the server today"; none exists (`git grep -il flake crates` finds two test files only: `crates/kbf-daemon/tests/local.rs`, `crates/kbf-driver-container/tests/fake_podman.rs`).

## 1. Decisions this document proposes

None is decided; section 10 asks the owner.

- **One surface.** The UI reads only the JSON read API of section 6. Every page names the API calls it made. An agent therefore gets everything the UI shows, machine-readable, with the same authority.
- **One history store.** Per-run records, the shape catalog and test outcomes live in one append-mostly history store beside the state store (section 5), written after the state-store commit, never on the acknowledgement path. Its backend follows the state store's (SQLite with SQLite, a Postgres database with Postgres), so a frontend that is stateless on Postgres stays stateless (5.2). It also becomes the durable home of invocation records, replacing the Raft-based stage 2 of `invocations.md` 3.5 (that log is removed by v2).
- **Every history row carries its instance name**, and every read is filtered by the caller's authority on that instance, as the built invocation routes are (section 8).
- **Shape identity covers every action** (kbf#765), refined by `cfg` and by a target name when one is known. Tests are the shapes marked as tests; the farm never needs the client to say "this is a test".
- **Built fields keep their names and formats.** New data adds keys; nothing built is renamed (6.1).
- **The client may add what REAPI cannot carry** (commit, target names, critical path, test cases) through one upload route, written only by the record's owner, and only on a farm whose policy authenticates each caller (4.6, 8). Nothing in the farm requires it.

## 2. Users and the questions they ask

### 2.1 Questions

| # | User | Question | Answered by (section) |
|---|---|---|---|
| D1 | developer, agent | Did my build pass; which actions failed and why? | invocation summary, failures view (4.1, 4.2) |
| D2 | developer, agent | Was the failure my code or the farm? | failure class per attempt, infra reason text (4.2) |
| D3 | developer, agent | Full stderr/stdout of every failed action, not only the first | failures view with tails and log links (4.2) |
| D4 | developer, agent | The exact command, to reproduce it | Command decoded, env names only (4.2, 8) |
| D5 | developer, agent | Where did the time go: queue, input fetch, run, upload? Which actions were slowest? | nine REAPI timestamps plus `granted` per attempt, sort and `top_slowest` (4.1, 4.2) |
| D6 | developer, agent | Why did it OOM, what limit did it hit, and how much will the farm book next time? | `memory_kill`, peak, `booked` bytes and `cap` for every memory-killed run, `raised` bytes for a requeue, memory floor per (instance, action digest); a changed digest starts again at the ladder's first rung unless the action names a size (4.2) |
| D7 | developer, agent | Cache hit or not; same digest ran before? | `how` per action, runs of a digest across builds (4.1, 6) |
| D8 | developer, agent | Is this test flaky, since when; did it fail before my change; since which commit? | test history by shape, reachable from a target label (4.3) |
| D9 | developer, agent | Is this action (test or not) getting slower? | duration trend by shape (4.3) |
| D10 | developer, agent | What is on the critical path? | client report; without one, slowest and phase totals (4.1) |
| D11 | developer, agent | Why did my action wait in the queue? | `wait_reason` and partition per attempt (4.2) |
| O1 | operator | Which nodes are up, serving, cordoned, under memory pressure; how were they then? | node view, node samples (4.4, 4.5) |
| O2 | operator | What is queued, for which platform, and why is it not placed? | queue view (4.4) |
| O3 | operator | Which leases are open, where, how old? | lease view (4.4) |
| O4 | operator | Is storage healthy: map, under-replicated, lost blobs, flush lag? | storage view (4.4) |
| O5 | operator | Which node fails actions that pass elsewhere; what else ran on a node when an action was killed there? | per-node failure counts and "failed here, passed elsewhere" on `GET /v1/nodes/{id}`, `GET /v1/runs?node=&from=&to=`, a per-node failure series (4.4, 4.5) |
| O6 | operator | A secret leaked into a log or argv: remove it | purge (8) |
| W1 | owner | Is the farm healthy right now? | health summary (4.4) |
| W2 | owner | Load, cache hit rate, queue wait and failure rate over weeks | series (4.5) |
| W3 | owner | How much idle capacity is there, and what does it do? | idle capacity series, idle-test results (7) |

### 2.2 What agents worked out by hand

**[I]** From agents debugging builds on this farm (generic shapes only): they could not tell a farm fault from an action failure without the server log (D2); they read the daemon journal on a node to find why the farm could not run an action, because the reason reaches only a server WARN line (`crates/kbf-server/src/farm.rs:610-618`); they found that the client's reported duration includes farm queue time and took phase times from the daemon's `lease phases:` line instead (D5); a container's OOM flag was false for an exit-137 kill, so they counted kernel memcg kills (D6); memory-ladder rungs looked like failures; they reran single tests by hand to judge flakiness (D8); they could not reach `/metrics` and `GET /v1/nodes` (O1). **[F]** Those two routes need no token: they are open reads (`crates/kbf-server/src/metrics.rs:6`, handler `crates/kbf-server/src/api.rs:66-72`, `docs/api.md:221`), served only on the optional operator listener `--api-listen` (`crates/kbf-server/src/config.rs:167-170`). The obstacle was reaching that listener, not authority, and it is the obstacle for the whole read API: every HTTP route is on that one plain-HTTP listener (8, "Today"). Every one of these becomes one API call below.

## 3. Entities, identity and links

| Entity | Id | Source | Links to |
|---|---|---|---|
| Invocation | client `tool_invocation_id` **[F]**; id-less calls are not recorded today (`crates/kbf-front/src/recorder.rs:17-19`, `minted: false` at `:450`); minting `kbf-<uuid>` is planned (`invocations.md` 2.3), not built | RequestMetadata (`crates/kbf-front/src/request_metadata.rs:103-119`) | actions; client report; commit |
| Action | action digest (sha256-size) | REAPI | runs, command, shape, result |
| Operation | REAPI operation name | scheduler | attempts; gone 60 s after it ends (`crates/kbf-sched/src/scheduler.rs:53`) |
| Attempt (lease) | `LeaseId`, shown `term.seq` (`crates/kbf-types/src/lease.rs:30`); v2 `epoch.seq` | scheduler | node, run record |
| Run record | `run_id`: 24 hex, time-ordered: 6 bytes of the commit time in milliseconds, then the first 6 bytes of SHA-256 over `(invocation, action digest, lease)`; the time prefix names the day partition and keeps inserts at the right edge of the key (5.2) | new (5.2) | all per-attempt data; `GET /v1/runs/{run_id}` |
| Shape | 16-hex key, kbf#765 (`crates/kbf-server/src/action_usage/shape.rs:61-146`) | computed from the Command at Execute | `cfg` variants, targets seen, run and duration history |
| Test target | `(instance, shape, cfg)`; plus target label when known | shape + client | outcomes over time |
| Node | node id from `Hello` | daemon | leases, samples |
| Blob | digest | storage | `available` flag, retention |
| Commit, branch | client-supplied strings; commits have no order across branches | client report or header (4.6) | invocations; history is per branch (4.3) |

- **[F]** buck2 sends only `tool_invocation_id` (its Build ID) and `tool_details` (with a fixed version); `action_id`, `action_mnemonic`, `target_id`, `configuration_id`, `correlated_invocations_id` are empty (`crates/kbf-front/tests/request_metadata.rs:33-63`, golden `crates/kbf-front/tests/data/buck2-execute.requestmetadata`). Bazel fills them all, read from its source only, not captured on the wire **[V]**.
- **[F]** Shape key = first 8 bytes of SHA-256 over a version tag, the normalized program words (up to 3) and the sorted, deduplicated normalized output patterns; RequestMetadata never enters it, and `cfg` (the config hash taken out of output paths) is not part of it (`crates/kbf-server/src/action_usage/shape.rs:61-146`).
- **[F]** Known limit: a buck2 remote test's outputs sit under `buck-out/v2/test/execution/<cell>/<hash>/<hash>/`, the first hash covering target and configuration, the second the test-case set. Such an action keys once per target, configuration and test-case set, with `cfg=-` (`crates/kbf-server/src/action_usage/shape.rs:36-42`). **[I]** Good for history (stable across builds); it cannot name the target, and a changed test-case set starts a new key, so one test's history can span several keys. The target lookup and key links of 4.3 join them.
- **[F]** The key-to-shape mapping exists today only in `action shape:` log lines, deduplicated in memory; the dedup set is cleared when it reaches 65,536 entries (`crates/kbf-server/src/action_usage.rs:59-61`, `:100-145`). That is a memory bound on the log, not a bound on distinct keys. It must become rows (5.2).
- **[I]** A cache hit carries no Command (buck2 uploads no Action for a hit), so its shape comes from a `digest -> shape` row written when that digest last executed (5.2).
- **[I]** Id-less runs: once history keys outcomes by instance and shape, an id-less run could still produce a test outcome with a null invocation. Recommend it does (section 10 Q12); today such a run leaves nothing.

## 4. Items: fields, source, retention, API

Each subsection: fields, source today **[F]** or what to add, retention and size, the route (section 6 lists all routes; "built" means it exists at HEAD).

**Rates used for sizes [I].** Daily volume: about 4 operations a second, the pilot rate assumed in the comment on `FINISHED_RETENTION` (`crates/kbf-sched/src/scheduler.rs:50-52`, cited by `invocations.md:246-247`), not a measurement: about 345,600 executions a day. Burst: 300 actions a second (`architecture-v2.md` 4.1 C7). Cache hits: executions x h/(1-h) at a stated hit rate h, sized at h = 0.9 (9 hits per execution) unless a line says otherwise **[V]**. Index overhead: every indexed row is sized at 1.5 x its raw bytes **[I]**.

### 4.1 Per build (invocation)

| Field | Source today | To add |
|---|---|---|
| tool, tool version, first/last seen, counts (execute, cache hit, joined, ran, failed, AC hit/miss, blobs asked/missing, bytes up/down) | **[F]** in-memory record header (`crates/kbf-invocation/proto/kbf/invocation/v1/invocation.proto:29-79`) | durable (5) |
| actions with `how` (cache_hit, cache_hit_execute, joined, ran), outcome | **[F]** `invocation.proto:108-132` | shape, cfg per row |
| queue and run histograms, reruns by reason, CPU and peak-memory sum/max | **[F]** computed per request (`crates/kbf-invocation/src/metrics.rs:100-137`) | phase totals (queue, fetch, run, upload); a list link per rerun reason |
| slowest actions | none: `/actions` is in record order, no sort **[F]** (`pages.rs:224-245` parses only paging, `outcome`, `how`) | `sort=` on `/actions`; `top_slowest` (20, by wall and by execution) in the summary |
| which node ran what | **[F]** `Attempt.worker` (`invocation.proto:135-149`); server overwrites `ExecutedActionMetadata.worker` with the node id (`crates/kbf-server/src/stamp.rs:24-36`) | per-node counts in the summary |
| cache hit rate | **[F]** counts above | none |
| commit, branch, user-visible target names | none: no REAPI field carries them | client report (4.6) |
| critical path | none: REAPI has no dependency edges **[F]** (the remote-execution proto); the farm can order by time only | client report: buck2 `log critical-path --format json`, Bazel profile/BES later **[V]**; without a report, `top_slowest` and phase totals are the one-call answer |

- **[F]** Records are in memory only, bounded by `--invocation-memory` (default 256 MiB, `crates/kbf-front/src/recorder.rs:42`), lost on restart; calls with no id are not recorded (`crates/kbf-server/src/farm/record.rs:93-95`).
- Size: 444 B average row, measured **[F]** (`crates/kbf-invocation/tests/bytes_per_action.rs`, `invocations.md:222-253`); at the assumed rate about 150 MB/day, 30 days about 4.5 GB **[I]**. The durable invocation header is about 300 B raw (counters, owner, instance names, commit and branch refs, report digests), 450 B indexed; at an assumed 1,000 invocations a day **[V]** and 400 days, about 180 MB (5.3).
- Routes: `GET /v1/invocations/{id}` and `/metrics` (built); `GET /v1/invocations` list (new, needs 5); `GET /v1/invocations/{id}/critical-path` (new, 4.6).

### 4.2 Per action and per attempt

| Field | Where it is today | To add |
|---|---|---|
| exit code, stdout/stderr digests, outputs (max 1,000), result digest | **[F]** row listing (`crates/kbf-front/src/recorder.rs:494-522`) | timed-out run: keep the partial result's log digests (today dropped, `crates/kbf-server/src/farm/record.rs:96-99`, `crates/kbf-server/src/timeout.rs:1-37`) |
| exact command | **[F]** only the Command digest (`invocation.proto:125`); contents by fetching the blob | decoded view, env names only (8) |
| input root digest, input bytes/files | not recorded. **[F]** The server's missing-inputs walk cannot count them: `Walked` holds only `absent` and `dirs_read` (`crates/kbf-front/src/input_tree.rs:363-368`), and the walk skips subtrees an earlier request verified (`:370-372`) | copy `Action.input_root_digest` at Execute; files and bytes counted by the daemon, which materializes the whole tree, carried in `auxiliary_metadata` (to add, daemon side). **[I]** `read_bytes` on the daemon's `transfers:` line (`crates/kbf-transfer/src/stats.rs:15-30`) counts bytes fetched, not bytes served from its cache, so it is fetch cost, not input size; both are shown |
| queued, granted, started, ended per attempt | **[F]** `invocation.proto:135-149`, returned as RFC 3339 strings (`crates/kbf-server/src/http_invocations/views.rs:309-317`) | none |
| REAPI timestamps | **[F]** nine in `ExecutedActionMetadata` (`remote_execution.proto:1192-1235`): `queued` set by the server (first submission kept across requeues, `crates/kbf-server/src/stamp.rs:24-38`), eight by the daemon in all three runtimes (`crates/kbf-driver-native/src/runtime.rs:561-610`, `crates/kbf-daemon/src/local.rs:94-171`, `crates/kbf-driver-container/src/runtime.rs:528`); stored inside the ActionResult blob only | copy all nine into the run record (`queued_timestamp` differs from `Attempt.queued` after a requeue) |
| `lease phases:` (lease_ms, inputs_ms, run_ms, outputs_ms, output files/dirs, `oom=1`) | **[F]** daemon log line only (`crates/kbf-daemon/src/phases.rs:150-172`) | derive from the copied timestamps; carry `oom` and counts in the report |
| cgroup: cpu usage/user/system, periods, throttled, throttled time, memory peak, NUMA bytes per node | **[F]** in `ResourceUsage.cgroup` inside `auxiliary_metadata` (`crates/kbf-proto/proto/kbf/worker/v1/worker.proto:436-480`); the row keeps only 4 legacy fields (`crates/kbf-front/src/recorder.rs:524-539`) | keep all of it in the run record; derive parallelism = cpu_usage / wall and numa_spread |
| NUMA node the lease was pinned to | **[F]** container driver `inputs phases:` line only, with `--lease-placement=numa-node` (`crates/kbf-driver-container/src/input_phases.rs:1-80`) | carry in `ResourceUsage` |
| OOM marker | **[F]** `worker::Result.memory_kill` (`crates/kbf-daemon/src/lease.rs:290-296`), never in the ActionResult; surfaces as requeue reason `OUT_OF_MEMORY` (`invocation.proto:166-206`) | `memory_kill` in the run record: `none`, `own_limit`, `node_pressure` |
| booking killed at, next booking, cap | **[F]** in the record only for a requeue: `encode.rs` sets `booked`/`raised` for `RequeueReason::OutOfMemory` alone (`crates/kbf-invocation/src/encode.rs:134-153`, `invocation.proto:166-172`). The attempt that ends the operation (killed at the largest node) is an `AttemptFailure` with class and reason text only (`encode.rs:126-129`), so those fields are empty for the kill an agent cares about most. **[F]** The data exists: `Answer.memory_runs` carries each killed run's worker, booking and `MemoryKill::OwnLimit{cap}` (`crates/kbf-types/src/work.rs:266-286`, filled at `crates/kbf-sched/src/scheduler.rs:879`); `crates/kbf-server/src/memory.rs:56-84` puts only the last booking into the client's status text and `ErrorInfo`. **[F]** The daemon's measured `used` and `limit` (`RuntimeError::OutOfMemory`, `crates/kbf-driver-container/src/runtime.rs:608-611`) reach the server only as status text (`crates/kbf-daemon/src/lease.rs:290-293`) | `booked_memory_bytes` (and `cap_bytes` for `own_limit`) on every memory-killed attempt, terminal ones included, from `Answer.memory_runs`; `raised_memory_bytes` on a requeue; `memory_used_bytes` and `memory_limit_bytes` as structured `worker::Result` fields, not text |
| memory floor per (instance, action digest) | **[F]** kept per `ActionKey` = (instance, action digest) (`crates/kbf-types/src/work.rs:57-62`; `Scheduler::memory_floor(&ActionKey)` at `crates/kbf-sched/src/scheduler/memory.rs:290`), not per shape key: a code change gives a new digest, which books the ladder's first rung or the size the action names (`memory.rs:9-11`), not the old digest's floor. In the scheduler only; lost on restart, bounded in count with the one raised longest ago forgotten first, never lowered (`crates/kbf-sched/src/scheduler/memory.rs:38-40`, `:75`); v2: a state-store row (`architecture-v2.md` 4.2) | `memory_floor_bytes` with `raised_at` on the action answer, for that digest. On the shape answer, a derived `largest_digest_floor` **[I]** (to add): the largest floor among the shape's digests seen in the window, naming that digest; it says what a past digest needed, not what the next build books. No decay to show today |
| retries | **[F]** attempts with requeue reason or failure class plus reason (≤1 KiB) (`crates/kbf-invocation/src/record.rs:121-180`) | farm-rerun budget (`farm_runs`/`farm_budget`) per attempt (today log-only, `crates/kbf-server/src/farm.rs:910-950`); the ladder rung is already `booked`/`raised` above |
| infra reason text | **[F]** in the record for a failure that ends the operation (`crates/kbf-server/src/farm/record.rs:100-103`); a requeue keeps only its reason enum | the daemon's text for every attempt |
| why it waited | **[F]** live only: `Scheduler::waiting(op)` gives the unservable reason while queued (`scheduler.rs:329-333`); nothing kept after placement | `wait_reason` (last reason given while queued) and partition (platform, size class, QoS) per attempt |
| transfer stats (reads, bytes, find-missing, outputs present, writes, retries) | **[F]** daemon `transfers:` line only; the code calls API carriage "a later stage" (`crates/kbf-transfer/src/stats.rs:1-6`) | carry in `auxiliary_metadata` (`worker-cache.md` plans it) |
| input sub-phases (image, walk, links, chown, cgroup setup) | **[F]** container `inputs phases:` line only | optional; carry as a map |
| server time before the offer (missing-inputs walk) | **[F]** `kbf_execute_walk_seconds`, unlabeled, aggregate only (`crates/kbf-front/src/metrics.rs:571-575`) | per-attempt `presubmit_us` |
| test log files (`test.log`, `test.xml`) | **[F]** when the client runs a test remotely they are ordinary outputs: in the outputs listing (`recorder.rs:494-522`, up to 1,000) and readable by `GET /v1/blobs/{digest}` today; a buck2 remote test's stdout/stderr is its log | link them by output path in the action view (`test_logs`) |
| test-case results | **[F]** no parser anywhere; `failure-classes.md:63-67` leaves test failure vs error to the client | client report (4.6); server parsing deferred (Q4) |
| live stdout of a running action | none: the daemon uploads stdout at the end **[I]** | out of scope |

**Attempts with no logs [F].** A memory-killed, lost or infra attempt has no stdout or stderr: the daemon's `failure()` sets `action_result: None` (`crates/kbf-daemon/src/lease.rs:351-360`), and `carry_usage` then adds a default `ActionResult` holding only usage (`lease.rs:313-316`). Such an attempt's `logs` key is absent (not measured, 6.1), so an agent does not wait for a log that never existed; its reason text and the usage block are what it has.

**Logs and their lifetime.** **[F]** stdout/stderr always go to the CAS by digest, capped at 1 GiB per stream (`crates/kbf-outputs/src/limits.rs:19-35`). Read by `GET /v1/blobs/{digest}` with one Range or `?inline=text` up to 4 MiB (`crates/kbf-server/src/http_blobs.rs:15-25`, `:155`). Retention policy is 7 days after last touch plus a 1-day quantum (`crates/kbf-meta/src/model.rs:178-202`), but the server never runs collection (no caller of `Cache::collect` in `crates/kbf-server/src`), so on S3/MinIO a log is kept forever, and on a `file://` cold store the evictor can drop it 60 s after last use under disk pressure (`crates/kbf-front/src/cache/evict.rs:30-33`). **[F]** A failed run's ActionResult is in the CAS but not in the AC (only exit 0 is cached, `crates/kbf-server/src/farm.rs:1142`), so nothing indexes it once the record is gone. The run record is that index.

**Tails in the record [I].** A failed or timed-out run's last 16 KiB of stdout and of stderr, after operator redaction (8), are stored with the record in a `run_tail` row keyed by `run_id`, so cross-build lists and `/failures` past the live view serve tails from rows and never read blobs. The history writer reads them after the commit, off the acknowledgement path, under its own share of the read budget, and skips a log over 64 MiB (`tail_omitted: "too_large"`) **[V]** (alternative: the daemon carries the tails in `auxiliary_metadata`). Arithmetic: 3,456 failed runs/day x 32 KiB = about 113 MB/day, 3.4 GB at 30 days (5.3). A tail stored is redacted with the patterns in force when it was written; a leak found later is purged (8).

**Pins for failures.** Proposal: pin a failed or timed-out run's stdout, stderr and ActionResult for 30 days within a byte budget (`--failure-pin-bytes`, default 20 GiB). Client reports never count against it: they have their own budget (4.6). **Per-instance shares**: each instance gets a share of the budget (a policy key; default the budget divided by the instances with a pin in the last 30 days), and eviction drops the oldest pin of the instance furthest over its share first, so one instance's noisy failing suite evicts its own pins, not another's. The Command is not pinned as a raw blob: its decoded, redacted rendering (argv after redaction, env names only, at most 16 KiB) is stored as a history row (8). **Per-run cap**: a stream over 16 MiB is not pinned and shows `pinned: false, pin_skipped: "too_large"`; one run therefore pins at most about 32 MiB, so failure pins always keep at least 20 GiB / 32 MiB = 640 failed runs farm-wide, and at least share / 32 MiB per instance; a bad build of huge logs evicts only its own instance's pins beyond that. Arithmetic **[I]**: at a 1% failure rate, 3,456 failed runs/day; at 64 KiB of logs each, 221,184 KiB = 226 MB/day, 6.8 GB for 30 days; command renderings at about 4 KiB each, 14 MB/day, 0.42 GB for 30 days. Failure rate and log size are **[V]**. How storage honours a pin is 5.4; nothing in storage does today.

**Run record size [I].** Ids and digests (run id 12, invocation ref 8, action 40, result 40, command 40, input root 40, stdout 40, stderr 40) 248 B; nine REAPI timestamps plus `granted`, 10 x 8 = 80 B; instance ref 4, shape 8, cfg 8, node ref 4, lease 16, exit code, outcome, memory kill 8; booked and raised 16; wait reason and partition 8; farm runs/budget 2; presubmit 4; cgroup 10 counters 80 B plus NUMA (2 nodes) 16 B; transfer 6 counters 48 B; about 560 B raw, about 825 B with its four indexes (by invocation, by action digest, by shape, by node: about 200 B of keys and slack; listed in 5.2). The shape index, `(instance, shape, cfg, time, run_id)` about 50 B an entry, is about 0.5 GB at 30 days, and the node index, `(node, time, run_id)` about 30 B, about 0.3 GB; both are inside the figure below. Outputs stay in a listing (about 2 a run, 44 B plus a 60 B path, 156 B indexed each, about 312 B a run), so the record replaces the 444 B row rather than adding to it: about 1,140 B/run, 394 MB/day, 11.8 GB at 30 days.

**Cache-hit rows [I].** A hit has no run, but `/actions` lists it today. The history keeps a `hit` row: instance 4, invocation ref 8, action digest 40, `how` 1, time 8, about 61 B raw, 90 B indexed; no output listing, because the ActionResult in the AC holds it. At h = 0.9: 3,110,400 hits/day, 280 MB/day, 8.4 GB for 30 days (h = 0.67: 1.9 GB).

**Attributing a hit to a shape [I].** A hit row carries no shape, but the daily rollup counts `cached` per (instance, shape, cfg, branch). The history writer resolves each hit's digest through an in-memory LRU of digest to shape (backed by `digest_shape`), keeps per-(day, instance, shape, cfg, branch) counters in memory, and every 10 minutes (and at shutdown) upserts only the keys that changed since the last flush. Until a flush the in-memory counters are the source of truth: the single frontend adds its unflushed deltas to a rollup read; with several frontends (P9) a rollup read is up to 10 minutes stale; a crash loses at most 10 minutes of counts, and `cached` counts are not rebuildable (5.1). The same flush upserts `last_cached_pass` for test shapes (4.3). A hit does not bump `digest_shape` row by row: the writer keeps the set of digests hit each day in memory and writes it once a day, sorted, as an append-only `digest_seen(day, instance, digest)` partition dropped after 30 days; `digest_shape` expiry keeps a row whose digest is in any live `digest_seen` partition, so a digest served only from cache keeps its row alive. Size **[I]**: about 70 B an entry indexed; at an assumed 1,000,000 distinct digests hit a day **[V]**, 70 MB/day, 2.1 GB at 30 days, and about 100 MB of memory for the day's set (capped; past the cap it flushes early into the same partition). Costs are in 5.2.

Routes: `GET /v1/invocations/{id}/actions` and `/actions/{digest}` (built; built fields unchanged, new keys added, new filters `requeue_reason=`, `memory_kill=`, new `sort=`); `GET /v1/invocations/{id}/failures` (planned PR 12, `invocations-api.md`); `GET /v1/runs/{run_id}` (new: one attempt); `GET /v1/runs?node=&from=&to=&class=&instance=` (new: runs by node and time, paged, instance-scoped as in 8); `GET /v1/actions/{digest}/runs` (new: every run of a digest across builds the caller may read); `GET /v1/commands/{digest}` (new); `GET /v1/blobs/{digest}` (built).

### 4.3 Shape and test history

**Every shape has history.** Duration trends and runs by key exist for every shape, so a compile or link that slows across builds (its digest changes with the code, its shape does not) is answerable: `/v1/shapes/{key}/daily` gives the trend, and `/v1/shapes/{key}/runs` lists the runs behind it (run id, digest, invocation, node, execution time) for any shape within the 30-day run tier, so the digest and commit where it slowed and the nodes that ran the slow ones are one more call. Past 30 days only test shapes keep per-run rows (outcomes); other shapes keep their daily rollups. Tests are a filter.

**What a test is to the farm [I].** An action whose shape marks it a test: a buck2 output under `buck-out/v2/test/execution/`, or a Bazel `action_mnemonic` of `TestRunner` with a `target_id`. Its key is `(instance, shape, cfg)`; `target` is attached when Bazel names it or a client report maps the key.

**From a target to its keys.** An agent knows a test by its target label. `GET /v1/shapes?target=<label>&program=` returns every `(shape, cfg)` key mapped to that target, with first and last seen; `/v1/shapes/{key}` carries `previous_keys` and `next_keys` when reports map two keys to the same target (a changed test-case set or configuration). Without any report or Bazel `target_id`, the link is unknown, and the answer says `target: null`.

**Outcome per run** (separate infra from test, as ResultDB and TestGrid do):

| Status | Rule |
|---|---|
| `passed` | ran, exit 0 |
| `failed` | ran, non-zero exit (Action class) |
| `timed_out` | Timeout class |
| `infra` | Infra, lost, node pressure, missing input; excluded from flakiness and first-failing math |
| `oom_rung` | an own-limit memory kill followed by a ladder rung; not a test result |
| `oom_killed` | an own-limit memory kill with no rung left (`memory_kill: own_limit`, no requeue): the attempt that ends the operation. Counts as a failure for `first_fail`, transitions and `status`: the action does not fit the farm's largest node, and the cause is the action, not the farm |
| `cached_pass` | an AC hit on a test shape's digest (only exit 0 is cached, so it is a pass, `source: cache`); counts as a pass for `status`, `last_pass`, transitions and grid cells, never for the digest rule (it adds no new digest observation) |
| `preempted` | an idle-band run killed for real work (7); not a test result |

A cache hit on a test is not an outcome row, which keeps outcome volume independent of h, but it is an observation. The rollup flusher, which already resolves each hit to its shape (4.2), upserts one `last_cached_pass {commit, invocation, at, digest}` per (instance, shape, cfg, branch); status, `last_pass` and transitions read it beside the outcome rows. Without it a fix served from cache (a revert that restores a digest that passed before, the usual case on a cached farm) would leave `status: failing` and `first_fail` at the broken commit for ever. Grid cells read `cached_pass` from the `hit` rows of their column's invocation within 30 days (joined through `digest_shape`), and from the daily rollup's `cached` count beyond. Volume: one row per key, about 80 B, at most 50,000 rows, written inside the rollup flush.

**Derived per `(instance, shape, cfg)`:**

- **Flaky, proven.** `failure-classes.md` 6.3 (decided): one action digest seen both passing and failing is flaky, a lower bound because a pass is cached. Computed from outcome rows, which carry the digest, so it holds for the whole 400-day window, not only while run records live. The answer names `proof_digest` and links its runs.
- **Flaky, possible.** An ambiguous run followed by a pass (6.3 case 3; the planned `kbf_possible_flaky_digests`, `failure-classes.md:906`).
- **Per branch.** Transitions, `flaky.since`, `last_pass` and `first_fail` are computed per branch, over outcomes whose invocation carries that branch (4.6), defaulting to a configured default branch (`--history-default-branch`, default `main`). Commits are opaque client strings, so they have no order across branches; a work-in-progress build that breaks a test on branch X never moves the default branch's `first_fail` or `flaky.since`. Outcomes with no branch count only toward the digest rule (proven flaky), which is branch-independent.
- **Transition score (hint).** Pass/fail transitions over the last N non-infra runs of one branch, ordered by commit time when known, else by time (the Buildkite transition count).
- **Flaky since.** The first non-infra transition, or the first digest seen both ways, inside the window: `{commit, invocation, at}`. Unlike `first_fail`, it does not move when the test passes again.
- **First failing commit.** The earliest commit after the last pass in the ordered history; without commits, the first failing invocation and its time. **[F]** No commit is in any REAPI field; it needs 4.6. Outcome rows carry a commit ref, so it works over 400 days.
- **Duration trend.** p50/p90 of execution time (from the copied timestamps, not wall incl. queue) per day, for every shape.

**Source today.** **[F]** None: the record is in memory, cross-build queries are out of the invocations plan (`docs/design/invocations.md:444-453`), and no flake table or `/v1/flakes` route exists (section 0).

**Sizes [I].**

| Table | Row | Volume | Retention | Size |
|---|---|---|---|---|
| test outcome | instance 4, shape 8, cfg 8, action digest 40, commit ref 8, branch ref 8, invocation ref 8, time 8, run id 12, status 1, source 1, duration 4, node 2, memory kill 1: about 112 B, 168 B indexed | assume 20% of executions are tests: 69,000/day | 400 days | 11.6 MB/day, 4.6 GB |
| daily rollup per `(day, instance, shape, cfg, branch)` (day leads the key, so one flush touches one day's pages) | runs, passes, fails, infra, cached, transitions, p50, p90, first/last seen: 68 B, 100 B indexed | assume 50,000 active (key, branch) pairs a day, tests and others **[V]** | 400 days | 5 MB/day, 2.0 GB |
| shape catalog | key, program words (redacted, 8), first 8 patterns, cfg list, targets seen with first/last seen: about 500 B, 750 B indexed | assume 2,000 new keys a day **[V]**: count distinct `action shape:` lines per day (kbf#765, on this branch) | 400 days after last seen | at most 2,000 x 750 B x 400 = 0.6 GB |
| digest to shape | instance 4, digest 40, shape 8, cfg 8, command digest 40, last executed 8: about 160 B indexed, plus an index on command digest, about 60 B (8) | one per executed distinct digest | 30 days after its last execution or its last `digest_seen` day (4.2) | at most 345,600 x 30 x 220 B = 2.3 GB |
| digest seen | day 2, instance 4, digest 40: about 70 B indexed, append-only, one partition a day | distinct digests hit a day, assumed 1,000,000 **[V]** | 30 days | 70 MB/day, 2.1 GB |

**[I]** Per-action rows dominate; multi-year test history is cheap.

Routes (new): `GET /v1/shapes?kind=test&status=failing|flaky|slower|oom_killed&target=&program=&branch=&since=` (list; `/v1/tests` is the same with `kind=test`), `GET /v1/shapes/{key}?instance=&cfg=&branch=` (catalog, cfgs, targets, key links, `largest_digest_floor`, current status, flake state; one row when instance and cfg are given, else a list of per-(instance, cfg) rows, paged), `GET /v1/shapes/{key}/runs?instance=&cfg=&branch=&since=` (for any shape: run records within 30 days, then, for a test shape, outcome rows beyond; each item says `source: "run"` or `"outcome"` and links its run, which answers `410` past 30 days), `GET /v1/shapes/{key}/daily?instance=&cfg=&branch=`, `GET /v1/grid?shapes=...&branch=&columns=commit|invocation` (6.4). `branch=` defaults to the default branch; `branch=*` reads all.

### 4.4 Farm state

| Field | Source today | To add |
|---|---|---|
| node id, connected, software, needs-attention, placement (serving, cordoned, draining, drained, paused), phase (including `MemoryPressure`) | **[F]** `GET /v1/nodes` (`NodeView`, `crates/kbf-server/src/fleet.rs:74`; route at `crates/kbf-server/src/api.rs:62-63`), an open read | none; `docs/api.md` example omits the `phase` field the code sends |
| booked, lease ids, phase per worker; queue length, queued ids, leases held | **[F]** public reads: `booked(worker)`, `leases_on(worker)`, `phase(worker)`, `queued()`, `queued_len()`, `leases_held()` (`crates/kbf-sched/src/scheduler.rs:380-437`) | expose on `GET /v1/nodes/{id}` |
| capacity (CPUs, memory, GPUs), free, whole-machine hold, devices, last heard | **[F]** in `Scheduler::Worker`, `pub(crate)` with no accessor (`scheduler.rs:115-147`) | public `Scheduler::snapshot()`; v2: `session/` and `member/` rows |
| memory-pressure kills per node | none kept as a count | a counter of `Failure::NodeMemoryPressure` results (`scheduler.rs:838`), per node |
| failures per node | none | on `GET /v1/nodes/{id}`: failed runs by class in a window (default 24 h, from the run table's node index), and "failed here, passed elsewhere": failed runs on this node whose (instance, action digest) has a passing run on another node (one digest-index probe per failed run, about 3,456 a day farm-wide **[I]**). Co-tenants of a killed run: `GET /v1/runs?node=&from=&to=` over the run's times |
| NUMA layout per node | **[F]** daemon `numa layout:` log line only (`crates/kbf-daemon/src/numa.rs:104`) | in `Hello` capabilities |
| queue depth | **[F]** one unlabeled gauge `kbf_sched_queued` (`crates/kbf-front/src/metrics.rs:635`); one global queue (`scheduler.rs:234`); per-op `waiting(op)` (`scheduler.rs:329-333`) | depth and oldest age by platform, size class, QoS; unservable counts by reason; v2: queue partitions (`architecture-v2.md` 4.2) |
| leases | **[F]** `kbf_sched_leases` gauge; lease ids only in drain views | `GET /v1/leases` |
| storage map, member states, under-replicated, handoff backlog, repair records, lost blobs | none in code: only in `architecture-v2.md` 6, 8 | with P3 to P6 |
| hot-store fill, flush lag | **[F]** `kbf_write_tier_*` with `--hot-store` (`crates/kbf-front/src/metrics.rs:701-797`) | none |
| health | **[F]** `/healthz`, `/readyz` with `stopping`, `leader`, `store` (`crates/kbf-server/src/health.rs:142-208`) | v2 meaning: state store reachable, map loaded; per-node health |
| alerts, operator actions | **[F]** `kbf-alert` exists unwired (no dependent crate); attention items logged only (`crates/kbf-server/src/farm.rs:419-424`); cordon/drain logged (`farm.rs:474`) | event rows (5.2) |

- **[F]** All of it is in memory under one `Mutex<State>` and lost on restart, cordons included (`crates/kbf-server/src/farm.rs:118`, `:185-203`). In v2 the state store holds it; reads are prefix scans plus the change feed (C5).
- Retention: current values only, except the samples of 4.5 and the event rows (400 days; about 1,000 events/day at 200 B is 73 MB/year **[I]**).
- Routes: `GET /v1/nodes` (built, open), `GET /v1/nodes/{id}`, `GET /v1/nodes/{id}/samples?from=&to=`, `GET /v1/runs?node=&from=&to=&class=&instance=` (runs that overlapped the window, from the run table), `GET /v1/queue`, `GET /v1/leases?node=` (current only), `GET /v1/storage/map`, `GET /v1/health`, `GET /v1/events?since=` (new).

### 4.5 Metrics over time

- **[F]** `/metrics` resets at process start; only `kbf_meta_commit_seconds` has a label (`crates/kbf-front/src/metrics.rs:649-657`); no daemon serves metrics; no scrape or time-series config exists in the repo. Two counters are kept but exported nowhere: `kbf_invocation_metadata_invalid` and `kbf_invocation_events_dropped` (`crates/kbf-front/src/request_metadata.rs:232`, `crates/kbf-front/src/recorder.rs:190`).
- **Two paths, one source.** Labeled families on `/metrics` (by platform, QoS, node, outcome class) for an operator's Prometheus; and a small built-in rollup of a fixed series set in the history store, so the owner's health view works with no external system.
- **Fixed series set** (from `architecture-v2.md` 11 plus the questions of 2.1): queue depth and oldest age by partition; claim wait p50/p90; leases; idle CPUs and memory per node; busy CPU fraction; executions, cache hits, failures by class, farm reruns, OOM rungs per minute; failures by class per node (4 classes: action, timeout, infra, memory); hot-store fill; flush lag; under-replicated and lost blobs; group-commit latency; history writer drops and queue fill.
- **Downsampling per series kind.** A mean is wrong for most of these: a mean of minute p90s hides one bad minute (one 60 s minute among 59 at 1 s averages to about 2 s), and a mean of a gauge hides the peak W2 and O2 ask about. So each series has a kind. **Counters** (executions, hits, failures, reruns, OOM rungs): a point is the sum over its interval; an hour sums its minutes. **Gauges** (queue depth, oldest age, leases, idle CPUs, fill, flush lag, under-replicated, lost blobs): a point is min, mean and max; an hour takes the min of mins, mean of means, max of maxes. **Latency** (claim wait, group-commit latency): a point is a fixed-bucket histogram (16 buckets x 4 B); an hour adds its minutes' buckets, and p50/p90 are read from the merged histogram, within one bucket's width.
- **Node samples** (booked, free, lease count, phase, pressure, CPU busy; 20 numeric fields) every 15 s, stored as binary rows (20 x 4 B, a time and a node ref: about 92 B), kept as long as run records (30 days), read by `GET /v1/nodes/{id}/samples?from=&to=` (paged). Each run's `node` links there with `from` and `to` set to the attempt's times, so "slow because of the node" is one call for the whole life of the run record.
- **Sizes [I].** Built-in: 40 series plus 8 nodes x 10 per-node series (6, plus 4 failure counters) = 120 series, assumed 62 counters, 54 gauges, 4 latency; a point is an 8 B time plus 8 B (counter), 3 x 8 B (gauge) or 64 B (histogram). 1-minute points for 14 days (20,160 a series): 62 x 20,160 x 16 B + 54 x 20,160 x 32 B + 4 x 20,160 x 72 B = 20.0 + 34.8 + 5.8 = 61 MB; hourly for 400 days (9,600 a series): 9.5 + 16.6 + 2.8 = 29 MB; about 90 MB in all. Node samples for 30 days: 8 x 5,760 x 30 x 92 B = 127 MB (as 0.5 KiB JSON it would be 708 MB). An external Prometheus over about 1,000 series at 15 s and about 1.5 B a sample stores about 8.6 MiB/day.
- Route: `GET /v1/series/{name}?from=&to=&step=&labels=` (new); `GET /v1/series` lists names, units and retention.

### 4.6 Client report (what REAPI cannot carry)

- Route: `POST /v1/invocations/{id}/report`, body one of: a buck2 build report (`--build-report`), buck2 `log what-ran --format json`, buck2 `log critical-path --format json`, a JUnit XML file per test, and a small JSON of `{commit, branch, repo}`. Bazel BES is later.
- **Who may write.** A caller whose `public` metadata equals the record's `owner` (`invocations.md` 2.4), and, once read tokens exist, not a `read` role. A different owner's report goes to its own `<id>~<n>` record, as its calls would, and never changes the first record's commit or target map. **[F]** The owner check means something only under a per-caller authenticator: `owner` is the caller's `public` metadata (`crates/kbf-front/src/recorder.rs:470-475`), an `allow` authenticator gives every caller the same fixed metadata (`crates/kbf-auth/src/policy.rs:280-290`), and with no policy file every caller's metadata is empty (`invocations.md:170-172`), so under either every caller owns every record. Hence the route is off unless the policy authenticates callers (8).
- **Bounds.** At most 32 MiB per report and 16 reports per invocation; over either, `413`. A per-owner quota (bytes a day, a policy key, default 1 GiB) on top; over it, `429`. Reports are CAS blobs pinned (5.4) in their own budget (`--report-pin-bytes`, default 10 GiB, kept 7 days), never in the failure-pin budget, so a report never evicts a failure log. Eviction drops reports of invocations with no failed run first. Volume **[I]**: a 9,000-action build's `what-ran` is about 2.7 MB at 300 B a line, plus a build report, about 3 MB an invocation; at the assumed 1,000 invocations a day that is about 3 GB a day, so the 10 GiB budget, not the 7 days, binds at about 3 days. That is enough: the joins that matter (commit, branch, target per shape, test cases) are copied into rows when the report is posted and outlive the blob.
- Gives: commit (first failing commit), target names per shape (joined by the action digest or output path in `what-ran` **[V]**), critical path, test-case results.
- Read back: `GET /v1/invocations/{id}/report` (the stored blobs, with links) and `GET /v1/invocations/{id}/tests` (parsed test cases joined to shape and run, paged); a failed test action's entry in `failures` carries `cases_href`.
- Alternative for the commit alone: a header the client sends on every call. **[V]** whether buck2's RE client config can add a header from the environment.
- Size **[I]**: a 50,000-action `what-ran` at about 300 B a line is 15 MB, which is why only the joins (target per shape, commit) are copied into rows.

## 5. Storage of history

### 5.1 Recorded at the time, or derivable

| Data | Recorded at the time | Derivable later |
|---|---|---|
| run record (attempts, timestamps, usage, class, node) | yes: the operation is dropped 60 s after it ends | from the ActionResult blob only while it lives, and only for runs that reported |
| hit rows | yes: a hit leaves no other trace | no |
| shape, cfg | yes: the Command is not uploaded for a hit and is dropped after the key is made | no |
| digest to shape | yes | no |
| test outcome rows | from run records | yes, rebuildable from run records within their 30 days |
| daily rollups | live: test and non-test shapes from run events, `cached` from hit events, flushed every 10 minutes (4.2) | test shapes from outcome rows within 400 days; non-test shapes from run records within 30 days only; `cached` counts not |
| series rollups | yes: gauges are instantaneous | no |
| invocation summary | from run records | yes |

### 5.2 Where it lives

- **Backend follows the state store [D]-pending.** **[F]** `architecture-v2.md:8` says the frontend "Holds no durable state of its own beyond the state store" and is stateless on Postgres. A local history file is durable frontend state outside it, so: SQLite history with a SQLite state store (that frontend already keeps its queue in a local file), a Postgres history database with a Postgres state store. The server refuses `--history-path` when `--state-store` is Postgres, so a Postgres frontend stays stateless and P9/P10 failover holds. Red test (U3): `--state-store=postgres://... --history-path=...` exits with a usage error. Adopting this amends `architecture-v2.md:8` to name the history store as the second durable store (this commit marks it pending there; section 10 Q1).
- **A history store separate from the state store.** The reason is the state store's commit budget (`architecture-v2.md` 4.1 C7): every `Start` and `Result` acknowledgement waits for a group commit, and history must not compete for it. A separate file or schema is not enough on its own:
  - **SQLite.** `history.db`, its own writer thread and lock, in WAL mode with `synchronous=NORMAL`: commits do not fsync, and power loss can cost the last batches, acceptable because drops are already allowed. At most about one commit a second; automatic checkpoints off, a passive checkpoint run by the history thread on a timer (default 60 s) so its fsync is not on the state writer's path. Both files sit on one disk by default (all-in-one keeps the store beside its data, `architecture-v2.md:56`), and C7's 200 to 1000 writes/s per fsync is a property of the device, so `--history-path` lets an operator put history on a second disk. **[V]** Gate: the P1 store-commits benchmark (1,500 writes/s) must still meet the C7 commit-latency bound with the history writer running at the burst rate below on the same disk.
  - **Postgres.** A schema shares the server's WAL, whose flush is the C7 group commit (`architecture-v2.md:67`), and its backup. Recommend a separate database, ideally on another server, through its own `--history-store` URL; defaulting to the state-store server is for small farms only, and then the store-commits benchmark must include the history WAL volume **[V]**. The WAL arithmetic is below.
- **Written after commit.** The frontend enqueues a run event after the `Result` commit, and a hit event after the answer. A bounded queue drains into batched inserts. A full queue drops and counts (`kbf_history_dropped_total`). History never delays a build; losing it loses rows, never results.
- **Write rate at the burst [I].** At 300 executions/s, one execution writes 1 run row (825 B), about 2 output rows (312 B), 0.2 outcome rows (34 B), 1 `digest_shape` insert (220 B) and, for 1% failed runs, a 32 KiB tail row (about 330 B on average): about 1.7 KB, 0.5 MB/s of row bytes. Hits at h = 0.9 add 2,700 rows/s of 90 B, 0.24 MB/s. Row bytes are not what the disk writes: both backends write whole pages, so the cost is pages dirtied per commit, counted per index below.
- **Day partitions [D]-pending.** `run`, `output`, `run_tail`, `hit`, `node_sample` and `digest_seen` are partitioned by day (a table per day on SQLite, native range partitions on Postgres; the `run_id` time prefix names the partition). Each index then holds one day (the run digest index about 345,600 entries, 24 MB), stays in the page cache, bounds what a checkpoint writes back, and retention drops a partition instead of deleting rows.
- **Every index [I].** Append (keys grow with time, inserts land on the right edge): `run` by `run_id`; `output` by (`run_id`, path); `run_tail` by `run_id`; `hit` by row id; `digest_seen` (written once a day, sorted). Clustered (few distinct leading values active at once): `run` and `hit` by (instance, invocation, time), assumed at most 50 builds in flight **[V]**; `run` by (node, time). Random (one leaf page per touched entry while touches are far fewer than pages): `run` by (instance, action digest, time) and by (instance, shape, cfg, time); `digest_shape` by (instance, digest) and by command digest; `test_outcome` by (instance, shape, cfg, branch, time) and by (instance, action digest). Rewritten in place: `shape_daily` by (day, instance, shape, cfg, branch), counter columns unindexed; `last_cached_pass` by (instance, shape, cfg, branch); `shape`, `shape_target`, `invocation` (low rate).
- **SQLite WAL [I].** WAL bytes per commit = pages dirtied x 4 KiB. At the burst, a commit of about one second of events: appended rows about 0.75 MB, about 190 pages; clustered indexes at most 50 + 50 + 8 pages; random indexes 300 (run by digest) + up to 300 (run by shape, fewer when builds repeat shapes) + 300 + 300 (`digest_shape` and its command index) + 60 + 60 (outcomes) = 1,320 pages. About 1,620 pages, **about 6.6 MB/s of WAL at the burst** (an earlier draft said "under 1 MB/s"; that counted row bytes). A rollup flush dirties at most today's rollup pages, 50,000 x 100 B = about 1,220 pages (5 MB), once in 10 minutes. At the 4/s daily average, with batches of 10 s, a commit dirties about 176 random pages (40 executions x 4.4 random entries) plus about 15 appended and clustered pages: about 0.78 MB per 10 s, about 6.7 GB/day of WAL written (random entries dominate, so batching saves little), plus at most 0.7 GB/day of rollup flushes; the WAL is truncated at each checkpoint, so this is write volume, not disk held.
- **SQLite checkpoint [I].** A checkpoint writes each page dirtied since the last one back once, at random offsets. At the burst, 60 s of touches cover nearly all of today's run digest and run shape index pages (about 6,000 and 4,000) and about 18,000 pages each of `digest_shape` (2.3 GB, about 560,000 pages) and its command index: about 46,000 pages, about 190 MB of random 4 KiB writes a checkpoint, about 3 MB/s and 770 writes/s. The checkpoint runs on its own connection and thread (a passive checkpoint does not block the WAL writer), so the event queue keeps draining; stalled, the 131,072-event queue fills in about 44 s at 3,000 events/s. **[V]** Gate (U3): at the burst for 10 minutes on the shared disk, WAL at most 8 MB/s, every checkpoint under 20 s, zero `kbf_history_dropped_total`, and the C7 commit-latency bound held. If it fails on one disk, history needs its own (`--history-path`).
- **Postgres WAL [I].** A record per row and per index entry (row bytes plus about 50 B), plus a full-page image (8 KiB before compression) for the first change to each page after a checkpoint. With `checkpoint_timeout` 15 minutes and day partitions, today's random index pages take at most one image each a checkpoint: the run digest and run shape indexes, about 5,000 pages at the day's end, half that on average, x 96 checkpoints = about 1.9 GB/day. `digest_shape` and its command index (about 280,000 pages) and the two outcome indexes (400 days) are not day-partitioned and take nearly one image per touch: 345,600 x 2 x 8 KiB = 5.7 GB/day and 69,120 x 2 x 8 KiB = 1.1 GB/day at the average rate. Row records: executions about 345,600 x 1.7 KB = 0.6 GB, hits 3,110,400 x 200 B = 0.6 GB. Rollups: changed keys x flushes x about 150 B, at most 50,000 x 144 = 7.2 million updates = 1.1 GB/day if every key changed in every flush, about 0.1 GB/day at an assumed 5,000 changed keys a flush **[V]**, plus images of today's rollup partition (at most about 900 pages x 96 checkpoints = 0.7 GB/day). `shape_daily` and `last_cached_pass` get `fillfactor` 70 and unindexed counters so updates stay HOT. Total about 11 GB/day before `wal_compression`, roughly half with it **[V]**, dominated by `digest_shape` images; that is why a separate database is recommended. **[V]** Partitioning `digest_shape` by day (lookups probe the newest partitions first) would cut its images to today's pages; left for measurement.
- **Queue and batch.** Capacity 131,072 events or 64 MiB, whichever is reached first: one whole 9,000-execution build with its hits (90,000 events, about 20 MB) fits with the writer stalled. Flush at 10 s or 5,000 rows (at the burst, 5,000 rows arrive in about 1.3 s). Queue fill is a series (4.5).
- **Several frontends (P9/P10).** Each frontend writes the runs it committed into one history database. On SQLite there is only one frontend.
- **Replaces `invocations.md` 3.5 stage 2.** That design keeps chunks in the CAS with a pin per invocation on the Raft metadata log, which v2 removes. Run and hit records indexed by invocation answer `GET /v1/invocations/{id}/actions` directly; there is no second durable record. The in-memory recorder stays as the live view of builds in flight.
- **Tables:** `invocation`, `run`, `output` (the listing), `run_tail`, `hit`, `shape`, `shape_target`, `digest_shape`, `digest_seen`, `test_outcome`, `last_cached_pass`, `test_case` (parsed from reports, 4.6), `shape_daily`, `series_minute`, `series_hour`, `node_sample`, `event`, `pin`, `purged`, `command_view`. Every row that names a build, run, digest or target carries `instance`.

### 5.3 Retention tiers

| Tier | Kept | Size at the assumed rate **[I]** |
|---|---|---|
| run records and listings | 30 days (`--history-runs-days`) | about 11.8 GB |
| hit rows | 30 days | about 8.4 GB at h = 0.9 (1.9 GB at h = 0.67) |
| digest to shape | 30 days after last execution or `digest_seen` day | at most 2.3 GB |
| digest seen | 30 days | about 2.1 GB at 1,000,000 distinct hit digests a day **[V]** |
| run tails (failed and timed-out runs) | 30 days | about 3.4 GB |
| invocation headers | 400 days | about 0.18 GB at 1,000 a day **[V]** |
| test outcomes | 400 days | about 4.6 GB |
| last cached pass | while the key lives | at most 4 MB |
| daily rollups | 400 days | about 2.0 GB |
| shape catalog and target map | 400 days after last seen | at most 0.6 GB |
| command renderings of failed runs | 30 days | about 0.42 GB |
| test cases parsed from reports | 30 days | depends on report volume **[V]** |
| series | 1 min 14 days, 1 h 400 days | about 90 MB |
| node samples | 15 s for 30 days, binary rows | about 127 MB |
| events | 400 days | about 73 MB |
| failure pins (logs, result) | 30 days within 20 GiB, shared per instance | at most 20 GiB, in storage, held by the pin set of 5.4 |
| client report pins | 7 days within 10 GiB (4.6) | at most 10 GiB, in storage, held by the pin set of 5.4 |

Total about 36 GB of history at h = 0.9 (about 30 GB at h = 0.67), plus the two pin budgets. Deletion of the day-partitioned tables (5.2) drops a partition; the 400-day tables delete by a time index in small batches **[V]**. Rows outlive the blobs they point to and show `available: false`, as `invocations.md` 3.6 already says. An outcome row outlives its run and invocation summary: its run link answers `410`, but its digest, commit and status stay, so flake proof and first failing commit hold for 400 days.

### 5.4 Pins in storage

**[F]** No storage-side pin exists. The `file://` evictor drops a blob 60 s after its last use under disk pressure (`crates/kbf-front/src/cache/evict.rs:28-30`), and the only pins in `kbf-meta` and `kbf-front/src/cache` are pending touches held until `Cache::collect` commits them (`crates/kbf-front/src/cache/touches.rs:11`). A `pin` row in the history store holds nothing on its own.

**Design (a): a pin set storage consults [I].** Chosen over (b), copies into a retained namespace, because (b) doubles the bytes of every pin and needs its own reads and purge path.

- **Owner.** The `pin` table (digest, instance, kind `failure|report`, until, bytes) is written by the history writer; the cache crate (`kbf-front` cache today, `kbf-storage` in v2) owns an in-memory `PinSet` of digests, loaded from that table at start and updated by the writer (an add or a drop is an in-process call on the single frontend). Size **[I]**: failure pins about 3,456 runs x 3 digests (stdout, stderr, result) a day for 30 days, 311,000, plus report pins about 3,000 a day for 7 days, 21,000: about 330,000 digests, about 20 MB of memory at 60 B each.
- **v1.** The evictor (`cache/evict.rs`) and `Cache::collect` skip a digest in the `PinSet`. Pinned bytes count against the disk the evictor protects, so the pin budgets (30 GiB together) must fit under the cache's free-space target; the server refuses to start when they do not **[V]**.
- **v2.** The hot store may drop a blob once it is flushed (the cold store has it), so the owner's hot-tier eviction needs no change. Cold-store retention and segment compaction (neither built, 8 Purge) must skip pinned digests; their designs consult the pin set through the state store, where the history writer mirrors pins as `pin/<digest>` rows **[V]**.
- **Expiry and purge.** A pin past `until`, or evicted by its budget's rule (4.2, 4.6), is dropped from the table and the set; purge drops the pin first (8).
- **Phase.** Pins land in U3 with the history store, before the report route of U4 and the pages of U6. Red test (U3): on a `file://` cold store under disk pressure, after the evictor runs, a pinned failed run's stderr is still readable from `/v1/blobs` and an unpinned blob of the same age is not.

## 6. Read API

### 6.1 Rules

- **The UI uses only this API.** A page is server-rendered from these JSON documents (PR #751's direction: embedded assets, no client toolchain **[I]**). Each page shows the URLs it read. A contract test lists, per page, the API calls it makes and fails if a page reads anything else.
- **One link form, as built.** **[F]** Every built link is under the key `href`, root-relative (`/v1/blobs/{digest}?instance=..`, or `/v1/invocations/{id}/actions/{digest}`), and a blob link is `null` when the blob route is off (`crates/kbf-server/src/http_invocations/views.rs:40-81`, `:152`, `:169`; `pages.rs:521`). Every new link uses the same form: the key is `href`, or `<name>_href` beside other keys of one object (`outputs_href`, `tree_href`, `runs_href`, `proof_runs_href`, `node_samples_href`, `cases_href`); it is root-relative, and a client resolves it against the listener it read the document from; a link whose route reads blob bytes (`/v1/blobs`, `/v1/commands`, `/v1/trees`) is `null` when the blob route is off, as built. `kbf build show <href>` (`invocations-api.md` 4.8) reads any link, so the CLI and the UI see the same documents. Every run anywhere (an attempt in an action, an outcome, a grid cell, an entry of `actions/{digest}/runs`) links to `GET /v1/runs/{run_id}`. A run returned outside its action's page carries the envelope of 6.3 (invocation, instance, action, command, result, outputs), so an agent arriving from a grid cell or a flaky shape can reach the build, the command and every output in one more call.
- **Index.** `GET /v1` returns every route with its parameters, units, retention and authority; `GET /llms.txt` is the same as plain text for an agent with no prior knowledge.
- **Paging** as built: `page_size` (default 100, max 1000) and opaque `page_token` (`crates/kbf-server/src/http_invocations/pages.rs:16-19`, parsing at `:224-235`). Filters are query parameters; lists are newest first unless `sort=` says otherwise.
- **Built fields keep their form.** **[F]** Built times are RFC 3339 strings (`queued`, `granted`, `started`, `ended`, `views.rs:309-317`), the lease is `term.seq`, the node is `worker`, an attempt's end is `{"result": "ok"}`, `{"requeue": ..., "reason": ...}` or `{"failure": ..., "reason": ...}` (`views.rs:298-307`). New timestamps use the same RFC 3339 strings; new durations and sizes carry units in their names (`_us`, `_ms`, `_bytes`). Absent means not measured, never zero. A rename would be a breaking change with a version bump; none is proposed.
- **Gone data says so.** `available: false` for a blob; `410` with the retention that expired it for a record past its tier.
- **Tails** on every route returning the run shape follow `invocations-api.md` 4.7: 16 KiB, no tail over 64 MiB (`tail_omitted: "too_large"`), `tail: null, tail_omitted: "busy"` when the read budget is not free within its wait. From U3 a failed or timed-out run's tails come from its `run_tail` row (4.2), so cross-build lists (`/v1/actions/{digest}/runs`, `/v1/shapes/{key}/runs`, `/v1/runs?node=`) serve `?tails=1` from rows and never read a blob; only `/failures` of a build still in the live recorder reads blobs.
- **A share of the read budget per caller.** **[F]** `READ_BUDGET_BYTES` (1 GiB) is one first-come-first-served budget for the whole route, with no per-caller share (`crates/kbf-server/src/http_blobs.rs:19-23`, `:58`). A page reads its logs one at a time (`invocations-api.md` 4.7), so one page holds at most 64 MiB, but nothing caps how many pages one caller has in flight, and open mode needs no credential. Proposed: each caller (an authenticated principal, or the peer address in open mode) holds at most 25% of the budget and 4 concurrent reads; past either, `429` with `Retry-After`, never a wait that takes another caller's share (U1). Red test: one caller looping `/failures` and `/v1/blobs` reads does not make a second caller's `/failures` tails `busy`.
- **Live updates.** `GET /v1/invocations/{id}/events` is fed in process by the recorder on the single frontend (phase U1). `GET /v1/events/stream` carries state rows (nodes, leases, queue) from the state store's change feed (C5), after P1 (phase U5). Both are server-sent events resuming by revision (`Last-Event-ID`); a poller reading by revision gets the same rows. Subscribers are capped per caller (default 4) and in total (default 256), over that `503` with `Retry-After`; a stream closes after 5 minutes with no event. **[V]** With several frontends (P9) a build's calls land on several recorders, so the invocation stream then needs the history store or a feed of its own.

### 6.2 Routes

| Route | Status | Answers |
|---|---|---|
| `GET /v1` , `GET /llms.txt` | new | index |
| `GET /v1/invocations?since=&tool=&commit=&state=` | new (5) | D1 list |
| `GET /v1/invocations/{id}` | built, keys added (`top_slowest`, phase totals, rerun list links) | D1, D5, D7, D10 |
| `GET /v1/invocations/{id}/actions?outcome=&how=&requeue_reason=&memory_kill=&sort=` | built, keys and filters added; `sort=` over `wall_ms`, `execution_ms`, `queue_ms`, `input_fetch_ms`, `output_upload_ms`, descending | D1, D5, D6, D7 |
| `GET /v1/invocations/{id}/actions/{digest}` | built, keys added | D2 to D6, D11 |
| `GET /v1/invocations/{id}/failures` | planned PR 12 | D1 to D4 in one call |
| `GET /v1/invocations/{id}/artifacts`, `/logs`, `/metrics` | built | D3, D5 |
| `GET /v1/invocations/{id}/critical-path` | new (4.6) | D10 |
| `POST /v1/invocations/{id}/report` | new (4.6), owner only, off without a per-caller authenticator (8) | commit, targets, tests |
| `GET /v1/invocations/{id}/report`, `/tests` | new (4.6) | D3, D8 |
| `GET /v1/invocations/{id}/events` | new (U1) | live |
| `GET /v1/runs/{run_id}`, `/v1/runs/{run_id}/outputs` | new | one attempt, D2 to D6; its output listing, paged |
| `GET /v1/runs?node=&from=&to=&class=&instance=` | new (U3): runs that overlapped the window on a node, paged, instance-scoped (8) | O5, D5 (co-tenants of a killed run) |
| `GET /v1/actions/{digest}/runs` | new | D7, flake proof |
| `GET /v1/commands/{digest}` | new | D4 |
| `GET /v1/trees/{digest}?path=` | new: decoded `Directory` listing (names, digests, sizes, executable bit), paged, scoped as blobs are (8) | inputs |
| `GET /v1/blobs/{digest}` | built | logs, outputs |
| `GET /v1/shapes`, `/v1/shapes/{key}`, `/runs`, `/daily` (each with `instance=`, `cfg=`, `branch=`); `/v1/tests` | new | D6, D8, D9 |
| `GET /v1/grid` | new | TestGrid (6.4) |
| `GET /v1/nodes` | built, open | O1 |
| `GET /v1/nodes/{id}`, `/v1/nodes/{id}/samples` | new; failure counts by class and "failed here, passed elsewhere" from U3 | O1, O5, D5 |
| `GET /v1/queue`, `GET /v1/leases` | new | O2, O3 |
| `GET /v1/storage/map` | new (P3) | O4 |
| `GET /v1/health` | new | W1 |
| `GET /v1/series`, `/v1/series/{name}` | new | W2 |
| `GET /v1/events`, `/v1/events/stream` | new (U5) | alerts, operator actions, audit, live |
| `POST /v1/purge` | new, operator | O6 |
| `GET /v1/idle`, `POST /v1/idle` | future (7) | W3 |

### 6.3 Shapes (abridged)

An attempt, as `actions/{digest}`, `failures` and `runs/{run_id}` return it. The first seven keys are the built ones, unchanged (`views.rs:298-317`); the rest are added. **This amends `invocations-api.md` 4.7** (planned PR 12, not built), whose example gave attempts as `{worker, queued_micros, run_micros, end}`: attempts in `/failures` are this shape, with `run_id` and its `href`, and the queue and run times are derived from the built timestamps; that example is edited in the same commit as this text. Inside `/invocations/{id}/actions/{digest}` the action-level keys (action, instance, command, result, outputs) are already on the action (**[F]** `crates/kbf-server/src/http_invocations/views.rs:153-168`, outputs and logs at `:177-216`); every run returned anywhere else (`/v1/runs/{id}`, `/v1/actions/{digest}/runs`, outcome rows, grid cells) adds the `envelope`, which the run record already stores (4.2):

```json
{"lease": "3.4812", "worker": "n4",
 "queued": "...Z", "granted": "...Z", "started": "...Z", "ended": "...Z",
 "end": {"failure": "out_of_memory", "reason": "..."},
 "run_id": "0190b5c2a1f4e3b0c2a1f4e3", "href": "/v1/runs/0190b5c2a1f4e3b0c2a1f4e3",
 "envelope": {"invocation": {"id": "...", "href": "/v1/invocations/{id}"}, "instance": "...",
   "action": {"digest": "...", "href": "/v1/invocations/{id}/actions/{digest}"},
   "command": {"digest": "...", "href": "/v1/commands/{digest}?instance=..."},
   "result": "...", "outputs_href": "/v1/runs/{run_id}/outputs"},
 "class": "out_of_memory", "exit_code": null,
 "memory_kill": "own_limit", "booked_memory_bytes": 34359738368, "cap_bytes": 34359738368,
 "raised_memory_bytes": null, "memory_used_bytes": 34360000000, "memory_limit_bytes": 34359738368,
 "farm_runs": 1, "farm_budget": 2,
 "wait_reason": "...", "partition": {"platform": "...", "size_class": "...", "qos": "ci"},
 "reapi_times": {"queued": "...Z", "worker_start": "...Z",
   "input_fetch_start": "...Z", "input_fetch_completed": "...Z",
   "execution_start": "...Z", "execution_completed": "...Z",
   "output_upload_start": "...Z", "output_upload_completed": "...Z",
   "worker_completed": "...Z"},
 "presubmit_us": 0,
 "inputs": {"root_digest": "...", "files": 0, "bytes": 0, "fetched_bytes": 0,
   "tree_href": "/v1/trees/{digest}?instance=..."},
 "usage": {"cpu_usage_us": 0, "cpu_user_us": 0, "cpu_system_us": 0, "cpu_periods": 0,
   "cpu_throttled": 0, "throttled_us": 0, "memory_peak_bytes": 0, "parallelism": 0.0,
   "numa_node": null, "numa_bytes": [{"node": 0, "bytes": 0}], "numa_spread": 0.0},
 "transfers": {"read_bytes": 0, "written_bytes": 0, "find_missing_digests": 0,
   "outputs_present": 0, "retries": 0},
 "logs": {"stdout": {"digest": "...", "available": true, "pinned": true,
   "kept_until": "...Z", "tail": "...", "href": "/v1/blobs/{digest}?instance=..."}, "stderr": {}},
 "test_logs": [{"path": "test.log", "digest": "...", "href": "/v1/blobs/{digest}?instance=..."}],
 "node_samples_href": "/v1/nodes/n4/samples?from=...&to=...",
 "shape": {"key": "0123456789abcdef", "instance": "...", "cfg": "-",
   "href": "/v1/shapes/{key}?instance=...&cfg=-"}}
```

`memory_kill` is one of `none`, `own_limit`, `node_pressure`. `booked_memory_bytes` is set on every memory-killed attempt (from `Answer.memory_runs`, 4.2), `cap_bytes` only for `own_limit`, `raised_memory_bytes` only for a requeue. The example is a memory-killed attempt and shows `logs` only to give its form; a real one has no `logs` key (4.2: such attempts have none); `logs` is absent, never empty, for memory-killed, lost and infra attempts. `kept_until` is the pin's end or, with no pin, absent: an unpinned blob's lifetime depends on touches and disk pressure and cannot be promised (4.2). The action answer adds `memory_floor_bytes` and `memory_floor_raised_at` for its (instance, action digest), from the scheduler (4.2).

A shape row: `{instance, shape, cfg, kind, target, previous_keys, next_keys, status, flaky: {state: "proven"|"possible"|"none", proof_digest, proof_runs_href, since: {commit, invocation, at}, transitions, window}, branch, last_pass: {commit, invocation, at}, first_fail: {commit, invocation, at}, duration_ms: {p50, p90, trend_7d_pct}, largest_digest_floor: {bytes, digest}, last_cached_pass: {commit, invocation, at, digest}, runs_href}`, all per branch (4.3). `status` is one of `passing`, `failing`, `oom_killed`, `flaky`; a `cached_pass` newer than the last non-infra outcome makes it `passing`. `flaky.state` reports `failure-classes.md` 6.3: `proven` is the digest rule, `possible` the case-3 marking.

A grid (TestGrid model): `{branch, columns: [{id, commit, started}], rows: [{shape, instance, cfg, target, cells: [[status, count, href], ...]}]}` (`status` includes `cached_pass`, whose `href` is the hit's invocation) with run-length encoded cells, so a green row costs one entry.

### 6.4 TestGrid view (future)

Rows are `(instance, shape, cfg)` on one branch, filtered by target prefix, program or status; columns are commits when reports supply them, else invocations; a cell links to its run, or to `runs` for a run-length span. The data is the outcome table of 4.3; nothing new is recorded. Default width 50 columns. Rows and columns are limited to instances the caller may read (8).

## 7. Idle-host test scheduling (future)

Purpose: confirm flakes, refresh duration baselines and recheck tests not run lately, on capacity real builds are not using. It reruns recorded actions; it cannot test new code.

**Data.**

- Idle capacity per node: capacity minus booked CPUs and memory (`booked(worker)` is public today; capacity needs `Scheduler::snapshot()`, 4.4), as a 1-minute series (4.5).
- Idle time per node: how long the node has had no lease from a real QoS level.
- Candidates: test keys from 4.3 whose last executed digest's closure (Action, Command, input root) is still in storage; checked by FindMissing before submit.

**Controls.**

- **A band below real work.** **[F]** QoS levels are `interactive`, `ci`, `batch` (preemptible) and custom levels; every level at or below `batch` is preemptible (`crates/kbf-types/src/qos.rs:19-28`, `docs/design/scheduler.md:254-275`). **[F]** Today every action is submitted at `ci`, and preemption of `batch` is planned, not built (`scheduler.md:271-275`). An `idle` custom level below `batch`: never preempts, is always preempted, and is admitted only to a node with no queued real work for its partition and idle for a dwell (default 5 min).
- **No preemption, no band.** Until preemption is built, idle runs could delay real work by a whole run. Gate: the band ships only after preemption, or with candidates limited to p90 run time under 60 s, which bounds the delay.
- **Whose run it is.** An idle run is submitted under the original row's instance, by a farm principal whose authority is `Execute` only, on instances that opted in. Candidates come only from instances or shapes an operator marked rerunnable (a policy key, off by default), because a test with external effects (network, publishing) would run again. Results are recorded with `source: idle` under that instance only.
- **Budget**: CPU-hours a day, idle leases per node (default 1), and CPUs left free per node (default 2).
- **Selection** (ordered): digest-level possible flakes (`failure-classes.md` 6.3) to confirm; shape-level transition score above a threshold; test keys not run for 7 days; keys whose duration trend rose. Each idle run uses `skip_cache_lookup` and `do_not_cache`, so it never changes the action cache.
- **Results** go into history with `source: idle`; a `preempted` run is not an outcome. A pass after a fail is 6.3 case 1; an idle run that fails on a digest that passed before is case 2, the only way a fail after a pass is seen. That is what idle reruns add.
- **Off by default**, enabled by flag; `POST /v1/idle` (operator authority) changes budget and pauses; `GET /v1/idle` shows budget, spend, running idle leases, preemptions and results.

**Sizes [I].** Bounded by the budget: 100 CPU-hours a day of 30 s tests is 12,000 runs a day, about 14 MB of run records, about 3.5% of the daily volume.

**Prior art.** Borg's free band in reclaimed capacity, lowest priority, killed first; Kubernetes `preemptionPolicy: Never` for the never-preempts half.

## 8. Access control and secrets

**Today [F].**

- The invocation routes are off (`404 off`) unless the policy names `invocations.getAuthorizer` or `--invocations-open` is given (`crates/kbf-server/src/http_invocations.rs:7-12`, `:110-117`). A read asks that authorizer on every instance name the record's calls used, all of which must allow (`http_invocations.rs:136-170`, `pages.rs:474-487`).
- The blob route asks its own `blobs.getAuthorizer` on the caller-chosen `?instance=` parameter, and is off unless both `blobs.getAuthorizer` and `invocations.getAuthorizer` are present (`crates/kbf-server/src/http_blobs.rs:5-10`, `:98-104`; `crates/kbf-auth/src/policy.rs:29-35`). It then reads `cache.read_blob(&digest)` from a CAS that is not partitioned by instance (`http_blobs.rs:201`, `:211-215`). So **a caller allowed on any instance can read any blob whose digest it learns**: a digest is a read capability.
- **The action cache is shared across instances too.** `GetActionResult` authorizes the request's `instance_name` (`crates/kbf-front/src/action_cache.rs:62-68`), looks the result up by action digest alone (`:71`) and records the call under the caller's instance (`:72-77`). A caller allowed only on instance A who computes B's action digest (deterministic from public sources in an open-source repo) gets B's ActionResult, and a `hit` row would land in A's history. Per-instance history scoping therefore does not make the farm multi-tenant.
- **One plain-HTTP listener for every route.** Every HTTP route, the invocation pages, blobs, health, `/metrics` and `/v1/nodes`, is merged into the one operator router on `--api-listen` (`crates/kbf-server/src/serve.rs:315-326`). It is plain TCP, no TLS (`bind_api`, `serve.rs:190-198`), off unless the flag is given, and its help text says "Reads are open to whoever reaches it: bind it where only operators do" (`crates/kbf-server/src/config.rs:167-169`). A bearer token sent to it crosses in cleartext, and a `Secure` cookie is never sent to it at all.
- `GET /v1/nodes`, `/metrics` and `/readyz` are open reads, no authenticator, on the `--api-listen` listener only (`metrics.rs:6`, `api.rs:66-72`, `docs/api.md:221`). No `farm.get` authority exists (`policy.rs` has `invocations_get` and `blobs_get`).
- Authentication policies are `allow`, `deny`, `any` and `all` only (`crates/kbf-auth/src/policy.rs:271-276`); none reads a credential. Without a policy file the server uses `Policy::allow_all`, and every caller's metadata is empty (`crates/kbf-server/src/config.rs:519-523`, `invocations.md:170-172`); `--invocations-open` fills in `allow` for the invocation and blob reads (`config.rs:534-538`).
- **Not built: read tokens.** `invocations-api.md` 4.9 (`:207-220`) specifies a `bearerToken` authentication policy and a `read` role refused by REAPI, landing with the client tokens of issue 227. **[F]** Neither exists: `git grep -i -E 'RefuseReadRole|bearerToken' crates` finds nothing (the only `bearer_token` is the alert webhook's, `crates/kbf-alert/src/webhook.rs:27`). Until they land, an agent on another machine reads only from a server whose policy allows everyone who reaches the listener, or through an authenticating proxy, and every "token" below is a prerequisite, not a fact.

**Proposed.**

- **A TLS read listener [D]-pending.** The read API and the pages are served on a listener separate from the loopback write gate, with TLS (`--read-listen` with `--read-tls-cert`, `--read-tls-key`), or behind the v2 LB's client TLS (`architecture-v2.md` 9, "Clients use TLS at the LB") with mTLS on the LB-to-frontend hop. It refuses to start without TLS unless `--read-listen-insecure` is given (for loopback or a trusted proxy), and the start line then warns. This is a prerequisite of read tokens and of the cookie option below (section 10 Q18). Phase U1; red test: `--read-listen` without TLS files and without `--read-listen-insecure` exits with a usage error, and a plain-HTTP request to a TLS read listener gets no page.
- **Tenancy [D]-pending, one of two.** (a) Key the AC by (instance, action digest), and then add the digest-in-record check below, which only then isolates instances. (b) Declare multi-tenant isolation out of scope for v1: one farm per trust domain, every instance on a farm trusts every other, and the docs say so. Recommend (b) for v1 (section 10 Q14): (a) changes the AC's key and storage sharding (`architecture-v2.md` 8), and a blob check over a shared AC is a check that cannot hold.
- **Instance scoping** (useful under either, as a view filter under (b) and a boundary under (a)). Every history row carries `instance` (5.2); the test key is `(instance, shape, cfg)`. Every list or aggregate route (`/v1/invocations`, `/v1/actions/{digest}/runs`, `/v1/runs/{id}`, `/v1/shapes*`, `/v1/tests`, `/v1/grid`, `/v1/events`) returns only rows whose instance the caller's `invocations.getAuthorizer` allows, asked as the invocation routes ask today (once per distinct instance per request). An invocation spanning instances stays all-must-allow, as built.
- **Open mode.** `--invocations-open` (and a policy whose `invocations.getAuthorizer` is `allow`) opens every new GET route that `invocations.get` gates to anyone who reaches the listener: `/v1/invocations`, `/v1/invocations/{id}/*` reads, `/v1/runs/{id}`, `/v1/actions/{digest}/runs`, `/v1/commands/{digest}`, `/v1/trees/{digest}`, `/v1/shapes*`, `/v1/tests`, `/v1/grid`, `/v1/events`, and, as built, `/v1/blobs`, minus the raw Command gate below. It never enables a write or `commands.getRaw`.
- **`farm.get`** **[D]-pending**: a new `kbf-auth` authority for `/v1/nodes/{id}`, `/samples`, `/v1/queue`, `/v1/leases`, `/v1/storage/map`, `/v1/series`, `/v1/health`, `/v1/events/stream`. Under it, a lease or queue entry shows node ids, sizes, ages and counts; its invocation id, action digest and instance appear only if the caller's `invocations.get` allows that instance. Whether the open `/v1/nodes`, `/metrics` and `/readyz` move behind it is section 10 Q11; it would break scrapers.
- **Raw Command gate (under (a) and (b)).** **[F]** Today nothing keeps the raw Command off `/v1/blobs`: `--invocations-open` fills in `allow` for `blobs_get` as well as `invocations_get` (`config.rs:534-537`), and the blob route serves any digest under any caller-chosen `?instance=` (`http_blobs.rs:5-10`). Every run envelope publishes `command.digest` (6.3), so under open mode `GET /v1/blobs/{command digest}` would return a Command with `TOKEN=x` in its env. Proposed, independent of tenancy: `/v1/blobs` and `/v1/trees` refuse (`403`) a digest that the recorder or history holds as an Action or Command digest (the live recorder's rows, `invocation.proto:125`; `digest_shape` by digest and by its `command_digest` column, 4.3) unless the caller has a separate `commands.getRaw` authority. `--invocations-open` and an `allow` policy never grant `commands.getRaw`; read tokens do not get it by default. Remaining gap: a Command the farm never recorded (an id-less call before U3, or a digest past its 30 days) is still readable by its digest.
- **Digest-in-record check** (only with tenancy (a)). `/v1/blobs` and `/v1/trees` check the digest against recorder and history rows (`run`, `output`, `pin`; not `hit`, whose result came from a shared AC under (b)) for an instance the caller is allowed on before serving.
- **Redaction is presentation, not a boundary.** Env values are never in a decoded command (decided in `invocations-api.md` flow (c)); operator redaction patterns apply to arguments, inline log tails and the shape catalog's program words (the key is computed before redaction, so keys stay stable). A program word holding a URL with userinfo is stored as scheme and host only: the shape normalizer keeps the argument after a bare program name as sent, so `curl https://user:token@host` would otherwise put the credential in `shape.program` (`crates/kbf-server/src/action_usage/shape.rs:43-59`, module docs on `program`). Full logs remain readable through `/v1/blobs`, and the raw Command through it with `commands.getRaw` only (the gate above, which `invocations-api.md:116-119` now names); the boundary is authority. Failure pins keep the redacted rendering, never the raw Command (4.2).
- **Purge** (O6) is a tombstone, because deleting one copy does not remove a blob in v2: reads fall back to replicas and previous-generation owners with read-repair (`architecture-v2.md:120`), handoff copies blobs (`:123`), the cold store holds append-only segments (`:127`, `:169`), daemon caches keep copies (`worker-cache.md`), AC entries name the digest, and history holds derived copies. `POST /v1/purge {digest}` on loopback with the write token, as cordon and drain are (`crates/kbf-server/src/api.rs:124-160`), does all of: (1) writes `purged/<digest>` in the state store, which every storage read, read-repair, handoff copy and closure check consults first, answering NotFound and never repairing; (2) deletes AC entries whose ActionResult references the digest; (3) tells daemons to evict it, carried in the map as the deny-list is (`architecture-v2.md` 9); (4) drops its pins; (5) deletes or re-renders derived rows that came from the purged Command or log (`command_view`, `shape.program` words, `test_case` messages, reason text) and marks run and output rows `available: false, purged: true`; (6) writes an `event` row. Steps (1) to (6) on v1 cover the single store; replicas, read-repair and handoff exist only from P5/P6 (`architecture-v2.md:166-167`), and the deny-list in the map from P3. Physical removal happens when compaction rewrites the segment holding the blob, within a bound the compaction design must state **[V]** (**[F]** no cold-store segment compaction exists today: `git grep -il compact crates` finds log compaction in `kbf-raft` and `kbf-store` snapshots and unrelated files, nothing over cold-store segments); until then the tombstone hides it. Red test (U5, single store): after a purge, `/v1/blobs/{digest}` answers `404` on the single frontend, the AC no longer returns a result naming the digest, and the run shows `purged`. Red test (U8, after P5/P6): killing the primary and reading through the replica or the previous-map owner still answers `404`, and no repair record is written.
- **Report writes.** `POST .../report` is off (`404 off`) unless the policy names an authenticator that yields a non-empty `public` principal (a per-caller authenticator: today none is built, 4.6). `--invocations-open` never enables it, and neither does `allow`. With one, it requires the owner match and, once roles exist, a non-read role, the 4.6 bounds and the per-owner quota. Red test (U4): with no policy file, `POST .../report` answers `404`.
- **Writes stay where they are**: cordon, drain, uncordon on loopback with a token; `POST /v1/idle` and `POST /v1/purge` the same.
- **UI pages.** Every page HTML-escapes every API string; a log, argv, JUnit case, target label, reason or file name is never rendered as markup. Every page answer sends `Content-Security-Policy: default-src 'none'; style-src 'self'; img-src 'self'; script-src 'self'; frame-ancestors 'none'` (no inline script) and `X-Content-Type-Options: nosniff`, as the blob route hardens itself today (`http_blobs.rs:14-18`). Red test (U6): a stderr of `<script>alert(1)</script>` renders as text.
- **How a human authenticates [D]-pending.** A browser cannot send `Authorization: Bearer` on page navigation. Either a reverse proxy injects the header (no farm change), or, once the TLS read listener exists (a `Secure` cookie is never sent over plain HTTP), a session cookie (`HttpOnly`, `Secure`, `SameSite=Strict`) is accepted on GET routes only, and every POST still requires the bearer header and refuses a request carrying an `Origin` header, as operator writes do (`api.rs:12-16`), so no cookie can make a POST. Recommend the proxy for v1 (section 10 Q16).
- **The UI is read-only** in v1; drain, cordon and purge stay CLI and API (PR #751's scope).
- Nothing in the history store leaves the farm.

## 9. Phased plan

Each phase lands a test that fails first. U1 and U2 need no state store and ship on today's single frontend; U3 to U6 need P1 (SQLite) and the single frontend only (U7 also needs preemption); U8 needs P5 and P6 (replicas, previous-map reads).

- **U0** This document.
- **U1 Per-action data on today's server.** Store shape, cfg, all nine REAPI timestamps, the full cgroup block, `memory_kill`, `booked`/`raised` bytes (booked and cap from `Answer.memory_runs` for every memory-killed attempt), structured `memory_used_bytes`/`memory_limit_bytes` from the daemon, farm-rerun budget, `wait_reason` and partition, and the daemon's reason text in the in-memory row; keep a timed-out run's log digests; `run_id` and `GET /v1/runs/{run_id}` with its envelope (6.3); `sort=` and the new filters on `/actions`; `top_slowest` and phase totals; `GET /v1/invocations/{id}/events` from the recorder; build `/failures` (PR 12) with the 6.3 attempt shape; the TLS read listener (8); the per-caller share of the blob read budget (6.1). Red: a fake-driver result whose `ResourceUsage.cgroup.memory_peak_bytes` is set shows that value at `GET .../actions/{digest}` (today dropped at `recorder.rs:524-539`); a timed-out action lists its stderr digest; an OOM-requeued action that ended `ok` is listed by `?requeue_reason=out_of_memory`; an action killed at the largest node shows non-null `booked_memory_bytes` and `cap_bytes` on its last attempt and no `logs` key; `GET /v1/runs/{id}` carries `envelope.invocation.href` and `envelope.command.href`; the built keys of an attempt are byte-identical before and after, and every new link in the answers is under `href` or `*_href`, root-relative, with blob-reading links `null` when the blob route is off; the read-listener and read-budget red tests of 8 and 6.1.
- **U2 Farm-state reads.** `Scheduler::snapshot()`; `GET /v1/nodes/{id}`, `/v1/queue`, `/v1/leases`; labeled `/metrics` families; export the two unexported counters; memory floor on the action answer. Red: in `kbf-sim`, three actions queued for a platform no node serves show `unservable: 3` with that reason; a node at full booking shows `free_cpus: 0`.
- **U3 History store** (after P1 provides `kbf-state` on SQLite). `history.db` (WAL, `synchronous=NORMAL`), run and hit records, shape catalog with target map, digest to shape, test outcomes with digest and commit, rollups for every shape, `last_cached_pass`, `run_tail`, `digest_seen`, day partitions, `instance` on every row, `GET /v1/invocations` list, `/v1/actions/{digest}/runs`, `/v1/runs?node=`, per-node failure counts, `/v1/shapes*`; durable invocation records; pins in storage (5.4) for failures and reports. Red: restart the frontend and `GET /v1/invocations/{id}` still answers; `--history-path` with a Postgres `--state-store` is refused; a blocked history writer does not delay a `Result` acknowledgement and increments `kbf_history_dropped_total`; 300 executions/s plus 2,700 hits/s for 10 minutes (so a rollup flush and several checkpoints land inside it) gives zero `kbf_history_dropped_total`, keeps the C7 commit-latency bound, WAL at most 8 MB/s and every checkpoint under 20 s (5.2); a digest served only by hits for 31 days keeps its `digest_shape` row and its hits keep counting toward its shape; a digest that failed then passed is listed `proven` flaky after its run records expire; a test that fails at commit A and is fixed at B by a revert served from the AC reports `status` not `failing` and `last_pass.commit == B`; three terminal own-limit kills give `status: oom_killed` and a `first_fail`; a non-test shape's `/v1/shapes/{key}/runs` lists its run records with node and digest; `/v1/runs?node=` over a node-pressure kill's times lists its co-tenant runs; a run that failed on one node and passed on another with the same digest is counted on the first node's "failed here, passed elsewhere"; the pin red test of 5.4; a caller allowed only on instance A gets zero rows of B from `/v1/shapes`, `/v1/actions/{d}/runs` and `/v1/events`; `/v1/shapes/{key}` without `instance=`/`cfg=` returns one row per (instance, cfg).
- **U4 Client report.** `POST .../report` and its reads, commit, targets, critical path, JUnit. Prerequisite: a per-caller authenticator (issue 227's tokens, or a proxy-supplied principal), without which the route is off (8). Red: a buck2 build report plus `what-ran` maps a test shape to its target, and `GET /v1/shapes?target=` finds it; first-failing commit is found across three reported invocations; with no policy file the POST is `404`; once read tokens land, a read token's POST is `403`; a report from a different owner leaves the first-failing commit unchanged; a failing outcome on branch X leaves the default branch's `first_fail` unchanged; a 33 MiB report is `413`; posting reports past the owner quota is `429` and leaves another instance's failed-run stderr pinned and `available: true` (pins from U3).
- **U5 Series, health, scoping of blobs.** Built-in series rollups, node samples and their route, `/v1/health`, `/v1/events`, `/v1/events/stream` (C5), purge on the single store, the raw Command gate, the blob digest check under tenancy (a); storage views as P3 to P6 land. Red: a fake clock across an hour with one minute whose claim waits are 60 s among minutes at 1 s gives an hourly p90 of at least 60 s, an hourly gauge max equal to the peak minute, and an hourly counter equal to the sum of its minutes; under tenancy (b), the docs state that an A-only caller can read B's results by digest (and under (a): an A-only caller's `GetActionResult` with B's action digest under instance A is a miss, and its `/v1/blobs` read of B's stderr is refused); with `--invocations-open`, `GET /v1/blobs/{command digest}` of a recorded Command whose env holds `TOKEN=x` answers `403`, and no route at all returns `x`; the single-store purge red test of 8; a run record 29 days old still has node samples for its times.
- **U6 UI pages**, after U3, on the single frontend; independent of P9/P10. Pages over the API only; the page encoding and CSP rules of 8. Red: the page contract test fails when a page reads a route its list does not name; a stderr of `<script>` renders as text and every page answer carries the CSP header. `architecture-v2.md` section 13 lists the UI under P10; this document proposes moving it (section 10 Q13). P10 keeps one task: proving the pages against Postgres and two frontends.
- **U7 Idle-host tests**, after preemption of `batch` is built. Red: in `kbf-sim`, a `ci` action arriving at a node with an idle lease starts within the preemption bound; a preempted idle run adds no outcome row; an instance not marked rerunnable yields no candidates.
- **U8 Purge across replicas**, after P5 and P6. The tombstone consulted by read-repair, handoff and previous-map reads; the deny-list in the map. Red: the replica red test of 8 Purge.

## 10. Open questions for the owner

Each has a recommendation; none is decided.

1. **History store home.** Separate `history.db` (own disk optional) / separate Postgres database, or tables in the state store? Recommend separate, with the fsync and WAL limits of 5.2, its backend following the state store's, and `architecture-v2.md:8` amended to name it as the second durable store (marked pending there in this commit).
2. **Durable invocation records.** Do history run records replace the chunks-and-pins stage 2 of `invocations.md` 3.5 (written for the Raft log v2 removes)? Recommend yes; one durable record.
3. **Commit and target source.** Client report upload, a per-call header, or both? Recommend the upload route first, the header if buck2 can send one from the environment [V].
4. **Test results inside outputs.** Should the server parse JUnit/`test.xml` outputs it sees, or only accept them in the client report? Recommend client report only in v1; the files are linked either way.
5. **Failure pins.** Pin failed runs' logs and result for 30 days within a 20 GiB budget shared per instance, held by a pin set the storage evictor and collector consult (5.4), streams over 16 MiB unpinned, the Command as a redacted rendering, client reports in their own 10 GiB, 7-day budget with a per-owner quota? Recommend yes.
6. **Retention tiers.** Accept 30 days for runs and hits, 400 days for invocation headers, test outcomes, rollups and the catalog (5.3)?
7. **`/v1/flakes`.** `failure-classes.md` 6.3 plans it; `/v1/shapes?kind=test&status=flaky` covers it with the proof digest included. Recommend one route and amend 6.3 (and its stale "bounded table" text).
8. **Arguments in the UI.** Show command arguments as sent with operator redaction patterns, env names only, redaction stated as presentation? Recommend yes.
9. **Metrics over time.** Built-in fixed rollup plus labeled `/metrics` for Prometheus, or Prometheus only? Recommend both; the owner's health view must not need an external system.
10. **Idle-host tests.** Wait for preemption, or ship earlier limited to tests under 60 s? Recommend wait.
11. **`farm.get` and the open routes.** Add `farm.get` for the new farm-state routes, granted to agent principals once issue 227's tokens exist (not built, 8)? Recommend yes. Move the open `/v1/nodes`, `/metrics`, `/readyz` behind it? Recommend no: they name no build, and moving them breaks scrapers.
12. **Id-less runs.** Should a run with no invocation id still write test outcomes (null invocation)? Recommend yes once U3 lands.
13. **UI phase.** Move the UI out of P10 to after U3 on the single frontend? Recommend yes.
14. **Tenancy.** The AC and CAS are shared across instances (8), so a blob check alone isolates nothing. (a) Key the AC by (instance, action digest) and add the digest-in-record check and `commands.getRaw`, or (b) one farm per trust domain in v1, documented? Recommend (b) for v1, (a) as a design of its own if a multi-tenant farm is wanted.
15. **Cached test outcomes.** Count cache hits in the daily rollup and as one `last_cached_pass` per key, not as outcome rows, with a cached pass counting as a pass for status and `last_pass`? Recommend yes: volume stays independent of the hit rate, and a fix served from cache is still seen (4.3).
16. **Human authentication for pages.** A reverse proxy that injects the bearer header, or GET-only session cookies? Recommend the proxy for v1; no cookie path until a POST can be proven cookie-proof (8).
17. **Branches in history.** Compute first-failing commit and flakiness per branch with a configured default branch (`main`)? Recommend yes; without it, one broken work-in-progress build poisons the default branch's history (4.3).
18. **Read listener.** Serve the read API and pages on a TLS listener separate from the loopback write gate (or behind the v2 LB's TLS), refusing plain HTTP unless an explicit insecure flag is given, as a prerequisite of tokens and cookies? Recommend yes, in U1 (8).
19. **Raw Command gate.** Refuse Action and Command digests on `/v1/blobs` and `/v1/trees` without `commands.getRaw`, under either tenancy option, never granted by open mode? Recommend yes (8).
