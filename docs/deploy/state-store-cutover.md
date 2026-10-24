# Moving a frontend's state to Postgres: the cutover

kbf keeps the scheduler's state in Postgres. A frontend that has run on a SQLite state file
moves to Postgres once, in a cold cutover an operator runs with the frontend stopped. The
tool for the move is `kbf-state-copy`. It is a binary of its own (crate `kbf-state-copy`), not
a verb of `kbf`, so the SQLite reader it carries is never in the farm's binary.

**The cutover is an operator-only step.** It stops the farm's scheduler for as long as the
copy and the restart take, writes to the production database, and rewrites the frontend's
configuration. It runs only when a person who administers the host has chosen to run it, at a
time they picked, with the frontend stopped on purpose. Nothing in kbf runs it for them: no
package script, no unit, no update and no CI job. A dry run writes nothing, but it refuses
while the frontend runs, as the real thing does.

There are three forms. The first is the copy alone, described from "Before you run it" on;
`cutover` is the copy followed by the change of the frontend's setting, and `rollback` undoes
that change:

```text
kbf-state-copy <state.db> <postgres-url> [--apply]
kbf-state-copy cutover <state.db> <postgres-url> --config <kbf.toml> [--apply]
kbf-state-copy cutover <state.db> <postgres-url> --unit-file <file> [--apply]
kbf-state-copy rollback --config <kbf.toml> [--apply]
kbf-state-copy rollback --unit-file <file> [--apply]
```

All three are dry runs without `--apply`. There is no `--force`, and no flag skips a check.
The SQLite file is never changed by any of them.

| Flag | Meaning |
|---|---|
| `--config <kbf.toml>` | the setting is the `state-store` key of the `[scheduler]` table of this file (what `kbf-installer setup` writes) |
| `--unit-file <file>` | the setting is the `--state-store=<URL>` flag of the command line in this systemd unit file or drop-in. Give exactly one of the two |
| `--unit <name>` | the frontend's systemd unit: `systemctl show` must say that the unit is loaded and that its active state is `inactive` (or `failed`). A name that no unit has is refused, so a mistyped name cannot pass |
| `--frontend-url <url>` | the URL to write into the setting, when the frontend reaches the database differently from this tool. Without it the setting gets the URL of the copy. It may carry no password, because the frontend refuses a URL that does: use the local socket form, `postgres:///kbf?host=/run/postgresql&user=kbf`, and put the password in the URL of the copy if the account that runs this tool needs one |
| `--apply` | do it; without it no file is changed |

## The cutover, step by step

