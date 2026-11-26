# The provider input-tree form on Linux, measured against the manifest form (item 1a)

What item 1a of [`docs/design/one-file-action-tree.md`](../design/one-file-action-tree.md) (section 6)
asks to be measured once Linux jobs use `executor-input-tree = "provider"`, on real traffic, read
only. Every number says whether it was **measured** (read from a log or a counter) or **inferred**
(worked out from measured numbers). A `p50/p90` pair is a per-job quantile, nearest rank.

## The answer

- **Item 1d: go.** The design builds item 1d "only if 1a shows the provider's own cost". It does:
  - **The provider wait.** A start in the provider form waits **128 ms p50** for its tree, from
    `admitted` to `provided` on a start with no other start on its host (measured, at the agent).
    That wait holds the `Prepare` call (tierstore answers it at once), tierstore's placement of the
    tree (the list read, hashed and walked, and its image made), the `Mount` call that waits for
    that placement, and the agent's turns to act on each answer.
  - **What 1d would save.** Item 1d removes `Prepare`, the placement and the `Mount`, but its fs
    attach still reads, hashes and walks the list. Section 4.7's per-MB rates put that at about
    15-30 ms for a list of this size (inferred), so 1d would save on the order of 100 ms p50
    (inferred). Item 1d's own third-lane measurement settles the number.
  - **The 20 ms bar.** This pass was asked whether `Prepare`, the placement or the agent's mount
    still cost more than 20 ms p50. The design does not set that bar. The provider wait is over it
    in every load class: uncontended 128 ms, light 150, burst 183.
  - **The agent's mount** is not separated by any log line. It sits in `resolved` to `created`, with
    the container's create, and that whole interval is 28 ms p50 uncontended in the provider form,
    against 32 ms in the manifest form (measured).
- **The attach moved; it did not shrink.** The FUSE open of the tree, which is tierstore's whole
  attach in the manifest form, fell from **123 ms to 1 ms p50** uncontended (measured). But the
  provider wait took its place: a host's placed-to-running is **214 ms against 207 ms p50**
  uncontended (n = 1,164 and 454, measured), a difference this pass cannot resolve. The design
  expected about 65 ms saved on a light load (section 4.7, inferred there).
- **Start to running on the provider form.** This covers the starts the hosts received from
  17:18:44Z to 18:13Z (n = 7,514).
  - A host's placed-to-running is **931 ms p50 / 11,306 ms p90**, and **207 / 764 ms**
    uncontended (measured).
  - From the Execute's arrival at kbf, running is 39.2 s p50 / 160 s p90 (attempt 0, n = 7,297,
    measured). Most of that is the wait for a host: submitted to placed is 2.3 s p50 and 144 s p90.
- **Memory.** The fs's trees take about the same room, laid out differently (measured; p50s do not
  add):
  - Heap index and mapped lists together are **668 MiB p50 in the manifest form and 659 MiB in the
    provider form** per host, while the provider window held more trees (396 against 326 p50).
  - The heap part fell from 668 MiB to 97 MiB. 550 MiB of lists were mapped, of a 2,048 MiB
    budget, and none was released for it.
  - tierstore's resident memory, which holds the fs, was 3.8-4.5 GiB per host for the instance
    that ran 68 minutes, about 56 of them in the provider form (measured once, at the end). It was
    not sampled before the switch.
- **`list-refused`: none. `input-unavailable`: 2** in the provider window, both on attempt 0, against
  17 in the manifest window, all on attempt 0 and all inside one tierstore restart (measured).
- **kbf's own side is not decided by this pass.** kbf's phase `start` was slower in the provider
  window: 750 ms against 244 ms p50, attempt 0.
  - So was every phase the form does not touch, such as REAPI prepare (511 against 95 ms).
  - The provider window also held a store incident (below).
  - The split that would decide it, `tree_list` and `tree_sidecar`, is on the frontend's metrics
    endpoint, which this pass could not read.

## What was compared, and how it differs from the plan

The plan is one frontend lane in the provider form against another in the manifest form, at the
same time.
- **The rollout switched the whole frontend at once.** At 17:18:44Z the frontend's one task was
  replaced by two, both in the provider form, so no Linux job ran in the manifest form after it.
- Every one of the 7,514 starts after the switch carries the agent's `provided` step, and none of
  the 15,278 before it does (measured, from the `Started` line).

So this pass compares **before and after the switch, on the same code**:

