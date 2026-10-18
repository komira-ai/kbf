# `kbf-installer deploy`: the gamma trigger

Status: built in `crates/kbf-installer/src/deploy/`; the routes it calls are specified in
`docs/api.md` and `docs/design/pull-updates.md` section 6.3, and `POST /v1/update/frontend`
and `GET /v1/update` are not yet served by `kbf-server` (see "What it assumes").

`kbf-installer deploy --commit <40hex> --api 127.0.0.1:<port> --token-file <path> --apply`
runs on the frontend host, as root, and drives the local operator API through one release.
It orders no node and rolls nothing back: the frontend offers the desired release and the
drain system rolls the nodes, one storage node at a time.

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

## What is not built

- The CAS smoke binary (`kbf-cas-smoke`). Until it exists the smoke is `skipped` and a deploy
  ends exit 5.
- The installer's `check` (with the Envoy range). Until it exists the summary has
  `check_skipped: true`.
- The in-process fallback for `POST /v1/update/frontend`.
- A bundle directory for `--bundle`; only `-` is read.
