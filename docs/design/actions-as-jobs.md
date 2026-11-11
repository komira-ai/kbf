# Every build action is a jobscheduler job on Linux, its root file system one tierstore tree over the kbf base tree

Status: **DESIGN, for the owner's review; nothing here is built and no code changes with this document**, so no
existing document is stale yet; section 11 names the documents each build item updates. Linux only; macOS is section
14, one open question and no more. This revision follows the owner's decisions of 2026-11-10 (section 1, O7 to
O15), which replace the first revision's job shape: the job's whole root file system, the base image included, is
a tierstore tree that jobscheduler mounts as an ordinary host directory, the download of inputs is temporary,
and the tierstore calls specific to kbf are `hydrate` and `release`, beside `put-ca` for outputs. It is amended by
the owner's decision of 2026-11-11 (O16): one **kbf base tree** per kbf release (the base image and `kbf-task`), and
one tree per action that names the base tree's subtrees by digest, so each job's root is a single read-only layer.

Written against kbf `main` at `6ad2b5931`, jobscheduler `main` at `f4924e67` (kbf's pin, release 0.1.218, is
`0c104a1d`, with the same public job API), jobscheduler's `docs/design/tree-inputs.md` (cited "js TI *n*"; kbf no
longer uses its tree inputs, O15) and tierstore's `docs/design/virtual-build-fs.md` (cited "ts *n*").
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
run's scratch directory as the writable upper layer, and below it one read-only directory of the node's
`tierstore fs` mount, which jobscheduler mounts as it would any host directory: the **action tree**, a tierstore tree
read lazily. Its root names, by digest, the top-level directories of the **kbf base tree** (the farm's one bare-bones
base image and `/.kbf-tools/kbf-task`, one tree per kbf release, held in tierstore), and beside them the action's
input root at `/work` and its Action and Command under `/.kbf`. Nothing is pulled, unpacked or copied before the
action starts; only the files the action opens are fetched, the base's files once per node for every job, and kbf
asks the node's tierstore to **hydrate** the tree as soon as it sees the job placed, so caches warm while the
container is made.
Outputs go straight to the node's tierstore, reached through a host port passed into the job: `kbf-task` hands each
output file to it, content-addressed, under the job's own fenced prefix, and the bytes stay hot in that node's cache for the next action;
the frontend promotes them to CAS and action-cache entries only from a clean job result. Until the tree path works on all nodes, today's job (the OCI image and the download) is the
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
| O3 | Inside a job only `kbf-task`'s fetch and upload phases use the network (in a tree job, only its upload to the passed-through node port); the action command runs network-less (2026-10-30). | 3.6 |
| O4 | No per-job disk quota; the farm pool books no disk (2026-11-09). | 3.5 |
| O5 | tierstore's filesystem is FUSE on Linux, everything under the hood in tierstore (2026-11-09). | 3, 4 |
| O6 | Flip fast, fix forward, one-line rollback, no shadow or gradual rollout for the flip itself (2026-10-28). | 8 |
| O7 | **One base image for the whole farm**, as bare-bones as possible, for hermetic builds; no per-action images (2026-11-10). | 3.3 |
| O8 | **The base image lives in tierstore as a tree.** The job's root file system is an overlay whose lower layers are the base-image tree and the action's tree (amended by O16: one action tree that names the base's subtrees, the only lower, 3.1), read lazily through `tierstore fs`, with the scratch directory as the upper; no OCI pull or unpack per job, no copy of inputs (2026-11-10). | 3.1 to 3.4 |
| O9 | **One build action per job** (2026-11-10). | 3.4 |
| O10 | **The cost per action is measured in milliseconds**; a no-op-action benchmark (job against today's daemon) is a flip gate, with a stated target (2026-11-10). | 10.1, 8 |
| O11 | **Copy-to-scratch is temporary**, only until lazy fetch and the overlay work; its removal is designed (2026-11-10). | 4.6, 7.2 |
| O12 | **Prefetch, now `hydrate` and `release`**: kbf tells tierstore which objects to warm, so caches fill while the container is made; not a jobscheduler preparing-stage fetch (2026-11-10; made `hydrate` by O15). | 4.5, 13 |
| O13 | **kbf publishes the farm's base image into the object store itself**, as a tierstore tree (the image's layers flattened into one tree), written once per base-image version, for example by kbf's release; the job names it by digest (2026-11-10). Amended by O16: the tree is the kbf base tree, one per kbf release, whose subtrees from the image stay the same digests while the image does; the job names the action tree, whose root names the base's subtrees. | 3.3 |
| O15 | **jobscheduler never treats tierstore specially.** It builds a job's root file system as a container engine does: ordered lower layers from host paths (on the farm, directories under the `tierstore fs` mount) or an OCI image from a registry on 127.0.0.1, a scratch upper, bind mounts, and host ports passed through into the job (jobscheduler's generic layered root file system design). The only tierstore piece specific to kbf's inputs is **hydrate** (with its `release`): kbf sees the job being created and asks the node's tierstore to warm the base and input trees (one action tree naming both, since O16). Outputs go to the node's tierstore through a passed-through host port. kbf drops `JobSpec.trees`, `JS_TREES_UNATTACHED` and tree attach (2026-11-10). | 3, 4.5, 5.2 |
| O14 | **Outputs are written straight into tierstore if possible**; otherwise `kbf-task` hands them to the node's tierstore (local-cache mode, write-through, 2026-10-30) through a node-local endpoint, content-addressed, into the job's fenced prefix, and kbf-task's own packs and presigned PUTs are removed once that works; promotion stays server-side, after a clean result (2026-11-10). | 5 |
| O16 | **One kbf base tree rather than a base tree and a tools tree** (2026-11-11): the base image flattened together with `/.kbf-tools/kbf-task`, one tree per kbf release, built and published by kbf's release; each action's job names one tree, whose root references the base tree's top-level manifests by digest beside `/work` and `/.kbf`, so the job's root is one read-only layer and a scratch upper. | 3.1, 3.3, 3.4, 4.1, 4.5 |

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

