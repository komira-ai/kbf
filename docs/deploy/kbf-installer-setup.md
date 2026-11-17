# kbf-installer setup

`kbf-installer setup` is the first configuration of a host. It makes the accounts and
directories kbf needs, writes `installer.toml` and `kbf.toml`, and enables the units for the
host's mode. It installs no release: kbf reaches a host as a package. The contract is
[the installer design](../design/kbf-installer.md) (sections 2 and 3); this page is how the verb
behaves in the code it describes. A host that has the old layout is not adopted by the installer: adoption = install the kbf and
kbf-proxy packages (package-update design). Setup is Linux only: a Mac runs no kbf, and its
actions run as js-agent jobs.

```text
sudo kbf-installer setup --mode=storage \
    --frontend-url=https://frontend.example.invalid:8443 --token-file=/srv/kbf/enroll.token   # dry run
sudo kbf-installer --apply setup ...                                                          # do it

sudo kbf-installer setup --mode=frontend \
    --listen=192.0.2.10:8980 --worker-listen=192.0.2.10:8981 \
    --worker-tls-cert=/etc/kbf/worker.pem --worker-tls-key=/etc/kbf/worker.key \
    --worker-client-ca=/etc/kbf/ca.pem \
    --storage-url=https://storage.example.invalid:8983 --storage-ca=/etc/kbf/ca.pem \
    --storage-client-cert=/etc/kbf/worker.pem --storage-client-key=/etc/kbf/worker.key
```

The second command gives no `--database`, so one `--apply` of it leaves a login role
with no password and no privilege and a database that role owns on this host, over the local
Postgres socket with peer authentication. The schema follows once the server admits the logins `setup`
makes as that role: on a stock server that means adding the `pg_hba.conf` and `pg_ident.conf`
lines `setup` prints, reloading the server and running `setup` again. See "The state database"
below for what has to be installed first and what the server must admit.

## `kbf setup` on a host from the packages

A host that has the `kbf` package has the verb in the `kbf` binary too: `sudo kbf setup ...`
is `kbf-installer setup ...` with the same flags, the same steps, files, accounts and state
database, and the same exit statuses. Its `--apply` and `--json` go after the verb
(`sudo kbf setup --apply --mode=storage ...`), and an error is `kbf setup: <why>` on standard
error. It has no `--proxy` flags at all (clap refuses them as unknown flags; `kbf-installer setup`
takes them only to refuse them, see "No proxy host"): `kbf` does not link the
proxy, so `kbf setup` refuses an `installer.toml` that has a `[proxy]` table rather than rewrite
it (it tells you to run `kbf-installer setup` and to leave the table in the file). The lines that
report the database step are the library's own text and name `kbf-installer setup (<mode>)`
whichever program printed them. The rest of this page is about both.

## Dry run and `--apply`

`--apply` is a flag of `kbf-installer` itself, so it goes before or after the verb. Without it
`setup` prints each step as `DRY-RUN: <step>`, ends with `dry run: nothing changed. Re-run with
--apply.` and changes nothing. With it, `setup` prints `+ <step>` and does the step. When the host
already matches the flags it prints `nothing to do`. Both runs check that they are root first, and
validate every flag and read the host before the first step; a dry run of the database step asks
the server and says what it would do. `--apply` also requires that the `kbf-installer` binary
is root's alone. `--json` makes an error one JSON object with a stable `error` word. No output
holds a token.

## Which flags a mode takes

| mode | `--frontend-url` | `--token-file` | `--database` |
|---|---|---|---|
| `storage` | required | required | refused |
| `frontend` | refused | refused | optional: left out, the local default (see below) |

