# Colocated fast path: one code path for a daemon on the server's host and on another

Status: **proposed, gated on a measurement (section 8).** Nothing here is built. It adds
a second transport for the bytes between a daemon and a server on the same host and
changes no wire message a remote daemon sends.

The farm will run in two shapes from one source tree. Today a server, its MinIO and a
daemon share a host. Later, servers and MinIO run on storage nodes and daemons on
separate build hosts and Mac nodes. This document says how both are served by the same
binaries, the same flags and the same tests, what a daemon next to its server can skip,
and under what conditions it is worth building at all.

## 1. The problem, and what is not yet known

One action that outputs a toolchain tree (15k files, 307 MB) spends 18.3 s in its
outputs phase: walk 7.8 s, spool 4.3 s, upload 6.0 s. Walk and spool run on the
daemon's host. The upload sends the spool to the server as `Segments` `PutRange` calls
over TLS and HTTP/2 (`kbf-transfer/src/segments.rs`); the server checks the bytes and
stages them on its tier (`kbf-front/src/segments.rs`, `Commit`).

Two things limit what we can claim from that number:

- **The 6.0 s is not shown to be transport.** It is about ten 32 MiB uploads, and
  `upload_spool` sends them one after another. Each is Begin, ranges, Commit, and Commit
  hashes the upload, syncs the tier and commits to the metadata log. 0.6 s per upload
  fits latency stacked in series as well as it fits bandwidth (51 MB/s is far below what
  TLS costs on one core).
- **The baseline is moving.** Unmerged branches already change this exact path:
  `daemon/segment-uploads-in-flight` (several uploads at once),
  `server/commit-overlap` (records hashed as their ranges arrive, so Commit hashes
  nothing) and `daemon/parallel-spool-hash` (spool re-hash on several threads).

So section 8 makes a measurement the first step, and every gain below is an estimate to
be replaced by it.

The typical action is different and the lane does not help it: a 160 to 480 MB input
root, mostly a 15k-file toolchain tree that 63% of actions share, tiny outputs, 96% of
blobs already in the node cache (`worker-cache.md`). Its time is hard links, per-
directory fetches and the container driver's ownership passes over root, upper and work
directories (`worker-cache.md`, the container lease). None of that crosses a network.
The directory cache and link plans in `worker-cache.md` are the larger lever for 63% of
actions; this lane does nothing for them.

## 2. Decision

**One code path. The topology is discovered when a session starts, never configured per
host.** Four rules:

1. **The control plane is always gRPC and always remote-capable.** `Session` (Hello,
   Welcome, leases, heartbeats), the action cache, metadata and scheduling are the
   existing protocol and server code in every topology. No second scheduler, no second
   commit path, no "embedded" mode of the server's logic.
2. **The data plane is behind the `Cas` trait** (`kbf-transfer/src/cas.rs`), as now. The
   transfer engine gains a second `Channel` for the `Segments` service: the same
   service, the same messages (`Begin`, `PutRange`, `Commit`), the same `Receiving` and
   `check` on the server, over a **Unix socket, without TLS**. The engine picks per
   upload: local channel if ready, else the TCP one. Callers (drivers, `kbf-outputs`, the
   tree materializer) do not change and cannot tell.
3. **The server offers; the daemon uses it only if it is provably the same host and the
   same server.** A server started with `--local-socket=<dir>` puts a host identity, a
   server instance and a session token in `Welcome`. The daemon connects only if the
   host identity equals its own, to a path from its own flag, and only after checking who
   owns the socket.
4. **Any failure runs the daemon as a remote one does.** The lane is an optimization with
   a per-operation fallback.

