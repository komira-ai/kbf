# One server, one daemon, one process: libraries, three binaries, a shared cache

Status: **proposed. Phase 0 is safe to build now; phases 1 and 2 are gated on the
measurement in section 11.** Nothing here is built. No wire message changes in any phase:
a remote daemon sends what it sends today, and a server accepts what it accepts today.

This replaces an earlier Unix-socket design for the same host (section 12). Its reviews'
findings (transport not the measured cost; typical action not helped; the user boundary;
OOM and crash blast radius) are answered in sections 2, 7 and 11.

## 1. Decision

1. **Server and daemon become libraries.** `kbf-server` and the daemon each expose
   "start me with these arguments and a shutdown future" and a handle to what they
   serve. Their `main` functions shrink to argument parsing, logging and signals.
2. **Three thin binaries**, one per deployment shape:

   | Binary | Runs | For |
   |---|---|---|
   | `kbf-server` | the server only | storage nodes of the target farm |
   | `kbf-daemon` | the daemon only; data plane over gRPC | build hosts, Mac nodes, and any host whose server is elsewhere |
   | `kbf-all-in-one` | both, in one process; data plane in-process | the one-node farm, dev boxes, small converged hosts |

3. **One data-plane interface, two implementations**: remote (today's transfer engine
   over mutual TLS) and in-process (direct calls into the server's cache and segment
   code). The daemon is generic over the interface and cannot tell which it has.
4. **The control plane stays gRPC** (`Session`, leases, heartbeats) in every binary. In
   all-in-one it runs over loopback, because it is small, not on the measured path, and
   keeping it the same means one scheduler, one commit path and one set of sim
   scenarios. An in-process control plane is not designed here and not needed.
5. **One CPU flag**, `--reserve-cpus`, on every binary. No pinning. What the reserve does
   not cover goes to builds (section 6).
6. **One conformance suite** for the data plane, run against both implementations
   (section 8).
7. **Measure first.** The in-flight do-less PRs (section 11) remove most of what this
   would otherwise save. Phase 0 changes no behaviour and is worth doing regardless;
   phases 1 and 2 each have a number that must be met.

## 2. What is known, and what is not

One action outputting a toolchain tree (15k files, 307 MB) spent 18.3 s in its outputs
phase: walk 7.8 s, spool 4.3 s, upload 6.0 s, measured once on `main` before the PRs of
section 11. The 6.0 s is about ten 32 MiB uploads sent one after another (Begin, ranges,
Commit, which hashes, syncs the tier and commits metadata): as consistent with latency in
series as with bandwidth, so **not shown to be transport**. The typical action differs: a
160 to 480 MB input root (a 15k-file toolchain tree that 63% of actions share), hard
links, per-directory fetches and ownership passes, 96% of blobs already in the node cache
(`worker-cache.md`), tiny outputs. None of that crosses a network; an in-process plane
does not change it. The directory cache and link plans do.

So the case for all-in-one is not mainly speed. It is: **one binary to deploy and
supervise on a single node, one place that decides how many CPUs builds get, one cache
budget instead of two, and no loopback hop and no second copy of bytes that never leave
the machine.** Section 11 says which of those must show up in numbers before phases 1
and 2.

## 3. The crate split

Today (read from the tree):

- `kbf-server` is already a library plus a 200-line `main.rs`. The `main` still holds
  the assembly: `start` (build the store and metadata log from flags), `run` (open the
  `Cache`, attach the write tier, start the flusher, bind the listeners), the start line
  and the signal handlers.
- The daemon is two crates. `kbf-daemon` is the library (session, leases, cache,
  report). The binary is in `kbf-node/src/main.rs`, 1193 lines, because it names the
  drivers and the drivers depend on `kbf-daemon`. It builds the concrete
  `Arc<Transfers>` and hands it to the container and native runtimes, which are
  generic over `kbf_transfer::Cas`.

Target:

| Crate | Becomes | Holds |
|---|---|---|
| `kbf-server` (lib) | `kbf_server::start(args, shutdown) -> ServerHandle` | store and metadata-log selection, `Cache::open`, tier attach, flusher, listeners. `ServerHandle` has the bound addresses, the `serving` future, and `data_plane()`: the `Arc<Cache>` plus the `SegmentsService` behind an adapter (section 4). The start line and `Routes` move with it. |
| `kbf-node` (lib) | `kbf_node::start(args, data_plane, shutdown) -> NodeHandle` | everything in today's `main.rs` below argument parsing: driver and runtime setup, cgroups, cache open, the `Daemon`. Generic over `C: Cas + DataPlane`. The `--cas` flag, TLS flags and `Transfers` construction move into `RemoteDataPlane::connect(args)`. |
| `kbf-server` (bin) | `kbf-server` | `Args::parse`, tracing, signals, `kbf_server::start`, print the start line. |
| `kbf-node` (bin) | `kbf-daemon` | `Cli::parse`, tracing, signals, `RemoteDataPlane::connect`, `kbf_node::start`. |
| `kbf-all-in-one` (new crate, bin) | `kbf-all-in-one` | one `Cli` flattening both argument sets, tracing, signals; `kbf_server::start`, then `kbf_node::start` with `server.data_plane()`. The only crate that depends on both libraries. |

Rules the split keeps, each with a check:

- **Neither library depends on the other.** `kbf-server` must not name drivers; `kbf-node`
  must not name `kbf-front` internals. Only `kbf-all-in-one` sees both, and the data plane
  type crosses through a trait in `kbf-transfer` (section 4). `layering.rs` gains the rule
  and a planted-edge mutant.
- **Binary names and packaging do not change** for the first two; `kbf-all-in-one` is a
  third artifact, signed and attested the same way.
- **A refactor, not a rewrite.** `kbf-node`'s `main.rs` also drops under the 1000-line
  budget. Merging `kbf-daemon` and `kbf-node` is the crate-layout question (#634).

## 4. The data-plane interface and its two implementations

The interface is what drivers already use: `kbf_transfer::Cas` (`get`, `put`,
`put_chunks`, `read_to`, `missing`, `put_spool`, `lease`) plus the segment-range read the
cache fill uses (`read_located`, today an inherent method of `Transfers`). Phase 0 puts
that method on a small `DataPlane: Cas` trait so nothing in the daemon names `Transfers`.

| | `RemoteDataPlane` | `LocalDataPlane` |
|---|---|---|
| Bytes move | `Transfers` over one mutual-TLS HTTP/2 connection to the worker listener | function calls into the server's `Cache` and `SegmentsService` in the same process |
| Authentication | the daemon's certificate, admitted per call by `identity.rs` | none on the wire; the daemon is constructed with the server's handle. The deny list still applies: each call is admitted for the daemon's own node id through the same `admit_call` check, so cordon and deny edits refuse the next call as they do today |
| Integrity | the server hashes what it receives (`VerifiedBlob::new`, the per-record check in `Receiving`) | **the same functions**, called directly. The in-process plane never constructs a `VerifiedBlob` without hashing, never passes a "trusted" flag, and has no code path that skips `check` |
| Upload ownership | `Connections` keyed by the peer `SocketAddr` | keyed by a new `Owner::Local(u64)`, one per `LocalDataPlane` (idle timeout, drop-on-close and admission release hang on it as for TCP) |
| Copies | file, spool read, protobuf encode, TLS encrypt, kernel, decrypt, decode, into the server's buffer | spool read into the server's buffer; no encode, no TLS, no kernel hop |

Why this is direct calls, not an in-memory channel: every `Segments` handler in
`SegmentsService` is a unary `fn(&self, Request<T>) -> Result<Response<U>, Status>` over
prost structs, so the in-process adapter builds the request struct and calls the
handler. The struct is moved, not serialized. The `remote_addr` the handlers read to find
an upload's owner becomes an `Owner` passed beside the request (Phase 0, the one change
inside `kbf-front`: `Connections::of(Option<SocketAddr>)` becomes `of(Owner)`). A tonic
`Channel` over a `duplex` pipe would keep HTTP/2 framing, protobuf and flow control, and
save only TLS: that is the transport the project owner rejected.

