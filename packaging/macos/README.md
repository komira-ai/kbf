# kbf-installer for macOS

The root installer reaches a Mac as a `.pkg`, directly or through a Homebrew
cask (contract `docs/design/kbf-installer.md`, sections 3, 7 and 9 P11). There is no
formula: a formula installs into the user's prefix, and `kbf-installer check` refuses a
user-owned prefix.

Neither way starts anything. `kbf` itself is never in the package: after
`sudo /usr/local/kbf-installer/bin/kbf-installer setup` writes `installer.toml`, the
installer fetches and verifies the first release into `/usr/local/kbf-installer/releases`,
and the service then takes the frontend's update orders. Macs are nodes only.

## The package

`cargo xtask pkg build --binary <kbf-installer> --launchd-plist
packaging/macos/com.komira.kbf-installer.plist [--trust <file>]... --version <v> --out <dir>`
writes `<dir>/kbf-installer-<v>-macos-arm64.pkg`, a flat component package (identifier
`com.komira.kbf-installer`) of these entries, all root:wheel, and no scripts:

| path | mode | |
|---|---|---|
| `/usr/local/kbf-installer/bin/kbf-installer` | 0755 | the binary, signed ad hoc by CI (`cargo xtask sign-darwin`) |
| `/usr/local/kbf-installer/trust/<name>` | 0644 | one per `--trust` file: pinned `gh`, `trusted_root.jsonl`, `installer.conf` |
| `/Library/LaunchDaemons/com.komira.kbf-installer.plist` | 0644 | this directory's property list |
| `/var/db/kbf-installer` | 0700 | the installer's state directory, empty |
| `/Library/Logs/kbf-installer` | 0755 | where the job writes |

Every directory on the way to the tool is 0755 root:wheel. The package creates no
configuration, no release and no user (`kbf`, `_kbf-fetch` and the lease
users are made by `setup`, as root; a payload cannot make users).

### The release asset

`artifacts.yml`'s `darwin-arm64` job builds the real package from the signed `kbf-installer` it has
just built: `cargo xtask pkg-trust` writes the trust directory from the pinned darwin `gh`, the
Sigstore trusted root that `gh` fetched and the branch the package is attested on
(`installer.conf` is `SOURCE_REFS=<that ref>` and nothing else; per-host settings are written by
`kbf-installer setup`, not shipped), `cargo xtask pkg build --version-of` writes the package,
`cargo xtask pkg-macos-install-test` installs it as root on the runner and checks it, and
`cargo xtask pkg-macos-asset` makes it `kbf-installer-<commit>-darwin-arm64.pkg`, listed in
`SHA256SUMS.darwin-arm64` and attested with its own bundle. The gh pinned for it is in
`artifacts.yml` (`GH_VERSION`, `GH_SHA256_AARCH64_DARWIN`). See
[docs/artifacts.md](../../docs/artifacts.md#the-kbf-installer-package-pkg-macos). The trust files
are 0755 (`gh`) and 0644, root:wheel, like the Linux package's.

### Written in Rust, not by `pkgbuild`

nfpm builds the Linux packages and does not write a `.pkg`. `pkgbuild` and `productbuild`
would be shorter but are not deterministic (they stamp the archive and the bill of
materials with the time and the host's owners), and a builder here gives the same bytes
for the same inputs, which a test checks by building twice from different directories. The
package is a xar archive of `Bom`, `Payload` (a gzip-compressed odc cpio archive) and
`PackageInfo`, written by `crates/xtask/src/pkg/`. The Bom format is undocumented; the
writer follows `mkbom` of bomutils, and is checked on Linux by an independent reader in the
tests and on macOS by Apple's `pkgutil --expand`, `lsbom` and `installer`
(`crates/xtask/tests/pkg_macos.rs`, the package steps of the `native-macos` job). The package is unsigned:
`productsign` with the release identity (contract decision 7) wraps the file without
changing it. CI signs the binary inside it ad hoc.

### The service does not start before configuration

Two gates, both tested on a Mac:

1. Installer does not load a LaunchDaemon. Nothing in the package does.
2. The property list holds the job back by itself: `RunAtLoad` is false and `KeepAlive` is
   `PathState` on `/usr/local/etc/kbf/installer.toml` (launchd's form of
   `ConditionPathExists`), so a reboot, or `launchctl bootstrap` by hand, leaves it not
   running until that file exists. `cargo xtask pkg build` refuses a property list that is
   not gated so, that does not run `kbf-installer run --launchd-label com.komira.kbf-daemon` from the installed prefix, or that
   has a key which starts a job without the file (`Sockets`, `StartInterval`,
   `StartCalendarInterval`, `StartOnMount`, `WatchPaths`, `QueueDirectories`,
   `LaunchEvents`).

### The cask

`cargo xtask pkg cask --pkg <pkg> --version <v> --url <url> --out <file>` renders the
cask. Its `uninstall launchctl:` unloads the job and `pkgutil:` removes the files the
package placed and forgets its receipt. What `setup` made (the releases under
`/usr/local/kbf-installer`, `installer.toml`, the state) stays, so a reinstall keeps the
node; `brew uninstall --zap --cask kbf-installer` removes it too.

## What the real kbf-installer must match

The property list runs `kbf-installer run --launchd-label com.komira.kbf-daemon` (the label `setup` bootstraps
for the node's daemon, `MAC_DAEMON_LABEL`) and waits for `installer.toml` under
`/usr/local/etc/kbf`; the tool is at `bin/kbf-installer` in the prefix. If the real
layout (`layout.rs`, contract P2) puts the macOS configuration or the tool elsewhere,
change the property list and `GATE`/`PROGRAM` in `crates/xtask/src/pkg.rs`; the tests
follow the constants. Every configuration path `kbf-installer` uses (`setup`, `run`, `proxy`) comes from `layout.rs`: `/etc/kbf` on Linux, `/usr/local/etc/kbf` on a Mac, so on a Mac `setup` writes `installer.toml`, `kbf.toml` and the tokens where the property list's gate and `GATE` wait. `kbf-setup`'s `setup::tests` (`the_launchd_gate_is_the_installer_toml_setup_writes_on_a_mac`) compares the plist's gate with the file the Mac plan writes. Until the real binary builds on the runner, CI packages a stub
(`xtask-fake` run as `kbf-installer`, which prints its arguments); `INSTALLER` in
`crates/xtask/tests/pkg_macos.rs` is the one path to change, and the step that needs the
real tool (`check` refusing a user-owned prefix) runs when it is.