Why a Unix-socket gRPC channel and not file descriptors passed over the socket (this
document's first draft): descriptors save one daemon read and one kernel copy per byte
(estimate 0.2 to 0.4 s per 307 MB, unmeasured) and cost a custom framed protocol,
`SCM_RIGHTS`, `unsafe`, descriptor-type and filesystem allowlists, a TOCTOU analysis and
a memory-pinning surface (appendix A). Using the existing service removes all of that:
the conformance suite differs only by transport, and the server's integrity check is the
code the remote path already runs. Descriptors stay available as a later change if the
measurement shows the copy matters; appendix A records what that change must satisfy.

**What the lane is for, stated plainly.** In the target split farm every daemon is
remote, no `LocalEndpoint` is offered to it, and no local code runs. The lane serves the
single box (dev and small farm) and a converged farm where each storage node also builds.
Its permanent cost is a second connector, an authorization path, a failure table, a
deploy check and tests on every PR. It is a convenience for small deployments, sized as
one, and it must earn its place by the measurement.

## 3. Topologies and what each does

| Topology | Server and daemon | `Welcome` carries endpoint | Segments transport |
|---|---|---|---|
| A. Single box (today; dev; small farm) | same host, one user or two | yes, host identity matches | local socket |
| B. Split (target) | servers on storage nodes, daemons on build hosts and Mac nodes | no, or host identity differs | TCP with TLS, as today |
| C. Converged, several hosts | each storage node also runs a daemon | yes, from the leader the daemon is connected to | local only when the leader is on the daemon's host |

How one source serves all three:

- **Host identity decides, silently.** `LocalEndpoint` carries the server's host id
  (Linux `boot_id`; a platform without one offers nothing and its daemon, which has no
  own id, never tries). A daemon whose own host id differs skips the connect entirely.
  In topology C a daemon whose leader is on another host therefore does not touch its own
  node's follower socket and does not log a warning. A real failure, a matching host with
  a refused handshake, stays loud.
- **The daemon needs no per-host setting.** `--local-fast-path=auto` is the default and
  correct everywhere. `off` and `require` exist for operators (section 9).
- **HA** (`deployment-topology.md`): only the leader owns daemon sessions, so only the
  leader's host offers the lane. A follower accepting uploads for a local daemon is out
  of scope.
- **Mac nodes** are topology B. The local code is plain tokio `UnixStream` and compiles
  on macOS; the `native-macos` CI job builds it, and a test proves a Mac-shaped daemon
  (no host id) makes no connect attempt.
- **A different server on the same host** cannot be mistaken for the session's server:
  one server process owns a given socket path per host, the token is minted per session
  and bound to `server_instance`, and the daemon checks the socket owner (section 4).

## 4. Authentication and authorization (new code, with its own tests)

This is not the existing admission code reused. `identity.rs` admits a call from the
client certificate tonic reads off the TLS connection; a Unix stream has none.
`Connections` in `kbf-front/src/segments/connection.rs` is keyed by TCP `SocketAddr`.
Both get a local counterpart, and each is named here so it is built and tested, not
assumed.

**Welcome.** Additive proto field `LocalEndpoint { host_id, server_instance, session_id,
token }`. The daemon's `Hello` gains an additive `local_lane_capable` bool, and the
server mints a token only when it is set and `--local-socket` is configured, so an old
daemon never causes one to exist. `token` is 32 bytes from the OS CSPRNG. `session_id`
is a non-secret 16-byte id the server indexes by. `Debug` for `Welcome` and
`LocalEndpoint` redacts the token, and a test asserts no log line, trace or sim recording
contains it.

**Token lifetime.** One token per `Welcome`. A new `Welcome` for the same node (a
takeover, or a `Hello` that recurs on a stream) revokes the previous token; so does the
end of the stream, a deny-list hit and a server restart. Revoking closes the local
connections that authenticated with it and drops their open uploads, as a closed worker
connection drops its uploads today. Local uploads do not survive their session.

**Socket path and peer check, client side.** The path comes from the daemon's own flag
(`--local-socket`, default a fixed directory under `/run`), never from `Welcome`, so a
mistaken or compromised server cannot aim the daemon at another socket it can reach (a
Podman API socket, for example). Before sending any token the daemon reads the socket
peer's uid (`SO_PEERCRED` through tokio's `peer_cred`, safe, no `unsafe` in our code) and
requires it to equal the expected server uid (`--local-server-uid`, default the daemon's
own uid, which is right on a one-user box). A squatter running as anyone else never sees
the token.

**Peer check, server side.** The server allowlists the peer uid (`--local-peer-uid`,
default the server's own uid; the two-user deploy renders the daemon's uid). Everything
else is refused. There is no "refuse the server's own uid" rule: a single-user box is a
supported shape, and the uid check is a filter in front of the token, not the proof.

