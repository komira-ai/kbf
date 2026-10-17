# CI/CD for kbf: beta, gamma, prod, and the walk back

Status: design. Nothing in this page is built except where it says **[F]** (read in the tree). **[P]** is a proposal.
**[V]** is something to verify by measurement before relying on it. Open pull requests and issues are named by number;
re-check their state before acting on this page, because the table in section 12 rots first.

## 0. The plan in one page

Every merge runs a **beta**: GitHub Actions stands up a whole farm on loopback (a frontend, three storage-and-daemon nodes,
one plain daemon, a pinned Envoy in front), runs real Buck2 builds, then updates every role while a build is running and
kills a node while a build is running. The assertions are numbers, not logs: zero failed actions, zero lost blobs, and
under-replicated blobs back to zero.

A commit that passes beta may be offered to the operator's own farm: **gamma**. The farm's pull updater applies it. The
deploy is itself a test (no build failed because a connection was dropped), and then a 24 hour **bake** runs with the
operator's real usage as the canary. The bake ends in a signed verdict.

A commit with a passing bake can be published: **prod** is the existing attested-artifact release (tag, files, notes), gated
on the verdict. **Rollback** is the updater walking back through the same mechanism it walked forward with.

```
 merge ──> artifacts (build once, attest) ──> beta (Actions, ephemeral farm)
                                                  │ result attested (same set hash)
                                                  v
                          gamma: operator's farm, pull updater, deploy-as-test
                                                  │ 24 h bake, real usage
                                                  v
                              signed verdict ──> prod: tag, attested files, notes
          any gate red ──> rollback: a newer signed set naming the old artifacts
```

Three rules hold in every stage, and each is a rule that exists in the project already: **one frontend update process
everywhere** (a new instance on other ports behind Envoy, the old one drains, in-flight `Execute` streams are kept; the
Buck2 client never re-sends a broken `Execute`), **nodes are updated by the frontend when idle or drained, one storage node
at a time, within N and N-1**, and **the farm pulls**: no stage pushes to a host and no stage holds a way into the
operator's network.

## 1. Goals and non-goals

Goals:

1. A regression that fails a build during an update is found on the merge that introduced it, not on the operator's farm.
2. The operator's farm is never the first place a release runs, and is never updated by a person typing commands to a host.
3. The bytes that run in beta, in gamma and in prod are the same bytes: one build per commit, promoted, never rebuilt.
4. Every gate is a number with a threshold that the doc states, and every gate fails closed: an absent metric is a failure.
5. Rollback is a normal, tested path, and the doc says which releases cannot be rolled back.

Non-goals: a deployment tool for anything but kbf; multi-region or multi-farm rollout; push-style deployment of any kind;
making the farm reachable from a public runner.

## 2. The stage model, reused from kci, and where kbf differs

kci (the release tool in the sibling komira repository) models a release as a list of stages. A stage has a name, an `after`
ordering, a named GitHub environment, and a result document; one job per stage; the same release directory is promoted to
every stage and never rebuilt (`build.set_hash == gamma.set_hash == prod.set_hash`); a validation job holds no identity
token; the workflow is checked against the stage list when it starts; `prod` needs a reviewer and is manual. **[F]**

What carries over, unchanged in meaning:

| kci | kbf |
|---|---|
| stage list with `after:` | `beta`, `gamma`, `prod`, in a small checked-in stage file |
| one environment per stage | `beta` (no secrets, no reviewer), `prod` (reviewer); `gamma` is described below |
| build once, promote by set hash | `artifacts.yml` already builds once per commit and writes `SHA256SUMS`; the set hash is the digest of that file |
| validation never a skip, distinct exit code | each stage ends in a result document with a verdict; "not run" is not "passed" |
| result document written RUNNING first, FINISHED on every exit | the same, one JSON per stage, uploaded as a workflow artifact |
| workflow checked against the stage list | a lint in `crates/kbf-it/tests` beside the existing workflow lints (hosted runners only, SHA-pinned actions, `permissions: {}`) |
| least privilege: the job with the publish token holds nothing else | `attest` is already the only job with `id-token: write`; the new jobs hold `contents: read` |
| dry run by default | `release.yml` dispatch is already a dry run; gamma-offer and prod get the same switch |

Where kbf differs, and why:

1. **There is a beta stage; kci has none.** kci has build, gamma, prod. Beta is new: an ephemeral farm per merge that
   publishes nothing. It is closest to kci's build-plus-validation, not to a channel.
2. **Gamma is not a publish destination.** kci's channels are places bytes are pushed, permanent and never overwritten.
   Gamma is a farm that pulls: "the desired release" is an entry in a signed feed, and the entry moves. A moving pointer is
   the opposite of never-overwrite, so the feed carries a monotonic serial (the updater already keeps a serial floor,
   `crates/kbf-updater` **[F]**), and a rollback is a higher serial naming older artifacts.
3. **Gamma and its bake run outside Actions.** A hosted runner on a public repository must not reach the operator's farm, so
   the farm's metrics cannot be read from a workflow. kci's farm-connected jobs are not adopted. The operator side runs the
   evaluator and returns a signed verdict (section 7).
4. **There is a time gate.** kci has no bake, soak or metric gate, and no rollback verb **[F]**. All three are new here.
5. **Code is not shared.** kci is Mojo on Buck2; kbf is Rust with `xtask`. The vocabulary carries over; the stage file is
   plain data checked by a Rust lint.
6. **kci's DEPLOY step** (a cloud resource graph apply) does not describe restarting kbf processes on a host. The
   deploy here is the pull updater and the frontend-ordered node steps of `split-mode-updates.md`.

### 2.1 The stage file [P]

`release/stages.toml` (name is open), read by the lint and by a small `xtask` that the workflows call:

```toml
[[stage]]
name = "beta"
runs = "actions"          # a job in .github/workflows/beta.yml
environment = "beta"
after = []                # needs artifacts.yml for the same commit
result = "beta-result.json"

[[stage]]
name = "gamma"
runs = "operator"         # not an Actions job: offer, deploy, bake, verdict
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
no job; the lint instead checks that `release.yml` verifies the verdict (section 8).

## 3. The artifact every stage consumes

`artifacts.yml` is the build stage and is not changed in kind. **[F]** It builds `kbf`, `kbf-daemon`, `kbf-server` and
`kbf-deploy` for linux x86_64, linux arm64 and darwin arm64, deterministic tarballs, SBOMs, `SHA256SUMS`; on a push to main
or feature/arch-v2 `attest` adds provenance and SBOM attestations and `verify` checks them. An attestation names its ref,
so a feature/arch-v2 asset does not verify as main's.

What the stages add:

- **Beta runs the attested files**, not a fresh `cargo build`. Beta downloads the commit's `artifacts.yml` output, runs
  `cargo xtask verify-assets` on it, and starts the farm from those binaries. Otherwise beta would test bytes nobody ships.
  The harness (`cargo test`) is the one thing built in the beta job; it is test code, not a shipped file.
- **The beta result is attested.** A last job, the only one in beta with `id-token: write` and `attestations: write`, attests
  `beta-result.json` (commit, set hash, harness commit, every gate with its measured value). No repository code runs in that
  job, as in `attest` today.
- **The feed entry** (section 6) names the set hash and carries a pointer to the artifact attestations and to the beta-result
  attestation. A node never sees a beta result; the operator's signer checks it (section 6.1).
- **A previous release for N-1.** Beta also fetches the last released set (or, before there is one, the newest earlier
  commit with an attested build on the same branch) and verifies it the same way. This is how the N/N-1 jobs of section 9 have
  an N-1.

A branch gap to settle before prod works for this branch: `release.yml` requires the commit to be on main, while this
line of work lands on feature/arch-v2 (section 13, Q1).

## 4. Stage 1: beta

### 4.1 Trigger and cost

Two layers, because "on every merge" and "a merge must not wait an hour" pull apart:

- **Smoke, in the merge queue** (`merge_group`): start the whole farm, run builds, assert zero failures. No update, no kill.
  Target under 10 minutes. This is a required Linux check.
- **Full, after the merge** (`push` to feature/arch-v2 and main): rolling update of every role under a running build,
  kill-a-node, the Envoy swap, the all-in-one variant, N/N-1. A red full run does not revert anything; it **blocks the
  commit from being offered to gamma** (the offer needs its beta-result attestation) and opens an issue automatically.

Neither runs on `pull_request`: they start a farm with root-level helpers and attest results, and a pull request from a
fork must not run either. The lint of section 2.1 enforces the absence.

A newer push cancels an older push's run, as `artifacts.yml` already does; a cancelled beta is "not run", never "passed".
The offer takes the newest commit with a green beta, so skipped commits cost nothing.

### 4.2 Runner shape: does a whole farm fit on a hosted runner

**[V]** The proposal is the standard hosted Linux runner (about 4 vCPU and 16 GB on a public repository; the 6 hour job
limit is far above need). The rehearsal harness already starts this topology as a `cargo test` in `test-x86`. **[F]** A
rough budget, to be measured before it is relied on:

| process | role | expected footprint |
|---|---|---|
| frontend (two instances during a swap) | scheduler, state store, Envoy upstream | a few hundred MB each |
| 3 storage-and-daemon nodes | storage, plus a worker each | the largest; segment size and cache caps set small by the harness |
| 1 plain daemon | worker | small |
| Envoy | pinned version | about 100 MB |
| Buck2 client, plus the actions | the load | the variable part |

The rule that keeps it inside the box: the harness sets segment size, cache and tier limits to test values, and the load is
actions that sleep or touch small files (the `//:pause` shape that the `m1`/`m2` cells use), plus a modest compile.
**Beta is a test of the update and failure paths, not a benchmark.** If a measurement shows the farm does not fit, the order
of remedies is: smaller limits; then fewer concurrent actions; then a larger GitHub-hosted runner (paid, still ephemeral and
managed by GitHub); and last, and only with the safety conditions below, a self-hosted runner.

