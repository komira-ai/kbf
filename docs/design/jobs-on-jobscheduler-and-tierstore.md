# kbf jobs on jobscheduler and tierstore: read-only trees in, write-through objects out

Status: **DESIGN, for the owner's review. Nothing here is built.** Every number in section 4 is an estimate from docs,
to be replaced by the benchmark gate. It supersedes four parts of [actions-as-jobs.md](actions-as-jobs.md): the output
call `put-ca` (its 5.2), the tierstore write grants (5.2, 13), the loopback port key `executor-node-store-port` and the
passed-through tierstore port (3.4, 3.6, 8.3), and the Linux nodes keeping kbf's own storage (`kbf storage`, 7.1 and
item 20); the top of that document says so. Section 11 lists the pieces that belong to tierstore and jobscheduler.

Written against kbf `main` at `fa42f0fda`, jobscheduler `main` at `e7350fb5` (kbf's pin is `0c104a1d`, section 3),
and tierstore `main` at `341792ce`. A path in parentheses is where a statement was checked; a path of another
repository is named with that repository.

## 1. Decision summary

- One action is one jobscheduler job. The kbf frontend submits it. The job runs the build rule on private scratch.
- tierstore is an object-store proxy with one generic filesystem interface on each node:
  - `<mp>/trees/<sha256>`: a read-only tree. A tree is a root manifest plus one manifest per directory, each named by
    its SHA-256; an entry of a file gives its sha256, its size, whether it is executable, and optionally a source
    (object key, offset) (tierstore `crates/tierstore/docs/tree-manifest.md`). Each file is a ranged, hash-verified
    read of an object through the node cache. tierstore never learns that a manifest describes kbf packs. A tree
    resolves on lookup.
  - `<mp>/put/<key prefix>/`: a writable "put directory". A file written there and committed by `rename` becomes an
    object at that key. The upstream create-only PUT, with a checksum, finishes before `rename` returns.
- Inputs: lazy reads through `trees/`. Hydrate prefetches at placement and is only a speedup.
- Outputs: kbf-task packs the outputs inside the job, in kbf's pack format. It writes the pack into the put directory
  and commits it. tierstore does not parse, hash for kbf, or pack anything.
- jobscheduler gets no change to its code on `main`. It sees a read-only host-path layer under a writable scratch
  layer, and a read-write host-path mount, under `--mount-root` (kbf moves its pin, section 3).
- Durability: ack only after the upstream PUT completes. No write-behind in v1.
- The frontend reads only the pack footer and record 0 (the TaskManifest). It does not re-read every byte.

## 2. Data flow for one action

**Before submit (frontend).**
1. TreeWriter writes the input tree manifests as create-only objects, memoised (`crates/kbf-front/src/action_tree.rs`;
   nothing calls it yet). The root is the base tree, `work`, and `.kbf/{action,command}`.
2. The frontend mints one key set per lease attempt: the prefix `cas/b/<build>/<nonce>/`. The names under it are
   `pack` and `L0..L3`, and, for an action that needs more, `pack1..pack7` and `L4..L7`: the bounds
   of today's plan, up to 8 packs and 8 large objects, `pack` (today's pack 0, which holds the manifest) committed
   last (`crates/kbf-task/src/plan.rs`). Today's nonce is a 16-byte HMAC-SHA-256 over the lease, the action, the mint minute and the
   slot, followed by the minute, and each key is `<nonce>-<k>` with its own MAC
   (`crates/kbf-front/src/proxy/packed/commit/untrusted/mint.rs`); one prefix for the attempt needs one MAC that
   covers no slot, a change to that mint and to `kbf_pack::mac_input`.
3. Job spec: network level none; no host ports; no secret files. Today's job also has no host ports, and differs in the other two: the operator's
   network level (default allow-list) and `files` holding the read grant and the PUT URLs
   (`crates/kbf-server/src/executor/submit.rs`).
   - Linux: the job's root is one read-only layer, the host path `<mp>/trees/<digest>` (`JobSpec.layers`), under the
     job's writable scratch, so the tree's `work` is `/work` and its `.kbf` is `/.kbf`, where `kbf-task --tree /`
     reads them (`crates/kbf-task/src/tree.rs`; actions-as-jobs 3.1). The output mount is
     `<mp>/put/cas/b/<build>/<nonce>` at `/out`, READ_WRITE, `no_exec`.
   - Mac: `/work` is built by kbf-task (section 6). The output mount is the same host path at `/work/.kbf-out`.

