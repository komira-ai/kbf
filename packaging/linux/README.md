# kbf-installer: the Linux packages

`kbf-installer` is the one package of the farm. It is the root tool that installs, verifies
and activates kbf releases; it ships no kbf runtime release (`kbf-installer setup`
installs the first one into `/var/lib/kbf-installer/releases/<sha>/`). The contract is
`docs/design/kbf-installer.md`, sections 3 (files and users), 7 (units and gating) and 9
(P11). A deb and an rpm are built by [nfpm](https://nfpm.goreleaser.com), pinned in CI by
SHA-256, from a configuration `cargo xtask pkg-config` writes: this directory holds the
inputs, and `crates/xtask/src/pkg.rs` is the one place that says what goes in.

| path | what |
|---|---|
| `sysusers.d/kbf-installer.conf` | the users `kbf`, `kbf-fetch`, `kbf-verify` (no login); installed to `/usr/lib/sysusers.d` |
| `tmpfiles.d/kbf-installer.conf` | `/var/lib/kbf-installer` (root 0755), `/run/kbf-installer`, `/var/cache/kbf-fetch`; installed to `/usr/lib/tmpfiles.d` |
| `units/` | the unit files, `kbf-*.service`, installed to `/usr/lib/systemd/system` (their own change; until it lands CI packages `testdata/units`) |
| `testdata/` | fixtures for the install test only: two gated units and three stand-in trust files |
| `test/ubuntu`, `test/fedora` | the fresh systems the install test uses, systemd as PID 1, pinned by digest |

## What the package holds

- `/usr/bin/kbf-installer` (0755);
- every `kbf-*.service` of the units directory in `/usr/lib/systemd/system` (0644);
- `/usr/share/kbf-installer/trust/`: `gh` (0755), `trusted_root.jsonl`, `installer.conf` (0644);
- the sysusers.d and tmpfiles.d files above;
- all of it `root:root`, with one time (`SOURCE_DATE_EPOCH`) on every file and the package.

It refuses anything else (a unit not named `kbf-*.service`, a fourth trust file, no unit).

Dependencies: deb `crun, uidmap, systemd, ca-certificates`; rpm the same with Fedora's name
for `uidmap`, `shadow-utils`. Never Podman, never Envoy: the installer fetches the Envoy
each release pins.

## Installing starts and enables nothing

The package has no maintainer script (`xtask pkg-install-test` fails on one, in both
formats), so nothing runs at install, remove or purge: no unit is enabled or started, no
user or directory is created by a script.

- Units ship disabled. Each gates on its configuration (`ConditionPathExists=` of
  `/etc/kbf/kbf.toml` for a node unit, `/etc/kbf/installer.toml` for
  `kbf-installer.service`); the test starts each `--gate` unit without its file (skipped,
  `ConditionResult=no`, no `ExecStart` run) and with it (condition holds).
- The users and directories come from `systemd-sysusers` and `systemd-tmpfiles`, which
  run at boot, and from `kbf-installer setup`. After `dpkg -i` and before either, they do
  not exist; the test runs both commands as boot does and checks the result.
- `remove` deletes the packaged files and keeps `/etc/kbf`, `/var/lib/kbf-installer` and
  the users. `purge` (deb) does the same and drops the package's record: the package owns
  none of what `setup` and the operator write, so it deletes none of it.

## Commands

```
cargo xtask pkg-config --binary <kbf-installer> --units packaging/linux/units \
  --trust <dir of gh, installer.conf, trusted_root.jsonl> \
  --sysusers packaging/linux/sysusers.d/kbf-installer.conf \
  --tmpfiles packaging/linux/tmpfiles.d/kbf-installer.conf \
  --platform linux-x86_64 --version <v> --out <dir>      # nfpm.yaml, listing.txt
cargo xtask pkg-build --nfpm <nfpm> --config <dir>/nfpm.yaml --out <pkgs>
cargo xtask pkg-install-test --package <file> --format deb|rpm --listing <dir>/listing.txt \
  --image-dir packaging/linux/test/ubuntu --gate kbf-node.service=/etc/kbf/kbf.toml ...
```

`pkg-build` builds each package twice with the sources' modification times changed
between the builds, and fails unless both are the same bytes (nfpm takes every time from
`SOURCE_DATE_EPOCH`, which the verb sets; the rpm's build host is fixed in the
configuration).

## CI

The `packages` job of `.github/workflows/ci.yml` fetches the pinned nfpm, builds the
test-only `kbf-installer-stub` and the packages, then runs `pkg-install-test` on the deb
in Ubuntu and the rpm in Fedora. The real binary replaces the stub by one path change:
the `--binary` of `pkg-config`.