What `LocalDataPlane` does not do: skip the server's checks, hold the cache's state
outside the server, or let a daemon-supplied path or descriptor reach the server. It
passes `Bytes`; the server hashes them into its own memory and stores that same buffer.
A digest mismatch is refused and indexes nothing, as for a remote daemon.

## 5. The shared cache

Two stores exist today, and they are different shapes, which limits what "one cache"
can mean:

- **The server's tier** (`kbf-tier`): block files `<epoch>-<seq>.blk` holding records
  **packed at byte offsets** inside an object that will be flushed to the object store as
  a segment. Unflushed blocks are the only copy and are never evicted; flushed `.seg`
  copies are evicted least-recently-read first when a reservation needs room.
- **The daemon's node cache** (`worker-cache.md` section 6): **one file per blob** under
  `blobs/`, 0444/0555, filled by the daemon alone (download into `tmp/`, hash, rename),
  hard-linked into leases, evicted by least recent use past `--cache-max-bytes`, with
  entries a running lease links pinned.

A packed record cannot be hard-linked: a link names a whole file. So "inputs are linked
from the tier" cannot mean linking out of a block. What can be shared, in order of how
much it changes:

1. **Fill from the tier without the network (phase 2a).** On a node-cache miss the
   daemon's fill reads the blob's record from the server's block or `.seg` copy through
   the in-process plane (`read_located` as a local call), hashes it while writing into
   `tmp/`, and renames it into `blobs/`. The rule "only the daemon writes `blobs/`" holds
   unchanged. Saves the loopback `ReadRange`, TLS and one copy on a miss; nothing on a
   hit. Given 96% hits, expect this to matter only for cold toolchain bumps.
2. **Outputs straight into the cache (phase 2b).** Section 6.7 of `worker-cache.md`
   already moves an uploaded output into `blobs/` by rename when the action is dead, the
   file is regular and `st_nlink == 1`. In all-in-one the same output is also staged on
   the tier (the server needs its bytes in a segment for durability and flush). So the
   output is read once for the upload and then renamed into the cache: no second download
   on the next action that consumes it, which is the case the native-cache copy mode
   cannot serve today. This needs the cache and the scratch directory on one filesystem,
   as 6.7 already requires.
3. **One budget (phase 2c).** `--cache-max-bytes` and `--write-tier-bytes` stay two
   limits, but all-in-one adds `--storage-max-bytes` over the filesystem they share, and
   evicts in this order when a reservation does not fit: cache entries no lease pins,
   then flushed `.seg` copies. Unflushed tier blocks are never evicted (the existing rule),
   and a blob present in both the cache and a `.seg` copy is held twice on disk; a
   `.seg` copy is dropped first because the cache entry is the one a lease can link. This
   is an accounting change, not a new store.

**Not done:** one content-addressed file store replacing both; the tier packs records so
one segment is one PUT and one commit. A per-blob view over packed records would be its
own document, if a measurement asks for it.

**Ownership with one user.** Actions still cannot write the cache: in a container's user
namespace the file's owner is unmapped (`worker-cache.md` 6.1), and the tier directory is
never mounted. What goes is the user boundary between the tier and an escaped action;
section 7 says what replaces it and where that stops.

**Eviction.** A hard link keeps an evicted file alive for running leases, so eviction
needs only today's pins. The shared budget in 2c is a counter both sides consult, not a
lock held across both.

**Native driver:** its actions run as the daemon's user and could write the cache, so it
never uses one (`cache.rs` already warns `native-same-uid`). All-in-one is Linux and
container driver only; `kbf-all-in-one --driver native` is refused at start. Macs run
`kbf-daemon`.

## 6. `--reserve-cpus`

**Meaning.** The number of CPUs the farm's own work may use on this host. Everything
else belongs to builds. It is a count, not a set: nothing is pinned (`AllowedCPUs=` and
`cpuset` stay unset, as `linux-build-host.md` says), because idle CPU should go to whoever
can use it.

**What it does, in three places, in every binary:**

1. **What the scheduler sees.** The node report's `cpus` is `host CPUs - reserve`
   (through `Capacity::within`, which already lowers `cpus` and `mem_gib`). The scheduler
   books leases against that, so a full host leaves the reserve free.
