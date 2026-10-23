# Moving a frontend's state to Postgres: the copy tool

kbf keeps the scheduler's state in Postgres. A frontend that has run on a SQLite state file
moves to Postgres once, in a cold cutover an operator runs with the frontend stopped. The
tool for the move is `kbf-state-copy`. It is a binary of its own (crate `kbf-state-copy`), not
a verb of `kbf`, so the SQLite reader it carries is never in the farm's binary.

This page covers the tool only. The steps around it (draining the nodes, stopping the
frontend, switching the frontend's state-store setting, rolling back) are the cutover runbook,
which comes with the cutover command.

```text
kbf-state-copy <state.db> <postgres-url>            # dry run: report and check, write nothing
kbf-state-copy <state.db> <postgres-url> --apply    # copy, check, commit, check again
```

There is no `--force`, and no flag skips a check.

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
