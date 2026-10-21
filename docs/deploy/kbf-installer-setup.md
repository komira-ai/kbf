# kbf-installer setup

`kbf-installer setup` is the first configuration of a host. It makes the accounts and
directories kbf needs, writes `installer.toml` and `kbf.toml`, installs the first release when it
is given one, and enables the units for the host's mode. The contract is
[the installer design](../design/kbf-installer.md) (sections 2 and 3); this page is how the verb
behaves in the code it describes. A host that has the old layout is moved first with
[`kbf-installer migrate`](kbf-installer-migrate.md); a Mac has its own page,
[installing on macOS](kbf-installer-macos.md).

```text
sudo kbf-installer setup --mode=daemon \
    --frontend-url=https://frontend.example.invalid:8443 --token-file=/srv/kbf/enroll.token \
    --release-source=url --release-url=https://releases.example.invalid/kbf --commit=<40 hex>   # dry run
sudo kbf-installer --apply setup ...                                                          # do it

sudo kbf-installer setup --mode=frontend --release-url=https://releases.example.invalid/kbf \
    --database='postgres:///kbf?host=/run/postgresql&user=kbf' --create-local-database \
    --listen=192.0.2.10:8980 --worker-listen=192.0.2.10:8981 \
    --worker-tls-cert=/etc/kbf/worker.pem --worker-tls-key=/etc/kbf/worker.key \
    --worker-client-ca=/etc/kbf/ca.pem \
    --storage-url=https://storage.example.invalid:8983 --storage-ca=/etc/kbf/ca.pem \
    --storage-client-cert=/etc/kbf/worker.pem --storage-client-key=/etc/kbf/worker.key
```

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

| mode | `--frontend-url` | `--token-file` | `--database` | release source |
|---|---|---|---|---|
| `storage`, `daemon`, `storage-and-daemon` | required | required | refused | `frontend` (default), `url` or `bundled` |
| `frontend`, `all-in-one` | refused | refused | required | `url` (default; `--release-url` required) or `bundled` |

The modes in the first row enroll with a frontend when they start, so they need its URL
(`https://host[:port]`, nothing after it) and an enrollment token file (an absolute path, one
word, mode 0600 or tighter, at most 4 KiB). The other two do not enroll; they keep their state in
Postgres, which the operator provides before `setup` runs. There is no `node` mode, and a Mac
accepts only `--mode=daemon`.

| flag | meaning |
|---|---|
| `--release-source url\|frontend\|bundled`, `--release-url` | where releases come from; `url` needs an `https` `--release-url`, `frontend` and `bundled` refuse one. `bundled` fetches nothing and takes no `--commit`: it writes `release_source = "bundled"`, and on a host with no release installs the one the package carries (checked by the same verify pipeline as a fetched one) between `installer.toml` and `kbf.toml`, and starts the floor on `seq` at that release's `seq`. On a host that has a release, `bundled` is refused (exit 2, before anything is written, a dry run too) unless the `kbf` that runs there declares the relay, `"relay": 1` or more in `kbf --version --json`: without it the frontend is never told that the host fetches nothing. Update `kbf` by the usual path first, then run setup again. Order of an upgrade: an installer whose reader refuses keys it does not know refuses to stage a `kbf` that prints `relay`, so the installers go first |
| `--release-ca` | for `url`: a PEM file of the CAs the file server's certificate is checked against instead of the system's (an absolute path, a copy owned by root); written as `release_ca`, not read by a fetch yet; `setup` does not check the file's owner, so keep it root-owned in a root-owned directory |
| `--frontend-worker-url`, `--frontend-ca`, `--frontend-ca-sha256` | for `frontend`, all three together: the frontend's worker listener (`https://host[:port]`), a copy of the CA that authenticates it and that copy's sha256; written as `frontend_url`, `frontend_ca`, `frontend_ca_sha256`, not read by a fetch yet; the CA copy must be root-owned in a root-owned directory (not checked by `setup`) |
| `--commit` | the first release (40 hex); needs `--release-source url` |
| `--min-generation N` | the floor for orders; it only goes up, `0` is refused, and a lower value than `installer.toml` holds is refused (exit 5) |
| `--database`, `--create-local-database` | the state database, see below |
| `--no-start` | enable units without starting them |
| `--installer-config` | accepts only the platform's own `installer.toml` path; it is a flag of `run` |
| `--proxy`, `--proxy-base-port`, `--buck2-auth`, `--buck2-token-file` | a frontend behind the bundled proxy, see below |

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
that flag's group.

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
| (no flag) | `[daemon] driver = "container"` | | always |

