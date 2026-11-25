# Submitting an action's job before its action tree is stored

> **Status: proposal. Nothing here is built.** It compares four ways to stop an Execute's job
> submission from waiting for its action tree's writes, and recommends one. Every task at the end
> waits for a decision before it starts.

Today the executor submits a run's job only after every write of the run's action tree has ended.
This was not always so. Until komira-ai/kbf#2385, attempt 0 submitted the job while the tree was
still being written. If the job's host looked up `trees/<digest>` before the root manifest (written
last) was in the store, the start failed with `<digest>: No such file or directory`
(`input-unavailable`), and the run went again as attempt 1. The wait now costs every start that
writes a tree. Its median is about 50 to 60 ms of the executor's `start` phase, and nothing else
happens in that time.

This page asks how much of that wait can overlap the submit, the placement and the delivery
without bringing back the failed start. The answer is **about 30 ms at the p50 of a start placed
at once, and up to the whole wait (about 50 to 60 ms) at best**. It cannot reach the start goal
alone: in the light window, a start placed at once goes from about 0.47 s to about 0.44 s
(sections 2 and 4).

Line references are to these trees, as read:

| repository | commit |
|---|---|
| komira-ai/kbf | `21da3803a` |
| komira-ai/tierstore | `a8607f0` |
| komira-ai/jobscheduler | `94e91073c` |

## 1. What happens today

### 1.1 kbf: the tree is named first, written after, and the submit waits

- **The name comes first.** `TreeWriter::write_as` builds every manifest and the root in memory,
  then calls `named(&tree)` (`crates/kbf-front/src/action_tree.rs:591-592`) before the first write.
- **Three write stages follow** (`action_tree.rs:593-620`):
  1. the blobs not yet stored anywhere (`WriteStage::Blobs`);
  2. the input root's manifests and `.kbf`, together (`WriteStage::Manifests`);
  3. the root (`WriteStage::Root`).
- **The root is written last on purpose.** The module doc says it is last "so a reader that finds
  the root finds every manifest and blob below it" (`action_tree.rs:21-24`).
- **Every write is create-only and content-addressed** under `cas/l/<sha256>`. An existing key
  counts as success (`action_tree.rs:18-20`), so repeating a write, or leaving one half done, is
  safe.
- **The executor gets the name early.** `Front::write_tree` spawns the writes and waits only for the
  name (`crates/kbf-server/src/executor/front.rs:774-796`). It parks the name for the job spec and
  parks the write's handle in `pending` (`front.rs:741-750`).
- **The name goes into the job spec at once.** The Linux spec's one layer is the host path
  `<trees>/<digest>` (`crates/kbf-server/src/executor/out.rs:102-108`, chosen at
  `executor/submit.rs:370-371`). A Mac job's layer is tierstore's mount provider, and its `ref` is
  the tree list's key (`out.rs:124-131`).
- **`fresh()` makes the submit wait.**
  - It writes the operation's row while the tree is written:
    `tokio::join!(tree_written(..), put_row)` (`crates/kbf-server/src/executor/run.rs:443`).
  - Only then does it call `submit_job` (`run.rs:455-463`).
  - `tree_written` waits on `Ingest::stored` (`run.rs:556-581`, `front.rs:577-583`). It is timed as
    step `tree_wait` and bounded by `prepare_limit`, 300 s by default
    (`executor/config.rs:153`).
  - The comment at `run.rs:441-442` and the `Ingest::stored` doc
    (`crates/kbf-server/src/executor/traits.rs:135-141`) both give the reason: the host opens the
    tree as soon as the job is placed.
- **A tree already written costs no wait.** A tree memoised in the last 600 s answers `stored` at
  once (`front.rs:577-583`, `action_tree.rs:136`).
- **If the tree write fails, no job was ever submitted** (`run.rs:452-454`). The failure maps
  through `not_minted` (`executor/submit.rs:307-316`):
  - `Missing` answers the Execute MISSING;
  - `Unavailable` is `infra`, so the run goes again;
  - `Refused` is `invalid`.
- **The guard for the old failure is a test.**
  `every_attempt_submits_its_job_only_once_its_tree_is_stored`
  (`crates/kbf-server/src/executor/run_tree_tests.rs:38`) holds the tree's writes and checks that
  no `SubmitJob` goes out on attempt 0 or attempt 1. Seven other tests in the same file cover the
  failure, cancel, bound and row cases of komira-ai/kbf#2385.

### 1.2 jobscheduler: a host path is opened once, at admission

- **The contract.** `docs/design/job-rootfs.md` section 7.2, rule 4 (lines 989-995):
  > a host path whose root must exist exists before the submit, or the start fails
  > `input-unavailable` (section 8). Each host path is opened once, when the agent has the start,
  > and a missing one fails the start at once: a path written after that open is not seen by that
  > start, only by a later one.
- **The code: the lookup is asked at admission.**
  - Each host path is looked up on the `js-lookups` pool when the start is admitted, before the
    image is ready (`crates/js-agent/src/lifecycle/machine/prepare.rs:277-291`).
  - The pool has 4 threads (`crates/js-agent/src/pool.rs:32`, `crates/js-agent/src/crun/fs/mod.rs:283`).
  - Each lookup's deadline is 60 s from when it was asked, queue time included (`LOOKUP_MS`,
    `crun/fs/mod.rs:69`, `:381-409`).
- **The code: one try, no retry.**
  - A lookup is one `openat2` beneath the root (`crun/fs/mod.rs:581-603`).
  - Only `EAGAIN` is retried (`crates/js-contain/src/job_fs/resolve.rs:90-109`).
  - `ENOENT` becomes `input-unavailable` at once (`crun/fs/mod.rs:623-630`).
- **What the client sees.** Only the code reaches the client: the job ends `FAILED_TO_START` with
  `start_failure_detail` `input-unavailable` (`crates/js-lease-server/src/per_job.rs:535-551`).
  The path and the OS error are in the agent's `start-failed` finding only.
- **What kbf does with it.** It classifies the failure `infra` and runs it again as the next
  attempt (`crates/kbf-server/src/executor/outcome.rs:488-490`). An operation has `RESUBMITS` = 2
  such attempts after the first (`outcome.rs:153`).
- **No job can be held back.** The job service has no call that holds a job, or names its input,
  after the submit. Its calls are `SubmitJob`, `GetJob`, `CancelJob`, `GetResult` and host and
  pool calls (`crates/js-proto/proto/jobscheduler/v1/service.proto:48-170`).
- **A resubmit may change nothing but the priority.** A submit under the same id with a spec that
  differs in more than its priority is refused `job-id-conflict`
  (`crates/js-proto/proto/jobscheduler/v1/job.proto:134-139`, `:175-177`).
- **A client that dies does not end its job.** A queued orphan ends at the queue wait. A placed
  one ends at the start timeout, 900 s from placement
  (`crates/js-lease-server/src/job_expiry.rs:65`, `docs/design/jobscheduler-design.md:190`).

### 1.3 tierstore: a lookup of `trees/<digest>` whose root is absent answers ENOENT at once

