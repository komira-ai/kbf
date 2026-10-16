# kbf architecture v2: frontend, state store, storage, daemon

Status: design, nothing built. It supersedes the leader-elected server cluster in `cluster.md`. Labels: **[F]** fact checked in the code at `200d7289` or on `main`, **[D]** owner decision, **[I]** inference, **[V]** verify before building on it. Numbers are arithmetic from stated hardware, not measurements; section 13 names the benchmarks to run first.

## 1. Decisions

- **[D]** kbf is four parts, as Buildbarn is frontend, scheduler, storage and worker.
  - **Frontend.** Stateless. Runs behind a load balancer (LB) in an autoscaling group; its DNS name is the one address clients, daemons and storage nodes know. Serves REAPI, runs the scheduler logic and the UI. Holds no durable state of its own.
  - **State store.** The queue, leases (each action handed out once), operations and membership. A small built-in replicated store for small farms; pluggable backends (etcd, Postgres) for clouds.
  - **Storage.** CAS, action cache (AC), SSD hot store, background flush to the cold store (MinIO or S3). Sharded by digest with a replication factor R (default 2). No leader. Storage nodes do not schedule.
  - **Daemon.** Runs actions.
- **[D]** Modes of the one `kbf` binary: `frontend`, `storage`, `daemon`, `storage-and-daemon` (the daemon reads and writes local storage in-process), `all-in-one` (frontend, storage, daemon and a single-node state store).
- **[D]** Kept: no per-action reservations (admission by CPUs, memory ladder); telemetry-driven cgroup sizing; NUMA placement; trusted-network and isolated-storage modes both tested; flags, not environment variables; no service bootstraps itself; tests never depend on timing; no compatibility with earlier formats.
- **[D]** This replaces the leader-elected design in `cluster.md`, the formation records in PR #750, the leader parts of PR #751, and the Envoy/xDS part of PR #746.

## 2. Answers to the discovery questions

- **How do daemons find storage nodes?** They do not look. A daemon knows the frontend name. The frontend sends the storage map (members plus hashing rule) in `Welcome` on the daemon's existing Session stream and pushes every change.
- **How does the frontend find storage nodes?** It reads membership from the state store. It never probes.
- **How do storage nodes find anything?** Same as daemons: they register with a frontend and hold a stream to it, and the map is pushed down it.
- **Does the frontend distribute work across storage nodes?** No. It places actions on daemons. It touches storage only to proxy client AC lookups and CAS bytes, routed by digest.
- **Does daemon-frontend traffic go through storage?** No. Work goes frontend to daemon directly. Daemons fetch inputs from, and upload outputs to, storage directly. Daemon-to-frontend traffic never carries bytes.

## 3. Crate mapping

- **[F]** Frontend: `kbf-front` (REAPI), `kbf-auth`, `kbf-sched`, `kbf-server`. State store: `kbf-raft`, the `ControlRecord` log, and a new `kbf-state` trait (no etcd or Postgres backend exists). Storage: `kbf-meta`, `kbf-tier`, `kbf-segments`, `kbf-objstore`, `kbf-front/src/cache/*`. Daemon: `kbf-daemon`, `kbf-node`, `kbf-transfer`, `kbf-driver-*`.
- **[F]** `kbf-front` owns both the REAPI services and the whole cache write path (`cache.rs:182`); storage is cut out along `cache/*` (P2). Scheduler state is not in the Raft log today and `farm.rs` runs under one `Mutex<State>`.
- **[F]** The `Blobs` trait (`kbf-server/src/local.rs:36-40`) is the in-process seam `kbf-all-in-one` uses. The storage-and-daemon plane is a `Cas` implementation over the local store, modelled on it.

## 4. State store

### 4.1 Contract

A backend is admitted only if it passes one conformance suite, run against an in-memory fake, the built-in store and every external backend.

