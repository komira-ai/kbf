# Start latency on the farm: request to enqueue and placed to running, against 100 ms

This measures where a kbf build action's start spends its time, on real farm traffic, read
only. The goal is 100 ms p50 from the Execute's arrival at kbf to the action running. This pass
measures it once the start-path changes listed below are deployed.

How to read the numbers:
- A number is **measured** when it was read from a log line. It is **inferred** when it was
  worked out from measured numbers.
- A `p50 / p90` pair is a per-job quantile, linearly interpolated between ranks.
- Medians of stages do not add up to the median of their sum.

## The answer

- **Request to enqueue misses 100 ms.** This is the Execute's arrival at kbf to the job service's
  answer to SubmitJob (measured, every kbf job):
  - light minutes: **244 / 938 ms** (n = 341);
  - burst minutes: **595 / 3,080 ms** (n = 24,031).
- **Placed to running misses 100 ms.** This is the job service placing the job on a host to the
  job's container running (measured, joined Linux starts):
  - light: **746 / 13,519 ms** (n = 262);
  - burst: **949 / 15,076 ms** (n = 17,343).
  - On a host with no other start under way (uncontended) it is **158 / 256 ms** in light
    minutes (n = 50) and **199 / 563 ms** in burst minutes (n = 1,251).
- **Against the previous measurement (2026-11-19).** That measurement, of the same log lines and
  not published, had about 310 ms to enqueue and 300-780 ms
  placed to running.
  - Request to enqueue is lower in light minutes (244 ms), and higher in a burst (595 ms).
  - Uncontended placed to running is lower (158-199 ms). Over every start, the light value
    (746 ms) is inside the earlier range and the burst value (949 ms) is above it.
- **What a user waits for is still the wait for a host.** Submitted to placed is **71.5 s p50 and
  308 s p90** over every joined start (measured). This is the hosts' capacity queue. It is
  outside the 100 ms path. Each other stage's p50 here is 70 to 300 times smaller.
- **The biggest remaining stage of the start path is kbf's REAPI prepare in a burst:**
  **248 / 2,023 ms** (joined, n = 17,343; measured), against 80 / 282 ms in light minutes.
  - Its p50 is the largest of any single start-path stage in a burst. The next are executor
    `start` at 216 ms and the host's provider wait at 153 ms.
  - In light minutes the largest stage is instead the provider wait, 148 / 513 ms, against
    REAPI prepare's 80 ms.
  - Where it is long, the input walk's store checks hold it (measured on the 77 Executes over
    10 s, below).
  - It is filed as komira-ai/kbf#2464.
- **The tail is the host agent's worker queue.**
  - 5,974 of the 23,444 joined starts (25.5%) took over 5 s from placed to running (measured).
  - Summed over those starts, 62% of their placed-to-running time is the agent's worker queue,
    from the job's arrival at the agent to the agent taking it (inferred from per-job measured
    stages).
  - This queue is 0 ms p50 on an uncontended host and 9.5 s p90 over every light start.