**Per-call authorization.** Every `Segments` call on the local socket carries
`session_id` and `token` in metadata. A tower layer on the local listener (it runs before
the message body is decoded) looks the session up, compares the token in constant time,
checks the stored `server_instance`, and then **consults the deny list on every call**
with the same cached-by-stat read the blob calls use (`admit_call` today), through a new
`admit_local(node_id)` that applies the deny list without a certificate. A deny-list
edit therefore refuses the next local `PutRange` or `Begin`, which is the rule the blob
calls have, not "at the next heartbeat".

**Upload ownership.** Uploads are owned by a `Connections` key that is an enum
`Tcp(SocketAddr) | Local(u64)`. The local id is assigned when the listener accepts a
stream (a small wrapper type gives the `Tracked` stream its id). Idle-timeout expiry
(`--segment-upload-idle-secs`), drop on close and release of `Cache::admit`'s budget hang
on that key exactly as for TCP; tests cover each for the local key.

**Pre-authentication limits.** A cap on connections that have not yet made an
authenticated call (`--local-preauth-connections`, small), a deadline for the first
authenticated call after accept, and rejection before the body is decoded, so an
unauthenticated peer cannot make the server read or allocate.

**Per-session limits.** `--local-uploads-in-flight` and a byte budget per session are
kept on the session, not the connection, so reconnecting does not reset them.
`--write-bytes-in-flight` still bounds server memory for local and remote uploads
together.

## 5. What the lane does and does not change

The server checks every byte against its digest, builds the footer itself, stages on its
tier, indexes with one `PutBlobs`/`PutStaged` commit and answers only after that commit
(`write-path.md` sections 4 and 6). The local channel carries the same `PutRange` bytes
into the same `Receiving`. The integrity argument is the remote path's: the server hashes
what it received into its own memory and stores that same buffer, so a malicious daemon
cannot poison the CAS by any bytes it sends over a socket any more than over TCP.

It replaces TLS and the loopback TCP stack with a Unix socket. It does not remove:

- the server's hash pass (the dominant cost if the CPUs lack SHA instructions, see
  section 8);
- the tier write, `fdatasync` and metadata commit;
- the daemon's walk, hash and spool.

**Rejected as the lane's byte mover: reflink and `copy_file_range`.** `FICLONERANGE`
needs block-aligned offsets and lengths, and the tier packs records at byte offsets.
ext4 has no reflink at all, and XFS needs it enabled. Reflink needs source and
destination on one filesystem, and the daemon's scratch and the server's tier (0700,
likely on NVMe) will usually be different filesystems on a storage node.
`copy_file_range` across filesystems behaves differently by kernel. Copy-then-hash is
also TOCTOU-unsafe where clone-then-hash is not. Reading into server memory, hashing and
writing the same buffer is one pass over the data and is what the remote path does.

**Page cache.** The spool is written dirty, never synced, and unlinked, so the daemon's
reads hit the page cache. The tier write adds a second cached copy. This is fine on
large-memory hosts. Nobody should add `fadvise` or `O_DIRECT` to "help".

**No local get.** Local get is dropped from this document, not deferred. With a 96%
node-cache hit rate a miss is a small fill, and the first draft's design (a sealed
`memfd` per batch that the server fills, the daemon reads and hashes again) costs three
or four passes for what is a loopback `ReadRange` today, and lets a node pin server
memory in the server's cgroup. If tier-resident reads ever matter, the same Unix-socket
channel carries `ReadRange` for free with no new design.

## 6. Security summary

**Principals.** `kbf-server` owns the tier (0700), the metadata log, the API token and
the S3 key. `kbf` runs the daemon, and the container driver runs actions in rootless
Podman as that user; the native Linux driver runs actions as `kbf` itself. A malicious
action is, at worst after an escape, **uid `kbf`**. Today neither unit sets
`NoNewPrivileges`, `ProtectSystem` or `RestrictAddressFamilies`, so that is the whole of
the isolation; the doc does not claim more.