- **C1.** Linearizable compare-and-set per key, with a version.
- **C2.** Atomic transaction over several keys.
- **C3.** Strictly monotonic counter (lease sequence, map generation, boot counters).
- **C4.** TTL keys with expiry decided by the store's clock, observable as an event, on an injectable clock in tests.
- **C5.** Ordered prefix scan and a change feed that resumes from a revision. Polling by revision is the baseline; push (watch, `LISTEN/NOTIFY`) is only an optimization, because notifications are lossy and do not survive poolers.
- **C6.** Durable acknowledgement across restart and failover.
- **C7.** Group commit, because about 5 durable writes per action **[I]** make an ungrouped fsync the limit.

Redis is not offered for lease handout: asynchronous replication fails C6 on failover, and clustered mode fails C2 without shared hash tags. Postgres needs synchronous commit (C6) and a reaper (C4). etcd meets C1 to C6 natively.

### 4.2 Contents

Operations; a queue indexed by platform, size class and QoS (a claim never scans unrelated entries); `inflight/<action_key>` dedup; leases; one `session/<daemon>` row; members; the map; whole-machine holds; memory-ladder floors; loss marks; repair records.

### 4.3 Built-in store

- `kbf-raft` plus a key-value state machine; `ControlRecord` (`Lease | Result | Refusal`, "commit before Start", `work.rs:323-330` **[F]**) becomes log entries.
- v1 is a single voter, embedded in the single frontend and in `all-in-one`. The docs say plainly: a frontend restart is a store restart, losing its disk loses the queue, and v1 is not highly available.
- A replicated built-in store must not live in an autoscaling group: a scale-in can remove a member and lose quorum. It is a separate small tier (owner choice 1), changed by an operator verb (`kbf admin state add-member`), never a startup flag. #747 is held for it, and #750's no-vote member and `drain`/`undrain` apply here; its formation records are void.

### 4.4 Epoch, expiry and outage

- **Epoch** is the store incarnation, bumped on restore, failover or replacement. `Welcome.epoch` carries it and a daemon seeing a new epoch kills leases from the old one **[F]**. A failover that did not bump it would leave stale leases running. Lease id is `(epoch, seq)`.
- **Renewal is per daemon session.** One `session/<daemon>` row is extended per interval; a `lease/<id>` row is a fencing token written at claim and commit only.
- **Post-outage grace.** After recovery or leader change every expiry timer restarts with a grace window; otherwise an outage makes all leases look expired and requeues all running work. Red test: a fake clock stopped across a simulated outage requeues nothing inside the window.
- Expiry is judged by the store's clock, never a frontend's.

## 5. Scheduler

**v1: one frontend** runs the scheduler as `farm.rs` does today, writing every change through the `kbf-state` trait; memory is a cache, the store is the truth. Global first-fit, holds and the ladder are unchanged. **[I]** Clients re-submit a lost Execute and dedup absorbs it, so what must be durable is small: operations, leases, membership, the map, the epoch.

**Several frontends: per-frontend pull-claim.** Placement is global first-fit under one mutex **[F]**, so N frontends cannot share it. A frontend owns the daemons connected to it and claims for their free slots:

- Predicate: platform, CPUs, ladder floor, cordons, whole-machine holds. Floors and holds come from a watch-fed cache; each frontend keeps a head cache per queue partition.
- **Batch claim:** one transaction claims k entries for k free slots, starting at a random offset in the top entries to bound contention.
- Transaction: require `op.state == Queued` at the version read, take the next `seq`, write `Leased(id)` and `lease/<id>`, delete the queue entry. A loser fails the first check and moves on, so one live lease per op is a store invariant.
- Lost: global best-fit and strict cross-daemon QoS fairness (QoS order within a partition is kept). The alternative, an elected placement leader, is what is being removed.

