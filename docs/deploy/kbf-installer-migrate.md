# Moving a host from the old layout to kbf-installer

A host installed by the dev installer (`kbf-deploy host ...` with the old helper) keeps its
releases in `/usr/local/kbf`, its deploy state in `/var/lib/kbf-deploy` (macOS `/var/db/kbf-deploy`:
`state.json` schema 1 and the old helper's `helper.json`) and runs units whose `ExecStart=` is
`/usr/local/kbf/current/kbf ...`. `kbf-installer migrate` moves such a host to the installer's tree
once, and keeps the old layout as the rollback path.

| | Linux | macOS |
|---|---|---|
| installer tree | `/var/lib/kbf-installer` | `/usr/local/kbf-installer` |
| installer state | the tree | `/var/db/kbf-installer` |
| old tree | `/usr/local/kbf` | `/usr/local/kbf` |
| old state | `/var/lib/kbf-deploy` | `/var/db/kbf-deploy` |
| old tools' lock | `/run/kbf-deploy.lock` | `/var/run/kbf-deploy.lock` |
| unit files | `/etc/systemd/system` | `/Library/LaunchDaemons` |

The design is [pull updates](../design/pull-updates.md) section 7.4. A Mac that the older Mac kit
installed is a different case, see "A host the Mac kit installed" below, and
[installing on macOS](kbf-installer-macos.md).

```text
kbf-installer migrate [--apply] --unit kbf-daemon.service [--unit ...] \
    [--helper-unit kbf-helper.service] [--installer-unit kbf-installer.service] \
    [--release-source url|frontend|bundled] [--min-generation N] [--fetch-user NAME]
kbf-installer migrate --rollback [--apply] [--unit ...] [--helper-unit ...] [--installer-unit ...]
```

| flag | meaning |
|---|---|
| `--unit` | a unit that runs kbf from `/usr/local/kbf/current`, to repoint at the installer's tree; repeat for several; `.service`, or `.plist` for launchd |
| `--helper-unit` | the old helper's unit: stopped and disabled, so the installer is the only manager |
| `--installer-unit` | the installer's own unit: enabled and started as the last step; a rollback stops and disables it first |
| `--release-source` | where the host gets its releases (`url`, the default, `frontend`, or `bundled`, which fetches nothing); decides where the replay floor is seeded from |
| `--min-generation` | the lowest generation an order may carry; the floor when there is no frontend to ask |
| `--fetch-user` | the unprivileged user the old binaries' `--version --json` runs as; default `kbf-fetch` (`_kbf-fetch` on a Mac) |
| `--rollback` | put the old layout back instead |

A unit name is lowercase letters, digits and `-`, then `.service`; or, for launchd, a label with
`.` allowed, then `.plist`.

## Dry run and `--apply`

Without `--apply`, `migrate` prints each step as `DRY-RUN: <step>` and changes nothing. With it, it
prints `+ <step>`. Only an apply needs root: any other user is refused with exit 2 and the
message `migrating changes root's files: run it as root (this is uid N, not 0)`, and this comes
before any release is run. A dry run needs the fetch user to exist, because it reads each
release's version as that user. On a Mac an apply with the standard fetch account creates it when it
is missing (the way `setup` does); a dry run says it cannot and that `migrate --apply` does. With
`--json` the report is one object, `{"verb":"migrate","apply":...,"message":...}`.

## What an apply does

1. **Reads** the old state, the helper's memory and the links. A swap on trial (activated, not
   confirmed) refuses the migration: confirm or revert it first. Links that disagree with the
   state refuse it. Both are exit 5.
2. **Names each release** (`current`, `previous`) from `--version --json` of its own binary, run as
   the fetch user (never root: a user whose uid or gid is 0 is a usage error, exit 2) with an
   empty environment and a time limit. The commit is never taken from a directory name, a link or
   the state: a directory that holds another build is refused (exit 3), and the commit its binary
   claimed is printed quoted and escaped. A link that is not `releases/<commit>` is refused (exit 3).
3. **Seeds the replay floor** in two places the installer reads it: the old (epoch, order id) is
   carried into `helper.json` and into `state.json`'s `floor`. The generation comes from the source
   named by `--release-source`: see "The replay floor".