1. **Find the setting.** The frontend takes `--state-store` from the `state-store` key under
   `[scheduler]` in its `kbf.toml` (on a host set up by `kbf-installer setup`,
   `/etc/kbf/kbf.toml`, an all-in-one host's included), or from a `--state-store=sqlite://<path>` flag on the command line in the unit file
   or in a drop-in of it (`/etc/systemd/system/<unit>.d/*.conf`). **A flag on the command line
   overrides the key.** Look in both places: `systemctl cat <unit>` shows the unit and its
   drop-ins, and `kbf.toml` shows the key. If the command line has `--state-store=`, that
   flag is the setting: give the file that holds it with `--unit-file`, and edit the key by
   hand afterwards to the same URL, so that taking the flag out later does not switch the
   frontend back. If only the key is there, give `--config`. The tool changes only the one
   file it is given; a cutover of the key while the flag is on the command line changes
   nothing the frontend reads.

   **Take out `--handover-socket` anyway.** The frontend no longer hands over: a
   `handover-socket` key under `[handover]` in its `kbf.toml`, or a `--handover-socket` flag,
   is accepted and ignored with a warning, and it no longer stops a `postgres://` store from
   starting. The tool does not look for it: remove it by hand so the warning goes.

   The dry run below names the file and the old and new value of the setting; it refuses a
   file whose setting does not name the state file you are copying.
2. **Drain.** If a load balancer or another proxy sends clients to this frontend, drain it
   there first, so that no client is in the middle of a build when the frontend stops. Pick a
   quiet time: while the frontend is stopped no client or node can reach the scheduler, and no
   build is scheduled.
3. **Stop the frontend.** `systemctl stop <unit>`, then check that `systemctl is-active <unit>`
   says `inactive`. Pass the unit to the tool with `--unit` so that it checks again (and
   refuses a name that is not a unit).
4. **Back up the state file.** Copy it (and its `-wal` file if one is there) somewhere else,
   for example `cp -p /var/lib/kbf/state.db /root/state.db.before-postgres`. The tool never
   changes the file and a rollback uses it where it is, so this copy is for the case in which
   the disk, not the tool, is the problem.
5. **Dry run.**

   ```text
   kbf-state-copy cutover /var/lib/kbf/state.db 'postgres:///kbf?host=/run/postgresql&user=kbf' \
       --config /etc/kbf/kbf.toml --unit kbf-frontend.service
   ```

   ```text
   kbf-state-copy cutover (dry run)
   setting: state-store in [scheduler] in /etc/kbf/kbf.toml
     now:  sqlite:///var/lib/kbf/state.db
     then: postgres:///kbf?host=/run/postgresql&user=kbf
   unit kbf-frontend.service: stopped
   kbf-state-copy (dry run)
   state file: /var/lib/kbf/state.db
   ... the report of the copy, as under "Dry run" below ...
   the target is empty, its schema is current and the clocks agree
   dry run: no file was changed; run again with --apply to copy, verify and switch the setting
   ```

   Keep the output. Fix what a refusal says (exit status 2) before going on.
6. **Cutover.** The same command with `--apply`. It copies and verifies exactly as the copy
   does, then switches the setting, and ends with:

   ```text
   copied key/value rows: 14, counters: 1; head revision now 36
   verified before the commit and again on a new connection after it: 10 families match
   switched state-store in [scheduler] in /etc/kbf/kbf.toml to postgres:///kbf?host=/run/postgresql&user=kbf
   the original is kept byte for byte as /etc/kbf/kbf.toml.pre-postgres
   next: start the frontend
   ```

7. **Check.** The report says the families match, the digests are those of the dry run, the
   setting file has the Postgres URL where the SQLite one was (and is otherwise as it was:
   `diff /etc/kbf/kbf.toml.pre-postgres /etc/kbf/kbf.toml` shows that one line), and the
   backup is there. For a unit file, run `systemctl daemon-reload` now: systemd reads the
   changed file only then.

   This is also the place for any other change to the frontend's settings planned for its
   next restart (for example a new history store, which needs the Postgres state store): make
   it now, in the same file, so the frontend restarts once. Keep such changes out of the
   `.pre-postgres` backup, which is the rollback; a rollback then also undoes them.
8. **Start the frontend and verify it serves.** `systemctl start <unit>`, then check that
   the unit is active, that its journal shows no state-store error, and that the operator API
   answers: `curl -fsS http://127.0.0.1:8982/readyz` is `200` with `"ready": true` (use the
   `api-listen` address of the host). Check that the frontend runs on Postgres and not on the
   SQLite file: while it runs, `flock -n /var/lib/kbf/state.db.lock true` succeeds, because a
   frontend on the SQLite file holds that lock. If it fails, a `--state-store=` that you did
   not change is still setting the store (step 1). Run one small build through the farm.

## Rolling back

Until the Postgres store has been trusted, the SQLite file and the backup of the setting are
the way back. To roll back: stop the frontend (step 3), then

```text
kbf-state-copy rollback --config /etc/kbf/kbf.toml --unit kbf-frontend.service
kbf-state-copy rollback --config /etc/kbf/kbf.toml --unit kbf-frontend.service --apply
```

(dry run, then the real thing; for a unit file, `--unit-file <file>` and a
`systemctl daemon-reload` after), and start the frontend. The rollback renames
`<file>.pre-postgres` over the file, so the file is the original again, byte for byte, with
its mode and owner, and the backup is gone. It refuses (status 2) when there is no backup.
The whole file is put back, not only the one setting: a change made to the file by hand after
the cutover is lost, and has to be made again.

**Writes made after the switch are lost on a rollback.** The SQLite file is as it was when
the frontend stopped for the cutover; nothing the farm did through Postgres since is in it.
Builds that were accepted in that time are not known to the rolled-back frontend, and their
clients must send them again. After a rollback the Postgres store still holds the copy: to
cut over again, drop its schema and set it up again first, because the copy refuses a store
that holds data.

## Moving to frontends that share the store

A frontend of this release that opens a Postgres store takes no writer generation. It makes
sure the store's `meta/generation` row holds the marker `shared-writers` instead, and any
number of frontends can then open the same database and serve at once: none fences another.
A frontend of an earlier release took the next generation at every start and checked it on
every write, so a second one over the same database stopped the first.

**This is a cold cutover, and it happens at the first start of this release on a database.**
The marker is written once, by whichever frontend opens first; frontends that open together
agree (the write is conditional on the row they read), and a store that already holds the
marker is left as it is. Do it in this order:

1. **Stop every frontend of an earlier release that uses the database**: each `kbf frontend`
   and `kbf all-in-one` unit, and any process of an earlier release started with
   `--admin-listen` (a candidate waiting for a takeover included). Check that each unit says
   `inactive`.
2. **Back up the database** (the `kbf_state` schema). The marker is not undone by going back to
   an earlier binary: see "If a frontend of an earlier release is started" below. The backup
   is the way back.
3. **Start the first frontend of this release.** Its open replaces the generation row (an
   8-byte number, or no row on a new database; a database copied by `kbf-state-copy` carries
   the SQLite file's number) with the marker. The epoch is not touched, so the daemons that
   reconnect keep the leases they hold.
4. **Start the others.** They find the marker and write nothing to the row.

**If a frontend of an earlier release is started after the marker is written**, it does not
start: it reads the row as `the generation row is not an 8-byte number` and exits. One that is
still running when the marker is written is refused by the store at its next write
(`state store fenced`) and stops serving; this is why step 1 comes first, so that no build is
cut short by it. To go back to an earlier release, restore the backup of step 2 (the writes
made since are lost, as after a rollback above).

A frontend of this release serves no admin port and has no takeover: it holds no generation
to hand over.

**The proxy's slot supervision does not work over a Postgres store.** It starts every child
as a candidate (`--await-takeover`) and then calls `takeover`, to start a frontend at boot or
to swap one in for another. A frontend of this release accepts and ignores `--await-takeover`
and serves no admin calls, so the child is never brought up and a swap fails before it stops
the old one. Run a frontend over a Postgres store under its own unit, without
`--await-takeover`, and start and stop it there.

A SQLite state file is held by one process and keeps its generation until it is moved to
Postgres.

## What the cutover does, and what it refuses

In order, so that every step that can refuse or fail comes before anything is changed:

1. The arguments: exactly one of `--config` and `--unit-file`; a URL for the setting that
   the store takes, with no password and with no space, quote or backslash in it.
2. The setting: the file is read and its one place to edit found. A `kbf.toml` must be valid
   TOML and hold one line `state-store = "<URL>"` (double or single quotes) under
   `[scheduler]`; a unit file must hold one `--state-store=<URL>` that is not in a comment or
   in quotes (the form `--state-store <URL>` with a space is not edited: write it with `=`
   first). The value must be a `sqlite://` URL of the state file being copied, by any link.
   A setting file whose `<file>.pre-postgres` or `<file>.postgres-new` already exists is
   refused (see "An interrupted cutover" below). The new text is the old text
   with the one value replaced, so comments and spacing stay; for a `kbf.toml` the new text is
   parsed and must equal the old with only that value changed, or the cutover refuses and
   leaves the file for you to edit.
3. The frontend is stopped: the unit (with `--unit`: it must be a unit, and inactive or
   failed), then the lock beside the state file, which is held to the end.
4. With `--apply`, the backup (`<file>.pre-postgres`, the original byte for byte) and the new
   text (`<file>.postgres-new`) are written beside the file with its mode and owner, and
   synced. If either name is taken the cutover fails here, having removed what it made.
5. The copy: the checks, the transaction, and both read-backs, as below. A copy that is
   refused or fails removes the two staged files and leaves the setting as it was.
6. The new text is renamed over the file (atomic, so the frontend sees the old file or the
   new one, never a half-written one) and the directory is synced.

A cutover that fails after step 5 committed (the second read-back found a mismatch, or the
rename in step 6 failed) has changed the target but not the setting; read the message, which
says what the target needs before another try: its schema dropped and set up again.

## An interrupted cutover

A cutover that is killed (a signal, a power loss) cannot remove the files it staged, and
the next cutover refuses to start while either is there. Look at the setting file and at the
files beside it, and at the target's row counts (the copy is one transaction, so a target is
either empty or holds the whole copy):

- **`<file>.pre-postgres` and `<file>.postgres-new` are both there and the setting still
  names `sqlite://`.** The rename did not happen. If the target holds the copy (the kill came
  after the commit, or the rename failed and the message said so), drop its schema and set it
  up again. Then remove both files by hand; the SQLite file was never changed.
- **`<file>.pre-postgres` is there, `<file>.postgres-new` is not, and the setting names
  Postgres.** The rename was done and the cutover is finished, though its report was lost.
  Go on from step 7.
- **Only one of the two is there.** The kill came while the files were being staged, before
  the copy began; remove the one that is there.

## The copy on its own

The rest of this page describes the copy, which `cutover` runs.

## Before you run it

- **The frontend is stopped, and stopped cleanly.** The tool refuses while a frontend holds
  the lock beside the state file (`<state.db>.lock`). A state file with no lock file beside
  it has never been opened by a frontend, and the tool creates none; there, confirm yourself
  that no frontend is running. A frontend killed in the middle of a
  write leaves a write-ahead log (`<state.db>-wal`) that holds commits the file does not;
  the tool refuses that too, because it reads the file without the log. Start the frontend
  and stop it cleanly once, and the log is folded into the file.
- **The account can read the state file and its lock file.** Run it as the account the
  frontend runs as, or as root. The tool opens the file read-only and immutable; it creates
  nothing beside it and never changes it.
- **The target is a database the host's setup has prepared**: the state schema at the version
  this build of the tool knows, and nothing in it. A store that holds a row, a feed entry, a
  counter or a head revision above 0 is refused, and so is a missing, older or newer schema.
- **The clocks agree.** Expiry times are copied as they are, so the host's clock and the
  database server's must be within 2 seconds of each other, or the copy is refused.
- **Prefer a URL without a password.** Command-line arguments can be read by other users,
  and the client does not read password files; the tool does not refuse a password, but
  put none in the URL if the account can do without. Use the local socket form the
  frontend uses, for example `postgres:///kbf?host=/run/postgresql&user=kbf`. The store is
  the one in the database's `kbf_state` schema.

## Dry run

```text
kbf-state-copy /var/lib/kbf/state.db 'postgres:///kbf?host=/run/postgresql&user=kbf'
```

It reads the file, prints what is in it, checks the target, and writes nothing, to the
target or beside the state file:

```text
kbf-state-copy (dry run)
state file: /var/lib/kbf/state.db
key/value rows: 14; counters: 1; highest version: 36; head revision: 36
key families (rows, digest):
  lease/        1  <64 hex digits>
  op/           2  <64 hex digits>
  ...
counter families (rows, digest):
  seq/          1  <64 hex digits>
the target is empty, its schema is current and the clocks agree
dry run: nothing was written; run again with --apply to copy
```

A key family is the first path segment of a key, with its slash (`op/`, `lease/`); a key with
no slash is a family of its own. The digest is the SHA-256 of the family's rows in key order:
for each row, its key, value, version and expiry (in microseconds), each in a fixed encoding
that `crates/kbf-state-copy/src/digest.rs` states. Two stores that hold the same rows give
the same digests. Keep the dry run's output: the report that `--apply` prints
first carries the same families and digests, so the two can be compared.

If a check fails, the report of the file is still printed, then the reason, and the run ends
with status 2.

## Copy

```text
kbf-state-copy /var/lib/kbf/state.db 'postgres:///kbf?host=/run/postgresql&user=kbf' --apply
```

It takes the checks of the dry run, then, in one transaction:

1. takes the lock a writer takes on the store's commit head, so a frontend that starts on
   the new store waits (the copy waits at most 10 s for it, and fails if it cannot get it),
   and checks again that the store is empty;
2. writes every key/value row and every counter with its version, converts each expiry from
   the file's nanoseconds to the store's microseconds (rounded down), and sets the store's
   head revision to the file's head revision, or to its highest version if that is higher,
   so the next write gets a revision above every copied row;
3. reads the rows and the head revision back inside the transaction and compares them with
   the file, family by family (row count and digest). A mismatch rolls the transaction back,
   and the target is as it was;
4. sets the store's revision sequence to the head revision (last, because a sequence is not
   rolled back with a transaction), and commits.

