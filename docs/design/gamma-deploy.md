# `kbf-installer deploy`: the gamma trigger

Status: built in `crates/kbf-installer/src/deploy/`. It drives the update API the frontend's
proxy serves (`docs/design/frontend-proxy.md`, sections 4, 5 and 9), frontend first.

`kbf-installer deploy --commit <40hex> --stage gamma|prod --offer offer.json|- --apply [--json] [--bundle -]` runs on the frontend host, as root, and drives the frontend's public port through one release.
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

The system's roots are what the platform's loader finds, which includes the file `SSL_CERT_FILE`
and the directory `SSL_CERT_DIR` name when the environment has them. A deploy run as root
must therefore not inherit those variables: `sudo`'s default `env_reset` drops them, and a
sudoers rule for `deploy` must not list them in `env_keep`. Anyone who can set them for root
chooses which servers root sends the token to. (The `cas` calls take their roots the same way.)

`--api <127.0.0.1:port>`, `--api-ca <pem>`, `--api-server-name <dns>` and `--token-file <path>`
default to the `api`, `api_ca`, `api_server_name` and `token_file` keys of
`/etc/kbf-installer/deploy.json` (`--config`), so a caller that may only run `deploy` names
none of them. The config must be a regular file nobody else may write (and root-owned); it is not
read when the flags give every key it could (`api`, `api_ca`, `api_server_name`, `token_file`,
`cas`, `cas_ca`, `cas_server_name`). The token file must be a regular file, mode 0600 or 0400.
The address must be loopback: a deploy drives the proxy of its own host. Run through `sudo` (the
environment carries `SUDO_UID`, set to the caller's uid), `deploy` refuses `--api`, `--api-ca`,
`--api-server-name`, `--token-file`, `--config`, `--lock`, `--platform`, `--result`, `--observe`,
`--observe-base-port`, `--observe-token-file`, the `--cas*` flags, `--stage`, `--no-offer` and
`--offer-max-age-h` with exit 2: the caller would choose where root sends the token and which file it reads. Those
values then come only from the root-owned config. Scope the sudoers rule to the exact argument
list too, with no wildcard: `deploy --apply --json --commit` followed by forty `[0-9a-f]`
classes, then `--offer -` and `--bundle -`. `--offer` is the one flag a sudo caller may give, and
only as `-`: the offer is the `offer.json` in the bundle (see "The offer"), so the caller names no
file for root to read, and any other value is exit 2. `HOST_FLAGS` in `deploy/cli.rs` is the list
of flags refused there (public, so that what builds the argument list can test it holds none).

The token in the token file is the operator token the proxy checks on every route but its
probes. Where `setup` puts that file is the proxy's design (`docs/design/frontend-proxy.md`,
section 4, "Auth"), not this verb's.

A farm whose `/v1/nodes` lists no node (an empty list, or no `nodes` array) is refused in
preflight (exit 3, `no_nodes`); the wait for nodes and the post check each need at least one
node, so an empty list never reads as "every node confirmed".

## The host's config: who writes it, and the sudoers rule

`kbf-installer setup` (frontend and all-in-one) writes `/etc/kbf-installer/deploy.json` from
`--deploy-stage gamma|prod` and, only where the defaults are wrong, `--deploy-api`,
`--deploy-api-ca`, `--deploy-server-name` and `--deploy-token-file`
(`docs/deploy/kbf-installer-setup.md`). The file is the type `deploy` reads
(`deploy::host_config::HostConfig`), so what `setup` writes is what `deploy` gets back. It
holds the token and the CA by path only; both files are the operator's, and `setup` refuses
(exit 5, the command that fixes it in the message) when one is missing or unsafe. It never
creates or copies the token.

The sudoers rule is a held step for the person who owns the host's accounts; no code writes
it. Install it with `visudo -f /etc/sudoers.d/kbf-deploy` (mode 0440, root:root), as one line
with no wildcard, where `ACCOUNT` is the deploy account and `[0-9a-f]` is repeated forty times
for the commit:

```
ACCOUNT ALL=(root) NOPASSWD: /usr/bin/kbf-installer deploy --apply --json --commit [0-9a-f]...[0-9a-f] --offer - --bundle -
Defaults!/usr/bin/kbf-installer env_reset
```

The offer and the release travel in one tar on standard input (`--offer -` reads the offer from
it), so the pipeline writes no file on the host: there is no path for the rule to pin and no
second command that could write one.

Nothing else is allowed through `sudo`: the flags `deploy` refuses there (`--api`, `--token-file`,
`--config`, `--stage`, `--no-offer`, ...) are exactly the ones that would let the caller choose
where root sends the token, and the rule does not list `SSL_CERT_FILE` or `SSL_CERT_DIR` in
`env_keep`.

## The offer

`--offer offer.json` is the document `cargo xtask release offer` writes (`kbf_gate::OfferDoc`).
It is a record, not a command: the deploy does not act on what it says, it refuses unless the
release it is about to move is the one the offer is for. Nothing is taken from a file name.

- **Where the offer is**: a file (`--offer offer.json`, read as a regular file of at most 64 KiB,
  opened once without following a link and read through that descriptor), or the bundle's own
  (`--offer -`, which needs `--bundle -`, and is the only form `sudo` allows). In the bundle it
  is the entry named exactly `offer.json`: one entry, not empty, at most 64 KiB, read and not
  uploaded (`Offer.json` and `offer.json.attestation` are no release file, so they are exit 2
  as any stray entry is). An `offer.json` in a bundle given to any other deploy (a file's offer,
  `--no-offer`) is refused too, as a bad bundle, exit 2: an offer that is there but not the one
  judged is not ignored. Either way it is parsed only through the validated `OfferDoc` parse,
  and every check below is the same. A bundle with no such entry is refused as `offer_missing`
  and one whose entry is not an offer as `offer_unreadable`; neither repeats what the entry
  held. The bundle's offer can be read only once the bundle is, so the `offer` step passes for
  it, and its record in the summary and its stage, commit and age checks (`offer_wrong_stage`,
  `offer_wrong_commit`, `offer_future`, `offer_stale`) happen in the `bundle` step.
- **Which stage this host is** (`--stage gamma|prod`, else the `stage` key of
  `/etc/kbf-installer/deploy.json`): the offer's `target` must be it. The stage is the host's
  own, never read from the offer and never given by a `sudo` caller, so an offer for prod
  cannot be used to deploy a gamma host or the reverse. An `--apply` with no stage is exit 2
  (it would skip the rule below); a plan with none checks no offer.
- **No offer**: a deploy that names a stage and gives no `--offer` is refused (exit 3,
  `offer_required`) unless `--no-offer` says so. `--no-offer` is logged on standard error and
  recorded in the summary (`offer.kind: "waived"`); it is for a test farm.
- **The commit**: the offer's `commit` is `--commit` (`offer_wrong_commit`).
- **The set**: with an offer the bundle must carry `SHA256SUMS` (one entry, not uploaded). Its
  SHA-256 is the offer's `set_hash` (`offer_set_mismatch`), and every tarball in the bundle is
  the file those sums name, by its bytes (`offer_file_mismatch`). A bundle with no
  `SHA256SUMS`, or no bundle, refuses (`offer_set_unverified`): the deploy cannot say which set
  it moves. Attestation bundles are not in the sums; they are verified offline downstream.
- **The `meta_log`**: the `kbf` of the host's platform in the bundle is first attested offline
  (the installer's pinned `gh` and trust root, the flags the stage uses, on copies in a scratch
  directory; a tarball with no attestation in the bundle, or one that does not verify, is
  `offer_set_unverified` and nothing of it is unpacked or run). Only then is it unpacked to a
  scratch directory under the installer's work directory and run as `kbf --version --json`, as
  the installer's `RUN_AS` user (never root; as itself in a plan). Its `meta_log` must equal the
  offer's, `null` included both ways: a binary naming none where the offer claims one, or the
  reverse, is refused (`offer_meta_log_mismatch`). This closes the offer's `meta_log` being
  supplied by an operator as files. The answer can only refuse a deploy.
