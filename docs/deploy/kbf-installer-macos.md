# Installing kbf on a macOS host

A Mac runs kbf as a node: `kbf --mode=daemon`, run by launchd. The root tool that sets the host up
and updates it is `kbf-installer`, the same binary as on Linux, and it reaches a Mac as a flat
`.pkg`. This page covers a new Mac, a Mac that the older Mac kit installed, and what the operator
still does by hand. A Mac is a node only: `setup` refuses every mode but `--mode=daemon`.

## Layout per platform

The paths are in one place, `crates/kbf-installer/src/layout.rs`. The two platforms differ in these:

| | Linux | macOS |
|---|---|---|
| installer tree (releases, links) | `/var/lib/kbf-installer` | `/usr/local/kbf-installer` |
| installer state (`state.json`, `status.json`, `orders/`) | the installer tree | `/var/db/kbf-installer` |
| lock | `/run/kbf-installer/lock` | `/var/run/kbf-installer/lock` |
| trust directory | `/usr/share/kbf-installer/trust` | `/usr/local/kbf-installer/trust` |
| configuration directory | `/etc/kbf` | `/usr/local/etc/kbf` |
| `kbf.toml`, `installer.toml` | in the configuration directory | in the configuration directory |
| enrollment token, operator token | `/etc/kbf/enroll.token`, `/etc/kbf/api-token` | `/usr/local/etc/kbf/enroll.token`, `/usr/local/etc/kbf/api-token` |
| kbf's own state (enrollment keys) | `/var/lib/kbf` | `/var/db/kbf` |
| fetch account | `kbf-fetch` | `_kbf-fetch` |

The installer tree is root's. On a Mac it is never a user-owned Homebrew prefix. Releases are under
`<installer tree>/releases/<commit>/`, with the links `releases/current` and `releases/previous`
beside them. A macOS release holds `kbf` and nothing else that runs; the daemon job runs
`/usr/local/kbf-installer/releases/current/kbf --mode=daemon --config /usr/local/etc/kbf/kbf.toml`.

## The package

The package is `kbf-installer-<commit>-darwin-arm64.pkg`, a flat component package with the
identifier `com.komira.kbf-installer`. It carries no scripts and installs, all as root:wheel:

| path | mode | what |
|---|---|---|
| `/usr/local/kbf-installer/bin/kbf-installer` | 0755 | the binary |
| `/usr/local/kbf-installer/trust/<name>` | 0644 | the pinned `gh`, the Sigstore trusted root, `installer.conf` |
| `/Library/LaunchDaemons/com.komira.kbf-installer.plist` | 0644 | the installer's own job |
| `/var/db/kbf-installer` | 0700 | the state directory, empty |
| `/Library/Logs/kbf-installer` | 0755 | where the job writes |