2. **How the process's CPU-heavy work is bounded.** Hashing, spool copy, CRC and record
   verification run on a bounded pool of `hash_threads` threads, not on the shared
   blocking pool (which also does `fdatasync` and file I/O and may be large). The async
   runtime's worker count is set from the reserve.
3. **What the kernel does under contention.** The process's cgroup gets
   `cpu.weight = cpu_weight(reserve x 1000)`, the same function that weights each lease
   from its booked CPU (100 per core), so under load the farm's work gets about the share
   it reserved against the leases' shares. Uncontended, nothing is capped.

**Pool sizing from the reserve `R` (initial rules; section 11 may change the constants):**

| Pool | `kbf-server` | `kbf-daemon` | `kbf-all-in-one` |
|---|---|---|---|
| Share of `R` | all of it | all of it | server `ceil(R/2)`, daemon `R - ceil(R/2)`, each at least 1; `R >= 2` required |
| Async worker threads | `max(2, ceil(share/2))` | same | per side, same rule, on one runtime per side |
| Hash/verify pool | `share` threads | `share` threads | one pool of `R` threads **shared**, since hashing is fungible |
| Blocking I/O pool | fixed bound, not CPU-derived | same | same |
| `cpus` reported | not reported (no daemon) | `host - R` | `host - R` |
| Default `R` | 4 | 2 | 6 |

The defaults are placeholders to be replaced by section 11; the rule that matters is that
the all-in-one default is the sum, so moving from split to all-in-one on one host leaves
builds the same CPUs.

**Overrides.** `--hash-threads`, `--async-threads` and `--blocking-threads` win over the
derived values; the start line prints what was derived and overridden (`reserve=4 async=2
hash=4`). A deploy sets `--reserve-cpus` and nothing else; a value over `R` is accepted
with a WARN. A reserve at or over the host's CPUs is refused at start, and `R < 2` in
all-in-one too. A cgroup that cannot be written (no delegation) is logged: the pools are
still bounded and only the kernel share is lost, which is today's behaviour.

## 7. Isolation and blast radius

The previous review of this idea found four facts to answer. They are real.

**The user boundary goes away.** Today `kbf-server` owns the tier (0700), the metadata
log, the API token and the S3 key; `kbf` runs the daemon and the actions. An escape from
an action lands as `kbf`, which cannot read those. In all-in-one one process has all of
them, and an escape lands as the same user. This design does not claim to restore that
boundary. It narrows the path:

- **Rootless containers and cgroups stay the isolation of the action.** Actions run in
  rootless Podman user namespaces with `pids.max`, memory and `cpu.weight` limits
  (unchanged); an action cannot see the tier, the cache's write path or the process's
  memory by anything but an escape.
- **Secrets are not on disk readable by the build user.** In all-in-one the S3 key and
  API token are delivered by the service manager's credential mechanism
  (`LoadCredential=`), not environment or a file in the user's home, and the unit sets
  `ProtectProc=invisible` and `PrivateTmp=` so a process outside the unit cannot read
  `/proc/<pid>/environ` or the credential path. With `kernel.yama.ptrace_scope >= 1` a
  process that is not a descendant cannot attach to the farm process. **This is a claim
  to verify** by a probe on a farm VM (read the credential and `/proc/<pid>/mem` as the
  same user from inside a container and from outside the unit); the doc does not assume
  it. `check-units` asserts the unit settings.
- **The server still verifies every digest.** A compromised daemon half of the process
  can write only bytes whose hashes the server computes itself, so it cannot poison the
  CAS by sending bytes under another digest. This is the property that survives merging.
- **What it does not protect:** an escaped action running as the farm user, inside the
  process's reach, can in principle obtain the tier and the credentials by a kernel bug.
  The split removes that; all-in-one does not.

