# Fast per-action sandboxes: shared read-only trees and a direct OCI runtime

Status: DRAFT, revision 2 (after security, adversarial and Rust-implementation review; section 15
records what each review changed or was refused). Nothing here is built. Applies to the Linux
container driver (`crates/kbf-driver-container`); the macOS native driver and the draft VM driver are
unchanged.

Claim tags: **[M]** measured on a kbf node or read in kbf code; **[P]** published (Sources);
**[I]** inference, to be settled by an experiment in section 11. Every latency number in section 7 is
[I]: nothing here promises a latency before the section 11 gates have run.

## 1. Summary

Today one action costs about nine `podman` processes, about 15k `link()` calls and four ownership
passes. Measured on a 44-core node [M]: p50 about 21 s create/start/cleanup and about 16 s linking a
15k-file toolchain tree at 50-80 concurrent actions; about 2 s for a tiny action on an idle node.

What the data say, and what they do not:

- Each `podman` invocation takes 0.10-0.25 s idle and 4-7 s at 80+ open leases, measured with
  podman's own monotonic clock inside the process [M]. Nine of them are about 45 s of a 60 s lease.
  CPU pressure and the SSD's own pressure were near zero over the same period [M], but PSI does not
  see uninterruptible waits on the ext4 journal or on rw-semaphores (they raise load average without
  raising pressure) [M]. **The cause of the inflation is therefore not attributed.** Candidates are
  podman's state database and storage locks, and fsync waits on the shared ext4 journal (99.9% of
  913,901 journal transactions were forced by fsync callers [M]). If the journal is the cause, this
  design inherits part of it. Section 11.3 makes the attribution a gate before the runtime change.
- Linking costs 0.6-0.75 ms per file idle against 17 us for the `linkat` itself [M]: the pipeline
  around the syscall (two blocking-pool hops per blob, an index mutex, sequential `create_dir`), times
  15k files, times every action. A plain loop would plausibly cost 0.3-0.8 s for 15k links [I]; section
  4.2 and step 0 test that before trees are credited with the whole 16 s.

The design, in order:

0. **Gates.** Attribute the podman time (E1) and measure raw `fsync`/`linkat` at 1, 8 and 64
   concurrency on the node's filesystem with no podman (E0); write a bulk linker and re-measure the
   16 s (E8); compute offline how many real actions the tree classifier covers (E9).
1. **Shared trees**, on the existing podman driver. A large Directory node that recurs across actions
   is written once to `trees/<digest>/` and mounted read-only into each action that has it.
2. **No podman.** A small launcher binary creates the action's namespaces and mounts, then runs
   `crun` on a bundle kbf writes. No image store, no database, no `conmon`, no `podman unshare`.
   About three execs per action instead of about seventeen.

**Primary:** launcher + `crun run`, rootfs and input root as overlayfs, trees as read-only mounts.
**Fallback (stated rollback):** step 1 on the podman driver (extra `:ro` volumes) plus the per-node
`--container-runtime` flag. It keeps the nine podman processes. An in-process runtime is **not
planned** (section 9).

## 2. Requirements and the industry baseline

Fresh sandbox per action, no reuse, no pre-started containers. Rootless (user `kbf`, no setuid
helper of ours, no root service). Isolation of the current strength: user, mount, pid, ipc, uts,
net namespaces; `--network=none`; the container's ids are subordinate ids, never kbf's
(`--userns=nomap`); seccomp, capability drop, masked paths; cgroup v2 limits. Base image pinned by
digest. Input root = shared read-only trees + small per-action files. Outputs read only from a
writable layer. Mac stays native.

What others do [P] (detail in the survey): BuildBuddy's executor writes an OCI bundle and runs
`crun` directly, with image layers as overlay lowers; Bazel's `linux-sandbox` builds namespaces and
mounts itself, binding inputs read-only; Buildbarn keeps one long-lived runner per slot and fills a
directory from its file cache; AWS Lambda restores a Firecracker snapshot. Lambda's speed comes from
doing no per-start file work and holding no shared lock, not from reusing a container; the same two
properties are what this design buys (trees: no per-file work; launcher: no shared state).

## 3. Architecture

```
kbf-daemon (user kbf, tokio)
  tree store      trees/<digest>/            read-only, hard links to the file cache, LRU; dir 0700
  image store     images/<digest>/rootfs/    unpacked once per image digest, read-only
  per action      lease/{root,upper,work,rootfs-upper,rootfs-work,bundle/}
  keeper          kbf-daemon __keeper (unchanged): holds the pipe, writes cgroup.kill on close
        |
        | spawn (1 exec): kbf-sandbox run <lease dir>      (a separate small binary, no tokio)
        v
kbf-sandbox (single-threaded, outside the lease cgroup)
  1 unshare user+mount ns; a forked helper execs newuidmap/newgidmap on its pid (sync pipe)
  2 mount --make-rprivate /; in the ns: mount rootfs overlay, /kbf/root overlay, ro tree attaches
  3 spawn crun --root <lease>/crun --cgroup-manager=cgroupfs run --bundle <lease>/bundle <id>
  4 wait for crun; cgroup.kill; wait cgroup empty; reown upper; remove rootfs-upper; exit
        |
        v
crun: nested user ns (container root = setup-ns id 1), pid/ipc/uts/net/cgroup ns,
      /proc /dev /sys, the pinned seccomp/caps/masked paths from config.json, pivot_root, exec
```