Why self-hosted is the last resort, on a public repository: a self-hosted runner executes whatever a workflow tells it, and
a workflow can be changed by a pull request. A non-ephemeral runner keeps state between jobs, so one job can leave something
for the next. The conditions if it is ever used: the runner is ephemeral (one job, then the machine is destroyed), it sits in a
network with no route to anything of the operator's, it takes only the `beta` workflow, that workflow never triggers on
`pull_request` or on a fork's code, the job holds no secrets, and the runner group is restricted to the one repository and
workflow. Arm runners are hosted (`ubuntu-24.04-arm`) and available for an arm beta variant; darwin is covered by `native-macos`
and `sign-darwin` and has no farm role in beta.

Cost shape **[V]**: per merge, one smoke (minutes) in the queue and one full run after; standard hosted runners on a public
repository carry no per-minute charge. The cost that matters is wall time and queue depth, which is why full is off the
merge path. Record job duration in the result document so a creeping beta shows in the numbers.

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
- **Envoy**: the pinned version (the one `test-x86` already fetches by checksum) started from the `box` topology render
  (#925), with the frontend as instances `a` and `b`. The Buck2 client goes through Envoy, never straight to a frontend.
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
  The root helper runs under `sudo` on the runner (`sudo-test` already does this in `ci.yml` **[F]**).

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
| B8 | `roll_all_in_one` | the all-in-one process updated by the same handover (`aio-handover.md`) | a second update path for the small topology |
| B9 | `mixed_versions` | N and N-1 in both directions, with and without a build in flight (section 9) | a made-required field; a repurposed field |
| B10 | `rollback_walks_back` | after B4, offer N-1 as a newer signed set and walk back (T16) | rollback restarting the frontend |

The gate expressions common to B1 to B6, B8 and B10, read from the scraped metrics and the builds' event logs:

- **0 failed actions.** Every action of every build in the load ended with its own exit code, none with a farm-class
  failure. The measure: the builds' event logs, and `kbf_execute_errors_total` delta equal to the number of actions the
  scenario failed on purpose (zero here). Today that counter has no cause label, so a dropped connection cannot be
  told from a client abort except through `kbf_frontend_relay_lost_total`. **[F]** A failed-actions-by-class counter is a
  missing piece (section 12, M10); until it exists the gate is: event logs show zero failed actions **and**
  `kbf_execute_errors_total` delta is zero.
- **0 builds lost to a handover**: `kbf_frontend_relay_lost_total` delta is 0.
- **0 lost blobs**: `kbf_cache_lost_blobs` is 0 at every scrape, and the delta of `kbf_cache_blob_losses_total` (a counter
  that never falls) is 0 over the scenario. The gauge alone can fall back to 0 after a loss.
- **Under-replicated blobs back to 0**: `under_replicated_blobs` is 0 within the settle window after each node step
  (**[P]** 300 s in beta; tuned from B1 measurements) and at the end. It may be above 0 only between a node going out and
  the settle window ending, and never for two nodes' worth at once. The gate is **absent means failed**: if the metric is
  not exported the run is red (T12's "absent debt metric" rule). The metric is not on feature/arch-v2 yet (#912, #913).
- **Never two members non-Active at a sampled instant**, from the membership gauge, in B1 and B4.
- **Boot counter +1 per updated process**, none for an untouched one: an update that restarted the wrong thing is red.

B5 and B6 are expected to *pass the build*. They are not "the farm survives anything": a second unplanned loss while one
node is out is the stop rule of T14 and is tested there, not asserted green here.

### 4.5 Order of work: the beta is built up in steps

A beta that needs every update PR to land first would not start for a long time. The harness is delivered as milestones, each
useful alone and each required from the day it lands:

| milestone | adds | blocked on |
|---|---|---|
| B-0 | harness library, the pinned Envoy, the real Buck2 load, B0, smoke in the merge queue | #925 box render; otherwise nothing |
| B-1 | B5, B6, B7 (kill-a-node, kill-a-daemon, crash matrix required) | #900, #939 (write failover) |
| B-2 | B1, B3 (node and daemon roll) | #912, #913, #914, #917 (the metrics and the picker) |
| B-3 | B2, B8 (frontend swap through Envoy, all-in-one) | #930, #937, #938, the wiring PR of #901, #925, #934 |
| B-4 | the pull updater replaces the stand-in for the real thing in B1 to B4; B10 | #908, #929, the poller of `split-mode-updates.md` 6.2 |
| B-5 | B9 against a real N-1, state-store and wire checks | a released N-1 (or the fallback of section 3) |

The assertions "0 failed actions" are only reachable once #791 (the frontend fails every in-flight action on restart) is
closed and the upstream behaviour in #792 is understood; B2 exists to prove that, and if it cannot be made reliable the model
is wrong and that is reported, as `split-mode-updates.md` says of T5, T6 and E1.

## 5. Stage 2: gamma, the operator's own farm

Gamma is the operator running the release on the farm they use. This page says "the operator's farm" and writes no address,
host or account, so the same text is right for anyone who runs kbf and chooses to take the same role.

### 5.1 The offer

A commit becomes offerable when the operator's signer has checked, offline from the farm:

1. the artifacts' provenance and SBOM attestations for the commit and set hash (`verify-assets attestations`);
2. the beta-result attestation for the same commit and set hash, with every gate green and its measured values inside the
   thresholds of section 4.4;
3. that the serial is higher than the last offered one.

The signer then signs a manifest (set hash, per-artifact digests, serial, platform rows) and puts it in the feed. This is the
design of the pull updater: the operator signs once after verifying; hosts do not verify Sigstore, they verify the operator's
signature against a pinned root, offline. **[F in the design, unbuilt]** (`fleet-updates-security.md`; #908 node client,
#929 root helper.) The residual stays as stated there: hosts trust the operator's signature, not the attestation.

Offering is dry-run by default (`--plan`): it prints what would change and which gates it read.

### 5.2 The deploy is a test

The pull updater, the frontend-ordered node steps and the frontend handover (`split-mode-updates.md`) run on the real farm
with real builds in flight. The deploy is a test because it is checked as one:

- **No build failed from a dropped connection**: `kbf_frontend_relay_lost_total` delta is 0 over the deploy, and every
  build that was in flight at any point of the deploy (the evaluator records the invocation ids at deploy start and at every
  step) finished without a farm-class failure (`failure-classes.md`: a farm problem never reaches the client as an action
  exit code). The invocations record carries the failure class **[F: `crates/kbf-invocation`]**.
- **No lost blobs** over the deploy: `kbf_cache_lost_blobs == 0` and the `kbf_cache_blob_losses_total` delta is 0.
- **Under-replicated back to 0** after each node step within the settle window; the run's own gates (preflight, one storage
  node out at a time, repaired before the next) hold, and every one fails closed.
- **Commit-confirm held**: no process hit its local auto-revert (a node that never becomes active reverts itself in
  minutes; that revert firing during a deploy fails the deploy).

A deploy that fails any of these halts, rolls back what it has changed (section 8) and fails gamma. Nothing about a halt is
silent: the result document names the gate, the value and the host step.

### 5.3 What gamma needs that beta cannot give

Real usage (builds nobody scripted), real devices and hosts, systemd, the real cold store, real load shape and duration.
Beta covers the update logic; gamma covers the environment. A defect that only the operator's environment shows becomes a
beta scenario in the same change that fixes it (section 11).

## 6. The feed

Gamma and prod are not channels; the pull updater reads a feed.

- A feed entry per release: serial, set hash, artifact digests, platform rows, a `stage` field (`gamma` or `prod`), and the
  operator's signature. Entries are only ever appended; the latest entry of a stage is what that farm should run.
- **The serial is monotonic and never reused.** A rollback is a new entry with a higher serial naming older artifacts (as
  the updater already specifies: "a rollback is a newer signed set naming old artifacts" **[F]**), so the floor never
  needs lowering and a replayed old entry is refused.
- A farm follows one stage. The operator's farm follows `gamma`; any farm that follows the public release follows `prod`.
- The feed is a static signed file set; nothing about it needs a server that can reach a farm. Hosts poll it (a conditional
  request, free when unchanged). Where it is hosted is not decided here (section 13, Q3).

### 6.1 Who signs, and what the signer checks

The operator holds the signing key. The signer is a command (`kbf release offer`, [P]) that refuses on any failed check of
5.1 and signs nothing. It is run by the operator or by the operator's own scheduled job on the operator's side; it is never
run by a public workflow, because that would put the key in Actions.

## 7. The 24 hour bake

After a clean deploy the farm runs the release for 24 hours on the operator's real usage. The bake is run by an evaluator
that is part of `kbf` and runs beside the farm [P]: it reads the farm's own metrics endpoint and invocation records, keeps
its state in a file (so a restart does not restart the clock), and writes the verdict.

**A bake with no usage is not a pass.** Real usage is the canary, so the bake has a minimum-load condition: at least
**[P]** 1000 executed actions and 10 builds across the window, and at least 3 distinct hours with load. Below that the
bake extends (it does not pass); the operator may waive it with a signed waiver that the verdict records.

Gates, evaluated continuously (a hard gate failing ends the bake red at once) and at the end:

| gate | measure | threshold | hard? |
|---|---|---|---|
| no infra-attributed failures | farm-class failed actions (the failed-actions-by-class counter, M10) | 0 over the bake | yes |
| lost blobs | `kbf_cache_lost_blobs` at every scrape; `kbf_cache_blob_losses_total` delta | gauge 0; delta 0 | yes |
| under-replicated | `under_replicated_blobs` | 0 at the end and 0 after the post-deploy settle window; above 0 for more than **[P]** 10 minutes outside a node step is a failure | yes |
| handover losses | `kbf_frontend_relay_lost_total` delta | 0 | yes |
| error rate | `kbf_execute_errors_total` / `kbf_execute_requests_total`, excluding client aborts once M11 exists | at most the larger of twice the baseline and the baseline plus **[P]** 0.5 percentage points | yes |
| latency | `Execute` queue-to-start p99 and `FindMissingBlobs` p99 against the baseline | not above **[P]** 1.5 times the baseline | no, reported; yes if above 3 times |
| write tier | flush failures, unflushed bytes, corrupt, `kbf_tier_flush_digest_mismatch_total` | no new failures or mismatches | yes |
| restarts | process boot counters | no process restarted that the deploy did not restart | yes |

The **baseline** is the same ratios over the 7 days before the deploy, taken by the evaluator at deploy start and stored with
the verdict. An absent metric fails the gate, never passes it. Thresholds marked [P] are starting values; the bake's first
runs are measured, the thresholds are written down with the evidence, and a change to a threshold is a reviewed change to
this page and to the evaluator's table in the same commit.

The evaluator is deliberately not a dashboard: a person reading a graph is not a gate and cannot be required.

### 7.1 The verdict

`gamma-verdict.json`: commit, set hash, serial, deploy start and end, bake start and end, each gate with measured value and
threshold, the minimum-load numbers, any waiver, and the operator's signature. A red verdict triggers section 8; a green one
makes the commit promotable.

## 8. Stage 3: prod, and rollback

### 8.1 Prod: publish

Prod is `release.yml`, extended. **[F]** today: a pushed `v*` tag on a commit on main whose `artifacts.yml` push run exists;
the `verify` job checks the sums and every attestation; the `publish` job, the only one with `contents: write`, attaches the
already-built files and is idempotent; a dispatch with a commit is a dry-run verify. It has no notes, no environment approval
and no dependency on any earlier stage.

The additions:

1. **A verdict check in `verify`.** The tag is an annotated, signed tag, created by the operator, whose message carries the
   gamma verdict. `verify` checks the tag signature against the operator's public key committed in the repository, and
   that the verdict names this commit and set hash, that every gate is green, that the bake lasted at least 24 hours, and that
   it was not waived without a recorded waiver. No new channel back from the operator's network is needed: the tag is the
   channel, and creating it is the operator's promotion. A tag with no verdict, or a red one, fails `verify`.
2. **A `prod` environment with a required reviewer**, as kci's prod, on the `publish` job.
3. **Release notes**: generated from the commits between the previous release and this one, plus the gamma verdict's summary
   (bake length, load, the gate values) and a list of anything that is not reversible (section 8.3). Notes are an artifact of
   the `verify` job, so a dry run shows them before anyone approves.
4. **The prod feed entry**: after `publish`, the operator signs a `prod` entry with the same set hash. Farms that follow the
   public release pull it.

Prod publishes the bytes gamma ran. If anything is rebuilt, `verify` fails: the set hash is the only thing it accepts.

### 8.2 Rollback

Rollback is the updater walking back, and it is the forward mechanism with the old release as the target.
(`split-mode-updates.md` section 2.11 **[F]**.)

- The feed gets a new entry with a higher serial naming the previous artifacts. The old directories are still on the hosts
  (running, previous, one more).
- Preflight runs; the frontend goes by a handover back (the old release started as the successor); nodes by the node step,
  idle by idle; across a protocol difference the order reverses (nodes first).
- **Triggers.** Local and fast: the commit-confirm auto-revert of a process that never becomes active (minutes, on the host,
  no controller needed). During a deploy: any gate of 5.2 halts and walks back what was changed. During a bake: a hard gate
  red makes the evaluator write a red verdict and **offer the rollback entry automatically** when the release is reversible
  (8.3); when it is not, it pages and offers nothing. A soft gate never rolls back by itself.
- **Prod rollback** is the same entry in the `prod` feed. The GitHub release is not rewritten (the published files are
  permanent, as kci's names are); the notes of the next release say what was withdrawn, and a withdrawn release is marked in
  the feed so no farm selects it.
- A rollback is tested in beta (B10) and its result belongs to the same attested record.

### 8.3 Not reversible, and said before the click

From `split-mode-updates.md` **[F]**: a release that changed the schema beyond expand (the contract step of expand then
contract), and a node whose log version was raised (an older binary cannot open it; the walk-back wipes that node's meta
directory and rebuilds from segment footers). A release containing either is flagged at the offer: the signer prints it, the
notes carry it, the evaluator's automatic rollback is off for it, and the contract step ships one release after the expand
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

Then B9 runs real processes: N frontend with N-1 nodes, N-1 frontend with N nodes (the downgrade order), a handover N-1 to N
and back, each with a build in flight (T10). The previous release comes from section 3. A release whose N-1 is unavailable
does not skip the check: the beta is red ("not run" is not "passed").

## 10. Failure handling and flake policy

- **A red beta is not retried to green silently.** A re-run of the same commit is allowed once, to classify; both attempts are
  kept. Red then green is recorded as **flaky** in the result document, the gate does not pass for promotion on the green
  attempt alone, and an issue is opened with both logs.
- **The assertions are not quarantinable.** A zero-failed-actions gate that flakes may be a real race in the update, and the
  race is the thing the stage exists to find. A flaky *scenario setup* (a port in use, a runner that ran out of memory) is
  fixed in the harness; the failed attempt is not hidden. The way out of a stuck gate is a fix or a reverted commit, never a
  threshold edit.
- **Timeouts fail.** Every scenario has a wall-clock bound; the job bound is above the sum. A hang is red with the supervisor's
  process list and logs attached.
- **Evidence is kept**: process logs, the scraped metric series, the builds' event logs and the result document upload as
  artifacts on every run, green or red, for the same 90 days as the builds.
- **Gamma halts, it does not push through.** A halted deploy leaves the farm on a state the run can name (steps done, steps
  not), resumable from the host's phase (T13) and never started from the top.
- **Operator-side failure of the evaluator** (it crashed, its metrics source is unreachable) extends the bake and pages; it
  never counts as a pass.
- **A broken main**: a red full beta on a commit means no later commit is offered until a later commit has a green one; there
  is no waiver of a red beta, only a fix.

## 11. Observability

- One result document per stage, with a shared shape (`stage`, `commit`, `set_hash`, `run_id`, `attempt`, `status`,
  `gates[]` with `name`, `measured`, `threshold`, `verdict`, `started`, `finished`), written RUNNING first and FINISHED on
  every exit, as kci does **[F]**; `run_id` is the Actions run id for beta and the evaluator's own id for gamma.
- Beta posts a job summary: the gate table, the time per scenario, and the update timeline (which role, when, how long it
  was out).
- A history of results (per commit: beta duration, gamma deploy length, bake gate values) so a drift in the numbers is
  visible before a gate trips. [P]: a file in the attested artifacts, not a service.
- The metrics the stages need (the table of section 12 marks which exist): `kbf_cache_lost_blobs`,
  `kbf_cache_blob_losses_total`, `kbf_execute_requests_total`, `kbf_execute_errors_total`, `kbf_execute_in_flight`,
  `kbf_frontend_relay_lost_total`, the write-tier and `FindMissingBlobs` series **[F]**; `under_replicated_blobs`,
  open-writes and repair debt, planned-down (#912, #913, #914) **[open]**; failed actions by class, and execute errors by
  cause **[missing]**.
- A scenario that was found only by gamma gets a beta scenario in the fix. The counts of such escapes are reported in the
  release notes.

## 12. Missing pieces and the issues to file

Status columns were read from feature/arch-v2 and the open pull requests when this page was written; re-check before use.
Issue titles are the text to file in this repository; an issue for each, public-safe, linking this page.

| # | Missing piece | Today | Issue to file |
|---|---|---|---|
| M1 | Beta workflow: whole farm per merge from the attested files, pinned Envoy in front, real Buck2 load, smoke in the merge queue | the rehearsal is a `cargo test` of one `Execute` and one cache hit; no Envoy, no Buck2, no update | "CI: beta workflow that starts a whole farm and runs real builds on every merge" |
| M2 | Harness: roll every role while a build runs, assert 0 failed actions, 0 lost blobs, `under_replicated` back to 0 (T1, T2, T5, T6, E1 to E3) | designed in `split-mode-updates.md` section 7; none built | "rehearsal harness: rolling update of every role under a running build" |
| M3 | Harness: kill-a-node, kill-a-daemon, crash matrix as a required job | #900 in review, frontend only; write failover #939 open | "rehearsal harness: kill a storage node and a daemon during a build; make the crash matrix required" |
| M4 | N/N-1: wire lint, state-store goldens, mixed-version run both ways, rollback (T10, T11, T16) | none | "CI: N/N-1 compatibility checks (wire lint, state-store goldens, mixed-version run)" |
| M5 | One frontend update process wired end to end (staged drain, relay, Envoy box, all-in-one) | #937 inert; #930, #925, #934, #938 open; the wiring PR of #901 not opened | "frontend update: wire staged drain, relay and the Envoy box into one update process" |
| M6 | Pull updater end to end: poller, signed feed client, node-side verification, no SSH push | #908, #929 open on an integration branch; `kbf-updater` is the older socket helper with no pull | "pull updater: poller, signed feed and node client end to end" |
| M7 | Signed feed and the offer command: stage entries, monotonic serial, beta-result check, plan mode | none | "release feed and `offer`: signed stage entries that require a passing beta" |
| M8 | Gamma deploy-as-test checks and the bake evaluator with baseline, minimum load, state file and signed verdict | none; confirm window is minutes | "bake evaluator: 24 hour bake with baseline, minimum load and a signed verdict" |
| M9 | Prod: verdict check on the tag, `prod` environment reviewer, release notes, prod feed entry | `release.yml` has none; no dependency on earlier stages | "release workflow: require a gamma verdict, add an approval environment and release notes" |
| M10 | Failed actions by class (farm, request, action, ambiguous) as a counter | invocations carry the class; no metric | "metrics: failed actions by failure class" |
| M11 | Execute errors by cause (client abort, dropped, error) | `kbf_execute_errors_total` has no cause | "metrics: label Execute errors by cause" |
| M12 | `under_replicated_blobs`, open writes, repair debt, planned-down exported | #912, #913, #914 open | covered by those pull requests; link, no new issue |
| M13 | Rollback: the updater walks back, automatic offer on a red hard gate, not-reversible flags | one-level manual `host rollback`; rollback only described | "updater: walk-back to the previous release, automatic on a red hard gate" |
| M14 | Previous-release fetch for N-1 | none | "CI: fetch and verify the previous release for N/N-1 tests" |
| M15 | Stage file and lint (workflow against stages; no `pull_request`; `id-token` only in `attest`) | the existing workflow lints do not know stages | "lint: check workflows against a stage file" |
| M16 | Container runtime in CI: remove podman jobs and the stand-in runtime from the harness | #936 open, second part not opened | covered by #936; link |
| M17 | Rollout engine (#803) is a push-style controller | open; its transport trait is the seam | "rollout engine: replace the push transport with the pull updater's, or retire it" |
| M18 | Required Linux checks on feature/arch-v2; branch convergence with main for tags | no protection rule was visible; `release.yml` wants main | "repo settings: required checks on feature/arch-v2; how releases are cut from it" |
| M19 | Existing, link only | #791 (frontend fails in-flight actions on restart), #792 (upstream Buck2 re-attach), #847 (resume a broken owner stream) | the dependencies of every "0 failed actions" gate |

## 13. Open questions, with recommendations

1. **Branch convergence for prod.** `release.yml` takes only commits on main; this line of work is on feature/arch-v2.
   Options: (a) tag only after the branch merges to main; (b) allow a release from the feature branch with its own
   attestation ref. *Recommend (a)*: prod stays main-only, gamma and beta run on both, and the first release waits for the
   merge. This keeps the one "same bytes, attested for main" rule.
2. **Required checks.** Make the smoke a required check on feature/arch-v2 (merge on green Linux checks) and the full run
   non-blocking for merge but blocking for the offer. *Recommend yes*, via a ruleset, as part of M18.
3. **Where the feed lives.** A signed static file set in a release asset of the repository, or a bucket the operator
   controls. *Recommend a release asset on a dedicated tag for the public feed and an operator-hosted copy for gamma*, so
   gamma does not depend on the public repository being reachable. Owner decision.
4. **Verdict channel.** The signed tag carrying the verdict (section 8.1) versus an operator-posted check or comment.
   *Recommend the tag*: no token for the operator's side to hold in Actions, nothing to forge without the key, and it is
   already the trigger of `release.yml`.
5. **Does the operator's farm need to be the only gamma?** One farm's usage can be too thin to be a canary. The minimum-load
   rule makes that visible (the bake extends). *Recommend keeping one gamma now*, and revisiting if bakes routinely extend.
6. **Automatic rollback during a bake.** *Recommend automatic offer for hard gates on a reversible release, a page otherwise*,
   with a person able to cancel before the serial is signed (a short, **[P]** 10 minute hold). Owner decision.
7. **Hosted runner capacity.** *Recommend measuring B-0 first* (peak memory and wall time of the whole farm on a standard
   runner) and deciding on a larger hosted runner only on that number. Self-hosted only under the conditions of 4.2.
8. **Baseline for the error-rate gate** over 7 days versus the previous release's bake. *Recommend 7 days*, since the
   previous bake is one data point and may itself have been bad.
9. **Where the harness sits when #936 lands.** The stand-in container runtime goes; the harness's cgroup planting must be
   replaced by a real crun on the runner. *Recommend doing B-0 on top of #936*, not before, so the harness is not written twice.

## 14. What this page changes in other documents

None yet. When the pieces land: `docs/artifacts.md` (the deployment verify job it says is planned becomes the signer's check),
`split-mode-updates.md` (section 7's test IDs get the CI job that runs them), and the workflow lints (section 2.1).
