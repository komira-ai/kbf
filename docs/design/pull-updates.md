# Pull-based updates: hosts update themselves over the session they already hold

Status: design; the wire contract (section 5) is frozen. Built or in review: `kbf-installer` (merged), `kbf-update` (the node client, #908) and `kbf-helper` (the root helper, #929). The rest is built in the units of section 11.
**Revised for two rulings and for the installer contract.** (1) Node updates and the frontend's own update are decoupled (section 1 item 7, section 8). (2) The host's root tool pulls the release itself, in an unprivileged child, on the frontend's order; kbf stages no release bytes and there is no inbox. [kbf-installer.md](kbf-installer.md) (komira-build-farm#1025) is the **authority for everything on the host**: the installer's CLI, file roots and ownership, the order handover, the fetch child, the verification order, the release manifest, the floors and the units. This document keeps the frontend side, the rollout, the preflight and the wire it reuses, and **points at the contract instead of restating a host rule**. The contract makes **no wire change**. Where a built crate (`kbf-update`, `kbf-helper`) still has the inbox or the order spool, the contract is the target and section 7 lists what is removed. `kbf-helper` is folded into `kbf-installer`; where later text still says "the helper", it means the root side of `kbf-installer`.
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

1. An operator (or an auto-update rule the operator set) names a **desired release**: one commit of this
   repository's `main`, whose build attestations are on GitHub. The frontend stores it.
2. The frontend tells a node what to do over the **mTLS session it already holds**: an `UpdateOrder` for that node. `UpdateOffer` is
   still sent after `Welcome` and at every change, for the "update ready" display and for the `frontend` release source; **nodes do not
   act on offers** ([kbf-installer.md](kbf-installer.md) section 8). No new inbound port, no SSH, no CI credential that reaches a host.
3. The order is "update to commit `<hash>`": `UpdateOrder{ACTIVATE}` over the session, carrying the commit, the epoch and the order id
   (the wire is unchanged). The unprivileged `kbf` only forwards it, as a small order record in the kbf-owned order spool, to the root `kbf-installer`
   ([kbf-installer.md](kbf-installer.md) section 4 and section 7 here). **No release bytes pass through `kbf` and there is no inbox.**
4. `kbf-installer` **pulls the release for that commit itself**, from the release source in its own config only, in an
   **unprivileged child** (no root while bytes arrive), and the root parent verifies the attested release manifest and each tarball
   before anything is unpacked or run ([kbf-installer.md](kbf-installer.md) section 5 has the order of checks). Only then does anything become root-owned.
   It never overwrites a running binary.
5. The **frontend alone orders** the rollout: for a host that is idle or drained it runs the per-host step of
   split-mode-updates.md 2.6 (cordon, wait for zero leases, on storage nodes mark the member planned-down, wait for
   write sessions and for the repair gate), then sends `UpdateOrder{ACTIVATE}`. The installer fetches, verifies,
   installs, flips `current` and restarts.
6. The new process registers; the frontend checks health and sends `UpdateOrder{CONFIRM}`. If confirmation does
   not arrive in the window, or the unit crash-loops, the installer reverts locally (commit-confirm).
7. **Node updates and the frontend's own update are decoupled.** "Update ready" (the desired release, set in the UI,
   the desktop app or the API) updates **storage and daemon nodes only**, through the drain system: **nodes update
   asynchronously, in any order, when idle or drained** (at most one storage node at a time; daemon-only hosts and Macs
   concurrently up to `max_hosts_out`, default 1; a busy Mac is deferred). The frontend is not in that rollout. Its own
   update is a **separate action**: the Envoy swap and handover (the new instance starts on the other port, Envoy routes
   new connections to it, the old one drains and keeps its in-flight `Execute` streams until they complete; it never
   waits for `open_executions == 0` and never restarts the serving frontend). Details: split-mode-updates.md 2.5, 2.6
   and 3.3, and section 8 below for what decoupling changes.
