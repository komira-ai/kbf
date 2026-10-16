# `kbf frontend`: one front door, no Envoy, no xDS

Status: design, nothing built. Written 2026-10-16. Labels: **[F]** fact read from `main` at 200d7289 (cluster.md, ADR 0001, the crates), **[I]** inference, **[V]** needs verification before code, **[D]** owner decision, **[Q]** open question (section 12). Nothing here was measured; no benchmark was run.

This document supersedes the client-routing parts of `cluster.md` (PR #744: section 6, 6.1, the Envoy rows of section 8, decision 10), the draft gateway and xDS work (PR #746), and the formation part of PR #750. The no-vote and drain parts of PR #750 stand (section 11).

## 1. Decision and recommended election model

**[D]** Drop Envoy and xDS. Add a fourth mode of the one `kbf` binary, `kbf frontend`. An open-source user runs frontends, servers and daemons, and nothing else. The frontend:

1. hosts the UI;
2. is where servers register, and where cluster formation is decided (one lock, one fsynced record);
3. proxies all REAPI client traffic (buck2, Bazel, CI) to the current leader;
4. tells daemons who the leader is.

**Recommended election model: the frontend forms the cluster once; Raft among the servers elects, as today.** The alternative, a frontend-owned lease with Kafka-style in-sync-replica (ISR) log shipping, was drafted and judged. The reasoning:

- **It matches the request.** The request says the frontend "has the initial leader election". That is formation. Re-election after a crash stays with Raft.
- **The frontend stays off the safety path.** Its only hard state is a cluster id. A frontend outage blocks new client sessions; writes, failover and daemons continue. Under a lease model every follower failure needs the frontend to shrink the ISR, so frontend downtime becomes farm write-unavailability, and losing its disk loses the only record of which node is safe to promote.
- **No new replication protocol to prove.** ISR shipping needs a superseding ADR and its own simulation. ADR 0001 [F] chose Raft by the deterministic-simulation criterion; this design keeps it. No new ADR is needed.
- **The work is comparable.** Both models need a transport and a catch-up path. Raft additionally needs membership change; PreVote and CheckQuorum are liveness and read-freshness features, not write safety.

The cost is that `kbf-raft` must be finished. It lacks InstallSnapshot, membership change, PreVote and CheckQuorum; `kbf-server`'s `raft_log` is a single-voter log with `NoTransport`; no multi-node transport exists; `set_leader` has no caller [F]. That work is on the critical path with or without the frontend, because the log carries the CAS index, staged table, action cache and `PutStaged` (cluster.md 4.1).

Standing decisions carried unchanged: no vote flags (voters and replicas are chosen automatically by lag, health, load and failure domains; operators use `kbf cluster drain|undrain`); no `--bootstrap` and no init command; two modes (trusted-network all-in-one everywhere; isolated-storage servers with daemon-only build nodes), both always tested; a node's own blobs stay local; hot store on SSD, cold store in object storage; admission by CPUs and the memory ladder; flags, not environment variables; tests never depend on timing; no compatibility with earlier formats.

## 2. What the frontend does and does not do

**Does:** registration and the formation decision; tracking the leader from registrations; a byte-level HTTP/2 proxy to the leader; client TLS termination and `kbf_auth`; `Resolve` for daemons; a read-only UI.

**Does not:** take part in Raft; store blobs or build state; decode REAPI messages; carry Raft traffic (never behind the frontend) or daemon work traffic; decide voters (the leader does, automatically); provide HA in v1.

It is a new crate, `kbf-frontend` (not `kbf-front`, which is the REAPI implementation over the store [F]).

## 3. Registration and formation

### 3.1 Protocol

`Registration.Register` is a unary mTLS call, repeated every `ttl/3` as a heartbeat. It extends the `gateway.proto` of PR #746 (`reapi_address`, `is_leader`, `term` returning `ttl_ms`) [F].

Request: `protocol_version`, `node_id`, `cluster_id` (empty if the node has no storage), `raft_address`, `reapi_address`, `is_leader`, `raft_term`, `log_initialized`, `last_log_index`. Reply: `ttl_ms` and one directive: `CREATE{cluster_id}`, `JOIN{cluster_id, leader_raft_address}`, `NONE`, `RETRY` or `REFUSE{reason}`.

Servers take `--frontend=<address>`. It replaces `--join`, `--cluster-id`, `--bootstrap`, `--accept-any-cluster` and `--electable`. `kbf all-in-one` without `--frontend` is a standalone single node and never contacts anything.

### 3.2 The record and the rules

The record is one file in `--record-dir`: `cluster_id`, `creator_node_id`, `state` (`forming` or `formed`), `schema_version`. The frontend holds an `flock` on the directory for its lifetime, so two frontends cannot share a record (two hosts are not covered; the operator runs one). Every decision runs under one mutex. The file is written to a temporary name, fsynced, renamed, and the directory fsynced, and only then is the reply sent.

1. **No record, registrant has empty storage.** Mint a `cluster_id`, write the record as `forming` with the registrant as creator, reply `CREATE`. A concurrent second fresh registrant waits on the mutex and gets `RETRY`.
2. **`forming` becomes `formed`** only when the creator's heartbeat reports `log_initialized` (its Raft configuration is durable). `JOIN` is refused (`RETRY`) while `forming`. A retry from the creator gets the same `CREATE` answer, matched by `node_id`. If the creator dies before initializing, no data exists anywhere; the operator deletes the record file and the next fresh registrant creates.
3. **Record exists, registrant has empty storage:** `JOIN`. The leader adds it as a learner and promotes it by the automatic voter policy.
4. **Registrant carries a different `cluster_id`:** `REFUSE`. The existing cluster wins.
5. **A server with empty storage and no reachable frontend does not start serving;** it retries. A server with storage runs Raft with its peers regardless of the frontend.
6. **No record, but a registrant carries a `cluster_id` (frontend disk loss).** The frontend never creates. It starts a listening period of one `ttl` in which it answers `RETRY` to everyone. If every registrant carrying an id agrees, it writes the record `formed` with that id. If two ids appear, it refuses all and shows the conflict in the UI; the operator restarts it with `--confirm-cluster-id=<id>`. All real state is on the servers, so adopting an id cannot lose data.

Limit, stated plainly: a frontend cannot tell "first start ever" from "disk lost" when the directory is empty. The listening period closes the common case (members are up and re-register); it cannot close the case where every member is down while a fresh node arrives. Operators keep the record directory on durable storage.

## 4. Leader tracking and fencing

Election is Raft among the servers. The frontend's registry is soft state with TTL entries, an injectable `Clock` and a `watch` channel (from PR #746's `registry.rs` [F]), so tests never sleep.

