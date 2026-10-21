# Fast per-action sandboxes: shared read-only trees and a direct OCI runtime

Status: describes what is built on `main`. Applies to the Linux container driver
(`crates/kbf-driver-container`) and to the shared input trees in `crates/kbf-daemon/src/trees`. The
macOS native driver uses the same trees by cloning (section 8). Anything not built is marked
**Planned:** and collected in section 9. Earlier drafts of this document proposed more than was built
(a bulk linker, a recurrence rule, `--tree-*` flags, tool bundles, measurement gates); where the code
differs, this text follows the code.

Claim tags: statements are read from the code named next to them. Nothing here promises a latency: no
benchmark of the shipped backend is recorded in this repository (section 9).

## 1. Summary

An action runs in a fresh rootless container made by `crun`. There is no container manager between
the daemon and `crun`: no image store of a third-party tool, no state database, no `conmon`. Two
things make an action cheap to start:

1. **Shared trees (section 4).** A large Directory node of an action's input root (a toolchain) is
   built once per node in `<data-dir>/trees/<hash>-<size>` and mounted read-only into every action
   that has it. The action's own input root holds an empty directory at that path. The action does not
   link the toolchain's files one by one.
2. **A direct OCI runtime (sections 3 and 6).** The daemon writes an OCI bundle (`config.json`, built
   with the `oci-spec` crate) and runs `crun run` on it through the `runc` crate. `crun` creates the
   user, mount, pid, ipc, uts, network and cgroup namespaces itself, so kbf has no launcher of its own
   and ships no AppArmor profile: the distribution's profile for `/usr/bin/crun` is the only grant for
   user namespaces the node needs.

The runtime binary is a flag (`--oci-runtime`, default `/usr/bin/crun`). `crun` is the only container
runtime the driver has; there is no podman backend and no `--container-runtime` flag.

## 2. Requirements and what is applied

- **A fresh sandbox per action, no reuse.** Each action gets its own bundle and namespaces, created by
  `crun run` and gone with its last process. Nothing is pre-started or pooled (`oci/mod.rs`,
  `oci/lease.rs`).
- **Rootless.** The daemon runs as an unprivileged user and holds nothing as root. The container's ids
  are the daemon user's subordinate ids (`/etc/subuid`, `/etc/subgid`, at least 65,536 of each,
  checked at start by `subids.rs`); the daemon user itself is not mapped. `crun` runs `newuidmap` and
  `newgidmap` for the mapping; start-up checks that both are on `PATH` and are setuid or hold file
  capabilities (`Oci::new`).
- **Isolation per container** (`oci/spec.rs`, `oci/hardening.rs`): new pid, network, ipc, uts, user,
  cgroup and mount namespaces; the network namespace has loopback only (`hosts` and `resolv.conf` are
  written to match: a name lookup fails at once); the Command's environment only; `nofile` and
  `nproc` rlimits; a pids limit and `memory.oom.group=1` on the container's cgroup; the eleven default
  capabilities of containers/common v0.57.4; that release's masked and read-only paths and its
  seccomp profile (kept as `seccomp.json`, pinned by sha256, with one reviewed edit to `personality`);
  `oom_score_adj` 0; `/sys` and `/sys/fs/cgroup` read-only. `process.noNewPrivileges` is `false` for
  an action's container and `true` for helper containers (`spec.rs`, `helper.rs`).
- **Image pinned by digest** (section 6.2). The container's root is the unpacked image, read-only.
- **Outputs are read only from the overlay's upper layer** (section 3.1).
- **Mac stays native.** The macOS driver is not a container driver; it shares the trees (section 8).

## 3. Architecture

```
kbf-daemon (unprivileged user, tokio)
  tree store      <data-dir>/trees/<hash>-<size>/      read-only trees, hard links to the cache; mode 0711 dir
  image store     <scratch>/oci-images/<hex>/rootfs/   unpacked once per image digest, read-only
  per lease       <data-dir>/leases/<lease>/{root,upper,work,rw/,etc/,bundle/,crun/,crun.log}
  keeper          kbf-daemon __keeper: holds a pipe, writes cgroup.kill on the lease cgroup when the daemon dies
  OCI backend     oci-spec -> bundle/config.json; runc crate client with command = --oci-runtime,
                  per-lease state dir (crun/) and JSON log (crun.log)
        |
        | helper container (crun run): give root/upper/work/rw to the container's ids   [section 5]
        | spawn: crun --root <lease>/crun --log <lease>/crun.log run --bundle <lease>/bundle <lease>
        v
crun
  user ns from the id mappings in config.json (newuidmap/newgidmap); mount, pid, ipc, uts, network,
  cgroup namespaces; mounts (read-only image root, /kbf/root overlay, read-only tree binds, writable
  lease directories, /proc /dev /sys); seccomp, capabilities, masked paths; exec of kbf-init, which
  runs the action as its child and reaps orphans
        |
        v  after the action exits
helper container: give the directories back to the daemon user; outputs are collected from upper;
the lease is removed (a helper container removes what the container's ids still own)
```

