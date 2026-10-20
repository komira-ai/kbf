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
    --database='postgres:///kbf?host=/run/postgresql&user=kbf' --create-local-database \
    --listen=0.0.0.0:8980 --worker-listen=0.0.0.0:8981 \
    --worker-tls-cert=/etc/kbf/worker.pem --worker-tls-key=/etc/kbf/worker.key \
    --worker-client-ca=/etc/kbf/ca.pem \
    --storage-url=https://storage.example:8983 --storage-ca=/etc/kbf/ca.pem \
    --storage-client-cert=/etc/kbf/worker.pem --storage-client-key=/etc/kbf/worker.key
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
server exists before `setup` runs (the operator provides it); the role and the database
exist too, or `--create-local-database` makes them (see "The state database" below).
`--database` says where, as `postgres:///NAME?host=SOCKET_DIR&user=ROLE`; the role is
required, because `setup` runs as root and the service as its own account. A URL with a password is
refused (`installer.toml` is world-readable), and no message quotes a URL. `--commit` installs
the first release before the host has enrolled, so it needs `--release-source=url`.
`--min-generation` is the operator floor for orders; a re-run may raise it and never lowers it.
`--installer-config` is a flag of `run`; `setup` writes `/etc/kbf/installer.toml`, where the
installer's unit reads it, and refuses another path. A Mac is a node only (`--mode=daemon`).

### The `kbf.toml` flags of a frontend and an all-in-one host

A node's listeners, certificates and storage URL come from enrollment, so its `kbf.toml` is
complete with the three keys above. A `frontend` or `all-in-one` host does not enroll; the
certificates and addresses are the operator's, and `setup` takes them as flags and writes each
as the key of the `kbf` flag it sets, in the table of that flag's group. The flag names that
differ from the key are marked.

| flag | `[table] key` | frontend | all-in-one |
|---|---|---|---|
| `--listen`, `--worker-listen` | `[listener]` | optional | optional |
| `--api-listen` | `[listener] api-listen` | default `127.0.0.1:8982` | default `127.0.0.1:8982` |
| `--reapi-tls-cert`, `--reapi-tls-key` | `[tls]` | both or none | both or none |
| `--worker-tls-cert`, `--worker-tls-key`, `--worker-client-ca` | `[tls]` | all three or none | all three or none |
| `--storage-url` (key `storage`) | `[storage-client] storage` | required | refused |
| `--storage-ca`, `--storage-client-cert`, `--storage-client-key` | `[storage-client]` | all three with `https`, none with `http` | refused |
| `--cold-store` | `[stores] cold-store` | refused | required, `file:///absolute/path` |
| `--meta-dir` | `[stores] meta-dir` | refused | required, absolute path |
| `--daemon-server` (key `server`), `--daemon-ca-cert` (`ca-cert`), `--daemon-cert` (`cert`), `--daemon-key` (`key`), `--node-id` | `[identity]` | refused | required |
| (none) | `[daemon] driver = "container"` | | always: the only driver `all-in-one` accepts |

A node mode refuses all of them. The rules, all checked before anything is written (exit 2,
the flag and the rule named, a value never quoted): an address is `IP:PORT`; a file flag is an
absolute path of plain characters, never the file's text; a URL has no user name, password,
path or query; `http://` storage is for a loopback host only (as `kbf` refuses any other); the
files named exist, are regular files (a link is not followed), and a key is owned by the user
the mode's unit runs as with no group or other permission bit and readable by its owner, as
`kbf`'s own start-up check requires, and a certificate or CA is owned by root or that user and
not writable by group or others; an `s3://` cold store is refused because its credentials are
in the environment, which `setup` does not manage; `--node-id` is a DNS label. Setting what
`kbf` accepts but `setup` has no flag for (`releases-dir`, `reapi-auth-policy`,
`worker-deny-list`, `proxy-auth-file`, the tuning keys) stays by hand: such keys survive a
re-run under the rule below.

