# kbf UI data: what the UI and its read API present

Status: design, nothing built beyond what is marked [F]. Data before visuals: this document says what the UI and its machine-readable read API must present, where each item comes from, how long it is kept and what it costs. It says nothing about layout. Labels as in `architecture-v2.md`: **[F]** fact checked in the code at `1117cdae` (this branch, `feature/arch-v2`) unless it says `main`, **[D]** owner decision, **[I]** inference, **[V]** verify before building on it. Sizes are arithmetic from stated rates, not measurements.

It builds on `invocations.md` and `invocations-api.md` (the per-build record and its read API) and on `architecture-v2.md` (frontend serves REAPI, scheduler and UI; SQLite/Postgres state store; sharded storage). It does not redo them; it lists what the UI adds.

## 0. Branch caveat and stale text

- **[F]** kbf#765 (the stable command-shape key, `cfg`, `outcome`, `lease`, `node` on the `action usage:` line, usage lines for failed runs, the key on `lease granted`) is merged on `main` (merge `8f4dcf1f`) and is **not** an ancestor of this branch's HEAD: `crates/kbf-server/src/action_usage.rs` does not exist here. At HEAD the line is keyed by `mnemonic` and `target` only and written only for a `Finished::Ran` result (`crates/kbf-server/src/farm/record.rs:100-107`, `:151-167`). Everything below that needs the shape key needs `main` merged into `feature/arch-v2` first (phase U1).
- **[F]** `invocations.md` section 9 says only PRs 1, 2, 3 and 5 have landed. The read API (PR 4) is in the code: `crates/kbf-server/src/http_invocations.rs:101-102`, routes at `crates/kbf-server/src/http_invocations/pages.rs:171-181`.
- **[F]** `invocations.md` section 1 and the `scheduler.md` Accounting text say no driver fills `ResourceUsage`. The container driver does (`crates/kbf-driver-container/src/cgroup_usage.rs:140-175`); the native driver does not (no `ResourceUsage` in `crates/kbf-driver-native/src`).
- **[F]** `failure-classes.md` section 9 says no metrics endpoint exists; `GET /metrics` exists (`crates/kbf-server/src/metrics.rs:24-40`).

## 1. Decisions this document proposes

None is decided; section 10 asks the owner.

