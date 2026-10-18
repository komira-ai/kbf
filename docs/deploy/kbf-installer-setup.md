# kbf-installer setup

`kbf-installer setup` is the first configuration of a host: it makes the accounts and
directories kbf needs, writes the three config files, installs the first release, and enables
the units for the host's mode. The contract is `docs/design/kbf-installer.md` (sections 2 and
3); this page is how the verb behaves.

```text
sudo kbf-installer setup --mode=daemon \
    --frontend-url=https://farm.example:8443 --token-file=/root/kbf.token \
    --release-source=url --release-url=https://releases.example/kbf --commit=<40 hex>   # dry run
sudo kbf-installer --apply setup ...                                                    # do it
```

Without `--apply` it prints each step as `DRY-RUN: <step>` and changes nothing. With it, it
prints `+ <step>` and does the step. It never prints a token.

## Which flags a mode takes

| mode | `--frontend-url` | `--token-file` | release source | Envoy checked |
|---|---|---|---|---|
| `storage`, `daemon`, `storage-and-daemon` | required | required | `frontend` (default) or `url` | no |
| `frontend`, `all-in-one` | refused | refused | `url` only (`--release-url` required) | yes |

The modes in the first row enroll with a frontend when they start, so they need its URL and
an enrollment token. The other two do not enroll. `--commit` installs the first release before
the host has enrolled, so it needs `--release-source=url`. `--min-generation` is the operator
floor for orders; a re-run may raise it and never lowers it. A Mac is a node only
(`--mode=daemon`).

## What it does, in this order

1. Checks it is root (and, to apply, that its own binary is root's alone), checks the flags
   (every problem is named), and on a frontend or all-in-one host checks that the installed
   Envoy (`/usr/bin/envoy`) is inside the tested range. Out of range or missing is `envoy_range`,
   exit 3, with the fix; kbf never fetches Envoy.
2. The `kbf-fetch` account (`_kbf-fetch` on a Mac); for the modes that run actions, the `kbf`
   user's subid range and lingering. The `kbf` and `kbf-server` users come from the package's
   sysusers files: a host without them is refused, naming the user.
3. `/etc/kbf`, the installer tree `/var/lib/kbf-installer` (root 0755) and its `orders/`
   (0700, owned by the user the mode's unit runs as).
4. Node modes: the enrollment token from `--token-file` to `/etc/kbf/enroll.token`, mode 0600,
   owned by the service user (kbf's loader refuses a token file another user owns).
5. Frontend and all-in-one: the first operator token, 32 random bytes as hex, in
   `/etc/kbf/api-token` (0600, owned by the service user), only when that file is not there.
   The output names the file and never the secret.
6. `/etc/kbf/installer.toml` (root 0644): the release source, the floor, and for the `frontend`
   source the node certificate and key paths that enrollment writes (`pki/daemon.*` for
   `daemon` and `storage-and-daemon`, `pki/storage.*` for `storage`). Settings a flag does not
   cover are kept from the file that is there. A file that does not parse is left alone (exit 5).
7. `--commit`: the first release, through the fetch child and the verify pipeline. A build
   without them (this one) stops here with exit 1, before `kbf.toml` and the units. When a
   release is installed already, `--commit` is not fetched.
8. `/etc/kbf/kbf.toml` (root:service group, 0640): `mode`, `[frontend] url`, `[enroll]
   token_file`, and on a frontend or all-in-one host `[tls] api-token-file`. A file that holds
   other settings is kept when these keys match, and refused (exit 5, the keys named) when
   they differ. A file of only these keys is rewritten.
9. The units, last: the mode's units and `kbf-installer.service`, `systemctl enable`, with
   `--now` when a release is installed (the installer's own service does not wait for one) and
   `--no-start` is not given. A unit already enabled is left alone. Nothing is restarted.

Every step is planned only where the host differs, so the same flags again is "nothing to do".

## Exit codes

0 done; 1 failed (including a first release this build cannot fetch); 2 usage or a missing
prerequisite (not root, bad flags, bad token file, a missing user); 3 `envoy_range`; 5 only
a person can go on (a `kbf.toml` or `installer.toml` setup will not overwrite, a floor that
would go down).

## Where this differs from the contract text, and why

The contract (`docs/design/kbf-installer.md`) is normative; these are the places `setup` does
something the text does not say or says differently, so a reader can change one or the other.

- **New flags.** The contract's flag list has no way to give the frontend URL it says goes into
  `kbf.toml`, so there is `--frontend-url`. `--token-file` is added to the CLI of P1.
- **The token's owner.** The contract says 0600 root:kbf. kbf's own loaders (`kbf-enroll`'s
  `token::read`, `kbf-server`'s `ApiToken::from_file`) refuse a token file that the running
  user does not own, or whose mode is not 0600 or 0400, so root:kbf 0600 would be unreadable
  to kbf. `setup` writes both tokens 0600 owned by the user the mode's unit runs as.
- **Which user.** The contract names `kbf` for `orders/`. The package's units run the
  frontend and storage as `kbf-server`, the others as `kbf`; `orders/` and the tokens belong
  to the user of the mode's unit, so the process that writes order records (the frontend
  writes `frontend_update`) can.
- **The first operator token on all-in-one.** The contract says `--mode=frontend`. An
  all-in-one host serves the same API and has the same chicken-and-egg, so it gets one too.
- **Who enrolls.** `--token-file` is refused for `frontend` as well as `all-in-one`: the
  frontend creates its CA on first start and enrolls nothing.
- **No migration guard yet.** The contract says `setup` refuses while the old layout exists
  unmigrated. That check is `migrate::guard` of P3; the call is added when both are merged.
- **`check`** (also P10 in the contract's table) is not part of this change.
