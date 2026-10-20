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
    --database='postgres:///kbf?host=/run/postgresql' \
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
server, the database and the role exist before `setup` runs (the operator provides them);
`--database` says where, as `postgres:///NAME?host=SOCKET_DIR`. A URL with a password is
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
   The operator's `[proxy]` table is kept (written back whole, defaults filled in). Its `listen`
   key is read here, as below.
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
