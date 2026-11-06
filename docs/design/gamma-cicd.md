# Gamma CI/CD: from a merge on main to the farm

Status: design. It describes the path a commit takes from a merge on `main` to the
operator's own farm (the **gamma** stage) as it is built today, lists every step a person
still does by hand on that path, and orders the pull requests that remove them.

How this page relates to the others:

- [gamma-deploy.md](gamma-deploy.md) describes the retired `kbf-installer deploy`, a push
  from the frontend host. Nothing in it runs any more. This page replaces it as the
  description of the gamma stage.
- [docs/deploy/kbf-updater.md](../deploy/kbf-updater.md), "Turn on the gamma stage", is the
  operator's setup for the `gamma` job. This page does not repeat it.
- [host-updates.md](host-updates.md) is the design of the generic host updater and the
  coordinator. The tasks below build on its migration order (its section 8) and do not
  change it. G1 keeps, for kbf's own gamma, the one part of its interim workflow (its
  section 7) that its later pieces do not replace: going back after a failed check of the
  whole farm, which no host's hook sees.
- [kbf-cicd.md](kbf-cicd.md) is the older plan of all stages. Where it disagrees with this
  page about gamma, this page is current.

## 0. In one page

A push to `main` runs `pipeline.yml`. When every job of that run passes, `release.yml`
starts. It verifies, tags and publishes the release, rehearses a host update on a GitHub
runner, and then its last job, `gamma`, tells the farm's update coordinator "the desired
version is this release". Its token is the run's own GitHub OIDC token, and it sends it as
`POST /v1/desired`. The hosts are not pushed anything. Each host's `kbf-updater` agent
heartbeats to the coordinator, is told the version, and updates itself through the root
helper `kbf-host-install`, which fetches the release's package and checks its attestation.
The job waits until the coordinator reports the farm converged. It then proves the farm
with a remote-only Buck2 build. When any of its steps fails, its last step sets the farm back to
the version the release replaced (G1, done). The coordinator also sets the farm back on its own,
job or no job, when enough hosts report the release failed after its commit (G2, partial: the
coordinator side is done, but kbf's agent does not report that yet, section 2.5).

What is still by hand (section 3):

- the coordinator's own update;
- a host's first install of the agent and helper programs (their units come with the package
  since G6);
- going back after a red gamma when the job cannot (CI is gone, until the agents report where
  kbf's update failed, or the coordinator's rules hold the farm);
- updating a Mac;
- one-time setup (the coordinator, its allow-list, the repository variables, enrolling
  each host).

The one-time setup stays by hand by design. The rest is removed by eight pull requests,
listed in order in section 4.

## 1. Rules this design keeps

- **Hosts drive their own updates.** The coordinator holds one desired version per
  service. Each host's updater asks for it and runs the service's update hook. Nothing
  connects to a host to install a release. The release workflow talks only to the
  coordinator (to set the desired version) and to the farm's remote-execution endpoint
  (for the proof build), never to a host.
  There is no signed release feed, no `offer`, and no stage entry a host reads.
- **Releases are automatic from `main`.** No tag is pushed by hand, no release is started
  by hand, and there is no release dry run. A failed release job is re-run from its run's
  page.
- **Pull requests run the pull-request gate, never a release.** `ci.yml` runs the lint,
  the fast tests and coverage. The long source suite (`beta.yml`) and the artifact smoke
  run on the push to `main`, as stages of the pipeline. A pull request tags nothing,
  publishes nothing and holds no OIDC token. The update rehearsals it may run
  (`update-rehearsal.yml`) update a host on a runner from the previous published release.
  They test the change, not a release.
- **No bake and no verdict evaluator.** The Beta record in the tag message is an audit
  record that nothing gates on. Watching a farm over time is left to a generic monitoring
  library. That library is not built, and no task here builds it.
- **Correct by construction, no runtime gate machinery.** A setting is fixed where it is
  defined: the service definition shipped with the coordinator, or the coordinator's
  allow-list file. It is checked once, when it is loaded. No task adds a gate, a verdict or
  a dry run in front of a roll. The tasks add ways back (G1, G2, C1, H1, H2), each of them
  behind a roll that has already gone wrong.
- **A red gamma check stops the roll.** It sets the farm back to the version it ran before
  (G1). When the farm cannot be moved that way, the operator uses the forced rollback:
  the coordinator's `force` (C1), or on the host directly `activate` (H1) and the pin (H2).
  A forced rollback never runs code from the release it leaves: the helper and the agent
  sit at fixed paths that no release writes (komira-ai/kbf#2233 keeps that guard).

## 2. The release path today, stage by stage

### 2.1 On a pull request

| workflow | jobs | what it gates |
|---|---|---|
| `ci.yml` | `lint`, `test-shard`, `test`, `task-macos` | the merge: formatting, clippy, the repository lints, `cargo deny`, the fast tests in shards, and 99% line and branch coverage per crate. It also runs `cargo xtask release main-control`, a lint of the release workflow's main-only rule, not a release run. |
| `update-rehearsal.yml` | `rehearse`, `rehearse-agent`, `rehearse-tierstore` | a change to the helper, installer, coordinator, updater, OIDC or CA crates, to `deploy-farm` or `release farm-version`, to `packaging/host-agent/linux`, or to the workflow itself (path filter). It updates a host on a runner from the previous release. It holds the run's read-only token, no write scope and no secret, and publishes nothing. |
| `host-install-linux.yml` | `real-linux` | a change to the helper or to `packaging/host-agent/linux` (path filter): the helper's unit and drop-in tests as root under real systemd. It holds `contents: read` only. The release does not need it. |
| `artifacts.yml` | the build and sums jobs | only with the label `needs-artifacts`: a build check that attests nothing. |

`beta.yml` and `smoke.yml` have no trigger of their own (`workflow_call` only), so a pull
request never runs them. `update-rehearsal.yml` and `host-install-linux.yml` also run on a
push to `main` that touches their paths, and both can be started by hand
(`workflow_dispatch`). They test; neither publishes, tags or holds a write scope, and no
release waits for either.

### 2.2 On a push to `main`: `pipeline.yml`

The pipeline's jobs are `ci` (`ci.yml` again, on the merged commit), `beta` (`beta.yml`:
the full test run with slow tests, Arm, Postgres, the object stores, crun, the Mac jobs,
the Linux packages, the cross-version and mixed-version cells, and `kbf-job-client` against
the pinned jobscheduler release), `artifacts` (stage
`build`: the seed build, plain `cargo build --locked --release` on GitHub-hosted runners),
`artifacts-sign` (stage `sign`: the attestations, after `ci` and `artifacts`), `smoke`
(after `artifacts-sign`: the end-to-end smoke on this run's files), `watch` (cancels the
run when any job fails) and `passed` (needs `ci`, `beta`, `artifacts`, `artifacts-sign` and
`smoke`).