What these checks do not cover, so the operator checks it: the directory that holds a key or a
certificate is not looked at (its owner and mode are the operator's), and `--cold-store` and
`--meta-dir` are checked as paths only, not for existing, being a directory or being writable
by the service.

Two edge cases of the rule below, stated plainly. A `kbf.toml` that holds only the keys `setup`
writes is `setup`'s: a hand edit of one of them (say `listen` or `api-listen`) is rewritten to
the flags, or the defaults, on the next run, because the file has nothing of the operator's to
protect. And a file that also holds other settings is refused (exit 5) when a key differs, which
includes a hand-set `api-listen` on a run without `--api-listen` (no flag means the default
`127.0.0.1:8982`); the message names `--api-listen` as the way forward, which is to pass the
address that is in the file.

### The deploy config of a frontend and an all-in-one host

`--deploy-stage gamma|prod` makes `setup` write `/etc/kbf-installer/deploy.json` (root's, mode
0644, written atomically), so `kbf-installer deploy` needs no flag naming the API, the token or
the stage. Without `--deploy-stage` nothing is written, and every other `--deploy-*` flag needs
it; a node mode takes none of them.

| flag | key | default |
|---|---|---|
| `--deploy-stage` | `stage` | required to write the file |
| `--deploy-api` | `api` | the proxy's public port: `[proxy] listen` of `installer.toml` (an unspecified address read as loopback), else `127.0.0.1:8980` |
| `--deploy-token-file` | `token_file` | `[proxy] operator_token_file` of `installer.toml` |
| `--deploy-api-ca` | `api_ca` | none (the system's roots) |
| `--deploy-server-name` | `api_server_name` | none (the API address's IP) |

The token and the CA are referenced by path and are the operator's: `setup` never copies or
reads them, and creates none but the first operator token (the default token, which a first
run may therefore name before it exists). Before it touches the host it checks that each exists, is a regular file
(a link is not followed), is owned by root or the service user, and that the token is mode 0600
or 0400 and the CA is not writable by group or others; a failure is exit 5 naming the flag and
the one command that fixes it. The five keys above are `setup`'s and are rewritten from the
flags on every run (a run again with the same flags changes nothing); any other key in the file
(`cas`, `cas_ca`, `cas_server_name`) is kept. A file that is not a JSON object is left alone and
named. The sudoers rule that lets the deploy account run `deploy` is a held step, written out in
`docs/design/gamma-deploy.md`, "The host's config: who writes it, and the sudoers rule".

## What it does, in this order

1. Checks it is root (and, to apply, that its own binary is root's alone), checks the flags
   (every problem is named), and refuses (exit 5, `unmigrated`) while the old layout of the
   retired `kbf-helper` and `kbf-deploy host` is the host's: its state (`/var/lib/kbf-deploy`)
   is there and no migration finished. The two never both manage a host. The refusal names
   the way out: `kbf-installer migrate --apply --fetch-user kbf-fetch [--unit UNIT]`
   (`_kbf-fetch` on a Mac; without `--apply` it is a dry run). A host `migrate` finished
   (its journal says `Done`) passes: it keeps `/usr/local/kbf` and `/var/lib/kbf-deploy` on
   purpose, as the rollback path, so their presence is not the test. An old layout whose
   journal cannot be read stops `setup` too (exit 1, `migrate`).
   On Linux a regular file at `/etc/systemd/system/<the mode's unit>` (for a frontend, also
   `kbf-frontend@.service`) shadows the packaged unit: systemd reads `/etc` before
   `/usr/lib`, and a unit that is already enabled is not enabled again, so the host would
   keep the old flags. If the migration journalled that file by name (it is the unit
   `migrate --unit` repointed), `setup` retires it: the bytes are kept in the migration's
   backup directory (`migration-units/<unit>.retired`), the file is removed, systemd is
   reloaded and the packaged unit is enabled. The dry run prints
   `DRY-RUN: retire <path> (shadows the packaged unit; original kept)`.
   `kbf-installer migrate --rollback` puts a retired file back (the migration's original
   where one was kept, else the retired file). Any other file there is refused (exit 5,
   `unit_shadow`) and named: `setup` never deletes a unit it did not see the migration take
   over. Move it aside by hand (`mv FILE FILE.old; systemctl daemon-reload`) and run `setup`
   again. A link there (`systemctl link`) is not refused.
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
   The operator's `[proxy]` table is kept (written back whole, defaults filled in). Its `listen`
   key is read here, as below.
7. Frontend and all-in-one: the state database's schema is created, or brought forward, by the
   installer's own migrations; a dry run asks the server and says what it would do. With
   `--create-local-database` the role and the database are made first. `setup` then logs in
   as root with the URL's role and, when the service account exists, as that account, so a
   server that would admit one and not the other is refused here and not at the first start.
   A server that cannot be reached, or that refuses a login, stops `setup` here (exit 1)
   with the recipe below, `installer.toml` written and no release, `kbf.toml` or unit;
   running it again finishes.
8. `--commit`: the first release, through the installer's `Fetcher` (the fetch child and the
   verify pipeline) and nothing else, then made `releases/current` with no trial. This build
   has no fetcher wired in, so it stops here with exit 1, before `kbf.toml` and the units.
   When a release is installed already, `--commit` is not fetched.
9. `/etc/kbf/kbf.toml` (root:service group, 0640): `mode`, `[frontend] url`, `[enroll]
   token_file`, on a frontend or all-in-one host `[tls] api-token-file` and `[scheduler]
   state-store` (the database URL), and the flags of the table above, in the order mode, `[frontend]`,
   `[enroll]`, `[listener]`, `[tls]`, `[storage-client]`, `[stores]`, `[identity]`, `[daemon]`,
   `[scheduler]`. A file that holds other settings is kept when the keys this run writes
   match, and refused (exit 5, the keys named, never a value) when they differ. A file of only
   the keys this run writes is rewritten. What counts as another setting is relative to the run:
   a `listen` in the file that this run gives no `--listen` for is the operator's, and the file
   is kept or refused, never rewritten without it.
10. The units, last: the mode's unit and `kbf-installer.service`, `systemctl enable`, with
   `--now` for a node's unit when a release is installed and `--no-start` is not given (the
   installer's own service does not wait for either). A unit that makes a frontend
   (`kbf-frontend.service`, `kbf-all-in-one.service`) is enabled and never started by
   `setup`: starting it takes the database, the TLS files and the port. A unit already
   enabled is left alone. Nothing is restarted. Those two modes also get `kbf-backup.timer`,
   enabled and started (the schema it dumps was made in step 7), and a drop-in that gives
   `kbf-backup.service` its `User=`: the mode's service user, the one the frontend unit runs as
   (see "Backup and restore").

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

**The proxy's public address.** `kbf-proxy.socket` ships listening on port 8980. A frontend or
all-in-one host that needs another address sets `listen = "host:port"` in the `[proxy]` table
of `installer.toml` (an IPv6 host in brackets; port 1 to 65535; the table must be complete, as
the proxy reads it too). On Linux `setup` then writes
`/etc/systemd/system/kbf-proxy.socket.d/listen.conf` (root:root 0644, atomically) with exactly

    [Socket]
    ListenStream=
    ListenStream=<the address>

The empty first line clears the packaged port. With no `listen`, `setup` writes no drop-in and
removes the one an earlier run wrote. Changing the file never restarts anything: the output
says to run `systemctl daemon-reload` and then `systemctl restart kbf-proxy.socket`, which the
operator does when the public port may drop the connections waiting in it. A bad `listen`
stops `setup` with exit 5 and names the key, never the value.

**Proxy units.** The bundled proxy's units (`kbf-proxy.socket` and `kbf-proxy.service`) are
the ones a `--proxy` host enables (next section); `mode_units` in `src/setup/plan.rs` is the one
place that names them.

### A frontend behind the proxy: `--proxy`

`--mode=frontend --proxy` sets the host up for `kbf-installer proxy`, which owns the public port
(the socket unit's), starts the frontend children itself on loopback ports and terminates TLS.
The proxy starts `kbf --mode=frontend` children, so `--proxy` is refused for every other mode
(an `all-in-one` process cannot sit behind it), and it is never implied: a re-run on a host
that has the table but not the flag keeps the table, enables `kbf-frontend.service` as before
and says so.

| flag | written as |
|---|---|
| `--reapi-tls-cert`, `--reapi-tls-key` (required) | `[proxy] server_cert_file`, `server_key_file`: the certificate clients see |
| `--worker-client-ca` (required) | `[proxy] client_ca_file`: verifies client certificates |
| `--proxy-base-port` (default 18000, 1024 to 65521) | `[proxy.upstream] base_port`: slot A uses it and the four after, slot B ten above |
| `--buck2-auth none\|bearer\|mtls`, `--buck2-token-file` | `[proxy] buck2_auth`, `buck2_token_file` (a key file only with `bearer`) |
| (none) | `[proxy] operator_token_file`: the operator token `setup` makes |

`setup` enables `kbf-proxy.socket` and `kbf-proxy.service` instead of `kbf-frontend.service`
(enabled, not started), writes none of those three TLS files into `kbf.toml` (the children
speak plain h2c behind the proxy), and refuses `--listen`, `--worker-listen`, `--api-listen`,
`--worker-tls-cert` and `--worker-tls-key`: the proxy gives each child its loopback ports on
its command line and nothing would read them. It refuses a base port whose ranges cover 8980
or 8982, the public socket's and the operator API's defaults. The table is checked by the
proxy's own `ProxyConfig::check`, so `setup` cannot write one the proxy would refuse at start.
If `kbf-frontend.service` is already enabled `setup` names it and leaves it: stop and disable
it before starting the proxy, since the two never run together.

Every other key of `[proxy]` (the secret file, the state directory, the hold bound, the limits)
is the proxy's default and is written out in full, so a hand edit survives a rerun. A `[proxy]`
table that is there is treated as `kbf.toml` is: the keys `setup` owns equal to the flags, it
is kept as it is; different over a table of defaults, rewritten; different over a table with
other settings, refused (exit 5) with the key names and no values.

## The state database

`frontend` and `all-in-one` keep their state in one Postgres database on the host. The
package only suggests the server (`Suggests: postgresql`, rpm `postgresql-server`): the
package is shared by node hosts, which use none, and apt installs `Recommends` by default,
which would start a database on every node. `setup` for these two modes refuses clearly when
no server answers at the URL's socket, and says what to install.

Install the server, then one of these.

**Local server, setup makes the role and database (`--create-local-database`).** `setup` asks
the server as its superuser, by running `/usr/bin/psql` as the operating-system user
`postgres` over the Unix socket the URL names: peer authentication, no password anywhere. It
creates, if missing, a login role (no password, not a superuser, no CREATEDB, CREATEROLE or
REPLICATION) and a database owned by it, both named by the URL. A re-run creates only what is
missing. It alters nothing that exists: a role that cannot log in or holds any privilege
(superuser, CREATEROLE, CREATEDB, REPLICATION, BYPASSRLS), and a database another role owns,
are refused (exit 1) and left as they are. The names must be
letters, digits, `_` and `-` (at most 63), the role neither `postgres` nor `pg_`-prefixed,
and the host a Linux host with a `postgres` account. The server's socket directory comes from
`host=` and must be absolute; a remote server is never reached this way. `--database` names
one server: a URL with several hosts or ports is refused, so that the role is never made on
one server and the login tried on another.

**`setup` never edits `pg_hba.conf` or `pg_ident.conf`.** It logs in the way kbf will and,
when the server refuses, prints the rules to add. For a role `<role>` in database
`<database>` and a service account `<service>` (`kbf-server` for `frontend` and `all-in-one`),
above the first `local` line of `pg_hba.conf`:

```text
local  <database>  <role>  peer map=kbf
```

and in `pg_ident.conf`:

```text
kbf  root       <role>
kbf  <service>  <role>
```

then reload the server (`pg_ctl reload`, or `systemctl reload postgresql`; a restart is not
needed) and run `setup` again. If the role is named like the service account, the first
`pg_ident` line alone is the missing part for root.

`kbf` is the name of the map and any name works, as long as the `pg_hba.conf` line and the
`pg_ident.conf` lines use the same one (a host that already has a map, `kbfmap` say, keeps
it): `setup` never reads either file, it only tries the logins, so it does not know the name.

**Remote or managed server, or a role and database the operator makes.** `setup` does not
reach a remote server's superuser and holds no password. Create the role (login, no
superuser) and a database it owns, make the server admit the logins above (with a server over
TCP, client certificates are out of scope, see `docs/design/frontend-proxy.md` section 11),
and run `setup` without `--create-local-database`: it only checks that it can log in, then
creates or brings forward the schema.

No refusal quotes the URL, the role, the database or the socket directory; the server's own
words, which do, stay in its log.

## Backup and restore

A frontend or all-in-one host keeps all of kbf's state in its Postgres database, so that
database is backed up. `kbf-backup.timer` starts `kbf-backup.service` every day (a host that
was down then runs it when it starts, and each run is delayed by up to an hour), and the
service runs `kbf-installer --apply backup` as the host's service user (`kbf-server` on a
frontend, `kbf` on an all-in-one host). The package's `kbf-backup.service` carries no `User=`:
`setup` writes it into `/etc/systemd/system/kbf-backup.service.d/10-service-user.conf` (root,
0644) from the service user of the host's mode, together with the mode's state directory
(`StateDirectory=` and the verb's `--dir`: `kbf-server` on a frontend, `kbf` on all-in-one, the
ones of the mode's own unit). The unit asserts that file, so where `setup` has not written it
(or it was deleted) the unit fails, rather than run as root or be skipped as a success. When
the drop-in changes under a timer that is enabled already (a mode change), `setup` runs
`systemctl daemon-reload` so the next run uses it. `setup` enables the timer on
the two modes; the units are in the package like the others (`packaging/linux/units`). The
backup runs as the role and in the database that "The state database" above provisions: both
come from the `database` URL of `installer.toml`, whether `setup` made them
(`--create-local-database`) or the operator did.

**What the backup is.** `pg_dump` in custom format of the database `installer.toml` names, in
`/var/lib/<state directory>/backups/kbf-<UTC time>.dump` (for example
`/var/lib/kbf-server/backups/kbf-20270115T080000Z.dump` on a frontend; on an all-in-one host
`/var/lib/kbf/backups/...`).
The directory is 0700 and each file 0600, owned by the service user that runs the unit (the
verb refuses a directory owned by anyone else): a dump holds the whole state, tokens and
member map included. Nothing about the database is assumed. The role, the database name and
the socket directory are the `database` URL of `installer.toml`, the one `setup` and the
frontend read (on the farm
`postgres:///kbf?host=/run/postgresql&user=kbf-server`: role `kbf-server`, database `kbf`).
With peer authentication the service user connects as that role, directly when the names are
the same or through the `pg_ident.conf` map `setup` tells you to add; no password exists
anywhere. A URL without `user=` leaves the role to libpq, which uses the OS user's name.

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
The package does not depend on it: it only suggests the Postgres server (see "The state
database"), and the server package brings its client on most distributions. Both are found by name in `PATH`; `--pg-dump` and
`--pg-restore` name other paths.

**The first backup, and its restore test.** The design requires both before the farm depends
on the database. On a new host, after `setup`, run one backup now and restore it into a
scratch database, which touches nothing live:

```text
sudo systemctl start kbf-backup.service        # a oneshot: returns when the backup is done
sudo ls -l /var/lib/<state directory>/backups   # kbf-<time>.dump, mode -rw-------
sudo -u postgres createdb --owner=<role in the database URL> kbf_restore_test
sudo -u <service user> kbf-installer --apply restore \
    --file /var/lib/<state directory>/backups/kbf-<time>.dump \
    --database kbf_restore_test
sudo -u <service user> psql 'postgres:///kbf_restore_test?host=<socket dir>&user=<role>' \
    -c '\dt kbf_state.*'
sudo -u postgres dropdb kbf_restore_test
```

`restore` runs `pg_restore` in one transaction that stops at the first error, taking no owner
or privilege from the dump (the connecting role owns what it makes, as `setup` made it). The
database must hold no kbf schema: a restore into one that does fails and changes nothing.
Without `--database` it restores into the database `installer.toml` names. `--database` takes
a bare name (as above: a database on the same server and socket, for the same role as
`installer.toml`'s URL) or a full URL with no password; the URL's `port=` (or the port of its
authority) is kept. On a frontend the user, role, socket and state directory are `kbf-server`,
`kbf-server`, `/run/postgresql` and `kbf-server`.

**Restoring the real state** (the database is lost or damaged). The state is what the backup
held: builds in flight are gone, and nodes register again.

```text
sudo systemctl stop kbf-frontend.service     # waits for open builds; nothing may write
sudo -u postgres dropdb <database in the URL>
sudo -u postgres createdb --owner=<role in the URL> <database in the URL>
sudo -u <service user> kbf-installer --apply restore --file /var/lib/<state directory>/backups/kbf-<time>.dump
sudo systemctl start kbf-frontend.service    # opens the store: its schema check must accept it
```

The tests: `src/backup_tests.rs` (retention, atomicity, modes and the arguments, reading the
role, database and socket from the URL, without a server), `src/setup/backup_dropin_tests.rs`
(the drop-in's `User=` and state directory are the mode's, and a reload follows a changed
drop-in under an enabled timer), `package_units.rs` (the unit carries no `User=` and asserts
the drop-in). They run in the PR's `test` job. `real_server_round_trip` needs a server and
`pg_dump` 17: it is `#[ignore]` and no job selects it yet (the `state-postgres` job of
`beta.yml` runs only the filters `database::` and `setup_creates_the_schema`, and its runner
has no PostgreSQL 17 client): it runs by hand, with `KBF_TEST_POSTGRES_URL` set
(`cargo test -p kbf-installer --lib -- --ignored backup::tests::real_server_round_trip`), until
a change to `beta.yml` adds the filter and the client. It makes the schema as `setup`
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

- **The `kbf.toml` flags (W10 gap B7).** The contract's `kbf.toml` holds the mode and the
  frontend URL only; a frontend or all-in-one file written so was a file `kbf` refused at start
  (`--storage` required, no cold store, no daemon identity). `--storage-url` is not
  `--storage`, which reads as the mode's name beside `--mode=storage`; the daemon half's flags carry
  `daemon-` because `--cert` and `--server` mean nothing on `setup`'s line. The operator API
  address has a default because `api-token-file`, which `setup` always writes, requires
  `api-listen` in `kbf`. `--admin-listen` (the handover port) is not a flag: it belongs to a
  frontend run under the proxy and needs that unit.
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
- **The old layout.** `setup` refuses while the old state exists unmigrated, as the
  contract says, and accepts a host exactly when `migrate` finished it (journal phase `Done`),
  not by path: a migrated host keeps both old roots as its rollback path. A journal of a
  migration that was rolled back is not done.
- **A unit file in `/etc/systemd/system` with a packaged unit's name.** Not in the contract.
  A farm node running the dev installer has `kbf-storage-and-daemon.service` there. If
  `migrate --unit` repointed it, `setup` retires it and enables the packaged unit (the node
  then runs the packaged unit's flags, so `kbf.toml` must be right first; `--no-start`
  enables without starting). A file the journal does not name is refused (exit 5,
  `unit_shadow`) rather than enable nothing and leave the old flags running. `setup` reloads
  systemd only after it retired a file.
- **No Envoy check, no Envoy unit.** The contract has `setup` test the installed Envoy against
  a tested range. The ruling since is that kbf manages no Envoy: the bundled proxy owns the
  frontend's port. `setup` neither checks Envoy nor enables `kbf-envoy.service`.
- **The operator token's path.** `frontend-proxy.md` names `/etc/kbf/operator.token`
  (root:kbf 0640, `operator_token_file` in `installer.toml`); the server reads the token from
  `[tls] api-token-file` today, so `setup` writes `/etc/kbf/api-token` and `kbf.toml` names
  it. When the proxy reads its own file, the path and the key move with it.
- **`check`** (also P10 in the contract's table) is not part of this change.
