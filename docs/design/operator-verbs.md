# Operator verbs: every verb, the route that does it, or why it stays local

Status: a reference table. Every route of `kbf-server` must have a row here by method and
path, and the node verbs (`:cordon`, `:drain`, `:uncordon`) one row each:
`crates/kbf-server/tests/route_docs.rs` fails otherwise (it does not check the gate column). The authority for
a built route is [../api.md](../api.md); for an open pull request, that pull request. This page
is the index: the verb an operator means, the one route that does it, and the gate it passes.

Labels: **[F]** read in code on this branch, **[V]** an open pull request, not on the branch,
**[N]** new in the object-store series ([object-store-registration.md](object-store-registration.md)),
**[D]** a decision.

The rule (distribution.md section 1): everything configurable is an API call first; `curl` is
the contract; the `kbf` CLI is a typed wrapper with no logic of its own. A verb that is not a
route stays local, and the last column says why.

**Gates** (the first column of routes below; the text is [../api.md](../api.md), "Who may write"):

- **open**: a read, answered to whoever reaches the address.
- **write**: loopback peer, no `Origin`, the token file, a Bearer token, and `Content-Type:
  application/json`. A release upload drops the content-type gate.
- **full**: the write gate on a read too, and `Cache-Control: no-store`.
- **policy**: the REAPI authentication policy and `invocations.getAuthorizer`; off (404) without
  them.
- **none**: health and metrics, which need no credentials.

## 1. Routes, one row each

Every row is one method and one path. A router `.route(` call that serves two methods has a row
per method; a custom method (`:cordon`) has a row per verb. The sources of the router are
`crates/kbf-server/src/{api,release/http,storage_health,health,metrics,http_blobs,
http_invocations,http_history,enroll/api}.rs` and `frontend.rs`, where they are merged.

### Cluster

| Verb | Method and route | Status | Gate |
|---|---|---|---|
| cluster status (nodes, software, placement) | `GET /v1/nodes` | [F] | open |
| list members of the storage map | `GET /v1/members` | [F] | open |
| cordon a node | `POST /v1/nodes/{node}:cordon` | [F] | write |
| drain a node | `POST /v1/nodes/{node}:drain` | [F] | write |
| uncordon a node | `POST /v1/nodes/{node}:uncordon` | [F] | write |
| set a member active or draining (join, leave) | `POST /v1/members/{id}` | [F] | write |
| deny an entry (serial, SPKI hash, node id) | `POST /v1/denied` | [F] | write |
| allow an entry again | `DELETE /v1/denied` | [F] | write |
| list the denied entries | `GET /v1/denied` | [N] unit F | write |
| frontend drain status | `GET /v1/frontend/drain` | [F] | open |
| begin the frontend's drain | `POST /v1/frontend/drain` | [F] | write |
| storage health (under-replicated blobs) | `GET /v1/storage` | [F] | open |

There is no rebalance verb: handoff of data between members is not built, and none is invented
here **[D]**.

### Enrollment