**What the lane adds.** Uid `kbf` can already read `node.key` and speak mutual TLS as
that node, and can upload any bytes the server will verify. The local channel is that
authority over a different transport, with one real difference: it **bypasses
network-layer controls** (per-connection limits, rate limiting at the TLS listener).
Section 4's per-session limits and the pre-authentication cap exist for that reason.

**What it must not do:** widen what `kbf` reaches on the server's side. Nothing of the
server's is opened by the daemon (no descriptor of any kind crosses), and no path comes
from the server.

Socket hygiene:
- Directory `0750 kbf-server:kbf` from `tmpfiles.d`, the server sets umask `0117` before
  `bind` so the socket is created `0660`, not chmod-ed afterwards.
- Std/tokio sockets are `CLOEXEC`; a test spawns a child and asserts the fd is closed.
  A leaked connected socket in an action would be an authenticated connection.
- Stale-socket removal at start: `lstat`, require a socket owned by the server uid, then
  unlink. It never removes a directory or follows a symlink.
- `check-units` asserts the `kbf` group has no other members (any member can connect).

## 7. Failure modes and fallback

The lane is never a state a daemon gets stuck in. The state is **per session**: a new
`Welcome` resets it.

Errors are classified, because a remote retry of a data error repeats the whole cost and
returns the same answer:

| Class | Examples | Action |
|---|---|---|
| Not applicable | no endpoint; host id differs; no own host id (Mac) | remote, counted as `not_applicable`, no log |
| Lane fault | connect refused or missing path; peer uid not the expected server; `UNAUTHENTICATED` (token revoked, restarted server); transport reset | that upload goes remote through the existing `upload_or_fall_back` (two attempts, then blob by blob); three consecutive lane faults disable the lane for the rest of the session; counted as `expected_but_failed` and logged once at WARN |
| Data error | `INVALID_ARGUMENT` (digest mismatch: a corrupt spool); `PERMISSION_DENIED` (deny list) | surfaced as `CasError::Corrupt`/`Read` or the denial; **no remote retry**; does not count toward the three |
| Server-side fullness | tier at budget | the server takes its direct path exactly as for a remote upload |

Other rows:
- Server dies between staging and answering: the reply is lost; the retry is remote or
  local after reconnect; `FindMissing` runs first so committed records are not resent;
  the commit is atomic, so all of an upload's records exist or none.
- Server restart: new `Welcome` carries a new `server_instance` and token; the lane
  reconnects after the session does.
- A fallback does not start until the local attempt has ended (the daemon drops its
  stream and the server drops the connection's upload), so a hung local call cannot
  double the bandwidth or hold admission.
- `--local-fast-path=off` is the operator's switch if the local path proves slower.
  There is no automatic comparison.

Retries are safe because every operation is addressed by digest and committed
atomically.

## 8. The measurement gate and expected gains

Nothing is built past phase 0 until these are run on the farm or the bench box. The
questions they answer decide whether the lane, or something topology-independent, is the
right next step.

1. **The `outputs phases:` line** (it already has `uploads`, `segments`,
   `put_range_calls`, `begin_ms` and the permit wait) for the 15k-file case, on `main`
   and on `main` plus the three branches in section 1. This is the new baseline. The
   remaining upload time is what the lane can still save.
2. **Server-side stage timings for the same upload**: bytes-in time versus `Commit`
   time, split into hash, tier write and `fdatasync`, and `PutStaged`. If `Commit`
   dominates, transport is not the cost and the lane saves little.
3. **A loopback `PutRange` throughput run** in `kbf-bench` (`connect.rs` exists): 16
   streams, 4 MiB ranges, TCP+TLS versus a null sink versus a Unix socket without TLS.
   This separates TLS, framing and memcpy. (Windows are already 8 MiB per stream, so
   HTTP/2 flow control is unlikely to be it.)
4. **CPU model and `sha_ni` flag** for the build hosts and the storage nodes, and
   single-core SHA-256 throughput as `sha2` 0.11 runs there. The Broadwell hosts have no
   SHA instructions, so the scalar path applies there; the first draft's "0.2 s per
   307 MB" assumed hardware that is not there. At a few hundred MB/s per core a pass
   over 307 MB is about a second, and today there are three passes (walk hash, spool
   re-hash, server check). The local lane removes none of them.
5. **Latency of `fdatasync` plus `PutStaged` per 32 MiB upload** on the tier device.

Estimates, to be overwritten by those numbers:

| Item | Today | After the three branches | Local lane on top |
|---|---|---|---|
| Upload of 307 MB, 15k files | 6.0 s | unknown (measure) | removes TLS and TCP CPU only; roughly 0.2 to 0.3 s of CPU on AES-NI hosts (estimate), more where loopback TCP copies hurt |
| Outputs phase | 18.3 s | unknown | walk 7.8 s and spool 4.3 s unchanged |
| Typical action | upload is milliseconds | same | no change |
| Input materialization | 96% node-cache hits | same | none (no local get) |
| Server CPU on a shared host | TLS + protobuf per byte | | less |

Read honestly: the lane can only save what the three branches leave, and the outputs of
a given toolchain tree are uploaded once; the next identical output is `FindMissing`
"present" with spool 0 and upload 0. The lane helps cold toolchain bumps and large new
outputs on a shared host, and is dead code in the target split farm.

**Topology-independent alternatives, in order of expected gain per complexity**, none
of which this document builds:

1. Land the three branches and re-measure (zero new surface, every topology benefits).
2. Hash across segments in parallel on the server, or rely on `commit-overlap`.
3. **Spool-less upload planning**: plan the upload over the original output files (the
   container upper directory lives until the lease ends) instead of a spool offset
   table. This removes the spool's write, read and 2x scratch use (`write-path.md`
   section 4) in both topologies and is the biggest in-process do-less win. It needs a
   bounded open-file strategy for 15k files and keeps hash verification on the server,
   where it is enforced today. It is its own design.
