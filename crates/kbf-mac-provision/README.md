# kbf-mac-provision

`kbf-mac-provision` applies and checks the profile of a Mac node: the host settings and
the worker profile that
[docs/design/mac-node-provisioning.md](../../docs/design/mac-node-provisioning.md)
(sections 2, 3 and 5) describes. It runs only programs macOS ships, over a profile of
`KEY=VALUE` lines that it parses and never evaluates. It replaced a POSIX `sh` script of
the same name and kept its profile format, item names, report lines and exit statuses.
What the script did not do:

- `appstore_auto_update` also reads and sets the older
  `/Library/Preferences/com.apple.commerce` `AutoUpdate`.
- Before its first change to a `power_*`, `updates_*` or `appstore_auto_update` setting,
  `apply` records every one of those settings as it found them in
  `/var/db/kbf-mac-provision/settings.before` (root's, mode 600) and keeps that file on
  later runs. `kbf-mac-provision revert` (root) puts back each recorded value that
  differs, deleting a preference that was not set, reads each one back, and moves the
  record aside. It refuses a record it cannot read or that lacks a setting. If a
  setting is not put back (its command fails, or it reads back otherwise), it prints a
  `FAIL` line for it, keeps the record where it is and exits 1; run it again once the
  setting can be changed.
- Three items for the lease-user pool of `docs/design/macos-end-state.md` (step 2),
  over five optional keys:
  - `kbf_volume` (`/Volumes/<name>`, default `/Volumes/kbf`): a mounted APFS volume
    (`diskutil info` gives the path itself as the mount point), not indexed (`mdutil -s`, and `.metadata_never_index` at its
    root) and excluded from Time Machine (`tmutil isexcluded`). `apply` writes the
    marker, runs `mdutil -i off` and `tmutil addexclusion`; it does not make the volume
    (`diskutil apfs addVolume` is the operator's).
  - `pool_users` (`pool_users`, 1 to 64, default 4; `pool_first_uid`, default the uid
    after `lease_uids` and never below 600): users `kbf-pool-<i>` for `i < pool_users`
    with uid `pool_first_uid + i`, primary group `kbfpool` (gid `pool_first_uid`),
    shell `/usr/bin/false`, home `kbf-pool-<i>` in the `home` directory of
    `kbf_volume` (theirs, mode 700, in a directory of root's, mode 755), hidden,
    `Password` `*` and no
    `AuthenticationAuthority`; none an administrator or in `wheel` (`dseditgroup`), and
    no other account holding a pool uid or the gid. The pool's uids must be at most
    65535 and outside `lease_uids`. `apply` makes them with `dscl . -create`, removes
    `admin` and `wheel` memberships with `dseditgroup -o edit -d`, and makes the homes
    only while the volume is mounted and its `home` directory is a directory of root's,
    mode 755 (never through a link there); it leaves a user whose uid another holds,
    and changes nothing while another group holds the gid.
  - `daemon_user` (`role`, the default, or `root`; `kbf_cache`, an absolute path):
    with `role` nothing changes and nothing is checked. With `root` the property list
    has no `UserName` or `GroupName`, the error log is root's, the node key (`kbf_key`)
    is a regular file with one link, root's, mode 600, and the data directory
    (`kbf_data_dir`) and `kbf_cache` (if set) are directories of root's, mode 755, on `kbf_volume`.
    `daemon_user=root` makes kbf-daemon run as root; a profile sets it only once the
    daemon runs every action as a pool user and refuses uid 0 (step 3 of that
    document).

```text
kbf-mac-provision check   --profile FILE [--keys K,K]   # one PASS/FAIL line per item; exit 1 on any FAIL
kbf-mac-provision apply   --profile FILE [--keys K,K]   # root: fix each FAIL, one CHANGE line per change
kbf-mac-provision print   --profile FILE                # the resolved profile, its SHA-256, the node label
kbf-mac-provision restart --profile FILE                # root: restart kbf-daemon, truncating its error log
kbf-mac-provision revert                                # root: put back the recorded power and update settings
```

- `apply` fixes each failing item, then checks again and prints only what still fails.
  A second `apply` prints nothing. Exit status: 0 when every item passes, 1 when one
  fails, 2 for bad usage, a refused profile, or a verb that refused or could not
  finish.
- **Check only** items are never changed by `apply`; it reports them and exits 1. An
  erase or a restore is the remedy for macOS and for simulator runtimes; FileVault and
  the administrator are the operator's.
- The node label is `profile=<profile>@<first 12 hex digits of the profile's SHA-256>`,
  passed to `kbf daemon` as a `--label`.
- `--keys` limits `check` and `apply` to the named items. `--root DIR` runs everything
  under DIR; only the tests use it.
- The job runs `/usr/local/kbf/current/kbf daemon` with the flags of the Mac kit's plist, one
  word per argument: `--driver native`, the profile's `kbf_server`, `kbf_cas`, `kbf_ca_cert`,
  `kbf_cert` and `kbf_key`,
  `--node-id` the host name, `--data-dir` the profile's `kbf_data_dir`, then the node
  label and the profile's labels. There is no `node` mode; the CLI's `--mode=daemon`
  flag replaces the `daemon` word when it lands.
- `kbf_data_dir` is the daemon's data directory (it keeps `leases/`, `cache/` and `trees/`
  under it; the profile has no size key, so the daemon sizes the directory from the
  filesystem). The job passes it as `--data-dir`. The old name `kbf_scratch` is still
  accepted, as `kbf_data_dir`, with one warning line on stderr; a profile that sets both
  keys is refused.
- An example profile, with placeholder values, is [example.profile](example.profile).

## Items

Every item is named by its profile key. `xcode`, `probe` and `expect_probe` may repeat;
every other key appears exactly once, and an unknown key is refused. A key is lower-case
letters and `_` only.

| Item (keys) | Values | Read with | Applied with |
|---|---|---|---|
| `macos_version`, `macos_build` | version, build | `sw_vers` | check only |
| `hostname` | `<farm>-mac-<nn>`, lower case, at most 63 characters | `scutil --get` `HostName`, `LocalHostName`, `ComputerName` | `scutil --set` each that differs |
| `admin_user` | account | `dscl`: it exists, and it and `root` are the only administrators: the names in `admin`'s `GroupMembership`, and every account in `dscl . -list /Users` that `dseditgroup -o checkmember` counts as a member of `admin` (which finds one by primary group 80 or a nested group) | check only |
| `role_user` (`role_id`) | `_name`, id 200-499 | `dscl`: user and group with that id, shell `/usr/bin/false`, home `/var/empty`, hidden, no password (`Password` is `*` and there is no `AuthenticationAuthority`); no other user and no other group holds the id | `dscl . -create`, and `-delete` of `AuthenticationAuthority` (refused if the id is taken) |
| `power_sleep`, `power_standby`, `power_powernap`, `power_autorestart`, `power_womp` | numbers | `pmset -g` | `pmset -a` |
| `updates_auto_download`, `updates_auto_install_macos`, `updates_auto_install_security`, `updates_config_data`, `appstore_auto_update` | 0 or 1 | `defaults read /Library/Preferences/com.apple.SoftwareUpdate` `AutomaticDownload`, `AutomaticallyInstallMacOSUpdates`, `CriticalUpdateInstall`, `ConfigDataInstall`, `AutomaticallyInstallAppUpdates` | `defaults write ... -bool`. **Verify on the pinned version:** the SoftwareUpdate payload is removed in macOS 27, and whether these local preferences are still honoured is not known; every line says so |
| `xcode_disable_manifest_sandbox` (optional) | 0 or 1 | `defaults read /Library/Preferences/com.apple.dt.Xcode IDEPackageSupportDisableManifestSandbox`; left out, the item passes as "not managed" | `defaults write ... -bool`. It is not in the revert record. `xcodebuild` resolving a Swift package runs each manifest under SwiftPM's own `sandbox-exec`, which macOS refuses inside the native driver's sandbox (`sandbox_apply: Operation not permitted`); `xcodebuild` takes the switch only as a user default, and this system-domain value is read by every user after their own. `swift build` has `--disable-sandbox` instead. Unverified until a macOS runner shows `xcodebuild` honouring the system domain |
| `remote_login` | 0 or 1 | `systemsetup -getremotelogin` | `systemsetup -f -setremotelogin` |
| `ssh_password_auth` | 0 or 1 | `/etc/ssh/sshd_config.d/000-kbf.conf` holds `PasswordAuthentication`, `KbdInteractiveAuthentication` and `PermitRootLogin no`, root-owned, mode 644; `sshd_config` includes the directory. What sshd uses: no file in `sshd_config.d` sorts before the drop-in, `sshd_config` sets none of `PasswordAuthentication`, `KbdInteractiveAuthentication`, `ChallengeResponseAuthentication`, `PermitRootLogin` before its `Include` line, and, as root, `sshd -T` reports the drop-in's three values (as another user the line says `sshd -T` was not run) | writes the drop-in (`000-` sorts first, and sshd keeps the first value it reads). It never removes an earlier drop-in or edits `sshd_config`: those stay FAIL for the operator |
| `firewall` | on, off | `socketfilterfw --getglobalstate` | `socketfilterfw --setglobalstate` |
| `filevault` | on, off | `fdesetup status` | check only: never toggled |
| `autologin` (`lease_uids`) | off; range, default in the design 600-699 | `autoLoginUser` in `/Library/Preferences/com.apple.loginwindow`, `/etc/kcpassword` | passes when auto-login is off and no `kcpassword` is left, or when the user is a `kbf-lease-*` account whose uid is in `lease_uids` and whose lease `kbf-mac-session`'s intent record (`/var/db/kbf-mac-session/session-intent`, one line `<term>.<seq>`, not a link) names: set by `session-login` for a `whole_machine` lease; `apply` leaves it. Otherwise, a lease user left behind included, `apply` clears both |
| `time_server` | host | `systemsetup -getusingnetworktime`, `-getnetworktimeserver` | `systemsetup -setusingnetworktime on`, `-setnetworktimeserver` |
| `time_max_offset_ms` | ms | `sntp -t 5` (read-only) | `sntp -sS` |
| `xcode` (`xcode_xip_dir`) | `BUILD:/Applications/Xcode*.app/Contents/Developer:SHA256` | `ProductBuildVersion` of each app's `version.plist`; any other `Xcode*.app` directory in `/Applications` is drift (links are skipped) | installs a missing or wrong Xcode from the `.xip` in `xcode_xip_dir` whose SHA-256 the line names (`xip --expand`, its build checked, `xcodebuild -license accept`, `-runFirstLaunch`); removes unpinned Xcodes |
| `default_developer_dir` | a pinned Xcode's path | `xcode-select -p` | `xcode-select -s` |
| `probe` (`expect_probe`) | `KEY:/PATH:SHA256`; `KEY:XCODE_BUILD=VALUE` | each probe script's SHA-256 | check only. `expect_probe` lines are validated (a known probe, a pinned Xcode, once each) and printed |
| `simulator_runtimes` | none | `/Library/Developer/CoreSimulator`: `Images/*.dmg`, `Profiles/Runtimes/*.simruntime`, `Volumes/*` | check only: runtimes live in VM images |
| `launchd_label` (`kbf_labels`, `role_user`, and the profile's own SHA-256 in the node label) | label | `/Library/LaunchDaemons/<label>.plist` equals the one the profile renders (section 5.3: `UserName`/`GroupName` the role account, `ProcessType` `Interactive`, `ExitTimeOut` 30, `kbf daemon` and its flags), root-owned, mode 644; `/Library/Logs/kbf` root's, mode 755; the error log in it a regular file with one link, the role account's, mode 644; the job loaded | makes the directory root's, replaces anything else at the error log's path (unlinked, never followed) with a new file of the role account, writes the plist, then restarts the job: `launchctl bootout`, the error log truncated while it is stopped, `launchctl bootstrap`. `restart` refuses, before it stops anything, when the error log is not such a file, so a link the role account plants is never emptied as root |

## Tests

| What | Where it runs | What it proves |
|---|---|---|
| [tests/](tests) (`cargo test -p kbf-mac-provision`) | every CI test job | Every item against a fake Mac ([tests/support](tests/support) answers each macOS program from state in memory and logs each call): a fresh Mac fails check on each item and passes after one apply; a second apply prints nothing and changes nothing; on a converged Mac each drift fails exactly its item, and apply fixes it or (check only) changes nothing; refused profiles; root; `--keys`; restart; a symbolic or hard link planted at the error log (never emptied); `sshd -T` skipped when not root; the Xcode sources; the lease auto-login rule; the pool items; the record and `revert`. The `coverage` job holds the crate's line and branch coverage at its baseline |
| [tests/as_root.rs](tests/as_root.rs) (`cargo xtask sudo-test -p kbf-mac-provision --test as_root`) | CI job `native-macos` (a hosted macOS VM, as root) | The binary on real macOS for what a VM can prove; its header lists what is applied, asserted, and only read |

What only a real node of the pinned version can prove (design section 10): `pmset`
on hardware, FileVault, Remote Login, the time server, whether the update preferences
are honoured, an Xcode install from a real `.xip`, and whether a freshly erased Mac
lists `_mbsetupuser` as an administrator. The hosted VM does, so `admin_user` fails
there; the program allows no exception today, and if real nodes list it too, removing
it or allowing it is a decision to make explicitly.
