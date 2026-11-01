# CI/CD for kbf: beta, gamma, prod, and the walk back

Status: design. Nothing in this page is built except where it says **[F]** (read in the tree). **[P]** is a proposal.
**[V]** is something to verify by measurement before relying on it. Open pull requests and issues are named by number;
re-check their state before acting on this page, because the table in section 12 rots first.

After the cutover, `main` is the only integration branch and the workflows know no other. The
`feature/arch-v2` references below that describe the plan, the status table (section 12) and the
decisions (section 13) are history as written, and are kept as such; the statements about what
the workflows do now say `main`.

**The frontend update.** Where this page plans a frontend update as a handover (a new instance
beside the old one behind Envoy, the old one relaying its open streams: B2, B8, B11, M5, the walk
back, the gate on `kbf_frontend_relay_untraced`), that mechanism was removed and kbf runs no
Envoy. A frontend of a pool behind `kbf-proxy` is rolled one at a time by `kbf-updater`: drained
at the proxy, retired (`POST /v1/frontend/retire`), installed, restarted and put back
(`docs/design/frontend-proxy.md` section 4). The series `kbf_frontend_relay_untraced` and
`kbf_handover_*` are no longer emitted, so a row or gate of the plan that reads them is not built
and cannot be.

## 0. The plan in one page