An action costs `crun` plus its two ownership helpers (also `crun` runs); the remaining work is
file-system work by the daemon. The six driver steps (prepare, start, watch, collect, clean,
verify-clean) are in `runtime.rs` and `docs/design/daemon.md#the-container-driver`.

### 3.1 What the container sees

| Path | Source | Writable |
|---|---|---|
| `/` | the unpacked image, `root.readonly = true` | no |
| `/kbf/root` | an `overlay` mount made by `crun`: lower `<lease>/root` (the input root), upper `<lease>/upper`, work `<lease>/work` | yes; **outputs are read from `upper` only** |
| `/kbf/root/<path>` per shared tree | bind of the tree, `rbind,ro,nosuid,nodev`, mounted after the overlay | no |
| `/tmp`, `/var/tmp`, `/run`, `/root` | binds of `<lease>/rw/{tmp,var-tmp,run,home}`, `rbind,rw,nosuid,nodev` | yes; disk-backed, discarded with the lease (a `tmpfs` would count against the lease's memory) |
| `/etc/hosts`, `/etc/hostname`, `/etc/resolv.conf` | files in `<lease>/etc`, read-only | no |
| `/proc`, `/dev`, `/dev/pts`, `/dev/shm`, `/dev/mqueue`, `/sys`, `/sys/fs/cgroup` | `crun`, from `config.json`; `/dev/shm` sized by `ContainerLimits` (default 64 MiB) | per OCI defaults |
| `/dev/kbf-init` | the container's first process, bound read-only | no |

The overlay options are pinned, not defaulted (`OVERLAY_OPTIONS`): `userxattr` (the overlay is mounted
inside a user namespace), `metacopy=off`, `redirect_dir=nofollow` (the one redirect mode the kernel
accepts together with `userxattr`), `index=off`, `xino=off`. A redirect or stub planted in the upper
layer therefore cannot change what collection reads.

The Command's working directory is inside `/kbf/root`. The driver makes the working directory and each
output's parent in the upper layer; an input that an output path names is moved into the upper layer
whole; a working directory that would be hidden by an input symlink or file is refused. A tree's mount
point is an empty directory of the lease's input root, created while the root is written, and
the classifier never chooses a node that holds or lies under an output (section 4.1).

### 3.2 Image root is read-only

`crun` takes a rootfs directory and does not mount one. The driver does not put an overlay on `/`: the
image's unpacked directory is the root, read-only, and the four directories above are where an action
writes besides `/kbf/root`. A write elsewhere under `/` fails `EROFS`.

## 4. Shared trees

An action's input root is mostly one toolchain, thousands of files that are the same for every action
that uses it. Linking each file into each action's input root costs the same time per file per action.
A large Directory node is instead built once per node in the tree store, and each action that has it
gets the tree's directory as a read-only mount (`kbf_daemon::trees::SharedMount`) at the node's path.
What is left (the directory skeleton above the trees and the small files beside them) is written
through the node's input cache as without trees (hard links to cache entries,
`docs/design/worker-cache.md`).

The store is `<--data-dir>/trees`. It is on under the container driver when the data directory's
filesystem is large enough, with a byte budget of a quarter of the filesystem less the floor it keeps
free (`--data-max-bytes` lowers it). There are no `--tree-*` flags: the other settings are the
defaults `MIN_FILES = 256` and `MAX_MOUNTS = 8` of `TreeConfig`. A store that cannot be opened (a path
with `:` or `,`, a directory another daemon owns) is logged, `trees: off`, and leases then link every
file.

### 4.1 Which Directory nodes qualify

`Trees::materialize` reads the Directory messages of the action's input root, level by level
(`classify::load`), counts files and bytes bottom-up (`summarize`), and picks nodes (`select`). A
Directory node becomes a shared tree when **all** hold (`classify::qualifies`):

- it holds at least `min_files` regular files and symlinks at any depth (default 256);
- the bytes of its regular files fit the store's budget;
- its path contains no `:` or `,` (a mount path stays free of option separators);
- the Command does not name it: neither the working directory nor any output path lies at or under
  the node, and no output path is an ancestor of it (the driver moves an output, whole, into the
  upper layer, mount point and all);
