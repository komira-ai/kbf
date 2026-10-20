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

sudo kbf-installer setup --mode=frontend --release-url=https://releases.example/kbf \
    --database='postgres:///kbf?host=/run/postgresql'
```

Without `--apply` it prints each step as `DRY-RUN: <step>` and changes nothing. With it, it
prints `+ <step>` and does the step. It never prints a token.

## Which flags a mode takes

| mode | `--frontend-url` | `--token-file` | `--database` | release source |
|---|---|---|---|---|
| `storage`, `daemon`, `storage-and-daemon` | required | required | refused | `frontend` (default) or `url` |
| `frontend`, `all-in-one` | refused | refused | required | `url` only (`--release-url` required) |

The modes in the first row enroll with a frontend when they start, so they need its URL and
an enrollment token. The other two do not enroll; they keep their state in Postgres. The
server, the database and the role exist before `setup` runs (the operator provides them);
`--database` says where, as `postgres:///NAME?host=SOCKET_DIR`. A URL with a password is
refused (`installer.toml` is world-readable), and no message quotes a URL. `--commit` installs
the first release before the host has enrolled, so it needs `--release-source=url`.
`--min-generation` is the operator floor for orders; a re-run may raise it and never lowers it.
`--installer-config` is a flag of `run`; `setup` writes `/etc/kbf/installer.toml`, where the
installer's unit reads it, and refuses another path. A Mac is a node only (`--mode=daemon`).

## What it does, in this order