**Memory and the out-of-memory killer.** Actions run in the `actions` cgroup with
`memory.max` and per-lease caps, so an over-limit build is killed inside its cgroup, not by
a global kill that may pick the farm process. The farm process sits in the supervisor leaf
with `memory.min` (raised in all-in-one to cover the server's cache and hashing buffers)
and `OOMScoreAdjust=` making it the last candidate: the split deployment's protection on
a converged host (`storage-node.md`), for one process. It fails if
`--actions-memory-max-gib` is sized so builds can use the farm's memory; `check-units`
checks the converged host's memory sum today and refuses all-in-one when it does not hold.

**A daemon crash takes the server down.** One process means a panic, a failed
assertion, a driver fd or thread exhaustion, or an abort in one half ends both. What
softens it:

- Tokio contains a panic in a spawned task (the workspace sets no `panic = "abort"`). The
  process ends on a panic outside a task, an `abort` or the OOM killer. Phase 1 adds a
  test that a panic in a driver task leaves the process serving.
- Container processes are the likeliest source of resource exhaustion, and they are in
  their own cgroup (`pids.max`, `nofile`, `nproc`), not in the farm process's.
- Restart recovers: the server's log is durable, leases are fenced, the daemon's cache
  rebuilds from `blobs/` (6.8). A restart costs the lease handover grace (`daemon.md`), as a
  server restart does today.

The honest consequence: **in a farm with several servers, a crash of the daemon half
restarts a server that may be the leader** and costs an election. That is why the target
farm runs split.

**When to use which.**

| Use | When |
|---|---|
| `kbf-all-in-one` | one node, a dev box, a small farm whose actions come from authenticated first-party clients, where a restart of everything is acceptable and there is no second host to protect |
| `kbf-server` + `kbf-daemon` on one host (converged) | several hosts, any HA servers, anything where a build host crash must not cost a leader, or where the user boundary around the credentials is wanted |
| `kbf-server` and `kbf-daemon` on separate hosts | the target farm; untrusted or multi-tenant work; Mac nodes |

The single binary is a deployment choice that costs isolation. A farm outgrows it by
changing the unit, not the code.

## 8. Tests: one suite, both data planes; sim coverage

**A conformance suite** in `kbf-transfer` tests, written against `DataPlane` once and run
for each implementation. `RemoteDataPlane` runs against an in-process server over a real
connection; `LocalDataPlane` against the same server's handle.

Cases both pass: put and get of an empty blob, 1 byte, 1 MiB and one over the segment
cap; `missing` before and after a put; a segment upload and a `read_located` of it; a
blob already present; an upload whose record bytes do not match their digest (refused,
nothing indexed, and no retry that repeats it); two concurrent uploads; an upload
abandoned on drop releases `--write-bytes-in-flight`; a deny-list edit mid-session
refuses the next call; the same sequence yields the same metadata (the final index is
compared between the two planes).

Each test names the mutant that turns it red:

| Test | Mutant |
|---|---|
| digest mismatch refused on `LocalDataPlane`, index unchanged | the local adapter constructs `VerifiedBlob` from the caller's claim, or skips `check` |
| deny-list edit refuses the next local call | local admission cached at construction |
| dropped local upload releases the write budget; a stalled local upload does not starve remote ones | release missing for `Owner::Local` |
| both planes produce the same index | one plane skips the footer or the staging commit |
| `kbf-all-in-one` starts and an action's outputs appear in the CAS with zero TCP `PutRange` bytes | the daemon is wired to `RemoteDataPlane` by mistake |
| `kbf-server` alone has no driver crate in its dependency tree; `kbf-node` none of `kbf-front` | `layering.rs` rule removed |
| a panic in a driver task leaves the process serving | driver run on the farm's main task |
| `--reserve-cpus` over the host, or `R = 1` in all-in-one, is refused | validation removed |
| derived pool sizes match section 6 for each binary at `R` in {2, 4, 6, 16}; reported `cpus` is host minus reserve; supervisor `cpu.weight` is `cpu_weight(R x 1000)` (fake cgroup root) | all-in-one sizes each side by the whole `R`; reserve ignored by the report; weight not written |
| native driver with all-in-one is refused | the check removed |
| cache fill from the tier writes through `tmp/` and the rename, and a corrupted tier record is refused at the daemon's hash | fill copies straight into `blobs/`, or trusts the tier's checksum |
| shared budget evicts unpinned cache entries, then `.seg` copies, never an unflushed block, never a pinned entry | eviction order swapped |

**Real two-process path.** `kbf-it` runs the split pair and `kbf-all-in-one` through the
same action (multi-file output, a cache hit on a second run) and compares CAS contents.
`deploy_units.rs` renders the all-in-one unit and runs its flags through the parser.

**Simulation.** `kbf-sim-cell` has a `FakeDaemon` and a fake server on a modelled
network. Phase 1 adds a data-plane mode to `FakeDaemon` that bypasses the network model
for bytes and keeps it for the control plane, with injectable faults (crash of the whole
process between staging and answering, restart with a stale session). The existing
properties 1 to 9 hold unchanged in both modes. Added: **A1** after any whole-process
crash, every upload either completed whole or left nothing indexed; **A2** the final index
of a run is equal between the network mode and the in-process mode for the same seed. The
sim server hashes by construction, so a sim property does not test that `kbf-front` hashes;
the digest-mismatch test above, with its mutant against the real code, does.

## 9. Deploy

- Today the one-node farm runs the two units on one host (the converged shape). Phase 1
  moves it to one `kbf-all-in-one` unit with `--reserve-cpus` replacing the two units' CPU
  settings; reverting is restoring the two units (state, flags and wire are the same).
- The target farm runs `kbf-server` on storage nodes and `kbf-daemon` on build hosts and
  Macs, each with its own `--reserve-cpus`. No change from this document except the flag.
- `kbf-deploy` gains a shape: `all-in-one`, rendering one unit. The existing `converged`
  rendering (both units) stays. `check-units` asserts for all-in-one: the credential and
  `ProtectProc` settings of section 7, the memory sum, `OOMScoreAdjust`, delegation,
  no native driver, and that `--reserve-cpus` is below the host's CPUs.
- `install-units` enables the unit; a host changes shape by an explicit step, never as a
  side effect of an update (`kbf-updater`).

## 10. Migration phases

**Phase 0: library seams and the shared interface. No behaviour change.** Reverts by
`git revert`.

- `kbf_server::start`/`ServerHandle` and `kbf_node::start`/`NodeHandle`; both mains thin;
  existing tests pass unchanged.
- `DataPlane: Cas` with the segment-range read; `RemoteDataPlane` wraps `Transfers`; the
  daemon is generic over it. `Owner` in `Connections` and the handlers.
- `--reserve-cpus` in `kbf-server` and `kbf-daemon` with the derived pools; unset by
  default, so reported `cpus` does not change until a deploy sets it.
- The conformance suite, for `RemoteDataPlane` only.
- Rebases onto the in-flight PRs of section 11: the contract the local plane needs is "a
  put feeds the server's incremental per-record check", not a function name.

**Measurement gate (section 11).** Run on the farm after the do-less PRs land.

**Phase 1: the all-in-one binary with in-process outputs.** `kbf-all-in-one`, the
`LocalDataPlane` for puts (outputs, stdout, stderr) and gets. `kbf-deploy` shape, unit, `check-units` rules, the whole-process-crash sim
mode, the driver-panic test. Reverts by switching the host back to the two units.

**Phase 2: the shared cache for inputs.** 2a fill from the tier, 2b outputs renamed into
the cache in all-in-one, 2c the shared budget, in that order, each behind its own
`--shared-cache` flag value (`fill`, `outputs`, `budget`) so each is measured and reverted
alone. The cache keeps its writer, its index and its restart rules.

Each phase is independently shippable; a later one does not rely on an earlier one's
flag. No phase changes a message a remote daemon sends.

## 11. The measurement gate

The PRs in flight that change this exact path: #654 (spool copy on the blocking pool, no
CRC-32C), #655 (up to four segment uploads at once, hardware CRC), #656 (each record
hashed as its `PutRange` arrives, so Commit hashes nothing), #653 (the `outputs phases`
line), and the thread flags proposed alongside. Once these land, measure on the one-node
farm and a split pair of hosts, same toolchain tree and same action:

| # | Measurement | Where it comes from |
|---|---|---|
| M1 | the `outputs phases` line for the 15k-file output (walk, hash, FindMissing, spool, upload) | #653, new baseline |
| M2 | server stage timings for that upload: bytes-in, Commit split into hash, tier write, `fdatasync`, `PutStaged` | server upload-path metrics |
| M3 | `PutRange` throughput over loopback: 16 streams, 4 MiB ranges, TCP+TLS vs a null sink vs an in-process call | `kbf-bench` (`connect.rs` exists); the in-process arm is new |
| M4 | CPU model, `sha_ni`, single-core SHA-256 and CRC-32C throughput on build hosts and storage nodes | one-off |
| M5 | typical action: wall time split into input link, ownership passes, run, outputs; node-cache hit rate; bytes filled per action | `worker-cache.md` metrics |
| M6 | CPU the farm's processes use (server and daemon) during a loaded hour, and the 95th percentile | process metrics, cgroup `cpu.stat` |
| M7 | memory high-water of the server and daemon on a loaded hour | cgroup |
| M8 | cold-miss fills: count, bytes and time per hour on the one-node farm | cache fill metrics |

**What justifies each phase** (thresholds are proposals for the project owner to set
before the run, so the result cannot choose its own bar):

- **Phase 0:** none. It is a refactor that makes the later choices cheap and the flag
  worth having. Do it.