4. The walk (7.8 s) and the directory cache and link plans for the typical action.
5. This lane, only if the remaining gap, measured, is worth a second transport.

## 9. Deploy

Same units, same binaries. Today `kbf-deploy` installs and enables both
`kbf-server.service` and `kbf-daemon.service`, with a separate server-only enable, and
there is no role key among the `KBF_*` settings; `check.rs` couples the two units (OOM
ordering, `MemoryMin` sums). So the rule is: **wherever both units render, the lane
flags render; no new role input.** A stray lane flag is harmless: a server with
`--local-socket` and no local daemon idles on a socket nobody can use, and a daemon
whose server is elsewhere skips on host identity.

- `kbf-server.service`: `--local-socket=/run/kbf-local`, `--local-peer-uid=<kbf uid>`,
  `--local-uploads-in-flight=<n>`. The directory comes from `tmpfiles.d`
  (`d /run/kbf-local 0750 kbf-server kbf -`) so its group is `kbf`.
- `kbf-daemon.service`: `--local-fast-path=require` on converged hosts,
  `--local-server-uid=<kbf-server uid>`. `require` does not refuse to start (turning an
  optimization into an outage); it makes the daemon report lane state to the server and
  log at ERROR when the lane is expected but not established, and `kbf-alert` alarms on
  the "expected but not active" gauge. A silently wrong group or missing `tmpfiles.d`
  entry otherwise costs seconds per action with a green deploy.
- `kbf-deploy linux check-units` gains: the socket directory is not world-accessible;
  `--local-socket` appears only where a daemon unit also renders; the `kbf` group has no
  other members; the daemon's user is in the directory's group.
