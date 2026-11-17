# kbf architecture

This document describes how kbf is put together: what each part does, how a request
moves through the farm, where state lives, and how the code is tested. It describes
the code on `main`. Parts that are designed but not built yet are marked
**planned**; nothing marked planned exists in the code today.

The design documents under [docs/design](docs/design) go deeper:

| Document | Covers |
|---|---|
| [direct-js.md](docs/design/direct-js.md) | what an Execute does now: one job of the job service, submitted at once and read back by polling; endings, resubmits and cancel; the state-store rows that let any frontend answer by name and take up a gone process's operations |
| [scheduler.md](docs/design/scheduler.md) | (**superseded** by direct-js.md) the earlier farm scheduler: operations, leases, fencing, QoS, placement, accounting |
| [simulation.md](docs/design/simulation.md) | what is simulated (the Raft log, the object store writes, the write tier), the conventions every simulation keeps, and the one-server cell |
| [storage.md](docs/design/storage.md) | the CAS and action cache, segments, metadata, retention, the object store interface |
| [write-path.md](docs/design/write-path.md) | uploads grouped into shared segments (one PUT and one metadata commit per segment), segment upload (a client's outputs as whole segments, one metadata commit) and segment-range fills, a local NVMe/SSD write tier flushed to the object store, what an acknowledgement promises at each step, crash and lost-disk recovery, rollout and tests (grouping built; the rest **planned**) |
| [write-tier-scenarios.md](docs/design/write-tier-scenarios.md) | every failure the local write tier must survive (crash points, store outages of minutes and hours, slow and failing stores, lost commits, disk full, bad sectors, torn tails, eviction and read races, node loss, leader change, clock jumps, the orphan sweep), the invariant each checks and the outcome expected, which run today against the direct path and which wait for the tier, and the simulation seams the tier plugs into (**planned**; the direct-path runs exist) |
| [metadata-snapshots.md](docs/design/metadata-snapshots.md) | the snapshot encoding of the metadata state, compaction of the single-voter log, starting from a snapshot, version refusal, size and pause at today's scale and 10x, and how the HA design inherits it (**planned**) |
| [worker-protocol.md](docs/design/worker-protocol.md) | (**superseded** by direct-js.md) `kbf.worker.v1`: messages and the rules both sides keep |
| [capabilities.md](docs/design/capabilities.md) | (**superseded in part**) node reports, ISA levels, matching an action to a node |
| [fleet-updates.md](docs/design/fleet-updates.md), [fleet-updates-security.md](docs/design/fleet-updates-security.md) | keeping node software current: rolling updates, MDM on Macs, Linux host updates, bare-metal GPU and app-install isolation, the Fleet UI; its security model: threat model, root helpers, signing keys, the MDM gate, enrollment (**planned**) |
| [updates.md](docs/design/updates.md) | updates by a coordinator and an updater on every host: the heartbeat and its reply, the desired state CI sets with an OIDC token, the per-role limits and the order the coordinator decides by, one host's A/B, the trust (enrollment, mTLS, OIDC, attestation), the failures, and the three PRs (the coordinator is built; the host agent and the deletion of the ssh path are **planned**) |
| [frontend-proxy.md](docs/design/frontend-proxy.md), [kbf-proxy-api.md](docs/design/kbf-proxy-api.md) | the front door of a frontend host: `kbf-proxy` terminating TLS and balancing each stream over a pool of frontends that share one Postgres state store, the identity headers it sends, a roll that drains, retires and restarts one frontend at a time, release compatibility, and the crate's API, limits and admin API |
| [deployment-topology.md](docs/design/deployment-topology.md) | where servers run, the Raft log on local disk, the client front that routes reads and uploads to any server and the rest to the leader, workers dialling the servers directly, the converged topology (servers, object store and workers on the same hosts), what is built and what is planned, the probes the front must pass (**planned**) |
| [mdm-backend.md](docs/design/mdm-backend.md) | (**superseded**: the gate it describes, `kbf-mdm-gate`, was never deployed and is deleted; the MDM gate is `hostkeeper-mdm-gate` in komira-ai/hostkeeper) MDM as a pluggable backend behind `kbf-mdm-gate`: the three operations the server uses, erase only by an operator's hardware-key-signed request, macOS 27 update progress, network reachability, moving the MDM, kbf's own configuration management (**planned**) |
| [invocations.md](docs/design/invocations.md) | a build's id (the client's `tool_invocation_id`), the per-invocation and per-action record, where it is kept and for how long, the read API an agent diagnoses a build with (a failures view with log tails, artifacts, metrics, a one-command `kbf build show` client, read tokens; the agent-facing part is [invocations-api.md](docs/design/invocations-api.md)), the configurable build-page URL and how a buck2 or Bazel user gets the link (**planned**) |

Decision records live in [docs/adr](docs/adr).

## What kbf is

