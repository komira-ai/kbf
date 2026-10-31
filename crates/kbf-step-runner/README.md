# kbf-step-runner

The step runner of the generic host updater: the root part's engine for one update of one service.
Why it is shaped this way: [docs/design/host-updates.md](../../docs/design/host-updates.md) (3.2 to 3.5,
migration step 4). This page states what the code holds.

The crate names no service and reads no declaration. The launcher that runs a step under the rights
of a declaration (transient systemd units) and the declaration loader are migration step 3; they plug
in through `StepLauncher` and `register::Declared`. The contract types (steps, exit codes, the JSON
last line, the params table, the next-step table) are those of `update-hook`.

| module | what it holds |
|---|---|
| `runner` | `StepRunner::run`: one update from `prepare` to its verdict. Writes the step record before and after each step, asks `update_hook::next` what follows, resumes from the record, runs the post-success watcher for `promotion_pending` |
| `record` | the step record (`schema`, service, target version, artifact digest, generation, step, attempt, state `pending`/`running`/`done`/`finished`, what the runner remembers between steps), written by temporary file, fsync, rename, directory fsync; a record of a newer schema is never changed |
| `artifact` | `Staged` and `tree_digest`: the staged tree and its sha256 at verification; checked before every step, resumed ones included |
| `launcher` | `StepLauncher` and `StepCall`: what the runner needs to run one step and to stop its unit by name |
| `deadman` | `Watch` and `DeadMan`: the bound of the running step, enforced on a thread of its own |
| `register` | the registration socket (one JSON line: name, version, optional loopback health address; `SO_PEERCRED` checked against the declaration) and the `Absent`/`Healthy`/`Unhealthy(Stopped)` the updater reports |
| `selfupdate` | the updater's own update: stage beside, switch the `current` link, restart; `previous` link and `flip_back` for the `OnFailure=` fallback; the two unit texts |
| `clock` | `Clock`, `Stamp`, `SystemClock`: time and the boot id as inputs |

## Rules the code keeps

- Nothing from a release runs unless its tree still has the digest the updater verified. A mismatch
  at any step is a `halt` with the reason `artifact_changed`, and nothing from the tree runs.
- A step found `running` after a crash has its unit stopped by name (`updater-<service>-<step>-<generation>`)
  before it is run again; a step found `done` is not run again.
- `cleanup` runs only after `verify` succeeded and is never rolled back; the watcher never rolls back.
- `rollback` runs the new release's hook, and the previous release's only when the new hook cannot
  start at all.
- The updater writes one params file per update, in its private state directory (refused unless owned by the running user and closed to others), names it in the step record, passes the same path to every step including those after a resume, and removes it when the update ends.
- Tests never wait for a time: a fake clock moves when a `sleep` or a test moves it, the dead-man thread
  is ended by a handshake, and crash tests stop the updater at every point of every record write.

What is not here: the unprivileged agent that talks to the coordinator, the registration client crate
(step 4a), the declaration loader and approval, the transient-unit launcher (step 3), and wiring a
binary. The root-only checks (a registration from another user, the real systemd restart limit) need
the `cargo xtask sudo-test` job and are not part of this crate's fast tests.

## Limits the caller should know

- A host that reboots in a loop restarts the verify window at every boot (the monotonic clock
  restarts, so the time spent before the reboot is unknown): such a host is not rolled back by the
  window, and the dead-man bounds each boot's `verify` only.
- `Layout` (the updater's own binary directory) must be given as a root-owned directory closed to
  group and others, in root-owned parents; the type does not check it.
- `UpdateOrder.artifact_digest` is passed through. The unpack step must hash the tarball bytes as it
  reads them and unpack from that same stream; this crate does not bind the digest to the tree.
- The staged tree is checked by path before every step; the guarantee that it cannot be swapped
  between the check and the exec rests on the tree being root-owned and read-only in root-owned
  parents.