**jobscheduler**: the public `JobSpec` has an `image` and no input field [code]. Its generic layered root file system
(O15) is being designed in jobscheduler: ordered lower layers from host paths or a local registry's image, a scratch
upper, bind mounts, host ports passed through. Section 13 lists exactly what kbf needs from it. js TI's tree inputs
(`JobSpec.trees`, `JS_TREES_UNATTACHED`, the agent's attach and detach, `input_failure`; their wire is
komira-ai/jobscheduler#842) are not used by kbf.

**tierstore**: the manifest reader and flattener, the fill socket, pins, `tierstore fs` on Linux (the 6.8 mode and
passthrough), crash recovery and the live hand-off are merged [code: `crates/tierstore/src/tree_manifest.rs`,
`src/server/treefs/`, `src/server/fill_socket/`]. A tree is named by the SHA-256 of its root **tree manifest**, a JSON
listing per directory whose files name their blob and, optionally, where its bytes are (`source: {key, offset}`);
writing manifests is the client's job (ts 6.1, 6.7). A tree is reachable at `<mount>/trees/<digest>` only while a run
has attached it on the node's control socket, and `attach ... prefetch all` warms it then
(`crates/tierstore/docs/operations.md`); nothing warms a tree before that, and nothing on the network attaches one.

## 3. The job

### 3.1 The root file system, layer by layer

[proposed, D2; O16] The job's root file system is one overlay, made by the agent as it makes any layered root (O15),
from one host directory under the node's `tierstore fs` mount, `<mount>/trees/<digest>`, and the run's scratch:

| Layer | What it is | Paths it supplies |
|---|---|---|
| upper (writable) | the run's scratch directory on the node's scratch volume, as the `/work` upper is today | every write of the run: outputs, copy-ups, whiteouts, the Command's temporary files |
| lower (read-only) | the **action tree**, one per action (4.1) | everything the action reads: `/work` (the input root), `/.kbf/action` and `/.kbf/command` (the Action and Command blobs), and, named by digest from the kbf base tree (3.3), `/.kbf-tools/kbf-task`, `/etc`, `/usr` and the mount points |

**The action tree's root** is a manifest of its own per action (4.1): every entry of the kbf base tree's root manifest
as it is (a top-level directory by its manifest's digest, a top-level symlink by its target, a top-level file by its
blob), and beside them two entries the base tree never holds, `work` (the input root's manifest) and `.kbf` (the
Action and Command). There is no layer order to keep and nothing for one layer to shadow: names meet only in that one
root manifest, which the frontend writes, and the base tree is refused when it is built if it holds `work`, `.kbf`
or `.kbf-run` (3.3).

**Shared by digest, cached once.** The root manifest and `.kbf` are the only parts new per action (a few KB: one
entry per top-level name of the base, and two more), written once per action. Below it every directory is named by its manifest's digest, so `/usr` in every action's tree on a node
is the same manifest and the same blobs as in every other; tierstore's caches are keyed by blob SHA-256, and a
manifest is a blob too (4.2), so each node fetches, caches and (with passthrough) shares the pages of the base's files
once for all jobs, as it does for a toolchain subtree two input roots share. A kbf release changes the base tree's
root and its `/.kbf-tools` only; `/usr`, `/etc` and the rest stay the same digests while the pinned base image does,
and stay warm in every cache across kbf releases. What is not shared today is tierstore's **flattened** form: it
flattens a tree whole when it first attaches it (ts 6.3, I1), and each action tree is a tree of its own, so each
action's flatten covers the base's entries again (E8: about 1.7 µs and 110 bytes of table per entry, a table held while
the job runs). The bundle's facts give the base's entry count; 10.1 counts that cost in the gate; section 13 asks
tierstore to flatten a shared subtree once per node; until it does, the base is kept small enough to meet the gate.

What the overlay stacks on: its one lower is a directory of the `tierstore fs` mount (`/run/tierstore/fs`, the same
path on every node, one `kbf.toml` key), so the stack is the overlay over FUSE with one lower that tierstore measured
(ts 6.6, Measured: linux E4); section 13 asks for that measurement on the farm's kernels. jobscheduler sees one host
path. For it to exist when the agent makes the overlay, a tree must resolve on lookup by its digest, whether or not
anyone attached it (section 13 asks tierstore for it); `hydrate` (4.5) then only warms and records, and a late or lost
`hydrate` costs speed, never a start.

Mount points the runtime binds over (`/proc`, `/dev`, `/sys`, `/run/js-files`) must exist as directories: the kbf
base tree holds them (3.3), so every action tree does. A tree job has no `/scratch` bind: its whole root is writable through the upper. No tmpfs is mounted on `/tmp`, `/var/tmp` or `/run`: they are directories of the base
tree, written in the upper on the scratch volume, so they are disk-backed and executable, as under today's daemon.
Every entry of a tree is owned by `fs.owner`, the host uid the action user maps to (set once per node, 8.3), so the action
user owns the base tree's `/tmp`, `/var/tmp` and `/run` and its writes there copy up into the upper; a tree's modes are
0644 and 0755 only (ts D10), so those directories are 0755, not 1777, which no single-user job needs.

### 3.2 The action's view

`/work` is the exec root and the Command's working directory is below it, as REAPI says. The Command runs in a
sandbox `kbf-task` makes (4.4): its own user, network, mount and PID namespaces. The job's grants (`/run/js-files`) and
`kbf-task`'s run directory (`/.kbf-run`, its stdout and stderr) are out of its sight. It can read, but not change in
the tree, `/.kbf-tools/kbf-task` and its own Action and Command under `/.kbf`; none of them is a secret. It sees the
base image's files, its inputs, and nothing of the node: the user namespace maps it to the action user and nothing else.

### 3.3 The kbf base tree

**The kbf base tree** [proposed, D3; O7, O16] is one tierstore tree per kbf release and platform: the farm's one base
image, flattened, with one more top-level directory, `/.kbf-tools`, holding that release's `kbf-task` (a static
binary, as it is today) and nothing else. It is how `kbf-task` gets into the bare root file system: neither mounted
from the node nor a layer of its own. The image's contents are chosen in its own build, as bare as hermetic builds
allow (hermetic actions bring their toolchains as inputs); this design requires only that the base tree holds the
mount-point directories above, `/tmp`, `/var/tmp` and `/run` as directories, the action user in `/etc/passwd` and
`/etc/group`, and none of `/work`, `/.kbf` (the names each action tree adds, 4.1) and `/.kbf-run` (`kbf-task`'s run
directory, 4.4). The image starts as today's
pinned base (`base-<platform>.manifest.json`), flattened with modes normalised (below); making it barer is that
image's own change.

**Who builds it, and how (O13, O16)** [proposed, D3]. kbf's release builds it, and the release's frontend publishes it:

1. **Built by the release.** A `cargo xtask base-tree` step of kbf's release workflow reads the pinned base image
   (`packaging/container/kbf-task/base-<platform>.manifest.json`, each layer checked against its digest), applies the
   layers in order with their whiteouts into one tree, makes hard links separate entries, adds `/.kbf-tools/kbf-task`
   (the release's own build, mode 0755), and writes one **tree bundle** per platform: every file's bytes and every
   directory's manifest, named by SHA-256, the base tree's root digest, and its facts (below). The bundle is a release
   asset like the others, listed in `SHA256SUMS` and covered by the release's attestation. The root digest changes
   with every release (its `/.kbf-tools` does); the manifest of every other top-level directory (`/usr`, `/etc`, ...)
   is a function of the pinned base manifest alone, so it changes only when that pin does: one base-image version, one
   set of subtrees, shared by digest across kbf releases.
2. **Flattened to what a tree can hold.** A tree has one owner and modes 0644 and 0755 only (ts 6.1, D10). The step
   normalises what only narrows access (a file becomes 0755 if any execute bit is set, else 0644; a directory 0755; the
   owner is dropped, so `/etc/shadow` 0640 becomes 0644, which is why the base holds no secret), lists every normalised
   path in the bundle's facts with the tree's entry count and depth (which every action tree counts with its own,
   4.6), and refuses, by name, what changes behaviour or what tierstore would refuse: a set-uid or set-gid file, a
   device node, a FIFO or socket, an extended attribute, a name with tierstore's `.wh.` prefix left after the
   whiteouts are applied, a path of the four reserved ones (`/work`, `/.kbf`, `/.kbf-run`, `/.kbf-tools`) in the
   image, and a base that alone passes one of tierstore's limits (4.6). A refused base fails the release build, not a
   deployment.
3. **Published by the frontend.** "Published by the release" means by the release's own frontend, at its start: the
   frontend is installed with its release's bundles, one per platform. Under `--executor js-agent` (so that 8.3's
   precondition is met before the trees switch; never under `kbf-daemon`), at start it checks the bundle of the
   executor's configured `arch` against the release's `SHA256SUMS`, and writes every object the store does not hold
   yet to `cas/l/<sha256>` with a create-only PUT through the client it already has (an existing key is success: the
   key is content-bound); the base image's objects already published by an earlier release cost one `HEAD` each, done
   once, and a release whose pin did not move adds only its `kbf-task`, its `/.kbf-tools` manifest and its root
   manifest. Only then does it serve, with the base tree's root manifest in hand; a publish that fails fails the start
   by name, as does a configured `arch` the release has no bundle for. No workflow holds a store credential, and no
   separate step writes the store: installing the release is the deployment (S3), and the frontend
   already holds the store write right.
4. **Named by digest.** The executor builds every action tree's root from the base tree's root manifest (4.1), so each
   job names the base's top-level directories by digest. They are not configuration: they come with the release, so a
   `kbf.toml` cannot point a farm at a base tree the release did not build.
5. **Old versions.** Each published base tree gets a row in the frontend's state store, `job-tree/<digest>` (release,
   first published, last named in a job), a new record type that an older release skips (B8). Nothing is deleted
   now: `cas/l/` has no collector (section 14), and a release rolled back to republishes its own base tree anyway, so
   a deleted one would come back. When `cas/l/` gets a collector, a base tree's objects are collectable when it is
   neither the current nor the previous release's, has not been named in a job for longer than the longest wall limit
   plus the job service's retention, and no other live tree names them (a newer base tree names the same subtrees
   while the pin has not moved). The per-action roots and `.kbf` manifests are `cas/l/` manifests like the input
   roots', collected with them (section 14).

### 3.4 The mapping: one action, one job