**Fencing.** Every `Start` and `Result` is a CAS on `op.lease == id`; a stale daemon gets `FENCED`. With several frontends a second fence is required: `session_gen` is bumped by CAS on every Hello, and a frontend writes lease changes for a daemon only while its `session_gen` matches, so an old frontend cannot requeue a moved daemon's live leases.

**Guarantee.** At most one live lease and one committed result per operation. Execution is at-least-once: a partitioned daemon can run past expiry and its result is fenced out; content-addressed outputs make a stale upload junk, not corruption.

**Reconnect.** A daemon's `Hello` carries `instance_id` and `predecessor_instance_id` **[F]**. The new frontend bumps `session_gen`, lists the daemon's leases and reconciles with its reported running set (the `worker.rs:1-52` reconcile **[F]**): still-running leases are re-bound, unknown ones, including claimed but never delivered, are requeued.

**Poison actions.** Lost-lease requeues have their own cap (`loss_requeues`), separate from `attempt`; the daemon's report distinguishes the cause where it can; over the cap the operation is refused.

**Waiters.** The operation is durable at submit. A waiter is held by the frontend with the client's stream and reached through the change feed, or in-process when committer and waiter are the same frontend (the v1 case). A client of a dead frontend re-attaches with `WaitExecution` through the LB.

## 6. Discovery, membership and the storage map

- Every node is configured with `--frontend <dns>` plus certificate flags; storage nodes and daemons add `--advertise-addr` where the bind address is not routable. Nothing else.
- **[F]** `connect.rs` already re-resolves DNS and backs off to 30 s.
- **Registration.** A storage node or daemon opens a stream to any frontend and sends role, `instance_id`, advertise address, capacity, zone label and protocol version. The frontend writes `member/<id>` with a TTL; the member id and boot counter (section 8.4) are assigned by the store, so no node picks its identity. Membership lives in the store, so a reconnect to another frontend loses nothing.
- **Storage nodes do not talk to the state store.** They reach it only through the frontend. This keeps the store's attack surface to frontends.
- **Heartbeats** refresh the TTL through the frontend; expiry is store-judged on the injectable clock reused from PR #746.
- **Map** = `{generation, members[{id, addr, zone, weight, state}], rule, R, min_protocol, deny_list}`. States: `joining`, `catching-up`, `active`, `draining`, `down`. `Welcome` carries it and a new `MapUpdate` message pushes changes; storage nodes get it down their registration stream.
- **Dwell.** A member whose TTL lapses becomes `down` but stays in the map for a dwell (reusing `TierDown`/`TierUp` and `--hot-store-abandon` from `cluster.md` 4.4 **[F]**) before it leaves and its slice is re-replicated. This prevents flapping. A generation bump is one CAS on `map`; two frontends racing produce one winner and one retry.
- **Staleness protocol.** Every storage RPC carries the caller's generation. A node that sees an older one answers `STALE_MAP` with the current number and the caller refetches; a node that sees a newer one fetches it. A lost push is therefore harmless.
- **Admission.** Holding a valid storage-role certificate gets a node a `joining` entry, not ownership. It becomes `active`, and a first owner, only after it has caught up (section 8.4). Who may hold a storage certificate is the operator's CA policy.
- **Hashing.** Rendezvous (highest-random-weight) hashing over digest, member id and weight; owners are the top R. This is the consistent-hashing family Buildbarn uses for sharding: removing a member remaps only the blobs that resolved to it, and there is no ring state to coordinate. Cost is one hash per member per digest, negligible at 6 to 100 members. Zone-aware second owner is optional and off by default.

## 7. Traffic

- Client to LB to frontend, always. Client AC and CAS: frontend to the owning storage node, pass-through, never buffered. Work: one daemon stream to a frontend, no bytes. Input fetch and output upload: daemon directly to owners via the map. Flush: storage to the cold store.
- **Proxy, not redirect.** REAPI ByteStream has no redirect, so stock clients could not follow one **[I]**; proxying keeps one address and one authentication point.
- One routing library implements `Cas` over the map for frontend and daemon, with an in-process arm for `storage-and-daemon`; `kbf-transfer` keeps its flight dedup across endpoints.
- Prefer an L4 LB for daemons; raise idle timeouts for L7; gRPC keepalive stays below the LB idle timeout.

