# kbf-installer setup

`kbf-installer setup` is the first configuration of a host. It makes the accounts and
directories kbf needs, writes `installer.toml` and `kbf.toml`, and enables the units for the
host's mode. It installs no release: kbf reaches a host as a package. The contract is
[the installer design](../design/kbf-installer.md) (sections 2 and 3); this page is how the verb
behaves in the code it describes. A host that has the old layout is not adopted by the installer: adoption = install the kbf and
kbf-proxy packages (package-update design). A Mac has its own page,
[installing on macOS](kbf-installer-macos.md).

```text
sudo kbf-installer setup --mode=daemon \
    --frontend-url=https://frontend.example.invalid:8443 --token-file=/srv/kbf/enroll.token   # dry run
sudo kbf-installer --apply setup ...                                                          # do it

sudo kbf-installer setup --mode=frontend \
    --database='postgres:///kbf?host=/run/postgresql&user=kbf' --create-local-database \
    --listen=192.0.2.10:8980 --worker-listen=192.0.2.10:8981 \
    --worker-tls-cert=/etc/kbf/worker.pem --worker-tls-key=/etc/kbf/worker.key \
    --worker-client-ca=/etc/kbf/ca.pem \
    --storage-url=https://storage.example.invalid:8983 --storage-ca=/etc/kbf/ca.pem \
    --storage-client-cert=/etc/kbf/worker.pem --storage-client-key=/etc/kbf/worker.key
```

## `kbf setup` on a host from the packages

A host that has the `kbf` package has the verb in the `kbf` binary too: `sudo kbf setup ...`
is `kbf-installer setup ...` with the same flags, the same steps, files, accounts and state
database, and the same exit statuses. Its `--apply` and `--json` go after the verb
(`sudo kbf setup --apply --mode=daemon ...`), and an error is `kbf setup: <why>` on standard
error. It has no `--proxy` flags: `kbf` does not link the proxy, so a frontend behind the proxy
and an all-in-one host (always behind it) are set up with `kbf-installer setup --proxy`, and
`kbf setup` refuses an `installer.toml` that has a `[proxy]` table rather than rewrite it (it
tells you to run `kbf-installer setup` and to leave the table in the file). The lines that
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
| `storage`, `daemon`, `storage-and-daemon` | required | required | refused |
| `frontend`, `all-in-one` | refused | refused | required |

The modes in the first row enroll with a frontend when they start, so they need its URL
(`https://host[:port]`, nothing after it) and an enrollment token file (an absolute path, one
word, mode 0600 or tighter, at most 4 KiB). The other two do not enroll; they keep their state in
Postgres, which the operator provides before `setup` runs. There is no `node` mode, and a Mac
accepts only `--mode=daemon`.

| flag | meaning |
|---|---|
| `--release-source`, `--release-url`, `--release-ca`, `--frontend-worker-url`, `--frontend-ca`, `--frontend-ca-sha256`, `--commit` | retired: `setup` no longer installs a release or writes where releases come from. The flags are still taken, for one release, so a script that passes them goes on working: their values are not checked and not used, and `setup` prints one `warning: ignored: ...` line naming the ones given, ahead of its steps |
| `--min-generation N` | the floor for orders; it only goes up, `0` is refused, and a lower value than `installer.toml` holds is refused (exit 5) |
| `--database`, `--create-local-database` | the state database, see below |
| `--no-start` | enable units without starting them |
| `--installer-config` | accepts only the platform's own `installer.toml` path; it is a flag of `run` |
| `--proxy`, `--proxy-base-port`, `--buck2-auth`, `--buck2-token-file` | a frontend behind the bundled proxy, see below; `--proxy` is required by `--mode=all-in-one` |
| `--storage-port` | `--mode=all-in-one` only: the loopback port of its sibling, default 8990, see "An all-in-one host" |

`--database` is `postgres:///NAME?host=SOCKET_DIR&user=ROLE`. The role is required, because
`setup` runs as root and the service as its own account. A URL with a password is refused (the
file is world-readable), and no message quotes a URL. Settings of `kbf` that `setup` has no flag
for (`releases-dir`, `reapi-auth-policy`, `worker-deny-list`, `proxy-auth-file` and the tuning
keys) are the operator's; see the configuration reference. Such keys survive a re-run under the
rule in "The `kbf.toml` file".