- **The lookup path.**
  1. A lookup of `trees/<digest>` is parked on one of the fs's fetch threads (`fs.threads`, 16 by
     default) (`crates/tierstore/src/server/treefs/node_fs.rs:360-363`).
  2. `Core::resolve` reads the name as a digest (`treefs/core/implicit.rs:53-58`).
  3. The tree is attached by `read_tree` (`treefs/core/placed.rs:233-293`).
  4. `read_tree` fetches the manifests level by level through the fill socket. One bound covers the
     whole read: `TREE_READ`, 30 s (`placed.rs:60-67`, from komira-ai/tierstore#404).
- **An absent root answers ENOENT.** When the upstream answers `upstream 404` for the root manifest
  itself, the result is `Unread::Gone` (`placed.rs:277-289`), which becomes `ENOENT`
  (`Core::refusal`, `implicit.rs:270-279`).
  - The fs logs this at info level only: `lookup of <digest>: absent in <ms> ms`
    (`implicit.rs:106-118`).
  - Any other failure becomes `EIO`, with a rate-limited warning.
- **The miss is not cached, in the kernel or in tierstore.**
  - The lookup is answered with a bare errno and a zero TTL (`node_fs.rs:176-198`).
  - The upstream is asked again at every lookup. The test
    `a_digest_the_upstream_has_no_tree_of_is_not_there_and_takes_no_number`
    (`treefs/core/implicit_tests.rs:78-107`) pins this.
  - `docs/design/virtual-build-fs.md:1236` states it as the contract: "the next lookup after the
    write resolves it".
- **The fs knows only the name.** No job context reaches a lookup: `Core::lookup` receives the
  parent and the name only (`treefs/core.rs:639-653`). The only thing a job can vary is the name.
- **The Mac route already waits, but not for a missing list.**
  - Through the mount provider, `Mount` already answers "not ready" (`UNAVAILABLE`), and the agent
    asks again (`crates/js-proto/proto/jobscheduler/provider/v1/provider.proto:23-25`;
    `crates/js-agent/src/lifecycle/machine/providing.rs`). The agent's `--mount-provider-wait`
    defaults to 120 s.
  - A tree list that is not stored yet is still refused at `Prepare`:
    `no tree list at <key>` (`crates/tierstore/src/server/provider/prepare.rs:265`).

### 1.4 The ENOENT this must not bring back

The failure was diagnosed in komira-ai/jobscheduler#925 and fixed in komira-ai/kbf#2385:

- The agent's finding was `<digest>: No such file or directory (os error 2)`.
- Every `input-unavailable` the frontend logged in the two hours that were read was an attempt 0.
- Every retry started within seconds.

Those counts come from the diagnosis. The logs behind them were not kept, so they are not
re-derived here.

After the fix (measured, read again for this page): in the 2026-11-24 window, 24 of the 8,406 jobs
the frontend submitted were attempt 1, and none was a later attempt.
- Source: the frontend's per-job `the job service took the job` lines.
- That bounds the retries of every cause to 0.3% of submits.
- The data does not say what caused those 24.

## 2. The numbers

**Source.** Real traffic on the build farm on 2026-11-24, read-only. The frontend logged 8,406
submitted jobs between 16:47 and 18:22Z. The analysis uses three windows, by when the executor
opened the operation:
- burst 16:50-17:10Z, 4,409 jobs;
- mid 17:10-17:34Z, 1,471 jobs;
- light 17:37-18:16Z, 1,432 jobs.

**How the data was joined.** Per job, by job id, across three logs:
- the frontend's per-job line (`log_submitted`, `run.rs:495-519`);
- the job service's placement stamp;
- the agent's `Started` timeline.

The agent's timeline gives the steps `taken`, `admitted`, `dirs` (the tree lookup begins),
`resolved` (it ends), `ready` and `created`.

**Placed at once** means queue ≤ 50 ms. **Histograms** are the frontend's
`kbf_executor_step_seconds`, which carries no job id. Their **cum** column runs from frontend start
to 18:05Z (n = 7,359). Their **last 14 min** column runs 18:08-18:22Z (n = 1,040). Both cover every
host, Macs included.

Every number below was recomputed for this page from the same data:
- the same per-job join, whose output is byte-identical to the original tables;
- the same histogram interpolation.

### 2.1 What the tree wait is (histogram, ms, p50 / p90; measured)

| step | cum | last 14 min |
|---|---|---|
| `tree_wait` | 61 / 219 | 49 / 398 |
| of it, Linux form: `tree_manifests` | 22 / 205 | 31 / 245 |
| of it, Linux form: `tree_root` | 13 / 48 | 17 / 126 |
| `rpc` (`SubmitJob`) | 22 / 60 | 28 / 123 |
| `row` | 4 / 25 | 4 / 18 |
| phase `start` | 85 / 242 | 92 / 451 |

Shape of `tree_wait` (cum, measured):
- 360 of 7,359 (4.9%) took 1 ms or less: memoised trees.
- 70 (1.0%) took over 0.5 s.
- None took over 2.5 s.
- The mean is 89 ms.

### 2.2 What happens between the submit and the lookup (per job, placed at once, ms, p50 / p90; measured)

The frontend stamps its submit time when the `SubmitJob` answer arrives (`run.rs:468`). So the
queue below starts after the rpc.

| segment | burst (n = 404) | mid (n = 503) | light (n = 282) |
|---|---|---|---|
| queue (answered → placed) | 18 / 43 | 11 / 34 | 9 / 38 |
| delivery (placed → the agent has it) | 19 / 10,182 | 28 / 10,330 | 22 / 104 |
| worker queue (→ `taken`) | 18 / 1,981 | 9 / 2,589 | 0 / 606 |
| admission (`taken` → `dirs`) | 0 / 1 | 0 / 1 | 0 / 1 |
| tree attach (`dirs` → `resolved`) | 114 / 489 | 107 / 597 | 108 / 292 |
| executor `start_ms` | 49 / 121 | 53 / 168 | 68 / 301 |
| op opened → running | 732 / 10,690 | 788 / 12,407 | 474 / 10,559 |

## 3. The options

Every option must keep three properties:

- **No false failures.** A start does not fail because its tree is still being written.
- **Real misses stay fast.** A tree that will never be stored (a lost object, a failed write)
  still fails, and soon.
- **Every job is accounted for.** No job is left for nobody to end.

### (a) The fs waits, within a bound, for a root the job's name marks as still being written

**The mechanism.**

- **kbf picks the name.** When kbf submits a job whose tree's writes have not ended, it names the
  tree `trees/<digest>.writing` instead of `trees/<digest>`.
  - The suffix is the flag on the job: it sits in the job's layer, which is a host path the job
    service passes through untouched.
  - The plain name keeps today's meaning: a missing root fails at once.
- **tierstore waits only for that name.**
  - It reads the `.writing` form as the same digest, with one difference. A root manifest that the
    upstream answers `upstream 404` for is asked for again after a short rest, until it is found
    or the wait bound `W` passes.
  - After `W` the answer is `ENOENT`, as today.
  - The rest of the attach is unchanged: the same manifests, the same tree number per digest, the
    same answers once found.
- **The wait lives in tierstore, in `read_tree`'s `fetch` closure** (`placed.rs:262-276`), for the
  root only (`wanted == digest`).
  - That is where `upstream 404` for the root is already told apart from every other failure
    (`placed.rs:280-283`).
  - The wait is counted inside the same `Until` as the rest of the read (`TREE_READ`, 30 s). The
    manifests below the root get whatever is left.
  - The rests need a wait that tests can drive. The core's `Clock` only reads the time
    (`crates/tierstore/src/clock.rs:12-15`).
    - The fs already has the waiting half: `Fills::pause` (`treefs/fills.rs:218`). The test rig
      turns each pause into a step of its clock (`treefs/core/tree_read_tests.rs:20-35`).
    - The wait calls `pause` with its own lengths. `Fills::rest` always pauses for one fixed
      `PAUSE`, so it does not fit here.
  - Each try is one fill through the fill socket, and a miss is not cached anywhere (section 1.3).
    So a try asks the upstream again.
- **The bound.** `W` = 10 s, a tierstore constant rather than a setting (decision D2):

  | bound | value |
  |---|---|
  | `W` | 10 s |
  | `TREE_READ` (`placed.rs:67`) | 30 s |
  | the agent's `LOOKUP_MS` (`crun/fs/mod.rs:69`), queue wait included | 60 s |
  | the job service's start timeout | 900 s |

  - In the measured window no tree wait passed 2.5 s (section 2.1).
  - A tree write slower than `W` ends its start `input-unavailable` as today, and the run goes again
    (failure table below).
- **The rests.** 10 ms first, doubling, at most 100 ms (decision D3). A start that finds its root
  stored on the first try, which is most of them (section 4), pays nothing.
- **Where the waiting lookup runs.**
  - A `.writing` lookup makes its first try on the fetch pool, like any lookup.
  - Only if the root is missing does it move to a deadline queue. A small pool of its own serves
    that queue, one try at a time, so no thread is held for a whole wait.
  - At most 16 lookups wait at once; one past that is answered `ENOENT` at once. Section 5.2 gives
    the reasons.
- **The cost while it waits.**
  - The waiting lookup holds one of the agent's 4 `js-lookups` threads, which is the price of the
    option. Section 4 estimates how often it is paid, and section 5.2 how much it adds.
  - It holds no fetch thread. So attaches of stored trees, and running jobs' opens of files the
    node does not hold yet, never wait behind it.
  - It holds no tree in memory: a root that is not there yet has nothing to read. Section 5.1
    gives what the trees a host does hold cost.
- **Two names, one tree.**
  - `<digest>` and `<digest>.writing` resolve to the same tree number and directory inode.
  - Lookups of one digest under its two names can now read and attach at the same time.
    - The `read_ahead` doc assumes one lookup per name (`implicit.rs:77-82`).
    - The attach that loses the race must keep the winner's number (`attach_read`,
      `implicit.rs:124-146`). Task 1 tests that.
  - The agent opens the host path once and keeps the descriptor, never the path.
    - The open: `crates/js-agent/src/crun/fs/mod.rs:581-603`. The `--mount-root` row of
      `crates/js-agent/src/lib.rs:107` says a path is "bound by what was opened, never by its path".
    - Its checks after the job read the clones it made, not the host path
      (`crun/fs/mod.rs:558-575`).
    - So the `.writing` name only has to resolve once.
