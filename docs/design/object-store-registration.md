# ADR: object-store registration is durable desired state, fetched by storage nodes over mutual TLS

Status: the control API and the node-side apply are built. The routes are in
`crates/kbf-server/src/object_store/api.rs` (`GET /v1/object-stores`, `/audit`,
`/connections`; `GET`, `PUT`, `DELETE` and `POST` (verify) on `/connections/{target}`; `PUT`
and `DELETE` on `/bindings/{role}`; they answer only when the frontend was started with
`--object-store-secrets-dir`), and a storage node with an `s3://` `--cold-store` and
`--frontend` fetches the registered store with `ColdStore.GetStoreConfig`, probes its keys
and opens its cache on them at its start (`crates/kbf-server/src/cold_apply.rs`). This page is
the design they follow; it was written as a proposal, and where it and the code differ, the
code is what runs. Every decision in section 4 was an **engineering default** that stood
until the maintainers answered the question beside it (section 14).

Labels, as in the other design pages: **[F]** read in code on this branch, **[D]** a decision
this page makes, **[V]** verify before building on it. A claim with none of them is a
definition.

**Supersession.** This ADR supersedes the object-store parts of the open design pull request
#966 (`docs/design/install-and-enrollment.md`, section 4.3 and the units B3b and B4a of its
section 11): the `ColdStore.Watch` stream, `PUT /v1/object-store` and the file
`<frontend state>/secrets/object-store.json`. It also supersedes the unimplemented
`ColdStore.GetKeys` rpc **[F: `crates/kbf-proto/proto/kbf/enroll/v1/enroll.proto`, no server
anywhere]**. #966 is someone else's open change and is not edited by this one. If #966 merges
first, a one-paragraph follow-up there points its section 4.3 at this page. Everything in #966
that is not about the object store (tokens, the CA, `kbf.toml`, the enrollment protocol) is
untouched.

Related: [operator-verbs.md](operator-verbs.md) (every operator verb and its route),
[../api.md](../api.md), [distribution.md](distribution.md) section 1 (the control API),
[pull-updates.md](pull-updates.md), [worker-protocol.md](worker-protocol.md).

## 1. The answer in ten lines

1. An operator registers the farm's object store through the frontend's operator API:
   a **connection** (a named endpoint and key pair) and a **binding** (the role `cold` to a
   connection, a bucket and a prefix).
2. The registration is **durable desired state** in the frontend's state store with a
   **generation**. The secret key is not in the state store: it is a versioned file the
   frontend account owns.
3. The frontend answers every storage node's `Register` and `Heartbeat` with the
   current `objstore_generation` only. No secret rides on a heartbeat.
4. A storage node that sees a generation it does not hold calls the new unary
   `ColdStore.GetStoreConfig(have_generation)` on the worker listener, over its mutual TLS
   identity. The answer is one atomic snapshot and is the only place the secret crosses a wire.
5. The node probes the store, opens it, and reports the generation it applied and a closed
   status code on its next heartbeat. The frontend keeps that acknowledgement as a short-TTL
   row in the state store (section 6), so it survives a restart and a proxy switch.
6. `GET /v1/object-stores` shows the desired state and whether every node that should have
   applied it has. It never claims convergence the frontend does not know.
7. Keys are held in memory on the node. A node restarted while the frontend is down serves its
   hot tier only and retries.
8. v1 has no hot credential swap. Rotation reaches a running node at its next start.
9. The store identity `(endpoint, bucket, prefix)` cannot change through a connection or a
   binding write once bound; only the deliberate `:repoint` call moves it (section 19).
10. This reads as "the frontend pushes" in effect, though no connection is opened from the
    frontend to a node. A `Watch` stream may be added later as a latency optimisation.

## 2. Facts this builds on

- **Nodes dial the frontend; the frontend dials nobody.** The worker listener's role table
  admits a method per peer role and denies any method not listed **[F:
  `crates/kbf-server/src/worker_roles.rs`, `WORKER_LISTENER_ROLES`]**. A frontend-to-node
  push would be the first connection of its kind, with a per-node retry queue. **[D]** that is
  not built.
- **Storage nodes already beat.** `Membership.Register` returns `member_id`, `boot`,
  `heartbeat_interval_ms` and the map; `Heartbeat` returns `map_generation` and the map when
  newer, and its request carries `have_generation` **[F: `crates/kbf-proto/proto/kbf/map/v1/map.proto`]**.
  The generation-and-fetch shape is the one the map already uses.