The `storage` mode enrolls with a frontend when it starts, so it needs its URL
(`https://host[:port]`, nothing after it) and an enrollment token file (an absolute path, one
word, mode 0600 or tighter, at most 4 KiB). A frontend host does not enroll; it
keeps its state in Postgres. On a host that runs the Postgres server, `setup` makes the role, the
database and the schema itself; the server is the one thing the operator installs first. There is
no `node` mode. The modes `daemon`, `storage-and-daemon` and `all-in-one` were removed: `--mode=` with
one of them exits 2 with a line that points to [kbf-modes.md](kbf-modes.md#the-removed-modes).

| flag | meaning |
|---|---|
| `--release-source`, `--release-url`, `--release-ca`, `--frontend-worker-url`, `--frontend-ca`, `--frontend-ca-sha256`, `--commit` | retired: `setup` no longer installs a release or writes where releases come from. The flags are still taken, for one release, so a script that passes them goes on working: their values are not checked and not used, and `setup` prints one `warning: ignored: ...` line naming the ones given, ahead of its steps |
| `--min-generation N` | the floor for orders; it only goes up, `0` is refused, and a lower value than `installer.toml` holds is refused (exit 5) |
| `--database`, `--create-local-database` | the state database, see below; `--create-local-database` is redundant when `--database` is left out |
| `--no-start` | enable units without starting them |
| `--installer-config` | accepts only the platform's own `installer.toml` path; it is a flag of `run` |
| `--proxy`, `--proxy-base-port`, `--buck2-auth`, `--buck2-token-file`, `--storage-port` | removed with the two-slot supervision. `kbf-installer setup` still parses them (they are hidden from the help) and refuses any of them, see "No proxy host" |

`--database` is `postgres:///NAME?host=SOCKET_DIR&user=ROLE`. Left out on a `frontend` host it is
`postgres:///kbf?host=/run/postgresql&user=kbf-server`, and `setup`
makes that role and database (see "The state database"). The role is required in a URL you give,
because `setup` runs as root and the service as its own account. A URL with a password is
refused (the file is world-readable), and no message quotes a URL. Settings of `kbf` that
`setup` has no flag for (`releases-dir`, `reapi-auth-policy`, `worker-deny-list`,
`proxy-auth-file` and the tuning keys) are the operator's; see the configuration reference. Such
keys survive a re-run under the rule in "The `kbf.toml` file".

## The `kbf.toml` flags of a frontend host

A storage node's listeners, certificates and storage URL come from enrollment. A `frontend`
host does not enroll, so its addresses, certificates and storage URL are the operator's.
`setup` takes them as flags and writes each as the key of the `kbf` flag it sets, in the table of
that flag's group.

| flag | `[table] key` | frontend |
|---|---|---|
| `--listen`, `--worker-listen` | `[listener]` | optional |
| `--api-listen` | `[listener] api-listen` | default `127.0.0.1:8982` |
| `--reapi-tls-cert`, `--reapi-tls-key` | `[tls]` | both or none |
| `--worker-tls-cert`, `--worker-tls-key`, `--worker-client-ca` | `[tls]` | all three or none |
| `--storage-url` (key `storage`) | `[storage-client] storage` | required |
| `--storage-ca`, `--storage-client-cert`, `--storage-client-key` | `[storage-client]` | all three with `https`, none with `http` |

The `storage` mode refuses all of them. Every rule is checked before anything is written (exit 2, the
flag and the rule named, a value never quoted). An address is `IP:PORT`. A file flag is an
absolute path of plain characters. A URL has no user name, password, path or query, and `http://`
storage is for a loopback host only. The files named must exist as regular files (a link is not
followed). A key must be owned by `kbf-server`, the user the frontend's unit runs as, with no group
or other permission bit; a certificate or CA is owned
by root or that user and not writable by group or others. The directory that holds a key or a
certificate is not looked at.

## The `kbf.toml` file

`setup` writes `/etc/kbf/kbf.toml` (root, group of the service user, mode 0640) with the keys in
this order: `mode`, `[frontend] url`, `[enroll] token_file`, `[listener]`, `[tls]` (including
`api-token-file` on a frontend host), `[storage-client]`,
`[scheduler] state-store` (the database URL). The rule for a file that is there:

- A file that holds only keys this run writes is rewritten to the flags.
- A file that also holds other settings is kept when the keys this run writes match, and refused
  (exit 5, the keys named, never a value) when one differs. A `listen` in the file that this run
  gives no `--listen` for is the operator's, so the file is kept or refused, never rewritten.
- A hand-set `api-listen` on a run without `--api-listen` counts as a difference, because no flag
  means the default `127.0.0.1:8982`; the message names `--api-listen` as the way forward.

Add another setting to the table it belongs to, never as a second table of the same name: TOML
refuses a table written twice, and so does `kbf` (exit 2, `duplicate key`).

## What it does, in this order

1. Checks it is root and, to apply, that its own binary is root's alone. Checks the flags (every
   problem is named). Then it refuses a unit that shadows the packaged one, see the next section.
   (`kbf-installer setup` has refused the proxy's flags before any of this, from the flags alone.)
2. The `kbf-fetch` account. An account of that name that is not a system
   account with no shell and no home is refused (exit 5) and left alone. The `kbf` and
   `kbf-server` users come from the package's sysusers files: a host without them is refused
   (exit 2), naming the user.
3. `/etc/kbf`, the installer tree `/var/lib/kbf-installer` (root, 0755) and its `orders/`
   (0700, owned by the user the mode's unit runs as).
4. `storage`: the enrollment token from `--token-file` to `/etc/kbf/enroll.token`, mode 0600,
   owned by the service user.
5. Frontend: the first operator token (32 random bytes as hex) in
   `/etc/kbf/api-token`, 0600, owned by the service user,
   only when that file is not there. The output names the file and never the secret.
6. `/etc/kbf/installer.toml` (root, 0644): the floor, for a `frontend` release source the node
   certificate and key paths that enrollment writes (`pki/storage.*` for `storage`), and for the
   frontend `database`.
   The release source and the keys that go with it are not written: a file that has them (a host
   set up by an earlier build) keeps them, and a host with no file gets `release_source =
   "frontend"`, the default. Settings a flag does not cover are kept. The file is closed: one that
   does not parse is left alone (exit 5). The operator's `[proxy]` table is kept (see "No proxy host"),
   and so is `frontend_config` if the file has it.
7. Frontend: the state database's schema is created or brought forward. Without
   `--database` (or with `--create-local-database`) the role and the database are made first,
   and the `pg_hba.conf` and `pg_ident.conf` lines the logins need are printed. A server that
   cannot be reached, or refuses a login, stops `setup` here (exit 1, or 5 where only a person
   can go on) with `installer.toml` written and no `kbf.toml` or unit; running it again finishes.
8. `/etc/kbf/kbf.toml`, as above.
9. The units, last. `systemctl enable`, with `--now` unless `--no-start`: the mode's unit
    (only when a release is installed), and no other (no package carries
    `kbf-installer.service` now). `kbf-frontend.service` is enabled and never started by
    `setup`: starting it takes the database, the TLS files and the port. `setup` enables no proxy unit. A unit that is enabled
    already is left
    alone, and nothing is restarted. On a host that an earlier build set up with
    `kbf-backup.timer`, `setup` also disables the timer if it is enabled, removes the drop-in an
    earlier `setup` wrote for `kbf-backup.service` (and its directory), and reloads systemd; a
    host that never had them gets no such step.

Every step is planned only where the host differs, so the same flags again is "nothing to do". A
run that stopped part way is finished by running it again. `kbf.toml` and the units come last, so
a stopped run leaves no config that a unit would start from. Changed flags rewrite the files they
own atomically and never restart anything.

## A unit that shadows the packaged one

A regular file at `/etc/systemd/system/<the mode's unit>` shadows the packaged unit:
systemd reads `/etc` before
`/usr/lib`, so the host would keep the old flags. `setup` refuses it (exit 5, `unit_shadow`) before any
step, with this message, and never deletes a unit it did not write:

```text
<path> is a regular file and shadows the packaged unit <unit> (systemd reads /etc before /usr/lib, so enabling would keep its old flags): move it aside (mv <path> <path>.old; systemctl daemon-reload) and run setup again; setup never deletes a unit
```

A link there (`systemctl link`) is not refused.

## No proxy host

The proxy is the bundled `kbf-installer proxy`, which owns the public port, terminates TLS and
balances each stream over a pool of frontends. It no longer starts, adopts or swaps frontend
children: its two-slot supervision was removed. The frontends run under their own units, and the
proxy is told about them by hand, with one `[[proxy.frontend]]` table for each in the `[proxy]`
table of `installer.toml` (the schema and the pool are in `docs/design/kbf-proxy-api.md`,
sections 7 and 9). `setup` therefore sets up no proxy host:

- **The proxy's flags are refused.** `--proxy`, `--proxy-base-port`, `--buck2-auth`,
  `--buck2-token-file` and `--storage-port` are still parsed by `kbf-installer setup` (hidden from
  the help), so that a script that passes one gets the reason and not "unexpected argument". A
  run that gives any of them stops at once, from the flags alone, before it checks that it is
  root and before it reads or changes the host: exit 2 (`invalid_flags` with `--json`), one line
  that names each flag given and says that two-slot supervision was removed and that a proxy is
  configured by hand with `[[proxy.frontend]]` tables.
- **A `[proxy]` table that the file has is kept.** `setup` reads it (the table of an earlier build,
  slot table `[proxy.upstream]` included, parses) and writes the file back in the canonical form
  with every key and value of the table kept, so a file that is in that form is the same bytes.
  A run without any proxy flag never drops or merges into it. Only the proxy refuses the slot
  table: `kbf-installer proxy` (and so the proxy's start, `--check` included) exits 2 with the
  words that say the supervision was removed and that a `[[proxy.frontend]]` table for each
  frontend takes its place; `setup` and the release fetcher go on reading the file, so edit the
  table at your own time. The proxy's keys `state_dir`, `hold_max_s` and `remote_operator_file`
  belonged to that supervision and to the CI credential of its update API: they are still
  accepted and kept, and nothing reads them.
  `kbf setup` does not link the proxy and so refuses a file that has the table (exit 5), telling
  you to run `kbf-installer setup` and to leave the table where it is. The top-level key
  `frontend_config`, which an earlier build wrote behind the proxy, is kept
  too; nothing reads it any more. `setup` enables the mode's own unit
  (`kbf-frontend.service` for a frontend) whatever the file holds, and says so when the file has
  a `[proxy]` table; it enables no proxy unit, and writes none of the proxy's keys.

No release ships `kbf-proxy.socket` any more, so `setup` writes no drop-in for the `listen` key of
the `[proxy]` table and leaves any `listen.conf` an earlier build wrote where it is.

## The state database

The `frontend` mode keeps its state in one Postgres database on the host. The package
only suggests the server, because storage hosts use none. A single box needs the server installed, a
`setup`, the rules `setup` prints added to the server, and a second `setup`:

```text
sudo apt install postgresql                     # Debian, Ubuntu
sudo dnf install postgresql-server              # Fedora: then
sudo postgresql-setup --initdb && sudo systemctl enable --now postgresql
sudo kbf-installer --apply setup --mode=frontend ...    # no --database: makes the role and database
# add the printed pg_hba.conf and pg_ident.conf lines, reload the server, then:
sudo kbf-installer --apply setup --mode=frontend ...    # same flags: makes the schema, the rest
```

- **The default.** With no `--database`, `setup` uses
  `postgres:///kbf?host=/run/postgresql&user=kbf-server`, written to `installer.toml` and, as `state-store`, to `kbf.toml`, and
  makes what is missing of it, as `--create-local-database` does. One run leaves a login role with no password and no privilege and a database that role
  owns; the schema is made once the server admits the logins (below), on that run if it already
  does and on the next if not. `--create-local-database` is still accepted and is redundant
  here; it matters beside a `--database` you give, to have `setup` make that role and database.
  To keep a role you made by hand, give `--database` without the flag (see "A remote or managed
  server"). A rerun finds the role, the database and the schema and changes nothing. An
  `installer.toml` that already names another database is not replaced: `setup` stops (exit 5)
  and asks for `--database` (the same value keeps it).
- **What is made.** `setup` runs `/usr/bin/psql` as the operating-system user `postgres` over the
  Unix socket the URL names (peer authentication, no password) and creates, if missing, a login
  role (no password, not a superuser, no CREATEDB, CREATEROLE or REPLICATION) and a database it
  owns. It alters nothing that exists: a role that cannot log in or holds any privilege, and a
  database another role owns, are refused (exit 5). Names are letters, digits, `_` and `-` (at
  most 63); the role is neither `postgres` nor `pg_`-prefixed; the socket directory is absolute;
  one server only.
- **Without psql or a running server, `setup` refuses (exit 5) with the install command**,
  before anything of the database is made: `apt install postgresql` on Debian and Ubuntu,
  `dnf install postgresql-server` then `postgresql-setup --initdb` on Fedora and its relatives
  (both are named when the host's `/etc/os-release`, or `/usr/lib/os-release` behind a link,
  says neither), and, for a server that is
  installed and stopped, to start it. The refusal also gives the way out by hand: create the
  role and database and pass `--database`.
- **`setup` never edits `pg_hba.conf` or `pg_ident.conf`.** On every run that makes the role and
  database or checks them (a dry run too), it prints the rules the logins need, as information, so
  a rerun after a refused login has them with the names filled in: above the first `local` line
  of `pg_hba.conf`, `local <database> <role> peer map=kbf` (for the default
  `local kbf kbf-server peer map=kbf`), and in `pg_ident.conf` the lines `kbf root <role>` and
  `kbf kbf-server <role>` (`kbf-server` is the
  account the frontend's unit runs as). Root
  needs the map
  because `setup` logs in as root with the role. `setup` then logs in the way kbf will (as root
  with the role, and as the service account when it exists) and, when the server refuses,
  stops and says which rule is missing. Add the rules, reload the server (not restart) and run
  `setup` again. The map name is any name used on both sides.
- **A remote or managed server.** `setup` holds no password and does not reach a remote
  superuser. Create the role and database, make the server admit the logins, and run `setup`
  with `--database` and without `--create-local-database`: it checks that it can log in, then
  creates or brings forward the schema.

Backups of the database are the operator's: Postgres is a runtime dependency that the operator
provides, and its own backup and restore tools apply. Neither `setup` nor the package runs one,
and `kbf-installer` has no `backup` or `restore` verb.

## Exit codes

| code | meaning in `setup` |
|---|---|
| 0 | done, or a dry run that found nothing wrong |
| 1 | failed: the database cannot be reached, a step failed, no free id for the fetch account |
| 2 | usage or a missing prerequisite: not root (`setup writes /etc and manages services: run it with sudo`), a binary root does not own alone, bad flags (the proxy's flags among them, see "No proxy host"), a bad token file, a missing `kbf` or `kbf-server` user, a removed mode |
| 5 | only a person can go on: a shadowing unit, a `kbf.toml` or `installer.toml` `setup` will not overwrite, a floor that would go down, a wrong fetch account |

`setup` does not take a lock, so it never exits 4.