4. **Adopts each release.** This build has no verify pipeline wired in, so every release is copied
   from the old tree by descriptor (regular files and directories owned by root, one link, not
   setuid, not writable by group or others, no file capabilities, no `VERIFIED` marker) and adopted
   **unverified**: no `VERIFIED` marker, listed in `status.json` under `unverified`, printed as
   `release <commit> copied UNVERIFIED: <why>` and a warning, and a rollback to it is refused until
   a verified copy replaces it. A tree that breaks a rule is refused with exit 1.
5. **Takes two locks**, the old tools' lock and the installer's, in that order and held to the end,
   each waited for ten seconds and then busy (exit 4): run it again. Under them it looks at the old
   layout a second time and judges it like the first look, and every write below is made from it
   alone. If `current` or `previous` is no longer what step 2 probed, or the old state has gone, it
   migrates nothing and ends with exit 5 (`the old layout changed under the migration (...): nothing
   was migrated, run it again`). A migration another run finished meanwhile is reported as done.
6. **Judges the unit files** before it stops anything. A unit that is a link, that group or others
   can write, or that is over 64 KiB ends the run with exit 1, with no service touched.
7. **Stops** the old helper's unit and the units of `--unit` that run (the list is journalled
   first), then looks a third time: the old helper acts on orders until its unit is stopped, so the
   floor is raised to what it remembers now. If the helper made a swap as it stopped, the stopped
   units are started again and the run ends with exit 5 as in step 5.
8. **Writes**, in this order: disables the helper (`disabled <unit>: the installer manages this host
   now`); `state.json` (schema 2); `helper.json`; `releases/current` and `releases/previous`
   (after the state: a link with no state beside it reads as a lost state); `status.json`; the
   units, see below; the node's `kbf.toml` keys, see below; then reloads the unit manager and starts
   what it stopped.
9. **Enables the installer's own unit**, last, with `--installer-unit`: `systemctl enable --now`
   unless `is-enabled` says `enabled` (`launchctl bootstrap` unless the job is loaded), so the
   migrated host takes orders without a `setup` run. A unit that will not come up ends the run with
   exit 5 before the journal says done (`<unit> could not be enabled and started (...): the host is
   moved but takes no orders; fix the unit, then run kbf-installer migrate --apply again`). The
   frontend and the proxy are never started.
10. **Journals** `migration.json` as `done` in the installer's state directory and, 
    leaves a note, `MIGRATED`, in the old state directory (an old host with state; a kit host has
    none).

The restart is steps 7 and 8: each running unit of `--unit` is stopped, rewritten, and started
again, so the node is down for that time and runs from the installer's tree afterwards.

### Removing the installer afterwards: `kbf-installer uninstall`

A rollback undoes an adoption, not the package: the installer never removes itself, so a rolled-back
Mac still has `/Library/LaunchDaemons/com.komira.kbf-installer.plist`, `/Library/Logs/kbf-installer`,
the package receipt (`pkgutil --pkgs`), the launchd disable override of `com.komira.kbf-installer`
(the rollback set it) and the `.rolled-back-<time>` files. `kbf-installer uninstall [--apply]` removes
them. It is refused (exit 5) unless the host is rolled back (journal phase `rolled_back`) or was never
adopted (no journal and no `state.json`), and takes the installer's lock (exit 4 while a `run` pass holds
it). Without `--apply` it prints each path it would remove and the receipt id, one per line, and changes
nothing. On a Mac it boots the job out (only if loaded), removes the plist, runs `launchctl enable
system/com.komira.kbf-installer` (clearing the override, so a reinstalled package is not left
disabled), runs `pkgutil --forget com.komira.kbf-installer` (only if the receipt is there), removes the
log directory, `installer.toml`, the installer's tree and every `<name>.rolled-back-<UTC time>` of both,
and last archives the journal as `migration.json.rolled-back-<time>` in `/var/db/kbf-installer`: the
journal is kept, never deleted. `uninstall.json` in that directory records the suffix and the steps
done after each one, so a run that died is finished by running it again; a host already clean says
"nothing to uninstall" and exits 0. On Linux the package's files (binary, unit file, trust files) are
the package manager's and are left: `--installer-unit kbf-installer.service` stops and disables the unit
and the journal is archived. `--installer-unit` names another job on a Mac.

`setup` runs `launchctl enable system/com.komira.kbf-installer` before it bootstraps the job (also with
`--no-start`), so installing the package again and running `setup` works on a Mac whose override was
set by a rollback.