A node mode refuses all of them. Every rule is checked before anything is written (exit 2, the
flag and the rule named, a value never quoted). An address is `IP:PORT`. A file flag is an
absolute path of plain characters. A URL has no user name, password, path or query, and `http://`
storage is for a loopback host only. The files named must exist as regular files (a link is not
followed). A key must be owned by the user the mode's unit runs as, with no group or other
permission bit; a certificate or CA is owned by root or that user and not writable by group or
others. An `s3://` cold store is refused, because its credentials are in the environment, which
`setup` does not manage. `--node-id` is a DNS label. The directory that holds a key or a
certificate is not looked at, and `--cold-store` and `--meta-dir` are checked as paths only.

### The node's update directory and orders directory

A `daemon` or `storage-and-daemon` host runs a daemon that takes update orders from the
frontend. On Linux `setup` writes two more keys into its `kbf.toml`, in the `[daemon]` table:
`update-dir = "/var/lib/kbf-installer"` and `orders-dir = "/var/lib/kbf-installer/orders"`.
Without them the node updates by hand and ignores every order. The installer writes
`status.json` into the first and reads the node's orders from the second. `all-in-one` gets
neither key, because its in-process daemon refuses `--update-dir`. On a Mac `setup` writes neither
key and checks them instead, see "The Mac path".

## The `kbf.toml` file

`setup` writes `/etc/kbf/kbf.toml` (root, group of the service user, mode 0640) with the keys in
this order: `mode`, `[frontend] url`, `[enroll] token_file`, `[listener]`, `[tls]` (including
`api-token-file` on a frontend or all-in-one host), `[storage-client]`, `[stores]`, `[identity]`,
`[daemon]`, `[scheduler] state-store` (the database URL). The rule for a file that is there:

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
`kbf.toml` there. `setup` does not write `[daemon] update-dir` or `orders-dir` on a Mac. It
checks the file: `update-dir` must be there and be the installer's state directory
(`/var/db/kbf-installer`), and `orders-dir` must be absent (the node uses `<update-dir>/orders`)
or be the installer's. A missing or different key, or no file, is refused with exit 5, naming the
key and saying that `kbf-mac-provision` renders it: run it again, then `setup`. The other keys
`setup` owns follow the rule above.

A Mac that `migrate` adopted from the Mac kit (the journal says `kit` and phase `done`) has the
kit's `kbf.toml`. `setup` then keeps every key the kit wrote byte for byte and only adds the keys
it needs that are missing; a key that is there with another value is refused (exit 5), naming the
key. See [installing on macOS](kbf-installer-macos.md).

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

1. Checks it is root and, to apply, that its own binary is root's alone. Checks the flags (every
   problem is named). Then it refuses the old layout, see the next section.
2. The `kbf-fetch` account (`_kbf-fetch` on a Mac). An account of that name that is not a system
   account with no shell and no home is refused (exit 5) and left alone. For the modes that run
   actions (`daemon`, `storage-and-daemon`, `all-in-one`) it also adds the `kbf` user's subuid and
   subgid ranges and enables lingering. The `kbf` and `kbf-server` users come from the package's
   sysusers files: a host without them is refused (exit 2), naming the user.
3. `/etc/kbf`, the installer tree `/var/lib/kbf-installer` (root, 0755) and its `orders/`
   (0700, owned by the user the mode's unit runs as).
4. Node modes: the enrollment token from `--token-file` to `/etc/kbf/enroll.token`, mode 0600,
   owned by the service user.
5. Frontend and all-in-one: the first operator token (32 random bytes as hex) in
   `/etc/kbf/api-token`, 0600, owned by the service user, only when that file is not there. The
   output names the file and never the secret.
6. `/etc/kbf/installer.toml` (root, 0644): the release source, the floor, for the `frontend` source
   the node certificate and key paths that enrollment writes (`pki/daemon.*` for `daemon` and
   `storage-and-daemon`, `pki/storage.*` for `storage`), and for the Postgres modes `database`.
   Settings a flag does not cover are kept. The file is closed: one that does not parse is left
   alone (exit 5). The operator's `[proxy]` table is kept.
7. Frontend and all-in-one: the state database's schema is created or brought forward. With
   `--create-local-database` the role and the database are made first. A server that cannot be
   reached, or refuses a login, stops `setup` here (exit 1) with `installer.toml` written and no
   release, `kbf.toml` or unit; running it again finishes.
8. `--commit`: the first release, through the installer's fetcher (the fetch child and the verify
   pipeline), then made `releases/current` with no trial. The fetcher is the host's own: it reads
   `installer.toml` and `installer.conf` and looks up the fetch user when `--commit` asks, so the
   files this run wrote count. A fetch that cannot be made (the `frontend` source, which this build
   does not fetch from yet; a missing fetch user) or that fails stops `setup`
   here with exit 1, before `kbf.toml` and the units. The branches a release may be attested for
   are `SOURCE_REFS` of `installer.conf` (default `refs/heads/main`); `source_refs` of
   `installer.toml` can only narrow that list, and a list with no ref in common refuses the fetch.
   When a release is installed already, `--commit` is not fetched and a note says so.
