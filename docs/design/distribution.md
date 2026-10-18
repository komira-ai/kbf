# Distribution: kbf, kbf-installer and the package

Status: design of record. Nothing here is built unless a sentence says so. Labels: **[V]** verified in
a primary source or in the repo, **[L]** likely (inference, or memory of a primary source), **[U]**
unverified or not yet designed.

This document replaces the distribution parts of `install-and-enrollment.md`
(where that document is present: it is in review) and the host layout of
[../deploy/host-releases.md](../deploy/host-releases.md). Updates themselves are specified in
`pull-updates.md` (wire, trust, release store) and [split-mode-updates.md](split-mode-updates.md)
(order of a rollout, the frontend's own update); this document says who runs them on a host.

## 1. The three things

| Thing | What it is | What it does | What it never does |
|---|---|---|---|
| `kbf` | The runtime. One binary, modes `frontend`, `storage`, `daemon`, `storage-and-daemon`, `all-in-one`, started with flags or `/etc/kbf/kbf.toml` | Runs the mode. At start it checks its host and refuses with one clear line per missing piece | Install, download or configure anything |
| `kbf-installer` | The one root tool for setup and for updates | Setup: writes `kbf.toml` and the enrollment token, downloads and verifies the `kbf` release (and, on frontend hosts, the Envoy the release pins), renders the Envoy config, places and enables units. Then runs as a service and carries out the frontend's update orders | Update itself. Only the package manager replaces it |
| The package | The way `kbf-installer` reaches a host | Ships the `kbf-installer` binary, unit files (installed disabled, gated), and the trust files | Start a service, depend on a distro Envoy package, own anything under `/var/lib/kbf/releases` |

**kbf refuses, it does not repair.** The start-up checks are the preflight of PR #1020 (branch
`startup/self-checks`): each missing piece (a directory, a user, a subuid range, a runtime, a token file) is
one line naming the piece and what provides it. The same checks run as `kbf <mode> --check`, a flag and not a
subcommand, which is what the unit gate calls (section 4).

**Releases.** `/var/lib/kbf/releases/<sha>/` holds one release (the `kbf` binary, its `units/` when it has
any, and a manifest). `current` and `previous` are links to a `<sha>` directory. A unit runs
`/var/lib/kbf/releases/current/kbf`. Only `kbf-installer` writes this tree.

### The control API and its clients

The frontend's operator API is the only way to configure a farm. The web UI, `curl`, the desktop app and the
thin `kbf` CLI verbs (`kbf cluster status|drain|undrain|member` is the model) are clients of it. Rules:

- Everything configurable is an API call first. There are no UI-only server paths and no logic in the CLI.
- `curl` is the documented contract. The CLI is a typed wrapper over the same calls.
- The desktop app wraps `kbf-installer` for the local host and the API for the farm. It holds no setup logic;
  it needs an elevation path for the installer (polkit or `pkexec` on Linux, an authorization prompt on macOS),
  which is **not designed [U]**.
- Today's API has nodes, members, drain, storage status, invocations and health [V, `../api.md`]. Writes pass
  five gates: loopback peer, no `Origin` header, a token file, a Bearer token and a JSON content type. Token
  minting, object-store registration and the update endpoints are designed, not built [V].
- A same-host proxy makes every caller loopback, which defeats the loopback gate, so the operator API and UI do
  not go through Envoy [V, split-mode-updates.md].

## 2. Per channel