## The `kbf.toml` flags of a frontend and an all-in-one host

A node's listeners, certificates and storage URL come from enrollment. A `frontend` or
`all-in-one` host does not enroll, so its addresses, certificates and stores are the operator's.
`setup` takes them as flags and writes each as the key of the `kbf` flag it sets, in the table of
that flag's group. An all-in-one host is always behind the proxy, so its public listeners and
certificates are the proxy's: the three files go to the `[proxy]` table and the listener flags are
refused, as for a frontend behind the proxy.

| flag | `[table] key` | frontend | all-in-one |
|---|---|---|---|
| `--listen`, `--worker-listen` | `[listener]` | optional | refused (the proxy's) |
| `--api-listen` | `[listener] api-listen` | default `127.0.0.1:8982` | refused (the proxy's) |
| `--reapi-tls-cert`, `--reapi-tls-key` | `[tls]` | both or none | both, in `[proxy]` |
| `--worker-tls-cert`, `--worker-tls-key` | `[tls]` | both with `--worker-client-ca`, or none | refused |
| `--worker-client-ca` | `[tls]` | with the worker pair, or none | required, in `[proxy]` |
| `--storage-url` (key `storage`) | `[storage-client] storage` | required | refused (derived from `--storage-port`) |
| `--storage-ca`, `--storage-client-cert`, `--storage-client-key` | `[storage-client]` | all three with `https`, none with `http` | refused |
| `--cold-store` | `[stores] cold-store` | refused | required, `file:///absolute/path` |
| `--meta-dir` | `[stores] meta-dir` | refused | required, absolute path |
| `--daemon-server` (key `server`), `--daemon-ca-cert` (`ca-cert`), `--daemon-cert` (`cert`), `--daemon-key` (`key`), `--node-id` | `[identity]` | refused | required |
| (no flag) | `[daemon] driver = "container"` | | always |

A node mode refuses all of them. Every rule is checked before anything is written (exit 2, the
flag and the rule named, a value never quoted). An address is `IP:PORT`. A file flag is an
absolute path of plain characters. A URL has no user name, password, path or query, and `http://`
storage is for a loopback host only. The files named must exist as regular files (a link is not
followed). A key must be owned by the user that reads it (the mode's unit; on an all-in-one host
the proxy's files are `kbf-server`'s and the daemon half's key is `kbf`'s), with no group or other
permission bit; a certificate or CA is owned by root or that user and not writable by group or
others. An `s3://` cold store is refused, because its credentials are in the environment, which
`setup` does not manage. `--node-id` is a DNS label. The directory that holds a key or a
certificate is not looked at, and `--cold-store` and `--meta-dir` are checked as paths only.

### The node's update directory and orders directory

The node no longer has an update client: a release reaches a host as a package that
`kbf-updater` installs. `--update-dir` and `--orders-dir` (`update-dir` and `orders-dir` in
`[daemon]`) are still accepted, hidden and ignored, with one warning at start, for one
release, because a `kbf.toml` or a plist written before the removal carries them. On Linux
`setup` still writes both keys into a `daemon` or `storage-and-daemon` host's `kbf.toml`:
`update-dir = "/var/lib/kbf-installer"` and `orders-dir = "/var/lib/kbf-installer/orders"`.
On a Mac it writes neither key and checks them instead, see "The Mac path".

## The `kbf.toml` file

`setup` writes `/etc/kbf/kbf.toml` (root, group of the service user, mode 0640) with the keys in
this order: `mode`, `[frontend] url`, `[enroll] token_file`, `[listener]`, `[tls]` (including
`api-token-file` on a frontend or all-in-one host), `[storage-client]`, `[stores]`, `[identity]`,
`[daemon]`, `[scheduler] state-store` (the database URL). On an all-in-one host this is the
sibling's file and `setup` writes a second one, `frontend.toml`, see "An all-in-one host" below.
The rule for a file that is there, either of the two:

- A file that holds only keys this run writes is rewritten to the flags.
- A file that also holds other settings is kept when the keys this run writes match, and refused
  (exit 5, the keys named, never a value) when one differs. A `listen` in the file that this run
  gives no `--listen` for is the operator's, so the file is kept or refused, never rewritten.
- A hand-set `api-listen` on a run without `--api-listen` counts as a difference, because no flag
  means the default `127.0.0.1:8982`; the message names `--api-listen` as the way forward.

Add another setting to the table it belongs to, never as a second table of the same name: TOML
refuses a table written twice, and so does `kbf` (exit 2, `duplicate key`).

## The Mac path

A Mac is a node (`--mode=daemon`) and `kbf-mac-provision` is the one writer of the daemon's
`kbf.toml` there. `setup` does not write `[daemon] update-dir` or `orders-dir` on a Mac and does
not require them; a file that has them is left as it is. The other keys
`setup` owns follow the rule above.

## What it does, in this order

1. Checks it is root and, to apply, that its own binary is root's alone. Checks the flags (every
   problem is named). Then it refuses the old layout, see the next section.
2. The `kbf-fetch` account (`_kbf-fetch` on a Mac). An account of that name that is not a system
   account with no shell and no home is refused (exit 5) and left alone. For the modes that run
   actions (`daemon`, `storage-and-daemon`, `all-in-one`) it also adds the `kbf` user's subuid and
   subgid ranges and enables lingering. The `kbf` and `kbf-server` users come from the package's
   sysusers files: a host without them is refused (exit 2), naming the user.
3. `/etc/kbf`, the installer tree `/var/lib/kbf-installer` (root, 0755) and its `orders/`
   (0700, owned by the user the mode's unit runs as; on an all-in-one host `kbf-server`, the
   proxy's).
4. Node modes: the enrollment token from `--token-file` to `/etc/kbf/enroll.token`, mode 0600,
   owned by the service user.
5. Frontend and all-in-one: the first operator token (32 random bytes as hex) in
   `/etc/kbf/api-token`, 0600, owned by the service user (on an all-in-one host `kbf-server`),
   only when that file is not there. The output names the file and never the secret.
6. `/etc/kbf/installer.toml` (root, 0644): the floor, for a `frontend` release source the node
   certificate and key paths that enrollment writes (`pki/daemon.*` for `daemon` and
   `storage-and-daemon`, `pki/storage.*` for `storage`), and for the Postgres modes `database`.
   The release source and the keys that go with it are not written: a file that has them (a host
   set up by an earlier build) keeps them, and a host with no file gets `release_source =
   "frontend"`, the default. Settings a flag does not cover are kept. The file is closed: one that
   does not parse is left alone (exit 5). The operator's `[proxy]` table is kept.
7. Frontend and all-in-one: the state database's schema is created or brought forward. With
   `--create-local-database` the role and the database are made first. A server that cannot be
   reached, or refuses a login, stops `setup` here (exit 1) with `installer.toml` written and no
   `kbf.toml` or unit; running it again finishes.
8. `/etc/kbf/kbf.toml`, as above (on an all-in-one host `/etc/kbf/frontend.toml` first).
9. The units, last. On Linux `systemctl enable`, with `--now` unless `--no-start`: the mode's unit
    (only when a release is installed), and `kbf-installer.service`. `kbf-frontend.service`,
    `kbf-all-in-one.service` (kept for a host of the earlier layout) and the proxy units, with
    `kbf-storage-and-daemon.service` beside them on an all-in-one host, are enabled and never started by `setup`: starting
    one takes the database, the TLS files and the port. A unit that is enabled already is left
    alone, and nothing is restarted. On a host that an earlier build set up with
    `kbf-backup.timer`, `setup` also disables the timer if it is enabled, removes the drop-in an
    earlier `setup` wrote for `kbf-backup.service` (and its directory), and reloads systemd; a
    host that never had them gets no such step. On a Mac it
    runs `launchctl bootstrap system /Library/LaunchDaemons/<label>.plist` for
    `com.komira.kbf-installer` (unless `--no-start`) and for `com.komira.kbf-daemon` (only with a
    release and without `--no-start`), each only if the job is not loaded.

Every step is planned only where the host differs, so the same flags again is "nothing to do". A
run that stopped part way is finished by running it again. `kbf.toml` and the units come last, so
a stopped run leaves no config that a unit would start from. Changed flags rewrite the files they
own atomically and never restart anything.

## The old layout, and a migrated host

On Linux a regular file at `/etc/systemd/system/<the mode's unit>` (for the proxy, also
`kbf-proxy.socket` and `kbf-proxy.service`) shadows the packaged unit: systemd reads `/etc` before
`/usr/lib`, so the host would keep the old flags. `setup` refuses it (exit 5, `unit_shadow`) before any
step, with this message, and never deletes a unit it did not write:

```text
<path> is a regular file and shadows the packaged unit <unit> (systemd reads /etc before /usr/lib, so enabling would keep its old flags): move it aside (mv <path> <path>.old; systemctl daemon-reload) and run setup again; setup never deletes a unit
```

A link there (`systemctl link`) is not refused.

## A frontend behind the proxy: `--proxy`

The proxy is the bundled `kbf-installer proxy`, which owns the public port, starts the frontend
children on loopback ports and terminates TLS. `--mode=frontend --proxy` (Linux only) sets a host
up for it. It is never implied: a re-run on a host that has a `[proxy]` table but not the flag
keeps the table, enables `kbf-frontend.service` as before and says so.

| flag | written as |
|---|---|
| `--reapi-tls-cert`, `--reapi-tls-key` (required) | `[proxy] server_cert_file`, `server_key_file` |
| `--worker-client-ca` (required) | `[proxy] client_ca_file` |
| `--proxy-base-port` (default 18000, 1024 to 65521) | `[proxy.upstream] base_port`: slot A uses it and the four after, slot B ten above |
| `--buck2-auth none\|bearer\|mtls`, `--buck2-token-file` | `[proxy] buck2_auth`, `buck2_token_file` (a key file only with `bearer`) |
| (none) | `[proxy] operator_token_file`: the operator token `setup` makes |
| (none) | `[proxy] remote_operator_file`: optional, off when absent; a path to the allow-list of a CI job's GitHub OIDC token (`[remote_operator]` table; claims in `docs/design/frontend-proxy.md` section 4a). Written by hand; a rerun keeps it |
| (none) | `[proxy] ui = true`: the proxy serves the read-only web UI at `/ui`, open (no certificate or token), `GET` and `HEAD` only; set `ui = false` in `installer.toml` to make `/ui` a 404 |

`setup` enables `kbf-proxy.socket` and `kbf-proxy.service` instead of `kbf-frontend.service`,
writes none of the three TLS files into `kbf.toml` (the children speak plain h2c behind the proxy),
and refuses `--listen`, `--worker-listen`, `--api-listen`, `--worker-tls-cert` and
`--worker-tls-key`. It refuses a base port whose ranges cover 8980, 8982 or 8990, and a
`kbf.toml` that still has `[tls]` keys. The table is checked by the proxy's own check, so `setup`
cannot write one the proxy would refuse at start. If `kbf-frontend.service` is enabled `setup`
names it and leaves it; stop and disable it before starting the proxy.

`kbf-proxy.socket` ships listening on port 8980. To change it set `listen = "host:port"` in the
`[proxy]` table of `installer.toml`; on Linux `setup` then writes
`/etc/systemd/system/kbf-proxy.socket.d/listen.conf` with `ListenStream=` (empty, to clear the
packaged port) and `ListenStream=<address>`. With no `listen` it removes the drop-in. It never
restarts the socket: it says to run `systemctl daemon-reload` and `systemctl restart
kbf-proxy.socket`.

## An all-in-one host: `--mode=all-in-one --proxy`

An all-in-one host is the proxy, its frontend children and, beside them, a sibling process that
holds the storage and the daemon: `kbf-storage-and-daemon.service`. It needs `--proxy` (without it
`setup` refuses, exit 2) and takes the proxy's flags and `--database` as a frontend does; its own
flags are the cold store, the metadata directory and the daemon half's identity. Linux only.
`--storage-port` (default 8990, 1024 to 65535, clear of the children's ports) is the loopback
port the sibling listens on and the children dial in plain text.

| file | owner, mode | holds |
|---|---|---|
| `/etc/kbf/kbf.toml` | root:`kbf`, 0640 | the sibling's: `mode = "storage-and-daemon"`, `[listener] storage-listen = "127.0.0.1:<port>"`, `[stores]`, `[identity]`, `[daemon] driver`. No `[frontend]` or `[enroll]` table: `kbf` reads it as plain flags, and the sibling is a farm of one that is not registered with any frontend |
| `/etc/kbf/frontend.toml` | root:`kbf-server`, 0640 | the children's: `mode = "frontend"`, `[scheduler] state-store`, `[tls] api-token-file`, `[storage-client] storage = "http://127.0.0.1:<port>"` |
| `/etc/kbf/installer.toml` | root:root, 0644 | the `[proxy]` table, and the top-level key `frontend_config = "/etc/kbf/frontend.toml"`, which makes the proxy start its children with `--config=` that file. `setup` writes the key on an all-in-one host only, and removes it from any other |
| `/etc/kbf/api-token` | `kbf-server`, 0600 | the first operator token, as for a frontend |

The units are `kbf-proxy.socket`, `kbf-proxy.service` and `kbf-storage-and-daemon.service`, in
that order. They are enabled and never started by `setup` (starting one takes the database, the
TLS files and the ports): start the sibling first, since the children dial it, then the proxy. The
proxy's files (the certificate pair and the client CA) and the operator token belong to
`kbf-server`; the daemon half's identity (`--daemon-server`, `--daemon-ca-cert`, `--daemon-cert`,
`--daemon-key`, `--node-id`) is the operator's, as before, and belongs to `kbf`. `--daemon-server`
now dials the proxy, so its host must be a name on the proxy's certificate. The `orders/`
directory of the installer's tree is `kbf-server`'s here, and the `kbf` account gets the subid
ranges and lingering that run the sibling's containers.

`frontend.toml` is written before `kbf.toml`, which is the file the sibling's unit waits for, and
both follow the rule for a file that is there ("The `kbf.toml` file").

**A restart of the sibling is felt by clients.** The sibling is no longer swapped by the proxy,
and a farm of one has no map for the proxy to hold requests against: while
`kbf-storage-and-daemon.service` is restarted, requests through the proxy that need the storage
answer `UNAVAILABLE`, and the daemon half drains first (`drain-deadline-secs`, 600 by default).
A frontend swap does not have this cost: only the children are swapped.

**The sibling's listener is plain text and has no authentication.** It serves h2c on loopback, so
any process on the host can reach `127.0.0.1:<port>`. The old all-in-one had no such socket.
Whether an action running in a container can reach the host's loopback depends on the container
driver's network; `setup` does not check it, so treat the port as open to every local user and to
what the host's actions can reach.

**The update path does not know this layout yet.** The installer's root half decides what a host
runs from the `mode` of its `kbf.toml`, which on this host is `storage-and-daemon`. It therefore
refuses `frontend_update` orders here (the host "runs no frontend"), and the unit it restarts is
the sibling's. Until it reads the children's file from `installer.toml`, the children of an
all-in-one host are not swapped through the update API.

**A host of the earlier layout is moved.** A `kbf.toml` that says `mode = "all-in-one"` is the
earlier layout, one process for everything (`kbf-all-in-one.service`). Run the same command as for
a fresh host, with `--proxy`; without it `setup` refuses (exit 5, `needs_operator`), naming the
file and saying what to add, and changes nothing. `setup` then splits the file:

- The original is kept first, root's 0600, as `kbf.toml.all-in-one` in
  `/var/lib/kbf-installer/migration-units/` (0700). It is made once and never overwritten. A copy
  that is there and holds another file is refused (exit 5) before anything is written, since
  neither may be lost.
- The settings carry over as flags: a flag this run does not give takes the old file's value
  (`--cold-store`, `--meta-dir`, the identity rows `--daemon-server`, `--daemon-ca-cert`,
  `--daemon-cert`, `--daemon-key` and `--node-id`, the REAPI pair, `--worker-client-ca`, and
  `[scheduler] state-store` for `--database`), and a flag that is given overrides it, except
  `--cold-store` and `--meta-dir`: the move keeps both where they are, so a value other than the
  file's is refused (exit 5, the flag named, no value printed) rather than starting the sibling on
  a metadata directory with no log of the cold store. They are then checked as a fresh host's are,
  so a value the file lacks is reported as its missing flag, and a carried key or certificate file
  must belong to the user the new layout reads it as (the REAPI key to `kbf-server`, which the
  earlier layout's did not; a REAPI certificate or client CA owned by `kbf` is refused too, since
  a certificate must be root's or `kbf-server`'s). The stores go to the sibling's `kbf.toml`, the
  state store to `frontend.toml`, the REAPI pair and the client CA to `[proxy]` of
  `installer.toml`; the metadata directory and the cold store are the same.
- The operator token stays the same token. If it is at `/etc/kbf/api-token` it is rewritten with
  its own bytes as `kbf-server`'s (the earlier layout's belonged to `kbf`, and the proxy's
  children could not read it); a token at another path that `[tls] api-token-file` names is
  copied there and the old file is left. If both files are there and differ, the one at
  `/etc/kbf/api-token` is kept and the output names the other as not used.
- A key with no place in the new layout (the listeners, `worker-tls-cert` and `worker-tls-key`,
  anything the operator added) is named in the output as `[table] key` and never printed; the
  original keeps it.
- `setup` stops and starts none of the services of the move (as on any run it enables
  `kbf-installer.service` and starts it): the earlier unit drains without a time limit and holds
  the metadata directory the sibling needs. It prints the commands, in order: stop
  `kbf-all-in-one.service` and wait until it has exited, disable it, `systemctl daemon-reload`,
  then start `kbf-storage-and-daemon.service` and the proxy units. A start that finds the metadata
  directory still held exits 75 and is retried. Until the unit is disabled both the earlier unit
  and the new ones are enabled, so do not reboot the host in between. Do not restart the earlier
  unit once `kbf.toml` is replaced; for that reason the move does not print the note that a
  running kbf keeps the old file until it is restarted.

A moved host's `kbf.toml` no longer says `all-in-one`, so a second run is a fresh host's: it plans
nothing new, does not look at the copy, and needs every flag, since the moved files are not read
for values. That includes a run repeated after a stop part way (`kbf.toml` is written last of the
files, before the units are enabled): give the flags of the first run again, or the missing ones
are reported by name (exit 2). A dry run shows the plan, including the copy, and writes nothing.

## The state database

`frontend` and `all-in-one` keep their state in one Postgres database on the host. The package only
suggests the server, because node hosts use none. `setup` refuses clearly when no server answers
at the URL's socket.

- **`--create-local-database`.** `setup` runs `/usr/bin/psql` as the operating-system user
  `postgres` over the Unix socket the URL names (peer authentication, no password) and creates, if
  missing, a login role (no password, not a superuser, no CREATEDB, CREATEROLE or REPLICATION) and
  a database it owns. It alters nothing that exists: a role that cannot log in or holds any
  privilege, and a database another role owns, are refused (exit 1). Names are letters, digits,
  `_` and `-` (at most 63); the role is neither `postgres` nor `pg_`-prefixed; the socket directory
  is absolute; one server only.
- **`setup` never edits `pg_hba.conf` or `pg_ident.conf`.** It logs in the way kbf will (as root
  with the URL's role, and as the service account when it exists) and, when the server refuses,
  prints the rules to add: above the first `local` line of `pg_hba.conf`
  `local <database> <role> peer map=kbf`, and in `pg_ident.conf` the lines `kbf root <role>` and
  `kbf <service> <role>` (`<service>` is `kbf-server`: on an all-in-one host it is the proxy's
  child that holds the connection, not the sibling that runs as `kbf`).
  Reload the server and run `setup` again. The map name is any name used on both sides.
- **A remote or managed server.** `setup` holds no password and does not reach a remote
  superuser. Create the role and database, make the server admit the logins, and run `setup`
  without `--create-local-database`: it checks that it can log in, then creates or brings forward
  the schema.

Backups of the database are the operator's: Postgres is a runtime dependency that the operator
provides, and its own backup and restore tools apply. Neither `setup` nor the package runs one,
and `kbf-installer` has no `backup` or `restore` verb.

## Exit codes

| code | meaning in `setup` |
|---|---|
| 0 | done, or a dry run that found nothing wrong |
| 1 | failed: the database cannot be reached, a step failed, no free id for the fetch account or no subid room, an unreadable migration journal |
| 2 | usage or a missing prerequisite: not root (`setup writes /etc and manages services: run it with sudo`), a binary root does not own alone, bad flags, a bad token file, a missing `kbf` or `kbf-server` user, a Mac mode other than `daemon` |
| 5 | only a person can go on: the old layout unmigrated, a shadowing unit, a `kbf.toml` or `installer.toml` `setup` will not overwrite, a floor that would go down, a wrong fetch account, a Mac `kbf.toml` that is not provisioned |

`setup` does not take a lock, so it never exits 4.
