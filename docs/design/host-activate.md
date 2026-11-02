# The host's emergency swap: `kbf-host-install activate`

Status: implemented by the pull request that adds it. This is the design of its decisions; how to
run it is [deploy/host-install.md, "The emergency command"](../deploy/host-install.md#the-emergency-command-activate).

## What it is for

A kbf release that misbehaves must be left without the coordinator, a download, or anything of
that release running. Every host keeps its last releases on disk (`releases/<commit>` and
`retain.json`, from the change that keeps the last N releases). `activate` makes one of them the
release that runs, for good, by the helper alone: root runs `kbf-host-install activate
kbf=<commit|version>` at the helper's fixed path, which no kbf release writes.

## Decisions

### Which binary does the work

The helper at its fixed path moves the links and asks the service manager (systemd, launchd) to
start the release named. It executes nothing under `releases/`: no version probe, no check, no
verify pipeline. A kept release whose binary is not a regular file root may run (missing, a link
to another release's, writable by others) is refused before anything changes, as one not kept is.

### No trial

The swap is final: no deadline, no revert. `previous` becomes the release left. A release on
trial stops being one: on Linux the installer's `state.json` is written whole (current, previous,
no `pending_confirm`) and the links follow; on a Mac the links move first and the socket's
`pending.json` is removed after (one that cannot be removed puts the links back, and links that
cannot be put back are `revert_failed`, saying what they name). If the links cannot follow the
state on Linux, the state is put back as it was, so the next activate leaves the release that
still runs. If the state cannot be put back either, the failure says so and names what `current`
links to; the next select takes the release it leaves from the `current` link, not from the state,
so `previous` is never a release that did not run.

A unit that does not come up on the release named is `restart_failed`. The host stays on that
release: going back would start the release being left. Every configured unit is restarted, each
whatever became of the others, so none goes on running the release left.

### The install lock, and taking it

`activate` takes the helper's install lock without waiting, as every install, switch and restart
does. The lock is a `flock`, which nothing can take from a live holder. So:

- Every taker writes its process id into the lock file after it locks it; a refusal names the
  holder (`pid N, which is alive`, `which is gone`, or a helper from before this one that wrote
  none).
- Every taker takes the lock while it holds a guard, `<lock>.take` (taken without waiting), and
  checks, once it holds the lock, that its file is still the one at the name. A taker that opened
  the old file before a replacement, and locks it once its holder lets go, is busy.
- `--force-lock` replaces the lock file: the holder keeps the old file, the name gets a new one
  that the forced taker locks. The forced taker takes the guard first and keeps it for as long as
  it holds the lock, so no other take completes meanwhile: a second forced request, and a normal
  one, are busy until the activate ends. Under the guard it replaces the file only if it is still
  the file it found held and the holder is still the one it read before it decided; a holder that
  came since (the first let go and another took the lock) is not taken from, and the forced taker
  is busy.
- The take is appended at once, before anything else is read, to `lock-takes.jsonl` beside the
  helper's `state.json` (one JSON line per take; a line cut short by a crash is ended before the
  next). It is a file of its own because helpers before this one read `state.json` strictly.

The locks taken inside the install lock (the installer's state, `retain.json`) are taken without
waiting too: an emergency command is refused, never left hanging, behind a writer that does not
let go. On a Mac the ssh path's apply job runs without the install lock; launchd is asked about
that job whatever its status file says. Only launchd's "no such service" (exit 113) or a listing
that says the job ended counts as not running: another status, a signal, or a `launchctl` that
cannot be run counts as running.

### The pin

`--pin` writes the hold file in the agent's state directory (`pin.json`, root's, 0644) and pins the
release in `retain.json`, so no prune removes it. They are written once the links name the
release (also after `restart_failed`): a refused or failed swap pins nothing. The hold file's
directory must be there and `retain.json` free, checked before anything changes. The hold file is
written first, then `retain.json`; a pin that fails between them is told (`state_unavailable`, the
release running) and is completed by asking again.

The agent's directory is the agent account's, so the hold file is written through the directory's
descriptor, made aside with `O_EXCL|O_NOFOLLOW`, renamed over the name, and the name is checked to
hold the file written (an owner that swaps the aside file before the rename makes the write an
error, not a published hold); the directory is synced. That account can always remove or replace
the file afterwards: the agent that reads it is to trust only a file owned by root.

### Compatibility

Nothing a previous release reads changes shape: `state.json` keeps its keys and words,
`retain.json` gains entries in its existing `pinned`, the lock file's content is never read by
the previous release's lock readers, and `lock-takes.jsonl` and the hold file are names it never
opens. The socket's new request and answers are sent only by and to this release; a previous
helper answers `activate` with `bad_command` and runs nothing. The tests that hold this are in
`crates/kbf-host-install/src/local/compat_tests.rs`.

### Interruption

On Linux the state is written before the links (the installer's crash story): a run cut short
after the state is finished by asking again. On a Mac, a run cut short between the move of the
links and the removal of a trial's marker leaves the marker, and its deadline then puts back the
release that ran before the trial, as for any trial (the window is one `unlink`); a run cut short
after the marker went is finished by asking again. Asking again for the release that runs restarts
its units and moves no link.
