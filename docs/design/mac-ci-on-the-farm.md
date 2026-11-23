# macOS CI jobs on the build farm's Macs

A GitHub workflow job that needs only a Mac (a build or a test as an ordinary user, no root, no
signing key) can run on one of the build farm's Macs instead of a GitHub-hosted macOS runner.
This page says how, using only what exists, and what is still missing.

## Mechanism: one REAPI action from a Linux runner

The job runs on a hosted Linux runner. The macOS work is one Remote Execution action whose
platform names the farm's Mac workers. The farm already runs such an action as a job on a Mac
([actions-as-jobs.md](actions-as-jobs.md), section 14): the frontend's executor submits a native
job with the released `kbf-task` bundle (`--executor-mac-task-bundle`), and the Mac's agent runs
it as a throwaway pool user under a sandbox that has no network.

Buck2 repositories get this from Buck2: a target whose execution platform is the Mac one. A plain Cargo
repository has no Buck2, and kbf may not use one for its own build (AGENTS.md, B13), so it gets
it from `cargo xtask mac-farm` (crates/xtask/src/mac_farm.rs), a REAPI client that sends one
Cargo command:

1. The input root is the checkout's tracked files; the crates of `Cargo.lock` for
   `aarch64-apple-darwin` (`cargo fetch`, on the runner, into a Cargo home of the run's own);
   and the pinned Rust toolchain for `aarch64-apple-darwin` (`toolchain.toml`: the release's
   rustc, cargo and rust-std tarballs by URL and SHA-256, the release `rust-toolchain.toml`
   names). A Mac has the Command Line Tools and no Rust, and an action has no network, so all
   of it travels with the action.
2. The Command is that toolchain's `cargo` with the job's arguments (`test -p kbf-outputs
   --locked`), offline, with the Mac's `/usr/bin` for its C compiler and linker.
3. The blobs the farm's CAS lacks are uploaded (batches for small ones, ByteStream for large).
   The CAS is shared, so an unchanged toolchain or crate is sent once, not per run.
4. The action is executed and followed to its end; a cut stream is followed again with
   WaitExecution, or executed again when the farm has forgotten the operation.

## Authentication

The farm's REAPI port is reachable only on its private network. A job joins it with
`tailscale/github-action` and the run's OIDC token (workload identity federation, tag `tag:ci`;
the organisation variables `TS_CLIENT_ID` and `TS_AUDIENCE`), so no secret is stored. The farm's
address, instance, TLS setting and Mac platform properties are the organisation variables
`FARM_ADDRESS`, `FARM_INSTANCE`, `FARM_TLS` and `FARM_DARWIN_ARM64_PROPERTIES`; no repository holds
an endpoint or a property set. There is no application-level check at the frontend today
(docs/reapi-auth.md: no `--reapi-auth-policy` accepts every call): membership of the network is
the gate.

That needs `id-token: write`. kbf's workflow lint lets a job hold it only for a push to `main`
(`SIGNING_PUSH_ONLY`), so that no pull request or merge-queue run holds a token that can sign
an attestation. A pull request's macOS jobs therefore stay on hosted runners until that rule
changes; the farm job here runs after the merge. A fork gets no token and no variables: the
tailnet join is skipped, and with no `FARM_ADDRESS` the verb prints a notice and passes.

The variables are not secrets, so a step's `env:` header in the log shows them, as it does in
the project's other farm workflows; the verb masks the address (`::add-mask::`) before it prints
anything, so nothing it prints shows it.

## Logs and the exit code

The farm answers with an `ActionResult`: the command's exit code, and its stdout and stderr as
CAS blobs, in full. The client prints both and ends 0 only for exit 0. When the farm could not
finish the job (lost, killed, over its time), the answer is a status with no result, and the
farm attaches the end of the job's output (`JOB_OUTPUT_TAIL`, as many bytes as the frontend is
set to keep); the client prints that and the status, and ends 1. Nothing is streamed while the action runs: the log shows the stage
changes (queued, running) and then the whole output. A passing result is cached by the farm, so
an unchanged commit's rerun is answered from the cache (and says so); a failing one is never
cached.

## Time limit

The action's timeout is `--timeout-secs` (default 2400 s), which the frontend holds under its
`--executor-max-run-secs` (default 3000 s) and refuses above it. At the deadline the command's
process group is killed and the result says exit 137. The client has a connect timeout and no
other deadline of its own: a step's `timeout-minutes` bounds a run that waits in the queue, or a
connection that hangs.

## When no Mac is free

A Mac that can run the action but is busy: the action waits in the job service's queue, and
the log says it is queued. A start a Mac refuses is placed again with back-off until the
action's deadline. No Mac registered or able to run it at all: the Execute is refused at once
with `INVALID_ARGUMENT`, and the job fails naming the refusal. A farm that cannot be reached
fails the connect.

## What a Mac action cannot do

The sandbox denies every network operation, loopback TCP and Unix sockets included, and a
process already under the sandbox cannot apply a sandbox profile of its own. So a test that
binds a socket (update-register's), or runs a Command under `sandbox-exec` (kbf-task's), fails
there, and stays on a hosted runner. There is no root, no GUI session, no device and no
keychain.

## Missing

- A per-pull-request farm job: needs a ruling on `id-token: write` for a pull request's job.
- A loopback-only network level for a Mac action: the job service has one for native bodies, but
  no job level maps to it and kbf sends `none`.
- Live output while the action runs (the REAPI has no log stream here).
- Root-needing jobs: a macOS VM job (two per Mac under the licence); the agent has a VM backend
  that is off unless configured, and kbf has no path to it.
- Signing and notarisation: where the keys live is not decided.

## In this repository

`macos-farm.yml`'s `macos-farm` job runs, after the merge, the macOS crates whose tests need
neither a socket nor a sandbox of their own (kbf-outputs, kbf-seatbelt, kbf-update) on a farm
Mac; `beta.yml`'s `native-macos` no longer runs kbf-outputs, and keeps the tests that cannot
move. A pull request still runs all of them on `macos-pr.yml`'s hosted runner (see
Authentication).