**One action per job** (O9). A farm lease is one job, as today. The job of an action whose tree was built:

| `JobSpec` field | Value | Today's job (the temporary fallback, 4.6) |
|---|---|---|
| `job_id` | `k<8 hex>-<term>-<seq>-<attempt>`, attempt 0 (1 for the resubmit of section 6) | the same |
| root file system | one lower layer, the host path `<mount>/trees/<digest>` of the action tree (3.1), and a scratch upper (jobscheduler's generic layered root, O15) | `image`: the kbf OCI image |
| `command` | `/.kbf-tools/kbf-task`, the whole program; there is no image entrypoint | empty: the image's entrypoint |
| `args` | the task's flags, with `--tree` | the task's flags |
| `env` | empty | the same |
| `needs` | `cpu_millis`, `memory_bytes` (the hard cap a daemon would set); no `disk_bytes` (D4) | the same |
| `constraints` | `os=linux`, the configured `arch`, contained | the same |
| `wall_limit_seconds` | the action's timeout plus the run margin | the same |
| `network` | `none` (3.6) | the operator's level, default allow-list |
| host ports passed through | one: the node's tierstore loopback port, at the same port on the job's loopback (5.2) | none |
| `files` | the output write grant (5.2; no read grant: nothing is read over the network) | the read grant and the PUT URLs |
| `labels` | `kbf-frontend`, `kbf-instance` | the same |

**The tree is fixed in the prepared spec.** It is built once, in the executor's `prepare`, from the base tree of the
running release, and kept in the lease's `Prepared` spec. A submit sent again after a lost answer sends that spec
unchanged and never consults the memo of 4.2: the layer is part of "the same job", so a rebuilt manifest under the same id would be `job-id-conflict`. Every rerun of section 6 is a new farm lease, so it has a new job id and fresh manifests.

A `whole_machine` lease gets today's job while it exists (D12): how a layered root combines with a whole-host
start is a question for jobscheduler (section 13).

### 3.5 Resources

`cpu_millis` and `memory_bytes` are the farm's booking, as today. `disk_bytes` is dropped (D4): O4, and while kbf books
it js-sched places a kbf job only on a host reporting `disk-gib` (kbf#2287 item 3). A tree's bytes are in tierstore's
cache under its own budget; only the run's writes land on the scratch volume.

### 3.6 Network

O3 holds, more strictly than today (D18). A tree job's network level is `none`: it reaches nothing but the one host port
passed through to it, the node's tierstore (5.2). The Action, the Command and the inputs are files of the root file
system, so there is no fetch phase and no read grant; the upload phase is `kbf-task` handing outputs to that port. The
Command runs in its own network namespace with only its own `lo` (`crates/kbf-task/src/network.rs`) [code], so even
the passed-through port is out of its reach. `tierstore fs` fetches blobs over the node's fill socket, outside the job.

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

**The action tree** is one more manifest on top, its root (3.1): every child of the running release's base tree's root
manifest (3.3), copied as it is, so `usr`, `etc` and `.kbf-tools` are directory children naming the base's own
manifests by digest and size, and beside them `work` (the input root's manifest) and `.kbf` (a manifest of two files,
`action` and `command`, mode 0644, whose sha256 are the Action's and the Command's digests and which are made
locatable like any input blob, D6), all sorted bytewise by name. The
base's subtrees are not read, copied or rewritten: the root names them, and they are already in the store (published
at start, 3.3). Its digest is the one tree named in the job.

Each new manifest is written with a create-only PUT to `cas/l/<sha256>` through the frontend's tierstore client (the
one the large-object backfill uses, `crates/kbf-front/src/large_backfill.rs`). An existing key is success: the key is
content-bound. A failure is "no tree" for that job (4.6).

### 4.2 Naming, lifetime, cache keys

- **Names.** A tree is named by the digest of its root manifest, not by the REAPI input-root digest: two manifests of
  one input root differ when a blob moved (a pack compacted), which is a new tree and costs a miss (ts 6.1).
