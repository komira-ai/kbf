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
1. Reads the installed version with `dpkg-query`. A target older than the installed version
   is refused; the same version is a no-op that reports `already_installed`. A package dpkg
   flags with an error (the third character of its status, `R` for reinstall required) is not
   cleanly installed: an install of the same or a newer version goes ahead (apt repairs it),
   an older one is refused as ever.
2. Runs `apt-get update` restricted to the channel's source file
   (`-o Dir::Etc::SourceList=<file> -o Dir::Etc::SourceParts=-`), then
   `apt-get install -y --no-install-recommends -o Dpkg::Options::=--force-confold
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
| query | `{"result":"query","installed_version":"..."\|null,"running_commit":"..."\|null,"in_progress":true\|false}` | 0 |
| refused | `{"result":"refused","reason":"<code>"}` | 2 |
| failed | `{"result":"failed","reason":"<code>"}`, detail on stderr | 3 |

Refusal codes: `bad_command`, `channel_not_allowed`, `config_invalid`, `config_unsafe`,
`channel_unsigned`, `key_untrusted`, `channel_insecure`, `older_than_installed`. Failure codes:
`audit_unavailable`, `query_failed`, `installed_version_invalid`, `package_state_unclean`,
`update_failed`, `install_failed`, `verify_failed`, `busy`, `lock_unavailable`.

Every line has a `result`: `installed`, `already_installed`, `query`, `refused` or `failed`.

`installed_version` of a query is the version dpkg reports when `kbf` is cleanly installed,
and null when it is not installed or dpkg flags an error on the package (the updater then
installs, which repairs it). `running_commit` is the commit `kbf --version --json` of the
installed binary reports, or null; the binary is killed after 10 seconds, and a binary that
fails, hangs or prints something else has none.

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
link): the time, the verb, the version, the channel, the outcome and the reason. A request
that was not understood is recorded escaped and cut at 200 characters. An install is not
started when the log cannot be opened.