- **An older tierstore.**
  - Today's `resolve` answers any name that is not a bare digest with `ENOENT`, at once
    (`implicit.rs:53-58`).
  - So on a host with an older tierstore, every `.writing` start fails `input-unavailable`, even
    when its tree is stored. kbf runs it again as the next attempt (`outcome.rs:488-490`). By then
    the tree is usually memoised, so that attempt gets the plain name.
  - A job's constraints cannot select a host by its tierstore (`Constraints`, `job.proto:80-110`).
    So the guard is the order of adoption: tierstore on every host first, then the kbf flag.
  - Turning the flag off brings back today's behaviour for the next run.
- **The kbf side.**
  - `fresh()` writes the row and then submits, while the tree is still being written.
  - It then waits for the writes before it watches the job, as attempt 0 did before
    komira-ai/kbf#2385 (the old `run.rs:287-332`).
  - The row is still written before the submit. A job is never known to the job service without a
    row naming it, because the takeover path depends on that row (`run.rs:583-604`).
  - The name is chosen when the spec is built in `prepare` (`submit.rs:216-231`), from whether the
    mint left writes pending.
    - `Ingest::stored` cannot answer that, because it takes the pending writes away
      (`front.rs:577-583`). So the mint, or `Inputs::tree`, also has to say whether writes are
      pending.
    - A memoised tree, or a later attempt whose tree is stored, gets the plain name.
    - A write that ends between `prepare` and the submit still gets `.writing`. That is harmless:
      the root is found on the first try.

**Failure cases.**

| case | what happens | who withdraws the job, and when | what the host logs |
|---|---|---|---|
| the tree write fails after the submit | `tree_written` answers the error. kbf cancels the job (`withdraw`, `run.rs:614-628`), then ends the run as `not_minted` says: MISSING, `invalid` or `infra` and again | kbf, as soon as the write fails. A queued job is cancelled at once; a placed or starting one is stopped (`service.proto:74-76`) | the agent ends the start on the stop. The fs lookup goes on to `W` and answers `ENOENT` (info `lookup of <digest>.writing: absent in <W> ms`). Its thread is held until then |
| kbf dies after the submit, before the root is stored | the row names the job. Another frontend takes the row up and watches the job (`run.rs:583-604`) | nobody needs to. The host's lookup ends at `W` with `ENOENT`, the start fails `input-unavailable`, and the taker runs the next attempt. That attempt writes the tree again (its writes are create-only, so a half-written tree is harmless) | as above, then the agent's `start-failed` finding `<digest>.writing: No such file or directory` |
| kbf dies after the root is stored | the job starts and runs. The taker finishes it from its row | nobody | as today |
| the job is cancelled while the tree is written | `cancel_asked` wins the select, and kbf withdraws the job. The write task goes on and ends harmlessly (it is spawned, `front.rs:774-796`) | kbf, at the cancel | as in the first row |
| the Execute ends MISSING (a blob is lost during the write) | the write answers `Missing`. kbf withdraws the job and answers MISSING, as today's `not_minted` does | kbf | as in the first row |
| the write takes longer than `W` | the start fails `input-unavailable`. kbf's write then ends, and the next attempt finds the tree memoised and uses the plain name with no wait. This spends one of the operation's attempts (decision D4) | the host ends it at `W` | warning `lookup of <digest>.writing: absent after waiting <W> ms` (new, rate-limited) |
| a retry or takeover | a later attempt mints again. A memo hit gives the plain name. A taken-up row only watches its job | — | — |
| a tree that will never exist, under the plain name | `ENOENT` at once, as today | — | as today |

**How the old ENOENT stays caught.**

- **kbf: a submit before the tree is stored never names the plain path.** A new test holds the
  tree's writes with the flag on and checks two things:
  - the spec submitted names `trees/<digest>.writing`;
  - a run whose tree is stored names `trees/<digest>`.
- **kbf: the existing guard keeps its meaning.** With the flag off,
  `every_attempt_submits_its_job_only_once_its_tree_is_stored` stays exactly as it is.
