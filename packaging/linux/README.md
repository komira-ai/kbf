# kbf-installer: the Linux packages

`kbf-installer` is the one package of the farm. It is the root tool that installs, verifies
and activates kbf releases; it ships no kbf runtime release (`kbf-installer record-release`
records the first one, placed in `/var/lib/kbf-installer/releases/<sha>/`). The contract is
`docs/design/kbf-installer.md`, sections 3 (files and users), 7 (units and gating) and 9
(P11). A deb and an rpm are built by [nfpm](https://nfpm.goreleaser.com), pinned in CI by
SHA-256, from a configuration `cargo xtask pkg-config` writes: this directory holds the
inputs, and `crates/xtask/src/pkg_linux.rs` is the one place that says what goes in.

| path | what |
|---|---|
| `sysusers.d/kbf-installer.conf` | the users `kbf`, `kbf-server`, `kbf-fetch`, `kbf-signer` (no login); installed to `/usr/lib/sysusers.d` |
| `tmpfiles.d/kbf-installer.conf` | `/run/kbf-installer` (0755), `/var/cache/kbf-fetch` (0711); the units author's file, packaged unchanged; installed to `/usr/lib/tmpfiles.d` |
| `needrestart/kbf.conf` | tells needrestart never to restart a unit named `kbf-*`, `kbf.*` or `kbf@*`; installed to `/etc/needrestart/conf.d/kbf.conf` (0644, a config file the host may edit) |
| `units/` | the unit files (`kbf-*.service`, `kbf-*.socket`, `kbf.slice`), installed to `/usr/lib/systemd/system`; `testdata/units` is the fixture for the unit tests only |
| `testdata/` | fixtures for the install test only: two gated units and three stand-in trust files |
| `test/ubuntu`, `test/fedora` | the fresh systems the install test uses, systemd as PID 1, pinned by digest |

## What the package holds

- `/usr/bin/kbf-installer` (0755);
- every file of `packaging/linux/units` (the `kbf-*.service` and `kbf-*.socket` files and `kbf.slice`; the
  unit files are packaging's input from the units author, packaged unchanged) in
  `/usr/lib/systemd/system` (0644);
- `/usr/share/kbf-installer/trust/`: `gh` (0755), `trusted_root.jsonl`, `installer.conf` (0644);
- the sysusers.d file (this package's) and the tmpfiles.d file (the unit author's, packaged
  unchanged; `/var/lib/kbf-installer` is made by `kbf-installer setup`, not at install);
- `/etc/needrestart/conf.d/kbf.conf` (0644): on Ubuntu, needrestart restarts every service that maps a library an
  unattended upgrade replaced. A restart of a storage member in the middle of a build leaves the blobs only that
  member holds unreadable for the restart, and the client's build fails. This drop-in sets needrestart's
  `override_rc` for kbf units to 0, so only kbf's own update path restarts them. A host without needrestart ignores it.
- all of it `root:root`, with one time (`SOURCE_DATE_EPOCH`) on every file and the package.

### Which frontend unit

A frontend host enables exactly one of two units; they bind the same ports.

- `kbf-frontend.service`: a host **without** the frontend proxy. One process, `kbf
  --mode=frontend --config /etc/kbf/kbf.toml`, run from `/var/lib/kbf-installer/releases/current/kbf`
  (no slots, no `--instance`, no `--handover-socket`); listeners, state store and certificates
  are `kbf.toml`'s. An update is a restart after `kbf-installer confirm` flips `current`.
- `kbf-proxy` (the unprivileged proxy that starts the frontend as its child from
  `releases/<commit>/kbf`): a host **with** the proxy does not enable a frontend unit.

It refuses anything else (a unit named none of `kbf-*.service`, `kbf-*.socket` and `kbf.slice`, a fourth trust file, no unit).

The socket-handover template (`kbf-frontend@.service`, instances `a` and `b`) is no longer
packaged: the proxy swaps frontend children instead, and a frontend with a Postgres store
refuses a control socket.

Dependencies: deb `crun, uidmap, systemd, ca-certificates`; rpm the same with Fedora's name
for `uidmap`, `shadow-utils`. Never Podman, never Envoy (a test pins it): Envoy
runs on frontend hosts only, is a host prerequisite, and nothing in this package fetches or
pins it.

## Installing starts and enables nothing

The package has no maintainer script (`xtask pkg-install-test` fails on one, in both
formats), so nothing runs at install, remove or purge: no unit is enabled or started, no
user or directory is created by a script.

- Units ship disabled. Each gates on its configuration (`ConditionPathExists=` of
  `/etc/kbf/kbf.toml` for a node unit, `/etc/kbf/installer.toml` for
  `kbf-installer.service`); the test starts each `--gate` unit without its file (skipped,
  `ConditionResult=no`, no `ExecStart` run) and with it (condition holds).
- `/etc/kbf/kbf.toml`, the file those gates test, is written by `kbf-installer setup`, not by
  the package: for a frontend or all-in-one host from flags that name the listeners, the
  certificates and keys (which the operator provides, owned by the unit's user) and, for a
  frontend, the storage node, for an all-in-one host the cold store and the daemon half's
  identity (`docs/deploy/kbf-installer-setup.md`, "The `kbf.toml` flags"). The unit's
  `ExecStartPre=... --check` reads that file, so a file `setup` wrote is one `kbf --check` accepts.
- The users and directories come from `systemd-sysusers` and `systemd-tmpfiles`, which
  run at boot, and from `kbf-installer setup`. After `dpkg -i` and before either, they do
  not exist; the test runs both commands as boot does and checks the result.
- `remove` deletes the packaged files and keeps `/etc/kbf`, `/var/lib/kbf-installer` and
  the users. `purge` (deb) does the same and drops the package's record: the package owns
  none of what `setup` and the operator write, so it deletes none of it.

## In CI: the real package, attested

`ci.yml`'s `packages` job builds the deb and the rpm from the test-only stub and the
fixtures under `testdata/` on every pull request (x86_64). `artifacts.yml` builds the real
one: each Linux job (`linux-x86_64` on `ubuntu-24.04`, `linux-arm64` on
`ubuntu-24.04-arm`, each with its own pinned nfpm and `gh`) runs the same verbs on the
`kbf-installer` it has just built, with a trust directory `cargo xtask pkg-trust` writes (the
pinned `gh`, the Sigstore trusted root `gh` fetched, and an `installer.conf` whose
`SOURCE_REFS` is the branch the package is attested on), installs it in a fresh Ubuntu
container (`cargo xtask pkg-install-test`, with `--probe`s that the packaged `gh` runs and
the binary names the commit), and `cargo xtask pkg-asset` makes
`kbf-installer-<commit>-<platform>.deb` an asset, listed in `SHA256SUMS` and attested with
a bundle ([docs/artifacts.md](../../docs/artifacts.md#the-kbf-installer-package-deb)).
Two call sites, one way to build: both go through `pkg-config` and `pkg-build`.

## What the units run

- `kbf-installer.service` runs `/usr/bin/kbf-installer run`. It names no unit: `run` restarts
  the unit `setup` enabled for the `mode` of `/etc/kbf/kbf.toml` (on a frontend or all-in-one
  host it also reads the state database from `/etc/kbf/installer.toml`), so one packaged unit
  serves every mode. `--unit` and `--launchd-label` still name the service by hand. The unit has
  no `ExecStartPre=`: the contract's `check` is not built, and a stub would fail every start.
- Each mode unit runs `/var/lib/kbf-installer/releases/current/kbf`, the link the installer
  creates and flips (`layout.rs`), after `kbf-installer boot-check`.
- `crates/kbf-integration-test/tests/package_installer_verbs.rs` parses every `kbf-installer` command line of
  these units, and of the macOS property list, with the binary's own parser and refuses a verb that
  is not built; it also holds the exec path to the layout's.

## Commands

```
cargo xtask pkg-config --binary <kbf-installer> --units packaging/linux/units \
  --trust <dir of gh, installer.conf, trusted_root.jsonl> \
  --sysusers packaging/linux/sysusers.d/kbf-installer.conf \
  --tmpfiles packaging/linux/tmpfiles.d/kbf-installer.conf \
  --needrestart packaging/linux/needrestart/kbf.conf \
  --platform linux-x86_64 --version <v> --out <dir>      # nfpm.yaml, listing.txt
cargo xtask pkg-build --nfpm <nfpm> --config <dir>/nfpm.yaml --out <pkgs>
cargo xtask pkg-install-test --package <file> --format deb|rpm --listing <dir>/listing.txt \
  --image-dir packaging/linux/test/ubuntu --gate kbf-daemon.service=/etc/kbf/kbf.toml ...
```

`pkg-build` builds each package twice with the sources' modification times changed
between the builds, and fails unless both are the same bytes (nfpm takes every time from
`SOURCE_DATE_EPOCH`, which the verb sets; the rpm's build host is fixed in the
configuration).

## CI

The `packages` job of `.github/workflows/beta.yml` fetches the pinned nfpm, builds the
test-only `kbf-installer-stub` and the packages, then runs `pkg-install-test` on the deb
in Ubuntu and the rpm in Fedora. The real binary replaces the stub by one path change:
the `--binary` of `pkg-config`.