Every merge runs a **beta**: GitHub Actions stands up a whole farm on loopback (a frontend, three storage-and-daemon nodes,
one plain daemon; Buck2 dials the frontend's REAPI port directly), runs real Buck2 builds, then updates every role while a build is running and
kills a node while a build is running. The assertions are numbers, not logs: zero failed actions, zero lost blobs, and
under-replicated blobs back to zero.

A commit that passes beta may be offered to the operator's own farm: **gamma**. For our own build farm the trigger is
automatic (RETIRED: `gamma.yml` and the `kbf-installer deploy` verb are deleted, and hosts take releases from OS packages and kbf-updater; the text of this page about the deploy is history): when a push to `main` has been cut into a release, `gamma.yml` verified the release, made the offer for gamma
and deployed the commit on the farm's frontend host over Tailscale SSH (section 5.4); the new frontend then offers it to
the nodes, which pull it. The deploy is itself a test (no build failed because a connection was dropped). Planned: gamma's
verdict (gamma verdict TBD, on a generic monitoring library) then comes from the operator's real usage over the metrics
the gamma farm emits during a 24 hour **bake** (section 7); nothing produces that verdict today.

**prod** is the existing attested-artifact release (tag, files, notes), cut from a push to `main` whose one pipeline run (`ci`, the `beta` source tests,
`artifacts`, `artifacts-sign` and the `smoke` on those artifacts) succeeded, and gated on the checks of the files (sections 8.0 and 8.1); no verdict document gates it. **Rollback** is the updater walking back through the same mechanism it walked forward with: the
desired commit is set to the older release under a higher generation.

```
 merge ──> artifacts (build once, attest) ──> beta (Actions, ephemeral farm)
                                                  │ result attested (same set hash)
                                                  v
                          gamma: operator's farm, pull updater, deploy-as-test
                                                  │ Planned: verdict from a monitoring library
                                                  v
                         (audit only) ──> prod: tag, attested files, notes
          any gate red ──> rollback: desired = the old commit, higher generation
```

Four rules hold in every stage, and each is a rule that exists in the project already: **one frontend update process
everywhere** (a new instance on other ports behind Envoy, the old one drains, in-flight `Execute` streams are kept; the
Buck2 client never re-sends a broken `Execute`; the serving frontend is never restarted), **nodes are updated in any order
when idle or drained, within N and N-1**, and on an upgrade the frontend goes first, only through Envoy, never waiting for
`open_executions == 0` (the order of `split-mode-updates.md`); **the farm pulls the bytes**: a node fetches its release
from the frontend and verifies it itself. The one push is the gamma trigger (section 5.4): it sends the release's files and
the order to deploy to the frontend host and nothing else, through a Tailscale identity limited to that one host and
command, so no other stage holds a way into the operator's network; and **hosts trust the build, not the operator**: a
host verifies the GitHub artifact attestation of what it installs, and there is no operator signing for now.

## 1. Goals and non-goals

Goals:

1. A regression that fails a build during an update is found on the merge that introduced it, not on the operator's farm.
2. The operator's farm is never the first place a release runs, and is never updated by a person typing commands to a host.
3. The bytes that run in the full beta, in gamma and in prod are the same bytes: one build per commit, promoted, never
   rebuilt. (The merge-queue smoke runs the queue ref's own build, which is not what ships; section 4.1. The first release
   is rebuilt after the branch merges to main and re-runs beta and gamma on that commit; section 13, Q1.)
4. Every gate is a number with a threshold that the doc states, and every gate fails closed: an absent metric is a failure.
5. Rollback is a normal, tested path, and the doc says which releases cannot be rolled back.

Non-goals: a deployment tool for anything but kbf; multi-region or multi-farm rollout; push-style deployment of any kind;
making the farm reachable from a public runner.

## 2. The stage model, reused from kci, and where kbf differs

kci is the release tool in the sibling komira repository (its stage model is in that repository's `docs/ci.md`). Restated
here so this page stands alone: a release is a list of stages, each with a name, an `after` ordering, a named GitHub
environment and a result document; one job per stage; the same release directory is promoted to every stage and never rebuilt
(`build.set_hash == gamma.set_hash == prod.set_hash`); a validation job holds no identity token; `prod` needs a reviewer and
is manual. kci's stages are build, gamma and prod, and kci's gamma is a publish channel. Here, "reuse" means vocabulary and
shape only: kbf's build stage is `artifacts.yml`, its gamma is a pull-based farm with no Actions job, and beta is added.

What carries over, unchanged in meaning:

| kci | kbf |
|---|---|
| stage list with `after:` | `build` (implicit, `artifacts.yml`), `beta`, `gamma`, `prod`, in a small checked-in stage file |
| one environment per stage | `beta` (no secrets, no reviewer), `prod` (reviewer); `gamma` is described below |
| build once, promote by set hash | `artifacts.yml` already builds once per commit and writes `SHA256SUMS`; the set hash is the digest of that file |
| validation never a skip, distinct exit code | each stage ends in a result document with a verdict; "not run" is not "passed" |
| result document written RUNNING first, FINISHED on every exit | the same, one JSON per stage, uploaded as a workflow artifact |
| workflow checked against the stage list | a lint in `crates/kbf-integration-test/tests` beside the existing workflow lints (hosted runners only, SHA-pinned actions, `permissions: {}`) |
| least privilege: the job with the publish token holds nothing else | `attest` is already the only job with `id-token: write`; the new jobs hold `contents: read` |
| dry run by default | `release.yml` has no hand-started run (it follows the pipeline, section 8.0); gamma-offer gets the dry-run switch |

Where kbf differs, and why:

1. **There is a beta stage; kci has none.** Beta is new: an ephemeral farm per merge that
   publishes nothing. It is closest to kci's build-plus-validation, not to a channel.
2. **Gamma is not a publish destination.** kci's channels are places bytes are pushed, permanent and never overwritten.
   Gamma is a farm that pulls: "the desired release" is the `update/desired {generation, commit, set_at}` row in the
   operator's frontend state store, and it moves. A moving pointer is the opposite of never-overwrite, so every move bumps a
   generation that only grows, and a rollback is the older commit under a higher generation. Downgrades are allowed; the
   generation is the guard (`pull-updates.md`, `split-mode-updates.md` 2.3).
3. **Gamma's bake runs outside Actions.** The deploy is started from Actions (section 5.4), but a hosted runner on a public
   repository must not read the operator's farm, so the farm's metrics cannot be read from a workflow. kci's farm-connected
   jobs are not adopted. Planned: the operator side runs the evaluator and returns a verdict (section 7).
4. **There is a metric gate, and no rollback verb in kci.** kci has no soak or metric gate, and no rollback verb. The deploy's
   gates are built (section 5.2); the verdict over gamma's metrics is Planned (section 7).
5. **Code is not shared.** kci is Mojo on Buck2; kbf is Rust with `xtask`. The stage file is plain data checked by a Rust lint.
6. **kci's DEPLOY step** (a cloud resource graph apply) does not describe restarting kbf processes on a host. The
   deploy here is the frontend update (a roll of the pool, `frontend-proxy.md` section 4), then the pull updater's node steps.

### 2.1 The stage file [P]

`release/stages.toml` (name is open), read by the lint and by a small `xtask` that the workflows call:

```toml
[[stage]]
name = "build"
runs = "actions"          # artifacts.yml, the implicit build stage; attests on push only
after = []
result = "SHA256SUMS"

[[stage]]
name = "beta"
runs = "actions"          # a job in .github/workflows/beta.yml
environment = "beta"
after = ["build"]         # smoke: the merge_group build; full: the push build and its attestations
result = "beta-result.json"

[[stage]]
name = "gamma"
runs = "operator"         # no workflow runs it: hosts take releases from OS packages and kbf-updater; the bake and the verdict (Planned, section 7) run outside Actions
after = ["beta"]
result = "gamma-verdict.json"

[[stage]]
name = "prod"
runs = "actions"          # release.yml
environment = "prod"
after = ["gamma"]
result = "release.json"
```

The lint refuses a workflow job that names a stage not in the file, an `actions` stage with no job, a `pull_request` trigger
on any of these workflows, an unpinned `uses:`, and `id-token: write` anywhere but `attest`. `runs = "operator"` stages have
no job. The stage file as built is `release/stages.yml`, which lists no gamma stage (no workflow runs gamma; hosts take releases from OS packages and kbf-updater), so `prod` follows `beta`.

## 3. The artifact every stage consumes

`artifacts.yml` is the build stage and is not changed in kind. **[F]** It builds `kbf`, `kbf-daemon`, `kbf-server`,
`kbf-deploy` and `kbf-installer` for linux x86_64, linux arm64 and darwin arm64 (docs/artifacts.md lists which binary for
which platform), deterministic tarballs, SBOMs, `SHA256SUMS`; on a push to main
`attest` adds provenance and SBOM attestations and `verify` checks them. An attestation names its ref,
so an asset attested on another branch (historically feature/arch-v2) does not verify as main's.

What the stages add:

- **The full beta runs the attested files**, not a fresh `cargo build`. It runs after the push build has attested (a job
  of `artifacts.yml` with `needs: verify`, section 4.1), downloads that output, runs `cargo xtask verify-assets` on it, and
  starts the farm from those binaries. Otherwise beta would test bytes nobody ships. The harness (`cargo test`) is the one
  thing built in the beta job; it is test code, not a shipped file.
- **The merge-queue smoke is unattested.** `artifacts.yml` also runs on `merge_group` (and `pull_request`) but attests only on
  a push, so no attested files exist before the merge. Smoke runs the binaries that `merge_group` build produced (same recipe,
  unattested, not the shipped bytes), or, if waiting for that build is too slow, binaries it builds itself with `cargo`.
- **The beta result is attested.** A last job, the only one in beta with `id-token: write` and `attestations: write`, attests
  `beta-result.json` (commit, set hash, harness commit, every gate with its measured value). No repository code runs in that
  job, as in `attest` today.
- **The offer** (section 6) sets the desired commit, and the hosts fetch the release and verify its artifact attestation. A
  node never sees a beta result; `kbf release offer` checks it before it sets anything (section 5.1).
- **A previous release for N-1.** Beta also fetches the last released set (or, before there is one, the newest earlier
  commit with an attested build on the same branch) and verifies it the same way. This is how the N/N-1 jobs of section 9 have
  an N-1.

Releases are cut from main only (decided, #961). Development continues on feature/arch-v2 until the farm works; the branch
then merges to main (#753), and from then on every release is cut from main. Beta and gamma run on the feature branch
meanwhile; nothing is published from it.

## 4. Stage 1: beta

### 4.1 Trigger and cost

Two layers, because "on every merge" and "a merge must not wait an hour" pull apart:

- **Smoke, in the merge queue** (`merge_group`): start the whole farm, run builds, assert zero failures. No update, no kill.
  It needs a build of the queue ref, so its duration is that build plus the smoke, not the smoke alone **[V]**: the target of
  10 minutes holds only if the build is cached or the smoke uses its own `cargo` binaries (a cheaper build than the multi-platform
  one, since smoke needs one platform). Measure that in B-0 before the check is made required; the order of preference is the
  `merge_group` build's output, then smoke's own build. Smoke is unattested and says so in its result document.
  `merge_group` runs repository code from the queue ref with no fork code, so the root-level helpers are acceptable there.
- **Full, after the merge** (`push` to main): rolling update of every role under a running build,
  kill-a-node, the Envoy swap, the all-in-one variant, N/N-1, on the attested bytes. It is part of the one
  pipeline of section 8.0: `smoke.yml`, called after the sign call of `artifacts.yml` in the same run, reads the attested files from
  that run's artifacts and needs no `workflow_run` for the order. Built today: the smoke job runs the `kbf` of the
  linux-x86_64 tarball of that run (checked against `SHA256SUMS` and for the commit); the jobs of `beta.yml` test the
  commit's source, build what they run and start beside `ci`; the scenarios that need the attested bytes are the later work named here. A red full run does not revert anything; it **blocks the
  commit from being offered to gamma** (the offer needs its beta-result attestation) and opens an issue automatically.
  A scenario becomes a required part of the full run on the day its blocking pull requests merge (section 4.5), not before,
  because a fail-closed gate on an absent metric is red by construction.

Neither runs on `pull_request`: they start a farm with root-level helpers and attest results, and a pull request from a
fork must not run either. The lint of section 2.1 enforces the absence.

A pipeline that has started is never cancelled (section 8.0, beta included); the pushes that arrive meanwhile wait, and only
the newest waiting one runs. A commit that never ran its beta has no result, which is "not run", never "passed". The offer
takes the newest commit with a green beta, so skipped commits cost nothing.

### 4.2 Runner shape: does a whole farm fit on a hosted runner

**[V]** The proposal is the standard hosted Linux runner (about 4 vCPU and 16 GB on a public repository; the 6 hour job
limit is far above need). The rehearsal harness already starts this topology as a `cargo test` in `test-x86`. **[F]** A
rough budget, to be measured before it is relied on:

| process | role | expected footprint |
|---|---|---|
| frontend (two instances during a swap) | scheduler, state store | a few hundred MB each |
| 3 storage-and-daemon nodes | storage, plus a worker each | the largest; segment size and cache caps set small by the harness |
| 1 plain daemon | worker | small |
| Buck2 client, plus the actions | the load | the variable part |

The rule that keeps it inside the box: the harness sets segment size, cache and tier limits to test values, and the load is
actions that sleep or touch small files (the `//:pause` shape that the `m1`/`m2` cells use), plus a modest compile.
**Beta is a test of the update and failure paths, not a benchmark.** If a measurement shows the farm does not fit, the order
of remedies is: smaller limits; then fewer concurrent actions; then a larger GitHub-hosted runner (billed, unlike the standard
one; still ephemeral and managed by GitHub); and last, and only with the safety conditions below, a self-hosted runner.

Why self-hosted is the last resort, on a public repository: a self-hosted runner executes whatever a workflow tells it, and
a workflow can be changed by a pull request. A non-ephemeral runner keeps state between jobs, so one job can leave something
for the next. The conditions if it is ever used: the runner is ephemeral (one job, then the machine is destroyed), it sits in a
network with no route to anything of the operator's, it takes only the `beta` workflow, that workflow never triggers on
`pull_request` or on a fork's code, the job holds no secrets, and the runner group is restricted to the one repository and
workflow. Arm runners are hosted (`ubuntu-24.04-arm`) and available for an arm beta variant; darwin is covered by `native-macos`
and `sign-darwin` and has no farm role in beta.

Cost shape **[V]**: per merge, one smoke (minutes) in the queue and one full run after; standard hosted runners on a public
repository carry no per-minute charge (a larger runner does). The default build concurrency in smoke is a figure measured in
B-0, not the 20 of T5, which is a likely memory risk. The cost that matters is wall time and queue depth, which is why full is off the
merge path. Record job duration in the result document so a creeping beta shows in the numbers.

**Container runtime [V].** The project runs containers with crun only. The harness today uses a stand-in runtime over a planted cgroup tree.
Real crun needs cgroup v2 delegation and a systemd scope; the crun jobs in `beta.yml` arrange that with a `Delegate=yes`
unit, and the same approach is to be tried for crun on `ubuntu-24.04`. Until that is shown to work (#936), **B-0 uses the
stand-in**, and the page says what that weakens: the `//:pause` and compile loads run through no real container path, so
beta proves the update and failure paths and not container execution (covered by the existing real-runtime jobs). The
runtime swap is a one-line change of the supervisor's configuration once #936 lands, not a rewrite.

### 4.3 The harness

`crates/kbf-cli/tests/rehearsal_in_a_box.rs` with `rehearsal/{pki,procs,units}.rs` **[F]** starts a real frontend, three
storage-and-daemon nodes and one plain daemon, each a real `kbf` process whose argv is the `ExecStart=` line that
`kbf_deploy::linux::modes::render` writes, each with its own loopback address and its own certificates from the farm's
certificate rows. A Buck2-shaped client writes and reads the CAS, runs one `Execute` and gets an action-cache hit. It
updates nothing, kills nothing, runs no real Buck2 and has no Envoy. **[F]** Its stated deviations: a directory as the cold
store, a stand-in container runtime over a planted cgroup tree (#936 removes this deviation: crun only), and no systemd.

The proposal is to grow it into a library (`rehearsal`) that two test cells share, instead of a second harness:

- `procs`: a supervisor with `start(role)`, `term(role)`, `kill(role)` (SIGKILL), `restart(role)`, and logs per process kept as
  artifacts. No role is started any way but the rendered unit line, so the unit text stays under test.
- **Envoy**: dropped from B0 per the no-Envoy ruling (the box Envoy kit is gone). The Buck2 client dials the frontend's
  REAPI port directly; B0 moves behind the kbf-installer proxy in a follow-up, once the proxy lands.
- **Cold store**: a directory by default (fast, no service); the `file://`-versus-MinIO difference is covered by the
  existing `objstore-s3` job and is not a beta concern.
- **Load**: the pinned Buck2 (the `kbf-cell m2` machinery **[F]**) runs builds in a loop for the whole scenario. Three
  kinds of action: a short compile; a long action (30 s, `//:pause` style) so builds span an update; and an uploader that
  writes new blobs so write paths are live during the roll. Concurrency is a harness parameter (default 20 builds, as T5).
- **Measurements**: a scraper reads every process's metrics endpoint every second into the result document; the builds' event
  logs give the per-action outcome.
- **The update under test is the real update code**, not a script that restarts processes: the frontend handover command,
  the node step and the pull-updater client and root helper, in that order of availability (milestones B2 to B4 below). The
  one honest deviation is that the supervisor, not systemd, starts the processes; the `systemctl` edge is covered by gamma.
  The root helper runs under `sudo` on the runner (`sudo-test` already does this in `beta.yml` **[F]**).

**Status of the first change (#942).** The helpers above are now the library `crates/kbf-harness`, which `kbf-cli`'s
rehearsal drives: `Farm` (a supervisor with `kill`, `term`, `restart`, a process table and a report of every process's
output for a hang), the metrics scraper, the gates, the result document, the Buck2 load, and `kbf-beta b0`, which runs scenario B0 and writes `beta-result.json`. `.github/workflows/beta.yml` runs it on
the merge queue and on a push; it builds its own binaries, so the result says it is unattested. Running it showed what
the design did not know:

- `kbf storage-and-daemon` runs the container driver only, and the stand-in `crun` runs one planted argv whatever a
  build sends, so B0 runs three `kbf storage` nodes and two plain `kbf daemon` processes with `--driver=native` (the
  result says no container path was exercised). The rehearsal keeps the stand-in over storage-and-daemon nodes.
- The `kbf storage` unit renders neither `--frontend` nor `--advertise-addr`, so the harness adds both. It registers the
  nodes with the frontend. (Envoy is dropped from B0 per the no-Envoy ruling; B0 moves behind the kbf-installer proxy in
  a follow-up. Buck2 dials the frontend's REAPI port directly.)
- The frontend holds no cache, so `kbf_cache_lost_blobs` and `kbf_cache_blob_losses_total` are the storage nodes': a
  storage node serves `/metrics` on `--metrics-listen` (plain HTTP, off unless given; no unit renders it, so the harness
  adds it on a port of its own). B0 scrapes each node as well as the frontend, and the two loss gates sum the nodes and
  fail closed when any node's series is absent or the node does not answer.
- Buck2 reads the remote execution client's address and CA when its daemon starts and ignores them on a command line, and
  refuses a `grpcs://` address: the harness appends them to the copy of the project's `.buckconfig`.
- A farm's processes bind their own ports, so the harness cannot hold the sockets, and a port found by binding port 0 and
  closing is free only until another program takes it. Each farm claims a block of 64 ports below the kernel's ephemeral
  range by holding a file lock named for the block (the kernel drops the lock when the holder dies), skips a block with any
  port in use, and takes every port from its block: two farms of one process or one host
  never share a port.
- A run that is cancelled or times out must leave nothing behind. Every farm process leads a process group of its own,
  ending a process ends its group, and the kernel kills a farm process whose harness dies (SIGKILL included). SIGTERM and
  SIGINT make `kbf-beta` end through its normal exits: the farm is stopped within one bound, the Buck2 daemons of the loops
  are killed (also if the run panics), and the result document ends `FAILED` with the reason. The frontend's control
  socket is removed. The Buck2 daemons are not the harness's children (each leads a session of its own, started by a
  client that has ended), so the kernel does not kill them: before the farm starts, the harness starts a sentinel,
  `kbf-beta reap`, in a session of its own with no parent-death signal and a pipe the harness holds as its input. A run
  that ends tells it `done`; a run that dies by any means, SIGKILL included, closes the pipe with no `done`, and the
  sentinel kills the daemons of the loops and removes the control socket. Only a SIGKILL of the sentinel too leaves them.
- Keys are written mode 0600 into directories of mode 0700, and none is under `logs/`, which the workflow uploads (a test
  scans it). The stand-in `buck2` is a binary of `kbf-harness`, not of `kbf-cli`, so no release builds it.

### 4.4 Scenarios and gates

All scenarios run inside one farm start where they can, to save minutes; each is a separate named test so a red run says
which. Each scenario states the defect it exists to catch, and the full run is checked once with a planted mutant per
scenario (the mutant is listed; a gate never seen red is not a gate).

| # | Scenario | Gate (all must hold) | Defect it catches (mutant) |
|---|---|---|---|
| B0 | `farm_under_build_load` | the load runs 5 minutes with no update: 0 failed builds, 0 failed actions, `kbf_cache_lost_blobs == 0` | a farm that cannot hold the load it is meant to be updated under (shrink a node's cap) |
| B1 | `roll_storage_nodes` | all three storage-and-daemon nodes updated one at a time, idle by idle (`split-mode-updates.md` T1) | two nodes at once (drop the slot CAS); a write failing in the planned-down window (skip planned-down) |
| B2 | `roll_frontend_envoy` | a handover through Envoy with 20 builds in flight and one `Result` held at the stand-down (T5, T6, E1, E2) | a fence after demote; no retry policy; GOAWAY with no health flip |
| B3 | `roll_daemon` | the plain daemon updated at zero leases; a leased daemon is deferred (T2, T15) | picking on lease count alone |
| B4 | `roll_everything` | B1 to B3 in the production order (nodes, frontend, daemon hosts) inside one running build loop | an ordering that breaks N/N-1 |
| B5 | `kill_a_storage_node` | `SIGKILL` of one storage-and-daemon node mid-build, restart it | the loss of a blob owned once; a write with no failover (depends on #939) |
| B6 | `kill_a_daemon` | `SIGKILL` of the plain daemon with a lease in flight | a Farm-class failure reaching the client as an action exit code (`failure-classes.md`) |
| B7 | `kill_frontend_cold` | the crash matrix, wired as a required job (#900) | each case's own defect |
| B8 | `roll_all_in_one` | the all-in-one process updated by the same handover (a design that was never built and is removed; an all-in-one host is updated by a restart) | a second update path for the small topology |
| B9 | `mixed_versions` | N and N-1 in both directions, with and without a build in flight (section 9) | a made-required field; a repurposed field |
| B10 | `rollback_walks_back` | after B4, set the desired commit to N-1 under a higher generation and walk back (T16) | rollback restarting the frontend |
| B11 | `rollback_mid_roll` | a rollback (desired set to the older commit) issued (a) after the first node step and (b) after the frontend handover, from that half-rolled state, with a build in flight; N and N-1 mixed in the reverse order | a walk back that assumes a clean farm; a node mid-step abandoned instead of finished or reverted |

The gate expressions common to B1 to B6, B8 and B10, read from the scraped metrics and the builds' event logs:

- **0 failed actions.** Every action of every build in the load ended with its own exit code, none with a farm-class
  failure. The measure: the builds' event logs, and `kbf_execute_errors_total` delta equal to the number of actions the
  scenario failed on purpose (zero here). Today that counter has no cause label, so a dropped connection cannot be
  told from a client abort (the series `kbf_frontend_relay_untraced` that once told them is gone). **[F]** `kbf_execute_failed_actions_total{class}` (`farm`, `request`, `action`, `ambiguous`; section 12, M10) counts operations
  finished as failed by class, and the beta gate `failed_actions_by_class` is the delta of every class over the run: zero for
  every class here, and a class whose series is absent fails it. The event-log gate `actions_failed` and the
  `kbf_execute_errors_total` delta stay beside it.
- **0 builds lost to a handover**: (not built: `kbf_frontend_relay_untraced` is gone, see the note at the top).
- **0 lost blobs**: `kbf_cache_lost_blobs` is 0 at every scrape, and the delta of `kbf_cache_blob_losses_total` (a counter
  that never falls) is 0 over the scenario. The gauge alone can fall back to 0 after a loss.
- **Under-replicated blobs back to 0**: `under_replicated_blobs` is 0 within the settle window after each node step
  (**[P]** 300 s in beta; tuned from B1 measurements) and at the end. It may be above 0 only between a node going out and
  the settle window ending, and never for two nodes' worth at once. The gate is **absent means failed**: if the metric is
  not exported the run is red (T12's "absent debt metric" rule). The metric is not on feature/arch-v2 yet (#912, #913).
- **Never two members non-Active at a sampled instant**, in B1 and B4: `kbf_members_not_active` is at most 1 in every scrape
  of the frontend (gate `members_down_at_most_one`). Absent means failed: a membership that cannot be read leaves the series out.
- **Each updated process restarted once, none other**: for the storage members, the growth of `kbf_member_boot{member}` over
  the scrapes is at most the restarts allowed for each member (gate `member_restarts`; 0 in B0). **[missing]** The frontend,
  daemon, proxy and updater have no boot series (M13): for them, in beta, the supervisor's process table (pid and
  start time per role); in gamma, the updater's phase log.

B5 and B6 are expected to *pass the build*. They are not "the farm survives anything": a second unplanned loss while one
node is out is the stop rule of T14 and is tested there, not asserted green here.

### 4.5 Order of work: the beta is built up in steps

A beta that needs every update PR to land first would not start for a long time. The harness is delivered as milestones, each
useful alone and each required from the day its blocking pull requests land (B-2's gates are red by construction until
#912 and #913 export their metric, so B-2 is not enabled before them):

| milestone | adds | blocked on |
|---|---|---|
| B-0 | harness library, the real Buck2 load, B0, smoke in the merge queue (on the stand-in runtime, section 4.2) | #925 box render; the runner measurement (M21) before smoke is required |
| B-1 | B5, B6, B7 (kill-a-node, kill-a-daemon, crash matrix required) | #900, #939 (write failover) |
| B-2 | B1, B3 (node and daemon roll) | #912, #913, #914, #917 (the metrics and the picker) |
| B-3 | B2, B8 (frontend swap through Envoy, all-in-one) | #930, #937, #938, the wiring PR of #901, #925, #934 |
| B-4 | the pull updater replaces the stand-in for the real thing in B1 to B4; B10 and B11 (rollback) | #908, #929, the poller of `split-mode-updates.md` 6.2 |
| B-5 | B9 against a real N-1, state-store and wire checks | a released N-1 (or the fallback of section 3) |

The assertions "0 failed actions" are only reachable once #791 (the frontend fails every in-flight action on restart) is
closed and the upstream behaviour in #792 is understood (this binds gamma's deploy-as-test too, section 5.2); B2 exists to prove that, and if it cannot be made reliable the model
is wrong and that is reported, as `split-mode-updates.md` says of T5, T6 and E1.

## 5. Stage 2: gamma, the operator's own farm

Gamma is the operator running the release on the farm they use. This page says "the operator's farm" and writes no address,
host or account, so the same text is right for anyone who runs kbf and chooses to take the same role.

### 5.1 The offer

A commit becomes offerable when `kbf release offer` ([P], run on the operator's side) has checked, from outside the farm:

1. the artifacts' provenance and SBOM attestations for the commit and set hash (`verify-assets attestations`);
2. the beta-result attestation for the same commit and set hash, with every gate green and its measured values inside the
   thresholds of section 4.4;
3. that every platform the farm runs is complete for the commit in the frontend's release store (tarball and attestation).

On a red or missing beta result, or an incomplete release, the command refuses and changes nothing. Otherwise it promotes the
commit by setting `update/desired` (`POST /v1/update/desired {commit}`, compare-and-set; the generation moves only when the
commit differs). It signs nothing. Each host's root helper verifies the GitHub build attestation offline when it stages, and
the frontend does not have to **[F in the design, unbuilt]** (`pull-updates.md` section 2; #908 node client, #929 root helper).
Hosts trust the attestation of the build, not the operator. The only exception is the held installer of a test farm, which
runs with attestation verification off; that is a test-farm setting, not a default and not part of this contract.

Offering is dry-run by default (`--plan`): it prints what would change and which gates it read.

The verb is `cargo xtask release offer` (`crates/xtask/src/release/offer.rs`), not a `kbf` subcommand: the pinned `gh` and the
online verification belong to the operator side. It makes no network write. Its output is the file `--out <path>` names (no default, so a refusal cannot leave a stale default-named file): `offer.json`
(`kbf_gate::OfferDoc`: commit, set hash, the beta run or a recorded waiver, `reversible`, the previous commit, the platforms found
complete), which the deploy reads; the desired-state write stays the deploy's. Until a beta result is attested, `--waive-beta
<reason> --operator <name>` is the only way past, for gamma only, and the waiver is recorded in `offer.json`. `reversible` is true
only when both builds state `meta_log` (in `kbf --version --json`) and `meta_log(N) <= meta_log(N-1)`; unknown is not reversible.

### 5.2 The deploy is a test

The deploy (the retired `kbf-installer deploy`, `gamma-deploy.md`) runs on the real farm with real builds in flight: it updates the
frontend first (`frontend-proxy.md` section 4: a roll that drains, retires, installs, restarts and
undrains one frontend at a time), and then watches the pull updater roll the nodes, one storage node at a
time. The deploy is a test because it is checked as one:

- **No build failed from a dropped connection**: every
  build that was in flight at any point of the deploy (the deploy records the invocation ids at its start and at every
  step) finished without a farm-class failure (`failure-classes.md`: a farm problem never reaches the client as an action
  exit code). The invocations record carries the failure class **[F: `crates/kbf-invocation`]**.
- **No lost blobs** over the deploy: `kbf_cache_lost_blobs == 0` and the `kbf_cache_blob_losses_total` delta is 0.
- **Under-replicated back to 0** after each node step within the settle window; the run's own gates (preflight, one storage
  node out at a time, repaired before the next) hold, and every one fails closed.
- **Commit-confirm held**: no process hit its local auto-revert (a node that never becomes active reverts itself in
  minutes; that revert firing during a deploy fails the deploy).

The deploy-as-test gates depend on #791 and #792 (a frontend restart fails in-flight actions, and the upstream client does not
re-attach), exactly as B2 does (section 4.5). Until both are closed the zero-failure gates cannot pass on a real farm, and
gamma deploy-as-test is blocked, not waived.

A deploy that fails any of these halts, walks back what it has changed (section 8) and fails gamma. Nothing about a halt is
silent: the result document names the gate, the value and the host step. The walk back is itself a frontend handover and node
steps and can drop connections too. It is held to the same checks and is reported, but a failure during it is **a red
verdict and a page, not a retry loop**: nothing retries a walk back on its own, and the host's commit-confirm revert is the
floor.

### 5.3 What gamma needs that beta cannot give

Real usage (builds nobody scripted), real devices and hosts, systemd, the real cold store, real load shape and duration.
Beta covers the update logic; gamma covers the environment. A defect that only the operator's environment shows becomes a
beta scenario in the same change that fixes it (section 11).

### 5.4 The gamma trigger, as built (RETIRED)

> RETIRED: `gamma.yml` is deleted and no release workflow run starts a gamma deploy; gamma moves to kbf-updater (package-update design, section 13). This section describes the workflow as it was, until the xtask `gamma` and `release stage-offer` verbs it names are deleted.

`.github/workflows/gamma.yml` (the stage row `gamma` of `release/stages.yml`: `runs: actions`, event `workflow_run`, the jobs
`offer` and `deploy`):

- **Trigger.** `workflow_run` of `release`, `completed`, branch `main`, and nothing else: there is no dispatch, schedule or
  pull request trigger, and a re-run of a run derives everything again from its commit. Each job runs only for a run that
  succeeded, was a `push` to `main` and is of this repository (not a fork's, a pull request's or a dispatch's). The
  workflow lint allows `workflow_run` only in this file, the workflow named `gamma`, and holds it to this shape: the exact
  trigger and `if:` of each job, the exact permission sets, the word `secrets` in one step, the host and user variables in
  one step, the `concurrency` group on the `deploy` job and nowhere else, a checkout of no ref, no event field in a
  script, and the names and paths of what it uploads (each with `overwrite: true`). Every `cargo xtask` line of the file
  is also given to xtask's own argument parser, so a misspelt flag fails a test and not the first release.
- **`offer`** has read scopes (`actions`, `attestations`, `contents`), no environment and no credentials.
  `cargo xtask gamma evidence` asks GitHub whether the run was the `release` workflow's file, a push to `main` that
  succeeded at exactly the event's commit, and whether its `publish` job succeeded; a push that cut nothing (docs only)
  gives `released=false` and the run ends green. A job list that is missing, not whole or without a job named `publish`
  (a renamed job) is an error, not a skip: a skip is a green run that deploys nothing. Then `cargo xtask release stage-offer` downloads the release, verifies every
  attestation before it runs anything of it, makes the offer for gamma with the fixed waiver of beta (for gamma only, never
  for prod, until the beta result is attested), and writes `offer.json` and the release's files into one tar, uploaded for
  three days. It writes nothing to any farm.
- **`deploy`** needs `offer`, runs in the environment `gamma` (deployments from `main` only) with `contents: read`, and is
  skipped, which is a green run, unless the offer staged a tar and the repository variables `GAMMA_HOST`, `GAMMA_USER` and
  `TS_TAG` are set: unsetting one is the stop switch. The Tailscale OAuth client, a secret of that environment, reaches one
  step, after `cargo xtask` is built. `cargo xtask gamma deploy` refuses unless the commit is the commit of the highest release
  tag (Rule S: a re-run of an old run deploys nothing), then runs `sudo -n /usr/bin/kbf-installer deploy --apply --json
  --commit <sha> --offer - --bundle -` on the host through `tailscale ssh`, with the tar on standard input. The deploy
  account may run that one command as root (`cargo xtask gamma sudoers-line` prints the line, made from the same argument
  list, with the commit as forty `[0-9a-f]` classes and no wildcard). The host verifies the attestations itself: the tar is
  bytes, not trust (`gamma-deploy.md`).
- **`check`** needs `offer` and `deploy`, has the deploy's `if:`, environment and concurrency group, and `contents: read`.
  After a deploy that succeeded it builds komira's `main` against the farm just updated, with the farm's normal caching,
  and passes only on `BUILD SUCCEEDED` with `local: 0` in buck2's last `Commands:` line (`cargo xtask gamma check-build`).
  The job summary shows that line and the verdict; buck2's output stays in a log file on the runner, because it names the
  farm. The targets are the repository variable `GAMMA_CHECK_TARGETS`; the farm's buckconfig is a secret of the
  environment. No flag that bypasses the cache is accepted anywhere (`gamma-deploy.md`, "The check after a deploy").
- **What is public.** This repository's logs and artifacts are. The installer's summary names nodes, so the table and the
  uploaded `summary.json` hold only an allow-list of it (the commit, the result, the exit status and reason code, the step
  names, the platform, state and seconds of each node, the post checks and the offer's numbers), never a node, a free text or
  a field the code does not know. The host and the user are public names: GitHub prints a repository variable in the log
  of every step that has it in its environment, so only the deploy step has them, and its header shows them (the verb masks
  them in what it prints afterwards, and drops what `tailscale ssh` writes to its standard error). Use the short tailnet
  name of the host, never an address. Hiding them would take secrets scoped to that step and a quiet `cargo`; that is the
  repository owner's choice and is not built.
- **Re-runs and a failed release run.** Both uploads overwrite, so re-running the jobs of `gamma.yml` (all of them, or
  the failed ones) does not fail on the artifact name. `gamma.yml` follows only a release run that succeeded: a failed
  `site` or `bump` job after a published release leaves the run failed, and gamma stays silent until the failed jobs are
  re-run and the run is green.
- **Rulings.** An irreversible release is not held: it deploys to gamma. The release notes are meant to carry the line
  (section 8.3); that line belongs to the notes verb and is not built by `gamma.yml` or `cargo xtask gamma`. The
  Tailscale identity may reach the one gamma host and nothing else, and never prod. A deploy can hold the `gamma`
  concurrency group for four hours. The group is the `deploy` job's, not the workflow's: GitHub cancels a pending run
  when a newer one queues, so a workflow-level group would let a push that cut nothing cancel a pending release. A queued
  deploy for a commit that is no longer the highest release is refused before anything reaches the farm.
- **Held, for the operator.** The environment and its policy, the variables and secrets, the Tailscale OAuth client and
  policy, and the host's account, sudoers line and setup were settings, not code; `cargo xtask gamma held-steps` printed
  them. It is deleted with the deploy (`docs/deploy/gamma.md`).

## 6. Promotion: the frontend's desired state

Gamma and prod are not channels and there is no feed. The pull updater reads the state the operator's frontend already
keeps (`pull-updates.md` is the authority for the rows; `split-mode-updates.md` for the order of the rollout).

- **One set of rows in the frontend's state store**: the release rows (`update/release/<commit>/<platform>/<name>`);
  `update/desired {generation, commit, set_at}`; `update/slot` (the rollout slot); `update/host/<id>` (per-host progress); and
  `update/policy`. The disk release index is the source of truth for the bytes; the rows say what is wanted and how far the
  rollout has got.
- **Promotion is setting the desired commit.** Gamma is promoted by setting the desired commit (and so a new generation) on
  the operator's frontend, after beta passed and the offer checks of 5.1 passed. Nothing else in this page moves a farm.
- **The generation is monotonic and never reused.** It is the guard: hosts act on a generation higher than any they have
  acted on, so a replayed older order is refused. Downgrades are allowed; there is no floor on commits. A rollback is the
  older commit under a higher generation (section 8.2).
- **Rollout order is split-mode's.** Nodes update in any order when idle or drained. On an upgrade the frontend goes first,
  and only through Envoy; across a protocol difference on a downgrade the nodes go first. The updater never waits for
  `open_executions == 0` and never restarts the serving frontend.
- **Which release a farm follows is that farm's own desired state.** The operator's farm is promoted by `offer` after beta.
  A farm that runs the public release has its operator set the desired commit to the published one (by hand, or by its
  `update/policy`); no stage reaches into it.
- Nothing here needs a server that can reach a farm: new attested releases reach a host as OS packages and are only ever
  proposed (`pull-updates.md`; the frontend release poller this paragraph once described was deleted, superseded by the
  host-driven package-update design); the operator or the offer command decides.

### 6.1 What the offer checks, and who runs it

`kbf release offer` ([P]) is run by the operator, or (RETIRED) for gamma by the `offer` job of the deleted `gamma.yml` (section 5.4, through
`release stage-offer`): that job has read scopes and no credentials, so running the checks in Actions puts no way into the
farm there. The one job that reaches the farm is `deploy`, through the Tailscale identity of section 5.4, and only for
gamma; the offer for prod is not made by a workflow. It refuses on any failed check of 5.1 and sets nothing. It holds no
signing key and no key is needed: the host-side trust is the GitHub attestation of the build.
Signing operator-authored manifests for hosts to verify is deliberately not done for now; if it returns it is a new decision
with its own page.

## 7. Gamma's verdict (Planned)

Planned: gamma verdict TBD, on a generic monitoring library. The library reads the metrics the gamma farm emits and
produces `gamma-verdict.json`, the document section 8.1 requires. Nothing here is built: there is no evaluator, no
time gate over the operator's real usage, no baseline and no automatic rollback. The deploy's own gates are built and are
section 5.2's. The earlier design of a 24 hour bake (its gate table, minimum load, baseline and hand-off) was removed with
its code, and is in the history of this page.

What the verdict must be, whoever produces it: a record, not a credential (no host reads it), naming the commit, the set
hash, the desired generation, the deploy's start and end, each gate with its measured value and threshold, and any waiver.
A red verdict triggers section 8; a green one makes the commit publishable (the tag of 8.1 carries it). An absent metric
fails its gate, never passes it, and a producer failure is never a pass.

## 8. Stage 3: prod, and rollback

### 8.0 The one pipeline of a push to main

A push to `main` runs one pipeline, `pipeline.yml`:

```
 push to main -+-> ci ------------------------------------------+
               +-> beta (source tests, beta.yml) ---------------+-> passed -> release
               +-> artifacts (build) -> artifacts-sign -> smoke -+      (release.yml)
               |                        (also needs ci)
               +-> watch (cancels the run when a job fails)
```

- **Builds start with ci; signing waits for it.** `artifacts.yml` is called twice. `artifacts` (input `stage: build`) needs
  nothing, so the four builds (12 to 15 minutes each, plus the wait for a macOS runner) start with `ci` instead of after
  it, and signs nothing (its call is granted the signing scopes only because GitHub checks a called workflow's job permissions against its caller's grant statically, skipped jobs included; every job that signs is skipped in this call, which the pipeline lint requires). `artifacts-sign` (`stage: sign`) needs `ci` and `artifacts` and runs only the `attest-*`
  and `verify-*` jobs, which read the files the build call uploaded to the same run; a commit that fails `ci` is built but
  never signed. The build call's files wait in the run's artifact store while the `beta` jobs run repository code, so
  the build call's `sums-*` jobs output the SHA-256 of the `SHA256SUMS` they upload (`sums assemble --digest-output`), the
  pipeline passes those digests to `artifacts-sign`, and each `attest-*` job, before it signs anything, checks the sums it
  downloaded against its digest and every listed file against the sums (two `sha256sum --check --strict` commands under the
  workflow's pinned bash, no repository code: the job holds `id-token`; the lint refuses a `shell:`, `defaults` or `env` that
  could turn them into no-ops). The file check proves the listed files match, not that `dist` holds nothing else (the sums job
  refuses an unlisted file). The files of the build call are covered from the sums job on; what a job could swap between a build job and
  its sums job is not. The attest jobs stay in `artifacts.yml` because an attestation's signer identity is the workflow file that
  signed, and the installers verify exactly that file. A pull request or merge-queue run has no `stage` input, so its
  build and sums jobs run as before and the attest and verify jobs never do.
- **Each check runs once.** `ci.yml`, `artifacts.yml`, `beta.yml` and `smoke.yml` have a `workflow_call` trigger and no `push`
  trigger of their own. `ci.yml` and `artifacts.yml` keep `pull_request` and `merge_group`, so the pull-request gate and the
  merge queue are unchanged; `beta.yml` and `smoke.yml` have `workflow_call` alone (they never ran for a pull request). In a called workflow
  `github.event_name` and `github.ref` are the caller's, so the jobs that attest still read as a push to `main`.
- **The pull request checks the static target.** The release builds `kbf-task` for `x86_64-unknown-linux-musl` and
  `aarch64-unknown-linux-musl` (`artifacts.yml`), where libc's type aliases differ from glibc's (`libc::Ioctl` is `c_int`, not
  `c_ulong`). The `lint` job of `ci.yml` therefore installs `musl-tools` and the musl target, as `artifacts.yml` does, and runs
  `cargo check --locked --target x86_64-unknown-linux-musl -p kbf-task --bin kbf-task`, so a glibc-only type is red on the pull
  request and not on the release build after the merge. `kbf-task` is the only binary the release builds for a musl target.
- **Smoke tests the files that will be released.** `smoke` runs after `artifacts-sign` in the same run and downloads that run's
  `assets-*` and `sha256sums` artifacts (no run id), checks them with `verify-assets sums`, checks the tarball's `kbf` with `build-commit check-asset` (this commit, a
  release build), and the smoke farm runs the `kbf` out of the linux-x86_64 tarball. The release then publishes those same files (`release.yml` downloads them from the same
  pipeline run and checks them again). Only the smoke reads the run's artifacts, so only it waits for `artifacts-sign`; every job of
  `beta.yml` tests the commit's source and builds what it runs, so it starts beside `ci` (the suite is the longest part
  of the pipeline, about an hour, and no longer waits behind two stages that it does not read). The lint refuses a
  `download-artifact` in `beta.yml`.
- **A red or cancelled stage stops the rest, and a red job stops the run.** `artifacts-sign` needs `ci` and `artifacts`, and `smoke` needs
  `artifacts-sign`; none can be skipped, and the workflow has no `always()` or `continue-on-error`. `passed` needs every
  stage and does nothing else, so the run succeeds, and the release follows, only when every job of every stage passed. GitHub
  cancels the rest of a run for a failed job only inside a matrix, so `watch` (`cargo xtask pipeline-watch`) polls the
  run's jobs and cancels the run as soon as one has failed or timed out; the run then ends cancelled and starts no
  release, and the failed job keeps its own conclusion. It ends when `passed` has completed, ends with a notice when it
  cannot read the jobs, and never makes a run red itself.
  **One rerun for a flake of an artifact job.** A job cannot be rerun while its run is in progress and `watch` keeps the run
  open, so the rerun is made after it, by `pipeline-rerun.yml`: it starts when a pipeline run completes (`workflow_run`, the
  second workflow the lint lets follow the pipeline, beside `release.yml`) and runs `cargo xtask pipeline-rerun`, which calls
  `POST /actions/runs/<id>/rerun-failed-jobs` once, only for a push to `main` that failed (or ended cancelled, as `watch` ends
  it), only in its first attempt (the rerun completes as attempt 2 and is ignored, so there is no loop), and only when EVERY
  failed job builds or moves artifacts: the three builds, `sums*`, `attest*` and `verify*` of `artifacts.yml`, by exact name
  (`RERUNNABLE`). A failed test, lint or smoke job, or a job nobody listed, means nothing is rerun and the pipeline stays failed,
  so a flaky test still fails loudly. Each rerun is a line in the summary of the workflow's run with the jobs and the logs of
  their first failure, so flakes stay visible and countable; a rerun GitHub refuses fails that run (exit 1). The rerun runs again
  every job that did not pass, the cancelled ones included. As a `workflow_run` workflow it runs the file as it is on `main`,
  with this repository's token, for a commit that may be any push: it never checks out or runs that commit's code (it builds
  xtask from `main`, before the step that holds the token), its token is `actions: write` and `contents: read`, held only under
  the exact `if:` of the lint (`RERUN_ONLY`), and it holds no secret. A change to this file or to the lint needs a token with
  the `workflow` scope to push.
- **A running pipeline finishes; the newest waiting push queues behind it.** The workflow's group is `pipeline-<ref>` with
  `cancel-in-progress: false`: the run in progress is never cancelled, and of the pushes that wait GitHub keeps the newest
  (a pending run is replaced by a newer one; the release plan measures from the highest tag, so the code of a replaced
  or failed run ships in the next release). Nothing that releases is a job of this run: it holds no tag, no
  release, no push to another repository, no environment and no secret. The only write scopes it grants are the signing
  scopes of the two `artifacts.yml` calls (only the sign call runs a job that uses them) and `actions: write` of the `watch` job (to cancel this run), both under the exact `if:`
  of a push to `main` (the `beta` call grants `attestations: read`
  for its `mixed-version` job); a called workflow gets at most what the caller grants, and GitHub refuses the whole
  workflow at startup when one asks for more, so the workflow lint compares every job of each called workflow with its
  caller's grant. No stage call passes `secrets:`.
- **The release is another workflow, started by the pipeline's success.** `release.yml` runs on `workflow_run` of
  `pipeline`, `completed`, `branches: [main]`, and every job requires the conclusion `success`, the event `push`, the branch
  `main` and this repository. Its concurrency group is `release-main` and never cancels; a pipeline run that was cancelled
  or red starts `release.yml` too (GitHub starts a `workflow_run` workflow for every completion), so its group is the
  run's own and every job skips, which keeps it from replacing a release waiting in `release-main`.

**The guarantee.** A release job is never cancelled by a push: tag, notes, publish and bump are jobs of a workflow no push
cancels, and a release starts only from a pipeline run that finished with `success`, which no push can then change.
A push that arrives while a release runs starts a new pipeline; its release waits for the running one (one pending run per
group, a newer pending one replacing an older). The pipeline is never cancelled, so a busy `main` cannot starve it.
`release plan` measures from the highest release tag that is an ancestor of the commit (the newest `v*` release tag, not
the tag of the commit being re-run), not from the one push: code from a push that was replaced while it waited, or that
queued behind a running pipeline, is always in the next release, and a docs-only push after code changes still releases that
code. With no release tag yet, the first release is everything. **Releases therefore happen at least once per pipeline
length**: as long as a pipeline can finish, the commit it finishes on releases everything since the last release. `code`
and `site` are classified over that whole range; `pipeline` is the commit's own change (its diff over its first parent),
because a range keeps a pipeline file until a release clears it, and a release that waits for no pipeline file in the range
would never be made. **The residual risks, stated:** (1) the run that started the release is read by id
(`github.event.workflow_run.id`): `release locate` refuses a run that is not the successful push run on `main` for the
commit, and one that no longer holds the artifacts (they expire after 90 days); (2) a release job a person cancels, or that
fails, is re-run from the run's page (failed jobs only), and `release tag` and `release publish` are idempotent, so a
half-made release is completed, not repeated; (3) a commit that changes the pipeline does not release itself, and the
next commit that does not releases both.

The workflow lints hold all of it (`crates/kbf-integration-test/tests/pipeline_workflow`, `release_workflow` and
`workflows`): the order of the stages and their `needs`, the triggers of the called workflows, the single call of each,
the concurrency of the pipeline (never cancelled) and the absence of a write in it, the scopes granted to each stage, the shape of the one
`workflow_run` trigger the repository allows (`workflows: [pipeline]`, `types: [completed]`, `branches: [main]`, in
`release.yml` alone), the group of the release, and that beta reads the artifacts of its own run.

### 8.1 Prod: publish

Prod is `release.yml`, extended. **[F]** today: started by the success of a `pipeline` run for a push to `main` (section 8.0);
the `verify` job checks the sums and every attestation of that run's files; the `publish` job, the only one with `contents: write`, attaches the
already-built files and is idempotent, in the `release` environment. The tag check
and the notes (items 1 to 3) are built. There is no hand-started dry run: a failed job is re-run from the run's page.

The additions:

1. **No verdict gates the release (a maintainers' ruling).** Release gating is the successful `pipeline` run (`ci`, `beta`, `artifacts`, `artifacts-sign` and `smoke`; `artifacts-sign` needs `ci` and `artifacts`) that started the release, and `release verify` in the `notes` job: the attestations and the release set, and the tag (annotated,
   on main, naming this commit). This is a release-process check in GitHub; it is **not host-update trust** (hosts never read
   the tag, and verify only the artifact attestation). Nothing judges a verdict document: the annotated tag's message carries
   the **Beta record** `release verdict-write` wrote (commit, set hash, tag, the pipeline run), as an audit record. A tag with no
   record, or one that says it failed, passes `verify` the same. The gamma farm's build check (the retired `gamma.yml`, now kbf-updater) does not gate the release. **No signing key**: who may push a `v*` tag is
   limited by the tag-protection ruleset on main (M19), and the `publish` job is gated by the `prod` environment's
   required reviewer (item 2). A signing key would add key custody without adding protection beyond those two.
2. **A `prod` environment with a required reviewer**, as kci's prod, on the `publish` job. **One approval per release:** the
   kbf.dev deploy job (the only one with `id-token: write`) `needs` `publish` and runs in a second environment, `prod-site`,
   which has no reviewer and admits `v*` tags only; the AWS role trusts `prod-site`, so the approved `publish` job (which
   holds `contents: write`) cannot assume it. Details and the held setup: `infra/site/README.md`.
   **The first release:** it needs no verdict and no waiver; `verify` is not changed or skipped for it.
3. **Release notes**: generated from the commits between the previous release and this one, plus the Beta record's summary
   (the gate values, as information) and a list of anything that is not reversible (section 8.3). Notes are an artifact of
   the `verify` job, so a dry run shows them before anyone approves, and they are the body of the GitHub release
   (`release publish --notes-file`, followed by how to verify the files). They open with "Before you click": what is not
   reversible, the waivers and the record in a line (or that the tag carries none). The not-reversible list is computed by `verify` alone (the record's
   own `not_reversible` lines are not read): `kbf --version --json` of this release and of the previous stable release, each run (`release identity`)
   only from a tarball whose attestation `verify` has already checked, compared with `kbf_gate::reversible` (`meta_log`;
   unknown is not reversible) and for a changed or unstated `protocol`. A first release has nothing to go back to. If a
   previous release exists and its identity cannot be read, the notes are not written (fail closed). Migrations are not
   listed: no machine-readable source exists, and none is taken from the record.
4. **Nothing is pushed to farms.** After `publish` the release is the attested files on the GitHub release. A farm that runs
   the public release gets them as OS packages its hosts pull (`pull-updates.md`; the frontend release poller described
   here earlier was deleted and is superseded by that design), and its operator sets the desired commit (section 6).

Prod publishes the bytes the build attested. If anything is rebuilt, `verify` fails: the set hash is the only thing it accepts.

### 8.2 Rollback

Rollback is the updater walking back, and it is the forward mechanism with the old release as the target.
(`split-mode-updates.md` section 2.11 **[F]**.)

- A rollback sets `update/desired` to the older release's commit, which takes a higher generation. Downgrades are allowed, and
  the generation is what stops a replayed older order. The old directories are still on the hosts (running, previous, one
  more), and the release rows and the disk release index still name the old bytes, which the host re-verifies against the
  attestation as for any release.
- Preflight runs; the frontend goes by a handover back (the old release started as the successor, only through Envoy, never
  restarting the serving one); nodes by the node step, idle or drained, in any order; across a protocol difference the order
  reverses (nodes first).
- **Triggers.** Local and fast: the commit-confirm auto-revert of a process that never becomes active (minutes, on the host,
  no controller needed). During a deploy: any gate of 5.2 halts and walks back what was changed. After a deploy: Planned (section 7);
  nothing rolls back automatically today, and a person sets the older commit.
- **Prod rollback** is the same move on each farm that runs the public release: its desired commit goes to the older release
  under a higher generation. The GitHub release is not rewritten (the published files are permanent, as kci's names are);
  the notes of the next release say what was withdrawn, and the package-update design does not propose a withdrawn commit
  (the release poller this once named was deleted, superseded).
- **When a rollback lands mid-roll [P].** A process mid-step is not interrupted: the updater finishes that step to its
  phase boundary (T13), then walks back from the current state. The first node step and the frontend handover are each a
  place a rollback can arrive; B11 tests both, with N and N-1 mixed in the reverse order.
- **Who sets the rollback.** A person sets the older commit directly (`POST /v1/update/desired`); no evaluator is involved.
  The floor is the host's own commit-confirm auto-revert, which is per process and minutes-scale.
- A rollback is tested in beta (B10, B11) and its result belongs to the same attested record.

### 8.3 Not reversible, and said before the click

From `split-mode-updates.md` **[F]**: a release that changed the schema beyond expand (the contract step of expand then
contract), and a node whose log version was raised (an older binary cannot open it; the walk-back wipes that node's meta
directory and rebuilds from segment footers). A release containing either is flagged at the offer: the offer command prints it, the
notes carry it, an automatic rollback (Planned) would be off for it, and the contract step ships one release after the expand
step, never in it.

## 9. N/N-1 compatibility in CI

Nodes are updated asynchronously, so every pair of adjacent releases must work together in both directions for as long as a
window of `split-mode-updates.md` 3.1 allows. Two checks run on every merge, before beta's long scenarios:

- **Wire.** Compare the wire definitions (the protobuf descriptors, the handover protocol, the `done/` row tags, the
  `ServerMessage` arms) of the commit against the previous release's, and fail on a removed or renumbered field, a field made
  required, a repurposed tag, or a changed handover version without the flag-day marker. This is a lint, not a farm.
- **State store.** Keep a golden state-store file per release (written by that release's binary). The commit's binary opens
  the previous release's golden, and the previous release's binary opens a file the commit wrote (the expand-only case); a
  contract change is refused while a previous-release frontend is configured. T11 **[F, unbuilt]**.

Both serve one rule: every on-disk and wire format changes only by adding (an optional field, a record kind, a command), so
each release starts on the data the release before it wrote, the release before it starts on the data the new one wrote, and a
reader skips what it does not know. A change to a format names the test that covers it. Every pull request runs, with no
previous binary, the codec fixture (`crates/kbf-log/tests/n_minus_1.rs`: an entry of every kind round-trips and a later field
after it is skipped), the metadata log opened as an earlier machine version
(`crates/kbf-server/tests/n_minus_1_log.rs`: the raises past that version are skipped; commands an earlier build lacks are
`raft_log_unknown.rs`), rows that carry a field their reader does not name (`crates/kbf-server/tests/n_minus_1_rows.rs`), and
two golden corpora of entries (`crates/kbf-log/tests/golden/`) and rows (`crates/kbf-server/tests/golden/rows/`) that every
later build must read as the same values. A corpus is append-only: a file is added when a release adds a kind, and a file that
exists is never rewritten or deleted. Beta runs the rule with a real N-1: `n_minus_1_restarts_on_data_n_wrote` and
`n_restarts_on_data_n_minus_1_wrote` (`crates/kbf-integration-test/tests/mixed_version.rs`, the `mixed-version` job of
`beta.yml`) stop each storage node of a farm and start it again as the other version on the same files, and say SKIPPED while
no release exists.

Then B9 runs real processes: N frontend with N-1 nodes, N-1 frontend with N nodes (the downgrade order), a handover N-1 to N
and back, each with a build in flight (T10). The previous release comes from section 3. A release whose N-1 is unavailable
does not skip the check: the beta is red ("not run" is not "passed").

## 10. Failure handling, flake policy and result documents

- **A red beta is not retried to green silently.** One re-run of the same commit is allowed, to classify; both attempts are
  kept. Red then green is recorded as **flaky**, the green attempt alone does not pass the commit for promotion, and an issue
  is opened with both logs.
- **The assertions are not quarantinable.** A zero-failed-actions gate that flakes may be a real race in the update, which is
  what the stage exists to find. A flaky scenario setup (a port in use, a runner out of memory) is fixed in the harness. The
  way out of a stuck gate is a fix or a reverted commit, never a threshold edit or a waiver of a red beta.
- **Timeouts fail.** Every scenario has a wall-clock bound; a hang is red with the process list and logs attached.
- **Evidence is kept**: process logs, scraped metric series, event logs and the result document upload on every run for the
  same 90 days as the builds.
- **Gamma halts, it does not push through.** A halted deploy leaves a state the run can name, resumable from the host's phase
  (T13), never started from the top. A failure of the verdict's producer (Planned, section 7) is never a pass.
- **A broken main**: a red full beta means no later commit is offered until a later commit has a green one.
- **Result documents.** One per stage, one shape (`stage`, `commit`, `set_hash`, `run_id`, `attempt`, `status`, `gates[]` with
  `name`, `measured`, `threshold`, `verdict`, `started`, `finished`), written RUNNING first and FINISHED on every exit, as kci
  does. Beta also posts a job summary (gate table, time per scenario, update timeline) and keeps a per-commit history file in
  the attested artifacts (beta duration, deploy length) so drift shows before a gate trips (M22).
- A scenario that only gamma found gets a beta scenario in the fix; the count of such escapes goes in the release notes.

## 11. Metrics the stages read

Exist **[F]**: `kbf_cache_lost_blobs`, `kbf_cache_blob_losses_total`, `kbf_execute_requests_total`, `kbf_execute_errors_total{cause}` (status, client_abort, dropped; M11),
`kbf_execute_failed_actions_total{class}` (M10), `kbf_execute_in_flight`, `kbf_execute_queued_seconds` (histogram), `kbf_find_missing_seconds` (histogram, no labels beyond `le`), `kbf_frontend_relay_untraced`, the write-tier series.
Open pull requests **[open]**: `under_replicated_blobs`, open writes, repair debt, planned-down (#912, #913, #914).
Exist: `kbf_member_state`, `kbf_members_not_active`, `kbf_member_boot` (storage members, M13).
**[missing]**: a boot series for the roles that are not storage members (M13).

## 12. Missing pieces and the issues to file

Status was read from feature/arch-v2 and the open pull requests when this page was written; re-check before use. Each row is
one issue with one done criterion. "Blocked by" names open pull requests or other rows.

| # | Missing piece | Done when | Blocked by | Issue |
|---|---|---|---|---|
| M1 | Harness library plus B-0: supervisor, real Buck2 load, B0, smoke in the merge queue | B0 green on a hosted runner on every merge; smoke measured, then required | #925; M21 |  #942 |
| M2 | Rolling update of every role under a running build (B1 to B4, B8) | each scenario red on its planted mutant and green; 0 failed actions, 0 lost blobs | M1, #912, #913, #914, #917, #930, #937, #938, #934 |  #943 |
| M3 | Kill a storage node and a daemon during a build (B5, B6); crash matrix (B7) required | scenarios green, matrix a required job | M1, #900, #939 |  #944 |
| M4 | N/N-1: wire lint, state-store goldens, mixed-version run both ways (B9) | lint refuses a planted removed field; B9 green | M1, M16 |  #945 |
| M5 | One frontend update process wired end to end (staged drain, relay, Envoy box, all-in-one) | B2 and B8 green | #937, #930, #925, #934, #938, #901 wiring PR |  #946 |
| M6 | Pull updater end to end: release poller (deleted since; superseded by the host-driven package updates of `pull-updates.md`), desired-state rows, host-side attestation verification, no SSH push | the updater replaces the stand-in in B1 to B4 | #908, #929, M7 |  #947 |
| M7 | `offer`: promotion by setting the desired commit, beta-result check, plan mode (no operator-signed feed) | `offer` refuses on a red or missing beta result and on an incomplete release; the generation only rises | |  #948 |
| M8 | Gamma deploy-as-test checks: in-flight invocation record, dropped-connection and lost-blob checks, halt and walk back | a planted dropped connection fails the deploy | M6, #791, #792 |  #949 |
| M9 | Gamma verdict producer: Planned, on a generic monitoring library over the gamma farm's metrics (the bake evaluator was removed) | an absent metric fails; a producer failure is never a pass | M10, M13 |  #950 |
| M10 | Metric: failed actions by failure class | counter present, labelled by class | | #951 (done: exported by the frontend, read by the beta gate `failed_actions_by_class`; the gamma producer of M9 will read the same series) |
| M11 | Metric: Execute errors by cause | client abort distinguished from dropped | |  #952 |
| M12 | Release workflow: tag check (the tag names the commit, on main; no signing key; no verdict gates), approval environment, notes | a tag naming another commit or off main fails `verify`; a tag with no record, or a failed one, passes | M7, M9, M19 |  #953 |
| M13 | Metrics: process boot counter and membership-state gauge | both exported; gates read them | |  #954 |
| M14 | Metric: `FindMissingBlobs` latency histogram | histogram exported (`kbf_find_missing_seconds`, done); latency gate (not yet added): the `latency-p99` gate will read `Metrics::quantile`, soft band 1.5x to 3x of the beta baseline; `quantile` reads one cumulative scrape (lifetime), so the gate needs the delta of the cumulative buckets between window start and end, not yet implemented, and a quantile in the `+Inf` bucket reads absent (red); no gate is registered yet (pending the M9 producer) | M9 |  #955 |
| M15 | Updater walk-back: automatic desired-commit set on a red hard gate, mid-roll behaviour, not-reversible flags (B10, B11) | B10 and B11 green | M6, M7 |  #956 |
| M16 | Fetch and verify the previous release for N-1 | N-1 resolved, or the fallback of section 3 | |  #957 |
| M17 | Stage file and lint (stages, no `pull_request`, `id-token` only in `attest`) | lint red on each planted violation | |  #958 |
| M18 | Rollout engine (#803) push transport replaced by the pull updater's, or retired | no push transport left | #803 |  #959 |
| M19 | Repo settings: required checks and merge rules on feature/arch-v2 (the maintainers' choice); the tag-protection ruleset for `v*` on main once release cuts start | settings in place | M1 |  #960 |
| M20 | Decision: releases are cut from main only, after feature/arch-v2 merges to main (Q1, decided) | decision recorded | |  #961 (closed) |
| M21 | Measure the whole farm on a standard hosted runner (peak memory, wall time, safe concurrency) | numbers recorded here | |  #962 |
| M22 | Result documents, flake classification and history file | one shape, flaky recorded, history written | M1 |  #963 |
| M23 | crun with cgroup v2 delegation on a hosted runner **[V]** | a real container action runs under crun in CI | #936 |  #964 |

Links, not issues: #912, #913, #914 (replication metrics), #936 (container runtime: crun only), #791, #792, #847 (the
dependencies of every "0 failed actions" gate).

## 13. Open questions, with recommendations

1. **Branch convergence for prod. Decided (#961): main only.** No release is cut from feature/arch-v2. Development
   continues there until the farm works, then the branch merges to main (#753), and every release from then on is cut from
   main. The merge makes a new commit, so bytes attested on the feature branch are never what prod publishes: the first
   release is built on main and runs beta and gamma on that commit.
2. **Required checks. Decided (#960): the maintainers choose the feature branch's required checks and merge rules.**
   The recommendation stands as input: the smoke as a required check on feature/arch-v2 (merges on green Linux checks), the
   full run non-blocking for merge but blocking for the offer. The tag-protection ruleset for `v*` applies to main once
   release cuts start.
3. **Where the bytes live for gamma.** Decided with the pull updater (#927): the frontend's release store and disk release
   index hold the bytes (superseded: the release poller that filled them from the GitHub release was deleted; updates are host-driven OS packages, `pull-updates.md`); there is no feed to host. Whether gamma
   should keep working when GitHub is unreachable is answered by that store (a release already stored is deployable).
4. **Record channel. Decided: the release tag's message, unsigned.** The Beta record (audit only; nothing gates on it) travels in the annotated `v*` tag on main;
   the tag-protection ruleset limits who can push it and the `prod` environment reviewer gates `publish`. No signing key
   (it would add custody with no new protection).
5. **Does the operator's farm need to be the only gamma?** One farm's usage can be too thin to be a canary. Planned: the
   verdict producer must make thin load visible. *Recommend keeping one gamma now*.
6. **Automatic rollback after a bad verdict (Planned).** *Recommend automatic offer for hard gates on a reversible release, a page otherwise*,
   with a person able to cancel before the desired commit is set (a short, **[P]** 10 minute hold). Owner decision.
7. **Hosted runner capacity.** *Recommend measuring B-0 first* (peak memory and wall time of the whole farm on a standard
   runner) and deciding on a larger hosted runner only on that number. Self-hosted only under the conditions of 4.2.
8. **Baseline for an error-rate gate** (Planned): 7 days versus the previous release. *Recommend 7 days*, since the
   previous release is one data point and may itself have been bad.
9. **Where the harness sits when #936 lands.** The stand-in runtime goes. *Recommend B-0 on the stand-in with the weakening
   stated (section 4.2)*, and a crun-with-delegation check on `ubuntu-24.04` as its own item, so beta is not delayed by #936.
10. **Length.** The page is about 590 lines against a 300 to 500 target. Review asked for fixes that add content (the
    missing-metric rows, the rollback cases), so the minor request to trim was only partly taken (sections 10
    and 11 were folded). *Recommend accepting the length*; the table in section 12 is most of it.

## 14. What this page changes in other documents

None yet. When the pieces land: `docs/artifacts.md` (the deployment verify job it says is planned becomes the host's attestation check and the offer's),
`split-mode-updates.md` (section 7's test IDs get the CI job that runs them), and the workflow lints (section 2.1).