- **tierstore: the plain name still fails fast.** The existing test
  `a_digest_the_upstream_has_no_tree_of_is_not_there_and_takes_no_number` stays unchanged.
- **tierstore: new tests for the `.writing` form**, on the injected clock:
  - a root added after two rests resolves to the same tree number as the plain name;
  - a root never added answers `ENOENT` at exactly `W`;
  - a manifest below the root that is missing is still `EIO` at once;
  - lookups of `<digest>` and `<digest>.writing` made at the same time get one tree number.
- **Across releases.** An older tierstore answers a `.writing` name `ENOENT` at once. The existing
  test above pins that behaviour, and the kbf side handles it as the "older tierstore" case above.
  The kbf test for that case: a `.writing` start that fails `input-unavailable` runs its next
  attempt under the plain name once its tree is stored.

**Contract text that changes.**

- **jobscheduler, `docs/design/job-rootfs.md`.**
  - 7.2 rule 4, lines 989-995. "A host path whose root must exist exists before the submit, or the
    start fails `input-unavailable`" becomes: it exists before the submit, *or its file system's
    lookup waits for it within the lookup bound of rule 5*; otherwise the start fails
    `input-unavailable`.
  - 7.4, lines 1036-1041: a waiting lookup holds one of the four lookup threads.
  - No jobscheduler code changes.
- **tierstore, `docs/design/virtual-build-fs.md`.** The namespace section (lines 462-472) and the
  failure table row at line 1236 gain the `.writing` form and `W`.
- **kbf.**
  - `docs/design/actions-as-jobs.md` 4.1 (lines 296-300);
  - the `Ingest::stored` doc (`traits.rs:135-141`);
  - the module doc of `run.rs` (lines 10-14);
  - `docs/deploy/kbf-modes.md` (the flag, and `tree_wait` now off the start path).

### (b) kbf submits at once and names the tree, or says it is stored, only later

**The mechanism.**

- The job service gets a way to hold a job's start until the client says its inputs are ready.
  That could be:
  - a new call (`ReleaseJob`, say);
  - or a spec field filled in at placement, which needs the same call to fill it.
- kbf submits right after the row, then makes the second call once `stored` answers.
- The tree's name is already known before the submit (`action_tree.rs:591-592`). So what (b) adds
  is not a late name. It is a gate.

**What it needs.**

- **jobscheduler, the public surface:**
  - a new rpc and HTTP route (`service.proto`, under its evolution rule at lines 6-13);
  - a stored job field, with its codec tags and `same_job_as` (`job-rootfs.md:428-432`);
- **jobscheduler, the host side:**
  - either a placement or heartbeat rule that does not send the start until it is released;
  - or a start-wire field with an agent wait state. The start wire ignores unknown fields
    (`job-rootfs.md:441-442`), so an older agent would start the job early unless placement
    requires a new capability, as `layered-rootfs` did.
- **kbf:** the second call, its retries, and the withdraw path of (a).

**Failure cases.**

| case | what happens | who withdraws the job, and when | what the host logs |
|---|---|---|---|
| the tree write fails | kbf never releases the job and cancels it | kbf, at the failure | nothing |
| kbf dies before the release | the job is never released. The job service ends nothing early for a client that is gone (`jobscheduler-design.md:190`). The job sits queued until the queue wait, or placed and holding its booking until the 900 s start timeout, unless a new release deadline is added. The taker could release or cancel it from the row | the taker (new code) or the job service's timeout | nothing until the timeout |
| kbf dies after the release | as today after a submit: the taker watches the job from its row | nobody | as today |
| cancelled during the write | kbf cancels an unreleased job | kbf | nothing |
| MISSING | as a failed write | kbf | nothing |
| retry or takeover | the release must be idempotent, and the taker must know whether the release was sent | kbf | — |

**How the old ENOENT stays caught.** The host cannot begin the lookup before the release. A
jobscheduler test checks that a start is not sent, or not admitted, before the release, and a kbf
test checks that the release follows `stored`. It is the safest option on that one point.

**Why it saves less.**

- The host learns of the release only after a second rpc and a second delivery.
- That delivery is the hop with today's 10 s tail (measured): 536 of the 3,909 `Started` lines in
  the collected agent logs (14%) have placed → received between 9 and 11 s, and none between 1
  and 9 s. The fixed step is inferred to be the dispatch lease after a message the agent could
  not queue.
- So (b) puts a second message on the farm's slowest path.

**Contract text.** jobscheduler's `service.proto`, `job.proto`, `docs/job-surface.md`,
`docs/http-gateway.md`, `job-rootfs.md` sections 2, 4.7 and 7.1-7.4, and `jobscheduler-design.md`
(the client-session rule at :190). This is a change to the job service's public interface.

### (c) kbf stores the root first and the rest after; a reader retries a root without its children (rejected)

**The mechanism.** kbf would reverse the stages: root first, then the manifests and blobs. A host
that finds the root but not a child would retry until the child appears.

**What breaks.**

1. **The written invariant of the tree writer.** "A reader that finds the root finds every
   manifest and blob below it" (`action_tree.rs:21-24`, `:613`) is the reason the root is written
   last. The base tree is published the same way, root last (`docs/artifacts.md:695-701`).
2. **A cut-off write leaves a broken tree under a content address.**
   - Today a write cut off by a crash leaves unnamed objects, and nothing points at them
     (`action_tree.rs:442-445`).
   - Under (c) it leaves a root whose children may never come.
   - Every reader of a tree would then have to treat "child missing" as "maybe still being written"
     rather than "lost". Those readers are the fs, the mount provider, `kbf-trees-client` and the
     tools.
   - So a lost object would no longer fail fast anywhere. That is the property the plain-name rule
     of (a) keeps.
3. **The failure moves to a worse place.**
   - The fs reads every manifest of a tree at the attach (`read_tree`). A missing child manifest is
     `EIO` with a warning there (`placed.rs:286-288`, `Core::refusal` at `implicit.rs:270-279`), not `ENOENT`.
   - The blobs are read later, by the running job. A blob not yet stored is an `EIO` inside the
     build. The build then fails as the action's own result, during `RUNNING`, rather than as a
     clean start failure kbf retries.
4. **It saves nothing over (a).** The attach needs every manifest, so the start still waits for the
   children. It only waits on an `EIO`-retry path instead of a root-only one.

**Failure cases.** Here the job would be submitted once the root is stored.

| case | what happens | who withdraws the job, and when | what the host logs |
|---|---|---|---|
| the tree write fails after the root | the root names children that never come. kbf withdraws the job, as in (a). The broken root stays under its content address | kbf, at the failure | warning `cannot attach <digest> on a lookup: cannot fetch manifest <child>: upstream 404`, or an `EIO` inside the running job |
| kbf dies mid-write | the same broken root. The host's retries go on until their bound, then the start fails | nobody; the host's bound | as above |
| cancelled during the write | kbf withdraws the job. The write goes on (it is spawned), and may still finish the tree | kbf, at the cancel | as above, until the stop |
| MISSING | as a failed write. The broken root stays | kbf | as above |
| retry or takeover | a retry gets `AlreadyExists` on the root and must still write every child. It repairs the tree only if it writes the same tree | — | as above |

