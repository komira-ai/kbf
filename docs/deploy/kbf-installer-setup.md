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
   `/var/lib/kbf-deploy`): the two never both manage a host, and the migration is not built.
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
   enabled is left alone. Nothing is restarted. Those two modes also get `kbf-backup.timer`,
   enabled and started (the schema it dumps was made in step 7; see "Backup and restore").

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

## Backup and restore

A frontend or all-in-one host keeps all of kbf's state in its Postgres database, so that
database is backed up. `kbf-backup.timer` starts `kbf-backup.service` every day (a host that
was down then runs it when it starts, and each run is delayed by up to an hour), and the
service runs `kbf-installer --apply backup` as `kbf-server`. `setup` enables the timer on the
two modes; the units are in the package like the others (`packaging/linux/units`).

**What the backup is.** `pg_dump` in custom format of the database `installer.toml` names, in
`/var/lib/kbf-server/backups/kbf-<UTC time>.dump` (for example `kbf-20270115T080000Z.dump`).
The directory is 0700 and each file 0600, owned by `kbf-server`: a dump holds the whole
state, tokens and member map included. The role is the OS user running the unit: with peer
authentication `kbf-server` connects as the Postgres role `kbf-server`, the one that owns the
database and that the frontend connects as, and no password exists anywhere.

**What one run does, in order.** It makes (or checks) the directory and closes it to other
users; removes the temp file of an earlier run that crashed; has `pg_dump` write a temp file
whose name is not a backup's; checks that the file is not empty and that `pg_restore --list`
can read it; syncs it and renames it to its name (a backup is a whole file or it is not
there); and only then removes the oldest backups beyond the newest `backup_keep`. Any failure
ends the run with exit 1, removes its temp file, leaves every older backup alone, and the
unit is `failed`: `systemctl status kbf-backup.service`, `journalctl -u kbf-backup.service`.
A second run in the same second refuses rather than replace a backup. Without `--apply` the
verb only says what it would do.

**How many.** `backup_keep` in `/etc/kbf/installer.toml`, 1 to 365, default 7 when the key is
not there (one a day is a week). `setup` keeps the key when it rewrites the file; the file is
closed, so a value out of range is refused by `check`, `run` and `backup` alike. The backups
are on the host's own disk, next to the database they protect: a copy to another machine is
the operator's (kbf does not push anywhere), and a disk lost takes both.

**Prerequisites.** `pg_dump` and `pg_restore` of the same major version as the server, or a
newer one (the `postgresql-client` package; a client older than the server refuses to dump).
The package does not depend on it yet. Both are found by name in `PATH`; `--pg-dump` and
`--pg-restore` name other paths.

**The first backup, and its restore test.** The design requires both before the farm depends
on the database. On a new host, after `setup`, run one backup now and restore it into a
scratch database, which touches nothing live:

```text
sudo systemctl start kbf-backup.service        # a oneshot: returns when the backup is done
sudo ls -l /var/lib/kbf-server/backups         # kbf-<time>.dump, mode -rw-------
sudo -u postgres createdb --owner=kbf-server kbf_restore_test
sudo -u kbf-server kbf-installer --apply restore \
    --file /var/lib/kbf-server/backups/kbf-<time>.dump \
    --database 'postgres:///kbf_restore_test?host=/run/postgresql'
sudo -u kbf-server psql 'postgres:///kbf_restore_test?host=/run/postgresql' -c '\dt kbf_state.*'
sudo -u postgres dropdb kbf_restore_test
```

`restore` runs `pg_restore` in one transaction that stops at the first error, taking no owner
or privilege from the dump (the connecting role owns what it makes, as `setup` made it). The
database must hold no kbf schema: a restore into one that does fails and changes nothing.
Without `--database` it restores into the database `installer.toml` names.

**Restoring the real state** (the database is lost or damaged). The state is what the backup
held: builds in flight are gone, and nodes register again.

```text
sudo systemctl stop kbf-frontend@a.service     # waits for open builds; nothing may write
sudo -u postgres dropdb kbf
sudo -u postgres createdb --owner=kbf-server kbf
sudo -u kbf-server kbf-installer --apply restore --file /var/lib/kbf-server/backups/kbf-<time>.dump
sudo systemctl start kbf-frontend@a.service    # opens the store: its schema check must accept it
```

The tests: `src/backup_tests.rs` (retention, atomicity, modes and the arguments, without a
server) and, in the state-postgres CI job, `real_server_round_trip`: schema made as `setup`
makes it, rows written through the state store, a backup, a restore into a second database as
a role that is not a superuser, and the store opened on it reading every row back.

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
  The migration (P3) is not built, so `setup` refuses while either old root is there; it
  cannot yet set up a host that ran the retired helper.
- **No Envoy check, no Envoy unit.** The contract has `setup` test the installed Envoy against
  a tested range. The ruling since is that kbf manages no Envoy: the bundled proxy owns the
  frontend's port. `setup` neither checks Envoy nor enables `kbf-envoy.service`.
- **The operator token's path.** `frontend-proxy.md` names `/etc/kbf/operator.token`
  (root:kbf 0640, `operator_token_file` in `installer.toml`); the server reads the token from
  `[tls] api-token-file` today, so `setup` writes `/etc/kbf/api-token` and `kbf.toml` names
  it. When the proxy reads its own file, the path and the key move with it.
- **`check`** (also P10 in the contract's table) is not part of this change.