**Placement.** The first GetJob answer naming `host_id` triggers `POST /_tierstore/v1/hydrate` (hold = job id). The
endpoint is tierstore's as it is (body `{"hold", "expires_ms", "trees"}`, tierstore
`crates/tierstore/src/server/hydrate.rs`); the kbf caller is actions-as-jobs 4.5 and is not built. The agent resolves
both host paths with `openat2` beneath its mount-root descriptors. Each lookup reaches tierstore fs:
- `trees/<digest>`: if it is not attached, fs fetches `cas/l/<digest>` through the fill socket (hash-verified, at
  most 64 MiB), parses it, and attaches it implicitly. Detach is by idle timeout. Both are new: today a tree appears
  only after the control verb `attach <sha256> for <run>` and leaves by `detach`
  (tierstore `crates/tierstore/src/server/treefs/control.rs`).
- `put/cas/b/<build>/<nonce>`: virtual directories under an allowed prefix exist implicitly and hold no state until a
  file is created.

**Unpack, run.** kbf-task `--tree` reads `/.kbf/action` and `/.kbf/command` and checks the digests. Input files fill
lazily on first open: a cache hit is local, a miss is a ranged fill. The Command runs in `/work`. Writes go to the
overlay upper on local scratch, so rename, rm -rf and mmap never touch tierstore.

**Pack.**
- Pass 1: walk the declared outputs (kbf-outputs, openat2/O_NOFOLLOW), then hash and classify each blob. Blobs up to
  the record cap (64 MiB) go in the pack. Larger ones become `L<i>` objects (`crates/kbf-task/src/plan.rs`,
  `crates/kbf-task-pack/src/task_pack.rs`).
- Compute the pack layout from sizes.
- Pass 2: write `pack.part` and `L<i>.part` into `/out`, hashing the whole object as bytes go out.

**Commit, per object, large outputs first, `pack` last.**
1. `rename("L0.part", "L0.sha256-<hex>.part")`: a local name change that records the expected digest. No upload.
2. `rename("L0.sha256-<hex>.part", "L0")`: the commit.
3. The same two steps for `pack`.
4. kbf-task waits on each commit `rename`. The call returns only after the upstream ack. On success it records the
   phase timings (section 4) in the TaskManifest, which is record 0 of `pack`. The manifest is written before the
   commit.
5. kbf-task exits 0 if every commit succeeded. The Command's exit code is data in the TaskManifest. That is today's
   contract (`crates/kbf-task/src/task.rs`, exit code 0: "the action ran (whatever its exit code ...) and its pack
   was stored"), and so is the promotion rule, which this design keeps: job SUCCEEDED, reason EXIT, exit 0
   (`crates/kbf-server/src/executor/outcome.rs`), pack present, TaskManifest verified, and the exit code taken from
   the manifest (`crates/kbf-front/src/proxy/packed/commit/untrusted/task.rs`).

**Frontend result.**
1. On SUCCEEDED/EXIT/0 the frontend reads `pack` by a suffix range (footer) and a prefix range (record 0), in
   parallel.
2. It checks:
   - the footer shape;
   - that the manifest matches the lease, the action, and the Command's declared outputs;
   - one HEAD per `L<i>` for size.
3. It builds the ActionResult, records pack-index locations, and writes the action cache (only if the Command exit
   code is 0 and `do_not_cache` is false).
4. It calls `release` for the hydrate hold.

## 3. Interfaces

### tierstore

**Put directory semantics (fs process, Linux FUSE and Mac NFSv3 share one core).**

| Operation | Result |
|---|---|
| `mkdir`, `create` O_EXCL, `write`, `flush`, `release`, `fsync` (no-op), `statfs`, `getattr` | allowed, under an allowed prefix only |
| `rename(src, dst)` | the commit; `src` must end `.part`, `dst` must not, same directory |
| `unlink` of a `.part` file | allowed (abort) |
| `link`, `symlink`, `mknod`, `truncate` other than at create, xattr, `rename` of anything else | EPERM or ENOTSUP |
| `open` for read of a committed name | EACCES in v1 |

- Writes go to a staged file in `fs.put.stage_dir`, which must be on the cache volume (same `st_dev`, checked at
  start). Staged files are named by an internal id, not the key, so case-folding filesystems cannot merge two keys.
- A SHA-256 context runs inline for sequential writes. Any other write pattern marks the digest unknown, and it is
  computed at commit.
- A `.part` file named `<dst>.sha256-<64 hex>.part` carries an expected digest. Without it, no expected digest is
  checked.

**Config keys** (in the `[fs]` table of the file `--config` names, where tierstore keeps its settings; they are not
flags, tierstore `crates/tierstore/src/server/treefs/config.rs` and `crates/tierstore/src/server/cli.rs`):
- `fs.put.enabled` (default false)
- `fs.put.prefixes` (e.g. `["cas/b/"]`)
- `fs.put.stage_dir`
- `fs.put.max_object_bytes`, `fs.put.max_dir_bytes`, `fs.put.max_dir_files`, `fs.put.min_free_bytes`
- `fs.put.commit_deadline_secs` (default 30; must be below the NFS client retransmit limit)
- `fs.put.stage_ttl_secs` (default 43200, and must exceed the longest job wall limit plus 900 s)
- `fs.put.committed_ttl_secs` (default 600; visibility only)
- `fs.put.cache_committed` (default true: the uploaded object is kept in the node cache under its key, so a later
  ranged fill of a span is a local read)
- `fs.tree_idle_secs`, `fs.tree_manifest_prefix` (default `cas/l/`, today a constant of the cache, tierstore
  `crates/tierstore/src/server/local_cache/layout.rs`)

**Fill-socket verb (fs process to server process, existing socket):**
`put <key> <sha256> <size> from <staged path>`
- Reply `{"ok":true,"created":true}` when the object was created.
- Reply `{"ok":true,"created":false}` when the key already exists with the same sha256.
- Reply `{"ok":false,"error":"exists_different|checksum|toolarge|upstream"}`.
- The fill socket's verbs today (`fill`, `touch`, `pin`, `unpin`, `pins-live`) answer in plain words (`ok ...`,
  `refused <reason>`), not JSON (tierstore `crates/tierstore/src/server/fill_socket/`); this verb's JSON reply is new.