| Channel | Installed | Starts when | First configuration | Updates |
|---|---|---|---|---|
| apt `.deb` | `kbf-installer`, units (disabled), trust files | Never at install. After configuration (section 4) | `sudo kbf-installer setup`, or the control API from a host that already has a frontend | `kbf-installer` pulls and applies releases on the frontend's order. `apt upgrade` replaces only `kbf-installer`, units and trust files |
| rpm / apk | Same payload and file list as the deb | Gating **not researched [U]**; Fedora and RHEL presets enable nothing by default [L] | Same | Same |
| Homebrew formula | `kbf-installer` only. No daemon | `brew services` is not used | `sudo kbf-installer setup` | `brew upgrade` replaces `kbf-installer` only; releases follow the installer |
| Homebrew cask and signed `.pkg` | `kbf-installer`, the launchd plist, trust files (macOS, nodes only: section 7) | Not at install. `kbf-installer setup` bootstraps the plist after the config validates | Desktop app or `sudo kbf-installer setup` | Same installer, signature preserved per release |
| `curl \| sh` tarball | The script unpacks the same payload as the package and runs the same placement code as the package build | Never by the script; it prints the next step | `sudo kbf-installer setup` | Same installer |
| Desktop app | App bundle with an embedded `kbf-installer` for the local host | Only when the wizard finishes | Wizard, over the installer and the API | The app updates itself (Docker Desktop pattern [L]); the host follows the installer |
| Frontend container image | `kbf`, no installer | By the container runtime. The entrypoint exits non-zero unless `kbf.toml` and the token are mounted | Mounted `kbf.toml`, then the API | A new image tag. No Envoy in the image: it runs behind the orchestrator's own proxy |

Notes:
- The tarball path is not equivalent to the package path unless it is the same code. Package and tarball both
  call one placement routine, and CI installs both and compares the file lists.
- `curl | sh` may add a package repository only after saying so and only with a `--no-repo` escape, because that
  is a trust change on the host. A signed apt repository is real continuing work: ship release assets and the
  script first, a hosted repository when a second operator exists.
- The `kbf` command on `PATH` is a link the installer maintains to `releases/current/kbf` (in `/usr/local/bin`,
  which no package lists). The package never ships a file the installer replaces.
- A formula install step runs as the user and cannot install a daemon. `sudo brew services start` would write
  a root LaunchDaemon [V, brew manpage], which is why the formula carries the installer and nothing else.

## 3. File ownership and skew

One owner per file.

| Path | Owner |
|---|---|
| `kbf-installer` binary, unit files, trust files (pinned attestation verifier, trusted root), sysusers and tmpfiles entries | The package |
| `/var/lib/kbf/releases/<sha>/`, `current`, `previous`, the `committed` marker, `/var/lib/kbf/envoy/<version>/` | `kbf-installer` |
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
8. **Remove and purge.** `remove` leaves the release tree and state. `purge` removes `/var/lib/kbf/releases`,
   `/var/lib/kbf/envoy` and `/etc/kbf`, and asks before deleting stored artifacts (product call [U]). Release
   files are not dpkg-owned, so the purge hook must delete them explicitly.
9. **Downgrade.** A package downgrade below a release's minimum is refused by the installer. A release rollback
   uses `previous`, never the package manager.

`/var/lib/kbf/releases` replaces `/usr/local/kbf`: Debian Policy 9.1.2 says packages must not place files in
`/usr/local` [V], and `dpkg -V` and `debsums` only look at files a package lists [L].

## 4. Start gating

The package installs units disabled: `dh_installsystemd --no-enable --no-start --no-restart-after-upgrade
--no-stop-on-upgrade` [V]. Every unit is gated twice, so nothing starts before `kbf-installer setup` has
written a configuration that passes the same check the daemon applies.

```ini
# kbf-daemon.service (one such unit per mode; modes differ in ExecStart and --check)
[Unit]
Description=kbf daemon
ConditionPathExists=/etc/kbf/kbf.toml
After=network-online.target
Wants=network-online.target

[Service]
Type=notify
User=kbf
StateDirectory=kbf
ExecCondition=/var/lib/kbf/releases/current/kbf daemon --check
ExecStart=/var/lib/kbf/releases/current/kbf daemon --config /etc/kbf/kbf.toml
Restart=on-failure

[Install]
WantedBy=multi-user.target
```

```ini
# kbf-frontend@.service (template; same gating)
[Unit]
Description=kbf frontend %i
ConditionPathExists=/etc/kbf/kbf.toml
Wants=kbf-envoy.service
After=kbf-envoy.service network-online.target

[Service]
Type=notify
User=kbf
ExecCondition=/var/lib/kbf/releases/current/kbf frontend --check
ExecStart=/var/lib/kbf/releases/current/kbf frontend --config /etc/kbf/kbf.toml --instance %i
```

- An unmet `Condition*=` skips the job quietly. `ExecCondition=` exiting 1 to 254 skips it, 255 or above fails
  it [L, `systemd.service(5)`]. `--check` therefore exits 1 for "not configured yet" and 255 for "configured
  and broken", with the one-line-per-piece output of section 1.