**Routing rule.** Among entries with `is_leader` set, in the cluster named by the record, choose the highest `raft_term`. Do not compare against terms reported by non-leaders: an isolated non-leader that inflates its term (there is no PreVote yet) must not block routing. A candidate is also verified by connecting to it over mTLS; a node answering `NOT_LEADER` is dropped and the frontend re-resolves. Within one term Raft has one leader, so no name tiebreak exists.

**What a stale entry can do.** A deposed leader keeps its entry for up to one TTL. It cannot commit a write, because a quorum will not acknowledge it; write safety does not depend on the frontend or on CheckQuorum. What it can do is serve reads from its own state. The window is: until it learns of a higher term. Bound it explicitly: a leader serves action-cache and index reads only while a quorum heartbeat is fresh (CheckQuorum, or ReadIndex), and fails them with `NOT_LEADER` otherwise; the window is one election timeout. Direct callers (daemons) get the same bound, not just clients.

**Daemon fencing.** The daemon epoch stays the `Takeover` entry's committed index [F: cluster.md 5.2]; daemons refuse a lower one. Replacing the wall-clock `process_term()` [F: `farm.rs:88`] is part of the server-registration PR.

## 5. REAPI proxy

**Routing in v1: everything to the leader** (as #744 chose). `grpc.health.v1` is answered locally; unknown paths get `UNIMPLEMENTED`. The mixed routing in `deployment-topology.md` (reads and uploads to any ready server) is later, after peer fetch and the lookup path pass in both modes.

**Cost, stated.** Client bytes go client, frontend, leader, which conflicts with "file bytes avoid the leader where possible". It is accepted for v1. The later design is a bounded peek at the first message of ByteStream `Read` and `Write` (the resource name) to route to the node that owns the blob; that breaks "never decodes" in a limited way and is v2. The first proxy PR adds byte counters, since no client-traffic figure exists [F: r3/r4 data have none]; the 1 GbE client box is unmeasured.

**Mechanism.** A byte-level HTTP/2 proxy that routes by `:path` and never decodes bodies. Candidates: `hyper` (a workspace dependency, used today with `http1` and `server` only) or a tonic raw-bytes codec on `h2` 0.4.20 [V]. **A spike picks one, and it is the first proxy PR**: a 64 MiB ByteStream round trip, trailers (`grpc-status`) preserved, flow control honoured.

**Streaming and flow control.** Frames are forwarded without buffering a whole message (unlike `kbf-front`, which buffers up to 17 MiB [F]). The proxy releases client window only after the upstream stream accepted the data, so a slow leader back-pressures the client. Memory is bounded by window size times `--max-concurrent-streams`; at the limit the answer is `RESOURCE_EXHAUSTED`. Windows are flags, sized from bandwidth times round-trip time, per stream and per connection; a 1 MiB window gives about 50 MB/s per stream at 20 ms RTT, below a 1 GbE link, so the default must be derived, not guessed.

**Upstream connections.** A small pool of h2 connections to the leader, each with its own window, so one stalled upload does not block unrelated unary calls on a shared connection window. Test: hundreds of concurrent unary calls beside one stalled stream.

**Deadlines and idle.** `grpc-timeout` is forwarded unchanged; client cancel or disconnect cancels upstream. There is **no idle timeout**: `Execute` and `WaitExecution` are silent for up to 300 s queued and about an hour running [F]. Dead peers are found by h2 PING keepalives on both legs (flags; the server sets none today [F]).

**Limits.** The leader stays authoritative: 17 MiB messages, 16 MiB batches, 1 GiB blobs [F]. The proxy does not parse messages, so v1 has no early rejection. Raise h2 frame and header-list limits above what these need.

**Failover mid-call.** The proxy never replays. In-flight calls end `UNAVAILABLE` and buck2 and Bazel retry. With no leader, a new call gets `UNAVAILABLE` with retry info after waiting at most a flag-set time.

**TLS and identity.**
- *Client leg:* the frontend terminates REAPI TLS and holds the client-facing name; `kbf_auth` layers and `RequestMetadataLayer` (the invocation id) run here [F]. Neither #746 nor #750 designed this, because Envoy did it.
- *Leader leg:* mTLS with a distinct *frontend* certificate class. A server serves client RPCs only to that class, so direct client access is closed. The frontend strips every client-supplied `x-kbf-*` header and sets the verified principal itself. On loopback (all-in-one with a frontend on the same host) plaintext is allowed.

## 6. How daemons find the leader

Daemon traffic does not go through the frontend (nodes have 10 GbE; the client box has 1 GbE). A daemon calls `Resolve` (daemon certificate class) and gets `{leader_address, epoch, server_list}`, then connects to the leader directly. It persists the last answer. With the frontend down it uses the cached leader and list, and any server answers `NOT_LEADER{hint}` from its Raft state, so the daemon re-finds the leader without the frontend. Daemons refuse a lower epoch.

## 7. UI scope, v1

No UI exists today [F]. v1 is read-only, served on `--ui-listen` behind TLS and the same `kbf_auth` layers, with assets embedded in the binary (server-rendered HTML, no node toolchain) [I].

| Page | Source |
|---|---|
| Nodes (role, term, TTL age, drain state) | registry plus the leader's `/v1/nodes` |
| Recent builds | leader's `/v1/invocations/*` |
| Health | registry, leader `/readyz`, formation state including conflicts |

The operator API listener has no TLS and open reads [F], so the frontend reads it over the cluster network and does not re-expose blob routes. Drain and undrain stay CLI-only. A queue and lease page needs new leader endpoints and is deferred.

## 8. Failure modes

| Event | Effect | Behaviour |
|---|---|---|
| Frontend crash or restart | No new client sessions; in-flight client streams die | Servers and daemons run on. Registrations rebuild the leader view within one TTL. The record survives, so no re-formation. |
| Frontend restart, empty record directory | Listening period (3.2 rule 6) | Never creates a second cluster; conflicts refused and shown. |
| Leader crash | Raft elects; the new leader registers | Clients see `UNAVAILABLE` and retry. Independent of the frontend. |
| Partition: frontend to leader only | Entry expires; no leader known | Clients get `UNAVAILABLE`; cluster and daemons unaffected. |
| Partition: old leader cut off | Cannot commit; stale reads bounded by one election timeout (section 4) | Frontend prefers the new leader's higher term once it registers. |
| Creator dies between `CREATE` and initialized | Record stays `forming`; joiners get `RETRY` | Operator deletes the record; no data existed. |
| Fsync failure at the frontend | Cannot form | Never answers `CREATE` before the fsync completes. |
| Slow client or proxy | Window stalls | Backpressure; memory bounded per stream. |
| Dead leader link | PING timeout | Stream cancelled, `UNAVAILABLE` to the client. |

## 9. Security

- **Registration:** server-class mTLS only. `cert.rs` from #746 accepts serverAuth with exactly one DNS SAN and refuses daemon certificates with `PERMISSION_DENIED` [F]. The SAN must equal the claimed `node_id` and the host of `reapi_address`, so one server cannot impersonate another.
- **Classes:** server, daemon and frontend, so no class can impersonate another. A compromised server certificate can still register; issuance is the real control, the same trust as Raft peers.
- **Leadership hijack:** a valid server certificate could claim `is_leader`. The routing check connects to the claimant and relies on its Raft answer; a persisted membership guard is not in v1 (appendix).
- **Frontend compromise** lets an attacker redirect clients and read their traffic; Envoy had the same power.

## 10. Operator story and compatibility

- `Register` carries `protocol_version`; the record carries `schema_version`. Upgrade order: frontend, then servers, then daemons; a newer frontend accepts an older server for one release.
- The frontend runs under a supervisor with `/readyz`. Clients need one stable name (DNS or a tailnet ingress; the topology doc's choice stands).
- Certificates need classes (server, daemon, frontend, client TLS). Using one CA with classes told apart by SAN or EKU and a dev-certificate helper keeps the first hour short; the "no bootstrap" rule does not forbid it, since it creates no cluster.

## 11. Phased PR plan (red-first, no test depends on time)

Each PR names a test that fails first. Behaviour tests run in both modes from PR 5.

1. **Docs.** This document, the `cluster.md` pointers, closing #746, and folding the formation text of #750 into this one (its no-vote and drain text stands; "gateway" becomes "frontend").
2. **`kbf-frontend`: registry and certificates.** Reuse `registry.rs`, `cert.rs`, `tests/support` and the mTLS flags (`--tls-cert`, `--tls-key`, `--client-ca`, `--ttl-ms`) from #746. Red: daemon certificate refused; wrong CA; SAN not equal to `node_id`; a lower-term leader claim does not displace a higher one.
3. **Formation record.** Red: two concurrent fresh registrants give exactly one `CREATE`; a crash after fsync and before the reply returns the same answer to the creator (fault-injected filesystem); `JOIN` refused while `forming`; mismatched id refused; disk-loss adoption and conflict. Planted mutant: answer before fsync must turn the crash test red.
4. **Raft prerequisites in `kbf-raft`** (several PRs, own plan): transport, membership change, InstallSnapshot, CheckQuorum, PreVote. Membership change lands before `JOIN` is promised. Red: a partitioned leader fails reads. **Multi-server clusters are refused until the transport and replication land**; before that, a deployment is one server plus the frontend.
5. **Server registration client** (`--frontend`), no start without a directive, epoch from the `Takeover` index.
6. **Proxy.** Spike, then pass-through, with byte counters. Red: 64 MiB round trip; bounded memory with a stalled reader; deadline and cancel propagation; a silent stream under paused tokio time with injected keepalives; trailers kept; spoofed `x-kbf-*` stripped; no leader gives `UNAVAILABLE`; hundreds of unary calls beside one stalled stream.
7. **TLS and auth on both legs.** Red: a direct client cannot call the leader.
8. **Daemon `Resolve` and cached fallback.** Red: lower epoch refused; work continues with the frontend down.
9. **UI v1.**
10. **Migrate `kbf-it` and deploy docs;** remove remaining Envoy references.
11. **Later:** frontend HA (section 12, Q6); first-message peek for blob bypass.

## 12. Choices that remain with the owner

1. **Election model.** Recommended: the frontend forms, Raft elects. Alternative: the frontend owns a lease and servers ship logs to in-sync replicas. It needs a superseding ADR and an ISR design with a clock-free epoch fence, and it makes the frontend part of write availability. Recommendation: Raft.
2. **Scope of the Raft completion track** (PR 4): transport, membership change, snapshots, CheckQuorum. Recommendation: yes, as its own plan; a 2-server cluster is then one voter plus one learner with manual promotion, since Raft needs three voters to fail over automatically.
3. **Client bytes through the leader in v1.** Recommendation: accept for one release, measure with the byte counters, then decide on the first-message peek.
4. **Frontend disk loss.** Recommendation: the listening period plus `--confirm-cluster-id` on conflict. Alternative: require the operator to back up the record. Either way it is a documented runbook step.
5. **Where the frontend runs.** On the client box the proxy hop is loopback and costs CPU, not NIC [I]. Recommendation: start there, single instance, under a supervisor.
6. **Frontend HA.** Not in v1. Recommendation: route-only extra frontends later, which refuse `CREATE` and learn the cluster id from registrants; this avoids consensus among frontends but needs a stable client address.

## Appendix: points considered and not adopted

- **Frontend-owned lease with ISR shipping.** Not adopted (section 1). Its good parts were kept: refuse to guess after record loss, a clock-free epoch fence idea (kept only as the latent option), window reservation in the proxy. Its defects, were it revisited: an ISR change must be requested by the leader and applied with a compare-and-set on the ISR version, with the leader counting a new member only after the reply; "has it" must mean fsynced; a promote that is never acknowledged must retry at a higher epoch; `min_isr` of 2 with two servers stalls on any failure.
- **A clock lease with self-fencing after `ttl`** (the third draft). Rejected: it puts the frontend's uptime into farm write availability and needs a skew margin; a long `ttl` to compensate makes failover slow.
- **Disk-loss recovery by "highest epoch seen plus one".** Rejected: the first arrival can be stale and win.
- **"Servers verify a lease signed by the frontend key."** Dropped: no key design existed. Not needed when Raft elects.
- **CheckQuorum as a write-safety requirement.** Corrected: Raft already prevents an isolated leader from committing; CheckQuorum buys read freshness and a faster `NOT_LEADER`.
- **A persisted membership guard against leadership hijack.** Cut from v1: the previous leader's membership report is soft state and empty after a frontend restart; persisting it adds record churn. The connect-and-verify check and certificate issuance cover v1.
- **Routing against the highest term from any registrant.** Replaced by the `is_leader`-only rule (section 4).
- **One retry of unary calls at the proxy.** Cut: it needs request buffering beside a non-buffering proxy; clients already retry.
- **Early oversize rejection at the proxy** (and a 2 h idle timeout, and subtracting a margin from `grpc-timeout`). Cut: it contradicts opaque pass-through; the leader enforces limits, PING handles dead peers.
- **UI drain/undrain, queue page, write actions.** Deferred.
- **Separate `kbf-gateway` binary, `ads.rs`, `envoy.rs`, their tests, `tests/golden`, the `envoy-types` dependency, the `envoy-config` subcommand and `docs/deploy/gateway.md`.** Discarded; `cert.rs`, `registry.rs`, `tests/support` and the mTLS flags are reused. The registry's "highest claimed term among live entries" leader choice is replaced by the rule in section 4.
