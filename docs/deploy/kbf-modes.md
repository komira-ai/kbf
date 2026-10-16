# The modes of the one `kbf` binary: rendering and checking their units

`kbf-deploy linux render-units --mode MODE` writes `kbf.slice` and `kbf-<mode>.service`
for one mode of `kbf` (`frontend`, `storage`, `daemon`, `storage-and-daemon`,
`all-in-one`; [architecture-v2.md](../design/architecture-v2.md), sections 1 and 11).
`kbf-deploy linux check-units --mode MODE` reads them back, each with its drop-ins, and
prints one `FAIL <rule>: <reason>` line per broken rule, then any `SKIP` or `WARN` line
(neither is a failure), then the summary:

```text
kbf-deploy linux render-units --config node.conf --out units --kbf-uid 1500 --mode daemon
kbf-deploy linux check-units --config node.conf --dir units --installable --mode daemon
```

Exit codes are those of the converged verbs ([storage-node.md](storage-node.md#rendering-the-converged-units)):
0 rendered or every rule holds, 1 a rule is broken, 2 refused.

## What check-units reads

- `kbf-<mode>.service` and `kbf.slice` in `--dir`, and their drop-ins: every `*.conf`
  file in `<name>.d/` beside them, applied after the unit in file-name order, as
  systemd applies them. Each key is read as systemd reads it: a single-valued key's
  last assignment wins, and an empty assignment resets a list (`Environment=`,
  `EnvironmentFile=`, `PassEnvironment=`, the credential keys, `Delegate=`,
  `DisableControllers=`, `AllowedCPUs=`, `AllowedMemoryNodes=`).
- On the host it runs on: `/sys/devices/system/cpu/online` and
  `/sys/devices/system/node/online` (rule `cpuset`, only when a unit sets a list), the
  files the unit's `EnvironmentFile=` lines name (rule `credential`), and `kbf
  check-args` (rule `parse`). Run it on the node, as root, for all three.

It does not read drop-ins elsewhere (`/run/systemd/system`, `/usr/lib/systemd/system`,
`service.d/` and prefix drop-ins) or changes made with `systemctl set-property` or
`systemctl edit --runtime`.

## The rules

| rule | what it refuses |
|---|---|
| `render` | an `@KEY@` placeholder left; with `--installable`, a `<...>` one |
| `continuation` | a comment line inside a continued directive (below) |
| `exec` | an `ExecStart=` that is not `<KBF_BIN_DIR>/kbf <mode>` |
| `flags` | a flag on a mode not built yet (kbf takes anything there and exits 2 at start) |
| `slice` | a unit not in `kbf.slice` |
| `delegate` | a mode that runs builds without `cpu`, `cpuset`, `memory` and `pids` delegated, or with one of them in `DisableControllers=` (the unit's, or `kbf.slice`'s) |
| `cpuset` | `AllowedCPUs=` or `AllowedMemoryNodes=` on a mode that runs builds, or on `kbf.slice`, narrower than the node's CPUs or NUMA nodes |
| `credential` | the cold store's key reaching a mode without a storage half that reads it (`daemon`, `storage-and-daemon`, `all-in-one`) |
| `parse` | a built mode's flags that `kbf <mode>` refuses |

**`delegate`.** systemd reads `Delegate=` this way (systemd.resource-control(5), and
`config_parse_delegate` in systemd's `src/core/load-fragment.c`): a boolean delegates
every controller (`yes`) or none (`no`); a list adds its controllers to the ones
already delegated; an empty value turns delegation on with no controller. So
`Delegate=yes` followed by `Delegate=memory` still delegates every controller, cpuset
among them, and the rule passes it. A slice cannot delegate: systemd enables on
`kbf.slice` every controller a unit under it delegates, unless the slice's
`DisableControllers=` lists it. The slice template sets only `MemoryMin=`.

**`cpuset`.** The daemon places leases on every CPU and NUMA node of the node
(`--lease-placement=numa-node` pins each lease to one NUMA node's CPUs and memory). A
list narrower than the node keeps the unit's cgroup, and every lease in it, off the
rest. A list is read as systemd reads it (`config_parse_cpu_set` in
`src/shared/cpu-set-util.c`): words split at spaces, tabs, line breaks and commas by
`extract_first_word` with `EXTRACT_UNQUOTE` only, so quotes are taken off
(`AllowedCPUs="0-1"` is the CPUs 0 and 1) and a `\` keeps the next character, with no
C escapes; an unclosed quote drops the whole assignment; each word a number or range
as `parse_range` and `safe_atou` read it (`0x`, `0o`, `0b` and a leading `0` pick the
base), a malformed one ignored. A `%` specifier is not expanded: a `WARN cpuset:` line
says the value was read as written. When the node's own sets cannot be read
(check-units run elsewhere), a set list is not compared and a `WARN cpuset:` line says
so. `StartupAllowedCPUs=` and `StartupAllowedMemoryNodes=` (boot and shutdown only)
are not read.

**`continuation`.** systemd.syntax(7): "When a comment line or lines follow a line
ending with a backslash, the comment block is ignored, so the continued line is
concatenated with whatever follows the comment block." That holds from systemd 241.
systemd 240 skips such a comment only when the `#` starts the line, and 239 and
older join the comment into the value (`config_parse` in `src/shared/conf-parser.c`
before the change that fixed systemd issue 10598), so its words become arguments of an
`ExecStart=`. The templates put every comment above its directive, and the rule refuses
one inside.

**`credential`.** It refuses, in `[Service]`:

- `EnvironmentFile=` naming the key file: its path, with `-`, quotes, `.`, `..` and
  repeated `/` taken off, is `<KBF_ETC>/s3.env` or ends in `/s3.env`, or it is a
  wildcard that matches `<KBF_ETC>/s3.env`. systemd expands every path with glibc's
  `glob(3)` (`safe_glob` in `src/basic/glob-util.c`, no flags, in PID 1's C locale),
  and the rule reads it the same way: `*`, `?`, `[...]` with ranges, `!` or `^`,
  `[:alpha:]` and the other C-locale classes, `[=c=]` and `[.c.]`, a leading `.`
  matched only by a `.`, and `\` escapes, taken off a path with no wildcard too
  (`/etc/kbf/\s3.env` is the key file). Braces and `~` are plain characters (systemd
  passes neither `GLOB_BRACE` nor `GLOB_TILDE`), and a `..` after a wildcard is read
  by its words, as if the wildcard matched some directory;
- `Environment=` setting any `AWS_*` variable, quoted or not, alone on the line or not:
  the key pair, `AWS_SESSION_TOKEN`, and `AWS_PROFILE`, `AWS_SHARED_CREDENTIALS_FILE`
  and `AWS_CONFIG_FILE` (which point an SDK at a key anywhere). A build node talks to no
  AWS-style store, so no `AWS_*` variable has a use there;
- `PassEnvironment=` naming an `AWS_*` variable;
- any `LoadCredential=`, `LoadCredentialEncrypted=`, `SetCredential=`,
  `SetCredentialEncrypted=` or `ImportCredential=` (opaque to the rule; a build node's
  units need none);
- on the node, an `EnvironmentFile=` whose file sets an `AWS_*` variable (a copy or a
  hard link of the key under another name), or that is a symbolic link (not followed).

What it cannot catch: offline, a file under another name that holds the key or links
to it, and a wildcard elsewhere that matches a copy named `s3.env`; on the node, the
files a wildcard `EnvironmentFile=` matches and a file it cannot read (each a
`SKIP credential:` line); `%` specifiers (not expanded); `UnsetEnvironment=` (not read,
so a variable it unsets still fails the rule); and the key reaching the node any other
way: a file the daemon reads by a flag, the service manager's own environment without
`PassEnvironment=`, or a key file other users can read.

## all-in-one is for a trusted box

`kbf all-in-one` runs builds in the process that holds the farm's state and its cold
store, as one user. The container driver runs each action as a subordinate id of that
user (`--userns=nomap`), but no test yet proves an action cannot read what the user
holds (architecture-v2.md, section 9). Until one does, the unit says so in a comment
and every `check-units --mode all-in-one` prints:

```text
WARN trusted: kbf all-in-one runs builds beside the farm's state and its cold store, as one user; the container driver runs each action as a subordinate id of that user (--userns=nomap), but no test yet proves an action cannot read what the user holds: use it on a trusted single box or for development only
```

Its cold store is `file://<KBF_DATA_DIR>/cold`, the single box's shape (section 11), so
the unit holds no cold-store key and rule `credential` applies to it. Its scheduler state is the SQLite
file `<KBF_DATA_DIR>/state.db`, the default of `kbf all-in-one` (a `--state-store=sqlite:///...`
names another; `postgres://` is refused), so the unit gives no state store flag.

## Upgrade kbf before kbf-deploy

Rule `parse` runs `<kbf> check-args <mode> <flags...>`. A `kbf` older than
`check-args` refuses the word, so `check-units --mode` fails a built mode's unit with:

```text
FAIL parse: kbf daemon refuses the flags of kbf-daemon.service (/usr/local/bin/kbf check-args: exit 2): error: unrecognized subcommand 'check-args'
```

(`all-in-one` the same, with its name.) Install the new `kbf` before running the new
`kbf-deploy` on a node, or check offline: on a machine with no `kbf` at the unit's
path and no `--kbf-bin`, the rule is skipped with a `SKIP parse:` line.
