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
    marker, runs `mdutil -i off` and `tmutil addexclusion -v` (the volume form);
    it does not make the volume
    (`diskutil apfs addVolume` is the operator's).
  - `pool_users` (`pool_users`, 1 to 100, default 100; `pool_first_uid`, default 61000
    and never below 600): users `kbf-pool-<i>` for `i < pool_users`
    with uid `pool_first_uid + i`, primary group `kbfpool` (gid `pool_first_uid`),
    shell `/usr/bin/false`, home `kbf-pool-<i>` in the `home` directory of
    `kbf_volume` (theirs, mode 700, in a directory of root's, mode 755), hidden,
    `Password` `*` and no
    `AuthenticationAuthority`; none an administrator or in `wheel` (primary group, listed by name in the group's
    `GroupMembership`, or by `GeneratedUID` in its `GroupMembers`; a membership through
    another group nested in it is not seen), and no other account holding a pool uid or the gid. The pool's
    uids must be at most 65535 and outside `lease_uids`. `check` reads `/Users` and
    `/Groups` once each (`dscl . -readall`) and judges every user from that: the cost
    does not grow with a spawn for each user. `apply` makes the group and the users in
    one `dscl .` process fed the commands on standard input, setting only what differs
    (a user that is right is left, so a second `apply` makes nothing, and a part of a
    pool is finished, not remade), removes `admin` and `wheel` memberships with
    `dseditgroup -o edit -d`, and makes the homes only while the volume is mounted and
    its `home` directory is a directory of root's, mode 755 (never through a link
    there). It changes nothing while another group holds the gid or an account that is
    not one of the pool's users holds a uid of the block, and when it cannot read the
    directory (a failed `dscl`, or no user in the answer) it changes nothing and
    says so. The default `pool_first_uid` is fixed, no longer the end of `lease_uids` plus 1: a Mac
    whose pool was made under the old default (a profile with no pool keys) has it remade at 61000 by the next
    `apply`, and the daemon's `--pool-first-uid` must change with it; a profile
    that sets `pool_users` and `pool_first_uid` is not affected.
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
kbf-mac-provision print   --profile FILE                # the resolved profile and its SHA-256
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
- A node carries no label: `kbf.toml` has no `label` key, and a profile that sets
  `kbf_labels` is refused (delete the line). An earlier version wrote the key; a Mac
  provisioned then fails `check` on `launchd_label` until `apply` rewrites its `kbf.toml`.
- `--keys` limits `check` and `apply` to the named items. `--root DIR` runs everything
  under DIR; only the tests use it.
- The job runs `/usr/local/kbf-installer/releases/current/kbf --mode=daemon --config /usr/local/etc/kbf/kbf.toml`
  and nothing after `--config`: `kbf.toml` is the one source of the daemon's settings. The
  `launchd_label` item renders it from the profile at that path (root's, mode 644, in a
  root's 755 directory, so the daemon account reads it and cannot replace it), and checks and
  fixes it beside the property list: `check` fails when the file is missing, differs from
  what the profile renders, or has another owner or mode; `apply` writes it, then the
  property list, then restarts the job. The file is not in `revert`'s record, which holds
  the power and update settings only, and `revert` leaves it (as it leaves the property
  list). The file holds only
  flag tables and `mode`, so `kbf` runs the daemon on those flags (a file with `[enroll]`,
  `[frontend]` or `[node]` tables is the enrolling start instead; this tool writes none).
  Each profile key goes to the table kbf-cli's `GROUPS` puts its flag in:

  | Flag `kbf --mode=daemon` took | Profile key | `kbf.toml` table | Key |
  |---|---|---|---|
  | (the mode) | | top level | `mode = "daemon"` |
  | `--driver native` | | `[daemon]` | `driver = ["native"]` |
  | `--server` | `kbf_server` | `[identity]` | `server` |
  | `--cas` | `kbf_cas` | `[identity]` | `cas` |
  | `--ca-cert` | `kbf_ca_cert` | `[identity]` | `ca-cert` |
  | `--cert` | `kbf_cert` | `[identity]` | `cert` |
  | `--key` | `kbf_key` | `[identity]` | `key` |
  | `--node-id` | `hostname` | `[identity]` | `node-id` |
  | `--data-dir` | `kbf_data_dir` | `[daemon]` | `data-dir` |

  Earlier versions also wrote `update-dir` and `orders-dir` under `[daemon]`. The node no
  longer reads them, `apply` no longer writes them, and a `kbf.toml` that still has them
  (and is otherwise the rendered file) passes `check` and is left as it is. The directories
  under `/var/db/kbf-installer` are no longer made or checked.

  The profile has no size key, so `--data-max-bytes` (`[daemon]`, `data-max-bytes`) is not
  rendered; the daemon sizes the data directory from the filesystem.
- `kbf_data_dir` is the daemon's data directory (it keeps `leases/`, `cache/` and `trees/`
  under it; the profile has no size key, so the daemon sizes the directory from the
  filesystem). It is the `data-dir` key of `kbf.toml`. The old name `kbf_scratch` is still
  accepted, as `kbf_data_dir`, with one warning line on stderr; a profile that sets both
  keys is refused.
- `vm_user_uid` (optional, 450 to 499, not `role_id`) is the standing VM user of a Mac that
  serves macOS guest VMs: the account `kbf-vm`, which the Mac rests logged in as, because a
  guest can only be installed and started from a user's GUI session. The profile may not
  set it with `filevault=on` (automatic login does not work with FileVault on). The account
  has the uid, primary group `staff`, shell `/bin/zsh`, home `kbf-vm` under `/Users` (its own, mode
  700), is hidden, has a password, and is in no group `admin`, `wheel`,
  `com.apple.access_ssh` or `com.apple.access_screensharing`; no other account holds the uid
  or answers to the name. `apply` makes a random password (32 bytes from `/dev/urandom`, in
  hex), sets it on the account, and then writes it to `/var/db/kbf-mac-session/vm-user-password`
  (root's, mode 400, in the state directory of kbf-mac-session, which `apply` makes root's,
  mode 700); a run that stops between the two leaves no valid file, so the next run makes
  another password. The root helper reads that file later: it may have to unlock the
  account's login keychain at session start (whether automatic login leaves it locked is
  not yet measured). The
  password is handed to `dscl` on standard input; it is never an argument, never printed
  and never in a report. A valid file is kept, so a second `apply` changes nothing and the
  password stays what the helper reads; a lost file is made again and set on the account (if the account had a password, `apply` says so in a NOTE: the account's login keychain, if it has one, is still locked with the old password).
  `apply` changes nothing while another account holds the uid or the name, or while a
  record named `kbf-vm` has another uid. `check` cannot compare the password with the
  account's (a hash cannot be read back).
  `autologin=vm` makes automatic login as this user the wanted state; see the `autologin` row.
- `updater_user` (optional, `_kbf-updater`) and `updater_id` (optional, 200 to 499, not
  `role_id` or `vm_user_uid`; it needs `updater_user`) are the service account of the
  macOS kbf-updater agent, whose property list runs as `_kbf-updater`. The name is the one
  the property list has, so no other is accepted. See the `updater_user` row.
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
| `updater_user` (`updater_id`) | `_kbf-updater`, id 200-499; unset: not managed | `dscl`: user and group of one id, shell `/usr/bin/false`, home `/var/empty`, hidden, no password, as for `role_user`; no other user and no other group holds the id; `/var/db/kbf-updater` (the agent's state: key, certificate, CA) is the account's, mode 700, and `/Library/Logs/kbf-updater` (its logs) the account's, mode 755 | `dscl . -create` of the user and group (with `updater_id` unset, the highest free id below 450, the VM user's range, that is not `role_id`), `-delete` of `AuthenticationAuthority`, and the two directories made, owned and chmoded (nothing is changed while another user or group holds the id) |
| `power_sleep`, `power_standby`, `power_powernap`, `power_autorestart`, `power_womp` | numbers | `pmset -g` | `pmset -a` |
| `updates_auto_download`, `updates_auto_install_macos`, `updates_auto_install_security`, `updates_config_data`, `appstore_auto_update` | 0 or 1 | `defaults read /Library/Preferences/com.apple.SoftwareUpdate` `AutomaticDownload`, `AutomaticallyInstallMacOSUpdates`, `CriticalUpdateInstall`, `ConfigDataInstall`, `AutomaticallyInstallAppUpdates` | `defaults write ... -bool`. **Verify on the pinned version:** the SoftwareUpdate payload is removed in macOS 27, and whether these local preferences are still honoured is not known; every line says so |
| `xcode_disable_manifest_sandbox` (optional) | 0 or 1 | `defaults read /Library/Preferences/com.apple.dt.Xcode IDEPackageSupportDisableManifestSandbox`; left out, the item passes as "not managed" | `defaults write ... -bool`. It is not in the revert record. `xcodebuild` resolving a Swift package runs each manifest under SwiftPM's own `sandbox-exec`, which macOS refuses inside the native driver's sandbox (`sandbox_apply: Operation not permitted`); `xcodebuild` takes the switch only as a user default, and this system-domain value is read by every user after their own. `swift build` has `--disable-sandbox` instead. Unverified until a macOS runner shows `xcodebuild` honouring the system domain |
| `remote_login` | 0 or 1 | `systemsetup -getremotelogin` | `systemsetup -f -setremotelogin` |
| `ssh_password_auth` | 0 or 1 | `/etc/ssh/sshd_config.d/000-kbf.conf` holds `PasswordAuthentication`, `KbdInteractiveAuthentication` and `PermitRootLogin no`, root-owned, mode 644; `sshd_config` includes the directory. What sshd uses: no file in `sshd_config.d` sorts before the drop-in, `sshd_config` sets none of `PasswordAuthentication`, `KbdInteractiveAuthentication`, `ChallengeResponseAuthentication`, `PermitRootLogin` before its `Include` line, and, as root, `sshd -T` reports the drop-in's three values (as another user the line says `sshd -T` was not run) | writes the drop-in (`000-` sorts first, and sshd keeps the first value it reads). It never removes an earlier drop-in or edits `sshd_config`: those stay FAIL for the operator |
| `firewall` | on, off | `socketfilterfw --getglobalstate` | `socketfilterfw --setglobalstate` |
| `filevault` | on, off | `fdesetup status` | check only: never toggled |
| `autologin` (`lease_uids`, `vm_user_uid`) | off, or vm; range, default in the design 600-699 | `autoLoginUser` in `/Library/Preferences/com.apple.loginwindow`, `/etc/kcpassword` | passes when auto-login is off and no `kcpassword` is left, or when the user is a `kbf-lease-*` account whose uid is in `lease_uids` and whose lease `kbf-mac-session`'s intent record (`/var/db/kbf-mac-session/session-intent`, one line `<term>.<seq>`, not a link) names: set by `session-login` for a `whole_machine` lease; `apply` leaves it. With `autologin=vm` it also passes when the user is the profile's VM user `kbf-vm` (its uid is `vm_user_uid`) and `/etc/kcpassword` is a file that holds the password of its password file. Otherwise, a lease user left behind and the VM user under `autologin=off` included, `apply` clears both. With `autologin=vm` (which needs `vm_user_uid`) the wanted state is the VM user, or a lease user during a `whole_machine` lease: `apply` writes `/etc/kcpassword` (root's, mode 600) from the password file and sets `autoLoginUser` to `kbf-vm`, only while `fdesetup status` says FileVault is off, the account exists and the password file is valid; it restarts nothing, so the change shows at the next login window |
| `vm_user_uid` (optional) | uid 450-499; left out, the item passes as "not managed" | `dscl . -readall` of `/Users` and `/Groups` (once each) for the account, its groups, the uid and the name; `ls` of the home, the state directory and the password file | `dscl .` fed `create` for the account's attributes, then `passwd` (the password on standard input); `dseditgroup -o edit -d` for a group it must not be in; the home, state directory and password file made root's or the user's with the modes above |
| `time_server` | host | `systemsetup -getusingnetworktime`, `-getnetworktimeserver` | `systemsetup -setusingnetworktime on`, `-setnetworktimeserver` |
| `time_max_offset_ms` | ms | `sntp -t 5` (read-only) | `sntp -sS` |
| `xcode` (`xcode_xip_dir`) | `BUILD:/Applications/Xcode*.app/Contents/Developer:SHA256` | `ProductBuildVersion` of each app's `version.plist`; any other `Xcode*.app` directory in `/Applications` is drift (links are skipped) | installs a missing or wrong Xcode from the `.xip` in `xcode_xip_dir` whose SHA-256 the line names (`xip --expand`, its build checked, `xcodebuild -license accept`, `-runFirstLaunch`); removes unpinned Xcodes |
| `default_developer_dir` | a pinned Xcode's path | `xcode-select -p` | `xcode-select -s` |
| `probe` (`expect_probe`) | `KEY:/PATH:SHA256`; `KEY:XCODE_BUILD=VALUE` | each probe script's SHA-256 | check only. `expect_probe` lines are validated (a known probe, a pinned Xcode, once each) and printed |
| `simulator_runtimes` | none | `/Library/Developer/CoreSimulator`: `Images/*.dmg`, `Profiles/Runtimes/*.simruntime`, `Volumes/*` | check only: runtimes live in VM images |
| `launchd_label` (`role_user`) | label | `/usr/local/etc/kbf/kbf.toml` equals the one the profile renders, root-owned, mode 644, in a root's 755 directory; `/Library/LaunchDaemons/<label>.plist` equals the one the profile renders (section 5.3: `UserName`/`GroupName` the role account, `ProcessType` `Interactive`, `ExitTimeOut` 30, `kbf --mode=daemon --config <kbf.toml>` and nothing else), root-owned, mode 644; `/Library/Logs/kbf` root's, mode 755; the error log in it a regular file with one link, the role account's, mode 644; the job loaded | writes `kbf.toml` (and its directory, root's, mode 755), makes the log directory root's, replaces anything else at the error log's path (unlinked, never followed) with a new file of the role account, writes the plist, then restarts the job: `launchctl bootout`, the error log truncated while it is stopped, `launchctl bootstrap`. `restart` refuses, before it stops anything, when the error log is not such a file, so a link the role account plants is never emptied as root |

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