- its digest has a store name (a SHA-256 digest, `<hash>-<size>`);
- it is the **highest** such node on its path: a node is chosen and its children are not; a node that
  does not qualify is walked into and its children considered;
- it is not the root.

At most `max_mounts` trees are taken, the largest by file count first (ties by path); the rest are
linked per action. A walk visits at most 4,194,304 directory places (`MAX_DIRECTORIES`), and all walks
are loops over a `Vec`, so a deep client tree cannot overflow a stack.

Summaries (digest to file and byte counts) are kept in memory in a map of at most 2^20 entries, emptied
when full. A node whose summary is already known to qualify is not read again: that is the walk a warm
node does not repeat. They are not written to disk.

**Planned:** a recurrence rule (a node becomes a tree only after it has been seen in several
distinct actions), a scheduler hint naming a node, and a cap on tree bytes built per minute. As built,
the first action that has a qualifying node builds its tree; a node unique to one action is still
built, held until evicted, and costs a build.

### 4.2 Preparation, and the bulk linker

1. **Single flight per digest** in the daemon: the first `acquire` of a name builds it in its own task;
   others wait for the builder's slot to close and look again (no file lock). A builder that fails or
   is dropped closes its slot and one waiter builds in its place (`TreeStore::acquire`).
2. **Build** in `trees/.tmp-<n>` (`build::build`): the Directory tree is written with the same
   machinery as a lease's input root, `make_directories` for the directories and symlinks and
   `write_files_as` for the files, which are hard links to the cache's entries (so a tree's files are
   the cache's inodes, owned by the daemon user and mode 0444 or 0555). Files are not copied. On
   macOS they are clones left read-only.
3. **Check** (`build::verify`) before the tree is named: the same names of the same kinds as the
   Directory messages, each file's size and executable bit, no group or other write bit, each symlink's
   target; directories are set to 0755. File contents are not re-hashed here: the cache hashes its
   entries when it first uses them after a start.
4. **Name** the tree by `renameat2(RENAME_NOREPLACE)` to `trees/<hash>-<size>`. A name present means a
   whole tree. A crash leaves only `.tmp-*` (and `.dead-*`), removed at the next start. If another
   rename won, the spare is deleted and the existing tree is used.

Directory messages are not cached across leases beyond the summaries above: each lease's walk reads
the messages of the nodes that are not already known.

**Planned:** a separate bulk linker (one blocking thread of `mkdirat` and `linkat` loops over a
pre-fetched tree, with the cache pins taken in one batch) replacing the shared write pipeline for
trees, and pre-warming a tree from the scheduler's placement hint. Today a tree is built with the same
write pipeline as any input root, and a cold node's first actions wait on that one build.

### 4.3 Eviction and accounting

- The store tracks each tree's file bytes and a total. The budget (`max_bytes`) bounds the total.
  Trees that share files with each other are each charged in full: **Planned:** charging each distinct
  inode once.
- A lease holds a `TreeLease`, a pin counted per tree. Only an unpinned tree is evicted, least recently
  used first, when the total is over budget: at a build, when a lease drops its pin, and at start. The
  tree is renamed to `trees/.dead-<n>` first (a new acquire builds afresh under the digest name) and
  removed in the background; a tree that cannot be renamed stays and the next eviction tries again.
- At start the store removes `.tmp-*` and `.dead-*`, and adopts every tree already named, counting its
  bytes. Pins begin at 0: a node that restarts has no running action (the keeper kills them).
- Hard links keep the inode: removing a cache name never frees a file a tree links, so the cache and
  the trees budgets each count bytes the other holds. The cache's free-space floor skips entries with
  more than one link, and stops with a warning if nothing else can be evicted. A tree larger than the
  budget is never chosen.
- Page cache: the tree, a lease's input and the cache entry are hard links to one inode, and
  overlayfs reads through it, so concurrent actions share one page-cache copy of a toolchain.
- Counters (`TreeStats`): trees, bytes, pinned, builds, hits, waits, evictions.

### 4.4 Correctness: an action cannot modify a shared tree

Two barriers, stated as what they are:

1. **A read-only bind mount.** `crun` makes the attach in the action's mount namespace with
   `rbind,ro,nosuid,nodev`. Inside a user namespace a read-only flag cannot be cleared by that
   namespace's root (`mount_namespaces(7)`: locked mount flags).
2. **Ownership and mode.** The tree's files are the cache's: owned by the daemon user, who is not
   mapped in the container, with no group or other write bit. The container's root holds
   `CAP_DAC_OVERRIDE` and `CAP_FOWNER` only over its own user namespace's inodes, so it has no
   override over these.

