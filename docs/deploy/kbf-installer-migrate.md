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
    --helper-unit kbf-helper.service [--installer-unit kbf-installer.service] [--min-generation N] [--release-source url|frontend]
kbf-installer migrate --rollback [--apply]
```

Without `--apply` it prints what it would do and changes nothing. With it, as root:

1. It reads the old state, the helper's memory and the links. A swap on trial (activated, not
   confirmed) refuses the migration: confirm or revert it first (exit 4). Links that disagree with the
   state refuse it (exit 5).
2. It names each release (`current`, `previous`) from `--version --json` of its binary, run as the
   `kbf-fetch` user (`--fetch-user`) with an empty environment and a 10 second limit. The commit is
   never taken from a directory name, a link or the state; a directory that holds another build is
   refused (exit 3).
3. It seeds the replay floor: the old (epoch, order id) is carried over; the generation comes from the
   frontend (`--release-source frontend`), else from `--min-generation`, else it is zero and the report
   says so plainly (`NO REPLAY FLOOR`). With the frontend as the source and no answer, and no flag, it
   refuses (exit 5).
4. It asks the verify pipeline to fetch and verify each release. Until that pipeline is part of the
   build, and whenever it cannot, the old directory is copied by descriptor (`openat`, `O_NOFOLLOW`,
   `fstat`; only regular files and directories owned by root, one link, not setuid, not
   group- or other-writable, no file capabilities, no `VERIFIED` marker) and adopted **unverified**:
   no `VERIFIED` marker, listed in `status.json` under `unverified`, printed as a warning, and a rollback
   to it is refused until a verified copy replaces it.
5. It stops the old helper's unit and the kbf units that run, disables the helper (so the helper and the
   installer never both manage the host), writes `releases/current` and `releases/previous`,
   `state.json`, `status.json` and the units' `Exec*=` lines (the first original of each unit is kept in
   `migration-units/`), reloads the unit manager, and starts what it stopped.

Every step can be run again: a finished migration is a no-op; one that stopped half way is finished by
the next run (the journal `migration.json` remembers what was stopped); a floor only goes up; a copy
with the source's digest is kept, any other leftover is replaced, scratch is swept. `setup` refuses
(exit 5) while the old state exists and no migration finished.

`--rollback` restores the unit files, enables the helper again, stops the installer's own unit and starts what
ran before. The old tree and state are never modified (a note, `MIGRATED`, is added to the old state
directory and removed again), which is why this works. It is refused once the installer has activated,
staged or put a release on trial since (exit 5): use `kbf-installer rollback` then.

Not done by the migration: `installer.toml` (the `min_generation` to put there is printed as the
report's value; `setup` writes the file), the users, and the `orders/` directory (`setup`).

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
