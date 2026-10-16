# One binary, two node types, two modes

Status: design, nothing built. Labels: **[F]** fact checked against `main` while writing this revision, **[F#744]** carried from the first draft of this document and not re-checked, **[I]** inference, **[D]** owner decision (2026-10-15), **[V]** verify before building on it.

This revision replaces the roles, data-path and leader-only-upload parts of the first draft. It does not modify `ha.md` (PR #303); section 11 says how the two relate.

## 1. Decisions

- **[D]** One `kbf` binary and two node types. There is no roles system.
  - A **server node** runs the server half: scheduler-capable, hot store on local SSD, holds object-store credentials, flushes to the cold store, and joins the cluster. With `--build` its own daemon runs beside it and talks to its own server half in-process, with no TLS and no network.
  - A **daemon-only node** (`--daemon-only`) runs only the daemon. It has no server half, no listener, no Raft directory, no hot store and no credentials. It connects to server nodes. It builds (that is all it does), so `--build` is meaningless with it and is refused.
- **[D]** `--leader-candidates` has the same value on every node. It lists the server nodes that vote and may lead (3 or 5). A server node not in the list is a full non-voting member: it serves, stores and may build, and never leads.
- **[D]** Two modes, both always supported and both tested. They are two configurations of the same two node types. **Nothing in the code branches on the mode**: it branches on node type, on `--build`, and on which implementation of an interface a node holds. A branch on "mode" is a design bug.
- **[D]** The same interfaces serve both modes: daemon to server (data plane and control plane, each with an in-process and a remote implementation and one conformance suite), and server to server (Raft replication, leader discovery, peer blob fetch).
- **[D]** Kept from the first draft: a leader-elected scheduler on a Raft log among the candidates; Envoy routes clients to the leader; unflushed hot blobs may be lost with a node; admission by CPUs with the memory ladder; flags, not environment variables; tests never depend on timing; no compatibility with earlier formats.
- **[I]** Stages. Stage 1 elects one leader with a strictly increasing epoch; queue and leases stay leader-local, so a failover loses them (as a leader change already does, `scheduler.md`). Stage 2 replicates the scheduler (PR #303's `FarmMachine`).
- **[I]** v1 voter membership is the static list. There is no membership change, no learner flow and no `--raft-join`. Non-voting server nodes are not Raft members in v1; they forward metadata to the leader (section 5.3).

## 2. The two modes side by side

| | Trusted-network mode (today's shape) | Isolated-storage mode (future) |
|---|---|---|
| Nodes | Every node is a server node with `--build`. Six today, each with MinIO. Three or five are candidates; the rest are non-voting. | Dedicated storage nodes are server nodes without `--build`, each with MinIO. Every build node is `--daemon-only`. |
| Credentials | Object-store credentials on every node. | Only on storage nodes. |
| Security boundary | The network (for example a tailnet). A stated, accepted risk (section 3.1). | No credentials on build nodes, plus a network policy making the object store reachable only from storage nodes (section 3.2). |
| Daemon to server, data | In-process, to its own node's server half. | Remote, to a storage node. |
| Daemon to server, control | In-process while its own half is the leader; otherwise remote to the leader. | Remote, to the leader. |
| Where a build's outputs sit | The building node's hot store. | The hot store of the storage node the daemon uploaded to. |
| Who fetches from whom | A node needing a blob fetches it from the holder, or from the cold store once flushed. | Daemons fetch from storage nodes. Storage nodes fetch from each other or the cold store. |
| Loss of one node | May lose unflushed outputs and a build slot. | May lose unflushed outputs of a storage node. A build node holds no blob; its loss costs leases only. |

The data path is one mechanism (section 4). Only the transport between a daemon and a server half differs.

## 3. Security

### 3.1 Trusted-network mode: the risk, stated plainly

Every node holds object-store credentials and every node runs untrusted build code beside them. A container escape, or a malicious action, on any one node yields credentials that can read, overwrite and delete the whole cold store, which is the build cache. Any process that can reach a node's ports can speak to that node. Mutual TLS between nodes stays on, but its certificate authority and keys live in the same network, so it protects against accidental cross-talk and not against a hostile process on the network or a compromised node. Anyone with a shell on any node holds the credentials for the whole store.

This is a stated, accepted and documented risk, for organizations whose network (for example a tailnet) is already the trust boundary. `SECURITY.md` and the deploy guide say so in these words and never call the mode hardened. The design neither weakens nor strengthens it, with two cheap exceptions that are required, because they remove a hole that costs nothing to close:

- **Action isolation on a `--build` server node.** An action is a child of the server process. It must not inherit the server's credentials. Requirements: the action environment is built from an allowlist, never inherited; credentials are read from a file named by a flag (argv is world-readable, and a file is not copied to children), opened by the server half only. Running actions under a uid other than the server's, so that `/proc/<ppid>/environ`, the credential file and process memory are unreadable, is the deployment guidance and a question for the owner (question 3). Without a separate uid the boundary is stated as "one action can read the credentials", not hidden. Test: a canary variable and a canary credential file in the server's environment never appear in an action's environment or command line, and with the separate uid the action cannot open the file. [I]
- **No flag drops inter-node TLS.** A second way to run the same link is a second code path to test. Rejected in v1 (appendix).

### 3.2 Isolated-storage mode: the boundary, and what it does not defend

Invariants:

- **I1. Credentials live only on server nodes.** Flag parsing refuses any store, credential or cold-store flag together with `--daemon-only`. A flag that would give a daemon-only node a credential is an error, never ignored. The invariant must hold at runtime, not only through linking, because the one binary links both halves. Today the daemon crates name `kbf-objstore` only as a dev-dependency [F: `crates/kbf-daemon/Cargo.toml`, `crates/kbf-transfer/Cargo.toml`].
- **I2. A daemon-only node starts and builds with no credentials present, and never uses them if they are.** It does not construct an object-store client on any code path. Inputs and outputs go through a server node. Ambient credentials count: provider-standard variables, credential files, and instance-metadata or workload-identity endpoints.
- **I3. The object store, and the instance-metadata endpoint, are reachable only from where policy allows.** The object store is reachable only from server nodes; the metadata endpoint is unreachable from build nodes. This is network policy, not code. Code assumes it and the farm drill proves it.
- **I4. The daemon protocol exposes no credential and no store location.** No operation returns a credential, a presigned URL or a cold-store URL. Errors a server returns to a daemon carry a code from a closed enumeration and a fixed message, never text from the store client.
- **I5. Certificate classes are separate.** Daemon, Raft and peer-fetch identities carry distinct classes in the certificate. The Raft listener and the peer listener refuse a daemon certificate; the worker listener refuses a server certificate used as a daemon. [I]

What isolated mode does and does not defend. It defends the object store's credentials and direct access: compromising a build node gives nothing to steal from it. It does **not** make a compromised build node harmless. An escaped daemon still holds a daemon certificate and can:

- read any blob whose digest it knows or guesses, through a storage node (reads are not tied to a lease today [V]);
- write blobs, which are content-addressed and verified, so a bad blob is stored under its own digest and harms no one else's;
- write action results, which is the serious one: a daemon can write an action-cache entry naming blobs it chose, and other nodes would trust it. Clients never write the action cache [F: `docs/reapi-auth.md`, "Where kbf differs"]; only a daemon does.

Mitigations, in order of cost:

1. **Result writes are tied to a lease [V].** The server accepts an action result from a daemon only for the action digest of a lease that daemon holds at the current epoch. A daemon then cannot write entries for actions it was not given. It can still lie about the result of its own action; that trust in the hermetic execution of its own lease is the remaining stated risk. Required before isolated mode is called supported.
2. **Per-lease read authorization** (a daemon reads only the digests in its leased input closure and the outputs it is producing). Costs a closure lookup per read. Stage 2, and a question for the owner (question 4): until it lands, the docs state that a compromised build node can read any blob it can name.
3. Distinct certificate classes (I5), so a build node cannot reach the Raft, peer or operator listeners.

### 3.3 Tests that prove the invariants

Each test is red first and has a mutant that must turn it red. Tests of absence are paired with tests of presence, because a sandbox with nothing in it cannot catch code that reads credentials only "if present".

- **S1, flag refusal (I1).** `--daemon-only` together with each of `--cold-store`, `--hot-store`, `--raft-dir`, `--advertise` and each credential flag exits non-zero naming the flag. Mutant: ignore the flag.
- **S2, construction guard (I1, I2).** In a test build the daemon-only path is given an object-store factory that panics when built or called. A daemon-only node completes a full build through a server node with the factory never called. Mutant: the daemon builds the store at startup.
- **S3, absence (I2).** A daemon-only node runs with an empty environment, an empty home directory and every credential-file path failing to open. It starts, connects, runs an action and uploads its outputs. Control run: a server node in the same sandbox fails to start, which shows the sandbox really holds no credentials.
- **S4, presence (I2).** The same run, with canary credentials in the environment and in a file the node could read, for a daemon-only node and for a trusted-mode `--build` server node. The canary must never appear in an action's environment or output, in logs, in an error string, in any RPC frame between nodes, or in a request to the fake store; and the fake store sees zero requests from the daemon-only node. Mutant: the daemon reads the variable. A canary string misses transformed leaks, so the error-text check (S6) is an exact-equality one.
- **S5, no object-store traffic from a build node (I2, I3).** In the simulator the fake object store records the caller identity of every request. Under a full isolated-mode build, requests come only from server nodes, and the fake does record the storage nodes' requests (negative control). Mutant: a daemon-side direct fetch. The identity is declared by the test, so S5 guards the code and not the network; the farm drill (S8) is the only proof of the network rule.
- **S6, error surface (I4).** For each store failure the fake can inject, the text a daemon receives equals a fixed expected string for its code; the test compares exactly. A second test lists the daemon-facing RPC methods and rejects any whose response type has a field carrying a URL or credential. The list check cannot see behaviour, so a review rule for new RPCs says the same.
- **S7, certificate classes (I5).** A daemon certificate presented to the Raft listener and the peer listener is refused; a server certificate used on the worker listener as a daemon is refused. Mutant: accept any certificate from the CA.
- **S8, farm drill (I3).** In isolated topology: apply the policy, run a real build to success, then from a build node try to open the object-store port and the metadata endpoint and require refusal or timeout; from a storage node the object-store connection must succeed. The build alone proves nothing, because a missing policy leaves a green build; the failing probe is the control. A held script prints the policy for the owner to apply.
- **S9, dependency lint (I1).** `layering.rs` gains a normal-dependency rule: no daemon-side crate names `kbf-objstore` or the S3 backend. This is a tripwire. It does not count as proof, because the one binary links both halves; S1 to S4 carry the weight.
- **S10, trusted mode states its risk.** The start line and `/readyz` of a trusted-mode node include the facts `credentials: present` and `boundary: network`, derived from configuration (a server node holding credentials is "present"; the boundary is "network" whenever any `--build` node holds them). It is a string match and guards only against silently presenting the weaker mode as the stronger one.

## 4. Data path

### 4.1 The index already exists

No new blob-location index is needed. The replicated state machine holds it [F: `storage.md`, `write-path.md` section 6, `crates/kbf-front/src/cache/tier.rs`, `cache/flush.rs`]:

- the CAS index: digest to `Location { store, object, offset }`;
- the staged table: object to tier, committed length and state;
- `AddTier { server, tier }`: which server owns which tier, and `AllocEpoch`, a new writer epoch at every start;
- `PutStaged` (an acknowledged group of blobs), `ObjectFlushed` (after the PUT and footer read-back), `FlushConflict`, `ObjectUnreachable { Missing | Corrupt }`;
- `BlobAnswer::Present { location, staged_on }`, plus `Unavailable` and `Absent`.

So the replicated blob-location index is the Raft log that stage 1 adds anyway. The design adds around it: each server node commits its own tier and takes its own writer epoch through the leader; a peer service to read a staged block; `TierDown` and `TierUp` for dead holders (4.4); and the read rules of 4.3. The first draft's leader-local index, rebuilt from node reports after a failover, is dropped: after every election it would be empty and correctness would depend on report timing.

The leader is on the metadata path only: one small commit per upload group (digests and offsets, a few dozen bytes per blob, group-committed). Bytes never pass through it. Two costs are stated: a leader outage stalls uploads, because an upload is acknowledged only after its commit applies; and log and snapshot growth is one entry per upload group, for which PR 0 produces a size estimate and the compaction budget [V].

### 4.2 Upload (both modes)

1. The daemon (or an Envoy-routed client) sends blobs to a server half. In trusted mode that is the daemon's own node, in-process. In isolated mode it is a storage node over mutual TLS. If the chosen server node fails mid-upload the daemon retries the whole group against another; nothing was acknowledged, and a repeated blob is harmless because storage is content-addressed.
2. The server half verifies the digests, appends to its tier block and syncs it [F: `write-path.md` 6.1].
3. It commits `PutStaged { tier, ... }` through the leader. A non-leader forwards the command. The caller is answered after the commit applies on the forwarding node.
4. A background flusher seals the block, re-hashes each record, PUTs the object and commits `ObjectFlushed`. From then on any server node can read the object from the cold store.

An acknowledged blob is on one node's synced disk and in the committed log, not in the cold store [F: `write-path.md` 6.3]. Losing that disk before the flush loses the blob. That is the accepted loss [D]. A minority-side or deposed leader cannot commit `PutStaged`, so it cannot acknowledge: this closes the acknowledged-but-unregistered window.

Forwarded commands are at-least-once across a leader change, so each has a stated idempotency rule:
- `AllocEpoch`: every apply yields a new epoch, so two applies never hand out the same one. A node that never saw the reply asks again and uses only the epoch it receives. Skipped numbers are harmless.
- `AddTier`: keyed by `(server, tier)`; a repeat is a no-op [F: the tier opens register-if-new, `cache/tier.rs`].
- `PutStaged`: an already-applied command is detected by the existing "a log can apply a command and report failure" handling [F: `cache/tier.rs`, comment on answering a failed range]. Whether the forwarded path keeps that guarantee across a leader change is **[V]** and gets its own failover test (PR 5).

### 4.3 Read and peer fetch (both modes)

A server half serving a read asks for `BlobAnswer`: from its applied state if it is a leader, else from the leader (a small lookup RPC; non-voting nodes have no local log in v1 and voter followers use the same path so there is one code path). Then:

| `staged_on` | Source |
|---|---|
| This node's tier | The local block file. Served without asking anyone, when the caller already knows the digest, because the bytes verify against it. |
| Another node's tier | **Peer fetch**: a ranged `ReadBlock { object, offset, len }` to the holder. The receiver hashes the bytes against the digest; a mismatch counts as `Unavailable` and is never cached. |
| none (flushed) | A ranged read of the cold store. The holder may read its own `.seg` file. |

- **Bounded fallback order** on a peer fetch: the holder, then the cold store if the object has flushed, then report missing. A fetch has a timeout, and a timeout is the same as unreachable.
- **No leader known.** A lookup that needs the leader answers `UNAVAILABLE`, never a stale "absent". Local-tier reads of a known digest do not need it. Builds still stall during an election because nothing is scheduled; that is accepted.
- **The peer service** sits on its own mutual-TLS listener, apart from the worker and REAPI listeners. The server identity is bound to the certificate, and daemon certificates are refused (I5). It is read-only and serves only staged blocks and `.seg` files the node holds.
- **Flush race.** A reader may be told "staged" while the flush completes. The holder then answers "flushed, use the cold store"; the reader retries once against the cold store and never commits `ObjectUnreachable { Missing }` for an object that was staged when it asked [F: `write-path.md` 6.4 and its flusher rules].
- **Daemon input fetch.** A daemon asks its server half, which does the lookup and streams the bytes. In trusted mode the daemon never calls a peer itself, so the daemon-to-server interface is the same in both modes.
- **Hot blobs.** A popular output, such as a toolchain tree, is served by its one holder until it flushes. v1 adopts the cheap remedy: a node that fetched a blob from a peer keeps it in its input cache (the separate cache of `write-path.md` 6.6, never a holder and never in the index). Replicating blocks to a second tier before the acknowledgement is already designed [F: `write-path.md` 6.3, "later, with three servers"]. It doubles storage-node write load in isolated mode, so it is a cost and a flag, not v1.
- **Locality** (stage 2): the leader's placement can prefer the node that holds an action's largest input, since the index already says where bytes are. A soft preference, measured before it is kept.

### 4.4 Loss of a node

The unflushed blobs of a lost node are lost [D]. The system says so rather than serving around it.

- **Down, disk intact (restart).** The tier recovers staged objects against the committed table [F: `write-path.md` 6.4]. Until the node is back its objects are unreachable from peers.
- **Declared down (new).** When a holder has been unreachable beyond `--tier-down-after`, or an operator says so, the leader commits `TierDown { tier }`. The state machine then answers `Unavailable` for every blob staged on that tier. The leader's clock only triggers the proposal; the committed entry is deterministic and no replica compares clocks. The trigger reads an injectable clock, so the simulator drives it in logical time. To stop a flapping holder from writing a log entry per flap, a flip needs a minimum dwell since the last one, and the flip count is a metric. Without `TierDown`, a dead holder's blobs read `Present` with nobody able to fetch them, for an unbounded time.
- **Writes to a down tier (the rule).** `PutStaged` naming a down tier is **rejected** with a distinct error. A holder that was partitioned and alive, on reconnecting, must first finish its own recovery check, take a new writer epoch, and commit `TierUp { tier }`, then retry. `TierUp` never brings back an object already marked `ObjectUnreachable`. A blob re-uploaded elsewhere while the tier was down is committed at a new location, which wins; the old block becomes an orphan the holder's restart sweep deletes [F: `write-path.md` 6.4, orphans]. An old copy is never wrong, only unreferenced. A minority-side holder cannot commit anything, so the rule matters only after the partition heals.
- **Disk gone.** `--hot-store-abandon=<id>` [F: `write-path.md` 6.4] marks the tier's objects `ObjectUnreachable { Missing }` permanently. A re-upload heals each entry.
- **What clients and actions see.** `FindMissingBlobs` reports `Unavailable` blobs as missing, so clients re-upload. `GetActionResult` is a miss when a closure blob is `Unavailable` [F: `storage.md`, "three answers"]; its per-output lookup cost is measured and the answer cached per applied index. An `Execute` with an `Unavailable` input fails with the REAPI missing-blob precondition. An intermediate output named only by an action-cache entry is recomputed when the entry misses. Whether the build client re-executes on a download failure is **[V]** (PR 0).
- `TierDown` and `TierUp` are new log commands: they raise the log format and the machine version and need the version gate of PR #303 section 4.7. Red first: a blob staged on a down tier is served as `Present`.

### 4.5 Isolated mode, same mechanism

Storage nodes are the only holders, so every blob is on a storage node's hot store or in the cold store. Build nodes hold an input cache and nothing else. A storage node's loss is a section 4.4 case; a build node's loss loses no blob. All build I/O crosses the network; storage-node NICs and disks are the first bottleneck, and the remedy is more storage nodes, not code. The set of storage nodes a daemon uses: the candidates in v1 (all storage nodes are candidates or the daemon is told which, question 5); later the leader's `Welcome` carries the server-node list as a hint with the candidates as fallback [V: protocol field]. With no leader, a daemon-only node uses the candidates; scheduling is stalled anyway.

## 5. Node resolution, leader and discovery

### 5.1 Flags

```
# server node (trusted mode: also builds)
kbf --node-id=ID --advertise=ADDR1 --leader-candidates=ADDR1,ADDR2,ADDR3 \
    --raft-dir=... --hot-store=file://... --cold-store=s3://... --build --reserve-cpus=N

# server node, storage only (isolated mode)
kbf --node-id=ID --advertise=ADDR1 --leader-candidates=ADDR1,ADDR2,ADDR3 \
    --raft-dir=... --hot-store=file://... --cold-store=s3://...

# non-voting server node: advertise is not in the list
kbf --node-id=ID --advertise=ADDR9 --non-voting --leader-candidates=ADDR1,ADDR2,ADDR3 ...

# daemon-only node (isolated mode build node, Macs)
kbf --daemon-only --leader-candidates=ADDR1,ADDR2,ADDR3 --reserve-cpus=N
```

Resolution is a pure function of (`--daemon-only`, `--build`, `--non-voting`, `--advertise`, list):

| Resolved | Rule |
|---|---|
| Daemon-only node | `--daemon-only` |
| Server node | not `--daemon-only` |
| Voter and possible leader | server node whose `--advertise` equals exactly one list entry, without `--non-voting` |
| Non-voting member | server node with `--non-voting` whose `--advertise` matches no entry |
| Builds | `--daemon-only`, or `--build` on a server node |

`--non-voting` is mandatory for a non-voting node; there is one rule, not two. Refused at startup, naming the flag: an empty list; duplicate entries; an `--advertise` matching more than one entry; an `--advertise` matching none without `--non-voting` (a typo cannot silently demote a voter); `--non-voting` with an `--advertise` that matches an entry; a server node without `--advertise`, `--hot-store` or `--cold-store`; `--daemon-only` with `--build`, `--non-voting` or any of `--advertise`, `--raft-dir`, `--hot-store`, `--cold-store` or a credential flag (S1).

Carried unchanged from the first draft [F#744]:
- The resolved type, vote status, build flag and list are logged and shown in `/readyz`.
- **List guard.** A hash of the sorted candidate set is on every Raft message; a mismatch is rejected and reported. Changing the list is a stop-all restart in v1.
- **Wiped disk.** A voter with an empty `--raft-dir` is not a member and does not vote or campaign until configured; first bootstrap of three empty logs is an explicit one-shot act. `currentTerm` and `votedFor` are durable before any reply.
- A failed log append is fatal for that voter.
- The registry of nodes never supplies Raft membership.
- The single-server pilot's log is not imported; that server becomes a daemon-only node, or a server node if it has a disk.

### 5.2 Leader and epoch

Carried from the first draft [F#744] and unchanged: a `Takeover` entry per leader term; `Welcome.epoch` is the committed index of that entry (strictly increasing, replacing the wall-clock `process_term()`, which exists at `crates/kbf-server/src/farm.rs:86` [F]); leader-ready means Raft leader, CheckQuorum satisfied and `Takeover` committed; step-down drops all leader-local scheduler state; the daemon refuses a lower epoch and kills leases of a different one; heartbeats stay out of the log; lease deadlines are leader-clock values. `set_leader` having no caller is [F#744]. Stage 2 adopts PR #303. Both stages run in both modes. Prerequisites in `kbf-raft`: PreVote, CheckQuorum, a network transport and, before compaction, InstallSnapshot [F#744].

### 5.3 Non-voting members and discovery

- **Non-voting members (v1).** They do not join the Raft group. They find the leader exactly as a daemon does, forward commits (`PutStaged`, `AllocEpoch`, `AddTier`, `TierUp`) and lookups to it, serve their own tier to peers, and build if `--build`. There is no learner flow and no read gate in v1, so no InstallSnapshot dependency on this path. The cost is a leader lookup per read of a non-local blob; its load at 6 to 20 nodes is expected to be small, to be measured in PR 5. Learner streaming, which would let voter and non-voter followers answer lookups locally under a staleness bound (PR #303 section 8), is stage 2 and is justified only by that measurement.
- **Daemons.** A node that is not leader-ready ends a `Session` promptly with `UNAVAILABLE`. The connect loop re-resolves every name, tries every address and backs off up to 30 s [F#744], so with three candidates it finds the leader within a round. Later the `kbf-leader` trailer of PR #303 section 9 shortens the search; it is followed only to an address already configured or resolved, only if it verifies, at most once per round, with backoff capped at 2 s while leases are held.
- **A node cut off from every candidate** keeps running actions, holds results and retries; a flag sets a ceiling after which it kills them. A cut-off build-capable server node also keeps serving peer fetch to nodes that reach it and local reads of known digests; it does not schedule or commit.
- Non-voting members never lead, since they are not in the list.
- Raft traffic uses its own mutual-TLS listener and is never behind Envoy.

## 6. Envoy

- **One cluster in v1:** the candidates only, health-checked on the leader-ready check (`kbf.leader`, backed by `/readyz`). All client traffic goes to the leader, which serves from its tier, by peer fetch, or from the cold store. Non-voting nodes are never in the cluster. This keeps the first draft's choice and records that it adds client load to the leader's NIC while there is one client box.
- Widening to a second cluster for blob traffic on any ready server node (health `kbf.reads`) is a later config change, allowed only after peer fetch, `TierDown` and the lookup path pass in both modes.
- `panic_threshold: 0`, so a lone healthy host of three does not make Envoy route to followers. Short check interval and low unhealthy threshold: failover is the election timeout plus interval times threshold, seconds of `UNAVAILABLE` that clients retry.
- Retries by method class: idempotent reads (`FindMissingBlobs`, `GetActionResult`, `BatchReadBlobs`, ByteStream `Read`, `GetCapabilities`) on `unavailable`, `connect-failure`, `reset`; `WaitExecution` on `unavailable`, `connect-failure`; `Execute` and writes only when nothing was sent.
- Envoy is a single point of failure for clients, not for the farm. Daemons and peers never go through it.
- Planned restarts of the leader transfer leadership in stage 2.

## 7. In-process versus remote

- Two interfaces, `DataPlane` and `ControlPlane`, each with an in-process and a remote implementation, and one conformance suite per interface run over every implementation [D]. Each suite checks: the same inputs give the same outputs and the same epoch behaviour on a leader change; a killed channel ends in the same daemon state; a corrupt blob mismatches the same way.
- **Data plane.** A `--build` server node always uses the in-process one to its own server half. A daemon-only node always uses the remote one. It never switches on a leader change.
- **Control plane.** The scheduler runs only on the leader. A `--build` server node's daemon uses the in-process control plane while its own half is leader-ready, and dials the leader remotely otherwise. On gaining leadership it opens an in-process session after `Takeover` commits; on losing it the same daemon redials the new leader. **This differs from the plain reading of "control in-process" and is flagged (question 1).** The alternative, a relay in every server half, adds a hop and a second path.
- Main uses a mutual-TLS loopback for the control plane, and only the data plane is in-process [F#744]. Until the in-process control plane lands (PR 11), the leader's own daemon keeps the loopback: correct, but short of "no TLS between a node's own halves". The gap is stated.
- Stage 1: the new epoch kills leases including the in-process ones. Stage 2: running actions survive and are adopted from `Hello`.
- A build-capable server node opens a worker listener to serve remote daemons and peers; the single-node "no worker listener" text of the all-in-one design is amended when that design lands. The one-node farm remains a degenerate cluster: one voter, one server node with `--build`.

## 8. Failure modes

"Same" means the behaviour and the test are identical in both modes; the test runs in both.

| Failure | Trusted | Isolated | Test (simulator, logical time) |
|---|---|---|---|
| Leader crash | Election; new `Takeover`. Its own daemon and tier go with it. Stage 1 loses queue and leases; stage 2 keeps them | Same; no build slot lost, since storage nodes hold none | epochs strictly increase; stage 2: leases survive |
| Minority or isolated leader | CheckQuorum steps it down; it cannot commit `PutStaged`, so it cannot acknowledge | Same | minority leader not leader-ready; no ack |
| Rejoining partitioned node | PreVote stops deposing | Same | term unchanged |
| Two nodes believe they lead | Stale one cannot commit `Takeover`; daemons reject lower epochs | Same | at most one leader-ready per term |
| Flapping | PreVote, stickiness, 2 to 4 s timeout | Same | no term churn |
| Slow follower | Majority commit does not wait | Same | catches up after InstallSnapshot |
| Voter append fails or disk full | Fatal: step down, exit | Same | injected error stops the voter |
| Wiped voter disk | Empty log: not a member | Same | empty-log node never votes |
| List mismatch | Rejected, shown in `/readyz` | Same | differing hashes never form a group |
| Holder lost | Unflushed outputs lost. `TierDown`, then `Unavailable` and re-upload or re-run. Its builds re-run | Same, storage nodes only. A build node's loss loses leases only | holder kill per mode |
| Holder partitioned but alive | Cluster sees `TierDown`; it serves its own daemon locally; on heal it must `TierUp` before `PutStaged` | Same | write to a down tier rejected |
| Peer fetch slow or corrupt | Timeout or hash mismatch: `Unavailable`; fall back holder, cold store, missing | Same | corrupt peer answer refused |
| Holder flaps | Minimum dwell between `TierDown` and `TierUp`; flip metric | Same | no log entry per flap |
| Cold store unreachable | A node builds from tier and peers; flush backs off; uploads fail `UNAVAILABLE` when the tier budget is full [F: `write-path.md` 6.1] | Storage nodes the same; build nodes unaffected | build completes with the store down |
| Daemon cut off from all servers | Holds results, retries, kill ceiling | Same | ceiling kills after the bound |
| No leader known | Lookups `UNAVAILABLE`; local-tier reads of known digests still work | Daemon-only nodes use the candidates | local read during election |
| Credentials leaked from a node | Every node is a source: accepted risk | Only storage nodes; build node holds none | S1 to S4 |
| Compromised build node | n/a (all nodes hold credentials) | Can read by digest and write results for its own lease; not the object store | S5, S7, lease-bound result test |
| Envoy picks a stale leader | Not-ready node answers `UNAVAILABLE` | Same | fake-backend routing |
| Mixed versions | Version gate before `TierDown` | Same | no new entry before the gate |

Surviving one node loss needs 2 of 3 voters; two losses stop scheduling and lose nothing flushed. In isolated mode with storage-node voters, losing build nodes never affects quorum. Client view on a stage-1 failover: queued work is lost, running work is killed, `Execute` streams break. **[V]** How the build client reacts is checked first (PR 0).

## 9. Phased plan

Each PR's first test fails before the change. No test depends on wall time. **Every behaviour test runs in both modes**: the testkit offers `Cluster::trusted(n)` and `Cluster::isolated(storage, builders)` and a `both_modes!` macro, and a meta-test greps the behaviour suites and fails on any test not wrapped. Farm drills assert end state, not duration, and run on the trusted farm today and on an isolated topology as soon as one exists.

0. **Spike.** Drive a real build client against a single leader, break the stream, and record the recovery. Confirm `GetActionResult` checks output presence and how the client reacts to a download failure of an `Unavailable` output. Estimate log and snapshot growth per upload group. Measure the lookup RPC cost. Edits this document.
1. **Resolver and refusals,** plus the testkit constructors and the `both_modes!` meta-test. Red first: duplicates and an unmatched advertise are accepted today; `--daemon-only` binds a listener and accepts store flags (S1).
2. **One binary.** Server and daemon libraries behind one entry point. Red first: a daemon-only node reaches a leader listed behind a non-leader. S2, S3, S4, S9 land here, including the action-environment scrub for `--build` server nodes.
3. **Raft safety in `kbf-raft`** (small PRs): CheckQuorum, PreVote, wiped-disk rule, fatal append.
4. **Network transport and static voter group** with the list hash, tested with both topologies. Certificate classes and S7 land here.
5. **Epoch, readiness, follower refusal.** `Takeover` index as `Welcome.epoch`, `set_leader` wired, step-down clears state, lower epochs rejected; followers refuse sessions. Behind a flag until the drill passes. Red first: wall-clock epochs are not ordered; followers accept sessions today.
6. **Per-node tier over the Raft log.** `AllocEpoch`, `AddTier` and `PutStaged` forwarded from any server node, with the idempotency rules of 4.2. Red first: a non-leader's `PutStaged` is not forwarded. Test, both modes: an upload through node B is readable from node C through B's block with zero blob bytes through the leader (a counter on the leader's blob bytes).
7. **`TierDown` and `TierUp`** with the version gate, the down-tier write rule and the dwell. Red first: a blob on a down tier is served as `Present`. Lands before peer fetch is exposed to clients.
8. **Peer fetch.** `ReadBlock` service, digest check at the receiver, flush race, fallback order, input-cache keep. Red first: a flushed-while-reading object commits `ObjectUnreachable`; a corrupt peer answer is accepted.
9. **Envoy config** with a golden test, a fake-backend routing test and the retry table. One cluster.
10. **InstallSnapshot,** then compaction allowed.
11. **Control-plane trait:** in-process and remote implementations, the conformance suite and the role-change switch. **Data-plane trait:** in-process and remote, conformance suite; the remote one is what isolated mode needs. Red first: a daemon-only node cannot read or write blobs without cold-store access. Result writes tied to a lease (3.2) land with the remote data plane.
12. **Drills.** Trusted: kill the leader under a real build; kill a holder under load. Isolated: S8, then the same kills.
13. **Stage 2:** replicated scheduler from PR #303, trailer redirect, `Welcome` server list, leadership transfer, learner streaming if the lookup measurement justifies it, per-lease read authorization, locality hint, optional second-tier block replication.

## 10. Open questions for the owner

1. **Control plane in-process only on the leader.** A follower's daemon must reach the scheduler remotely. Is "in-process while the node is the leader, remote otherwise" what you meant? Recommendation: yes; the alternative is a relay in every server half.
2. **`TierDown` after how long, and by whom.** Is a leader-decided, committed `TierDown` (with `TierUp` on return, and `--hot-store-abandon` the only permanent marking) acceptable? Recommendation: yes; the alternative leaves dead holders' blobs `Present` and lets each read fail.
3. **Actions under a separate uid** on trusted `--build` server nodes. Cheap on a Linux server, harder on a Mac. Recommendation: make it the deployment guidance, enforce only the environment scrub in code.
4. **Per-lease read authorization** for daemons in isolated mode. Recommendation: ship result-write-by-lease now, read authorization in stage 2, and document the read-by-digest risk until then.
5. **Which server nodes a daemon-only node uses for data.** Recommendation: the candidates in v1 (make every storage node a candidate), the `Welcome` list later.
6. **Learners.** Non-voting nodes forward lookups to the leader in v1. Acceptable until the measurement in PR 0 and PR 6 says otherwise?
7. **Block replication** to a second tier before the acknowledgement: a stage 2 flag in both modes, or isolated mode only?
8. The single-server pilot's log and unflushed hot blobs are abandoned. Acceptable?
9. Stage 1 loses queued and running work on every failover, subject to PR 0. Acceptable as an interim?
10. Is one Envoy on the client box enough while CI depends on it, and is client traffic through the leader's NIC acceptable meanwhile?
11. Adopt the wiped-disk rule and a stop-all restart to change the list for v1?
12. Write side: verify-on-read first, acknowledge-after-flush for `UpdateActionResult` only if misses are frequent. Adopt?

## 11. Relation to PR #303 (`ha.md`)

- Absorbed as stage 2: the replicated scheduler (`FarmMachine`), epoch as log id, `Takeover`, the trailer redirect, the version gate and learner flow stay specified there.
- Superseded for v1: the two-cluster front (`kbf-reads` on any server). With per-node hot stores and a read path that needs `TierDown` and peer fetch first, v1 routes client traffic to the leader (section 6); widening is the later config change.
- Superseded: growth from 1 to 3 voters by `--raft-join`; v1 uses a static list.
- Added here: the node types and resolver, daemon-only nodes, the two modes and their security invariants, the stage-1 epoch from a `Takeover` index, the wiped-disk rule, the tier loss semantics, Envoy settings.

## Appendix: what changed, and review points not adopted

Changed from the first draft: the roles system is gone (two node types, `--build`, `--daemon-only`, `--non-voting`); leader-only uploads and "followers serve no reads" are replaced by per-node hot stores over the existing replicated index; the leader-local index is dropped; security, loss semantics and the two-mode rule are new.

Review points not adopted, and why:

- *Make every server node a voter, to avoid non-voting nodes.* Rejected: the owner decided that non-candidates are full non-voting members. Forwarding lookups to the leader is the v1 answer.
- *Cut the in-process control plane from v1.* Not cut. The interface and conformance suite land first and the in-process implementation is sequenced late (PR 11), because the owner decided it; only the order is the judges' point.
- *Cut the `Welcome` server list.* Deferred to stage 2, not cut: v1 uses the candidates.
- *Treat the simulator's caller-identity check as proof.* It is kept as a code tripwire (S5) and the farm probe is the proof (S8); the judges' point that identity is declared by the test is accepted.
- *Treat the dependency lint as proof.* Accepted as a tripwire only (S9).
- *Drop the `/readyz` mode text because it is a string match.* Kept as S10, with its limit stated; it exists so the weaker mode cannot be shown as the stronger.
- *A flag to drop inter-node TLS in trusted mode.* Rejected: a second way to run one link.
- *The first draft's leader-local blob-location index, rebuilt from reports.* Rejected: it is empty after every election and correctness would depend on report timing.
- *Operator's list as a seed with leader-driven learners and `--bootstrap`; follower reads without ReadIndex; gating readiness on the local daemon; a wall-clock epoch first; the `/v1/nodes` and metric names.* Rejected in the first draft for the reasons recorded there [F#744].