| | manifest form | provider form |
|---|---|---|
| window (UTC), by the time the host received the start | 14:50:00-17:18:44 | 17:18:44-18:13:00 |
| kbf frontend | komira-ai/kbf#2437's merge | the same; from 17:35Z komira-ai/kbf#2438's merge, which adds one test file and no production code |
| frontend tasks | one | two (replaced by two more at 17:35-17:36Z) |
| tierstore | 0.3.185; 0.3.186 from 17:07Z | 0.3.186 |
| js-agent | 0.1.416; 0.1.417 from 16:53Z | 0.1.417 |
| hosts | the same five Linux build hosts, A-E | the same |
| starts | 15,278 | 7,514 |
| starts joined to kbf's attempt-0 line | 14,799 | 7,297 |

- The provider window ends at 18:13Z, where tierstore 0.3.187 started on every host.
- Of the manifest window's starts, 462 have no kbf line at all: their Executes reached kbf before
  14:50Z, where the kbf log read begins. The by-load columns hold joined starts only.
- **Load windows.**
  - A minute is light when at most 40 Executes arrived at kbf in it, and burst when at least 200
    did.
  - The provider window has **10 light minutes and 20 burst minutes**, not the 30 and 30 the plan
    asks for. The manifest window has 25 and 44 (measured).
  - The answer for item 1d does not depend on this: the provider wait is over 20 ms p50 in every
    class.
- **Uncontended** starts are those whose host ran no other start between this one's arrival and its
  `running`: 1,164 in the manifest form and 454 in the provider form.
  - They compare the forms without the host's queue, which differed between the windows (below).
  - "Uncontended" means no other start on the host. It does not mean an idle tierstore: earlier
    trees' fills still share it.

## The host's start, step by step (measured)

From the agent's `Started` line. Its steps are milliseconds since the agent received the job:
- `taken`, then `admitted`;
- `dirs`: the scratch directories are made;
- `provided`: the provider's `Mount` answer is in (the provider form only);
- `resolved`: every mount's host path is found, and the mounts' assembly is asked for;
- `created`: the container is made.

The agent sends `Prepare` right after `admitted`, before it makes the directories. `dirs` follows
`admitted` by 3 ms p50, so neither the `Prepare` call nor the agent's record of it holds the start
up. `open_ms` is the job's runtime's FUSE open of the tree. In the manifest form it is tierstore's
whole attach (manifests read and the tree built); in the provider form the tree was placed before.

**Uncontended** (the like-for-like comparison):