- The server does a create-only PUT through the cache (`put_through`, `If-None-Match: *`, tierstore
  `crates/tierstore/src/server/local_cache/keep.rs`, `crates/tierstore/src/backend.rs`) with
  `x-amz-checksum-sha256`; nothing in tierstore sends that header today, so it is new. Multipart is used above the
  existing threshold (`server.s3_single_put_max`, default 5 GiB, in parts of `server.s3_part_bytes`, default 16 MiB,
  tierstore `crates/tierstore/docs/configuration.md`), with part checksums, also new. After a 412 it HEADs and
  compares the stored sha256.

**Errno mapping at `rename`:** 0 on created or same-sha256 exists; EEXIST on exists_different; EBADMSG on digest
mismatch (against the name's expected digest or the daemon's own); EFBIG on toolarge; EIO on upstream failure after
the deadline; EBUSY if a FUSE handle is still open on the source.

**Other tierstore changes:**
- Trees resolve on lookup (above). The `attach` control verb stays for operations.
- Metrics: `tierstore_put_commit_seconds{phase=hash|upload}`, `tierstore_put_result_total{result}`,
  `tierstore_put_stage_bytes`, `tierstore_put_swept_total`.
- Docs: a "The put directory" section in `crates/tierstore/docs/operations.md`.
- Unchanged: the hydrate/release API; the read-only tree, which answers EROFS or NFS3ERR_ROFS on every write.

### jobscheduler (generic only)

No proto change and no new flag. It relies on features built on jobscheduler `main` for Linux `crun` (items 2 to 6 of
its `docs/design/job-rootfs.md`): kbf's pin, `0c104a1d`, has no `layers` or `mounts` in its `job.proto`, so kbf moves
its pin first. The macOS side (its item 7) is designed and not built.
- Agent `--mount-root put=<mp>/put` (rw) and `--mount-root-ro trees=<mp>/trees` (jobscheduler
  `crates/js-agent/src/cli.rs`).
- `JobSpec.layers`, `Mount{host_path, target, mode, sub_path, no_exec}` (jobscheduler
  `crates/js-proto/proto/jobscheduler/v1/job.proto`). Linux's root is one read-only layer from a host path, its
  writes the job's own. The output mount is `READ_WRITE`.
- Network level none.
- A FUSE source with `allow_other`, and `openat2` without `RESOLVE_NO_XDEV`.