The `artifacts-sign` call holds `id-token: write` and `attestations: write`. The
`artifacts` call is granted the same scopes only because GitHub checks a called workflow's
job permissions against its caller's grant, and its signing jobs are skipped. The
attestations name `.github/workflows/artifacts.yml` on `refs/heads/main`, and every host
checks that signer before it installs anything. `pipeline-rerun.yml` re-runs a failed
pipeline run once, only when every failed job is an artifact build or move job.

### 2.3 After a green pipeline: `release.yml`

`release.yml` is started by one event: the completion of a `pipeline` run (`workflow_run`).
`plan` checks that run's conclusion (`success`), event (`push`), branch (`main`) and
repository. Every other job needs `plan`, and all of them but `verify` repeat the check. It has its own concurrency group, `release-main`, which never cancels. When
`plan` finds the range since the last tag is docs only, or changes the pipeline itself,
the jobs after `plan` are skipped and the run is green. The code ships with the next
release.

The workflow has no secret but the release bot's GitHub App key (`RELEASE_BOT_PRIVATE_KEY`,
with the variable `RELEASE_BOT_APP_ID`). The `release` environment is used by `tag`,
`publish`, `bump` and `gamma`.

The job table below lists the jobs in the order they run. The repository lint `gamma_cicd_doc` (in
`crates/kbf-integration-test/tests/repo_lints.rs`) fails when this table names a job that
`release.yml` lacks, or when `release.yml` has a job this table omits.

<!-- release-jobs: begin -->
| job | needs | token and rights | what it does | what it gates |
|---|---|---|---|---|
| `plan` | none | `GITHUB_TOKEN`: `contents: read`, `actions: read` | `cargo xtask release plan`: the next version (`VERSION` holds `MAJOR.MINOR`, the patch is one above the highest tag), and whether the range is code, site or pipeline. The variable `RELEASE_FREEZE` refuses every release while it is set. | every later job: they run only when the range is code and does not change the pipeline |
| `verify` | `plan` | `contents: read`, `actions: read`, `attestations: read` | refuses a commit that is not on `main`. Downloads the pipeline run's assets, sums and bundles, checks `SHA256SUMS`, and verifies every attestation online and every bundle offline, as a host does. Rebuilds nothing. | `tag`, `notes`, `publish` |
| `tag` | `plan`, `verify` | `contents: read`, plus a release-bot App token (`contents: write`, this repository), in environment `release` | writes the Beta record and creates the annotated tag, create-only. Only the release bot may create a `v*` tag. | `notes`, `publish`, `bump` |
| `notes` | `plan`, `verify`, `tag` | `contents: read`, `actions: read`, `attestations: read` | `release verify` (the tag names this commit, and the set hash), the identity of this release and the one before it (after their attestations are verified), and the release notes | `publish` |
| `publish` | `plan`, `verify`, `tag`, `notes` | `GITHUB_TOKEN` `contents: write`, the only job that has it, in environment `release` | creates the GitHub release with those files and notes. A re-run adds missing files and replaces none. | `bump`, `rehearse`, `rehearse-agent`, and through them `gamma` |
| `bump` | `plan`, `tag`, `publish` | `contents: read`, plus a release-bot App token scoped to the downstream repository that pins kbf as a submodule, in environment `release` | opens a pull request there that moves the pin to the released commit | nothing: no job needs it, and the farm path does not depend on it |
| `rehearse` | `plan`, `publish` | `contents: read`, the run token passed as a file | a host laid out from the previous release is updated to this one by the real helper under real systemd (`update_rehearsal`) | `gamma` |
| `rehearse-agent` | `plan`, `publish` | `contents: read`, the run token passed as a file | the same host is updated through the whole control loop: this commit's coordinator, `cargo xtask deploy-farm`, and the previous release's agent and frontend (`update_rehearsal_agent`) | `gamma` |
| `gamma` | `plan`, `publish`, `rehearse`, `rehearse-agent` | `contents: read`, `id-token: write` (the only job with OIDC), in environment `release`, no secret | `release farm-version` (the version the hosts report, `<semver>+g<12 hex>`). Then it joins the farm's Tailscale network when `TS_CLIENT_ID` is set, through workload identity federation. Then `cargo xtask deploy-farm --deadline-min 60`: it records a baseline, sends `POST /v1/desired`, and polls `GET /v1/convergence` until the farm converges. Then `cargo xtask prove-farm`: a remote-only Buck2 build, every action new. Last, only when an earlier step failed, `cargo xtask deploy-farm restore`: the version this release replaced is set back as desired (G1). | nothing after it. The release stays published whatever it says. Its result is the run's colour. |
<!-- release-jobs: end -->

### 2.4 The desired version, from the CI token to the hosts

