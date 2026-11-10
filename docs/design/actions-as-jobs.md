# Every build action is a jobscheduler job on Linux: inputs from tierstore's filesystem, outputs to tierstore

Status: **DESIGN, for the owner's review; nothing here is built and no code changes with this document**, so no
existing document is stale yet; section 11 names the documents each build item updates. Linux only; macOS is section
14, a list of open points and no more. Written against kbf `main` at `6ad2b5931`, jobscheduler `main` at `f4924e67`
(kbf's pin, release 0.1.218, is `0c104a1d`, four commits earlier, with the same public job API), the open pull request
komira-ai/jobscheduler#842, jobscheduler's `docs/design/tree-inputs.md` (cited "js TI *n*") and tierstore's
`docs/design/virtual-build-fs.md` as merged in komira-ai/tierstore#274 and revised after it (cited "ts *n*").

Claims are tagged **[code]** (read in the named file of this repository, or of jobscheduler or tierstore where named),
**[js design]** / **[ts design]** (stated by those designs and not built) or **[proposed]** (this document).

## 0. What changes, and what does not

**Before.** Each Linux node runs `kbf storage-and-daemon`; the daemon holds a `kbf.worker.v1` session to the frontend,
copies or hard-links each action's input root into a scratch directory, runs the action in a rootless `crun`
container, and uploads the outputs over the worker listener (`crates/kbf-daemon`, `crates/kbf-driver-container`)
[code]. A second executor is built and opt-in: `--executor js-agent` makes every placed action a job of jobscheduler's
job service, whose job runs `kbf-task`; the task downloads the inputs over HTTPS into scratch and PUTs the outputs to
the lease's fixed set of presigned keys (one or more packs and any large objects, pack 0 last), which the frontend
verifies and turns into CAS entries and the action-cache entry (`crates/kbf-server/src/executor/`, `crates/kbf-task`,
`docs/deploy/job-service-cutover.md`) [code].

**After.** On Linux every action is a job (the second executor, made the only one). The job names the action's input
root as a tierstore tree; the node's `js-agent` attaches it from `tierstore fs` and gives it to the job as its writable
exec root (an overlay over the read-only tree), so nothing is downloaded or copied before the action starts and only
the files the action opens are fetched. `kbf-task` keeps its download for the cases where the tree could not be
attached (the agent says so in `JS_TREES_UNATTACHED`) or was not built (section 4.2), and for the time before
`tierstore fs` is on the nodes. Outputs go where they go today: create-only PUTs to the job's own keys, then the
frontend's ingest.

**What does not change.** The REAPI surface (Execute, WaitExecution, the action cache, the CAS), the answers clients
get (section 6), the action-cache rule (exit 0 and not `do_not_cache`), the frontend's in-flight deduplication, the
memory ladder and the farm's rerun budgets, the Macs (still `kbf-daemon`), and the storage half of the Linux nodes.

Contents: 1 rulings; 2 where things stand; 3 action to job; 4 inputs; 5 outputs; 6 failures and retries; 7 what is
deleted and what stays; 8 the flip; 9 tests; 10 numbers; 11 build items; 12 decisions; 13 what this asks of
jobscheduler and tierstore; 14 macOS later and other open points.

## 1. The rulings this starts from

Settled by the owner and not reopened here:

| Id | Ruling | Where it lands |
|---|---|---|
| O1 | kbf is a jobscheduler **client** only (2026-11-03, reversing 2026-10-29): no js server libraries inside kbf; kbf submits jobs through the client generated from jobscheduler's service definition. `js-agent` is standalone; `crun` with OCI images is the Linux runtime. | section 3 (`kbf-job-client` over `js-proto`), section 7 |
| O2 | A kbf job writes only to its own fenced object-store keys; turning them into CAS and action-cache entries is a trusted server-side step in kbf (2026-10-29). | section 5 |
| O3 | Inside a job only `kbf-task`'s fetch and upload phases use the network; the action command runs network-less (2026-10-30). | sections 3.4, 4.5 |
| O4 | No per-job disk quota; the farm pool books no disk (2026-11-09). | section 3.3 (kbf stops booking disk) |
| O5 | tierstore's filesystem is FUSE on Linux, everything under the hood in tierstore (2026-11-09). Until it is on the nodes, kbf keeps copying inputs to scratch; that interim must keep working. | sections 4.6, 8.3 |
| O6 | Flip fast, fix forward, one-line rollback, no shadow or gradual rollout for the flip itself (2026-10-28). | section 8 |

## 2. Where things stand

**kbf** [code]:

- `--executor` (`crates/kbf-server/src/executor/mod.rs`, `ExecutorKind`) picks `kbf-daemon` (the default) or
  `js-agent`. `kbf-deploy job-service apply|back-out` is the held procedure around the switch, with a canary job
  (`docs/deploy/job-service-cutover.md`, `crates/kbf-deploy/src/cutover*.rs`).
- In `js-agent` mode the frontend's farm (`crates/kbf-server/src/farm.rs` over `kbf-sched`) still decides **which
  operation runs and how often**: deduplication, the memory ladder, `FARM_RERUNS` (2) and `NOT_STARTED_RERUNS` (3).
  It places each lease on one of `--executor-hosts` virtual workers `js:<frontend>-<n>`, which hold no host and bound
  how many actions are in flight; the job service decides the host (`executor/host.rs`).
- The job is the kbf image (`--executor-kbf-image`, entrypoint `/usr/bin/kbf-task`,
  `crates/xtask/src/pkg_image_asset.rs`), built on the base image the actions' Commands see. Its id is
  `k<8 hex of sha256(frontend id)>-<term>-<seq>-<attempt>` (`executor/submit.rs`, `job_id`; the attempt is always 0
  today), which makes the submit idempotent. The read grant and the presigned PUTs travel as job files or, where the
  operator allows it, arguments.
- Before it submits, the frontend reads the Action, the input root and every `Directory` under it and checks every
  file is held (`crates/kbf-front/src/prepare_run/closure.rs`, `collect_closure`); it keeps only the digests.
- `kbf-task` reads the Action, the Command and the input root through the read grant (or, with `--locate`, straight
  from the store by pack span), lays the inputs out under `/scratch/work/in`, runs the Command in its own user and
  network namespace (loopback only; `kbf-network=allow` lifts it per action), collects outputs, writes packs with the
  manifest at offset 0, and PUTs them (`crates/kbf-task/src/lib.rs`, `task.rs`). Its exit code is the contract: 0 the
  action ran (its own exit code and a timeout are in the manifest) and its pack is stored; 10 fetch, 11 run start,
  12 pack, 13 put, 14 setup, 15 no confine.
- kbf#2287 lists six things the real job service at the pin does not offer yet: `Job.start_failure_detail` is always
  empty; label values must be lower case; js-sched admits a job that books disk only on a host reporting `disk-gib`;
  a client token sees only its own jobs; the request body limit is 64 KiB; `GetLogs` without a stream has no live
  output.

**jobscheduler**: the public `JobSpec` has no input or output field at the pin or on `main` [code: `js-proto`, the two
trees' protos are byte-identical]. js TI designs `JobSpec.trees` (`TreeInput { digest, dest, writable,
job_stages_if_unavailable }`), `JobResult.input_failure`, `RESULT_REASON_INPUT_FAILED`,
`REFUSAL_CODE_INVALID_TREE_INPUT`, the capability `tierstore-trees`, the attach in the agent's preparing stage and the
detach at every end [js design]. komira-ai/jobscheduler#842 (open) is its build item 1: the agent wire and the
`tierstore-trees` key in `js-caps`. Items 2 to 4 (public API, the agent's tierstore client, `crun`) are not filed. js TI
9 leaves "naming its trees in the job and dropping its own download when the tree attached" to kbf: this document.

**tierstore**: the manifest reader and flattener, the fill socket, pins, `tierstore fs` on Linux (the 6.8 mode and
passthrough), crash recovery and the live hand-off are merged [code: `crates/tierstore/src/tree_manifest.rs`,
`src/server/treefs/`, `src/server/fill_socket/`]. The unit `tierstore-fs@.service` ships in the hostkeeper package but
starts only where a marker file exists, and the update hook does not act on it yet
(`crates/tierstore/docs/update-hook.md`). tierstore does not read REAPI: a tree is named by the SHA-256 of its root **tree manifest**, a JSON
listing per directory whose files name their blob and, optionally, where its bytes are (`source: {key, offset}`);
writing manifests is the client's job (ts 6.1; ts 6.7, "the executor's other half"; its build item 10) [ts design].

## 3. One action, one job

### 3.1 The mapping

**One action per job** [proposed, D1]. A farm lease is one job, as today. Batching actions into one job would put
several actions' outputs behind one key set and one exit code, mix their failures, and need a second scheduler inside
the job; the per-job cost it would save is measured in section 10 before anyone argues for it.

The job of an action whose input root has a tree (section 4) [proposed]:

| `JobSpec` field | With a tree | Without (today's job) |
|---|---|---|
| `job_id` | `k<8 hex>-<term>-<seq>-<attempt>`, attempt 0 (1 for the resubmit of section 6) | the same, attempt 0 |
| `image` | the kbf image, by digest | the same |
| `command` | `/usr/bin/kbf-task`: with trees `command` is the whole program, an absolute path, and the image's entrypoint is not used (js TI 2.4, 2.5) | empty: the entrypoint runs. A non-empty `command` without trees is **appended** to the entrypoint (js TI 2.5), which is why the two shapes differ here |
| `args` | the task's flags, plus `--inputs tree` | the task's flags |
| `env` | empty | the same |
| `needs` | `cpu_millis`, `memory_bytes` (the hard cap a daemon would set); **no `disk_bytes`** (D3) | the same |
| `constraints` | `os=linux`, the configured `arch`, contained | the same |
| `wall_limit_seconds` | the action's timeout plus the run margin | the same |
| `network` | the operator's level, default allow-list (3.4) | the same |
| `files` | the read grant and the pack URLs, with `--executor-secret-delivery files` | the same |
| `labels` | `kbf-frontend`, `kbf-instance` | the same |
| `trees` | exactly one: `{digest: <root manifest>, dest: "", writable: true, job_stages_if_unavailable: true}` | absent |

A `whole_machine` lease (`needs.whole_host`) gets today's job, without a tree, for now (D15): js TI does not say how
the attach in the preparing stage orders with a whole-host start (`exclusive-not-ready`), and section 13 asks.

**The tree is fixed in the prepared spec.** It is built once, in the executor's `prepare`, and kept in the lease's
`Prepared` spec. A submit sent again after a lost answer sends that spec unchanged and never consults the memo of 4.3:
`trees` is part of "the same job" (js TI 2.4), so a rebuilt manifest under the same id would be `job-id-conflict`.
Every rerun of section 6 is a new farm lease, so it has a new job id and fresh manifests.

### 3.2 The image

For the first flip the kbf image stays the job's image (D16, option a): the action's `container-image` names the base,
and an action that names another is refused as today (`docs/deploy/kbf-modes.md`). The tree is the input root, never
the root file system.

Running each action in its own image is a later design (D16, option c): the job's image would be the action's, and
`kbf-task` would reach the job as a read-only tree. That needs a way to name a mount outside `/work` in the public job
API, which `TreeInput.dest` (relative to the exec root) cannot say today. Like `JobSpec.trees` itself, it is a change
to jobscheduler's public interface, and both go to the owner's review of public interfaces before they are built.

### 3.3 Resources

`cpu_millis` and `memory_bytes` are the farm's booking, as today. `disk_bytes` is dropped (D3): O4 says the pool books
no disk, and while kbf books it js-sched places a kbf job only on a host that reports `disk-gib` (kbf#2287, item 3).
With trees, the bytes of the inputs are in tierstore's cache under its own budget, and only writes and copy-ups land in
the run's scratch (js TI 6); without them the fetched inputs land in scratch as today, unbooked, as every other farm
job's writes do.

### 3.4 Network

O3 holds as it does today: the job's network level is the allow-list `kbf-deploy job-service agent-egress` prints (the
pack PUT endpoint, the read listener and, with `--executor-locate`, the store), and `kbf-task` runs the Command in a
namespace with only `lo` (`crates/kbf-task/src/network.rs`) [code]. With a tree attached the task's network use
shrinks to reading three small blobs through the read grant (4.4) and the PUTs; the job's level does not change per
job (D9), because only the job knows whether its tree attached.

## 4. Inputs

### 4.1 From an input root digest to a tierstore tree

The frontend writes the tree manifests, in the executor's `prepare`, before it submits [proposed, D4].

**The `Directory` messages.** A manifest per `Directory` needs each `Directory`'s names, kinds, executable bits,
symlink targets and child digests. `collect_closure` already reads every `Directory` of the input root before every
submit and drops each after taking its digests (`prepare_run/closure.rs`) [code]. With trees on, it keeps them for the
manifest writer: at most `--task-closure-max-digests` of them (500,000 by default), the bound the closure already
enforces, and released when the job is submitted. So the walk the frontend makes today is the only walk; what is added
is building, hashing and storing the manifests, which section 10 measures in the prepare phase. (The scope walk of the
job-facing read listener, `job_read/walk.rs`, is a different walk, made lazily when the task presents its grant; it is
not used here.)

**Where each blob is.** A manifest's file names its blob and, unless the blob is a whole object `cas/l/<sha256>`, a
`source: {key, offset}` (ts 6.1). kbf's pack index already gives that: `GET /locate` answers each digest with
`pack {key, offset, length}` or `large {key: cas/l/<hex>}` (`crates/kbf-front/src/job_read/locate.rs`) [code]. The
manifest writer uses the same two lookups (the pack index's location of a digest, and tierstore's presence of
`cas/l/<hex>` for a blob over the pack store's `max_blob_bytes`) through a frontend-internal interface, **not** the
listener's `GET /locate`, whose per-request bounds (65,536 packed digests, 256 large-object presence checks, one 10 s
store deadline; the rest answered `absent`, `job_read.rs` `Limits`) would refuse trees to exactly the large toolchain
closures this design is for. The writer asks the index in chunks of at most those sizes, under one deadline of its own
for the whole closure; a deadline that passes is "no tree" for that job (4.2), never a failed action.

One manifest per REAPI `Directory`, built bottom-up:

| REAPI `Directory` node | Manifest child |
|---|---|
| `FileNode {name, digest, is_executable}` | `{"name", "kind":"file", "sha256", "size", "exec": is_executable, "source"}`; no `source` for a large object or a file of size 0 |
| `DirectoryNode {name, digest}` | `{"name", "kind":"dir", "sha256": <child manifest's digest>, "size": <child manifest's length>}` |
| `SymlinkNode {name, target}` | `{"name", "kind":"symlink", "target"}` |

Children are sorted bytewise by name across REAPI's three node lists. A manifest has no field for REAPI's
`node_properties` (`unix_mode`, `mtime`, and named properties), so a `Directory` that sets any of them, on itself or on
a node it lists, is not written as a tree: the job is today's (`why=unrepresentable`, 4.2). The download does not
honour them either (`kbf-task` sets only the mode from `is_executable`, 0755 or 0644, `crates/kbf-task/src/fetch.rs`,
`create_file`) [code], so the fallback changes nothing an action sees; it keeps the tree from claiming a property it
drops. Without properties, a tree gives the same modes as the download and one fixed mtime (ts D10).

A manifest's digest is the SHA-256 of its bytes. Each new manifest is written with a create-only PUT to
`cas/l/<sha256>` in the store the node's tierstore reads (ts 6.1: child manifests are fetched from there), through the
frontend's existing tierstore client (the large-object backfill's, `crates/kbf-front/src/large_backfill.rs`). An
existing key is success: the key is content-bound. A PUT that fails is "no tree" for that job.

### 4.2 When a tree is not built

The executor submits **without** `trees` (today's job, inputs fetched) when [proposed, D5]:

| Reason (`why`) | When |
|---|---|
| `off` | `executor-input-trees` is off (8.3) |
| `whole_host` | a `whole_machine` lease (3.1, D15) |
| `unlocated` | a non-empty blob of the closure has no location: it is only in a storage node's write tier or an open segment, or a large blob is not in `cas/l/` yet (large objects reach it only by the backfill or the ingest's write-through) |
| `store` | the index or tierstore did not answer within the writer's deadline, or a manifest PUT failed |
| `unrepresentable` | a name tierstore refuses (`.wh.` prefix, ts 6.1), a `Directory` or node with `node_properties` (4.1), or the closure is over a tierstore limit (2^24 - 1 entries, a manifest over 64 MiB, depth over 4096) |
| `refused` | the resubmit of a tree job whose submit or start was refused (section 6) |

Each is counted (`kbf_executor_inputs_total{from="fetch",why}` beside `{from="tree"}`), because the share of actions
served by trees is one of the numbers of section 10. None of these states is a configuration mistake that a deploy-time check could fix once:
where a blob lives and whether a node's `tierstore fs` answers change while the farm runs, so the fallback is the
normal path for them, not machinery around a mistake (B20). The one configuration fact, every node reporting the
capability, is checked once at the switch (8.3). None is an error: the fallback is the path every action takes
today.

### 4.3 Naming, lifetime, cache keys

- **The name** of an action's tree is the digest of its root manifest. It is not the REAPI input-root digest: two
  manifests of one input root differ when a blob moved (a pack compacted), and that is a new tree, which costs a miss
  and nothing else (ts 6.1).
- **The frontend's memo** maps a `Directory` digest to its manifest's digest, so a shared subtree (a toolchain) is one
  set of manifests built and PUT once. A memo hit skips the build and the PUT. It is bounded by entries (oldest out
  first), and each entry expires after a fixed time of its own (10 minutes, a constant, not a flag: B20; on the frontend's injected
  clock) and **no later
  than the earliest-expiring of its children**, so a parent never outlives the freshness of what it names. Pack spans
  are not remembered anywhere else (the index is asked afresh each time a manifest is built); a pack moved within the
  memo's time is caught by the input failure of section 6, which drops every memo entry of that action's input root,
  so the rerun's manifests come from the index as it is then.
- **tierstore's cache** is keyed by blob SHA-256 (`_tiering/fill/<sha256>`), so a file read by two actions, two trees
  or two jobs on a node is fetched once per node; a tree's blobs are pinned while it is attached (ts 6.5).
- **The attachment** is the job's: the agent attaches the tree for `js.<run id>` in the job's preparing stage and
  detaches it at exit and at every other end (js TI 3.3, 3.5). kbf never talks to tierstore's control socket.
- **The manifests** stay in the store. tierstore never deletes a manifest or a blob, and the client must not delete one
  an attached tree uses (ts 6.1). kbf deletes nothing under `cas/l/` today (the `cas/b/` janitor never lists it,
  `docs/deploy/kbf-modes.md`), so a manifest lives as long as the store does; collecting `cas/l/` is an open point
  (section 14), the same for manifests as for the large objects already there.
- **The blobs a manifest names** are packs and large objects kbf already holds. A pack compacted away while a tree that
  names it is attached fails that file's open with `EIO`, recorded on the attachment (ts 6.5); section 6 turns it into
  a rerun.

A job cannot influence a tree: manifests are written by the frontend from `Directory` messages it read and checked
itself, under keys that are the manifests' own SHA-256, and the job's write grant covers only its `cas/b/` keys.

### 4.4 What the job sees

js TI 3.4: a writable tree at the exec root becomes the lower directory of the overlay the agent mounts at `/work`; the
action's writes, copy-ups and whiteouts go to the run's upper directory on the scratch volume; the FUSE mount is not in
the container's mount namespace. Nothing else of the container changes (js TI 3.2), so the run's scratch stays bound
at `/scratch`.

`kbf-task` changes its layout in both modes, so that the Command sees the same paths whether its inputs came from the
tree or from the download [proposed, D6]:

| Path | Before | After |
|---|---|---|
| input root, the Command's exec root | `/scratch/work/in` | `/work` |
| `private/` (the Command's home, temporary and cache directories) | `/scratch/work/private`, named by `HOME`, `TMPDIR`, `XDG_CACHE_HOME` | still `/scratch/work/private` on disk, never inside the input root; the Command sees it only at the paths of the next paragraph |
| stdout, stderr | `/scratch/work/{stdout,stderr}` | unchanged |
| outputs | collected relative to the input root | the same, relative to `/work` |

The exec root's absolute path then differs between executors: `/kbf/root` under the daemon
(`crates/kbf-driver-container/src/container.rs`, `EXEC_ROOT`), `/scratch/work/in` under the task today, `/work` after.
It already differs today between the two executors. An action whose outputs embed its absolute working directory (debug
info's `comp_dir`, a depfile) gives different bytes under each, so an action-cache entry may differ by executor, and
a rollback to the daemon (8.1) keeps the entries the task wrote. Hermetic builds do not depend on it; section 9's
conformance comparison reports such actions by name instead of failing on them (D6).

With `--inputs tree`, `kbf-task`:

1. takes the value of `JS_TREES_UNATTACHED`, which the binary's `main` reads once and passes to the library as a
   parameter (the library never reads the environment; the variable is set by the agent, a platform-set marker under
   S2, not configuration); if it names `.` (the exec root), the tree was not attached and the task fetches the input
   root into `/work`, exactly as without the flag (4.6);
2. otherwise reads the Action, the Command and the root `Directory` through the read grant (three small blobs, the
   fetch phase of O3) and checks that each top-level name of the root `Directory` exists in `/work` with its kind. A
   mismatch is exit 10 with the line `fetch: the input tree is missing and was not reported unattached`, so an action
   never runs over a root that lost its tree without the agent saying so. (An old server that drops `trees` is caught
   earlier, by the spec check of section 6; failing that, its job would run `kbf-task` after the entrypoint, js TI 2.5,
   and fail on its arguments before any action ran.)
3. before the Command starts, makes the parent directory of every output path and the working directory under `/work`
   (in the overlay, so in the upper directory), as REAPI asks of the worker; then runs the Command there in the
   sandbox below, and collects, packs and PUTs as today;
4. records `"from": "tree"` or `"from": "fetched"` in the manifest's existing advisory `inputs` statistics
   (`kbf-task-pack` `InputStats`, version 2 manifests), which the ingest counts (D12). `InputStats` refuses an unknown
   member (`deny_unknown_fields`), so the ingest that reads `from` ships one release before any task writes it
   (section 11, items 6 and 7).

**The Command's sandbox inside the job** [proposed, D17]. In both modes, before the tree switch exists (section 11,
item 4), `kbf-task` starts the Command in its own mount and PID namespaces, beside the user and network namespaces it
already makes (`crates/kbf-task/src/network.rs`, `CLONE_NEWUSER | CLONE_NEWNET`) [code]:

| Path in the Command's view | What it is |
|---|---|
| `/work` | the input root (the overlay), the exec root |
| `/tmp`, `/var/tmp`, `/run` | bound from `private/tmp`, `private/var-tmp`, `private/run`: on the scratch volume, writable, executable, not the job's 64 MiB `noexec` tmpfs. Actions that hard-code `/tmp` or run a program from it work as under the daemon, whose container binds disk-backed `/tmp`, `/var/tmp` and `/run` |
| `/root` (`HOME`; `XDG_CACHE_HOME` is `/root/.cache`) | bound from `private/home` |
| `/scratch` | an empty read-only tmpfs over it, mounted after the binds above |
| `/run/js-files` (the read grant and the PUT URLs) | gone: `/run` is the bind above |

So the Command cannot read the job's grants or the task's stdout and stderr files, and cannot write anywhere but
`/work` and its own private directories. The PID namespace ends every process the Command started when it exits, so no
process of the action is left running while `kbf-task` reads the outputs (`kbf-outputs` requires it). The frontend's
MAC key (`--task-secret-file`) is never in the job: it stays in the frontend.

The outputs are read by `kbf-outputs`' walk, which already opens every component relative to its parent's descriptor
with `O_NOFOLLOW` (`crates/kbf-outputs/src/collect.rs`) [code]. On Linux each of those opens becomes `openat2` with
`RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS | RESOLVE_NO_MAGICLINKS`, so the kernel refuses any path out of `/work`, a `..`
included, in an upper directory the action wrote; macOS keeps `O_NOFOLLOW`. The action controls the upper directory,
and the walk trusts nothing in it.

Known differences from a laid-out tree, measured by tierstore over this overlay (ts 6.6, Measured: linux E4) and
accepted: renaming an input **directory** with `rename(2)` fails `EXDEV` (GNU `mv` falls back to a copy); an input's
mtime is the tree's fixed one. Section 10 counts both on real builds.

### 4.5 The network inside the job, with trees

O3 is unchanged in shape: the fetch phase (three blobs, or the whole input root on the fallback) and the upload phase
use the job's network; the Command never does. `tierstore fs` fetches blobs over the node's fill socket, outside the
job: the job has no route to tierstore for reads and needs none.

### 4.6 The attached path and the `JS_TREES_UNATTACHED` fallback

Every tree is submitted with `job_stages_if_unavailable: true` [proposed, D7]. The cases (js TI 2.3, 3.1, 5):

| On the node | What happens | Inputs come from |
|---|---|---|
| `tierstore fs` answers the attach | the tree is the exec root | the tree, lazily |
| the agent reports `tierstore-trees`, and the attach fails for a reason not the tree's (socket missing or refusing, `upstream <status>`, `no space`) | the job starts with an empty exec root and `JS_TREES_UNATTACHED=.` | the task's download into `/work` |
| as above, but `tierstore fs` hangs | the same, after the agent's 120 s attach bound, which the job's wall limit does not count | the task's download, 120 s late |
| the attach answers a path that is not `<mount>/trees/<digest>` | the start fails `tree-unavailable`, with no fallback | the resubmit of section 6 |
| tierstore refuses the tree itself (`tree-invalid`) | the start fails, with no fallback | the resubmit of section 6 |
| the agent does not report `tierstore-trees` | a job with `trees` is not placed there | another node; if no registered node reports it, the submit is refused `no-capable-host` (section 6) |

The capability is a start-time fact of the agent (js TI 2.3, TI7): a node whose `tierstore fs` fails after its agent
started keeps getting tree jobs. A refusing socket costs nothing but the fetch; a hung one costs up to 120 s per tree job
on that node. The remedy is the operator's: restart that node's agent (its probe then fails and the capability is
dropped) or turn the trees switch off (8.3).

## 5. Outputs

Already built, and kept [code; O2]:

1. **The fenced keys.** Per lease the frontend mints `cas/b/<build>/<nonce>-<k>` pack keys and `-L<i>` large-object
   keys, MAC'd with `--task-secret-file` (`crates/kbf-front/src/prepare_run.rs`, `kbf-task-pack` `task_key`), and
   presigns one create-only PUT per key with a presign-only credential, good for the wall limit plus
   `--task-url-slack-secs`. The job can write those keys once and nothing else; it holds no delete right and no store
   credential (B16).
2. **The promotion.** The ingest starts only from the job's **result**, never from a PUT [proposed, D18]: a pack is
   promoted only when `GetResult` says state `SUCCEEDED`, reason `EXIT`, exit code 0 and no `input_failure`. Any other
   result leaves the keys to the janitor, however complete the pack looks. A tree read that failed while the task had
   already PUT its pack (the action swallowed an `EIO` and exited 0) is therefore never promoted: js TI 2.4 makes such
   a result `FAILED` with `INPUT_FAILED`, and kbf checks `input_failure` itself as well, so a result that carries one
   is refused whatever its state. Today's executor already ingests only after the job's end (`executor/runs.rs`,
   `Verdict::Ingest`) [code]; item 9 of section 11 makes the rule the four conditions and tests each. Then the
   frontend's ingest
   (`crates/kbf-front/src/proxy/packed/commit/untrusted/task.rs`, `ingest_set`) verifies the keys' MACs, the packs'
   shape and footers, the manifest (this lease, this action), each digest's single place and the output `Tree`s,
   streams large objects through SHA-256 into the CAS, hashes every record of every pack as it records it in the pack
   index (pack 0 last), and only then rebuilds the `ActionResult` from verified digests and writes the action cache
   (exit 0 and not `do_not_cache`). Nothing the task claims is trusted.
3. **Cleanup.** A key no ingest recorded (a failed job, a refused pack, an input failure, a lost frontend) is deleted
   by the `cas/b/` janitor after `--task-janitor-grace-secs` (at least 6 h). The janitor is off by default; it is a
   precondition of the flip (8.1).

What trees change for outputs: nothing on the wire. `kbf-task` still reads outputs from `/work` (the merged view: an
output the action wrote is in the upper directory; an unchanged input named as an output is read lazily through the
tree) and still packs and PUTs them. js TI 3.2 refuses a `run` with outputs on `crun` (`outputs-unsupported-here`), so
there is no agent-side output collector to use, and none is asked for: the task's pack is the one output path, with one
trust boundary, the ingest. B16's "read once, hash and store in one pass, no whole-file buffering" is the task's pack
writer's, as today.

## 6. Failures and retries

Every row ends in one of the farm's existing classes (`crates/kbf-server/src/executor/outcome.rs`, then
`crates/kbf-server/src/answer.rs`), so the client's answers do not change. "Rerun" is a new farm lease and charges
`FARM_RERUNS` (2); after it is spent the client gets `INTERNAL` with the farm-fault text (`fault.rs`).

| What happens | The job service says | The executor does (new rows marked) | The client gets |
|---|---|---|---|
| Inputs missing from the CAS at Execute | (no job) | (no job) | `FAILED_PRECONDITION` with one `MISSING` violation per blob, as today |
| Attach fails, not the tree's fault | the job runs with `JS_TREES_UNATTACHED=.` | nothing: the task fetches; counted `from="fetched"` | the action's result |
| The submit of a tree job is refused `no-capable-host` (no registered host reports `tierstore-trees`) or `invalid-tree-input` | `SubmitJob` refusal | **new**: submit the same lease again as attempt 1 without trees (`why=refused`), once; today `no-capable-host` is `Invalid`, which stays so for a job without trees | the action's result |
| The start of a tree job is refused (`AGENT_REFUSED`: `tree-invalid`, a bad attach path, or any other refusal, which the empty start detail cannot tell apart, kbf#2287 item 1) | `FAILED_TO_START` | **new**: the same resubmit, attempt 1 without trees, once, and an error logged naming the input root | the action's result |
| A fill fails while the job runs (`EIO`: a moved pack, the store down past tierstore's bound, tierstore aborted) | `FAILED`, `INPUT_FAILED`, `tree-read-failed`, whatever the exit code | **new**: infra, rerun; the root's memo entries dropped (4.3); the pack not ingested | the rerun's result, or `INTERNAL` |
| The job passed its wall limit and the detach reports `stall_ms` over a tenth of it | `FAILED`, `INPUT_FAILED`, `tree-stalled` | **new**: infra, rerun | as above |
| The detach at exit is not answered and the job did not succeed | `FAILED`, `INPUT_FAILED`, `tree-unavailable` | **new**: infra, rerun | as above |
| The submit is answered with a `Job.spec` that lacks the `trees` sent (a server older than the trees release, js TI 7) | `SubmitJob` answer | **new**: cancel the job, infra, rerun, an error logged naming the server | as above; the fix is upgrading the server |
| The action's own timeout (fired by `kbf-task` inside the job: exit 0, a timeout in the manifest) | `SUCCEEDED`, exit 0 | ingest, as today | `DEADLINE_EXCEEDED`, with the partial result |
| The job's wall limit (the task itself hung) | `FAILED`, `WALL_LIMIT` | timeout, as today | `DEADLINE_EXCEEDED` |
| The job's memory limit | `FAILED`, `MEMORY` | out-of-memory, the memory ladder, as today | the result after the ladder, or `FAILED_PRECONDITION` with `OUT_OF_MEMORY` |
| The agent or host is lost | `LOST` | lost, rerun, as today | the rerun's result, or `INTERNAL` |
| Cancelled by someone else | `CANCELLED` | infra, rerun, as today | as above |
| `kbf-task` exits 10 to 15 | `FAILED`, `EXIT` | infra, rerun, as today | as above |
| The pack is refused at ingest | (exit 0) | infra, rerun with new keys, as today | as above |
| The action writes more than the scratch volume holds | the action's own `ENOSPC` | the action's result | the action's nonzero exit, as with a laid-out tree |

Notes:

- **`INPUT_FAILED` is a rerun even after exit 0.** A recorded fill failure means some open failed with `EIO`; an
  action that tolerates a missing optional file can exit 0 with a wrong output. tierstore's rule (ts 7.4) makes it the
  host's failure; kbf does not ingest that pack, and its keys go to the janitor.
- **The resubmit without trees is within the lease** (attempt 1 of the same farm lease; the job id's attempt field
  exists for this) and charges no farm budget: it is today's job, whose own failures are classed as today. It is made
  at most once per lease, so it cannot loop.
- **A stall the action's own timeout catches is not seen.** js TI 5's `tree-stalled` needs the job to pass its **wall
  limit**; kbf's action timeout fires inside the task first, so a stall in `tierstore fs` reaches the client as
  `DEADLINE_EXCEEDED`. Section 13 asks for `stall_ms` in the job's result; with it the executor applies the same rule to
  the action's timeout (stall over a tenth of it: rerun, not a timeout). Until then it is a known gap, and section 10
  counts timeouts per arm.
- **Fill time is the action's time.** A file's bytes are fetched on its first open, inside the action's run: the
  120 s attach bound covers the manifests only, and the first open of a large toolchain file waits for the whole blob.
  On a cold node that time counts against the action's timeout. Section 13 asks jobscheduler and tierstore for a
  prefetch the agent waits for in the preparing stage, which the wall limit does not count; until then the cold
  first-byte latency of the largest toolchain files is measured before the trees switch (section 10, 8.3), and the
  switch waits for it.
- **A rerun may land on the same host.** The farm avoids a virtual worker, not a host (`farm.rs`, `avoided`), and the
  job spec has no way to say "not that host", so a node with a broken `tierstore fs` can take both reruns of an action
  and end it `INTERNAL`. Section 10 counts input failures by `Job.host_id`, section 13 asks jobscheduler for a host to
  avoid on a job, and until then the remedy is the operator's (4.6: restart or stop that node's agent). The same holds
  for `LOST` today.
- **Retries stay kbf's.** The job service retries nothing (`jobscheduler-design.md`: retry policy is the client's).

## 7. What is deleted after the flip, and what stays

In the order of section 11 (items 15 and 16), after the flip has held for one release with no rollback [proposed]:

1. **The nodes move first**: the six nodes run `kbf storage` instead of `kbf storage-and-daemon` (an operations step
   through the host updater; the storage half is the same process code, so the store keeps serving throughout). This
   is the point after which the executor rollback is no longer one line (8.4).
2. **Then the code**, once no unit selects `--driver container`:

| Deleted | Size (non-test lines of `src`, `wc -l`) | Why it can go |
|---|---|---|
| `crates/kbf-driver-container` | 11,704 | the Linux daemon's `crun` driver; no Linux daemon runs |
| the daemon's `container` driver wiring, its Linux input cache and its shared-tree code in `kbf-daemon` and `kbf-node` | part of 16,282 and 4,438 | Macs use the `native` driver and the clone cache, which stay |
| the placement filter of 8.2 | small | no Linux daemon connects |

What stays:

- **The farm** (`farm.rs`, `farm/`, `kbf-sched`) as the operation state machine: deduplication, the memory ladder,
  rerun budgets, fencing, and placement on Mac daemons and on virtual workers. That the farm uses jobscheduler's
  `js-sched` crate for this is not changed here; O1 is about the server libraries (`js-server-core`, `js-store`),
  removed in kbf#2279.
- **The worker listener and `kbf-daemon`** for the Macs (`kbf-driver-native`).
- **The executor** (`executor/`, 4,303 lines), **`kbf-job-client`** (2,539), **`kbf-task`** (8,173) with its fetch and
  locate code, **`kbf-task-pack`**, the job-facing read listener, `prepare_run` and the ingest. The fetch path is the
  fallback of 4.2 and 4.6, so it is permanent, not interim.
- `--executor kbf-daemon` stays while Macs run daemons, since the frontend serves them either way; after the
  deletions it means "no Linux executor", which the release notes say.

## 8. The flip on the six Linux nodes

Two switches, each set for the whole farm at once (O6), each with a rollback line. The first does not depend on
tierstore; the second does.

### 8.1 The first switch: the executor

Preconditions, checked once before `kbf-deploy job-service apply`:

1. **`js-lease-server`** at a release kbf works with: label values as kbf writes them (or the frontend id made lower
   case, at the config's source), one client token across frontends, a body limit kbf's submits fit (kbf#2287 items 2,
   4, 5). Servers are upgraded before clients (js TI 7).
2. **`js-agent`** on all six nodes as the `.deb` from the apt repository, installed and set by hostkeeper
   (komira-ai/jobscheduler#841), running `--runtime crun` with `uidmap` installed, its flags file holding the
   `--egress=allow-list` and `--egress-allow` lines `kbf-deploy job-service agent-egress` prints, and joined to the
   farm pool.
3. The pool books no disk (O4), and kbf's executor no longer asks for it (section 11, item 2).
4. The `cas/b/` janitor is on, on the leader (`--task-janitor-key-file`).
5. Secrets go as files (`executor-secret-delivery = "files"`).
6. The placement filter of 8.2 and the Command's sandbox of 4.4 are in the running release (section 11, items 3 and 4).
7. **The no-op benchmark** of section 10 has run on the six nodes and its result is reported to the owner: the latency
   of an action that does nothing as a job against the same action on today's daemon. The switch waits for it. It is
   a measurement read by people, not a test with a threshold (T10).

The switch is the existing procedure: `kbf-deploy job-service apply --apply` (staged `kbf.toml`, `kbf --check`, a drain
that waits by default (B15), restart, the canary job).

**Rollback line:** `kbf-deploy job-service back-out --apply`, which puts the kept pre-switch `kbf.toml` back byte for
byte and restarts the frontend. By hand, without that backup, it is `executor = "kbf-daemon"` with
`executor-secret-delivery = "files"` and `executor-locate` removed in the same edit: `kbf-daemon` refuses both at start
(`crates/kbf-server/src/frontend/executor_flags.rs`, `docs/deploy/job-service-cutover.md`). `back-out` restores the
whole file, so an edit made after the switch (the trees switch of 8.3 included) is undone with it.

### 8.2 The Linux daemons during the window

The daemons keep running beside `js-agent` until the nodes move (section 7), so that the rollback is one line and not
a redeploy. While `--executor js-agent` is set, the farm's placement skips every Linux daemon worker; Mac daemons are
placed as before [proposed, D8]. This is a filter computed at placement from the running executor setting, **not** a
cordon: a cordon is a stored row with an owner that only the operator lifts 
(`crates/kbf-server/src/farm/cordon_owner.rs`), which a rollback would leave behind. The filter writes nothing, leaves the operator's cordons and
drains as they are, and is gone with the restart of the rollback. The idle daemon costs its process and its
connection; the node's cores and memory are the agent's (`memory_schedulable_percent`).

### 8.3 The second switch: trees

`executor-input-trees = true` [proposed, D10]. Preconditions, checked once:

1. jobscheduler's tree items 1 to 4 (js TI 9) released; the lease server upgraded to that release; kbf's pin moved to it
   (section 11, item 11).
2. On each of the six nodes: tierstore's package with `tierstore fs` running (`tierstore-fs@a`, its marker made, `[fs]`
   with `fs.owner` equal to the agent's job uid, which the agent's start-time probe checks, js TI 3.1); the node's
   tierstore reading the bucket and prefix the frontend's pack store writes (the manifests' `source` keys are that
   store's keys); `--scratch-low-water-gib` at least the cache budget where the cache shares the scratch volume (js TI
   6); the agent configured with `--tierstore-fs` and `--tierstore-control` and **started after** `tierstore fs` (the
   capability is probed once, at the agent's start).
3. `ListHosts` shows `tierstore-trees=true` on all six.
4. The one-node run of js TI 9 item 6 passed, with a kbf task reading a tree.
5. The cold first-byte latency of the largest toolchain files through `tierstore fs` (section 10) is measured and
   reported to the owner.

The kernel: passthrough needs 6.9 or later; ts C8 chose the 7.0 kernel for the farm, which also has FUSE resend, so a
restart of `tierstore fs` is invisible to running jobs. On a 6.8 node there is no resend: a crash or restart of
`tierstore fs` aborts the reads in flight (ts 7.2), and those jobs end `tree-read-failed` and are rerun. The trees
switch does not wait for 7.0, since the 6.8 mode is correct; but until tierstore's update hook acts on
`tierstore-fs@` (`update-hook.md`), a node's `tierstore fs` is updated only after its agent is drained (B15).

**Rollback line:** `executor-input-trees = false` and a frontend restart: every job is today's job again, with nothing
changed on the nodes. If the capability is lost on every node (all agents started before `tierstore fs`), tree submits
are refused `no-capable-host` and resubmitted without trees (section 6), so builds keep working at the fetch path's
speed until the agents are restarted.

### 8.4 Fix forward, and when the rollback stops being one line

After a switch, a defect is fixed in the next release, not by switching back, unless builds fail at a rate the
rollback would end; the owner calls the rollback. The nodes move (section 7, step 1) after one release has shipped with
the executor switch on and no rollback. From that move on, the executor's rollback is the previous kbf release plus
the previous node units, not a line; the release notes say so. The trees switch stays one line for good.

## 9. Tests that prove it

Hermetic tests run in the pull request that builds the part (section 11); the real-service, farm and conformance runs
are beta or runbook, never in the pull-request set (B9). No test asserts a duration (T10): bounds use the paused clock
the executor tests already run on, and fakes answer on explicit events.

**Executor, against `FakeJobServer` (taught `trees`, `INPUT_FAILED`, `no-capable-host` for tree jobs and the spec
echo), in `crates/kbf-server/tests/executor/`:**

- `a_job_with_a_tree_names_it_and_runs_kbf_task_as_its_command`: the spec field by field, as in 3.1;
- `a_job_without_a_tree_is_todays_job_byte_for_byte`, once for each reason of 4.2 (the attempt in the job id is 1
  for `why=refused`, its only difference);
- `no_job_books_disk`, for a shared and a whole-host lease;
- `input_failed_is_a_rerun_whatever_the_exit_code`, for `tree-read-failed`, `tree-stalled` and `tree-unavailable`,
  each with exit 0 and nonzero, and the pack not ingested;
- `an_input_failure_drops_the_roots_manifests`: the rerun's tree digest follows a pack that moved;
- `a_refused_tree_submit_is_resubmitted_once_without_trees`, for `no-capable-host` and `invalid-tree-input`, and
  `a_refused_tree_start_is_resubmitted_once_without_trees`; the second refusal of the resubmit is classed as today;
- `no_capable_host_is_still_invalid_for_a_job_without_trees`;
- `a_spec_answered_without_its_trees_is_cancelled_and_rerun` (the fake drops `trees`);
- `a_resubmit_after_a_lost_answer_sends_the_same_trees`, with the memo changed between the two sends;
- the outcome table's test (which holds `outcome.rs` to its table) with the new rows;
- `a_pack_is_promoted_only_from_a_clean_result`: with a complete, valid pack already PUT, nothing is promoted for a
  result that is `FAILED`, that has reason `INPUT_FAILED` or `WALL_LIMIT`, that has a nonzero exit, or that is
  `SUCCEEDED` with exit 0 but carries an `input_failure`; the one clean result promotes it.

**Manifest writer, in `crates/kbf-front`:**

- golden manifests of a root with a file, an executable, an empty file, a large object, a symlink and a nested
  directory, byte for byte;
- each manifest parsed and its tree flattened by tierstore's own `tree_manifest::parse_manifest` and `flatten` (a test
  dependency at kbf's tierstore pin), so kbf's writer and tierstore's reader cannot drift;
- children sorted bytewise across REAPI's three node lists; a shared subtree built and PUT once (a memo hit does not
  PUT);
- the memo, on an injected clock the test advances: one entry past its bound drops the oldest; an expired entry is
  rebuilt; a parent expires no later than its earliest child;
- each tierstore limit at its boundary and one past it, and a `.wh.` name: no tree, `why=unrepresentable`;
- a closure of 65,536 and of 65,537 packed blobs and of 256 and 257 large blobs gets a tree (the writer's chunks are
  not the listener's caps); an index that does not answer by the deadline: no tree, `why=store`;
- a non-empty blob with no location: no tree, `why=unlocated`; a create-only PUT that finds the key present is
  success; one that fails is `why=store`;
- `collect_closure` with trees on keeps the `Directory` messages and with trees off keeps none;
- a `Directory` with `node_properties` on itself, on a file node and on a directory node: each no tree,
  `why=unrepresentable`.

**`kbf-task`, in `crates/kbf-task` (the library is given the `JS_TREES_UNATTACHED` value as a parameter, so no test
sets the process environment):**

- `--inputs tree` with the tree present: nothing is fetched but the Action, the Command and the root `Directory`;
- with `.` unattached: the inputs are fetched into the exec root (a test directory standing in for `/work`);
- with neither: exit 10 and the exact line of 4.4;
- the layout table of 4.4 in both modes; `private/` never inside the input root;
- an executable input is 0755 and another 0644, attached and fetched;
- outputs that are unchanged inputs, changed inputs, new files and directories, collected from the exec root;
- `InputStats.from` written as `tree` and as `fetched`;
- one test of the binary run as a child process with the variable set on the child only, proving `main` passes it on;
- the output paths' parents and the working directory exist before the Command starts, in both modes;
- the sandbox (on Linux, in the CI job that already runs `kbf-task`'s namespace tests): the Command writes and runs a
  program from `/tmp`, `/var/tmp` and `/run`, and the bytes are under `private/`; it cannot see `/run/js-files` or any
  file under `/scratch`; a background process it leaves is gone before the outputs are read; an output that is a
  symlink out of `/work`, a directory renamed to one, and a `..` in a path are each refused by the walk.

**Frontend farm:** `linux_daemons_get_no_lease_while_the_executor_runs`,
`a_mac_daemon_is_placed_beside_the_virtual_workers` (a macOS action on a Mac daemon, a Linux action on a virtual
worker), and `a_restart_under_kbf_daemon_places_the_linux_daemons_and_leaves_no_cordon_row`.

**Compatibility (B8):** the one format touched is the task manifest's `InputStats`. The reader (item 6) accepts a
version 2 manifest whose `inputs` has `from` and one without it; the writer (item 7) ships a release later, and its
test reads the written manifest with the reader of item 6. That a previous reader refuses `from` is pinned by a test,
since it is why the order is what it is. The field-set comparison of fake and real job service
(`the_fake_and_the_real_server_answer_with_the_same_fields`) gains `trees` and `input_failure` when the pin has them.

**Real service (beta, root, `crates/kbf-integration-test`, against the pinned `js-lease-server` and `js-agent`, with a
fake `tierstore fs` control socket in the test's scratch directory):** the existing `executor_real_service_integration`
cases, plus `a_tree_job_reaches_a_host_that_reports_trees_only`, 
`a_tree_job_on_an_agent_whose_tierstore_refuses_fetches_its_inputs`, `a_tree_job_on_an_agent_whose_tierstore_hangs_fetches_its_inputs` (the fake holds the attach until
the agent's injected bound passes) and `a_tree_job_with_no_capable_host_is_resubmitted_without_trees`.

**Farm integration (a runbook on the six nodes, not a pull-request test):** with real `crun` and `tierstore fs`, an
action that reads a file of its tree, writes a new output, changes an input and deletes one: the output is in the CAS,
the action-cache entry is written, the run directory is gone, the agent's integrity check passes. Then the same with
`tierstore fs` stopped (fetched), with it hung (fetched after the bound), and with a pack of the tree deleted between
attach and open (`INPUT_FAILED`, rerun with a new tree, success).

**Conformance:** a pinned Buck2 build of the farm's usual targets, run by `kbf-harness`'s B0 scenario against a
frontend in each arm of section 10, gated on B0's gates (zero failed builds, zero failed actions by class,
`kbf_cache_lost_blobs == 0`); each action's outputs are compared with the daemon arm's action-cache entry for the same
action digest, and actions that differ only by an embedded exec-root path (4.4) are listed by name, not failed.

**Why this list is complete for the failure and boundary cases.** It takes each new row of section 6 (each refusal at
submit and at start, each of the three `input_failure` codes with each exit, the dropped `trees`), each reason of 4.2,
each tierstore limit and each lookup bound on both sides of its boundary, each row of 4.6 (attached, refused, hung,
not capable anywhere), each branch of the task's steps 1 and 2 of 4.4, the memo's bound, expiry and parent rule, the
lost-answer resubmit, both directions and the release order of the one format change, the placement filter on, off
and after a rollback, each result that must not promote a pack and the one that must, each place a `node_properties`
can be set, and each way out of the Command's sandbox (a hidden path, a leftover process, a symlink, a rename, a `..`).
Rows of section 6 that are unchanged keep the tests they have.

## 10. Numbers to measure

Measured, not asserted (T10), on the six nodes, and compared by `js-analysis`'s rule (the 95% Welch interval of the
difference excludes zero; runs interleaved by arm; warm-ups labelled and excluded) [proposed]:

| Arm | What |
|---|---|
| A | today: `kbf-daemon`, the container driver, the node input cache |
| B | the executor, inputs fetched (`executor-input-trees = false`): the interim |
| C | the executor, trees, `tierstore fs` in the 6.8 mode |
| D | the executor, trees, passthrough (a 7.0 node) |

Each as p50, p90 and p99 with its interval, and each marked measured or inferred:

1. **Per-action overhead** = lease to result minus the Command's own run time. Its parts and where each comes from:
   prepare (closure, keys, grant and, in C and D, the manifests), start, queue (which holds the agent's preparing
   stage, the attach included, and the heartbeat waits; the pool's `heartbeat_interval` is recorded with the arm), task
   and ingest from `kbf_executor_phase_seconds{phase}`; fetch, run and pack from the manifest's `timing` (the task's
   own claims, labelled so); the rest of the task span after pack is the PUT. Arm A from the daemon's `lease phases:`
   line. Client side, from Buck2's event logs, by `kbf-baseline`.
2. **The no-op benchmark, the gate of the executor switch (8.1).** An action that runs `true` over an empty input root,
   with `skip_cache_lookup`, submitted serially and then 64 at a time, through arm A (today's daemon) and arm B (a job),
   interleaved, at least 30 runs per arm after labelled warm-ups. Reported: Execute to answer at the client, and its
   parts by the phases above, p50 and p90 with their intervals. It isolates what a job adds per action: the submit,
   placement, the start, the task's start and namespaces, the result's round trip and the ingest.
3. **Cold first-byte latency, the gate of the trees switch (8.3).** The time to the first byte of `read` on the largest
   files of the farm's toolchains (the compilers and linkers, each 100 MB or more) through `tierstore fs` with its cache
   empty, on a 6.8 node and on a 7.0 node, against the same file from a laid-out tree.
4. **Cold, warm and partial.** Cold: tierstore's cache and the node input cache emptied, and `skip_cache_lookup`.
   Warm: the same build again with `skip_cache_lookup`. Partial: one input changed. Plus, for C and D, the prepare
   phase of a large toolchain root with the memo cold and warm.
5. **Build wall time** of the conformance builds.
6. **The share of actions served by trees** and each `why` of 4.2 (`kbf_executor_inputs_total`), and `from` of the
   manifests: an arm with trees on and most actions `fetched` is a store or node mismatch to fix at its source.
7. **Bytes**: inputs read per action against the input root's size (tierstore's fill counters), scratch bytes written.
8. **Failures**: `INPUT_FAILED` per thousand actions by code and by host, `EXDEV` failures, timeouts per arm.

Apart from the two gates, these numbers do not decide whether to flip (O6 decided that); they decide what to fix first: if B's overhead over A is
mostly in queue, the pool's heartbeat; if C's is in reads, passthrough and prefetch (ts 12, phase 3).

## 11. Build items, in order

Code items are one pull request each; items 5, 14 and 15 are operations steps, not pull requests. Items 1 to 9 need
nothing new from jobscheduler. Each pull request updates the documents named with it.

1. **This design.**
2. **Executor: no disk booking.** `Needs.disk_bytes` unset; `--executor-output-disk-bytes` removed, refused by name.
   Docs: `docs/deploy/kbf-modes.md`.
3. **Farm: Linux daemons get no lease under `--executor js-agent`** (8.2) and its three tests. Docs: `kbf-modes.md`,
   `job-service-cutover.md`.
4. **kbf-task: the Command's own mount and PID namespaces** (4.4): disk-backed `/tmp`, `/var/tmp` and `/run` bound from
   `private/`, `/scratch` and `/run/js-files` out of its sight, the new `HOME` and cache paths, and the output walk's
   `openat2` resolution; its tests. Docs: `kbf-modes.md` where it describes the task's environment.
5. **The executor switch** (8.1), by `kbf-deploy`, after the no-op benchmark of section 10 (its gate). Arms A and B of
   section 10 are measured after it.
6. **kbf-task-pack: `InputStats.from` read** (optional, `tree` or `fetched`) and counted by the ingest; the reader's
   tests.
7. **kbf-task: the input root at `/work`, `--inputs tree`, `JS_TREES_UNATTACHED`, the root check and writing
   `InputStats.from`**, one release after item 6; the `kbf-task` tests. No job sends `--inputs tree` yet; the layout
   change ships alone and runs on today's path. Docs: `docs/artifacts.md` and `kbf-modes.md` where they describe the
   task's layout.
8. **tierstore pin** moved to a release with `tree_manifest` (a test dependency of item 10).
9. **kbf-server executor: the ingest gate** (section 5): a pack is promoted only after a result with state `SUCCEEDED`,
   reason `EXIT`, exit 0 and no `input_failure`; its tests. It needs no new field to land: today's results already
   carry the first three, and an absent `input_failure` reads as empty.
10. **kbf-front: the manifest writer**: `collect_closure` keeping the `Directory` messages, the index and presence
    lookups in chunks, the manifests, the memo, the `NodeProperties` check, the PUTs; no caller yet.
11. **jobscheduler pin** moved to the release with js TI items 1 to 4 (`kbf-js-version.toml`, the workspace pins, the
    fake taught `trees`, `INPUT_FAILED` and the spec echo, the field-set test).
12. **kbf-server executor: trees in the job**: 3.1, `executor-input-trees`, the counter, the rows of section 6, the
    resubmit without trees; the executor tests. Docs: `kbf-modes.md`, `job-service-cutover.md`, `docs/api.md` (its
    `kbf_executor_*` metrics).
13. **kbf-integration-test: the real-service tree cases** (beta).
14. **The trees switch** (8.3), after the one-node run, `tierstore fs` on all six and the cold first-byte measurement
    of section 10. Arms C and D after it.
15. **The nodes move to `kbf storage`** (section 7, step 1), after one release with the executor switch on and no
    rollback.
16. **Delete the Linux container driver**, the daemon's Linux-only wiring and the placement filter of item 3, once no
    unit selects `--driver container`. Docs: `kbf-modes.md`, `job-service-cutover.md`, `docs/artifacts.md`,
    `docs/design/daemon.md` where they describe the container driver.

The measurements' results are written into this document when they exist. Two of them are gates: the no-op benchmark
before the executor switch, and the cold first-byte measurement before the trees switch (8.1, 8.3).

## 12. Decisions

Each was not decided by the rulings or the two designs; the owner may overturn any.

- **D1. One action per job.** Batching would mix failures and outputs behind one exit code and need a scheduler inside
  the job; its gain is measured first (section 10, the no-op action).
- **D2. The second executor becomes the only Linux path**, rather than a new one: it is built, tested against the real
  service, and already carries O2 and O3.
- **D3. kbf stops booking disk.** O4; kbf#2287 item 3 shows a booked disk keeps kbf's jobs off a pool that books none.
- **D4. The frontend writes the manifests**, from the `Directory` messages its pre-submit closure already reads and
  from its own pack index; not the task (it would need a store write right, against O2) and not the agent
  (jobscheduler stays generic). It does not go through the read listener's `/locate`, whose per-request bounds are
  for untrusted callers.
- **D5. A tree is built only when every non-empty blob has a location**; otherwise the job is today's. A tree with a
  file tierstore cannot fetch fails at open, which is a rerun, not a slower run.
- **D6. The input root is `/work` in both of the task's modes**, so an action sees the same paths whether its inputs
  were attached or fetched; `private/` stays under `/scratch`. The daemon's `/kbf/root` is not changed (it is deleted
  later); actions that embed the exec root's path differ by executor, as they do today.
- **D7. `job_stages_if_unavailable` is always set.** A node's `tierstore fs` outage then costs speed, not builds.
- **D8. Linux daemons are skipped at placement, not stopped or cordoned, while the executor runs**, so the rollback is
  one line and leaves no state behind.
- **D9. The job's network level does not depend on whether the tree attached**; only the job knows that.
- **D10. Trees have their own switch**, off by default: a job with trees is placed only on nodes reporting the
  capability (js TI 2.3), so sending trees before all six report it would put the work on the nodes that do, or have
  it refused if none do.
- **D11. One writable tree at the exec root, no read-only sub-trees** (the daemon's shared toolchain trees have no
  counterpart): js TI 3.4 and its decision TI4 allow a writable tree only at the exec root, a read-only bind would
  change what an action may write, and tierstore's cache already shares a toolchain's bytes across trees by blob
  digest.
- **D12. The fallback is counted from the task's manifest** (`InputStats.from`), reader first: the job's environment
  is not visible to the frontend otherwise.
- **D13. An input failure charges `FARM_RERUNS`** like any farm fault; it gets no budget of its own.
- **D14. A refused tree job is resubmitted once, within its lease, without trees**, rather than marking its input root
  fetch-only for everyone: the start detail cannot say whether the tree or the host was refused (kbf#2287 item 1), and
  a farm-wide mark on a shared toolchain root would take trees from every action that uses it.
- **D15. `whole_machine` leases get no tree for now**: the order of the attach and the whole-host start is not
  designed by js TI (section 13).
- **D16. The job's image is the kbf image for the first flip** (option a); each action in its own image with
  `kbf-task` given as a read-only tree (option c) is a later design. Option b, a root file system nested inside the
  job's, is not taken. Option c, and `JobSpec.trees` itself, change jobscheduler's public interface and go to the
  owner's review of public interfaces (3.2).
- **D17. The Command runs in its own mount and PID namespaces inside the job** (4.4): disk-backed `/tmp`, `/var/tmp`
  and `/run` as under the daemon, the job's grants and `/scratch` out of its sight, every process of it ended before
  its outputs are read, and the output walk resolved beneath `/work` by the kernel. It ships before the executor switch,
  since the job's own `/tmp` is a small `noexec` tmpfs for both modes.
- **D18. Outputs are promoted only from the job's result** (state `SUCCEEDED`, reason `EXIT`, exit 0, no
  `input_failure`), never from a PUT: the task PUTs before the agent's detach can report a failed read.
- **D19. Two measurements gate the two switches** (8.1, 8.3): the no-op benchmark and the cold first-byte latency, each
  reported to the owner before the switch. Every other number of section 10 decides what to fix first.

## 13. What this asks of jobscheduler and tierstore

None of these is kbf code; each is a change for its own repository and design. jobscheduler, all additive, for its
tree build items or after:

- `stall_ms` in the public `JobResult` (the detach's answer), so kbf can tell a stall from the action's own timeout
  (section 6);
- `Job.start_failure_detail` filled from the agent's refusal code (kbf#2287 item 1), so a refused tree can be told from
  a refused host;
- a host to avoid on a job (a rerun's previous host), so a rerun of an input failure goes elsewhere (section 6);
- how the attach orders with a whole-host start, so `whole_machine` leases can have trees (D15);
- **a prefetch inside the preparing stage**: the agent attaches with `prefetch all` (ts 6.5) and waits for it, under
  the preparing bound the wall limit does not count, so fill time stops being charged to the action (section 6);
- **digest authorization**: a tree digest is today the only credential for reading what it names, and a manifest's
  `source` may name any key of the store. The server should accept a tree from a client only within what that client
  may read (and tierstore's fill socket should take sources only from a prefix tied to the writer), so that one
  client of the job service cannot read another's blobs by naming a digest or writing a manifest;
- **a low-water stop at run time**: the scratch volume is shared with tierstore's cache and nothing bounds a job's
  writes (no quota, O4); `--scratch-low-water-gib` is checked at admission only. When free space falls below it while
  jobs run, the agent should stop the job that is growing and report a reason of its own, so one runaway action does
  not turn into `EIO` in every other job on the node. This is not a quota.

tierstore:

- **refuse at attach, not during the job**, when a tree's flattened manifests or its pins would pass the node's memory
  or `pin_limit`: many large distinct trees at once must give `tree-unavailable` (the fetch fallback), never an `EIO`
  in a running job;
- the fill-source restriction above, on its side.

kbf relies on the manifest format of ts 6.1 as merged, `cas/l/<sha256>` for manifests and large objects, and ranged
`source`s into kbf's packs.

## 14. macOS later, and other open points

**macOS (later; the Macs are in device-management enrollment):**

- whether Mac actions become jobs at all, and through which runtime (the agent's `sandbox-exec` per-run user, or a VM);
- the writable tree on macOS: tierstore's NFSv3 frontend serves read-only trees and the client builds its own writable
  directory (ts 8, option a); whether `kbf-task` or the agent builds it (js TI Q1);
- Seatbelt grants for the tree (js TI 4.2) beside `kbf-task`'s own macOS confine;
- until then the Macs keep `kbf-daemon` with the native driver, and the frontend keeps the worker listener.

**Other open points:**

- **Collecting `cas/l/`.** Nothing deletes there today, manifests included. A collector must not delete a manifest or a
  blob an attached tree uses (ts 6.1); its design belongs with storage retention.
- **`kbf-task-pack` refuses unknown fields** in a manifest, which is why item 6 must ship before item 7. Making its
  readers skip unknown members, as B8 asks of readers, is a change of its own.
- **The farm's virtual workers** bound kbf's in-flight work by configured shapes, not by the pool's real capacity;
  unchanged here.
- **The submit-to-start floor** is bounded by the agent's heartbeat. If section 10 shows it dominant, a shorter pool
  heartbeat is a jobscheduler configuration change, measured before it is made.