- **A plan checks the same.** Without `--apply` the set, the files, the attestation and the
  probe all run (they only read and write a scratch directory, and run an attested binary as
  the unprivileged user); only the upload and the update are skipped. A dry run therefore
  cannot say OK for a bundle the apply would refuse.
- **Reversibility**: an offer that claims `reversible: true` is asked of the build this host
  runs now, for a file's offer and the bundle's alike. The host runs
  `<current release>/kbf --version --json` as the installer's `RUN_AS` user (never root; as
  itself in a plan) and takes its `meta_log`; the claim holds only if the offer's `meta_log` is
  at most that number. The host's answer is recorded as `offer.host_reversible`: `true` where
  it holds, `false` where the running build is below, names none or does not answer (unknown
  is not reversible). This never refuses a deploy: a release is deployed whether or not it can
  be rolled back, because holding it would hold the next one too (it makes the same claim
  against the same host). The bake's hand-off and the release notes read `host_reversible`,
  not the offer's `reversible`, to know whether a rollback is possible. An offer that claims
  `reversible: false` is not asked: the probe is not run, and the summary has no
  `host_reversible`. The offer's `previous` and `previous_meta_log` stay the author's word:
  they are recorded, never checked against the farm.
- **What the offer does not prove.** It is an unsigned record. `host_reversible` reads the build
  of the frontend host only: a node still at another commit than the host's `current` one is
  not covered, so the bake must not treat `reversible` as proof that a rollback works on every
  node. Only the host platform's `kbf` is attested at deploy
  time. The other platforms' tarballs are bound only by `SHA256SUMS`, and `SHA256SUMS` only by
  the offer's `set_hash`, which is itself unsigned; the frontend's offline attestation check
  at update time is the backstop for them.
- **Age**: `created` may be at most `--offer-max-age-h` hours old (default 72;
  `offer_stale`) and at most 300 s ahead of the host's clock (`offer_future`).
- **Platforms**: every platform the farm runs (the nodes' and the host's) is one the offer found
  complete (`offer_platform`).