The ENOENT incident would turn into an `EIO` incident that no test of today's catches.

**Contract text.** tierstore's `virtual-build-fs.md` would have to give up "root lacking = ENOENT,
other = EIO" (lines 467-469). kbf's `action_tree.rs` docs and `artifacts.md` would change. The
change is rejected.

### (d) No change

**What stays.**
- Every start whose tree is not memoised waits for its tree's writes before it is submitted: about
  50 to 60 ms at the p50 (histogram), and 219 to 398 ms at the p90.
- None of that wait overlaps the job service or the host.
- The ENOENT stays caught by the existing kbf test.
- No contract changes.

**Failure cases** (today's code, section 1.1):

| case | what happens | who withdraws the job, and when | what the host logs |
|---|---|---|---|
| the tree write fails | no job is ever submitted. The run ends as `not_minted` says | nothing to withdraw | nothing |
| kbf dies during the write | the row names a job that was never submitted. The taker reads `NOT_FOUND` and runs the next attempt (`forgotten`, `run.rs:679-681`) | nothing to withdraw | nothing |
| kbf dies after the submit | the taker watches the job from its row | nobody | as for any start |
| cancelled during the write | the run ends `CANCELLED` with no job (`run.rs:564-566`) | nothing to withdraw | nothing |
| MISSING | as a failed write | nothing to withdraw | nothing |
| retry or takeover | a later attempt waits for its own tree. A memoised tree costs no wait | — | as for any start |

**What still shrinks the wait without a contract change.**
- komira-ai/kbf#2420 (open issue) starts the tree write beside the input walk's held-file check.
  At most it saves the walk's `walk_files` time: 2 ms at the p50 and 95 ms at the p90 (histogram,
  cum).
- A tree list written in strict sequence on the Mac form (`action_tree/listing.rs`) could overlap
  its sidecar's lookups.
- Neither moves the p50 much. The root's own write (`tree_root` 13-17 ms p50) is always left, since
  the root is written every time and never memoised.

## 4. Expected saving per option

**Definitions.**
- `H`: the time a job needs after the submit before the agent's lookup begins.
- Under (a) the lookup waits `R = max(0, tree_wait − H)`. The start is earlier by
  `S = tree_wait − R = min(tree_wait, H)`.

**Two ways to count `H`.**
- **Strict:** `H` = queue + delivery + worker queue + admission. This leaves out the `SubmitJob`
  rpc.
- **With rpc:** `H` also counts the rpc (22 ms, cum p50). Under (a) the rpc also overlaps the
  writes, because today the submit waits for the tree first (`run.rs:443-463`).

All the medians are from sections 2.1 and 2.2. Subtracting medians is inferred, not measured.

| option | window | `tree_wait` p50 (histogram) | `H` strict / with rpc (sum of placed-at-once p50s) | `R` strict / with rpc | saving `S` at the p50, strict / with rpc |
|---|---|---|---|---|---|
| (a) | burst | 61 | 55 / 77 | 6 / 0 | **55 / 61** |
| (a) | mid | 61 | 48 / 70 | 13 / 0 | **48 / 61** |
| (a) | light | 61 (cum) | 31 / 53 | 30 / 8 | **31 / 53** |
| (a) | light | 49 (last 14 min) | 31 / 53 | 18 / 0 | **31 / 49** |
| (b) | burst / mid / light | 61 | lookup can begin only at `tree_wait` + a second rpc (22) + a second delivery (19 / 28 / 22) | — | **36 / 20 / 9** (with rpc; `S = min(tree_wait, H − rpc₂ − delivery₂)`) |
| (c) | any | — | the attach still needs every manifest | — | **0 over (a)**, and rejected |
| (d) | any | — | — | — | **0** |