This is the layering rootless podman uses today [M, code: `podman unshare` chowns to `1:1`]: a first
namespace ("setup ns") where id 0 is kbf and ids 1..65535 are kbf's subordinate ids (`/etc/subuid`,
size checked at start by `subids.rs`); a second, made by `crun`, where container id 0 is setup-ns id 1
(so no id of kbf is mapped: today's `nomap`). The launcher is root of the setup ns, so it can mount
overlayfs and write kbf-owned inodes; the action never sees the setup ns.

Why a separate binary, not a mode of the daemon: the AppArmor `userns,` grant (D2) attaches to an
executable path. A profile on the tokio daemon, which parses network input, would give that daemon
user namespaces and be inherited by everything it execs. A profile on `kbf-sandbox` alone is as
narrow as the grant can be, and the launcher's `unsafe` (section 12) lives in a crate of its own.

The `__keeper` stays a separate tiny process. Merging it into the launcher would put the dead-man
switch inside a process that may be OOM-killed alone or SIGKILLed: the launcher is deliberately **not**
in the lease cgroup (only `crun`'s children are), and if the launcher dies alone the keeper still
kills the lease cgroup. A conformance test SIGKILLs the launcher and expects the container to die.

### 3.0 Launcher handshake (the riskiest new code)

A process cannot write its own id maps; someone must exec the two setuid helpers on its pid after
`unshare(CLONE_NEWUSER)` and before it continues. Chosen: **the launcher forks a one-purpose helper**
before it unshares. The helper waits on a pipe, then execs `newuidmap` then `newgidmap` on the
launcher's pid (it runs as kbf, so the setuid check holds), and reports the result on a second pipe.
The launcher blocks on that pipe with a bounded timeout; EOF or a nonzero status is a typed error
(`MapFailed`), never a hang. `setgroups` is left as the helper requires. After the maps exist the
launcher runs `mount --make-rprivate /` (`MS_REC|MS_PRIVATE`) **before any mount**: on a systemd host
`/` is shared and the lease's mounts would otherwise propagate to peer namespaces. Rejected: doing it
in the daemon (a pipe protocol in a multi-threaded process and an async step on every start).

### 3.1 What the container sees

| Path | Source | Writable |
|---|---|---|
| `/` | overlay: lower `images/<image digest>/rootfs`, upper `lease/rootfs-upper` | yes, discarded |
| `/kbf/root` | overlay: lower `lease/root` (per-action files), upper `lease/upper` | yes; **outputs are read from `upper` only** |
| `/kbf/root/<path>` per shared tree | read-only mount of `trees/<digest>` | no |
| `/proc`, `/dev`, `/sys`, `/dev/shm` | crun, from the pinned config | per OCI defaults; `/dev/shm` sized by `ContainerLimits` |

Overlay options are pinned, not defaulted (section 5.3). `/kbf/root` keeps today's meaning (working
directory inside it; an output path already in the input root is refused `INVALID_ARGUMENT` before
anything runs). A tree's mount point is an empty directory in `lease/root`, created by `materialize` as
a directory node, so no symlink can sit at a mount point. The launcher attaches each tree with
`open_tree(OPEN_TREE_CLONE)`, `mount_setattr(MOUNT_ATTR_RDONLY | NOSUID | NODEV)`, `move_mount` onto an
fd opened with `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)`: one detached read-only mount,
attached; no bind-then-remount window.

## 4. Shared trees

### 4.1 Which Directory nodes qualify

The daemon walks the action's input root from the top, as `kbf_daemon::tree` does. A Directory node
becomes a shared tree when **all** hold:

- it holds at least `--tree-min-files` regular files and symlinks below it (default 256, the C4b
  threshold in `worker-cache.md` 6.6);
- **it has recurred**: its digest has been seen in at least `--tree-min-sightings` distinct actions
  (default 2) according to the summary map, or a scheduler hint names it. A parent's digest changes when
  any descendant does, so in an exec root where toolchain and sources share a directory the highest
  node is action-unique; a tree built for it is built per action, evicted, and is worse than today.
  A node that has not recurred is **descended into** and its children are considered instead;
- no output path of the `Command`, and not the working directory, lies at or under it (the `private`
  set `materialize_cached` takes today): such a subtree is walked into, and only its qualifying
  children are considered. (An output under `external/` with working directory `.` disqualifies a
  whole toolchain; correct, and counted by E9 so the cost is known);
- it is not the root, and it is the **highest** node on its path that meets the rules above;
- the action has fewer than `--tree-max-mounts` trees (default 8, largest first; the rest are linked
  per action as today), and the node has built fewer than `--tree-build-bytes-per-min` bytes of trees
  in the last minute (the cap stops a corpus of unique digests from churning the store).

Everything else is per-action: the directory skeleton, symlinks and files outside trees, written
with the bulk linker (4.2). The classifier also enforces an **aggregate cap**: if more than
`--tree-max-uncovered-files` (default 2,000) files stay per-action, the action is logged as
"uncovered" with its count, so a classifier that covers too little shows up in the node report rather
than as a silent 16 s.

A node's file count comes from a **summary** (digest -> files, bytes, sightings) filled bottom-up on
the first walk that fetches the node's Directory protos, kept in memory and in `trees/<digest>/.summary`.
The Directory cache (C4a: decoded `Directory` messages by digest, shared by all leases) is a
prerequisite: without it the walk above the trees is a CAS round trip per level per action [M, code:
no Directory cache exists today].

### 4.2 Preparation, and the bulk linker

1. Single-flight by digest in the daemon (a `OnceCell` per digest in a map; no file lock).
2. Build in `trees/.tmp-<random>/` with the **bulk linker**: one blocking thread, plain `mkdirat` and
   `linkat` loops over a pre-fetched Directory tree, no per-file task hops and no per-file index lock
   (pins are taken in one batch). Directories 0755, symlinks as in the tree, each file a hard link to
   its cache entry (the cache pins it while the tree exists; files stay owned by kbf). The same linker
   serves the per-action tree and the fallback path, replacing `materialize_cached` for them; trees must
   not be built with the slow pipeline, or a cold node pays 11 s idle and about 40 s loaded and every
   waiter queues behind it.
3. `renameat2(RENAME_NOREPLACE)` to `trees/<digest>`. Presence of the final name means complete. A
   crash leaves only `.tmp-*`, swept at start. Losing the rename race: delete mine, use theirs.
4. Cost: one bulk link, paid once per digest per node. Directory inodes only; file data is the cache's.

**Cold node.** Eighty first-wave actions would wait on the same single flights. Pre-warming from the
first sight of a digest in the scheduler's placement hint (`worker-cache.md` 8) is part of step 1, not
a later option, and E10 measures the first minutes after a deploy against today.

### 4.3 Eviction and accounting

- A tree holds one cache pin per distinct file. Trees for different toolchain versions share inodes,
  so weight is **not** charged per tree; the store tracks the set of distinct pinned inodes (a
  refcount per inode in the cache) and charges each once. `--tree-store-max-bytes` bounds pinned
  distinct bytes. The cache's LRU cannot free a file a tree links, so the two budgets could starve each
  other: a test fills the store to its cap and checks the cache can still admit (risk 8).
- An in-memory refcount counts attached actions. Only refcount 0 is evicted: `rename` to
  `trees/.dead-<n>` first (a new request re-prepares under the digest name), then the `remove.rs` walk
  in the background, then unpin. An in-use mount source is EBUSY anyway; the refcount keeps the policy
  simple and the failure a logged skip.
- At daemon start there are no running actions (the keeper kills them), so refcounts begin at 0.
- Hard links cut link-count pressure (the 65,000-link ext4 ceiling per inode, `worker-cache.md` 6.5):
  one link per tree per file instead of one per action per file.
- **Page cache.** The tree, the per-action lower and the cache entry are hard links to the same inode,
  and overlayfs reads through the real inode, so 80 actions share one page-cache copy of a toolchain.
  That is a reason to keep hard links over copies. Shared pages are charged to the first cgroup that
  touches them, and copy-up never writes a cache inode (the lower is never written).

### 4.4 Correctness: an action cannot modify a shared tree

There are **two** barriers, plus detection; they are stated as what they are.

1. The attach is a **read-only mount** set by the launcher in the setup ns. The container's ns is a
   child of it, so the flag is locked there [P, `mount_namespaces(7)`]. Conformance row: as container root
   with `CAP_SYS_ADMIN` in its own ns, `mount -o remount,rw` fails (this also answers whether `crun`
   creates its mount namespace after the user namespace so inherited mounts are locked; the test is the
   evidence).
2. Files are owned by kbf, which is unmapped in the container, and have no group/other write bit.
   The cache already creates entries 0444 (0555 executable) and `verify.rs` checks mode and owner when it
   places an entry [M, code: `layout.rs` `Key::mode`, `verify.rs:39`]; REAPI carries only `is_executable`,
   so kbf sets modes itself. Container root has no `CAP_DAC_OVERRIDE`/`CAP_FOWNER` over an inode whose
   owner it does not map. A conformance test fails if any cached file is group- or other-writable.

Not barriers, and said so:

- The host-side path to a tree is closed by mode: `trees/` and its parent are 0700 kbf-owned
  (conformance test). The container has no path to the cache or `trees/`; only the tree is mounted.
- `verify.rs` runs on `place`; a write through a shared inode after placement is not detected there. The
  tree store therefore re-hashes a **sample** of tree files periodically (bytes read per hour capped)
  and on any `EROFS`-bypass suspicion, and treats a mismatch as a cache poisoning event: evict, alert.
  This is detection after the fact, not a barrier.

Edge cases:

- **Symlinks in a tree** stay symlinks and resolve inside the container, never on the host. Names are
  checked at preparation by `check_names` (no `.`, `..`, `/`, NUL, duplicates).
- **Hard links.** REAPI has no hard-link node; equal files may share one cache inode, so `st_nlink > 1`;
  `link(2)` from a tree path into `/kbf/root` fails `EXDEV`. Tools that hard-link without a copy fallback
  fail (risk 3).
- **Ownership.** Per-action files and tree files are kbf-owned and unmapped, so an action cannot edit its
  own inputs in place: a toolchain that does gets `EACCES` (not only `EROFS`). That matches today.
- **`rename(2)` across a tree boundary** fails `EXDEV`; a write into a tree fails `EROFS`. Today a new
  file in a toolchain directory succeeds. This is a behaviour change (D1). The escape hatch is **per-run
  fallback to the bulk-linked per-action tree for that digest** (today's semantics, at today's cost
  minus the pipeline overhead), not a writable shared overlay (rejected, section 15).
- **Output under a tree** disqualifies that tree (4.1), so outputs are always in `upper`.
- **Unexpected writers.** The tree directory is written only by the daemon (preparation, eviction),
  never through a mount; rename-then-delete means an in-use tree is never edited.

## 5. Ownership and mounts

### 5.1 Why idmapped mounts are not the answer

Creating an idmapped mount needs `CAP_SYS_ADMIN` in the filesystem's user namespace; a rootless user
does not have it, and podman's documentation says `idmap` is rootful-only [P]. EROFS/composefs needs a
root mount on 6.8 [P]. Mapping container root to kbf would remove the chown passes but undo the
isolation `worker-cache.md` 5.1 depends on. None is used.

### 5.2 What replaces the chown passes

| Today (`give_overlay`, 4 execs of podman or helper) | New |
|---|---|
| chown the input root's directories to `1:1` (15k `statat`, one `fchown` per directory) | Shared trees need none. The per-action tree (hundreds of entries) has its **directories only** chowned in-process by the launcher, **reusing the `chown_dirs` walker** (moved to a shared module, not copied). Files and symlinks are never touched: they are hard links to cache inodes, and `chown` on one re-owns the shared inode for the cache, every tree and every other action, which would end barrier 2. |
| chown `upper`, `work` to `1:1` | Same walker, before the mount; the directories are empty. |
| after the run, chown `root`, `upper`, `work` back to `0:0` | Reown of `upper` described below. `rootfs-upper`/`rootfs-work` are removed in the setup ns, without a chown. |

Conformance test: stat a cached inode's owner and mode before and after a lease and expect them
unchanged (mutant: a chown that follows files).

**Reown order (after the run).** (1) `crun` has exited; (2) write `cgroup.kill` and **wait until the
cgroup is empty**, so no live process can race the walk by swapping directories or symlinks; (3) walk
`upper` by descriptor with `O_NOFOLLOW`, as `chown_dirs.rs` and `outputs.rs` do, giving each entry to
kbf. Rules: `chown` clears setuid/setgid and file capabilities on non-directories (kernel `chown_common`
`ATTR_KILL_SUID/SGID/PRIV`; confirmed on 6.8 by a test, not assumed): an action that creates a mode-4755
file and a `security.capability` xattr must produce an output with neither. Outputs are attacker-shaped,
so the collector stays `O_NOFOLLOW`, size-capped and special-file refusing as today, and also handles
mode-000 directories, symlinks to host paths and sparse files; the checklist is a test row.

The order cannot assume the launcher survived: section 8 defines `kbf-sandbox reap` for leases left by
a dead daemon.

An alternative to try (E4) is making the **per-action** lower's directories 0777 so no chown is needed.
It changes `ls -l` inside the container and is measured only; it is never applied to a shared tree.

### 5.3 Overlay and mount options (pinned)

Both overlays are mounted with `userxattr,metacopy=off,redirect_dir=off,index=off,xino=off`, and `/`,
`/kbf/root` and every tree attach carry `nosuid,nodev`. With `userxattr` the overlay metadata lives in
`user.overlay.*` xattrs, which the container can write on its own files; `metacopy`/`redirect_dir` off
means a stub or redirect planted in `upper` cannot make collection read empty data or follow a redirect.
The collector handles the opaque-directory xattr and the 0:0 whiteout character device explicitly
(a whiteout is a deleted file, not an output). Today these options are podman's defaults; they become a
tested constant (a unit test pins the option string, a conformance row reads `/proc/self/mountinfo`).
`volatile` is a candidate on both overlays (E2/E3), since neither upper needs crash safety.

## 6. Runtime: what kbf writes

### 6.1 The bundle, and the hardening podman used to supply

**Podman, not `crun`, supplies today's hardening** [M, code: `podman.rs` passes only `--userns=nomap`,
`--network=none`, limits; seccomp, capabilities, masked paths, `/dev` and sysctls are podman's
defaults]. `crun` applies only what `config.json` contains; its `crun spec` output is minimal (to be
confirmed by running `crun spec` on the CI runner, step 0). Removing podman without replacing that
policy would give the action the full capability set in its user namespace and no seccomp filter, and the
main attack on a rootless sandbox is a kernel bug reached through capabilities in a user namespace (for
example `unshare(CLONE_NEWUSER|CLONE_NEWNET)` then nftables).