The package is not signed with a Developer ID, so Gatekeeper does not open it by a double click.
Verify the download and install it from a terminal; the commands are in
[the artifacts guide](../artifacts.md#the-kbf-installer-package-pkg-macos):

```text
shasum -a 256 --check --strict --ignore-missing SHA256SUMS
sudo installer -pkg kbf-installer-<commit>-darwin-arm64.pkg -target /
```

Installing places files and starts nothing. The package makes no configuration, no release and no
user.

### The installer's job

`/Library/LaunchDaemons/com.komira.kbf-installer.plist` has the label `com.komira.kbf-installer`
and runs `/usr/local/kbf-installer/bin/kbf-installer run --launchd-label com.komira.kbf-daemon`.
`run` acts on the orders the node wrote and kickstarts `com.komira.kbf-daemon` after a swap. The
job holds itself back: `RunAtLoad` is false and `KeepAlive` is `PathState` on
`/usr/local/etc/kbf/installer.toml`, so it does not run until that file exists. It throttles
restarts to ten seconds and writes to `/Library/Logs/kbf-installer/kbf-installer.log` and
`kbf-installer.err.log`.

## A new Mac

1. **Install the package** as above.
2. **Provision the host.** `kbf-mac-provision apply --profile FILE` (as root) fixes the host
   settings and renders the worker profile: it writes `/usr/local/etc/kbf/kbf.toml` and the daemon
   job's plist, and starts the job. It is the one writer of the daemon's `kbf.toml` on
   a Mac. The `kbf.toml` it renders has no `update-dir` or `orders-dir`; a file that still has them
   from an earlier run is accepted and left as it is. Use the label `com.komira.kbf-daemon` for the daemon job
   (`launchd_label` in the profile), because that is the label `setup` and the installer's job look
   for.
3. **Run `setup`** as root:

   ```text
   sudo /usr/local/kbf-installer/bin/kbf-installer setup --mode=daemon \
       --frontend-url=https://frontend.example.invalid:8443 --token-file=/srv/kbf/enroll.token   # dry run
   sudo /usr/local/kbf-installer/bin/kbf-installer --apply setup ...
   ```

On a Mac `setup`:

- makes the `_kbf-fetch` account with `dscl`, at the lowest free id from 300 to 399, hidden, shell
  `/usr/bin/false`, home `/var/empty`; an account of that name with another shape is refused (exit
  5) and left alone;
- makes `/usr/local/etc/kbf` and `/var/db/kbf-installer` (root, 0755) and `orders/` inside it (0700,
  owned by root: the plan treats the Mac daemon as root's), copies the enrollment token to
  `/usr/local/etc/kbf/enroll.token` (0600), and writes `/usr/local/etc/kbf/installer.toml` (0644),
  which is the file that releases the installer's job;
- looks users up with `dscl`, not `getent`, and has no subid or lingering step;
- **never writes `update-dir` or `orders-dir` into `kbf.toml`**, and does not require them.
  The keys `setup` itself owns (`mode`, `[frontend] url`, `[enroll]
  token_file`) follow the same rule as on Linux: a file with other settings is kept when they match
  and refused when they differ;
- runs `launchctl bootstrap system /Library/LaunchDaemons/com.komira.kbf-installer.plist` unless
  the job is loaded or `--no-start` is given, and bootstraps `com.komira.kbf-daemon` the same way
  only when a release is installed and `--no-start` is not given.

Rerunning `setup` with the same flags prints `nothing to do`. [The setup guide](kbf-installer-setup.md)
has the flags, the dry run and the exit codes.

### The first release

`setup` installs no release (it takes `--commit` and the flags of the release source for one more
release, ignores them and says so). Place the first release by hand and record it: put the `kbf`
binary in `/usr/local/kbf-installer/releases/<commit>/kbf` (root-owned,
not writable by group or others, executable by others, a Mach-O file), then run

```text
sudo kbf-installer --apply record-release --commit <40 hex>
```

`record-release` checks the files, writes the `VERIFIED` marker and makes the release `current`. The
operator vouches for the commit: nothing is fetched or attested. After that `setup` bootstraps the
daemon job.

## A Mac the Mac kit installed

The older Mac kit left `/usr/local/kbf` with `releases/`, the links `current` and `previous`, a
settings file `etc/av2_mac.conf` and the LaunchDaemon `com.komira.kbf-daemon`. The installer does
not adopt that layout: there is no `kbf-installer migrate` verb. Adoption = install the kbf and
kbf-proxy packages (package-update design, `design.md` section 12), then run `setup`.

## What is still by hand

- Installing the package, and verifying its checksum and attestation first.
- Running `kbf-mac-provision` with a profile, and everything in it that is check-only: the
  administrator account, FileVault, macOS and Xcode installs, the APFS volume for the lease users.
- Creating the enrollment token on the frontend and putting the token file on the Mac.
- Placing and recording the first release (above).
- Updating: a Mac takes no update order yet. `kbf-installer run` activates a release only after it
  has counted the live leases, and for a launchd job it has no lease source, so it refuses the
  order (`cannot tell whether leases are live`). Update by hand, see [updating a host](update-a-host.md).
- A signature: nothing in the package is signed with a Developer ID.