## How a unit is rewritten

In a systemd unit only the value of an `Exec*=` or `Environment=` line changes, and in a launchd
plist only a `<string>` line, and only where `/usr/local/kbf/current` is a whole path. Every other
byte stays. The file is read without following a link and must be root's. Its first original is
kept, byte for byte, in `migration-units/<unit>` under the installer's state directory (mode 0600).
The report says, for each unit, one of `repointed at the new tree (the original is kept)`,
`already runs from the new tree`, `does not run kbf from the old link: left alone` or `no such unit
file: skipped`.

The node's `kbf.toml` gets two keys for the order flow. On Linux, for a host whose `kbf.toml` has
`mode = "daemon"` or `"storage-and-daemon"`, the migration adds `[daemon] update-dir` and
`orders-dir` (the installer's state directory and its `orders/`), makes the order spool (0700, the
`kbf` user's), keeps the first original of the file as `migration-units/kbf.toml`, and changes
nothing else in the file. A key that names another directory is the operator's and is reported as a
warning, never changed. On a Mac the migration writes nothing there and checks instead: see
[installing on macOS](kbf-installer-macos.md).

## The replay floor

The floor stops an old signed order from being replayed. `--release-source` and `--min-generation`
decide the generation:

| source | `--min-generation` | result |
|---|---|---|
| `url` (default) or `bundled` | given | generation is that value |
| `url` or `bundled` | not given | generation zero, and the warning `NO REPLAY FLOOR: no frontend to ask and no --min-generation: replay memory starts from generation zero, so a stale signed order for an older release is not refused until you set min_generation` |
| `frontend` | given | the frontend's generation, or the flag when the frontend cannot be asked (a warning says so) |
| `frontend` | not given | the frontend's generation; when it cannot be had, exit 5 (`the frontend's current generation could not be had (...): retry, or give --min-generation`) |

This build has no frontend client, so `--release-source frontend` always falls back to the
flag, or refuses. A floor only goes up: a repeated migration never lowers it. When the generation is
above zero the report prints `min_generation = N is the floor setup writes into installer.toml`;
`migrate` does not write `installer.toml`, so pass the same `--min-generation` to
[`setup`](kbf-installer-setup.md).

## Refusals and exit codes

| exit | when |
|---|---|
| 0 | done, already migrated (`already migrated: nothing to do`), or no old layout (`no old layout here: nothing to migrate`) |
| 1 | a unit file or release tree that breaks a rule, a file the migration wrote earlier that does not read back, an I/O error |
| 2 | an apply that is not root; a fetch user that is missing, or is uid or gid 0 |
| 3 | a release directory that holds another build than its name; a link that is not `releases/<commit>`; a version probe that failed; a half or unknown Mac kit host |
| 4 | the old tools' lock or the installer's is held past ten seconds |
| 5 | see below |

Exit 5, a person has to go on:

- **Conflict.** The installer's `state.json` exists beside the old state and there is no migration
  journal: `the installer's state.json exists beside the old state and no migration ran: one of them
  is stale, and the migration will not choose`. A Mac kit host that also has the old tools' state
  is refused the same way.
- **A swap on trial** in the old state, **links that disagree** with the state, the old layout that
  **changed under the migration**, a **floor** that cannot be had, the installer's unit that would
  not come up, a Mac `kbf.toml` that is not provisioned.
- **MovedOn.** `--rollback` after the installer moved on: `cannot roll back: the installer has
  activated or staged a release since the migration; go back with kbf-installer rollback instead`.
- **No journal.** `--rollback` on a host that was not migrated: `cannot roll back: this host was not
  migrated (no migration.json)`.
- A Mac that was rolled back to the kit (see below), until the package is installed again.

While the old state exists and no migration finished, `run`, `confirm`, `revert`, `rollback`,
`gc`, `record-release` and `setup` refuse with exit 5 and name this verb.

### A host left half-installed by a failed install

A failed install can leave the installer's own state on a host that still has the old layout. The
usual sign is `state.json` in `/var/lib/kbf-installer` (macOS `/var/db/kbf-installer`) with no
`migration.json`. `migrate` then refuses with the Conflict above, because it cannot tell which of
the two states is the live one. Neither is deleted for you. Before `migrate` runs on such a host,
check that the old layout is the one that ran, stop `kbf-installer.service` if it started, and move
the installer's new tree aside by hand, for example with `mv /var/lib/kbf-installer
/var/lib/kbf-installer.aside`. `migrate --apply` then makes the tree again from the old layout.
Keep the moved tree until the migration is done. On a Mac only the state directory
`/var/db/kbf-installer` is the installer's own state; the package's tree under
`/usr/local/kbf-installer` is not moved.

## Re-running

Every step can be run again. A finished migration is a no-op. One that stopped half way is finished
by the next run, because the journal `migration.json` remembers what was stopped. A copy with the
source's digest is kept, any other leftover is replaced, and scratch is swept.

## `--rollback`

`--rollback` stops the installer's unit (the one `--installer-unit` names), restores the units'
originals (`<unit>: the original is back`), puts back a unit file that `setup` retired when it had
no original (`<unit>: the file setup retired is back (it had no original)`), restores the node's
`kbf.toml` if it is still the original plus the keys the migration added, enables the helper again
and starts what ran before. The old tree and state are never modified apart from the `MIGRATED`
note, which is removed again, and that is why this works. It is refused with exit 5 (MovedOn) once the
installer has activated, staged or put a release on trial since: use `kbf-installer rollback` then.
If the installer's state cannot be read, the rollback goes ahead and warns that it could not check.
It takes the same two locks as the apply (busy is exit 4), decides from what it reads under them,
and a rollback another run finished is reported as `already rolled back: nothing to do`. The new
tree is kept, and `migrate --apply` moves the host over again.

## A host the Mac kit installed

A Mac that the Mac kit installed has no `state.json`, `helper.json` or lock file: the links
`/usr/local/kbf/current` and `previous` are the state, and the job is the LaunchDaemon
`com.komira.kbf-daemon` in `/Library/LaunchDaemons`. `migrate` adopts it as an old host:

- It is a kit host when the old state is absent and `/usr/local/kbf` holds `releases/`, a `current`
  link, the kit's settings file `etc/av2_mac.conf`, and a plist whose first argument is under
  `/usr/local/kbf/current/`. With one marker and not the other it refuses and says which is
  missing (`not a whole kit host`, exit 3), and a plist in neither of the kit's two forms
  (`kbf --mode=daemon --config <file>` or `kbf daemon ...`) is refused with exit 3.
- It runs `kbf`, not `kbf-daemon`, to read a kit release's version.
- There is no old state to carry over and no old lock to take, so only the installer's lock is
  taken; the old tools' state directory gets no note.
- The plist is repointed whole-path like any unit; with no `--installer-unit` the migration enables
  the package's `com.komira.kbf-installer.plist` anyway, because a kit host's installer is always
  that job.
- A Mac's `kbf.toml` is checked, not edited: `[daemon] update-dir` must be there and be the
  installer's state directory, and `orders-dir` absent or the installer's, otherwise it refuses
  before anything is stopped, exit 5.
- The kit's job file on a host that is not a Mac is refused (`<path> is the Mac kit's job file and this is not a Mac`, exit 3).

Rolling a kit host back is different, because the kit refuses to manage a Mac while
`/usr/local/kbf-installer` exists, and two owners of one host is what this verb prevents. After
the kit's plist and job are back, the rollback boots out and disables
`com.komira.kbf-installer`, moves `/usr/local/etc/kbf/installer.toml` aside (the job's `KeepAlive`
is gated on that path), and moves `/usr/local/kbf-installer` aside. Both get one suffix,
`.rolled-back-<UTC time>` as `YYYYmmddTHHMMSSZ`, chosen once and kept in the journal, so a run that
died part way finishes with the same name. A dated name that is taken is refused (exit 5, nothing
replaced). `/var/db/kbf-installer`, the journal, stays. To hand the host to the installer again,
install the `kbf-installer` package again and run `migrate --apply`; until a tree is there again
`migrate` and `setup` refuse with exit 5: `this Mac was rolled back to the Mac kit and the
installer's tree was moved aside: reinstall the kbf-installer .pkg to hand the host to the
installer again`.

## The old tools' lock

The old tools' lock is `flock(2)` on `/run/kbf-deploy.lock` (macOS `/var/run/kbf-deploy.lock`). On
Linux that is where the deploy kit took its own lock. That the builds running on a host lock the
same way is a check on the host before its first migration: a build that locks with `fcntl` or on
another path is not excluded by this lock.
