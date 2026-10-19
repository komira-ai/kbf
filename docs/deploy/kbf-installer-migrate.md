# Moving a host from the old layout to kbf-installer

A host installed by the dev installer (`kbf-deploy host ...` with the old helper) keeps its
releases in `/usr/local/kbf`, its deploy state in `/var/lib/kbf-deploy` (macOS
`/var/db/kbf-deploy`: `state.json` schema 1, and the old helper's `helper.json`) and runs units whose
`ExecStart=` is `/usr/local/kbf/current/kbf ...`. `kbf-installer migrate` moves such a host to the
installer's tree (`/var/lib/kbf-installer`; macOS tree `/usr/local/kbf-installer`, state
`/var/db/kbf-installer`) with `state.json` schema 2, once, and keeps the old layout as the
rollback path. The design is `docs/design/pull-updates.md` section 7.4; the steps and their
re-run rules are in the module docs of `crates/kbf-installer/src/migrate/mod.rs`.

```text
kbf-installer migrate [--apply] --unit kbf-daemon.service [--unit ...] \
    --helper-unit kbf-helper.service [--installer-unit kbf-installer.service] [--min-generation N] [--release-source url|frontend] \
    [--mode frontend|all-in-one|storage|daemon|storage-and-daemon]
kbf-installer migrate --rollback [--apply]
```

Without `--apply` it prints what it would do and changes nothing. With it, as root (anyone else is
refused, exit 2, before any release is run):

1. It reads the old state, the helper's memory and the links. A swap on trial (activated, not
   confirmed) refuses the migration: confirm or revert it first (exit 5). Links that disagree with the
   state refuse it (exit 5).
2. It names each release (`current`, `previous`) from `--version --json` of its binary, run as the
   `kbf-fetch` user (`--fetch-user`, never root: a user whose uid or gid is 0 is a usage error, exit 2)
   with an empty environment and a 10 second limit. The commit is
   never taken from a directory name, a link or the state; a directory that holds another build is
   refused (exit 3), and the commit its binary claimed is printed quoted and escaped.
3. It seeds the replay floor, in the two places the installer reads it: the old (epoch, order id)
   is carried over into `helper.json` (the memory `run` orders against, for both kinds of order) and
   into `state.json`'s `floor`; the generation (`floor.generation`, and the value `setup` writes as
   `min_generation`) comes from the
   frontend (`--release-source frontend`), else from `--min-generation`, else it is zero and the report
   says so plainly (`NO REPLAY FLOOR`). With the frontend as the source and no answer, and no flag, it
   refuses (exit 5).
4. It asks the verify pipeline to fetch and verify each release. Until that pipeline is part of the
   build, and whenever it cannot, the old directory is copied by descriptor (`openat`, `O_NOFOLLOW`,
   `fstat`; only regular files and directories owned by root, one link, not setuid, not
   group- or other-writable, no file capabilities, no `VERIFIED` marker) and adopted **unverified**:
   no `VERIFIED` marker, listed in `status.json` under `unverified`, printed as a warning, and a rollback
   to it is refused until a verified copy replaces it.