| p50 / p90, ms | manifest | provider |
|---|---|---|
| starts | 1,164 | 454 |
| `admitted` to `provided` (the provider wait) | - | **128 / 346** |
| `provided` to `resolved` (the bound directory's lookup) | - | **5 / 12** |
| `dirs` to `resolved` (in the manifest form: the tree's lookup) | 129 / 438 | - |
| `open_ms` (FUSE open) | **123 / 309** | **1 / 4** |
| `taken` to `resolved` | 130 / 440 | 135 / 395 |
| `resolved` to `created` (the mounts' assembly and the create) | 32 / 191 | 28 / 228 |
| received to `running` | 191 / 732 | 179 / 687 |
| placed to `running` | **214 / 817** | **207 / 764** |

Every joined start, by load:

| p50 / p90, ms | manifest all | manifest light | manifest burst | provider all | provider light | provider burst |
|---|---|---|---|---|---|---|
| starts | 15,278 | 367 | 9,430 | 7,514 | 84 | 6,246 |
| `admitted` to `provided` | - | - | - | 183 / 1,277 | 150 / 552 | 183 / 1,262 |
| `dirs` to `provided` (the wait for it, past the directories) | - | - | - | 171 / 1,240 | 137 / 455 | 171 / 1,192 |
| `provided` to `resolved` | - | - | - | 13 / 699 | 11 / 291 | 14 / 757 |
| `dirs` to `resolved` | 304 / 1,243 | 501 / 2,374 | 316 / 1,304 | - | - | - |
| `open_ms` | 233 / 787 | 304 / 916 | 242 / 831 | 2 / 51 | 2 / 13 | 2 / 56 |
| `taken` to `resolved` | 306 / 1,251 | 502 / 2,375 | 317 / 1,312 | 243 / 2,005 | 164 / 939 | 246 / 2,006 |
| `resolved` to `created` | 52 / 1,418 | 77 / 1,855 | 55 / 1,707 | 95 / 3,295 | 49 / 974 | 102 / 3,283 |
| placed to received | 57 / 201 | 75 / 268 | 60 / 197 | 91 / 486 | 30 / 70 | 100 / 533 |
| received to `taken` | 2 / 2,157 | 8 / 5,426 | 3 / 2,468 | 79 / 6,849 | 6 / 2,234 | 117 / 7,263 |
| placed to `running` | 631 / 4,860 | 832 / 9,144 | 690 / 5,679 | 931 / 11,306 | 398 / 7,127 | 1,041 / 11,411 |

The "manifest all" and "provider all" columns hold every start in the window; the light and burst
columns hold joined starts only.

- **Why the provider form looks slower over every start** (931 against 631 ms p50): that is the
  hosts' queue, not the tree. The agent took a received job at 79 ms p50 against 2 ms, and placed
  to received was 91 against 57 ms. Uncontended, the two forms are within 7 ms of each other.
- **Light minutes do not mean a quiet host.** A load class is the Executes' arrival minute at kbf;
  a job of a light minute can still wait behind a burst on its host.
- **Is the provider wait the agent's polling?** No, measured and from the code.
  - The agent steps on a 1 s tick, but a provider answer wakes it at once.
  - The wait's distribution is smooth from 18 ms, with its mode at 110-130 ms and its p25 at 113 ms.
  - It does include the time each answer waits for the agent's worker to be free. Uncontended
    starts bound that; the all-start rows do not.
  - `provided` is marked at the agent's first step after the `Mount` answer.
- **The sum of the design** (section 4.7, inferred), for the lists measured here:
  - A list here is about 1.48 MiB and 18,500 entries (measured, below).
  - Its fill, transfer, hash and walk come to about 15-30 ms (inferred, from section 4.7's per-MB
    rates, whose fill and transfer terms were not measured on the node).
  - The provider wait measures 128 ms uncontended, so about 100 ms of it is the provider's own
    (inferred).
  - Which part that is (the fill's wait, the image write, the two calls, the agent's turns) is in
    no log line. tierstore's metrics would say, and they need root.

## Memory (measured)

**The fs's trees**, from tierstore's periodic `tierstore fs:` line (one per host per minute or so).
The heap part is the trees' index; the mapped part is the tree lists under `fs.tree_list_ram_bytes`.

| per host, p50 / max | manifest | provider |
|---|---|---|
| samples (five hosts) | 685 | 248 |
| trees in memory | 326 / 661 | 396 / 729 |
| entries | 5.0 M / 11.0 M | 7.1 M / 14.5 M |
| heap, MiB | 668 / 1,464 | 97 / 338 |
| lists mapped | 0 | 385 / 729 |
| mapped, MiB (budget 2,048) | 0 | 550 / 1,159 |
| heap and mapped, MiB (its own p50) | **668 / 1,464** | **659 / 1,316** |
| released for the budget | 0 | 0 |
| MiB per list | - | 1.48 / 2.83 |
| entries per tree | - | 18,496 / 21,825 |

- Per host the totals are 594-708 MiB p50 in the provider form, against 613-735 MiB in the manifest
  form. That is at a heavier load: more trees and more entries.
- Section 4.6 of the design expected the heap copies to go and the mapped pages to stay under the
  budget. Both hold.

**tierstore's resident memory** (the fs runs inside tierstore's process):
- **After the switch.** The process RSS was 4.46, 4.27, 3.82, 4.35 and 4.15 GiB on hosts A-E
  (measured).
  - It was read at 18:15Z, from the 0.3.186 instances. Those started at 17:07Z and stopped at
    18:16-18:18Z, once 0.3.187 had taken over.
  - At the read they had run 68 minutes, about 56 of them in the provider form.
- **Before the switch** it was not sampled: no log line carries it, and this pass began after the
  switch. One host read 3.19 GiB at 17:22Z, 15 minutes after its tierstore started and 3.5 minutes
  into the provider form.
- **The service's memory peak** counts the page cache, so it bounds RSS from far above (measured,
  from systemd's account at each stop):
  - 27.9-36.2 GiB (median 33.0) for the instances that ran 16:15-17:11Z, only in the manifest form;
  - 30.6-45.3 GiB (median 35.1) for those that ran 17:07-18:18Z, mostly in the provider form.
  - The second set ran 15 minutes longer, so this does not separate the forms.

## `list-refused` and `input-unavailable`, by attempt (measured)

| | manifest window | provider window |
|---|---|---|
| `list-refused` (kbf's outcome, the agent's refusals, tierstore's `not supported` / `no tree list`) | 0 | 0 |
| `input-unavailable`, attempt 0 | 17 | 2 |
| `input-unavailable`, attempt 1 or later | 0 | 0 |

- The manifest window's 17 were all at 16:20-16:21Z, during one tierstore instance hand-over.
- The provider window's 2 were at 17:29:55Z, on one frontend task, with no tierstore restart
  under way.
- A third, at 18:14:26Z, fell in the 0.3.187 hand-over, after the window.

## kbf's side: phase `start` (measured, not attributed)

From kbf's per-job `the job service took the job` line, for attempt-0 Executes that arrived in the
window:

| p50 / p90, ms | manifest (n = 20,732) | provider (n = 9,148) |
|---|---|---|
| REAPI prepare (`presubmit_ms`; the form does not touch it) | 95 / 1,306 | 511 / 2,832 |
| executor prepare (`prepare_ms`; the form does not touch it) | 54 / 384 | 198 / 653 |
| executor `start` (`start_ms`; it holds the tree write) | 244 / 1,703 | 750 / 13,549 |
| `start` over 5 s | 468 (2.3%) | 1,630 (17.8%) |

- **Every phase rose,** the two the form leaves alone among them. So the frontend and its store were
  slower in the provider window for reasons besides the tree.
- **The rise began before the switch.** REAPI prepare was 247 and 394 ms p50 in the 10-minute
  buckets of 16:30Z and 16:40Z.
- **Both incidents (next section) touched these numbers.** The manifest window ended in the first,
  and the provider window's first ten minutes drained its backlog. The provider window held the
  second.
- **What would attribute it:** kbf's `tree_list` and `tree_sidecar` steps, and the slot and 503
  waits of `kbf_executor_tree_write_seconds` (komira-ai/kbf#2437).
  - They are only on the frontend's metrics endpoint, which is reachable only inside its
    container's network namespace and needs root.
  - This pass did not read them.
- **So item 1b's size is still the design's bound** (at most 60 ms at the p50, section 6), not a
  measurement.

## Incidents in the windows (measured; not caused by the form)

1. **The frontend's token rotation, 17:09:24-17:18:45Z, at the end of the manifest window.**
   - The job service answered the frontend's one task `Unauthenticated` until the two new tasks
     started (measured). Its cause, that the job service already asked for the new client token
     and the old task still sent the old one, is inferred from the order of the rollout.
   - 726 runs were run again and **363 operations finished failed** (`code=14`, `the job service did
     not take the job (Unauthenticated)`).
   - It is a rollout ordering effect, the same for either form.
2. **A store write stall and a 0-byte blob, 17:46-18:01Z, in the provider window.**
   - 17:46-17:58Z: 240 runs were run again because `the run's action tree was not written within
     the preparation's limit`. Each of those 226 operations was taken later.
   - 17:49Z: kbf's write of one 232 MB input blob to `cas/l/` was answered
     `400 RequestTimeout: No byte of the request body arrived within 30s`. From then on the store
     held a **0-byte object at that blob's key**.
   - Every later action naming the blob was refused by kbf's placement check (`cas/l/ holds an
     object of 0 bytes for <digest>`): **205 operations finished failed** (`code=14`) up to
     18:01:34Z, all on that one blob.
   - The check and the write are on the blob path, before either form's tree is written, so this is
     not the provider form's cost.
   - That a timed-out write leaves an empty object at the blob's key is a defect of its own, to be
     followed up apart from this page.

## What was not measured, and why

| number | why not |
|---|---|
| `tree_list`, `tree_sidecar`, the tree writes' slot and 503 waits | the frontend's metrics endpoint needs root (above) |
| tierstore's own placement time, apart from the agent's wait on it | tierstore's metrics are not readable without root; no log line carries a duration |
| the agent's mount, apart from the container's create | both are in `resolved` to `created`; no step marks between them |
| a lane in the manifest form at the same time as the provider lane | the whole frontend was switched at once |
| 30 light and 30 burst minutes in the provider form | the window from the switch to tierstore 0.3.187 held 10 and 20 |
| tierstore's resident memory before the switch | not sampled; no log line carries it |
| the hosts' capability `mount-provider.tierstore` (step 4 of 4.3) | not re-read. Inferred to hold: every Linux job after the switch was placed and started in the provider form, which placement allows only on a host that reports it |
| a sixth Linux host | it was not reachable; its jobs, if any, are not in the join |

## How it was read

All read only, as an unprivileged user, with nothing deployed, restarted or written:
- **kbf:** the frontend tasks' logs from 14:50Z on (`the job service took the job`, `the run ended`,
  `finished failed`, the outcome lines), for the task of the manifest form and the four tasks
  since.
- **js-agent:** `job: Started` lines from each host's journal, 14:50Z on.
- **tierstore:** each host's journal (`tierstore fs:` memory lines, `lookup of`, `mount provider` and
  refusal lines), the process RSS at 18:15Z, and systemd's account of each instance at its stop.
- **Versions:** the packages' install log on each host, and the frontend's first log line.
- **The join:** kbf's job name against the agent's `run_id`.
  - A start is in a window by the time its host received it.
  - A load class is the arrival minute of its Execute at kbf.
- **What each step means** was read from js-agent 0.1.417 and tierstore 0.3.186.