- **The memo** maps a `Directory` digest to its manifest's digest, so a shared subtree (a toolchain) is built and PUT
  once; a hit skips both. It is bounded by entries (oldest out first), and each entry expires after a fixed time (10
  minutes, a constant, B20; on the frontend's injected clock) and no later than the earliest-expiring of its children.
  A failure `release` reports (section 6) drops every entry of that action's input root, so the rerun's manifests come from the
  index as it is then.
- **tierstore's caches** are keyed by blob SHA-256 (`_tiering/fill/<sha256>` on a build node), so a file read by two
  actions, two trees or two jobs on a node is fetched once per node; manifests are blobs too, so the base's
  manifests and files named by every action tree are fetched once per node, as is a toolchain subtree two input
  roots share. A tree's blobs are pinned while a `hydrate` hold names it (4.5, ts 6.5); the frontend keeps a standing
  hold on the kbf base tree on every node, so the base's read blobs stay pinned whichever action tree reads them.
- **Holds, not attachments.** kbf never talks to tierstore's control socket, and jobscheduler never talks to tierstore
  (O15). A tree resolves on lookup (3.1); what kbf adds is a `hydrate` hold per job, released after the job ends, which
  warms the tree and records what failed while it held it (4.5, section 6).
- **The manifests** stay in the store. tierstore never deletes a manifest or a blob, and the client must not delete one
  a tree in use names (ts 6.1). kbf deletes nothing under `cas/l/` today (the `cas/b/` janitor never lists it); its
  collection is an open point (section 14).
- **A job cannot change a tree**: the manifests are the frontend's, written from `Directory` messages it read and
  checked, under keys that are their own SHA-256; the job's write grant covers only its own output prefix (5.2).

### 4.3 What `kbf-task` does in a tree job

With `--tree`, `kbf-task`:

1. reads `/.kbf/action` and `/.kbf/command`, hashes the Action and checks it is the digest of its `--action` flag (so a
   job never runs another action's tree), and decodes both; there is no read grant and no fetch;
2. before the Command starts, makes the parent directory of every output path and the working directory under `/work`
   (in the upper layer), as REAPI asks of the worker;
3. runs the Command in the sandbox of 4.4, under the action's timeout;
4. collects the outputs from `/work` and hands them to the node's tierstore (5.2), and records `"from": "tree"` in the manifest's
   advisory `inputs` statistics (`kbf-task-pack` `InputStats`; the reader that knows `from` ships one release before the
   writer, because `InputStats` refuses an unknown member, D13).

### 4.4 The Command's sandbox

[proposed, D7] In every job, tree or not, `kbf-task` starts the Command in its own mount and PID namespaces, beside
the user and network namespaces it already makes (`network.rs`, `CLONE_NEWUSER | CLONE_NEWNET`) [code]:

- `/run/js-files` (the job's grants) is covered by an empty read-only tmpfs, and so is `kbf-task`'s own run directory
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

### 4.5 Hydrate: warming the node while the container is made

[proposed, D8; O12, O15] The executor polls each job (`GetJob`, as it does today). The first answer that names the
job's `host_id` (state `PLACED` or `STARTING`: the agent is about to make, or is making, the container) makes it send
that node's tierstore one **`hydrate`** call, and not wait for it:

- **What it names**: the job id as the hold's name; the one tree, the action tree's root digest; and a bounded
  list of blobs to fill first, each with its digest, size and `source`. Not the whole input tree: an input root that
  holds a toolchain names gigabytes the action mostly never opens, and filling it all would undo the lazy read. kbf
  names the Action and the Command, and the files of the input root's `Directory` manifests that the memo of 4.2 did
  not have (the action's sources and generated inputs, new to the farm), largest last, up to 256 MiB per action (a constant, B20). Shared
  subtrees are left to lazy reads and the node's cache, where earlier actions put what they read.
- **What tierstore does**: reads and flattens the tree's manifests (so the first lookups in the container are
  answered from memory; the base's manifest blobs are already in the node's cache from earlier jobs, though the
  flatten covers its entries again, 3.1), fills the listed blobs into the node's cache below the priority of any read a job waits for,
  pins what it fills for the hold, and from then records, for the hold, every fill that failed on its tree and the
  time reads of it waited (`stall_ms`), as an attachment records them today (ts 7.4).
- **The end**: after `GetResult`, before any promotion, the executor sends **`release`** with the job id; tierstore ends
  the hold and answers with what it recorded. A hold kbf never releases (a frontend that died) ends by itself after
  the job's wall limit plus `--task-url-slack-secs`, which the hold carries.

The calls kbf needs (section 13), on the node's tierstore at the node's address (a `kbf.toml` table of the six nodes'
endpoints, checked at start), authorized by the frontend's key:

- `POST /_tierstore/v1/hydrate` with `{"hold": "<job id>", "expires_ms": <n>, "tree": "<sha256>", "blobs":
  [{"sha256", "size", "source"}]}`, answered `202` at once;
- `POST /_tierstore/v1/release` with `{"hold": "<job id>"}`, answered `{"failures": [{"path", "sha256", "reason"}],
  "stall_ms": <n>}`; `not held` for a hold already ended (idempotent).

A `hydrate` that fails, is refused or is late costs only speed: the tree resolves on lookup anyway (3.1). What is lost
then is the record of failures before the hold began, which section 6 accounts for. The kbf base tree gets a standing
hold per node: the same call, its hold named for the base tree's digest, its tree the base tree, no blobs, and an
expiry of a fixed hour (a constant, B20). A hold pins the node's cached blobs of the tree it names, whoever filled
them (ts 6.5), so this one keeps the base's files that any action read in cache for every action tree, within
tierstore's `pin_limit`. The frontend renews it by sending the same call again before it expires (the same hold name
extends it) while its release is the running one; a replaced release's hold lapses at its expiry. Nothing reads what
it records: a fill that fails on the base during a job is recorded by that job's own hold too, since its tree names
the base's subtrees.

### 4.6 Today's job, the temporary fallback

Until its removal (7.2), the executor submits today's job (the kbf OCI image, inputs downloaded) instead of a tree
job when [proposed, D9]:

| Reason (`why`) | When |
|---|---|
| `off` | `executor-trees` is off (8.3) |
| `whole_host` | a `whole_machine` lease (D12) |
| `store` | the index or tierstore did not answer within the writer's deadline, or a PUT failed |
| `unrepresentable` | a name tierstore refuses (`.wh.` prefix), `node_properties` (4.1), or a tierstore limit passed (2^24 - 1 entries, a manifest over 64 MiB, depth over 4096), counted over the whole action tree: entries are the base's (from the bundle's facts, 3.3), the input root's and four (`work`, `.kbf` and its two files); depth is the larger of the base's and one more than the input root's |
| `refused` | the resubmit, once, of a tree job whose submit or start was refused (section 6) |

Each is counted (`kbf_executor_inputs_total{from="fetch",why}` beside `{from="tree"}`). These are states that change
while the farm runs, not configuration mistakes (B20); the share of each is a number of section 10, and the removal of
7.2 waits on it.

What is **not** a fallback: a tree job cannot start without its tree, since it is its root file system. A node
whose `tierstore fs` is down has no `<mount>/trees/`, the agent cannot make the root, and the start fails; kbf
resubmits it as today's job (section 6). kbf uses none of js TI's tree inputs (`JobSpec.trees`,
`JS_TREES_UNATTACHED`, `job_stages_if_unavailable`), per O15.

## 5. Outputs

### 5.1 Straight into tierstore: evaluated, not now

The cleanest shape (O14, preferred) would have the action write its outputs into tierstore directly. It does not work
yet, for reasons of tierstore's and of REAPI's [proposed, D16]:

- **Not as the overlay's upper.** overlayfs cannot use a FUSE mount as its upper directory, so the upper stays the run's
  local scratch.
- **Not as writable tierstore directories bound at the output paths inside `/work`.** `tierstore fs` is read-only by
  decision (ts C4 = A; the writable views were dropped, ts 12 item 11), so there is nothing writable to bind. And if
  there were, it would break what REAPI lets an action do:
  - an output file is a file, and a bind's target is fixed: tools that write a temporary file and `rename(2)` it onto
    the output (compilers and linkers do) get `EXDEV` across the mount, or `EBUSY` on the mount point itself;
  - an action may `rm -rf` an output directory and make it again (`EBUSY` on a mount point), create outputs whose
    parents do not exist yet, or write an output path that is also an input it reads first;
  - an output written and then deleted, or written by an action that then fails, would already be stored: the store
    would hold what the action never committed to.

So (a) waits for a writable tierstore that keeps those semantics; nothing in this design depends on it.

### 5.2 The first cut: the task hands each output to the node's tierstore

[proposed, D17] After the Command exits and its namespaces are gone (4.4), `kbf-task` walks the outputs (`kbf-outputs`,
`openat2` beneath `/work`) and hands each file, each output `Tree` and `Directory`, and stdout and stderr to the node's
tierstore, one stream per blob:

- **The call** (section 13): `POST /_tierstore/v1/put-ca`, sent to `127.0.0.1:<port>` inside the job, which
  jobscheduler passes through to the node's tierstore on the host's loopback (O15), with the bytes as the body and the job's **write grant** as
  a bearer token. tierstore hashes the stream as it stores it, keeps it at `<prefix><sha256>` (create-only; an existing
  object of that digest is success), writes it through to its upstream before it answers (local-cache mode,
  write-through, O14), keeps the bytes in the node's cache under their digest, and answers `{"sha256", "size", "key"}`.
  The task never computes a digest before it sends, so each output is read once (B16).
- **The write grant** comes from tierstore: the frontend mints it per lease with the code of the tierstore release
  that has `put-ca` and write grants (item 14 waits on that release; section 13). kbf mints no tierstore grant
  today: tierstore v0.3.29 removed the read grant and its `mint_grant`, and kbf dropped its store read grants
  with the pin of item 9 [code]. It is a token scoped to the job's prefix `cas/j/<build>/<nonce>/`
  (MAC'd with `--task-secret-file`, as the `cas/b/` keys are), allowing create-only writes of content-addressed objects
  below it and of one named object, `commit`; no read, no list, no delete; expiring with the wall limit plus
  `--task-url-slack-secs`. It travels as a job file (`/run/js-files`), out of the Command's sight. A job can write only
  below its own prefix (O2, B16), and only from its own node, since the port is the node's.
- **The commit.** Last, the task writes its manifest (the `TaskManifest` of `kbf-task-pack`, in a new version that
  names every output blob by digest and no pack) through the same call, and then `commit`, whose content is the
  manifest's digest. A job that died before `commit` left objects that nothing names.
- **Which node.** Always the job's own: the passed-through port is its node's tierstore, so the task needs no table
  of nodes and no host name, and the bytes stay in the cache of the node the next action on it reads from.

**The promotion, from the result only** [proposed, D10]. The frontend promotes a job's outputs only when `GetResult`
says state `SUCCEEDED`, reason `EXIT`, exit code 0, **and** the job's `release` (4.5) answers no failure; never from a
write. Any other result leaves
the prefix to the janitor, however complete it looks: an action that swallowed an `EIO` from a failed tree read and
exited 0 has already written its outputs before `release` reports the failure. Today's executor already
ingests only after the job's end (`executor/runs.rs`, `Verdict::Ingest`) [code]; this makes the four conditions the
rule. The ingest then reads `commit` and the manifest at the lease's own prefix (the MAC proves it is the lease's),
checks the manifest names this lease and action, checks that every object it lists exists below the prefix with the
digest's size, and records each as a whole-object location in the pack index: the bytes stay where tierstore put them,
and are not copied. tierstore hashed each one as it stored it, and kbf checks the SHA-256 of every range it reads
(`docs/design/storage.md`, reads) [code], so a wrong byte is never served. Only then does it write the action cache
(exit 0 and not `do_not_cache`).

**The node stays hot.** A later action on the same node that reads one of these outputs (the next step of a build
reads the last step's output) names it in its tree with `source` its `cas/j/` key, and `tierstore fs` finds the
bytes already in the node's cache by digest (section 13 asks that a `put-ca` and a fill of the same digest share the
cache entry).

**Cleanup.** A prefix with no promoted object is deleted by the janitor after `--task-janitor-grace-secs` (at least
6 h), which learns `cas/j/` beside `cas/b/`; it is off by default and a precondition of both switches (8.1 for `cas/b/`, 8.3 for `cas/j/`). Promoted
objects are CAS locations like any other. The run's upper layer is removed with the run directory by the agent, as
root, whiteouts and `work/work` included (ts 6.6).

### 5.3 Today's job, and the end of the packs

Today's job (4.6) keeps today's output path: the fenced `cas/b/<build>/<nonce>-<k>` pack keys, one presigned
create-only PUT per key, and the ingest of `ingest_set` (`crates/kbf-front/src/prepare_run.rs`,
`proxy/packed/commit/untrusted/task.rs`) [code], with the same promotion rule. Once 5.2 works (the trees switch on, its
writes counted in section 10 with no ingest refusals it did not explain), the packs go with today's job (7.2): the
task's pack writer and PUTs, the presign key, and the `cas/b/` ingest.

## 6. Failures and retries

Every row ends in one of the farm's existing classes (`executor/outcome.rs`, then `answer.rs`), so the client's
answers do not change. "Rerun" is a new farm lease and charges `FARM_RERUNS` (2); after it the client gets `INTERNAL`.

| What happens | The job service says | The executor does (new rows marked) | The client gets |
|---|---|---|---|
| Inputs missing from the CAS at Execute | (no job) | (no job) | `FAILED_PRECONDITION` with `MISSING` violations, as today |
| A tree job's submit is refused (`no-capable-host`, or the layered root's own refusal) | `SubmitJob` refusal | **new**: while the fallback exists, submit the lease again as attempt 1, today's job (`why=refused`), once; after 7.2, infra | the action's result |
| A tree job's start fails (`AGENT_REFUSED` or a start timeout: the tree's path is missing because `tierstore fs` is down, the overlay cannot be made, or a refusal the empty start detail cannot name, kbf#2287 item 1) | `FAILED_TO_START` | **new**: the same resubmit, once, while the fallback exists; after 7.2, infra, rerun | the action's result, or `INTERNAL` |
| A fill failed while the job ran (`EIO`: a moved pack, the store down past tierstore's bound) | whatever it says (the job may even have succeeded) | **new**: `release` names the failure: infra, rerun; the root's memo entries dropped; the outputs not promoted (5.2) | the rerun's result, or `INTERNAL` |
| The action timed out (in `kbf-task`) or hit its wall limit, and `release` reports `stall_ms` over a tenth of the action's timeout | `SUCCEEDED` with a timeout in the manifest, or `WALL_LIMIT` | **new**: infra, rerun, not a timeout | as above |
| `release` is not answered (the node's tierstore down) | any | **new**: retried until the hold's expiry; then, if the job did not succeed, infra, rerun; if it did, the outputs are not promoted and the action reruns | as above |
| The submit is answered with a spec that lacks the layer or the passed-through port (a server older than the layered-root release) | `SubmitJob` answer | **new**: cancel, infra, an error naming the server | as above; fixed by upgrading the server |
| The action's own timeout (fired inside `kbf-task`: exit 0, a timeout in the manifest) | `SUCCEEDED`, exit 0 | promote, as today | `DEADLINE_EXCEEDED` with the partial result |
| The job's wall limit (the task itself hung) | `FAILED`, `WALL_LIMIT` | timeout, as today | `DEADLINE_EXCEEDED` |
| The job's memory limit | `FAILED`, `MEMORY` | the memory ladder, as today | the result after the ladder, or `FAILED_PRECONDITION` with `OUT_OF_MEMORY` |
| The agent or host is lost | `LOST` | lost, rerun, as today | the rerun's result, or `INTERNAL` |
| Cancelled by someone else; `kbf-task` exits 10 to 15; the outputs refused at ingest (no `commit`, a manifest for another lease, an object missing or of the wrong size) | as today | infra, rerun, as today | as above |
| tierstore refuses a write (the grant expired, a key outside the prefix, the node out of space, upstream down past its bound) | `kbf-task` exits 13 | infra, rerun | as above |
| The action writes more than the scratch volume holds | the action's own `ENOSPC` | the action's result | its nonzero exit, as with a laid-out tree |

Notes:

- **A failed fill is a rerun even after exit 0**, and its outputs are never promoted (5.2): an action can tolerate a
  missing optional file and exit 0 with a wrong output (ts 7.4). jobscheduler does not see these failures (it does not
  know tierstore, O15); `release` is where kbf learns them. A failure before the hold began (a `hydrate` later than
  the first reads) is not attributed; section 10 counts the fills tierstore records outside any hold, so that gap is
  measured, not assumed small (section 13 asks tierstore to count them). Failures on the base's subtrees, which every action tree names, are attributed to
  every job whose hold saw them, which reruns an innocent job at worst.
- **The resubmit is within the lease** (attempt 1; the job id's attempt field exists for this), at most once, and
  charges no farm budget: it is today's job, whose own failures are classed as today.
- **Fill time is the action's time.** A file's bytes are fetched on its first open, inside the action's run. The
  `hydrate` of 4.5 is what keeps that off the action's clock; how much it does is measured (10.3).
- **A stall is not the action's timeout**: with `stall_ms` from `release`, a timeout during which reads of the tree
  waited for over a tenth of the action's timeout is a farm fault and rerun (the rule of ts 7.4), not
  `DEADLINE_EXCEEDED`.
- **A rerun may land on the same host**: the farm avoids a virtual worker, not a host (`farm.rs`, `avoided`), and the
  job spec cannot name a host to avoid; section 13 asks for one, and until then the operator restarts or stops a bad
  node's agent. Section 10 counts the failures `release` reports by host.
- **Retries stay kbf's**; the job service retries nothing.

## 7. What is deleted, and when

### 7.1 The daemon path

After the executor switch has held for one release with no rollback, in two steps (section 11 lists them as items 20
and 21):

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
4. the tree job's start failures (section 6) are no more frequent than today's job's.

Then, one pull request each (section 11, item 22 and after):

| Deleted | Why it can go |
|---|---|
| `kbf-task`'s fetch, locate and batch code (`fetch.rs`, `locate.rs`, `batch.rs` and their tests), its fetch flags and the read-grant files | nothing is fetched by a job |
| the frontend's job-facing read listener (`--job-listen`, `job_read*`), its read grants, and the large-object backfill tee that rides on it | nothing reads through it |
| the kbf OCI image (`pkg_image_asset`'s task image, `packaging/container/kbf-task/Dockerfile`; the pinned `base-<platform>.manifest.json` stays, the input of `cargo xtask base-tree`) and `--executor-kbf-image`, `--executor-base-image`, `--executor-locate`, `executor-trees` | the kbf base tree replaces the image; trees are the only job |
| `kbf-task`'s pack writer and presigned PUTs (`pack.rs`, `plan.rs`, `put.rs`, `put_urls.rs`), the frontend's presign key and `cas/b/` key minting, and the `cas/b/` ingest (`untrusted/task.rs`) | outputs go through 5.2 |
| the resubmit as today's job (D9) and the `why` reasons | there is nothing to fall back to |

After the removal an action whose input root cannot be a tree is refused by name: `INVALID_ARGUMENT`, naming the
`node_properties` or the tierstore limit it hit (it was never going to run differently). A tree job whose start fails
is a farm fault and rerun, like a lost host today. A job's network is `none` plus the passed-through node tierstore port.

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
(`executor_flags.rs`, `job-service-cutover.md`). `back-out` restores the whole file kept before the switch, so every later edit (the trees switch,
the node endpoint table) is undone with it; with `executor = "kbf-daemon"` those keys are inert anyway. The
drain of the restart waits for the jobs in flight (B15).

### 8.2 The Linux daemons during the window

The daemons keep running beside `js-agent` until the nodes move (7.1), so the rollback is one line. While
`--executor js-agent` is set, the farm's placement skips every Linux daemon worker; Mac daemons are placed as before
[proposed, D11]. It is a filter computed at placement from the running setting, not a cordon (a stored row only the
operator lifts, `farm/cordon_owner.rs`), so the rollback's restart leaves nothing behind.

### 8.3 The second switch: trees

`executor-trees = true`. Preconditions, checked once:

1. jobscheduler's generic layered root, the host-port pass-through and the pushed start and pushed or long-polled
   result (section 13) released, the lease server and agents upgraded, kbf's pin moved (item 16); the gate below
   cannot be met without the last two.
2. On each node: `tierstore fs` running at the same mount path (`tierstore-fs@a`, its marker made, `fs.owner` the
   uid the agent's job user maps the action user to); the node's tierstore reading the bucket and prefix the frontend's
   pack store writes, listening on loopback for the passed-through port and on the node's address for the frontend;
   `--scratch-low-water-gib` at least the cache budget where the cache shares the scratch volume; tierstore's
   `hydrate`, `release`, `put-ca`, write grants and trees that resolve on lookup (section 13) released on every
   instance; the loopback port `put-ca` listens on is one `kbf.toml` key (`executor-node-store-port`), the same on every
   node, checked at the frontend's start; the `cas/b/` janitor has learned `cas/j/`.
3. The kbf base tree published by the frontend of the running release (3.3).
4. The overlay over one `tierstore fs` lower measured on the farm's 6.8 and 7.0 kernels, with the per-action
   flatten of the base's entries (3.1; shared by tierstore or within the gate's budget, section 13). A node where it
   fails gets start failures, which kbf resubmits as today's job (section 6), until it is fixed.
5. A one-node run: a real build action as a tree job, outputs promoted, the run directory gone, the agent's integrity
   check clean.
6. **The gate**: the no-op benchmark, arm C against arm A, meets the target of 10.1, and the cold first-byte numbers
   of 10.3 are with the owner.

On a 6.8 node there is no FUSE resend: a crash or restart of `tierstore fs` aborts the reads in flight (ts 7.2), and
`release` reports those failures and the jobs are rerun; until tierstore's update hook acts on `tierstore-fs@`, a node's
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

**Executor** (`crates/kbf-server/tests/executor/`, against `FakeJobServer` taught the layered root, the
passed-through port and the spec echo, and a fake of tierstore's `hydrate` and `release` in the test's process):

- `a_tree_job_names_one_tree_and_runs_kbf_task`: the spec field by field as in 3.4, one lower layer, the action
  tree's path;
- `todays_job_is_byte_for_byte_today_but_its_attempt`, once for each `why` of 4.6;
- `no_job_books_disk`, for a shared and a whole-host lease;
- `outputs_are_promoted_only_from_a_clean_result`: complete outputs already committed are not promoted for `FAILED`, for
  `WALL_LIMIT`, for a nonzero exit, for `SUCCEEDED` whose `release` names a failure, or for one whose `release` was
  never answered; the clean result
  promotes it;
- `a_failure_release_names_is_a_rerun_whatever_the_exit_code`, with exit 0 and nonzero;
- `a_timeout_with_stall_ms_over_a_tenth_is_a_rerun`: `stall_ms` at a tenth of the action's timeout is a timeout, one
  millisecond more is a rerun;
- `release_is_retried_until_the_hold_expires`; then a successful job is not promoted and reruns, an unsuccessful one
  is infra and reruns;
- `a_failure_release_names_drops_the_roots_manifests`;
- `a_refused_tree_submit_and_a_failed_tree_start_are_resubmitted_once_as_todays_job`, and the second refusal classed as
  today;
- `a_spec_answered_without_its_layer_or_port_is_cancelled_and_rerun`;
- `a_resubmit_after_a_lost_answer_sends_the_same_tree`, with the memo changed between the sends;
- `hydrate_is_sent_once_when_the_host_is_first_seen_and_never_waited_for`: a `hydrate` that fails, hangs or is
  refused changes nothing of the job; the blob list stops at 256 MiB and names only blobs the memo did not have;
- the outcome table's test with the new rows.

**Manifest writer** (`crates/kbf-front`):

- golden manifests (a file, an executable, an empty file, a large object, a symlink, a nested directory, the action
  tree's root), byte for byte;
- `the_action_tree_root_names_the_base_children_by_digest`: the root holds every child of the base tree's root
  manifest unchanged (a directory, a top-level symlink, a top-level file), beside `work` and `.kbf`, sorted bytewise;
  no manifest of the base is read or written in `prepare`;
- every manifest parsed and flattened by tierstore's own `tree_manifest::parse_manifest` and `flatten` (a test
  dependency at kbf's tierstore pin), so writer and reader cannot drift;
- children sorted across REAPI's three node lists; a shared subtree built and PUT once;
- the memo on an injected clock: the bound, expiry, a parent no later than its earliest child;
- an unlocated blob written to `cas/l/` through a SHA-256 check before the tree is named; a blob whose bytes do not
  hash is not written and the job is today's (`why=store`);
- 65,536 and 65,537 packed blobs, 256 and 257 large ones, all in one tree (chunks, not the listener's caps);
- each tierstore limit at its boundary and one past it, counted with a base tree's entries and depth (an input root
  that fits alone but not beside the base is refused), a `.wh.` name, and `node_properties` on a `Directory`, a file
  node and a directory node: `why=unrepresentable`;
- a create-only PUT that finds the key: success; one that fails: `why=store`.

**Base tree bundle and publishing** (`cargo xtask base-tree`, the frontend's start): a base image with each refused kind
(set-uid, set-gid, a device, a FIFO, a socket, an extended attribute, a reserved path) fails the build by name; modes
0600, 0640, 0700, 0750 and 1777 are normalised to 0644 or 0755 and listed in the facts; whiteouts across layers are
applied; a hard link becomes two entries; `/.kbf-tools` holds exactly the release's `kbf-task`, mode 0755; the
same pinned base gives the same manifest digest for every top-level directory but `/.kbf-tools` from two releases
with different `kbf-task` builds, and a different root digest; the facts' entry count and depth equal the tree's; the
frontend publishes only the objects the store lacks (a second release on the same pin writes only its `kbf-task`,
`/.kbf-tools` manifest and root manifest), treats an existing key as success, refuses to start on a bundle not in
`SHA256SUMS` or a publish that failed, and records `job-tree/<digest>`; a previous release's state reader skips that
record; a base image holding `/.kbf-run`, a `.wh.` name left after the whiteouts, or more entries than tierstore's
limit alone is refused by name; under `kbf-daemon` nothing is published, and an `arch` with no bundle fails the
start; a standing `hydrate` hold on the kbf base tree is sent to each node, sent again before its expiry while its
release runs, and not renewed by a later release (4.5).

**Outputs** (`crates/kbf-front`, `crates/kbf-task`, against a fake of tierstore's `put-ca` in the test's process):
the grant allows below its prefix and `commit` only, refuses a key outside it, a read, a delete and a write after its
expiry; the ingest promotes a complete commit and refuses, each by name, no `commit`, a manifest of another lease or
action, an object missing or of the wrong size; a job that died before `commit` leaves a prefix the janitor deletes
after its grace and not before; the task reads each output once (the fake counts the reads of a file it was handed);
the task sends to the passed-through port and to nothing else.

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

**Real service** (beta, root, `crates/kbf-integration-test`, the pinned `js-lease-server` and `js-agent`, one plain
directory in the test's scratch directory standing in for the action tree, and a fake tierstore on the passed-through
port): `a_tree_job_runs_over_its_one_layer_and_writes_through_the_passed_port`,
`a_tree_job_whose_layer_path_is_missing_is_resubmitted_as_todays_job`,
`a_tree_job_with_no_capable_host_is_resubmitted_as_todays_job`.

**Run directory removal** is the agent's (as root, whiteouts and `work/work` included) and tested in jobscheduler (its
removal case for an overlay run directory); kbf adds no test of it. **After the removal of 7.2**, item 22's pull requests carry the tests of
an input root refused by name (`node_properties`, each tierstore limit) and of a failed tree start rerun as a farm fault.

**Farm integration** (a runbook on the nodes): real `crun`, `tierstore fs` and an action tree over the kbf base tree; an action that reads a
file of its input root and one of the base tree, writes an output, changes and deletes an input: the output promoted,
the run directory gone, the integrity check clean. Then with `tierstore fs` stopped (resubmitted as today's job), and
with a pack of the tree deleted between `hydrate` and the first open (`release` names it, rerun, success).

**Conformance**: a pinned Buck2 build of the farm's usual targets, through `kbf-harness`'s B0 scenario against a
frontend in each arm of section 10, gated on B0's gates (zero failed builds and actions, `kbf_cache_lost_blobs == 0`);
each action's outputs compared with the daemon arm's action-cache entry for the same action digest, and actions that
differ only by an embedded exec-root path (`/kbf/root` under the daemon, `/work` in a job) listed by name, not failed.

**Why this list is complete for the failure and boundary cases.** It takes each new row of section 6 (each refusal at
submit and at start, each failure `release` can report with each exit, `stall_ms` on both sides of its bound, an
unanswered `release`, the layer missing from the answered spec, each result that must not promote and
the one that must), each `why` of 4.6, each lookup bound and tierstore limit on both sides, each place `node_properties`
can be set, each refused kind of the tree builder, each way out of the sandbox (a hidden path, a leftover process, a
symlink, a rename, a `..`), the action tree's root over the base (each kind of base child, the reserved names, the
limits counted with the base), `hydrate`'s failure, hang and refusal and its blob bound, the lost-answer resubmit, the one format
changes (D13, `job-tree/`, the manifest version of 5.2) and their release order, and the placement filter on, off and after a rollback. Unchanged rows keep their tests.

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

**The target** [proposed, D14], on idle nodes with warm caches (the kbf base tree and `./noop` already in each
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
| prepare: closure, keys, manifests (memo warm) | under 10 ms | a few small manifests per action, the root among them (the base's top-level entries copied, not read) |
| submit and the service's placement pass | under 10 ms | one RPC and one pass |
| the start reaching the agent | **one heartbeat interval today** (default 5 s; 250 ms in kbf's real-service test pool) | the agent learns of a start at its heartbeat; the target needs the start pushed to the agent (section 13) |
| lookup of the warm action tree by the overlay | under 10 ms | flattened manifests already in memory after `hydrate`; the base's manifests in the node's cache, though each action tree is flattened whole, base entries included, at E8's 1.7 µs per entry unless tierstore shares a flattened subtree by digest (section 13) |
| `crun` create and start, overlay, network | about 58 ms | measured: 57.8 ms median with the allow-list (jobscheduler `agent-egress.md`, `js-launch-measure`) |
| `kbf-task` start, sandbox, `./noop`, `put-ca` of stdout, stderr and the manifest, `commit` | under 30 ms | four small writes to the node's loopback |
| the result reaching kbf | **the executor's poll interval** plus the agent's next heartbeat | the target needs a long-poll or pushed result (section 13) |
| ingest | under 20 ms | reads `commit` and the manifest, checks three objects |

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
4. **The fallback share** by `why` (4.6), and failures: fills `release` reports per thousand jobs by host, fills
   tierstore records outside any hold (4.5), `EXDEV`,
   timeouts per arm. These decide the removal of 7.2.

### 10.3 Hydrate, measured

Per action: how many of the blobs it opened were already in the node's cache at first open, with `hydrate` sent and
with it off (one arm of C each); the time from the job's placement to `hydrate`'s arrival and to the container's
first read; and the time to the first byte of the largest toolchain files (each 100 MB or more) cold, on a 6.8 node
and a 7.0 node. If `hydrate` arrives after the first reads (a pushed start makes the container sooner), the numbers
say so, and the next step is kbf sending it from the job's `PLACED` state only, or tierstore filling on the overlay's
first lookup.

## 11. Build items, in order

Code items are one pull request each; items 7, 19, 20 and 22 are operations steps. Items 1 to 12 need nothing new from
jobscheduler or tierstore; items 13 to 15 need tierstore (13: `hydrate` and `release`; 14 and 15: `put-ca` and write
grants), and item 16 needs jobscheduler. Each pull request updates the
documents named with it.

1. **This design.**
2. **Executor: no disk booking**; `--executor-output-disk-bytes` removed, refused by name. Docs: `kbf-modes.md`.
3. **Farm: Linux daemons get no lease under `--executor js-agent`** (8.2). Docs: `kbf-modes.md`,
   `job-service-cutover.md`.
4. **kbf-task: the sandbox** (4.4) for today's job. Docs: `kbf-modes.md`.
5. **Executor: the promotion rule** (5.2) for today's packs, and its tests.
6. **kbf-bench: `noop`** (10.1). Docs: its README.
7. **The executor switch** (8.1), after the no-op report of arm B.
8. **kbf-task-pack: the manifest readers skip unknown members** (B8), and read `from` and the version that lists
   outputs by digest.
9. **tierstore pin** to a release with `tree_manifest` (a test dependency of items 10 and 11). Every such release
   lacks the read grant (removed in tierstore v0.3.29), so the frontend's store read grants and their three
   `--job-store-*` flags go with it; `GET /locate` then names no store, and a task reads its inputs from the
   listener.
10. **kbf-front: the manifest writer**: `collect_closure` keeping `Directory` messages, the lookups in chunks, the
    `cas/l/` writes of unlocated blobs, the manifests, the action tree's root over a given base tree's root manifest
    (4.1) with the limits counted with the base's facts, the memo, `node_properties`; no caller yet.
11. **xtask and kbf-front: the kbf base tree's bundle and its publishing** (3.3): `cargo xtask base-tree` in the
    release (the base image flattened, `/.kbf-tools/kbf-task` added, the refusals, the facts), the frontend's start
    publishing it under `--executor js-agent` and handing its root manifest and facts to item 10's writer as its
    input, the `job-tree/` record. Docs: `job-service-cutover.md`,
    `docs/artifacts.md`.
12. **kbf-task: `--tree`** (4.3) and writing `InputStats.from`, one release after item 8.
13. **Executor: `hydrate` and `release`** (4.5), one tree per call, the standing hold on the kbf base tree, the node endpoint table, and the rows of section 6
    that `release` decides, against a release of tierstore that has the calls.
14. **kbf-front: output write grants, the `cas/j/` ingest and the janitor's `cas/j/`** (5.2), against a release of
    tierstore that has `put-ca` and write grants.
15. **kbf-task: outputs through `put-ca`** (5.2), one release after items 8 and 14.
16. **jobscheduler pin** to the release with the generic layered root, host-port pass-through and the pushed start and
    result (the fake taught them, the spec echo, the field-set test).
17. **Executor: tree jobs**: 3.4, each job naming one tree, the action tree, as its one lower layer over a scratch
    upper, with item 13's `hydrate` sent for that tree; `executor-trees`, the resubmit as today's job, the counters,
    the rows of section 6.
    Docs: `kbf-modes.md`, `job-service-cutover.md`, `docs/api.md`.
18. **kbf-integration-test: the real-service tree cases** (beta).
19. **The trees switch** (8.3), after the one-node run and the gate of arm C.
20. **The nodes move to `kbf storage`** (7.1).
21. **Delete the Linux container driver**, the daemon's Linux-only wiring and the placement filter. Docs: `kbf-modes.md`,
    `job-service-cutover.md`, `docs/artifacts.md`, `docs/design/daemon.md`.
22. **The removal of the copy to scratch and the packs** (7.2), when its conditions are read from section 10; then one
    pull request per row of its table, the task's fetch first and the image last. Docs: the same.

## 12. Decisions

Each was not decided by the rulings or the two designs; the owner may overturn any. Two are changes to jobscheduler's
public interface and go to the owner's review of public interfaces before jobscheduler builds them: the generic
layered root and the host-port pass-through of section 13 (jobscheduler's own design, O15).

- **D1. The second executor becomes the only Linux path**: it is built, tested against the real service, and carries
  O2 and O3.
- **D2. The root is one overlay over one tree per action** (3.1, O16): the action tree is the only lower layer and
  the scratch the upper; its root names the kbf base tree's top-level directories by digest beside `/work` and
  `/.kbf`, so there is no layer order to keep, and the base is still shared by digest and cached once per node by
  blob.
- **D3. kbf's release flattens the pinned base image and its own `kbf-task` into one kbf base tree bundle, and the
  release's frontend publishes it at start** (O13, O16): no workflow holds a store credential, the base comes with the
  release rather than from configuration, and only its root and `/.kbf-tools` change with a kbf release; its other
  subtrees change only with the base pin, so they stay shared and warm across releases.
- **D4. kbf stops booking disk** (O4; kbf#2287 item 3).
- **D5. The frontend writes the manifests**, from the `Directory` messages its closure already reads and its own pack
  index, not through the read listener's capped `/locate`.
- **D6. Every blob is made locatable before submit** (written to `cas/l/` if it has no location), so where a blob
  lives never refuses a tree.
- **D7. The Command runs in its own mount and PID namespaces** in every job: the grants out of sight, no process left
  while outputs are read, the output walk resolved beneath `/work` by the kernel; in today's job also disk-backed
  `/tmp`, `/var/tmp` and `/run`.
- **D8. `hydrate` is sent to the node's tierstore when kbf first sees the job's host, and `release` after the job
  ends** (4.5): the node is not known at submit, the call is never waited for, and `release` is where kbf learns the
  failures jobscheduler cannot see.
- **D9. Today's job is the fallback while it exists**, chosen per action for the reasons of 4.6, and resubmitted once
  for a refused or failed tree start; js TI's tree inputs are not used (O15).
- **D10. Outputs are promoted only from a clean result**, never from a PUT.
- **D11. Linux daemons are skipped at placement while the executor runs**, not stopped or cordoned.
- **D12. `whole_machine` leases get today's job until a whole-host job can have a layered root** (section 13).
- **D13. The fallback is counted from the task's manifest** (`InputStats.from`), reader first; the reader also skips
  unknown members from then on, so a frontend rolled back one release reads a newer task's manifest.
- **D14. The gate's proposed target is 50 ms extra at p50 and 100 ms at p90 against the daemon, 250 ms and 500 ms end
  to end**, for a warm no-op action. Only `crun`'s 58 ms in it is measured; the rest are inferred from 10.1's parts, so
  the owner sets the final numbers on the first report.
- **D15. A failure `release` reports charges `FARM_RERUNS`** like any farm fault.
- **D16. Outputs straight into a writable tierstore directory are not designed now** (5.1): `tierstore fs` is
  read-only, and a bind at an output path breaks REAPI's renames and recreations.
- **D17. The task hands each output to the node's tierstore with one content-addressed call, under a prefix-scoped write
  grant, and the ingest adopts the objects in place** (5.2): each output is read once, the node stays hot, and the
  trust boundary is the clean result plus tierstore's own hashing and kbf's read-time check.
- **D18. A tree job's network is `none`, with the node's tierstore passed through**: the job reaches nothing else, and
  the Command does not reach even that.

## 13. What this asks of jobscheduler and tierstore

None of these is kbf code; each is a change for its own repository and design.

**jobscheduler** (additive; none of it knows tierstore, O15):

- **The generic layered root** (its own design): a job's root file system from lower layers given as host
  paths (a kbf tree job names exactly one) and a writable upper on the scratch volume; no image, no pull, no unpack; the command an absolute
  path in the layers; `/proc`, `/dev`, `/sys` and `/run/js-files` bound over directories the layers hold; no tmpfs on
  `/tmp`, `/var/tmp` or `/run`. A public interface change, for the owner's review.
- **A host port passed through**: a port on the host's loopback reachable at the same port on the job's loopback,
  with the job's network otherwise `none`. A public interface change, for the owner's review.
- **A pushed start and a pushed (or long-polled) result**, so neither waits for a heartbeat or a client's poll: without
  them the target of 10.1 cannot be met.
- `Job.start_failure_detail` filled from the agent's refusal code (kbf#2287 item 1); a host to avoid on a job (a
  rerun's previous host); the layered root for a whole-host job (D12).
- **A low-water stop at run time**: the scratch volume is shared with tierstore's cache and nothing bounds a job's
  writes (no quota, O4); `--scratch-low-water-gib` is checked at admission only. When free space falls below it, stop
  the job that is growing and report a reason of its own, so one runaway action does not become `EIO` in every other
  job on the node. Not a quota.

**tierstore**:

- **`hydrate` and `release`** (4.5): `POST /_tierstore/v1/hydrate` with `{"hold", "expires_ms", "tree", "blobs"}`,
  answered `202` at once: flatten the tree, fill the listed blobs below every waiting read, pin the tree's cached
  blobs for the hold, record the hold's failures and `stall_ms`; the same call for a hold already held extends its
  expiry; `POST /_tierstore/v1/release` with `{"hold"}`, answered with what was
  recorded, `not held` when it already ended; a hold ends by itself at its expiry. Authorized by the frontend's key, on
  the node's address. With `put-ca`, these are the pieces of tierstore specific to kbf (O15).
- **Trees that resolve on lookup**: `<mount>/trees/<digest>` resolves for any digest whose root manifest is in the
  store, attached on demand and held while it is referenced (the overlay is mounted over it, or a hold names it), so a
  layer path exists when jobscheduler makes the overlay whether or not `hydrate` came first. With one tree per action,
  a node attaches one single-use tree per job, as many at once as it runs jobs, each detached when its overlay is
  unmounted and its hold released. With it, nothing on the job's path needs the control socket.
- **`put-ca` and prefix-scoped write grants** (5.2): `POST /_tierstore/v1/put-ca`, body the bytes, bearer a write
  grant minted with the frontend's key; stores at `<grant prefix><sha256>` as it hashes the stream, create-only (an
  existing object of the digest is success), writes through to its upstream before answering, answers
  `{"sha256", "size", "key"}`; the grant also allows one named `commit` object, and nothing else (no read, list or
  delete). On a build node it listens on loopback (for the passed-through port), and the bytes stay in its cache under
  their digest, the entry a later fill of the same digest finds. tierstore today has no key that writes under one
  prefix only (`crates/tierstore/src/server/mod.rs`); this is that key.
- **The overlay with one lower from the FUSE mount**, as E4 measured it (rootless `crun`, copy-up, whiteouts),
  measured again on the farm's 6.8 and 7.0 kernels over an action tree that names the kbf base tree's subtrees.
- **A shared subtree flattened once per node, or its cost stated**: every action tree names the same base subtrees
  by digest, and tierstore flattens a tree whole when it is first attached (ts 6.3, I1), so without sharing each
  action pays the base's entry count in flatten time (E8: about 1.7 µs and 110 bytes per entry) and holds that table
  while it runs. kbf asks that a flattened subtree be kept once per node by its manifest digest and reused by every tree
  that names it, each tree's inode numbers still its own and derivable as I1 needs (an overlay sees one tree); if
  tierstore keeps whole-tree flattening, the cost per action is measured in 10.1 and the base kept small enough to
  meet the target.
- **Refusal before the job, not during it**, when a tree's flattened manifests or its pins would pass the node's
  memory or `pin_limit`: the lookup that would resolve it fails (the start fails, and kbf resubmits), never `EIO` in a
  running job.
- **A count of fills that failed outside any hold** (a metric of the node's tierstore), so the gap of section 6 is
  measured.
- **Who may read a tree**: once a tree resolves on lookup, its digest is the only credential for reading what it
  names, and a manifest's `source` may name any key of the store. On the farm every job is kbf's, so nothing crosses a
  tenant today; before a second client's jobs share the nodes, fill sources must be tied to their writer's prefixes
  and lookups to what the job's client may read.

kbf relies on the manifest format of ts 6.1 as merged, `cas/l/<sha256>` for manifests and whole objects, and ranged
`source`s into kbf's packs and whole-object `source`s into `cas/j/`.

## 14. macOS later, and other open points

**macOS** is out of scope for now (the Macs are in device-management enrollment; they keep `kbf-daemon` with the
native driver, and the frontend keeps the worker listener). One open question: macOS has no overlayfs, so the shape of
3.1 does not carry over. tierstore itself would have to serve each job a writable view, its lower layer the tree and
its upper a local APFS directory, through its macOS mount (the NFSv3 frontend serves read-only trees today, and the
core's views were dropped, ts 12 item 11). Whether tierstore builds that, and how a Seatbelt-confined job is pointed at
it, is for a macOS design.

**Other open points:**

- **Collecting `cas/l/`.** Nothing deletes there today, manifests and converted image files included. A collector
  must not delete what a tree in use names (ts 6.1); it belongs with storage retention.
- **`kbf-task-pack` refused unknown fields** until item 8, which is why that reader ships a release before the writer
  (D13).
- **The farm's virtual workers** bound kbf's in-flight work by configured shapes, not the pool's real capacity.
- **The base image's contents**: this design fixes what the base tree must and must not hold, not what else; making it
  barer is the base image's own change, measured by the conformance build.
