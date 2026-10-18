# kbf-installer: contract

Status: design of record for the parallel PRs. Marks: **(built)** exists on `feature/pull-updates`; **(to build)** does not. Where this contradicts `pull-updates.md`, #1021 (distribution), #966 or #1024, this one wins and those PRs are edited to match (section 11). Wire messages stay frozen.

## 1. Purpose
`kbf-installer` is the one root tool for setup and updates on every channel. It is the only writer of root-owned release files. It never updates itself; only the package manager does.
1. **kbf** is runtime only: modes plus flags, or `/etc/kbf/kbf.toml`. It never installs, fetches or restarts anything. It forwards an order (section 4) and reports status. kbf.toml has no update or installer keys.
2. **kbf-installer** installs, fetches, verifies, activates, confirms, reverts releases, and fetches and pins Envoy.
3. **The package** ships `kbf-installer`, the unit files (disabled) and the trust files. It ships no kbf runtime release; `setup` installs the first one.

Not goals: self-update, a distro Envoy dependency, an installer network listener, root logic in kbf.

## 2. CLI
One binary (**to build**; today a library plus `kbf-helper`). Mutating verbs are a dry run without `--apply` (**built**). Flags: `--json`, `--apply`; `--root DIR` exists only in builds with the `test-sandbox` feature. Exit codes: 0 ok; 1 failed; 2 usage or missing prerequisite; 3 refused (verification, policy); 4 busy; 5 needs operator. Every verb but `status`, `check`, `envoy list` needs root.

| Verb | What it does |
|---|---|
| `setup` | First install. `--mode`, `--release-source url\|frontend`, `--release-url`, `--commit` (default: latest in source), `--no-start`. Fetches and verifies Envoy and the release, writes `installer.toml` (the only writer), creates users and directories. Idempotent. |
| `status` | Current, previous, staged, pending confirm, Envoy, last refusal. `--json` equals `status.json` (section 4). |
| `serve` | Run by `kbf-installer.service`: take orders from the order socket and the control socket, expire windows, write status. No release polling. |
| `update --commit SHA [--frontend]` | Operator order. Talks to `serve` over the control socket, or runs inline if `serve` is down. |
| `confirm`/`revert ID` | Operator override of a pending swap, naming the activation id. Exit 5 if none pending. |
| `rollback` | `current` to `previous`, no window. Operator only; the only verb allowed below the release floor. |
| `gc` | Keeps current, previous, one more. State-aware (S22). |
| `boot-check` | Counts starts of a release on trial; reverts when the budget is spent (**built** logic). |
| `quiesce` | For maintainer scripts: takes the lock, exit 5 if a swap is pending (S25). |
| `envoy fetch\|verify\|path\|list` | Pinned Envoy under the installer tree. |
| `check` | Validates installer config, trust files, path chain. Lists every problem. |

`stage`, `activate`, `fetch-child` are internal.

## 3. Files, ownership, one source per setting
Three roots, never nested: kbf's own state `/var/lib/kbf` (user `kbf`, systemd StateDirectory, untouched by the installer); the installer tree `/var/lib/kbf-installer` (root); package files.

Package-owned (root:root, no group/other write, changed by package upgrade only):
- `/usr/bin/kbf-installer`, `/usr/lib/systemd/system/kbf-*.service`.
- `/usr/share/kbf-installer/trust/`: pinned `gh`, `trusted_root.jsonl` (with `valid_until`), `installer.conf` (signer workflow, OIDC issuer, repo, allowed source refs, `deny` commit list, `min_release_seq`). Replaces `/usr/local/libexec/kbf-deploy`. Allowed source refs live **only** here; `installer.toml` may narrow them, never widen (set intersection).

Installer-owned (root:root):
- `/var/lib/kbf-installer/releases/<sha>/`: `kbf`, `kbf-daemon` where it applies, `units/`, `manifest.json`, `VERIFIED` marker written last. Files 0755/0644; no setuid, setgid or file capabilities.
- `/var/lib/kbf-installer/current`, `previous`: relative links `releases/<sha>`. Frontend instances use `slots/<instance>` links (section 6).
- `/var/lib/kbf-installer/envoy/<version>-<sha256[:16]>/envoy` plus `envoy.sha256`.
- `/var/lib/kbf-installer/state.json` (0600, schema 2) and `state.json.bak`; `work/` (0700); lock `/run/kbf-installer/lock`.
- Layout constants live in `layout.rs`, one place. macOS: root-owned `/usr/local/kbf-installer` tree (not a user-owned Homebrew prefix), `/var/db/kbf-installer` for state.