Therefore kbf **vendors and pins by digest**, in `kbf-sandbox`: the seccomp profile (containers-common
`seccomp.json` at a pinned version), the capability set (checked against what the podman driver gets
today), the masked and read-only paths, `noNewPrivileges: true`, the minimal `/dev` device list, the
default sysctls (`net.ipv4.ping_group_range`), a private cgroup namespace, `/sys/fs/cgroup` read-only,
and `--no-new-keyring` behaviour. A golden `config.json` generated from the vendored inputs is checked in.

`lease/bundle/config.json` is otherwise generated from the inputs of `create_args` in `podman.rs`:

| podman (today) | OCI `config.json` |
|---|---|
| `--network=none` | `network` namespace with no path; loopback up (E2 checks that `crun` does it) |
| `--userns=nomap`, subids | `linux.uidMappings/gidMappings`: container `0..65534` -> setup-ns `1..65535` |
| `--user=0:0` | `process.user` 0:0 |
| `--unsetenv-all`, env | `process.env` = the Command's variables only |
| `--cgroup-parent`, `memory.oom.group=1` | `linux.cgroupsPath` = the lease cgroup the daemon creates; daemon writes `memory.oom.group` as now |
| pids, `nofile`, `nproc` | `linux.resources.pids`, `process.rlimits` |
| `/dev/shm` size | `tmpfs` mount with `size=` from `ContainerLimits` |
| `--oom-score-adj=0` | `process.oomScoreAdj` |
| hostname | `hostname` (`/etc/hosts`, `/etc/hostname` from small files in the lease dir) |
| seccomp, caps, masked paths, noNewPrivileges | the vendored, pinned set above, written explicitly |