## 8. Storage

- **Writes: chain replication.** From a 1 GbE frontend or Mac, sending to all R owners halves unique upload (about 58 of 117 MB/s) **[I]**. A writer sends each blob once to the primary (rank-1 owner), which forwards to the replica over the storage fabric and acknowledges when both hold it on the hot tier. With the replica down, the primary acknowledges after its own durable write and records `repair/<digest>` in its journal and, through a frontend, in the state store, so the record survives the primary. "Live owner" is judged by the map generation, so a stale view gets `STALE_MAP`, not a silent single copy.
- **Reads.** Owners in rank order, then the previous generation's owners, then the cold store through an owner's index. Non-first hits trigger rate-limited read-repair. FindMissing is split by owner and asks the primary only; a replica is not asked on a miss, since cold builds are mostly misses.
- **AC.** Sharded by action digest, with the small `ActionResult` inline. An entry is written only after its closure is acknowledged: for a client `UpdateActionResult` the AC owner checks the closure with batched FindMissing to the other owners; for an action's own result the daemon supplies the acknowledgements. Reads present-check the result blob only (cached per generation). Closure blobs flush before the AC entry.
- **Loss marks.** When a hot tier is abandoned, digests it held and never flushed get `loss/<digest>`; an AC read whose result blob is marked is a miss. Residual: a later-evicted closure blob gives a client a missing output and a re-execute, not corruption. The lost-blob gauge makes it visible.
- **Join and drain.** Generation g+1 is published; readers use g first until handoff finishes. A joining node is a write owner at once but read last; old owners copy changed blobs, throttled; then it is `active`. Drain is the reverse and is not a kill. A node drops a blob only after every new owner confirms it, or the cold-store PUT is acknowledged and the index entry committed.
- **Rebuild.** A new owner has no index for blobs a previous owner flushed and rebuilds from self-describing footers (`startup_pass.rs` does it for `file://` **[F]**; listing S3/MinIO is new work **[I]**), staying `catching-up` until done.
- **Object keys.** `AllocEpoch` is a global write (`state.rs:25` **[F]**); ids become `(node id, boot counter)`, the counter from C3 at registration, which removes it from the data path and avoids collision with the old server's objects.
- **One flusher.** The rank-1 owner flushes; the replica evicts after the flush is confirmed and flushes only if the primary is down past the dwell. Otherwise both write the same blobs into different objects and double PUT bytes and S3 cost.
- **Segments.** One `Segments.Begin/PutRange/Commit` session per owner, with a minimum segment size and a bound on open sessions so segments do not shrink to about 1/(6/R) of today's **[I]**.

## 9. Security

- mTLS on every hop with the role (frontend, storage, daemon) in the certificate; storage accepts only expected roles per endpoint. Certificates and keys come from files; nothing in environment variables. Clients use TLS at the LB.
- Daemon identity and deny-list checks (`identity.rs` **[F]**) must be repeated on storage nodes, because the worker listener's `ByteStream` moves there. The frontend issues a short-lived signed storage-access grant in `Welcome`; storage nodes verify it with the frontends' public key (provisioned as a file). The deny-list rides in the map, so a denied daemon loses storage access when the generation turns.
- **Trusted-network:** daemons and storage trust each other on the private network; mTLS stays on.
- **Isolated-storage:** only storage nodes hold cold-store credentials. Daemon-only nodes (the Macs, a cloud daemon ASG) hold none and reach bytes only through storage. A test proves a daemon-only process cannot reach the cold store.
- **Credential gap on `storage-and-daemon`.** A node that holds cold-store credentials and also runs untrusted actions is not isolated unless storage runs under a different uid from actions and the credentials are unreadable by the action's uid. Without that, the six Broadwell nodes are trusted-network only. A test must show an action cannot read the credential (owner choice 5).
- The state store is reachable only from frontends (and its own members).

