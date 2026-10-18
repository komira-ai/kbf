# kbf-installer: contract

Status: design of record for the parallel PRs. Marks: **(built)** exists on `feature/pull-updates`; **(to build)** does not. Orders, the order spool, the fetch child, generation and the verification steps are agreed with `pull-updates.md` (#1024), which is their authority (section 12); settings, CLI, units and layout names are decided here, and #1021 (distribution), #966 and #1024 are edited to match (section 11).

## 1. Purpose
`kbf-installer` is the one root tool for setup and updates on every channel. It is the only writer of root-owned release files. It never updates itself; only the package manager does.
1. **kbf** is runtime only: modes plus flags, or `/etc/kbf/kbf.toml`. It never installs, fetches or restarts anything. It writes an order record into the installer's `orders/` directory (section 4) and reports status. kbf.toml has no update or installer keys.
2. **kbf-installer** installs, fetches, verifies, activates, confirms, reverts releases, and fetches and pins Envoy.
3. **The package** ships `kbf-installer`, the unit files (disabled) and the trust files. It ships no kbf runtime release; `setup` installs the first one.

Not goals: self-update, a distro Envoy dependency, an installer network listener, root logic in kbf.

## 2. CLI
One binary (**to build**; today a library plus `kbf-helper`). Mutating verbs are a dry run without `--apply` (**built**). Flags: `--json`, `--apply`; `--root DIR` exists only in builds with the `test-sandbox` feature. Exit codes: 0 ok; 1 failed; 2 usage or missing prerequisite; 3 refused (verification, policy); 4 busy; 5 needs operator. Every verb but `status`, `check`, `envoy list` needs root.

| Verb | What it does |
|---|---|
| `setup` | First install. `--mode`, `--release-source url\|frontend`, `--release-url`, `--min-generation`, `--commit`, `--no-start`. Fetches and verifies Envoy and the release through the same fetch child and checks as an update, writes `installer.toml` (the only writer), creates users and directories. Idempotent. |
| `status` | Current, previous, staged, pending confirm, Envoy, last refusal. `--json` equals `status.json` (section 4). |
| `run` | Run by `kbf-installer.service`, no listening socket. Each pass: finish an owed restart, revert an expired swap, sweep scratch, act once on each record in `orders/` in (epoch, id) order, write status. No release polling. |
| `update --commit SHA --frontend` | Operator order for the frontend host (section 6). Runs inline as root under the lock (exit 4 while a `run` pass holds it); no control socket. |
| `confirm`/`revert ID` | Operator override of a pending swap, naming the activation id; inline under the lock. Exit 5 if none pending. |
| `rollback` | `current` to `previous`, no window. Operator only; the only verb allowed below the release floor. |
| `gc` | Keeps current, previous, one more. State-aware (S22). |
| `boot-check` | Counts starts of a release on trial; reverts when the budget is spent (**built** logic). |
| `quiesce` | For maintainer scripts: takes the lock, exit 5 if a swap is pending (S25). |
| `envoy fetch\|verify\|path\|list` | Pinned Envoy under the installer tree. |
| `check` | Validates installer config, trust files, path chain. Lists every problem. |

`stage`, `activate`, `fetch-child` are internal.

## 3. Files, ownership, one source per setting
Three roots, never nested: kbf's own state `/var/lib/kbf` (user `kbf`, systemd StateDirectory, untouched by the installer); the installer tree `/var/lib/kbf-installer` (root:root 0755, not kbf's StateDirectory; everything the root side reads as authority lives here); package files. Only `orders/` in the installer tree is writable by `kbf`.

Package-owned (root:root, no group/other write, changed by package upgrade only):
- `/usr/bin/kbf-installer`, `/usr/lib/systemd/system/kbf-*.service`.
- `/usr/share/kbf-installer/trust/`: pinned `gh`, `trusted_root.jsonl` (with `valid_until`), `installer.conf` (signer workflow, OIDC issuer, repo, allowed source refs, `deny` commit list). Replaces `/usr/local/libexec/kbf-deploy`. Allowed source refs live **only** here; `installer.toml` may narrow them, never widen (set intersection).