`crun run` stays foreground with `process.terminal=false`; stdout and stderr are the files the daemon
opens now. The OOM classification stays on the lease cgroup's `memory.events`. `crun --root
<lease>/crun` keeps runtime state per action: no shared directory.

**Launcher exit contract.** The launcher is a separate process, so errors reach the daemon as an exit
status. A small enum, defined in `kbf-sandbox` (`thiserror`; `anyhow` only in `main`), maps to the
driver's Failed/Infra split: setup failed, id-map failed, mount failed, runtime failed (crun itself
failed), reown failed, and "the action exited N". `crun`'s own failures are told apart from an action
that exited 1 by running crun with `--log` to the lease dir and by a status file the launcher writes
(`action_exit`) only after crun reports the process exited; no status file means a runtime failure.
A table test covers the mapping.

**File descriptors.** The launcher holds `open_tree` fds, the tree-store directory, the lease directory
and the keeper pipe, all in the setup ns that owns host files; any such fd inherited by `crun`'s child is
a path to the host through `/proc/self/fd` (the 2024 runc "Leaky Vessels" class). Everything is opened
`O_CLOEXEC`; before exec the launcher calls `close_range(3, !0, CLOSE_RANGE_CLOEXEC)`; a conformance row
asserts `/proc/self/fd` inside lists only 0, 1, 2 (mutant: one fd opened without CLOEXEC).

### 6.2 Base image

`images/<digest>/rootfs` is produced once per image digest and never changes. Acquisition is off the
start path. Step 2a: extract from the podman store that already holds the image (`check_pinned` reads it
today). Step 2b: a pure-Rust pull and unpack run in a setup ns by `kbf-sandbox unpack`, so layer files
with uid X land owned by setup-ns id X+1. A digest pin proves provenance, not that the tar is benign, so
the unpacker is treated as a parser of untrusted input: path traversal and symlink-then-write ordering
are refused (every path is opened relative to the rootfs with `openat2 RESOLVE_BENEATH`), hardlinks may
only point inside the rootfs, setuid bits and `security.capability` xattrs in layers are kept only as the
image declares and reviewed (the image is pinned), device nodes are refused, decompressed size is
capped, and the tar crate is chosen after checking recent parser-desync advisories (a Rust async tar
fork had one in late 2025: confirm the number and the fixed version before choosing; [U]). It runs in the
narrowest namespace that works and is fuzzed like the spec parser. Image-by-digest pinning, the "tag or
index digest is `INVALID_ARGUMENT`" rule and the `kbf.owner` sweep stay as today.

### 6.3 Why `crun` and a launcher, not `crun` alone; why a CLI exec, not a library

`crun` can mount an overlay named in `mounts` (podman's `:O` volume does) but a rootless overlay as `/`
needs a mount made before `crun` sets the rootfs. A launcher also owns ownership and teardown. If E2
shows `crun` alone can mount the rootfs overlay, the launcher shrinks to the id-map handshake, the
ownership walk and reap.

The runtime is the **`crun` CLI exec'd from a single-threaded launcher**. Libraries were rejected for
these reasons: `libcrun` needs `./configure --enable-shared`, its README makes no API-stability promise,
it would add bindgen plus libyajl, libseccomp and libcap to the build closure and link C that calls
`fork` and `unshare` into our binary, and it saves one `execve` (about 1 ms) against a 47 ms floor.
youki's `libcontainer` is a 0.x crate with a moving API, needs libseccomp and libsystemd at build time,
has few reverse dependencies, and the one independent run we have recorded 36 failures in 1,000 starts
for 0.3.3 (a 2024 blog, "not scientific"; `runtimes.md`). Speed is not the argument (the 111 vs 47 ms
bench compares CLI to CLI).

### 6.4 `crun` provisioning

`crun` comes from the distribution package on nodes (1.14 on Ubuntu 24.04 [M]) and from the same package
version in CI; `kbf-sandbox` ships a minimum version and `sandbox-check` asserts it. The experiment
results E1, E2 and the loopback behaviour are recorded **with the crun version** beside each, since all
three are version-dependent. Whether the hosted runner image ships `crun` is checked in step 0; if not,
installing it is an explicit `runner_setup` step.

## 7. Latency, and what it does to 21 s + 16 s

Anchors:

- `crun` full create/start/delete of a trivial container: 47 ms (youki README bench, kernel 6.5, 16
  cores) [P]; about 17 ms each over 100 sequential runs (`crun` README, machine unstated) [P]. The first
  bench drops caches on each run and does not appear to be rootless; neither covers overlay mounts, the
  `newuidmap` double exec, netns teardown or cgroup removal. They are anchors, not predictions.
- One `unlinkat` of a hard link: 17 us; a 15k-link `remove_tree` 0.26 s idle [M].
- One `podman` invocation: 0.10-0.25 s idle, 4-7 s at 80+ open leases [M].

Expected per action, idle, excluding the payload [I] (a hope until E2 runs):

| Phase | Expected |
|---|---|
| spawn launcher, helper, `newuidmap`, `newgidmap` (4 execs) | 3-10 ms |
| rootfs overlay + `/kbf/root` overlay + up to 8 tree attaches | 2-10 ms |
| bundle write, per-action tree (hundreds of links), chown of its directories | 5-15 ms |
| `crun` create/start (netns, cgroup, `/proc`, seccomp, pivot_root) | 20-60 ms |
| teardown: cgroup kill+rmdir, remove per-action tree and `rootfs-upper`, reown `upper` | 10-60 ms |
| **Total** | **40-150 ms**, versus about 2 s now; the real number may land in the low hundreds |

**Under 60-80 concurrent actions nothing is published and I promise no number.** The data do **not**
show the inflation is podman's and not the kernel's (section 1). What this design removes with certainty
is podman's database, storage locks and process starts. The remaining shared kernel state, which the
draft's first version understated, and which E3 times separately at 8 and 64 concurrent:

- `namespace_unlock()` calls `synchronize_rcu_expedited()` after dropping `namespace_sem` whenever
  anything was unmounted; every sandbox teardown unmounts and mount setup takes `namespace_sem` for write,
  so at 64 concurrent it is a global serialization point [P, 6.8 `fs/namespace.c`, from review reading;
  confirm by timing];
- network namespace teardown is asynchronous on a single `netns` workqueue (`cleanup_net` takes
  `pernet_ops_rwsem` and calls `synchronize_rcu()`) [P, 6.8 `net/core/net_namespace.c`; same caveat],
  so netns creation/teardown rates are a possible ceiling;
- `cgroup_mutex` at mkdir/rmdir, and the count of dying cgroups (they slow `memory.stat` reads);
- the ext4 journal: the link, unlink and delete work, the action's own fsyncs (compilers fsync), the
  `discard` mount option (202M discard ops, about 0.9 ms each [M]) and `rootfs-upper` all share one SATA
  SSD and one journal.

Mitigations considered and measured, not assumed: `volatile` overlays (E2/E3); fixed-interval `fstrim`
against `discard` (E5); `rootfs-upper` on a size-capped tmpfs or separate volatile filesystem (small
`/tmp`-style writes; section 12); fewer mounts; batched deletes.

Pass criterion for the stage: p50 setup+teardown at 64 concurrent under 1 s and flat from 8 to 64
within a factor of 3; a miss names the kernel resource in E3.

Against the measured costs: the **16 s of linking** becomes about 0 for tree-covered files plus the
first-sight preparation (4.2), and the bulk linker (E8) may remove most of it before trees are used; the
**21 s** becomes the table's figure only if the inflation was podman's (E0/E1 decide). Unchanged: server
round trips, Directory fetches the C4a cache does not cover, output hashing and upload (about 0.4 s per
lease in the A8 run [M]), and the payload.

## 8. Concurrency and recovery

- Tree hit: one `stat` of `trees/<digest>`, an atomic increment. Miss: the per-digest single flight. No
  global lock; a map shard lock is held for the lookup only.
- Image hit: a `stat` of `images/<digest>/rootfs`. Miss: single flight, off the lease's timeout clock.
- Per-action directories are private to the lease. `crun --root` is per lease. No database.
- The launcher is a separate process with no tokio runtime, so a blocking pipe read or mount call cannot
  stall a worker of the 88-thread runtime. The daemon's synchronous `cgroup.create` and keeper read on
  async workers is a separate finding and is fixed in the new driver by doing cgroup work in
  `spawn_blocking`.
- `--max-open-leases` stays as an admission bound; the knee-capping stopgap is not part of this design.

**Orphan cleanup needs a namespace.** Per-lease directories hold files owned by real subordinate ids
(`rootfs-upper`, the chowned per-action tree, `upper` mid-run), which kbf cannot delete from outside a
user namespace. After a SIGKILL of the daemon the launcher may be dead too. The launcher binary has a
second verb, `kbf-sandbox reap <lease>`: it enters a setup namespace, chowns back and removes. The
start-up sweep runs it for every leftover lease; a conformance test runs it on a lease left by a killed
daemon. Image-store eviction (files owned by subordinate ids after unpack) uses the same verb.

## 9. Fallbacks and what is not chosen

- **F1, podman with shared trees** (no new runtime): `--volume trees/<digest>:/kbf/root/<path>:ro` per
  tree. Same store, classifier and tests. Ships first and proves the layout; keeps the nine podman
  processes. Together with the per-node `--container-runtime` flag it is the stated rollback.
- **F2, an in-process Rust runtime** (own `clone3`/`pivot_root`, `seccompiler`, caps, masked paths):
  **not planned.** It would be a second runtime to audit against every kernel fix and every runc/crun
  escape CVE, and kbf would own the whole hardening list (risk 6). It is a last resort only if E2/E3
  show `crun`'s own floor exceeds the budget, and it would need a named maintainer and a cost estimate
  before anyone starts.
- **Not chosen:** the Podman REST API (saves process starts, not podman's locks, database, `conmon` or
  storage layer); bubblewrap/nsjail (no OCI image or cgroup story; one more binary); gVisor (syscall-
  heavy builds pay); composefs/EROFS and idmapped mounts (need root on 6.8, section 5.1); microVMs as the
  default (section 10); `libcrun`/`libcontainer` (6.3).
- **Conformance cross-check:** one CI job runs the same generated bundles under `runc`, to separate "our
  `config.json` is wrong" from "crun behaves differently" and avoid a crun-only bundle.

## 10. Coexistence

- **macOS native driver:** untouched. The classifier and tree store live in `kbf-daemon`, so the Mac
  driver can use the same decision with its own mechanism (`clonefile` of `dirs/<digest>`, C4c in
  `worker-cache.md`).
- **VM driver (draft, PR #392):** a stronger-isolation tier, not a rival. Its `VmHost` trait takes a
  read-only inputs share; for a Linux microVM host the same classifier would give one read-only block
  image per tree digest. Firecracker has no virtio-fs, so a block image is the form [P]. It is not on this
  plan's path.
- **Scheduler and node report:** the driver keeps `KIND`/`DRIVER`; the node report adds `runtime =
  podman | sandbox`, the tree store's size and hit rate, and the uncovered-files count (4.1).

## 11. Migration, tests, measurement

### 11.1 Steps (each reversible by a flag, each mergeable alone)

| # | Change | Flag | Rollback |
|---|---|---|---|
| 0 | Gates E0, E1, E8, E9, plus `crun spec` and runner-image checks; per-verb timings in `Podman::output`; `make_directories` vs links in the `inputs:` line | none | n/a |
| 1 | Directory cache (C4a) + bulk linker + tree store + classifier + pre-warm, behind podman `:ro` volumes (F1) | `--shared-trees=off\|on` | off |
| 2 | `kbf-sandbox` crate (launcher, `reap`, bundle writer, vendored hardening, image store 2a), **together with the `ContainerRuntime` trait** whose methods (create, run, wait, kill, collect, reap) are named from the conformance suite below; both implementations land in the same PR | `--container-runtime=podman\|sandbox` | per node |
| 3 | Pure-Rust image pull and unpack (2b); remove the last podman dependency | same | per node |

The trait is not landed as a standalone refactor (a trait with one implementation is a concrete type),
and step 2 does not move the 7,000-line crate: both runtimes live in `kbf-driver-container`'s lease
code first, reusing its `cgroup`, `delegate`, `subids`, `outputs`, `remove`, `cas` modules; extract a
shared crate only after step 3. **Step 2 starts only when E0/E1 have named the cause of the inflation and
E7 (AppArmor) and E2 (the runtime floor) have passed.**

### 11.2 Conformance

The same lease-level tests run against both runtimes through one test body parameterised by the runtime
constructor (today's real-podman tests in `runtime_tests.rs` become the shared suite). Each row names the
defect it catches and the mutant to plant:

| Test | Defect / planted mutant |
|---|---|
| exit code, signal, timeout (stdout/stderr kept), OOM at lease cap vs backstop; runtime failure vs action exit 1 | wrong status mapping from `crun` |
| writes outside `/kbf/root` do not appear in outputs; outputs only from `upper` | output leak from the rootfs layer |
| write, chmod, rename, unlink, `link`, `mount -o remount,rw` (as container root with `CAP_SYS_ADMIN` in own ns) into a shared tree each fail; tree digest unchanged | mutants: drop `MOUNT_ATTR_RDONLY`; chown the tree |
| cached inode owner and mode unchanged after a lease; no cached file is group/other-writable; `trees/` and parent are 0700 | mutant: chown following files; mode 0644 entry |
| **hardening, read from inside:** `Seccomp:2`, `NoNewPrivs:1`, `CapEff` equals the pinned mask; `/proc/kcore`, `/proc/sys`, `/sys/firmware` masked or read-only; `unshare`, `mount`, `bpf`, `keyctl` return EPERM/ENOSYS; `perf_event_open`, `userfaultfd` blocked; the `/dev` list matches | mutants: seccomp section removed; add `CAP_SYS_ADMIN` |
| `/proc/self/fd` lists only 0, 1, 2 | mutant: fd without CLOEXEC |
| container ids: `id` and `uid_map` show no id of kbf; a file kbf owns is `nobody` | mapping regressed to include kbf |
| inside, write `memory.max`/`pids.max` and expect EACCES/EROFS; `/sys/fs/cgroup` read-only | container raises its own limits |
| a mode-4755 file and a `security.capability` xattr created by the action reach the output with neither; mode-000 dir, symlink to a host path, sparse file handled | reown/collector gaps |
| overlay: `mountinfo` shows the pinned options and `nosuid,nodev`; planted metacopy/redirect xattr in `upper` is not followed | option defaults |
| tree mount survives `crun`'s rootfs bind and private remount and shows `ro` with locked flags in `mountinfo`; lowers that are mounts stack to the depth we use (the 6.8 limit is run, not listed) | nested-mount differences |
| an output path under a would-be tree disqualifies it; one differing file per action builds no tree | tree churn; outputs in a tree |
| two actions share a tree concurrently; eviction of a tree in use is refused | refcount / rename-delete order |
| daemon killed mid-action: keeper kills the cgroup; next start's `reap` clears the lease; **launcher SIGKILLed alone: the container still dies** | orphaned sandbox |
| env contains only the Command's variables; fresh netns has loopback only and no abstract-socket path to the host | environment or network leak |
| node-start preflight (`sandbox-check`) fails clearly on each missing prerequisite | opaque failure on a new host |
| `id`/`ls -ln /` of the base image in the unpacked rootfs (2b); unpacker refuses traversal, device nodes, outside hardlinks | silent permission bug; tar attack |

Unit tests assert `config.json` against the same table as the argv tests in `podman.rs` and against the
golden file generated from the vendored hardening inputs, so a dropped limit, flag or seccomp section
is a red test. Launcher mutants: skip `MS_PRIVATE`; drop `MOUNT_ATTR_RDONLY`; chown `upper` without
`O_NOFOLLOW` (a symlink planted in `upper` must not escape); skip the reown (output hashing must fail).

**CI.** The existing hosted job (`podman-x86`/`podman-arm`, `cargo xtask podman-tests` in a
`Delegate=yes` unit) works because Ubuntu ships profiles allowing `userns` for `podman` and `crun`. The
launcher is a test binary under `target/debug/deps/` with no profile; on `ubuntu-24.04` with
`kernel.apparmor_restrict_unprivileged_userns=1` the `uid_map` write fails with EPERM (the symptom in
actions/runner-images#10443). `runner_setup` therefore loads a throwaway `userns,` profile for the test
binaries via `apparmor_parser`, or sets the sysctl to 0 on the ephemeral one-job VM, with a comment that
this must never be copied to a node. `sandbox-check` has a unit test that it names the AppArmor sysctl
when the maps fail. The arm job is verified separately (`newuidmap` and `/etc/subuid` setup may differ).
`sandbox-check` is a library of functions each returning a typed `Missing(...)`, runnable without root
and without podman, so it is also the first CI step and fails there with the message a node would give.

### 11.3 Measurement plan (a probe binary in `kbf-bench`, run on a test node only with the maintainers' explicit go)

The probe runs as user kbf. It reports per-phase histograms (spawn, ns setup, mounts, `crun`, payload,
teardown) at 1, 8, 32 and 64 concurrent, for payloads: `true`; read 1,000 files of a 15k-file tree;
write 200 MB; fork 500. **Gates (before step 2): E0, E1, E7, E2.**

- **E0** raw `fsync` and `linkat`/`unlinkat` loops, 1/8/64 concurrent, on the node's filesystem, no
  podman. Says whether the journal alone reproduces the inflation.
- **E1** `strace -f -tt -c` / `perf trace -s` of one idle lease and one at 60 concurrent on today's
  driver: time per phase and the lock waited on. Settles the section 1 attribution.
- **E7 (first, with E2)** AppArmor: can a `userns,` profile on `kbf-sandbox` be written so that the
  mounts, `pivot_root` and `umount` it performs, and `crun`'s nested namespace under a transition, are
  all allowed? Which profile does the action run under (it must not inherit the launcher's by accident;
  either that profile is a deliberate tight jail denying `/home/kbf/**`, the cache and `/etc/subuid`, or
  `crun`'s shipped profile applies; the test checks which). If no profile works, the primary path dies and
  F1 is the answer.
- **E2** a hand-written bundle run by `crun` from a hand-built setup ns, no tree: the runtime floor,
  whether `crun` alone can mount the rootfs overlay, whether it brings loopback up, with `--cgroup-
  manager=cgroupfs` checked against the no-internal-process rule for the lease cgroup. Record crun's
  version.
- **E3** E2 + the tree attach, + the nine-tree case, at 1..64 concurrent; per-phase timing of
  `unshare(NEWNET)`, mount and umount at 8 and 64, the dying-cgroup count, `volatile` overlays on/off.
- **E4** ownership of the per-action lower: 0755 + chown vs 0777 + no chown; owner and mode of the
  upper's files; whether kbf can read them without the launcher.
- **E5** delete/unmount cost with and without `discard` (a test-node mount change) and with fixed-
  interval `fstrim`; `volatile` on `rootfs-upper`.
- **E6** a corpus run of the real toolchain targets with trees mounted: every `EROFS`/`EXDEV` is a
  finding for D1.
- **E8** the bulk linker against the current pipeline, idle and at 64 concurrent; then trees against the
  bulk linker at 64, which is where shared-inode contention (the case for trees) shows or does not.
- **E9** offline, no node access: from the A8 journal and the CAS input roots, the distribution of input
  files per action, the share inside qualifying nodes, the share of actions with fewer than 1,000
  uncovered files, how often an output under a tree disqualifies one, and how many digests recur.
  **Gate for step 1.**
- **E10** the first minutes after a deploy (cold trees, 80 first-wave actions) against today.

Kill criteria: E2 slower than 300 ms idle, E3 flat-lining above 1 s at 64 concurrent, or E7 showing no
workable profile, stops the primary path and leaves F1 shipped.

## 12. Rootless requirements and security posture

Ubuntu 24.04 nodes (kernel 6.8, `crun` 1.14, `uidmap`, cgroup v2) [M]:

- `kernel.apparmor_restrict_unprivileged_userns=1` is on [M]. `kbf-sandbox` needs an AppArmor profile with
  `userns,` (delivered by `kbf-deploy`); the grant is on the launcher binary, not the daemon (3). The
  alternative, the sysctl off, widens every user on the box (D2). The profile is not a hard boundary
  (published bypasses of the restriction exist [P, Qualys 2025]); it narrows the exposure.
- `newuidmap`/`newgidmap` setuid, `/etc/subuid`/`/etc/subgid` with at least 65,536 ids for kbf, **and no
  overlap with any other user's range** (`subids.rs` checks size, not overlap; `sandbox-check` adds the
  overlap check); `user.max_user_namespaces` above zero.
- cgroup v2 with `Delegate=yes` for the unit and `cpu memory pids io` delegated (`delegate.rs`). The
  lease cgroup files stay kbf-owned, which is what keeps the container from raising its own limits
  (`DAC_OVERRIDE` does not apply to an unmapped owner); cgroupns is private.
- Unprivileged overlayfs (kernel 5.11 or newer [P], 6.8 here) with `userxattr`. **The cache, `trees/`,
  `lease/` and the image store must be on one filesystem**, because hard links require it (`images/`
  too, so that unpack and eviction stay on the same device as the lower mounts); ext4 or XFS with
  `ftype=1`, **not** `noexec`, `nosuid` or `nodev`. The node's single SATA SSD carries journal, discard
  and rootfs-upper traffic together; `sandbox-check` records the device layout of each host and flags
  this, and a separate small volatile filesystem for `rootfs-upper` is considered (E5).
- **Kernel floor.** Overlayfs and user namespaces have a long CVE history (copy-up setuid issues, the
  2023 Ubuntu overlayfs pair; confirm numbers against NVD before citing, [U]). Mounting overlay in a
  setup ns is not new exposure (podman does it too), but `sandbox-check` refuses a kernel below a pinned
  patched floor, and `nosuid` is the default (5.3).
- Per new host: `kbf-node sandbox-check` (user, sysctl, subid ranges and overlap, `newuidmap` bits,
  cgroup delegation, a trial overlay mount in a trial namespace, filesystem flags and device, `crun`
  version, kernel floor) before it takes sandbox leases. What those hosts have is not known to me; this
  list is what to verify.

**Unsafe code.** The launcher adds `open_tree`, `mount_setattr`, `move_mount`, `openat2`, `renameat2`,
`close_range`. Prefer `rustix` (already in the workspace) over raw `libc::syscall`; confirm each function
exists in the pinned version (`cargo doc` on the farm) before relying on it. All `unsafe` stays in one
small module of `kbf-sandbox` with a `// SAFETY:` comment per call; the crate states its `unsafe`
policy explicitly (an exception if the workspace forbids it).

## 13. Risks

1. **The inflation is not podman's.** If E0 reproduces it without podman, or E3 stays slow at 64, the
   kernel (mount-namespace RCU, netns teardown, `cgroup_mutex`, the journal) is implicated. Mitigation:
   the gates in 11.3 come first; the fix is fewer mounts, batching, `volatile`, a separate filesystem.
2. **Write-in-tree behaviour (D1).** Toolchains that write next to themselves fail `EROFS`. Mitigation:
   E6 before the flip; per-digest fallback to the per-action tree.
3. **Cross-mount tools.** `rename`/`link` across a tree boundary give `EXDEV`; most tools fall back to
   copy, some do not.
4. **Ownership model.** The security argument rests on kbf being unmapped in the container. Every change
   to the mappings restates it; the tests plant the mutant.
5. **Launcher is new privileged-ish code** (ns creation, id-map handshake, mounts, chown by descriptor).
   Small, single-threaded, no tokio; reviewed like `chown_dirs.rs`; spec parsing and the unpacker fuzzed.
6. **Hardening is kbf's to own once podman is gone.** The vendored seccomp/caps/paths are a supply-chain
   input: pinned by digest, updated deliberately, the update is a reviewed diff of the golden file.
   Bazel's `linux-sandbox` source is a checklist [P].
7. **Fresh sandbox per action excludes warm tricks:** no namespace or netns pools, no prestarted
   containers. What may be prepared ahead are inputs (trees, images), not sandboxes.
8. **Tree store and cache coupling:** trees pin cache entries; the budgets in 4.3 are tested against the
   cache's LRU so neither starves the other.
9. **Kernel floor:** designed for 6.8; no 6.13 fd-layers or 6.15 features. Test on 6.8, not a newer dev box.
10. **Image extract ownership and parsing (2b):** a wrong id shift is a silent permission bug; a hostile
    tar is a setup-ns-root exploit. Covered by the rows in 11.2 and 6.2.
11. **Classifier coverage unknown until E9.** If few actions qualify, the benefit shrinks to the bulk
    linker's.

## 14. Decisions for the maintainers

- **D1.** Shared trees are read-only (`EROFS` on write), where today a new file in a toolchain directory
  works. Recommend yes (the same as Bazel's sandbox and Buildbarn's virtual mode), with per-digest
  fallback to the per-action tree.
- **D2.** An AppArmor `userns,` profile for `kbf-sandbox`, delivered by `kbf-deploy`, instead of the
  sysctl off. Recommend the profile, subject to E7.
- **D3.** No root component anywhere in this plan. Recommend keeping it that way (idmapped mounts, EROFS
  and the Firecracker jailer all need one).
- **D4.** Authorise the gate experiments (E0, E1, E7, E2) on a test node. Nothing past step 1 starts
  without their results.

## 15. Review ledger

Three reviews of revision 1 (security, adversarial, Rust implementation). Every must-fix was accepted;
the items declined or changed in kind are listed with reasons.

| Review item | Disposition |
|---|---|
| Sec M1 / Rust M1: podman, not crun, supplies seccomp, caps, masked paths | Accepted. Section 6.1 vendors and pins them; 11.2 reads them from inside; mutants named. |
| Sec M2: chown directories only | Accepted (5.2), reuse `chown_dirs`, owner-unchanged test. |
| Sec M3: barrier 4 is not a barrier | Accepted: two barriers, 0444/0555 already enforced by the cache [M], sampled re-hash for detection (4.4). |
| Sec M4: leaked fds | Accepted (6.1). |
| Sec M5: reown order, suid, attacker-shaped outputs | Accepted (5.2). |
| Sec M6: pin overlay options | Accepted (5.3). |
| Sec S1-S7 | Accepted: kernel floor and `nosuid` (12), AppArmor on the launcher and which profile the action runs under (3, E7), keeper kept external (3), tar-parser treatment (6.2), podman's lost defaults enumerated (6.1), netns/abstract sockets (11.2), 0777 trick demoted (5.2). The CVE numbers are not cited because they were not verified. |
| Sec questions | Q1 is a test row (4.4); Q2 added to `sandbox-check` (12). |
| Adv M1: inflation not attributed | Accepted: section 1 and 7 reworded; E0/E1 are gates. |
| Adv M2: bulk linker ablation | Accepted: E8, and tree builds use the bulk linker (4.2). |
| Adv M3: "highest node" makes unique trees | Accepted: recurrence rule, byte-rate cap, differing-file test (4.1). |
| Adv M4: coverage unknown | Accepted: E9 is the gate for step 1; aggregate uncovered-files cap. |
| Adv M5: journal contention, `volatile`, `fstrim` | Accepted: candidates in E2/E3/E5, not last. |
| Adv M6: shared-kernel-state list | Accepted (7, E3, `rprivate` in 3.0). Kernel-source claims are tagged as review reading to confirm by timing. |
| Adv M7: AppArmor is a gate | Accepted: E7 moved first. |
| Adv M8: `--tree-mode=overlay` with 0777 lowers | **Changed.** Rejected as written: a world-writable directory in a shared lower contradicts the immutability argument. The escape hatch is per-digest fallback to a per-action tree. |
| Adv S1-S8 | Accepted: anchors reworded, filesystem layout and one-device rule, page-cache paragraph, inode-level weight accounting, pre-warm in step 1, E9 counts disqualification, ownership/EACCES note, dying cgroups in E3. OVL_MAX_STACK stays a CI-run fact, not cited. |
| Rust runtime choice | Agreed (CLI exec from a single-threaded launcher). The speed argument against `libcontainer` is dropped; reasons are in 6.3. |
| Rust M2: orphan cleanup needs a namespace | Accepted: `kbf-sandbox reap` (8). |
| Rust M3: id-map handshake | Accepted: helper-fork design chosen over daemon-side (3.0). |
| Rust M4: CI AppArmor | Accepted (11.2 CI paragraph). Not yet verified on the arm runner. |
| Rust M5: crun provisioning | Accepted (6.4). |
| Rust M6: trait with one implementation | Accepted: trait lands with the second implementation (11.1). |
| Rust S1-S7 | Accepted: exit contract and error enum (6.1), `rustix`/unsafe module (12), nested-mount tests, launcher mutants (11.2), F2 "not planned" (9), `runc` cross-check job (9), `sandbox-check` as typed library (11.2). |

Not verified, each an experiment above: what `crun spec` emits; whether `crun` alone can mount a
rootless overlay as `/`; whether it brings loopback up; the 6.8 overlay stack-depth limit; the AppArmor
behaviour of the launcher/`crun` pair; `rustix` coverage of the new mount API; whether the hosted runner
image ships `crun`; the CVE and advisory numbers named as [U].

## Sources

Fetched 2026-10-14 unless noted.

- crun README (benchmark, `libcrun`): https://github.com/containers/crun
- youki README (47/111/224 ms bench): https://github.com/youki-dev/youki; `libcontainer`: https://docs.rs/libcontainer/latest/libcontainer/
- Red Hat, introducing crun: https://www.redhat.com/en/blog/introduction-crun
- Podman `run` (`--rootfs :O`, `idmap` is rootful-only): https://docs.podman.io/en/latest/markdown/podman-run.1.html; seccomp policy: https://docs.podman.io/en/v4.6.0/markdown/options/seccomp-policy.html
- Default seccomp profile is containers-common `seccomp.json` (copied by a downstream project): https://git.notmyidea.org/almet/dangerzone/commit/3e434d08d1443c118ead809a4150ff1f3c73173e
- Kernel overlayfs (unprivileged mounts, `userxattr`, `metacopy`, `lowerdir+`): https://docs.kernel.org/filesystems/overlayfs.html; 6.8 `fs/overlayfs/super.c`, `fs/erofs/super.c`, `fs/namespace.c`, `net/core/net_namespace.c`: https://github.com/torvalds/linux/tree/v6.8
- `mount_namespaces(7)`, `user_namespaces(7)`, `open_tree(2)`, `mount_setattr(2)`, `openat2(2)`: https://man7.org/linux/man-pages/
- Ubuntu AppArmor userns restriction: https://discourse.ubuntu.com/t/understanding-apparmor-user-namespace-restriction/58007; bypasses (2025): https://www.qualys.com/2025/three-bypasses-of-Ubuntu-unprivileged-user-namespace-restrictions.txt
- Hosted-runner failure for unprivileged user namespaces: https://github.com/actions/runner-images/issues/10443
- Bazel sandboxing and `linux-sandbox`: https://bazel.build/docs/sandboxing
- BuildBuddy `ociruntime` (crun directly, layers as overlay lowers): https://github.com/buildbuddy-io/buildbuddy/blob/master/enterprise/server/remote_execution/containers/ociruntime/ociruntime.go
- Buildbarn: https://github.com/buildbarn/bb-remote-execution
- Firecracker spec and Kata design (no virtio-fs): https://github.com/firecracker-microvm/firecracker/blob/main/SPECIFICATION.md; https://github.com/kata-containers/kata-containers/blob/main/docs/design/virtualization.md
- composefs: https://github.com/containers/composefs
- kbf code read: `kbf-driver-container/src/{lib,podman,runtime,tree,chown_dirs}.rs`, `kbf-daemon/src/{runtime,tree}.rs`, `kbf-daemon/src/cache/{layout,lease,verify}.rs`, `docs/design/worker-cache.md` 5.1 and 6.6; the per-step attribution of a lease from the overhead investigation.