Requirement for job-rootfs build item 8 (an end-to-end run on one Linux node, a runbook entry under jobscheduler's
`docs/runbooks/` and not a test of the PR set): include a read-write FUSE mount in the run.

### kbf

- kbf-task:
  - `--out-dir <path>` (default `/out`; the Mac platform profile sets `/work/.kbf-out`).
  - `--stage-and-commit`: the two-step rename above.
  - Phase timings in the TaskManifest: `walk`, `hash`, `write`, `commit`, beside today's advisory `fetch_ms`,
    `run_ms` and `pack_ms` (`crates/kbf-task-pack/src/task_manifest.rs`).
  - The exit-code rule from section 2.
  - It replaces `--pack-url-file` and `put.rs` after the cutover.
- kbf-task-pack: `PackLayout::plan(&[BlobMeta])` and `write_to`, with a byte-equality test against the existing
  writer (`write_pack_with`, `crates/kbf-task-pack/src/task_pack.rs`), so the pack and manifest readers (the
  manifest readers last changed in komira-ai/kbf#2294) are unchanged.
- kbf-front:
  - `--task-out-mode presigned|put-dir` (default presigned until the flip).
  - `--task-out-root <mp>/put`.
  - Key minting changes from `<nonce>-<k>` to `<nonce>/<name>`, keeping the mint minute in the nonce for the
    janitor; the ingest's MAC check and the janitor, which reads a key's age from its `-<k>` or `-L<i>` name
    (`crates/kbf-server/src/frontend/janitor.rs`), learn the new layout.
  - A reserved-name check: an input root with a child `.kbf-out` is rejected (today's reserved names are `work`,
    `.kbf` and `.kbf-run`, `crates/kbf-front/src/action_tree/base.rs`).
  - A new `ingest_put_dir` replaces `ingest_set`: it reads footer and record 0 only.
  - Pack-index rule: first recorded location for a digest wins, as the index already does
    (`crates/kbf-pack-index/src/lib.rs`). New: a failed hash-verified fill drops that location.
- kbf-pack-index, the action cache (owner decision, 2026-11-13: no kbf storage node holds it):
  - `ActionStore` (`crates/kbf-pack-index/src/action.rs`): `get_action(action digest)` and
    `put_action(action digest, encoded ActionResult, packs)`. A put replaces (last writer wins) and records the
    packs of the action's outputs in the same transaction, all or nothing, so a result is never readable while a
    blob it names has no recorded pack. The store holds the encoded bytes and does not read them. A result of no bytes
    is valid. The bound is 1 MiB (`MAX_RESULT_BYTES`), as `MAX_BATCH` bounds a batch.
  - Postgres: migration 3, `action_cache` (`migrations/0003_action_cache.sql`), one new table `action_result`, one row
    per action digest with the result inline, no foreign key, outside the commit head. A database at migration 2
    keeps working for the pack index, and the action calls on it say `NotMigrated`. A cold cache is a valid cache:
    nothing is imported from the old storage nodes. Memory: `MemoryPackIndex` implements the same trait. Both run the
    conformance suite `action_conformance` (A1 to A9).
  - Not in this change: callers (the frontend's action-cache reads and writes), retention of rows, and a row's
    touch time.

## 4. Per-action overhead budget (estimates, ms)

What this design adds to an action with about 5 outputs and about 200 KB, compared with no output handling:

| Step | ms |
|---|---|
| Two extra mounts in the agent (`open_tree`, `mount_setattr`, `move_mount`, bind) | 0.3 to 1 |
| `trees/` lookup, manifest cached on node (miss: one fill, 3 to 8) | 0.2 |
| Walk + hash outputs (0.5 ms/MB with SHA-NI) | 0.3 to 0.7 |
| Create + write + close of `pack.part` | 0.1 to 0.3 |
| Digest rename | 0.05 |
| Commit rename: staged hash (inline, near 0), `put` verb, upstream create-only PUT with checksum | 3 to 8 |
| Frontend: suffix and prefix ranged GETs in parallel | 1 to 2 |

- Total about 5 to 12 ms.
- The upstream PUT dominates and is the cost of "ack after upstream".
- Large outputs add a memory-bandwidth copy into the staged file: about 0.5 to 1 ms/MB.
- An action with many outputs stays one pack and one PUT while they fit one pack: 128 MiB and 16,384 records
  (`MAX_PACK_BYTES`, `MAX_PACK_RECORDS`, `crates/kbf-task-pack/src/task_pack.rs`). So output count does not multiply
  PUTs below that. Past it, the records spread over `pack1..pack7` as today's plan does (section 2), one PUT each.
- Gate: kbf-task and tierstore report the phase timings in the results. Tests assert that the fields exist, never
  their values. The benchmark includes a 1-output action, a 1000-output action, and a 500 MB output (a single PUT
  under tierstore's default `server.s3_single_put_max`).
- Removed against today's path: HEAD of the pack and the whole-pack read and per-record hashing in the frontend.

## 5. Failure, crash, partial writes, GC

**Commit point.** The `rename` return is the only commit.
- Before the upstream PUT completes, nothing exists upstream.
- The PUT is atomic (single PUT, or multipart that is invisible until complete), and the checksum is verified.
- No state can exist where an object is visible upstream but looks valid and is incomplete.
- The job waits on `rename`, not on `close` or `fsync`. FUSE `release` is asynchronous. NFSv3 has no close. `flush`
  is called once per descriptor, including dup'd and forked ones.

**Cases:**
- Job killed or crashed while writing: `.part` files remain in the stage and are never uploaded. The job is not
  SUCCEEDED, so no promotion.
- kbf-task exits 0 with a missing commit: not possible, because it checks every `rename`. If it happens anyway, the
  frontend's footer read answers 404 and the attempt is retried as an infrastructure failure.
- Upstream down: the daemon retries with jittered backoff until `fs.put.commit_deadline_secs`, then `rename` returns
  EIO. The staged file is kept. kbf-task may repeat the same commit `rename` while the job runs.
- Retry after an ack that was lost (daemon restart, reset): the staged bytes are identical, so the upstream answers
  412, the stored sha256 matches, and the result is created=false, success. A different sha256 gives EEXIST and fails
  the attempt.
- tierstore fs restart: staged files and a journal persist in `stage_dir`, and the mount is held by the existing
  handover. Without handover the job sees ENOTCONN or ESTALE, fails, and the attempt reruns with new keys.
- Job CANCELLED or LOST: a zombie may still write. Its keys belong to its own attempt only. The frontend ingests only
  the attempt whose result was SUCCEEDED/EXIT/0, so late bytes are orphans. Create-only is the fence, so no tombstones
  are needed. A rerun uses a new nonce.
- Frontend crash between ingest and action-cache write: ingest is idempotent and re-reads on restart. Otherwise the
  objects are orphans.
- Same-attempt determinism: pack contents are a pure function of the sorted outputs and a manifest written once in
  the job. Retries re-upload the same staged file, so membership never changes between tries.

**GC:**
- Stages: a sweep every 60 s removes `.part` files older than `fs.put.stage_ttl_secs`, then committed visibility
  entries after `committed_ttl_secs`. At startup, stage files with no journal entry are removed after 10 minutes.
- Quotas: ENOSPC on `create` when the volume is below `min_free_bytes`. EDQUOT at `max_dir_bytes` or `max_dir_files`.
- Cache: ordinary evictable entries, no pins.
- Upstream: kbf's existing `cas/b/` janitor, grace 6 h by default and at least 5 h (`DEFAULT_GRACE`, `MIN_GRACE`), keeps keys the pack index references
  (`crates/kbf-pack/src/sweep.rs`, `crates/kbf-server/src/frontend/janitor.rs`). Add: abort multipart uploads under
  `cas/b/` older than grace (the janitor deletes keys only today).

## 6. Mac

- One design. The same fs core serves NFSv3 on loopback (nfsserve). The mount carries `trees/` and `put/`; the server
  enforces read-only on `trees/`.
- Inputs (C9 of tierstore's `docs/design/virtual-build-fs.md`): kbf-task builds `/work` as real directories with one
  symlink per input file into a read-only mount of the tree. The tree is mounted below `/work` because the Mac agent,
  as designed (jobscheduler `docs/design/job-rootfs.md`, 5.1), allows mounts only at or below `/work`. A read-only or
  read-write mount exactly at `/work` is refused (`mount-target-unsupported-here`). Cost is about 40 us per file. An
  action that opens an input for writing fails; that limit already exists.
- Outputs: the host path `<mp>/put/cas/b/<build>/<nonce>` is mounted at `/work/.kbf-out` as a symlink in `exec/` with
  a Seatbelt write grant. `process-exec` is denied because the mount sets `no_exec`, as on Linux (job-rootfs 5.2 and
  5.4; designed, not built). The path name `.kbf-out` is reserved and checked.
- NFS has no open handle count. The commit therefore requires the expected digest in the source name. kbf-task always
  supplies it. A short or unflushed file fails with EBADMSG instead of being published.
- Unmeasured: whether close(2) flushes writes synchronously, whether a killed process's writes are flushed, and
  loopback write throughput. The commit-deadline default must be checked against the NFS retransmit limit.
- Out-of-order writes defeat the inline hash. The digest is then computed at commit, about 0.5 s/GB per core.

## 7. Security (who can write what)

- The job holds no credential, no network, and no tierstore address. It can write only under its own bind-mounted
  directory. It cannot list the parent directories. The nonce is not given to the job in any other form.
- tierstore uploads with its own upstream identity. Only prefixes in `fs.put.prefixes` are writable. Keys are
  create-only, so a job cannot overwrite an existing object, including one from another attempt.
- Reachability: the host path is reachable only through a mount chosen by the trusted frontend. The agent admits
  mounts only under `--mount-root`. Residual risk: any submitter that can name a host path under the put root could
  write there. Do not share the jobscheduler with untrusted submitters.
- Daemon hardening (root process, job-controlled names): `openat2` with `RESOLVE_BENEATH|NO_SYMLINKS` on the stage,
  UTF-8 names up to 255 bytes, no `.` or `..`, no symlinks created, setuid/setgid stripped, a depth cap.
- Integrity: tierstore verifies the staged bytes against the expected digest and against its own digest, and the
  upstream verifies `x-amz-checksum-sha256`. Readers verify sha256 on every fill.
- Consequence of the frontend not re-reading every record: a hostile job can name any digest in its own pack's
  manifest, including a digest of a popular blob. The frontend keeps the first recorded location per digest and drops
  a location after a failed fill. That bounds the effect to the hostile action's own outputs and one failed fill. It
  is weaker than today's full verification, which hashes every record of every pack
  (`crates/kbf-front/src/proxy/packed/commit/untrusted/task.rs`) (open question 1).

## 8. Build order (each PR small, independently mergeable)

**tierstore**
1. Staged-file store as a library: state machine, journal, quotas, inline hasher, sweep, crash-replay tests. No FUSE.
2. Fill-socket `put` verb and `Backend` create-only put with checksum, and 412 with matching sha256 as success.
   Integration test against the real upstream, including a mid-body cut and a second PUT. This PR proves the one
   unverified assumption: that the upstream verifies `x-amz-checksum-sha256`. If it does not, tierstore stores the
   digest as object metadata and compares on 412.
3. Linux FUSE `put/` frontend with the restricted profile and the errno mapping. Real-mount tests: writer killed,
   concurrent renames, daemon restart with handover.
4. Trees resolve on lookup: implicit attach by `fill <digest>`, idle detach.
5. NFS `put/` frontend for macOS on the same core.
6. Operations docs and metrics.

**jobscheduler**
1. Runbook only (item 8 of job-rootfs, a runbook entry under `docs/runbooks/`, not a test of the PR set): an
   end-to-end run on one Linux node with a read-write FUSE mount and a layer from a FUSE host path. Document that the
   tree's `fs.owner` must equal the host uid `sub_uid+1000` (LR5).
2. Optional generic ask for the Mac owners: mounts at any non-reserved target under `exec/`.

**kbf**
1. `PackLayout::plan` and `write_to`, with a byte-equality test.
2. kbf-task `--out-dir` sink: stage, digest rename, commit rename, exit-code rule, phase timings. Tests use a plain
   directory with a fake commit.
3. kbf-front: `--task-out-mode put-dir`, key layout, mount spec, `ingest_put_dir`, first-location-wins (the index's
   rule already) with drop-on-failed-fill,
   reserved-name check. Before it, kbf's jobscheduler pin moves to a release with `layers` and `mounts`.
4. One Linux node end to end with both modes. Benchmark gate with the phase timings.
5. Mac: `--materialize-work`, `/work/.kbf-out`, one Mac node.
6. Flip the default. Delete the presigned PUT path, `put.rs`, the pack read listener and `/locate`, as in
   actions-as-jobs 5.3 and 7.2. The janitor keeps the multipart abort it gained (section 5).

## 9. Rejected alternatives

**Loopback object-store API on a host port.** The job streams a presigned PUT to `127.0.0.1:P`. It works with no
tierstore change and no jobscheduler change, but the job needs a bearer credential and a network path, and it puts an
object-store protocol in the job. It also differs on Mac (host_port must equal container_port). The owner ruled that
job I/O is one filesystem. It stays as a fallback only.

**`put-ca` with write grants (actions-as-jobs 5.2).** tierstore would hash each blob and keep a `commit` object. That
puts kbf's packing knowledge and hashing in tierstore and needs a grant system. It conflicts with "tierstore never
packs or hashes for kbf".

**Seal/ingest verb: tierstore walks a job directory and packs it.** The seal verb needs the frontend to address a
node's tierstore and run a post-exit RPC. tierstore would also have to write kbf's pack format.

**Writable tree views or overlay upper on tierstore.** Overlay cannot use FUSE as upper. Views were dropped (C4, C9
of tierstore's `docs/design/virtual-build-fs.md`). rename onto an output gives EXDEV, and `rm -rf` of an output
directory gives EBUSY. Build rules need these on private scratch.

**close or fsync as the commit.** FUSE `release` is asynchronous and `flush` runs per descriptor. NFSv3 has no close.
`rename` is a single synchronous call on both.

**Write-behind or `durable=local`.** It saves about 3 to 8 ms per action, but an ActionResult could point at bytes
only on a dead node. The owner decided no write-behind in v1.

**Lookup-created stage plus per-run tombstones.** It needs state on lookup and a seal RPC. With create-only object
keys and virtual directories, neither is needed.

## 10. Open questions for the owner

1. Verification weakened: the frontend reads only the manifest, so a hostile job can register a bad location for a
   digest of a popular blob. The mitigation is first-location-wins plus drop-on-failed-fill. Is that acceptable for
   jobs that run only code we build, or should the frontend verify digests it has not seen before?
2. Exit-code contract: kbf-task exits 0 when the pack is committed and the Command's exit code lives in the
   TaskManifest, so a failing build can still return its stdout and stderr. That is today's contract and today's
   "SUCCEEDED + EXIT + 0" rule (section 2, step 5), kept unchanged for the put directory. Confirm?
3. Mac mount target: kbf uses `/work/.kbf-out` because the Mac agent allows mounts only at or below `/work`. Should we
   ask the jobscheduler owners to lift that restriction, or accept the reserved name?
4. Upstream checksum support: if Silo/MinIO does not verify `x-amz-checksum-sha256` on single and multipart PUT, are
   you fine with tierstore storing the digest as object metadata and verifying on HEAD (verification is then after the
   fact), instead of by the upstream on the PUT?

## 11. Cross-repository items

None of these is kbf code, and this design changes no code in those repositories. Each is a pull request in its own
repository against its own design, in the order of section 8.

**komira-ai/tierstore** (against [virtual-build-fs.md](https://github.com/komira-ai/tierstore/blob/341792ce/docs/design/virtual-build-fs.md)):

| Item | What | Section |
|---|---|---|
| TS1 | Staged-file store library | 3, 5, 8 item 1 |
| TS2 | Fill-socket `put` verb, create-only put with `x-amz-checksum-sha256`, 412 with matching sha256 as success | 3, 8 item 2 |
| TS3 | Linux FUSE `put/` frontend, errno mapping | 3, 8 item 3 |
| TS4 | Trees that resolve on lookup, idle detach (today: [`control.rs`](https://github.com/komira-ai/tierstore/blob/341792ce/crates/tierstore/src/server/treefs/control.rs)) | 2, 8 item 4 |
| TS5 | NFS `put/` frontend for macOS | 6, 8 item 5 |
| TS6 | "The put directory" in [`operations.md`](https://github.com/komira-ai/tierstore/blob/341792ce/crates/tierstore/docs/operations.md), the `tierstore_put_*` metrics | 3, 8 item 6 |

**komira-ai/jobscheduler** (against [job-rootfs.md](https://github.com/komira-ai/jobscheduler/blob/e7350fb5/docs/design/job-rootfs.md)):

| Item | What | Section |
|---|---|---|
| JS1 | job-rootfs item 8's runbook run with a read-write FUSE mount and a FUSE layer; `fs.owner` equal to `sub_uid+1000` (LR5) documented | 3, 8 item 1 |
| JS2 | Optional: Mac mounts at any non-reserved target under `exec/` | 6, 8 item 2, 10 question 3 |
| JS3 | A release of layers and mounts (job-rootfs items 2 to 6, built on `main`) that kbf's pin can move to | 3 |