All of these answer 404 until enrollment is wired into the frontend (#1055 **[V]**).

| Verb | Method and route | Status | Gate |
|---|---|---|---|
| create an enrollment token | `POST /v1/enroll/tokens` | [F] | full |
| list tokens | `GET /v1/enroll/tokens` | [F] | full |
| revoke a token | `DELETE /v1/enroll/tokens/{id}` | [F] | full |
| list enrolled nodes | `GET /v1/enroll/nodes` | [F] | full |
| revoke an enrolled node | `POST /v1/enroll/nodes/{id}/revoke` | [F] | full |

### Object store

Designed in [object-store-registration.md](object-store-registration.md); unit C unless noted.
All answer 404 when the feature is off.

| Verb | Method and route | Status | Gate |
|---|---|---|---|
| register or replace a connection | `PUT /v1/object-stores/connections/{name}` | [N] | full |
| list connections | `GET /v1/object-stores/connections` | [N] | full |
| show a connection | `GET /v1/object-stores/connections/{name}` | [N] | full |
| delete a connection | `DELETE /v1/object-stores/connections/{name}` | [N] | full |
| probe a connection | `POST /v1/object-stores/connections/{name}:verify` | [N] | full |
| bind a role | `PUT /v1/object-stores/bindings/{role}` | [N] | full |
| unbind a role | `DELETE /v1/object-stores/bindings/{role}` | [N] | full |
| desired state and convergence | `GET /v1/object-stores` | [N] (convergence from unit D) | full |
| audit log | `GET /v1/object-stores/audit` | [N] | full |

### Releases and updates

| Verb | Method and route | Status | Gate |
|---|---|---|---|
| upload a release file | `PUT /v1/releases/{commit}/{platform}/{name}` | [F] | write, no content-type gate |
| list releases | `GET /v1/releases` | [F] | open |
| read the desired release, releases, policy | `GET /v1/update` | [V] #976, #985 | open |
| set the desired release (also a rollback: an older commit, a higher generation) | `POST /v1/update/desired` | [V] #976 | write |
| preview what a commit would change | `GET /v1/update/plan` | [V] #985 (design) | open |
| pause updates | `POST /v1/update/pause` | [V] #976 | write |
| resume updates | `POST /v1/update/resume` | [V] #976 | write |
| per-host update state | `GET /v1/update/hosts` | [V] #1007 | policy (`farm.getAuthorizer`) |
| the frontend's own update order, read | `GET /v1/update/frontend` | [V] #1062 | open |
| the frontend's own update order, write | `POST /v1/update/frontend` | [V] #1062 | write |

There is no separate rollback route. There is no update-policy route (`POST /v1/update/policy`
is named in the pull-updates design and has no code), no image-pull verb and no CA-rotation
route on this branch: all three are out of scope of the object-store series (Q5).

### Reads of builds

| Verb | Method and route | Status | Gate |
|---|---|---|---|
| a build (invocation) | `GET /v1/invocations/{id}` | [F] | policy |
| its action rows | `GET /v1/invocations/{id}/actions` | [F] | policy |
| one action's row | `GET /v1/invocations/{id}/actions/{digest}` | [F] | policy |
| its artifacts | `GET /v1/invocations/{id}/artifacts` | [F] | policy |
| its logs | `GET /v1/invocations/{id}/logs` | [F] | policy |
| its metrics | `GET /v1/invocations/{id}/metrics` | [F] | policy |
| list invocations | `GET /v1/invocations` | [F] | policy |
| list shapes | `GET /v1/shapes` | [F] | policy |
| list tests | `GET /v1/tests` | [F] | policy |
| one shape | `GET /v1/shapes/{key}` | [F] | policy |
| a shape's recent runs | `GET /v1/shapes/{key}/runs` | [F] | policy |
| one run | `GET /v1/runs/{id}` | [F] | policy |
| a run's logs | `GET /v1/runs/{id}/logs/{stream}` | [F] | policy |
| a blob's bytes | `GET /v1/blobs/{digest}` | [F] | policy |
| outcomes of shapes across builds | `GET /v1/grid` | [V] #891 | policy |
| runs of an action digest | `GET /v1/actions/{digest}/runs` | [V] #891 | policy |
| runs on a node in a window | `GET /v1/runs` | [V] #891 | policy |

### Health

| Verb | Method and route | Status | Gate |
|---|---|---|---|
| liveness | `GET /healthz` | [F] | none |
| readiness | `GET /readyz` | [F] | none |
| metrics | `GET /metrics` | [F] | none |

The Mac device manager is a separate service, not the operator API (`crates/kbf-mdm`: `GET
/v1/macs`, `GET /v1/macs/{serial}`, `POST` on `.../enforce`, `.../withdraw`, `.../profile`,
`.../grant-admin`, `.../erase` and `.../bring-forward`). It leaves the product (distribution.md
section 8) and is not an operator verb of the farm.

## 2. Verbs that are not routes

| Verb | Where it lives | Stays local because |
|---|---|---|
| apply a registered object store on a storage node | the node's reconcile at start (unit E) | no operator verb: the node reconciles by itself; in v1 a rotation reaches a running node at its restart |
| restart a node so it picks up a rotation | the installer (`kbf-installer`, #1048, #1051) | kbf never restarts itself; the installer verb for it does not exist yet, so a rotation ends with the operator waiting on `pending` (a cluster write, held) |
| frontend state backup | none: Postgres is backed up by the installer | no route and no flag |
| meta-backup, meta-verify | `kbf-deploy linux` **[F]** | briefly stops `kbf-server`; installer or root only |
| install, update, roll back, confirm, revert, gc, status and check of a host | `kbf-installer` (#1048, #1051, #1082, #1083) **[V]** | root and host-local; the installer is the root tool, and `setup` mints the first token |
| the rollout controller | `kbf-deploy rollout`, #803, #970 **[V]** | a host state machine; installer territory |
| start a mode, `--check`, check-args | `kbf <mode>` **[F]** | the local runtime; it installs nothing |
| frontend handover, enroll, `kbf node` | `kbf frontend handover`, `kbf enroll`, `kbf node` **[F]** | replaced by the installer's side-by-side children (#1054, #1062) and removed by #1049 **[V]** |
| render, check, install units, swap, verify certificate; envoy render, check, apply | `kbf-deploy` **[F]** | become installer logic and package units; root |
| `mdm`, `minio`, `pilot`, `erase-request` | `kbf-deploy` **[F]** | leave the product (distribution.md section 8); an erase request is signed with a security key on the operator's machine |
| `kbf cluster status`, `drain`, `undrain`, `member`, `denied`, `object-store status\|audit\|connection\|bind\|unbind\|verify` | the `kbf` CLI **[F]** (the last two groups: unit G1) | a thin client of the routes above, no logic |
| frontend log tail | none | cut (Q4) |
| update policy, image pull | none | no source verb exists (Q5) |
| rotate the CA | its own unit | out of the object-store series (Q5) |
