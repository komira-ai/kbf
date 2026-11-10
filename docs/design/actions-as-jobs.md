# Every build action is a jobscheduler job on Linux, its root file system made of tierstore trees

Status: **DESIGN, for the owner's review; nothing here is built and no code changes with this document**, so no
existing document is stale yet; section 11 names the documents each build item updates. Linux only; macOS is section
14, a list of open points and no more. This revision follows the owner's decisions of 2026-11-10 (section 1, O7 to
O12), which replace the first revision's job shape: the job's whole root file system, the base image included, is now
made of tierstore trees, and the download of inputs is temporary.

Written against kbf `main` at `6ad2b5931`, jobscheduler `main` at `f4924e67` (kbf's pin, release 0.1.218, is
`0c104a1d`, with the same public job API), the open pull request komira-ai/jobscheduler#842, jobscheduler's
`docs/design/tree-inputs.md` (cited "js TI *n*") and tierstore's `docs/design/virtual-build-fs.md` (cited "ts *n*").
Claims are tagged **[code]** (read in the named file of this repository, or of jobscheduler or tierstore where named),
**[js design]** / **[ts design]** (stated by those designs and not built) or **[proposed]** (this document).

## 0. What changes, and what does not

**Before.** Each Linux node runs `kbf storage-and-daemon`; the daemon copies or hard-links each action's input root
into a scratch directory, runs the action in a rootless `crun` container over an OCI image unpacked on the node, and
uploads the outputs over the worker listener (`crates/kbf-daemon`, `crates/kbf-driver-container`) [code]. A second
executor is built and opt-in: `--executor js-agent` makes every placed action a job of jobscheduler's job service; the
job is the kbf OCI image, whose `kbf-task` downloads the inputs over HTTPS into scratch, runs the action, and PUTs the
outputs to the lease's own presigned keys, which the frontend verifies and turns into CAS and action-cache entries
(`crates/kbf-server/src/executor/`, `crates/kbf-task`) [code].

