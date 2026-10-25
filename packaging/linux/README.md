# kbf: the Linux package

One package, `kbf`: the runtime `/usr/bin/kbf`, the systemd units of its modes and `kbf.slice`,
the users and directories they run with, and the trust directory that verifies attestations
offline. Installing a newer `kbf` package is the update: the package manager replaces
`/usr/bin/kbf`. (Until v0.0.7 a second package, `kbf-installer`, carried the users, directories,
slice and trust directory and the proxy; releases no longer publish it, and the `kbf` package
takes over its files: see "Upgrading a host that has kbf-installer" in
[docs/artifacts.md](../../docs/artifacts.md#upgrading-a-host-that-has-kbf-installer).) The
contract is `docs/design/kbf-installer.md`, sections 3 (files and users), 7 (units and gating)
and 9 (P11). A deb and an rpm are built by [nfpm](https://nfpm.goreleaser.com), pinned in CI by
SHA-256, from a configuration `cargo xtask pkg-config` writes: this directory holds the inputs,
and `crates/xtask/src/pkg_linux.rs` is the one place that says what goes in it.

| path | what |
|---|---|
| `sysusers.d/kbf-installer.conf` | the users `kbf`, `kbf-server`, `kbf-fetch`, `kbf-signer` (no login) and the group `kbf-signer-clients` (the signer and `kbf-server` are its members: see `kbf-signer --client-group`); installed to `/usr/lib/sysusers.d/kbf.conf` |
| `tmpfiles.d/kbf-installer.conf` | `/run/kbf-installer` (0755), `/var/cache/kbf-fetch` (0711); the units author's file, packaged unchanged; installed to `/usr/lib/tmpfiles.d/kbf.conf` |
| `needrestart/kbf.conf` | tells needrestart never to restart a unit named `kbf-*`, `kbf.*` or `kbf@*`; installed to `/etc/needrestart/conf.d/kbf.conf` (0644, a config file the host may edit) |
| `units/` | the unit files, installed to `/usr/lib/systemd/system`: the five mode units and `kbf.slice` go in the package; `kbf-installer.service`, `kbf-proxy.service` and `kbf-proxy.socket` are the retired installer's and proxy's and are in no package (a test names them); `testdata/units` is the fixture for the unit tests only |
| `testdata/` | fixtures for the package tests only: gated units (the five mode units and the slice among them) and three stand-in trust files |
| `test/ubuntu`, `test/fedora` | the fresh systems the install test uses, systemd as PID 1, pinned by digest |

(The source files keep the name `kbf-installer.conf`: `packaging/install.sh` and `cargo xtask
installer-tarball`, which no release publishes, still place them under it.)

## What the package holds

- `/usr/bin/kbf` (0755), the `kbf` the same job built, byte for byte the one in its release tarball;
- the unit of every mode, the five files `kbf-all-in-one.service`, `kbf-daemon.service`,
  `kbf-frontend.service`, `kbf-storage-and-daemon.service` and `kbf-storage.service`
  (`KBF_UNITS`), and `kbf.slice` (`SLICE`), in `/usr/lib/systemd/system` (0644). `cargo xtask
  pkg-config` fails when the unit directory lacks one;
- `/usr/share/kbf/trust/`: `gh` (0755), `trusted_root.jsonl`, `installer.conf` (0644). The
  directories above it, `/usr/share/kbf` and the trust directory, are the package's too;
- `/usr/lib/sysusers.d/kbf.conf` (the users and the group) and `/usr/lib/tmpfiles.d/kbf.conf` (the
  unit author's file, packaged unchanged; `/var/lib/kbf-installer` is made by `kbf-installer
  setup`, not at install);
- `/etc/needrestart/conf.d/kbf.conf` (0644): on Ubuntu, needrestart restarts every service that
  maps a library an unattended upgrade replaced. A restart of a storage member in the middle of
  a build leaves the blobs only that member holds unreadable for the restart, and the client's
  build fails. This drop-in sets needrestart's `override_rc` for kbf units to 0, so only kbf's
  own update path restarts them. A host without needrestart ignores it;
- nothing else, all of it `root:root`, with one time (`SOURCE_DATE_EPOCH`) on every file and the
  package;
- dependencies (deb) `crun, uidmap, systemd, ca-certificates`, (rpm) the same with Fedora's name
  for `uidmap`, `shadow-utils`, and a suggestion of `postgresql` (rpm `postgresql-server`) for a
  frontend or all-in-one host that keeps its state on the host; no dependency on another package,
  and none on `kbf-installer` (`pkg-kbf-asset` refuses a deb whose `Depends` names it);
- in the deb only, `Replaces: kbf-installer (<< 0.1.1)` and `Breaks: kbf-installer (<< 0.1.1)`
  (`KBF_DEB_REPLACES`, `KBF_DEB_BREAKS`; every `kbf-installer` deb that was published is
  `0.1.0+<commit>`). The mode units, the slice, the needrestart drop-in and the trust files were
  files of the `kbf-installer` package, and dpkg refuses to unpack `kbf` over files another
  package owns unless `kbf` says it replaces that package; `Breaks` makes the package manager
  remove the old package when it installs this one, so nothing is left beside it with the
  rest of its files (its binary, the proxy's units). The files change hands, the users and the
  directories stay, and removing the old package afterwards takes none of them:
  `cargo xtask pkg-upgrade-test` runs exactly that, with the real apt and dpkg. The rpm names
  nothing: an rpm `Obsoletes` would remove the old package, and no rpm is published.
  `pkg-kbf-asset` checks the deb's `Depends`, `Replaces` and `Breaks`.

### Which frontend unit

A frontend host runs `kbf-frontend.service`, and may put `kbf-proxy` in front of the frontends.

- `kbf-frontend.service`: a frontend. One process, `kbf
  --mode=frontend --config /etc/kbf/kbf.toml`, run from `/usr/bin/kbf` (no slots, no
  `--instance`, no `--handover-socket`); listeners, state store and certificates are
  `kbf.toml`'s. An update is a restart after the `kbf` package is upgraded.
- `kbf-proxy` (the unprivileged proxy in the `kbf-installer` package: the public port and the
  load balancer over the frontends of its pool, which run on their own; it starts none of
  them).

`pkg-config` refuses a unit directory without the unit of a mode or the slice and a trust
directory with a fourth file or without one of the three; the other units of the directory are
not packaged.

The socket-handover template (`kbf-frontend@.service`, instances `a` and `b`) is no longer
packaged: `kbf` serves no control socket
(`--instance`, `--handover-socket` and the other flags of the removed handover are accepted,
ignored and logged as retired).

Never Podman, never Envoy (a test pins it): nothing in the package depends on, installs or checks
Envoy, and kbf runs none.

## Installing starts and enables nothing

The package has no maintainer script (`xtask pkg-install-test` fails on one, in both formats; the
configuration names none, which a test in `pkg_linux/tests.rs` pins), so nothing runs at install,
upgrade, remove or purge: no unit is enabled, started or restarted, no user or directory is
created by a script.

- Units ship disabled. Each gates on its configuration (`ConditionPathExists=` of
  `/etc/kbf/kbf.toml` for a mode unit); the test starts each `--gate` unit of the package
  without its file (skipped, `ConditionResult=no`, no `ExecStart` run) and with it (condition
  holds). The mode units' gates are held by
  `crates/kbf-integration-test/tests/package_units.rs`, which reads the files.
- **Upgrading `kbf` restarts nothing.** `apt install kbf` or `dnf upgrade kbf` replaces
  `/usr/bin/kbf`; a running process keeps the old file. An operator (or the tool that drains a
  node and installs the new package) restarts the mode unit after the install
  (`systemctl restart kbf-daemon.service`; a stop drains: a storage node finishes the calls in
  flight, a daemon its leases). The `kbf` package carries no script that restarts a unit, and
  no other mechanism does it yet.
- `/etc/kbf/kbf.toml`, the file those gates test, is written by `kbf-installer setup`, not by
  the package: for a frontend or all-in-one host from flags that name the listeners, the
  certificates and keys (which the operator provides, owned by the unit's user) and, for a
  frontend, the storage node, for an all-in-one host the cold store and the daemon half's
  identity (`docs/deploy/kbf-installer-setup.md`, "The `kbf.toml` flags"). The unit's
  `ExecStartPre=... --check` reads that file, so a file `setup` wrote is one `kbf --check` accepts.
  On a host with `/usr/bin/kbf` and no release tree, `setup` starts the unit of a node mode as it
  does for a host with a release.
- The users and directories come from `systemd-sysusers` and `systemd-tmpfiles`, which
  run at boot, and from `kbf-installer setup`. After `dpkg -i` and before either, they do
  not exist; the test runs both commands as boot does and checks the result.
- `remove` deletes the packaged files and keeps `/etc/kbf`, `/var/lib/kbf-installer` and
  the users. `purge` (deb) does the same and drops the package's record: the package owns
  none of what `setup` and the operator write, so it deletes none of it.

## In CI: the real packages, attested

`beta.yml`'s `packages` job builds the deb and the rpm from the test-only stub and the real
units and installs the deb in Ubuntu and the rpm in Fedora. `artifacts.yml` builds the real
ones: each Linux job (`linux-x86_64` on `ubuntu-24.04`, `linux-arm64` on `ubuntu-24.04-arm`,
each with its own pinned nfpm and `gh`) runs the same verbs on the `kbf` it has just built,
with a trust directory `cargo xtask pkg-trust` writes (the pinned `gh`, the Sigstore trusted
root `gh` fetched, and an `installer.conf` whose `SOURCE_REFS` is the branch the package is
attested on), installs the deb in a fresh Ubuntu container (`cargo xtask pkg-install-test`, with
`--probe`s that the packaged `gh` runs and the binary names the commit), takes it over a host
that has the previous release's `kbf-installer` and `kbf` debs (`cargo xtask pkg-upgrade-test`:
the debs are fetched from that release and checked against SHA-256 pins in the workflow), and
`cargo xtask pkg-kbf-asset` makes `kbf-<commit>-<platform>.deb` an asset, listed in `SHA256SUMS`
and attested with a bundle, refused unless its `/usr/bin/kbf` is the `kbf` of the job's own
tarball and its trust directory is for the branch it is attested on
([the `kbf` package](../../docs/artifacts.md#the-kbf-package-deb)).
Two call sites, one way to build: both go through `pkg-config` and `pkg-build`.

## What the units run

- `kbf-installer.service` (in no package now; it stays in `units/` for `packaging/install.sh`) runs `/usr/bin/kbf-installer run`. It names no unit: `run` restarts
  the unit `setup` enabled for the `mode` of `/etc/kbf/kbf.toml` (on a frontend or all-in-one
  host it also reads the state database from `/etc/kbf/installer.toml`), so one packaged unit
  serves every mode. `--unit` and `--launchd-label` still name the service by hand. The unit has
  no `ExecStartPre=`: the contract's `check` is not built, and a stub would fail every start.
- Each mode unit runs `/usr/bin/kbf --mode=<mode>`, the `kbf` package's file, after
  `kbf boot-check --mode=<mode>` (which brings the state database named by the `database` key of
  `/etc/kbf/installer.toml` to the schema that `kbf` itself needs, for the modes that keep state
  in it, and fails the start if it cannot) and `kbf --mode=<mode> --check`. The installer's own
  `boot-check --kbf=<kbf> --mode=<mode>` stays for one release for the units of the earlier
  packages: it runs `<kbf> boot-check --mode=<mode>` over the host's `installer.toml` and ends
  as that ended. It needs a `kbf` that has the verb: upgrade the `kbf` package with the
  installer or before it.
- `crates/kbf-integration-test/tests/package_installer_verbs.rs` parses every `kbf-installer` command line of
  these units, and of the macOS property list, with the binary's own parser and refuses a verb that
  is not built; it also holds every `kbf` the units exec to `/usr/bin/kbf`.

## Commands

```
cargo xtask pkg-config --kbf <kbf> --units packaging/linux/units \
  --trust <dir of gh, installer.conf, trusted_root.jsonl> \
  --sysusers packaging/linux/sysusers.d/kbf-installer.conf \
  --tmpfiles packaging/linux/tmpfiles.d/kbf-installer.conf \
  --needrestart packaging/linux/needrestart/kbf.conf \
  --platform linux-x86_64 --version <v> --out <dir>
      # <dir>/nfpm.yaml and listing.txt
cargo xtask pkg-build --nfpm <nfpm> --config <dir>/nfpm.yaml --out <pkgs>
cargo xtask pkg-install-test --name kbf --package <file> --format deb|rpm --listing <dir>/listing.txt \
  --image-dir packaging/linux/test/ubuntu --gate kbf-daemon.service=/etc/kbf/kbf.toml ...
cargo xtask pkg-upgrade-test --old-installer <kbf-installer deb> --old-kbf <kbf deb> \
  --package <pkgs> --listing <dir>/listing.txt --image-dir packaging/linux/test/ubuntu
cargo xtask pkg-kbf-asset --package <pkgs> --platform linux-x86_64 --commit <sha> \
  --kbf-tarball <kbf-sha-platform.tar.gz> --expect-ref refs/heads/<branch> [--gh <gh>] \
  (--out <dist> | --verify-only)
```

`pkg-build` builds each package twice with the sources' modification times changed
between the builds, and fails unless both are the same bytes (nfpm takes every time from
`SOURCE_DATE_EPOCH`, which the verb sets; the rpm's build host is fixed in the
configuration).

## CI

The `packages` job of `.github/workflows/beta.yml` fetches the pinned nfpm, builds the
test-only `kbf-installer-stub` and the package (the stub stands for `kbf`), then runs
`pkg-install-test` on the deb in Ubuntu and the rpm in Fedora. The real binary replaces the
stub by one path change: the `--kbf` of `pkg-config`.