- **`GetKeys` has a client and no server.** `ColdStore.GetKeys` is declared with a client in
  `kbf-enroll` that holds the answer in memory only **[F: `enroll.proto`,
  `crates/kbf-enroll/src/keys.rs`]**. Keeping it beside `GetStoreConfig` would be two ways to
  do one thing, so the wire change replaces it.
- **`NodeConfig` also carries the store.** `NodeConfig.cold_store` (an
  `s3://bucket/prefix/?endpoint=...` URI, never a key) and `credential_mode` exist, and
  `kbf node` builds `--cold-store` from the first **[F: `enroll.proto`,
  `crates/kbf-cli/src/node.rs`]**. Section 8 says what becomes of them.
- **The store flag today.** A storage node is given `--cold-store`; `S3Store` refuses an
  `https` endpoint (`TLS is not built yet; use http`) **[F:
  `komira-ai/tierstore:crates/objstore-s3/src/s3/mod.rs`]**.
- **Registration precedes the cache open.** With `--frontend`, the node binds its listener and
  registers before the cache opens, so the cache writes as the registration's member and boot
  **[F: `crates/kbf-server/src/storage_node.rs`, `start_storage`]**. The cold store is opened
  by the caller before `start_storage`, so a node with no `--cold-store` needs that order
  changed. **[V]** in the node unit (section 12, PR E) before anything is deferred.
- **The operator API's write gate.** Loopback peer, no `Origin`, a token file, a Bearer token,
  `Content-Type: application/json` **[F: `docs/api.md` "Who may write",
  `crates/kbf-server/src/api.rs`, `gate`]**. The enrollment routes apply all of it to a GET
  too, answer `Cache-Control: no-store`, answer 404 when the feature is off and 409 on a
  fenced frontend (the object-store routes have no fence: section 18) **[F: `crates/kbf-server/src/enroll/api.rs`]**. This ADR copies that shape.
- **Several frontends serve at once.** Under the ruling of section 18, the frontends of a host
  share one Postgres behind the `kbf-proxy` bundled in `kbf-installer`. Remote operators reach the
  API through a TLS-terminating proxy, whose connections arrive from loopback and pass the gate
  **[F: `docs/api.md`, "Through a reverse proxy"]**.

## 3. Vocabulary

- A **connection** is a named endpoint plus a key pair (and optional region, `conditional_put`
  and `min_put_rate`).
- A **binding** maps a **role** to `{connection, bucket, prefix}`.
- The only v1 role is `cold`, today's `--cold-store`. `history` and `backup` answer 422
  `role_not_supported` until a consumer exists.
- A role here is neither an IAM role nor an enrollment role (`storage`, `daemon`).
- **Identity** is `(endpoint, bucket, prefix)`.
- **Generation** is the counter of `objstore/gen`: it moves when the desired state changes and
  not when a call changes nothing.

## 4. Decisions

Each is an engineering default pending the owner; the question is in section 14.

| # | Decision | Question |
|---|---|---|
| D1 | Delivery is notify-by-generation, fetch over storage-role mutual TLS, ack by heartbeat. `GetKeys` and #966's `Watch` and `PUT /v1/object-store` are superseded. | Q1 |
| D2 | The terms of section 3. | Q2 |
| D3 | Node keys are in memory only. A node without `--cold-store` defers its open until the registration arrives, after the order is verified and `/readyz` says what a node with no store reports. | Q3 |
| D4 | The frontend log route is cut from this work. | Q4 |
| D5 | Out of scope: update policy, image pull (no source verb exists), meta-backup (an installer and root verb), CA rotation (its own unit). | Q5 |
| D6 | The secret file is host-local, readable by every frontend of the host, with versioned names. No writer baton and no `not_writer`. One token, loopback gate, no operator and admin split. | Q6 |
| D7 | Store identity is immutable after the first bind, except through `:repoint` (section 19). | Q7 |
| D8 | A node with a `--cold-store` flag keeps running on it and reports `identity_mismatch` if the registered identity differs. | Q8 |
| D9 | `--cold-store registered` names no store: the node waits for the registration, runs on its endpoint, bucket, prefix and keys, and never falls back. Moving a farm to another store is a registration change and a restart of each node. | (the one-store plan) |

## 5. The control API

One bearer token, no roles **[D]**. Every route passes the full write gate, GET included,
like `/v1/enroll/*`: the routes expose or change the farm's door, so they are not open to
whoever reaches the address. A refused call changes nothing. Every answer carries
`Cache-Control: no-store`. A body is capped, and a body type refuses unknown fields. Errors are
`{"error": "...", "code": "..."}`, `code` a closed, stable string and never a store's or a
node's own text. 404 when the feature is off (section 11). No route is fenced.