Installer-owned (root:root):
- `/var/lib/kbf-installer/releases/<sha>/`: `kbf`, `kbf-daemon` where it applies, `units/`, and a `VERIFIED` marker written into the directory before it is renamed into place. Files 0755/0644; no setuid, setgid or file capabilities.
- `/var/lib/kbf-installer/releases/current`, `releases/previous`: relative links to `<sha>`. Frontend instances use `slots/<instance>` links (section 6).
- `/var/lib/kbf-installer/envoy/<version>-<sha256[:16]>/envoy` plus `envoy.sha256`.
- `/var/lib/kbf-installer/state.json` (0600, schema 2) and `state.json.bak`; it supersedes the schema 1 `helper.json` of #1024. `status.json` (0644, section 4). `work/<sha>.partial/` (0755, extraction; `dl/` inside it 0700 holds the tarball), swept at every start. `orders/` (`kbf:kbf` 0700). Lock `/run/kbf-installer/lock`.
- Layout constants live in `layout.rs`, one place. macOS: root-owned `/usr/local/kbf-installer` tree (not a user-owned Homebrew prefix), `/var/db/kbf-installer` for state.

`state.json` schema 2 (types fixed by the golden fixture, section 9 P1; `deny_unknown_fields`): `schema`, `current`, `previous` (confirmed releases only), `staged`, `pending_confirm {activation "<epoch>-<order_id>", commit, previous, deadline_wall, deadline_mono_s, starts_left}`, `envoy {current, previous}`, `floor {epoch, order_id, generation}`, `serving_instance` (frontend), `last_acked_order_id`, `unacked` (copy of the order being acted on, with root's first-seen time of each record in `orders/`). `floor` replaces the built, unused `order_floor`. Schema 1 to 2 migration (including the string activation id) belongs to P3.

Config:
- `/etc/kbf/kbf.toml`: kbf's runtime config, root:kbf 0640 (#966). The installer reads only its existence.
- `/etc/kbf/installer.toml` (root:root 0644, TOML, unknown keys refused): `release_source` (`url`|`frontend`), `release_url`, `source_refs` (narrowing only), `confirm_window_s` (default 900, bounds 120-1800), `confirm_window_max_s`, `min_generation`, `min_protocol` (operator floors), `fetch_timeout_s` (900, bounds 60-3600), `max_component_bytes` (512 MiB, at most 1 GiB), `min_activation_interval_s` (60), and for the `frontend` source `node_cert`, `node_key` (paths `setup` copies from enrollment). Source is read only from this file, never from an order, offer, environment or flag outside `setup`. This supersedes `[update]` in kbf.toml of #1024.

Users: `kbf` (runtime). `kbf-fetch` (`_kbf-fetch` on macOS): dedicated, no home, `nologin`, no groups, owns no file; it runs the download and the `--version --json` probe, never anything with credentials. There is no `kbf-verify` user.

## 4. Order handover
Channel decides authority, not a body field. There is no order socket, no control socket and no installer listener.
- **Wire orders** reach the installer only as files in `/var/lib/kbf-installer/orders/` (`kbf:kbf` 0700 under the root 0755 tree, so `kbf` traverses to it). `kbf-update` writes one record per order, to a temporary name starting with `.` and renames it into place; the installer ignores dot names. The installer opens `orders/` with `openat` relative to a descriptor it holds and `O_NOFOLLOW`, takes an entry only if `fstat` says a regular file with one link, reads it **once into memory** (at most 1 MiB) and parses from that copy, never re-reading a path; it then removes the entry by name through the same descriptor. At most 64 records are held. A record is expired and removed when older than the confirm-window maximum (1800 s) **measured from root's first-seen time** (kept in memory and in `state.json`), never the file's mtime, so a stuck or hostile writer cannot grow the directory or replay a stale order after a restart. A larger, unparsable, symlinked or non-regular entry is refused and removed. A record is a locator and a set of claims, not trust.
- **Operator orders** (`update --frontend`, `confirm`, `revert`, `rollback`) are CLI runs as root (section 2); `kbf` cannot run them.
- **Record**: `{ "order_id": u64, "epoch": u64, "generation": u64, "verb": "activate"|"confirm"|"revert", "commit": "<40 hex>", "window_s": u32 }`; for `activate` also the offer's components for the host's own platform (`name`, `sha256`, `size`, `attestation`, each at most 256 KiB of bundle) and the **expected facts** `platform`, `protocol`, `min_protocol`, `state_schema`, `profile` from the frontend's preflight. Unknown and duplicate keys refused. Nothing in a record is a path, URL, command or unit; unit names come from a code table (S2). The activation id is `<epoch>-<order_id>`. A node holding no offer for the commit writes no record and reports `refused` ("no offer for this commit").
- **Ordering**: (epoch, order_id), each once (**built**): older epoch and replays dropped, a jump above 1000 beyond the floor refused, a parse failure never advances the floor. **Also** (epoch, generation) lexicographically: within an epoch the generation must be at or above the highest acted on and at most 1000 above it per order; generation 0 refuses `activate`. A rollback is the old release ordered again with a higher generation. This stops a replay, not a forgery by a compromised `kbf`; against that the defence is the operator floor (`min_generation`, `min_protocol` in `installer.toml`), and the intended end state, a frontend-signed grant verified before any fetch, is not designed here.
- **Refused before any fetch** (reason in `status.json`): replay or older, commit not 40 lowercase hex, component set not exactly what the platform needs, size over `max_component_bytes`, expected facts missing (fail closed), a pending trial, below the floor, or a live lease. The installer observes leases itself (Linux: the cgroup of the action unit; macOS: the launchd job and processes by lease uid); the record's claim is never used.
- **Unacknowledged orders**: the record is removed after it is read, but the order is acknowledged only after the flip, so the installer keeps its own copy in `state.json` and reruns it from the start (nothing was flipped, so repeating is safe) unless it has expired.
- **`window_s`** is an input, clamped to `[120, confirm_window_max_s]`; on Linux the window starts at the first start (`boot-check`), so `window_s` is advisory there; on macOS it arms at activation.
- **Status**: `/var/lib/kbf-installer/status.json` (root-owned 0644, atomic rename; in the persistent tree, so it survives reboot), schema 2: **built** fields `installed_commit`, `previous_commit`, `staged_commit`, `state`, `refusal`, `acked_order_id`, `staged{...}`, plus `envoy {current, previous}`, the probe's reported facts, days left on the trust root and "unverified: rollback refused" flags. `state` words: `idle`, `fetching`, `staged`, `activated_unconfirmed`, `confirmed`, `reverted`, `refused`, each 1:1 with `UpdateStatus.State`. `helper_present` means installer present. `kbf-installer.service` starts before the runtime units (`Before=`); `/run/kbf-installer` (tmpfiles.d) holds only the lock.
- A swap pending refuses `activate` (`pending`, exit 4): one confirm window at a time, and `previous` is only ever a confirmed release. A confirm is accepted only if it names the pending activation, the unit is active, `/proc/<MainPID>/exe` is in `releases/<current>`, and the first boot-check has run.
- **macOS**: `kbf` runs as root there, so a compromise of `kbf` is a compromise of root; the separation of this section holds on Linux only. The fetch child and the probe still run unprivileged.

## 5. Fetch and verify
**Release source** (from `installer.toml`, never from an order): `frontend` (default; `ReleaseService.Fetch` by sha256 over mutual TLS, server authenticated by the pinned CA and the frontend name checked against the certificate SAN) or `url` (an HTTPS file server with the store layout `files/<sha256>`; system roots are the transport anchor only, bytes are trusted by verification, not TLS). `setup` refuses non-https, user information, query or fragment. **The child follows no redirect at all**; any 3xx is a failed fetch. No other source is contacted; a compromised frontend can only serve bytes the verification refuses.

**Assets**: tarballs `<name>-<40hex>-<platform>.tar.gz` (**built**, platforms `linux-x86_64`, `linux-arm64`, `darwin-arm64`), each attested by the release workflow as its own subject. The attestation bundle travels in the order record, from the offer the frontend published; it is self-authenticating, so the frontend is a courier, not a trust anchor. Hosts never fetch `SHA256SUMS` (advisory only) and never use `gh attestation download`. There is no release manifest: the facts a release declares come from `kbf --version --json` and are checked against the record (step 5).

**Fetch child** (`kbf-installer fetch-child`, **to build**; the same binary, one child per component, also used by `setup`): the root parent sets `setgroups(0)`, `setgid`, `setuid` to `kbf-fetch` (ids read back), `no_new_privs`, `chdir /`, an empty environment, closes every descriptor but 0 (`/dev/null`), 1 (byte pipe), 2 (log pipe) and 4 (lent identity), `RLIMIT_FSIZE` 0 (it cannot create a file), `RLIMIT_CORE` 0, `RLIMIT_NOFILE` 16, `RLIMIT_AS` 1 GiB, small `RLIMIT_NPROC`; Linux `PR_SET_PDEATHSIG`; macOS kills the process group when the pipe closes. Seccomp and `sandbox_init` are later hardening, not required. **Bytes pass through the pipe**: the child writes the tarball to stdout; the parent hashes while reading and writes a file it created in root-owned `work/<sha>.partial/dl/`, so the child has no object to swap or link. Stderr is read up to 4 KiB and never parsed, the rest drained; the parent uses `poll` and non-blocking reads. Caps: at most `size` bytes (one more kills the child, fewer is a failed fetch), connect 30 s, 60 s without a byte, `fetch_timeout_s` per component, no ranges or resume. Free space under the installer tree must cover the tarball, its extraction (1 GiB or ten times the declared size if smaller) and the release it replaces, computed before anything is opened. A short read, timeout or unreachable source is `refused` with that reason and is not retried; the frontend holds the run and a person re-issues it.
**Identity**: for the `frontend` source the parent lends the node certificate and key over descriptor 4 (Linux anonymous memory file; macOS socketpair; never disk). **A compromised child can steal the node key**: an accepted v1 residual (seven-day certificate). The follow-up is a fetch-scoped client certificate (a distinct extended key usage accepted only for `Fetch`), which replaces P15's proxy idea.

**Every root step is bounded**: wall-clock limit, process group killed on expiry, output read up to 4 KiB and parsed strictly; `gh` runs with an empty environment and empty `HOME`. Limits: 120 s `gh`, 120 s extraction, 10 s probe.

**Verification order**, by the root parent, on files it opened itself (`O_NOFOLLOW`, `fstat` regular and owned); nothing is unpacked or run before step 4:
0. Trust files and every path component of installer, `gh`, trust dir are root-owned, not links, not group/other writable (`openat2` walk). Trust `valid_until` not passed (exit 5), with days left in `status.json`.
1. **Count**: exactly `size` bytes arrived, within `max_component_bytes`.
2. **Digest**: the SHA-256 computed while reading equals the record's `sha256` **and** the attestation bundle's subject digest (the record's digest is only a locator).
3. **Attestation**, offline: pinned `gh` and `trusted_root.jsonl`, `--repo`, `--signer-workflow`, `--cert-oidc-issuer`, `--source-ref` (allowed refs), `--predicate-type`, `--deny-self-hosted-runners` unless decision 6 says otherwise, and `--source-digest` equal to the order commit (**built** `attest.rs` flags). Failure ends the order; nothing from the child is read further.
4. **Shape**: gzip and tar with exactly one regular file named `<name>` (no link, device, directory, absolute path or `..`), at most 1 GiB, extracted by the parent into `work/<sha>.partial/` as root, mode 0755. Binary kind and CPU checked; macOS quarantine refused and `codesign --verify --strict`.
5. **The binary names itself before the flip**: `<name> --version --json` run as `kbf-fetch` (empty environment, no credentials, no network that is enforced in v1) reports the order commit (12 hex prefix) and `protocol`, `min_protocol`, `state_schema`, `profile`, `platform` that must equal the record's expected facts, satisfy the operator floor, and be supported by this installer (else `installer_too_old`). Mismatch refuses before anything is renamed.
6. **Install**: write `VERIFIED` into the directory, rename to `releases/<sha>`, fsync the parent, then `staged` in `state.json`. A `releases/<sha>` without `VERIFIED` and not referenced by `current`, `previous` or `pending_confirm` is removed; a referenced one stays and is flagged "unverified: rollback to it refused". "Already installed" means `VERIFIED` exists, never skips verification of a new fetch, and a rollback to it skips steps 1-4 but reruns 5. Directories made by the test-farm switch never carry the marker.
7. **Envoy** (frontend only): its pin (`version`, `url`, `sha256`) is a component of the frontend release, attested and fetched like any other; sha256 checked, ELF machine check, `envoy --version` by descriptor as `kbf-fetch`, install into `<version>-<sha256[:16]>`, never overwriting a directory with different bytes.

**Refusals** (exit 3, stable `refusal` word): `trust_chain`, `hash`, `attestation`, `commit_or_ref`, `facts`, `installer_too_old`, `platform`, `size`, `disk`, `fetch`, `replay`, `generation`, `floor`, `pending`, `lease`, `denied`. Lock contention is exit 4, not a refusal.

## 6. Update sequences
**Node update** (the "update ready" click sets the desired release; `POST /v1/update/desired` of #1024 6.3 is the API, P14 builds on it). The frontend orders storage and daemon nodes one at a time through the drain system:
1. Drain, then `UpdateOrder{ACTIVATE, commit, generation}`; `kbf-update` writes the record (section 4) and does nothing else.
2. Installer fetches and verifies (section 5), then writes `pending_confirm`, flips `previous` then `current` (state first), then restarts the node units named in its code table.
3. Linux: window starts at the first start (`boot-check`), 3 start attempts; macOS armed at activation, `expire` triggers.
4. New kbf reports `ACTIVATED_UNCONFIRMED`; the frontend sends `CONFIRM`. A compromised new release can self-confirm; accepted residual (section 10).
5. No confirm by the deadline (monotonic plus wall), start budget spent, or `REVERT`: revert state first, then links, then restart; report `REVERTED`.
Crash at any step: `reconcile` makes links match `state.json` on next start (**built**). Fetch failure leaves `current` untouched and reports `refused`.

**Frontend update**: separate action, never on the wire. Entry: `kbf-installer update --commit SHA --frontend`, run by root; the release comes from the configured source with the components named by the operator's order, verified as in section 5.
1. Verify the release and, if the pin differs, Envoy.
2. Point `slots/<idle>` at the new release. Instances run `slots/%i/kbf`, so a crash restart of the serving instance never picks up the new release; `current` flips only at confirm.
3. `systemctl start kbf-frontend@<idle>`; handover per `frontend-handover.md` (**built**). Needs `--instance %i` in `kbf frontend` (**to build**, P13 adds the flag to `kbf-cli`).
4. If Envoy changes, start `kbf-envoy@<new>`, move the frontend, retire the old after drain.
5. Confirm: the successor reports ready (notify) and the old reports stood down inside `confirm_window_s`; the frontend's own handover signals are the confirm. Else revert: if the old instance already stood down, start it and wait for ready before flipping slots back.
6. `kbf frontend handover` is removed after this lands. Order records never touch `kbf-frontend@*` or `kbf-envoy@*`, on an all-in-one host too; this holds after P13, replacing "forbid until P13".

## 7. Units and gating
Plain files in `packaging/linux/units/`, installed disabled, never enabled by package scripts: `kbf-installer.service`, one unit per mode `kbf` offers (`frontend` as `kbf-frontend@.service`, `storage`, `daemon`, `storage-and-daemon`, `node`, `all-in-one`; P11 lists them from `kbf --help`), `kbf-envoy@.service`. Paths are literal; there are no `@...@` substitutions. The `check-units` rules (`handover`, `state-store`, `stop`, `credential`, `storage`) move with the units (P11).
```ini
[Unit]
ConditionPathExists=/etc/kbf/kbf.toml
[Service]
ExecStartPre=+/usr/bin/kbf-installer boot-check
ExecStartPre=/var/lib/kbf-installer/releases/current/kbf <mode> --check
ExecStart=/var/lib/kbf-installer/releases/current/kbf <mode>
RestartPreventExitStatus=2
```
`--check` prints every missing prerequisite and exits 2 (**to build**, P12; #1020 runs the same checks at every start). It is `ExecStartPre`, not `ExecCondition`, because `ExecCondition` skips silently on exit 1-254. `boot-check` runs as root (`+`); lock contention waits and neither counts as a start nor fails the unit. Node mode keeps its `kbf enroll` pre-start from #966.
`kbf-installer.service` (no socket unit): `Type=simple`, `Restart=on-failure` with exit 4 treated as success, `ConditionPathExists=/etc/kbf/installer.toml`, `ExecStartPre=/usr/bin/kbf-installer check`, hardening (`NoNewPrivileges`, `ProtectSystem=strict`, `ReadWritePaths=/var/lib/kbf-installer /run/kbf-installer`, `PrivateTmp`, trimmed capabilities); P11 lints the `systemd-analyze security` score. `kbf-envoy@.service` runs `ExecStartPre=+kbf-installer envoy verify` against the pin in `state.json`. Frontend template rules from the handover design apply (`Type=notify`, finite `TimeoutStartSec`, `RuntimeDirectoryPreserve=yes`, no `Conflicts=`).
macOS: launchd plists in `packaging/macos/`; nodes only, via cask or `.pkg` (a formula cannot place a root LaunchDaemon, #966). `kbf-installer` there is root-owned; a user-owned prefix is refused by `check`.

## 8. Compatibility and migration
- Wire: the frozen messages stay, plus the one additive change of #1024 section 5, made together with its tests: `UpdateOrder.generation = 6` and `UpdateOffer` `protocol = 5`, `min_protocol = 6`, `state_schema = 7`, `profile = 8` (older peers ignore them; zeros are refused fail closed). `update_order = 9`, `update_status = 7` unchanged.
- Offers stay on the wire and nodes consume them: the node builds the order record's components, attestation and expected facts from the offer it holds for the commit. `ReleaseService.Fetch`'s caller is the installer's fetch child, not `kbf`. The planner's "release complete" check reads the frontend's preflight row; a node order to an incomplete release is refused by the planner.
- Installer vs release: no manifest range. A release declares `protocol`, `min_protocol`, `state_schema` in `--version --json`; the installer refuses (`installer_too_old`) one above what it supports, and any state schema bump needs an installer that knows it.
- Migration is owned by P3: `setup` adopts `/usr/local/kbf` and `/var/lib/kbf-deploy` once (state schema 1 to 2, links, trust files), keeps a rollback path, and covers the test-farm hosts that run the dev installer. `kbf-helper` and `kbf-installer` never both manage one host: `setup` refuses while the old state exists unmigrated.

## 9. PR breakdown
Fakes: temp roots, fake clock, `file://` and local HTTPS source, recorder for `systemctl`/`launchctl`, fixture order records and bundles, a pinned fake `gh`.

| # | PR | Tests against |
|---|---|---|
| P0 | Lint of the attested set (tarballs and bundles reach the offer); `docs/artifacts.md` | xtask tests; fixture release |
| P1 | Binary and CLI skeleton, exit codes, `--json`, dry run, and **golden fixtures** for order record, status, state 2, `installer.toml` | golden tests; every later PR reuses them |
| P2 | `layout.rs` new roots, state schema 2, `floor`, `.bak`; update `kbf-deploy` paths and unit templates in lockstep | temp roots |
| P3 | `migrate.rs`: old layout and state 1 to 2, test-farm hosts | old-layout fixtures |
| P4 | `orders/` reader (openat, `O_NOFOLLOW`, read once, caps, first-seen expiry), status writer; `kbf-update` writes records; remove inbox and `helper.json` inbox reads | fake kbf writer, symlink/FIFO/oversize/mtime cases, replay/crash (**built** `contract_tests`) |
| P5 | Fold `kbf-helper` into `run`; activation, boot-check, reconcile, gc, quiesce | **built** `crash_tests`, systemctl recorder |
| P6 | `record.rs`: record parse, (epoch, generation) rule, floors, facts compare | fixture matrix |
| P7 | Fetch child (pipe, rlimits, lent identity) and URL source | local HTTPS server, size/timeout/3xx, Linux drop test |
| P8 | Verify pipeline `stage.rs` (section 5 order, lease check, bounded steps) | **built** attest/archive/binary tests, fake `gh`, hanging stubs |
| P9 | Envoy pin, fetch, verify, verbs | fake Envoy script, mismatch cases |
| P10 | `setup`, `settings.rs`, `installer.toml`, `check` path-chain, trust expiry | idempotency, partial failure |
| P11 | Units, nfpm, macOS pkg, unit lint, hardening score | `systemd-analyze verify`, container root |
| P12 | `kbf <mode> --check` for every mode | missing-prerequisite fixtures |
| P13 | Frontend update: slots, `--instance`, `kbf-envoy@` | fake systemctl, **built** frontend tests |
| P14 | Desired release and node sequencing on `update/desired` | fake status files |
| P15 | Fetch-scoped client certificate (extended key usage) replacing the lent node identity | fake `ReleaseService` |

Dependencies: P0 and P1 first (P1 is the first merge); P2 before P3, P4, P5, P8; P4 before P5 and P14; P6 needs P1 fixtures; P7 needs P2 and the settings struct from P1; P8 needs P0, P6, P7; P9 needs P6; P10 needs P7, P9, P11 to run for real but tests against fakes; P13 needs P11, P12, P5; P15 needs P7. P4 changes the record and status writers and readers in one PR, and the wire change of section 8 lands with it. `kbf-helper` and the old units stay until P5 lands.

## 10. Security rules
Threats: A compromised `kbf`; B compromised fetch child or release host; C local user; D old or malicious signed release. Root is out of scope.
- S1. Authority comes from the channel: only `orders/` carries wire orders (section 4). S2. Wire orders act on a fixed code table of node and daemon units; unit and instance names never come from order content.
- S3. Release monotonicity: (epoch, generation) rule, operator floors, trust-file `deny` list; only `rollback` goes lower. S4. `source_refs` narrows only; no PR or feature-branch refs in the default.
- S5. One pass: hash while reading the pipe; the child can create no file. S6. The probe runs on the file that is renamed, extracted by root. S7. Every artifact is bound to the order commit (`--source-digest`). S8. Names and versions are validated, never free-form. S9. `SHA256SUMS` is advisory; trust is the attestation, and the record's digest is only a locator.
- S10. Spawn hygiene, caps, network rules, dedicated uids, download directory outside the root tree (section 5). S11. The node identity is lent over a descriptor and a compromised child can steal it (accepted v1 residual; follow-up P15).
- S12. `window_s` clamped. S13. One activation at a time, minimum interval, `previous` only confirmed. S14. Strict record parsing: duplicate keys refused, ids and generation bounded, dot names ignored, one read into memory.
- S15. `--root` absent from release builds; environment variables ignored; `gh`, `systemctl`, `launchctl` run by absolute path with an empty environment, `GH_NO_UPDATE_NOTIFIER=1`. S16. Trust files expire; the trust root cannot self-update, so revocation ships as a package. S17. The package signing key is the root of trust; packages are signed before any public channel (decision 7).
- S18. Envoy is pinned by attested sha256 and re-verified at every start; never overwritten.
- S19. Stage, fsync, rename, fsync parent, then state. S20. A directory is not proof (section 5 step 7). S21. Missing or corrupt `state.json`: use `.bak`, else write nothing and exit 5; never treat as empty. S22. `gc` never removes current, previous, staged, pending, or a release a process executes from; it takes the lock. S23. Links via `current.new` rename; `state.json` is the source of truth.
- S24. Confirm deadlines use monotonic plus wall time. S25. `run` re-execs between orders if its binary is replaced; maintainer scripts call `quiesce`.
- S26. Frontend revert order per section 6. S27. Frontend orders need root on the CLI only. S28. Leases are observed by the installer, never taken from a record.
- Residuals (accepted): a compromised `kbf` can drop, delay or burn order ids (and on macOS is root), and a bad new release can self-confirm; a compromised release pipeline producing valid attestations from an allowed ref defeats the design (mitigate with protected refs and S4).

## 11. Open decisions (recommendation first) and edits to other PRs
1. Frontend update on the wire: no; operator CLI only. A wire field is a CEO decision.
2. Offers: nodes consume them to build the record (section 8).
3. Release source for a farm without a public URL: any controlled static HTTPS location serving the `files/<sha256>` layout; or the `frontend` source.
4. Envoy: pinned upstream URL, attested sha256 as a component of the frontend release, not mirrored.
5. Attestation bundles: stored with the release in the frontend's store and carried in the offer.
6. `--deny-self-hosted-runners`: the build farm is self-hosted; decide whether the release workflow runs on GitHub-hosted runners (recommended, keep the flag) before P8.
7. Package formats: deb, rpm, macOS cask/pkg; repository signing keys decided before any public channel.
8. Mac component name `kbf-daemon` stays until a release needs the rename.
9. Order channel: decided, the `orders/` directory (section 12), not a socket.

Edits needed: #1024 (section 12), #1021 (no shipped first release; `releases/current` path; `kbf-envoy@`), #966 (`ExecStart` path; gating; no formula for the root installer), #1020 (`--check` flag).

## 12. Agreed with pull-updates.md (#1024)
- Orders: kbf writes records into `orders/` (0700, kbf-owned) under the root-owned `/var/lib/kbf-installer`; the installer reads each once with `O_NOFOLLOW`; no order socket, no control socket; status returns through the root-owned `status.json`.
- Taken from #1024 over the first draft of this contract: generation and its floor, components and attestation in the record, expected facts checked before the flip, pipe-based fetch child (no download directory owned by `kbf-fetch`), no redirects, `files/<sha256>` URL layout, the probe as `kbf-fetch`, lent node identity, lease check by the installer, `releases/current`, `run` instead of `serve`, `VERIFIED` before rename. Dropped as a result: the release manifest, `release_seq`, `kbf-verify`, `/var/cache/kbf-fetch`.
- Decided here and overriding #1024: the update source and floors live in `/etc/kbf/installer.toml`, not `[update]` in `kbf.toml`; `state.json` schema 2 replaces `helper.json`.
