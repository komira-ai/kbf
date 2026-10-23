# kbf-host-install: the root side of a host-driven update

`kbf-host-install` is the program a host runs, as root, when the updater asks it to install
a version of the `kbf` package. It is not a command anyone types: sshd runs it as the forced
command of one key, and it does only what the allowlist on that host permits.

## The key

The updater's ssh key is installed in root's `authorized_keys` as

```text
from="203.0.113.10",restrict,command="/usr/libexec/kbf/kbf-host-install" ssh-ed25519 AAAA...
```

`from=` is the address the updater connects from. `restrict` turns off forwarding, a
terminal and the rest. `command=` runs this program whatever the client asked for; the
client's request arrives only in `SSH_ORIGINAL_COMMAND`. The program reads nothing from its
arguments or standard input.

One more entry point exists, and it is not reachable over ssh: an install runs this same
program again inside a systemd unit (see below), with the request in the variable
`KBF_HOST_INSTALL_UNIT`, which only `systemd-run` sets. The variable counts only when none of
`SSH_ORIGINAL_COMMAND`, `SSH_CONNECTION`, `SSH_CLIENT` and `SSH_TTY` is set; in an ssh session it
is ignored, and the program handles `SSH_ORIGINAL_COMMAND` as usual.

## The two requests

The whole string must be one of these, with single spaces and nothing before or after:

```text
install kbf=<version> from <channel>
query
```

- `<version>` is `[0-9][0-9A-Za-z.+~-]*`, at most 128 bytes, without an epoch (no `:`), and a
  version `dpkg` itself accepts (so `1.0-` is refused).
- `<channel>` is `[a-z0-9][a-z0-9-]{0,31}`.
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

## The answer

One JSON line on stdout.

| request | line | exit |
|---|---|---|
| install, done | `{"result":"installed","installed_version":"...","unit_state":"finished"}` | 0 |
| install, same version | `{"result":"already_installed","installed_version":"...","unit_state":"not_run"}` | 0 |
| query | `{"result":"query","installed_version":"..."\|null,"running_commit":"..."\|null,"node_id":"..."\|null,"in_progress":true\|false}` | 0 |
| refused | `{"result":"refused","reason":"<code>"}` | 2 |
| failed | `{"result":"failed","reason":"<code>"}`, detail on stderr | 3 |

Refusal codes: `bad_command`, `channel_not_allowed`, `config_invalid`, `config_unsafe`,
`channel_unsigned`, `key_untrusted`, `channel_insecure`, and, on a Mac, for a channel whose source
is GitHub, `unknown_release`, `ambiguous_release` and `fetch_failed`. Failure codes:
`audit_unavailable`, `query_failed`, `installed_version_invalid`, `package_state_unclean`,
`update_failed`, `install_failed`, `verify_failed`, `busy`, `lock_unavailable`.

Every line has a `result`: `installed`, `already_installed`, `query`, `refused` or `failed`.

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
gap between the two apt steps is covered); a `kbf-host-install-*` unit is listed by
`systemctl list-units` in the state `active`, `activating`, `deactivating` or `reloading`; or
some process holds the dpkg frontend lock (read from `/proc/locks`; nothing is locked or
waited for). If the listing itself fails, the query fails (`query_failed`) and does not
guess.

## The audit log

Every invocation, refused ones included, appends one JSON line to
`/var/log/kbf-host-install.log` (created root-owned, mode 0600, never through a symbolic
link): the time, the verb, the version, the channel, the outcome and the reason. On a Mac the
apply job appends one more line when it ends: verb `apply`, the version, outcome `applied` or
`failed`, and `result`, the status line (`done`, `failed:not_welcomed:rolled_back`, ...). A request
that was not understood is recorded escaped and cut at 200 characters. An install is not
started when the log cannot be opened.

## On a Mac

A Mac takes the same two requests (`install kbf=<version> from <channel>` and `query`) and
answers in the same shape, but it installs the attested release tarball
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

1. The binary is `/usr/local/libexec/kbf/kbf-host-install`, `root:wheel` 0755, in directories
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
   token_file = "/usr/local/etc/kbf/host-install/github.token"   # optional; see below
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
   for releases attested under it), and GitHub-hosted runners only;
5. the tarball holds exactly one regular file named `kbf`, a Mach-O file for arm64;
6. the binary carries no `com.apple.quarantine` attribute (it is removed, and proven gone);
7. `codesign --verify --strict` accepts it and its signature has the hardened runtime;
8. the new binary, run as `_kbf`, accepts the daemon's live configuration
   (`kbf --mode=daemon --config /usr/local/etc/kbf/kbf.toml --check`);
9. the staged binary is root's, in directories only root can write.

Steps 4 and 8 start their program through `/usr/sbin/chroot -u <uid> -g <gid> -G <gid> / ...`:
started by `Command::uid` and `gid` alone, a child on macOS keeps the old effective gid (group 0,
`wheel`) as a supplementary group, and `chroot` sets the one group itself. The real-Mac test
checks that the child's groups are its own gid and nothing else.

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
`komira-ai/kbf`, and then handles the two files exactly as it handles the files of a `dir`
(copied once into the stage, `sums` checked when set, then the offline attestation check and
everything after it). The build attestation is still the only trust anchor; the listing and the
download only bring the bytes. A directory channel stays for a Mac that cannot reach GitHub.

What the install does, in this order, with the pinned `gh`, run as `nobody` (the same privilege
drop as the attestation check) in a directory that `nobody` owns inside the stage, with its
own `HOME`:

1. lists the repository's releases with `gh api`, 100 a page, newest first, page after page until
   a page is not full, and at most 30 pages; never only the latest, so a rollback to an old release
   works;
2. finds the asset named `kbf-<40 hex>-darwin-arm64.tar.gz` whose commit starts with the 12 hex
   digits of the version, across all the pages read. None is `unknown_release`; assets of two
   different commits are `ambiguous_release`; the same commit in two releases is one release (the
   newest). A listing cut at the 30-page limit without a match is `fetch_failed`, not
   `unknown_release`, since not all of it was seen. When the channel has `sums`, the commit is
   the one the sums list, and the listing must hold it;
3. builds the download itself from the release's tag and the two asset names:
   `gh release download <tag> --repo komira-ai/kbf --pattern <tarball> --pattern <bundle> --dir <dir>`.
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
- A token for the API. `gh api` refuses to run without one (`gh release download` does not), so
  a channel names a `token_file`: a file that is root's alone to write (checked like the allowlist)
  holding a GitHub token, one line of letters, digits, `_` and `-`. It needs no scope: the repository is
  public and the token only has to exist. The root process reads it and gives it to `gh` as `GH_TOKEN`
  for the listing and the download, and never to the attestation check. Without one the listing
  fails and the install is `fetch_failed`.

#### The residuals

- The token is a credential on the Mac. It can read public data only, but it is an account's token,
  and `nobody`'s `gh` process holds it in its environment while it runs.
- The listing is read through GitHub's API and is not itself trusted: it can only choose among
  releases (an old attested release is installable, as with a directory) or withhold one (the
  install fails). What it picks must still pass the attestation for the full commit.
- A request costs one API call per 100 releases plus the download's, against GitHub's rate limit
  for the token (or the address, without one). A listing longer than 3000 releases does not show
  its oldest releases.
- The size limit is watched about every 10 ms, so a fast download can write a little past it
  before it is killed; the copy then refuses the file, and the stage is removed.
- A channel with `sums` pins the commit and the tarball's hash, and needs an edit of the sums file
  for each new release; the point of this source is a channel without them.