`state.json` schema 2 (types fixed by the golden fixture, section 9 P1; `deny_unknown_fields`): `schema`, `current`, `previous` (confirmed releases only), `staged`, `pending_confirm {activation "<epoch>-<order_id>", commit, previous, deadline_wall, deadline_mono_s, starts_left}`, `envoy {current, previous}`, `floor {epoch, order_id, release_seq}`, `serving_instance` (frontend), `last_acked_order_id`. `floor` replaces the built, unused `order_floor`. Schema 1 to 2 migration (including the string activation id) belongs to P3.

Config:
- `/etc/kbf/kbf.toml`: kbf's runtime config, root:kbf 0640 (#966). The installer reads only its existence.
- `/etc/kbf/installer.toml` (root:root 0644, TOML, unknown keys refused): `release_source` (`url`|`frontend`), `release_url`, `source_refs` (narrowing only), `confirm_window_s` (default 900, bounds 120-1800), `confirm_window_max_s`, `min_generation` (operator floor), `min_activation_interval_s` (60), `max_component_bytes` (1 GiB). Source is read only from this file, never from an order, environment or flag outside `setup`. This supersedes `[update]` in kbf.toml of #1024.

Users: `kbf` (runtime). `kbf-fetch` (`_kbf-fetch` on macOS): dedicated, no login, owns only `/var/cache/kbf-fetch/<random>`, created per fetch and removed after. `kbf-verify`: dedicated uid for `gh` and `--version` runs, no network.

