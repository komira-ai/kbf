# update-rights

What the host updater lets a hook do. Why it is shaped this way:
[docs/design/host-updates.md](../../docs/design/host-updates.md) (sections 2.4, 3.3, 3.4 and 3.4.1).
The crate names no product and depends on no crate of this workspace but the hook contract
(`update-hook`); `crates/kbf-integration-test/tests/layering.rs` keeps both true.

| module | what it holds |
|---|---|
| `declaration` | `/etc/updater.d/<service>.toml`, typed. Every table refuses a key it does not know. `Declaration::parse` applies the rules of the text alone. |
| `load` | `load_dir`: the directory and each file are owned by the trusted account (root) and not writable by the group or others; the accounts exist; `hook.user` is not the service's account unless root; paths are checked again on what they point to; the record directory is not writable by the service; no two declarations share an account, a unit, a health port or a record directory. A declaration that fails refuses that one service. |
| `names`, `reserved` | The grammars (no `-`, `+`, `!` prefix, no `..`, no white space) and the reserved-path deny list. |
| `widening` | `widening(old, new)`: the fields that ask for more. Any change other than removing a set element or lowering a number. |
| `approval`, `cli` | The record of approved declaration digests (`/etc/updater.d/approved`) and `approve <service> <sha256>`, which the root part of the updater runs as `approve`. |
| `gate` | `gate(active, incoming, digest, approvals)`: install, or `Deferred` with the reason `rights_change`. |
| `launch` | The `systemd-run` command of one step as a pure function (`command`, `properties`), `run_step` over a `Spawner`, and `Systemd`, the real one. |

## Choices the design left open

- **Both parties of a conflict are refused.** Neither has the better claim, and the order the
  directory lists them in must not decide.
- **A first install is deferred until approved**, whoever the hook runs as: with no active
  declaration there is nothing to be narrower than.
- **The record directory check** looks at the directory and every directory above it that
  exists, by owner, group (primary and listed) and others; a root service is not checked.
- **`ReadOnlyPaths=` does not hide anything.** `ProtectSystem=strict` leaves the whole tree
  readable, so `read_paths` says what the hook is declared to read and confines nothing.
- **An empty address family list is `RestrictAddressFamilies=` with an empty value.** Through
  `systemd-run -p` that is an empty allow list (every family refused); the root test opens a unix
  socket under it and must fail. `none` is refused by systemd as an unknown family.
- **A reserved directory may hold a read-only entry**, but a write path or a record directory
  inside one is refused, except under `/var/lib/updater/hooks`, where the records live.
- Account names come from `/etc/passwd` and `/etc/group` only, not from NSS.

- **String parameters.** `[hook.params]` takes numbers, booleans and strings, except for a hook
  that runs as root: the design allows a string only for a key a schema lists, and none exists,
  so a root hook is refused a string. A float parameter that is `nan` is never a lowered number.
- **The targets are checked again.** `recheck_targets` repeats the reserved-path rules on what each
  entry points to now; `run_step` calls it before every launch (`Declared::recheck` for a loaded one) and refuses the launch (`RunError::Recheck`)
  when a target changed, because a link can be re-pointed after the load. `check_state_dir` is the same for the record directory.
- **The deny list** also holds `/run/systemd/{transient,generator*,system.control}` and
  `/var/lib/polkit-1`; `/proc`, `/sys` and `/dev` may be read and never written.
- **Hook accounts.** A hook may not run as another declared service's account, and two
  declarations may not share a hook account; root is exempt from both, as it is from the shared
  service account rule.
- **Approving takes a lock** (`approved.lock` beside the record, flock) so two approvals at once
  do not lose one, and the directory of the record must be the trusted account's and closed.

- **Write paths are an allow-list.** A `write_paths` entry must lie one component under `/var/lib`,
  `/var/cache`, `/run` or `/opt` (and anything below); the deny list still catches the reserved
  names inside those roots (`/var/lib/dpkg`, `/var/lib/polkit-1`, `/var/cache/apt`, `/var/spool/cron`),
  and `/bin`, `/sbin`, `/lib`, `/lib64` are named for the hosts without a merged `/usr`. `state_dir`
  and `read_paths` are not held to the list.
- **A root hook's paths cannot be swapped.** `ReadWritePaths=` follows links, so for a hook that
  runs as root every existing directory above a read or write path (and above what it resolves to)
  must be owned by root and closed to the group and others; `recheck_targets` repeats it before
  each launch. "Root" is the account that resolves to uid 0 (`Launch::root_hook`, derived from the uid in `Launch::new`), whatever its name.
- **A root hook writes in its own directory.** For a hook that runs as root each write path is at or
  below `<write root>/<service>` or a name beginning with the service's and a `-`
  (`/var/lib/svc`, `/var/lib/svc-state`), so the one-component rule cannot hand it `/var/lib/sudo` or
  `/run/udev`.
- **The dash rule stops at another service.** A root hook's directory `<root>/<service>-…` is
  refused when it begins with a longer declared name (`tierstore` may not write `/var/lib/tierstore-b/x`
  if `tierstore-b` is a service of the host), across declarations like the conflicts. A service
  may not be named for a directory of the system (`names::SYSTEM_NAMES`), and the reserved list is the
  last word on `/var/lib/sudo`, `/run/udev`, `/run/tmpfiles.d`, `/run/systemd/network`,
  `/var/lib/apt` and `/var/lib/cloud`.
- **Root is derived once.** `Launch::new` takes the hook's uid and `Launch::of` a loaded declaration;
  the flag is private, `Declared::recheck` supplies the uid to `recheck_targets`, and nothing a
  caller passes can turn the root rules off.
- **Environment names.** `LD_*`, `MALLOC_*`, `PYTHON*`, `DYLD_*`, `GIT_*` and a list of loader, resolver, shell
  and interpreter names (`GCONV_PATH`, `BASH_ENV`, `IFS`, `PERL5OPT`, `RUBYOPT`, `NODE_OPTIONS`, `GLIBC_TUNABLES`, `OPENSSL_CONF`, `GIT_*`, and the like;
  `names::HIJACKING_ENV`) are refused for every hook, and `PATH` only as a list of absolute
  directories.

## What polkit can and cannot scope

A rule for the `manage-units` action limits an account to the units it lists. But `systemctl mask`
needs `manage-unit-files`, whose policy implies `manage-units` for every unit, so an account that
holds it can manage any unit. Hooks therefore never mask, and the launcher never grants
`manage-unit-files` (a test holds that); if anything generates polkit rules from a declaration, it
must not grant it either. The root tests assert both halves through `pkcheck`.

## Tests

`tests/` runs without root. `tests/rights_root.rs` needs root and systemd and runs in the
`update-rights-linux` workflow through `cargo xtask sudo-test -p update-rights --test rights_root`:
a hook that exceeds its rights fails, and the six probes of section 11 each run as a test.
