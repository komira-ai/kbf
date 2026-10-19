# `kbf-installer deploy`: the gamma trigger

Status: built in `crates/kbf-installer/src/deploy/`. It drives the update API the frontend's
proxy serves (`docs/design/frontend-proxy.md`, sections 4, 5 and 9), frontend first.

`kbf-installer deploy --commit <40hex> --apply [--json] [--bundle -]` runs on the frontend host, as root, and drives the frontend's public port through one release.
It orders no node, sets no desired release and rolls nothing back: it asks the update API to
swap the frontend, waits for that update to be `done`, and then watches the nodes, which the
new frontend rolls (one storage node at a time) once it has baked.

## One address, over TLS

Every call of a deploy goes to one address, the frontend's public port, which the proxy owns
(`docs/design/frontend-proxy.md`, section 2). The proxy serves the update API
(`/v1/update/frontend*`) itself and passes every other `/v1/*` call (the release store, the
node and member lists, `GET /v1/update`) to whichever child is active, so the address does
not change when the children swap. It speaks TLS only, so the client is HTTP/1.1 over TLS,
one request per connection, with the operator token as the bearer token on every request.

The certificate is verified, always: under the name it carries (`--api-server-name`; without
one, the address's IP, which a certificate must then carry), against the system's roots and
the CA certificates of the PEM file `--api-ca`, if given. A host whose certificate comes from
the farm's own CA names that file; a host with no root at all (no system roots, no file)
refuses to start. The token is sent only to a server that has proved the name.

`--api <127.0.0.1:port>`, `--api-ca <pem>`, `--api-server-name <dns>` and `--token-file <path>`
default to the `api`, `api_ca`, `api_server_name` and `token_file` keys of
`/etc/kbf-installer/deploy.json` (`--config`), so a caller that may only run `deploy` names
none of them. The config must be a regular file nobody else may write (and root-owned); it is not
read when the flags give every key it could (`api`, `api_ca`, `api_server_name`, `token_file`,
`cas`, `cas_ca`, `cas_server_name`). The token file must be a regular file, mode 0600 or 0400.
The address must be loopback: a deploy drives the proxy of its own host. Run through `sudo` (the
environment carries `SUDO_UID`, set to the caller's uid), `deploy` refuses `--api`, `--api-ca`,
`--api-server-name`, `--token-file`, `--config`, `--lock`, `--platform` and the `--cas*` flags
with exit 2: the caller would choose where root sends the token and which file it reads. Those
values then come only from the root-owned config. Scope the sudoers rule to the exact argument
list too, with no wildcard: `deploy --apply --json --commit` followed by forty `[0-9a-f]`
classes, then `--bundle -`.

The token in the token file is the operator token the proxy checks on every route but its
probes. Where `setup` puts that file is the proxy's design (`docs/design/frontend-proxy.md`,
section 4, "Auth"), not this verb's.

A farm whose `/v1/nodes` lists no node (an empty list, or no `nodes` array) is refused in
preflight (exit 3, `no_nodes`); the wait for nodes and the post check each need at least one
node, so an empty list never reads as "every node confirmed".

## Steps

Each is a named entry in the summary's `steps`, with its duration.

1. `bundle`: with `--bundle -`, a tar on standard input holding
   `<name>-<commit>-<platform>.tar.gz` and the same plus `.attestation`, for `kbf` and
   `kbf-daemon` only. Any other entry is exit 2 before an upload. Each file is
   `PUT /v1/releases/{commit}/{platform}/{name}`; other bytes under a stored key is 409, exit 3.
   This is bytes, not trust: every node and the frontend's own update verify the
   attestation offline.
2. `preflight`: the release is complete (every component attested) for the platform of every
   node and for the host's own; no node is offline (unless `--allow-offline`); the installer's
   own check passes; no frontend update is going on (`GET /v1/update/frontend` shows none, or
   one whose phase is `done`, `failed` or `rolled_back`). The nodes it saw are the nodes the
   deploy waits for in step 5.
3. `baseline`: the desired commit and generation before the deploy, in `previous_desired`.
4. `frontend`: `POST /v1/update/frontend {commit, request_id, nodes: true, rollback: false}`,
   then `GET /v1/update/frontend` until the update the POST named is over (`--handover-timeout-s`,
   default 3600: it covers the new frontend's bake).
   - **One request id per run**, generated from the system's random source, carried by every
     retry. A POST that did not get through (a failed connection, `429`, `5xx`, an answer lost
     after the API had accepted it) is made again every five seconds with the same id, and the
     API answers it with the update it started the first time, so a retry starts no second
     update. A new run has a new id.
   - **Only our update counts.** The status shows one update at a time, the last one after it
     ended. A status of another update (the one before, or one a person started) is not this
     deploy's verdict, whatever its phase.
   - **Only `done` is success.** Any earlier phase is an update still going on. `failed` and
     `rolled_back` are failures (exit 1, `frontend_failed`, `frontend_rolled_back`) whose
     detail and `frontend.failure` carry the step and the reason the API gave. A status that
     cannot be had right now is asked for again; a `200` that is not an update status stops
     the deploy (`api_error`). The deadline names the phase the update was in
     (`frontend_timeout`).
   - **Answers to the POST that end the deploy:** `409 incompatible` is a refusal (exit 3);
     `422` (the release is not complete for the platform) is a refusal (`release_incomplete`);
     `409 update_open`, `old_child_draining`, `installer_busy` or any other `409` is busy (exit 4);
     `409 state_lost`, `request_id_reused`, `request_id_retired` and any other status are
     failures (`api_error`). `409 same_commit` (the frontend already runs the commit) is no
     update: the step records `already_running` and the deploy goes on to the nodes, so a run
     repeated after a node was fixed can finish.
5. `nodes`: the new frontend offers the commit to the nodes in the `nodes` phase of its update
   (`nodes: true`); its controller rolls them. The deploy sets nothing: it reads
   `GET /v1/update` until the desired release is the commit (a node's budget; otherwise
   exit 1, `nodes_not_offered`, saying what the desired release is), then polls `GET /v1/nodes`
   until each node's `update.state` is `confirmed` on the commit. Each node has its own budget
   (`--node-timeout-s`, default 1800), restarted when a node confirms. The first node that
   reverts, is refused, is unsupported or times out ends the deploy; the nodes not reached are
   `pending`. A state left by an older offer (a lower `offered_generation`) is not this
   deploy's verdict. **Every node the preflight saw is waited for, listed or not**: the new
   frontend lists only the nodes that have reconnected to it, so a list of confirmed nodes
   that lacks one of them is a roll still going on (a node that is not listed by the end of its
   budget is `timed_out`, with the platform the preflight knew; one named in `--allow-offline`
   is `offline`).
6. post checks, all run even after one fails: every storage member `active` (up to 120 s),
   every node connected, `serving`, `up_to_date` and on the commit and the server on it,
   `GET /readyz`, and the CAS smoke.

Without `--apply` steps 1 to 3 run read-only (the bundle is checked, not uploaded) and the
result is `plan`.

The order of the old nodes-first deploy (`--order`, and a `POST /v1/update/desired` made by
the deploy) is gone: the desired release is the new frontend's to set, after it has baked,
and a post of it to the old frontend would be refused when the commit is not that frontend's
own. A caller that still sends `--order` gets exit 2.

## Exit codes

0 deployed and all post checks green, or a plan. 1 failed (`node_not_confirmed`,
`node_offline`, `nodes_not_offered`, `frontend_failed`, `frontend_rolled_back`,
`frontend_timeout`, `postcheck_failed`, `upload_failed`, `api_error`). 2 usage or missing
prerequisite (no token file, not root with `--apply`, bad bundle, a CA file with no
certificate, no random source). 3 refused (`release_incomplete`, `incompatible`,
`upload_conflict`, `node_platform_unknown`, `check_failed`, `no_nodes`). 4 busy (lock held,
`frontend_busy`). 5 needs an operator: the CAS smoke did not run (`smoke_skipped`). A skipped
check never reads as a pass.

## Summary

One JSON line on standard output, nothing else there; log lines go to standard error. It holds
the commit, the previous desired commit and generation, the new generation, `result`, `exit`,
`reason`, `detail`, times, `steps`, `platforms`, `nodes` (`id`, `platform`, `state`,
`running_commit`, `secs`), `frontend` (`update_id`, the last `phase` seen, `failure` with
`step` and `reason` when the update ended `failed` or `rolled_back`, `already_running`), and
`post`. It holds no token, no token path and no host name. `schema` is 2: the `frontend` of
schema 1 was an order (`order_id`, `state`).

## What it assumes

- The proxy answers `POST /v1/update/frontend` 202 (or 200) with `{"update_id", "phase"}`, and
  `GET /v1/update/frontend` with the status of `docs/design/frontend-proxy.md` section 4. The
  client reads them with the API's own types (`UpdateRequest`, `StatusBody`), so a field
  renamed on one side is a compile or a test failure and not a deploy that waits for ever. It
  ignores fields it does not know.
- `GET /v1/update` answers `{"desired": {"commit", "generation"}}` from the active child.
- `GET /v1/nodes` carries, per node, `update.{state, running_commit, refusal,
  offered_generation, up_to_date, availability}` and the node's platform as `update.platform`
  (or `software.os` and `software.arch`).
- The lock is `/run/kbf-installer/lock` (`--lock`).
- Rolling back a frontend update, aborting one, and a downgrade (`rollback: true`) are the
  update API's own routes; `deploy` has no flag for them.

## The CAS smoke

Where it dials, first that applies: `--cas http(s)://host:port`; `cas` in the host config
(`deploy.json`, which `sudo` callers cannot override); else the frontend's own config. That is
`/etc/kbf/kbf.toml` (`[listener] listen`, default 127.0.0.1:8980; `[tls] reapi-tls-cert`), then the
flags of the first installed `kbf-frontend@.service`, `kbf-all-in-one.service` or
`kbf-server.service` that sets `--listen` (a flag overrides its key). An unspecified listen
address is dialled on loopback, a specific one at that address. A configured certificate makes
the address `https://`, verified under the certificate's first DNS name (its common name if none);
`cas_server_name` / `--cas-server-name` sets another name. An `https://` address is verified
against the system roots plus the PEM file `cas_ca` / `--cas-ca`, if given. With no address from
any of these the smoke is `skipped` and the deploy ends exit 5. A plan (no `--apply`) reads none
of this. Instance b of the frontend template (ports ten above) is not derived: the unit's own
`--listen` is read. On a host with the proxy this is the proxy's public port, the address
`--api` names, and the file `--cas-ca` names is usually the file `--api-ca` names.

The smoke writes 64 random bytes with `ByteStream.Write` (the commit must report their size),
asks `FindMissingBlobs` for that blob and a random one nobody wrote (present, missing), reads the
blob back and compares every byte, then asks the action cache for a random digest (must be
NOT_FOUND; a hit or another status fails). One bound for all of it, `--smoke-timeout-s`
(default 60). An unreachable listener or any failed check is `postcheck_failed`, exit 1. The
instance name is empty. The blob stays in the store until eviction.

## What is not built

- The installer's `check` (with the Envoy range). Until it exists the summary has
  `check_skipped: true`.
- A bundle directory for `--bundle`; only `-` is read.
- A run of the whole deploy against the real proxy and the real children. The deploy's tests
  play the API (a fake that answers with the API's own types, served over TLS from a test CA).
  The client's transport is proved against the real proxy by `crates/kbf-it/tests/deploy_through_proxy.rs`
  (the update route, the forwarded `/v1/*` calls, a three MiB upload, a wrong token); the run
  of the steps with the real proxy waits for the update API to be served on the proxy's update route.
