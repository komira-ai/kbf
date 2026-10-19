# Pull-based updates: hosts update themselves over the session they already hold

Status: design; the wire contract (section 5) is frozen. Built or in review: `kbf-installer` (merged), `kbf-update` (the node client, #908) and `kbf-helper` (the root helper, #929). The rest is built in the units of section 11.
**Revised for two rulings:** (1) node updates and the frontend's own update are decoupled (section 1 item 7, section 8); (2) the host's root tool pulls the release itself, in an unprivileged child, on the frontend's order, and kbf no longer stages release bytes into an inbox (sections 1, 2, 4, 5, 7 and 10). Where a built crate (`kbf-update`, `kbf-helper`) still has the inbox, this text is the target and section 7.5 lists what is removed. The root tool is `kbf-installer` as [distribution.md](distribution.md) section 6 describes; `kbf-helper` is folded into it. Where later text still says "the helper" (the as-built order rules, `helper.json`, `kbf-helper boot-check`), it means the root side of `kbf-installer`.
Adapted from the pull-update design study; labels: **[F]** read in code at the time of writing,
**[D]** a design decision, **[V]** verify before building on it.

**Supersession.** [split-mode-updates.md](split-mode-updates.md) is the authority for the **order of a rollout, the per-host
step, the idle and parallelism policy, and the frontend's own update**. It supersedes section 1 items 5 and 7 and section 8
items 1, 2, 5 and 6 of this document, which are rewritten below to match. This document remains the authority for the wire
(section 5), the release store and desired-release rows (section 6), the host (section 7) and trust (section 2).

Related: [split-mode-updates.md](split-mode-updates.md) (the sequence), [fleet-updates.md](fleet-updates.md) and [fleet-updates-security.md](fleet-updates-security.md) (the
earlier push-shaped design this one cuts down), [architecture-v2.md](architecture-v2.md) (sections 4 and 11: the
state store, protocol skew, upgrade order), [worker-protocol.md](worker-protocol.md), [../api.md](../api.md).

## 1. The answer in ten lines

1. An **external caller** names a **desired release**: one commit of this repository's `main`, whose build attestations are on GitHub.
   Updates are **triggered externally** through the frontend's control API; for us the caller is the gamma deployment, which calls the API
   from our network. The frontend stores the desired release. Support is **on-prem only** for now; cloud update providers are deferred (section 12).
2. Every host learns the desired release over the **mTLS session it already holds** with the frontend: an
   `UpdateOffer` sent after `Welcome` and at every change. No new inbound port, no SSH, no CI credential that
   reaches a host.
3. The order is "update to commit `<hash>`": `UpdateOrder{ACTIVATE}` over the session, carrying the commit, the
   generation, the epoch and the order id. The unprivileged `kbf` only relays it, as a small record, to the root
   tool `kbf-installer`. **No release bytes pass through `kbf` and there is no inbox.**
4. `kbf-installer` **pulls the release for that commit itself**, from the configured release source only, in an
   **unprivileged child** (no root while bytes arrive). The root parent re-checks everything the child hands it:
   size, SHA-256, the build attestation (verified offline with a pinned `gh` and a pinned trusted root), the archive
   shape and the binary's own `--version --json`. Only then does anything become root-owned: it installs into a new
   versioned directory. It never overwrites a running binary.
5. The **frontend alone orders** the rollout: for a host that is idle or drained it runs the per-host step of
   split-mode-updates.md 2.6 (cordon, wait for zero leases, on storage nodes mark the member planned-down, wait for
   write sessions and for the repair gate), then sends `UpdateOrder{ACTIVATE}`. The installer fetches, verifies,
   installs, flips `current` and restarts.
6. The new process registers; the frontend checks health and sends `UpdateOrder{CONFIRM}`. If confirmation does
   not arrive in the window, or the unit crash-loops, the installer reverts locally (commit-confirm).
7. **Node updates and the frontend's own update are decoupled.** "Update ready" (the desired release, set by an external API call to the
   frontend's control API) updates **storage and daemon nodes only**, through the drain system: **nodes update
   asynchronously, in any order, when idle or drained** (at most one storage node at a time; daemon-only hosts and Macs
   concurrently up to `max_hosts_out`, default 1; a busy Mac is deferred). The frontend is not in that rollout. Its own
   update is a **separate step, also triggered externally**, by `POST /v1/update/frontend {commit}` on the operator API (section 6.5): the
   frontend writes an order for its own host, `kbf-installer` there pulls and verifies the release, starts the new instance behind the installed
   Envoy, waits for its `/readyz` and hands over; the old instance stops scheduling and relays its open streams until they close (it never
   waits for `open_executions == 0` and never restarts the serving frontend). Details: split-mode-updates.md 2.5, 2.6
   and 3.3, and section 8 below for what decoupling changes.
8. Rollback is setting the desired release back: a **higher generation** naming an older commit. The old
   directory is still on disk, so nothing is downloaded.
9. Only the unprivileged `kbf` binary is ever pulled. `kbf-installer` (which includes the former helper), unit files, the
   sudoers entry and the pinned trust material change only by the package or a script a human runs. Root code is never
   auto-updated, and the code that downloads runs without root.
10. The operator sees every host's version (`GET /v1/nodes`) and the desired release (`GET /v1/update`), read-only. Nothing in the product
    decides to update: the trigger is the external API call of item 1.

## 2. Trust model (MVP decision)

**Hosts trust the GitHub build attestation of this repository's `artifacts` workflow for `refs/heads/main`,
verified offline on the host** with a pinned `gh` and a pinned `trusted_root.jsonl`, exactly as the scoped
installer for the development nodes does it (the verification is offline; the download that precedes it is a separate
step in an unprivileged child, section 7.1): `gh attestation verify <tarball> --bundle <attestation>
--custom-trusted-root <pinned trusted root> --repo <owner/repo> --signer-workflow
<owner/repo>/.github/workflows/artifacts.yml --cert-oidc-issuer <GitHub Actions issuer> --source-ref
refs/heads/main --source-digest <the offered commit> --predicate-type <build provenance>`, with no network. The
commit the attestation names must be the commit the offer names.

The frontend **only stores and distributes** the desired commit and the release bytes. It is **not trusted for
integrity**, and neither is the release source or the fetch child: the root parent re-checks the tarball's SHA-256
against the attestation's subject digest and the attestation against the pinned root, and installs nothing else. The approval is the external API call that sets the desired release.

| Actor | Can do | Cannot do |
|---|---|---|
| GitHub / CI | build, attest, publish | reach a host, push to the frontend |
| Compromised **frontend** (or a mirror named as the release source) | choose when and in what order; withhold; serve any bytes or any older attested build; deny service | make a host install a binary that lacks a valid `main` attestation of this workflow; run anything as root |
| Compromised **fetch child** (a parser or TLS bug while bytes arrive) | write bytes to its pipe; **steal the node key it was lent** (accepted residual in v1: seven-day certificate; follow-up is a fetch-scoped certificate, 7.1); stall until the time cap | write any file, reach any file of `kbf` or root, make the parent install bytes that fail the checks of section 7.2 |
| Compromised **service user** (or an action under the native driver); **on macOS `kbf` runs as root, so a compromise of it there is a compromise of root** | write an order record to the spool (an early or stale activation of a release that is attested, or a refused record), **including a `frontend_update` record that starts a frontend swap to an attested commit within the floors** (section 7); stop relaying | install an unattested binary; put bytes on the install path (there is no inbox); touch the versions directory, the installer or the units |
| Anyone who can push to `main` | get an attested build | (see residuals) |

Residuals, stated plainly:

- **`main` has no branch protection.** Anyone who can push to it gets an attested build, and so a binary every
  host accepts when the frontend offers it. The attestation proves *where a binary was built*, not *that a
  reviewer approved the source*.
- **A hostile frontend can offer any older attested build.** Downgrades are allowed on purpose: a rollback is the
  desired release set to an older commit. The guard is the **desired-state generation**; the installer keeps a floor of (epoch, then generation) in `state.json` plus the operator floor in `installer.toml` (section 7), a commit downgrade is allowed, and a rollback is the old release offered again with a higher generation, which gets a new, higher order id. A host
  acts on the highest generation it has seen, a higher generation always wins, and an order carries the store epoch so
  a restored store cannot replay one. A host refuses "older" only below the operator-set floor (`min_generation`, `min_protocol`).
- **Test-farm switch.** The held installer for the test farm temporarily runs with attestation verification off (to be
  added back). That is a test-farm setting only; no deployed default and no part of this contract.
- **The binary holds credentials.** On a storage-and-daemon host the `kbf` process holds the cold-store
  credential, so the set of commits that can be attested is the set of programs that can read it.
- **The design's separate Ed25519 release key is deferred.** A release key held off CI and off the shared session
  account would close the first two residuals (a human signs; a floor can be signed in). It stays a documented
  later option: the offer would grow a signed manifest field, and the helper would check it before the
  attestation. Nothing in the MVP wire prevents adding it.

## 3. What exists (and where this builds on it)

- **Artifacts** [F]: one `kbf-<commit>-<platform>.tar.gz` per platform holding one binary, `SHA256SUMS`, SBOMs,
  and a build-provenance attestation per tarball, on pushes to `main`. `kbf --version --json` prints `{commit,
  min_protocol, protocol, profile, state_schema, version}`. A release is verified, not rebuilt.
- **Versioned host layout** [F]: `/usr/local/kbf/releases/<commit>/`, `current` and `previous` symlinks, a
  `state.json` with a pending confirm (and an order-floor field kept only so an older state file reads); `kbf-deploy host install|rollback|gc`. The units run
  `<KBF_BIN_DIR>/kbf --mode=<mode>`, so pointing `KBF_BIN_DIR` at `current` needs no new unit shape.
- **Rollout state machine** [F]: a blocking engine (gate, stage, drain, wait idle, swap, wait healthy, wait
  repaired, confirm, undrain; rollback on failure; pause; fleet lock) over a `Transport` trait, with a fake fleet
  and planted-fault tests. It had no real `Transport`; this design supplies one over sessions.
- **The session** [F]: `kbf.worker.v1.Worker.Session`, `Welcome` (with the lease `epoch`), `MapUpdate` pushes,
  `NodeStatus` sent once after `Welcome`. The server keeps the newest per-node status from the node's current
  stream only.
- **The frontend** [F]: the operator API (`/v1/nodes`, node cordon/drain/uncordon, `/v1/frontend/drain`), a state
  store (`kbf-state`: compare-and-set rows C1, monotonic counters C3), and a drain that ends in a process exit.
- **Not built, and failing closed until it is** [F]: `under_replicated_blobs` (a planned metric of
  architecture-v2 section 11). The storage gate of section 8 refuses to order a storage node while the metric is
  absent.

## 4. Components

```text
 external caller / API          frontend host                              every other host
 -------------               -------------                              ----------------
 /v1/update, /v1/releases -> kbf --mode=frontend                         kbf --mode=storage-and-daemon
 (gamma deployment)           release store (--releases-dir)             update client (kbf-update)
                               desired release, rules (state store)         | writes order records only
                               offers + orders over sessions                | order spool (small records only)
                               rollout controller                           v
                                                                          kbf-installer (root, no socket)
                                                                            fetch child (kbf-fetch) -- pipe --> parent
                                                                            versioned dirs, commit-confirm
```

| Piece | Where | Notes |
|---|---|---|
| Wire messages | `crates/kbf-proto/proto/kbf/update/v1/update.proto` | messages and one service; frozen (section 5) |
| Platform names | `kbf_types::ReleasePlatform` | `linux-x86_64`, `linux-arm64`, `darwin-arm64`; from a node's `os` and `arch` report entries |
| Release store, desired release, offers, `ReleaseService` | `kbf-server` | section 6 |
| Update client | `kbf-update` (the node's side, unprivileged) | relays offers and orders, writes `activate`, `confirm` and `revert` order records to the spool (and refuses any wire order that names another verb), reports status, refuses an order while a lease is live. It fetches nothing and stages nothing |
| Installer (fetch in a child, verify offline, install, flip, revert) | `kbf-installer`, a root unit on each host; `kbf-helper` is folded into it ([distribution.md](distribution.md) section 6) | verbs and the fetch child in section 7 |
| Rollout controller | `kbf-server`, replaces the older record in `rollout.rs` | section 8 |
| Fleet view | read-only over `/v1/nodes` and `/v1/update`; the UI page is deferred | section 9 |

## 5. The wire contract (frozen)

`kbf.update.v1`, in `crates/kbf-proto/proto/kbf/update/v1/update.proto`, is the contract; this section states the
rules the field comments cannot. Numbers in `kbf.worker.v1`: `DaemonMessage.update_status = 7`,
`ServerMessage.update_offer = 8`, `ServerMessage.update_order = 9`. `UpdateStatus` is its own message because
`NodeStatus` is sent once per `Welcome` and an update changes more often.

**Component.** One tarball, `<name>-<commit>-<platform>.tar.gz`, holding exactly one regular file named `<name>`.
`name` is `kbf` or `kbf-daemon`. `sha256` is 64 lowercase hex digits of the tarball and is also its name in
`ReleaseService.Fetch`. `attestation` is the build-provenance bundle (JSON lines, at most 256 KiB) of the tarball.

**Which components a platform needs.** `linux-x86_64`, `linux-arm64`: `kbf`. `darwin-arm64`: `kbf` (no darwin `kbf-daemon` is built; the installer refuses a Mac offer that names it). A
platform is **complete** for a commit when the frontend holds every component it needs, with their attestations.

**UpdateOffer.**
- `generation` strictly increases at each change of the desired release, survives a frontend restart, and a
  rollback to an older commit gets a *higher* generation than the release it replaces. Setting the commit that is
  already desired changes nothing and keeps the generation.
- `platform` is the node's own, derived by the frontend from the node's `os` (`linux`, `macos`) and `arch`
  (`x86_64`, `arm64`) report entries. A node with any other pair gets no offer.
- The offer is **level-triggered**: sent after `Welcome`, and to every connected node at every change. A node that
  connects later gets the current offer too. A node whose platform is not complete gets none and is shown
  `unavailable`.
- An offer with the generation the node already holds is a repeat (for example the platform became complete after
  the change). A node acts on a (generation, commit) pair once.

**UpdateOrder.** `order_id` comes from a store counter and strictly increases within an `epoch`; `epoch` is the
`Welcome` epoch, which moves only when a store is restored (a handover keeps it). Orders are ordered by **(epoch, then
id)**, by the node's client and again by the helper, which acts on each pair once: a higher epoch is newer whatever its
id (a restored store counts ids again), an older epoch is refused whatever its id, the same epoch with a higher id is
newer however large the gap (the counter is the store's, not the host's), and the same pair again is a replay. A rollback is the old release ordered again with a higher generation. The installer's floor is
(epoch, generation), compared lexicographically (section 7), together with the operator floor in `installer.toml`. The helper names an activation `<epoch>-<id>`, so the same id in two epochs is two
activations. An epoch more than 1000 above the last acted on is refused with the reason in `status.json` and not
acknowledged. This is best effort: it blunts a typo or a stray huge epoch, but an order forged at acked+1, or a first
order at a huge epoch, is taken and wedges the installer until root clears `state.json`'s floor (the schema 1 state file of the as-built helper, which P3 migrates into `state.json` schema 2); the first order a host
acts on is measured from nothing and is taken at any epoch. A compromised service user can still write a first order
with a huge epoch: an availability limit, the same exposure as ordering an activation early (section 7.3 of the
design study). `window_s` is the confirm window of `ACTIVATE`, **per platform as built**: on Linux the helper ignores it, and the window starts at the new
process's first start (`kbf-installer boot-check --window-secs`, default 900, range 120 to 1800); on macOS there is no start hook, so the window is
armed at activation and `window_s` is clamped to 120 to 1800. Either way a compromised frontend cannot set a year.

**UpdateStatus.** Sent after `Welcome` and on every change. `running_commit` is the process's own build commit
(12 hex or `unknown`); `installed_commit` and `staged_commit` are 40 hex from the helper, empty if not known.
`acked_order_id` is the newest order acted on; `offered_generation` the newest offer seen. `helper_present` (field 8) says whether the privileged installer is on the host (the field keeps its name; it is `kbf-installer` now). `STATE_FETCHING` is the installer pulling the release for an order (section 7.1); `STATE_STAGED` is a release verified and installed into its directory but not yet activated, a window of one order, since the fetch is part of the `ACTIVATE` order and there is no pre-staging. `staged_protocol`, `staged_min_protocol` and `staged_state_schema` (fields 9 to 11) are what the newest installed, not yet active, release declares in `kbf --version --json`, 0 when none. **Because nothing is staged before the order, the frontend's ordering decision (split-mode-updates.md 2.4 and 3.3) cannot read these fields in advance: its inputs are the release's declared protocol and schema recorded when the release was uploaded or proposed (the preflight row `update/available/<commit>`), and the fields are the host's check afterwards, which the controller compares with the preflight value before it sends `CONFIRM`.** A node that never sends
it is listed `unsupported`, and a rollout halts at it (it never skips a host silently).

**ReleaseService.Fetch.** On the worker listener, admitted nodes only (mutual TLS). The argument is 64 lowercase
hex digits and nothing else; an unknown digest is `NOT_FOUND`. The server never builds a path from anything but the
validated digest. Chunks are at most 1 MiB. **The caller is the installer's fetch child (section 7.1), not the `kbf` process.** It serves the release's tarballs and its attestation bundles, each under the digest of its bytes.

**ReleaseService.Describe.** For a caller that was given no offer (a `frontend_update` order is a locator). `DescribeRequest{commit, platform}`
(`platform` optional) returns a `ReleaseDescription{commit, components}`: per described platform, the tarballs a host of it needs, each with
its `sha256` and `size` and its bundle's `bundle_sha256` and `bundle_size`; a platform is described only when it is complete, as for an offer.
Admitted as `Fetch` is (one row of the role table). `INVALID_ARGUMENT` for a commit that is not 40 lowercase hex digits or an unknown platform,
`NOT_FOUND` for a commit with no stored tarball, `FAILED_PRECONDITION` naming what an incomplete platform lacks. The same description is
`GET /v1/releases/{commit}` as JSON, the file `releases/<commit>.json` of a `url` source (`docs/api.md`). It is a claim; the installer verifies
what it fetches (`kbf-installer.md` section 5).

**The order, as the node relays it.** `UpdateOrder` is unchanged and carries `order_id`, `epoch`, `verb`, `commit` and
`window_s`. The "update to commit `<hash>`" order is `VERB_ACTIVATE`. It gains one additive field, `uint64 generation = 6`
(proto3, compatible both ways; an older node ignores it). **Generation 0, or a frontend that predates the field, means the
installer refuses `activate`**: the frontend must be updated first (section 8 item 1). `UpdateOffer` gains the expected facts of
the release, `uint32 protocol = 5`, `uint32 min_protocol = 6`, `uint32 state_schema = 7` and `string profile = 8` (the platform
is the existing field 3); the frontend fills them from the preflight row `update/available/<commit>` and an offer without them
carries zeros, which the installer refuses (fail closed). **Both additions count against the frozen contract of this section:**
they are additive and compatible, but they are two changes to frozen messages and are made together in one wire change with the
tests of section 10. The installer checks the generation is monotonic and above the operator floor (section 7), which the (epoch, id) rule alone
cannot give it. The offer's components, attestation and expected facts reach the installer in the order record the node writes from
the offer it holds for that commit; a node holding no offer for the commit refuses (`STATE_REFUSED`, "no offer for this commit").

**Compatibility.** A server that predates these fields ignores `update_status`; a node that predates them ignores
the offers and orders, never sends a status, and is listed `unsupported`.

### Contracts owned by the node client and the installer

Two small things cross from the unprivileged `kbf` to the root `kbf-installer`, and both are specified by the
`kbf-update` and installer units and frozen with them: the **order record** (section 7: one file per order in the
installer's order spool, a few hundred bytes of JSON) and the world-readable `status.json` the installer writes. This
document does not restate their fields so that there is one source. **There is no inbox**: no offer directory, no
tarball, no attestation bundle is written by `kbf`, and no entry the unprivileged process creates names a path, a
URL or a unit.

## 6. The frontend

### 6.1 Flags and failure

`--releases-dir DIR` names the content-addressed release store (files named by their SHA-256). Every update
endpoint needs it **and** a state store (`--state-store`). Without either, every update endpoint answers **503**
saying which is missing; nothing answers an empty 200.

### 6.2 State store rows

| key | value |
|---|---|
| `update/release/<commit>/<platform>/<name>` | the component: `sha256`, `size`, and the attestation once uploaded |
| `update/desired` | the desired release: `generation`, `commit`, `set_at` |
| `update/generation` | the counter (C3) the generations come from |
| `update/slot` | the rollout slot: one storage holder plus up to `max_hosts_out` host holders, each `{host, order_id, epoch}`, with the run state; no time to live (section 8) |
| `update/host/<id>` | the controller's per-host phase `{phase, order_id, generation, since, boot_before}`: what a restarted controller resumes from (split-mode-updates.md 2.6) |
| `update/available/<commit>` | the release source's proposal and its preflight result; metadata only (split-mode-updates.md 6.2) |
| `update/policy` | `{paused (bool), idle_wait, min_serving, max_hosts_out (default 1), mac_force_after, new_work_only_to_updated, accept_failing_builds}`; the older `update/auto {enabled}` row is removed |

**A release upload triggers its preflight.** The `PUT` of a release's last needed component starts the preflight for the commit and
writes the `update/available/<commit>` row (metadata, the declared `protocol`, `min_protocol`, `state_schema`, `profile`, and the
result). **Until the row exists the frontend sends no offer for that commit**, since the offer's expected facts (section 5) come from it.
The operator sees the release as "preflight pending" until the row is written, and "preflight failed" with the reason if it fails; neither
state can be set as the desired release.

**Who supplies what the host verifies.** The attestation bundles an offer carries are **stored by the frontend's release store** (the
release's CI upload `PUT`s `<name>.attestation` beside each tarball, section 6.3); the frontend is a courier and not a trust anchor, since the
installer verifies each bundle against the pinned root. **Envoy is a dependency, not part of a release**: it is installed as a prerequisite
(a package dependency or host preparation), kbf supports a tested range of Envoy versions, nothing in a kbf release pins Envoy, and the installer
never fetches it. Release-manifest and Envoy-component pinning, and cross-version Envoy upgrades, are deferred (section 12).

**The disk index is the source of truth for bytes.** The release store on disk (`files/<sha256>` and
`index/<commit>/<platform>/<name>`, with attestation bundles, which are also named by their digest in `files/`) is authoritative for what the frontend can serve. The
`update/release/...` rows are metadata over it (digest, size, attestation present); a lost or stale row is rebuilt from
the disk index, never the reverse. The desired release, the slot, the per-host phase rows and the policy are state-store rows.

### 6.3 Operator API (writes need the existing token gate)

- `PUT /v1/releases/{commit}/{platform}/{name}`: the tarball as the body, streamed to a `.partial` file with the
  SHA-256 computed while streaming (size cap 512 MiB), fsynced, then renamed to `<sha256>`; the answer carries the
  SHA-256. `PUT .../{name}.attestation`: the bundle (at most 256 KiB). The same (commit, platform, name) with
  different bytes is **409**: a release is immutable. The frontend also checks the tarball is gzip+tar holding
  exactly one regular file named `<name>`; that is an early failure for mistakes, not trust. Path segments are
  validated (40 lowercase hex, a known platform, a known name) and never joined raw.
- `GET /v1/releases`: commits with, per platform, which components are present and whether the platform is complete.
- `GET /v1/releases/{commit}`: the description `ReleaseService.Describe` gives for every complete platform, as JSON (section 5): the
  files of the release by digest, and the `releases/<commit>.json` of a `url` source.
- `POST /v1/update/desired {commit}`: the commit needs at least one complete platform (else 409). Compare-and-set
  on `update/desired`; the counter gives the new generation only when the commit differs from the current one.
- `GET /v1/update`: `{desired, releases}`. The policy fields are set through `POST /v1/update/policy` (admin). There is no auto-update endpoint.
- `GET /v1/nodes` gains, per node, an `update` object: state, the three commits, offered generation, refusal,
  `helper_present`, `up_to_date` (true, false or null) and `availability` (`ok`, `unavailable`, `unsupported`).

### 6.4 Sessions

**As built.** The frontend looks at the desired row and the release store once a farm tick and
keeps one offer per complete platform; each session sends its node's platform offer after
`Welcome` and whenever that offer changes (the same offer is never sent twice on a session).
A look that fails keeps the table. The node's platform is read from its first `Hello`.
The rollout controller (`kbf-server/src/update`) runs on the frontend with a state store, once a
farm tick, and starts no host when an operator wrote `update/policy.auto_nodes` false or paused the
run (a policy never written reads as `auto_nodes` true: the desired write is the click; hosts already
in a step are always moved). Its orders go out as `UpdateOrder` on the host's stream with the `Welcome`
epoch (an order of another epoch is refused before it is sent); a confirmation carries an id
of its own, drawn from the order counter, because a node drops a repeated id. Its facts come
from the farm and each node's `UpdateStatus` of its current stream. A Linux host
is joined to its storage member by the address it advertised at enrollment, matched exactly; the
mark is `Members::plan_down` and its undoing `Members::undo_plan`; a host that matches no member is
never started. **Not yet:** no operator action starts a run with `auto_nodes` written off.
`GET /v1/nodes` lists `update` per node and `GET /v1/update/hosts` its detail (`docs/api.md`).

After `Welcome`, and to every connected node at every change of the desired release or of a platform's
completeness, the frontend sends an `UpdateOffer` for that node's platform. `UpdateStatus` is kept as the newest per
node and ignored from a replaced stream, exactly as `NodeStatus` is.

### 6.5 Frontend self-update

`POST /v1/update/frontend {commit}` (write gate as the other writes; admin) is the one route that updates the frontend. Gamma calls it. The commit must have a complete platform for the frontend host (else 409), and no frontend
update may be in flight (409). The frontend then **writes an order record into its own host's `orders/` directory**, by the same mechanism as
`kbf-update` uses for nodes (a dot-prefixed temporary name, then rename; section 7), with `verb=frontend_update`, the `commit`, a `generation` from the
store counter, the store `epoch` and an `order_id`. The record is a locator: it carries no components and no expected facts, and the root half fetches and verifies the release itself ([kbf-installer.md](kbf-installer.md) section 6). The route returns the order id; the installer refuses a second order while one is in flight (`frontend_busy`) and one whose installed Envoy is outside the tested range (`envoy_range`). The root CLI `kbf-installer update --commit SHA --frontend` stays as the **local fallback** and runs the same code. It does not go
through a node session, and nothing is sent on the wire. The installer's reply is the host's `status.json`, which the route's status
read (`GET /v1/update`) reports.

## 7. The host

```text
/var/lib/kbf-installer/                         root:root 0755; the installer's own tree (not kbf's StateDirectory)
  releases/<commit>/kbf                         root:root 0755, never modified after install
  releases/<commit>/VERIFIED                    written by root after 7.2 passes; "installed" means this exists
  releases/current  -> <commit>                 the running one (units run releases/current/kbf)
  releases/previous -> <commit>
  work/<commit>.partial/                        root:root 0755, extraction of one fetch; swept at start (7.3)
  work/<commit>.partial/dl/                     root:root 0700, the downloaded tarball only
  state.json (schema 2)                         root-owned; supersedes the schema 1 helper state file, P3 migrates it
  status.json                                   /var/lib/kbf-installer/status.json, root-owned 0644, atomic rename
  orders/                                       kbf:kbf 0700; order records written by the service user; no release bytes
```

Everything the root side reads as authority lives in a root-owned tree that `kbf` cannot write. The earlier layout put
`releases/` and the state file under `/var/lib/kbf`, which is `kbf`'s `StateDirectory`; the service user would
then own the directory that holds the links and the state, whatever the files' modes. The install root is
`/var/lib/kbf-installer` and [distribution.md](distribution.md) section 1 (which names `/var/lib/kbf/releases`) must follow.
Retention: the running, the previous and one more directory. Only `kbf-installer` writes the tree except `orders/`.

`kbf-installer` runs as a root unit with no listening socket (the former `kbf-helper` is folded into it). Its service
loop has two commands, with no free-form arguments (its flags name the unit and the directories; nothing in an order
record is a path, a URL, a command or a unit; the platform is the host's own, read by the installer). **On macOS `kbf --mode=daemon`
runs as root (macos-end-state.md), so there a compromise of `kbf` is a compromise of root; the order spool and the fetch child
do not change that, and this section's separation of `kbf` from root holds on Linux only.** The probe and the fetch child below
still run unprivileged there, so a fault in them is not root.

| Command | Does |
|---|---|
| `kbf-installer run` | every 5 seconds (a fixed constant, not a key), each pass: finish a restart it owes; revert a swap whose confirm window ran out; sweep leftover scratch (7.3); act on order records from `orders/` in (epoch, id) order, once each. For `activate`: refuse it while a trial is pending (a release activated and not yet confirmed or reverted: there is one confirm window at a time, never two timers), check the order (below), **fetch, verify and install the release for the commit (7.1, 7.2)**, write `pending_confirm`, flip `previous` then `current`, restart the one configured service; `confirm`: keep it; `revert`: flip back and restart; write `status.json` |
| `kbf-installer confirm`, `revert`, `rollback`, `update --frontend` | the operator verbs: **inline root CLI runs under the installer's lock** (exit 4 while a `run` pass holds it); no socket, and `kbf` cannot run them. `confirm` and `revert` name the activation; `rollback` is the only verb below the floor. `update --commit SHA --frontend` is the local fallback of the `frontend_update` order (section 6.5) |
| `kbf-installer boot-check` | `ExecStartPre` on Linux: count the starts of a release on trial and revert one that keeps failing before it starts again; start the confirm window at the first start |

**The order spool.** `orders/` is owned by `kbf` with mode 0700. Root opens it with `openat` and `O_NOFOLLOW` relative to a
descriptor it holds, takes an entry only if `fstat` says a regular file with one link, reads it **once into memory** and
parses from that copy (never re-reads a path); the file is then removed by name through the same descriptor. At most 64 records
are held and a record older than the confirm-window maximum (1800 s), **measured from root's first-seen time** (recorded in memory and in
`state.json` when root first reads it, never the file's mtime, which the writer controls), is expired and removed, so a stuck or hostile writer cannot
grow the directory or replay a stale order after a restart. `kbf-update` writes each record to a temporary name starting with `.` and renames it into place; **the installer ignores dot names**,
so it never reads a half-written record. A record is at most 1 MiB; a larger, unparsable, symlinked or
non-regular record is refused and removed.

An order record is `{order_id, epoch, generation, verb, commit, window_s}` (`verb` is `activate`, `confirm`, `revert` or `frontend_update`, the contract's record, [kbf-installer.md](kbf-installer.md) section 4) plus, for `activate`, the offer's components
for the host's platform (`name`, `sha256`, `size`, `attestation`: the same bytes the offer carried, at most 256 KiB each) and the
**expected facts about the release**: `platform`, `protocol`, `min_protocol`, `state_schema` and `profile`, as the frontend's
preflight recorded them. These are **locators and claims, not trust**: section 7.2 re-derives every one that matters, and the
expected facts are the claim that 7.2 step 5 checks the binary against *before* the flip. Before fetching, the installer refuses
(and says why in `status.json`) an order that is a replay or older by the (epoch, id) rule of section 5, a commit that is not 40
lowercase hex, a component set that is not exactly what the host's platform needs, a size over the cap (7.1), a pending trial,
or a host with a live lease. The installer observes leases itself and reads nothing the daemon or `kbf` writes: **on Linux** it
observes the lease processes' cgroup (membership of the systemd scope or unit that runs actions); **on macOS** it observes the
launchd job and the processes by uid (the lease users). The record's claim is never used.

**Generation and floor.** The (epoch, id) rule orders orders but cannot tell an old order from a replay of one. The installer
therefore also compares **(epoch, generation) lexicographically**, mirroring the order rule: a higher epoch wins whatever its generation,
an older epoch is refused, and within one epoch the generation must be at or above the highest acted on (a rollback is a higher
generation naming an older commit, so monotonic holds) and at most **1000 above it per order** (best effort, like the epoch guard:
it blunts a stray huge value, it does not stop a forger who steps by 1000). `UpdateOrder.generation` is the additive field of
section 5. **This check stops a replay, not a forgery.** Against a compromised `kbf` (which writes the record) the only defence is
the **operator floor** in root-owned `/etc/kbf/installer.toml`, `min_generation` and `min_protocol`, below which no order is taken. The
intended end state is a frontend-signed grant on each order, verified before any fetch against a pinned key; that key and its
rotation are not designed here, so v1 is the monotonic check plus the floor.

Orders carry the verbs `activate`, `confirm` and `revert`; they are not installer subcommands. **A `confirm` is accepted only if** it names
the pending activation (`<epoch>-<id>`), the unit is active, `/proc/<MainPID>/exe` resolves inside `releases/<current>`, and the first
`boot-check` of that release has run; otherwise it is refused and the window keeps running. **The verb selects the dispatch.** An `activate` record is the node sequence of section 8 item 2 and **never touches `kbf-frontend@*` or the host's own
frontend mode**. A `frontend_update` record is accepted only on a host whose `kbf.toml` mode is `frontend` or `all-in-one`, and only from `orders/`
(or the root CLI); it is subject to the same ordering, floors and refusals as `activate`, and is dispatched to the frontend update: verify as for a
node (7.1, 7.2), check the installed Envoy is in the tested range, point the idle slot at the release, start the other `kbf-frontend@` instance, wait for
its `/readyz`, hand over, then confirm ([kbf-installer.md](kbf-installer.md) section 6). A second one while one is in flight is refused `frontend_busy`. The installer must have the
frontend-host activation of split-mode-updates.md (unit U5) before it takes a `frontend_update` order.

**Who may write which verb (trust consequence).** `orders/` belongs to `kbf`, so **any process running as `kbf` on a frontend or all-in-one host can
request a frontend swap by writing a `frontend_update` record**; on an all-in-one host that includes the node daemon. This is accepted: both run as `kbf`,
and the release must still pass every verification of 7.2, so the swap can only move to an attested commit within the floors (`min_generation`,
`min_protocol`, the generation rule and the deny list). A compromised `kbf` could already order a node update; the new power is to start the frontend
handover, an availability exposure bounded by the single in-flight rule, not an integrity one. **A wire order received over a node session never
becomes a `frontend_update` record**: the wire `UpdateOrder` verb enum has `activate`, `confirm` and `revert` only, `kbf-update` maps those and nothing
else, writes no other verb from any input, and refuses an order that names anything else. Only the frontend process, writing for its own host from
section 6.5, writes `frontend_update`.

- The installer **never executes the new binary as root**. The `--version --json` probe runs as `kbf-fetch` (no credentials, no
  network, no file of its own; the `kbf` user holds the cold-store credential and the node key and is not used), with an empty
  environment, the rlimits of 7.1 and the bounds of 7.2.
- Order of writes: `pending_confirm` before the flip, the flip before the restart; on start the installer reconciles
  its state against the links, resolving to the state that keeps `previous` intact.
- Linux: an `ExecStartPre` boot check decrements a start counter while `current` is unconfirmed, so a crash loop
  reverts in seconds with no timer. macOS: a LaunchDaemon; the revert timer is the only trigger.
- macOS: a binary that carries `com.apple.quarantine` is refused; it must verify with `codesign
  --verify --strict` and carry the CI ad-hoc signature (`adhoc,runtime`).

### 7.1 The fetch child

The installer pulls the release itself. The download is the only step that touches the network, and it runs
**without root**.

**Release source and settings.** Where the child connects, and every installer setting, is configured in `/etc/kbf/installer.toml`
(root:root 0644, unknown keys refused), written only by `kbf-installer setup` and changed afterwards only by root. It is never taken from an
order, an offer, the environment or `kbf.toml`, which has no update keys ([kbf-installer.md](kbf-installer.md) section 3, the single owner of the key list):

```toml
release_source = "frontend"        # "frontend" (default) or "url"
# release_url = "<https base URL of a mirror>"   # only with release_source = "url"
fetch_timeout_s = 900              # whole fetch of one component; 60 to 3600
max_component_bytes = 536870912    # 512 MiB, the store's own cap; at most 1 GiB
min_generation = 0                 # operator floor: no order below this generation is taken
min_protocol = 0                   # operator floor: no release whose protocol is below this is installed
# node_cert = "<path>"             # release_source = "frontend" only; setup copies it from enrollment
# node_key  = "<path>"             # likewise; read by root, lent to the fetch child (below)
```

`setup` takes `--release-source frontend|url`, `--release-url` and `--min-generation`. `frontend` derives the endpoint from the frontend address
and the pinned CA that enrollment already wrote (the worker listener, `ReleaseService.Fetch`, mutual TLS). `url` is a plain
HTTPS file server that serves the release store's layout, `files/<sha256>`; setup refuses anything but `https`, a URL with
user information, a query or a fragment, and **the child follows no redirect at all** (any 3xx is a failed fetch). A `frontend`
source authenticates the server with the pinned CA and checks the frontend's name against the certificate's SAN; a `url` source
uses the system trust roots, which the doc names as the trust anchor of the transport only (the bytes are trusted by 7.2, not by
TLS). No
other source is ever contacted: an order or an offer cannot redirect a fetch, and neither can the frontend (a compromised
frontend can only serve bytes from the source, which 7.2 refuses). A missing `release_source` means `frontend`.

**Which user.** A dedicated system user and group `kbf-fetch`, created by the package (sysusers; `_kbf-fetch` on macOS,
created by setup with the lease users), with no home, shell `nologin`, no supplementary groups and no file of its own. It is
not `kbf`: the `kbf` process holds the cold-store credential, the node key and writes the order spool, and a bug in a TLS or HTTP
parser should not reach them.

**How it starts.** The root parent re-executes its own binary as `kbf-installer fetch-child` (no separate binary to ship),
after: `setgroups(0)`, `setgid`, `setuid` to `kbf-fetch` (checked by reading the ids back), `no_new_privs`, `chdir /`, an
empty environment, every descriptor closed except 0 (open on `/dev/null`), 1 (the byte pipe), 2 (a log pipe) and 4 (the lent identity),
`RLIMIT_FSIZE` 0 (it cannot create a file), `RLIMIT_CORE` 0, `RLIMIT_NOFILE` 16, `RLIMIT_AS` 1 GiB, a small `RLIMIT_NPROC`,
and on Linux `PR_SET_PDEATHSIG` SIGKILL. The arguments are fixed flags the parent validated: source kind, URL, the 64-hex
digest, the byte limit. A seccomp filter and (macOS) a `sandbox_init` profile are a hardening that does not change this
contract and are not required for the first version.

**How bytes pass.** A **pipe**, not a staging directory. The child writes the tarball to its standard output; the parent
reads it, hashes it as it arrives and writes it to a file it created itself in a root-owned `0700` scratch directory
(`work/<commit>.partial/dl/`). The child has no writable file system object at all, so there is nothing for it to swap, link or
modify after the parent has looked (the TOCTOU and symlink exposure of a directory owned by the fetch user is exactly what the
inbox had). One child per component; the attestation is not fetched (it came in the order record and is self-authenticating,
7.2). Standard error is read by the parent up to 4 KiB for the log and is never parsed; bytes beyond 4 KiB are drained and
dropped so a chatty child cannot block on a full pipe. The parent reads both pipes with `poll` and non-blocking reads, so a
stall on one cannot hold the other or the parent. On macOS, where `PDEATHSIG` does not exist, the parent closes the pipe and
the child's death is the timeout and a `SIGKILL` of its process group.

**The node's identity.** For `release_source = "frontend"` the child needs the node's client certificate and key for the mutual TLS
of `Fetch`. The parent (root) reads them from `node_cert` and `node_key` in `installer.toml` and lends them over descriptor 4 (on Linux an anonymous
in-memory file; on macOS a socketpair, so the key is never written to disk), closed when the child exits; the child never has a
path to the key directory. **A compromised fetch child can steal the node key.** That is an accepted residual in v1: the
certificate lives seven days, the exposure is the one the `kbf` process already has, and it is the price of serving releases
on the authenticated listener. The follow-up is a fetch-scoped client certificate (a distinct extended key usage the frontend
accepts only for `ReleaseService.Fetch`), so a stolen fetch identity cannot open a worker session. With `release_source = "url"` nothing is lent.

**Setup uses the same child.** `kbf-installer setup` downloads the first release through `fetch-child` and checks it with 7.2,
exactly as an update does, so the bootstrap path is not a second, unconfined downloader. **The first release at setup comes from the `url`
source**, because there is no node certificate yet (enrollment writes it later); a host that will use the `frontend` source switches
`release_source` after enrollment, and the next update uses it.

**Size and time caps.** The component's size is read from the order record; it must be at most `max_component_bytes`, and the
host must have free space under `/var/lib/kbf-installer` for the tarball (`size`), its extraction (the decompressed cap, 1 GiB, or
the declared size times ten if smaller), and a retained release being replaced: that sum is computed from the record before anything
is opened, and the order is refused if it does not fit. The parent reads at most `size` bytes: one byte more ends the child with SIGKILL and the order is
refused; fewer than `size` bytes at end of stream is a failed fetch. Time: connect 30 s; no byte for 60 s kills the child; the
whole component `fetch_timeout_s`. Range requests and resumption are not used.

### 7.2 What the root side re-verifies

The parent does not trust the child, the source, the order record, or the files it has just written. In this order, on
files it opened itself (`O_NOFOLLOW`, then `fstat` to confirm a regular file it owns):

**Every step root runs is bounded**: a wall-clock limit, the process group killed on expiry, output read up to 4 KiB and
parsed strictly (anything else is a failure), `gh` run with an empty environment and an empty `HOME`. The limits are 120 s for
`gh`, 120 s for extraction and 10 s for the probe.

1. **Count**: exactly the declared `size` bytes arrived, and the declared size is within the cap.
2. **Digest**: the SHA-256 the parent computed while reading equals the `sha256` of the record **and** the subject digest of the
   attestation bundle (the record's digest is only a locator; the attestation's is the trusted one).
3. **Attestation**, offline, with the pinned `gh` and the pinned trusted root and the exact flags of section 2: repository,
   signer workflow, issuer, `refs/heads/main`, predicate type, and `--source-digest` equal to the **commit of the order**. The
   bundle is the record's; if it fails, nothing from the child is read further. The pinned `trusted_root.jsonl` expires: the
   installer reports the days left in `status.json`, and the frontend raises an alert when fewer than 30 remain.
4. **Shape**: gzip and tar with exactly one regular file named `<name>` (no link, device, directory, absolute path or `..`),
   decompressed output at most 1 GiB, extracted by the parent into `work/<commit>.partial/` (not into `dl/`) with mode 0755 and
   owner root. The tarball stays in `dl/` (0700, root), which the probe never needs to traverse.
5. **The binary names itself, and must match the order, before the flip**: `<name> --version --json`, run as `kbf-fetch` with an
   empty environment, reports the order's commit (12 hex prefix) and a `protocol`, `min_protocol`, `state_schema`, `profile` and
   `platform` that must **equal the expected facts in the order record**, and must satisfy the operator floor. A mismatch refuses
   the order **before** anything is renamed or flipped. If the record carries no expected facts (the frontend had no preflight
   row), the order is refused: fail closed. The values are also written to `status.json`; the frontend compares them with its own
   preflight (section 5, UpdateStatus). macOS: quarantine and `codesign --verify --strict`.
6. **Only now** does the parent write `VERIFIED` into the directory, `rename` it to `releases/<commit>/` and `fsync` the parent
   directory. Before this rename no link, no unit and no path a service looks at has changed.

The child's exit status, its output and its log are not inputs to any of this: a child that "succeeds" with other bytes fails
at 1 or 2, and one that fails after delivering all the bytes is judged on the bytes. **A release counts as already installed only
if `releases/<commit>/VERIFIED` exists**, written by this procedure; a rollback to such a directory skips steps 1 to 4 (it re-runs
5 and the floor) and no fetch happens. An unreferenced directory without the marker is treated as absent and removed; a referenced one is kept and flagged as above. Directories made by the
test-farm switch (§2, attestation off) never carry the marker and so never count as verified.

### 7.3 A crash in the middle

- **Child dies or is killed (OOM, timeout, bug)**: end of stream before `size` bytes. The parent removes the scratch directory,
  sets `STATE_REFUSED` with the reason (`fetch: short read`, `fetch: timeout`, `fetch: source unreachable`) and does **not**
  retry. Nothing but scratch has changed. The frontend's controller treats it as a failure before the swap: the host is
  uncordoned, the run goes `held`, and a person re-issues the update (a new order id).
- **Parent or host dies during the fetch**: `work/*.partial` is left. The installer deletes every
  entry under `work/` and every unreferenced `releases/` directory without `VERIFIED` at the start of each run; **the sweep never removes
the target of `current` or `previous`**, whatever its marker: a referenced directory without `VERIFIED` stays and `status.json`
flags it "unverified: rollback to it refused"; a partial file is never resumed or trusted, since its bytes were not verified. The child
  has `PDEATHSIG` (Linux) or is killed by process group (macOS), and a closed pipe, so it does not outlive the parent. A
  record is removed from the spool only after it was read into memory, but the order is acknowledged only after the flip, so the
  installer keeps its own copy (`state.json`, root-owned) of the unacknowledged order and runs it again from the beginning
  (nothing was flipped, so repeating it is safe), unless it has expired (7.1 spool rule).
- **Between the verified rename and the flip**: the release directory exists and is verified; `current` is unchanged. A repeat
  of the order finds the directory with its `VERIFIED` marker and goes to the flip. The flip itself is the existing `activate` crash story
  (section 10).
- **Disk full**: the free-space check refuses first; if it still fills, the write fails and the scratch is removed.

**Residuals, stated.** Two nodes can be a release apart while a rollout is held (node-to-node skew is within N/N-1 by section 8). A
host's local revert (its own timer or boot check) is not seen by the frontend until the host next reports, so the controller can
briefly hold a stale phase. Neither is new; the fetch makes neither worse. **"No network for the probe and for `gh`" is not enforced in v1**: both run as
unprivileged users with an empty environment and no credentials, but nothing in the first version blocks their sockets (a network
namespace or a sandbox profile is the hardening of the same list as seccomp, section 12).

### 7.4 Adoption of an existing host

A host installed by the earlier layout (`/usr/local/kbf`, with `current`, `previous` and `state.json`) moves to
`/var/lib/kbf-installer` in `kbf-installer setup`. The steps, in this order:

1. **Name the running commit from the probe only.** Run `--version --json` of the installed binary as `kbf-fetch`, under the probe
   bounds of 7.2. The commit is never taken from `state.json`, a symlink name or anything `kbf` can write.
2. **Ask the configured source for the offer metadata of that commit**: `sha256`, `size`, the attestation and the expected facts
   (`protocol`, `min_protocol`, `state_schema`, `profile`, platform). All of it is an **untrusted claim** that 7.2 re-derives; none is
   believed. Fetch the tarball through the fetch child and run 7.2 on it. On success install it as `releases/<commit>/` with `VERIFIED`.
   If the source cannot give the facts (or the tarball), setup says so and adopts the running release **as unverified** (step 4).
   The old `previous` is handled the same way if its commit can be fetched.
3. **Seed the replay memory and the floor.** With `release_source = "frontend"`, fetch the frontend's current (epoch, generation) for the
   desired release over the same source and write it into `state.json` as the highest acted on and into `min_generation` in `installer.toml`. With
   `release_source = "url"`, seed from the operator's `--min-generation` flag; without the flag setup warns plainly that replay memory starts
   from zero.
4. **Copy and repoint.** Unverified adoption copies the running directory into the new tree by descriptor: `openat` with
   `O_NOFOLLOW` for every component, `fstat` to accept only regular files and directories with one link, owner root, no setuid; no
   path is re-resolved. The copy has no `VERIFIED`, `status.json` flags "unverified: rollback to it refused", and setup prints it. It
   is never adopted silently and never marked verified without 7.2.
5. **Stop the service, repoint `current` and `previous`, rewrite the unit's `KBF_BIN_DIR`, start the service.** The service is
   stopped before the links move, so no process runs from a tree being replaced.

**Re-run rule.** Every step is idempotent and keyed by the commit: step 1 is a read; step 2 finds `releases/<commit>/VERIFIED` and
skips; step 3 only raises the memory and floor, never lowers it; step 4 skips a destination that exists with the same content, and
removes a half-copied one (no `VERIFIED`, not referenced); step 5 compares the links and units to the target and does what is
missing. A setup that crashed at any point is safe to run again; it never starts a service from a half-written tree, because the
service is only started after step 5 completes.

### 7.5 What is removed

- **The inbox** (`<generation>-<commit>/` entry directories holding `offer.json`, tarballs and attestation bundles, written by
  the service user) and the unprivileged `kbf`'s write access to any place the root side reads bytes from.
- **`kbf-update`'s staging role**: it no longer calls `ReleaseService.Fetch`, no longer writes tarballs, and no longer reports
  `staged` after a download. Its remaining job: relay offers and orders from the session, write order records, report status,
  refuse an order while a lease is live.
- **Staging under `/var/lib/kbf`**: the install tree moves to the root-owned `/var/lib/kbf-installer`.
- **The helper's staging pass** ("stage the newest inbox entry itself") and its no-network rule: the root tool now has a
  network path, confined to the fetch child.
- **The inbox tests** (symlink, FIFO and oversized entries): replaced by the fetch tests of section 10.

## 8. The rollout

The frontend alone decides order. It holds the **update slot** in the state store (compare-and-set, no time to live): one
storage holder plus up to `max_hosts_out` (default 1) holders for daemon-only hosts and Macs. If a holder dies its entry stays
held and the run goes `held`; a broken host never lets another take its place.

1. **Order.** The desired release rolls out to **storage and daemon nodes only**; the frontend is not part of it (section 1
   item 7). Nodes update asynchronously, in any order, each when it is idle or drained, driven by the frontend; every pair
   of versions in the farm is within N/N-1, so order does not matter for correctness. At most one storage-and-daemon node
   is in a step at a time (R=2); daemon-only hosts and Macs may be in a step concurrently up to `max_hosts_out` (default 1) and the capacity floor `min_serving`; a busy
   Mac is deferred, not killed. **Because the frontend no longer goes first on its own, the planner refuses to order a node to a
   release whose `[min_protocol, protocol]` the running frontend does not accept** (409, "update the frontend first"; it reads the same preflight row `update/available/<commit>` that fills the offer's expected facts, section 5), and refuses
   a node downgrade whose protocol the running frontend no longer accepts; the operator updates the frontend as a separate
   action and then the nodes. This replaces the "frontend first on an upgrade, nodes first on a downgrade" ordering of
   split-mode-updates.md 3.3, which assumed one rollout; that document needs the matching edit.
2. **Per host** (split-mode-updates.md 2.6, which is the authority): selection when idle, or the least-loaded candidate after
   `idle_wait`; cordon; wait for zero leases; on a storage node mark the member planned-down and wait for in-flight write
   sessions to reach zero; re-check the storage gate (every other member `Active`, aggregate repair debt zero); send
   `UpdateOrder{ACTIVATE}` ("update to commit `<hash>`"); the installer fetches, verifies, installs, flips and restarts the
   node (section 7); wait for registration, `Active` and repaired; send `UpdateOrder{CONFIRM}`; uncordon. **The fetch happens
   inside the drained step**, so the host is out of service for the download as well as the swap; the time cap of 7.1 bounds it.
   Pre-fetching a release while the host still serves is deferred (section 12).
3. **Failure**: an error after the swap rolls back, never retries. A fetch or verification that fails before the swap changes
   nothing on the host (section 7.3): it reports `REFUSED` with the reason, is uncordoned, and the run goes `held`. A host
   reverted by its own timer reports `REVERTED`; the run goes `held` and stays held until a person acts.
4. **Pause** stops granting between hosts, never mid-host. **Rollback** (a fleet walk-back) sets an older commit as the
   desired release. That write gets a **new, higher generation** from the store counter, so the installer accepts it: generation orders the
   decisions, not the releases, and the same gates and mechanism apply. Only a release below the operator's `min_generation`, or on the package's
   `deny` list, needs the local root `kbf-installer rollback` verb (section 7).
5. **Protocol skew and state schema**: split-mode-updates.md 3.3 (order rule) and 3.4 (the state-store schema is
   expand-then-contract; a contract change is refused by the planner and takes the cold path).
6. **The frontend updates only through Envoy, as its own step** (not as a step of the node rollout above). It is triggered by `POST
   /v1/update/frontend {commit}` (section 6.5), and Envoy is already installed on the host as a prerequisite and is not touched by the update.
   The installer on the frontend host takes the `frontend_update` order (section 7), fetches and verifies the release exactly as on a node
   (section 7.1 and 7.2), points the idle slot at it (the running instance keeps its release; `current` flips only at confirm), starts the other
   instance of `kbf-frontend@{a,b}` on its own ports, and waits for that instance's `/readyz`. The new process takes the state-store
   writer baton from the old one over the control socket (#879's protocol without the passed sockets), Envoy routes new connections to it, and
   the old instance **stops scheduling and relays its open `Execute` streams until they close, with no timeout** (an `Execute` stream that
   ended early is a build that fails; nothing is answered from `done/` rows and there is no timeout), and exits when they end. The installer **confirms** once the new instance is healthy and the old reports stood down,
   **A new instance that never becomes ready is stopped and the host reports `reverted`; the old instance is untouched and keeps serving.** After the
   old instance has stopped scheduling the successor is the only scheduler and there is no automatic revert: the way back is to update again with the
   older commit at a higher generation. The rollout **never waits for
   `open_executions == 0` and never restarts the serving frontend**. **Only one frontend update may be in flight**: the route answers 409
   (`frontend_busy`) while one is, and the installer refuses a second `frontend_update` order with `frontend_busy`. Steps E0 to E9, the writer options and the failure
   matrix are split-mode-updates.md 2.5 and 2.9.

## 9. The fleet view

Every host's running and installed version (`GET /v1/nodes`) and the desired release (`GET /v1/update`), read-only. All-in-one is the same
call against one machine.

## 10. Failure modes and their tests

Each safety property has a planted mutant that turns its test red.

- **Wrong bits**: a flipped byte of a tarball is refused at the hash; an attestation for another digest, repo,
  workflow or ref is refused; an order record that is a symlink, a FIFO or oversized is refused with no read outside
  the order spool.
- **The fetch child** (each with a planted mutant that turns the test red): a source that sends one byte more than `size`
  (killed, refused); that stalls (killed at the no-byte limit); that sends the right length of other bytes (refused at the digest
  even though the child exits 0); that exits non-zero after sending all the right bytes (judged on the bytes); a child that tries to
  create a file (fails: `RLIMIT_FSIZE`) or to read `/etc/kbf` (fails: not `kbf`'s user); the parent killed mid-fetch (scratch swept,
  no link changed, order runs again); a `url` source that redirects off its origin (refused); a configured source that is not
  `https` (setup refuses); an order whose digest differs from the attestation subject (refused at 7.2 step 2).
- **Installer properties** (each test names the planted mutant it must catch):
  - The sweep keeps `current` and `previous` targets without `VERIFIED` and flags them; mutant: the sweep deletes them (the
    running release disappears).
  - An unreferenced directory without `VERIFIED` is deleted; mutant: any directory counts as installed (an unverified rollback is taken).
  - Adoption: a failed re-fetch adopts as flagged unverified; mutant: adopts with `VERIFIED` (a rollback to unattested bytes).
  - (epoch, generation): a replay at the same generation is refused as a replay, an older epoch is refused whatever its
    generation, a generation more than 1000 above in one epoch is refused; mutants: compare generation alone; drop the cap.
  - Generation 0 refuses `activate`; mutant: treat 0 as "no check".
  - The operator floor refuses an order below `min_generation` and a release below `min_protocol` after the probe; mutant: floor
    ignored when the record is well formed.
  - Expected facts mismatch (protocol, `min_protocol`, `state_schema`, `profile`, platform), or absent: refused before any
    rename or link change; mutants: check after the flip; treat absent as match.
  - `activate` while a trial is pending is refused; mutant: allowed (two confirm timers).
  - Spool: a symlink swapped in after listing is not followed (`O_NOFOLLOW`); the 65th record is refused; a record is expired by
    root's first-seen time even if its mtime is set to now; mutants: `open` by path; mtime-based expiry.
  - Bounds: a `gh` stub that hangs, a probe that prints 1 MiB or prints malformed JSON, an extraction that never ends: each
    killed within its limit, whole process group, order refused; mutant: no wall clock.
  - The probe runs as `kbf-fetch` and cannot read `/etc/kbf` or the key; mutant: run as `kbf` or root.
  - Redirect: any 3xx fails the fetch; mutant: follow same-origin redirects.
  - Lease source: a record claiming no leases while root's own observation (cgroup or launchd and uid) shows a live lease is refused; mutant: trust the record.
- **Verb** (planted mutant in brackets): a wire order, from a fake server that adds a field naming `frontend_update`, becomes an `activate`/`confirm`/`revert`
  record or is refused, never a `frontend_update` record [kbf-update copies a verb from the order]; an `activate` record never starts a `kbf-frontend@`
  unit [dispatch on the unit named by the record]; a `frontend_update` record on a host whose mode is not `frontend` or `all-in-one` is refused
  [dispatch ignores the host mode]; a second `frontend_update` order while one is in flight is refused `frontend_busy`, and the route answers 409 [no
  in-flight guard]; a `frontend_update` record below the floor, with a bad attestation or off the allowed ref is refused [frontend path skips 7.2];
  the old instance's open `Execute` stream is relayed to its end, with no timeout, while the new instance serves new ones [stream cut at a deadline];
  a new instance that never reports `/readyz` is stopped and `reverted`, slots and `current` unchanged [no readiness wait].
- **Capacity**: two hosts ordered at once (the slot admits one storage holder and at most `max_hosts_out` others); storage under-replicated or the metric absent (no
  order); a host with live leases (order withheld, and the node refuses it too).
- **Bad binary**: starts and exits at once (the counter reverts, Linux); runs but never registers (the window
  expires, the host reverts, the run goes `held`); the frontend dies in the window (the host reverts); `previous`
  missing (the host stays, reports `REFUSED`).
- **Crash story**: the helper killed at each step of `activate` leaves the links at the old or the new state, never a mix; power loss between
  rename and directory sync leaves one whole state. The service restart owed after a revert or a window expiry is not yet guaranteed to survive a crash
  between the link move and the restart; #929 persists the restart-owed before the link move, and until it lands the guarantee is the links only.
- **Frontend**: a restart after an order resumes from the host's recorded phase and does not stage again; a replayed
  order id is a no-op and a lower epoch is refused.
- **Store**: the release store is immutable per (commit, platform, name); a crash between writing and renaming
  leaves no component; traversal segments touch nothing outside the store.

## 11. Units

| Unit | What |
|---|---|
| wire and store (this contract) | `update.proto`, `ReleasePlatform`, the release store, the desired release, `/v1/update`, `/v1/releases`, update view in `/v1/nodes`, offers over the session, `ReleaseService.Fetch` |
| node client | status reporting, refuse with live leases, relay of offers and orders as order records (no fetch, no staging) |
| installer core | versioned directories, the fetch child (7.1), the root-side checks (7.2), offline attestation check, commit-confirm, revert, retention |
| rollout controller | the slot, the per-host step, the gates, resume, pause, the frontend switch through Envoy (split-mode-updates.md) |
| fleet view | read-only `/v1/nodes` and `/v1/update` |
| bootstrap and rehearsal | the host-side scripts a human runs once per host; rehearsal on development nodes |

## 12. Deferred

- **Deferred:** an update banner or button, the "update available" notification and UI login; the desktop app; cloud update providers; cross-version Envoy upgrades; any pinning of Envoy or a release manifest in kbf releases; a managed build service; Postgres and several frontends.
- The Ed25519 release key and a signed floor (section 2).
- Sigstore verification beyond the attested-`main` rule (a protected `main`, required reviews).
- Closing the early-order gap of an order-record writer under the native driver (the signed grant above does).
- Pre-fetching a release into `releases/<commit>/` while the host still serves (it needs a second, non-activating order verb, and the same verification).
- A frontend-signed grant on each order, verified before any fetch (section 7); its key and rotation are undesigned.
- A seccomp filter and a macOS sandbox profile for the fetch child; a separate download credential in place of the lent node identity; blocking the probe and `gh` sockets (network namespace or sandbox).
- Maintenance windows, percentage waves, tracks beyond canary plus stable.
- Several frontends on a shared database (Postgres). A zero-impact update of one frontend does not need it: it is the Envoy switch of section 8.
- OS, Xcode, VM-image and MDM updates ([fleet-updates.md](fleet-updates.md) owns those).
- Updating the helper, the units, the sudoers entry and the pinned trust material: manual by design.
