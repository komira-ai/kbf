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
caller's uid), `deploy` refuses `--api`, `--token-file`, `--config`, `--lock`, `--platform`, `--result`, `--observe`, `--observe-base-port` and `--observe-token-file`
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

## Gates and the result document

An applied deploy is judged by seven gate rows (six gates) and writes them, with the
deploy's own outcome, to a `kbf-gate` result document: `--result <path>` (a host flag, like
`--api`: refused through `sudo`; the config's `result` key; default
`/var/lib/kbf-installer/deploy-result.json`). The document is written `RUNNING` before the
first request and `FINISHED` before `deploy` returns; a document that cannot be written ends
the deploy first (`result_unwritable`). `--set-hash <64hex>` is the hash of the release set
(`SHA256SUMS`); without it the document carries a hash of what `GET /v1/releases` lists for
the commit. `--settle-timeout-s` (default 600) is gate E's window.

**Where the gates read.** Either `--observe <127.0.0.1:port>` (one frontend that does not move)
or `--observe-base-port <N>` (the children of the frontend proxy: slot A listens from `N`, slot B
from `N + 10`, the operator listener is at offset 2 of a slot) is required with `--apply`
(exit 2 without one, or with both; host flags; config keys `observe`, `observe_base_port`).
`--observe-token-file` (host flag; key `observe_token_file`) defaults to the `--token-file`
token: one operator token guards both. The gates do not read `--api`: the public proxy answers
`/metrics` itself (its own `kbf_proxy_*` counters, not the frontend's) and serves the update
routes, and no `/v1` path carries `kbf_frontend_relay_untraced` or the cache gauges, so only
the serving child's operator listener can be read. Release and update calls go only to `--api`.
Both addresses must be loopback.

With `--observe-base-port` the deploy follows the handover. It resolves the serving child at
the preflight and again after every frontend step: the slot `GET /v1/update/frontend` names in
`active_slot` (present while that update is known), else the only child that answers
`/metrics`; none answering, or both with no slot named, ends the deploy (`observe_unresolved`).
The result document's `observed` lists `<step>: <child>` each time the child read changes
(for example `preflight: slot a 127.0.0.1:18002`, `frontend: slot b 127.0.0.1:18012`).
[A] Before any update has run on a host the API names no slot, so the preflight probes; a host
where both children are left running after an earlier swap needs the older one stopped.

`/metrics` is scraped from `--observe` at the preflight, after `desired`, at every
poll of the node wait, after the frontend step and at the post checks. A measurement that is
not there is red: a scrape that fails or cannot be parsed, a series that is missing, a
listing that is cut off. The first red ends the deploy (`gate_red`; exit 1, or 3 in the
preflight where nothing has changed) and the detail names the gate, the value and the host step.

| Gate row | Reads | Red when |
|---|---|---|
| `a_inflight_ids` | `GET /v1/invocations?state=open` at the preflight, at each node's confirmation and after `desired` and the frontend; then `?since=&until=` over the whole deploy | a listing cannot be read (also when the route is off: it needs `--history-objects`) or is partial |
| `b_relay_untraced` | `kbf_frontend_relay_untraced` at every scrape | any scrape is above 0 or lacks the series |
| `c_farm_class_failures` | every recorded id's `/actions` pages, after the deploy (also after a failed one) | an attempt ended `infra`, `lost` or `lost_*` (client-side filter), an id cannot be read, or its record is `rows_capped` (not every attempt is visible) |
| `d_lost_blobs`, `d_blob_losses_delta` | `kbf_cache_lost_blobs`; `kbf_cache_blob_losses_total` minus its first scrape | any scrape is above 0 or lacks the series; the counter went backwards |
| `e_under_replicated` | `kbf_under_replicated_blobs` | not 0 at the preflight (a farm that reports no series at all is refused there as `gate_unjudgeable`, exit 3: it needs storage members that have reported, as the post check on `/v1/members` already does); not back to 0 within the settle window after a node's step; the next node's step began (`activated_unconfirmed`) or confirmed before that |
| `f_auto_revert` | node states and the frontend order | a node or the frontend reverted by its own timer |

The deploy never orders a node, so gate E cannot hold the drain system's next step; it
watches and fails the deploy when the next step ran before the repair.

The one-line summary on standard output stays beside the document until the callers read the
document instead, and is removed on 2026-12-01. Exit 0 is a `green` document, exit 5
(`smoke_skipped`) is `extended` (no decision, not a pass), every other exit is `red`.

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
