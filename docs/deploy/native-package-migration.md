# Moving a hand-placed host to the `kbf` package

A daemon host set up before the packages kept each release in
`/var/lib/kbf-installer/releases/<commit>`, with a unit of its own in `/etc/systemd/system` that
runs `releases/current/kbf` with the node's flags ([host releases](host-releases.md), "by hand" in
[update-a-host.md](update-a-host.md)). A host on the `kbf` package runs the packaged unit, which
runs `/usr/bin/kbf`, and is updated by installing the next package
([host-install.md](host-install.md#on-a-linux-daemon-host-that-runs-the-package)). This page is the
one-time move from the first to the second. It is a held step: nothing here runs by itself, and
each node is moved by an operator, as root, one at a time.

The move is one command, `kbf-host-install --migrate-to-package`. It plans by default and changes
nothing without `--apply`.

```text
kbf-host-install --migrate-to-package --layout deb --channel stable \
    --unit kbf-storage-and-daemon.service --dir /root/kbf-release
kbf-host-install --migrate-to-package --layout deb --channel stable \
    --unit kbf-storage-and-daemon.service --dir /root/kbf-release --apply
```

## Before: what the node needs

- `kbf-host-install` ([host-install.md](host-install.md#getting-the-binary)) and the pinned
  `gh`, the trusted root and `installer.conf` that the check uses, which the `kbf` package
  carries (`/usr/share/kbf/trust`; a host that still has the old `kbf-installer` package is read
  at `/usr/share/kbf-installer/trust`, until v0.0.9). The program refuses when they are not
  root's.
- `/etc/kbf/host-install-releases.toml` with the channel named by `--channel`, as for the local
  socket (`source = "github"`; its branch is the one the attestation must name).
- The node on the hand-placed layout at a release, with none on trial: `kbf-deploy host status`
  names `current` and prints `pending confirm: none`. The release it runs is the one the package
  must be, so the move changes the way the node is installed and not the build it runs. Move to a
  newer build afterwards, with the local socket.
- For an `all-in-one` unit, `/etc/kbf/installer.toml` readable: the packaged unit runs `kbf
  boot-check` first, which reads that file for a `database` key (none is fine; an unreadable or
  invalid file stops the start). The other daemon modes do not read it. A hand-placed node has the
  file already, since the installer reads it.
- The node drained by whatever drains it (for a storage node, take it out of routing first:
  [update-a-host.md](update-a-host.md#restarting-a-storage-member)). Moving restarts its unit once.
  Do one node at a time.
- The three files of the release of the commit the node runs, in a directory that only root can
  write, downloaded and left as they are:

  ```text
  kbf-<commit>-linux-<cpu>.deb            (or .rpm)
  kbf-<commit>-linux-<cpu>.deb.sigstore.json
  SHA256SUMS
  ```

  `<cpu>` is `x86_64` or `arm64`. `gh release download <tag> --repo komira-ai/kbf --pattern
  'kbf-<commit>-linux-<cpu>.deb*' --pattern SHA256SUMS --dir <dir>` fetches them. The release
  publishes the deb; it does not publish an rpm yet, so an rpm host has no file to put here until
  it does.

## What the program checks first

Both the plan and `--apply` do these, and change nothing:

1. The installer's state names the release the node runs (`current`) and has no release on trial.
2. The unit is the unit of a daemon mode (`kbf-daemon`, `kbf-storage`, `kbf-storage-and-daemon` or
   `kbf-all-in-one`), is a regular file in `/etc/systemd/system`, has exactly one `ExecStart=`, and
   that runs the `kbf` of a release under `/var/lib/kbf-installer/releases`. Its flags are plain
   words (letters, digits and `_ . / : = @ + , ~ - ? &`, so a cold-store URL with a query is
   carried as it is): whitespace, a quote, a backslash, a `$`, a `%`, a `;` or the like in a flag stops the move, and the line says
   which word. A `--mode=` it gives must be the mode of the unit's name.
3. No drop-in `10-node-flags.conf` and no `<unit>.hand-placed` file is there already.
4. The package in `--dir` is the package of that commit and passes every check an update's package
   passes: the sha256 `SHA256SUMS` lists for it, `gh attestation verify` offline (this repository's
   `artifacts.yml` on a GitHub-hosted runner, for the full commit, on the channel's branch, run as the
   unprivileged user), and the package manager's own reading of the file (`dpkg-deb --field` or
   `rpm --query --package`): the package `kbf`, for this CPU, whose version names the commit the node
   runs. A package that fails any of them is never installed.

## What it does

In this order. A failure at any step stops the program with status 1; a refusal above is status 2.

1. The package manager installs the verified file (`dpkg --install` or `rpm --upgrade
   --oldpackage --replacepkgs`; the one file, no repository, no network). The packaged units are
   installed disabled under `/usr/lib/systemd/system`, behind the hand-placed unit, so nothing
   changes for the running node.
2. Writes the drop-in `/etc/systemd/system/<unit>.d/10-node-flags.conf`.
3. Renames `/etc/systemd/system/<unit>` to `<unit>.hand-placed`, so the packaged unit takes its
   place, and runs `systemctl daemon-reload`.
4. Restarts the unit and waits until it is active.
5. Checks: the unit's main process runs `/usr/bin/kbf` (`/proc/<MainPID>/exe`), and
   `/usr/bin/kbf --version --json` reports the commit the node ran.
6. Only then removes the old layout, `/var/lib/kbf-installer/releases`.

If step 2, 3, 4 or 5 fails, the program puts the hand-placed unit back (removes the drop-in, renames
the unit back, rereads, restarts) and says so; the old layout is still whole at that point, because
step 6 has not run. The package stays installed; it runs nothing. A step 6 that fails leaves a node
that runs the package and a directory to remove by hand.

## How the node's flags carry over

The packaged unit reads `/etc/kbf/kbf.toml` for its flags. The hand-placed node gives them on the
command line, and a `kbf` that is given flags of its mode runs on them and does not read the file.
So the flags move to a **systemd drop-in** on the packaged unit, unchanged. The program writes it
from the unit it read. For a unit like this one:

```text
[Service]
User=kbf
Environment=RUST_LOG=info
ExecStart=/var/lib/kbf-installer/releases/current/kbf --mode=storage-and-daemon \
    --cold-store=file:///var/lib/kbf/cold --meta-dir=/var/lib/kbf-meta
Restart=always
```

the drop-in is:

```text
[Unit]
ConditionPathExists=

[Service]
User=kbf
Environment=RUST_LOG=info
ExecStart=
ExecStart=/usr/bin/kbf --mode=storage-and-daemon --cold-store=file:///var/lib/kbf/cold --meta-dir=/var/lib/kbf-meta
ExecStartPre=
ExecStartPre=+/usr/bin/kbf boot-check --mode=storage-and-daemon
ExecStartPre=/usr/bin/kbf --mode=storage-and-daemon --cold-store=file:///var/lib/kbf/cold --meta-dir=/var/lib/kbf-meta --check
```

- `ExecStart=` is the node's own, with the program replaced by `/usr/bin/kbf`. If the unit gave no
  `--mode`, the mode of the unit's name is added.
- `ExecStartPre=` is cleared and given again as the packaged unit has it (`boot-check`, then
  `--check`), with the node's flags on the `--check`, because `--check` without flags would read a
  `kbf.toml` the node does not have.
- `ConditionPathExists=` is emptied: the packaged unit starts only once `/etc/kbf/kbf.toml` exists,
  and this node's configuration is its flags.
- The account is the one the old unit ran as (`User=` and `Group=`; `User=root` when it named none),
  so the node keeps the access to its data it had. Moving a node to the `kbf` account the packaged
  unit names is a separate step, with a `chown` of its data.
- `SupplementaryGroups=`, `WorkingDirectory=`, `EnvironmentFile=` and `Environment=` are carried as
  they are.
- Everything else the old unit said is **not** carried: the packaged unit's own values apply
  (slice, delegation, limits, stop and kill behaviour, restart policy). That is the reason for the
  move; read the old file, kept as `<unit>.hand-placed`, if the node relied on one.

To put the node's flags in `/etc/kbf/kbf.toml` instead (one table per flag group, the flag's name
without the dashes as the key: [kbf-units.md](kbf-units.md#choosing-the-mode---mode-kbftoml-enrollment))
is a later change the operator can make by hand: write the file, delete the drop-in and run
`systemctl daemon-reload` and a restart. The move does not do it, because a node that was running
should come up on exactly the flags it ran on.

## After

- Point the node's update agent at the package. In `kbf-host-local.service`
  ([packaging/host-agent](../../packaging/host-agent/README.md)) add `--layout deb` (or `rpm`) to the
  `kbf-host-install --serve` line, keep the `--restart-unit` for the same unit, and restart it.
  From then on `install`, `switch`, `confirm` and `revert` work on the package.
- Check the node as the updater does (`kbf-updater` status, the frontend's view of the node).
- When the node has been healthy as long as the operator likes, delete
  `/etc/systemd/system/<unit>.hand-placed`. Nothing reads it.

## What is not moved

Macs keep the signed tarball. A frontend host and the frontend containers are moved another way, and
`kbf-proxy` is not touched.
