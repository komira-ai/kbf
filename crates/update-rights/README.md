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
| `approval`, `cli` | The record of approved declaration digests (`/etc/updater.d/approved`) and `approve <service> <sha256>`, which `kbf-host-install approve` runs. |
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

## Tests

`tests/` runs without root. `tests/rights_root.rs` needs root and systemd and runs in the
`update-rights-linux` workflow through `cargo xtask sudo-test -p update-rights --test rights_root`:
a hook that exceeds its rights fails, and the six probes of section 11 each run as a test.