*(For example, the burst row's strict `H` is queue 18 + delivery 19 + worker queue 18 +
admission 0 = 55, and 77 with the rpc's 22.)*

**A per-job check.** This is inferred: the frontend's per-job line carries `start_ms`, not
`tree_wait`. Take `tree_wait ≈ start_ms − 22` (the rpc's p50). Then compute `R` and `S` per job,
with `H` with rpc (each job's own strict `H` plus 22), over the Linux jobs placed at once.

The column below is each job's strict `H`, before the 22 is added. Its p50 is a median of
per-job sums, so it is larger than the sums of medians above: the segments' tails come together.

| window | `tree_wait` proxy p50 / p90 | strict `H` p50, per job | `R` p50 / p90 / mean / share > 0 | `S` p50 / p90 / mean |
|---|---|---|---|---|
| burst (n = 404) | 27 / 99 | 246 | 0 / 8 / 20 / 12% | 27 / 73 / 49 |
| mid (n = 503) | 31 / 146 | 194 | 0 / 36 / 24 / 17% | 26 / 71 / 42 |
| light (n = 282) | 46 / 279 | 64 | 0 / 171 / 63 / 32% | 30 / 89 / 47 |

**Reading the tables.**

- **The saving.** For the median start placed at once, (a) saves about 30 ms on the Linux jobs
  (the per-job proxy). Using histogram medians, which include the Mac form's slower lists, the
  saving is 31-61 ms.
  - It is not the whole 50-60 ms: when the host is fast (light window, strict count), a third to a
    half of the wait is still there at the lookup.
- **The share that waits.** 12-32% of starts would still wait at the lookup. In the light window
  that wait is 171 ms at the p90.
  - The rests add up to one rest (≤ 100 ms) to those starts only (inferred).
- **What it does to the start goal.** The light window's 474 ms op → running p50 for a start placed
  at once becomes about 0.44 s.
  - The 100 ms goal needs the other parts of the start too: the attach (108 ms p50), the REAPI and
    executor floor, and the delivery tail.
- **The thread cost of (a).**
  - A waiting lookup holds an agent lookup thread for `R`: a mean of 20-63 ms per start placed at
    once (proxy).
  - The attach itself already holds one for 108 ms p50, and those 4 threads are already a queue in
    busy windows.
  - So (a) should not go on before the lookup pool is measured with its queue time
    (komira-ai/jobscheduler#1068).
- **No numbers for the Macs.** The Macs ran many of the window's jobs, but their agents' logs were
  not in the data, so they have no per-job join. Their saving is not estimated.

## 5. Option (a) on a host: tree memory and threads

Before any task of (a) starts, two host-side questions need answers:
- what a host's attached trees cost in memory;
- how many lookups can wait at once without starving the attaches of trees already stored.

The figures in this section have three sources:
- **The attach lines.** tierstore's `lookup of <digest>: ok in <ms> ms; <K> manifests` lines on two
  Linux hosts, 16:40-18:08Z on 2026-11-24: 2,434 attaches of 2,283 distinct trees. These are the
  hosts called *A* and *B* below.
- **The base tree.** The base-tree bundle of the deployed kbf release v0.0.127, which is a release
  asset.
- **The frontend's walk counters.**

All of it was read from data already collected. No host was read for this section.

### 5.1 Tree metadata in memory

**How a tree is held.** All of this is read from tierstore code at `a8607f0`.

- **An attached tree of manifests is a flattened index on the heap**
  (`crates/tierstore/src/server/treefs/tree.rs:22-44`):
  - one `TreeEntry` per entry;
  - a table of each directory's children, about 8 bytes per entry.
  - Nothing of it is mmap-backed. No file under `crates/tierstore/src/server/treefs` maps memory (a
    grep for `mmap`). A tree there is vectors on the heap, or a tree list read into a buffer
    (`core/placed.rs:91`).
  - Each manifest is read whole into a buffer (`core/placed.rs:80-87`) and parsed. The buffer is
    dropped once it is parsed.
- **Per entry it costs about 128 bytes plus the name** (inferred). That is 104 bytes of
  `TreeEntry`, 16 of name header and 8 of tables.
  - The 104 bytes come from the field layout in `tree_manifest.rs:157-218`. A standalone copy of the
    types gave the same `size_of`.
  - tierstore's own estimate of a tree's heap (`Tree::heap_bytes`, `tree.rs:119-123`) uses the same
    sum.
  - Its estimate leaves out a file's pack key and a link's target, which a manifest tree allocates
    per entry. So the real figure is somewhat higher for trees whose files are in packs.
- **While a tree is read, the peak is about 1.5 times what is kept** (inferred). The parsed
  manifests of the whole tree live until the flattening ends (`tree_manifest/waves.rs:53-97`).
- **How long a tree stays.** A tree attached by a lookup stays until no request has named it for
  `fs.tree_idle_secs`, which is 900 s by default. The sweep runs every 60 s, so a tree stays at most
  about 960 s after it was last named (`core/memory.rs:1-32`, `core/implicit.rs:39-44`).
- **Nothing caps tree memory today.**
  - There is no limit on the count or bytes of trees in memory.
  - The fs unit has no memory limit (tierstore `docs/design/virtual-build-fs.md:912`).
  - At each sweep the fs logs the total: `<N> trees in memory, <M> of them attached: <E> entries,
    about <B> MiB` (`core/memory.rs:441-450`). That line is how a host measures this directly.

**Per tree, over real trees.**

| quantity | p50 | p99 | max | source |
|---|---|---|---|---|
| manifests per tree, `K` (distinct directories, root included) | 505 | 2,279 | 2,657 | **measured**: the attach lines, n = 2,339 with `K` > 0 (95 more had `K` = 0) |
| the base tree every action tree holds | 316 manifests, 2,792 entries, 350,719 bytes of manifests, mean name 9.3 bytes | | | **measured**: the v0.0.127 base-tree bundle, parsed |
| entries per tree | 4,500-6,600 | 20,000-42,000 | 23,000-50,000 | **inferred**: the base tree plus `K` − 316 action directories at 8.8 to 20.2 entries each. 8.8 is the base tree's own ratio. 20.2 is the frontend's per-walk mean less the base tree: (3,194 − 2,004 files + 378 − 316 directories) over the 62 action directories that leave. The means are `kbf_execute_walk_files_checked_total` and the dir cache's hits and misses, over 8,832 walks. They count distinct files, so they understate repeated ones |
| manifest bytes per tree | 0.6-0.9 MB | 2.7-5.7 MB | 3.1-6.7 MB | **inferred**: entries × the base tree's 133.6 bytes of manifest per entry: 372,939 bytes over its 2,791 entries below the root, duplicate directories counted each time (125.6 bytes per entry stored once) |
| index in memory per tree | 0.6-0.9 MB | 2.8-5.8 MB | 3.2-6.9 MB | **inferred**: entries × about 137 bytes |

**Trees a host holds at once.** Inferred. The count is the distinct trees attached in any trailing
960 s, on the 10 s grid, after each host's tierstore restart. Trees attached by a run, and trees
named again, stay longer, so this is a floor.

| host | p50 | p99 | max |
|---|---|---|---|
| *A* (busier, 1,976 distinct trees) | 343 | 750 | 756 |
| *B* | 29 | 115 | 166 |

**What that needs, against what the hosts have.** Inferred.

- At *A*'s peak, 756 trees of the mean size need about 0.9-1.6 GB of index. The mean `K` is 934,
  which gives 8,200-15,300 entries per tree.
- If every one were as large as the largest tree seen, the bound is about 5.2 GB (756 × 6.9 MB).
- Each lookup reads one tree, on a fetch thread. With up to 16 reads at once (section 5.2 keeps
  that), the peak while reading adds at most a few hundred MB.
- The job service schedules up to 256,000 MiB (about 250 GiB) on that host. That is the limit its
  `predicted-memory` refusals named during the same measurement, not the host's `MemTotal`. The
  measurement's notes quote the refusal line; the kept log does not hold it, so it is not
  re-derived. Against that limit, the trees are 0.3-0.6% at the mean size and 2% at the bound.
- This is today's cost. Option (a) does not add to it:
  - a waiting lookup reads nothing until its root is stored;
  - the tree it then reads is the same tree, under the same number, that a later plain lookup would
    read;
  - two names do not make two trees (section 3(a), "Two names, one tree").
- What (a) adds is per waiting lookup: one queued closure and its FUSE reply, a few hundred bytes.

### 5.2 Fetch threads, and the agent's lookup threads

**What holds what today.**

- **tierstore.** `fs.threads` (16 by default) sets both the FUSE reader threads and the fetch
  threads (`treefs/serving.rs:84-86,221`).
  - Every lookup in `trees/` is parked on a fetch thread (`treefs/node_fs.rs:168-173,360-363`). So
    are the opens that wait for a fill.
  - The fetch queue is one unbounded FIFO (`treefs/fetches.rs:49-56`).
- **The agent.** It sends tierstore at most as many lookups at once as its `js-lookups` pool has
  threads: 4 (`crates/js-agent/src/pool.rs:32`, `crun/fs/mod.rs:283`).
  - Only the agent opens `trees/<digest>` by name. Jobs see a clone of the opened directory
    (`job-rootfs.md:584-610`).
  - A lookup blocked in the kernel keeps its agent thread until the fs answers (`pool.rs:16-17`).

**How many lookups would wait at once.** Inferred from the per-job model of section 4: each joined
start's `R`, starting at its `dirs` step, on its own host.

| | burst | mid | light |
|---|---|---|---|
| starts that would wait (of the joined jobs) | 78 of 963 | 93 of 978 | 112 of 962 |
| most waiting at one moment on one host | 11 | 10 | 12 |
| starts on one host in the busiest 10 s, if every one waited the full `W` | 113 | 81 | 76 |
| waits as a share of the agent's lookup-thread time (sum of `R` / sum of `dirs` → `resolved`) | 9.3% | 1.9% | 8.4% |

- **The worst case:** many starts land before their trees are stored. In the third row's 10 s, 113
  starts on one host each wait the full 10 s. That is the store failing for every write at once.
- **Today that would starve the attaches of stored trees.**
  - Only 4 of those lookups reach tierstore at once, because the agent has 4 threads. The rest
    queue inside the agent.
  - Those 4 sit on 4 of tierstore's 16 fetch threads for the whole `W`.
  - Nothing in tierstore stops another client from parking more: a second agent, a raised agent
    pool, or a tool.
  - With 16 parked, every fetch thread sleeps. Every attach of a stored tree, and every running job's
    open of a file the node must fill, waits up to `W` behind them.

**The bound.** It applies to the `.writing` lookups only (decision D6).

1. **The first try is an ordinary lookup.**
   - A `.writing` lookup is parked on the fetch pool, as every lookup in `trees/` is today. It makes
     its first try of the root there.
   - When the root is stored (most starts, section 4), the attach goes on there as for the plain
     name. Nothing else differs, and no waits structure is touched.
2. **Only a lookup that missed waits, and it waits without a thread.**
   - On `upstream 404` for the root, the fetch thread does not rest. It hands the request (its
     closure and FUSE reply) to a deadline queue and is free at once.
   - The queue is served by a small pool, `fs-wait`, of 2 threads. At each due time a waits thread
     makes one try, which is one fill of one manifest, and puts the request back with its next due
     time.
   - A waits thread is held for one try at a time, never for a whole wait.
3. **A root that appears goes back to the fetch pool for its attach.**
   - When a try finds the root, the request is parked on the fetch pool again for the attach.
   - So the waits pool never reads a tree, and at most 16 trees are read at once, as today.
   - A fetch thread is never held by a wait. Attaches of stored trees, under either name, and
     running jobs' opens of files the node must fill never queue behind a waiting lookup.
4. **`W` counts from the first try.**
   - A request whose `W` has passed is answered `ENOENT` at its next due time, whether or not a try
     is in flight.
   - With 2 waits threads and tries of a few ms each, a due time is late by at most a few tries.
     So a lookup is answered within about `W`, plus one try, plus the attach if the root appeared.
     The attach is bounded by `TREE_READ`, as any attach is.
5. **A cap of 16 lookups waiting after a miss.**
   - Only lookups whose first try missed count against it. A lookup of a stored root never does.
   - One past the cap is answered `ENOENT` at once, with a rate-limited warning.
   - The cap bounds the queue's memory and the upstream tries per second: 16 waits at one try per
     100 ms is at most 160 tries a second.
   - The cap is a backstop against a client with more threads. The 4-thread agent never reaches it.
   - Refusing at a lower cap would fail real starts. In the same model, a cap of 2 refuses
     1.3-3.5% of starts and a cap of 4 refuses 0.6-1.8%. Those are counts of modelled waits that
     overlap, before the agent's own limit of 4.
6. **The agent's threads are not bounded by tierstore.**
   - A waiting start holds one of the agent's 4 for its `R`. In the model that is 1.9-9.3% more
     lookup-thread time than the attaches already take.
   - That is a queue for other starts on the host, not a starvation: each wait ends within `W`.
   - Removing that queue is a matter of the agent's own pool size, its lookup-pool work, not of this
     design. Task 4 measures it with the pool's queue time (komira-ai/jobscheduler#1068).

**Tests that hold the bounds.** In tierstore, on the existing rigs, with progress by events and the
injected clock. No wall time is measured.

- **A stored tree is answered while the waits are full.**
  - Set up 1 fetch thread and 1 waits thread. Fill the queue with waiting `.writing` lookups whose
    tries are held on a gate.
  - Then a lookup of a stored tree is answered while they still wait, under the plain name and
    under `.writing` alike.
  - This fails if a wait holds the fetch thread.
  - The rig and the helpers: `node_fs_stall_tests.rs:17-24` `hand`, `fetches_tests.rs:15-50`, and
    `testing.rs:947-953` `until`.
- **A found root is attached on the fetch pool.** With the fetch thread held on a gate, a waiting
  lookup whose root has appeared is not answered until the fetch thread is free. With the waits
  thread held instead, it is answered.
- **The cap.**
  - The 17th lookup to miss is answered `ENOENT` before any gate opens, with the clock unmoved and
    no try made.
  - A 17th lookup of a stored root is answered with its tree.
- **From the first try.** With 1 waits thread held in a try, a second waiting lookup whose `W`
  passes is answered `ENOENT` at its next due time on the injected clock, not after the held try
  ends.
- **Draining.** The fs's drain returns only after every waiting lookup is answered.
- **Memory.** After a `.writing` lookup of an absent root is answered `ENOENT`, the trees in memory
  are as before: `footprint()`, `core/memory.rs:388-399`. Once the root is stored, a `.writing`
  lookup and a plain lookup of the same digest leave one tree in memory, with one tree's
  `heap_bytes`.
- **Two traps in the existing rigs**, which the tests must avoid:
  - `Wire::deliver` waits until no request is parked (`node_fs_tests.rs:316-319`). So the waits
    pool's count must stay out of `parked()`, or the tests use `hand` and wait on the reply.
  - `Rig::new`'s pause does nothing and its clock never moves (`testing.rs:646-652`). So the wait
    tests need a core whose pause moves the clock, such as `core_over` in
    `core/tree_read_tests.rs:39-69`, moved into `testing.rs`.

## 6. Comparison and recommendation

| | (a) fs waits for a `.writing` name | (b) release call | (c) root first | (d) no change |
|---|---|---|---|---|
| a start never fails for a tree still being written | yes, within `W`; past `W` it fails as today and is retried | yes | no: it becomes `EIO`, at the attach or mid-run | yes |
| a lost tree still fails fast | yes, under the plain name | yes | no | yes |
| an orphaned job | ends at `W` | sits until a timeout, unless new code releases or cancels it | — | none |
| contract changes | jobscheduler doc text only; tierstore's fs gains a name form | jobscheduler public API, stored job, start wire, capability | tierstore and kbf invariants | none |
| saving at the p50, placed at once | about 30 ms (per job), 31-61 ms (medians) | 9-36 ms | 0 | 0 |
| new cost | up to `W` of an agent lookup thread per waiting start (1.9-9.3% more lookup-thread time, modelled); no fetch thread; no tree memory; one upstream read per rest | a second message on the path with the 10 s tail | correctness | 50-60 ms per start |

**Recommended: (a).**

- It is the only option that saves most of the wait and keeps all three properties.
- Its contract change in jobscheduler is one sentence of documentation.
- Its interface change is additive: a new name form beside an unchanged one.
- It must be adopted in order:
  1. tierstore on every host;
  2. then the kbf flag, off by default, on one frontend lane, measured against the other.

**What it leaves.** It is worth doing only with the other start work, because by itself it moves a
start placed at once from about 0.47 to about 0.44 s.

## 7. Implementation tasks, in order

None is started. Each needs a decision first, marked **[decision]**.

1. **[decision] tierstore: a `trees/<digest>.writing` lookup waits up to `W` for its root.**
   - **Repo:** komira-ai/tierstore.
   - **Files:**
     - `crates/tierstore/src/server/treefs/core/implicit.rs`: `resolve` reads the suffix (check it
       cannot collide with a placement key, `placement_of`).
     - `treefs/core/placed.rs`: `read_tree`'s `fetch`, root only, inside the existing `Until`; the
       rests through `Fills::pause` (`treefs/fills.rs`).
     - `treefs/core/implicit.rs`: `attach_read` keeps one number when both names attach at once.
     - `treefs/node_fs.rs`, `treefs/fetches.rs`, `treefs/serving.rs`, as section 5.2 sets out:
       - the deadline queue and its 2 `fs-wait` threads;
       - a `.writing` lookup that missed handed to it, and one whose root appears parked again on
         the fetch pool;
       - the cap of 16 lookups that missed, its `ENOENT` answered at once;
       - `W` counted from the first try;
       - drain covering the queue.
     - `treefs/testing.rs`: `core_over` moved here from `core/tree_read_tests.rs`, and a wire rig
       around it with its own fetch and wait counts.
     - `docs/design/virtual-build-fs.md`: lines 462-472 and 1236.
   - **Tests that fail before**, on the injected clock in `treefs/core/tree_read_tests.rs`,
     `implicit_tests.rs` and the fs's wire tests:
     - a `.writing` lookup whose root is added after two rests resolves to the same tree number as
       the plain name (today it is `ENOENT`);
     - one never added is `ENOENT` at exactly `W`;
     - lookups of both names at the same time get one tree number;
     - the bounds of section 5.2:
       - a stored tree, under either name, answered while the waits are full;
       - a found root attached on the fetch pool;
       - the 17th lookup to miss refused at once, while a 17th of a stored root is answered;
       - `W` kept while a try is held;
       - drain;
     - the memory checks of section 5.2: no tree in memory after a refused wait; one tree for both
       names.
   - **Tests that hold today and must keep holding:**
     - a missing child is still `EIO` at once;
     - the plain name is unchanged (the existing test).
   - **Measured after:** a `waited <ms>` field on the fs's lookup line, a count of lookups that
     reached `W`, and a count of lookups the cap refused.
   - **Why it needs a decision:** a new name form in tierstore's fs namespace, which is an
     interface.
2. **[decision] jobscheduler: say in `job-rootfs.md` that a host path's file system may wait inside
   the one lookup.**
   - **Repo:** komira-ai/jobscheduler.
   - **Files:** docs only. 7.2 rule 4 (lines 989-995) and 7.4 (lines 1036-1041).
   - **Tests:** a doc change has no failing test. If no agent test covers a lookup that is answered
     late but within `LOOKUP_MS`, add one that checks the start goes on.
   - **Why it needs a decision:** it changes what a client may rely on.
3. **[decision] kbf: `--executor-submit-before-stored`, off by default.**
   - **Repo:** komira-ai/kbf.
   - **Files:**
     - `crates/kbf-server/src/executor/run.rs`: `fresh()` writes the row, then submits, then
       awaits `tree_written` and withdraws the job on a failure;
     - `executor/traits.rs` and `executor/front.rs`: the mint, or `Inputs::tree`, also says
       whether the tree's writes are pending;
     - `executor/out.rs` and `executor/submit.rs`: the `.writing` layer, chosen in `prepare`;
     - `executor/config.rs` and the CLI flag;
     - docs: `docs/design/actions-as-jobs.md` 4.1, the `Ingest::stored` doc,
       `docs/deploy/kbf-modes.md`.
   - **Tests that fail before**, in `crates/kbf-server/src/executor/run_tree_tests.rs`:
     - with the flag on and the writes held, the job is submitted and names
       `trees/<digest>.writing`;
     - a failed write after the submit sends one `CancelJob` and answers as `not_minted` says;
     - a cancel during the write withdraws the job;
     - a memoised tree names the plain path;
     - a `.writing` start that fails `input-unavailable` runs its next attempt under the plain
       name once its tree is stored.
     - With the flag off, every existing test passes unchanged.
   - **Measured after:** none until task 4.
   - **Why it needs a decision:** a new CLI flag.
4. **[decision] farm: turn the flag on for one frontend lane once every Linux host runs task 1's
   tierstore.**
   - **Measure for at least 30 minutes of light load, against the other lane, per job:**
     - op → running and `start_ms` p50 / p90 for starts placed at once;
     - the share of `.writing` lookups that waited, and their wait p50 / p90;
     - `input-unavailable` by attempt, which should stay at about zero on attempt 0;
     - the agent lookup pool's queue time (komira-ai/jobscheduler#1068);
     - the waits pool's most at once and its refusals, which should be none;
     - the p90 of running jobs' cold opens, against the other lane;
     - each host's `trees in memory` line (section 5.1), to replace the inferred tree counts and
       bytes with measured ones.
   - **Success:** a p50 saving of at least 25 ms, no rise in start failures, and no refusal by the
     cap.
   - **Why it needs a decision:** it changes the farm.
5. **[decision] the Mac form: the same flag for the mount provider.**
   - **Repos:** komira-ai/tierstore (`server/provider/prepare.rs`: a `ref` marked as being written
     makes `Prepare` wait for its list within a bound, instead of refusing
     `no tree list at <key>`) and komira-ai/kbf (the provider layer's `ref`).
   - **Tests:** the same pattern as task 1.
   - **Why it is last:** it can only be measured once the Macs' starts are in the per-job join.

## 8. Decisions

- **D1. The flag is in the name.**
  - The fs sees nothing of a job but the name (section 1.3), and the agent's allowed roots
    (`--mount-root-ro=trees=…`) already admit any name under `trees/`. So a suffix needs no
    jobscheduler code and no host configuration.
  - A marker object in the store would cost the round trip it is meant to save, and would outlive a
    crash.
  - A wait on every absent root would slow down every real miss.
- **D2. `W` = 10 s is a constant, not a setting.**
  - It must stay well inside the 30 s `TREE_READ` it shares and the agent's 60 s.
  - The measured tree waits all ended within 2.5 s.
  - A setting would answer a slow store with a knob instead of fixing the store.
- **D3. Rests of 10 ms doubling to 100 ms.** Short enough that a start found waiting pays little.
  Few enough that a 10 s wait asks the upstream about 100 times, not thousands.
- **D4. A start that fails at `W` spends an attempt.** It is the old ENOENT's path, reached only by
  a write slower than `W`, which did not happen in the measured window. It is counted rather than
  given a free retry.
- **D5. The row is still written before the submit.** It keeps the takeover path's rule that a
  submitted job always has a row.
- **D6. A lookup that missed waits in a deadline queue, not on a thread.**
  - **A cap on the fetch pool alone** still lets waits hold the fetch threads that stored attaches
    and cold opens need. With one fetch thread, one wait would block everything.
  - **A waits pool that held a thread per wait, and attached there**, was rejected too:
    - a waits thread could be held for a whole `TREE_READ` by an attach;
    - lookups queued behind it would then pass their `W` unanswered.
  - **The queue** keeps both pools' work short: fetch threads read trees; waits threads make one
    try at a time.
  - **The sizes:** 2 waits threads, because a try is a few ms. The cap is 16, which the agent
    cannot reach. It is a backstop, not a limiter: in the model a cap at 2 or 4 would fail 0.6-3.5%
    of starts.
  - All three are constants, for the reason of D2.
- **D7. No cap on tree memory is added.**
  - (a) adds no tree to memory.
  - Today's cost is about 0.9-1.6 GB at the busiest host's peak, inferred, against about 250 GiB that
    the job service schedules there.
  - Task 4 reads the fs's own memory line on each host to confirm that with measured numbers.

## 9. What this does not cover

- **The REAPI part of the start.** The Execute's authorization, its reads and the input walk before
  the executor.
- **The tree attach itself** (108 ms p50).
- **The 10 s delivery step.** Separate work removes these.
