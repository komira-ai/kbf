# Deploy: one inventory, and kbf-deploy as the installer for every role

**Status: proposal.** Nothing in this document is built except what [section 1](#1-today)
says exists on `main`. It asks the maintainers to decide the questions in
[section 6](#6-decisions-asked-for) before any code is written.

A kbf farm today is installed by several tools that each know part of it: `kbf-deploy`'s
kits (`linux`, `pilot`, `minio`, `mdm`, `envoy`), `kbf-mac-provision`, and operators'
own shell kits that fill the gaps. Each reads its own settings file, and nothing says, in
one place, which host runs what. This document proposes:

1. **One inventory**: a single versioned file per farm that lists every host, its
   platform, its roles and each role's settings. kbf defines the format and validates it;
   the operator keeps the real file.
2. **`kbf-deploy` as the one installer**: every role (server, daemon, all-in-one, the
   object store's drop-in, the Mac host settings, later the MDM and the client front) is
   installed, upgraded, checked and removed by `kbf-deploy` from that inventory, on
   systemd (Linux) and launchd (macOS) behind one interface, with the plan, dry-run and
   apply engine that exists in `kbf-host`.
3. **A migration** in which the shell kits become thin wrappers over `kbf-deploy` and
   are then deleted, and a path for many hosts: rolling upgrades with drain and
   uncordon now, and the server-ordered rollouts of
   [fleet-updates.md](fleet-updates.md) later.

## 1. Today

### 1.1 Roles and what installs each

| Role | Binary | OS | Installed by today | Service manager | Gap |
|---|---|---|---|---|---|
| Server | `kbf-server` | Linux | `kbf-deploy linux install-units` (converged node), or `kbf-deploy pilot server-install` (one operator's user) | systemd system unit, or a systemd user unit | `install-units` always installs the server **and** the daemon, and refuses unless the object store is a systemd unit on the host (`NoMinioUnit`); no server-only profile |
| Daemon, container driver | `kbf-daemon` | Linux | `kbf-deploy linux install-units`, only together with the server | systemd system unit | no daemon-only profile; no `--cache-dir`/`--cache-max-bytes` in the unit; the lease directory is made `0700`, rootless Podman needs `0711` ([#594](https://github.com/komira-ai/komira-build-farm/issues/594)) |
| Daemon, native driver | `kbf-daemon` | macOS | **two installers**: `kbf-mac-provision apply` (its `launchd` item renders the LaunchDaemon from a profile, binary at `/usr/local/kbf/current`), and operators' shell kits with their own property list, layout and role account | launchd system daemon | neither is `kbf-deploy`; neither passes the cache flags the macOS input cache needs ([#712](https://github.com/komira-ai/komira-build-farm/pull/712)) |
| All-in-one | `kbf-all-in-one` | Linux | nothing: planned ([#657](https://github.com/komira-ai/komira-build-farm/pull/657)) | | |
| Mac host settings and lease-user pool | `kbf-mac-provision` | macOS | `kbf-mac-provision apply --profile` | | runs by hand on each Mac; not packaged as a release asset |
| Object store (MinIO) | none of kbf's | Linux | not by kbf. `kbf-deploy minio buckets` and `minio nodeport` prepare it; `linux render-units` writes a priority drop-in for a MinIO systemd unit | the operator's | a MinIO that runs as a pod is outside `install-units`' model |
| Client front (Envoy) | none of kbf's | Kubernetes | `kbf-deploy envoy render/check/apply` | a cluster, not a host | none for this document |
| Apple MDM services | upstream binaries | Linux | `kbf-deploy mdm fetch/install/tls/...` | systemd system units | none for this document |
| MDM gate | `kbf-mdm-gate` | Linux | nothing; not packaged | | |
| Update helper | `kbf-updater` | Linux | nothing; not packaged | | |
| Build client overlay | none | client | `kbf-deploy pilot buckconfig` | | client-side, not a host role |

Release assets ([artifacts.md](../artifacts.md)): `kbf-daemon` for `darwin-arm64`,
`linux-x86_64` and `linux-arm64`; `kbf-server` and `kbf-deploy` for the two Linux
platforms. `kbf-deploy` has no darwin asset because no CI job builds or tests it, or
`kbf-host`, on macOS.

### 1.2 What the shell kits still do that kbf-deploy does not

Operators who ran kbf before `kbf-deploy` covered their case keep shell kits. Across the
kits we know of, these are the jobs no `kbf-deploy` verb does:

1. **Host preparation on Linux**: create the service users, their subordinate uid and
   gid ranges and lingering, install Podman. `install-units` only checks these and
   refuses with the command that fixes each.
2. **A daemon-only or server-only node, with the object store as a pod**: render with
   `render-units`, then hand-edit and copy units, because `install-units` cannot.
3. **Certificates**: run the operator's CA, issue the server and node certificates, copy
   them to each host. `kbf-deploy` checks certificates (`pilot server-install` checks the
   chain and key; `install-units` checks the files exist) but issues none.
4. **Binaries**: download the attested assets of a `main` run or a release, check
   `SHA256SUMS` and the attestations, stage them on each host over `ssh`.
5. **The macOS daemon**: the role account, the directory layout, the property list, the
   load and the wait for the daemon's `welcomed` line, and the cache flags.
6. **Cross-host order and one-off migration steps**: which host goes first, removing a
   previous build farm's worker from a host, switching the clients' configuration,
   tearing the previous farm down, and the rollback of each. These belong to one
   migration, not to a farm's lifecycle, and stay out of kbf (section 4.3).

### 1.3 Facts the design builds on

- **`kbf-host` is the one way kbf changes a host** (`crates/kbf-host`): a `Planner` adds a
  file or directory step only when the host differs, `execute` prints `DRY-RUN:` lines
  by default and acts only with `Mode::Apply`, a root plan applies only as root from a
  binary root alone can replace, and a re-plan after an apply has no file steps.
  `kbf-updater` already applies through it.
- **`kbf-deploy` runs on the host it changes.** It has no remote transport.
- **Two host abstractions exist.** `kbf_host::Exec` with its fake `FakeExec` (Linux kits,
  `kbf-updater`), and `kbf-mac-provision`'s own `host::Host` with the test fake
  `FakeMac`. `kbf-mac-session`'s `MacHost` is the session helper's real host, not a
  deploy fake. Nothing named `LinuxHost` exists.
- **The daemon's argument list is built in two places**: the `linux` kit's
  `kbf-daemon.service` template and `kbf-mac-provision`'s `launchd::daemon_args`.
- **The settings files are `KEY=VALUE`** over a closed key set per kit
  (`crates/kbf-deploy/src/config.rs`); an unknown or repeated key is refused.
- **Upgrades keep one previous binary**: `install-units` keeps `<name>.previous`;
  `kbf-mac-provision`'s design keeps `/usr/local/kbf/<commit>/` with a `current` symlink
  ([mac-node-provisioning.md](mac-node-provisioning.md) section 4.3).
- **Drain exists**: `POST /v1/nodes/{node}:cordon`, `:drain`, `:uncordon` ([api.md](../api.md)),
  kept in the server's memory. `GET /v1/nodes` reports each node's `daemon_version` as
  the package version only, without the commit.

## 2. The inventory

### 2.1 Where it lives

- **kbf (public)** owns the format: the schema, a commented example
  (`docs/deploy/inventory.example.toml`), and the validator, `kbf-deploy inventory check`.
- **The operator** keeps the real file in their own repository and runs `inventory
  check` on it in their CI, with the `kbf-deploy` release they pin. kbf never sees it.
- **Addresses are optional in the file.** A host is named by its DNS name. An operator
  who will not commit addresses passes an uncommitted overlay (`--overlay FILE`: host id
  to address, nothing else), which `inventory check` merges and validates the same way.

### 2.2 Format

One TOML file, `schema = 1` on its first line, unknown keys refused at every level
(`serde` with `deny_unknown_fields`). TOML because the file needs comments (every value
on a farm has a reason, today written as comments in `KEY=VALUE` files) and nesting (a
host with several roles), and both are clumsy in `KEY=VALUE` and missing from JSON. It
adds one dependency, `toml`. A schema change bumps `schema`; `kbf-deploy` refuses a
version it does not know, in both directions.

```toml
schema = 1

[farm]
release = "v0.3.0"                 # the kbf release every role installs (2.4)
sums_sha256 = "<64 hex>"           # sha256 of that release's SHA256SUMS
ca_cert = "/etc/kbf/ca.pem"        # on every host
servers = ["storage-a.example.internal"]
object_store = { endpoint = "http://storage-a.example.internal:9000", bucket = "kbf-cas" }
images = ["docker.io/library/ubuntu@sha256:<64 hex>"]

[[host]]
id = "storage-a"                   # the node id; also the certificate's DNS name
dns = "storage-a.example.internal"
platform = "linux-x86_64"
ram_gib = 256
roles.server = { meta_log = "raft", raft_dir = "/var/lib/kbf-server/raft", reservation_gib = 16 }
roles.daemon = { scratch = "/var/lib/kbf/leases", cache_max_bytes = "200G", reserve_cpus = 4 }
roles.object_store = { kind = "systemd", unit = "minio.service", reservation_gib = 32 }

[[host]]
id = "mac-1"
dns = "mac-1.example.internal"
platform = "darwin-arm64"
roles.mac_host = { profile = "mac-studio" }
roles.daemon = { scratch = "/Volumes/kbf/leases", cache_max_bytes = "100G", labels = ["pool=darwin"] }
```

### 2.3 What it records per host

| Field | Why |
|---|---|
| `id`, `dns` | node id, certificate name, what daemons dial |
| `platform` | picks the release asset and the service backend; a role not built for it is refused |
| `ram_gib`, `cpus` (optional) | the memory split the `linux` kit computes today, and `--reserve-cpus` bounds |
| `roles.<role>` | each role's own closed set of settings (2.5) |
| paths | scratch, cache, tree, Raft and log directories; the certificate and key **paths** |
| `labels` | the daemon's `--label` set |

### 2.4 What it never records

**No secret.** Not a private key, not the object store's credentials, not an MDM push
key, not an operator token. The inventory names the **path** where each secret is on
the host; `kbf-deploy` checks its owner and mode and, for a certificate, that it chains
to the CA and matches its key (as `pilot server-install` does today), and never reads a
secret into a plan line, an argument or a log. A secret reaches a service as it does
today: a key file only its service reads, or an `EnvironmentFile` (the object store
key), never an argument.

### 2.5 Validation

`kbf-deploy inventory check --inventory FILE [--overlay FILE]` reads only and exits 0,
1 (a rule broken, one `FAIL <rule>: <why>` line each), or 2 (refused: does not parse).
Rules, each with a test that plants the broken case:

- unique host ids and DNS names; every name in `farm.servers` is a host with the
  `server` (or `all-in-one`) role; one server or three (the shapes of
  [deployment-topology.md](deployment-topology.md));
- a role allowed on the host's platform (`server` and `all-in-one`: Linux only;
  `mac_host`: macOS only);
- `reserve_cpus` only on `daemon` and `all-in-one`, and below the host's CPU count;
- the per-host memory split of today's `linux` kit (RAM minus the object store, the
  server and at least 4 GiB of headroom leaves the builds at least 1 GiB);
- the cache directory outside the scratch directory and on the same filesystem (the
  daemon's own rule, [worker-cache.md](worker-cache.md)); no cache on the Linux native
  driver;
- every rendered unit or property list parses with the binary's own argument parser,
  as `crates/kbf-it/tests/deploy_units.rs` does for the documented units today.

`kbf-deploy inventory render --inventory FILE --out DIR` writes every host's units and
property lists into `DIR/<host>/`, so a reviewer diffs a change to the farm as files.

## 3. kbf-deploy as the single installer

### 3.1 Commands

```text
kbf-deploy inventory check  --inventory FILE [--overlay FILE]
kbf-deploy inventory render --inventory FILE [--overlay FILE] --out DIR
kbf-deploy host check     --inventory FILE --host ID [--role R]...
kbf-deploy host apply     --inventory FILE --host ID [--role R]... [--assets DIR] [--apply]
kbf-deploy host uninstall --inventory FILE --host ID [--role R]... [--apply]
```

`host` verbs run on that host, as root for system services, and refuse when the
running host is not `ID` (hostname and platform). Without `--role`, every role the
inventory gives the host. `apply` installs and upgrades: there is no separate upgrade
verb, because an upgrade is an install whose plan differs in the binary and a restart.
The exit codes are those of the `linux` kit's host verbs today (0 done or nothing to do,
1 a step or the check after an apply failed, 2 refused).

### 3.2 The role model

```rust
/// One role on one host: what it needs, and the plans that install and remove it.
trait Role<E: Exec> {
    fn name(&self) -> &'static str;
    /// Read-only: one PASS or FAIL per item, as `kbf-mac-provision check` prints.
    fn check(&self, host: &HostEntry, exec: &E) -> Report;
    /// The steps that bring the host to this role at the inventory's release.
    fn plan_apply(&self, host: &HostEntry, svc: &dyn ServiceManager<E>, p: &mut Planner<'_, E>) -> Result<(), Refusal>;
    fn plan_uninstall(&self, host: &HostEntry, svc: &dyn ServiceManager<E>, p: &mut Planner<'_, E>) -> Result<(), Refusal>;
}
```

Roles in the first version: `server`, `daemon`, `all-in-one` (once #657 builds it),
`object_store` (kind `systemd`: the priority drop-in; kind `external`: nothing on the
host, buckets only), `mac_host`, and `host_prep` (Linux users, subordinate ids,
lingering, Podman; implied by `daemon` with the container driver). Later: `mdm`,
`mdm_gate`, `updater`. The client front stays a cluster verb (`envoy`), outside `host`.

### 3.3 Service backends

```rust
/// A service manager: where a service's file goes, and how it is started and asked about.
trait ServiceManager<E: Exec> {
    fn render(&self, spec: &ServiceSpec) -> Result<Vec<u8>, Refusal>;
    fn file(&self, name: &str) -> HostPath;            // the unit or property list
    fn plan_reload_and_restart(&self, p: &mut Planner<'_, E>, names: &[&str]);
    fn plan_stop_and_disable(&self, p: &mut Planner<'_, E>, names: &[&str]);
    fn state(&self, exec: &E, name: &str) -> Result<ServiceState, HostError>;
}
```

- `SystemdSystem` (`/etc/systemd/system`, `systemctl`), `SystemdUser` (the pilot's user
  service), `LaunchdSystem` (`/Library/LaunchDaemons`, `launchctl bootstrap`,
  `kickstart -k`, `print`).
- `ServiceSpec` holds what both managers express: the argument list, the user, restart
  on failure, the open-file limit, the stop timeout. Linux-only resources (slice,
  `Delegate=`, `MemoryMin=`, `OOMScoreAdjust=`, `CPUWeight=`) are a `linux` field that
  the launchd backend refuses if set, so a spec never silently drops a limit.
- **The systemd unit text stays template-based.** The `linux` kit's templates and
  `check-units`' priority rules work and are tested; only the argument list moves to the
  shared builder below.
- **One argument builder per binary**: `daemon_args(host, farm)`, `server_args(...)`,
  used by every backend and by `kbf-mac-provision`, whose own `daemon_args` is deleted.
  Defaults it applies:
  - **Cache on by default**: `--cache-dir` (on the scratch volume, outside `--scratch`)
    and `--cache-max-bytes` for the container driver on Linux and the native driver on
    macOS; never for the native driver on Linux, which refuses it.
  - `--reserve-cpus` only for `daemon` and `all-in-one`.
  - **Flags, not environment variables**: configuration reaches a binary as flags. The
    environment carries only what the platform sets (`XDG_RUNTIME_DIR` for rootless
    Podman) and secret material through `EnvironmentFile`.

### 3.4 Plan, dry run, apply, idempotence

Every role plans through `kbf_host::Planner` and runs through `kbf_host::execute`, so
the existing guarantees hold for every role: a dry run asks the host the same questions
an apply does, prints `DRY-RUN:` lines, and changes nothing; a re-plan after an apply
has no file steps; a root plan runs only as root from a root-owned binary. Every role
gets the test the `linux` kit has today: apply on the fake host, plan again, assert the
second plan is empty.

`kbf-host` must build and be tested on macOS first: `user.rs` looks users up with
`getent`, which macOS lacks (`dscl` or `getpwnam_r` instead), and no CI job runs it on
macOS. `kbf-mac-provision` moves from its own `host::Host` to `kbf_host::Exec`, so one
fake (`FakeExec`) covers both platforms and the second abstraction goes.

### 3.5 Binaries, versions and rollback

- **Layout**: every binary under `<prefix>/<version>/` with a `current` symlink, which
  units and property lists name (`/opt/kbf` on Linux, `/usr/local/kbf` on macOS, as
  [mac-node-provisioning.md](mac-node-provisioning.md) section 4.3 already designs). A
  `deployed` file records the version and each binary's SHA-256, so `check` reports a
  binary changed by hand. This replaces `<name>.previous`.
- **Where the bytes come from**: the inventory's `farm.release`. `--assets DIR` takes
  files staged by the operator; without it `apply` downloads the release's assets for
  the host's platform (the existing `https` client). Either way it refuses an asset whose
  SHA-256 is not the one in that release's `SHA256SUMS`, and a `SHA256SUMS` whose own
  SHA-256 is not `farm.sums_sha256`. The attestation check stays off the host, where
  `gh` runs ([artifacts.md](../artifacts.md)); the inventory's pinned sum carries its
  result to the host.
- **Upgrade on one host**: stage the new version beside the old, check its `--help`
  names every flag the unit passes (as `install-units` does), switch `current`, restart,
  wait for health (3.6). If health never comes, switch `current` back, restart, and exit
  1. The previous version stays until the next upgrade.
- **Rollback** is `apply` with the older release in the inventory; no separate verb.
- **Missing assets**: `kbf-mac-provision`, `kbf-updater` and `kbf-mdm-gate` are not
  release assets, and `kbf-deploy` has no darwin asset. Each role whose binary is not
  packaged is refused until it is.

### 3.6 Health

- `server`: `GET /readyz` on the API listener is 200 (as `install-units` waits today).
- `daemon`: the server's `GET /v1/nodes` shows the node `connected`, reporting the new
  build. This needs the daemon's build commit in `daemon_version`, which today carries
  only the package version; until then, the daemon's `welcomed` log line after the
  restart.

## 4. Migration

### 4.1 Steps in kbf (none touches a host)

| Step | Delivers | Deletes | Proof |
|---|---|---|---|
| K1 | inventory schema, `inventory check` and `render`; the `linux` kit's settings derived from a host entry | nothing yet | a converted example renders **byte for byte** the units `linux render-units` writes from today's `config.example`; one planted inventory per rule turns `check` red |
| K2 | the `linux` roles split: `server`, `daemon`, `object_store` (systemd or external), `host_prep`; lease directory `0711` (#594) | the "always both, always a MinIO unit" path of `install-units` | `FakeExec` plans per role; mutants: a daemon-only plan that writes `kbf-server.service`; `0700` again |
| K3 | versioned layout, release fetch, sum checks, upgrade with switch-back | `<name>.previous` | `FakeExec`: health never comes, the plan switches back; mutant: no switch-back. A flipped byte in an asset is refused |
| K4 | `kbf-host` on macOS; the launchd backend; the macOS `daemon` and `mac_host` roles; `kbf-mac-provision` on `kbf_host::Exec` and the shared `daemon_args`; a macOS CI job for `kbf-host` and `kbf-deploy`; darwin assets for `kbf-deploy` and `kbf-mac-provision` | `kbf-mac-provision`'s `host::Host`, its `daemon_args` and its plist writer (one writer of the property list) | `FakeExec` launchd plans on every platform; on the hosted macOS runner as root (`cargo xtask sudo-test`, as `kbf-mac-provision`'s `as_root` does) a real `launchctl bootstrap` of a stub daemon, then uninstall leaves nothing |
| K5 | the pilot's server as the `server` role on `SystemdUser`; `pilot buckconfig` stays as a client verb | `pilot server-install` after one release that carries both | the old verb's tests ported to the role |
| K6 | `daemon_version` carries the build commit | | a unit test on the reported string |

The `--config FILE` (`KEY=VALUE`) form of `linux` and `pilot` stays for one release
after K1 and K5 with a deprecation line, then goes: two ways to describe a host must not
both live on. `mdm` and `envoy` keep their configs until they become inventory entries
(a later step, open question 4).

### 4.2 Steps in an operator's repository

Each kit becomes a wrapper that calls `kbf-deploy host apply` with the operator's
inventory (and keeps the operator's own checks around it), then is deleted once the
wrapper has run on every host once:

| Kit job (1.2) | Becomes |
|---|---|
| Linux node install (1, 2) | `kbf-deploy host apply --role host_prep --role daemon [--role server]` |
| Binaries (4) | `--assets DIR` after the operator's attestation check, or the release fetch |
| macOS daemon (5) | `kbf-deploy host apply` on the Mac (`mac_host`, `daemon`) |
| Certificates (3) | stays the operator's until kbf issues node certificates (open question 3) |
| Cross-host order, previous-farm steps (6) | stays the operator's; retires with that migration |

**What waits.** A kit that an in-progress migration runs (a cutover runbook calls it by
path, and its tests are the only gate on a fix it carries) is switched only after that
migration is declared done. The kbf steps K1 to K6 land before, since they change no
host.

### 4.3 Out of scope for kbf

Removing another build farm's workers, switching clients from one farm to another, and
tearing the old farm down are one-time steps of one operator. kbf does not model them.

## 5. Scale

- **Fan-out**: `kbf-deploy` changes only the host it runs on. Running it on N hosts is
  the operator's loop (`ssh host kbf-deploy host apply ...`), in the order the
  inventory lists hosts. `inventory render` lets CI show the whole farm's change before
  any host runs it.
- **Rolling upgrade now** (before server-ordered rollouts): per daemon host, the
  operator's loop calls `:drain` with a deadline, waits for `drained`, runs `host apply`
  with the new release, waits for health (3.6), then `:uncordon`. A drain that pauses at
  its deadline stops the loop; nothing proceeds by itself
  ([fleet-updates.md](fleet-updates.md) section 4.3). A helper verb, `kbf-deploy node
  drain|uncordon --server URL` (operator token from a file), avoids each operator
  writing the API calls (open question 2).
- **Servers**: one server means a short outage per upgrade; with `--meta-log=raft` the
  cache survives the restart and daemons reconnect on their own. Cordons live in the
  server's memory and are lost at its restart. At three servers: transfer leadership
  away first, one server at a time (fleet-updates.md section 4.2, pre-gate).
- **Later, server-ordered**: once the rollout driver and `kbf-updater` reach phase P2 of
  fleet-updates.md, a daemon host's version changes through a signed software set the
  server orders, not through `kbf-deploy`. `kbf-deploy` then installs a host the first
  time, changes its configuration, and installs `kbf-updater` itself. Both write the
  same versioned layout (3.5) through the same `kbf-host` engine, so neither undoes the
  other.

## 6. Decisions asked for

| # | Question | Recommendation |
|---|---|---|
| 1 | Inventory format: TOML (new `toml` dependency), JSON (no new dependency, no comments), or a directory of `KEY=VALUE` files, one per host (today's parser, no nesting)? | TOML, `deny_unknown_fields`, `schema = 1` |
| 2 | Does `kbf-deploy` gain remote execution (`ssh`) and a rollout loop, or stay host-local, with fan-out and drain left to the operator's loop plus a small `node drain/uncordon` verb? | host-local, plus `node drain/uncordon`; server-ordered rollouts (fleet-updates.md) are the long-term fan-out, so a second orchestrator in `kbf-deploy` would be thrown away |
| 3 | Certificates: does `kbf-deploy` make the node's key and CSR on the host (mac-node-provisioning.md section 9.2), leaving signing to the operator's CA, or stay check-only? | key and CSR on the host; signing stays the operator's |
| 4 | Do the MDM services and the client front become inventory entries, or keep their own configs? | keep their configs in the first version; revisit after K5 |
| 5 | Default cache size when the inventory gives none: refuse, or a share of the scratch volume? | refuse: a wrong default fills a disk silently |
| 6 | Upgrade failure on one host: switch back automatically (3.5), or stop and leave the new version for an operator? | switch back on that host, and stop the loop |
| 7 | Package `kbf-mac-provision`, `kbf-updater` and `kbf-mdm-gate` as release assets now, or each when its role lands? | each with its role step (K4 for `kbf-mac-provision`) |
