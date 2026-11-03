# The host agent: units and jobs

The files of a host's update agent (`docs/design/updates.md`). On Linux the `kbf` deb ships and
enables the helper's and the agent's units and the agent's account ("A host that runs the package",
below); the other files here an operator places. The binaries are in no package: they are release
assets (`docs/deploy/host-install.md`, "Getting the binary"), placed at fixed paths no release
writes.

| file | what |
|---|---|
| `linux/kbf-host-local-frontend.service`, `linux/host-install-frontend.toml` | the same socket for a host whose frontend is a container (`--frontend-container`), and an example of its `/etc/kbf/host-install-frontend.toml`; use this unit or the next, not both |
| `linux/kbf-host-local.service` | in the `kbf` deb, enabled: `kbf-host-install --serve --layout deb`, as root: the local socket (`/run/kbf-host-install/local.sock`, mode 0600, owned by `kbf-updater`) that installs the attested package on trial, restarts `kbf-daemon.service`, and goes back by itself at the deadline (`docs/deploy/host-install.md`, "On a Linux daemon host that runs the package"). A host laid out by hand places this file itself without `--layout deb`, with a `--restart-unit` for each unit it runs; it moves to the package once with `kbf-host-install --migrate-to-package` (`docs/deploy/native-package-migration.md`) |
| `linux/kbf-updater-agent.service` | in the `kbf` deb, enabled: `kbf-updater agent`, as `kbf-updater`: the heartbeat to the coordinator and the host's A/B through that socket |
| `linux/kbf-updater.conf` | in the `kbf` deb, as `/usr/lib/sysusers.d/kbf-updater.conf`: the sysusers.d entry of the `kbf-updater` account (no login, no home), and of the group `updater-register` whose members may register with the root part |
| `linux/kbf-updater-root.service` | `kbf-updater root`, as root: the root part of the generic host updater (below) |
| `linux/kbf-updater-root.conf` | the tmpfiles.d entry of `/run/updater`, the directory of the registration socket: root's, group `updater-register`, mode 0750 |
| `linux/updater.d/kbf.toml`, `linux/install-declaration.txt` | the declaration of the kbf hook, and the commands that install it as `/etc/updater.d/kbf.toml`, root-owned, mode 0644 |
| `enroll-example.txt` | the enrollment command, as the tests read it |
| `macos/com.komira.kbf-host-install-local.plist` | the same socket on a Mac, as root, in `/var/db/kbf-host-install` |
| `macos/com.komira.kbf-updater-agent.plist` | the agent on a Mac, as `_kbf-updater` |

The values in the files are examples (`coordinator.farm.example`, `frontend.farm.example`, the
state directory): a host that runs the package gives the farm's in a drop-in (below); one laid out
by hand edits its copies. `crates/kbf-updater/tests/agent_packaging.rs`
reads these files and checks that every command line parses as the programs parse it, that
the two ends agree on the socket and on the account that may connect to it, and that the agent
is not root.

## A host that runs the package

The `kbf` deb installs `kbf-host-local.service` and `kbf-updater-agent.service` to
`/usr/lib/systemd/system` and enables both with its own links in
`/usr/lib/systemd/system/multi-user.target.wants/`, with no script (`packaging/linux/README.md`).
Both are gated on their programs, which no package carries. A new Linux daemon host is then:

1. **The package**: `apt-get install ./kbf-<commit>-linux-<arch>.deb` (or `dpkg --install`), of the
   release the farm runs.
2. **The two programs**: `kbf-host-install` and `kbf-updater` of the same release, from their
   verified tarballs (`docs/deploy/host-install.md`, "Getting the binary"), placed at
   `/usr/libexec/kbf/kbf-host-install` and `/usr/libexec/kbf/kbf-updater`, `root:root`, mode 0755,
   in directories only root can write. No release writes these paths: an update installs the
   package and leaves them as they are.
3. **The farm's names**: one drop-in, the same file on every host of the farm, that gives the
   agent the farm's coordinator and frontend. `ExecStart=` empty first clears the shipped line:

   ```ini
   # /etc/systemd/system/kbf-updater-agent.service.d/farm.conf
   [Service]
   ExecStart=
   ExecStart=/usr/libexec/kbf/kbf-updater agent --coordinator <coordinator>:8443 --frontend <frontend>:8981 --enroll-frontend <frontend>:8444 --state-dir /var/lib/kbf-updater --cordon-dir /var/lib/kbf-updater --channel stable
   ```

   The helper's channels are `/etc/kbf/host-install-releases.toml`, as for any Linux host
   (`docs/deploy/host-install.md`, "On Linux"). The shipped helper restarts `kbf-daemon.service`:
   a host whose kbf runs under another mode unit gives the helper's whole line in a drop-in of
   `kbf-host-local.service` the same way, with its own `--restart-unit`.
4. **Boot**, or run what a boot runs: `systemd-sysusers` (the `kbf-updater` account), then
   `systemctl daemon-reload` and `systemctl start kbf-updater-agent.service` (which starts the
   helper too).
5. **Enroll** (below). The agent started before the host was enrolled says so once in its journal
   (`not enrolled: ... waiting for kbf-updater enroll`), calls nothing, and starts by itself once
   `kbf-updater enroll` has run: no restart is needed.

To turn a packaged unit off, `systemctl mask` it; an upgrade keeps the mask. A host whose frontend
runs as a container masks `kbf-host-local.service` and runs `kbf-host-local-frontend.service`
instead (the socket is the same). A host laid out by hand that has its own copies under
`/etc/systemd/system` keeps them: they take precedence over the package's.

