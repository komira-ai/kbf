# kbf-host-install: the root side of a host-driven update

`kbf-host-install` is the program a host runs, as root, when the updater asks it to install
a version of the `kbf` package. It is not a command anyone types: sshd runs it as the forced
command of one key, and it does only what the allowlist on that host permits.

## Getting the binary

A release carries `kbf-host-install` as a tarball per platform, built, signed and attested by
the same run as every other asset ([../artifacts.md](../artifacts.md)):

| Host | Asset |
|---|---|
| Linux, x86_64 | `kbf-host-install-<commit>-linux-x86_64.tar.gz` |
| Linux, arm64 | `kbf-host-install-<commit>-linux-arm64.tar.gz` |
| macOS, Apple silicon | `kbf-host-install-<commit>-darwin-arm64.tar.gz` (ad hoc signed with the hardened runtime) |

No package ships it: neither the `kbf` nor the `kbf-installer` deb installs it, so a Linux
host gets it from the tarball too. Each tarball holds one member, `kbf-host-install`. Its
`<commit>` is the commit of the release the host is on or going to. The updater,
`kbf-updater`, is the same kind of asset for the operator's Linux box and for a Mac's host agent
(`kbf-updater-<commit>-linux-<arch>.tar.gz` and `kbf-updater-<commit>-darwin-arm64.tar.gz`, one
member `kbf-updater`; the Mac one is ad hoc signed with the hardened runtime).