## 10. Locality

`storage-and-daemon` nodes keep a local read cache of files they produced or fetched; hashed owners stay authoritative and the cache is never consulted for correctness. With 6 nodes and R=2 about 2/3 of input bytes are remote even there **[I]**; at 10 GbE that is fine, and SATA SSD is the tighter limit. Each daemon exports its local and remote byte fraction so locality-aware claim ordering later has data.

## 11. Deploy shapes and operation

- **Single box.** `kbf all-in-one --data-dir ./kbf-data`: one process, `file://` cold store, embedded store, no LB, no formation step.
- **Our farm.** Frontend on the client box (1 GbE), six `storage-and-daemon` Broadwell nodes (10 GbE), two `daemon` Macs (1 GbE); one CA, `--frontend`, certificate flags and `--advertise-addr` on each node. The single frontend is a scheduling single point of failure; running actions finish.
- **Cloud.** Frontend ASG behind an LB, external store, storage on local-NVMe instances, S3, a daemon ASG scaled on queue depth. Storage scale-in is a drain, not a kill; storage is not autoscaled on a metric in v1.
- **Growth.** Adding a node is one command with no edit on existing nodes. The README is the smoke test.
- **Operation.** `/metrics`, `/healthz`, and `/readyz` meaning store reachable and map loaded. Metrics: queue depth by platform, claim wait, idle CPUs, under-replicated blobs, handoff backlog, hot-store fill, flush lag, lost blobs. `Hello` and the map carry protocol versions (`min_protocol`); store schema changes expand first and contract a release later so rollback is safe; upgrade order is store, storage one at a time, frontends, daemons.

## 12. Failure modes

- **Frontend dies:** the LB drops it; daemons re-resolve DNS and reconcile; clients re-attach by operation name; a claim never delivered is requeued.
- **Store unavailable:** no new claims; running actions continue; grace on recovery; a failover bumps the epoch.
- **Daemon partitions past expiry:** duplicate run, result fenced, outputs idempotent.
- **Storage node dies:** reads from the replica, one-copy writes with a durable repair record, drop from the map after the dwell. Both owners of an unflushed blob lost: loss mark, AC miss, rerun.
- **Cold store down:** flush backs off, hot tier absorbs, then writes refuse (`cluster.md` 4.4).
- **Client box dies (v1):** the whole control plane is down; accepted.

## 13. Phased plan (each PR lands a test that fails first)

- **P0** This document; `cluster.md` marked superseded.
- **P1 `kbf-state`.** Trait, fake, single-voter built-in, suite C1 to C7; scheduler writes through it. Red: non-linearizable fake fails C1; caller-clock expiry fails C4; restart replays acknowledged claims; stale `complete` rejected.
- **P2 `kbf-storage`.** Cut `cache/*` out; `storage` mode; one-member map; `storage-and-daemon` with the in-process `Cas`. Red: the REAPI conformance suite passes through a separate storage process.
- **P3 Membership and map.** Register, heartbeat, `Welcome.map`, `MapUpdate`, rendezvous, dwell, `STALE_MAP`. Red: removing a member remaps only its blobs; TTL expiry on a fake clock.
- **P4 Sharding, R=1.** Router, per-owner Segments. Red: a blob exists only on its owner.
- **P5 R=2.** Chain replication, repair records, one flusher, loss marks, AC ordering. Red: kill an owner mid-build and it finishes; AC write refused with a missing closure blob.
- **P6 Rebalance.** Join, drain, previous-map read, handoff, S3 footer scan. Red: a join mid-build never yields a false miss.
- **P7 Security.** Storage-side identity, grants, uid separation, isolated-mode test.
- **P8 Cutover.** New frontend and storage run against the same MinIO while the old server serves; storage rebuilds its index; daemons flip by pointing `--server` at the frontend name; rollback is flipping back (cold store append-only, new keys carry node ids).
- **P9 Several frontends.** Batch pull-claim, `session_gen`, grace, poison cap, `WaitExecution` from a second frontend. Red: two claimers, one winner; a result committed on B reaches a waiter on A. Fake clock and deterministic interleaver.
- **P10** External backend (owner choice 2) passing P1's suite; replicated store; mixed-version tests; UI.

