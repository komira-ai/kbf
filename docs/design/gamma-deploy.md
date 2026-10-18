# `kbf-installer deploy`: the gamma trigger

Status: built in `crates/kbf-installer/src/deploy/`; the routes it calls are specified in
`docs/api.md` and `docs/design/pull-updates.md` section 6.3, and `POST /v1/update/frontend`
and `GET /v1/update` are not yet served by `kbf-server` (see "What it assumes").

`kbf-installer deploy --commit <40hex> --apply [--json] [--bundle -]` runs on the frontend host, as root, and drives the local operator API through one release.
It orders no node and rolls nothing back: the frontend offers the desired release and the
drain system rolls the nodes, one storage node at a time.

`--api 127.0.0.1:<port>` and `--token-file <path>` default to the `api` and `token_file` keys of
`/etc/kbf-installer/deploy.json` (`--config`), so a caller that may only run `deploy` names
neither. The config must be a regular file nobody else may write (and root-owned); it is not
read when both flags are given. The token file must be a regular file, mode 0600 or 0400.
The address must be loopback. Run through `sudo` (the environment carries `SUDO_UID`, set to the
caller's uid), `deploy` refuses `--api`, `--token-file`, `--config`, `--lock` and `--platform`
with exit 2: the caller would choose where root sends the token and which file it reads. Those
values then come only from the root-owned config. Scope the sudoers rule to the exact argument
list too, with no wildcard: `deploy --apply --json --commit` followed by forty `[0-9a-f]`
classes, then `--bundle -`.

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
   own check passes; no frontend update is in flight.
3. `baseline`: the desired commit and generation before the deploy, in `previous_desired`.
4. `desired`: `POST /v1/update/desired {commit}`.
5. `nodes`: poll `GET /v1/nodes` until each node's `update.state` is `confirmed` on the commit.
   Each node has its own budget (`--node-timeout-s`, default 1800), restarted when a node
   confirms. The first node that reverts, is refused, is unsupported or times out ends the
   deploy; the nodes not reached are `pending`. A state left by an older offer (a lower
   `offered_generation`) is not this deploy's verdict.
6. `frontend`: `POST /v1/update/frontend {commit}`, then poll until our order is `confirmed`
   (`--handover-timeout-s`). `reverted` leaves the old instance serving. Requests that fail
   during the swap are retried.
7. post checks, all run even after one fails: every storage member `active` (up to 120 s),
   every node connected, `serving`, `up_to_date` and on the commit and the server on it,
   `GET /readyz`, and the CAS smoke.

`--order frontend-first` runs step 6 before step 4. The default is nodes first, as the
design for this verb says; `kbf-cicd.md` section 0 and `pull-updates.md` section 8 order an
upgrade frontend first, so choose by the release's protocol facts until the verb reads them.

Without `--apply` steps 1 to 3 run read-only (the bundle is checked, not uploaded) and the
result is `plan`.

## Exit codes

0 deployed and all post checks green, or a plan. 1 failed (`node_not_confirmed`,
`node_offline`, `frontend_reverted`, `frontend_timeout`, `postcheck_failed`, `upload_failed`,
`api_error`). 2 usage or missing prerequisite (no token file, not root with `--apply`, bad
bundle). 3 refused (`release_incomplete`, `upload_conflict`, `node_platform_unknown`,
`check_failed`). 4 busy (lock held, `frontend_busy`). 5 needs an operator: the CAS smoke did
not run (`smoke_skipped`). A skipped check never reads as a pass.

## Summary

One JSON line on standard output, nothing else there; log lines go to standard error. It holds
the commit, the previous desired commit and generation, the new generation, `result`, `exit`,
`reason`, `detail`, times, `steps`, `platforms`, `nodes` (`id`, `platform`, `state`,
`running_commit`, `secs`), `frontend` (`order_id`, `state`), and `post`. It holds no token, no
token path and no host name.

## What it assumes

- `GET /v1/update` answers `{"desired": {"commit", "generation"}, "frontend": {"order_id",
  "state"}}` with `state` one of `pending`, `activating`, `confirmed`, `reverted`; and
  `POST /v1/update/frontend` answers 200 to 202 with `{"order_id"}`, or 409 when busy. These
  two are the contract the frontend update has to meet; neither is served yet.
- `GET /v1/nodes` carries, per node, `update.{state, running_commit, refusal,
  offered_generation, up_to_date, availability}` and the node's platform as `update.platform`
  (or `software.os` and `software.arch`).
- The lock is `/run/kbf-installer/lock` (`--lock`).

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
`--listen` is read.

The smoke writes 64 random bytes with `ByteStream.Write` (the commit must report their size),
asks `FindMissingBlobs` for that blob and a random one nobody wrote (present, missing), reads the
blob back and compares every byte, then asks the action cache for a random digest (must be
NOT_FOUND; a hit or another status fails). One bound for all of it, `--smoke-timeout-s`
(default 60). An unreachable listener or any failed check is `postcheck_failed`, exit 1. The
instance name is empty. The blob stays in the store until eviction.

## What is not built

- The installer's `check` (with the Envoy range). Until it exists the summary has
  `check_skipped: true`.
- The in-process fallback for `POST /v1/update/frontend`.
- A bundle directory for `--bundle`; only `-` is read.
