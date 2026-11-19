# kbf calls the job service directly

Status: **BUILT.** A living document: it is changed in the pull request that changes what it describes.

kbf no longer schedules. Every REAPI Execute that the action cache does not answer is one job of jobscheduler's job service,
submitted at once; kbf reads the job back until it ends and answers the operation. There are no leases, no virtual workers,
no slots, no `Start` window, no self-fence, no farm rerun budgets and no queue in kbf: how many jobs run, where and when is
the job service's.

This page replaces, for what kbf does with an Execute, the farm scheduler of [scheduler.md](scheduler.md), the lease protocol of
[worker-protocol.md](worker-protocol.md), the virtual workers and ownership modes of [n-frontends.md](n-frontends.md) and the
farm parts of [actions-as-jobs.md](actions-as-jobs.md) (sections 2 and 6). Those pages are kept as the record of the earlier
design; each says so at its top.

## 1. What an Execute does

```mermaid
sequenceDiagram
    participant C as Client
    participant F as Frontend (kbf-front)
    participant X as Executor (kbf-server)
    participant S as State store
    participant J as Job service
    C->>F: Execute(action)
    F->>F: action cache, inputs present
    F->>X: Dispatch::submit
    X->>X: join an open operation of the same action, or open operations/<id>
    X->>X: prepare the run: read the action, mint the key, write the tree
    X->>S: exec/op/<id> {owner, action, attempt, job, key}
    X->>J: SubmitJob(k<frontend hash>-<id>-<attempt>)  (no CPU, no memory; size-class and size-fallback labels)
    loop every --executor-poll-ms
        X->>J: GetJob
    end
    X->>J: GetResult
    X->>X: ingest the pack (writes the action cache)
    X->>S: delete exec/op/<id>, put done/<id> and doneof/<id>
    X-->>C: the operation, done
```

- **Joining.** An Execute of an action that is open on this frontend joins its operation: one job, every caller answered.
- **The job.** As before ([jobs-on-jobscheduler-and-tierstore.md](jobs-on-jobscheduler-and-tierstore.md)): the kbf image or the
  action tree, no CPU and no memory, labelled `size-class` with the build target and whether it is a test (the job service
  sizes it from that class's history), `size-fallback` with its kind, what runs (whose history sizes it while the target has
  too few runs; jobscheduler's `docs/design/sizing.md`, section 7.8), `kbf-frontend` and `kbf-instance`. A platform that names `kbf-lease=whole_machine` gets a whole host.
- **The id.** `k<first 8 hex of sha256(--executor-frontend-id)>-<operation>-<attempt>`: a submit whose answer was lost is sent
  again unchanged and makes no second job, and a frontend that takes the operation up computes the same id. It has three parts
  where the ids of earlier releases had four.
- **Watching.** The pinned job service has no watch call; each operation's task reads its job every `--executor-poll-ms`
  (250 ms by default) until it ends, then reads its result once.

## 2. Endings, retries and cancel

`executor/outcome.rs` keeps the table of how a job ends. A clean result (succeeded, exit 0) is ingested and answers the callers.
An ending of the farm's (the host lost, the job unknown to the service, refused at its start, the task tool failing, a pack the
ingest refuses, killed at its memory limit) runs the operation again as its next attempt, a new job and a new key, at most
**2** times (`RESUBMITS`); then the callers are answered `UNAVAILABLE` (or, for memory, `FAILED_PRECONDITION` with
`ACTION_OUT_OF_MEMORY`). The action's own timeout is `DEADLINE_EXCEEDED`, and an action that cannot run here
`INVALID_ARGUMENT`, at once. Each attempt writes under its own create-only key, so a zombie of an earlier one never writes into
a later one's result.

`CancelOperation` cancels the job (`CancelJob`) and answers the callers `CANCELLED`. A stream that ends does not cancel.

## 3. Over a state store: answers by name, restarts and N frontends

| Key | What |
|---|---|
| `meta/next_op` | the shared counter operation ids are reserved from, in blocks of 1024 (the format of earlier releases) |
| `exec/live/<instance>` | this process is alive; renewed every quarter of `--executor-live-secs` (20 s), with that time to live |
| `exec/op/<operation>` | an unfinished operation: owner, instance, action, attempt, job id, put directory (version byte + JSON) |
| `exec/act/<hash>-<size>/<instance, hex>` | the unfinished operation of an action (version byte + 8-byte id), written with the first attempt's `exec/op/` row and deleted with its answer |
| `done/<operation>`, `doneof/<operation>` | how it finished and what it ran (the format of earlier releases), at least 10 minutes |

- **By name.** `WaitExecution` and `GetOperation` of `operations/<id>` are answered from memory, else from `done/`; an
  operation another frontend runs is followed until its `done/` row appears.
- **Restart.** A process that ends leaves its rows. Once its liveness row has expired (on the store's clock), any frontend of
  the store takes each of its `exec/op/` rows with one compare-and-set and reads the job back where it is. A job that commits
  into a put directory is finished from it; one that writes to a presigned url, which only the process that held the url can
  finish, is cancelled and run again. Clients reattach with `WaitExecution`. Nothing is cancelled when a frontend stops: a
  stop deletes the liveness row, so another frontend takes its operations up at once.
- **One job per action.** An Execute of an action that another frontend of the store runs follows that operation (found
  by its `exec/act/` row) instead of submitting a job of its own, and is answered with its answer under its own name; the
  join of Executes within one process stays in memory. A follower whose operation its own callers cancelled runs the action
  itself. A row naming an operation that is no longer open (an earlier release finished it) is deleted and written again.
- **Ownership.** Only the row's owner writes it, each write a compare-and-set on the version it read with itself as owner; an
  owner that lost the row stops running the operation and its callers follow the answer the new owner writes.

What an earlier release loses by not knowing `exec/live/`, `exec/op/` and `exec/act/` (B8): nothing it reads; its own `op/` rows
are not read by this release, so a client of an operation it left reattaches by Execute (a cache hit or a new run). It writes no
`exec/act/` row, so an Execute on a frontend of this release does not join an operation an earlier release runs: the join across
frontends is lost while both run, nothing else.

## 4. What is kept as a bound

Nothing counts jobs. The one bound is a memory guard: at most 65 536 open operations per process (`MAX_OPEN`), past which an
Execute is refused `UNAVAILABLE`.

## 5. Measured

`kbf_executor_phase_seconds` has `submit` (the Execute to the job service's answer to its submit) and `execute` (the Execute
to its callers answered) beside the earlier phases; their medians, before and after on the farm, are the numbers this change is
judged by.