## 4. Order handover
Channel decides authority, not a body field.
- **Wire orders** arrive at a root-owned unix stream socket `/run/kbf-installer/orders.sock` (root:kbf 0660; dir 0755). The installer checks `SO_PEERCRED` uid `kbf`, reads at most 4096 bytes, one order per connection, rate limited, and persists the order itself before replying. No spool directory, so no symlink, FIFO, hardlink, sticky-bit or tmpfs-fill cases.
- **Operator orders** (`update --frontend`, `confirm`, `revert`, `rollback`) use `/run/kbf-installer/control.sock` (0600, uid 0 by `SO_PEERCRED`). `kbf` cannot reach it.
- **Body** (the **built** `order.rs` format, unchanged): `{ "order_id": u64, "epoch": u64, "verb": "activate"|"confirm"|"revert", "commit": "<40 hex>", "window_s": u32 }`; unknown and duplicate keys refused. No `target` or `generation` field: orders on `orders.sock` can only act on the node units table in code (S2). The activation id is `<epoch>-<order_id>`.
- **Ordering** (**built**): (epoch, order_id), each once; older epoch and replays dropped; an epoch or order_id jump above 1000 beyond `floor` refused. A parse failure never advances the floor. A rollback is the old release ordered again with a higher id.
- **`window_s`** is an input, clamped to `[120, confirm_window_max_s]`; on Linux the window starts at the first start (installer config), so `window_s` is advisory there (**built**); on macOS it arms at activation.
- **Status**: `/run/kbf-installer/status.json` (0644, atomic rename), schema 2: **built** fields `installed_commit`, `previous_commit`, `staged_commit`, `state`, `refusal`, `acked_order_id`, `staged{...}`, plus `envoy {current, previous}`. `state` words: `idle`, `fetching`, `staged`, `activated_unconfirmed`, `confirmed`, `reverted`, `refused`; each maps 1:1 to `UpdateStatus.State` (`fetching` is the #1024 addition). `acked_order_id` is rebuilt from `state.json` after reboot. `helper_present` means installer present. `kbf-installer.service` starts before the runtime units (`Before=`) and `/run/kbf-installer` is made by `tmpfiles.d`, so it survives restarts and package upgrades.
- Orders that arrive while a swap is pending are refused (`pending`, exit 4) and `previous` is only ever a confirmed release. A confirm is accepted only if it names the pending activation, the unit is active, `/proc/<MainPID>/exe` is in `releases/<current>`, and the first boot-check has run.

## 5. Fetch and verify
**Release source** (from `installer.toml`): `url`: HTTPS base, asset path `<base>/<commit>/<asset>`; at most 3 redirects, never to http, no proxy variables. `frontend`: `ReleaseService.Fetch` by sha256; the child must not hold the node identity, so it needs the local fetch proxy (P15). Until then the farm uses `url`; a farm without a public URL serves the release assets from any static HTTPS location it controls (decision 3). The frontend holds no attestations the installer relies on.

**Assets**: tarballs `<name>-<40hex>-<platform>.tar.gz` (**built**, platforms `linux-x86_64`, `linux-arm64`, `darwin-arm64`); new: `kbf-<40hex>-manifest.json` and `<asset>.sigstore.json` bundles as release assets. Hosts never fetch `SHA256SUMS` (advisory only) and never use `gh attestation download`.

**Release manifest**, schema 1, attested as its own subject by the release workflow (P4 first):
```json
{ "schema": 1, "commit": "<40 hex>", "release_seq": 0, "min_safe_seq": 0,
  "installer": {"min": "x.y.z", "max": "x.y.z"},
  "protocol": 1, "min_protocol": 1, "state_schema": 2,
  "assets": {"<platform>": {"<name>": {"file": "...", "sha256": "...", "size": 0}}},
  "envoy": {"linux-x86_64": {"version": "1.x.y", "url": "https://...", "sha256": "..."}} }
```
`sha256` is of the tarball, which is also the attestation subject. `file` must equal `<name>-<commit>-<platform>.tar.gz`. Names match `^[a-z0-9._-]+$`; Envoy `version` matches `^\d+\.\d+\.\d+$`, `url` is https, `sha256` non-empty. `size` is capped by `max_component_bytes`. `envoy` is Linux frontend-only and ignored for node orders. `release_seq` is assigned by the workflow. The installer version is its workspace version. Unknown fields refused.

**Fetch child** (`fetch-child`, **to build**): the parent closes every fd above 2, then `setgroups([])`, `setresgid/uid` to `kbf-fetch`, `NO_NEW_PRIVS`, clears capabilities, sets `RLIMIT_NPROC/FSIZE`, empty environment, `HOME=/nonexistent`, seccomp or `sandbox-exec` allowing network and its download dir only, inside a transient unit with `KillMode=control-group` on Linux. Caps: manifest 1 MiB, per-file and per-release size from the manifest, gzip expansion cap, no multi-member or trailing data, 10 min timeout, free-space check. The child is dead (`waitpid` plus group kill) before the parent opens anything.

**Verification order**, by the root parent; nothing unpacked or run before step 6:
1. Trust files and every path component of installer, `gh`, trust dir are root-owned, not links, not group/other writable (`openat2` walk). Trust `valid_until` not passed (exit 5 if so).
2. Parse the order; check floor, epoch, commit syntax, `deny` list, pending state, minimum interval.
3. Child downloads manifest and bundle. Parent opens child files with `openat2(RESOLVE_BENEATH|NO_SYMLINKS)`, requires regular, `nlink == 1`, owner `kbf-fetch`, size within cap, copies by descriptor to root scratch hashing in the same pass. Verify the manifest attestation as `kbf-verify`: `gh attestation verify --bundle` offline with `--custom-trusted-root`, `--repo`, `--signer-workflow`, `--cert-oidc-issuer`, `--source-ref` (allowed refs), `--source-digest <commit>`, `--predicate-type`, `--deny-self-hosted-runners` unless decision 6 says otherwise (**built** `attest.rs` flags).
4. Manifest `commit` equals the order commit; `installer.min <= self <= installer.max`; protocol and state schema compatible; platform present; `release_seq >= floor.release_seq` and `>= min_safe_seq` of any manifest seen and `>= min_release_seq` of the trust file (else `downgrade`).
5. Each tarball: child download, root copy, hash against the manifest, own attestation with the same flags including `--source-digest <commit>`.
6. Own tar reader unpacks one regular file; ignores archive mode, owner, xattrs; `fchmod` 0755. Check binary kind and CPU, macOS ad hoc signature. Run `--version --json` by descriptor as `kbf-verify` (no network, empty env, 5 s) and require the commit.
7. Install: write `releases/.stage-<sha>.<rand>/`, re-hash the staged file against the manifest, fsync files and directory, rename to `releases/<sha>`, fsync parent, write `VERIFIED`, then `staged` in `state.json`. A `releases/<sha>` not listed in state or lacking a matching fresh hash is unverified and re-verified or removed; "already installed" never skips verification.
8. Envoy (frontend only): fetch by the child, sha256 against the attested manifest, ELF machine check, `envoy --version` by descriptor as `kbf-verify`, install into `<version>-<sha256[:16]>`, never overwriting a directory with different bytes.

**Refusals** (exit 3, stable `refusal` word): `trust_chain`, `hash`, `attestation`, `commit_or_ref`, `installer_too_old`, `platform`, `size`, `disk` (below 2x offered size), `replay`, `downgrade`, `pending`, `denied`. Lock contention is exit 4, not a refusal.

## 6. Update sequences
**Node update** (the "update ready" click sets the desired release; `POST /v1/update/desired` of #1024 6.3 is the API, P14 builds on it). The frontend orders storage and daemon nodes one at a time through the drain system:
1. Drain, then `UpdateOrder{ACTIVATE, commit}`; kbf forwards it to `orders.sock` and does nothing else.
2. Installer fetches and verifies (section 5), then writes `pending_confirm`, `previous`, `current` in that order (state first), then restarts the node units named in its code table.
3. Linux: window starts at the first start (`boot-check`), 3 start attempts; macOS armed at activation, `expire` triggers.
4. New kbf reports `ACTIVATED_UNCONFIRMED`; the frontend sends `CONFIRM`. A compromised new release can self-confirm; accepted residual (section 10).
5. No confirm by the deadline (monotonic plus wall), start budget spent, or `REVERT`: revert state first, then links, then restart; report `REVERTED`.
Crash at any step: `reconcile` makes links match `state.json` on next start (**built**). Fetch failure leaves `current` untouched and reports `refused`.

**Frontend update**: separate action, never on the wire. Entry: `kbf-installer update --commit SHA --frontend` over the control socket.
1. Verify the release and, if the pin differs, Envoy.
2. Point `slots/<idle>` at the new release. Instances run `slots/%i/kbf`, so a crash restart of the serving instance never picks up the new release; `current` flips only at confirm.
3. `systemctl start kbf-frontend@<idle>`; handover per `frontend-handover.md` (**built**). Needs `--instance %i` in `kbf frontend` (**to build**, P13 adds the flag to `kbf-cli`).
4. If Envoy changes, start `kbf-envoy@<new>`, move the frontend, retire the old after drain.
5. Confirm: the successor reports ready (notify) and the old reports stood down inside `confirm_window_s`; the frontend's own handover signals are the confirm. Else revert: if the old instance already stood down, start it and wait for ready before flipping slots back.
6. `kbf frontend handover` is removed after this lands. `orders.sock` orders never touch `kbf-frontend@*` or `kbf-envoy@*`, on an all-in-one host too; this holds after P13, replacing "forbid until P13".

## 7. Units and gating
Plain files in `packaging/linux/units/`, installed disabled, never enabled by package scripts: `kbf-installer.service`, one unit per mode `kbf` offers (`frontend` as `kbf-frontend@.service`, `storage`, `daemon`, `storage-and-daemon`, `node`, `all-in-one`; P11 lists them from `kbf --help`), `kbf-envoy@.service`. Paths are literal; there are no `@...@` substitutions. The `check-units` rules (`handover`, `state-store`, `stop`, `credential`, `storage`) move with the units (P11).
```ini
[Unit]
ConditionPathExists=/etc/kbf/kbf.toml
[Service]
ExecStartPre=+/usr/bin/kbf-installer boot-check
ExecStartPre=/var/lib/kbf-installer/current/kbf <mode> --check
ExecStart=/var/lib/kbf-installer/current/kbf <mode>
RestartPreventExitStatus=2
```
`--check` prints every missing prerequisite and exits 2 (**to build**, P12; #1020 runs the same checks at every start). It is `ExecStartPre`, not `ExecCondition`, because `ExecCondition` skips silently on exit 1-254. `boot-check` runs as root (`+`); lock contention waits and neither counts as a start nor fails the unit. Node mode keeps its `kbf enroll` pre-start from #966.
`kbf-installer.service`: `Type=simple`, `Restart=on-failure` with exit 4 treated as success, `ConditionPathExists=/etc/kbf/installer.toml`, `ExecStartPre=/usr/bin/kbf-installer check`, hardening (`NoNewPrivileges`, `ProtectSystem=strict`, `ReadWritePaths=/var/lib/kbf-installer /run/kbf-installer /var/cache/kbf-fetch`, `PrivateTmp`, trimmed capabilities); P11 lints the `systemd-analyze security` score. `kbf-envoy@.service` runs `ExecStartPre=+kbf-installer envoy verify` against the pin in `state.json`. Frontend template rules from the handover design apply (`Type=notify`, finite `TimeoutStartSec`, `RuntimeDirectoryPreserve=yes`, no `Conflicts=`).
macOS: launchd plists in `packaging/macos/`; nodes only, via cask or `.pkg` (a formula cannot place a root LaunchDaemon, #966). `kbf-installer` there is root-owned; a user-owned prefix is refused by `check`.

## 8. Compatibility and migration
- Frozen wire reused unchanged: `UpdateOrder{order_id, epoch, verb, commit, window_s}`, all `UpdateStatus` fields, `update_order = 9`, `update_status = 7`. No wire change in this contract.
- `UpdateOffer` and `ReleaseService.Fetch` stay on the wire for the update-ready display and the `frontend` source; nodes do not consume offers. The planner's "release complete" check reads the manifest, not frontend attestation rows; a node order to an incomplete release is refused by the planner.
- Installer vs release: `installer.min/max` gates installs; `installer_too_old` waits for a package upgrade. Any state schema bump needs a range bump.
- Migration is owned by P3: `setup` adopts `/usr/local/kbf` and `/var/lib/kbf-deploy` once (state schema 1 to 2, links, trust files), keeps a rollback path, and covers the test-farm hosts that run the dev installer. `kbf-helper` and `kbf-installer` never both manage one host: `setup` refuses while the old state exists unmigrated.

## 9. PR breakdown
Fakes: temp roots, fake clock, `file://` and local HTTPS source, recorder for `systemctl`/`launchctl`, fixture manifests and bundles, a pinned fake `gh`.

| # | PR | Tests against |
|---|---|---|
| P0 | Workflow attests the manifest; `xtask manifest`; lint of the attested set; `docs/artifacts.md` | xtask tests; fixture release |
| P1 | Binary and CLI skeleton, exit codes, `--json`, dry run, and **golden fixtures** for order, status, state 2, manifest, `installer.toml` | golden tests; every later PR reuses them |
| P2 | `layout.rs` new roots, state schema 2, `floor`, `.bak`; update `kbf-deploy` paths and unit templates in lockstep | temp roots |
| P3 | `migrate.rs`: old layout and state 1 to 2, test-farm hosts | old-layout fixtures |
| P4 | Order socket, control socket, status writer; `kbf-update` becomes a forwarder; remove inbox | fake kbf writer, replay/crash (**built** `contract_tests`) |
| P5 | Fold `kbf-helper` into `serve`; activation, boot-check, reconcile, gc, quiesce | **built** `crash_tests`, systemctl recorder |
| P6 | `manifest.rs` parse, ranges, release_seq/floor | fixture matrix |
| P7 | Fetch child and URL source, privilege drop | local HTTPS server, size/timeout/redirect, Linux drop test |
| P8 | Verify pipeline `stage.rs` (section 5 order, copy-hash-install) | **built** attest/archive/binary tests, fake `gh` |
| P9 | Envoy pin, fetch, verify, verbs | fake Envoy script, mismatch cases |
| P10 | `setup`, `settings.rs`, `installer.toml`, `check` path-chain, trust expiry | idempotency, partial failure |
| P11 | Units, nfpm, macOS pkg, unit lint, hardening score | `systemd-analyze verify`, container root |
| P12 | `kbf <mode> --check` for every mode | missing-prerequisite fixtures |
| P13 | Frontend update: slots, `--instance`, `kbf-envoy@` | fake systemctl, **built** frontend tests |
| P14 | Desired release and node sequencing on `update/desired` | fake status files |
| P15 | Frontend source: local fetch proxy socket | fake `ReleaseService` |

Dependencies: P0 and P1 first (P1 is the first merge); P2 before P3, P4, P5, P8; P4 before P5 and P14; P6 needs P0 fixtures; P7 needs P2 and the settings struct from P1; P8 needs P0, P6, P7; P9 needs P6; P10 needs P7, P9, P11 to run for real but tests against fakes; P13 needs P11, P12, P5; P15 needs P7. P4 changes the order and status writers and readers in one PR. `kbf-helper` and the old units stay until P5 lands.

## 10. Security rules
Threats: A compromised `kbf`; B compromised fetch child or release host; C local user; D old or malicious signed release. Root is out of scope.
- S1. Authority comes from the channel (section 4). S2. Wire orders act on a fixed code table of node and daemon units; unit and instance names never come from order content.
- S3. Release monotonicity: `release_seq` floor, `min_safe_seq`, trust-file `deny` list and `min_release_seq`; only `rollback` goes lower. S4. `source_refs` narrows only; no PR or feature-branch refs in the default.
- S5. One pass, one copy: hash while copying; the child is dead first. S6. Hash the staged file again and run the file that is renamed. S7. Every artifact is bound to the order commit (`--source-digest`). S8. Names and versions are validated, never free-form. S9. `SHA256SUMS` is advisory; trust is attestation plus manifest.
- S10. Spawn hygiene, caps, network rules, dedicated uids, download directory outside the root tree (section 5). S11. Fetch proxy (P15) checks `SO_PEERCRED` for `kbf-fetch`, serves only Fetch by sha256, never hands over the node identity.
- S12. `window_s` clamped. S13. One activation at a time, minimum interval, `previous` only confirmed. S14. Strict order parsing: file or body ids agree, duplicate keys refused, ids bounded.
- S15. `--root` absent from release builds; environment variables ignored; `gh`, `systemctl`, `launchctl` run by absolute path with an empty environment, `GH_NO_UPDATE_NOTIFIER=1`. S16. Trust files expire; the trust root cannot self-update, so revocation ships as a package. S17. The package signing key is the root of trust; packages are signed before any public channel (decision 7).
- S18. Envoy is pinned by attested sha256 and re-verified at every start; never overwritten.
- S19. Stage, fsync, rename, fsync parent, then state. S20. A directory is not proof (section 5 step 7). S21. Missing or corrupt `state.json`: use `.bak`, else write nothing and exit 5; never treat as empty. S22. `gc` never removes current, previous, staged, pending, or a release a process executes from; it takes the lock. S23. Links via `current.new` rename; `state.json` is the source of truth.
- S24. Confirm deadlines use monotonic plus wall time. S25. `serve` re-execs between orders if its binary is replaced; maintainer scripts call `quiesce`.
- S26. Frontend revert order per section 6. S27. Frontend orders need the control channel only.
- Residuals (accepted): a compromised `kbf` can drop, delay or burn order ids, and a bad new release can self-confirm; a compromised release pipeline producing valid attestations from an allowed ref defeats the design (mitigate with protected refs and S4).

## 11. Open decisions (recommendation first) and edits to other PRs
1. Frontend update on the wire: no; operator/control socket only. A wire field is a CEO decision.
2. Offers: keep for display and `frontend` source; stop consuming on nodes.
3. Release source for a farm without a public URL: any controlled static HTTPS location first; P15 later.
4. Envoy: pinned upstream URL, attested sha256 in the manifest, not mirrored.
5. Attestation bundles: `.sigstore.json` release assets.
6. `--deny-self-hosted-runners`: the build farm is self-hosted; decide whether the release workflow runs on GitHub-hosted runners (recommended, keep the flag) before P8.
7. Package formats: deb, rpm, macOS cask/pkg; repository signing keys decided before any public channel.
8. Mac component name `kbf-daemon` stays until a release needs the rename.
9. Order channel: socket (recommended, S1) over a spool directory.

Edits needed: #1024 (`[update]` leaves kbf.toml; roots; socket instead of inbox; `generation` removed), #1021 (no shipped first release; current path; `kbf-envoy@`), #966 (`ExecStart` path; gating; no formula for the root installer), #1020 (`--check` flag).