**The token.** `deploy-farm` asks the runner's token service for a token whose audience
is `KBF_COORDINATOR_URL` exactly as written. It gets a fresh token for every call, since
the coordinator accepts a token once (it remembers each `jti` until expiry). The
coordinator's allow-list (`--oidc-config`, a `[remote_operator]` entry) admits a token only
when all of these match:

- the issuer and the audience;
- `repository_id` and `repository`;
- `job_workflow_ref` (`release.yml` on `refs/heads/main`);
- `ref`;
- `environment` (`release`);
- `event_name` (`workflow_run`);
- `runner_environment` (`github-hosted`).

It also checks the token's lifetime, at most 600 seconds by default. A token that fails any
of these is refused with 401 `unauthorized`. An entry's `services` list (default `kbf`) is
the set of services a token it admits may set. Setting any other service is 403
`forbidden`. Neither changes anything.

**What CI may do.** It may do two things: `POST /v1/desired` for the services its entry
lists, and `GET /v1/convergence`. It may not use the operator actions (`POST /v1/operator`:
`exclude`, `include`, `set_labels`), and it may not read `GET /v1/status`. The operator
token is refused as a CI token, and the other way round. A CI set of a version marked bad
clears the mark.

**What a desired write does.** The coordinator replaces the service's desired state (a
version, `min_up` per role, and an optional `max_updating`). Nothing is sent to a host. On
each heartbeat (every 15 s) the coordinator decides for that host, in a fixed order of
rules: the quiet start after a restart, a bad mark, the host updating or busy, the host up
to date, the host failed at this version, a deferral, the canary, no update live anywhere (no host of any role holds an update slot for any
service; a `max_updating` of 0 holds the role, a larger number has no effect), the bake, the role order (a frontend waits for every other role), and `min_up` (a frontend is
never the last one up). The first rule that holds back the order wins. Otherwise the reply
is `Update { version }`.

The agent then has the helper fetch, verify, drain, switch and check health. It confirms,
or the helper reverts on its own. kbf's shipped definition (`kbf.services.toml`) sets no
canary and no bake, and sets automatic rollback at the default threshold (G2): the larger of
25% of the live hosts and two.

**What makes the job red.** `deploy-farm` exits 1 when:

- the coordinator says `blocked` (a live host failed its update to this version);
- a role has fewer hosts at the version than were live at the baseline;
- the desired version was replaced by another writer (`Superseded`);
- the counts did not change for `--stall-min` (default 20 minutes);
- the deadline passed (`--deadline-min`, default 60);
- a call was refused with 401 or 403;
- no OIDC token is available;
- the coordinator runs another commit and `KBF_REQUIRE_COORDINATOR_CURRENT` is `true`;
- any other error: a 503 still there after four attempts, a bad URL, or an answer it
  cannot read.

`prove-farm` is red unless Buck2 ran more than zero remote actions and zero local ones.

**When the job does nothing.** When `KBF_COORDINATOR_URL` is unset, `deploy-farm` prints a
notice and passes. When `FARM_ADDRESS` is unset, `prove-farm` does the same. A fork, or a
repository with no farm, therefore gets a green `gamma` that touched nothing. The notice on
the run summary is the only sign of it. This is intended: the farm's address is
configuration, not code, and it is not committed.

### 2.5 What a red gamma does to the farm today

