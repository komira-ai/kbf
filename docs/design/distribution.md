# Distribution: kbf, kbf-installer and the package

Status: design of record. Nothing here is built unless a sentence says so. Labels: **[V]** verified in
a primary source or in the repo, **[L]** likely (inference, or memory of a primary source), **[U]**
unverified or not yet designed.

This document replaces the distribution parts of `install-and-enrollment.md`
(where that document is present: it is in review) and the host layout of
[../deploy/host-releases.md](../deploy/host-releases.md). Updates themselves are specified in
`pull-updates.md` (wire, trust, release store) and [split-mode-updates.md](split-mode-updates.md)
(order of a rollout, the frontend's own update); this document says who runs them on a host. The
normative contract for `kbf-installer` (CLI, files, orders, fetch and verification, units) is
[kbf-installer.md](kbf-installer.md) (#1025); where this overview and that document differ, that document wins.
Scope: on-prem only for now. What is deferred is listed in section 10.

## 1. The three things

| Thing | What it is | What it does | What it never does |
|---|---|---|---|
| `kbf` | The runtime only. One binary: `kbf --mode=frontend\|storage\|daemon\|storage-and-daemon\|all-in-one`, or `mode = "..."` in `/etc/kbf/kbf.toml` (a flag overrides the file). `--config`, `--check` and `--instance` are flags. No positional arguments, no `node` mode; the old subcommand forms are aliases until the test farm has moved, then are removed | Runs the mode. At start it checks its host and refuses with one clear line per missing piece. Enrollment runs inside the start of the `storage`, `daemon` and `storage-and-daemon` modes; the token limits which roles a host may hold. The frontend creates its CA on first start into an empty `--ca-dir` | Install, download or configure anything. It has no `enroll`, `ca` or `token` command: tokens are operator-API routes |
| `kbf-installer` | THE package, and the one root tool for setup and updates of kbf | Setup: writes `kbf.toml` (mode, frontend URL) and places the enrollment token from `--token-file` (0600 root:kbf), pulls the `kbf` release itself (an unprivileged fetch child, the configured source only, verified before anything is root-owned), places and enables units. Then runs as a service: reads orders from the kbf-owned `orders/` directory (dot-temp name, then rename), keeps its state under the root-owned `/var/lib/kbf-installer`, reads settings from `/etc/kbf/installer.toml` | Update itself (only the package manager replaces it), fetch or pin Envoy, decide to update |
| The package | The way `kbf-installer` reaches a host | Ships the `kbf-installer` binary, unit files (installed disabled, gated), and the trust files. Has no package `Depends:` on Envoy: setup and `check` test the installed Envoy on a frontend host (section 5) | Start a service, own anything under `/var/lib/kbf-installer/releases` |

**kbf refuses, it does not repair.** The start-up checks are the preflight of PR #1020 (branch
`startup/self-checks`): each missing piece (a directory, a user, a subuid range, a runtime, a token file) is
one line naming the piece and what provides it. The same checks run as `kbf --mode=<mode> --check`, a flag and not a
subcommand, which is what the unit gate calls (section 4).

**Releases.** `/var/lib/kbf-installer/releases/<sha>/` holds one release (the `kbf` binary, its `units/` when
it has any, and a `VERIFIED` marker). `current` and `previous` are links to a `<sha>` directory. A unit runs
`/var/lib/kbf-installer/releases/current/kbf`. Only `kbf-installer` writes this tree, and kbf's own state stays in
`/var/lib/kbf`. Layout, state file and orders directory: kbf-installer.md section 3.

### The control API and its clients

The frontend's operator API is the only way to configure a farm. The web UI, `curl`, and the thin `kbf` CLI
verbs (`kbf cluster status|drain|undrain|member` is the model) are clients of it. Rules:

- Everything configurable is an API call first. There are no UI-only server paths and no logic in the CLI.
- `curl` is the documented contract. The CLI is a typed wrapper over the same calls.
- Today's API has nodes, members, drain, storage status, invocations and health [V, `../api.md`]. Writes pass
  five gates: loopback peer, no `Origin` header, a token file, a Bearer token and a JSON content type. Token
  minting (a route of this API, not a `kbf` command), object-store registration and the update endpoints are
  designed, not built [V].
- **Updates are triggered externally.** Something outside kbf calls the control API to set the desired release;
  the frontend then rolls the storage and daemon nodes through the drain system, one storage node at a time.
  The frontend's own update is a second route, `POST /v1/update/frontend {commit}` (same write gates). The
  frontend writes an order record into its own host's `orders/` directory, the mechanism nodes use, and the
  host's `kbf-installer` pulls and verifies the release, starts the new instance behind the installed Envoy,
  waits for its `/readyz`, then hands over; the old instance stops scheduling, relays its open `Execute`
  streams until they close, and exits. The installer confirms, and reverts if the new instance never becomes
  ready. One frontend update at a time; a second is refused. `kbf-installer update --frontend` stays as the
  local fallback. For
  our own farm the trigger is its deployment pipeline. kbf has no update button and does not poll for releases
  (section 10).
- A same-host proxy makes every caller loopback, which defeats the loopback gate, so the operator API and UI do
  not go through Envoy [V, split-mode-updates.md].

## 2. Per channel

| Channel | Installed | Starts when | First configuration | Updates |
|---|---|---|---|---|
| apt `.deb` | `kbf-installer`, units (disabled), trust files | Never at install. After configuration (section 4) | `sudo kbf-installer setup`, or the control API from a host that already has a frontend | `kbf-installer` pulls and applies releases when the externally set desired release reaches the host. `apt upgrade` replaces only `kbf-installer`, units and trust files |
| rpm / apk | Same payload and file list as the deb | Gating **not researched [U]**; Fedora and RHEL presets enable nothing by default [L] | Same | Same |
| Homebrew cask and signed `.pkg` | `kbf-installer`, the launchd plist, trust files (macOS, nodes only: section 7) | Not at install. `kbf-installer setup` bootstraps the plist after the config validates | `sudo kbf-installer setup` | Same installer, signature preserved per release |
| `curl \| sh` tarball | The script unpacks the same payload as the package and runs the same placement code as the package build | Never by the script; it prints the next step | `sudo kbf-installer setup` | Same installer |
| Frontend container image | `kbf`, no installer | By the container runtime. The entrypoint exits non-zero unless `kbf.toml` and the token are mounted | Mounted `kbf.toml`, then the API | A new image tag. No installer and no Envoy in the image: it runs behind the orchestrator's own proxy |

Notes:
- The tarball path is not equivalent to the package path unless it is the same code. Package and tarball both
  call one placement routine, and CI installs both and compares the file lists.
- `curl | sh` may add a package repository only after saying so and only with a `--no-repo` escape, because that
  is a trust change on the host. A signed apt repository is real continuing work: ship release assets and the
  script first, a hosted repository when a second operator exists.
- The `kbf` command on `PATH` is a link the installer maintains to `/var/lib/kbf-installer/releases/current/kbf` (in `/usr/local/bin`,
  which no package lists). The package never ships a file the installer replaces.
- There is no Homebrew formula for the root installer: a formula install step runs as the user and cannot place
  a root LaunchDaemon [V, brew manpage]. macOS uses the cask or the signed `.pkg`.

## 3. File ownership and skew

One owner per file.

| Path | Owner |
|---|---|
| `kbf-installer` binary, unit files, trust files (pinned attestation verifier, trusted root), sysusers and tmpfiles entries | The package |
| `/var/lib/kbf-installer/` (releases, `current`, `previous`, `state.json`, `status.json`; `orders/` is the one part `kbf` writes), the `committed` marker, `/etc/kbf/installer.toml` | `kbf-installer` |
| `/etc/kbf/kbf.toml`, the enrollment token, the rendered Envoy config | `kbf-installer` (generated files, not listed in the package, so no conffile rule is touched [V, Debian Policy 10.7.3]) |

Rules:

1. **The release is authoritative for behavior.** `kbf --version` reports the release. The package version
   gates the installer, units and trust files only.
2. **Compatibility range.** Each release declares the minimum and maximum installer version it supports; the
   package declares the minimum release it supports. The installer refuses to switch to a release outside the
   range and `kbf-installer check` reports a violation.
3. **A seed never promotes.** If the package carries a first release, it is copied only while no `current`
   exists and the `committed` marker is absent. A later package upgrade never re-seeds and never moves
   `current` or `previous`. Otherwise a package upgrade could roll a host back [L].
4. **The installer cannot replace its own verifier.** `kbf-installer` and the trust files change only by a
   package upgrade. A release that needs a newer installer is refused until the package catches up (rule 2).
   This is "root code is never auto-updated" [V, pull-updates.md section 1 item 9], now stated for the one root
   binary.
5. **A package upgrade restarts nothing it does not have to.** Units use `--no-restart-after-upgrade` and
   `--no-stop-on-upgrade`. The installer service itself restarts on its own upgrade; a running `kbf` does not.
6. **Skew test, required in CI.** Package N+1 files with release N, and release N+1 with package N files,
   across the declared range. Both must start.
7. **One lock.** A package upgrade (in its maintainer script) and the installer's switch take the same lock so
   they cannot interleave. The lock is **not designed [U]**.
8. **Remove and purge.** `remove` leaves the release tree and state. `purge` removes `/var/lib/kbf-installer` and
   `/etc/kbf`, and asks before deleting stored artifacts (product call [U]). Release
   files are not dpkg-owned, so the purge hook must delete them explicitly.
9. **Downgrade.** A package downgrade below a release's minimum is refused by the installer. A release rollback
   uses `previous`, never the package manager.

`/var/lib/kbf-installer/releases` replaces `/usr/local/kbf`: Debian Policy 9.1.2 says packages must not place files in
`/usr/local` [V], and `dpkg -V` and `debsums` only look at files a package lists [L].

## 4. Start gating

The package installs units disabled: `dh_installsystemd --no-enable --no-start --no-restart-after-upgrade
--no-stop-on-upgrade` [V]. Every unit is gated twice, so nothing starts before `kbf-installer setup` has
written a configuration that passes the same check the daemon applies. The unit text is in kbf-installer.md
section 7; its shape:

```ini
# one unit per mode (frontend is a template, kbf-frontend@.service, with --instance %i)
[Unit]
ConditionPathExists=/etc/kbf/kbf.toml
[Service]
ExecStartPre=+/usr/bin/kbf-installer boot-check
ExecStartPre=/var/lib/kbf-installer/releases/current/kbf --mode=<mode> --check
ExecStart=/var/lib/kbf-installer/releases/current/kbf --mode=<mode> --config /etc/kbf/kbf.toml
RestartPreventExitStatus=2
```

- An unmet `Condition*=` skips the job quietly. `--check` is an `ExecStartPre`, not an `ExecCondition`, because
  `ExecCondition` skips silently on exit 1 to 254 [L, `systemd.service(5)`]; `--check` prints one line per
  missing piece and exits 2.
- `kbf-installer setup` validates, writes `kbf.toml` and the token to temporary names in `/etc/kbf`, fsyncs,
  renames them into place with the config last, then runs `systemctl enable --now`. A crash between steps
  leaves no config that passes the condition.
- Not `policy-rc.d`: that file is a local administrator and image tool [V].
- Rootless containers: the daemon runs as `User=kbf` with `Delegate=yes`, and `crun` is the only container
  runtime; it runs in the `kbf` user's own manager kept alive by lingering. `--userns=nomap` needs a subuid and subgid range for `kbf`
  (`usermod --add-subuids` is the documented fix). `DynamicUser=` is not used: the daemon needs a fixed
  user that owns subid ranges, and `crun` runs `newuidmap` [V, `daemon.md`]. Creating the `kbf` user is
  sysusers (package); the subid range and lingering are done by `kbf-installer setup` as root, because
  `systemd-sysusers` allocates no subid ranges [L].
- Signer: the key that signs certificates and grants lives under its own uid. The package adds the user
  `kbf-signer` and the unit `kbf-signer.service` (frontend and all-in-one hosts only); the signer binary ships as
  a release component, and the frontend talks to it over a local socket. Until that is built, the frontend uses
  its in-process file signer (#966 [V]). Details: kbf-installer.md sections 3 and 7.

## 5. Envoy is a host prerequisite the installer checks

Envoy is the frontend's update path: the new frontend instance starts beside the old one behind Envoy, Envoy
sends new connections to it, and the old instance stops scheduling and relays its open `Execute` streams until
they close ([kbf-installer.md](kbf-installer.md) section 6, [frontend-handover.md](frontend-handover.md)).

- Envoy is **not** part of a kbf release, **not** pinned in a manifest and **not** fetched by `kbf-installer`,
  and it is **not** a package `Depends:` (one `.deb` serves every host, and a dependency cannot apply per host).
  Host preparation installs it, on frontend and all-in-one hosts only. `kbf-installer setup
  --mode=frontend|all-in-one`, `check` and the frontend update compare the installed Envoy with the range the
  farm has tested (`installer.conf`) and refuse with one line naming the problem and the fix. There is no
  separate frontend package. Storage and daemon hosts need no Envoy.
- The installer renders kbf's Envoy configuration from the frontend's own ports and instances (the renderer
  leaves `kbf-deploy` with its golden tests) and asks the installed Envoy to reload it. Envoy's service unit
  is its own package's.
- Through Envoy: REAPI (L7, with stream-timeout settings) and worker mTLS (L4 passthrough). Not through it: the
  operator API and UI (section 1).
- Upgrading Envoy across versions is not designed (section 10).
- Open problems that belong to the handover design, not this document: sharing of the registry and token store
  while two instances overlap, which instance takes operator writes during the overlap, and the shape of the
  container image (section 2 chooses no Envoy in it).

## 6. The installer as a service

After setup, `kbf-installer` keeps running as a root unit (no listening socket). It takes orders only as
records `kbf` writes into the `orders/` directory, and fetches with an unprivileged child (kbf-installer.md
sections 4 and 5):

- **Activate**: verify the staged release offline (build attestation, pinned `gh` and trusted root), switch
  `current`, restart the unit; the **confirm** order keeps it, a missed window or a crash loop **reverts** to
  `previous` locally (commit-confirm).
- **Frontend update**, ordered by a `frontend_update` record the frontend writes for its own host after
  `POST /v1/update/frontend` (the local fallback is `kbf-installer update --commit SHA --frontend`): it starts
  the new instance behind the installed Envoy, waits for its readiness, confirms or reverts, and hands over; the old instance stops
  scheduling and relays its open `Execute` streams until they close, with no timeout (OSS Buck2 never calls
  `WaitExecution`). One frontend on SQLite now; Postgres and several frontends later.
- **Node drain and restart** as part of the per-host step.
- **Fetch identity.** With the `frontend` release source the installer fetches with the host's enrolled node
  certificate. A storage-and-daemon host holds two role pairs: `installer.toml` points at the daemon pair if
  present, else the storage pair, and `ReleaseService.Fetch` admits any enrolled role. An all-in-one host does
  not enroll (it has no remote roles) and takes its first release from the `url` source or a release given to `setup`.

It grows from the existing `kbf-installer` and `kbf-helper` crates on `feature/pull-updates` [V]: the layout and
verbs of the former, the root half of an update from the latter. The result is one root binary; there is no
separate `kbf-helper` or `kbf-updater` binary in the end state. It carries no code that downloads or runs
anything the attestation does not cover.

## 7. macOS

Macs are nodes only. Listener passing, Envoy and the loopback gate are Linux designs, and a Mac
hosting a frontend is unsupported [U].

- `kbf --mode=daemon` runs as root in a system LaunchDaemon [V, macos-end-state.md]. Actions run as a pool of
  unprivileged lease users (`kbf-pool-0` onward) created through OpenDirectory. A pkg payload cannot create them,
  so `kbf-installer setup` creates them, as root, after the config validates. The pkg also does not bootstrap
  the plist; setup does.
- The pkg is signed (notarized). Its payload is `kbf-installer`, the plist, root-owned directories and trust
  files; not the lease users, not the token.
- **Signing is load-bearing.** A TCC grant is keyed to the signing identity, so every release the installer
  activates must be signed with the farm identity or Full Disk Access is lost on each update. The installer
  verifies the signature and preserves it; the cask must not re-sign.
- Tokens reach a Mac by file push, never inside the pkg.
- `mac-node-provisioning.md` still describes a hidden role user and `/usr/local/kbf`. The two documents
  disagree until that one is corrected; kbf-installer.md, this document and macos-end-state.md win.

## 8. The binary end state and the order of work

One `kbf` binary with five modes, selected by `--mode`. These stay separate processes, because the technical need is real:
`kbf-installer` (root, and must not be the process it updates), `kbf-guest` (runs inside the guest),
`kbf-vmm` (its own entitlement; ship only when it does something), `kbf-mac-session` (macOS GUI session,
shrinking per macos-end-state.md), and `kbf-mdm-gate` (keeps a credential away from the frontend; a mode of
`kbf` on the MDM host is the alternative, a decision of its own). The hidden re-exec modes `__keeper`,
`__as-user` and `__chown-dirs` stay inside `kbf`. Test-only binaries (the fake container runtimes, the
fake systemctl, `kbf-keeper-swap`) are never shipped.

Deleted when their replacement works on the test farm, not before (`kbf-deploy` is frozen, #1017, and gets no
new work; the farm installs through it today):

- the `kbf-all-in-one` binary (its tests move to `kbf --mode=all-in-one` first), the `kbf-updater` binary (by landing
  pull-updates), `kbf-helper` (folded into `kbf-installer`), and later the `kbf-server` and `kbf-daemon`
  binaries (after the farm and the Macs run `kbf --mode=<mode>`); the old subcommand forms, which are aliases until then;
- `kbf frontend handover` and its unit rule, once the installer performs the baton in the frontend update;
  `kbf check-args`, once `--check` exists on every mode;
- `kbf-deploy`, last: `host` becomes the installer, `linux render-units|install-units` become package units,
  `envoy` becomes the installer's config render, `log`, backups and image pull become API routes, and the MDM,
  object-store provisioning and pilot kits leave the product.

Order:

1. Land pull-updates (installer, node client, helper) and the start-up checks (#1020).
2. Add `/etc/kbf/kbf.toml`, `--mode` and `--check` to every mode, with the old subcommands as aliases. Additive.
3. Put every operator verb behind a frontend API route. Additive.
4. Grow `kbf-installer`: setup, the release fetch, the service loop. Build the package from it (deb,
   tarball, then the Mac pkg).
5. Move the test farm onto it one host at a time; each host needs the owner's go.
6. Mac nodes run `kbf --mode=daemon` (decision 2 below), then delete the `kbf-daemon` and `kbf-server` binaries.
7. Remove the handover verb and `kbf-deploy` piece by piece.

## 9. Open decisions

1. **First-token bootstrap and remote control-API access.** An API that needs a token cannot mint the first one,
   and the loopback write gate makes remote `curl` against a remote frontend awkward.
   There is no `kbf token` command; tokens are operator-API routes. Decided (engineering): the first operator
   token is created by `kbf-installer setup --mode=frontend`, run as root on the frontend host. It writes the
   token once to a root-only file and prints the file's path, never the secret; it creates none once any
   operator token exists. Open: an authenticated remote mode (token and TLS on a separate
   operator listener) with its own threat model, before any remote client is built.
2. **The darwin component rename.** Decided: the darwin release component is `kbf`, and the Mac runs
   `kbf --mode=daemon`; `kbf-mac-provision` moves to the same layout. The rename is one coordinated change with the
   signing allow-list (`tools/ci/darwin-entitlements.txt`, `sign-darwin`) and an explicit contract version bump,
   before the `kbf-daemon` binary is deleted.
3. **How a frontend host gets Envoy.** Decided: host preparation installs it (frontend and all-in-one hosts
   only); setup checks it against the tested range (section 5). The release never carries it and the installer
   never fetches it.

## 10. Later

Deferred, not designed here: an update banner or button and UI login; the desktop app; cloud (ASG, ECS) update
providers; cross-version Envoy upgrades; a managed build service; Postgres and several frontends.

## Sources

Debian: `dh_installsystemd`, `deb-systemd-helper(1p)`, `policy-rc.d`, Debian Policy 9.1.2 and 10.7.3, FHS 3.0
section 5.8, `debsums(1)`. systemd: `systemd.unit(5)`, `systemd.service(5)`, `systemd.exec(5)`,
`sysusers.d(5)`, `tmpfiles.d(5)`; cited from memory, not fetched. Repo: `kbf-installer.md`, `docs/api.md`, `daemon.md`, `pull-updates.md`,
`split-mode-updates.md`, `frontend-handover.md`, `macos-end-state.md`, `mac-node-provisioning.md`,
`../deploy/host-releases.md`, `../deploy/linux-build-host.md`; the kbf crates on `feature/pull-updates` and
`startup/self-checks`.