**Going back below the first release that ships the units.** dpkg removes the files an older
package does not list, so installing a `kbf` deb older than the first release that ships these
units deletes them, and their links, on a host that got them from the package. The running agent
and helper go on until the next boot, and then neither starts. Before a farm is set back that far,
copy both units to `/etc/systemd/system` on such hosts and `systemctl enable` them there.

## Enrolling a host

A host gets its updater certificate from the farm CA with a one-time token. On the frontend,
the operator makes the token for this host (`POST /v1/enroll/tokens`, `docs/api.md`) with
`"roles": ["updater"]`, the host's `node_id` and its `host_role` (`storage`, `daemon` or
`mac-daemon`); it is good for one use. On the host, as the agent's account (`kbf-updater`, or
`_kbf-updater` on a Mac), with the token in a file of that account's, mode 0600:

```sh
install -d -o kbf-updater -m 0700 /var/lib/kbf-updater
sudo -u kbf-updater sh -c 'umask 077; cat > /var/lib/kbf-updater/enroll.token'   # paste the token, end with ^D
sudo -u kbf-updater /usr/libexec/kbf/kbf-updater enroll --frontend frontend.farm.example:8444 --node-id host-1 --token-file /var/lib/kbf-updater/enroll.token --state-dir /var/lib/kbf-updater
```

The key is made on the host and stays there (mode 0600, the account's); only a request signed
by it is sent. The command checks the frontend against the CA pin in the token, checks what it
gets back, writes `pki/updater.key`, `pki/updater.pem` and `pki/ca.pem` into the state
directory, and removes the token file. The agent then starts (or is restarted, if it was
waiting) and renews the certificate itself at half its life. `enroll-example.txt` is that
command, which `crates/kbf-updater/tests/agent_packaging.rs` checks against the unit's
`--state-dir` and the program's flags.

## What the agent needs

- **Its certificate**: from the enrollment above, in the state directory. The certificate says
  which node this is and what the host runs; the agent's flags say neither. (Placed files still
  work, with `--client-cert`, `--client-key` and `--ca` in place of `--state-dir`; nothing
  renews them. The agent records the cordons it makes in `--cordon-dir`, which defaults to the
  state directory, else the directory of `--client-key`; with placed files in a root-owned
  directory (`/etc/kbf`) under the unit's `ProtectSystem=strict`, that default is not writable,
  so give `--cordon-dir /var/lib/kbf-updater` (the unit's `StateDirectory`) or a directory named
  in `ReadWritePaths=`. The agent checks it at start and refuses to start with one it cannot
  write.)
- **A configuration for the helper's channels**: on Linux `/etc/kbf/host-install-releases.toml`,
  on a Mac the existing `host-install.toml`. See `docs/deploy/host-install.md`, "The local socket".
- **No operator token, as shipped.** The drain of the node, its undrain and its status (a storage
  host's: its own storage health) are calls to the frontend's worker listener (`--frontend`, the
  address daemons dial) made over mutual TLS with the host's own certificate. The frontend
  serves exactly those four, for the node in the certificate and no other, and nothing else from
  an updater's certificate; no token is involved (`docs/design/updates.md`, "Trust"). The
  worker listener must offer `http/1.1` beside `h2`, which `--worker-tls-*` does. An operator who
  wants the full token on a host adds `--operator-api` and `--token-file` to the agent's command
  line (the API is on the frontend's loopback), knowing that the agent's account can then read it;
  the agent uses it instead of the certificate.
- **Where the certificate is renewed**: `--enroll-frontend`, the frontend's enroll listener
  (`--enroll-listen`), which is the address the enrollment above was given.
- **A frontend host** runs its frontend as a container (`linux/kbf-host-local-frontend.service`
  in place of `kbf-host-local.service`, with `/etc/kbf/host-install-frontend.toml`; see
  `docs/deploy/host-install.md`, "A frontend container"). Its agent runs the A/B through the
  helper, which drains the container through the frontend's own operator API, recreates it from the
  new attested image and puts the old image back when the new one is not ready. The agent drains no
  node unless `--daemon-node` names the daemon the host also runs.

## The root part

`kbf-updater root` (`linux/kbf-updater-root.service`) is the root half of the generic host
updater (`docs/design/host-updates.md`, 3.4 and 3.5). It reads the declarations in
`/etc/updater.d` (a declaration that is not root's, or that others may write, is refused, and only
its service), keeps a step record per service under `/var/lib/updater/state`, and runs each step
of a service's hook as a transient systemd unit with the rights the declaration names. The agent
reaches it only through its socket (`/run/kbf-updater-root/root.sock`, mode 0600, the agent's
account's), with "update this service to this version" or "where is this service's update";
it never names a step. At its start, before it answers, it rolls back a `cutover` a boot cut
off and runs each declaration's boot entry; only then does it tell systemd it is ready
(`Type=notify`), so the agent's unit, ordered after it, starts once that is done. One root part
holds the state directory at a time.

A declaration that asks for more than the active one, and the first declaration of a service, is
installed only once an operator has approved its digest, which the deferred answer gives:

```sh
sudo /usr/libexec/kbf/kbf-updater approve kbf <sha256 of the declaration>
```

A root part started with another `--approvals FILE` reads that file; approve into it with
`kbf-updater approve --approvals FILE kbf <sha256>`.

The kbf declaration runs `kbf update-hook` as root, so it is approved once per host. Install it
with the commands of `linux/install-declaration.txt`.

What is not moved yet: the root part runs releases another step has fetched, verified and sealed
under `/var/lib/updater/releases`, and that step is not built. The agent keeps updating kbf
through the helper (`kbf-host-local.service`) and its `--service` flags until it is.
