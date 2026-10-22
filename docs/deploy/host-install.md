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
root and not writable by its group or others, or the program refuses to run.

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

The path of this file is a constant of the program. No flag and no environment variable
changes it.

## What an install does

0. Takes a non-blocking `flock` on `/run/kbf-host-install.lock` for the whole install; if
   another install holds it, answers `failed` with reason `busy` and runs nothing.
1. Reads the installed version with `dpkg-query`. A target older than the installed version
   is refused; the same version is a no-op that reports `already_installed`.
2. Runs `apt-get update` restricted to the channel's source file
   (`-o Dir::Etc::SourceList=<file> -o Dir::Etc::SourceParts=-`), then
   `apt-get install -y --no-install-recommends -o Dpkg::Options::=--force-confold
   -o DPkg::Lock::Timeout=300 -t <suite> kbf=<version>` with the same restriction. Each
   runs in a transient systemd unit (`systemd-run --wait --collect --pipe --quiet
   --unit=kbf-host-install-<version>.service`), so a dropped ssh connection does not stop `dpkg`
   half way.
3. Reads the installed version again and reports `installed` only if it is the target.

No shell runs anywhere: every process is started with an explicit argument vector and an
environment of `PATH` and `LANG=C` alone.

## The answer

One JSON line on stdout.

| request | line | exit |
|---|---|---|
| install, done | `{"result":"installed","installed_version":"...","unit_state":"finished"}` | 0 |
| install, same version | `{"result":"already_installed","installed_version":"...","unit_state":"not_run"}` | 0 |
| query | `{"installed_version":"..."\|null,"running_commit":"..."\|null,"in_progress":true\|false}` | 0 |
| refused | `{"result":"refused","reason":"<code>"}` | 2 |
| failed | `{"result":"failed","reason":"<code>"}`, detail on stderr | 3 |

Refusal codes: `bad_command`, `channel_not_allowed`, `config_invalid`, `config_unsafe`,
`channel_unsigned`, `key_untrusted`, `older_than_installed`. Failure codes:
`audit_unavailable`, `query_failed`, `installed_version_invalid`, `package_state_unclean`,
`update_failed`, `install_failed`, `verify_failed`, `busy`, `lock_unavailable`.

`running_commit` is the commit `kbf --version --json` of the installed binary reports, or
null. `in_progress` is true while a `kbf-host-install-*` unit is active or while some
process holds the dpkg frontend lock (read from `/proc/locks`; nothing is locked or waited
for).

## The audit log

Every invocation, refused ones included, appends one JSON line to
`/var/log/kbf-host-install.log` (created root-owned, mode 0600, never through a symbolic
link): the time, the verb, the version, the channel, the outcome and the reason. A request
that was not understood is recorded escaped and cut at 200 characters. An install is not
started when the log cannot be opened.