- **One surface.** The UI reads only the JSON read API of section 6. Every page names the API calls it made. An agent therefore gets everything the UI shows, machine-readable, with the same authority.
- **One history store.** Per-run records, the shape catalog and test outcomes live in one append-mostly history store beside the state store (section 5), written after the state-store commit, never on the acknowledgement path. It also becomes the durable home of invocation records, replacing the Raft-based stage 2 of `invocations.md` 3.5 (that log is removed by v2).
- **Test identity is the command shape** (kbf#765), refined by `cfg` and by a target name when one is known. The farm never needs the client to say "this is a test".
- **The client may add what REAPI cannot carry** (commit, target names, critical path, test cases) through one upload route. Nothing in the farm requires it.

## 2. Users and the questions they ask

### 2.1 Questions

| # | User | Question | Answered by (section) |
|---|---|---|---|
| D1 | developer, agent | Did my build pass; which actions failed and why? | invocation summary, failures view (4.1, 4.2) |
| D2 | developer, agent | Was the failure my code or the farm? | failure class per attempt, infra reason text (4.2) |
| D3 | developer, agent | Full stderr/stdout of every failed action, not only the first | failures view with tails and log links (4.2) |
| D4 | developer, agent | The exact command, to reproduce it | Command decoded, env names only (4.2, 8) |
| D5 | developer, agent | Where did the time go: queue, input fetch, run, upload? | eight REAPI timestamps per attempt, lease phases (4.1, 4.2) |
| D6 | developer, agent | Why did it OOM, and how much memory does it need? | OOM marker, peak memory, ladder rung, memory floor per key (4.2) |
| D7 | developer, agent | Cache hit or not; same digest ran before? | `how` per action, runs of a digest across builds (4.1, 6) |
| D8 | developer, agent | Is this test flaky; did it fail before my change; since which commit? | test history by shape (4.3) |
| D9 | developer, agent | Is it getting slower? | duration trend by shape (4.3) |
| D10 | developer, agent | What is on the critical path? | client report only (4.1) |
| O1 | operator | Which nodes are up, serving, cordoned, under memory pressure? | node view (4.4) |
| O2 | operator | What is queued, for which platform, and why is it not placed? | queue view (4.4) |
| O3 | operator | Which leases are open, where, how old? | lease view (4.4) |
| O4 | operator | Is storage healthy: map, under-replicated, lost blobs, flush lag? | storage view (4.4) |
| O5 | operator | Which node fails actions that pass elsewhere? | per-node failure rates from run records (4.3, 4.5) |
| W1 | owner | Is the farm healthy right now? | health summary (4.4) |
| W2 | owner | Load, cache hit rate, queue wait and failure rate over weeks | series (4.5) |
| W3 | owner | How much idle capacity is there, and what does it do? | idle capacity series, idle-test results (7) |

### 2.2 What agents worked out by hand

**[I]** From agents debugging builds on this farm (generic shapes only): they could not tell a farm fault from an action failure without the server log (D2); they read the daemon journal on a node to find why the farm could not run an action, because the reason reaches only a server WARN line (`crates/kbf-server/src/farm.rs:606-614`); they found that the client's reported duration includes farm queue time and took phase times from the daemon's `lease phases:` line instead (D5); a container's OOM flag was false for an exit-137 kill, so they counted kernel memcg kills (D6); memory-ladder rungs looked like failures; they reran single tests by hand to judge flakiness (D8); they had no read token for `/metrics` (O1). Every one of these becomes one API call below.

## 3. Entities, identity and links

| Entity | Id | Source | Links to |
|---|---|---|---|
| Invocation | client `tool_invocation_id`, else minted `kbf-<uuid>` | RequestMetadata (`crates/kbf-front/src/request_metadata.rs:103-119`) | actions; client report; commit |
| Action | action digest (sha256-size) | REAPI | runs, command, shape, result |
| Operation | REAPI operation name | scheduler | attempts; gone 60 s after it ends (`crates/kbf-sched/src/scheduler.rs:53`) |
| Attempt (lease) | `(term, seq)`, v2 `(epoch, seq)` | scheduler | node, run record |
| Run record | `(invocation, action digest, lease)` | new (5.2) | all per-attempt data |
| Shape | 16-hex key, kbf#765 (`main:crates/kbf-server/src/action_usage/shape.rs:61-146`) | computed from the Command at Execute | `cfg` variants, targets seen, test history |
| Test target | `(shape, cfg)`; plus target label when known | shape + client | outcomes over time |
| Node | node id from `Hello` | daemon | leases, samples |
| Blob | digest | storage | `available` flag, retention |
| Commit | client-supplied string | client report or header (4.6) | invocations |

- **[F]** buck2 sends only `tool_invocation_id` (its Build ID) and `tool_details` (with a fixed version); `action_id`, `action_mnemonic`, `target_id`, `configuration_id`, `correlated_invocations_id` are empty (`crates/kbf-front/tests/request_metadata.rs:33-63`, golden `crates/kbf-front/tests/data/buck2-execute.requestmetadata`). Bazel fills them all, read from its source only, not captured on the wire **[V]**.
- **[F]** Shape key = first 8 bytes of SHA-256 over a version tag, the normalized program words (up to 3) and the sorted, deduplicated normalized output patterns; RequestMetadata never enters it, and `cfg` (the config hash taken out of output paths) is not part of it (`main:crates/kbf-server/src/action_usage/shape.rs:61-124`).
- **[F]** Known limit: a buck2 remote test's outputs sit under `buck-out/v2/test/execution/<cell>/<hash>/<hash>/`, the first hash covering target and configuration, the second the test-case set. Such an action keys once per target, configuration and test-case set, with `cfg=-` (`main:.../shape.rs:35-42`). **[I]** Good for history (stable across builds); it cannot name the target, and a changed test-case set starts a new key.
- **[F]** The key-to-shape mapping exists today only in `action shape:` log lines, deduplicated in memory and cleared at 65,536 keys (`main:crates/kbf-server/src/action_usage.rs:59-64`, `:100-145`). It must become rows (5.2).
- **[I]** A cache hit carries no Command (buck2 uploads no Action for a hit), so its shape comes from a `digest -> shape` row written when that digest last executed (5.2).

## 4. Items: fields, source, retention, API

Each subsection: fields, source today **[F]** or what to add, retention and size, the route (section 6 lists all routes; "built" means it exists at HEAD).

Rate used for sizes **[F]**: the pilot rate of about 4 operations a second (`invocations.md` 3.3, `docs/design/invocations.md:246-251`), so about 345,600 executions a day. **[I]** Real days are bursty (the architecture budget, `architecture-v2.md` 4.1 C7, is 9,000 actions in 2 h); the daily figure is a ceiling for sizing.

### 4.1 Per build (invocation)

| Field | Source today | To add |
|---|---|---|
| tool, tool version, first/last seen, counts (execute, cache hit, joined, ran, failed, AC hit/miss, blobs asked/missing, bytes up/down) | **[F]** in-memory record header (`crates/kbf-invocation/proto/kbf/invocation/v1/invocation.proto:29-79`) | durable (5) |
| actions with `how` (cache_hit, cache_hit_execute, joined, ran), outcome | **[F]** `invocation.proto:108-132` | shape, cfg per row |
| queue and run histograms, reruns by reason, CPU and peak-memory sum/max | **[F]** computed per request (`crates/kbf-invocation/src/metrics.rs:100-137`) | phase histograms (fetch, run, upload) from 4.2 |
| which node ran what | **[F]** `Attempt.worker` (`invocation.proto:135-149`); server overwrites `ExecutedActionMetadata.worker` with the node id (`crates/kbf-server/src/stamp.rs:24-36`) | per-node counts in the summary |
| cache hit rate | **[F]** counts above | none |
| commit, branch, user-visible target names | none: no REAPI field carries them | client report (4.6) |
| critical path | none: REAPI has no dependency edges **[F]** (the remote-execution proto); the farm can order by time only | client report: buck2 `log critical-path --format json`, Bazel profile/BES later **[V]** |

- **[F]** Records are in memory only, bounded by `--invocation-memory` (default 256 MiB, `crates/kbf-front/src/recorder.rs:42`), lost on restart; calls with no id are not recorded (`crates/kbf-server/src/farm/record.rs:108-110`).
- Size **[F]**: 444 B average row measured (`docs/design/invocations.md:222-253`); about 150 MB/day at 4 ops/s; 30 days about 4.5 GB.
- Routes: `GET /v1/invocations/{id}` and `/metrics` (built); `GET /v1/invocations` list (new, needs 5); `GET /v1/invocations/{id}/critical-path` (new, 4.6).

### 4.2 Per action and per attempt

| Field | Where it is today | To add |
|---|---|---|
| exit code, stdout/stderr digests, outputs (max 1,000), result digest | **[F]** row listing (`crates/kbf-front/src/recorder.rs:505-532`) | timed-out run: keep the partial result's log digests (today dropped, `farm/record.rs:111-114`, `crates/kbf-server/src/timeout.rs:1-37`) |
| exact command | **[F]** only the Command digest (`invocation.proto:121`); contents by fetching the blob | decoded view, env names only (8) |
| input root digest, input bytes/files | not recorded | copy `Action.input_root_digest` and the walk's counts |
| queue, granted, started, ended per attempt | **[F]** `invocation.proto:135-149` | none |
| input fetch, execution, output upload start/end | **[F]** all three runtimes set them in `ExecutedActionMetadata` (`crates/kbf-driver-native/src/runtime.rs:561-610`, `crates/kbf-daemon/src/local.rs:94-171`, `crates/kbf-driver-container/src/runtime.rs:528`); stored inside the ActionResult blob only | copy all eight REAPI timestamps into the run record |
| `lease phases:` (lease_ms, inputs_ms, run_ms, outputs_ms, output files/dirs, `oom=1`) | **[F]** daemon log line only (`crates/kbf-daemon/src/phases.rs:150-172`) | derive from the copied timestamps; carry `oom` and counts in the report |
| cgroup: cpu usage/user/system, periods, throttled, throttled time, memory peak, NUMA bytes per node | **[F]** in `ResourceUsage.cgroup` inside `auxiliary_metadata` (`crates/kbf-proto/proto/kbf/worker/v1/worker.proto:436-480`); the row keeps only 4 legacy fields (`crates/kbf-front/src/recorder.rs:535-552`) | keep all of it in the run record; derive parallelism = cpu_usage / wall and numa_spread |
| NUMA node the lease was pinned to | **[F]** container driver `inputs phases:` line only, with `--lease-placement=numa-node` (`crates/kbf-driver-container/src/input_phases.rs:1-80`) | carry in `ResourceUsage` |
| OOM marker | **[F]** `worker::Result.memory_kill` (`crates/kbf-daemon/src/lease.rs:290-296`), never in the ActionResult; surfaces as requeue reason `OUT_OF_MEMORY` with booked/raised bytes (`invocation.proto:166-206`) | `memory_kill` in the run record |
| retries | **[F]** attempts with requeue reason or failure class plus reason (≤1 KiB) (`crates/kbf-invocation/src/record.rs:121-180`) | farm-rerun budget and ladder rung per attempt (today log-only: `farm.rs:890-929`) |
| infra reason text | **[F]** in the record for a failure that ends the operation (`farm/record.rs:115-118`); a requeue keeps only its reason enum | the daemon's text for every attempt |
| transfer stats (reads, bytes, find-missing, outputs present, writes, retries) | **[F]** daemon `transfers:` line only; the code calls API carriage "a later stage" (`crates/kbf-transfer/src/stats.rs:1-6`) | carry in `auxiliary_metadata` (`worker-cache.md` plans it) |
| input sub-phases (image, walk, links, chown, cgroup setup) | **[F]** container `inputs phases:` line only | optional; carry as a map |
| server time before the offer (missing-inputs walk) | **[F]** `kbf_execute_walk_seconds`, unlabeled, aggregate only (`crates/kbf-front/src/metrics/execute.rs:101-147`) | per-attempt `presubmit_us` |
| test logs and test result files | **[F]** no parser anywhere; `failure-classes.md:63-67` leaves test failure vs error to the client | client report (4.6); server parsing deferred (9) |
| live stdout of a running action | none: the daemon uploads stdout at the end **[I]** | out of scope |

**Logs and their lifetime.** **[F]** stdout/stderr always go to the CAS by digest, capped at 1 GiB per stream (`crates/kbf-outputs/src/limits.rs:19-35`). Read by `GET /v1/blobs/{digest}` with one Range or `?inline=text` up to 4 MiB (`crates/kbf-server/src/http_blobs.rs:15-25`, `:155`). Retention policy is 7 days after last touch plus a 1-day quantum (`crates/kbf-meta/src/model.rs:178-202`), but the server never runs collection (no caller of `Cache::collect` in `crates/kbf-server/src`), so on S3/MinIO a log is kept forever, and on a `file://` cold store the evictor can drop it 60 s after last use under disk pressure (`crates/kbf-front/src/cache/evict.rs:30-33`). **[F]** A failed run's ActionResult is in the CAS but not in the AC (only exit 0 is cached, `crates/kbf-server/src/farm.rs:1117`), so nothing indexes it once the record is gone. The run record is that index.

**Pins for failures.** Proposal: pin a failed or timed-out run's stdout, stderr, ActionResult and Command for 30 days within a byte budget. Arithmetic **[I]**: at a 1% failure rate, 3,456 failed runs/day; at 64 KiB of logs each, 221 MB/day, 6.6 GB for 30 days; the budget (default 20 GiB, oldest pin dropped first) caps the tail risk of large logs. Failure rate and log size are **[V]**.

**Run record size [I].** Ids and digests (invocation ref 8, action 40, result 40, command 40, input root 40, stdout 40, stderr 40) 248 B; eight timestamps 64 B; shape 8, cfg 8, node ref 4, lease 16, exit code, outcome, memory kill 8; cgroup 10 counters 80 B plus NUMA (2 nodes) 16 B; transfer 6 counters 48 B; about 500 B, about 650 B with SQLite row and index overhead. At 345,600/day: 225 MB/day; 30 days 6.7 GB. Outputs stay in the existing listing (44 B plus path each), so the record replaces the 444 B row rather than adding to it **[I]**: about 1 KB/run total, 10 GB at 30 days.

Routes: `GET /v1/invocations/{id}/actions` and `/actions/{digest}` (built; extended with the fields above); `GET /v1/invocations/{id}/failures` (planned PR 12, `invocations-api.md`); `GET /v1/actions/{digest}/runs` (new: every run of a digest across builds); `GET /v1/commands/{digest}` (new); `GET /v1/blobs/{digest}` (built).

### 4.3 Test history

**What a test is to the farm [I].** An action whose shape marks it a test: a buck2 output under `buck-out/v2/test/execution/`, or a Bazel `action_mnemonic` of `TestRunner` with a `target_id`. Its key is `(shape, cfg)`; `target` is attached when Bazel names it or a client report maps the key.

**Outcome per run** (separate infra from test, as ResultDB and TestGrid do):

| Status | Rule |
|---|---|
| `passed` | ran, exit 0 |
| `failed` | ran, non-zero exit (Action class) |
| `timed_out` | Timeout class |
| `infra` | Infra, lost, node pressure, missing input; excluded from flakiness and first-failing math |
| `oom_rung` | an own-limit memory kill followed by a ladder rung; not a test result |
| `cached` | AC hit on a digest whose last run passed; counts as a pass, flagged |
| `preempted` | an idle-band run killed for real work (7); not a test result |

**Derived per `(shape, cfg)`:**

- **Flaky.** `failure-classes.md` 6.3 (decided) keeps the digest rule: one action digest seen both passing and failing is flaky, a lower bound because a pass is cached. History adds a shape-level score: pass/fail transitions over the last N non-infra runs, ordered by commit when known, else by time (the Buildkite transition count). Both are shown; the digest rule is the proof, the transition score the hint.
- **First failing commit.** The earliest commit after the last pass in the ordered history; without commits, the first failing invocation and its time. **[F]** No commit is in any REAPI field; it needs 4.6.
- **Duration trend.** p50/p90 of execution time (from the copied timestamps, not wall incl. queue) per day.

**Source today.** **[F]** None: the record is in memory, cross-build queries are out of the invocations plan (`docs/design/invocations.md:444-453`), and no flake table or `/v1/flakes` route exists (grep `flake` finds tests only).

**Sizes [I].**

| Table | Row | Volume | Retention | Size |
|---|---|---|---|---|
| test outcome | shape 8, cfg 8, invocation ref 8, time 8, run ref 8, status 1, duration 4, node 2: about 50 B, 100 B indexed | assume 20% of executions are tests: 69,000/day | 400 days | 6.9 MB/day, 2.8 GB |
| daily rollup per `(shape, cfg)` | runs, passes, fails, infra, transitions, p50, p90, first/last seen: 64 B | assume 20,000 test keys | 2 years | 1.3 MB/day, 0.9 GB |
| shape catalog | key, program words, first 8 patterns, cfg list, targets seen, first/last seen: about 500 B | bounded by distinct keys; the in-memory set clears at 65,536 | while seen in 400 days | 33 MB at 65,536 keys |
| digest to shape | digest 40, shape 8, cfg 8: 120 B indexed | one per executed distinct digest | 30 days after last seen | under 1.3 GB at 345,600/day |

Cached hits recorded as outcomes multiply the outcome row count by the hit rate; still under 10 GB a year at a 3x factor. **[I]** Per-action rows dominate; multi-year test history is cheap.

Routes (new): `GET /v1/tests?status=failing|flaky|slower&since=` (list), `GET /v1/tests/{shape}` (catalog, cfgs, targets, current status, score), `GET /v1/tests/{shape}/runs?cfg=&since=` (outcomes with run links), `GET /v1/tests/{shape}/daily`, `GET /v1/grid?tests=...&columns=commit|invocation` (6.4).

### 4.4 Farm state

| Field | Source today | To add |
|---|---|---|
| node id, connected, software, needs-attention, placement (serving, cordoned, draining, drained, paused), phase | **[F]** `GET /v1/nodes` (`NodeView`, route at `crates/kbf-server/src/api.rs:62-63`) | none; `docs/api.md` example omits the `phase` field the code sends |
| capacity (CPUs, memory, GPUs), booked, free, lease count, whole-machine hold, last heard, memory-pressure kills | **[F]** in `Scheduler::Worker`, `pub(crate)` (`crates/kbf-sched/src/scheduler.rs:114-147`); no accessor | public `Scheduler::snapshot()`; `GET /v1/nodes/{id}`; v2: `session/` and `member/` rows |
| NUMA layout per node | **[F]** daemon `numa layout:` log line only (`crates/kbf-daemon/src/numa.rs:104`) | in `Hello` capabilities |
| queue depth | **[F]** one unlabeled gauge `kbf_sched_queued` (`crates/kbf-front/src/metrics.rs:635`); one global queue (`scheduler.rs:234`); per-op `waiting(op)` (`scheduler.rs:299-445`) | depth and oldest age by platform, size class, QoS; unservable counts by reason; v2: queue partitions (`architecture-v2.md` 4.2) |
| leases | **[F]** `kbf_sched_leases` gauge; lease ids only in drain views | `GET /v1/leases` |
| storage map, member states, under-replicated, handoff backlog, repair records, lost blobs | none in code: only in `architecture-v2.md` 6, 8 | with P3 to P6 |
| hot-store fill, flush lag | **[F]** `kbf_write_tier_*` with `--hot-store` (`crates/kbf-front/src/metrics.rs:701-797`) | none |
| health | **[F]** `/healthz`, `/readyz` with `stopping`, `leader`, `store` (`crates/kbf-server/src/health.rs:142-208`) | v2 meaning: state store reachable, map loaded; per-node health |
| alerts, operator actions | **[F]** `kbf-alert` exists unwired (no dependent crate); attention items logged only (`farm.rs:418-423`); cordon/drain logged (`farm.rs:473`) | event rows (5.2) |

- **[F]** All of it is in memory under one `Mutex<State>` and lost on restart, cordons included (`crates/kbf-server/src/farm.rs:117`, `:184-203`). In v2 the state store holds it; reads are prefix scans plus the change feed (C5).
- Retention: current values only, except the samples of 4.5 and the event rows (400 days; about 1,000 events/day at 200 B is 73 MB/year **[I]**).
- Routes: `GET /v1/nodes` (built), `GET /v1/nodes/{id}`, `GET /v1/queue`, `GET /v1/leases?node=`, `GET /v1/storage/map`, `GET /v1/health`, `GET /v1/events?since=` (new).

### 4.5 Metrics over time

- **[F]** `/metrics` resets at process start; only `kbf_meta_commit_seconds` has a label (`crates/kbf-front/src/metrics.rs:649-657`); no daemon serves metrics; no scrape or time-series config exists in the repo. Two counters are kept but exported nowhere: `kbf_invocation_metadata_invalid` and `kbf_invocation_events_dropped` (`crates/kbf-front/src/request_metadata.rs:232`, `crates/kbf-front/src/recorder.rs:190`).
- **Two paths, one source.** Labeled families on `/metrics` (by platform, QoS, node, outcome class) for an operator's Prometheus; and a small built-in rollup of a fixed series set in the history store, so the owner's health view works with no external system.
- **Fixed series set** (from `architecture-v2.md` 11 plus the questions of 2.1): queue depth and oldest age by partition; claim wait p50/p90; leases; idle CPUs and memory per node; busy CPU fraction; executions, cache hits, failures by class, farm reruns, OOM rungs per minute; hot-store fill; flush lag; under-replicated and lost blobs; group-commit latency; history writer drops.
- **Sizes [I].** Built-in: 40 series plus 8 nodes x 6 per-node series = 88 series, 16 B a point: 1-minute points for 14 days = 88 x 1,440 x 14 x 16 B = 28 MB; hourly for 400 days = 88 x 24 x 400 x 16 B = 13.5 MB. Node snapshots (about 0.5 KiB as JSON, 20 numeric fields) every 15 s for 2 days: 8 x 5,760 x 2 x 0.5 KiB = 45 MiB. An external Prometheus over about 1,000 series at 15 s and about 1.5 B a sample stores about 8.6 MiB/day.
- Route: `GET /v1/series/{name}?from=&to=&step=&labels=` (new); `GET /v1/series` lists names, units and retention.

### 4.6 Client report (what REAPI cannot carry)

- Route: `POST /v1/invocations/{id}/report`, body one of: a buck2 build report (`--build-report`), buck2 `log what-ran --format json`, buck2 `log critical-path --format json`, a JUnit XML file per test, and a small JSON of `{commit, branch, repo}`. Bazel BES is later.
- Gives: commit (first failing commit), target names per shape (joined by the action digest or output path in `what-ran` **[V]**), critical path, test-case results.
- Alternative for the commit alone: a header the client sends on every call. **[V]** whether buck2's RE client config can add a header from the environment.
- Size **[I]**: reports are stored as CAS blobs pinned with the invocation; a 50,000-action `what-ran` at about 300 B a line is 15 MB, which is why only the joins (target per shape, commit) are copied into rows.

## 5. Storage of history

### 5.1 Recorded at the time, or derivable

| Data | Recorded at the time | Derivable later |
|---|---|---|
| run record (attempts, timestamps, usage, class, node) | yes: the operation is dropped 60 s after it ends | from the ActionResult blob only while it lives, and only for runs that reported |
| shape, cfg | yes: the Command is not uploaded for a hit and is dropped after the key is made | no |
| digest to shape | yes | no |
| test outcome rows | from run records | yes, rebuildable from run records within their 30 days |
| daily rollups | from outcome rows | yes, from outcome rows within 400 days |
| series rollups | yes: gauges are instantaneous | no |
| invocation summary | from run records | yes |

### 5.2 Where it lives

- **Proposal: a history store separate from the state store.** On SQLite, a second file `history.db` beside the state store, its own writer thread and lock; on Postgres, a separate schema. Reason: the state store's commit budget (`architecture-v2.md` 4.1 C7) and its single SQLite writer are on every acknowledgement; 10 GB of history in the same file would lengthen checkpoints and backups of the queue.
- **Written after commit.** The frontend enqueues a run event after the `Result` commit; a bounded queue drains into batched inserts. A full queue drops and counts (`kbf_history_dropped_total`). History never delays a build; losing it loses rows, never results.
- **Several frontends (P9/P10).** Each frontend writes the runs it committed; Postgres holds one table set. On SQLite there is only one frontend.
- **Replaces `invocations.md` 3.5 stage 2.** That design keeps chunks in the CAS with a pin per invocation on the Raft metadata log, which v2 removes. Run records indexed by invocation answer `GET /v1/invocations/{id}/actions` directly; there is no second durable record. The in-memory recorder stays as the live view of builds in flight.
- **Tables:** `invocation`, `run`, `output` (the listing), `shape`, `digest_shape`, `test_outcome`, `test_daily`, `series_minute`, `series_hour`, `node_sample`, `event`, `pin`.

### 5.3 Retention tiers

| Tier | Kept | Size at 4 ops/s **[I]** |
|---|---|---|
| run records and listings | 30 days (`--history-runs-days`) | about 10 GB |
| failure pins (logs, result, command) | 30 days within 20 GiB | at most 20 GiB, in storage |
| test outcomes | 400 days | about 2.8 GB |
| daily rollups, shape catalog | 2 years | about 1 GB |
| series | 1 min 14 days, 1 h 400 days | about 42 MB |
| node samples | 15 s for 2 days | about 45 MiB |
| events | 400 days | about 73 MB |

Total under 15 GB of history plus the pin budget. Deletion is by day partition (a table per day on SQLite, or a time index with batched deletes **[V]** which is cheaper on the writer). Rows outlive the blobs they point to and show `available: false`, as `invocations.md` 3.6 already says.

## 6. Read API

### 6.1 Rules

- **The UI uses only this API.** A page is server-rendered from these JSON documents (PR #751's direction: embedded assets, no client toolchain **[I]**). Each page shows the URLs it read. A contract test lists, per page, the API calls it makes and fails if a page reads anything else.
- **One link form.** Every response carries absolute links; `kbf build show <link>` (`invocations-api.md` 4.8) reads any link, so the CLI and the UI see the same documents.
- **Index.** `GET /v1` returns every route with its parameters, units, retention and authority; `GET /llms.txt` is the same as plain text for an agent with no prior knowledge.
- **Paging** as built: `page_size` (default 100, max 1000) and opaque `page_token` (`crates/kbf-server/src/http_invocations/pages.rs:43`). Filters are query parameters; lists are newest first unless stated.
- **Units in names**: `_us`, `_ms`, `_bytes`, `_unix_ms`; absent means not measured, never zero.
- **Gone data says so.** `available: false` for a blob; `410` with the retention that expired it for a record past its tier.
- **Live updates.** `GET /v1/invocations/{id}/events` and `GET /v1/events/stream` as server-sent events resuming by revision (`Last-Event-ID`), fed by the state store's change feed (C5). A poller reading by revision gets the same rows.

### 6.2 Routes

| Route | Status | Answers |
|---|---|---|
| `GET /v1` , `GET /llms.txt` | new | index |
| `GET /v1/invocations?since=&tool=&commit=&state=` | new (5) | D1 list |
| `GET /v1/invocations/{id}` | built | D1, D7 |
| `GET /v1/invocations/{id}/actions?outcome=&how=` | built, fields extended | D1, D5, D7 |
| `GET /v1/invocations/{id}/actions/{digest}` | built, fields extended | D2 to D6 |
| `GET /v1/invocations/{id}/failures` | planned PR 12 | D1 to D4 in one call |
| `GET /v1/invocations/{id}/artifacts`, `/logs`, `/metrics` | built | D3, D5 |
| `GET /v1/invocations/{id}/critical-path` | new (4.6) | D10 |
| `POST /v1/invocations/{id}/report` | new (4.6) | commit, targets, tests |
| `GET /v1/invocations/{id}/events` | new | live |
| `GET /v1/actions/{digest}/runs` | new | D7, flake proof |
| `GET /v1/commands/{digest}` | new | D4 |
| `GET /v1/blobs/{digest}` | built | logs, outputs |
| `GET /v1/tests`, `/v1/tests/{shape}`, `/runs`, `/daily` | new | D8, D9 |
| `GET /v1/grid` | new | TestGrid (6.4) |
| `GET /v1/nodes`, `GET /v1/nodes/{id}` | built, new | O1 |
| `GET /v1/queue`, `GET /v1/leases` | new | O2, O3 |
| `GET /v1/storage/map` | new (P3) | O4 |
| `GET /v1/health` | new | W1 |
| `GET /v1/series`, `/v1/series/{name}` | new | W2 |
| `GET /v1/events` | new | O5, audit |
| `GET /v1/idle`, `POST /v1/idle` | future (7) | W3 |

### 6.3 Shapes (abridged)

A run, as `actions/{digest}` and `failures` return each attempt:

```json
{"lease": "3:4812", "node": "n4", "end": {"failed": "infra", "reason": "..."},
 "class": "infra", "exit_code": null, "memory_kill": "none", "ladder_rung_bytes": null,
 "farm_runs": 1, "farm_budget": 2,
 "times_unix_ms": {"queued": 0, "granted": 0, "worker_start": 0,
   "input_fetch_start": 0, "input_fetch_completed": 0, "execution_start": 0,
   "execution_completed": 0, "output_upload_start": 0, "output_upload_completed": 0,
   "worker_completed": 0},
 "presubmit_us": 0,
 "usage": {"cpu_usage_us": 0, "cpu_user_us": 0, "cpu_system_us": 0, "cpu_periods": 0,
   "cpu_throttled": 0, "throttled_us": 0, "memory_peak_bytes": 0, "parallelism": 0.0,
   "numa_node": null, "numa_bytes": [{"node": 0, "bytes": 0}], "numa_spread": 0.0},
 "transfers": {"read_bytes": 0, "written_bytes": 0, "find_missing_digests": 0,
   "outputs_present": 0, "retries": 0},
 "logs": {"stdout": {"digest": "...", "available": true, "kept_until_unix_ms": 0,
   "tail": "...", "link": "..."}, "stderr": {}},
 "shape": {"key": "0123456789abcdef", "cfg": "-", "link": "..."}}
```

`kept_until_unix_ms` is the pin's end or, with no pin, absent: an unpinned blob's lifetime depends on touches and disk pressure and cannot be promised (4.2).

A test row: `{shape, cfg, target, status, flaky: {digest_proof: bool, transitions: n, window: n}, last_pass: {commit, invocation, at}, first_fail: {commit, invocation, at}, duration_ms: {p50, p90, trend_7d_pct}, runs_link}`.

A grid (TestGrid model): `{columns: [{id, commit, started_unix_ms}], rows: [{shape, cfg, target, cells: [[status, count], ...]}]}` with run-length encoded cells, so a green row costs one pair.

### 6.4 TestGrid view (future)

Rows are `(shape, cfg)` filtered by target prefix, program or status; columns are commits when reports supply them, else invocations; a cell links to `runs`. The data is the outcome table of 4.3; nothing new is recorded. Default width 50 columns.

## 7. Idle-host test scheduling (future)

Purpose: confirm flakes, refresh duration baselines and recheck tests not run lately, on capacity real builds are not using. It reruns recorded actions; it cannot test new code.

**Data.**

- Idle capacity per node: capacity minus booked CPUs and memory (needs `Scheduler::snapshot()`, 4.4), as a 1-minute series (4.5).
- Idle time per node: how long the node has had no lease from a real QoS level.
- Candidates: test keys from 4.3 whose last executed digest's closure (Action, Command, input root) is still in storage; checked by FindMissing before submit.

**Controls.**

- **A band below real work.** **[F]** QoS levels are `interactive`, `ci`, `batch` (preemptible) and custom levels; every level at or below `batch` is preemptible (`crates/kbf-types/src/qos.rs:19-28`, `docs/design/scheduler.md:254-275`). **[F]** Today every action is submitted at `ci`, and preemption of `batch` is planned, not built (`scheduler.md:271-275`). An `idle` custom level below `batch`: never preempts, is always preempted, and is admitted only to a node with no queued real work for its partition and idle for a dwell (default 5 min).
- **No preemption, no band.** Until preemption is built, idle runs could delay real work by a whole run. Gate: the band ships only after preemption, or with candidates limited to p90 run time under 60 s, which bounds the delay.
- **Budget**: CPU-hours a day, idle leases per node (default 1), and CPUs left free per node (default 2).
- **Selection** (ordered): digest-level possible flakes (`failure-classes.md` 6.3) to confirm; shape-level transition score above a threshold; test keys not run for 7 days; keys whose duration trend rose. Each idle run uses `skip_cache_lookup` and `do_not_cache`, so it never changes the action cache.
- **Results** go into history with `source: idle`; a `preempted` run is not an outcome. A pass after a fail on the same digest is the flake proof of 6.3 case 2.
- **Off by default**, enabled by flag; `POST /v1/idle` (operator authority) changes budget and pauses; `GET /v1/idle` shows budget, spend, running idle leases, preemptions and results.

**Sizes [I].** Bounded by the budget: 100 CPU-hours a day of 30 s tests is 12,000 runs a day, 12 MB of run records, about 5% of the daily volume.

**Prior art.** Borg's free band in reclaimed capacity, lowest priority, killed first; Kubernetes `preemptionPolicy: Never` for the never-preempts half.

## 8. Access control and secrets

- **[F]** The invocation routes are off (404 `off`) unless the policy names `invocations.getAuthorizer` or `--invocations-open` is given (`crates/kbf-server/src/serve.rs:314-324`). Every new read route of section 6 takes an authority: `invocations.get` for builds, runs, tests, blobs; `farm.get` for nodes, queue, leases, map, series, events, idle.
- **Agents get read tokens** (`invocations-api.md` 4.9, PR 14) for both authorities. An agent that cannot read `/metrics` cannot answer O1.
- **Writes stay where they are**: cordon, drain, uncordon on loopback with a token (`crates/kbf-server/src/api.rs:136-194`); `POST /v1/idle` the same. `POST .../report` takes the invocation's own authority (an id is a claim, not a secret: `invocations.md` 2.4).
- **Command contents.** Environment variable names, never values (decided in `invocations-api.md` flow (c)). Arguments are shown as sent; an operator-configured redaction pattern list applies to arguments and to inline log tails. Logs are shown as the action wrote them; the CSP-sandbox and `nosniff` headers stay on blob reads (`crates/kbf-server/src/http_blobs.rs`).
- **The UI is read-only** in v1; drain and cordon stay CLI and API (PR #751's scope).
- Nothing in the history store leaves the farm; the client report is stored as the client sent it, under the invocation's authority.

## 9. Phased plan

Each phase lands a test that fails first. Phases U1 and U2 need no state store and ship on today's single frontend.

- **U0** This document.
- **U1 Per-action data on today's server.** Merge `main` (kbf#765) into `feature/arch-v2`; store shape, cfg, all eight timestamps, the full cgroup block, `memory_kill`, ladder rung and the daemon's reason text in the in-memory row; keep a timed-out run's log digests; build `/failures` (PR 12). Red: a fake-driver result whose `ResourceUsage.cgroup.memory_peak_bytes` is set shows that value at `GET .../actions/{digest}` (today dropped at `recorder.rs:535-552`); a timed-out action lists its stderr digest.
- **U2 Farm-state reads.** `Scheduler::snapshot()`; `GET /v1/nodes/{id}`, `/v1/queue`, `/v1/leases`; labeled `/metrics` families; export the two unexported counters. Red: in `kbf-sim`, three actions queued for a platform no node serves show `unservable: 3` with that reason; a node at full booking shows `free_cpus: 0`.
- **U3 History store** (after P1 provides `kbf-state` on SQLite). `history.db`, run records, shape catalog, digest to shape, test outcomes, rollups, `GET /v1/invocations` list, `/v1/actions/{digest}/runs`, `/v1/tests*`; durable invocation records. Red: restart the frontend and `GET /v1/invocations/{id}` still answers; a blocked history writer does not delay a `Result` acknowledgement and increments `kbf_history_dropped_total`; a digest that failed then passed is listed flaky.
- **U4 Client report.** `POST .../report`, commit, targets, critical path, JUnit. Red: a buck2 build report plus `what-ran` maps a test shape to its target; first-failing commit is found across three reported invocations.
- **U5 Series and health.** Built-in series rollups, node samples, `/v1/health`, `/v1/events`; storage views as P3 to P6 land. Red: a fake clock across 61 minutes yields one hourly point equal to the mean of 60 minute points.
- **U6 UI pages** (P10). Pages over the API only; failure pins. Red: the page contract test fails when a page reads a route its list does not name.
- **U7 Idle-host tests**, after preemption of `batch` is built. Red: in `kbf-sim`, a `ci` action arriving at a node with an idle lease starts within the preemption bound; a preempted idle run adds no outcome row.

## 10. Open questions for the owner

Each has a recommendation; none is decided.

1. **History store home.** Separate `history.db` / Postgres schema, or tables in the state store? Recommend separate (5.2).
2. **Durable invocation records.** Do history run records replace the chunks-and-pins stage 2 of `invocations.md` 3.5 (written for the Raft log v2 removes)? Recommend yes; one durable record.
3. **Commit and target source.** Client report upload, a per-call header, or both? Recommend the upload route first, the header if buck2 can send one from the environment [V].
4. **Test results inside outputs.** Should the server parse JUnit/`test.xml` outputs it sees, or only accept them in the client report? Recommend client report only in v1.
5. **Failure pins.** Pin failed runs' logs, result and command for 30 days within a 20 GiB budget? Recommend yes.
6. **Retention tiers.** Accept 30 days for runs, 400 days for test outcomes, 2 years for rollups (5.3)?
7. **`/v1/flakes`.** `failure-classes.md` 6.3 plans it; this document's `/v1/tests?status=flaky` covers it with the digest proof included. Recommend one route, `/v1/tests`, and amend 6.3.
8. **Arguments in the UI.** Show command arguments as sent with operator redaction patterns, env names only? Recommend yes.
9. **Metrics over time.** Built-in fixed rollup plus labeled `/metrics` for Prometheus, or Prometheus only? Recommend both; the owner's health view must not need an external system.
10. **Idle-host tests.** Wait for preemption, or ship earlier limited to tests under 60 s? Recommend wait.
11. **Agent read tokens** for farm-state routes, not only invocation routes? Recommend yes.