- `kbf-envoy.service` carries the same `ConditionPathExists` on its rendered config. Use `Wants=` and `After=`,
  not `BindsTo=`: a unit bound to a condition-skipped unit is stopped [L].
- `kbf-installer setup` validates, writes `kbf.toml` and the token to temporary names in `/etc/kbf`, fsyncs,
  renames them into place with the config last, then runs `systemctl enable --now`. A crash between steps
  leaves no config that passes the condition.
- Not `policy-rc.d`: that file is a local administrator and image tool [V].
- Rootless containers: the daemon runs as `User=kbf` with `Delegate=yes`; rootless Podman runs in the `kbf`
  user's own manager kept alive by lingering. `--userns=nomap` needs a subuid and subgid range for `kbf`
  (`usermod --add-subuids` is the documented fix). `DynamicUser=` is not used: the daemon needs a fixed
  user that owns subid ranges, and `crun` runs `newuidmap` [V, `daemon.md`]. Creating the `kbf` user is
  sysusers (package); the subid range and lingering are done by `kbf-installer setup` as root, because
  `systemd-sysusers` allocates no subid ranges [L].

## 5. Envoy is pinned per release

Envoy is the frontend's update path: the new frontend instance starts beside the old, Envoy sends new
connections to it, the old one drains ([split-mode-updates.md](split-mode-updates.md),
[frontend-handover.md](frontend-handover.md)). The earlier question of listener passing versus Envoy is closed:
Envoy.

- The release manifest names an Envoy **version and sha256** (per OS and architecture). On a frontend host the
  installer downloads that build, verifies the sha256 (the manifest is covered by the release attestation), and
  places it under `/var/lib/kbf/envoy/<version>/`. Nodes never get Envoy.
- The package does **not** depend on a distro Envoy package. Versions drift, and an `apt upgrade` of Envoy
  would restart it outside the handover.
- The installer renders the Envoy config from the frontend's own ports and instances (the renderer leaves
  `kbf-deploy` with its golden tests), and `kbf-envoy.service` runs the pinned build.
- A release that pins a different Envoy upgrades it through the same handover: start the new Envoy, move the
  frontend onto it, retire the old. The installer does this on the frontend's order; apt never touches it.
- Hot restart kills the parent's connections at `--parent-shutdown-time-s`, so that setting must exceed the
  longest action. The frontend owns the value [V, split-mode-updates.md].
- Through Envoy: REAPI (L7, with stream-timeout settings) and worker mTLS (L4 passthrough). Not through it: the
  operator API and UI (section 1).
- Open problems that belong to the handover design, not this document: the policy for an update that waits for
  the longest action, sharing of the registry and token store while two instances overlap, which instance takes
  operator writes during the overlap, and the shape of the container image (section 2 chooses no Envoy in it).

## 6. The installer as a service

After setup, `kbf-installer` keeps running as a root unit (no listening socket). It holds the frontend's
session through `kbf` (the unprivileged process fetches and stages; see pull-updates.md section 1) and takes
the frontend's orders:

- **Activate**: verify the staged release offline (build attestation, pinned `gh` and trusted root), switch
  `current`, restart the unit; the **confirm** order keeps it, a missed window or a crash loop **reverts** to
  `previous` locally (commit-confirm).
- **Envoy swap and frontend handover** on frontend hosts (section 5).
- **Node drain and restart** as part of the per-host step.

It grows from the existing `kbf-installer` and `kbf-helper` crates on `feature/pull-updates` [V]: the layout and
verbs of the former, the root half of an update from the latter. The result is one root binary; there is no
separate `kbf-helper` or `kbf-updater` binary in the end state. It carries no code that downloads or runs
anything the attestation does not cover.

## 7. macOS

Macs are nodes only. Listener passing, Envoy hot restart and the loopback gate are Linux designs, and a Mac
hosting a frontend is unsupported [U].

- `kbf daemon` runs as root in a system LaunchDaemon [V, macos-end-state.md]. Actions run as a pool of
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
  disagree until that one is corrected; this document and macos-end-state.md win.

## 8. The binary end state and the order of work