8. Rollback of the desired release is a **higher generation** naming an older commit. The installer refuses an order below its
   release floor (`release_seq`, [kbf-installer.md](kbf-installer.md) section 5 step 4), so a fleet rollback to an older release is **not yet specified end to end**:
   section 8 item 4 states the open point.
9. Only the unprivileged `kbf` binary is ever pulled. `kbf-installer`, the unit files and the pinned trust material change only by
   the package ([kbf-installer.md](kbf-installer.md) section 1). Root code is never auto-updated, and the code that downloads runs without root.
10. The operator sees every host's version, an "update available" notification, a button to update, and an
    optional auto-update rule. All-in-one is the same button on one machine.

## 2. Trust model (MVP decision)

**Hosts trust the GitHub build attestation of this repository's `artifacts` workflow, verified offline on the host** with a pinned `gh` and
a pinned trusted root, for the release manifest and for each tarball, bound to the order's commit. The exact flags, the allowed source refs
(held only in the package's trust files), the `deny` list and the floors are [kbf-installer.md](kbf-installer.md) section 5 and 10 (S3, S4, S7); this document does not
restate them. The facts a node is checked against (`protocol`, `min_protocol`, `state_schema`, `release_seq`, the asset digests) come from
the **attested manifest**, not from the offer or the order.

The frontend **only stores and distributes** the desired commit and the release bytes. It is **not trusted for integrity**, and neither are
the release source or the fetch child: the root parent verifies, and installs nothing else.

| Actor | Can do | Cannot do |
|---|---|---|
| GitHub / CI | build, attest, publish | reach a host, push to the frontend |
| Compromised **frontend** (or a mirror named as the release source) | choose when and in what order; withhold; serve any bytes or any older attested build; deny service | make a host install a binary that lacks a valid `main` attestation of this workflow; run anything as root |
| Compromised **fetch child** (a parser or TLS bug while bytes arrive) | write bytes to its download directory; stall until the time cap. **It never holds the node identity**: the `frontend` source goes through the local fetch proxy (P15), which checks the peer and serves only `Fetch` by sha256 ([kbf-installer.md](kbf-installer.md) S11), so there is no stolen-key residual | reach any file of `kbf` or root, make the parent install bytes that fail the checks of [kbf-installer.md](kbf-installer.md) section 5 |
| Compromised **service user** (or an action under the native driver); **on macOS `kbf` runs as root, so there a compromise of it is a compromise of root** | drop, delay or burn order ids; send a well-formed order for an attested release (an early or stale activation, bounded by the floors) | install an unattested binary; put bytes on the install path (the spool carries only order records, read once with `O_NOFOLLOW`); reach the operator control channel; touch the releases tree, the installer or the units ([kbf-installer.md](kbf-installer.md) S1 and S2) |
| Anyone who can push to `main` | get an attested build | (see residuals) |

Residuals, stated plainly:

- **`main` has no branch protection.** Anyone who can push to it gets an attested build, and so a binary every
  host accepts when the frontend offers it. The attestation proves *where a binary was built*, not *that a
  reviewer approved the source*.
- **A hostile frontend can order any older attested build.** The guards are the installer's: the order rule (epoch, then id), the
  `release_seq` floor, the manifest's `min_safe_seq`, the package's `deny` list and `min_release_seq`, and the operator's
  `min_generation` ([kbf-installer.md](kbf-installer.md) section 4, 5 and S3). Only the operator's local `rollback` verb goes below the floor.
- **Test-farm switch.** The held installer for the test farm temporarily runs with attestation verification off (to be
  added back). That is a test-farm setting only; no deployed default and no part of this contract.
- **The binary holds credentials.** On a storage-and-daemon host the `kbf` process holds the cold-store
  credential, so the set of commits that can be attested is the set of programs that can read it.
- **The design's separate Ed25519 release key is deferred.** A release key held off CI and off the shared session
  account would close the first two residuals (a human signs; a floor can be signed in). It stays a documented
  later option; nothing in the MVP wire prevents adding it.

## 3. What exists (and where this builds on it)

- **Artifacts** [F]: one `kbf-<commit>-<platform>.tar.gz` per platform holding one binary, `SHA256SUMS`, SBOMs,
  and a build-provenance attestation per tarball, on pushes to `main`. `kbf --version --json` prints `{commit,
  min_protocol, protocol, profile, state_schema, version}`. A release is verified, not rebuilt.
- **Versioned host layout** [F]: `/usr/local/kbf/releases/<commit>/`, `current` and `previous` symlinks, a
  `state.json` with a pending confirm (and an order-floor field kept only so an older state file reads); `kbf-deploy host install|rollback|gc`. The units run
  `<KBF_BIN_DIR>/kbf <mode>`, so pointing `KBF_BIN_DIR` at `current` needs no new unit shape.
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
 operator / UI               frontend host                              every other host
 -------------               -------------                              ----------------
 /v1/update, /v1/releases -> kbf frontend                               kbf (daemon / storage-and-daemon)
 (click, auto rule)            release store (--releases-dir)             update client (kbf-update)
                               desired release, rules (state store)         | forwards orders, reports status
                               offers + orders over sessions                | order spool (kbf-owned, small records)
                               rollout controller                           v
                                                                          kbf-installer serve (root)
                                                                            fetch child -- download dir --> root parent
                                                                            releases tree, commit-confirm
```

| Piece | Where | Notes |
|---|---|---|
| Wire messages | `crates/kbf-proto/proto/kbf/update/v1/update.proto` | messages and one service; frozen (section 5) |
| Platform names | `kbf_types::ReleasePlatform` | `linux-x86_64`, `linux-arm64`, `darwin-arm64`; from a node's `os` and `arch` report entries |
| Release store, desired release, offers, `ReleaseService` | `kbf-server` | section 6 |
| Update client | `kbf-update` (the node's side, unprivileged) | writes each order as a record in the order spool, reports status. It fetches nothing and stages nothing |
| Installer (fetch in a child, verify offline, install, flip, revert) | `kbf-installer`, a root unit on each host; `kbf-helper` is folded into it | everything on the host: [kbf-installer.md](kbf-installer.md) (section 7 here lists the pointers) |
| Rollout controller | `kbf-server`, replaces the older record in `rollout.rs` | section 8 |
| Release discovery | the frontend polls GitHub, read-only | proposes a desired release; never applies |
| Fleet view | the UI, over `/v1/nodes` and `/v1/update` | section 9 |

## 5. The wire contract (frozen)

`kbf.update.v1`, in `crates/kbf-proto/proto/kbf/update/v1/update.proto`, is the contract; this section states the
rules the field comments cannot. Numbers in `kbf.worker.v1`: `DaemonMessage.update_status = 7`,
`ServerMessage.update_offer = 8`, `ServerMessage.update_order = 9`. `UpdateStatus` is its own message because
`NodeStatus` is sent once per `Welcome` and an update changes more often.

**Component.** One tarball, `<name>-<commit>-<platform>.tar.gz`, holding exactly one regular file named `<name>`.
`name` is `kbf` or `kbf-daemon`. `sha256` is 64 lowercase hex digits of the tarball and is also its name in
`ReleaseService.Fetch`. `attestation` is the build-provenance bundle (JSON lines, at most 256 KiB) of the tarball.

**Which components a platform needs.** `linux-x86_64`, `linux-arm64`: `kbf`. `darwin-arm64`: `kbf-daemon`. A
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
id)**, by the node's client and again by the installer, which acts on each pair once: a higher epoch is newer whatever its
id, an older epoch is refused whatever its id, the same epoch with a higher id is newer, and the same pair again is a replay. An
epoch or id more than 1000 beyond the installer's floor is refused (best effort: it blunts a typo or a stray huge value, it does not
stop a forger who steps by 1000). The activation is named `<epoch>-<order_id>`. `window_s` is an input the installer clamps, and its
per-platform meaning (Linux: the window starts at the first start; macOS: armed at activation) is [kbf-installer.md](kbf-installer.md) section 4.

**UpdateStatus.** Sent after `Welcome` and on every change. `running_commit` is the process's own build commit
(12 hex or `unknown`); `installed_commit` and `staged_commit` are 40 hex from the installer, empty if not known.
`acked_order_id` is the newest order acted on; `offered_generation` the newest offer seen. The installer's `status.json` states map one to one
onto `UpdateStatus.State` ([kbf-installer.md](kbf-installer.md) section 4; `fetching` is the installer pulling the release for an order). `helper_present` (field 8) now means the
installer is present. `staged_protocol`, `staged_min_protocol` and `staged_state_schema` (fields 9 to 11) are what the newest installed, not
yet active, release declares, taken from its **attested manifest** and checked against the binary's own `--version --json`; 0 when none.
Because nothing is staged before the order, the frontend's ordering decision (split-mode-updates.md 2.4 and 3.3) reads the same facts from the
preflight row `update/available/<commit>`, which is built from the manifest (section 6.2); the controller compares the host's report with that
row before it sends `CONFIRM`. A node that never sends it is listed `unsupported`, and a rollout halts at it (it never skips a host silently).

**ReleaseService.Fetch.** On the worker listener, admitted nodes only (mutual TLS). The argument is 64 lowercase
hex digits and nothing else; an unknown digest is `NOT_FOUND`. The server never builds a path from anything but the
validated digest. Chunks are at most 1 MiB. **The caller is the installer's local fetch proxy (P15), on behalf of the fetch child**, so the child
never holds the node identity. Until P15 exists a farm uses the `url` source ([kbf-installer.md](kbf-installer.md) section 5).

**The order, as the node forwards it.** `UpdateOrder` is **unchanged** and carries `order_id`, `epoch`, `verb`, `commit` and
`window_s`. The "update to commit `<hash>`" order is `VERB_ACTIVATE`. **There is no wire change**: no `generation` on the order and no new
offer fields. Monotonicity comes from the manifest's `release_seq` and the installer's floors ([kbf-installer.md](kbf-installer.md) section 5 step 4 and S3), and the expected
facts come from the attested manifest. The node writes the order body as it is into the order spool; it adds nothing and needs no offer for the commit.
The ordering rule is below; the spool's rules are section 7.

**Compatibility.** A server that predates these fields ignores `update_status`; a node that predates them ignores
the offers and orders, never sends a status, and is listed `unsupported`.

### Contracts owned by the node client and the installer

Two small things cross from the unprivileged `kbf` to the root `kbf-installer`: the **order record** in the order spool (section 7;
its body is the wire order of [kbf-installer.md](kbf-installer.md) section 4, nothing more) and the world-readable `status.json` the installer writes (schema in [kbf-installer.md](kbf-installer.md)
section 4). This document does not restate their fields so that there is one source. **There is no inbox**: no offer directory, no
tarball, no attestation bundle is written by `kbf`, and no entry the unprivileged process creates names a path, a URL or a unit.

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
| `update/policy` | `{auto_nodes (bool), auto_frontend (bool), paused (bool), auto_frontend, idle_wait, min_serving, max_hosts_out (default 1), mac_force_after, new_work_only_to_updated, accept_failing_builds}`; replaces the older `update/auto {enabled}`, whose flag is `auto_nodes` |

**A release upload triggers its preflight.** The release's attested manifest (`kbf-<commit>-manifest.json`, [kbf-installer.md](kbf-installer.md) section 5) is uploaded to
the store with its bundle, and the `PUT` of the last asset the manifest lists for a platform starts the preflight for the commit. The preflight
reads the manifest (it does not trust it; the host verifies) and writes the `update/available/<commit>` row: `release_seq`, `min_safe_seq`,
`protocol`, `min_protocol`, `state_schema`, the platforms it is complete for, and the result. **Until the row exists the frontend does not
offer the release and cannot set it as the desired release.** The operator sees it as "preflight pending", or "preflight failed" with the
reason. The planner's "release complete" check and the 409 of section 8 item 1 read this row, so the frontend and the host judge the same facts.

**The disk index is the source of truth for bytes.** The release store on disk (`files/<sha256>` and
`index/<commit>/<platform>/<name>`, with attestation bundles) is authoritative for what the frontend can serve. The
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
- `POST /v1/update/desired {commit}`: the commit needs at least one complete platform (else 409). Compare-and-set
  on `update/desired`; the counter gives the new generation only when the commit differs from the current one.
- `GET /v1/update`: `{desired, auto_update, releases}`. `POST /v1/update/auto {enabled}` sets `update/policy.auto_nodes`; the other policy fields are set through `POST /v1/update/policy` (admin).
- `GET /v1/nodes` gains, per node, an `update` object: state, the three commits, offered generation, refusal,
  `helper_present`, `up_to_date` (true, false or null) and `availability` (`ok`, `unavailable`, `unsupported`).

### 6.4 Sessions

After `Welcome`, and to every connected node at every change of the desired release or of a platform's
completeness, the frontend sends an `UpdateOffer` for that node's platform. `UpdateStatus` is kept as the newest per
node and ignored from a replaced stream, exactly as `NodeStatus` is.

## 7. The host

Everything on the host is owned by [kbf-installer.md](kbf-installer.md). This section keeps only what this design asks of it, and the order spool, which is the handover
from `kbf` to the installer in this design.

| Rule | Owner |
|---|---|
| CLI, exit codes, `setup`, `serve`, `update`, `confirm`, `revert`, `rollback`, `gc`, `boot-check`, `quiesce` | [kbf-installer.md](kbf-installer.md) section 2 |
| Roots (`/var/lib/kbf`, `/var/lib/kbf-installer`, package files), ownership, `state.json`, `installer.toml`, users | [kbf-installer.md](kbf-installer.md) section 3 |
| Fetch child, release source, caps, the verification order, the manifest, `release_seq`, the floors, the refusal words | [kbf-installer.md](kbf-installer.md) section 5 and 10 |
| Node update sequence, commit-confirm, the frontend update | [kbf-installer.md](kbf-installer.md) section 6 |
| Units and gating | [kbf-installer.md](kbf-installer.md) section 7 |

**Release source and floors.** They are in `/etc/kbf/installer.toml` (root-owned, unknown keys refused), written only by `kbf-installer
setup`, never taken from an order, an offer, the environment or `kbf.toml` ([kbf-installer.md](kbf-installer.md) section 3). `release_source` is `url` (an HTTPS base;
**at most 3 redirects, never to http**, no proxy variables) or `frontend` (`ReleaseService.Fetch` by sha256 through the local fetch proxy
(P15), so **the fetch child never holds the node identity**). The operator floor is `min_generation`; the package's trust files hold
`min_release_seq` and the `deny` list. This replaces an `[update]` table in `kbf.toml`: `kbf.toml` has no update keys.

**The order spool.** The handover from `kbf` to the installer is a directory, `/var/lib/kbf-installer/orders/`, owned by `kbf` with mode
0700 inside the root-owned installer tree. Root opens it with `openat` and `O_NOFOLLOW` relative to a descriptor it holds, accepts an
entry only if `fstat` says a regular file with one link, reads it **once into memory**, parses from that copy, and removes it through
the same descriptor. At most 64 records are held, and a record is expired by **root's first-seen time** (never the file's mtime) after the maximum
confirm window. A record is at most 4 KiB and is exactly the wire order of [kbf-installer.md](kbf-installer.md) section 4: `{order_id, epoch, verb, commit, window_s}`,
unknown and duplicate keys refused. **It carries no components, no digest, no attestation and no expected facts**: the installer takes
the platform from itself and everything else from the attested manifest. Which channel is authority, the ordering rule and the refusals
(`pending`, `replay`, `downgrade`, `denied`) are [kbf-installer.md](kbf-installer.md) section 4 and 5. A record that is a symlink, a FIFO, a hardlink, oversized or unparsable is
refused and removed. Engineering amends contract section 4 to match this spool.

**What this design asks the contract to state** (each is a security point of the reviews of this document that the contract does not yet
carry; they move into [kbf-installer.md](kbf-installer.md) and are deleted here when it does):
1. **Bounded root steps.** Every step root runs (`gh`, extraction, the `--version --json` probe) has a wall-clock limit, its process
   group killed on expiry, output read up to 4 KiB and parsed strictly; `gh` runs with an empty environment and `HOME`.
2. **Lease check by root's own observation.** Before an `ACTIVATE` the installer observes leases itself: on Linux the lease processes'
   cgroup (membership of the systemd scope or unit that runs actions); on macOS the launchd job and the processes by uid. It reads no file
   `kbf` or the daemon writes.
3. **The probe has no credentials.** `--version --json` runs as `kbf-verify` (contract) with no `kbf` credential; "no network for the probe
   and `gh`" is not enforced in v1.
4. **The sweep never removes `current` or `previous`.** A referenced directory without `VERIFIED` stays and `status.json` flags "unverified:
   rollback to it refused"; an unreferenced one without it is removed.
5. **Adoption of an existing host** (`setup`, P3 and P10): (a) the running commit comes **only** from `--version --json` of the installed binary
   under bounds, never from state `kbf` can write; (b) the source's metadata for that commit is an untrusted claim re-derived by the verification
   order, and if it cannot be had the release is adopted **as unverified**, flagged and printed, never silently; (c) the floor is seeded from
   the frontend's current release for `source = frontend`, or from an operator `--min-generation` flag, or setup warns plainly that it starts from
   zero; (d) the copy is by descriptor (`openat`, `O_NOFOLLOW`, `fstat`: regular files and directories, one link, owner root, no setuid);
   (e) the service is stopped before the links move; every step is idempotent and a crashed setup is safely re-run.
6. **Pending trial.** `ACTIVATE` is refused while a trial is pending (contract section 4 has this; kept here as a test row).
7. **macOS.** `kbf daemon` runs as root there, so a compromise of `kbf` is a compromise of root; the contract's separation holds on Linux only.
8. **Trust-root expiry.** The pinned `trusted_root.jsonl` expiry is reported in `status.json` and the frontend alerts at fewer than 30 days
   (contract `check` covers expiry; the alert is this design's).

### 7.1 What is removed

- **The inbox** (`<generation>-<commit>/` entry directories holding `offer.json`, tarballs and attestation bundles, written by the
  service user) and the unprivileged `kbf`'s write access to any place the root side reads bytes from.
- **`kbf-update`'s staging role**: it no longer calls `ReleaseService.Fetch`, no longer writes tarballs, no longer reports `staged` after a
  download, and no longer relays offers to the installer. It writes order records and reports status.
- **The helper's staging pass** and its no-network rule: the root tool has a network path, confined to the fetch child.
- **The `[update]` table in `kbf.toml`**, the order-record components, `generation` on the order and the expected-facts fields on the offer
  (earlier drafts of this document): not part of the design.
- **The inbox tests** (symlink, FIFO, oversized entries): replaced by the spool tests of section 10.

## 8. The rollout

The frontend alone decides order. It holds the **update slot** in the state store (compare-and-set, no time to live): one
storage holder plus up to `max_hosts_out` (default 1) holders for daemon-only hosts and Macs. If a holder dies its entry stays
held and the run goes `held`; a broken host never lets another take its place.

1. **Order.** The desired release rolls out to **storage and daemon nodes only**; the frontend is not part of it (section 1
   item 7). Nodes update asynchronously, in any order, each when it is idle or drained, driven by the frontend; every pair
   of versions in the farm is within N/N-1, so order does not matter for correctness. At most one storage-and-daemon node
   is in a step at a time (R=2); daemon-only hosts and Macs may be in a step concurrently up to `max_hosts_out` (default 1) and the capacity floor `min_serving`; a busy
   Mac is deferred, not killed. **Because the frontend no longer goes first on its own, the planner refuses to order a node to a
   release whose `[min_protocol, protocol]` the running frontend does not accept** (409, "update the frontend first"; it reads the same preflight row `update/available/<commit>` that section 6.2 builds from the attested manifest), and refuses
   a node downgrade whose protocol the running frontend no longer accepts; the operator updates the frontend as a separate
   action and then the nodes. This replaces the "frontend first on an upgrade, nodes first on a downgrade" ordering of
   split-mode-updates.md 3.3, which assumed one rollout; that document needs the matching edit.
2. **Per host** (split-mode-updates.md 2.6, which is the authority): selection when idle, or the least-loaded candidate after
   `idle_wait`; cordon; wait for zero leases; on a storage node mark the member planned-down and wait for in-flight write
   sessions to reach zero; re-check the storage gate (every other member `Active`, aggregate repair debt zero); send
   `UpdateOrder{ACTIVATE}` ("update to commit `<hash>`"); the installer fetches, verifies, installs, flips and restarts the
   node ([kbf-installer.md](kbf-installer.md) section 5 and 6); wait for registration, `Active` and repaired; send `UpdateOrder{CONFIRM}`; uncordon. **The fetch happens
   inside the drained step**, so the host is out of service for the download as well as the swap; the fetch timeout of [kbf-installer.md](kbf-installer.md) section 5 bounds it.
   Pre-fetching a release while the host still serves is deferred (section 12).
3. **Failure**: an error after the swap rolls back, never retries. A fetch or verification that fails before the swap changes
   nothing on the host ([kbf-installer.md](kbf-installer.md) section 6): it reports `REFUSED` with the reason, is uncordoned, and the run goes `held`. A host
   reverted by its own timer reports `REVERTED`; the run goes `held` and stays held until a person acts.
4. **Pause** stops granting between hosts, never mid-host. **Rollback** of the desired release to an older commit is a higher
   generation with the same gates and mechanism, **but the installer refuses an order whose `release_seq` is below its floor** ([kbf-installer.md](kbf-installer.md)
   section 5 step 4), and only the operator's local `rollback` verb goes lower. So a fleet-wide rollback by the desired release works only to a
   release at or above the floor. How a fleet rolls back below it (an operator lowering the floor on every host, or the per-host `rollback`
   verb driven by the controller) is **open**, and is raised with engineering.
5. **Protocol skew and state schema**: split-mode-updates.md 3.3 (order rule) and 3.4 (the state-store schema is
   expand-then-contract; a contract change is refused by the planner and takes the cold path).
6. **The frontend updates only through Envoy, as its own action** (not as a step of the node rollout above; the operator or the
   `auto_frontend` rule starts it). The installer on the frontend host fetches and verifies the release exactly as on a node
   ([kbf-installer.md](kbf-installer.md) section 5 and 6) and points the idle slot at it
   without restarting the serving instance, then starts the other instance of `kbf-frontend@{a,b}` on its own ports. The new
   process takes the state-store writer baton from the old one over the control socket (#879's protocol without the
   passed sockets), Envoy routes new connections to it, and the old instance fails its health check, sends GOAWAY, answers its
   open `Execute` and `WaitExecution` streams from the `done/` rows, and exits when they end. The rollout **never waits for
   `open_executions == 0` and never restarts the serving frontend**; the confirm window starts when the new instance is
   serving. Steps E0 to E9, the writer options and the failure matrix are split-mode-updates.md 2.5 and 2.9.

## 9. The fleet view

Every host's running and installed version (`GET /v1/nodes`), an **update available** notification (the frontend
polls GitHub, read-only, for the newest attested artifacts run on `main` and proposes it as a release), a **button**
that sets the desired release, and an optional **auto-update rule** (`POST /v1/update/auto`). The frontend applies a
release only on the click or the rule. All-in-one is the same button on one machine.

## 10. Failure modes and their tests

Each safety property has a planted mutant that turns its test red.

- **Wrong bits and the fetch**: tests are the contract's (P4 to P10, [kbf-installer.md](kbf-installer.md) section 9): a flipped byte, an attestation for another
  digest, repo, workflow or ref, a size or time overrun, a redirect to http, a fourth redirect. Mutants are planted there.
- **The spool** (planted mutant in brackets): a symlink swapped in after listing is not followed [open by path]; a FIFO, hardlink or
  oversized record is refused [no `fstat` check]; the 65th record is refused [no count cap]; a record is expired by root's first-seen time even
  if its mtime is set to now [mtime expiry]; a record with a component list or unknown key is refused [lenient parse].
- **Decoupling**: the desired release changing never sends an order to a frontend unit or to `kbf-envoy@*` [mutant: the node rollout
  includes the frontend host]; a frontend update does not start a node order [mutant: shared slot].
- **Preflight**: a release with no `update/available/<commit>` row is not offered and cannot be set as desired [mutant: offer on upload];
  a failed preflight reads "preflight failed" with the reason [mutant: shows pending forever]; the 409 and the offer read the same row
  [mutant: a second copy of the facts].
- **Requirements of section 7**: each of items 1 to 5 has a test with a planted mutant (a `gh` stub that hangs, a probe that prints 1 MiB; a
  record claiming no leases while the cgroup shows one; the sweep deleting `current`; adoption marking `VERIFIED` after a failed re-fetch).
- **Capacity**: two hosts ordered at once (the slot admits one storage holder and at most `max_hosts_out` others); storage under-replicated or the metric absent (no
  order); a host with live leases (order withheld, and the node refuses it too).
- **Bad binary**: starts and exits at once (the counter reverts, Linux); runs but never registers (the window
  expires, the host reverts, the run goes `held`); the frontend dies in the window (the host reverts); `previous`
  missing (the host stays, reports `REFUSED`).
- **Crash story**: the installer killed at each step of `activate` leaves the links at the old or the new state, never a mix; power loss between
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
| node client | status reporting, writing order records to the spool (no fetch, no staging) |
| installer | everything on the host: [kbf-installer.md](kbf-installer.md) section 9 (P0 to P15) |
| release source | the frontend's read-only poll of GitHub for the newest attested `main` artifacts |
| rollout controller | the slot, the per-host step, the gates, resume, pause, the frontend switch through Envoy (split-mode-updates.md) |
| fleet view | the UI page and the auto-update rule |
| bootstrap and rehearsal | the host-side scripts a human runs once per host; rehearsal on development nodes |

## 12. Deferred

- The Ed25519 release key and a signed floor (section 2).
- Sigstore verification beyond the attested-`main` rule (a protected `main`, required reviews).
- Pre-fetching a release while the host still serves (it needs a second, non-activating order verb, and the same verification).
- A frontend-signed grant on each order, verified before any fetch; its key and rotation are undesigned.
- Blocking the probe's and `gh`'s sockets (network namespace or sandbox profile).
- A fleet rollback below the release floor (section 8 item 4).
- Maintenance windows, percentage waves, tracks beyond canary plus stable.
- Several frontends on a shared database (Postgres). A zero-impact update of one frontend does not need it: it is the Envoy switch of section 8.
- OS, Xcode, VM-image and MDM updates ([fleet-updates.md](fleet-updates.md) owns those).
- Updating the installer, the units and the pinned trust material: by the package only.