**After.** On Linux every action is one job, and the job has **no OCI image**. Its root file system is an overlay: the
run's scratch directory as the writable upper layer, and below it three read-only tierstore trees, read lazily through
`tierstore fs`: the **action tree** (the action's input root at `/work`, and its Action and Command), the **tools tree**
(`kbf-task`), and the farm's one bare-bones **base tree** (the base image, held in tierstore). Nothing is pulled,
unpacked or copied before the action starts; only the files the action opens are fetched, and kbf asks tierstore to
warm them when it submits the job. Outputs go where they go today: create-only PUTs to the job's own keys, then the
frontend's ingest. Until the tree path works on all nodes, today's job (the OCI image and the download) is the
temporary fallback; section 7.2 designs its removal.

**What does not change.** The REAPI surface (Execute, WaitExecution, the action cache, the CAS), the answers clients
get (section 6), the action-cache rule (exit 0 and not `do_not_cache`), the frontend's in-flight deduplication, the
memory ladder and the farm's rerun budgets, the Macs (still `kbf-daemon`), and the storage half of the Linux nodes.

Contents: 1 rulings; 2 where things stand; 3 the job; 4 inputs; 5 outputs; 6 failures and retries; 7 what is deleted;
8 the flip; 9 tests; 10 numbers and the gate; 11 build items; 12 decisions; 13 what this asks of jobscheduler and
tierstore; 14 macOS later and other open points.

## 1. The rulings this starts from

Settled by the owner and not reopened here:

| Id | Ruling | Where it lands |
|---|---|---|
| O1 | kbf is a jobscheduler **client** only (2026-11-03): no js server libraries inside kbf; kbf submits jobs through the client generated from jobscheduler's service definition. `js-agent` is standalone; `crun` is the Linux runtime. | 3, 7 |
| O2 | A kbf job writes only to its own fenced object-store keys; turning them into CAS and action-cache entries is a trusted server-side step in kbf (2026-10-29). | 5 |
| O3 | Inside a job only `kbf-task`'s fetch and upload phases use the network; the action command runs network-less (2026-10-30). | 3.6 |
| O4 | No per-job disk quota; the farm pool books no disk (2026-11-09). | 3.5 |
| O5 | tierstore's filesystem is FUSE on Linux, everything under the hood in tierstore (2026-11-09). | 3, 4 |
| O6 | Flip fast, fix forward, one-line rollback, no shadow or gradual rollout for the flip itself (2026-10-28). | 8 |
| O7 | **One base image for the whole farm**, as bare-bones as possible, for hermetic builds; no per-action images (2026-11-10). | 3.3 |
| O8 | **The base image lives in tierstore as a tree.** The job's root file system is an overlay whose lower layers are the base-image tree and the action's tree, read lazily through `tierstore fs`, with the scratch directory as the upper; no OCI pull or unpack per job, no copy of inputs (2026-11-10). | 3.1 to 3.4 |
| O9 | **One build action per job** (2026-11-10). | 3.4 |
| O10 | **The cost per action is measured in milliseconds**; a no-op-action benchmark (job against today's daemon) is a flip gate, with a stated target (2026-11-10). | 10.1, 8 |
| O11 | **Copy-to-scratch is temporary**, only until lazy fetch and the overlay work; its removal is designed (2026-11-10). | 4.6, 7.2 |
| O12 | **Prefetch**: kbf tells tierstore which objects to prefetch at submit time, so caches warm while the container is made; not a jobscheduler preparing-stage fetch (2026-11-10). | 4.5, 13 |

## 2. Where things stand

**kbf** [code]:

- `--executor` (`crates/kbf-server/src/executor/mod.rs`) picks `kbf-daemon` (the default) or `js-agent`;
  `kbf-deploy job-service apply|back-out` is the held procedure around the switch, with a canary job
  (`docs/deploy/job-service-cutover.md`).
- In `js-agent` mode the frontend's farm (`farm.rs` over `kbf-sched`) still decides which operation runs and how often
  (deduplication, the memory ladder, `FARM_RERUNS` 2, `NOT_STARTED_RERUNS` 3). It places each lease on one of
  `--executor-hosts` virtual workers, which hold no host; the job service decides the host (`executor/host.rs`).
- The job today is the kbf OCI image: the farm's base image (`packaging/container/kbf-task/base-<platform>.manifest.json`)
  plus one layer holding `usr/bin/kbf-task` (`crates/xtask/src/pkg_image_asset.rs`). Its id is
  `k<8 hex>-<term>-<seq>-<attempt>` (`executor/submit.rs`; attempt 0 today), which makes the submit idempotent.
- Before it submits, the frontend reads the Action, the input root and every `Directory` under it and checks every
  file is held (`crates/kbf-front/src/prepare_run/closure.rs`, `collect_closure`); it keeps only the digests.
- `kbf-task` reads the Action, the Command and the inputs through a read grant (or by pack span with `--locate`),
  lays the inputs out under `/scratch/work/in`, runs the Command in its own user and network namespace (loopback only),
  collects outputs (`kbf-outputs`, `O_NOFOLLOW` at every component), packs and PUTs them. Exit 0 means the action ran
  (its own exit code and a timeout are in the manifest); 10 to 15 are the farm's failures.
- kbf#2287 lists what the real job service at the pin lacks: `Job.start_failure_detail` is always empty; label values
  must be lower case; a job that books disk is placed only on hosts reporting `disk-gib`; a client token sees only its
  own jobs; the request body limit is 64 KiB; no live logs without a stream.

**jobscheduler**: the public `JobSpec` has an `image` and no input field [code]. js TI designs tree inputs below the
exec root `/work` (`JobSpec.trees`, `TreeInput`, `JobResult.input_failure`, `RESULT_REASON_INPUT_FAILED`, the
capability `tierstore-trees`, attach in the preparing stage, detach at every end) [js design];
komira-ai/jobscheduler#842 (open) is its first item, the agent wire and the capability key. A job whose **root file
system** is made of trees is not designed there: section 13 asks for it.

**tierstore**: the manifest reader and flattener, the fill socket, pins, `tierstore fs` on Linux (the 6.8 mode and
passthrough), crash recovery and the live hand-off are merged [code: `crates/tierstore/src/tree_manifest.rs`,
`src/server/treefs/`, `src/server/fill_socket/`]. A tree is named by the SHA-256 of its root **tree manifest**, a JSON
listing per directory whose files name their blob and, optionally, where its bytes are (`source: {key, offset}`);
writing manifests is the client's job (ts 6.1, 6.7). `attach ... prefetch all` warms a tree on the node once a run
attaches it (`crates/tierstore/docs/operations.md`); nothing warms a tree before that.

## 3. The job

### 3.1 The root file system, layer by layer

[proposed, D2] The job's root file system is one overlay, made by the agent in the container's mount namespace:

| Layer | What it is | Paths it supplies |
|---|---|---|
| upper (writable) | the run's scratch directory on the node's scratch volume, as the `/work` upper is today | every write of the run: outputs, copy-ups, whiteouts, the Command's temporary files |
| lower 1 (top) | the **action tree**, one per action (4.1) | `/work` (the input root), `/.kbf/action` and `/.kbf/command` (the Action and Command blobs) |
| lower 2 | the **tools tree**, one per kbf release (3.3) | `/.kbf-tools/kbf-task` |
| lower 3 (bottom) | the **base tree**, one for the farm (3.3) | everything else: `/etc`, `/usr`, the mount points |

Why this order: a name is resolved from the top, so a layer may only add paths below it, never shadow one. The three
trees are built so that their paths do not meet (the base tree may hold no `/work`, `/.kbf` or `/.kbf-tools`; the
tools tree holds only `/.kbf-tools`; the action tree only `/work` and `/.kbf`), checked when each is built, so the order
changes nothing an action sees; it puts the tree that changes per action on top, where lookups of `/work` stop first.

Why three trees, not one tree per action merging all three: the base and tools trees are the same digest for every job,
so each node attaches them once per job but reads, caches and (with passthrough) shares their pages once for all jobs;
the per-action tree is small (its manifests name `/work`'s subtrees, which are shared by digest anyway). Why the tools
tree is not part of the base: the base then changes only when the farm's base image does, not with every kbf release,
and stays warm in every cache across kbf releases.

What the overlay stacks on: all three lowers are directories of the one `tierstore fs` mount
(`/run/tierstore/fs/trees/<digest>`), so the stack is the overlay over FUSE that tierstore measured for one lower (ts
6.6, Measured: linux E4), with three lowers instead of one; section 13 asks for that measurement with three.

Mount points the runtime binds over (`/proc`, `/dev`, `/sys`, `/run/js-files`) must exist as directories: the base
tree holds them (3.3). A tree job has no `/scratch` bind: its whole root is writable through the upper. No tmpfs is mounted on `/tmp`, `/var/tmp` or `/run`: they are directories of the base
tree, written in the upper on the scratch volume, so they are disk-backed and executable, as under today's daemon.
Every entry of a tree is owned by `fs.owner`, the host uid the action user maps to (js TI 3.1 checks it), so the action
user owns the base tree's `/tmp`, `/var/tmp` and `/run` and its writes there copy up into the upper; a tree's modes are
0644 and 0755 only (ts D10), so those directories are 0755, not 1777, which no single-user job needs.

### 3.2 The action's view

`/work` is the exec root and the Command's working directory is below it, as REAPI says. The Command runs in a
sandbox `kbf-task` makes (4.4): its own user, network, mount and PID namespaces. The job's grants (`/run/js-files`) and
`kbf-task`'s run directory (`/.kbf-run`, its stdout and stderr) are out of its sight. It can read, but not change in any
tree, `/.kbf-tools/kbf-task` and its own Action and Command under `/.kbf`; none of them is a secret. It sees the base
tree's files, its inputs, and nothing of the node: the user namespace maps it to the action user and nothing else.

### 3.3 The base tree and the tools tree

**The base tree** [proposed, D3] is the farm's one base image (O7), converted once into a tierstore tree and stored with
create-only PUTs. Its contents are chosen in its own build, as bare as hermetic builds allow (hermetic actions bring
their toolchains as inputs); this design requires only that it holds the mount-point directories above, `/tmp`,
`/var/tmp` and `/run` as directories, the action user in `/etc/passwd` and `/etc/group`, and none of `/work`, `/.kbf`,
`/.kbf-tools`. It starts as today's pinned base (`base-<platform>.manifest.json`), flattened with modes normalised
(below); making it barer is that image's own change.

**The tools tree** holds one directory, `/.kbf-tools`, with `kbf-task` (a static binary, as it is today) and nothing
else. It is how `kbf-task` gets into the bare root file system: neither in the base nor mounted from the node.

**How they are built.** An operator verb, `kbf-deploy job-trees`, reads the pinned base image (each layer checked against
its digest, whiteouts applied, hard links made separate entries) and the `kbf-task` binary of the release, writes each
file as a whole object `cas/l/<sha256>` and each directory's manifest the same way, and prints the two root digests
and the two `kbf.toml` lines (`executor-base-tree`, `executor-tools-tree`). A tree has one owner and modes 0644 and
0755 only (ts 6.1, D10), so the verb normalises what only narrows access (a file becomes 0755 if any execute bit is set,
else 0644; a directory 0755; the owner is dropped, so `/etc/shadow` 0640 becomes 0644, which is why the base holds no
secret) and refuses, by name, what changes behaviour: a set-uid or set-gid file, a device node, a FIFO or socket, an
extended attribute, and a path of the three reserved ones. It lists every normalised path in its dry run. It is a held step: a
dry run by default, `--apply` to write, never forcing (S3). The frontend reads both digests from `kbf.toml` and checks
at start that both root manifests are readable in the store; a missing one fails the start by name (configuration
checked once, B20).

### 3.4 The mapping: one action, one job

**One action per job** (O9). A farm lease is one job, as today. The job of an action whose tree was built:

| `JobSpec` field | Value | Today's job (the temporary fallback, 4.6) |
|---|---|---|
| `job_id` | `k<8 hex>-<term>-<seq>-<attempt>`, attempt 0 (1 for the resubmit of section 6) | the same |
| root file system | the three trees of 3.1, top first (a new field, section 13) | `image`: the kbf OCI image |
| `command` | `/.kbf-tools/kbf-task`, the whole program; there is no image entrypoint | empty: the image's entrypoint |
| `args` | the task's flags, with `--tree` | the task's flags |
| `env` | empty | the same |
| `needs` | `cpu_millis`, `memory_bytes` (the hard cap a daemon would set); no `disk_bytes` (D4) | the same |
| `constraints` | `os=linux`, the configured `arch`, contained | the same |
| `wall_limit_seconds` | the action's timeout plus the run margin | the same |
| `network` | the operator's level, default allow-list (3.6) | the same |
| `files` | the PUT URLs (no read grant: nothing is read over the network) | the read grant and the PUT URLs |
| `labels` | `kbf-frontend`, `kbf-instance` | the same |

**The trees are fixed in the prepared spec.** They are chosen once, in the executor's `prepare`, and kept in the
lease's `Prepared` spec. A submit sent again after a lost answer sends that spec unchanged and never consults the memo
of 4.3: the trees are part of "the same job" (as `trees` are, js TI 2.4), so a rebuilt manifest under the same id would
be `job-id-conflict`. Every rerun of section 6 is a new farm lease, so it has a new job id and fresh manifests.

A `whole_machine` lease gets today's job while it exists (D12): how a root made of trees orders with a whole-host
start is a question for jobscheduler (section 13).

### 3.5 Resources

`cpu_millis` and `memory_bytes` are the farm's booking, as today. `disk_bytes` is dropped (D4): O4, and while kbf books
it js-sched places a kbf job only on a host reporting `disk-gib` (kbf#2287 item 3). A tree's bytes are in tierstore's
cache under its own budget; only the run's writes land on the scratch volume.

### 3.6 Network

O3 holds: the job's network level is the operator's allow-list, and the Command runs in a namespace with only `lo`
(`crates/kbf-task/src/network.rs`) [code]. In a tree job the task's only network use is the PUTs: the Action, the
Command and the inputs are files of the root file system, so there is no fetch phase and no read grant. `tierstore fs`
fetches blobs over the node's fill socket, outside the job.

## 4. Inputs

### 4.1 From an input root to an action tree

The frontend writes the manifests, in the executor's `prepare`, before it submits [proposed, D5].

**The `Directory` messages.** A manifest per `Directory` needs each one's names, kinds, executable bits, symlink
targets and child digests. `collect_closure` already reads every `Directory` of the input root before every submit and
drops each after taking its digests [code]; with trees on, it keeps them for the writer, at most
`--task-closure-max-digests` (500,000 by default), released when the job is submitted. The walk the frontend makes
today stays the only walk; what is added is building, hashing and storing the manifests, measured in section 10.

**Where each blob is.** A file entry names its blob and, unless the blob is a whole object `cas/l/<sha256>`, a
`source: {key, offset}` (ts 6.1). The pack index gives `{pack key, offset}` for a packed blob, and tierstore's presence
of `cas/l/<hex>` covers a large one; these are the lookups `GET /locate` makes (`job_read/locate.rs`) [code], used here
through a frontend-internal interface in chunks, **not** through the listener's `/locate`, whose per-request bounds
(65,536 packed digests, 256 presence checks, a 10 s deadline, `job_read.rs` `Limits`) would refuse trees to large
toolchain closures.

**Every blob is made locatable** [proposed, D6]. A blob with no location (only in a storage node's write tier or an
open segment, or a large blob whose `cas/l/` object was never written) is written by the writer itself to
`cas/l/<sha256>`, streamed from the frontend's CAS through a SHA-256 check, with a create-only PUT, before the job is
submitted. So a tree is never refused for where a blob happens to be, and after the fallback is gone (7.2) nothing
needs it.

**The manifests.** One per REAPI `Directory`, bottom-up:

| REAPI `Directory` node | Manifest child |
|---|---|
| `FileNode {name, digest, is_executable}` | `{"name", "kind":"file", "sha256", "size", "exec": is_executable, "source"}`; no `source` for a whole object or a file of size 0 |
| `DirectoryNode {name, digest}` | `{"name", "kind":"dir", "sha256": <child manifest's digest>, "size": <child manifest's length>}` |
| `SymlinkNode {name, target}` | `{"name", "kind":"symlink", "target"}` |

Children are sorted bytewise by name across REAPI's three node lists. A manifest has no field for REAPI's
`node_properties` (`unix_mode`, `mtime`, named properties): a `Directory` that sets any, on itself or a node it lists,
gets no tree (4.6). The download does not honour them either (`kbf-task` sets only the mode from `is_executable`,
`crates/kbf-task/src/fetch.rs`, `create_file`) [code], so this keeps a tree from claiming a property it drops, and
changes nothing an action sees today.

**The action tree** is one more manifest on top: `{"work": <the input root's manifest>, ".kbf": <a manifest of two
files, "action" and "command", whose sha256 are the Action's and the Command's digests>}`. Its digest is the tree named
in the job.

Each new manifest is written with a create-only PUT to `cas/l/<sha256>` through the frontend's tierstore client (the
one the large-object backfill uses, `crates/kbf-front/src/large_backfill.rs`). An existing key is success: the key is
content-bound. A failure is "no tree" for that job (4.6).

### 4.2 Naming, lifetime, cache keys

- **Names.** A tree is named by the digest of its root manifest, not by the REAPI input-root digest: two manifests of
  one input root differ when a blob moved (a pack compacted), which is a new tree and costs a miss (ts 6.1).
- **The memo** maps a `Directory` digest to its manifest's digest, so a shared subtree (a toolchain) is built and PUT
  once; a hit skips both. It is bounded by entries (oldest out first), and each entry expires after a fixed time (10
  minutes, a constant, B20; on the frontend's injected clock) and no later than the earliest-expiring of its children.
  An input failure (section 6) drops every entry of that action's input root, so the rerun's manifests come from the
  index as it is then.
- **tierstore's caches** are keyed by blob SHA-256 (`_tiering/fill/<sha256>` on a build node), so a file read by two
  actions, two trees or two jobs on a node is fetched once per node. A tree's blobs are pinned while it is attached
  (ts 6.5). The base and tools trees are attached by every job, so their read blobs stay pinned on every busy node.
- **The attachments** are the job's: the agent attaches the three trees for its run before it makes the container and
  detaches them at every end (as js TI 3.3 and 3.5 do for one tree). kbf never talks to tierstore's control socket.
- **The manifests** stay in the store. tierstore never deletes a manifest or a blob, and the client must not delete one
  an attached tree uses (ts 6.1). kbf deletes nothing under `cas/l/` today (the `cas/b/` janitor never lists it); its
  collection is an open point (section 14).
- **A job cannot change a tree**: the manifests are the frontend's, written from `Directory` messages it read and
  checked, under keys that are their own SHA-256; the job's write grant covers only its `cas/b/` keys.

### 4.3 What `kbf-task` does in a tree job

With `--tree`, `kbf-task`:

1. reads `/.kbf/action` and `/.kbf/command`, hashes the Action and checks it is the digest of its `--action` flag (so a
   job never runs another action's tree), and decodes both; there is no read grant and no fetch;
2. before the Command starts, makes the parent directory of every output path and the working directory under `/work`
   (in the upper layer), as REAPI asks of the worker;
3. runs the Command in the sandbox of 4.4, under the action's timeout;
4. collects the outputs from `/work`, packs and PUTs them as today, and records `"from": "tree"` in the manifest's
   advisory `inputs` statistics (`kbf-task-pack` `InputStats`; the reader that knows `from` ships one release before the
   writer, because `InputStats` refuses an unknown member, D13).

### 4.4 The Command's sandbox

[proposed, D7] In every job, tree or not, `kbf-task` starts the Command in its own mount and PID namespaces, beside
the user and network namespaces it already makes (`network.rs`, `CLONE_NEWUSER | CLONE_NEWNET`) [code]:

- `/run/js-files` (the PUT URLs) is covered by an empty read-only tmpfs, and so is `kbf-task`'s own run directory
  (`/.kbf-run` in a tree job, in the upper; under `/scratch` in today's job); in today's job, `/scratch` is covered the same way after `private/` is bound at
  `/tmp`, `/var/tmp`, `/run` and `/root`, so those are disk-backed there too instead of the job's 64 MiB `noexec`
  tmpfs (`js-contain` `TMPFS`) [code];
- the PID namespace ends every process the Command started when it exits, so nothing of the action is running while
  its outputs are read (which `kbf-outputs` requires);
- the output walk opens each component with `openat2` and `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS |
  RESOLVE_NO_MAGICLINKS` on Linux (today it opens each relative to its parent with `O_NOFOLLOW`,
  `crates/kbf-outputs/src/collect.rs`) [code], so the kernel refuses a path out of `/work`, a `..` included, in an upper
  layer the action wrote.

The frontend's MAC key (`--task-secret-file`) is never in a job.

### 4.5 Prefetch at submit

[proposed, D8; O12] Right after `SubmitJob` answers, the executor sends tierstore **`prefetch`** with the digests of the
blobs it wants warm, each with its size and `source`, and does not wait for the fill. Not the whole tree: an input root
that holds a toolchain names gigabytes the action mostly never opens, and prefetching it all would undo the lazy read.
kbf names the blobs of the files this action's tree does **not** share with an earlier one (the files under
`Directory` manifests the memo of 4.2 did not have: the action's own sources and generated inputs), largest last, up to
256 MiB per action (a constant, B20). Shared subtrees are left to lazy reads and the node caches, where earlier actions
already put what they read. kbf sends the hint twice:

1. **at submit**, to the farm's storage tier (the tierstore the build nodes fill from), which warms the shared copy for
   whichever node the job lands on;
2. **at placement**, when `GetJob` first shows the job's `host_id`, to that node's build-node tierstore, which warms
   the node's local cache, the one `tierstore fs` reads.

The submit hint is the one that carries the benefit: it is sent before any wait. The placement hint is useful only
while the start waits for a heartbeat; with the pushed start section 13 asks for, it lands about when the container
starts, and 10.3 measures whether it still helps or should be dropped.

The call kbf needs (section 13): `POST /_tierstore/v1/prefetch` with `{"blobs": [{"sha256", "size", "source"}]}` on
tierstore's existing front, authorized by the frontend's key, answered `202` at once. It is a hint, never an error for
the job: a failed or dropped hint costs only speed; how tierstore queues and orders it is tierstore's. The fill is the
fill socket's existing ranged `fill`, started by a call. The base and tools trees are hinted whole (they are small and
every job reads from them), once per node when their digests change. The node-to-endpoint map for step 2 is one
`kbf.toml` table, checked at start.

### 4.6 Today's job, the temporary fallback

Until its removal (7.2), the executor submits today's job (the kbf OCI image, inputs downloaded) instead of a tree
job when [proposed, D9]:

| Reason (`why`) | When |
|---|---|
| `off` | `executor-trees` is off (8.3) |
| `whole_host` | a `whole_machine` lease (D12) |
| `store` | the index or tierstore did not answer within the writer's deadline, or a PUT failed |
| `unrepresentable` | a name tierstore refuses (`.wh.` prefix), `node_properties` (4.1), or a tierstore limit passed (2^24 - 1 entries, a manifest over 64 MiB, depth over 4096) |
| `refused` | the resubmit, once, of a tree job whose submit or start was refused (section 6) |

Each is counted (`kbf_executor_inputs_total{from="fetch",why}` beside `{from="tree"}`). These are states that change
while the farm runs, not configuration mistakes (B20); the share of each is a number of section 10, and the removal of
7.2 waits on it.

Note what is **not** a fallback any more: `JS_TREES_UNATTACHED` (js TI 2.2) lets a job start without an input tree
and stage it itself. A tree job cannot: its root file system is the trees, so a tree that cannot be attached means the
container cannot be made, and the start fails (section 6). kbf does not use `job_stages_if_unavailable`.

## 5. Outputs

Built, kept, with one rule added [code; O2]:

1. **The fenced keys.** Per lease the frontend mints `cas/b/<build>/<nonce>-<k>` pack keys and `-L<i>` large-object
   keys, MAC'd with `--task-secret-file` (`crates/kbf-front/src/prepare_run.rs`), and presigns one create-only PUT per
   key with a presign-only credential, good for the wall limit plus `--task-url-slack-secs`. The job can write those
   keys once and nothing else; it holds no delete right and no store credential (B16).
2. **The promotion, from the result only** [proposed, D10]. A pack is promoted only when `GetResult` says state
   `SUCCEEDED`, reason `EXIT`, exit code 0 and no `input_failure`; never from a PUT. Any other result leaves the keys
   to the janitor, however complete the pack looks: an action that swallowed an `EIO` from a failed tree read and
   exited 0 has already PUT its pack before the agent's detach reports the failure. Today's executor already ingests
   only after the job's end (`executor/runs.rs`, `Verdict::Ingest`) [code]; this makes the four conditions the rule.
   The ingest (`crates/kbf-front/src/proxy/packed/commit/untrusted/task.rs`, `ingest_set`) then verifies the keys'
   MACs, the packs, the manifest (this lease, this action), each digest's single place and the output `Tree`s, streams
   large objects through SHA-256 into the CAS, hashes every record as it records the packs (pack 0 last), and only then
   writes the action cache (exit 0 and not `do_not_cache`).
3. **Cleanup.** A key no ingest recorded is deleted by the `cas/b/` janitor after `--task-janitor-grace-secs` (at least
   6 h); it is off by default and a precondition of the flip (8.1). The run's upper layer, outputs included, is removed
   with the run directory by the agent, as root (js TI 3.5; whiteouts and `work/work` cannot be removed by the action
   user, ts 6.6).

`kbf-task` reads outputs from `/work`, the merged view (an output the action wrote is in the upper layer; an unchanged
input named as an output is read lazily through the tree). js TI 3.2 refuses agent-side outputs on `crun`, so the
task's pack stays the one output path, with one trust boundary, the ingest. B16's "read once, hash and store in one
pass" is the task's pack writer's, as today.

## 6. Failures and retries

Every row ends in one of the farm's existing classes (`executor/outcome.rs`, then `answer.rs`), so the client's
answers do not change. "Rerun" is a new farm lease and charges `FARM_RERUNS` (2); after it the client gets `INTERNAL`.

| What happens | The job service says | The executor does (new rows marked) | The client gets |
|---|---|---|---|
| Inputs missing from the CAS at Execute | (no job) | (no job) | `FAILED_PRECONDITION` with `MISSING` violations, as today |
| A tree job's submit is refused (`no-capable-host`: no registered host can attach trees; or the root-tree field's own refusal) | `SubmitJob` refusal | **new**: while the fallback exists, submit the lease again as attempt 1, today's job (`why=refused`), once; after 7.2, infra | the action's result |
| A tree job's start fails (`AGENT_REFUSED` or a start timeout: a tree could not be attached, `tierstore fs` down, or a refusal the empty start detail cannot name, kbf#2287 item 1) | `FAILED_TO_START` | **new**: the same resubmit, once, while the fallback exists; after 7.2, infra, rerun | the action's result, or `INTERNAL` |
| A fill fails while the job runs (`EIO`: a moved pack, the store down past tierstore's bound) | `FAILED`, `INPUT_FAILED`, `tree-read-failed`, whatever the exit code | **new**: infra, rerun; the root's memo entries dropped; the pack not promoted (5) | the rerun's result, or `INTERNAL` |
| The job passed its wall limit and the detach reports `stall_ms` over a tenth of it | `FAILED`, `INPUT_FAILED`, `tree-stalled` | **new**: infra, rerun | as above |
| The detach at exit is not answered and the job did not succeed | `FAILED`, `INPUT_FAILED`, `tree-unavailable` | **new**: infra, rerun | as above |
| The submit is answered with a spec that lacks the root trees (a server older than the root-tree release) | `SubmitJob` answer | **new**: cancel, infra, an error naming the server | as above; fixed by upgrading the server |
| The action's own timeout (fired inside `kbf-task`: exit 0, a timeout in the manifest) | `SUCCEEDED`, exit 0 | promote, as today | `DEADLINE_EXCEEDED` with the partial result |
| The job's wall limit (the task itself hung) | `FAILED`, `WALL_LIMIT` | timeout, as today | `DEADLINE_EXCEEDED` |
| The job's memory limit | `FAILED`, `MEMORY` | the memory ladder, as today | the result after the ladder, or `FAILED_PRECONDITION` with `OUT_OF_MEMORY` |
| The agent or host is lost | `LOST` | lost, rerun, as today | the rerun's result, or `INTERNAL` |
| Cancelled by someone else; `kbf-task` exits 10 to 15; the pack refused at ingest | as today | infra, rerun, as today | as above |
| The action writes more than the scratch volume holds | the action's own `ENOSPC` | the action's result | its nonzero exit, as with a laid-out tree |

Notes:

- **`INPUT_FAILED` is a rerun even after exit 0**, and its pack is never promoted (5): an action can tolerate a
  missing optional file and exit 0 with a wrong output (ts 7.4).
- **The resubmit is within the lease** (attempt 1; the job id's attempt field exists for this), at most once, and
  charges no farm budget: it is today's job, whose own failures are classed as today.
- **Fill time is the action's time.** A file's bytes are fetched on its first open, inside the action's run. The
  prefetch of 4.5 is what keeps that off the action's clock; how much it does is measured (10.3).
- **A stall the action's own timeout catches is not seen**: js TI 5's `tree-stalled` needs the job's wall limit, and
  kbf's action timeout fires inside the task first. Section 13 asks for `stall_ms` in the result, with which the
  executor applies the same rule to the action's timeout.
- **A rerun may land on the same host**: the farm avoids a virtual worker, not a host (`farm.rs`, `avoided`), and the
  job spec cannot name a host to avoid; section 13 asks for one, and until then the operator restarts or stops a bad
  node's agent. Section 10 counts input failures by `Job.host_id`.
- **Retries stay kbf's**; the job service retries nothing.

## 7. What is deleted, and when

### 7.1 The daemon path

After the executor switch has held for one release with no rollback, in two steps (section 11 lists them as items 18
and 19):

1. **The nodes move first**: the six nodes run `kbf storage` instead of `kbf storage-and-daemon` (an operations step;
   the storage half is the same code, so the store keeps serving). From then the executor's rollback is the previous
   release and units, not a line (8.4).
2. **Then the code**, once no unit selects `--driver container`: `crates/kbf-driver-container` (11,704 non-test lines of
   `src`), the daemon's container wiring, Linux input cache and shared-tree code in `kbf-daemon` and `kbf-node` (Macs
   keep the `native` driver and the clone cache), and the placement filter of 8.2.

### 7.2 The copy to scratch (O11)

The download path is today's job: the kbf OCI image, the read grant, `kbf-task`'s fetch. It is removed when all of
these hold, each read from section 10's numbers, not assumed:

1. the trees switch has been on for one release with no rollback;
2. all six nodes run the 7.0 kernel (passthrough and FUSE resend, ts C8), so a `tierstore fs` restart costs no job;
3. over that release, **no** action fell back as `unrepresentable` or `whole_host` (after the removal those would fail
   for good, so the bar is zero, not a share: `whole_host` needs jobscheduler's answer of section 13 first), and the
   transient reasons (`store`, `refused`) are below one action in a thousand and traced to their causes;
4. the root-tree job's start failures (section 6) are no more frequent than today's job's.

Then, one pull request each (section 11, item 20 and after):

| Deleted | Why it can go |
|---|---|
| `kbf-task`'s fetch, locate and batch code (`fetch.rs`, `locate.rs`, `batch.rs` and their tests), its fetch flags and the read-grant files | nothing is fetched by a job |
| the frontend's job-facing read listener (`--job-listen`, `job_read*`), its read grants and store grants, and the large-object backfill tee that rides on it | nothing reads through it |
| the kbf OCI image (`pkg_image_asset`'s task image, `packaging/container/kbf-task/`) and `--executor-kbf-image`, `--executor-base-image`, `--executor-locate`, `executor-trees` | the base and tools trees replace the image; trees are the only job |
| the resubmit as today's job (D9) and the `why` reasons | there is nothing to fall back to |

After the removal an action whose input root cannot be a tree is refused by name: `INVALID_ARGUMENT`, naming the
`node_properties` or the tierstore limit it hit (it was never going to run differently). A tree job whose start fails
is a farm fault and rerun, like a lost host today. The job network's allow-list keeps only the PUT endpoint.

What stays: the farm, the executor, `kbf-job-client`, `kbf-task` (tree mode), `kbf-task-pack`, `prepare_run`, the
ingest, the worker listener and `kbf-daemon` for the Macs.

## 8. The flip on the six Linux nodes

Two switches, each set for the whole farm at once (O6), each with a rollback line and a gate (O10).

### 8.1 The first switch: the executor (today's job)

Preconditions, checked once:

1. **`js-lease-server`** at a release kbf works with (lower-case labels or the frontend id made lower case at its
   source, one client token across frontends, the body limit; kbf#2287 items 2, 4, 5); servers before clients.
2. **`js-agent`** on all six nodes as the `.deb` from the apt repository, set by hostkeeper
   (komira-ai/jobscheduler#841), `--runtime crun`, `uidmap` installed, its flags file holding the egress lines
   `kbf-deploy job-service agent-egress` prints, joined to the farm pool.
3. The pool books no disk, and kbf no longer asks for it (item 2).
4. The `cas/b/` janitor is on, on the leader; secrets go as files.
5. Items 3 to 5 of section 11 (placement filter, sandbox, promotion rule) are in the running release.
6. **The gate**: the no-op benchmark of 10.1 has run, arm B against arm A, and its report is with the owner.

The switch is the existing `kbf-deploy job-service apply --apply` (staged `kbf.toml`, `kbf --check`, a drain that waits
by default (B15), restart, the canary). **Rollback line:** `kbf-deploy job-service back-out --apply`, which puts the
kept `kbf.toml` back byte for byte and restarts the frontend; by hand, `executor = "kbf-daemon"` with
`executor-secret-delivery = "files"` and `executor-locate` removed in the same edit, since `kbf-daemon` refuses both
(`executor_flags.rs`, `job-service-cutover.md`). `back-out` restores the whole file kept before the switch, so every later edit (the trees switch, the tree
digests, the prefetch node table) is undone with it; with `executor = "kbf-daemon"` those keys are inert anyway. The
drain of the restart waits for the jobs in flight (B15).

### 8.2 The Linux daemons during the window

The daemons keep running beside `js-agent` until the nodes move (7.1), so the rollback is one line. While
`--executor js-agent` is set, the farm's placement skips every Linux daemon worker; Mac daemons are placed as before
[proposed, D11]. It is a filter computed at placement from the running setting, not a cordon (a stored row only the
operator lifts, `farm/cordon_owner.rs`), so the rollback's restart leaves nothing behind.

### 8.3 The second switch: trees

`executor-trees = true`. Preconditions, checked once:

1. jobscheduler's root-tree jobs, the pushed start and the pushed or long-polled result (section 13) released, the
   lease server upgraded, kbf's pin moved (item 14); the gate below cannot be met without the last two.
6. The three-lower overlay over `tierstore fs` measured on the farm's 6.8 and 7.0 kernels (section 13). A node where it
   fails gets start failures, which kbf resubmits as today's job (section 6), until it is fixed.
2. On each node: `tierstore fs` running (`tierstore-fs@a`, its marker made, `fs.owner` the agent's job uid, which the
   agent's start-time probe checks), the agent started after it and reporting the capability; the node's tierstore
   reading the bucket and prefix the frontend's pack store writes; `--scratch-low-water-gib` at least the cache budget
   where the cache shares the scratch volume (js TI 6); tierstore's `prefetch` call (4.5) released on every instance.
3. The base and tools trees built (`kbf-deploy job-trees --apply`) and named in `kbf.toml`.
4. A one-node run: a real build action as a tree job, outputs promoted, the run directory gone, the agent's integrity
   check clean.
5. **The gate**: the no-op benchmark, arm C against arm A, meets the target of 10.1, and the cold first-byte numbers
   of 10.3 are with the owner.

On a 6.8 node there is no FUSE resend: a crash or restart of `tierstore fs` aborts the reads in flight (ts 7.2), and
those jobs end `tree-read-failed` and are rerun; until tierstore's update hook acts on `tierstore-fs@`, a node's
`tierstore fs` is updated only after its agent is drained (B15). **Rollback line:** `executor-trees = false` and a
frontend restart: every job is today's job again, nothing changed on the nodes.

### 8.4 Fix forward, and when a rollback stops being one line

A defect after a switch is fixed in the next release, not by switching back, unless builds fail at a rate the rollback
would end; the owner calls the rollback. The trees switch's rollback is one line until 7.2 removes today's job; the
executor's until the nodes move (7.1). The release notes say when each ends.

## 9. Tests that prove it

Hermetic tests run in the pull request that builds the part; real-service, farm and benchmark runs are beta or
runbook, never in the pull-request set (B9). No test asserts a duration (T10): bounds use the paused or injected clock,
and fakes answer on explicit events.

**Executor** (`crates/kbf-server/tests/executor/`, against `FakeJobServer` taught root trees, `INPUT_FAILED` and the
spec echo):

- `a_tree_job_names_its_three_trees_and_runs_kbf_task`: the spec field by field as in 3.4, the trees top first;
- `todays_job_is_byte_for_byte_today_but_its_attempt`, once for each `why` of 4.6;
- `no_job_books_disk`, for a shared and a whole-host lease;
- `a_pack_is_promoted_only_from_a_clean_result`: a valid pack already PUT is not promoted for `FAILED`, for
  `INPUT_FAILED` or `WALL_LIMIT`, for a nonzero exit, or for `SUCCEEDED` with an `input_failure`; the clean result
  promotes it;
- `input_failed_is_a_rerun_whatever_the_exit_code`: one case each for `tree-read-failed`, `tree-stalled` and
  `tree-unavailable`, each with exit 0 and with a nonzero exit (the agent decides `tree-stalled`; its boundary of a
  tenth of the wall limit is jobscheduler's test, js TI 8);
- `an_input_failure_drops_the_roots_manifests`;
- `a_refused_tree_submit_and_a_failed_tree_start_are_resubmitted_once_as_todays_job`, and the second refusal classed as
  today;
- `a_spec_answered_without_its_trees_is_cancelled_and_rerun`;
- `a_resubmit_after_a_lost_answer_sends_the_same_trees`, with the memo changed between the sends;
- `prefetch_is_sent_at_submit_and_at_placement_and_never_waited_for`: a hint that fails, hangs or is refused changes
  nothing of the job;
- the outcome table's test with the new rows.

**Manifest writer** (`crates/kbf-front`):

- golden manifests (a file, an executable, an empty file, a large object, a symlink, a nested directory, the action
  tree's `work` and `.kbf`), byte for byte;
- every manifest parsed and flattened by tierstore's own `tree_manifest::parse_manifest` and `flatten` (a test
  dependency at kbf's tierstore pin), so writer and reader cannot drift;
- children sorted across REAPI's three node lists; a shared subtree built and PUT once;
- the memo on an injected clock: the bound, expiry, a parent no later than its earliest child;
- an unlocated blob written to `cas/l/` through a SHA-256 check before the tree is named; a blob whose bytes do not
  hash is not written and the job is today's (`why=store`);
- 65,536 and 65,537 packed blobs, 256 and 257 large ones, all in one tree (chunks, not the listener's caps);
- each tierstore limit at its boundary and one past it, a `.wh.` name, and `node_properties` on a `Directory`, a file
  node and a directory node: `why=unrepresentable`;
- a create-only PUT that finds the key: success; one that fails: `why=store`.

**Frontend start**: a `kbf.toml` whose base or tools tree is missing from the store, whose base tree holds `/work`,
`/.kbf` or `/.kbf-tools`, or whose prefetch node table names an unparsable endpoint, is refused by name; at start the
base and tools trees are hinted once.

**`kbf-deploy job-trees`**: a base image with each refused kind (set-uid, set-gid, a device, a FIFO, a socket, an
extended attribute, a reserved path) refused by name; modes 0600, 0640, 0700, 0750 and 1777 normalised to 0644 or
0755 and listed in the dry run; whiteouts across layers applied; a hard link made two entries; the dry run writes
nothing; the digests printed equal the trees' root manifests.

**`kbf-task`** (`crates/kbf-task`):

- `--tree`: the Action and Command read from `/.kbf` (a test directory standing in for the root), an Action whose
  digest is not `--action`'s is exit 10 with its line, nothing is fetched;
- the output paths' parents and the working directory exist before the Command starts;
- the sandbox, in the CI job that already runs its namespace tests: the Command cannot see `/run/js-files` or the
  task's run directory; in today's job it writes and runs a program from `/tmp`, `/var/tmp` and `/run`, with the bytes
  under `private/`; a background process it leaves is gone before outputs are read; an output that is a symlink out of
  `/work`, a directory renamed to one, and a `..` are each refused by the walk;
- `InputStats.from` written; B8 pair: `a_manifest_with_from_is_read_by_the_item_8_reader` (the previous release on new
  data, since item 8 is the previous release of item 12) and `a_manifest_without_from_is_read_by_this_release`; and
  `an_unknown_member_is_skipped` in `kbf-task-pack`.

**Frontend farm:** `linux_daemons_get_no_lease_while_the_executor_runs`,
`a_mac_daemon_is_placed_beside_the_virtual_workers`,
`a_restart_under_kbf_daemon_places_the_linux_daemons_and_leaves_no_cordon_row`.

**Real service** (beta, root, `crates/kbf-integration-test`, the pinned `js-lease-server` and `js-agent`, a fake
`tierstore fs` control socket in the test's scratch directory): `a_tree_job_reaches_a_host_that_reports_trees_only`,
`a_tree_job_whose_trees_cannot_be_attached_is_resubmitted_as_todays_job`,
`a_tree_job_with_no_capable_host_is_resubmitted_as_todays_job`.

**Run directory removal** is the agent's (as root, whiteouts and `work/work` included) and tested in jobscheduler by
js TI 8's removal case; kbf adds no test of it. **After the removal of 7.2**, item 20's pull requests carry the tests of
an input root refused by name (`node_properties`, each tierstore limit) and of a failed tree start rerun as a farm fault.

**Farm integration** (a runbook on the nodes): real `crun`, `tierstore fs` and the three trees; an action that reads a
file of its input root and one of the base tree, writes an output, changes and deletes an input: the output promoted,
the run directory gone, the integrity check clean. Then with `tierstore fs` stopped (resubmitted as today's job), and
with a pack of the tree deleted between attach and open (`INPUT_FAILED`, rerun, success).

**Conformance**: a pinned Buck2 build of the farm's usual targets, through `kbf-harness`'s B0 scenario against a
frontend in each arm of section 10, gated on B0's gates (zero failed builds and actions, `kbf_cache_lost_blobs == 0`);
each action's outputs compared with the daemon arm's action-cache entry for the same action digest, and actions that
differ only by an embedded exec-root path (`/kbf/root` under the daemon, `/work` in a job) listed by name, not failed.

**Why this list is complete for the failure and boundary cases.** It takes each new row of section 6 (each refusal at
submit and at start, each `input_failure` code with each exit, the dropped trees, each result that must not promote and
the one that must), each `why` of 4.6, each lookup bound and tierstore limit on both sides, each place `node_properties`
can be set, each refused kind of the tree builder, each way out of the sandbox (a hidden path, a leftover process, a
symlink, a rename, a `..`), the prefetch hint's failure, hang and refusal, the lost-answer resubmit, the one format
change and its release order, and the placement filter on, off and after a rollback. Unchanged rows keep their tests.

## 10. Numbers, and the gate

Measured, not asserted (T10), on the six nodes; arms interleaved, warm-ups labelled and excluded, and two arms
compared by `js-analysis`'s rule (the 95% Welch interval of the difference excludes zero) [proposed]:

| Arm | What |
|---|---|
| A | today: `kbf-daemon`, the container driver, the node input cache |
| B | the executor, today's job (OCI image, inputs downloaded) |
| C | the executor, tree jobs, `tierstore fs` in the 6.8 mode |
| D | the executor, tree jobs, passthrough (a 7.0 node) |

### 10.1 The no-op benchmark: the gate (O10)

**Where.** Two frontends on the same six nodes and the same job service, used at once: arm A is a frontend in
`kbf-daemon` mode with the Linux daemons; arm B or C is a frontend in `js-agent` mode with its own frontend id (a
benchmark frontend, not serving builds). The client alternates blocks between the two, so load and drift on the nodes
fall on both arms alike.

**What.** An action whose Command is `./noop` (a static program in its input root that exits 0), whose input root
holds that one file, with a fresh value of an environment variable in each Command so that every run is a new action
digest, sent with `skip_cache_lookup`. Through each arm: 300 runs one at a time, then 300 with 64 in flight, in blocks
of 20 alternating between arms.

**How.** A new verb, `kbf-bench noop` (Rust, beside the existing `upload` and `read`), sends the Executes and records,
per run, the time from sending Execute to the done operation at the client, with the arm and the frontend's phase
metrics for that lease (`kbf_executor_phase_seconds`, the daemon's `lease phases:` line), as JSON lines that
`js-analysis` reads. Nothing in it asserts a number.

**The target** [proposed, D14], on idle nodes with warm caches (the base and tools trees and `./noop` already in each
node's cache), one at a time:

| | p50 | p90 |
|---|---|---|
| a tree job (arm C) against today's daemon (arm A): the extra time per action | at most **50 ms** | at most **100 ms** |
| a tree job, Execute to done, end to end | at most **250 ms** | at most **500 ms** |

The gate is met when the upper end of each 95% interval is under its target; the report goes to the owner with each
part of the time. Arm B is reported against A before the executor switch, against the same numbers; it is expected to
miss them (it downloads), and the report says by how much and where.

**Where the milliseconds go, and what the target needs** (inferred, to be replaced by the measured parts):

| Part | Expected | What it rests on |
|---|---|---|
| prepare: closure, keys, manifests (memo warm) | under 10 ms | a few small manifests per action |
| submit and the service's placement pass | under 10 ms | one RPC and one pass |
| the start reaching the agent | **one heartbeat interval today** (default 5 s; 250 ms in kbf's real-service test pool) | the agent learns of a start at its heartbeat; the target needs the start pushed to the agent (section 13) |
| attach of three warm trees | under 10 ms | flattening cached manifests |
| `crun` create and start, overlay, network | about 58 ms | measured: 57.8 ms median with the allow-list (jobscheduler `agent-egress.md`, `js-launch-measure`) |
| `kbf-task` start, sandbox, `./noop`, pack, PUT | under 30 ms | one small pack |
| the result reaching kbf | **the executor's poll interval** plus the agent's next heartbeat | the target needs a long-poll or pushed result (section 13) |
| ingest | under 20 ms | one small pack |

So the target is reachable only when the start and the result stop waiting for a heartbeat and a poll; that is the
first thing the benchmark will show, and section 13 asks for it.

### 10.2 What else is measured

1. **Per-action overhead** on real builds = lease to result minus the Command's own run time, split as above
   (prepare, start, queue, task and ingest from the executor's phases; run and pack from the manifest's `timing`, the
   task's own claims, labelled so; arm A from the daemon's `lease phases:`), and client side by `kbf-baseline`.
2. **Cold, warm and partial**: caches emptied (tierstore's on the nodes and the storage tier, the daemon's input
   cache), the same build again, one input changed; and the prepare phase of a large toolchain root with the memo cold
   and warm.
3. **Build wall time** of the conformance builds.
4. **The fallback share** by `why` (4.6), and failures: `INPUT_FAILED` per thousand by code and host, `EXDEV`,
   timeouts per arm. These decide the removal of 7.2.

### 10.3 The prefetch, measured

Per action: how many of the blobs it opened were already in the node's cache at first open, with the prefetch hint
sent and with it off (one arm of C each), and the time to the first byte of the largest toolchain files (each 100 MB
or more) cold, on a 6.8 node and a 7.0 node. If the hint at placement arrives after the first opens, the numbers say
so, and the answer is section 13's (a hint the agent forwards, or a placement the hint can follow).

## 11. Build items, in order

Code items are one pull request each; items 7, 17, 18 and 20 are operations steps. Items 1 to 12 need nothing new
from jobscheduler or tierstore; item 13 needs tierstore's `prefetch` call. Each pull request updates the documents named with it.

1. **This design.**
2. **Executor: no disk booking**; `--executor-output-disk-bytes` removed, refused by name. Docs: `kbf-modes.md`.
3. **Farm: Linux daemons get no lease under `--executor js-agent`** (8.2). Docs: `kbf-modes.md`,
   `job-service-cutover.md`.
4. **kbf-task: the sandbox** (4.4) for today's job. Docs: `kbf-modes.md`.
5. **Executor: the promotion rule** (5) and its tests.
6. **kbf-bench: `noop`** (10.1). Docs: its README.
7. **The executor switch** (8.1), after the no-op report of arm B.
8. **kbf-task-pack: the manifest readers skip unknown members** (in `InputStats` and the manifest's other objects,
   B8), and `from` read and counted.
9. **tierstore pin** to a release with `tree_manifest` (a test dependency of items 10 and 11).
10. **kbf-front: the manifest writer**: `collect_closure` keeping `Directory` messages, the lookups in chunks, the
    `cas/l/` writes of unlocated blobs, the manifests, the action tree, the memo, `node_properties`; no caller yet.
11. **kbf-deploy: `job-trees`** (3.3) and the frontend's start check of the two digests. Docs: `job-service-cutover.md`.
12. **kbf-task: `--tree`** (4.3) and writing `InputStats.from`, one release after item 8.
13. **Executor: the prefetch hint** (4.5), against a release of tierstore that has the call.
14. **jobscheduler pin** to the release with root-tree jobs, the pushed start and result, and `input_failure` (the fake
    taught them, the spec echo, the field-set test).
15. **Executor: tree jobs**: 3.4, `executor-trees`, the resubmit as today's job, the counters, the rows of section 6.
    Docs: `kbf-modes.md`, `job-service-cutover.md`, `docs/api.md`.
16. **kbf-integration-test: the real-service tree cases** (beta).
17. **The trees switch** (8.3), after the one-node run and the gate of arm C.
18. **The nodes move to `kbf storage`** (7.1).
19. **Delete the Linux container driver**, the daemon's Linux-only wiring and the placement filter. Docs: `kbf-modes.md`,
    `job-service-cutover.md`, `docs/artifacts.md`, `docs/design/daemon.md`.
20. **The removal of the copy to scratch** (7.2), when its four conditions are read from section 10; then one pull
    request per row of its table, the task's fetch first and the image last. Docs: the same, and `docs/artifacts.md`
    for the task image.

## 12. Decisions

Each was not decided by the rulings or the two designs; the owner may overturn any. Two are changes to jobscheduler's
public interface and go to the owner's review of public interfaces before jobscheduler builds them: the root-tree job
of section 13 (which replaces, for kbf, js TI's `JobSpec.trees` at `/work`) and anything that lets a job name a mount
outside its root.

- **D1. The second executor becomes the only Linux path**: it is built, tested against the real service, and carries
  O2 and O3.
- **D2. The root is one overlay over three trees, top first: action, tools, base** (3.1); their paths never meet, so the
  order changes nothing an action sees, and the per-action tree is on top.
- **D3. The base tree is converted from the farm's pinned base image by a held operator verb**, which refuses what a
  tree cannot hold rather than dropping it; the tools tree is `kbf-task` alone, so the base does not change with kbf
  releases.
- **D4. kbf stops booking disk** (O4; kbf#2287 item 3).
- **D5. The frontend writes the manifests**, from the `Directory` messages its closure already reads and its own pack
  index, not through the read listener's capped `/locate`.
- **D6. Every blob is made locatable before submit** (written to `cas/l/` if it has no location), so where a blob
  lives never refuses a tree.
- **D7. The Command runs in its own mount and PID namespaces** in every job: the grants out of sight, no process left
  while outputs are read, the output walk resolved beneath `/work` by the kernel; in today's job also disk-backed
  `/tmp`, `/var/tmp` and `/run`.
- **D8. Prefetch is a tierstore call by tree digest, sent at submit to the storage tier and at placement to the node**,
  never waited for; the node is not known at submit.
- **D9. Today's job is the fallback while it exists**, chosen per action for the reasons of 4.6, and resubmitted once
  for a refused tree job; `JS_TREES_UNATTACHED` is not used, since a root made of trees cannot start without them.
- **D10. Outputs are promoted only from a clean result**, never from a PUT.
- **D11. Linux daemons are skipped at placement while the executor runs**, not stopped or cordoned.
- **D12. `whole_machine` leases get today's job until jobscheduler says how a root of trees orders with a whole-host
  start.**
- **D13. The fallback is counted from the task's manifest** (`InputStats.from`), reader first; the reader also skips
  unknown members from then on, so a frontend rolled back one release reads a newer task's manifest.
- **D14. The gate's proposed target is 50 ms extra at p50 and 100 ms at p90 against the daemon, 250 ms and 500 ms end
  to end**, for a warm no-op action. Only `crun`'s 58 ms in it is measured; the rest are inferred from 10.1's parts, so
  the owner sets the final numbers on the first report.
- **D15. An input failure charges `FARM_RERUNS`** like any farm fault.

## 13. What this asks of jobscheduler and tierstore

None of these is kbf code; each is a change for its own repository and design.

**jobscheduler** (additive):

- **A job whose root file system is an ordered list of trees**, top first, over the run's writable upper on the scratch
  volume, with no image: no pull, no unpack, the command an absolute path in the trees, the job's environment its
  `env` alone; the trees attached in the preparing stage and detached at every end as js TI 3.3 and 3.5 attach one;
  `/proc`, `/dev`, `/sys` and `/run/js-files` bound over directories the trees hold, no `/scratch` bind; no tmpfs on `/tmp`,
  `/var/tmp` or `/run`. A public interface change (owner's review); it supersedes, for kbf, a `/work` tree of js TI.
- **A pushed start and a pushed (or long-polled) result**, so neither waits for a heartbeat or a client's poll: without
  them the target of 10.1 cannot be met.
- `stall_ms` in the public `JobResult`; `Job.start_failure_detail` filled from the agent's refusal code (kbf#2287 item
  1); a host to avoid on a job (a rerun's previous host); how a root of trees orders with a whole-host start.
- **A low-water stop at run time**: the scratch volume is shared with tierstore's cache and nothing bounds a job's
  writes (no quota, O4); `--scratch-low-water-gib` is checked at admission only. When free space falls below it, stop
  the job that is growing and report a reason of its own, so one runaway action does not become `EIO` in every other
  job on the node. Not a quota.

**tierstore**:

- **`prefetch`** (4.5): `POST /_tierstore/v1/prefetch` with `{"blobs": [{"sha256", "size", "source"}]}`, on the storage
  tier and on a build node's instance, authorized by the caller's key, answered `202` at once, never an error to a job;
  the fill socket's ranged `fill`, started by a call, at a priority below the reads jobs wait for.
- **An overlay of three lowers from the one FUSE mount**, measured as E4 measured one (rootless `crun`, copy-up,
  whiteouts, the stack depth on 6.8 and 7.0).
- **Refusal at attach, not during the job**, when a tree's flattened manifests or its pins would pass the node's memory
  or `pin_limit`: many large distinct trees at once must fail the start (which kbf resubmits), never `EIO` in a running
  job.
- **Fill sources tied to their writer**: a manifest's `source` may name any key of the store, and a tree digest is the
  only credential for reading what it names. tierstore's fill should take sources only below prefixes tied to the
  manifest's writer, and jobscheduler should accept a tree from a client only within what that client may read, so one
  client of the job service cannot read another's blobs by naming a digest or writing a manifest.

kbf relies on the manifest format of ts 6.1 as merged, `cas/l/<sha256>` for manifests and whole objects, and ranged
`source`s into kbf's packs.

## 14. macOS later, and other open points

**macOS (later; the Macs are in device-management enrollment):**

- whether Mac actions become jobs, and through which runtime (the agent's `sandbox-exec` per-run user, or a VM);
- a root made of trees on macOS: tierstore's NFSv3 frontend serves read-only trees, and the client builds any writable
  view itself (ts 8, option a); a Seatbelt profile cannot replace a root, so the macOS shape differs (js TI 4);
- until then the Macs keep `kbf-daemon` with the native driver, and the frontend keeps the worker listener.

**Other open points:**

- **Collecting `cas/l/`.** Nothing deletes there today, manifests and converted image files included. A collector
  must not delete what an attached tree uses (ts 6.1); it belongs with storage retention.
- **`kbf-task-pack` refused unknown fields** until item 8, which is why that reader ships a release before the writer
  (D13).
- **The farm's virtual workers** bound kbf's in-flight work by configured shapes, not the pool's real capacity.
- **The base image's contents**: this design fixes what the base tree must and must not hold, not what else; making it
  barer is the base image's own change, measured by the conformance build.
