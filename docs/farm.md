# The farm lane: the workspace with Buck2 on the build farm, as a shadow

Cargo is kbf's build, and that does not change. CI's gate (`ci.yml`, the required checks `lint` and
`test`), the coverage floors, `macos-pr.yml` and every release (`artifacts.yml`, the seed build the
seed lint holds to plain cargo on GitHub-hosted runners) use cargo alone.

The farm lane builds the same workspace a second way, as a shadow: with Buck2, through
[buildrules](https://github.com/komira-ai/buildrules)' Rust rules, every action on the
remote-execution build farm or taken from its cache. `.github/workflows/farm.yml` runs it on every
push to `main` and on every pull request from a branch of this repository (not a fork's, not
Dependabot's). It is not a gate: no check of it is required, it never runs in the merge queue, and
a build farm outage turns it red and holds nothing. Nothing of it is shipped.

## What it builds

`buck2 build //:ci` runs three steps for each member of the workspace, but where the member's
`[package.metadata.buildrules]` leaves one to `ci.yml` (below):

- `:fmt`: the pinned `rustfmt --check` over the files `cargo fmt` reads;
- `:lint`: clippy over the library, the binaries and every test target, with `[workspace.lints]`
  and `clippy.toml`, warnings as errors;
- `:test`: each test target run as a build action. Its `.passed` marker exists only if every test
  of the target passed, and a cached marker means the tests passed on byte-identical inputs.

Every target also runs `//:check`, which is red unless the generated table matches the Cargo files.

The summary line must read `Commands: N (cached: N, remote: N, local: 0)`: no action ran on the
runner. `farm.yml` runs the build as one command, `cargo xtask farm-build`
(`crates/xtask/src/farm.rs`). It refuses to start without the build farm's worker property set,
refuses a stale generated table and hands over its fix (below), fails a build that does not say
`local: 0`, and stops buck2's daemon however the build ended.

The files of the build, all generated or written once by `buildrules`; cargo reads none of them:

| file | what | written by |
|---|---|---|
| `.buckconfig` | the cells, buildrules pinned by commit, the execution platform | `buildrules init` |
| `toolchains/`, `platforms/` | the pinned Rust toolchain and the remote execution platform | `buildrules init` |
| `BUCK` | `cargo_workspace`, and the root files members read, exported by reference | by hand, once |
| `crates/*/BUCK` | one `cargo_package` per member | `buildrules cargo adopt`, once; then by hand (`extra_srcs`, below) |
| `[package.metadata.buildrules]` in each member's `Cargo.toml`, `[workspace.metadata.buildrules]` in the root's | the policy: which steps `//:ci` runs | `buildrules cargo adopt`, then by hand |
| `buildrules.lock.bzl`, `buildrules.bzl`, `third_party/buildrules/` | Cargo's unit graph of every member; every crates.io crate as a unit pinned by SHA-256; each git dependency (jobscheduler's and tierstore's crates) as GitHub's archive of its pinned commit, by SHA-256 and size (`git_archives.tsv`) | `buildrules sync`; never edited by hand |

`Cargo.toml`, `Cargo.lock` and `rust-toolchain.toml` stay the source. `//:check` fails every
target when the generated table no longer matches them.

## A stale table: the fix is a patch of the run

A pull request that changes a manifest or the lock and not the table makes the table stale. This
includes a move of the jobscheduler or tierstore pin. You do not need `buildrules` to fix it: the
job makes the fix and hands it to you (`crates/xtask/src/farm/table.rs`).

1. Before the build, the job builds the `buildrules` command line from the pinned buildrules cell
   on the build farm (`buildrules//cli:buildrules`) and runs `buildrules check`.
2. When check finds the table stale or a generated file missing, the job runs that
   `buildrules sync` on the runner (which has the network a new git commit's archive needs),
   checks the table again, and writes what sync changed as a patch. It fails without building.
   Its error and the job's summary name the inputs that changed, list the files the patch changes,
   and give the one command that applies it; the patch is the run's artifact `buildrules-sync`.
3. You read the patch and apply it from a checkout of the branch, then commit and push:

   ```sh
   gh run download <run id> -R komira-ai/kbf -n buildrules-sync -D buck-out/buildrules-sync-<run id> && git apply buck-out/buildrules-sync-<run id>/buildrules-sync.patch
   ```

   The summary prints it with the run's id filled in. `buck-out/` is ignored by git. On a pull
   request the job's checkout is the merge of the branch into main, so the patch is made against
   that merge; if `git apply` refuses it, merge main into the branch and push, and the next run
   carries a new patch.

buildrules can also sync on the farm (`cargo_sync`, with no network), but not this workspace yet: a
member of a git dependency that inherits its own root's `workspace.dependencies` does not resolve
there (komira-ai/buildrules#187). So `BUCK` declares no sync target, and the sync with the network
above is the one way to regenerate the table.

The job does not push the regenerated files to the branch itself: that needs a token that can
write to the repository, in the one job that holds an OIDC token and runs a pull request's code;
a push made with the job's own token starts no workflow; and a generated change of thousands of
lines would land on the branch unread. A patch the contributor applies keeps the job read-only and
the change reviewed.

A Buck2 action sees only its declared inputs. A library unit sees `src/` and `build.rs`, a test
target `tests/`, and a build script its whole package. A file a unit reads beyond that is declared
in the member's BUCK file (`extra_srcs`), with the reason in a comment there.

## What stays in ci.yml, and why

A farm action has its declared inputs and nothing else: no network, no `PATH` but buildrules'
busybox directory, no git checkout, not root, no Docker or systemd, and only the Postgres
buildrules starts, which runs with `synchronous_commit` off. buildrules' test gate refuses a
test target that has an `#[ignore]`d test, because an ignored test is a test that never runs, and
`:test` runs a whole test target, so it cannot leave one slow test out as the pull-request profile
does. Each member below says why in its `[package.metadata.buildrules]`, and its tests run in
`ci.yml` alone.

**Built and linted, tests in ci.yml** (`ci = ["fmt", "lint"]`):

| member | why its tests stay hosted |
|---|---|
| `host-step` | `tests/fs_real.rs` holds an `#[ignore]`d test |
| `kbf-front` | its library tests and `tests/measure_hit.rs` hold `#[ignore]`d tests; slow tests (`large_blobs`, `proxy`, `write_group`) |
| `kbf-host` | its library tests hold an `#[ignore]`d test |
| `kbf-host-install` | its `real_drop`, `real_linux_unit` and `update_rehearsal*` targets hold `#[ignore]`d tests (root, systemd) |
| `kbf-image` | `tests/live.rs` holds `#[ignore]`d tests |
| `kbf-installer` | its library tests and `tests/real_gh.rs` hold `#[ignore]`d tests; a slow test (`binary`) |
| `kbf-integration-test` | its real-service targets hold `#[ignore]`d tests; the repository lints read the whole repository; Postgres |
| `kbf-log` | `tests/golden_corpus.rs` holds an `#[ignore]`d test |
| `kbf-meta` | `tests/evict.rs` holds an `#[ignore]`d test |
| `kbf-raft` | `tests/sim` holds an `#[ignore]`d test; slow tests (`sim`) |
| `kbf-server` | `tests/s3.rs` and `tests/state_golden.rs` hold `#[ignore]`d tests; slow tests; Postgres |
| `kbf-sim` | its `determinism` and `toy_counter` targets hold `#[ignore]`d tests |
| `kbf-sim-cell` | `tests/cell.rs` holds an `#[ignore]`d test; slow tests (`cell`, `write_path`, `write_tier`) |
| `kbf-task` | its `network`, `probe` and `sandbox` targets hold `#[ignore]`d tests |
| `kbf-updater` | `tests/root_part_root.rs` holds an `#[ignore]`d test |
| `mdm-core` | `tests/installed_host.rs` holds an `#[ignore]`d test |
| `update-rights` | its `boot_root` and `rights_root` targets hold `#[ignore]`d tests (root) |
| `kbf-cli` | slow tests (`rehearsal_in_a_box`) |
| `kbf-history` | a slow test (`history`) |
| `kbf-transfer` | slow tests (`engine`) |
| `xtask` | a slow test (`check_asset`) |
| `kbf-pack-adopt`, `kbf-pack-index`, `kbf-setup`, `kbf-state-copy` | Postgres with `synchronous_commit` on, which the state store requires; buildrules' Postgres runs with it off (komira-ai/buildrules#166). `kbf-state-copy` also reads `kbf-server`'s golden state file |
| `kbf-signer` | its Unix sockets, below the test's temporary directory, are longer than a socket path may be on the farm |
| `kbf-enroll` | a config test expects `/proc/self` to be a symlink root owns, which it is not inside a farm action |
| `kbf-coverage` | its command-line tests run git and read the repository's `coverage-floors` |
| `kbf-deploy` | its HTTPS and mdm tests load the host's trusted root certificates, which a farm action has none of; one test reads a document of the repository |
| `kbf-harness` | its stand-in test looks for a binary cargo builds beside the test binary in `target/` |
| `kbf-coordinator` | its binary's tests need the host's trusted root certificates; `tests/container_unit.rs` reads the container's unit file and Dockerfile from the repository's packaging |

No member declares buildrules' Postgres fixture (`postgres = { url-env = ... }`): every member
whose tests need Postgres either needs it durable (above) or is left to `ci.yml` for another reason.

Some checks only `ci.yml` makes, for every member:

- Coverage: the floors of `coverage-floors` are cargo-llvm-cov's, measured on the fast tests.
- Doctests: buildrules does not run them.
- The macOS tests (`macos-pr.yml`), the Arm run, and the jobs that need root, systemd, Docker or a
  service other than Postgres (`beta.yml`).

## Running it yourself

You need buck2 (the release in buildrules' `tools/buck2`) and a `.buckconfig.local` (gitignored)
that names the build farm: `[buck2_re_client]` with the endpoints and instance, and
`[buildrules_re] linux_x86_64_properties` with the worker property set
(`.buckconfig.local.example` has the keys). This repository holds neither. Then:

```sh
buck2 build //crates/kbf-types:test    # one member's tests
buck2 build //:ci                      # what farm.yml builds
buck2 log what-failed                  # the failing actions and their commands
```

Read the summary line: `local` must be 0. There is no local execution platform, so with no farm
configured nothing runs on your machine.

## How the job reaches the build farm

The build farm is on a private Tailscale network. The job joins it with
`tailscale/github-action`, pinned by commit, as an ephemeral node tagged `tag:ci`, with the job's
OIDC token (workload identity federation: no secret is stored). Its inputs are the organisation's
variables `TS_CLIENT_ID` and `TS_AUDIENCE`. Then `cargo xtask farm-config`
(`crates/xtask/src/farm_config.rs`) masks the farm's address in the log (as given, its host, and
every address the host resolves to, which buck2's connection errors print) and writes the farm's
buckconfig under `~/.buckconfig.d`, where buck2 reads it, from `FARM_ADDRESS`, `FARM_INSTANCE`,
`FARM_TLS` and `FARM_LINUX_X86_64_PROPERTIES`. It refuses an empty value, so a run without the
variables never builds on the runner.

That join is why the job holds `id-token: write` on a pull request, the one job of this repository
that does. The workflow lint (`crates/kbf-integration-test/tests/workflows/farm.rs`) holds it to:

- exactly that scope and `contents: read`, under a top-level `permissions: {}`;
- the exact `if:` that skips a fork's pull request (which gets no OIDC token) and Dependabot's;
- the Tailscale step's inputs exactly the variables above, pinned by a full commit, and exactly one
  such step;
- `cargo xtask farm-build` of `//:ci` after the join;
- the triggers, the one job and its runner;
- across every workflow, no other job a pull request or the merge queue starts that holds
  `id-token: write`.

If that `if:` were wrong, pull-request code would get this repository's OIDC token, and with it a
place on the build farm's network. The job names no secret and holds no other write scope.

A pull request's build and a build of `main` share the build farm's action cache, as every build of
every repository on the build farm does. The cache is keyed by the action's digest, so a result is
reused only for byte-identical inputs and command.

## A build whose connection to the farm dies

A connection to the farm can die without a word (an uplink that drops, a relay that goes away):
buck2's remote execution client keeps the stream open and waits for an answer that never comes,
and its `Waiting on ...` line looks the same for a running action and a lost one. Without help the
step runs until its timeout.

So every buck2 command that talks to the farm runs under komira-ai/komira's `assert_remote run`,
the tool of komira's `buck2-farm` action: in this workflow `farm-build`'s two buck2 builds (the
`buildrules` command line, then `//:ci`), and in `release.yml`'s `gamma` job the proof of
`prove-farm`. While buck2 runs, the tool reads the runner's TCP connections to the farm (the
`engine_address` of the buckconfig) from the kernel once a second. A live farm sends something on
every connection at least every 30 s, and closes one that moved nothing for 150 s; when a
connection has heard nothing from the farm for 300 s, the tool ends buck2 (SIGINT, then SIGKILL)
and its daemon (`buck2 kill`), and treats the run like a cut connection: it waits for the farm's
port to answer, up to `farm_wait_seconds` (600 s, written by `farm-config` and `prove-farm`), and
runs the same command again, at most three runs in all. It never reads the log's silence and never
bounds how long one action may run: a slow build on a live connection is never cut.

What you see when it ends a run, in the job's log:

```text
buck2-farm: watchdog: the connection to the farm from local port 41022 has heard nothing from the farm for 5 min 1 s, past the bound of 5 min 0 s: the farm has closed it and buck2 would wait on it for ever; ending buck2 (SIGINT)
buck2-farm: stopping buck2's daemon, which holds the dead connection (`./buck2 kill`)
buck2-farm: the first run hung on a dead connection to the farm and was ended; waiting for the farm's port to answer, up to 10 min 0 s
```

then the next run's output, and one line in the job summary saying the run hung, how long the
farm was silent and that the step ran again (run 2 of at most 3). If the third run hangs too, or
the farm does not answer within the wait, the step fails with the tool's line saying so. The farm's
address is never printed; the lines name the runner's local port.

The tool is not copied here and no action of komira's is used (this repository allows only
`actions/*` and `tailscale/github-action`): the job checks out komira's
`tools/build/ci/assert_remote/` alone, at a full commit of komira main, into `buck-out/komira`
(ignored by git and buck2), fetches Zig 0.15.2 (the release komira builds the tool with) pinned by
SHA-256, and builds it with `zig build-exe` before the tailnet join. The workflow lint's
`farm_steps` (`crates/kbf-integration-test/tests/workflows/farm_steps.rs`) holds every workflow to
it: no step runs buck2 itself; each `cargo xtask farm-build` or `prove-farm` is given
`--assert-remote "$RUNNER_TEMP/assert_remote"`, after a pinned checkout of the tool's sources and
its build in the same job; and both workflows pin one komira commit.

### Every step has its own bound

In a workflow with a farm step (`farm.yml`, `release.yml`), every step has its own
`timeout-minutes`, under its job's, and the lint holds that too: a step the watchdog does not watch
(a fetch, a cache step, a cargo build) is cut by its own bound, never left to the job's. Each bound
is three times the longest successful run of the step, or of a step with the same command in
another job of the workflow, rounded up to 5, 10, 15 or 20 minutes, and never under 5 (the runs:
the 13 successful `farm.yml` runs and the 80 latest successful `release.yml` runs as of the change,
read from each run's job and step times). The exceptions are the watched steps and the new ones:

| step | longest successful run | bound |
|---|---|---|
| `farm.yml` `buck2 build //:ci on the build farm, local 0` | 12.6 min (a cold build that later failed: 21 min) | 120, under the job's 150: three cold runs and two ended runs with their waits |
| `release.yml` `gamma` `Prove the farm with a remote-only Buck2 build` | none in the window (the proof is off while `KBF_SETS_DESIRED` is unset) | 40: two ended runs (5 min each, 10 min wait) and a last run; the job's is raised to 135, the sum of its steps' |
| the Zig fetch, the tar, the build of `assert_remote` | new | 15, 5, 10 |
| the `-3` attempts of the apt jobs | none (never needed) | their first attempt's |

What the watchdog does not cover: steps that do not run buck2. `macos-farm.yml` reaches the farm's
Mac platform with `cargo xtask mac-farm`, its own REAPI client, not buck2; `buildrules sync`, run
on the runner when the table is stale, runs buck2 itself, outside the tool. Those, and every other
step, are cut by their own bound.