The test `a_container_cannot_change_a_shared_tree_or_lift_its_read_only_flag`
(`crates/kbf-driver-container/tests/crun_cache.rs`, run by `cargo xtask crun-tests` on a node with
rootless `crun`) has an action try ten writes (create, append, truncate, chmod, mkdir, rename inside
and out, hard-link into the input root, remove, `mount -o remount,rw`), expects every one refused, and
a second lease reads the tree whole.

Also stated:

- The store directory is mode 0711 (the daemon user alone may list or write; others may search). The
  container's ids are "others" to it and must search it to bind a tree; every directory above it must
  be searchable too, and the store logs a warning naming the nearest one that is not. Trees are
  reached by name, and names are digests the container is never told.
- **Symlinks** in a tree stay symlinks and resolve inside the container. Names are checked when the
  Directory messages are read (no `.`, `..`, `/`, NUL, duplicates).
- **Hard links.** A file shared with the cache has `st_nlink > 1`; `link(2)` from a tree path into
  `/kbf/root` fails `EXDEV`. A tool that hard-links an input without a copy fallback fails.
- **`rename(2)` across a tree boundary** fails `EXDEV`; a write into a tree fails `EROFS`, and a write
  to an existing file `EACCES`. An action that writes into a toolchain directory fails where it
  succeeded without trees. A node without a tree store keeps the old behaviour. **Planned:** a per-run
  fallback to per-action links for such an action; there is none today.
- **An output under a tree** disqualifies that tree (4.1), so outputs are always in `upper`.
- The tree directory is written only by the daemon (build, eviction), never through a mount; rename
  then delete means an in-use tree is never edited.
- **Planned:** periodically re-hashing a sample of a tree's files. Today a write through a shared
  inode would not be detected after placement; the barriers above are what prevent it.

## 5. Ownership: helper containers

The container's ids are subordinate ids, so the daemon cannot create, chown or delete the files they own
from outside a user namespace. `crun` makes that namespace too, as a **helper container**
(`oci/helper.rs`): container id 0 is the daemon user and ids `1..=65536` are the subordinate range, so
an action's id X is helper id X+1 (an action runs as 1000:1000, `1001:1001` in a helper;
`CONTAINER_OWNER`). A helper runs one program, never a shell: the daemon's own binary in its
`__chown-dirs` or `__unpack-layers` mode (descriptor-relative, never following a symlink), or
`/usr/bin/find ... -delete` from a read-only bind of the host's `/usr`. It has no network, the same
seccomp and capability set, a small shared root of mount points (`prepare_rootfs`), and the
directories it works on bound at `/t/0`, `/t/1`, ...

A lease uses helpers three times:

1. before the run, `__chown-dirs` gives the lease's `upper`, `work` and `rw` directories (and the
   input root's directories: with the cache, its files are hard links the container reads as
   "others", so only directories are given; without the cache, everything) to the container's ids;
2. after the container has exited, `__chown-dirs` gives them back to the daemon
   user before outputs are collected;
3. at clean, a helper removes what the container's ids still own when the daemon user cannot.

The shared trees need no ownership work: they are never given, only bound read-only. The image is
unpacked by a helper too (6.2).

## 6. Runtime: what kbf writes

### 6.1 The bundle and `crun`

`oci::spec::action_config` builds `config.json` from typed `oci-spec` builders (the table in its
documentation lists each isolation setting next to its line); `oci::hardening` lists the capability
set, masked paths and seccomp filter; `oci::lease` writes the bundle (`config.json` plus `hosts`,
`hostname`, `resolv.conf`). The container's cgroup is `<lease cgroup>/ctr`, made by `crun` in the
daemon's delegated subtree; memory and CPU limits stay on the lease cgroup.

The first process is `kbf-init` (a static binary embedded in the driver, written under the scratch
directory and bound at `/dev/kbf-init`). It runs the action's argv as its child, reaps what the action
leaves orphaned, and reports why a program could not be started in a file the driver reads.

### 6.2 Image store

`oci::image` fetches an image by digest from its registry with `kbf-image` and unpacks it once per
digest into `<scratch>/oci-images/<hex>/rootfs`, read-only afterwards. The manifest must hash to the
pinned digest and be one image, not an index; the config and every layer blob are hashed as written and
must match the manifest. A helper container with the action's user namespace applies the layers
(`__unpack-layers`), so every file is owned by the id the action sees. The first use of an image pays
the fetch (images given with `--image` are prepared at start); a lease then needs one `stat`.
Concurrent first uses share one fetch. No container tool takes part.

### 6.3 `crun` through the `runc` crate