The job's last step, run only when an earlier one failed, sets the farm back (G1): while this
release is still the desired version, it sets the version and minimums the release replaced,
which the coordinator keeps as `replaced`, conditional on the generation it read, and the run
stays red. That covers a red deploy (`blocked`, a stall, the deadline, a lost host) and a red
proof (`prove-farm`), which the coordinator does not see at all. Until the restore runs, nothing
stops the roll: a host whose update failed its health check is put back by its helper, and it
keeps its slot only until it heartbeats healthy again (rule 7 of
`crates/kbf-coordinator/src/decide.rs`). Then the next host of that role is ordered to the same
version. kbf's shipped definition turns on the coordinator's automatic rollback
(`rollback = "automatic"`, komira-ai/kbf#2181; G2): when the larger of 25% of the live hosts and
two (two of six) report the release failed after its commit, the coordinator marks it bad and
sets the last good version back as desired, with or without the job. It counts only a failure
that a host reports with its phase (`after_commit` or `gate_release`, not blamed on the
environment). kbf's own A/B update on a host (`crates/kbf-updater/src/agent/update.rs`) reports a
failed update with no phase, so on a farm of today's agents the coordinator still holds the
release (`blocked`), and the job's restore is the way back. A run that is cancelled, or
whose runner is lost, restores nothing; the operator re-runs the failed jobs, or uses C1. The
next code push releases again, and its `gamma` sets a newer version. Only `RELEASE_FREEZE` stops
that.

## 3. Steps still done by hand

| # | step | how often | today | removed by |
|---|---|---|---|---|
| M1 | Deploy the coordinator: its certificate from the farm CA with the role `coordinator`, the allow-list file, the operator token file, and `--services-file` for services other than kbf | once per farm | by hand ([kbf-updater.md](../deploy/kbf-updater.md), "Turn on the gamma stage", step 1) | stays by hand: it is the root of the farm's trust and is set where it is defined |
| M2 | Update the coordinator: a new image, the tag changed in its unit, a restart. It is needed before any release that changes `crates/kbf-coordinator` or the wire (`crates/kbf-updater/src/wire.rs`). | every such release | by hand ([kbf-updater.md](../deploy/kbf-updater.md), "Updating the coordinator"). `KBF_REQUIRE_COORDINATOR_CURRENT=true` only fails the deploy. | G7 |
| M3 | Install the agent and helper units on a new host, and add `--layout deb` to the helper's line | once per host | done: the `kbf` deb ships and enables both units, the helper's with `--layout deb` (G6). Still by hand: placing the two programs, which no package carries so that no release writes them, and the farm's names, one drop-in that is the same on every host ([packaging/host-agent/README.md](../../packaging/host-agent/README.md), "A host that runs the package") | G6 (done) |
| M4 | Issue a host's enrollment token (`POST /v1/enroll/tokens`) and run `kbf-updater enroll` | once per host | by hand | stays by hand: issuing an identity is the operator's act |
| M5 | Set the repository and organisation variables (`KBF_COORDINATOR_URL`, `KBF_COORDINATOR_CA`, `KBF_MIN_UP_*`, `TS_CLIENT_ID`, `TS_AUDIENCE`, `FARM_*`), the Tailscale trust credential and its policy for `tag:ci`, and the `release` environment | once per farm | by hand | stays by hand: it is configuration of the farm, not of kbf |
| M6 | After a red gamma, set the farm back: choose the version, and set it as desired. Only a CI token can, so that means re-running an earlier release's `gamma` job. | every red gamma | the ordinary case is done by the job (G1, done); when CI is gone, by the coordinator (G2, coordinator side done) once the agents report where kbf's update failed, by hand until then; by hand when the ordinary rules hold the farm | G1 (the ordinary case, done), G2 (when CI is gone; partial: it needs the agent's phase, section 2.5), C1 (when the ordinary rules hold the farm) |
| M7 | Go back on a host when the coordinator is down or cannot order it | rare | by hand. `kbf-deploy host rollback` serves hand-laid hosts only. On a package host, the helper's `revert` goes back only from a release on trial, and `switch` needs a release staged by `install`, on a trial clock. Nothing switches to any other kept release. | H1, H2 |
| M8 | Update a Mac: it takes no update order yet, because the installer has no lease source for a launchd job ([kbf-installer-macos.md](../deploy/kbf-installer-macos.md), "What is still by hand") | every release | by hand | G8 |
| M9 | Update a farm's only frontend, or its only storage host. A frontend is never the last one up, and a lone storage host with the default storage minimum (`KBF_MIN_UP_STORAGE` unset, so 1) would leave none up, so the coordinator orders neither, and `gamma` fails at its stall or deadline. The operator sends `install`, `switch` and `confirm` to the helper's socket in a window. | every release, on such a farm | by hand ([kbf-updater.md](../deploy/kbf-updater.md), "A frontend host") | stays by hand: it is the operator's choice between an outage and more hosts. Either run two frontends behind a load balancer ([frontends-behind-a-load-balancer.md](../deploy/frontends-behind-a-load-balancer.md)) and two storage hosts, or set `KBF_MIN_UP_STORAGE` to 0 and accept that the storage host is down while it updates. |
| M10 | Clear a halt that a host's hook reported (komira-ai/kbf#2187, open) | when a hook halts | will be by hand | stays by hand: a halt asks for a person by design |
| M11 | Re-run a release job that failed for a reason outside the commit | rare | by hand, failed jobs only | stays by hand: the next code push releases the same code anyway |

Not on the farm path, and so not counted: merging the `bump` pull request downstream, and
raising `MAJOR.MINOR` in `VERSION`.

## 4. Tasks, in order

Each task is one pull request. Each test is written first and fails before the change.
The tests use fakes driven by handshakes (a request count, an answer, an exit status),
never by elapsed time.

| order | task | removes | needs | issue |
|---|---|---|---|---|
| 1 | G1 a red gamma sets the farm back (done) | M6 (ordinary case) | none | komira-ai/kbf#2239 |
| 2 | G2 kbf ships automatic rollback (partial: the coordinator side is done, the agent's phase is not) | M6 (CI gone) | none (komira-ai/kbf#2181 merged) | komira-ai/kbf#2240 |
| 3 | H1 local `activate` | M7 | none (komira-ai/kbf#2233 merged) | komira-ai/kbf#2242 |
| 4 | C1 coordinator `force` | M6 (farm held by its rules) | komira-ai/kbf#2187, H1 | komira-ai/kbf#2241 |
| 5 | H2 the agent's pin | M7 | H1, C1 | komira-ai/kbf#2243 |
| 6 | G6 agent and helper units in the package (done) | M3 | komira-ai/kbf#2236 | komira-ai/kbf#2244 |
| 7 | G7 the coordinator updated by the updater | M2 | G6, [host-updates.md](host-updates.md) step 6a | komira-ai/kbf#2245 |
| 8 | G8 a Mac takes update orders | M8 | the Mac lease users ([macos-end-state.md](macos-end-state.md), steps 3 and 4) | komira-ai/kbf#2246 |

G1 and G2 come first because they close the gap of section 2.5. H1, C1 and H2 are the
forced rollback, in that order: a forced order on a host ends in H1's `activate`, and the
pin's force case needs C1. G1 relies on them for the cases it cannot handle. G6 and G7 take the
remaining hand steps out of the release-to-release path. G8 is last because its dependency
is the largest.

### G1. A red gamma sets the farm back

**Status: done.** The coordinator's record is `crates/kbf-coordinator/src/writes.rs`, the
restore is `crates/xtask/src/deploy_farm/restore.rs`, and the workflow rules are
`crates/kbf-integration-test/tests/release_workflow/restore.rs`. The record of the writes is kept
in a file beside the state file (its name with `.writes` added), so the state file keeps the form
earlier builds read; it names the state file by its inode and modification time, so any rewrite of the state file it did
not see (a stop between the two writes, an earlier build) drops it. The coordinator's own rollback
is a write too, and raises the generation. "A green run makes no restore" is held by the
workflow: the step runs under `if: ${{ failure() }}` only, which the release workflow lint pins.
Where this section says the restore names the forced rollback, it names the hand step of M6 (a
re-run of the `gamma` job of the release to go back to) until C1 is built. For "Time",
every step of the job has its own `timeout-minutes` and the job's is at least their sum, so a
hanging proof is cut and the restore still runs.

**Change.** The PR has two parts. The baseline the job goes back to is kept by the
coordinator, not by the job, so it is there whatever happened to the job.

1. **The coordinator records what a write replaced.** These additions are additive, and
   they are on the control API only.
   - A service's desired state gains `generation` and `replaced`.
   - `generation` is raised by every accepted write, including one that sets the version
     already desired.
   - `replaced` holds the version and the minimums that the last change of version
     replaced. Setting the same version again keeps it, so `replaced` still names the
     version the farm ran before this release.
   - `POST /v1/desired` answers with both fields, and `GET /v1/convergence` shows them.
   - `POST /v1/desired` takes an optional `if_generation`. When it does not name the current
     generation, the write is refused with 409 `conflict` and changes nothing.
   - `POST /v1/desired` takes an optional `write_id`, a text of the caller's. The desired
     state shows the `write_id` of the write that made it (empty for a write without one).
     This is the receipt a caller checks to know that a state is its own write.
2. **`cargo xtask deploy-farm restore --version <this release>`** is the last step of the
   `gamma` job, run when an earlier step of the job failed. It reads `GET /v1/convergence`.
   When the desired version is this release, it does three things:
   - It sets `replaced` (version and minimums) as desired, with `if_generation` set to the
     generation it read and `write_id` set to `restore-<run id>-<attempt>`. It uses the same CI identity with a fresh OIDC token: the
     coordinator accepts each token once, so no call reuses one. A 503 is retried as on
     every other call.
   - It prints what it set.
   - It exits 1, so the run stays red.

It sets nothing, and says why, in each of these cases:

- the desired version is another one (another writer, a newer release, or the
  coordinator's own rollback has it now);
- `replaced` is empty (a first deploy);
- the coordinator shows no `generation` (it is older than this change): the restore names
  the forced rollback instead;
- `KBF_COORDINATOR_URL` is unset.

A 409 means some write was accepted between the read and the restore. That includes one
that set this release again, such as a re-run's forward write, because every accepted
write raises `generation`.

On a 409 the restore reads once more, to tell its own retry from another writer:

- **Its own retry.** A 503 answered a restore that had in fact landed. The desired state
  now shows the restore's own `write_id`, so it reports the restore as done. The version
  alone proves nothing: another writer may have chosen the same version, or the same
  version with other minimums.
- **Another writer.** The state shows another `write_id`, or none. That write stands,
  even when its version and minimums equal `replaced`. Nothing is restored, and the restore
  exits 1 naming the state now and its `write_id`.

The baseline is the coordinator's record, so two cases restore the right version:

- the forward write took effect but its answer was lost;
- a failed job is re-run while the desired version is already this release.

In both, the restore sets the version the farm ran before this release, never this release.

**Time.** The workflow passes `--deadline-min` to `deploy-farm` explicitly. The job's
`timeout-minutes` must leave room after it for the proof and the restore, and
`release_workflow` checks that. A run that is cancelled, or that a runner loses before the
last step, restores nothing. `GET /v1/convergence` still shows `replaced`, and the operator
then does one of two things:

- re-runs the failed jobs. The forward write sets the same version and changes nothing,
  and the restore step runs.
- or uses C1 `force` to `replaced`.

It is an ordinary desired set, so it goes through the ordinary rules. G1 stays after G2:
the coordinator's automatic rollback sees only what hosts report, and `prove-farm` checks
the whole farm, which no host reports.

`release_workflow` pins the step, its condition, its place as the job's last step, and the
job's timeout.

**Tests:**

Coordinator:

- a write records `replaced` and raises `generation`;
- setting the same version again raises `generation` and keeps `replaced`;
- a stale `if_generation` is refused with 409 and changes nothing, including after writes
  that went from A to B and back to A in between;
- a request without `if_generation` behaves as today.

`deploy-farm`, against a fake coordinator driven by handshakes:

- a failed proof after a green convergence makes exactly one `POST /v1/desired` of
  `replaced`, with its minimums and `if_generation`, after the release's own;
- a `blocked`, a stall, a deadline and a lost host each do the same;
- the forward write is stored but its answer is dropped, and the restore still sets
  `replaced`;
- a re-run, with the desired version already this release, restores `replaced` and not
  this release;
- a competing write, applied by the fake when the restore's read arrives, gets the
  restore a 409. The competing write survives, and the run exits 1 naming it. This holds
  when the competing write sets this same release again;
- a restore that lands but whose answer is a 503 is retried, gets a 409, reads its own
  `write_id`, and reports the restore done;
- another writer sets the `replaced` version between the restore's read and its write,
  once with other minimums and once with the same ones. The restore gets a 409, reads
  another `write_id`, and reports another writer's result in both cases, never a restore
  done;
- the restore presents a token no earlier call used. The fake refuses a replayed `jti`
  with 401, and the restore still lands;
- `Superseded` and an empty `replaced` make no post;
- a coordinator without `generation` makes no post, and the run names the forced
  rollback;
- a 503 on every attempt of the restore, and a 403, each exit 1, naming `replaced` and
  the forced rollback;
- a green run makes no restore.

Workflow lint: it fails on a copy of `release.yml` that lacks the step, gives it another
condition, or has a job timeout that leaves no room for the proof and the restore after
the deadline.

### G2. kbf ships automatic rollback

**Status: partial.** The coordinator side is done:
`crates/kbf-coordinator/src/kbf.services.toml` sets `rollback = "automatic"` and
`rollback_after_failed = { percent = 25, min_hosts = 2 }`. The release path is not: kbf's agent
does not yet report the phase this rollback counts (the first limit below), so on a farm of today's
agents M6 (CI gone) is still a hand step. The tests of the coordinator are
`crates/kbf-coordinator/src/kbf_rollback_tests.rs`; the deploy, the restore and a fleet whose
updates take beats, against the coordinator as shipped, are
`crates/kbf-coordinator/tests/gamma_rollback.rs`; and what the agent's own update reports on a
failure is pinned in `crates/kbf-updater/src/agent/update_report_tests.rs`, whose exact bytes
`kbf_rollback_tests` feeds to the coordinator. Two limits, both stated where they are found:

- The coordinator counts only failures that a host reports with a phase after the commit. kbf's own
  A/B update on a host (`crates/kbf-updater/src/agent/update.rs`) reports a failed update with no
  phase, so on a farm of today's agents the coordinator holds a release that kbf hosts fail
  (`blocked`); it does not roll it back. The rollback acts on kbf once the agent reports the phase
  of its own update, or once kbf is updated through its hook ([host-updates.md](host-updates.md),
  section 8, step 5). Until then the way back when CI is gone stays the hand step of M6.
- The deploy reads every 30 s and hosts beat every 15 s, and one update is live at a time. The
  first failed host makes the farm `blocked` before the second fails, so a deploy whose read falls
  between the two ends `blocked`, and G1's restore then sets `replaced` back. The failed version is
  then no longer desired, so the coordinator marks nothing. The deploy ends `Superseded` only when
  both failures land between two of its reads. `gamma_rollback.rs` has both orders: at the
  shipped interval the deploy ends `blocked` and the restore sets `replaced` back, and with reads
  45 s apart it ends `Superseded` and the restore posts nothing.

A coordinator of a kbf farm now writes its ops file (`--ops-file`) once the farm first converges,
to keep the last good version, and `GET /v1/status` shows it as `ops`.

**Change.** `crates/kbf-coordinator/src/kbf.services.toml` sets `rollback = "automatic"`
for `kbf`, with the default threshold (25% of live hosts, at least 2). The coordinator
then marks a version bad and goes back to `last_good` on its own, from the failures hosts
report after their commit point. That works even when the job's runner is gone. A services
file that replaces the `kbf` entry still decides for its farm.

**Tests:**

- the shipped definition loads with `rollback = automatic`;
- on a fake fleet of six hosts, two post-commit failures of version N set desired to
  `last_good` and mark N bad;
- one failure does not;
- a first version, which has no `last_good`, is never rolled back;
- `before_commit` and `environment` failures never count;
- `deploy-farm` waiting on N then ends `Superseded`, and G1's restore makes no second post.

### H1. Local `activate`

**Change.** `kbf-host-install activate kbf=<commit|version> [--pin]`, run by root with the
fixed-path helper, also on the `--serve` socket. It swaps to a release the host keeps
(komira-ai/kbf#2233), or one that `install` has staged, with no fetch and no trial clock.

It refuses a release the host does not keep, and lists the ones it keeps. It takes the
install lock and refuses a live holder. `--force-lock` takes a dead holder's lock and
audits it. It runs nothing from the release being left. It works on Linux and on a Mac.

With `--pin`, the order is fixed so that a crash leaves the host held:

1. it writes the hold file (H2) and syncs it, under the install lock;
2. it switches;
3. it records the switch.

A crash after step 1 leaves a host that is held but not yet on the pinned release.
Running `activate` again finishes the swap, and the heartbeat shows the pin and the
release that runs, so the gap is visible.

**Tests:**

- `activate` swaps to a kept release with the network cut;
- it refuses a release not kept and lists the kept ones;
- the swap runs nothing from the release being left;
- it refuses a live lock holder;
- `--force-lock` takes a dead holder's lock and audits it;
- for `--pin`, a crash injected at each step boundary, followed by a restart, leaves the
  host held, and the next `activate` completes the swap;
- the same on a Mac (type-checked in CI, run in the Mac job of `beta.yml`).

### C1. Coordinator `force`

**Change.** A new operator action `force {service, version, reason, max_updating?}` on
`POST /v1/operator`, for the operator token only. A CI token gets 403.

It bypasses:

- the canary;
- the bake;
- `min_up`;
- a deferral;
- a bad mark on the target;
- a standing halt;
- each host's record that it failed the target before. A force lifts it, as a new desired
  version does, while ordinary writes keep today's rule 4.

It keeps:

- the role order;
- `max_updating`, which the request may raise (a raise has no effect while the coordinator allows one live update at a time across every service);
- `min_version`. A force below the service's `min_version` (the first release that has the
  host-updates hook) is refused with that reason, and names the manual path.

Its effects, each one audited:

- the desired state is set with `forced: true` and a new force generation;
- a halt is cleared through the same path as the operator's clear (komira-ai/kbf#2187);
- the target is unmarked;
- the version left is marked bad, so the coordinator never orders it on its own again:
  automatic rollback never chooses it. An explicit CI desired write of it still clears
  the mark, as today;
- `last_good` moves only after the forced version has converged.

**On the host.** A host's forced slot is freed when it reports running the forced version,
whether or not it is healthy. The agent that takes a forced order never puts the target on
trial, so no helper trial deadline exists that could revert it:

1. When the host does not keep the release, `install` stages it. That fetches and lays it
   out, and switches nothing.
2. H1 `activate` makes the staged or kept release the one that runs. This is one durable
   step with no trial, and no `switch` or `confirm` is sent.

If the agent stops between the two steps, the host still runs the release it ran. The
order ages out, and the coordinator sends it again. The agent still reports health.

**On the wire.** The agent parses the coordinator's reply with unknown fields refused
(`RawReply`, `RawServiceReply`), so `forced` is sent only to an agent that announced
`forced-v1` in `agent_features`, as `halt-v1` is today. An older agent gets today's reply
shape. It goes back the ordinary way, with its own revert-on-unhealthy, and its slot is
freed the ordinary way.

The next ordinary desired write ends the forced state.

**Tests:**

- a force reaches every live host despite a halt, a bad mark, `min_up` and a deferral;
- a host that failed the target before receives the forced order, while an ordinary write
  of the same target still withholds it;
- the halt is cleared, and the three effects are in the audit;
- the version left is never chosen by automatic rollback, and an explicit CI write of it
  clears the mark and orders it;
- `last_good` moves only after convergence;
- `max_updating`, and a raised value, are honoured;
- a CI token gets 403;
- a force exactly at `min_version` is accepted, and one release below it is refused with
  its reason;
- hosts that report unhealthy after the swap still all receive the force;
- with the agent stopped after `install` and before `activate`, the host runs its old
  release, and the force is sent again once the order ages out;
- with the agent stopped right after `activate`, the helper's injected clock moved far
  ahead and the helper restarted, the host still runs the target, and the helper holds
  no trial;
- an unhealthy forced target stays. The helper's injected clock is moved past the trial
  deadline it would have had, the helper is restarted, and the host still runs the
  target;
- the reply sent to an agent that did not announce `forced-v1` parses with the previous
  release's reply parser (a golden of its shape). That agent goes back when the target is
  healthy, and reverts by its own rule when it is not.

### H2. The agent's pin

**Change.** `activate --pin` writes a root-owned hold file. While the file exists:

- the agent takes no order, and reports `pinned <commit>` in every heartbeat (an additive
  field);
- the helper refuses `install`, `switch` and `activate` from the agent (`pinned`), so an
  update the agent accepted before the pin cannot complete after it. That includes a
  forced update whose release was staged before the pin and is activated after it. The
  helper checks the hold under the install lock, which `activate` also takes, so no
  activation slips between the check and the swap. The one `activate` from the agent the
  helper runs while a hold exists is the recovery that a force naming exactly the pinned
  commit asks for (below).

The coordinator shows the host as pinned, and counts it neither failed nor converged.
`kbf-host-install unpin` clears the pin.

A C1 force can also clear it. The coordinator sends a pinned host an order only when a
force names exactly the pinned commit: `Update { version, forced, unpin }`. It sends it
whether or not the host already runs that commit, because a crash in H1 can leave a pin
persisted before its release was activated. This is the one order a pinned host takes:

1. When the host does not run the pinned commit, the agent runs H1 `activate` on it
   first, with no trial.
2. It then removes the hold through the helper.

When the host already runs the commit, it switches nothing. An ordinary order, or a force
of another commit, leaves the pin.

**Tests:**

- a pinned host is not driven by the next heartbeat's order;
- the coordinator shows it pinned, and counts it neither failed nor converged;
- an update accepted before the pin cannot finish: the fake agent sends `switch` after the
  hold file appears, and the helper refuses it;
- a forced update staged before the pin cannot finish. Its release is staged, the operator
  pins another release, and then the agent's `activate` is refused with `pinned`. The host
  keeps running the pinned release;
- a force naming the pinned commit clears the pin and switches nothing, on a host that
  runs it;
- after a crash that left the pin persisted and the release not yet activated, and a
  restart, a force naming the pinned commit activates it, clears the pin, and the host
  reports the commit and converges;
- an ordinary order, and a force of another commit, leave the pin;
- `unpin` restores following the desired version;
- an older coordinator that does not know the field ignores it, and the host still holds.

### G6. Agent and helper units in the package

**Status: done.** The as-built notes follow the tests below.

**Change.** The `kbf` deb ships `kbf-host-local.service`, `kbf-updater-agent.service` and
`kbf-updater.conf` from `packaging/host-agent`, with the helper's `--layout deb`. Installing
the package enables both units. An agent that has no enrollment state yet waits for one.
It does not fail in a loop, and it says what it waits for.

With this, a host's first install is the package plus M4.

**Tests:**

- `pkg-install-test` (the `packages` job of `beta.yml`) finds both units installed and
  enabled, with `--layout deb` on the helper's line;
- an agent started with no state reports "not enrolled" once and makes no call;
- after `kbf-updater enroll` it heartbeats;
- the package lint fails on a package without either unit.

**As built.**

- **What the deb holds.** `kbf-host-local.service` and `kbf-updater-agent.service` in
  `/usr/lib/systemd/system`, and `kbf-updater.conf` as `/usr/lib/sysusers.d/kbf-updater.conf`,
  from `packaging/host-agent/linux` (`pkg-config --host-agent`).
- **How it enables them.** The package has no maintainer script, so it ships the links
  `/usr/lib/systemd/system/multi-user.target.wants/<unit> -> ../<unit>`. systemd reads them at
  every boot. An operator turns a unit off with `systemctl mask`, which an upgrade keeps.
- **The rpm.** It holds none of these: its helper would need `--layout rpm`, and no rpm is
  published.
- **The programs.** The units are gated on their programs (`ConditionFileIsExecutable=`). The
  programs stay in no package: an update is an installation of the package, and
  komira-ai/kbf#2233 keeps the helper and agent paths out of every release's writes. A host with
  the package and not yet the programs skips both units instead of failing them.
- **The farm's names.** The coordinator's and the frontend's addresses are configuration of the
  farm. The shipped agent line carries examples, and a host gives the farm's in one drop-in.
  A host's first install is therefore the package, the two programs and that drop-in, then M4.
- **Going back.** dpkg deletes what an older package does not list. Installing a deb older than
  this release's deletes the units on a host that got them from the package, and neither starts
  after the next boot. Before setting a farm back that far, copy the units to `/etc/systemd/system`
  on such hosts. A host whose units were placed by hand under `/etc/systemd/system` is not
  affected.
- **The tests.**
  - the `packages` job of `beta.yml` runs `pkg-install-test` with gates on both units;
  - `pkg_install` checks the units are installed and wanted by `multi-user.target`, with
    `--layout deb` on the helper's line, and that the rpm holds neither;
  - `an_agent_with_no_enrollment_says_not_enrolled_once_and_calls_nothing` (`kbf-updater`): an
    agent with no enrollment says "not enrolled" once and makes nothing, so it calls nothing;
  - `once_the_host_is_enrolled_the_waiting_agent_heartbeats` (`kbf-updater`): after the
    enrollment, the waiting agent is made from the enrolled certificate and heartbeats;
  - `the_agent_looks_between_the_steps_of_an_enrollment_and_starts_once_it_is_usable`
    (`kbf-updater`): a refused attempt, its retry and the installed answer, after which the
    agent's TLS client, made from the enrolled files, completes a mutual-TLS handshake in memory
    with a coordinator the farm's CA certified;
  - `a_package_without_the_helper_or_the_agent_unit_is_refused` (`pkg_kbf_asset`).

### G7. The coordinator is updated by the updater

**Change.** The coordinator's host runs the generic updater. `kbf-coordinator` becomes a
service with its own declaration and update hook. Its hook pulls the release's attested
image, restarts the container, and checks `GET /v1/convergence` for the new `commit`. It
is ordered before every other service (`priority`). It is ordered alone, by the
coordinator it replaces, and the quiet start after the restart holds every other order.

The `gamma` job then sets `kbf-coordinator` first and `kbf` after it. It always passes
`--require-coordinator-current`, because the coordinator is then always the release's.

**Tests:**

- a rehearsal: a coordinator of release N-1 told `kbf-coordinator = N` restarts as N,
  reports commit N, and only then orders the kbf hosts;
- a coordinator image that does not start is reverted by the hook, and `deploy-farm` fails
  on the commit check without ordering any kbf host;
- the workflow lint fails on a `gamma` job that sets `kbf` before `kbf-coordinator`.

### G8. A Mac takes update orders

**Change.** The installer counts a Mac's live leases from the native driver's lease
records, as it does a container's. A launchd job then has a lease source. With live leases
the order is `Deferred`. With none, the release is activated.

**Tests:**

- an order with a live lease is deferred and holds no slot;
- with none, the release is activated and confirmed;
- a lease record that cannot be read refuses the order with the reason, as today;
- the coordinator's `mac-daemon` role converges on a fake fleet with one Mac.

### Deferred: RH1, rehearse the forced rollback every release

RH1 extends the release's rehearsal and gives `gamma` one more `needs:`. After the forward
update to N, the coordinator forces the host back to N-1 with the release source cut off.
Then, with the coordinator stopped, the host runs `activate kbf=<N> --pin` and then
`unpin`. It needs C1, H1 and H2, which expose everything it uses. It is not scheduled, and
no issue is opened for it.

## 5. The older update issues, under host-driven updates

| issue | asked for | status |
|---|---|---|
| komira-ai/kbf#948 | a signed release feed with stage entries, and `offer` refusing a release without a passing beta | **obsolete.** There is no feed and no offer. The coordinator holds the desired version, and a release exists only after a green pipeline. `release offer` and the poller were built and deleted. |
| komira-ai/kbf#953 | a signed verdict in the tag, an approval environment, and release notes | **obsolete** for the verdict: the Beta record gates nothing, by design. The `release` environment and the notes are built. |
| komira-ai/kbf#947 | a host polls a signed feed and applies with no push and no SSH | **superseded** by the agent, the coordinator and the helper. The model it asked for is built that way. What is left is the generic updater wiring (komira-ai/kbf#2236). |
| komira-ai/kbf#957 | fetch and verify the previous release for N/N-1 tests | **done.** `cargo xtask release fetch-previous` is used by the `mixed-version` job of `beta.yml`, and `notes` verifies the previous release. |
| komira-ai/kbf#949 | gamma checks that fail a deploy that drops a connection or loses a blob, then halt and walk back | **partly obsolete.** The `kbf-installer deploy` gates it extended are deleted. The halt and the walk back are now the hook's `verify`, the coordinator's halt (komira-ai/kbf#2187), G1, G2 and C1. `prove-farm` is the gamma check. A check that no client build fails during a roll is not built, and belongs to the beta smoke, not this path. |
| komira-ai/kbf#956 | walk back to the previous release, and an automatic offer on a red hard gate | **partly obsolete.** There is no offer. Going back is `last_good` and automatic rollback (komira-ai/kbf#2181, G2), the kept releases (komira-ai/kbf#2233) and C1, H1 and H2. The marking of releases that cannot be reversed is built. Its rollback scenarios are RH1, deferred. |
| komira-ai/kbf#945 | N/N-1 checks: a wire lint, state-store goldens, a mixed-version run | **still needed, and not part of this path.** The goldens and the mixed-version and cross-version cells are built. The wire lint (no field removed or renumbered) is not. |
| komira-ai/kbf#1017 | freeze `kbf-deploy` and retire it | **still needed.** Its step "the updater switches releases" is the host-driven model. H1 replaces `kbf-deploy host rollback` for package hosts. The crate's remaining verbs serve hand-laid hosts. |

## 6. What this page does not cover

- **The farm's own state.** Which hosts the farm has, whether its `release` environment has
  reviewers, and the values of its variables are the operator's configuration and are not
  in this repository. Section 2 describes what the workflows and the coordinator do with
  whatever they are given.
- **Services other than kbf.** A second service (such as tierstore) is rolled with
  `deploy-farm --service NAME` from its own repository's release. That service's gamma is
  its own repository's design. G1's restore works the same for it.
- **Watching a farm over time.** As section 1 says, that is a generic monitoring library,
  not built.