Then it reads everything back once more on a new connection and compares again. While it
copies, the tool holds the frontend's lock on the state file (the lock file a frontend
creates beside it, when there is one), so no frontend can start on the file meanwhile; the
file itself is not changed.

The change feed is not copied. Its history is not state, and a reader of the new store starts
at the head.

```text
copied key/value rows: 14, counters: 1; head revision now 36
verified before the commit and again on a new connection after it: 10 families match
```

## Exit status

| status | meaning |
|---|---|
| 0 | done: the dry run's checks pass, or the copy is committed and verified twice |
| 2 | refused: a precondition does not hold (frontend running, log not folded in, target not empty, schema not current, clocks apart). Nothing was changed |
| 1 | failed: a call failed, or the copy did not verify, or the command line is wrong |

A failure before the commit leaves the target empty. A failure of the second read-back
(a mismatch, or a read that cannot be made) happens after the commit, which cannot be
undone: the message says the target holds a copy that does not match, or that the copy was
committed but could not be read back, and the target must be checked and have its schema
dropped and set up again before another try. If the connection is lost during the commit
itself, the outcome is not known: look at the target's row counts before running again (a
second run on a target that holds the copy is refused with status 2). The state file is
untouched in every case, so it remains the rollback.

`cutover` and `rollback` use the same statuses. A status 2 is also a setting that is not the
state file's (not a `sqlite://` URL, another file, no place to edit), a URL for the setting
with a password, a backup that is already there, a unit that is not stopped, and, for a
rollback, no backup to restore. A status 1 is also a command line with both or neither of
`--config` and `--unit-file`, a setting file that cannot be read, and a file that cannot be
made or renamed. Every refusal and every failure of the copy, before or after its commit,
leaves the setting file as it was; after the commit the target holds the copy, and the
message says so. The one failure after the file is switched is a failed sync of its
directory, and its message says the rename was done.