1. Checks it is root (and, to apply, that its own binary is root's alone), checks the flags
   (every problem is named), and refuses (exit 5) while the old layout of the retired
   `kbf-helper` and `kbf-deploy host` is on the host (`/usr/local/kbf` or
   `/var/lib/kbf-deploy`): the two never both manage a host. The refusal names the command
   that is the way out once `kbf-installer migrate` exists (komira-ai/kbf#1083):
   `kbf-installer migrate --apply --fetch-user kbf-fetch [--unit UNIT]` (`_kbf-fetch` on a
   Mac). Today `migrate` is not on this build and `setup` refuses on either path whatever
   was migrated, so the way out today is by hand: stop the host's units and move both roots
   aside. The target contract is different (see "The old layout" below): `setup` will accept a
   host exactly when `migrate` has finished it (its journal says `Done`), not by path.
   On Linux it also refuses (exit 5, `unit_shadow`) when a regular file at
   `/etc/systemd/system/<the mode's unit>` (for a frontend, also `kbf-frontend@.service`)
   would shadow the packaged unit: systemd reads `/etc` before `/usr/lib`, and `setup`
   does not enable a unit that is already enabled, so the host would keep the old flags.
   Move the file aside (`mv FILE FILE.old; systemctl daemon-reload`) and run `setup` again;
   `setup` never deletes or rewrites a unit. A link there (`systemctl link`) is not refused.
   Envoy is not checked: the proxy bundled in `kbf-installer` owns a frontend's public port,
   and a load balancer in front of it is the operator's.
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
6. `/etc/kbf/installer.toml` (root 0644): the release source, the floor, for the `frontend`
   source the node certificate and key paths that enrollment writes (`pki/daemon.*` for
   `daemon` and `storage-and-daemon`, `pki/storage.*` for `storage`), and for the Postgres
   modes `database`. Settings a flag does not cover are kept from the file that is there. A
   file that does not parse (it is closed: an unknown key, a wrong type or a value out of
   bounds is refused) is left alone (exit 5). The release source is read only from this file.
7. Frontend and all-in-one: the state database's schema is created, or brought forward, by the
   installer's own migrations; a dry run asks the server and says what it would do. `setup`
   connects as the user running it (root) unless the URL names a role with `user=`, so the
   server must let that role in from root (peer authentication through a `pg_ident` map, or a
   local `trust` rule). A server that cannot be reached stops `setup` here (exit 1), with
   `installer.toml` written and no release, `kbf.toml` or unit; running it again finishes.
8. `--commit`: the first release, through the installer's `Fetcher` (the fetch child and the
   verify pipeline) and nothing else, then made `releases/current` with no trial. This build
   has no fetcher wired in, so it stops here with exit 1, before `kbf.toml` and the units.
   When a release is installed already, `--commit` is not fetched.
9. `/etc/kbf/kbf.toml` (root:service group, 0640): `mode`, `[frontend] url`, `[enroll]
   token_file`, on a frontend or all-in-one host `[tls] api-token-file` and `[scheduler]
   state-store` (the database URL). A file that holds other settings is kept when these keys
   match, and refused (exit 5, the keys named) when they differ. A file of only these keys is
   rewritten.
10. The units, last: the mode's unit and `kbf-installer.service`, `systemctl enable`, with
   `--now` for a node's unit when a release is installed and `--no-start` is not given (the
   installer's own service does not wait for either). A unit that makes a frontend
   (`kbf-frontend@a.service`, `kbf-all-in-one.service`) is enabled and never started by
   `setup`: starting it takes the database, the TLS files and the port. A unit already
   enabled is left alone. Nothing is restarted.

Every step is planned only where the host differs, so the same flags again is "nothing to do"
(the database step reports the schema as current). A run that stopped part way is finished by
running it again.

**The re-run rule.** `setup` keeps no state of its own: every run reads the host and plans the
difference. So a run that failed at any step (a test plants a failure at each step of each Linux
mode) is finished by the same command, and the host then equals the one a clean run leaves. The
order makes the half-done host safe: `kbf.toml` and the units come last, so a stopped run leaves
no config that a unit would start from. Changed flags rewrite the files they own atomically and
never restart anything; a floor only goes up; a file `setup` cannot safely rewrite stops it
with exit 5.

**Proxy units.** The bundled proxy's units (`kbf-proxy.service` and `.socket`) are not on
`feature/arch-v2` yet (kbf#1153), so a frontend or all-in-one host enables the units that exist
(`mode_units` in `src/setup/plan.rs` is the one place to change when they land).

## Exit codes

0 done; 1 failed (including a database that cannot be reached and a first release this build
cannot fetch); 2 usage or a missing prerequisite (not root, bad flags, bad token file, a
missing user); 3 a release the fetcher rejected; 5 only a person can go on (a `kbf.toml` or
`installer.toml` setup will not overwrite, a floor that would go down, the old layout).

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
- **The old layout.** The contract says `setup` refuses while the old state exists unmigrated.
  Today the migration (P3, komira-ai/kbf#1083) is not on this build, so `setup` refuses while
  either old root is there, even on a host that was migrated (a migrated host keeps both roots
  as its rollback path); it cannot yet set up a host that ran the retired helper. The target:
  `setup` accepts the old layout if and only if `migrate` finished the host (its journal
  phase is `Done`), and otherwise refuses with the `migrate` command; the path check goes
  when that lands.
- **A unit file in `/etc/systemd/system` with a packaged unit's name.** Not in the contract.
  A farm node running the dev installer has `kbf-storage-and-daemon.service` there; `setup`
  refuses (exit 5, `unit_shadow`) rather than enable nothing and leave the old flags running.
  Retiring such a file (after `migrate`) waits for #1083.
- **No Envoy check, no Envoy unit.** The contract has `setup` test the installed Envoy against
  a tested range. The ruling since is that kbf manages no Envoy: the bundled proxy owns the
  frontend's port. `setup` neither checks Envoy nor enables `kbf-envoy.service`.
- **The operator token's path.** `frontend-proxy.md` names `/etc/kbf/operator.token`
  (root:kbf 0640, `operator_token_file` in `installer.toml`); the server reads the token from
  `[tls] api-token-file` today, so `setup` writes `/etc/kbf/api-token` and `kbf.toml` names
  it. When the proxy reads its own file, the path and the key move with it.
- **`check`** (also P10 in the contract's table) is not part of this change.