Fetch the tarball with its bundle and check the bundle before the binary is placed anywhere
(the flags are those of [../artifacts.md](../artifacts.md#provenance-bundles-verifying-a-tarball-offline)):

```text
gh release download <tag> --repo komira-ai/kbf \
  --pattern 'kbf-host-install-<commit>-<platform>.tar.gz*' --pattern SHA256SUMS --dir dl
gh attestation trusted-root > dl/trusted_root.jsonl
cd dl && sha256sum --check --strict --ignore-missing SHA256SUMS      # shasum -a 256 -c on a Mac
gh attestation verify kbf-host-install-<commit>-<platform>.tar.gz \
  --bundle kbf-host-install-<commit>-<platform>.tar.gz.sigstore.json \
  --custom-trusted-root trusted_root.jsonl --repo komira-ai/kbf \
  --signer-workflow komira-ai/kbf/.github/workflows/artifacts.yml \
  --cert-oidc-issuer https://token.actions.githubusercontent.com \
  --source-ref refs/heads/main --source-digest <commit> \
  --predicate-type https://slsa.dev/provenance/v1 --deny-self-hosted-runners
tar -xzf kbf-host-install-<commit>-<platform>.tar.gz kbf-host-install
```

Place the binary at the path the key or the Mac section below names, as root, in directories
only root can write. `kbf-host-install --version` prints `kbf-host-install <package version>`
and does nothing else: it is the one argument the program takes outside the Mac's two
internal ones, and what the release uses to check the signed Mac binary runs.

## The key

The updater's ssh key is installed in root's `authorized_keys` as

```text
from="203.0.113.10",restrict,command="/usr/libexec/kbf/kbf-host-install" ssh-ed25519 AAAA...
```

`from=` is the address the updater connects from. `restrict` turns off forwarding, a
terminal and the rest. `command=` runs this program whatever the client asked for; the
client's request arrives only in `SSH_ORIGINAL_COMMAND`. The program reads nothing from its
arguments or standard input (bar `--version`, above).

One more entry point exists, and it is not reachable over ssh: an install runs this same
program again inside a systemd unit (see below), with the request in the variable
`KBF_HOST_INSTALL_UNIT`, which only `systemd-run` sets. The variable counts only when none of
`SSH_ORIGINAL_COMMAND`, `SSH_CONNECTION`, `SSH_CLIENT` and `SSH_TTY` is set; in an ssh session it
is ignored, and the program handles `SSH_ORIGINAL_COMMAND` as usual.

## The three requests

The whole string must be one of these, with single spaces and nothing before or after:

```text
install kbf=<version> from <channel>
restart frontend=<name>
query
```

- `<version>` is `[0-9][0-9A-Za-z.+~-]*`, at most 128 bytes, without an epoch (no `:`), and a
  version `dpkg` itself accepts (so `1.0-` is refused).
- `<channel>` is `[a-z0-9][a-z0-9-]{0,31}`.
- `<name>` is `-`, which names the unit `kbf-frontend.service`, or an instance name
  `[a-z][a-z0-9-]{0,31}`, which names `kbf-frontend@<name>.service`. The unit string is built
  from the checked name alone; no other unit can be named, and nothing is expanded.
- The package is always `kbf`.

Anything else is refused: exit status 2, and no process is started.

## The allowlist

`/etc/kbf/host-install.toml` names the channels an install may use. It must be owned by
root and not writable by its group or others, or the program refuses to run. So must every
directory above it, up to `/` (whoever can write a directory can replace what is in it); the
same holds for each source file and each key file below.

```toml
[channels.candidate]
suite = "candidate"
source_list = "/etc/kbf/host-install/candidate.sources"

[channels.stable]
suite = "stable"
source_list = "/etc/kbf/host-install/stable.sources"
```

`source_list` is the one apt source the install may read: a deb822 file (`Types:`,
`Signed-By:`) or a one-line apt list (`deb [signed-by=/path] ...`). It must be owned by
root and not writable by others, and every entry in it must name `Signed-By` as absolute
paths of key files that are themselves owned by root and not writable by others. A source
without `signed-by`, with a key given as a fingerprint or inline, or with a key a user could
replace, is refused (`channel_unsigned`, `key_untrusted`).

A source that names a key and still weakens the check is refused too (`channel_insecure`):
the options `trusted`, `allow-insecure`, `allow-weak` and `allow-downgrade-to-insecure` of the
one-line form, and the deb822 fields `Trusted`, `Allow-Insecure`, `Allow-Weak` and
`Allow-Downgrade-To-Insecure`, in any case, with any value but a plain no (`no`, `false`,
`off`, `without`, `disable`, `0`). A directory above one of these files that its group or
others can write is `config_unsafe` (`key_untrusted` for a key).

The path of this file is a constant of the program. No flag and no environment variable
changes it.

## What an install does

The process sshd starts checks the request, then hands the work to one transient systemd unit
and relays its answer. The work is in one unit, run by one process that holds the install
lock for all of it, so a dropped ssh connection stops neither `dpkg` nor the lock, and there is
no moment between the two apt steps at which an install looks idle.

The process sshd starts:

1. Reads the allowlist and the channel's source and checks their signing (above); refuses
   if any check fails. Opens the audit log; fails (`audit_unavailable`) if it cannot.
2. Probes `/run/kbf-host-install.lock` without taking it; if another install holds it, or it
   cannot be asked about, answers `failed` with reason `busy` and starts nothing. It never
   holds the lock itself.
3. Runs `systemd-run --wait --collect --pipe --quiet --unit=kbf-host-install-<version>.service
   --property=RuntimeMaxSec=1800 --setenv=KBF_HOST_INSTALL_UNIT=<the request> --
   /usr/libexec/kbf/kbf-host-install`, and relays the last line the unit prints as its own
   answer, with the unit's stderr as the detail of a failure. A unit that gives no answer (it
   could not start, was killed, printed something else) is `install_failed`; an answer for a
   version other than the one asked for is `verify_failed`.

The unit (this program again, as root, with the fixed environment) does the install:

0. Reads the allowlist and the source again, takes a non-blocking `flock` on
   `/run/kbf-host-install.lock` and keeps it until it exits; if another install holds it,
   answers `failed` with reason `busy` and runs nothing.
1. Reads the installed version with `dpkg-query`. The same version is a no-op that reports
   `already_installed`; any other version, older or newer, is installed. A rollback is an
   install of an older version, and nothing here refuses it. A package dpkg flags with an
   error (the third character of its status, `R` for reinstall required) is not cleanly
   installed: an install of any version goes ahead, the same one included (apt repairs it).
2. Runs `apt-get update` restricted to the channel's source file
   (`-o Dir::Etc::SourceList=<file> -o Dir::Etc::SourceParts=-`), then
   `apt-get install -y --no-install-recommends --allow-downgrades -o Dpkg::Options::=--force-confold
   -o DPkg::Lock::Timeout=300 -t <suite> kbf=<version>` with the same restriction.
3. Reads the installed version again and reports `installed` only if it is the target and
   the package is cleanly installed.

No shell runs anywhere: every process is started with an explicit argument vector and an
environment of `PATH`, `LANG=C` and `DEBIAN_FRONTEND=noninteractive` alone.

## What a restart does

A package install restarts no `kbf` unit, so a client that has installed a new version asks for
the restart of the one frontend unit it rolls, after the install. Linux only: a Mac refuses
the request as `bad_command` (it has no such unit).

The process sshd starts does all of it, and holds the install lock (`flock` on
`/run/kbf-host-install.lock`) from before the first command until it answers, so while it runs a
restart and an install do not overlap: an install that finds the lock held is `busy`, and so is a
restart.

1. Opens the audit log; fails (`audit_unavailable`) if it cannot, and runs nothing.
2. Takes the lock without waiting; if another install or restart holds it (or it cannot be
   opened), answers `failed` with reason `busy` (`lock_unavailable`) and runs nothing.
3. Runs `systemctl restart -- <unit>` (killed after 120 seconds). A start failure or an exit
   status but 0 is `failed` with reason `restart_failed`, and the detail on stderr holds the
   exit status and what `systemctl` wrote.
4. Runs `systemctl is-active -- <unit>` (each look killed after 10 seconds) every half second
   until it exits 0, which it does for the states `active` and `reloading` (`activating` does
   not count).
   A unit that is not active 120 seconds after the restart returned is `failed` with reason
   `restart_timeout`. Otherwise the answer is `restarted`, naming the unit.

The restart does not read the channel allowlist. Nothing in the program notices a dropped ssh
connection: it goes on with the restart and the wait (at most about 250 seconds in all),
holds the lock until then, and its answer is lost, so a restart asked for again in that time is
`busy`. If something outside ends the process, the lock goes with it and systemd carries on with
the restart. Either way the client asks the host again after that bound.

## The answer

One JSON line on stdout.

| request | line | exit |
|---|---|---|
| install, done | `{"result":"installed","installed_version":"...","unit_state":"finished"}` | 0 |
| install, same version | `{"result":"already_installed","installed_version":"...","unit_state":"not_run"}` | 0 |
| restart, done | `{"result":"restarted","unit":"kbf-frontend@<name>.service"}` (`kbf-frontend.service` for `-`) | 0 |
| query | `{"result":"query","installed_version":"..."\|null,"running_commit":"..."\|null,"node_id":"..."\|null,"in_progress":true\|false}` | 0 |
| refused | `{"result":"refused","reason":"<code>"}` | 2 |
| failed | `{"result":"failed","reason":"<code>"}`, detail on stderr | 3 |

Refusal codes: `bad_command`, `channel_not_allowed`, `config_invalid`, `config_unsafe`,
`channel_unsigned`, `key_untrusted`, `channel_insecure`, and, on a Mac, for a channel whose source
is GitHub, `unknown_release`, `ambiguous_release` and `fetch_failed`. Failure codes:
`audit_unavailable`, `query_failed`, `installed_version_invalid`, `package_state_unclean`,
`update_failed`, `install_failed`, `verify_failed`, `busy`, `lock_unavailable`, and for a restart
`restart_failed` and `restart_timeout`.

Every line has a `result`: `installed`, `already_installed`, `restarted`, `query`, `refused` or
`failed`.

`installed_version` of a query is the version dpkg reports when `kbf` is cleanly installed,
and null when it is not installed or dpkg flags an error on the package (the updater then
installs, which repairs it). `running_commit` is the commit `kbf --version --json` of the
installed binary reports, or null (it is the installed binary's, not the running process's: a daemon that was not restarted after an upgrade still runs the older one, which the Mac side avoids by reading the process); the binary is killed after 10 seconds, and a binary that
fails, hangs or prints something else has none. `node_id` is the id of the node: `[node] id`
of `/etc/kbf/kbf.toml`, else its `[identity] node-id`, else the short host name in lower case
(the id an enrolling node takes when its configuration names none). It is null when the file
is missing or is not root's alone to write, or when the value is not a DNS label. A client that
acts on one host among several compares it with the id it expects before it does anything.

`in_progress` is true when any of three things holds, and false only when none does: the
install lock `/run/kbf-host-install.lock` is held (probed, then let go at once; the process
that does the work holds it from before it reads the package database until it ends, so the
gap between the two apt steps is covered; a running restart holds it too, so `in_progress` is
true during one); a `kbf-host-install-*` unit is listed by
`systemctl list-units` in the state `active`, `activating`, `deactivating` or `reloading`; or
some process holds the dpkg frontend lock (read from `/proc/locks`; nothing is locked or
waited for). If the listing itself fails, the query fails (`query_failed`) and does not
guess.

## The audit log

Every invocation, refused ones included, appends one JSON line to
`/var/log/kbf-host-install.log` (created root-owned, mode 0600, never through a symbolic
link): the time, the verb (`install`, `restart` or `query`), the version, the channel, the
outcome and the reason, and, on the line of a restart, the `unit`. On a Mac the
apply job appends one more line when it ends: verb `apply`, the version, outcome `applied` or
`failed`, and `result`, the status line (`done`, `failed:not_welcomed:rolled_back`, ...). A request
that was not understood is recorded escaped and cut at 200 characters. An install or a restart
is not started when the log cannot be opened.

## On a Mac

A Mac takes the same `install kbf=<version> from <channel>` and `query` requests (a `restart`
is refused as `bad_command`) and answers in the same shape, but it installs the attested release tarball
`kbf-<commit>-darwin-arm64.tar.gz` (not a package), into the kit layout, and restarts the
daemon's launchd job on it. The program is the same binary; the platform picks the Mac side.
Nothing here is a Homebrew formula or a `kbf-installer` order.

### The layout it works in

```text
/usr/local/kbf/releases/<commit>/kbf        the release (root:wheel, 0755), and beside it
/usr/local/kbf/releases/<commit>/VERSION    the version text it was installed as
/usr/local/kbf/current   -> releases/<commit>   relative links, moved by atomic rename
/usr/local/kbf/previous  -> releases/<commit>
/usr/local/kbf/etc/av2_mac.conf             NODE_ID=<id> (the node's id)
/usr/local/kbf/log/kbf-daemon.err.log       where launchd writes the daemon's stderr
/Library/LaunchDaemons/com.komira.kbf-daemon.plist    the daemon's job, run as _kbf
```

The error-log path is the `StandardErrorPath` of the kit's daemon plist. The `/Library/Logs/kbf`
paths of `kbf-mac-provision` belong to that tool's own layout; this program does not read them
and this document does not describe them.

`<version>` is the version text of a build, `0.1.<seq>+g<12 hex digits>-<run>`. Its 12 digits
pick the one tarball `kbf-<40 hex digits>-darwin-arm64.tar.gz` whose commit starts with them:
among the file names of the channel's `dir`, or, when the channel pins its releases with
`sums`, among the lines of its `SHA256SUMS`, or, for a channel whose source is GitHub, among the
assets of the repository's releases. No tarball is an unknown release and two are ambiguous
(`verify_failed`, with the reason on stderr; a channel on GitHub refuses with `unknown_release`
and `ambiguous_release`). The version text is kept in `VERSION`,
and `query` reports it as `installed_version`.

### The files, once per Mac (the operator's one root session)

Everything below is done once, as an administrator with `sudo`; this program does none of it.

1. The binary is the `kbf-host-install` of the verified darwin-arm64 release tarball ([Getting the binary](#getting-the-binary)), placed at `/usr/local/libexec/kbf/kbf-host-install`, `root:wheel` 0755, in directories
   (`/usr/local`, `libexec`, `kbf`) that group and others cannot write. An install checks this
   before it does anything else and refuses with `config_unsafe` when the program or one of
   those directories is not root's or is writable by group or others (some Intel Homebrew
   setups leave `/usr/local` group-writable).
2. An ordinary account for the updater's ssh login. It is not root and not an administrator;
   the examples call it `kbfupdate`. Remote Login is on, and its
   `~kbfupdate/.ssh/authorized_keys` has exactly one line:

   ```text
   from="203.0.113.10",restrict,command="/usr/local/libexec/kbf/kbf-host-install" ssh-ed25519 AAAA...
   ```

3. The single sudoers rule, in `/etc/sudoers.d/kbf-host-install` (`root:wheel` 0440, checked
   with `visudo -cf`):

   ```text
   kbfupdate ALL=(root) NOPASSWD: /usr/local/libexec/kbf/kbf-host-install --as-root
   ```

   It allows one program with one argument, to one account. The program then reads the
   request from its standard input and checks it against the grammar again, so the rule does
   not widen what the key can ask for. The sudoers entry, not the ssh key, is what bounds
   `kbfupdate`; a `Defaults:kbfupdate !setenv, env_reset` line keeps its environment out of the
   program.
4. `/usr/local/etc/kbf/host-install.toml`, root's alone to write:

   ```toml
   [channels.stable]
   dir = "/usr/local/kbf-host-install/stable"
   sums = "/usr/local/etc/kbf/host-install/stable.SHA256SUMS"   # optional
   source_ref = "refs/heads/main"      # optional; this is the default

   [channels.online]                   # a channel on GitHub instead of a directory
   source = "github"
   token_file = "/usr/local/etc/kbf/host-install/github.token"   # optional: a higher rate limit; see below
   ```

   A channel takes its releases from a `dir` or from `source = "github"`, never both (the
   allowlist is `config_invalid` when it names both or neither). `sums` and `source_ref` go with
   either. A channel on GitHub is described in [its own section](#a-channel-on-github).

   The trust anchor is the build attestation, which every release must pass (check 4 below).
   `sums` is optional. When it is set it is an extra freeze list: the pinned `SHA256SUMS` of the
   releases the channel offers, root's alone to write, and a release it does not list is
   refused, and a tarball is taken only when its sha256 is listed there. When it is absent the
   attestation alone decides. `dir` is where the release files
   are put: `kbf-<commit>-darwin-arm64.tar.gz` and `kbf-<commit>-darwin-arm64.tar.gz.sigstore.json`.
   Whoever puts them there is not trusted; they are copied once and checked. Putting them there
   is not done by this program.
5. The pinned `gh` and the Sigstore trusted root that the `kbf-installer` package installs
   (`/usr/local/kbf-installer/trust/gh` and `trusted_root.jsonl`), root-owned as that package
   leaves them.

The first install into a Mac that the kit set up needs nothing else: the kit's links, plist and
`av2_mac.conf` are what this program reads.

### The three entries

| process | started by | does |
|---|---|---|
| `kbf-host-install` as `kbfupdate` | sshd (the forced command) | passes the request, unchanged, to the root process through `sudo -n ... --as-root` and prints its line and exit status |
| `kbf-host-install --as-root` as root | that sudo rule | reads the request from standard input; checks it, audits it, answers; for an install, checks the release and launches the apply job |
| `kbf-host-install --apply <stage>` as root | launchd, as the one-shot job `com.komira.kbf-host-install-apply` | lays the release out, moves the links, restarts the daemon, records the result |

A login that is already root (the key is root's) handles the request in its own process and
needs no sudo. `--as-root` and `--apply` are refused for any other user, and `--apply` for any
stage but the program's own.

### What an install checks first

Before anything on the Mac changes, in this order (a failure changes nothing and removes the
stage):

1. the request, the channel and its pinned sums (when it has any), and that `gh` and the trusted
   root are root's;
2. the files of the channel's `dir` (or, for a channel on GitHub, the two files fetched as
   `nobody` into a directory it owns inside the stage) are copied once, into `/var/db/kbf-host-install/stage`
   (root's, mode 0700), without following links, as regular files with one link and under a size
   limit; only that copy is checked from here on;
3. when the channel has sums, the sha256 of the tarball is the one they list (a channel without
   sums skips this);
4. `gh attestation verify` accepts the tarball and its bundle, offline, run as `nobody` and
   never as root. This is the trust anchor, and it binds: the tarball's sha256 as a subject
   (`gh` hashes the staged copy), the build's source digest to the full 40-digit commit (not the
   version's 12 digits), its source ref to the channel's branch, the signer to
   `komira-ai/kbf`'s `artifacts.yml` workflow (the repository's former name is still accepted,
   for releases attested under it; a channel may name [another repository and
   workflow](#a-channel-of-another-repository)), and GitHub-hosted runners only;
5. the tarball holds exactly one regular file named `kbf`, a Mach-O file for arm64;
6. the binary carries no `com.apple.quarantine` attribute (it is removed, and proven gone);
7. `codesign --verify --strict` accepts it and its signature has the hardened runtime;
8. the new binary, run as `_kbf`, accepts the daemon's live configuration
   (`kbf --mode=daemon --config /usr/local/etc/kbf/kbf.toml --check`);
9. the staged binary is root's, in directories only root can write.

Steps 4 and 8 start their program with the privilege drop made in the child just before it
executes (`setgroups` to the one gid, then `setgid`, then `setuid`), in `/`: started by
`Command::uid` and `gid` alone, a child on macOS keeps the old effective gid (group 0, `wheel`) as
a supplementary group. A drop through `chroot(8)` is not used: macOS kills a process that calls
`chroot(2)` and then executes a program (`SIGKILL` at once, even for `/usr/bin/true`). The
real-Mac tests check that a system program run as `nobody` exits 0 and that the child's groups are
its own gid and nothing else.

The stage is opened to traverse-only (0711) for steps 4 and 8, which run as other users, and
is 0700 otherwise. While it is 0711 the users `nobody` and `_kbf` can pass through it but
cannot list it or write to it. Every file in it is root's and not writable by them except
`gh-home`, the directory of `gh`'s own home, which belongs to `nobody`; the files are public
release bytes. It is closed to 0700 again before `plan.json` is written. The same version already installed and running is `already_installed`; an
older version is installed like a newer one.

### The apply

The install verb returns once it has launched the apply job, so a dropped ssh session cannot
end the work between `launchctl bootout` and `launchctl bootstrap`. It answers

```json
{"result":"apply_started","version":"...","commit":"...","apply":"running"}
```

and exits 0. The job:

0. starts from the links the install saw: `current` and `previous` as they were when the install
   was accepted are in `plan.json` (so the job, a rollback, and whoever has to repair a job that
   was killed halfway work from the same facts; the next install repairs the kit too);
1. lays out `releases/<commit>/kbf` and `VERSION` (a release already there must be the same
   bytes), after checking that the file and every directory above it up to `/usr/local/kbf` are
   root's and closed to others, again without a quarantine attribute;
2. moves `previous` to the release that ran and `current` to the new one, each by renaming a
   link made aside;
3. runs `launchctl bootout system/com.komira.kbf-daemon`, waits (at most 60 seconds) until
   `launchctl print` no longer finds the service (bootout returns before the service is gone, and
   a bootstrap in that gap fails with an input/output error), then
   `launchctl bootstrap system /Library/LaunchDaemons/com.komira.kbf-daemon.plist`;
4. waits (at most 180 seconds) for the `welcomed` line of the NEW process in the daemon's error
   log, counting only what the log gained since before the bootstrap, and then requires (for at
   most 60 seconds) that the daemon's process runs the release that was started: the same reading
   `query` reports as `running_commit`;
5. if step 3 or 4 fails, moves both links back, checks that the old release's file is root's,
   restarts it the same way (with the same two checks) and records what became of it;
6. removes the stage, then writes the result line, so that the status leaves `running` only when
   nothing of the job is left to remove; and appends the outcome to the audit log.

Nothing here waits for leases: the drain that keeps builds from being cut is the updater's, done
before it asks for the install. `bootout` is synchronous and launchd kills the daemon after its
exit timeout.

The result is one line in `/var/db/kbf-host-install/status` and the `apply` field of `query`:

| `apply` | meaning |
|---|---|
| `null` | no apply has been recorded |
| `running` | the job is running (`in_progress` is true) |
| `done` | the new daemon logged `welcomed`; `running_commit` is the new commit |
| `failed:<reason>` | before anything restarted: `plan_invalid`, `stage_untrusted`, `stage_changed`, `release_differs`, `quarantine`, `layout_failed`, `release_untrusted`, `links_failed`, `launch_failed`; and `apply_lost` for a `running` status whose job is gone |
| `failed:<reason>:rolled_back` | the restart failed (`bootout_timeout`, `bootstrap_failed`, `not_welcomed`, `wrong_release`) and the previous release is running again |
| `failed:<reason>:rollback_failed` | the previous release did not come up either |
| `failed:<reason>:no_previous` | there was no release before this one to go back to |

A reader polls `query` until `in_progress` is false and then reads `apply`.

A second install while the apply job is loaded and has not ended, including the moment after
launchd has loaded it and before it has started, is `busy`: the stage is not touched. A failed
move of the links is undone before `failed:links_failed` is recorded, so `previous` keeps naming
the release before the last apply.

### A rollback is an install of an older version

There is no floor and no "not older" refusal. To go back, install the older version from the
channel that offers it; it takes the same path, and `previous` is only where the last apply came
from.

The other side of this is that whoever holds the updater's key can install any release the
channel offers that passes the attestation, including an old one with a known defect. On a
channel with `sums` the way to make a release uninstallable is to remove its line from that file;
on one without, it is to remove its files from `dir`.

### `query` on a Mac

```json
{"result":"query","installed_version":"..."|null,"running_commit":"..."|null,"node_id":"..."|null,"in_progress":true|false,"apply":"..."|null}
```

- `installed_version` is the `VERSION` of the release `current` names, or null.
- `running_commit` is the commit of the program the RUNNING daemon executes: the pid from
  `launchctl print system/com.komira.kbf-daemon`, and the file `lsof` says that process has
  mapped as its program, which must be `releases/<commit>/kbf`. It is null when any step does not
  say; it is never read from the disk. A release that `current` names and the daemon has not
  restarted onto is therefore not reported as running.
- `node_id` is `NODE_ID` of `av2_mac.conf`, read as data: every line must be blank, a `#`
  comment or `KEY=VALUE` with a plain value, the value must be a DNS label, and it must appear
  once; any other file gives null.
- `apply` is new on a Mac, and the install answer `apply_started` is too. The failures reuse the
  closed codes of the Linux side (`verify_failed` for every check above, `install_failed` for a
  stage or launch that failed, `busy`, `package_state_unclean` for a missing kit); the words are
  on stderr.

### A channel on GitHub

A channel with `source = "github"` needs no file delivery and no edit of the allowlist for a new
release: the install fetches the release itself, as `nobody`, from the GitHub releases of
`komira-ai/kbf` (or the repository the channel names, [below](#a-channel-of-another-repository)), and then handles the two files exactly as it handles the files of a `dir`
(copied once into the stage, `sums` checked when set, then the offline attestation check and
everything after it). The build attestation is still the only trust anchor; the listing and the
download only bring the bytes. A directory channel stays for a Mac that cannot reach GitHub.

What the install does, in this order, with the system `curl` (listing) and the pinned `gh`
(download), both run as `nobody` (the same privilege drop as the attestation check) in a
directory that `nobody` owns inside the stage, with its own `HOME`:

1. lists the repository's releases with `/usr/bin/curl` from the public REST endpoint
   `https://api.github.com/repos/komira-ai/kbf/releases?per_page=100&page=N` (the channel's repository), with no token. The
   flags are fixed: no configuration file (`-q` first), HTTPS only, no redirect, an HTTP error
   fails the command, a time limit, a size limit, and an empty environment. It is 100 a page, newest first, page
   after page until a page is empty, and at most 30 pages (an empty page ends it; a short one
   does not); never only the latest, so a rollback to an old release works;
2. finds the asset named `kbf-<40 hex>-darwin-arm64.tar.gz` whose commit starts with the 12 hex
   digits of the version, across all the pages read. None is `unknown_release`; assets of two
   different commits are `ambiguous_release`; the same commit in two releases is one release (the
   newest). A listing cut at the 30-page limit without a match is `fetch_failed`, not
   `unknown_release`, since not all of it was seen. When the channel has `sums`, the commit is
   the one the sums list, and the listing must hold it;
3. builds the download itself from the release's tag and the two asset names:
   `gh release download <tag> --repo komira-ai/kbf --pattern <tarball> --pattern <bundle> --dir <dir>`
   (the channel's repository again).
   The URLs in the listing (`browser_download_url`, `url`) are never read or followed, and an entry
   whose tag is not letters, digits and `. _ + -` is not used;
4. runs that download under a time limit (15 minutes) and a limit on what it may write (the
   tarball limit plus the bundle limit, 512 MiB and 256 KiB, watched while it runs; the sizes the
   listing gives are checked before it starts). A file over its limit, a timeout, a network
   failure, an answer that is not a listing and a release without its bundle are `fetch_failed`;
5. copies the two files once, as root, into the stage, and removes the directory `nobody` wrote
   in. From here on nothing differs from a directory channel.

Every one of these refusals is exit status 2 with no change to the kit, the status file or
launchd, and no stage left behind.

#### What the Mac needs

- Outbound HTTPS to GitHub: `api.github.com` and `github.com`, and the host GitHub serves
  release assets from. The child's environment is cleared, so no proxy variable reaches `gh`;
  a Mac that can reach GitHub only through a proxy cannot use this source.
- No token. The listing and the download of a public repository need none, and by default
  no secret is on the Mac.
- Optionally, a token for a higher rate limit. A channel may name a `token_file`: a file that is
  root's alone to write (checked like the allowlist) holding a GitHub token, one line of letters,
  digits, `_`, `-` and `.`, at most 1024 bytes (a workflow token of GitHub Actions is about 520). It needs no scope: the repository is public. The root process reads it and
  sends it to `curl` as an `Authorization` header in a configuration on `curl`'s standard input
  (never in its arguments, which every user can read), and gives it to `gh release download` as
  `GH_TOKEN`; the attestation check never gets it.
- The system `curl` that macOS ships, `/usr/bin/curl`, which must be root's and not writable by
  others (checked like the pinned `gh`, and only for a GitHub channel: a directory channel never
  runs it).

#### The residuals

- A token, when a channel names one, is a credential on the Mac. It can read public data only,
  but it is an account's token; `nobody`'s `curl` process holds it in memory and `gh release
  download` in its environment while they run. Without a `token_file` there is none.
- The listing is read through GitHub's API and is not itself trusted: it can only choose among
  releases (an old attested release is installable, as with a directory) or withhold one (the
  install fails). What it picks must still pass the attestation for the full commit.
- Without a token GitHub's API allows 60 requests an hour per IP address, shared by every
  machine behind the same address. One install costs one request per 100 releases listed (a
  repository with fewer than 100 releases: one request for the releases and one for the empty
  page that ends the listing, so two), and the download is not counted against the API limit.
  Past the limit GitHub answers 403, and the install is `fetch_failed` with nothing staged:
  retry later (the limit resets within the hour), or name a `token_file`, which raises it to
  the token's own. A listing of about 3000 releases or more does not
  show its oldest releases, so whoever can publish many releases can make a rollback to an old
  one fail (`fetch_failed`); it cannot make a wrong release install.
- The size limit is watched about every 10 ms, so a fast download can write a little past it
  before it is killed; the copy then refuses the file, and the stage is removed.
- A channel with `sums` pins the commit and the tarball's hash, and needs an edit of the sums file
  for each new release; the point of this source is a channel without them.

### A channel of another repository

By default a channel takes its releases from `komira-ai/kbf`, trusts the attestation of that
repository's `artifacts.yml`, and names its files `kbf-<commit>-...`. A channel with
`source = "github"` may name its own:

```toml
trusted_workflows = ["owner/other/.github/workflows/release.yml"]   # top level, before the first [channels.*]; empty by default

[channels.other]
source = "github"
repository = "owner/other"
signer_workflow = "owner/other/.github/workflows/release.yml"
artifact_prefix = "other"                 # optional; "kbf" is the default
```

- `repository` is `owner/name` (letters, digits and `-` in the owner; letters, digits and
  `. _ -` in the name). It is the repository the releases are listed from (the URL above), the
  one `gh release download --repo` reads, and the `--repo` of the attestation check.
- `signer_workflow` is the workflow whose attestation is believed (`--signer-workflow`): a
  `.yml` or `.yaml` file directly under that repository's `.github/workflows/`. It comes with
  `repository`; one without the other is `config_invalid`.
- `artifact_prefix` replaces `kbf` at the start of the release files' names
  (`<prefix>-<commit>-<platform>.tar.gz`, `.deb`, `.rpm`, and `<prefix>-frontend-image-<commit>-...`).
  It is lower-case letters, digits and `-`, and goes only with `source = "github"` and no `sums`
  (a directory's file names and a sums file are kbf's).
- A string that is not safe to put in a URL or an argument of `gh` makes the file `config_invalid`
  when it is read, so no channel of the file is usable. So does an entry of `trusted_workflows`
  that is not `owner/name/.github/workflows/<file>.yml`.
- **Any signer other than exactly `komira-ai/kbf` with `.github/workflows/artifacts.yml` is
  refused when the channel is used**, unless the same file lists that exact workflow in the
  top-level `trusted_workflows`. This holds for another workflow of `komira-ai/kbf` too (a channel
  naming its `ci.yml` needs the entry), and an entry for one workflow does not trust the other
  workflows of its repository. The answer is `channel_not_allowed`, and the words on stderr (the
  socket's `detail`) name the channel and the workflow. Trusting a workflow to sign what this host
  installs is the operator's act, on a file that must be root's alone to write like the rest of
  the allowlist; nothing trusts one by default, and `trusted_workflows` is empty when the key is
  absent.
- The other rules of the check do not change: the attestation must come from a GitHub-hosted
  runner, name the full commit, the channel's branch, and a subject whose sha256 is the file's,
  and be checked offline by the pinned `gh` and trusted root. The former `komira-ai/komira-build-farm`
  name is accepted for the `komira-ai/kbf` default pair only; for any other pair `gh` is asked once.
- Only the signer, the listing and the file names are the channel's. What is checked about the
  contents (the tarball holds the one `kbf` executable, a package is named `kbf`) is unchanged.

A file without these keys means what it meant before.

## The local socket

`kbf-host-install --serve` is the program's other door, for the agent mode of `kbf-updater`
([kbf-updater.md](kbf-updater.md#the-agent-mode), [../design/updates.md](../design/updates.md)).
It serves a unix socket instead of the ssh forced command, which stays as described above and
is not changed. It does one host's A/B: put the next release beside the one that runs, switch to
it on trial, keep it or go back.

```text
kbf-host-install --serve --socket PATH --socket-user NAME [--window-secs N] [--keep-releases N]
                 [--layout installer|deb|rpm] (--restart-unit UNIT... | --frontend-container)
```

- `--socket`: an absolute path. The program binds it itself (a `umask` of 0177, then a `chown`
  to the account), mode 0600, in a directory that is root's and that others cannot write. A
  stale socket file is replaced; a file of another kind in the way is an error.
- `--socket-user`: the one account that may connect. On Linux it is looked up in `/etc/passwd`,
  on a Mac with `dscl`; root is refused.
- `--window-secs`: how long a release on trial has to be confirmed, 120 to 1800 (default 600).
- `--keep-releases`: how many releases the host keeps besides the ones that must stay, 2 to 32
  (default 4). See [The releases a host keeps](#the-releases-a-host-keeps).
- `--layout`: how a Linux host keeps its releases. `installer` (the default) is the hand-placed
  layout of `kbf-installer`, described under "On Linux". `deb` and `rpm` are a host whose units
  are the `kbf` package's and run `/usr/bin/kbf`, updated by installing that package ("On a Linux
  daemon host that runs the package"). A Mac takes none.
- `--restart-unit`: on Linux, a unit that runs the release (`kbf-<name>.service` or
  `kbf-<name>@<instance>.service`), repeated for each; at least one is required, unless the host
  serves a frontend container. A Mac restarts the daemon's launchd job and takes none.
- `--frontend-container`: a Linux host whose frontend runs as a container, not as units: the
  release is the frontend's image and the switch recreates the container
  ([A frontend container](#a-frontend-container)). Not given with `--restart-unit`, nor with
  `--layout deb` or `rpm`.
- `--frontend-config PATH` (with `--frontend-container`): the absolute path of the container's
  configuration, default `/etc/kbf/host-install-frontend.toml`. See "Two frontends on one host".

Nothing else is configured by a flag or the environment: the files this program reads as root are
constants, as above. The units and jobs that start it are in
[`packaging/host-agent`](../../packaging/host-agent/README.md).

### The five requests

One line in, one JSON line out, one connection at a time. The same grammar for the version and
the channel as the forced command's (single spaces, no control byte; no path or URL anywhere).

```text
query
install kbf=<version> from <channel>
switch kbf=<version>
confirm kbf=<version>[ last_good=<version>]
revert
```

| request | does |
|---|---|
| `install` | looks `<version>` up in the channel, fetches the release **on the host**, verifies it, and lays it out beside the one that runs. No link moves and nothing restarts. The same version asked again is answered from what is laid out; the version the host runs already is answered at once (`current: true`) and teaches the host its version text |
| `switch` | puts the laid-out release in place **on trial**: the pending marker, with its deadline, is written before the first link moves; then the links move and the units restart on it. A unit that does not come up puts the host back at once |
| `confirm` | keeps the release on trial, then removes the releases the host no longer keeps. `last_good` names the coordinator's last good release, which the host then keeps whatever its age. Asked again after it was answered, the answer is the same |
| `revert` | goes back to the release that ran, now |
| `query` | what runs, what is on trial and until when, how the last update ended, whether another install, switch or restart is running, and the releases the host keeps |

Answers: `staged` (`version`, `commit`, `current`), `switched` (`version`, `commit`,
`deadline_unix`), `confirmed`, `reverted` (`version`, the release the host is back on, when it
knows its text), `query` (`commit`, `version`, `node_id`, `pending`, `last`, `busy`, and
`releases` when the host keeps release directories), and
`{"result":"refused","reason":CODE}` (nothing ran) or `{"result":"failed","reason":CODE}` with a
code from a closed set: `bad_command`, `channel_not_allowed`, `config_invalid`, `config_unsafe`,
`unknown_release`, `ambiguous_release`, `fetch_failed`, `verify_failed`, `pending`, `not_staged`,
`nothing_pending`, `no_current`, `busy`, `stage_failed`, `switch_failed`, `revert_failed`,
`state_unavailable`, `audit_unavailable`, `peer_not_ready`, `runtime_failed`. The detail of a refusal goes to the journal, cleaned,
never to the line.

### The releases a host keeps

A host with release directories (the `installer` layout, and a Mac's kit) keeps, in `releases/`:
the release that runs (`current`), the one before it (`previous`), every pinned release, the
coordinator's last good release, and the `--keep-releases` newest others (by the time each was
laid out). So going back to any of them needs no download. The rest are removed after a
`confirm`, and only then: never while a release is on trial, and not by `install` or `switch`.
On a Mac, the ssh path's apply removes them once the new daemon is welcomed, keeping 4 others.

What must stay besides the links is in `retain.json`, beside the installer's state (Linux,
`/var/lib/kbf-installer/retain.json`) or in the work directory (Mac): `pinned` (a list of
commits), `last_good` (a commit) and `versions` (the version text of each release, for the
listing). Every key is optional and an unknown one is ignored. A file that cannot be parsed
removes nothing, since it may hold a pin.

`query` lists the kept releases, newest first, as `releases`: one object per release with its
`commit`, its `version` (null when the host does not know it) and `why` it must stay (`current`,
`previous`, `on_trial`, `pinned`, `last_good`; left out for one of the newest others). At most 64
are listed, and a release that must stay is always among them. `kbf-deploy host status` prints the
same list.

The agent does not send `last_good` yet: the coordinator does not carry it in its orders. Until it
does, the coordinator's last good release is kept only as `previous`, a pin, or one of the newest
others.

### The revert is this program's

A release on trial has a deadline, in wall-clock seconds, on disk with the marker. The program
checks it about five times a second between connections: a release that nobody confirmed by its
deadline is put back by this program, **needing nothing of the release on trial to run**, the
updater to be alive, or the host to have stayed up. The first check after a start is the same
check, so a host that rebooted with a release on trial goes back at once if the deadline passed
while it was down, and waits out what is left of the window if it did not. A revert that
cannot be done (the units will not start on the release that ran) leaves the marker, and the next
check tries again.

Every request but `query` is audited like a forced-command request (verb `stage`, `switch`,
`confirm` or `revert`), and the install, the switch and the confirm are not run when the audit log
cannot be opened. A revert is never held back by it. The install lock is the same file as the
forced command's, so the two never overlap.

### On Linux

(A host whose frontend is a container is [below](#a-frontend-container).)

The release goes in the layout of `kbf-installer` ([host-releases.md](host-releases.md)):
`/var/lib/kbf-installer/releases/<commit>`, the `current` and `previous` links, and the
installer's `state.json`, which is also where the pending marker lives (`pending_confirm`, with
the deadline). A host that was set up by hand with `kbf-deploy host install` has all of it; a host
with no `current` release has nothing to go back to and is refused (`no_current`). The channels
are in `/etc/kbf/host-install-releases.toml`, in the format of the Mac's allowlist below (a
channel whose `source = "github"`, which may name its own [repository and signer](#a-channel-of-another-repository); the ssh path's `host-install.toml` is the apt channels' and is
not read):

```toml
[channels.stable]
source = "github"
```

`install` lists the repository's releases, downloads `kbf-<commit>-linux-<arch>.tar.gz` and its
bundle as the installer's unprivileged user, copies them once into the installer's scratch, and
runs the installer's verify pipeline over the copy with the pinned `gh` and trusted root
([host-releases.md](host-releases.md#what-is-trusted)): the attestation must be of this
repository's `artifacts.yml` (or the channel's own [signer](#a-channel-of-another-repository)) on a hosted runner, for the full commit, on the channel's branch; the
tarball must hold the one executable of this CPU; and the binary run as the unprivileged user must
say it is that commit. Nothing is fetched when the trust files are not root's. A release whose
`protocol` or `state_schema` is newer than this program's `kbf-installer` understands is refused
(the installer's rule, `verify_failed`): put the newer installer on the host first.

### On a Linux daemon host that runs the package

With `--layout deb` (or `rpm`) the host's units are the packaged ones
([packaging/linux](../../packaging/linux/README.md)), which run `/usr/bin/kbf`. No release is laid
out beside the running one and no link moves: the update is two installations of a package file,
with the trial recorded first. The same five requests apply, with the same answers and codes, and
the same channels file as above. The package and a record of it live in the program's state
directory (`/var/lib/kbf-host-install`: `package.json` and `packages/`, root's, 0600 and 0700).

| request | does |
|---|---|
| `install` | fetches the release's `kbf-<commit>-linux-<cpu>.deb` (or `.rpm`), its `.sigstore.json` bundle and the release's `SHA256SUMS` as the installer's unprivileged user, copies them once as root, and checks the copy, offline, before it keeps anything: the sha256 `SHA256SUMS` lists for the file; `gh attestation verify` (this repository's `artifacts.yml`, or the channel's own signer, on a hosted runner, for the full commit, on the channel's branch, with the pinned `gh` and trusted root); and the package manager's own reading of the file (`dpkg-deb --field`, `rpm --query --package`): the package `kbf`, for this CPU, at the version asked for. Nothing is installed, restarted or recorded |
| `switch` | checks the kept file's sha256 again; reads the installed version from the package database; keeps a verified copy of the package that runs (the one kept already, or the release of that version fetched and checked the same way); writes the trial, with its deadline, naming that copy; then installs the new file (`dpkg --force-confold --install`, or `rpm --upgrade --oldpackage --replacepkgs`: one file, no repository, no dependency resolution) and restarts each `--restart-unit` and waits for it. A package that fails to install, or a unit that does not come up, puts the previous package back at once |
| `confirm` | ends the trial and forgets every kept file but the new package and the one before it |
| `revert` | and the deadline: installs the kept copy of the package that ran (fetching and checking it again if the file is gone or changed), ends the trial, restarts the units |
| `query` | as above; `commit` is the commit of the package this host kept, else the 12 digits of the installed version, else the version text |

A rollback is an installation of an older version and is never refused: `dpkg` downgrades by itself
and `rpm` is told `--oldpackage`. The deadline is on disk before the package manager is asked for
anything, so a restart of this program or a reboot finds the trial and goes back once the deadline
has passed, as for the other layout. A going back that fails (the package manager cannot install the
kept file) leaves the trial, and the next try finds it. The audit log and the install lock are the
same as above.

The release publishes a deb for each CPU. It does not publish an rpm yet, so on an rpm host an
`install` is `unknown_release` until it does; the code path is the same.

A node that was set up by hand moves to this layout once, with
`kbf-host-install --migrate-to-package` ([native-package-migration.md](native-package-migration.md)).

### A frontend container

With `--frontend-container` the release is the frontend's image
(`kbf-frontend-image-<commit>-linux-<arch>.oci.tar`, [artifacts.md](../artifacts.md#the-frontend-image-oci))
and the runtime is Docker. The five requests are the same, so the pending marker, its deadline and
the revert at the deadline are the same. The host needs `docker` at `/usr/bin/docker`, the channels
file above (the image is fetched from the same releases), the installer's trust files (the pinned
`gh` and trusted root), and a root-owned `/etc/kbf/host-install-frontend.toml`, which describes the
container ([the example](../../packaging/host-agent/linux/host-install-frontend.toml)):

| key | meaning |
|---|---|
| `name`, `restart` | the container's name and Docker's restart policy (`always`, `unless-stopped`, `on-failure[:N]`). `no` is refused: nothing else starts the container, so it would stay down after a reboot of the host or of Docker |
| `network`, `user` | optional: `--network` and `--user` |
| `group_add` | optional: a list of numeric group ids as quoted strings (`["990"]`), each passed as `--group-add <gid>` after `--user`, on the container a switch makes and on the old one put back. Each is decimal digits only, from 1 to 4294967295: no name, sign, space or `0`, and `config_invalid` otherwise. A frontend that reaches `kbf-signer`'s socket through the signer's `--client-group` needs that group's id here, or a recreated container can no longer sign |
| `ports`, `mounts`, `command` | `--publish` and `--volume` entries, and the flags after the image; each entry is one argument, never a shell word |
| `api` | where this host reaches the frontend's operator API (a published port, or the host's own listen address under `network = "host"`), for `/readyz` and `/healthz` |
| `drain_token_file` | the operator token as a path **inside** the container |
| `peers` | optional: the other frontends' operator APIs this host can reach |
| `drain_secs` | optional, at least 1, no upper limit. Left out (the default), a switch waits for the old container to end however long its builds take. Set, it is how long the switch waits before it gives up; it never stops, removes or replaces a container that still runs. Keep it below about 9 minutes, or expect the updater (whose switch call is limited to 10 minutes) to report the host Unreachable during a longer drain while the switch completes. A file with `drain_secs = 0` still starts the helper; the switch refuses it (`config_invalid`) |
| `ready_secs` | optional (default 90, 10 to 600): how long a new container has to be ready |

`group_add` is written into the record only when it is set, so that a host that sets none writes
the record it always did and an older helper still reads it. **A host that sets `group_add` must
settle (confirm) or revert any release on trial before the helper is rolled back to a version
without `group_add`**: that version refuses a record with a key it does not know, and could not run
the deadline revert.

The trial window (`--window-secs`) starts when the new container does, so it never counts the
drain: only `ready_secs` is checked against it (`config_invalid` when `ready_secs` is longer).

**install** fetches the image, its attestation bundle and the release's `SHA256SUMS` as the
installer's unprivileged user, copies them once into a directory of root's, and checks, before the
runtime sees the file: the copy's sha256 is the one `SHA256SUMS` lists for the image (and the one
the channel's pinned `sums` list, if it has some), and `gh attestation verify` accepts the copy
offline for the full commit on the channel's branch, from `artifacts.yml` on a hosted runner. Only
then is the image loaded (`docker image load`), and its id recorded in `frontend.json` (root's,
0600, in `/var/lib/kbf-host-install`). Containers are made from that id, never from a tag. A
release that fails the check changes nothing: nothing is loaded and nothing is stopped.

**switch**, in this order:

1. it refuses (`peer_not_ready`) if any of `peers` does not answer `/readyz` with 200, before
   anything is written. This program can see only its own host and these peers; without `peers`
   the coordinator's order (one frontend at a time, never the last ready one) is the guard;
2. the restart policy is set to `no` (a drained frontend exits by itself, and must not be brought
   back as the old release);
3. the frontend is asked to drain through its own operator API: `docker exec` runs
   `/usr/bin/kbf cluster drain --frontend --token-file <drain_token_file>` in the container, where
   the API is on loopback ([api.md](../api.md#get-v1frontenddrain-and-post-v1frontenddrain)):
   `/readyz` answers 503 and the work it has finishes;
4. it waits until the container has ended (a drained frontend exits when it is idle), as long as
   that takes. With no `drain_secs` there is no deadline. A configured `drain_secs` only ends the
   waiting: the switch fails (`switch_failed`, "still draining") and **nothing is stopped,
   removed or replaced**. The old container keeps running and draining with its configured
   restart policy put back, no marker is written, and the old release is current. Ask for the
   switch again (the updater's next run does) and it drains again and waits again;
5. only now is the pending marker written to `frontend.json`, with its deadline, the images of
   both releases and a copy of the configuration; the ended container is stopped (`docker stop`,
   30 s, a formality: it has ended) and removed;
6. the new container is made from the new image with **the configuration of step 5, byte for
   byte**: `docker run --detach --name <name> --restart <restart> [--network] [--user]
   [--group-add]... [--publish]... [--volume]... <image id> <command>...`. The deadline of the
   trial is put on the marker as soon as it starts: `--window-secs` after the new container
   started, however long the drain took;
7. it waits up to `ready_secs` for `/readyz` to answer 200 and `/healthz` to name the release's
   commit.

A failure before step 5 (the drain request fails, the runtime stops answering) leaves the old
container as it is and puts its restart policy back. If the helper itself dies during the drain, the
policy is left off; when the helper starts and finds no marker and a container whose policy is
off, it sets the configured policy back and logs it. When a step from 5 on fails, the container is removed and made again from the **old** image id with the
same configuration, waited for the same way (on the old commit), the marker is cleared, and the
switch answers `failed` (`switch_failed`). If the old container cannot be made again either,
the answer is `revert_failed` and the marker stays: the deadline, or a `revert`, tries again.

Four things to know when you set it up:

- **`peers` is the only guard this program has against taking down the last ready frontend.**
  Without it nothing here looks at another frontend, and only the coordinator's order protects
  you. List every other frontend's operator API this host can reach; the example file's address
  is a documentation address to replace.
- **The operator API must be reachable on the published port.** `api` is a published port on the
  bridge network, so the frontend inside the container must listen on a non-loopback address
  (not `--api-listen 127.0.0.1:...`), or `/readyz` is never seen and every switch fails. Writes
  still require a loopback peer, which Docker's NAT does not give an outside caller; the drain
  runs inside the container, where the peer is loopback.
- **On the host network** (`network = "host"`, passed as `docker run --network host`), the
  container shares the host's network. Give no `ports`: there is nothing to publish. The helper does not refuse
  `ports` here; it passes them on as `--publish`, and Docker ignores them on the host network. `api` is then the operator listen address on the host itself, so
  `--api-listen 127.0.0.1:...` works, and writes from a loopback peer are accepted. The other
  frontend listeners are the host's own ports.
- **The health the updater's agent reports is what the last switch saw.** It comes from the
  release this program recorded as current, which `switch` sets after `/readyz` answered 200 on
  the target commit. It is not a probe made at each heartbeat: a container that crashes later is
  reported healthy until the next switch or the host's own supervision (Docker's restart policy)
  deals with it.

**confirm** clears the marker and forgets the old image. **revert**, which the deadline also
calls (so a host that rebooted in the middle of a switch is put back when the first check after the
start finds the deadline passed), removes whatever container there is and makes the old one again
from the copy of the configuration in the marker, not from the file as it is now.
A revert looks for the old image **before** it removes anything: if the image is gone, it fails
(`revert_failed`) and the container that runs is left as it is, and so is a switch that fails
readiness and cannot go back. Do not run `docker image prune -a` or `docker system prune -a`
while a release is on trial (a plain `docker image prune` is safe: the old image keeps its tag).

Before the first switch the release that runs is read from the container's image reference
(`kbf-frontend:<commit>`, the name `docker image load` gives it); make the first container from the
image of a release with the same options as the file. A container that already exists, made by hand
from `kbf-frontend:<commit>`, is adopted the same way: write a file that describes it (its name,
network, user, groups, mounts, ports and command) and the first switch starts from the image it
runs, with nothing recorded before.

#### Two frontends on one host

A host that runs a frontend container for each of two names (for example `kbf-fe-a` and
`kbf-fe-b`) runs one helper per container, each its own unit with its own file and socket:

```text
kbf-host-install --serve --socket /run/kbf-host-install/fe-a.sock --socket-user kbf-updater \
  --frontend-container --frontend-config /etc/kbf/host-install-frontend-a.toml
```

A helper started with a `--frontend-config` other than the default keeps its files under the name
of the container in that file. **Per helper**, in `/var/lib/kbf-host-install`: the record
`frontend-<name>.json` and the A/B state `state-frontend-<name>.json`; the install lock
`/run/kbf-host-install-frontend-<name>.lock`; and the download and check directories under the
installer's work directory (`<commit-prefix>.<name>.fetch`, `<commit>.<name>.check`). **Shared** by all
helpers: the audit log (append-only, safe to share) and Docker's image store. Two helpers for two
containers therefore share no record, pending marker, lock or download, and the deadline revert of
one cannot touch the other's container. The helper for the default file keeps `frontend.json`,
`state.json`, the host's install lock and the plain directory names, as before. A named helper does
**not** take the host's install lock (`/run/kbf-host-install.lock`) that the installer and the
other paths take; it serializes only with itself. The name is read when the helper starts; a file
that names another container afterwards is refused with `config_invalid` until the helper is
restarted. Give the helpers different `--socket`
paths (and different `--socket-user`s, or the same one with a socket each). Each file lists the
other frontend in `peers`.

The helper makes each container with the mounts its own file lists, and shares nothing between
the two. So each file must give its frontend its own state directory and, when
`--history-objects` is a `file://` directory, its own history directory. The file store locks
`LOCK` in that directory (`crates/kbf-objstore/src/fs_store.rs:294-304`), so a second frontend on
the same directory fails to start.

### On a Mac

The release is checked and laid out by the same code as the ssh path's install (above), into the
kit tree, with the channels of the same `host-install.toml`. The swap is the apply job's: the links,
then a restart of the daemon's job that must say it was welcomed, and the old release started again
if it does not. The pending marker is `pending.json` in the work directory, root's and 0600, and
holds the links as they were.