| Route | Does |
|---|---|
| `PUT /v1/object-stores/connections/{name}` | create or replace a connection; idempotent; 201 created, 200 changed or unchanged |
| `GET /v1/object-stores/connections` | list connections, never a secret |
| `GET /v1/object-stores/connections/{name}` | one connection, never a secret |
| `DELETE /v1/object-stores/connections/{name}` | delete; 409 `in_use` while bound |
| `POST /v1/object-stores/connections/{name}:verify` | probe only; no state change |
| `PUT /v1/object-stores/bindings/{role}` | bind `cold` |
| `DELETE /v1/object-stores/bindings/{role}` | unbind; 409 `in_use` once any node has applied it |
| `GET /v1/object-stores` | desired state and convergence |
| `GET /v1/object-stores/audit?limit=` | audit rows |

**Names.** `{name}` is `[a-z0-9][a-z0-9_-]{0,62}`, otherwise 400 `bad_name`. The name becomes a
file name, so `..`, a slash, a NUL byte and `:` (which also collides with the `:verify` custom
method) must never reach the filesystem. The same rule holds for `{role}` against the closed
role set.

**PUT connection body.** `{endpoint, region?, access_key_id, secret_access_key,
conditional_put?, min_put_rate?, expect_generation?, verify?}`; `verify` defaults to true.

- `secret_access_key` is a `Secret` newtype: zeroize on drop, `Debug` prints `Secret(..)`, no
  `Serialize`.
- An `https://` endpoint answers 422 `tls_unsupported` while `S3Store` refuses TLS, so a
  registration is never accepted and then fails on every node.
- A failed frontend probe answers 422 `probe_failed` and changes nothing. That probe proves the
  frontend can reach the store; each node probes too.
- `expect_generation` mismatching answers 409 `generation_mismatch` and writes nothing.
  Without it, the last writer wins and both writes are in the audit log.
- Changing `endpoint` of a bound connection answers 409 `identity_change`. Credentials,
  region, `conditional_put` and `min_put_rate` change freely.

**PUT answer.** `{name, generation, changed, access_key_id, secret_set, verified: {ok, put,
get, delete, conditional_put}}`. No secret and no digest of one.

**GET answer.** `{generation, connections[], bindings[], convergence: {converged, expected,
applied, unsupported, nodes: [{id, state, applied_generation, status, last_seen_secs}]}}`.

- `status` is one of `ok | pending | probe_failed | identity_mismatch | unsupported`: a closed
  code, never a node's free text.
- `expected` counts active members that are not draining or denied. A node that has never
  reported the field is `unsupported` (an older build ignores the new heartbeat fields), is
  counted in `unsupported` and not in `applied`, and is named in `nodes`, so version skew is
  not read as "not fetched yet".
- A node not seen since the frontend restarted is `pending` with a null `last_seen_secs`.
- `converged` is true only when every expected node has applied the current generation with
  status `ok`.

## 6. Persistence

**State rows** (`kbf-state`), hand-derived prost rows that ignore unknown fields, like the
`enroll/` rows **[F: `crates/kbf-state`, the enrollment rows]**:

- `objstore/conn/{name}`: the non-secret fields, `secret_version`, `updated_generation`.
- `objstore/bind/{role}`.
- `objstore/gen`.
- `objstore/audit/{seq}`: the outcome, the name, the generation, the time. No body field and
  no secret. The actor is "the API token"; no finer identity exists to record **[D]**.
- `objstore/ack/{member}`: a node's applied generation and closed status, a short-TTL row
  (15 s, refreshed by each heartbeat) written with compare-and-set. The unit D code
  implements this; the plan had it in memory.

**Schema rule (ruling, section 18).** These are rows in the existing `kbf-state` key/row
store: `objstore/conn`, `bind`, `gen`, `audit` and `ack` are key prefixes, not tables, and
this work adds no table and no column. `kbf-installer` creates and migrates the schema; `kbf`
creates or alters nothing at start and refuses a schema version it does not support. Should a
unit truly need a schema change, it ships as a `kbf-installer` forward migration, expand-first,
that a frontend of the previous build running beside a new one tolerates, and the lead is told first.

One atomic multi-key transaction per mutation covers the row, the generation bump and the
audit row, compare-and-set on the generation. A PUT that changes nothing writes an audit row
with `outcome=unchanged` and does not bump the generation.

**Secret file** `<secrets dir>/objstore/{name}.{secret_version}.json`, mode 0600, in a
directory created 0700. The write order, which `Secret` and the file store keep:

1. write a temp file in the same directory, under a name unique to the call (two concurrent
   PUTs never share one);
2. fsync it;
3. rename it into place;
4. fsync the directory;
5. compare-and-set the row that names the new `secret_version`.

Version N-1 stays until the row for N commits. A start-up sweep removes files no row names. A
crash between 4 and 5 leaves an orphan and the live secret untouched. A row never names a
missing file. If a named file is missing or wider than 0600, the routes answer
`secret_missing` and `GetStoreConfig` answers `UNAVAILABLE` (fail closed). The idempotent
compare reads the file and compares in constant time; **no digest of the secret is stored**
anywhere, because a digest of a short secret is an oracle.

The failpoint tests for these steps prove the order of the steps and the recovery rule. They do
not prove fsync durability, which only a power cut shows; this ADR does not claim it. The
failpoint mechanism, if it is a dependency or a feature, is named in the storage unit and
cleared with `deny.toml` there.

**Backups.** The secret is not in the state store, so a backup of Postgres carries no
credentials; the installer backs Postgres up, and this work adds no backup route or flag. A
filesystem backup of the host or the installer must exclude `<secrets dir>`; the operator
documentation names the exclusion, and the installer's backup list is checked for it **[V]**.

**Acks** are the `objstore/ack/{member}` rows above: short-TTL rows in the existing store,
written by heartbeats with compare-and-set. They survive a frontend restart and a node's move
to another frontend, so neither resets convergence. An expired row is "not heard from",
so `converged` is false until the node beats again: never a false converged.

## 7. Delivery

- `RegisterResponse` and `HeartbeatResponse` gain `objstore_generation` (additive), emitted to
  the storage role only. The requests gain `applied_objstore_generation` and a closed
  `apply_status`.
- `ColdStore.GetStoreConfig(have_generation)` on the worker listener returns one atomic
  snapshot (connection, binding, secret, `secret_version`) or "unchanged". The role table row
  is storage only; any method not listed is denied, and a test keeps the deny when the row is
  deleted. Per call, not per connection: the peer holds the storage role, is not on the deny
  list, and its registry row is `active` when a registry is present. A node revoked or denied
  gets `PERMISSION_DENIED` on its next call even on a live session. With no enrollment
  (`None`) a denied node is still denied.
- **Direction of a generation.** A node refetches on any generation that **differs** from the
  one it holds, not only a higher one. A restored Postgres backup or a swapped state database
  can lower `objstore/gen`, and a node that ignored the lower number would never converge. A
  snapshot also carries a store epoch **[D]**, so "lower" and "different" are told apart in
  logs.
- **The node applies at start (v1).** It fetches, holds the keys in memory, probes
  (put, get, delete of one object under `.kbf-probe/`, removed afterwards), then opens the
  store, retrying with backoff while the frontend is down and serving hot only meanwhile. It
  reports the applied generation only after the open and the probe succeed. A crash between
  fetch and report claims nothing, and the refetch is idempotent.
- **Flag precedence.** A node started with `--cold-store` whose identity differs from the
  registered one keeps running on its flag and reports `identity_mismatch`. If the flag
  matches, the registration supplies the keys only.