One `kbf` binary with five modes. These stay separate processes, because the technical need is real:
`kbf-installer` (root, and must not be the process it updates), `kbf-guest` (runs inside the guest),
`kbf-vmm` (its own entitlement; ship only when it does something), `kbf-mac-session` (macOS GUI session,
shrinking per macos-end-state.md), and `kbf-mdm-gate` (keeps a credential away from the frontend; a mode of
`kbf` on the MDM host is the alternative, a decision of its own). The hidden re-exec modes `__keeper`,
`__as-user` and `__chown-dirs` stay inside `kbf`. Test-only binaries (the three copies of the fake podman, the
fake systemctl, `kbf-keeper-swap`) are never shipped.

Deleted when their replacement works on the test farm, not before (`kbf-deploy` is frozen, #1017, and gets no
new work; the farm installs through it today):

- the `kbf-all-in-one` binary (its tests move to `kbf all-in-one` first), the `kbf-updater` binary (by landing
  pull-updates), `kbf-helper` (folded into `kbf-installer`), and later the `kbf-server` and `kbf-daemon`
  binaries (after the farm and the Macs run `kbf <mode>`);
- `kbf frontend handover` and its unit rule, once the installer performs the baton on the frontend's order;
  `kbf check-args`, once `--check` exists on every mode;
- `kbf-deploy`, last: `host` becomes the installer, `linux render-units|install-units` become package units,
  `envoy` becomes the installer's render, `log`, backups and image pull become API routes, and the MDM,
  object-store provisioning and pilot kits leave the product.

Order:

1. Land pull-updates (installer, node client, helper) and the start-up checks (#1020).
2. Add `/etc/kbf/kbf.toml` and `--check` to every mode. Additive.
3. Put every operator verb behind a frontend API route. Additive.
4. Grow `kbf-installer`: setup, the release and Envoy fetch, the service loop. Build the package from it (deb,
   tarball, then the Mac pkg).
5. Move the test farm onto it one host at a time; each host needs the owner's go.
6. Mac nodes run `kbf daemon` (decision 2 below), then delete the `kbf-daemon` and `kbf-server` binaries.
7. Remove the handover verb and `kbf-deploy` piece by piece.

## 9. Open decisions

1. **First-token bootstrap and remote control-API access.** An API that needs a token cannot mint the first one,
   and the loopback write gate makes remote `curl` and a desktop app against a remote frontend awkward.
   Recommendation: a local-only first-token path run as root on the frontend host (`kbf-installer` writes a
   0600 operator token file, prints no secret, refuses once any operator token exists), plus an authenticated
   remote mode (token and TLS on a separate operator listener) with its own threat model, before the desktop app
   is built.
2. **The darwin component rename.** The pull-updates wire names the Mac component `kbf-daemon`, and the wire
   contract is frozen. Moving Macs to `kbf daemon` means a component named `kbf`. Recommendation: rename it in
   one coordinated change with the signing allow-list (`tools/ci/darwin-entitlements.txt`, `sign-darwin`),
   accepted by an explicit version bump of the contract, and do it before the `kbf-daemon` binary is deleted.
3. **Envoy binary source.** Recommendation: the release manifest pins the upstream Envoy build by version and
   sha256, and the installer fetches it from the frontend's release store (which mirrors it as a release
   asset, so a host needs no outbound access beyond the frontend) and verifies the sha256. Upstream URLs are a
   fallback for the first frontend only. Alternative: build Envoy ourselves, which is more work and not
   recommended now.

## Sources

Debian: `dh_installsystemd`, `deb-systemd-helper(1p)`, `policy-rc.d`, Debian Policy 9.1.2 and 10.7.3, FHS 3.0
section 5.8, `debsums(1)`. systemd: `systemd.unit(5)`, `systemd.service(5)`, `systemd.exec(5)`,
`sysusers.d(5)`, `tmpfiles.d(5)`; cited from memory, not fetched. Homebrew: `services` manpage. Envoy: hot
restart and `--parent-shutdown-time-s`. Repo: `docs/api.md`, `daemon.md`, `pull-updates.md`,
`split-mode-updates.md`, `frontend-handover.md`, `macos-end-state.md`, `mac-node-provisioning.md`,
`../deploy/host-releases.md`, `../deploy/linux-build-host.md`; the kbf crates on `feature/pull-updates` and
`startup/self-checks`.