Before P4 and P5, benchmark on the farm (none run yet): no-op action latency p50/p99, store commits per second with group commit, proxy MB/s at R=1 and R=2 on the 1 GbE box, AC lookup latency, PUT bytes with one flusher versus two.

## 14. Existing documents and PRs

- **`cluster.md` (#744).** Carries over: 3.1/3.2, 4.3, 4.4, the `Takeover` epoch idea. Replaced: 5.2, 5.3, 6.
- **#750:** superseded; no-vote and `drain`/`undrain` move to the replicated store, formation records are void.
- **#751:** leader tracking, fencing and leader finding are void; the REAPI proxy, UI scope, registration and mTLS flags carry into a trimmed frontend doc.
- **#746:** registration shape and injectable-clock registry reused; xDS dropped. **#747:** held for the replicated store. **#743:** compatible with the in-process `Cas`; rebase on P1 and P2.
- **`deployment-topology.md`** and `storage.md` 432-460 describe the opposite and need a rewrite after P2.

## 15. Owner's remaining choices

1. **Where does a replicated built-in store run?** Recommendation: a separate small tier (a sixth mode, `state`, one or three nodes); frontends in an ASG are never members. Embed the store only in `all-in-one` and the one-frontend v1.
2. **First external backend.** Recommendation: etcd (meets C1 to C6 natively), then Postgres with synchronous commit. Redis is not offered for lease handout.
3. **Execution semantics.** Recommendation: accept at-least-once with fenced results.
4. **Write durability.** Recommendation: ack when primary and replica both hold the blob (chain over the fast fabric); degrade to one only with a durable repair record.
5. **The six Broadwell nodes.** Recommendation: separate uids for storage and actions, with a test that an action cannot read the cold-store credential; otherwise they are trusted-network only.
6. **Placement.** Recommendation: single-frontend scheduler as today in v1; per-frontend pull-claim only when a second frontend exists.
7. **Closure and rebalance rulings.** Recommendation: write-time closure check plus a result-blob present-check and loss marks on read; read under the previous map with background handoff.
8. **Frontend placement on our farm.** Recommendation: keep it on the client box; chain replication removes the double upload. Move it to a 10 GbE node if the proxy benchmark shows the NIC is the limit.

## Appendix: points considered and rejected

- **Writer sends to all R owners:** halves upload on 1 GbE; chain replication instead.
- **Both replicas flush:** correct but doubles PUT bytes; one flusher.
- **Three frontends as store members:** breaks scale-in and statelessness.
- **Storage nodes reading the store:** wider attack surface; the frontend pushes the map.
- **Per-lease heartbeats and per-daemon queue scans:** store write and read cliffs; per-session renewal, indexed partitions, batch claims.
- **`LISTEN/NOTIFY` as the feed:** lossy; polling baseline.
- **Redis for leases:** fails C6.
- **Any-owner AC hit with W=1 and no loss marks:** unbounded missing-output risk; write ordering and loss marks added.
- **Certificate alone admits a hash owner:** `joining` and `catching-up` states added.
- **Lease-id fencing alone:** enough for one frontend only; `session_gen` in P9.
- **Asking the replica on a FindMissing miss:** an extra round trip on every genuinely missing blob.
- **Unlimited read-repair:** turns reads into writes; rate-limited.