9. `/etc/kbf-installer/deploy.json`, only when `--deploy-stage` is given (see "The deploy config of a frontend and an all-in-one host").
10. `/etc/kbf/kbf.toml`, as above.
11. The units, last. On Linux `systemctl enable`, with `--now` unless `--no-start`: the mode's unit
    (only when a release is installed), and `kbf-installer.service`. `kbf-frontend.service`,
    `kbf-all-in-one.service` and the proxy units are enabled and never started by `setup`: starting
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

`setup` refuses to run while the old layout of the retired `kbf-helper` and `kbf-deploy host` is
the host's: its state (`/var/lib/kbf-deploy`, on a Mac `/var/db/kbf-deploy`) is there and no
migration finished. The two never both manage a host. It exits 5 with `unmigrated` and the
message:

```text
the old layout's state is still the host's (<path>) and no migration finished: kbf-installer and the retired kbf-helper/kbf-deploy never both manage a host; run `kbf-installer migrate --apply --fetch-user kbf-fetch [--unit UNIT]` (without --apply it is a dry run), then run setup again
```

(`_kbf-fetch` on a Mac.) The test is the migration's journal, not the old paths. A host that
`migrate` finished (journal phase `done`) passes, and it keeps `/usr/local/kbf` and
`/var/lib/kbf-deploy` on purpose as its rollback path, so their presence alone does not stop
`setup`. A migration that was rolled back is not finished. A journal that cannot be read stops
`setup` with exit 1 (`migrate`); a Mac that was rolled back to the kit is refused with exit 5 until the
package is installed again.

On Linux a regular file at `/etc/systemd/system/<the mode's unit>` (for the proxy, also
`kbf-proxy.socket` and `kbf-proxy.service`) shadows the packaged unit: systemd reads `/etc` before
`/usr/lib`, so the host would keep the old flags. `setup` retires such a file only if
`migrate --unit` journalled it by name. Then it:

1. keeps the bytes in `migration-units/<unit>.retired` under the installer's state directory;
2. removes the file;
3. runs `systemctl daemon-reload`;
4. enables the packaged unit, even if the old file's link made it look enabled.

The dry run prints `DRY-RUN: retire <path> (shadows the packaged unit; original kept)`.
`kbf-installer migrate --rollback` puts a retired file back. Any other file there is refused (exit
5, `unit_shadow`) with this message, and `setup` never deletes a unit it did not see the migration
take over:

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
  `kbf <service> <role>` (`<service>` is `kbf-server` for `frontend`, `kbf` for `all-in-one`).
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
| 1 | failed: the database cannot be reached, `--commit` (the fetch could not be made or did not finish: a `frontend` source, no fetch user, a source that cannot be reached), a step failed, no free id for the fetch account or no subid room, an unreadable migration journal |
| 2 | usage or a missing prerequisite: not root (`setup writes /etc and manages services: run it with sudo`), a binary root does not own alone, bad flags, a bad token file, a missing `kbf` or `kbf-server` user, a Mac mode other than `daemon` |
| 3 | the fetcher rejected the release: what arrived failed a check (size, digest, attestation, shape, the binary's report) |
| 5 | only a person can go on: the old layout unmigrated, a shadowing unit, a `kbf.toml` or `installer.toml` `setup` will not overwrite, a floor that would go down, a wrong fetch account, a Mac `kbf.toml` that is not provisioned |

`setup` does not take a lock, so it never exits 4.