- **Phase 1** needs at least one of: M3 shows TLS+framing+copy is at least 25% of M1's
  upload time after the PRs; M6 shows the server's TLS and protobuf CPU is at least one
  core during a loaded hour on a one-node farm; or the operational case (one unit,
  one CPU flag) is wanted for its own sake on the one-node farm. If M2 shows Commit
  (hash, `fdatasync`, `PutStaged`) is over 70% of upload time, transport is not the cost,
  and phase 1 is justified by the operational case alone, stated as such.
- **Phase 2a** needs M8 to show cold fills over a threshold of node time (proposal: 10%
  of a host's action wall time). With 96% hits it probably does not.
- **Phase 2b** needs M5 to show that consumed-after-produced outputs (an output that is
  an input of a later action on the same node) are a measurable share of misses.
- **Phase 2c** needs M7/disk accounting to show a host that runs out of one budget while
  the other is slack.

If M1 after the PRs is under about 3 s and M5's typical action is dominated by links and
ownership passes, close phases 1 and 2 as "not worth it" and take the topology-independent
items: spool-less upload planning, the walk (7.8 s), and the directory cache and link
plans of `worker-cache.md`.

## 12. Rejected alternatives

**A Unix-socket `Segments` channel between a daemon and a server on one host** (the
first version of this document, #657). Rejected as the main design by the project owner
("the unix socket is a bit inefficient too"), and by its own reviews:

- It keeps a transport: protobuf framing, a kernel copy and flow control, even without
  TLS. The measured cost was not shown to be transport (section 2).
- It needed new authentication the existing admission code does not give: a session
  token, a peer-uid check on both sides, a per-call deny-list check, a socket path from the
  daemon's flag, pre-authentication limits. Each is a permanent surface.
- It was dead code in the target farm and left two binaries and two CPU settings on the
  one box.

**File descriptors over a socket:** a framed protocol, `SCM_RIGHTS`, filesystem
allowlists and a TOCTOU analysis for one saved read. **A shared content-addressed file
store replacing the tier and the node cache:** discards the packed segment (one PUT and one
metadata commit per upload). **An in-process control plane:** nothing measured points at
it, and it would fork the scheduler's commit path from the split deployment's. **Reflink
or `copy_file_range` into the tier:** block alignment and cross-filesystem behaviour make
it unreliable, and copy-then-hash is TOCTOU-unsafe.

## 13. Open questions

1. **For the project owner: the thresholds in section 11**, set before the run.
   Recommendation: as written.
2. **Move the one-node farm to `kbf-all-in-one` in phase 1 for operational reasons
   alone** (one unit, one CPU flag), even if M3 and M6 show no transport saving?
   Recommendation: yes, say so in that change; it reverts by a unit change.
3. **Is `LoadCredential` plus `ProtectProc` enough in place of the user boundary on one
   node?** A claim with a planned probe, not a fact. Recommendation: no non-first-party
   work on all-in-one until the probe has run and a security review has read it.
4. **Default `--reserve-cpus`** (4, 2, 6) are placeholders until M6.