- **A post-install handshake check** (`kbf-deploy linux verify-local-lane`, run as the
  daemon's user): connect, pass the peer-uid check, make one authenticated call. Static
  string checks do not prove the lane works.
- The tier directory stays 0700 `kbf-server`; no unit gives the daemon access to it.
- Unit sandboxing (`RestrictAddressFamilies=AF_UNIX AF_INET AF_INET6` and friends) does
  not exist today and is a separate small change, not part of this one.

## 10. Tests: one suite, three transports

**A conformance suite for `Cas` put**, written once against the trait in
`kbf-transfer` tests, run for each instance:

| Instance | What it is | Extra assertion |
|---|---|---|
| `remote` | engine over an in-process server, lane disabled | TCP `PutRange` bytes > 0 |
| `local` | lane forced on, same server | TCP `PutRange` bytes == 0 and local bytes == spool length |
| `fallback` | lane on, a fault injected at each lane-fault row of section 7 | result equals `remote`'s; fault observed; nothing duplicated or lost |

Cases every instance passes: sizes (empty, 1 byte, 1 MiB, over the segment cap), a blob
already present, an upload whose record bytes do not match their digest (the server
refuses and indexes nothing; for `local` and `fallback` also **no remote retry**), two
concurrent uploads, a revoked token mid-upload. There is no "file truncated mid-upload"
or "FUSE descriptor" case: no file descriptor crosses, and the daemon's spool is read by
the daemon.

Each test names the mutant that turns it red:

| Test | Mutant |
|---|---|
| `local` shows zero TCP bytes | engine silently routes local calls to the TCP channel |
| digest mismatch refused on `local` | the local listener skips `check`, or the lane hashes before copying |
| no token / wrong token / token of another session / token after takeover / token after restart -> `UNAUTHENTICATED` | the layer skips the token compare, compares a prefix, or ignores `server_instance` |
| deny-list edit mid-session refuses the next `PutRange` | the layer caches the deny list per connection |
| wrong-uid peer refused by the server; wrong-uid server refused by the daemon before any token is sent | allowlist inverted, or daemon sends first |
| path in `Welcome` that differs from the flag is ignored | daemon follows `Welcome` |
| pre-auth stall closed at the deadline; pre-auth cap holds | deadline or cap removed |
| local upload dropped on close releases `--write-bytes-in-flight`; stalled local upload does not starve remote ones | release missing for the `Local(u64)` key |
| `Debug` of `Welcome` shows no token | derived `Debug` |
| host id differs -> no connect attempted; Mac-shaped daemon -> no connect | skip removed |
| data error not retried and not counted; lane-fault counted | classification collapsed |
| version skew: old daemon with new server (no token minted), new daemon with old server (remote) | server mints unconditionally |

Peer-uid predicates are unit-tested as pure functions. The real two-user path (group
membership, directory modes, `SO_PEERCRED` across users) cannot run in one-uid CI or in
`kbf-it`; it is covered by `check-units`, the post-install handshake check on a real
converged host, and a farm run on a two-user VM. Until that exists, the two-user path
has no automated test and the doc says so.

**Server-side**: the shared `Receiving`/`check`/commit code is covered by the `PutRange`
tests. The new tests cover what is new: the layer, `admit_local`, the `Local(u64)` key.

**`kbf-sim-cell`.** `FakeDaemon` gets a local lane that bypasses the network model, with
injectable faults (socket down, crash between staging and answer, stale token). The
existing properties 1 to 9 hold unchanged. Added: **L2** after any lane fault the same
upload completes remotely with no duplicate index entry. The earlier L1 ("no record is
indexed whose bytes the server did not hash") is dropped as evidence: the sim server
hashes by construction, so it validates the sim, not `kbf-front`. That claim is checked
by the digest-mismatch test and its mutant against the real `kbf-front` code.

**`kbf-it`** runs a real server with `--local-socket` and a real daemon with `auto`
(one uid), runs an action with a multi-file output, asserts the lane was used and the
server saw no TCP `PutRange` for it; a second run with `off` asserts the reverse.
`deploy_units.rs` runs the rendered flags through both parsers.

**Call sites.** Phase 0 verifies the native driver (`runtime.rs`), the container driver
and the daemon's `cas.rs` all reach `put_spool` through the engine, so one lane covers
all three; a test per driver proves it.

**Visibility.** Each lease's transfers log line and the outputs timing record gain
`lane=local|remote|fallback(reason)`, so one slow action can be attributed. Process
metrics: lane state, local and remote bytes and calls, fallbacks by reason,
`not_applicable` versus `expected_but_failed`, authentication failures, the time the
server spends hashing local uploads.

## 11. Rollout

0. **Measure** (section 8), and **rebase the seams onto `server/commit-overlap`** if it
   lands first. The contract the local channel needs is "`PutRange` feeds the server's
   incremental per-record check", not a particular function name; the old `check` at
   Commit is going away. Add the proto fields, the `Cas` conformance suite for `remote`,
   the call-site checks. Ships nothing a user sees.
1. **Decide**: build phase 2 only if measurement 1 leaves a gap that 2 and 3 attribute
   to transport. Otherwise close this document as "not worth it" and take the
   topology-independent items in section 8.
2. **Local channel for put**, behind `--local-socket` (off by default) and
   `--local-fast-path` (`auto`). Enable on the single-box farm, measure, compare with
   section 8.

Each phase reverts by turning the flag off. No phase changes what a remote daemon sends
or a server accepts from one.

## 12. Out of scope, and why

- **Merging the binaries or an in-process mode.** The two users exist because the server
  holds the S3 key, the API token, the tier and the log, and the daemon runs untrusted
  work; one process or one user removes that boundary and gives a build OOM the server's
  address space (`kbf.slice` ordering exists to prevent that). Not planned.
- **Giving the daemon the tier directory or object-store credentials.**
- **Follower-local puts in HA**, **cross-host fast paths** (RDMA, shared filesystems),
  and **a different control-plane protocol on one host** (loopback gRPC is not their
  cost).
- **Speeding the walk and the spool**, except as the topology-independent items in
  section 8.

## 13. Open questions

1. **For the project owner: build gated, or not at all?** Recommendation: gated. Run the section 8
   measurement now (it also tells us what to do for the split farm); build the Unix-
   socket lane only if a gap remains and the single-box and converged shapes matter.
2. Does the farm ever host tenants that must not read each other's blobs? The
   recommendation is to assume not. There is no local get, so the lane does not change
   the answer either way.
3. Default of `--local-socket`: off in the binary, rendered on by `kbf-deploy` where
   both units render. Recommendation: keep.

## Appendix A. If descriptors are ever added

Only if the measurement shows the extra daemon read and kernel copy matter. Passing the
spool file's descriptor over `SCM_RIGHTS` must then satisfy, in addition to section 4:

- Fd passing code lives in one small crate with `# SAFETY:` notes and its own fuzz or
  property test on the framing; no scattered `libc` calls.
- Receive buffer of exactly `CMSG_SPACE(1 fd)`; `MSG_CTRUNC` closes the connection
  (otherwise a peer pushes up to 253 descriptors per message into the server's fd table);
  `MSG_CMSG_CLOEXEC`.
- The descriptor check is an **allowlist**: `f_type` in {ext4, xfs, btrfs, tmpfs,
  overlayfs} plus the spool directory's filesystem, `S_ISREG`, read-only-compatible, and
  an `fstat` size covering every record. A blocklist of FUSE and network filesystems is
  open-ended (rootless Podman gives `kbf` unprivileged FUSE). Testable in CI without
  FUSE with a procfs or `/dev/shm` fd, plus a real FIFO.
- A blocking `pread` cannot be cancelled by a deadline: run it on a dedicated bounded
  pool, not tokio's shared blocking pool (which also does tier `fdatasync`), and coalesce
  adjacent records into one read.
- Re-open the spool read-only through `/proc/self/fd` (Linux), so the server never
  holds a writable descriptor of a daemon file; the server reads into memory and hashes
  that buffer ("never hash one copy and store another"). The reader sits behind a trait
  so a test double can mutate the file between the size check and the read.
- Per-file descriptors (instead of a spool) require `openat2(RESOLVE_NO_SYMLINKS |
  RESOLVE_BENEATH)`, a regular-file and link-count check (an output tree is attacker-
  shaped and could name `node.key`), and a check of what the spool walk does today,
  since the same hole may already exist there. About 250 descriptors fit per message,
  so 15k files needs a `RLIMIT_NOFILE` check on both sides.
- Any server-to-daemon descriptor (a get) is charged against a memory budget and capped
  per connection; a daemon-supplied writable fill file the server `pwrite`s into is
  preferable to a server `memfd` (no server-cgroup memory).

## Appendix B. Review disposition

Every must-fix from three reviews (security and isolation; performance and baseline;
design and tests), and what happened.

| # | Finding | How addressed |
|---|---|---|
| S1 | Revocation weaker than today (heartbeat vs first call after) | Deny list consulted on every local call via `admit_local` (section 4); stated as new code with tests; token revoked on takeover and per-`Welcome` lifetime stated |
| S1b | Admission claimed to be the same code; `Connections` keyed by TCP address | Section 4 names the new key `Tcp\|Local(u64)` and the new layer |
| S2 | No session id, no server challenge, replayable HELLO | Superseded: no handshake MACs. Bearer `session_id` + token on every call over a socket whose owner uid the daemon checks first; CSPRNG, constant-time compare, redacting `Debug` kept |
| S3 | Daemon must not take socket path from `Welcome` | Path from the daemon's flag; `Welcome` carries no path (section 4); daemon checks `SO_PEERCRED` uid before sending a token |
| S4 | Server uid check is a denylist; pre-auth limits; `MSG_CTRUNC` | Uid allowlist; pre-auth cap, deadline, pre-body rejection (section 4); `CTRUNC` is gone with fds and recorded in appendix A |
| S5 | Descriptor check is a blocklist; blocking `pread` pool | No descriptors in phase 2; the allowlist and pool requirements are recorded in appendix A |
| P1 (M1) | Baseline predates in-flight work | Section 1 and the section 8 baseline row; phase 2 gated on it |
| P2 (M2) | Seam moved to `commit-overlap` | Section 11 phase 0 states the contract (incremental per-record check) and rebases onto that branch if it lands first; "same `check`" wording removed |
| P3 (M3) | SHA-NI assumption wrong on Broadwell | Section 8 measurement 4; the "0.2 s" claim removed; three passes named |
| P4 (M4) | Transport cost probably small | Section 8 measurement 3 separates TLS, framing and memcpy; the gain table is marked unmeasured |
| D1 | Single-uid boxes break the lane | Same-uid refusal dropped; uid allowlist/expected-uid flags default to own uid; the two-user path's test coverage and its gap are stated (section 10) |
| D2 | Wrong-server connects are normal and warn | Host identity in `LocalEndpoint`; mismatch is silent and counted `not_applicable`; failures stay loud (sections 3, 7) |
| D3 | No user in the target farm | Said plainly in section 2 and 8; sized as a small-deployment convenience; walk and link plans named as the larger levers |
| D4 | Ownership keyed by TCP peer | Same as S1b |
| D5 | Simpler alternative rejected on false premise | Adopted: Unix-socket gRPC is the design; fds moved to appendix A |
| D6 | Premise not measured before building | Section 8 gate precedes any build |
| D7 | Fallback classification | Section 7 table; state is per session |
| D8 | Per-action lane visibility | `lane=` field and gauges (section 10) |
| D9 | Silent degradation | `--local-fast-path=require` plus alarm gauge plus post-install handshake check (section 9) |
| D10 | Deploy facts wrong | Section 9 states the actual install behaviour and the render-where-both-render rule; no `/proc/self/fd` reopen, so unit sandboxing is moot |
| D11 | Tests that cannot fail | Section 10: per-test mutants; L1 dropped as evidence; FUSE/mid-upload truncation cases removed with fds; pure-function uid predicates |

Should-fix and suggestions taken: local get dropped (S5, security 6, design); page-cache
and reflink reasoning added; document trimmed; `upload_or_fall_back` reused; per-
session limits; socket hygiene (umask, `CLOEXEC`, `lstat`, group membership);
`unsafe` avoided; version skew, Mac and call-site tests; admission-starvation test.

**Rejected or deferred, with reasons:**

- **Refuse to start when `require` cannot establish the lane.** Rejected: it turns an
  optimization into an outage on every server restart. The daemon reports and alarms
  instead.
- **Fold spool-less planning and parallel server hashing into this document.** Deferred
  to their own designs: they are topology-independent, touch the planner and drivers,
  and should not wait on or hide behind a lane decision.
- **Keep the HMAC handshake and harden it.** Not needed once the transport is the
  authenticated service; a captured bearer token on a local socket needs root, and
  root already owns the daemon's memory.
- **Harden the units with `RestrictAddressFamilies` here.** A separate small change; it
  is independent of the lane and would hide this diff.
- **Memfd budget, sealing flags and the daemon-supplied fill file.** Moot with get
  dropped; the preference is kept in appendix A.
- **"Do not build until measured" as deleting the idea.** Kept as a gate, not a
  deletion: the single-box shape is what runs today, and the project owner asked for a run-
  together mode.