kbf is a remote build and test farm for [Bazel](https://bazel.build) and
[Buck2](https://buck2.build). Build tools talk to it with the
[Remote Execution API v2](https://github.com/bazelbuild/remote-apis) (REAPI): they
upload inputs to a content-addressable store (CAS), look up results in an action
cache (AC), and ask the farm to execute actions that miss. kbf runs those actions on
worker machines and stores the results so the next build, on any machine, can reuse
them.

There is no client-side kbf component. Bazel and Buck2 speak REAPI to `kbf-server`
directly.

## The programs

kbf ships two binaries: `kbf` (crate `kbf-cli`), which runs a role of a farm as a mode,
and `kbf-server`, the program `kbf`'s two modes are built from, which still builds and runs
on its own.

**`kbf`** takes its mode from `--mode` (or `mode = "..."` in `kbf.toml`; the mode is a
flag, never a word after `kbf`):

- `frontend`: the REAPI front end. It takes the listener, executor, state-store and log
  flags of `kbf-server`, requires `--storage`, and holds no blob stores. With `--executor
  js-agent` it submits each action as a job of the jobscheduler's job service (crate
  `kbf-server`, module `executor`; [kbf-modes.md](docs/deploy/kbf-modes.md));
- `storage`: the storage node. It takes the store, write and log flags of `kbf-server`
  and no state-store flag: a storage node reaches the state store only through a
  frontend.

The modes `daemon`, `storage-and-daemon` and `all-in-one` were removed; asking for one exits
2 with a line that says what replaced it ([kbf-modes.md](docs/deploy/kbf-modes.md#the-removed-modes)).
A mode refuses the flags that belong to another mode. `kbf cluster ...` is not a mode:
it is a client of the frontend's operator API. Source: `crates/kbf-cli/src/lib.rs`.

**`kbf-server`** (crate `kbf-server`) is the whole server in one process: it serves REAPI to build clients and
`kbf.worker.v1` to storage nodes, on two separate listeners. It holds the cache, the
executor and the record of which storage nodes are connected. Today one process runs every
role (`--role=all`). The flags are:

- `--listen` (REAPI, default `127.0.0.1:8980`) and `--worker-listen` (storage nodes, default
  `127.0.0.1:8981`);
- `--reapi-tls-cert` and `--reapi-tls-key` to serve the REAPI listener over TLS (both,
  or neither for plain text);
- `--worker-tls-cert`, `--worker-tls-key`, `--worker-client-ca` to serve the worker
  listener over mutual TLS (all three, or none for plain text), and
  `--worker-deny-list` for the certificates and nodes it refuses. TLS and mutual TLS
  are optional: a listener without them serves plain text on any address, with a
  warning on the start line when that address is not loopback (see
  [Security model](#security-model));
- `--cold-store`, the durable object store, a URI: `s3://bucket/prefix?endpoint=...`
  (query keys `endpoint`, `region`, `conditional-put`, `min-put-rate`; the S3 key pair
  comes from the standard `AWS_ACCESS_KEY_ID` and `AWS_SECRET_ACCESS_KEY` variables,
  never from the command line) or `file:///dir`, a directory on the local disk (under
  the fixed prefix `kbf/<log id>/`), which refuses writes that would leave its file
  system with less than 5% of it (at least 1 GiB) free and evicts the least recently
  used objects to stay above that; `--hot-store`, optionally, a `file://`
  directory on a fast disk in front of it (the write tier, sized from its filesystem's
  free space, at most `--hot-store-max-bytes`); and the metadata log's directory,
  `<hot store>/meta` or `--meta-dir`. There is no `memory:` store: a `file://` path on a
  tmpfs is one (storage glossary: [storage-node.md](docs/deploy/storage-node.md#where-the-bytes-live-storage-glossary));
- `--heartbeat-interval-ms`, `--vm-host-floor-cpus`, `--vm-host-floor-mem-gib` and
  `--pack-commit-sample`, retired flags of the worker session: accepted, ignored and warned
  about at start;
- `--finished-retention-secs` (default 600), how long a finished operation is kept
  for WaitExecution before its name is `NOT_FOUND`;
- `--shutdown-timeout-secs` (default 0, no deadline), how long a stop (SIGINT or SIGTERM) waits
  for REAPI clients to disconnect after their open Execute and WaitExecution streams
  that are not done are ended `UNAVAILABLE` (a finished operation a stream is waiting
  on is still sent first);
- `--state-store`, where the executor keeps the rows of its open operations (`exec/op/`,
  `exec/live/`) and the answers of finished ones (`done/`, `doneof/`):
  `postgres:///kbf?host=/run/postgresql`
  (a database on a local Unix socket, reached as the process's own user; a URL with a
  password is refused) or `sqlite:///absolute/path`. A restarted process, or another
  frontend of the store, takes up the operations a gone process left
  ([direct-js.md](docs/design/direct-js.md) section 3). Left out, the operations are in
  memory and a restart loses them ([architecture-v2.md](docs/design/architecture-v2.md) section 4.3);
- `--invocation-memory` (bytes, default 268435456, 256 MiB), the memory the invocation
  records take together ([invocations.md](docs/design/invocations.md) section 3.5):
  each build's calls, jobs and outcomes, kept in memory only and lost on restart;
- `--api-listen`, off unless given: the operator API, HTTP/JSON under `/v1`
  ([docs/api.md](docs/api.md)). Reads are open, so bind it where only operators
  reach it; writes need the token in `--api-token-file` (owner-only file), come from
  loopback, and carry no `Origin` header. Run `kbf-server` as a user other than the
  one the job service's agents and jobs run as, or a job on the same host can read the token.

**Where an action runs.** Every Execute that the action cache does not answer runs as one job of
the jobscheduler's job service, submitted at once by the frontend's executor (`--executor
js-agent`, the only value) with no CPU or memory and the build target as its size-class label.
kbf does not schedule: how many jobs run, where and when is the job service's. The executor
reads each job back every `--executor-poll-ms` (250 ms) until it ends
([direct-js.md](docs/design/direct-js.md)). The job is the
`kbf-task` tool (crate `kbf-task`), which runs the action's Command and commits its outputs
into a put directory that the frontend then reads once, hashes and records. With no
`--executor` the frontend builds no executor: it serves the cache and the CAS and answers
Execute `FAILED_PRECONDITION`.

## Crates

kbf is one Cargo workspace. Some crates are **pure**: they hold decision logic, do
no I/O, read no clock and draw no random numbers. Time and entropy are inputs. This is
what lets a simulation seed replay a run exactly (see [Testing](#testing)).

| Crate | Pure | What it holds |
|---|---|---|
| `kbf-types` | yes | digests, images by manifest digest (`ImageRef`), platforms, farm time, the `StateMachine` trait and `Effect` enum, `HostRole` (what a host runs, for the update coordinator) |
| `kbf-meta` | yes | the metadata state machine: CAS index, action cache, closure check, retention |
| `kbf-segments` | yes | the segment format and FastCDC chunking |
| `kbf-caps` | yes | CPU capability parsing, ISA levels, request matching |
| `kbf-raft` | yes | a sans-IO Raft core ([ADR 0001](docs/adr/0001-consensus.md)) and its host loop over `Storage`, `Transport` and `Machine` traits, with an in-memory storage |
| `kbf-invocation` | yes | the invocation record ([invocations.md](docs/design/invocations.md) section 3): each build's header, counters and action rows, built only from events (calls, job submits, resubmits, finishes), its `kbf.invocation.v1` encoding, and the memory bound of section 3.5 (minted, then idle records evicted; then the largest open one closed to new rows, and whatever else does not fit left out) |
| `kbf-map` | yes | the storage map (generation, members with address, zone, weight and state, rule, replication factor, minimum protocol, deny entries) and weighted rendezvous hashing over it: write owners, read order, and read order across two generations ([architecture-v2](docs/design/architecture-v2.md) sections 6 and 8); depends on `kbf-types` and a fixed-key SipHash only, so the frontend, storage, the daemon and the transfer engine can all use it |
| `kbf-mapsync` | no | the storage map in motion: the `MapSource` seam a frontend implements (the map it holds and a generation that ticks when it changes), and the `MapHolder` a storage node keeps the newest pushed map in, never going back ([architecture-v2](docs/design/architecture-v2.md) section 6); depends on `kbf-map` and tokio's watch channel only |
| `kbf-estimator` | yes | per-key memory and CPU size summaries and estimates learned from earlier runs, with an OOM floor and raise-only state (the scheduler no longer uses it; `kbf-front`'s metrics read its key and summary types) |
| `kbf-objstore` | no | the `ObjectStore` trait and the S3 store, re-exported from `objstore-s3` (komira-ai/tierstore), an in-memory store, a local-directory store (`FsStore`, `kbf-server --cold-store=file://...`), the conformance suite, the segment-footer reader |
| `objstore-s3` | no | not a member of this workspace: the `ObjectStore` trait, its key, range and page types and the S3 store, taken from [komira-ai/tierstore](https://github.com/komira-ai/tierstore) at the revision pinned in the root `Cargo.toml`; `kbf-objstore` re-exports it |
| `update-hook` | no | the contract between a host updater and the update hook a service ships (the five steps and their arguments, exit codes 0/1/75/76, the JSON last line, the `--params` table, the next-step table as a pure function) and the conformance harness a service uses as a dev-dependency to prove its hook keeps it; the hook is any executable, `update-hook` is our convention; depends on no other workspace crate and is named without `kbf` (`kbf-integration-test` keeps both true) ([README](crates/update-hook/README.md)) |
| `kbf-package-hook` | no | `kbf package-hook <step>`: the update hook of the native `kbf` package, which hostkeeper runs on a farm node it updates through apt ([deploy/farm-hostkeeper.md](docs/deploy/farm-hostkeeper.md)): `prepare` checks the staged `kbf --check` against the node's configuration and stops the unit (kbf drains on SIGTERM, with no bound), `cutover` starts it, `verify` waits for it to be active at the version set, `rollback` restarts it on the previous package, `cleanup` removes the record; each step works from a record under a lock in its state directory. It keeps the contract of `update-hook` (its conformance harness runs against a fake host) and holds the one rule of the native version (`<major>.<minor>.<seq>`) and the constants of the manifest `release desired` sends |
| `update-register` | no | the client of the host updater's registration socket: a service sends its name, version and an optional loopback health address and holds the connection open for as long as it runs (dropping it is `stopped`); each refusal the updater sends is a typed error; depends on no other workspace crate and is named without `kbf` (`kbf-integration-test` keeps both true) ([README](crates/update-register/README.md)) |
| `kbf-step-runner` | no | the step runner of the generic host updater: one update from `prepare` to its verdict through a `StepLauncher`, the persisted step record and its resume, the dead-man thread, the staged-artifact digest checked before every step, the registration socket (`SO_PEERCRED`, liveness and version) and the updater's own update with its fallback; names no service and reads no declaration ([README](crates/kbf-step-runner/README.md)) |
| `kbf-tier` | no | the local write tier's disk layer ([write-path.md](docs/design/write-path.md) section 6): block files in the hot store directory (`kbf-server --hot-store`) written in reserved ranges and synced (file and name) before a range may be committed, seals at the segment limit or after the seal-after interval, the hot store's budget, and the restart: the `TIER` check, staged blocks cut to their committed length, and the orphan sweep, against the committed state through `StagedIndex`; once flushed, blocks become `.seg` copies evicted least recently read first, only in what the budget leaves over. `kbf-front` (`Cache::attach_tier`, `Cache::flush_tier`) stages writes on it, reads staged blobs from it, and runs the flusher; `kbf-server` attaches it with `--hot-store` over either metadata log (nothing raises the log: the tier's commands commit to the Raft metadata log as it is) |
| `kbf-state` | no | a re-export of the `js-state` crate of the jobscheduler repository, at the revision the workspace pins (`pub use js_state::*`; the `postgres`, `test-support` and `test-database` features forward to the same ones of `js-state`), so the code and its tests (the state store trait the executor writes its operation rows through ([architecture-v2](docs/design/architecture-v2.md) section 4.1), the in-memory fake, the SQLite and Postgres backends, the conformance suite C1 to C8, `share`, and `postgres::schema::extend` and `ExtensionPool`, with which a crate that keeps tables of its own in the schema (`kbf-pack-index`) ships its migration) live and run in that repository's CI; the `state-postgres` CI job and the pull-request gate run kbf's consumers over the Postgres backend |
| `kbf-job-client` | no | a typed gRPC client of the job service (`jobscheduler.v1.JobService` of `js-proto`, at the revision the workspace pins), which the executor of `kbf-server` calls: submit (idempotent on the caller's job id), get, list (one page, or every page up to 10000 jobs), cancel, result and logs, taking and returning `js-proto`'s own messages; the job id checked before anything is sent; the bearer token from a 0600 file sent in the metadata of every call to the configured origin only, TLS pinned to one CA, plain http for loopback only; errors typed by what the caller does next (`Unreachable` nothing sent, `Transport` and `Decode` the server may have acted, `Refused` a refused submit with its refusal code, `Status` any other definite answer), each of the 17 gRPC codes classed in one table; with the `fake` feature, `FakeJobServer`, an in-process job service with the service's rules (idempotent submit and `job-id-conflict`, admission and its refusals, states moved by the test, cancel, results kept for the retention window on its own clock, host silence, file contents never returned) and faults of the connection; golden bytes in `tests/golden` copied from `js-proto`, held to the released service by `tests/js_real_service.rs` of kbf-integration-test (field set by field set, against the release `kbf-js-version.toml` pins); reaches no scheduler crate but `js-proto`, and no farm crate depends on it yet (the deploy tool `kbf-deploy` does, for `job-service check` and the canary job of `job-service apply`, and kbf-integration-test as a dev-dependency, for those tests) |
| `kbf-trees-client` | no | a typed gRPC client of a build node's tierstore trees interface (`tierstore.trees.v1.TreesService` of `tierstore-trees-api`, at the one tierstore revision the workspace pins), which no farm code calls yet (the executor's input-tree stage of [trees-for-action-inputs.md](docs/design/trees-for-action-inputs.md) will): `CreateTree`, `GetPlacement` and `DeleteTree`, taking and returning the generated messages; the node's tree digest and the placement id checked and kept as `TreeDigest` and `PlacementId`, no digest of kbf's own computed; one node per client (`host:port`, as a host's address label carries it); each call signed with SigV4 over the gRPC path and the SHA-256 of its whole body, as tierstore checks it, the secret never sent; TLS pinned to one CA, plain for loopback and, off loopback, only with an explicit opt-in; errors typed by what the caller does next (`Unreachable` nothing sent, `Transport` and `Decode` the node may have acted, `Refused` a refusal of the node by its tierstore name with what it is about, `Status` any other definite answer); with the `fake` feature, `FakeTreesServer`, an in-process trees interface that checks each signature and keeps the node's rules (idempotent create by tree and locations, location taken, unknown child tree, marked sources missing, short or lying, a pin limit under `wait`, a salted digest of its own, unknown placement, directory listings) and faults of the connection; reaches no kbf crate and no tierstore crate but `tierstore-trees-api` (`trees_client_layer` of `tests/layering.rs`, which also holds every tierstore crate to one revision) |
| `kbf-usage` | no | the profile names, shape index and sink the usage recorder was built on; no part of `kbf-server` reads it now |
| `kbf-history` | no | build history in the object store ([ui-data.md](docs/design/ui-data.md) section 5), as a library not yet wired into the server: the one container format (a versioned header, zstd blocks of length-prefixed protobuf records, a footer with the block index and the family summary, a trailer with the footer's CRC-32C), object keys led by retention class (`r7`, `r30`, `r45`, `r400`) and the `ui/` index row keys; the run, hit and outcome records, tails, command renderings and failed-run sections, each with its `instance`; time-ordered run ids minted from memory (stream blocks from the `ui/streams` counter, a fallback range that never waits); the writer core (chunks cut by size, age, day, stream and close; object written first and index rows second through `kbf-state`; bounded queues and a bounded set of open invocations that drop and count; rows dropped past the bound restored from the object's footer with no LIST); the reader (a run id to its record, an invocation to its header and newest chunk summary, its records a bounded page of chunks at a time, all-must-allow across instances); the index seam (`HistoryIndex`: rows under an `IndexRoot`, every blocking state-store call through an `Offload` the server supplies); and `Scoped`, an object store that refuses every key outside `history/` |
| `kbf-front` | no | the REAPI services over a `Cache` and a `Dispatch`, and the invocation `Recorder` the services and the farm send events to (a channel bounded in events and bytes that drops, never waits) |
| `kbf-gate` | no | the fail-closed gate library every CI/CD stage shares ([kbf-cicd](docs/design/kbf-cicd.md) sections 5.2, 7 and 10): the result document (one shape, written RUNNING first and FINISHED on every exit a drop guard or `fail_and_exit` can see), a Prometheus text reader whose absent series is red, the gate verdicts, and the one definition of a release set's hash; its only I/O is the document's atomic write, and the layering test keeps it off the async stack and off other kbf crates |
| `kbf-storage` | no | the storage boundary ([architecture-v2.md](docs/design/architecture-v2.md) sections 3 and 13): the `Storage` trait (FindMissing, whole-blob read and write, a streamed write, the `Segments` service, the action cache) and its `StorageError`. `kbf-front`'s `CacheStorage` puts the cache behind it and `kbf-server`'s `LocalStore` calls through it; the cache's code is to move here. Depends on no frontend crate: not `kbf-auth` (`crates/kbf-integration-test/tests/layering.rs`) |
| `kbf-peer` | no | the role a peer's certificate names ([architecture-v2.md](docs/design/architecture-v2.md) section 9): the one reader of its `urn:kbf:role:<name>` URI subjectAltName (`role_of`), which the storage service's per-endpoint table and the worker listener's per-method table (`kbf-server`'s `worker_roles`) ask; no gRPC, so a tool that checks a certificate can use it too; `uri_values_of`, the URI values of one prefix (the update coordinator reads an updater's host role with it) A fourth role, `updater`, is a host's updater (`urn:kbf:role:updater`, with the host role in `urn:kbf:host-role:<name>` read by `uri_values_of`); the worker listener's role table serves it four HTTP calls about its own node (`kbf-server`'s `node_self`) and no gRPC method. |
| `kbf-enroll` | no | the node's side of enrollment (`docs/design/install-and-enrollment.md`, sections 4 and 5, on its own branch until it merges): `kbf.toml` with every field validated, the enroll token file, first contact checked against the CA fingerprint in the token (`pin`), `Enroll` and `Renew` over `kbf.enroll.v1` with one key per role made on the node, the answer's checks, the key and certificate files written atomically (`store`), the schedule of renewal, and the cold-store keys a storage node holds in memory only; a library, called by the start of a node mode in `kbf-cli` (`kbf --mode=storage`) `Config::updater` and `Store::unopened` serve a host's updater, whose role `updater` has its own `updater.key` and `updater.pem` (`kbf-updater enroll`, `docs/deploy/kbf-updater.md`). |
| `kbf-server` | no | the server: wires front, executor and storage together (`start` returns a `ServerHandle`; the `kbf-server` binary is a thin main over it, so `kbf` can run the server in its own process; the stores its flags name, `--cold-store`, `--hot-store` and `--meta-dir`, are parsed and resolved in `stores`); the executor (`executor`, built by `--executor js-agent`), which runs each action that misses the cache as one job of the jobscheduler's job service, polls it to its end and answers the operation ([direct-js.md](docs/design/direct-js.md)) |
| `kbf-proto` | no | generated code for REAPI, `kbf.worker.v1`, `kbf.storage.v1`, `kbf.log.v1`, `kbf.map.v1` (the storage map's messages, with their conversions to and from `kbf-map` types) and `kbf.update.v1` (self-update: offer, order, status, release fetch) |
| `kbf-log` | no | the replicated log's entry encoding: `kbf-meta` commands and the control commands (batched farm inputs, a result with its action-cache entry, takeover, checkpoint, machine version) to and from `kbf.log.v1`, under the log's format version |
| `kbf-transfer` | no | the shared blob transfer engine: the `Cas` trait, and `Transfers`: one HTTP/2 connection to the worker listener with explicit windows, one in-flight limit and byte budget for every caller, one fetch per digest however many callers ask, FindMissing before every upload, resumed reads after a broken connection, and each caller's `TransferStats` |
| `kbf-image` | no | the node's image source: a digest-verified registry client (hyper over rustls) and the layer unpacker (whiteouts, ownership, in-root path resolution) |
| `kbf-cas-conformance` | no | the checks every implementation of the data plane (`kbf_transfer::Cas`) must pass, generic over the implementation (`tests!`); run against the gRPC implementation `kbf_transfer::Transfers` |
| `kbf-threads` | no | thread and disk sizing for the server (disk: a filesystem's size and free space, the floor it keeps free, and the shares a cache, a tree store or a hot store may take: `disk`); thread sizing: the server has no reserve and uses every CPU it may (one async worker and one hashing thread per logical CPU, tokio's blocking cap); `--async-threads`, `--blocking-threads` and `--cpu-threads` override it; and the tokio runtime built with them |
| `kbf-tls` | no | TLS for a server listener whose certificate, key and client CA are files that change while it runs: `TlsSource` re-reads them on an interval (`--tls-reload-interval-secs`) and swaps in a new `rustls` configuration only if the new files make a working one (readable, PEM, key matches certificate, client CA has a usable certificate, leaf not expired), otherwise it keeps the current one and counts the refusal (`kbf_tls_reloads_total`); `Incoming` accepts the connections a tonic server is given, handshaking each under the configuration current at its accept, so a renewal never ends a connection in flight; `peer_certs` and `remote_addr` read a request's peer. `kbf-server` uses it for the REAPI and worker listeners and the storage node's listener (`kbf-storage` reads a caller's certificate through it, `service/admission.rs`) |
| `mdm-core` | yes | **frozen**, successor `hostkeeper-mdm-core` in komira-ai/hostkeeper: the MDM host layer's renderer: settings, the systemd units and Caddyfile of NanoHUB, the SCEP server, NanoDEP and the proxy, the enrollment and ADE profiles, the pinned upstream tools; bytes from settings text, no I/O; links no other workspace crate ([mdm-host.md](docs/mdm-host.md)) |
| `host-step` | no | **frozen**, successor `hostkeeper-mdm-step` in komira-ai/hostkeeper: the planner of host steps (directory, file, user, unit action, command) with a dry run by default, over a `Host` that is the machine (`RealHost`, which follows no link on the way) or a fake (`fake::FakeHost`); links no other workspace crate |
| `mdm-host` | no | **frozen**, successor `hostkeeper-mdm` in komira-ai/hostkeeper: the binary `mdm-host`: `fetch`, `install`, `tls` and `verify` of the MDM on one Linux host, built on `mdm-core` and `host-step`; a binary of its own beside the update coordinator, never part of `kbf`, and links no other workspace crate (`kbf-integration-test`'s `mdm_layering` keeps the three apart); kbf-deploy's `mdm` kit is the older installer it was ported from ([mdm-host.md](docs/mdm-host.md)) |
| `kbf-host` | no | the one way kbf changes a host, shared by `kbf-deploy` and `kbf-installer`: a plan of steps (write a file, make a directory, remove, run a tool by absolute path with an empty environment, run one and write its filtered output to a file, wait for an HTTP URL), made from queries that change nothing (a query or tool may run as another user, with that user's groups) and printed as `DRY-RUN:` lines by default and carried out only in apply mode; a file or directory step is planned only when the host differs, so a re-run after an apply plans nothing; a root plan applies only as root, from a binary root alone can replace, and a user plan never as root. With the `testkit` feature, a fake host that records each step and query, for other crates' tests |
| `kbf-installer` | no | the root tool of a host: the host side of a kbf release, shared by `kbf-deploy host ...` (the manual path) and the root helper that applies pulled updates, so a host has one state file and one layout ([host-releases.md](docs/deploy/host-releases.md)): `layout` (in `kbf-setup`, re-exported here; releases under `/var/lib/kbf-installer/releases/<commit>/`, the `current` and `previous` links beside them, the platform's state directory, lock and trust directory, in one place), `state` (`state.json`, schema 2, under an exclusive lock, replaced whole by a rename, with `state.json.bak` as the last good copy; the commit-confirm record and the order floor), `release` (`install`, `stage`, `rollback` and `gc`, through `kbf-host`'s plan of steps; a release holds `kbf`), and the offline checks of a release a fetch brought in (the fetch itself is a later change; there is no inbox of release bytes now): `attest` (the pinned `gh attestation verify`, run as an unprivileged user with a cleared environment and a dead proxy), `archive`, `binary` (the executable's kind, `--version --json`), `spool` (a directory the unprivileged `kbf` writes, opened without following links and each file judged by its descriptor), `trust` (the root-owned trust list), `settings` and `refusal` (one enum, a stable code per case). Its binary, `kbf-installer`, carries the root helper of the pull-based updates ([pull-updates.md](docs/design/pull-updates.md) section 7; the `helper` module): `run` reads the order records the node's `kbf-update` client writes into the order spool (at most 64, each opened without following a link, read once, expired 1800 s after root first saw it; no socket), acts on activate/confirm/revert orders (an activation goes through a `Fetcher` trait that pulls and verifies the release, and refuses while a trial is pending or a lease is live), restarts the one service named by its own flag, and writes `status.json` (schema 2); `boot-check` runs before each start of the service and reverts a release that keeps failing; `setup` (the `setup` module, in `kbf-setup` and re-exported here; [kbf-installer-setup.md](docs/deploy/kbf-installer-setup.md)) makes a host ready for its mode: the fetch account, `orders/`, the enrollment and first operator tokens, `installer.toml`, the state database's schema on a frontend host, the first release through the `Fetcher`, `kbf.toml`, then the units (a frontend's is enabled, never started); it refuses while the retired helper's layout is on the host |
| `kbf-setup` | no | the library behind `kbf-installer setup`, which `kbf-installer` re-exports under the old paths (`kbf_installer::setup`, `layout`, `installer_toml`, `database`, `exit`, `sha256_hex`, `CommitId`): `setup` (the verb), `layout` (the host's paths per platform), `installer_toml` (the settings file `setup` writes and the proxy verb reads), `database` (the state database's schema step), `exit` (the exit codes), `sha256_hex`, and `CommitId`; it names `kbf-proxy` only in `installer_toml` and `setup/proxy.rs`, and does not depend on `kbf-installer` |
| `kbf-deploy` | no | the deploy tool; a kit that changes a host does it through `kbf-host`. It reads closed-key `KEY=VALUE` config files; its unit reader (`unit`, re-exported from `kbf-unitfile`) is the one the docs check uses, `template` is strict `@KEY@` substitution, and `https` is a small HTTPS client (rustls with ring, the host's trusted roots). Four kits. `minio`: `buckets` makes buckets, a policy for them and a service account key file through `mc`, with the admin key in `mc`'s environment, never on an argv, and `nodeport` adds a NodePort Service for a MinIO's S3 port through `kubectl`. `linux`: `swap` makes the swapfile ([storage-node.md](docs/deploy/storage-node.md#the-swapfile)), `meta-backup` and `meta-verify` back up and check a kbf-server's metadata log, and `verify-cert` checks a certificate against its row of the farm's table; the `kbf` modes' units are the packaged files `kbf-installer` installs ([kbf-units.md](docs/deploy/kbf-units.md)), not this kit's. `mdm`: `fetch` downloads the pinned upstream archives of an Apple MDM (NanoHUB, the SCEP server, NanoDEP, Caddy, lego, mdmctl) as the operator's user and checks each sha256; as root, `install` checks the host and the archives, unpacks the binaries, makes the secrets and the SCEP CA, renders and installs the units and the Caddyfile from compiled-in templates and starts the services; `tls` installs lego's certificate job, its renewal timer and the proxy's reload on renewal, gets the first certificate over DNS-01 and starts the proxy; `push-cert` checks an APNs push certificate and its key, gives them to NanoHUB on loopback and writes the enrollment profile (a property list); `dep` links NanoDEP to Apple Business Manager and defines an ADE profile, refused unless it is supervised, mandatory and not removable; the API calls are printed as steps are and made only with `--apply`; `verify` checks, read only, the services, the listeners, the routes through the HTTPS proxy, the TLS certificate, DNS and the modes of the state and secret files; the `mdm` kit is frozen, its successor `hostkeeper-mdm` in komira-ai/hostkeeper ([mdm-host.md](docs/mdm-host.md)); `pilot`: as the operator's own user, never root, `server-install` checks the host (the bind address on the configured interface, free ports, a server certificate that chains to the CA for the configured name with its own 0600 key, a 0600 S3 key file outside every git work tree, a kbf-server binary for this CPU whose `--help` lists every flag its unit passes) and runs kbf-server as a capped systemd user service, waiting for its start line after a restart, and `buckconfig` writes the buck2 overlay that points one linked worktree at it, never replacing or removing a `.buckconfig.local` it did not write |
| `kbf-sim` | no | the deterministic simulation kernel |
| `kbf-integration-test` | no | integration tests, repository lints, the end-to-end harness |
| `kbf-coverage` | no | the coverage gate CI runs |
| `kbf-baseline` | no | client-side farm numbers from buck2's own event logs (`--event-log FILE.json-lines.gz`, or `buck2 log show` output): actions by where they ran (remote, remote cache hit, local, local cache, buck2 itself), the build's wall time, nearest-rank p50/p90/p99 and maximum of each action's time and of the farm's reported queue and execution times, the cache hit share and the failed actions, per build and over a set, as a table and as JSON. It reads only the client's log, so the same build against two farms compares line by line |
| `kbf-bench` | no | times blob uploads to a server's CAS, and reads back, as a client stores an action's outputs: a tree generated to a shape from a seed (or read from a directory), sent over a CAS client and TLS loader by ByteStream or batch calls, a set number in flight, with FindMissingBlobs optional; reports wall time, blobs/s, MB/s and nearest-rank p50/p90/p99 and maximum per kind of request, as a table and as JSON ([README](crates/kbf-bench/README.md)) |
| `kbf-state-copy` | no | the cutover tool that moves a frontend's SQLite state file into an empty Postgres store, a binary of its own and never part of `kbf` (`kbf-state-copy <state.db> <postgres-url>` is a dry run, `--apply` copies; [state-store-cutover.md](docs/deploy/state-store-cutover.md)): it reads the file through rusqlite alone, read-only and immutable, and refuses a frontend that holds the lock beside it, a write-ahead log that holds writes the file does not, a store that is not empty or whose schema is not the version it was built for, and clocks more than 2 seconds apart; it then copies every row and counter in one transaction with its version kept and its expiry converted from nanoseconds to microseconds, sets the head revision and revision sequence past the highest version, compares row counts and a SHA-256 digest per key family inside the transaction before it commits (a mismatch rolls back) and again on a new connection after; its `cutover` command (an operator-only step) runs that copy and then points the frontend's one state-store setting, the `[scheduler] state-store` key of `kbf.toml` or a `--state-store=` flag of a unit file, at Postgres, keeping the original file byte for byte as `<file>.pre-postgres` and renaming the new text over it, and its `rollback` command renames the backup back (both are dry runs without `--apply`, and neither changes the SQLite file); exit status 0 done, 2 refused, 1 failed |
| `kbf-alert` | no | alerts with their exact fix; the alert book (raise and resolve with hysteresis) and the outbox, both pure modules; the webhook notifier, which keeps the outbox in a file. Nothing raises alerts yet |
| `kbf-store` | no | a Raft server's durable state on the local disk, as a `kbf_raft::Storage`: a segmented append-only log of CRC-32C records, the hard state (term, vote) and the newest snapshot, whose segments it covers are removed only once it is durable; a torn tail cut at open, any other damage refused; the first write or fsync error stops the store; all I/O through an `Fs` trait, with a fault-injecting in-memory `FaultFs` for crash tests |
| `kbf-sim-cell` | no | the whole-cell simulation: one server assembled from `RaftMetaLog` on a fault-injecting disk, with fake clients (`FindMissingBlobs`, upload, check again) and an object store, checked against properties 1 and 5 to 7 of the high-availability design and the write path's properties 8 and 9 after every step, with object-store faults (hang, slow, failures, lost answers), crashes at named points of the write path, the local write tier (`kbf-tier` on a second fault-injecting disk, staged writes, the flusher, reads, crashes at named tier points, a full disk, a bad sector, eviction) checked against the tier's invariants (properties 10 to 13), and a seed sweep that reports every failing seed ([simulation.md](docs/design/simulation.md#3-the-cell-kbf-sim-cell), [write-tier-scenarios.md](docs/design/write-tier-scenarios.md)) |
| `kbf-ui` | no | the server-rendered pages ([ui-data.md](docs/design/ui-data.md) 6.1 and 8): pages made only from the read API's JSON documents through a `Source` the server gives them, each declaring the routes it reads (its reader refuses any other) and listing the URLs it read; HTML escaped by construction (`html::Html`, no public raw write); every answer with the `Content-Security-Policy` of section 8 and `nosniff`; one embedded token-based stylesheet, no script. Builds (an id lookup), Build and Action; not mounted on the server yet |
| `kbf-unitfile` | yes | the systemd unit-file reader the deploy docs check, `kbf-deploy`, `kbf-harness` and the packaged-unit tests share: `UnitFile::parse` (sections, `Key=value`, `\` continuations joined with a space, comments skipped, systemd's `WHITESPACE` trimmed), last-assignment-wins reads (`get`, `get_in`, `exec_start`, `flag`), `extend` for a drop-in, and `comments_in_continuations`; no kbf dependency |
| `tierstore` | no | not a member of this workspace: the generic hot/cold object store, its S3 front and the `tierstore` binary, taken from [komira-ai/tierstore](https://github.com/komira-ai/tierstore) at the revision pinned in the root `Cargo.toml` (`kbf-front` and `kbf-task` use it; the `tierstore` release asset is built and released there) |
| `tierstore-trees-api` | no | not a member of this workspace: the wire contract of tierstore's trees interface (`trees.proto`, package `tierstore.trees.v1`, and the messages and stubs generated from it), taken from [komira-ai/tierstore](https://github.com/komira-ai/tierstore) at the revision pinned in the root `Cargo.toml`; `kbf-trees-client` uses its client half, and its fake the server half |
| `tierstore-bench` | no | a measurement tool for tierstore: an S3 client of the instance under test and of the stores behind it, with cells (put-to-ack, cold read, copy lag, tree read), a fault proxy, a versioned result JSON and a CSV of raw samples carrying role names only, and a harness-side request log; every verb built ([README](crates/tierstore-bench/README.md)) |
| `kbf-pack` | no | the packing client, a library wired into the frontend (`--pack-store-endpoint` and its companions, kbf#1706; with them and `--pack-index` the frontend also takes `CommitPack`, kbf#1747), all off unless `--pack-store-endpoint` is given: `Packer` owns the write rule (blobs the pack index lacks are cut into packs of at most a byte and a blob limit, each pack written to the object store with a create-only write under `cas/d/<node>/<boot>/<seq>` or `cas/f/<frontend>/<boot>/<seq>`, retried under a new key when a write fails, and recorded in the `kbf-pack-index` pack index only after the store has acknowledged it, so a row never points at a pack that is not stored; a blob too big for a pack is a pack of its own; a record that fails is tried again for the same pack, and when every attempt fails the pack is unreferenced and garbage collection finds it), `check_create_only` (the startup test that a second write of a key is refused), `Reader` (locates blobs through the index, reads each pack with ranged requests, one for blobs within a gap of each other, hashes every blob before returning it, reports a blob that is missing, lost or damaged as the blob's `Fetch`, and tells the index which blobs it served in batches) and `ReadCache` (a bounded on-disk cache of blobs by digest, least recently used out first, written by temporary file and rename into a directory and files only the owner can use, checked by digest on every read, rebuilt from the directory on open), and `Adopter` (the segments storage members flushed before there were packs have the layout of a pack, so it lists the members' cold store, reads each segment's footer and never its records, and records one pack per segment under the segment's own key, in place and with no byte copied: a dry run reports the packs, rows and bytes an apply would add, at most, and reads a sample of blobs back to check footers against bytes, an apply is idempotent on the key, a key held with other contents is a reported conflict, and the report, which is also the checkpoint, is saved after the records of the objects it covers are in the index, so a stopped run resumes after its last key) |
| `kbf-pack-index` | no | the pack index, which records where each blob lies inside a pack: the `PackIndex` trait the frontend and the packing client use, the in-memory index and the conformance suite P1 to P10 both backends pass, and, with the `postgres` feature, `PostgresPackIndex` in two tables of the state schema outside the key/value and feed tables and the commit head, with migration 2 (`schema`), which with the state store's migration 1 is the set a kbf release embeds (`schema::embedded`); it depends on `kbf-state` and on no other crate of the workspace |
| `kbf-pack-adopt` | no | the operator tool that runs `Adopter`, a binary of its own and never part of `kbf` ([pack-adopt.md](docs/deploy/pack-adopt.md)): it does nothing unless run, a run is a dry run unless `--mode apply` is given, and the only thing it writes to the object store or the database is the pack index (Postgres, a URL with no password) with `--mode apply`, besides the checkpoint and report files it is told to write; the source is an S3-compatible store (`--source-store-endpoint`, `--source-store-bucket`, `--source-prefix`, `--source-store-region`, a credentials file only its owner can read and never a flag), a `file://` source is refused, and `--mode apply` is refused unless `--pack-store-bucket` names the source bucket; `--checkpoint-file` (written beside and renamed, resumed from, ignored once finished), `--report-json`, `--segments-per-second`, `--footer-concurrency`, `--limit-segments`, `--verify-sample` (20 in a dry run) and `--fail-on-conflict`; exit status 0 done, 2 refused, 1 failed |
| `kbf-coordinator` | no | the update coordinator ([updates.md](docs/design/updates.md)), a library and a binary: tells each host's updater which version to run, within per-role limits. Updaters dial `POST /v1/heartbeat` over mutual TLS (the farm CA's certificates; the caller is the certificate's `updater` role, its host role in `urn:kbf:host-role:<name>` and its one DNS name as the node id, never the body) and get `nothing` or `update` with a version (`kbf_updater::wire`). The decision (`decide`) is a pure function of the desired state, the heartbeats seen and the time: the host is behind and not updating; its role has room (at most N updating, a host that failed keeps its slot until it heartbeats healthy, and is not told the version it failed again); never the last frontend that is up, and the storage and frontend minimums met by live heartbeats (a host silent for 45 s is down); hosts that are not frontends before frontends; no order within 30 s of starting; a rollback is a desired version older than the hosts'. The desired state (version, per-role limits, minimums) is the one thing stored, one JSON file replaced atomically; CI sets it with `POST /v1/desired` and a GitHub Actions OIDC token checked by `kbf-oidc` (the operator token is refused), and `GET /v1/status` is read with the operator token. The TLS files are re-read on an interval (`kbf-tls`). The image definition is `packaging/container/kbf-coordinator/Dockerfile`; the hosts' side is `kbf-updater agent` and `kbf-host-install --serve` |
| `kbf-updater` | no | the core of the optional package that rolls a new kbf version over a farm's hosts, one at a time, frontend first (a library and a stub binary; no listener of its own; the one adapter is `SshInstaller`, the install port over ssh to `kbf-host-install`'s forced command: an argument vector with no shell and the pinned host key in a private 0600 temp file, a version and channel checked against the host's grammar before any process starts, one capped JSON line read against a closed set of shapes that must agree with the exit status, a call bounded by a timeout, and ssh's own failure (exit 255) a distinct unreachable kind of `PortError`; a contract test feeds the host's real answer types to the reader): the API model (`deny_unknown_fields`, a closed error-code set, no source or URL field), the one version rule shared with the host-side wrapper, the root-owned host inventory, the roll state machine over ports (safe-to-stop, install and query, health, clock and a frontend member's own API) with a scripted fake of each (the inventory still reads the roles `daemon` and `mac-daemon` of earlier releases and lists no host for them), and `roll.json`, written by temp file, fsync, rename and directory fsync before every phase change; a restart asks a host in `installing` what it has before installing again, a roll it cannot learn the outcome of is `interrupted` and blocks new rolls until aborted, and an unreadable or self-contradicting record refuses to start the updater; a frontend that is a member of a pool of frontends (an inventory line with `instance=` and `base=`) is rolled first and alone, behind whatever load balancer there is: it drains itself (`POST /v1/frontend/drain`), exits, and is installed on (once per host) and restarted once every other member has answered `/readyz` 200 for `--peer-ready-secs`, and every failure leaves it not serving; a frontend that lists no `instance=` and `base=` is refused before the installer is called (`docs/deploy/kbf-updater.md`). Its agent mode (`kbf-updater agent`, [updates.md](docs/design/updates.md)) is an unprivileged updater on a host that heartbeats to the coordinator over mutual TLS (platform, running version, health, last update, and the target while an A/B runs, on a thread of its own so it goes on through the A/B; the node id and role are read from its own certificate with the reader the coordinator uses, `identity`, and are in no field) and runs the host's A/B when told "update to X" (X is the only thing taken from the reply): `kbf-host-install --serve` fetches and verifies the release on the host, the helper puts X on trial, the node's health is asked, and X is confirmed or the helper goes back; a restart in the middle finds the helper's pending marker and does not install twice; a storage host's own storage health is a call to the frontend's worker listener over mutual TLS with the host's own `updater` certificate and no token (`kbf-server`'s `node_self` serves the node calls for the certificate's node id and nothing else); the token flags (`--operator-api`, `--token-file`) are an opt-in fallback. `kbf-updater enroll` gets the certificate with a one-time token (`kbf-enroll`, the host role fixed by the token, the key made on the host) and the agent renews it at half its life through `kbf_enroll::renew` (`docs/deploy/kbf-updater.md`, "Enrolling a host") |

A test in `kbf-integration-test` reads the dependency graph and fails if a pure crate depends,
directly or not, on an async runtime, a network crate or a random source, or if its
`lib.rs` stops denying the items listed in [clippy.toml](clippy.toml) (clocks, sleep,
sockets, hashed collections).

## Request paths

### Cache traffic

`Capabilities`, `ContentAddressableStorage`, `ByteStream` and `ActionCache` are served
from one `Cache` (crate `kbf-front`). The cache keeps its index in a metadata state
machine and its bytes in an object store, packed into segments. The promises it keeps:

- **FindMissingBlobs never omits a digest.** A digest that does not parse fails the
  whole call. A blob whose stored copy cannot be read is reported missing, so the
  client uploads it again.
- **Present means durable.** An upload is acknowledged, and a blob reported present,
  only once its bytes are in the object store and its location is committed to the
  index.
- **Every byte is verified,** against its digest, on upload and on read.
- **A hit never points at missing files.** `GetActionResult` answers through the
  closure check: the result and every blob it names must be present and readable,
  or the lookup is a miss.
- **Clients never write the action cache.** `UpdateActionResult` is always
  `PERMISSION_DENIED`; results reach the cache only from the frontend, after a job that ran the action ended.

See [storage.md](docs/design/storage.md) and, for the planned write path,
[write-path.md](docs/design/write-path.md).

### Executing an action

```
client                      kbf frontend                             job service (js-agent)
  | Execute                    |                                         |
  |--------------------------->| 1. AC lookup (closure check): hit -> done
  |                            | 2. Action, Command, input tree in CAS?  |
  |                            |    no -> FAILED_PRECONDITION (MISSING)  |
  |                            | 3. submit: join an open operation of    |
  |                            |    the same action, or open a new one   |
  |<-- QUEUED -----------------| 4. write exec/op/<id>, submit the job --->| kbf-task runs the action,
  |                            |    (no CPU, no memory, size class)      | writes outputs to its put dir
  |                            |<-- GetJob every --executor-poll-ms -----|
  |                            |<-- job ended ---------------------------|
  |                            | 5. read the pack, hash, outputs stored? |
  |                            | 6. accept, write the AC entry           |
  |<-- done (ExecuteResponse) -|                                         |
```

1. **Action cache.** Unless the client set `skip_cache_lookup`, a hit that passes the
   closure check answers at once with `cached_result` set. Nothing runs.
2. **Inputs.** The `Action`, its `Command` and every blob of its input tree must be in
   the CAS; each one that is not becomes a `MISSING` violation in a single
   `FAILED_PRECONDITION`, as REAPI asks.
3. **Submit.** The front hands the request to the executor through the `Dispatch`
   trait. If an identical action (same instance name and action digest) is already
   open on this frontend, the caller is attached to its operation (in-flight dedup).
4. **Job.** The executor submits the job at once, under an id made of the frontend, the
   operation and the attempt, so a submit whose answer was lost makes no second job. It
   keeps no queue: the job service decides when and where the job runs. A job that ends
   as the farm's fault (host lost, job unknown, refused at its start, killed at its memory
   limit) is submitted again as the next attempt, at most 2 times; an action's own timeout
   or an action that cannot run is answered at once.
5. **Result.** The frontend accepts a clean result only if every output it names is
   already stored: it reads the pack the job committed once, hashes it and stores the
   blobs before it records the result.
6. **Answer.** An accepted successful result (exit code 0, not `do_not_cache`) is
   written to the action cache *before* the callers are answered.

`CancelOperation` cancels the job and answers the callers `CANCELLED`. A stream that ends does
not cancel it, and nothing is cancelled when a frontend stops.

`WaitExecution` streams the same stages for an operation name that Execute returned, and
`GetOperation` reads the stage the name's operation is at, once. A finished operation is
kept for `--finished-retention-secs` (600 by default), in which WaitExecution streams its
done operation, so a client whose Execute stream broke still gets a result the action
cache does not keep; then it is forgotten, and the next Execute for it is answered from
the action cache.

Over a state store, an operation name is `operations/<id>`:
the operation's id, which the store numbers (each server reserves blocks of ids, so an id
names one operation on every server of the store and across restarts). Any server answers
the name: from its memory if it runs the operation, otherwise from the store's `done/` row (an operation another frontend runs is followed until that row appears),
and a result stays in the store for at least ten minutes (`DONE_FLOOR`, or the finished
retention if that is longer), so a client that reconnects to another server after the finish still
gets it. After that, or for a name nobody gave, the answer is `NOT_FOUND`. A server with no
state store (tests only) names an operation `operations/{term}-{n}`: its process's term
and a count that starts at 0 in each process, and answers no
other name, so a client still holding a name from an earlier process is never attached to
another action's operation. A name of that form is answered over a store only by the
process that issued it, while it holds the caller.

See [direct-js.md](docs/design/direct-js.md).

## Where state lives

| State | Today | Planned |
|---|---|---|
| CAS index, action cache, farm time | `MetaState` behind `RaftMetaLog`, applied from a single-voter Raft log on local disk (`--meta-dir`, or `<hot store>/meta`); behind `MemoryMetaLog`, in the process's memory, only in tests | the Raft log with three voters |
| Blob bytes | segments in an object store (in memory, or an S3 bucket) | the same, with garbage collection and multiple stores |
| Open operations, finished answers | the executor's memory, and with `--state-store` (Postgres or SQLite, through `kbf-state`; a frontend always has one) the rows `exec/op/<id>` (owner, action, attempt, job id), `exec/live/<instance>` (a process is alive) and `done/<id>`, `doneof/<id>`, written before the job is submitted and deleted when the operation finishes; without a store a restart loses them | the same rows, kept as they are |
| Jobs | the job service's, not kbf's: kbf holds only each operation's job id | the same |

Under `MemoryMetaLog` (tests only; the binary has no flag for it) the index is in memory,
so a restarted server forgets every blob, and an S3 store gets a fresh key prefix at
each start so it never reads objects a previous process wrote. The binary always runs
the Raft log: a restart reopens the log in its directory and keeps the index and the
action cache, and every start writes under the prefix the log committed when it was
created, `<cold store prefix><log id>/`, each in a new writer epoch.

Every piece of state sits behind a trait or a pure state machine, so moving it into a
replicated log changes the implementation behind the seam, not its callers:

- `MetaLog` (in `kbf-front`): `commit` a metadata command, `query` the committed
  state. `MemoryMetaLog` is the single-process implementation; `RaftMetaLog` (in
  `kbf-server`) implements the same two calls over a single-voter Raft log.
- `ObjectStore` (in `kbf-objstore`): five calls any S3-compatible store can serve, and `may_hold`, which a local store answers from its files and any other with yes.
- `Dispatch` (in `kbf-front`): how the REAPI front reaches what runs its Executes (the
  executor of `kbf-server`, one job of the job service for each).
- `StateMachine` (in `kbf-types`): inputs in, effects out; the metadata state is one.

## One endpoint, a leader and its followers

Today kbf runs as one `kbf-server` process. The shape decided for more than one
server (**planned**) is in [deployment-topology.md](docs/design/deployment-topology.md),
with what exists today and the probes still to run. In short:

- **Servers on storage hosts,** each at a fixed address with a stable DNS name;
  dedicated storage hosts are preferred, and a converged layout (servers, the object
  store and workers on the same hosts) is supported. Durable state is a Raft log on
  each server's local disk, with its snapshots also copied to the object store: one
  voter first, then three voters on three hosts for high availability (one leader,
  two followers). Elastic or stateless servers are not part of the design.
- **One name for the farm.** Bazel and Buck2 are configured with one remote address,
  and Buck2 sends everything to it. A tailnet ingress with a constant name and an
  automatic certificate terminates the clients' TLS; an HTTP/2 proxy behind it
  health-checks the servers and routes by gRPC method: reads and uploads to any server
  ready to serve reads, every other call to the leader. Each server reports both
  answers on two readiness paths and on `grpc.health.v1` on its REAPI listener (built
  today with `/readyz`'s one answer, [api.md](docs/api.md#grpchealthv1-on-the-reapi-listener)). The
  proxy reaches each server over TLS with a certificate from the farm's internal CA
  (the REAPI listener's TLS is built today), or in plain text on loopback when it runs
  on the same host (see [Security model](#security-model)).
- **Any server serves reads; the leader does the rest.** At three servers, every
  server answers the read path (`ByteStream.Read`, `BatchReadBlobs`,
  `FindMissingBlobs`, `GetActionResult`) from its replicated state and accepts upload
  bytes, which it writes to the object store before sending the leader the small
  metadata commit. So the byte traffic is shared across the three hosts. A follower
  may answer a false "missing", which only makes the client upload again, but never a
  false "present". The leader of the control log keeps every metadata write,
  `Execute` and `WaitExecution`. A follower takes over when it is
  elected. The first deployment is a single server.
- **Executes need no leader.** Each frontend submits its own jobs to the job service; over a
  state store any frontend answers an operation by name and takes up a gone frontend's
  operations ([direct-js.md](docs/design/direct-js.md) section 3).

## Security model

Build clients and storage nodes reach `kbf-server` by different paths, and each path has its
own protection.

**Build clients go through a front.** TLS for Bazel and Buck2 ends at a front that
holds a real certificate for the farm's client-facing name: a load balancer, or a
proxy on a WireGuard mesh such as `tailscale serve` in its HTTPS mode. Behind the
front, `kbf-server` serves REAPI either as plain-text gRPC, bound to loopback (front on
the same host) or to the mesh interface, whose traffic WireGuard already encrypts, or
over TLS of its own (`--reapi-tls-cert`, `--reapi-tls-key`, built today) with a
certificate from the farm's internal CA. That TLS is for the hop from the front's proxy
to a server on another machine, and for clients that trust the internal CA and dial a
server directly. This is adequate for a private network; it does not replace the
front's certificate for the client-facing name, and it authenticates no caller. With
several servers (**planned**), the front is a tailnet ingress followed by an HTTP/2
proxy that routes reads and uploads to any ready server and every other call to the
leader ([deployment-topology.md](docs/design/deployment-topology.md)); that pair is
not yet probed.

- **Today:** the REAPI listener (`--listen`, default `127.0.0.1:8980`) serves plain
  text, or TLS when given a certificate and key (`--reapi-tls-cert`,
  `--reapi-tls-key`, both or neither). Under TLS it presents a server certificate
  only: it asks clients for none, so a client certificate identifies no caller. A key
  that does not match its certificate stops the server before it prints its start
  line. Over TLS as over plain text, it runs the authentication policy and the
  per-call authorizers of `--reapi-auth-policy`, in Buildbarn's model
  ([docs/reapi-auth.md](docs/reapi-auth.md)); the policies built so far (`allow`,
  `deny`, `any`, `all`, and instance-name prefixes) check no credential. Without the
  flag every call is accepted, and whoever reaches the port can read action inputs
  and outputs, write the CAS and Execute actions.
- **Planned:** credential policies in the same file (a bearer JWT in the
  `authorization` header, a remote authentication service), checked by `kbf-server`
  itself behind the front; the front passes the header through and does not check
  it. A peer address does not identify a caller here: a proxy on the same host
  connects from loopback, whoever its client is.
- **Built: TLS is optional; plain text off loopback is warned about, not refused.**
  `kbf-server` serves a listener without its TLS flags in plain text on whatever
  address it is given. When the REAPI or worker listener serves plain text on an
  address that is not loopback (a wildcard such as `0.0.0.0` or `::`, or any
  interface address; `::ffff:127.0.0.1` counts as loopback), the start line ends with
  one ` warning=plaintext-off-loopback:<listeners>` token, `<listeners>` being
  `reapi`, `worker` or `reapi,worker`, and the server starts. Whether plain text is
  acceptable on that network (a loopback front, a mesh interface WireGuard already
  encrypts) is the operator's call. A deployment that wants TLS, ours included, turns
  it on in its config: `--reapi-tls-cert` and `--reapi-tls-key` for a REAPI hop that
  crosses machines, and the three worker TLS flags for storage nodes on other machines.
  The warning looks at the bind address only: it does not restrict which peers
  connect, and it does not look at `--reapi-auth-policy`.
- **A front that works: `tailscale serve` in HTTPS mode.** A probe with
  `tailscale serve` 1.102.4 ran its HTTPS mode (`tailscale serve --https=<port>
  http://127.0.0.1:<reapi>`) in front of a plain-text REAPI listener on loopback.
  That version has no `h2c://` backend scheme; the `http://` backend carried gRPC.
  Through it, over TLS, these all worked: GetCapabilities, FindMissingBlobs, a 64 MiB
  ByteStream write and read back with a matching sha256, BatchUpdateBlobs and
  BatchReadBlobs (up to 1 MiB each), Execute, and WaitExecution on a finished action.
  A Buck2 remote-only build got through capabilities, uploads and Execute (the probe's
  fake driver wrote no outputs, so the action itself failed on missing outputs).
  Configuring `tailscale serve` needs root or a Tailscale operator on the host.
  `kbf-server` sees every such connection as coming from loopback. The HTTPS mode can
  add Tailscale identity headers (such as `Tailscale-User-Login`; not tested by the
  probe); `kbf-server` does not read them.
- **Not supported for REAPI: `tailscale serve`'s TLS-terminated TCP mode**
  (`--tls-terminated-tcp`). Its certificate verifies, but it negotiates no ALPN, and
  gRPC clients require `h2`. Buck2 fails with "HTTP/2 was not negotiated"; gRPC's C
  core (for example Python `grpcio`) fails with "Cannot check peer: missing selected
  ALPN property".
- **Long silent streams.** Through the HTTPS front above, a queued Execute and a
  WaitExecution stream for an action no node could run carried no messages for 300 s;
  both then ended with `FAILED_PRECONDITION` when `--unservable-wait-secs=300` failed
  the action. That front did not cut them. Another front whose idle timeout is shorter
  than the longest queue wait would cut these streams.

**Storage nodes do not go through the front.** A storage node's `--frontend` names the worker
listener, not the client front above. The worker listener keeps its own mutual TLS end to end. With
several servers (**planned**), nodes dial the servers' own names, with no balancer
in between ([deployment-topology.md](docs/design/deployment-topology.md)).

- Nodes connect only over mutual TLS (`https://` URLs). The server's worker listener serves
  mutual TLS when given a certificate, key and client CA. A node's certificate must name
  its node id as its one DNS subjectAltName, so a certificate can speak only for its own
  node, and a deny list (serials, public keys, node ids), looked at again at every
  call, refuses leaked or retired certificates without a restart. There is no CRL or OCSP;
  short certificate lifetimes bound what the list misses. See
  [worker-protocol.md](docs/design/worker-protocol.md#node-identity-and-the-deny-list).
- **Built: roles on the worker listener.** The role is read from the leaf certificate
  (`urn:kbf:role:<name>`) and each method takes only the roles expected there: a
  storage node registers, heartbeats and fetches the registered cold store. The `daemon` role
  (`urn:kbf:role:daemon`) is still read from a certificate, and no
  program of this release holds it. Nothing else of REAPI is served there
  (Execute, the action cache and `Capabilities` answer UNIMPLEMENTED), and on a plain-text
  worker listener every blob call is refused UNAUTHENTICATED. See
  [worker-protocol.md](docs/design/worker-protocol.md#blobs-on-the-worker-listener).

**Inside the farm:**

- Only the frontend writes the action cache, from a result it accepted, and the metadata
  state machine itself refuses an action-cache write from any role but `Daemon`
  (the role the frontend's own writes carry).
- A job runs with no network in the job service's own sandbox; the Command sees its mounted
  tree and its put directory and no more ([kbf-modes.md](docs/deploy/kbf-modes.md)).

To report a vulnerability, see [SECURITY.md](SECURITY.md).

## Testing

The project's testing rules are T1 to T19 of [AGENTS.md](AGENTS.md): tests assert
behaviour, a bug fix ships a test that failed before the fix, and the coverage gate checks every crate for 99% of lines and branches, or its floor in
[coverage-floors](coverage-floors), which may only rise. How the code is tested:

- **Pure cores, unit-tested.** The metadata, segment, capability and Raft
  crates are plain state machines with plain tests; each test says which defect it
  catches.
- **Deterministic simulation.** `kbf-sim` runs state machines on a virtual clock with
  a seeded random stream and a message bus that delays, drops, duplicates, reorders
  and partitions. The same seed gives the same trace hash on any machine, so a failing
  seed is a complete bug report. The
  Raft core runs there with crashes part-way through its effects (checks: election
  safety, log matching, leader completeness, state machine safety).
- **Contract suites.** Every object store backend must pass the conformance suite in
  `kbf-objstore`; CI runs it against the in-memory store, the local-directory store,
  and MinIO and RustFS containers, with and without Object Lock.
- **Real services.** Server tests run the REAPI and worker services over real gRPC,
  including mutual TLS.
- **End to end.** The `beta` workflow starts real `kbf` processes as the packaged units
  start them (a frontend and storage nodes, under certificates of the farm's rows), and the
  mixed-version test runs the tip (N) and the release before it (N-1) in one farm, both ways,
  and restarts each storage node as the other version on the files it had. The executor is
  tested against the real job service and an agent (`executor_real_service_integration`),
  and `kbf-deploy job-service apply` against a scripted frontend host. These farms run
  actions only through the executor.
- **Repository lints.** Workflows may only use hosted runners and pinned actions from
  an allowed set; public text may carry no machine addresses or home paths.
