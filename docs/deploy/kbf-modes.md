# The kbf modes: frontend and storage

`kbf` is one binary with two modes: `frontend` (the REAPI service, the executor and the
operator API) and `storage` (a storage node). A farm is one or more frontends and the storage
nodes they route to; the actions a frontend places run as jobs of the jobscheduler's job
service. This page says what each mode runs, which ports it uses and who dials whom, how
to grow from one host to many, and how each mode stops. Every flag and default below is the
one in the code; settings the page does not name are in the config reference at
kbf.dev/docs/config/.

The units that start each mode, and the rules `kbf-deploy` checks them against, are in
[kbf-units.md](kbf-units.md). Installing a host is [kbf-installer-setup.md](kbf-installer-setup.md).

## The modes

A mode is chosen with `kbf --mode=<mode>`, or with `mode = "..."` in `/etc/kbf/kbf.toml`
(`kbf` alone reads the file). `kbf --mode=<mode> --help` lists the mode's flags, and a mode
refuses by name the flags that belong to another.

| mode | what the process runs | listens on | state it keeps |
|---|---|---|---|
| `frontend` | REAPI, the executor, the worker listener, the operator API; no blobs | REAPI, worker listener, optional operator API, optional enrollment listener | the state store of its operations |
| `storage` | the storage service: the cache and the metadata log | the storage port | blobs and the metadata log |

### The removed modes

`daemon`, `storage-and-daemon` and `all-in-one` were removed. `kbf --mode=daemon` (and the
other two, as a flag or as `mode` in `kbf.toml`) exits 2 with a line that points here, and
a host whose `kbf.toml` still names one does not start. What replaces them:

- **A storage node** runs `kbf --mode=storage` (`kbf-storage.service`), as the host that ran
  `storage-and-daemon` did for the storage half. Its `kbf.toml` keeps its storage keys; the
  `[daemon]` table of a file still parses and holds only the runtime's thread counts.
- **Placed actions** run as js-agent jobs: the frontend started with `--executor js-agent` makes
  one job of the job service of each action it places, on Linux and on a Mac. No `kbf` process
  runs on a build host to take work, so a build host has no `kbf` unit. The executor is
  described below, and the move onto it in [job-service-cutover.md](job-service-cutover.md).
- **One host** that was `all-in-one` runs a `frontend` and a `storage` node as separate
  processes, with the job service on its own; see "Growing from one host to many" below.

The flags of the worker session are retired: `--heartbeat-interval-ms`, `--vm-host-floor-cpus`,
`--vm-host-floor-mem-gib` and `--pack-commit-sample` are accepted, ignored and warned about at
start, for one release, so a unit that still carries them starts.

The vocabulary of the removed modes stays readable, so older data and certificates are still
understood: the role `daemon` (`urn:kbf:role:daemon`), the host roles `daemon` and `mac-daemon`,
and every protobuf message.