- **`--cold-store registered` (D9).** The word is the whole flag: the node holds no URL and no
  key, and the registration is the one place its store comes from. There is no flag store to
  compare with, so there is no `identity_mismatch`, and no fallback, so a node with no
  registration waits and a node whose keys fail the probe stops (`probe_failed` is what the
  frontend sees of a node that has a store of its own; a registered node has none, so it exits 75
  and the restart reads what the operator registered next). The node asks before it opens its
  metadata log, because the log keeps the objects under the registered prefix and refuses to
  open under another one, so it is no member of the farm while it waits. Nothing else names
  the store: `--cold-store-credentials-file` is refused at the start, and `AWS_ACCESS_KEY_ID`,
  `AWS_SECRET_ACCESS_KEY` and `AWS_ENDPOINT_URL` are not read. It reports
  the generation it fetched as `ok`; a new generation while it runs is `pending`, and the node
  takes it at its next start. A changed endpoint is therefore a registration change and a
  restart of each node, **and it is safe only when the new store already holds every object
  the node's index names**: the same bucket and prefix, the old endpoint being write-through
  (for example a write-through cache in front of the same bucket). A node does not check
  that: an index entry for an object the new store lacks is a read that fails. A changed
  **bucket is not checked at the start** (the log's `Bootstrap` holds only the prefix), so
  moving to another bucket, with its objects copied, is the operator's duty. A changed
  prefix is refused at the start with a message that says so (`PrefixChanged`) and needs an
  empty `--meta-dir`. While it waits the node says so (`waiting for the registered cold
  store`, with the frontend) and each failed ask says its attempt, status code and reason;
  a frontend too old to serve `ColdStore` (`Unimplemented`) is waited for, as is one that is
  down, while a denied node (`PermissionDenied`, `Unauthenticated`) and a request the
  frontend calls malformed (`InvalidArgument`, `FailedPrecondition`, `NotFound`) end the start
  at once, since waiting does not change them. The cold-store HTTP client reads `HTTP_PROXY`,
  `HTTPS_PROXY` and `ALL_PROXY` from the environment (reqwest's default, for the URL path as
  for `registered`), and kbf cannot turn that off for one store: the client is built inside
  the `objstore-s3` crate, which has no such setting. Put the endpoint's host in `NO_PROXY`
  on a host that sets a proxy. Only a storage node
  takes the word: the frontend, `all-in-one` and `--history-objects` refuse it.
- **A bad credential** reaching a node reports `probe_failed`; a node that already had a store
  keeps it.
- **No fence.** The proxy forwards each stream to one frontend of the pool, so registration
  writes and `GetStoreConfig` reach whichever it picked. There is no writer role to lose, and a
  frontend holds no state the others lack. Generation bumps are compare-and-set in Postgres, and
  a secret file is written whole under a new versioned name, so a reader in another frontend
  never sees a partial file.
- **Who is calling.** The proxy terminates TLS and verifies mutual TLS, so a node's identity
  (node id and role) may arrive from the proxy and not from the child's own TLS peer (#1095).
  The per-call check above therefore sits behind one function, "who is calling", and nothing
  else parses a peer certificate. Units D and E build to that rule, so #1095 swaps the source
  in one place.
- **A newer frontend.** The frontend updates first, so a node meets an N+1 frontend. The node
  ignores unknown fields in `GetStoreConfig` and heartbeat answers, and a generation or
  status it does not understand is reported (`unsupported`), never a crash.
- **No hot swap.** Rotation reaches a running node at its next start, and so does a new store
  for a node on `--cold-store registered`; until then `GET` shows that node `pending`. kbf never restarts itself. A hot swap needs a lock-free load of the
  credentials on every request of the S3 path and a performance review, and is not scheduled.
- **Readiness.** `/readyz` of a node with no store says so by a check name, so an operator
  can tell "no store registered" from "store down" **[V]** in PR E.

## 8. `NodeConfig` and `GetKeys`

`NodeConfig.cold_store` and `credential_mode` are a second way to deliver the same setting.
**[D]** After this change they are **deprecated**: nothing new writes them, the proto comment
says so, and the registration is the only source of the store's keys and identity. Their
removal rides with #1049, which removes `kbf node` (the reader of `cold_store`) and touches
`kbf-enroll` and `kbf-cli/node.rs`, the same files the wire unit moves the `GetKeys` client
through. Whichever merges second adapts: if #1049 lands first, the wire unit has no `node.rs`
line to touch and moves only `keys.rs`; if the wire unit lands first, #1049 drops the
`cold_store` reader along with `kbf node`. `GetKeys` is replaced, not kept: the client in
`keys.rs` moves to `GetStoreConfig`, keeping memory-only storage and zeroize.

**[D] `kbf node` passes `--cold-store registered`.** The frontend still writes `cold_store`
into `Enroll` and `Renew` answers, but `kbf node` no longer reads it: a stored URI went stale
when the farm re-pointed its store, and a node enrolled before the registration had none. The
node starts with the word `registered` and fetches the registration itself, before it opens its
metadata log, so it needs no URI from enrolment, and the old refresh of an empty `cold_store`
at start is gone. The text below says what `cold_store` holds for the nodes that still read it
(older builds): the URI
holds the bucket, prefix, endpoint, region and put settings of the `cold` binding and no key
(`docs/api.md`, enroll section). A node uses the `cold_store` it is sent with its own process credentials, so
the frontend picks the endpoint that receives the node's signed requests: the same trust as the worker and CAS endpoints it already sends. The node's registration fetch then finds the same identity
and applies the keys. `cold_store` goes from `Enroll` and `Renew` once no build reads it.

## 9. Secrets: where they can leak, and what stops it

| Path | Rule |
|---|---|
| a prost message deriving `Debug` carries the secret on the wire | the response holds the secret in a wrapper type whose `Debug` is redacted by construction; a sentinel test over gRPC trace output |
| the request body and the JSON buffer in the frontend are not zeroized | the frontend sets no core dump and non-dumpable, as the node client does **[F: `kbf-enroll/src/keys.rs`]**; the residual exposure (heap memory of a live process readable by its own user or by root) is accepted and stated here |
| `probe_failed` text | a closed code for `:verify` and PUT; never the S3 error body; tested |
| an error body or a log line echoing the request | the 400, 401, 413, 415, 422 and 500 bodies and TRACE logs carry no body bytes; tested with a sentinel |
| the node | keys never on disk, in argv or in the environment; a scan of the temp dir and the arguments |
| the state database, the audit rows, `Debug` output | never hold the secret |
| a sentinel scan that passes vacuously | each scan first asserts the sentinel **is** in the secret file, then that it is nowhere else |
| the daemon wire | `Welcome`, `MapUpdate`, `Session` and the daemon heartbeat carry no store field; apart from the read-only key of `--pack-read`, the daemon builds no cold store: its crates cannot reach `kbf-front`, `kbf-server` or `ureq` **[F: `crates/kbf-integration-test/tests/layering.rs`, `crates/kbf-proto/tests/daemon_wire.rs`]** |

## 10. The probe is a request from the frontend to an operator's endpoint

`:verify` and `PUT` with `verify` make the frontend connect to an endpoint the operator named,
carrying the key. That is an operator-only action behind the token, but it is also a server-side
request the operator chooses the target of. The probe therefore has a connect timeout and a
total timeout, a size bound on what it reads, and sends nothing but the probe object. It does
not follow redirects. The target is not restricted to a public address, because a private farm's
store is private; the token is what authorises it **[D]**.

## 11. Flags and the feature switch

Configuration is flags **[D]**. Names are defaults pending the code:

- `--object-store-secrets-dir <path>` names the secrets directory. Setting it switches the
  feature on; without it every `/v1/object-stores/*` route answers 404 behind the gate. One
  flag, not a boolean beside a path that could disagree.

Each frontend of a host has the flag of its own, and the directory must be readable by every one
of them: a frontend started without it answers 404 on these routes.

## 12. Failure modes

| Failure | Behaviour |
|---|---|
| node offline during a registration | `pending`, `converged: false`, with `last_seen_secs`; it reconciles at its next heartbeat; no frontend queue exists to lose |
| frontend restart or proxy switch | rows and acks are durable (acks are short-TTL rows): `converged` stays right; an ack that expired reads as not heard from, so `converged: false` until the node beats |
| frontend crash between file and row | an orphan file, swept; the live secret untouched |
| frontend down at node boot | the node cannot open a registered store (keys are memory-only); it retries with backoff and serves hot only |
| bad credential reaching a node | `probe_failed`; a node that had a store keeps it |
| revoked or denied node | `PERMISSION_DENIED` on its next call; keys it already holds are not recalled, so the operator rotates after revoking |
| several frontends at once | the proxy forwards each stream to one of them, and any may take a registration write; Postgres compare-and-set arbitrates the generation; versioned secret files are never partial |
| concurrent operators | `expect_generation` compare-and-set; otherwise last writer wins and both are audited |
| Postgres restored from an older backup | generation lower than a node's: the node refetches on a difference (section 7) |
| a node on an older build | `unsupported`, counted separately |
| the secret file missing or wider than 0600 | the routes answer `secret_missing`; `GetStoreConfig` answers `UNAVAILABLE` |

## 13. Rotation, and the proxy

**Rotation** (copied into `docs/api.md` with the routes). The API never revokes a key.

1. Add the new key at the store.
2. `PUT` the connection with the new secret from a file, with the JSON content type, which the
   gate requires and `curl -d` does not send:
   `curl -X PUT -H 'Authorization: Bearer ...' -H 'Content-Type: application/json'
   --data @connection.json .../v1/object-stores/connections/main`.
   An example never puts a secret inline.
3. Restart the storage nodes through the installer (v1). This is a cluster write and is the
   operator's.
4. Poll `GET /v1/object-stores` until `converged` is true.
5. Then revoke the old key at the store.

There is no operator verb that makes a node pick a rotation up, because kbf never restarts
itself and the installer has no such verb yet. Until it has one, rotation ends with the
operator waiting on `pending`; the verbs table has a row for it
([operator-verbs.md](operator-verbs.md)). A hot swap would remove the wait (section 7).

**The proxy.** The reverse proxy bundled in `kbf-installer` terminates TLS and connects from
loopback, so it passes the gate and remote operators (the desktop app) can call these routes.
There is no Envoy. Whether it may carry the secret-bearing route (`PUT .../connections/{name}`)
or only the reads is a question for the owner (Q6), because the secret then crosses the
proxy. Until answered, the operator documentation says not to forward it.

## 14. Questions for the owner

- **Q1 Delivery direction.** Nodes dial the frontend and the frontend role may call nothing,
  so a literal frontend-dials-node push would be the first of its kind, with a per-node retry
  queue. Default: D1. Accept this as "the frontend pushes", or require a true dial-out push?
- **Q2 Meaning of "connection" and "bucket and role".** No definition exists in code, docs or
  plans. Default: section 3. Confirm, or supply the real definition.
- **Q3 Node secret at rest.** Default: memory only, hot-only service while the frontend is
  down, and a node without `--cold-store` defers its open. Alternative: a 0600 node-local
  file, which leaves a secret on every node. Which?
- **Q4 Frontend log route.** A log tail was in both candidate designs. Default: cut. If
  wanted, it is a frontend-own ring buffer with a redaction test. Cut or keep?
- **Q5 "Every remaining verb".** Update policy and image pull have no source verb on the
  branch. Default: out of scope until one exists; meta-backup stays an installer and root
  verb; CA rotation stays its own unit. Confirm.
- **Q6 Token and proxy.** Default: the secret file is host-local and every frontend of the host reads
  it; the one-token loopback gate stays. May the installer's proxy carry secret-bearing routes
  (section 13)? Multiple hosts need a shared secret store (#966 section 14.8), still open.
- **Q7 Immutable identity.** Moving to a different store is `:repoint` (section 19).
- **Q8 Flag precedence.** Default: D8. Confirm.

## 15. Held steps (the owner's, never a change's)

None of these is done by a pull request in this series; each needs the owner's explicit go.

- Minting the real object-store key pair, its bucket and IAM policy, and any object-lock backup
  bucket (object lock cannot be enabled on an existing bucket). The first `PUT` on any live
  farm.
- Creating `<secrets dir>` on a live host, including the packages' systemd unit changes
  (`StateDirectory`, `ReadWritePaths`).
- The first operator token on any live frontend.
- Restarting nodes for a rotation (step 3 of section 13): a cluster write.
- Any edit or comment on #966.
- Adding or changing a required check, a DCO app, a label (the `macos` label among them), or
  any branch-protection setting.

## 16. Composition with open work

- **#995, #997, #1028** (merged): the enrollment routes are wired as `None` on the branch
  binary, so they answer 404. The node-facing check of section 7 does not wait for enrollment
  to be wired (#1055): a test pins that a denied node is denied with enrollment `None`.
- **#1055** (draft) edits `frontend.rs`. Our wiring is one merge line and one service
  registration, so a textual rebase is all it needs.
- **#976, #1018, #1022, #1007, #1062** add `/v1/update/*` to the same router. None is this
  work; the surface that can conflict is the merge lines in `frontend.rs`. #1007 also edits
  `crates/kbf-server/src/api.rs` and the `kbf-auth` policy: before the control API unit
  writes its routes it reads #1007's diff and, if a per-route policy exists, registers the new
  routes in it. The route-level rule for these routes is section 5's, not "n/a".
- **#1049** removes `kbf node`: section 8.
- **#1064**: section 11.
- `distribution.md` section 1 said tokens, registration and updates were all "designed, not
  built". The first is built (`/v1/enroll/tokens`, answered 404 until enrollment is wired);
  this change corrects the sentence.

## 17. Units

Each is its own pull request, red first, listing its mutants. Each that adds code also
meets the coverage gate (`coverage-floors`).

| Unit | Does | After |
|---|---|---|
| A | this ADR, [operator-verbs.md](operator-verbs.md), the planned stanza in `docs/api.md`, the correction in `distribution.md` | |
| B | DTOs with `Secret`, the `objstore/` rows, the generation compare-and-set, audit rows, the versioned secret-file store with its start-up sweep | |
| C | the control API: connections, bindings, `:verify`, status and audit | B |
| D | the wire: `GetStoreConfig` replaces `GetKeys`, generation fields, the storage-only role row, the ack rows (`objstore/ack/{member}`, TTL, compare-and-set) | B |
| E | the node applies at start: fetch, probe, open, report, identity guard | D |
| F | `GET /v1/denied` only: the backup route was dropped with SQLite (#1094) | |
| G1 | thin `kbf cluster` verbs for these routes | C, F |
| G2 | end-to-end failure scenarios in `kbf-integration-test` | E |

The drift guard (every route of the operator router appears in `docs/api.md` and in
[operator-verbs.md](operator-verbs.md), method and path) is the test
`crates/kbf-server/tests/route_docs.rs`. It reads the `.route(` calls of the sources of
`kbf-server`, which are the merged routers, not a test router, and a planted undocumented
route, method or node verb (`:cordon`, `:drain`, `:uncordon`, each needs its own row) turns it
red. It is a textual scan, with the limits its header names. It does not check the gate column of the verbs table, which
lives in the handlers.

## 18. Ruling on the frontend architecture (overrides earlier text of this page)

- **Postgres only.** SQLite is dropped; Postgres is a runtime dependency of the frontend and
  the all-in-one. Rows, generation and audit go through the `kbf-state` store on Postgres;
  tests use its fake or Postgres. Every "state store" above means that.
- **No writer baton.** The frontends of a host share one Postgres under the proxy bundled in
  `kbf-installer`, and the proxy forwards each stream to one of them. `not_writer` and any
  fence gating of these routes are removed. The secret file is host-local and readable by every
  frontend of the host; writes are atomic with versioned names.
- **The frontend updates first**, then triggers node updates, so nodes accept an N+1 frontend
  (section 7).
- **No Envoy.** Earlier mentions of the proxy mean the installer's.
- **The proxy forwards each stream to one frontend.** A registration write reaches the frontend
  the proxy picked, and writes go through the shared store, so every frontend sees them. Acks are
  rows in the shared store, so a restart or a roll keeps convergence. The word "both serve" in
  earlier sections means "any frontend of the host may be the one the proxy picks".
- **Schema.** Rows in the existing store only; see the schema rule in section 6.
- **Identity.** One "who is calling" function, whose source may be the proxy (section 7).
- **No state migration:** a fresh Postgres. **A second update is rejected while one runs.**

## 19. Re-point

`POST /v1/object-stores/connections/{name}:repoint` (`kbf cluster object-store repoint`)
moves the store the role `cold` names. It is the only call that does: a connection `PUT` that
names another endpoint, and a binding `PUT` that names another bucket or prefix, still answer
409 `identity_change`, and an unbind still answers 409 `in_use` once a node has applied the
binding.

- **Request.** `{endpoint, bucket?, prefix?, expect_generation}`. Bucket and prefix keep the
  binding's current values when absent (an empty prefix given is an empty prefix).
  `expect_generation` is required: a repoint decided on a stale reading is refused with 409
  `generation_mismatch`. There is no `verify` field and no `--no-verify`: a repoint is always
  probed.
- **Which connection.** The one the `cold` binding names. A connection that exists but is not
  the bound one answers 409 `not_bound`; one that does not exist answers 404
  `no_such_connection`.
- **Order.** Check the new endpoint, bucket and prefix as `bind` does; probe the new identity
  with the connection's registered key and its region, `conditional_put` and `min_put_rate`;
  only then write. A failed probe answers 422 `probe_failed` with the steps and writes nothing.
  The key is the connection's: kbf signs with the one registered key whichever endpoint is
  registered, so a repoint carries no credential.
- **The write** is one state-store transaction that compare-and-sets `objstore/gen`, replaces
  the connection row's endpoint and the binding row's bucket and prefix, stamps both with the
  new generation, and appends the audit row. A lost race is 409 `conflict`, as for every write.
  The secret file is untouched. A repoint to the identity already registered writes only an
  `unchanged` audit row and leaves the generation.
- **Audit.** The row's action is `repoint`, its subject the connection name, and its `detail`
  is `<old endpoint> <old bucket>/<old prefix> -> <new endpoint> <new bucket>/<new prefix>`,
  with the generation the repoint made. No credential is in it. (A frontend of the previous
  version reading that row does not know the action and answers `corrupt_rows` to the audit
  list until it is replaced; every other read is unaffected. Frontends therefore update
  first. From this version on the reader tolerates an action code it does not know: the row
  lists with action `unknown (N)`, N being the code, and its subject and detail, so a later
  action cannot break the listing of readers of this release. Known actions keep their
  snake_case names in the answer.)
- **Who follows.** Only a node that takes its store from the registration (`--cold-store
  registered`) moves, at its next start: a running node keeps the store it opened and reports
  `pending` for the new generation, as it does for any change. A node that names a store URL
  on its own is unaffected beyond what it does today: it keeps running on its flag and reports
  `identity_mismatch` against the new registration, and a node with no keys of its own refuses
  to start on it. A repoint therefore changes nothing for such a node and is not a way to
  move it.
- **Unbind afterwards.** The binding's `updated_generation` is the repoint's, so nodes that
  applied an earlier generation do not count as using it until they restart onto the new
  store.