- **Input-tree form (the frontend's lane flag): provider, on every Linux start.** The other form
  is manifests.
  - Every one of the 23,444 joined starts carries the agent's `provided` step. That step is
    there only when the frontend sends the tree in the provider form
    (`executor-input-tree = "provider"`).
  - No start ran in the manifest form (measured, from the agent's `Started` line).
  - The frontend's configuration file itself is readable only by root, so it was not read.

## What ran

The window is 21:01-23:54Z on 2026-11-26, by the Execute's arrival at kbf. Nothing was deployed,
restarted or started by this pass.

| component | version in the window | start-path content |
|---|---|---|
| kbf frontend | `7f6a8dc8` to 21:56Z, `4572c279` to 22:59Z, `b9655795` after | `7f6a8dc8` is the merge of komira-ai/kbf#2454; the later commits change no start-path code: `4572c279` adds komira-ai/kbf#2459 (a doc) and #2457 (deletes two unused crates); `b9655795` adds komira-ai/kbf#2453 (a Mac test change) and #2455 (the store janitor) |
| job service | 0.1.421 to 21:43Z, then 0.1.422 | 0.1.421 holds komira-ai/jobscheduler#1092 and #1109; 0.1.422 adds a Mac-only change (komira-ai/jobscheduler#1119) |
| host agent | 0.1.419 to 21:50Z, then 0.1.422 | no agent change between the two on Linux |
| tierstore | 0.3.189 to 22:39Z, then 0.3.190 | 0.3.190 adds komira-ai/tierstore#581 (a version 2 tree list read from one fill); the burst rows below are split at 22:39Z as a check |

Every change named for this measurement is in what ran, checked as an ancestor of the deployed
commit or release tag:
- kbf: komira-ai/kbf#2394, #2417, #2421, #2423, #2424, #2425, #2437, #2440 and #2454;
- tierstore: komira-ai/tierstore#441, #442, #469 and #476;
- jobscheduler: komira-ai/jobscheduler#1092, from 0.1.421.

The version of each piece was read as follows:
- the frontend's version from its first output line;
- the job service's from its image label;
- the agent's and tierstore's from the package database and upgrade log of each host.

The window starts at 21:01Z because the job service took up 0.1.421 at 21:00Z. Before that, two
earlier spans ran without komira-ai/jobscheduler#1092, and the first also without
komira-ai/kbf#2440 and #2454. Their request to enqueue, light / burst p50, every kbf job:
- 18:47-20:04Z: 690 / 2,771 ms;
- 20:05-20:56Z: 666 / 1,138 ms.

Both are higher than this window's 244 / 595 ms (measured, from the same log line). Those spans
also had fewer light minutes (20 and 5), so they are a comparison, not a result.

## Method

- **Sources, all read as an unprivileged user:**
  - kbf frontend: the per-job line `the job service took the job` (komira-ai/kbf#2423). It carries
    `reapi_unix_ms`, `presubmit_ms`, `prepare_ms`, `start_ms`, `since_reapi_ms` and
    `submitted_unix_ms`.
  - kbf frontend: the `Execute spent 10s or more before its first operation` line, and the lines
    of runs ended or run again.
  - job service: the SubmitJob call lines and the placement lines.
  - host agent: each job's `Started` line. It holds the agent's steps (`taken`, `admitted`,
    `dirs`, `provided`, `resolved`, `ready`) as milliseconds since the agent received the job,
    the placement and receipt times, the runtime's `running` time and the tree lookup's stamps.
- **Hosts.**
  - Starts are joined on five Linux hosts, A-E.
  - A sixth Linux host did not answer, so its 4,696 placements in the window have no `Started`
    line and are left out of the host rows.
  - The four Mac hosts' starts (2,759) appear only in the kbf rows.
- **Join.** A kbf job joins its `Started` line by job id: 23,444 of the 28,468 kbf jobs placed on
  Linux hosts. The rest were placed on the host that did not answer, or had not started by the
  read.
- **Load classes.** The traffic comes in CI pushes, so a class is a set of minutes, by the number
  of Executes that reached kbf in that minute:
  - **light:** 1 to 40 in the minute. There were **23 minutes**. The target was 30, but the farm
    had no more light minutes in three hours.
  - **burst:** at least 200 in the minute: **67 minutes**.
  - Of the window's other minutes, 63 had 41-199 Executes and 20 had none.
  - A class says how busy kbf was. A job of a light minute can still wait behind a burst on its
    host.
- **Uncontended:** no other start ran on the job's host between its receipt and its `running`.
- **Clocks.**
  - kbf and the job service share one host's clock, which was within 1 ms of NTP.
  - The hosts' clocks were within 5 ms of NTP, except host D's, which was 23 ms off.
  - Rows from a kbf or job-service time to a host time carry that error.

## The stages, p50 / p90 in ms (measured)

Joined Linux starts. Light n = 262, burst n = 17,343.

| stage | from -> to | light | burst |
|---|---|---|---|
| REAPI prepare | `presubmit_ms`: authorization, action-cache lookup, Action and Command reads, input walk, operation id | 80 / 282 | **248 / 2,023** |
| executor prepare | `prepare_ms` | 12 / 61 | 9 / 95 |
| executor `start` | `start_ms`: the action tree's write and the operation row's, then the SubmitJob call | 117 / 684 | 216 / 892 |
| **request to enqueue** | arrival at kbf -> SubmitJob answered (`since_reapi_ms`) | **230 / 7,524** | **649 / 3,318** |
| capacity queue | SubmitJob answered -> placed | 286,266 / 417,284 | 54,155 / 255,397 |
| delivery | placed -> received by the agent | 85 / 1,111 | 94 / 548 |
| agent worker queue | received -> `taken` | 55 / 9,490 | 118 / 8,797 |
| admission | `taken` -> `admitted` | 0 / 0 | 0 / 0 |
| directories | `admitted` -> `dirs` | 3 / 9 | 3 / 18 |
| provider wait | `admitted` -> `provided` | 148 / 513 | 153 / 1,329 |
| lookup | `provided` -> `resolved` | 18 / 931 | 15 / 812 |
| assembly | `resolved` -> `ready` | 11 / 515 | 11 / 795 |
| create | `ready` -> `running` | 40 / 466 | 50 / 547 |
| **placed to running** | placed -> `running` | **746 / 13,519** | **949 / 15,076** |

- The request to enqueue row here is over joined starts. The headline above counts every kbf job
  (Mac hosts and the host that did not answer included).
  - The p50s agree within 20 ms in light minutes and 55 ms in a burst.
  - The light p90s do not agree (938 against 7,524 ms). About a tenth of light jobs stalled over
    5 s in executor `start`: 31 of 341, and 26 of the 262 joined. So the p90 falls at the edge of
    that stalled tail and moves between them.
- In the provider form the FUSE open of the tree takes 3 / 123 ms in light and 2 / 44 ms in a
  burst, inside the lookup row. The tree is placed before the open.

**Uncontended** (light n = 50, burst n = 1,251):

| stage | light | burst |
|---|---|---|
| REAPI prepare | 68 / 257 | 372 / 2,070 |
| executor `start` | 80 / 131,957 | 230 / 3,049 |
| delivery | 22 / 41 | 23 / 89 |
| agent worker queue | 0 / 1 | 0 / 7 |
| provider wait | 47 / 157 | 121 / 220 |
| lookup | 6 / 30 | 7 / 12 |
| assembly | 5 / 8 | 6 / 10 |
| create | 22 / 32 | 23 / 57 |
| **placed to running** | **158 / 256** | **199 / 563** |

- The light executor `start` p90 of 131,957 ms comes from 11 of the 50 jobs, which stalled over
  5 s in executor `start` (see "What is left", item 3). The other 39 are near the p50.
- A job that stalls in kbf is submitted after its burst has passed, so it is often placed at once
  and starts on a quiet host.

**Split at the tierstore upgrade** (burst; 0.3.189, n = 11,019 joined / 0.3.190, n = 6,324):
- provider wait: 154 / 151 ms p50;
- placed to running: 1,277 / 684 ms p50;
- agent worker queue: 196 / 51 ms p50;
- request to enqueue: 624 / 690 ms p50.

The provider wait did not move. Placed to running fell after 22:39Z, and so did the agent's
worker queue. The tierstore change is not separated from the hosts' load here.

## What is left, by stage

1. **The capacity queue** (71.5 s p50) is the wait for a free host. It is about the hosts'
   capacity and placement, not the start path, and earlier measurements named it.
2. **kbf's REAPI prepare in a burst** (248 / 2,023 ms) is the largest p50 of the path.
   - It is three times its light value, so it grows with the frontend's load.
   - **Where it is long, the input walk holds it.** 77 Executes spent 10 s or more before their
     first operation (measured, from kbf's WARN line). Over them, p50s (which need not add):
     - their time before the first operation (`presubmit_ms`) is 12,802 ms;
     - the input walk (`walk_ms`) is 11,944 ms of it;
     - inside the walk, the presence checks of Directories taken from the content cache
       (`dirs_ms`) are 9,071 ms;
     - the file checks (`files_ms`) are 3,442 ms;
     - the submit (`submit_ms`) is 13 ms.
     - For scale, the first logged Execute checked 2,183 cached Directories and 17,552 files.
   - **Below 10 s, the split is not logged.** It is only on the frontend's metrics endpoint,
     which this pass could not read. So the p50's own split is not measured.
   - Filed as komira-ai/kbf#2464: put the walk's split on the per-job line, and stop checking a
     cached Directory's presence on every Execute.
3. **Executor `start`** (216 ms p50 in a burst) holds the action tree's write and the SubmitJob
   call.
   - 671 of 31,570 kbf jobs (2.1%) spent over 5 s in it, and 272 (0.9%) over 60 s (measured).
   - 345 runs were run again because `the run's action tree was not written within the
     preparation's limit` (measured; 323 of them at 22:50-23:00Z).
4. **The provider wait** (148-153 ms p50, 47-121 uncontended) is the host's wait for its tree:
   the `Prepare` call, tierstore's placement of the tree and the `Mount` call.
   The open issue komira-ai/jobscheduler#1145 (send `Mount` as soon as `Prepare` is answered)
   would remove one of the agent's worker steps from it.
5. **The agent's worker queue** sets the p90 of placed to running (above).
   The open issue komira-ai/jobscheduler#1147 would take work that a start waits behind off the
   agent's workers.

## What this pass did not measure

- **Metrics endpoints.** The frontend's and the job service's metrics endpoints were not read.
  They are reachable only inside their containers' network, and reading them needs root.
  - Not measured, as a result: the split of REAPI prepare below 10 s, the tree write's slot and
    503 waits (komira-ai/kbf#2437), and the job service's SubmitJob phases
    (komira-ai/jobscheduler#1109).
- **The frontend's configuration file** is readable only by root. The lane above is from the
  agent's steps.
- **The sixth Linux host** did not answer, and the Mac hosts were not read. Their starts are only
  in the kbf rows.
- **Light minutes:** 23 against the target of 30.

## Incidents in the window (measured)

- **801 operations finished failed** (`code=14`), every one with `no put directory: no action
  tree: cas/l/ holds an object of 0 bytes`.
  - Two input blobs of about 231 MB each are held in the store as 0-byte objects: 514 and 287
    failures.
  - They came at 21:10-21:20Z and 22:40-23:10Z.
- **345 runs were run again** because their action tree was not written within the preparation's
  limit. This is a start-path symptom, counted under executor `start` above.
- **The frontend's host was busy.** It runs kbf, the job service and their database, on 16
  hardware threads (`nproc`). Its 1-minute load was 8.4 p50 and 16.8 p90, sampled each minute
  from 21:02 to 23:02Z (measured). The job service used 2.6 cores p50 and 4.8 p90 of those
  threads.