**`frontend`.** It serves REAPI (Execute, operations) and, when `--worker-listen` is
given, the worker listener (storage nodes' `Membership` calls and a host updater's node calls). Storage nodes connect to that listener, so a
frontend that has a storage node gives the flag; a frontend with none (`--executor js-agent` and no storage
node) binds no worker port. It routes each cache call to a storage node. It holds no blobs
and refuses every flag of a store. `--storage` is the URL of one storage node, and is required
unless the frontend runs with no storage node at all ([below](#a-frontend-with-no-storage-node)).
`--state-store` is required too, a Postgres URL (a `sqlite://` one still starts in this
release; see "Growing from one host to many"): a frontend given none, by the flag or by `state-store` in `kbf.toml`, exits 2 naming the flag
before it listens on anything, and `kbf --mode=frontend --check` exits 2 the same way. A
Postgres store that is not up yet is waited for, `--state-store-wait-secs` seconds (default
300; 0 does not wait; the window is the time spent waiting between attempts), and a start that runs out of the wait exits 1, not 2
([kbf-units.md](kbf-units.md#the-state-store-postgres-for-the-frontend)). With
the TLS flags the frontend keeps the farm's storage map and routes every call by it;
`--storage` is then only the bootstrap node.

`--pack-index` (off by default; the `pack-index` key of `[scheduler]` in `kbf.toml`) opens
the pack index at start: two tables of the Postgres state store (`pack` and `blob`) that
say which pack holds each blob and where. The `kbf.worker.v1.PackIndex` service is not served. Unless the frontend is also given the pack store flags below, nothing else reads
or writes them. It needs a `postgres://` state store whose schema has the tables, which `kbf-installer setup` and the update step create, and a `sqlite://`
state store is refused with a line that says so (exit 2). With the flag off no pack table is
touched. The tables are an additive schema step: a kbf of the previous release keeps running
on the migrated schema.

**Packs** (off by default; the `[packs]` table of `kbf.toml`). With
`--pack-store-endpoint` (an `http://host:port` S3-compatible object store; given more than once,
or as an array in `kbf.toml`, it is one store reached through each endpoint in turn, such as the
tierstores of the farm's nodes in front of one bucket; a request that could not connect is sent
to the next, at most three endpoints for one request, and one that may have reached a store is
never sent to another endpoint),
`--pack-store-bucket` and `--pack-store-credentials-file` (JSON
`{"access_key_id": "...", "secret_access_key": "..."}`, mode 0600 or 0400; the secret is never
a flag), and `--pack-index`, the frontend keeps the blobs clients upload in packs: many blobs
in one object, written to the store with a create-only request and recorded in the pack
index before the upload is acknowledged (a failed write answers UNAVAILABLE; packs finished
before the failure stay recorded, and the client sends the rest again). FindMissingBlobs asks the index and then, for the digests it lacks, the storage
nodes. A read, the client's and Execute's own, looks in the index first and fetches only the
blob's bytes from its pack with a ranged request, hashes them, and serves them (a pack that
holds other bytes than the digest names is never served); a blob the index lacks is read
from the storage nodes as before. A blob over 64 MiB and a compressed blob are not packed.
The closure check of the action cache counts a blob as held when the index has it: the check
of a result a job reports, and of every hit (Execute's and a client's `GetActionResult`),
works out what the result names (its output files, standard streams and the files of its
output trees) and asks the index first and the storage nodes for the rest, so an output that
is only in a pack satisfies it; the entry handed to the nodes names only the blobs they hold.
A result with an output that neither holds is refused on a write and is a miss on a hit. On a
routed farm a client's `GetActionResult` asks each blob's primary node only (as
`FindMissingBlobs` does), where Execute's check also tries the blob's other owners, so right
after a map change a client may see a miss for a result that Execute still hits, and runs the
action again. With no pack flags the nodes check the entry alone, as before.
`--pack-bytes` (default 32 MiB) is the length at which a pack is closed, `--pack-writer`
(default `frontend`) names the frontend in its pack keys, `--pack-store-region` (default
`us-east-1`) is the signing region, and `--pack-cache-dir` with `--pack-cache-bytes` keeps
the blobs read from packs on a local disk. A build's job commits its pack into its put directory
and the frontend records it after re-hashing it; no other caller commits a pack. `--pack-commit-sample` is retired: accepted, ignored
and warned about at start, for one release. At start the frontend checks that the store
refuses to replace an object, and does not serve if it does not. `GET /metrics` gains the
`kbf_pack_*` families. Left out, no object store is built or contacted and the frontend
serves exactly as before.

The uploads into packs share a bound on the bytes they hold in flight,
`--pack-inflight-bytes` (default 1 GiB, at least 64 MiB, the largest blob that is packed, so
that no upload is refused for its size; at most 4294967295): an upload takes its blob's size as
soon as its resource name has arrived, before it reads on, and gives it back when it ends, and
one that finds the bound spent waits its turn, so a burst of uploads is slowed down and none is
failed for it (`kbf_pack_upload_waits_total` counts the waits, `kbf_pack_inflight_bytes` and
`kbf_pack_inflight_peak_bytes` the bytes taken). The bound counts blob bytes, not the memory of
the process. A `BatchUpdateBlobs` is decoded before its handler runs, so the bytes it holds
while it waits are not counted, and it has no idle timeout; reads are not counted either (a
read holds one blob, at most 64 MiB, or one batch). `--pack-upload-idle-timeout` (default 30
seconds, at most a day) ends a ByteStream upload that gets no byte of its body for that long
with `DEADLINE_EXCEEDED`: it stores and records nothing, gives its bytes back, and is logged
and counted (`kbf_pack_upload_idle_timeouts_total`). The time before the resource name has
arrived is not counted, so a client that never sends it is not cut by this timeout. An upload
that keeps sending, however slowly, is never cut and keeps its bytes until it ends, so
`--pack-inflight-bytes` is the only bound on that. A client that claims a blob and sends
nothing holds only its reservation, never a buffer of the claimed size. Both flags are
accepted and ignored without `--pack-store-endpoint`.

Every write to the pack store shares one bound, `--pack-store-writes` (default 64, from 1 to
4096): the packs of client uploads, the large blobs past them, and the blobs and manifests of
action trees (a manifest takes its `--task-manifest-writes` slot first). A write past it waits
at the frontend for one to end, so a burst queues here and not at the store. A streamed write
holds its slot for its whole body: a client's upload of a blob over 64 MiB keeps one for as long
as it takes to send (one that stops sending is ended by `--pack-upload-idle-timeout`), so as many
slow large uploads as there are slots hold the other writers back until one ends.

Every call of the frontend to the pack store (a client's upload or read, an action's preparation
and ingest, base publication, the janitor's listings and deletes) waits out a store that answers
503, such as a store failing over its metadata (up to about 30 s). The call is made
again after jittered waits that double from about 0.5 s to 8 s (each between half and all of
its nominal length, so that the calls a failover stopped together do not come back together),
until the waits add up to 60 s; a 503 after that is the call's answer, its message saying how
long it waited. The store is then taken as down: until it answers a call with anything but a
503, every 503 is its call's answer at once, so a store down for good costs 60 s once, not once
for each call a write, a preparation or a sweep makes. The store's `Retry-After` is not read: the store client gives the frontend no
header of an error answer. A 503 `SlowDown` (tierstore taking as many uploads as it can) of a
write whose body is in hand is back-pressure: it is waited out, holding the write's slot, for as
long as the store answers it, and those waits do not count toward the 60 s. The writes of an
action tree are streamed, so they repeat in their own retry, which reads the blob again: a 503
within `--task-put-retry-secs` (default 60), a `SlowDown` past it; that retry sits above the
bound, so no slot is held while it waits. The waits are counted in
`kbf_pack_store_wait_seconds` and the calls that ran out of them in
`kbf_pack_store_waits_exhausted_total` ([api.md](../api.md)). An action's run whose preparation
or ingest waits does not fail and spends no attempt; one whose store is still answering 503
after the waits is the farm's failure, run again as any other.
A client call is bounded by the `grpc-timeout` it sent: when that passes first, an upload, a
`BatchReadBlobs`, or a ByteStream `Read` before its first bytes ends `DEADLINE_EXCEEDED` (a
moment early, a tenth of the timeout and at most a second, so that the answer reaches the
client before the transport cancels the call), never `UNAVAILABLE`; a call without a
`grpc-timeout` waits for the store, and one whose store is still answering 503 after the waits
ends `UNAVAILABLE`, which a client repeats. A streamed upload of a client blob over 64 MiB
cannot be sent twice, so a 503 while it is being sent fails it `UNAVAILABLE`; such an upload
first asks the store whether it holds the blob, and that question waits out a failover, so an
upload sent again during one waits before it sends its bytes.

Where the frontend runs the actions it is asked to execute is `--executor` (the `[executor]` table of
`kbf.toml`). `js-agent`, the only value, runs every Execute that the action cache does not answer as one job of the job
scheduler's job service ([direct-js.md](../design/direct-js.md)). Any other value is refused,
`kbf-daemon` (the value of the releases that ran a daemon on each node) included. Left out, the
frontend builds no executor and contacts no job service, whatever the other `--executor-*` flags
say: it serves the cache and the CAS and answers an Execute that misses the cache
`FAILED_PRECONDITION`.

kbf does not schedule. The job is submitted at once, with no count, queue or capacity in kbf: how
many jobs run, where and when is the job service's. The frontend reads each job back every
`--executor-poll-ms` (250 ms; 100 to 10000) until it ends and answers the operation. Stopping the
frontend cancels nothing: its jobs run on, and over a `--state-store` another frontend, or the same
one restarted, takes the operations up (below). A Linux job is the kbf image (`--executor-kbf-image`), whose `ENTRYPOINT` is the task tool:
the job's arguments are the tool's flags and nothing is mounted into the container. It names no
cores and no memory: it carries a `size-class` label, the action's build target and whether it is
a test run (`root//crates/kbf-server:kbf-server` built is `root.crates.kbf-server.kbf-server.build`,
its test run `….test`; an action whose output paths name no target is `shape.` and its shape key),
and the job service books it what the past runs of that class used, or, for a class with too few
runs, its cold size (jobscheduler's `docs/design/sizing.md`, section 7.7). A Mac job names no
cores or memory either, with the same label. A job needs a host of `os=linux` and
`--executor-arch` (default `x86_64`) that runs containers, and a wall limit of the action's
timeout plus the run margin. The Macs of the job service are used when `--executor-mac-task-bundle` is given (the release's `kbf-task`
bundle by its manifest digest, the image of a Mac job; without it no macOS action is run),
`--executor-mac-os-version` (the least macOS the Macs run, 26 by default; an
action's `os_version` above it is refused) and `--executor-mac-task-out-root` (the tier store's
put directory on the Macs, `--task-out-root` by default); a macOS action becomes a job of a
Mac's native runtime (docs/design/actions-as-jobs.md, section 14), which the job service at
kbf's pin cannot run yet. It books no disk: the
job carries no `disk_bytes`, so the job service places it on a host whatever scratch disk that
host reports, and the job writes to its host's scratch with no quota. The kbf image is built from
the base environment the actions' Commands see (`--executor-base-image`); both are named by
digest, and the host pulls the kbf image. An action's `container-image` platform property names the base: an action that names
another image is refused as invalid (the message names both), one that names none gets the base,
and a tag is refused. It needs both image flags, and the job service (`--executor-job-url`,
`https://`, or `http://` to loopback only; `--executor-job-token-file`, mode 0600 or 0400;
`--executor-job-ca-file` for an `https` URL), `--executor-frontend-id` (the `kbf-frontend` label
on the frontend's jobs, hashed into every job id; every frontend of one farm that shares a
`--state-store` gives the same one), the packs (`--pack-store-endpoint`, through which the
pack a task writes is read, hashed and turned into the action's result) and nothing else for the
output: a put-dir job (the only mode, `--task-out-mode put-dir`) carries no url, no grant and no secret, and its pack is a file
of its put directory, which the frontend ingests. The executor's presigned path (a write url, a read token, the secret delivery
`argv` or `files`) was deleted, and so were the flags only it read: `--executor-secret-delivery`,
`--executor-insecure-argv-secrets`, `--executor-work-dir`, `--executor-allow-plain-http-put`,
`--executor-allow-plain-http-grant`, `--executor-locate`, `--executor-network` and
`--executor-allow-open-network`. A put-dir job is given no secret and no network, whatever they said. Each is still
declared, hidden from `--help`, and refuses any value at start, naming this release and telling you to delete the flag, so a
frontend started with one (or a `kbf.toml` whose `[executor]` table still has the key, as the template
of the releases before wrote `executor-secret-delivery`, `executor-insecure-argv-secrets`, `executor-locate`, `executor-network`
and `executor-allow-open-network` as live lines) does not start until the line is removed: change the file before the binary. The executor no longer speaks to a lease server: the
flags `--executor-lease-url`, `--executor-lease-token-file`, `--executor-lease-ca-file`,
`--executor-lease-ttl-secs` and `--executor-class` are gone, and a frontend started with one (or
a `kbf.toml` that names one) fails at start naming it. `--executor-output-disk-bytes`, the scratch
disk a job once asked for its outputs, is gone the same way, since a job books no disk: a frontend
started with it, with either executor, or a `kbf.toml` whose `[executor]` table still has
`executor-output-disk-bytes`, fails at start naming it; delete the flag or the key.

The flags of the farm scheduler kbf once had are retired: accepted, hidden from `--help`, ignored, and
a start that is given one logs a single warning. They are `--executor-hosts`, `--executor-host-cpus`,
`--executor-host-memory-gib`, `--executor-host-slots`, `--executor-prepare-concurrency`,
`--executor-ownership`, `--executor-mac-hosts`, `--unservable-wait-secs`, `--memory-ladder-gib`,
`--exclusive-actions`, `--no-memory-climb`, `--learned-sizes`, `--scheduler-core`,
`--placement-load`, `--placement-predictions`, `--usage-profiles`, `--whole-node-*`,
`--default-platform` and `--qos-allowed`. Remove them from a `kbf.toml` at the next edit.

Each attempt of an operation is one job, whose id is made from the frontend id, the operation and
the attempt (`k<8 hex digits of the sha256 of the frontend id>-<operation>-<attempt>`): a submit sent again after its
answer was lost, and a frontend that takes the operation up, name the job already made, so no
action is submitted twice. `CancelOperation` is a cancel of the job. How a job ends is mapped to
failure classes as the table in `crates/kbf-server/src/executor/outcome.rs` says: a job
stopped at its wall limit is the action's own timeout, answered `DEADLINE_EXCEEDED` at once; one
killed for memory, a lost job (the host's fault) and any other ending of the farm's are submitted again as
the next attempt, a new job and a new key, at most 2 times, after which the callers are answered
`UNAVAILABLE` (`FAILED_PRECONDITION` with reason `ACTION_OUT_OF_MEMORY` for memory); and a submit the service refuses because no
host could ever run it ends the action as invalid at once. A job's pack is read, and its outputs
promoted, only from a clean result: the job and its result both say succeeded, the result's
reason is the job's exit and its exit code is 0. Any other ending, a result that leaves out its
exit code included, leaves the pack unread however complete it is, and the action runs
again (or is answered, as the table says). A whole-machine action
(`kbf-lease=whole_machine`) is one job too: it needs the whole host and nothing beside it (no
cores, memory or disk of its own: the host's are all the job's), with the image, arguments,
constraints and wall limit of any other. A whole-host
job the job service could not ready a host for (`exclusive-not-ready`) is an infrastructure
failure, and the action runs again.

**What an action's Command sees in its job.** In a put-dir job (`--tree`, the farm's) the task tool
makes no sandbox of its own, since the job is one (its own user namespace, no network, a layered
root; owner decision of 2026-11-15, `docs/design/actions-as-jobs.md` 4.4, D7): it starts the Command
directly. The Command sees the job's root as it is, with `/run/js-files` empty (a put-dir job has no
URL, no grant and no secret), the run's `/.kbf-run` (its streams and `private/`) and the put
directory `/out`, which is writable (it is the job's own output path). The task ends the Command's
process group when it exits or the timeout passes, and is the subreaper that reaps every process
the Command leaves as it ends while the Command runs, and kills and reaps every process it left, `setsid`
included, before the outputs are read. The job log has no sandbox or
network line for it: nothing is made, so nothing is unusual.

`kbf-task` has no other mode: it refuses `--grant-url`, `--pack-url-file`, `--put-urls-file`, `--fetch-*`, `--locate` and
`--work-dir` (exit 2), so the mount and PID namespaces of its former grant mode, the `sandbox: ...` job log line and the covers
of `/run/js-files` and `/scratch` no longer exist for a job. Its confinement code (`crates/kbf-task/src/sandbox.rs`) is
reached only by `kbf-task --probe-confinement`, which prints `probe sandbox:`.

`--executor-input-tree` says what a Linux put-directory job's root is read from: `manifests`, the
default, writes the action tree as tierstore manifests and gives the job the host directory
`<mp>/trees/<digest>`; `provider` writes it as one tierstore tree list (`cas/l/<sha256>`) and gives
the job tierstore's mount provider, which the host's agent asks for the tree as soon as the job is
assigned (docs/design/actions-as-jobs.md, 4.7). `provider` needs `--task-out-mode put-dir` and a pool
whose hosts serve the provider (`--mount-provider tierstore=<socket>` on their agents); a job no host
can serve is refused by the job service and the farm sees an infrastructure failure. A Mac job always
reads manifests.

**Over a state store.** With `--state-store` the executor keeps the rows `exec/op/<operation>` (owner,
action, attempt, job id), `exec/live/<instance>` and `done/`, `doneof/` that
[direct-js.md](../design/direct-js.md) section 3 lists, and the operation ids come from the store's
shared counter. Any frontend of the store answers `WaitExecution` and `GetOperation` of any operation by name,
and a process whose liveness row has expired (`--executor-live-secs`, default 20 seconds after its
last renewal, on the store's clock; a stopping process deletes its row at once) has its unfinished
operations taken up by another frontend, one compare-and-set for each, which reads the job back
where it is. A job that commits into a put directory is finished from it; one that writes to a
presigned url, which only the process that held the url can finish, is cancelled and run again.
Clients reattach with `WaitExecution`. Without a `--state-store` a restart loses the open operations, and its
clients' builds fail. Every frontend of a farm may run the executor.

The executor prepares each job with the frontend's **task runs** (below), so
`--executor js-agent` is refused when the flags are read unless `--task-secret-file` (with
`--task-out-root`) and `--pack-store-endpoint` are given, and unless they agree with it: the
longest job (`--executor-max-run-secs` plus `--executor-run-margin-secs`) must fit 14400 s less
`--task-url-slack-secs`, the longest the keys of a put directory may be good for once the slack is
added. The job has no network and reads no grant, so no read listener (`--job-listen`) is needed.
The job's wall limit is the action's timeout plus the margin. A job that
cannot be prepared because an input blob is missing, malformed or over the closure budget ends as
an invalid argument to its caller and is not rerun; one that cannot be prepared because the store
cannot be read is rerun. `--executor-base-image` and `--executor-kbf-image`
(`<repository>@sha256:<digest>`, the `docker://` scheme optional) replace the former
`--executor-default-image`, `--executor-images-have-no-entrypoint`, `--executor-task-tool` and
`--executor-root-der`: a missing or tag-named image, or the same image twice, is refused when the
flags are read. The farm's certificate authority for the task's `https` reads must be in the kbf
image. A secret is never part of an error, a log line or a
metric. `GET /metrics` gains the `kbf_executor_*` families, among them the jobs submitted, those
submitted again after a lost answer, the attempts resubmitted
(`kbf_executor_attempts_resubmitted_total`), the operations of a gone process taken up
(`kbf_executor_operations_reattached_total`) and those cancelled; `kbf_executor_phase_seconds` has the phases `submit` (the
Execute to the job service's answer to its submit) and `execute` (the Execute to its callers
answered). A frontend with no executor has none of them. The remaining settings are
`--executor-run-margin-secs` (300; what a job's wall limit
adds to the action's timeout), `--executor-failed-stderr-log-bytes` (512, at most 8192, 0 for
none: the end of a failed job's standard error is one WARN line
of the frontend's log, `the end of the standard error of a job that did not end well`; the whole
output goes with the result as before), `--executor-max-run-secs` and `--executor-default-timeout-secs`
(3000), `--executor-call-timeout-secs` (5), `--executor-poll-ms` (250, from 100 to 10000: how often each operation's job is read back from the job service), `--executor-prepare-limit-secs` (300: how long a run's input
tree may take to publish). A job is given no network, whatever a `kbf.toml` of the releases before
said with `--executor-network`. A setting that breaks a rule the executor's safety rests on is
refused at start, exit status 2. A server that has more than
65 536 open operations refuses the next Execute `UNAVAILABLE`: the one bound kbf keeps.

**The kbf base tree: `--executor-base-tree <DIR>`.** Needed with `--executor js-agent`: the
frontend's cache of the base tree bundle of its release, a directory **the frontend's user may
write** (uid 65532 in the container image). The release's `SHA256SUMS` and its bundle for
`--executor-arch`, `kbf-base-tree-<commit>-linux-<arch>.tree.tar`, are kept in `<DIR>/<commit>/`.
Nothing installs them and no operator copies them: when `<DIR>` has them neither there nor loose
(the layout every earlier release asked for, which still works as it was), the frontend fetches
them from the release it was built from, as below. Before it serves, and so before the executor is spawned and any job
names a tree, the frontend takes the bundle of its own commit and of `linux-` and
`--executor-arch`, checks it against `SHA256SUMS` (listed once, and the file's SHA-256 is the
listed one), and writes every object of the tree that the pack store does not hold to
`cas/l/<sha256>` with a create-only write, through the client and credentials of
`--pack-store-endpoint` (which must be allowed to write `cas/l/`). An object already at its key,
or whose key another writer takes meanwhile, counts as held: the key is bound to its content. A
second release on the same base image writes only its own `kbf-task`, its `.kbf-tools` manifest
and its root manifest. The start logs one line with the root digest and the counts written and
held. With a state store (`--state-store`) it then records the tree as the row
`job-tree/<root sha256>` (the commit and the time of the first publish; an existing row is left
as it is). A bundle that is missing for the configured arch, not listed in `SHA256SUMS`, not the
listed bytes, of another commit or platform, an object of another length at its key, a store
that fails, or a build that does not know its commit, fails the start with a line that begins
`--executor-base-tree:` and names the file or the key. Nothing is read or written under
a frontend with no `--executor`, whatever the flag says. See [job-service-cutover.md](job-service-cutover.md), "The kbf base tree".

**The fetch of the bundle: `--executor-base-tree-url`, `--executor-base-tree-keep`,
`--executor-base-tree-timeout-secs`.** `--executor-base-tree-url` is a list of releases in the
shape of GitHub's `GET /repos/<owner>/<repo>/releases` (an array; each release has `assets`, each
with `name` and `browser_download_url`); the default is
`https://api.github.com/repos/komira-ai/kbf/releases`, kbf's own releases. The frontend reads it
a page at a time (`?per_page=100&page=N`) until a release lists both the bundle's file name, which
holds the commit it was built from, and `SHA256SUMS`; it downloads those two files (redirects
followed, https only, except plain http to a loopback address) into
`<DIR>/.fetch-<commit>-<instance>/`, a directory of this process alone (its executor instance
id), hashes the bundle against the digest that `SHA256SUMS` lists, and only then renames the
directory to `<DIR>/<commit>/`. A download that stops or is not the listed bytes is removed and
fails the start; a partial file is never where anything reads. Once `<DIR>/<commit>/` exists, a
start fetches nothing. Two frontends that share `<DIR>` may fetch one commit at once: the one
that renames second keeps the other's checked release and removes its own download. After a
fetch the cache keeps the running release and the `--executor-base-tree-keep` (default 1) newest
other releases, for a rollback of the frontend, and removes the rest and the downloads of a
stopped start once they are a day old (a younger one may be another frontend's, filling); it
never touches files loose in
`<DIR>` or names that are not a commit. `--executor-base-tree-url=""` fetches nothing: the files
are expected in `<DIR>` as before, and a missing one is named by the publish.
`--executor-base-tree-timeout-secs` (default 60) is how long a connect, a response head or the
gap between two chunks of a body may take. The frontend needs a route to the URL's host (and to
the host the release's assets are served from) at its start when the cache lacks its release,
and only then; a host with no route out sets `--executor-base-tree-url=""` and stages the files
by hand.

**Switching a running frontend onto the job service.** The executor-to-job-service
mode is `executor = "js-agent"` with the job service's keys (`executor-job-url`,
`executor-job-token-file`, `executor-job-ca-file` for `https`) in
the `[executor]` table. `kbf-deploy job-service plan` prints what the switch sets in a frontend's
`kbf.toml` and the flags of the mode it runs now; `kbf-deploy job-service apply --apply` checks
the service with kbf's client token, writes those keys (the original kept as
`kbf.toml.pre-job-service`), drains and restarts the frontend through `kbf-host-install`, waits
for `GET /readyz` and runs one canary job. Both verbs are dry runs without `--apply`. The
procedure, the canary and the refusals are in [job-service-cutover.md](job-service-cutover.md).

Per-action overhead of the executor is read from two places. The
executor's own phases are the histogram `kbf_executor_phase_seconds{phase=...}` on `GET /metrics`:
`prepare` (the Execute to its job being ready: its action read, its put directory minted and
its action tree named), `start` (ready to the job service answering the submit with the job: the wait for
the tree's writes, with the operation's row written meanwhile, then each `SubmitJob`), `queue` (accepted to first read back as running), `task` (first
read back as running to read back as ended), `run` (accepted to read back as ended, whether or not
it was seen running), `ingest` (read back as ended to the read of the pack done), `submit` (the Execute to the job service's answer to its submit) and `execute` (the Execute to its callers answered). They are
stamped by the executor on its own clock, once per poll of each job, and the job service learns a
job's state from its host's agent, which trails the host by one or two beats: `queue`, `task` and
`run` are good to a pass plus a heartbeat or two, not to the instant the host started or ended the
job, and a job that starts and ends between two reads has a `run` and no `queue` or `task`. A phase that was not observed
is absent, never zero. The buckets start at 1 ms. Where `prepare` and `start` go is the histogram
`kbf_executor_step_seconds{step=...}`, each step timed around its one wait: `facts`, `put_dir` (with the closure's
`action`, `walk` and `held`), `tree` (to the action tree's name), `row`, `rpc` (each `SubmitJob` call), `backoff`
(the wait before a submit is sent again) and `tree_wait` (a run waiting for its tree's writes, before its submit,
while its row is written). [api.md](../api.md) lists them. The task's own account of its steps (`fetch_ms`, `run_ms` and `pack_ms` of the manifest) is carried
in the result's execution metadata as the `input_fetch`, `execution` and `output_upload` start and
completed times, marked as a task's claim, and the run record and the read API say so with
`phase_times: "task_claim"` (a run recorded by an earlier release is `measured`, and so is a record written before the
field). The manifest holds durations only, so the three are laid back to back and end at the time the
frontend read the pack. For a `task_claim` run only the three durations are data, each as long as the
task said and none the frontend's own measurement: the instants, and the gaps between them and the
server's `worker_start` and `worker_completed`, are not (the delay from the task's end to the
frontend's read lands before `input_fetch_start`). A claim is fine for a measurement and never for a
decision: nothing reads it to decide, and kbf keeps no estimator, so
nothing is learned from these times. A step the task reports as 0 ms has no timestamps. A task that writes into a put
directory (`kbf-task --stage-and-commit`) also writes the phase timings `walk_ms`, `hash_ms`,
`write_ms` and `commit_ms` in the manifest's `timing`; ingest does not carry them.

**Task runs** (off by default; the `[task-runs]` table of `kbf.toml`). With `--task-secret-file`
(and `--task-out-root`, the tier store's put directory on every host, `<mp>/put`, an absolute path;
the action trees are under `<mp>/trees` beside it) the frontend prepares the runs of a host
executor (docs/design/jobs-on-jobscheduler-and-tierstore.md). For each run of an Action it reads
the Action, its Command and the whole input tree and checks that every blob is held (a blob that is
missing is an error, never a smaller tree, and a tree over `--task-closure-max-digests`, default
500000, or `--task-closure-max-bytes`, default 32 GiB, is refused), writes the action tree over the
published kbf base tree, and mints one put directory, `cas/b/<operation>-<attempt>/<nonce>` (its nonce a MAC over
the run, the action and the minute). The directory's keys are good for the run's wall limit plus
`--task-url-slack-secs` (default 600), and never for more than 4 hours (14400 seconds). The job has
no image: its root is the one read-only layer `<mp>/trees/<digest>`, it runs
`/.kbf-tools/kbf-task --tree --stage-and-commit --out-dir /out`, the directory
`<mp>/put/cas/b/<operation>-<attempt>/<nonce>` is mounted read-write and `no_exec` at `/out`, and it has no
network, no file and no secret. The host's agent must list
`<mp>/put` as a `--mount-root` (read-write) and `<mp>/trees` as a mount root. The task commits one
object, `pack` (the one-pass object, `--task-out-format one-pass`, the only value); the ingest
reads its trailer and manifest, refuses an output path the Command does not declare, streams each
large blob into the CAS, and records the object's index in the pack index without reading its
records (the first location recorded for a digest is kept, and a read of a record that does not
hash to its digest drops that location). The ingest still reads the packs and large objects a
task of an earlier release committed. The secret file holds one secret of at least 32 bytes,
owned by the server's user and readable by it alone; the mint is made from it, and the job-facing
listener (if one is served) must verify grants with the same secret. No secret is a flag. Left
out, no secret file is read and the frontend serves exactly as before.

**What a prepare reads, and the caches that spare it** (the `[task-runs]` table). Preparing a run
walks the input root of its action, decoding each `Directory` message, then writes the action tree
and mints the put directory. Four caches, all keyed by digest or by the action, make a repeated prepare
do no store call:

- *The directory cache* holds the decoded `Directory` messages by digest. It is always on in memory
  (256 MiB). With `--task-directory-cache-dir DIR` and `--task-directory-cache-bytes BYTES` (each
  needs the other; BYTES at least 1) it also keeps them on disk, one file each, at most BYTES of
  them, the least recently used removed first. Every read from the disk is checked against the
  digest, so nothing in the directory is trusted, and a restart reads from the disk what the memory
  lost. One process per directory: nothing else may write there. A directory that cannot be opened
  fails the start, naming `--task-directory-cache-dir` and the path.
- *The place memo* of the tree writer remembers where the bytes of a blob already are (a span of a
  pack, or a whole object), so that a repeated write finds them without asking the pack index or
  the store. An entry is good for 10 minutes, and a blob the writer wrote whole is remembered at
  once.
- *The tree cache* of the executor's front remembers, for an action and the form of its input tree
  (manifests or tree list), the tree that was written: a hit skips the closure walk, the lookups and
  every write, and only the put directory is minted. An entry is good for 10 minutes, and the cache
  holds a bounded number of entries.
- *The action facts cache* remembers what the action says of its image, platform and timeout, by the
  action's digest. It never expires, since an action's digest names its bytes, and holds a bounded
  number of entries.

A prepare that hits all of them reads nothing from the store and writes nothing. All four are
memory held by the process (the disk half of the first aside) and lose nothing a restart cannot
fetch again.

**Removed flags in a `kbf.toml`.** The flags a frontend removed are the executor's presigned
path (eight), the presigned task outputs (ten) and the store read grants (three, under
`[job-read]`). A `kbf.toml` read by `kbf --mode=frontend` (a start or `--check`) that holds any of
them as keys is refused once, exit status 2, before anything starts and before `--check` looks at
the host: one line per problem of the file, every removed key named as ``removed key
`<table>.<key>` in <file>: <why>`` (the reason of its set, which ends in the way out, to delete
it), together with every other problem the file has, so the operator deletes them all in one
edit. A removed flag given on the command line is still refused by the flag parser, naming that
flag. `kbf --version --json` lists the flags a build refuses as `removed_flags`, and the release
notes' "Config migration" section (under "Before you click") lists the ones a release adds to the
previous release's list: delete those keys from every frontend's `--config` file before the release
starts. When the previous release prints no `removed_flags` (every release before the field), the
section lists all of the new release's and says the previous release does not say which it refused.
`kbf --mode=frontend --check --config FILE`, run with the new release's binary, names every key a
file still holds.

**Where a task's outputs go** (`--task-out-mode`, `put-dir`, the only value and the default; the
`[task-runs]` table). The `presigned` mode of the releases before, a presigned write of each pack
to the tier store and a read grant for the inputs, was removed: `--task-out-mode presigned` is
refused at start, naming this release, and so is each key only it read (`task-put-endpoint`,
`task-put-bucket`, `task-put-region`, `task-put-credentials-file`, `task-read-url`,
`task-run-wall-secs`, `task-max-packs`, `task-max-large`, `task-large-credentials-file` and
`task-large-max-bytes`), so a `kbf.toml` that still sets one is changed before the binary is
(the farm's config of komira-ai/komira-internal drops them in the same change that updates the pin).
An input root that holds `.kbf-out` at its top is refused, and a `whole_machine` action cannot run
in this mode (the job service gives a whole-host job no mount).

`--task-out-format` (`one-pass`, the only value and the default) says what the task writes (docs/design/one-pass-task-object.md). The task commits `pack`
alone, one object whose manifest and index are at its end, read once from the outputs, with no cap
on its length or on a blob (a blob over the record cap is a record of the object, found by its
index row and read by range; a frontend whose reads of a large blob go to the storage nodes copies
it there by a ranged read, as it did the `L<i>` object). The frontend always adds `--object-format
one-pass` to the task's flags, which `kbf-task` still accepts though it has no other format. The
`packs` value is gone: a config file that sets it is refused at start, so remove the key or set
`one-pass`. The ingest still reads the packs and large objects an earlier release's task wrote
(one in flight at a restart, or a stored object), and nothing already stored is touched. Every
frontend and every task image must be this release or later; in put-dir mode the task is the base
tree of the frontend's own build, so they move together. To go back to a task that writes packs,
run the release before this one, with the config rolled back before the binary. The tail of a
one-pass object is read in one range bounded by `--task-max-object-manifest-bytes` (default 4 MiB)
plus `--task-object-bytes-per-entry` (default 1024) for each of `--task-max-object-entries`
(default 1,048,576) records. The frontend also checks the object's file index against its
manifest and, given its action-tree writer, publishes each output directory as a tierstore
manifest whose files point into the object, so a later input root holding the same directory
reuses it.

**The `cas/b/` janitor** (off by default; the `[task-runs]` table of `kbf.toml`). The packs and
large objects tasks write under `cas/b/` are not all kept: a refused run, a run that failed after
its write, the unused keys of a key set, and a large object the ingest has already copied into the
CAS leave keys nothing refers to. With `--task-janitor-key-file` the leader (the server that holds
its roles: the `leader` flag of `/readyz`) lists `cas/b/` every `--task-janitor-interval-secs` (default 900) and deletes
a key that is older than `--task-janitor-grace-secs` (default 21600, which is also the least: the
18000 seconds at which an ingest refuses a key, the 4 hours the keys of a put directory are good for and an
hour for the ingest, and an hour more as a margin between frontends), is not one an ingest is verifying
right now, and is not recorded in the pack index (asked again for that key just before the
delete; a pack the ingest recorded stays where it lies and is never deleted). The age is the
minute in the key's own name, for a pack (`-<k>`) and a large object (`-L<i>`) alike; a key under
`cas/b/` whose name does not parse is kept and counted. It never lists or deletes under `cas/l/`
(content-bound large objects), `cas/f/<writer>/` (frontend packs) or `cas/d/` (packs of the removed daemon). The
key file is the JSON of the tier store's GC key (`--gc-key-file` there), whose prefix must be
exactly `cas/b/`, owned by the server's user, mode 0600 or 0400; it signs the janitor's requests to
the store of `--pack-store-endpoint` (same endpoint, bucket and region), which the janitor needs,
as it needs `--task-secret-file` and `--pack-index`. A sweep lists `--task-janitor-page-size`
(default 500) keys at a time and waits `--task-janitor-page-pause-ms` (default 1000) after each
page, which bounds how fast it lists and deletes. `--task-janitor-dry-run` counts and logs what a
sweep would delete and deletes nothing. A key the janitor is about to delete is claimed in the
registry the ingests hold their keys in, after the pack index's last answer and through the delete,
and a claim and a hold on one key exclude each other: an ingest that has begun makes the janitor
keep the key, and one that begins after the claim is refused (`claimed`) before it reads anything.
An ingest also refuses a key 18000 s old (`expired`), so a key seen as ended only after a long
outage is never committed. Holds and claims are per process: across frontends (clocks that differ,
or a fenced old leader still ingesting while the new leader sweeps) a key is safe by the age rule
alone, and its margin is the grace less 18000 s, an hour at least. The leadership is asked before each page and each delete,
and a sweep that finds it lost stops (`..._abandoned_total`). A 401 or 403 on a delete ends the
sweep at that key, and so do 5 deletes failing in a row; each is one `..._sweep_errors_total` and one log
line. `GET /metrics` has `kbf_cas_b_janitor_listed_total`,
`..._reaped_total`, `..._kept_young_total`, `..._kept_verifying_total`,
`..._kept_referenced_total`, `..._kept_unparsed_total`, `..._kept_outside_total`,
`..._would_reap_total`, `..._reap_errors_total` (deletes the store refused, retried by the next
sweep), `..._sweeps_total` and `..._sweep_errors_total` (sweeps the listing or the pack index
stopped).

The blobs the storage members flushed before the frontends wrote packs are in no pack the
index knows, so a read of one still goes to the members. `kbf-pack-adopt`, an operator tool and
a binary of its own, indexes those segments where they lie, with no byte copied and nothing
written to the object store: [pack-adopt.md](pack-adopt.md) gives the order to run it in.

**The job-facing read listener** (off by default; the `[job-read]` table of `kbf.toml`). It is
the frontend's own read listener, not an executor mode: the jobscheduler's job service, which
serves under a `--job-listen` of its own, is another process, and the switch of
an executor onto it is in [job-service-cutover.md](job-service-cutover.md). No executor job
reads from it: a job of the executor reads its mounted tree and writes its put directory. It
serves whatever holds a read grant (a token valid until its expiry, for one Action: it reads that
Action, its Command and its input tree, and no other blob). Such a reader is untrusted code that
holds no client certificate, so the REAPI listener is not open to it; `--job-listen <addr>` opens
another, over HTTPS (or plain HTTP, below), that serves `GET /<hex>/<size>` and `POST /batch` (below) with
`Authorization: Bearer <read grant>` and nothing else (any other path is 404, any other method
405). It needs the secret and a transport (a certificate and key, or `--job-plain-http`), and takes
two more flags that have defaults:

- `--job-tls-cert` and `--job-tls-key`: the certificate chain and key of the listener (PEM), issued
  by the farm CA that the task is told to trust. The TLS is the server's alone: no client
  certificate is asked for. The task dials the frontend by IP address, and a client that dials an
  IP address accepts only a certificate that names it as an IP subjectAltName, so the certificate
  must carry one for each address a job dials the frontend at (for a farm of frontends, one
  certificate per frontend with its own address, or one naming them all). The start checks this
  and exits 2 when the certificate has none, or when `--job-listen` is on one address and the
  certificate does not name it; on `0.0.0.0` or `::` it needs some IP name. The files are read
  again every `--tls-reload-interval-secs` like the other listeners' (the names are checked at
  start only).
- `--job-plain-http`, in place of the two: serves the same listener in plain HTTP, with no TLS,
  for a farm on its own network: TLS is optional on a trusted job network, and a read grant is a
  bearer token, so whoever can read that network can use a grant until it expires. It serves the same
  routes and the same bounds, and every request still needs a valid read grant, checked as over
  TLS; what changes is that the grant and the blobs cross the network unencrypted. Given with `--job-tls-cert` or
  `--job-tls-key` it is refused when the flags are read (which of the two to serve is not a
  guess to make); there are no IP-name checks and no TLS reload series in this mode.
- `--job-grant-secret-file`: the farm secret the grants are signed with (at least 32 bytes, mode
  0600 or 0400, owned by the server's user; surrounding whitespace is dropped). Every frontend
  of the farm, and whatever issues the grants, reads the same secret, so a grant is verified by
  any frontend. It is never a flag.
- `--job-max-blob-bytes` (default 536870912, 512 MiB): the largest blob a response carries.
- `--job-reads-in-flight` (default 32): the blob reads open at once.
- `--job-read-budget-bytes` (default 1073741824, 1 GiB): the bytes of blob reads open at once. A
  read takes its blob's size first (a streamed blob a bounded window of 16 MiB, a batch the sizes
  of its blobs together, and the reads of the scope walk their own sizes); one over the budget
  waits for room and never fails, and a blob larger than the whole budget is read alone. The room
  stays taken until the response has been sent (or the connection dropped), so that clients that
  are slow to read do not hold more than the budget; a batch also pays 64 KiB for each gap
  between its blobs, and takes at most half of `--job-reads-in-flight` while it reads. An Action
  or Directory blob over 4 MiB gives its Action no scope (it is not read).
- `--job-read-idle-secs` (default 60): a response that holds budget and whose client takes no
  chunk for this long is dropped, its budget given back and its response ended (the client sees a
  broken response). A client that reads is not cut off however long its response takes in all; one
  that opens a response and never reads it holds budget for this long and no longer. The scope
  of an Action is worked out once however many tasks ask for it at the same time.
- `--job-store-grant-key-file`, `--job-store-url` and `--job-store-bucket` were removed:
  tierstore v0.3.29 removed the store read grants they configured. A start that gives one, as a
  flag or as its `kbf.toml` key, is refused at once, exit status 2, with a line that names the
  flag and that release; delete it. A `GET /locate` answer has no header line and no `g`, and a
  reader gets every blob from this listener.
- The tee that copies a large blob to tierstore while it is streamed buffers up to 32 MiB for each
  live put (64 MiB in all at the default concurrency of two). That buffer is outside
  `--job-read-budget-bytes`, which counts what a read and its reply hold.

The answers are 200 with the blob and its `Content-Length`; 401 for a missing, malformed, forged,
expired or revoked grant (with `WWW-Authenticate: Bearer`); 403 for a digest outside the grant's
scope, or a grant whose Action has no readable inputs in the store; 404 for a blob that is not
held, an unknown path, and 405 (with `Allow: GET`) for another method; 413 for a blob over
`--job-max-blob-bytes`, without reading it; and 503 when the store cannot answer. A body carries a
reason code (`missing_token`, `invalid_token`, `expired`, `revoked`, `out_of_scope`, `no_scope`,
`too_large`, `not_found`, `method_not_allowed`, `store_unavailable`) and nothing else. A blob is
read as the REAPI listener's reads are (the pack index first, then the storage nodes, checked
against its digest). A blob of at most 64 MiB is read whole and sent whole. A larger one is
streamed from the storage nodes through SHA-256, one chunk at a time with the last held back until
the whole has hashed to its digest, so a response never holds more than a few MiB of it; a blob
whose bytes are not its digest's ends its response short of its `Content-Length` (the client sees
a broken response and refuses it) and is counted in `kbf_job_read_stream_ended_early_total`. At
most `--job-reads-in-flight` reads are open at once (a streamed one until its response ends), at
most 256 connections are open, a client has 10 seconds to send a request's headers, and a
connection lives at most 10 minutes. The listener's socket is closed the moment a drain begins;
the connections open finish their request. `GET /metrics` of the operator API gains
`kbf_job_read_requests_total` (by `outcome`), `kbf_job_read_bytes_total` and
`kbf_job_read_stream_ended_early_total` (by `reason`: `digest_mismatch` or `store_error`), and
`kbf_tls_reloads_total` a `listener="job"` series. Where the jobs run must be allowed to reach
this address and port (the egress allowlist of the worker's network), and the listener should be
on an interface the jobs, and nothing outside the farm, can reach.

`POST /batch` (same grant, `Content-Type: application/json`) answers many small blobs of the
grant's Action in one request, for a task that would otherwise make thousands of GETs. The body
is `{"digests":["<64 hex>/<size>", ...]}` with 1 to 256 entries (a repeated digest is answered
each time) and at most 64 KiB. The whole request is refused, before any blob is read, with the
reason codes above: 401 and 403 for the grant, 400 `malformed` (bad JSON or digest, no entries or
more than 256, a body over 64 KiB or one that does not arrive within 10 seconds), and 413
`too_large` when the sizes asked for add up to more than 4194304 (a digest of a blob that alone is
over 4194304 bytes is not added, it is answered `too_large` below). Otherwise it is 200
`application/octet-stream` with one frame per digest in request order: a status byte (0 ok, 1
not_found, 2 out_of_scope, 3 too_large (use GET), 4 store_unavailable), the length as 8 bytes
big-endian (the blob's size when ok, else 0), and the bytes. Each digest is checked against the
grant's scope, the batch's read takes one reservation of `--job-reads-in-flight` places (at most
half of them, however many blobs it reads; a GET takes one), and the frontend hashes every ok
blob against its digest. The blobs of a batch are read together: with packs, those in a pack are
read with few ranged reads of the object store (one for blobs that lie close together), and only
the others from the storage nodes. A GET of `/batch` is 405 with `Allow: POST`. The golden request and
response bytes are `crates/kbf-front/tests/data/job_read_batch.golden`. `/metrics` gains
`kbf_job_batch_requests_total` (by `outcome`) and `kbf_job_batch_blobs_total` (by `status`).

**`storage`.** It serves the storage service over gRPC and nothing else: no REAPI, no
executor, no operator API (only `GET /metrics`, on `--metrics-listen`, off unless given).
It has no state store; it reaches one only through a frontend. Without `--frontend` it is a
farm of one member. With `--frontend <url>` it registers with that frontend, heartbeats to
it, and takes the storage map from it.

### A frontend with no storage node

A frontend started with no `--storage` has no storage node and dials none. It needs three
things in place of one: `--executor js-agent` (the actions run as jobs of the job service, so no
process reads or writes a blob on a node), `--pack-index` (the Postgres state store holds the
index of the blobs and the action cache) and `--pack-store-endpoint` with its bucket and
credentials file (the object store, tierstore, that holds the packs). A start without any of the
three is refused naming the flag. The three
`--storage-client-*` flags need `--storage` and are refused without it.

What it does itself, with those three:

- **Blobs.** A blob a client uploads (BatchUpdateBlobs, ByteStream `Write`) is packed when it is
  within the pack bound, and is stored as the object `cas/l/<sha256>` and recorded in the pack
  index when it is larger. Reads are ranged reads of the object the index names. FindMissingBlobs
  asks the index and nothing else. A compressed ByteStream name is refused INVALID_ARGUMENT.
- **The action cache** is the `action_result` table of the state store: one row per action digest,
  the encoded `ActionResult` inline. A client never writes it; the frontend writes a job's result
  into it in the transaction that records the packs of its outputs. The table is made by the
  installer's migration 3 (`kbf-installer`), which must have run before the frontend starts: a
  frontend over a database at migration 2 starts and fails each action-cache call `NotMigrated`.
  A cache that is empty at the start is a valid cache; nothing is imported from storage nodes.
- **Directory rows.** The walk of an action's input tree before its job is submitted reads each
  level of Directories from the `directory` table of the state store (installer migration 4) with
  one query, and reads only the Directories without a row from their packs; the walk's
  Directories read from packs are recorded as rows once it ends, by a task the preparation does
  not wait for; see `docs/design/metadata-in-postgres.md`. A database at migration 3 starts and
  every walk reads the blobs, logging a warning for each failed read, and for a failed record
  of the walk's rows.
- **Readiness.** `GET /readyz` and `grpc.health.v1` fail when the state store does not answer a
  read and when the job service does not answer a read of a job (`executor` in the failing
  list). Nothing probes the object store or any storage node: an object store that is down is
  found by the call that needs it, which waits out its 503s for up to 60 s (above) and then
  fails UNAVAILABLE.
- **Not served:** GetTree, SplitBlob, SpliceBlob and the chunk-mapping calls.

The flags of such a frontend are the `[scheduler]` and `[executor]` keys of its `kbf.toml`, the
same as any frontend's, less `storage`: see
[jobs-on-jobscheduler-and-tierstore.md](../design/jobs-on-jobscheduler-and-tierstore.md).
Neither the rows of `action_result` nor the `cas/l/` objects have a collector yet.

### The one-port proxy

`kbf-proxy` is the public front door of a frontend host. It terminates TLS on one port,
checks the client certificate when one is presented, and routes each HTTP/2 stream (not
each connection) to a frontend of its pool on loopback, telling the frontend who the
caller is. REAPI, worker sessions, the operator API and enrollment all arrive on that one
port; a path the proxy does not know is answered by the proxy and never forwarded. The
proxy exists so that a frontend can be updated without failing a build: the frontends of
its pool (`[[proxy.frontend]]` tables of the `[proxy]` table) run on their own, and an
update drains one at a time through the proxy's admin API. The proxy starts, adopts and
swaps no frontend: the two-slot supervision it once had was removed, and a `[proxy]` table
with the `upstream` table is refused. It fronts `kbf --mode=frontend` only. The update of a pool is described in
[kbf-updater.md](kbf-updater.md).

To run several frontends behind a load balancer of your own instead (Envoy, with a
compose example), see [frontends-behind-a-load-balancer.md](frontends-behind-a-load-balancer.md).

### The installer's role

`kbf-installer setup --mode=<mode>` makes a host ready for one mode: the accounts and
directories, `/etc/kbf/installer.toml` and `/etc/kbf/kbf.toml`, the first operator token on
a frontend host, the state database's schema, and the mode's units. A
`storage` host is given the frontend's URL and an
enrollment token and enrolls itself at its first start. A `frontend` host
does not enroll; its certificates and addresses are the operator's, and `setup` takes them as
flags. A host from the packages can run the same verb as `kbf setup --mode=<mode>` (without the
proxy's flags). Details: [kbf-installer-setup.md](kbf-installer-setup.md). A frontend can also run as
a container image, with no installer: [containers.md](containers.md).

## Ports and who dials whom

The defaults, from the flags themselves. A listener whose default is loopback is for one
host; a farm sets routable addresses.

| listener | flag | default | modes |
|---|---|---|---|
| REAPI | `--listen` | `127.0.0.1:8980` | `frontend` |
| worker listener | `--worker-listen` | off unless given (a frontend with no storage nodes serves none; give it to serve storage nodes) | `frontend` |
| operator API | `--api-listen` | off unless given | `frontend` |
| enrollment | `--enroll-listen` | off unless given | `frontend` |
| job-facing read | `--job-listen` | off unless given | `frontend` |
| storage service | `--storage-listen` | `127.0.0.1:8982` | `storage` |
| storage metrics | `--metrics-listen` | off unless given | `storage` |
| the proxy's public port | `ListenStream=` of `kbf-proxy.socket` | `8980` | proxy hosts |
| the proxy's frontends | `base_port` of each `[[proxy.frontend]]` table | none | proxy hosts |

Some defaults come from tools around the binary, not from `kbf`:

- `kbf-installer setup` writes `api-listen = 127.0.0.1:8982` into `kbf.toml` of a frontend
  host when it is not given. `kbf cluster --api` defaults to the same address.
- The units that `kbf-deploy` renders give a storage node port 8983 (`KBF_STORAGE_PORT`),
  because 8982 is the operator API's on a frontend host; the enrolled node takes its storage
  port from the frontend's configuration. The renderer's REAPI, worker and operator
  ports are 8980, 8981 and 8982.
- Behind the proxy, a frontend of the pool listens on its `base_port` (REAPI),
  `base_port + 1` (worker), `+ 2` (operator API), `+ 3` (admin calls, which a frontend no
  longer serves) and `+ 4` (enrollment); the `base_port`s of two frontends are at least five
  apart. `--instance` no longer moves a frontend's listeners: the flag is accepted and
  ignored.

A listener serves plain text unless its TLS flags are given. The REAPI and worker listeners
accept plain text on any address, and off loopback the start line carries a warning. A
storage node refuses plain text off loopback.

### Who dials whom

| caller | callee | port | how it is secured |
|---|---|---|---|
| a REAPI client (Buck2) | the frontend, or the proxy | `--listen`, or the proxy's public port | TLS with the frontend's REAPI certificate; the REAPI authentication policy (`--reapi-auth-policy`) decides who may call |
| a storage node | the frontend's worker listener (`--frontend`) | `--worker-listen` | mutual TLS; the certificate names the role `storage`, with names that cover the host of `--advertise-addr` |
| the frontend | the storage nodes | each member's registered address | mutual TLS; the frontend's certificate names the role `frontend` |
| a storage node | its peers | each peer's registered address | mutual TLS; copies a write to the blob's second owner |
| an operator or `kbf cluster` | the operator API | `--api-listen` | a write must come from a loopback peer (or through the proxy, with its secret) and carry the operator token as a bearer token; reads are open to whoever reaches the address |
| a new node | the enrollment listener | `--enroll-listen` | TLS that authenticates the server only; the node pins the CA from its token |
| a storage node | the cold store | the object store's endpoint | the S3 key pair, below |

Three things follow.

- Clients never talk to storage; the frontend does that for them. Storage nodes
  never talk to clients.
- A storage node dials the frontend, and the frontend dials the node back at the address it
  registered. That address is `--advertise-addr`, or the address the node bound, which must
  then not be a wildcard.
- The operator API is plain HTTP/JSON under `/v1`. With a proxy it is reached through the
  public port. Without one, bind `--api-listen` where only operators can reach it, and run
  `kbf cluster` on the frontend's host: a write from another host is refused (403).

### The cold store and its keys

The cold store is the durable home of every blob: a `file://` directory on one host, or an
`s3://` bucket for a farm.

- **Registration.** The operator registers the farm's store with the frontend
  (`/v1/object-stores/...`, or `kbf cluster object-store`). The routes exist when the
  frontend runs with `--object-store-secrets-dir`, which needs `--api-listen`,
  `--api-token-file` and `--state-store`. The endpoint, bucket and prefix are kept in the state
  store. The secret key is kept only in that directory, one file of mode 0600 per
  connection, and it never enters the state store, a backup of it, or `kbf.toml`. The
  endpoint must be `http://`: an `https://` endpoint is refused, because the S3 client does
  not speak TLS yet.
- **A node gets the keys by asking.** An enrolled storage node's `NodeConfig` carries the
  store's URI (`cold_store`) and never a key. After it registers with the frontend, the node
  calls `ColdStore.GetStoreConfig` on the worker listener, with its storage certificate, and
  opens its cache on the answer. It holds the key in memory only, with core dumps off. The
  registered store must be the store the node's `--cold-store` names (endpoint, bucket and
  prefix), or the node keeps its own and reports `identity_mismatch`. The keys are probed
  before the cache opens on them. `kbf node` passes `--cold-store registered` instead of that
  URI, so the store is whatever the frontend has registered at each start: the node waits for a
  registration, runs on its endpoint, bucket, prefix and keys, and never falls back. Moving the
  farm to another endpoint is a new registration and a restart of each node. With `registered`
  `--cold-store-credentials-file` is refused and the `AWS_*` variables are not read.
- **A rotation reaches a node at its next start.** There is no hot swap; until then the
  frontend shows the node as `pending`.
- **A node's own key pair is a file.** `--cold-store-credentials-file PATH` names a JSON file,
  `{"access_key_id": "...", "secret_access_key": "..."}`, that the server's user owns with mode
  0600 or 0400; a symbolic link or any other file is refused, at start, with exit 2, naming the
  flag and the path and never the content. The same flag and file format serve `storage` and
  `kbf-server` with an `s3://` `--cold-store`; a `file://` cold store refuses
  it. `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` still work for one more release when no
  file is named, and the start logs one warning that says they are deprecated (never a value).
- **A fallback.** A node that has a key pair of its own (the file, or `AWS_ACCESS_KEY_ID` and
  `AWS_SECRET_ACCESS_KEY`) gives up on the frontend after four tries and starts on those. A node without them
  waits for the frontend, as it waits for the registration.
- **History's own key pair.** `--history-credentials-file PATH` (`history-credentials-file` in
  `[history]` of `kbf.toml`) names a file of the same form and rules as `--cold-store-credentials-file`
  for an `s3://` `--history-objects`, so a Docker Swarm secret can feed it. Given, the `AWS_*` variables
  are not read and a bad file is refused at start; left out, they are read as before.
- **Who never holds a key.** A frontend serves no blob itself and holds no store and no credential.

The units that `kbf-deploy` renders for `storage` still give the key
pair through `EnvironmentFile=<KBF_ETC>/s3.env`; see [kbf-units.md](kbf-units.md).

## When to choose which

### One host

A development box, a trial, or a small team with one host runs the three programs it needs on
it: a `frontend`, a `storage` node (a `file://` cold store) and the job service whose agents run
the actions. It is more to run than the single process it replaces, and its limits are those of
one host:

- **No replication.** There is one storage member, so there is one copy of each blob; the
  replication factor is 2 but is clamped to the number of members. Losing the host's disk
  loses the cache and the action cache.
- **No high availability.** The host is the frontend, the storage and the builder. A
  restart of the host restarts all three.

### Distributed: a farm

Choose a farm for more than one build host, for any need to survive the loss of a host, or
to keep the state store on a host that runs no builds. A farm is one frontend (alone or behind
the proxy), storage nodes (`storage`) and the job service with its agents, which run the actions.

- **Replication.** The map places each blob on `r` owners, and `r` is 2. It is not a flag.
  A write goes to the primary and is copied to the second owner. With one storage member the
  map has one owner.
- **A member that is down.** A member's row lives `--member-ttl-secs` (15 by default, at least
  3) after its last heartbeat; nodes heartbeat at a third of that. Then the member is
  `down` in the map and writes go to the other owner. It stays in the map for
  `--member-dwell-secs` (300 by default); a member that registers again in that time takes its
  place back. After the dwell it **leaves** the map and its digests go to other owners.
  Both flags need `--state-store`.
- **A member that is restarted on purpose.** Waiting for the TTL costs a connection attempt
  per write and refuses action-cache commits. Instead, before the restart, run
  `kbf cluster member <id> --state planned-down [--for-secs N] --token-file F`
  (`POST /v1/members/{id}` with `{"state": "planned_down"}`). The member goes `down` at once,
  one new map generation, and the dwell is counted from that call. `--for-secs` defaults to
  900 and is at most 86400; a heartbeat after it has passed returns the member to `active`, so
  a restart that never happens strands nothing. The member registering again after the
  restart, or `--state active`, undoes it. `GET /v1/storage` shows `"planned_down": true`.
- **A draining member.** `--state draining` takes a member out of the write path without
  taking it `down`; it is read first. The state of every member is in `GET /v1/members`.
- **A frontend is one process; several may share a store.** Frontends over one Postgres
  store share it and none fences another (see
  [state-store-cutover.md](state-store-cutover.md)); a SQLite file is held by one process, so
  only one frontend can use it. A farm with one frontend schedules nothing while it is down;
  with several behind the proxy, the others carry on, and a frontend is updated by a roll
  ([kbf-updater.md](kbf-updater.md)).


Storage hosts, their disks and memory are in [storage-node.md](storage-node.md). Hosts that run
the actions are the job service's, not kbf's.

## Growing from one host to many

There is no verb that makes a farm from one host. You set up the farm's parts in this order.
What the code supports is below.

1. **The state store.** A frontend has no default and refuses to start without `--state-store`.
   `sqlite:///absolute/path` is accepted for this release only: a start on it works and logs a
   warning that SQLite is removed in the next release (see the upgrade notes). A farm's frontend uses Postgres:
   `kbf-installer setup` with no `--database` provisions the role and database in one run
   (the database `kbf` and the role `kbf-server` on the local socket; the schema follows once
   the server admits the logins, which on a stock server means a second run after the printed
   lines are added), prints the `pg_hba.conf` and `pg_ident.conf` lines the logins need,
   refuses with the command that installs Postgres when `psql` or the server is missing, and
   never edits either file; a role you made by hand is passed with `--database`. Postgres must
   flush commits before it acknowledges them (`synchronous_commit` on) or the start is
   refused. A URL with a password is refused: the command line is readable by every user on
   the host, so the role is reached by the socket and peer authentication. The state store
   also holds the storage members, the enrollment registry and the object-store registration.
2. **The CA and certificates.** A farm needs one CA and one certificate per node, each with
   exactly one role (`frontend` or `storage`, as `urn:kbf:role:<role>`; `urn:kbf:role:daemon`
   is still read and no node of this release holds it). The frontend runs the enrollment
   listener: `--enroll-listen`, `--ca-dir`, `--frontend-name` (repeatable) and `--ca-signer`
   (the socket of the `kbf-signer` process that holds the CA key; `--ca-dev-file-signer` is
   for development; a frontend that runs as another account than the signer needs the
   signer's `--client-group`, see the signer's socket in [api.md](../api.md)). The operator makes a token with `POST /v1/enroll/tokens`, and a node
   enrolls with it at its first start and renews at half of its certificate's life (7 days by
   default, `--enroll-cert-lifetime-secs`). A node host needs no hand-issued certificate. The
   table of what each certificate must contain is in [kbf-units.md](kbf-units.md).
3. **The cold store.** `file://` stays on one host only. Choose an `s3://` store every
   storage node can reach, register it with the frontend as above, and give each node the
   same `--cold-store` URI. A fresh prefix is a fresh cold store.
4. **The frontend.** Run `kbf --mode=frontend` on its own host, or behind the proxy (a pool of
   frontends given as `[[proxy.frontend]]` tables, written by hand: `kbf-installer setup` sets up
   no proxy). Bind `--listen` and `--worker-listen`
   to addresses the nodes reach, give them the TLS flags, and set `--storage` (required) to
   the first storage node, with `--storage-client-cert`, `--storage-client-key` and
   `--storage-ca` for the mutual TLS to it. Add `--api-listen` and `--api-token-file` for
   operators.
5. **The storage nodes.** On each host: `kbf-installer setup --mode=storage`
   with `--frontend-url` and `--token-file`. The node takes its flags from `kbf.toml` and the
   frontend's configuration: `--frontend` (the worker endpoint), `--advertise-addr`,
   `--storage-listen` on all addresses at the storage port, the three certificate flags,
   `--hot-store` and `--meta-dir`. `kbf --mode=storage --check`
   checks the host and exits.
6. **Promote the new members.** In a farm formed more than a minute ago, a node it has not
   seen before registers as `joining` and stays so until `kbf cluster member <id> --state
   active`, unless the frontend runs with `--auto-promote`. Promoting does not copy any blob
   to the member. Read the risk in the operator API's section on `POST /v1/members/{id}` in
   `docs/api.md` before you promote a joiner whose slice is not empty.


What a frontend takes beyond the single-host case: `--storage`, `--storage-ca` and its client
pair, `--enroll-listen` and its companions, and `--object-store-secrets-dir`; it has no
flag of a store. The storage half takes `--frontend`, `--advertise-addr`, `--storage-zone` and
`--storage-weight`.

**Planned:** copying a joining member's slice to it
(the join handoff, architecture-v2.md section 8.4) is not built.

## Stopping and draining

| mode | on SIGTERM or SIGINT | what bounds it |
|---|---|---|
| `frontend` | drains, by the sequence below | `--drain-lb-wait` (10 s by default); nothing else by default: the drain waits for every open execution, and `--drain-deadline` (none by default) only reports; to force a stop, a second signal or `--shutdown-timeout-secs` (0 by default: none) |
| `storage` | stops | none of its own |

**The frontend.** Buck2 does not recover an Execute stream that breaks after it started, so
a stop that cuts a stream fails its action. On the first signal, or on
`POST /v1/frontend/drain` (`kbf cluster drain --frontend --token-file F`), the frontend
(which has a state store) takes itself out of any load balancer in five steps
([kbf-units.md](kbf-units.md#draining-a-frontend-behind-a-load-balancer)): `/readyz` turns to
503 while everything is still served; after `--drain-lb-wait` it refuses each new Execute with
`UNAVAILABLE` (which Buck2 retries), takes no more work from the state store, and sends
GOAWAY on the REAPI, worker and operator API listeners; it keeps every open Execute and
WaitExecution stream until its action is done, however long that takes, except a stream
whose operation was never started, that no worker on this frontend can run and that no
other frontend can take, which ends `UNAVAILABLE` at the end of the load balancer wait;
then it ends the worker streams and exits 0. There is no default deadline. A
`--drain-deadline` that is set (no upper bound) only reports when it passes with executions
open: a WARN with the count, and `deadline_passed` in `GET /v1/frontend/drain`; no stream is
ended, the process stays up, and the drain keeps waiting, whether a signal or the endpoint
began it. `GET /v1/frontend/drain` reports the phase. The way to force a stop is a second
signal, which skips the waits, or `--shutdown-timeout-secs` running out on a drain a
signal began; either ends the streams that are left `UNAVAILABLE`, and their builds fail. A frontend with no state store (a test setup) drains at once: `/readyz`
503, new Execute calls refused, GOAWAY on the REAPI listener, and exit 0 once the REAPI
connections have closed. Under systemd that needs `TimeoutStopSec=infinity`; the units that
`kbf-deploy` renders set it.

**`storage`.** A storage node serves until the signal and stops.

A stop of a storage member that is not planned makes the frontend wait for the TTL before it
marks the member `down`; use `planned-down` first, as above.

## Related pages

- [kbf-units.md](kbf-units.md): each mode's packaged systemd unit and how it is checked.
- [kbf-installer-setup.md](kbf-installer-setup.md): `kbf-installer setup`, the state database,
  the proxy host and backup.
- [update-a-host.md](update-a-host.md): updating a host.
- [storage-node.md](storage-node.md): storage hosts, disks, memory and the cold store on a
  single host.

- [pack-adopt.md](pack-adopt.md): indexing the segments the storage members already hold into
  the pack index, in place.
- [job-service-cutover.md](job-service-cutover.md): moving a frontend's executor onto the
  jobscheduler's job service: what must run first, kbf's client token, and
  `kbf-deploy job-service check`, `plan` and `apply` (with its canary). Not the
  frontend's own `--job-listen`, which is its job-facing read listener.
- The operator API, with its gates, in [api.md](../api.md).