All of them but the platforms are checked before the first upload, in the steps `offer` (stage,
commit, age of a file's offer) and `bundle` (a bundle's offer, set, files, `meta_log`, and the
host's word on `reversible`, which refuses nothing). The platforms are checked in `preflight`,
which runs after the bundle's files are stored: an `offer_platform` refusal (exit 3) comes after
the uploads, which only add files under the commit's key. Every other offer refusal is exit 3 and
the store is untouched. The summary's `offer` object carries the offer's `commit`, `set_hash`,
`created`, `reversible`, `previous`, `meta_log` and `previous_meta_log`, copied from the
validated document, and the host's own `host_reversible` where the offer claims `reversible`,
for the bake's hand-off file; the deploy does not write that file itself. Not built: the offer
does not yet bound the generation (`expect_generation`), which stays with the update API.

## Steps

Each is a named entry in the summary's `steps`, with its duration.

1. `bundle`: with `--bundle -`, a tar on standard input holding
   `<name>-<commit>-<platform>.tar.gz` and the same plus `.attestation`, for `kbf` and
   `kbf-daemon` only, and the two entries read and not uploaded: `SHA256SUMS` and, for
   `--offer -`, `offer.json`. Any other entry is exit 2 before an upload. Each file is
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
     deploy's verdict, whatever its phase. So if a person aborts our update and starts another,
     the deploy cannot tell that ours ended: it reads on until the deadline (`--handover-timeout-s`),
     and `frontend_timeout` then names the update the API shows, next to the phase ours was last
     seen in. It does not give up sooner because a status that has not caught up with the
     POST looks the same.
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
(for example `preflight: slot a 127.0.0.1:18002`, then `frontend: slot b 127.0.0.1:18012; also slot a 127.0.0.1:18002`).

**The child that handed over is read too.** It is the one that reports `kbf_frontend_relay_untraced` (set by `Farm::follow` on the fenced farm), and the successor never does; it keeps draining after the swap. So at every scrape after a handover the other slot is scraped as well, when it answers: gate B, `kbf_cache_lost_blobs` and the loss counter apply to it, and a red names it (`[read from slot a …]`). One that does not answer has finished and is not waited for. `kbf_cache_lost_blobs` and `kbf_cache_blob_losses_total` are folded from the metadata log (`MetaState`), so on the shared Raft log both children report the same numbers; an in-memory log begins again in a new process. The loss counter is therefore judged per child: a child's baseline is the deploy's first reading while its own reading is not below it (a shared log: a loss across the handover shows), else its own first reading (a fresh process); a counter that falls within one child is still red.

**Signals.** `SIGTERM` and `SIGINT` seal a still-`RUNNING` document red, `interrupted by SIGTERM`, before the process exits 128 + the signal (a finished document is left alone). Only a `SIGKILL` or a crash leaves it `RUNNING`, which no reader counts as a pass.
[A] Before any update has run on a host the API names no slot, so the preflight probes; a host
where both children are left running after an earlier swap needs the older one stopped.

`/metrics` is scraped from the observed frontend at the preflight, after the frontend step (the
deploy is frontend-first: `--api` is the proxy's public port, over TLS, and the update the
deploy starts there hands the frontend over before the nodes roll), at every poll of the node
wait and at the post checks. A measurement that is
not there is red: a scrape that fails or cannot be parsed, a series that is missing, a
listing that is cut off. The first red ends the deploy (`gate_red`; exit 1, or 3 in the
preflight where nothing has changed) and the detail names the gate, the value and the host step.

| Gate row | Reads | Red when |
|---|---|---|
| `a_inflight_ids` | `GET /v1/invocations?state=open` at the preflight, at each node's confirmation and after the frontend step; then `?since=&until=` over the whole deploy | a listing cannot be read (also when the route is off: it needs `--history-objects`) or is partial |
| `b_relay_untraced` | `kbf_frontend_relay_untraced` at every scrape | any scrape is above 0 or lacks the series |
| `c_farm_class_failures` | every recorded id's `/actions` pages, after the deploy (also after a failed one) | an attempt ended `infra`, `lost` or `lost_*` (client-side filter), an id cannot be read, or its record is `rows_capped` (not every attempt is visible) |
| `d_lost_blobs`, `d_blob_losses_delta` | `kbf_cache_lost_blobs`; `kbf_cache_blob_losses_total` minus its first scrape | any scrape is above 0 or lacks the series; the counter went backwards within one child. A fresh frontend child with an in-memory metadata log restarts the counter at 0 (its first reading is its baseline), so a loss made in the handover window before that reading is visible only while the old child still answers; the durable Raft log has no such gap (both children read the same history) |
| `e_under_replicated` | `kbf_under_replicated_blobs` | not 0 at the preflight (a farm that reports no series at all is refused there as `gate_unjudgeable`, exit 3: it needs storage members that have reported, as the post check on `/v1/members` already does); not back to 0 within the settle window after a node's step; the next node's step began (`activated_unconfirmed`) or confirmed before that |
| `f_auto_revert` | node states and the frontend update's end | a node reverted by its own timer, or the frontend update ended `failed` or `rolled_back` |

The deploy never orders a node, so gate E cannot hold the drain system's next step; it
watches and fails the deploy when the next step ran before the repair.

The one-line summary on standard output stays beside the document until the callers read the
document instead, and is removed on 2026-12-01. Exit 0 is a `green` document, exit 5
(`smoke_skipped`) is `extended` (no decision, not a pass), every other exit is `red`.

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
- **A deploy that can end `0` against the real proxy.** The post checks need `GET /readyz` to
  answer 200, and the proxy answers a constant 503 until its poll of the active child writes
  its readiness (unit U4b-3, `docs/design/kbf-proxy-api.md`, sections 15 and 18). Until then every
  real run ends `postcheck_failed`, exit 1, and a gamma host stays inert.
- A run of the whole deploy against the real proxy and the real children. The deploy's tests
  play the API (a fake that answers with the API's own types, served over TLS from a test CA).
  The client's transport is proved against the real proxy by `crates/kbf-integration-test/tests/deploy_through_proxy.rs`
  (the update route, the forwarded `/v1/*` calls, a three MiB upload, a wrong token); the run
  of the steps with the real proxy waits for the update API to be served on the proxy's update route.