5. It takes the old tools' lock and the installer's, in that order and held to the end, and looks at
   the old layout a second time under them, because what it read before the locks may have moved (a
   verb of the old tools, the old helper acting on an order, another run of this verb finishing). Each
   lock is waited for ten seconds, as the operator's verbs wait for it, and then the run ends busy
   (exit 4): run it again. The second look is judged like the first (a
   swap on trial, links that disagree with the state, the installer's state beside the old one, a
   finished journal), the floor is seeded from the memory it read, and every write below is made from it
   alone. The frontend, when it is the source of the floor, is asked once, before the locks: its answer
   is what the second look seeds the floor with. If `current` or `previous` is no longer what step 2
   probed, or the old state has gone, nothing is migrated and it ends with exit 5 (`the old layout
   changed under the migration`): run it again. A migration another run finished meanwhile is reported
   as done. Before it stops anything it reads and judges every unit file it will rewrite (a link, a
   file that group or other can write, or one over 64 KiB ends the run with exit 1, no service touched
   and nothing of the installer's written). Then it stops the old helper's unit and the kbf units that
   run, and looks a third time: the old helper acts on orders until its unit is stopped, so what it
   remembers is final only now. The floor is raised to that (a floor only goes up). If the helper made
   a swap as it stopped (`current` or `previous` moved), or the old state went, the stopped units are
   started again and the run ends with the same exit 5, with the installer's state, the links and the
   units as they were. Then it disables the helper (so the
   helper and the installer never both manage the host), writes
   `state.json` (the installer's own type, under its lock, so the next write keeps a `.bak`),
   `helper.json`, then `releases/current` and `releases/previous` (after the state: a link with no
   state beside it reads as a lost state), `status.json` (the shape the node reads, with `unverified`
   added) and the units' `Exec*=` lines (the first original of each unit is kept in
   `migration-units/`), reloads the unit manager, and starts what it stopped.
6. **A frontend or all-in-one host** (`--mode frontend` or `--mode all-in-one`) also crosses to Postgres in
   the same run (`docs/design/frontend-proxy.md` section 11); a node (`storage`, `daemon`,
   `storage-and-daemon`) keeps no server state, and a run with no `--mode` makes no database step. The first
   crossing cannot be a swap, and **no state is copied**: the farm starts on an empty database, the SQLite
   file is not read, not copied and not changed, and a Postgres row is never copied back.
   - Postgres, its `kbf` role and its `kbf` database must exist already (design section 11 has the package
     depend on Postgres and `setup` create the role and database; while the old state is unmigrated `setup`
     of this mode refuses, see below, so the order of the two is the host preparation's to settle); the
     migration creates none of them. Its first look asks the database (the same call as `setup`'s dry
     run) and, when it cannot be asked, refuses with exit 2 naming what must exist, before any program runs
     and in a dry run too. A database whose schema is ahead of this installer is refused (exit 1).
   - A dry run lists the schema step (create, bring forward, or nothing to apply), the units whose
     `--state-store` would move, and the warning below. It asks the database and changes nothing.
   - An apply, once it holds both locks and has looked again, runs the installer's schema step (the one
     `setup` runs: the embedded, expand-only migrations up to this installer's newest) **before anything is
     stopped**: nothing reads that database yet, and a refusal (a contract step pending, a server that will
     not migrate; exit 1) leaves the host as it was. It then stops, writes and starts as in step 5, with one
     more edit per `--unit`: in the `Exec*=` lines (and the lines their backslashes continue), a
     `--state-store=sqlite://...` becomes `--state-store=postgres:///kbf?host=/run/postgresql`. The first
     original of the unit, SQLite flag included, is kept like any other. A unit that names a Postgres store
     already is reported and left; a unit that gives no `--state-store` is left, and when no unit was
     switched the report warns that the store is then whatever `kbf.toml` (`[scheduler] state-store`) or the
     mode's default names: a flag on the unit's command line overrides the key, and the key itself is not
     edited.
   - The warning every plan and run carries: the store starts empty, so nodes enroll again and the cordons,
     `desired`, the release index, the run history and the builds in flight are not carried over. Drain the
     farm first.

   The old tools' lock is `flock(2)` on `/run/kbf-deploy.lock` (macOS `/var/run/kbf-deploy.lock`).
   On Linux that is where the deploy kit took its own (`File::lock` on that path, in the `state.rs`
   that became the installer's); the macOS path is this verb's reading, and no kit source here shows
   it. That the builds running on a host lock the same way is a check on the host before its first
   migration: a build that locks with `fcntl` or on another path is not excluded by this lock.

Every step can be run again: a finished migration is a no-op; one that stopped half way is finished by
the next run (the journal `migration.json` remembers what was stopped, and the Postgres URL the units were
switched to); a floor only goes up; a copy with the source's digest is kept, any other leftover is replaced,
scratch is swept; the schema step finds its work done, and a unit already on Postgres is left as it is.
A host migrated without `--mode` stays on its SQLite file (a finished migration is a no-op); to cross it,
roll back (`migrate --rollback --apply`) and migrate again with `--mode`. While the old
state exists and no migration finished, `run`, the operator's verbs (`confirm`, `revert`,
`rollback`, `gc`) and `setup` of a mode with a database refuse (exit 5) and name this verb; the
rest of `setup` calls the same check when it is built. `status.json` names `unverified` releases
until the helper's first pass rewrites it (the helper does not carry the list yet; the journal
`migration.json` and the missing `VERIFIED` marker do).

`--rollback` stops the installer's own unit, restores the unit files (on a frontend or all-in-one host that
puts the SQLite `--state-store` back; the file was never touched and nothing is copied back from Postgres, so
what the farm wrote there since is not in it, and the database is left as it is), enables the helper again and
starts what ran before. The old tree and state are never modified (a note, `MIGRATED`, is added to the old state
directory and removed again), which is why this works. It is refused once the installer has activated,
staged or put a release on trial since (exit 5): use `kbf-installer rollback` then. If the
installer's state cannot be read at all, the rollback goes ahead and warns that it could not check.
It takes the same two locks as the apply, the old tools' first, each waited for ten seconds and then
busy (exit 4), and decides from what it reads under them: a release the installer activated while the
rollback waited is seen and refuses it, a rollback another run finished meanwhile is reported as done,
and the installer is stopped with its lock held, so it writes no state while the old layout comes back.

Not done by the migration: `installer.toml` (the `min_generation` to put there is printed as the
report's value; `setup` writes the file), the users, and the `orders/` directory (`setup`); on a frontend or
all-in-one host, installing Postgres and creating its `kbf` role and database (`setup`; the migration refuses
while they are absent), and the `kbf.toml` `state-store` key (see step 6).

## The test farm's hosts

The hosts that run the dev installer are the ones this verb exists for. Their layout is fixed
in `crates/kbf-installer/testdata/testfarm/`: units rendered by `kbf-deploy linux render-units
--mode` with `KBF_BIN_DIR=/usr/local/kbf/current`, `state.json` schema 1, the `helper.json` of
the deployed helper (which also keeps `handled_entry`, a key the migration ignores), the old
helper's unit and a Mac's daemon plist. `src/migrate/testfarm_tests.rs` migrates each and
checks the unit the node starts from; a test in `kbf-deploy` keeps the units equal to what the
kit renders and checks that the rewritten unit still passes the kit's rules. What those tests
cannot do is run the node: the repository allows no script as a stand-in binary, so a start is
shown by what it needs (the program is a regular executable file in the new tree with the
bytes of the release that ran, the arguments are the ones the unit had), not by running it.