`oci::crun` runs `crun` with the `runc` crate (0.3, async client): `GlobalOpts::command` names the
binary, `--root` is the lease's `crun/` directory (no state shared between actions) and `--log` is
`crun.log`. `Runc::run` waits for the process, so the container's exit status is `crun run`'s own:
there is no `state` query and no window in which a container has stopped but its exit code is not
recorded. The container's stdout and stderr are the lease's files. A `crun run` whose future is
dropped is killed and reaped. `crun delete --force` runs at clean.

### 6.4 `crun` provisioning

The binary named by `--oci-runtime` (and the daemon's own binary) must be a file at start; a node
that names a missing one refuses to start. The node also needs `newuidmap` and `newgidmap` (package
`uidmap`), a subordinate range, and a delegated cgroup subtree. The `.deb` and `.rpm` packages depend on `crun` and `uidmap` (or `shadow-utils`)
(`xtask/src/pkg_install.rs`).

## 7. Concurrency and recovery

- **Keeper.** Each lease has a `kbf-daemon __keeper` process holding the read end of a pipe only the
  daemon has. If the daemon dies the kernel closes the pipe and the keeper writes the lease cgroup's
  `cgroup.kill` (repeatedly, for a `crun run` the dead daemon left in flight). The container, cgroup and
  scratch directory stay for the next start's sweep.
- **Clean on every path** (success, failure, timeout, kill, a dropped future): kill the lease cgroup,
  make sure `crun run` has ended, `crun delete --force`, remove the cgroup and the scratch directory,
  then verify neither remains. A clean that fails turns the lease into a failure.
- **Trees under a lease** are pinned until the lease is gone (the lease's `trees` field), so eviction cannot take
  a tree an action has mounted; a mount source in use would be `EBUSY` anyway.
- **Not built:** re-adopting running leases after a daemon restart.

## 8. macOS

`Delivery::Clone`: macOS has no bind or overlay mounts. The store's trees are built from clones of the
cache's entries (read-only, as the cache's own), and an action's copy of a tree is **one `clonefile`**
of the tree's directory into its input root. The clone is the lease's own (copy-on-write, its own
inodes, moded like the other inputs), so an action may write it and the tree stays as it was; the store
lies outside every lease directory, where the sandbox lets nothing be written. A volume that refuses the
clone (`ENOTSUP`, `EXDEV`) gets the tree's files one by one and the refusal is logged once per node.
The native driver on Linux never uses the cache or the trees.

## 9. Planned, and what is not measured

Everything below is **Planned:** (not built); each also appears in the sections above.

- **Measurement.** No benchmark of the shipped backend is in this repository. The draft of this
  document was written against a `podman` backend that no longer exists; its latency figures described
  that backend and are not claimed for this one. A probe of `crun`'s floor, of a tree build against
  the per-file path, and of the first minutes after a deploy (a cold node waits on one build per
  tree) has not been run.
- A recurrence rule and a per-minute build cap for trees (4.1).
- A dedicated bulk linker, and pre-warming from a scheduler hint (4.2).
- Charging each distinct inode once in the trees budget (4.3).
- A per-run fallback to per-action links for an action that writes into a toolchain (4.4), and
  sampled re-hashing of tree files (4.4).
- Tree-specific flags (`--tree-*`): none exist; the store is sized from the filesystem (section 4).
- A tmpfs for `/tmp`: the writable directories are on disk by design (3.1).
- Re-adopting leases after a daemon restart (section 7).
- Networked actions: an action's network namespace has loopback only.

## Sources

- `crun`: https://github.com/containers/crun; the `runc` crate (containerd/rust-extensions):
  https://docs.rs/runc/latest/runc/; `oci-spec`: https://docs.rs/oci-spec/latest/oci_spec/
- Kernel overlayfs (unprivileged mounts, `userxattr`, `metacopy`): https://docs.kernel.org/filesystems/overlayfs.html
- `mount_namespaces(7)` (locked mount flags), `user_namespaces(7)`: https://man7.org/linux/man-pages/
- Ubuntu AppArmor user-namespace restriction (the distribution's profile for `/usr/bin/crun`):
  https://discourse.ubuntu.com/t/understanding-apparmor-user-namespace-restriction/58007
- Code read for this revision: `kbf-daemon/src/trees/{mod,classify,build,store,place}.rs`,
  `kbf-driver-container/src/{runtime,container,keeper,remove}.rs`,
  `kbf-driver-container/src/oci/{mod,spec,hardening,helper,image,init,lease,crun}.rs`,
  `kbf-node/src/data_dir.rs`, `kbf-driver-container/tests/crun_cache.rs`.
