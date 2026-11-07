# AGENTS.md

The rules every change to this repository is written and reviewed against, by people and coding agents alike. `CLAUDE.md` imports this file
(its one line is `@AGENTS.md`), so every agent reads the same text. [CONTRIBUTING.md](CONTRIBUTING.md) says how to build, test and sign
commits, and links here for the rules.

The file has three parts: the shared core, the same text word for word in every repository of the project; the language rules that apply to
this repository's code; and this repository's own rules. Rules are cited by id.

The shared core is the text between the two marker lines below. The repository lint `agents_md`
(`cargo test -p kbf-integration-test --test repo_lints agents_md`, in `crates/kbf-integration-test/tests/agents_md/`) fails when it differs
from the checked-in copy `tools/agents/shared_core.md`, or that copy from its hash in `tools/agents/shared_core.sha256`; a change to the
core changes all three in one pull request.

<!-- shared-core:begin -->
## 1. Shared core

Severity: BLOCK must be fixed before merge. SHOULD is fixed unless the author gives a reason.

### Tests and coverage

- T1 BLOCK. Every touched package or crate is at least 99% line and 99% branch as measured by the PR check. Aim 100%; never demand above 99. Check: the coverage job table. Fix: add behaviour tests for the uncovered lines and branches.
- T2 SHOULD. Coverage means line and branch percent. Reject claims based on test-file or test-function counts.
- T3 BLOCK. No coverage floor or baseline is lowered or set below 99, and no shared coverage census or shared ratchet file is added. Floors live in each package. Check: diff of floor/baseline files.
- T4 BLOCK. Do not hide code from measurement: no `#[cfg(not(coverage))]`, `#[coverage(off)]`, moving a crate into `exclude`, `test = false` binaries. A `main.rs` only forwards to its library. Check: grep the diff.
- T5 BLOCK. A bug fix ships a test that failed before the fix and is wired in (`#[test]` in the crate; named in `test_srcs` for Mojo). Check: find the test, read it against the pre-fix code, check the PR text says what it asserts.
- T7 BLOCK. Never edit a test green over a capability gap: no weakened or deleted assertions, no tests removed from `test_srcs`, no `tests_known_failing`.
- T8 BLOCK. Tests assert behaviour. The PR body lists tests by name with the behaviour each proves. A "Catches ..." line is allowed; stating the defect caught is SHOULD.
- T9 BLOCK. No mutant bookkeeping in code or PR text: no mutant tables, planted-defect run ids, "seen red" notes, or test names/comments mentioning mutants. Check: grep diff and PR body for `mutant`, `seen red`, `planted defect`.
- T10 BLOCK. Tests never assert timing: no elapsed-time bounds, latency ceilings, or sleeps racing real timeouts. Inject a clock or use explicit progress events. Check: grep for `Instant::now`, `elapsed`, `sleep(`, `perf_counter`, timeout races used as pass/fail.
- T11 BLOCK. Tests are written in the component's own language. No new shell or Python tests (`*_selftest.sh`, `tests/*.sh`, `py_test`), and no growth of existing shell selftests, except for the Python SDK surface. New cases go to a Rust tool with Rust tests.
- T12 BLOCK. Tests are hermetic and deterministic: no network or DNS (use fakes), no hard-coded ports or fixed paths, no shared state between tests, no silent skip (a skip states its reason), no retry-until-green. Use the repo's scratch helpers; tests never touch the host's real service directories.
- T13 SHOULD. Tests cover failure and boundary cases, not only the happy path. A no-leak or redaction test asserts the full message by exact equality.
- T15 BLOCK. A new gate, lint, or check has a negative case that fails on a defect. A warm cached green does not prove a closure change. Describe it as a behaviour test (T9 applies).
- T19 SHOULD. A behaviour change updates goldens and any docs describing it in the same PR; a generator or codegen change moves its goldens in the same PR (BLOCK for codegen).

### Design and change shape

- B8 BLOCK. Formats change additively only: new optional field, record type, oneof case, table, or column-with-default. Reject renumbering, wire renames, removing or re-meaning a shipped field, new required fields, refusing unknown data, or raising a version floor. The PR names the test that runs the previous release on new data and vice versa.
- B20 SHOULD. Do not answer a config mistake with runtime machinery (gates, counters, extra metrics, dry-runs). Fix the config at the source and check it once at deploy. No speculative abstractions or ports without a concrete use case.
- B22 SHOULD. A change touching more than one package, or taking more than a day to reverse, links a design doc. Moving or deleting a package needs the supersession check.
- P7 SHOULD. One subject per PR. Refactors are split from behaviour changes; interfaces land before the code that uses them.
- P10 SHOULD. Docs, headers, and comments describe only what the code does. Flag any doc the diff contradicts or leaves stale (BLOCK when a header contradicts its code).

### CI and workflows

- B9 BLOCK. A PR runs only build and unit tests with coverage. Reject release dry-runs, a `pull_request` trigger on a release workflow, new PR-gate jobs, slow tests in the PR set, and darwin/macOS jobs or targets in per-PR checks. Advisories never block; licences/bans/sources do.
- B10 BLOCK. Workflows: hosted runners only (no self-hosted or paid), actions pinned by commit SHA, no `pull_request_target`, external tools pinned by URL and SHA-256. Workflows are actionlint clean.
- B12 SHOULD. Releases are automatic from main: no manual tag step, no bake/gamma-verdict evaluator.

### PR text and commits

- P1 SHOULD. The title says concretely what changes. No vague words: trustworthy, harden, robust, improve.
- P2 SHOULD. The body leads with the user-facing change (before and after, and what does not change); then a layer-by-layer walk in reading order with the why; then compatibility and failure modes; then tests by name with the behaviour each asserts. Plain language. Suggest a split if part can land alone.
- P3 BLOCK. Commit messages cite issues as `Refs <owner>/<repo>#<n>`, never `Fixes`, `Closes`, `Resolves`. A bare `#12` is SHOULD; name the repo.
- P4 SHOULD. Authors commit under their own identity. A commit written with a model ends with a `Co-authored-by:` trailer naming that model. Repos enforcing DCO: every commit, merge commits included, has a `Signed-off-by:` trailer matching the author email (BLOCK there).
- P5 SHOULD. History is not rewritten: no `fixup!`/`squash!` commits, no force-push, no amend or rebase of pushed commits. A correction is a new commit saying what it corrects.
- P6 BLOCK. The PR contains only paths for its purpose. No `.claude/agent-memory/**`, scratch, logs, `.out` files, or unrelated churn. Check: file list.
- P8 SHOULD. Stacked PRs: the body states the stack and merge order; the base is main at merge time; stacked trains land with merge commits, not squash.
- P9 BLOCK. CI ran on the PR head SHA, not a stale merge ref. Compare the CI run SHA with the head SHA.
- P11 SHOULD. Every number in PR text (tests, callers, PRs, cache percentages) names the query that produced it and separates measured from inferred.

### Secrets and configuration

- S1 BLOCK. No secret, token, key, endpoint address, or worker property set committed. No remote-execution endpoint or `instance_name` committed. Check: grep for URLs with ports, bearer tokens, private keys, `AWS_`/`GOOGLE_` key material, `remote_execution` keys.
- S2 BLOCK. Configuration is passed as flags, not environment variables. Allowed env: platform-set mode markers, secret material, provider-standard credentials (`AWS_*`, `GOOGLE_*`). Check: grep for `getenv`, `std::env::var`, `env::var_os`, `os.environ`; judge each against the three exceptions.
- S3 BLOCK. A diff must not make a cluster write, upload, deploy, or cloud spend executable by default, or run against a gamma or prod account. Steps only the owner runs are held scripts: dry-run default, never forcing, `bash -n` clean, in the documented location.
- S4 SHOULD. Committed scripts and docs use the workspace scratch directory, not `/tmp`. No `rm -rf` in scripts; use the scratch-removal tool.
<!-- shared-core:end -->

## 2. Rust

kbf is a Cargo workspace of Rust crates; these are the rules of its code. The tool of T11 is a Rust test or a `cargo xtask` verb here.

- R1 BLOCK. `cargo fmt --all --check` and `cargo clippy --workspace --all-targets --locked -- -D warnings` are green. No new `#[allow(...)]`/`#![allow(...)]` without a stated reason.
- R2 BLOCK. No new `unsafe`. The `unsafe` this workspace has is system and framework calls (libc, the macOS frameworks) in the crates that own those resources; a change adds none, and a crate with `#![forbid(unsafe_code)]` keeps it.
- R3 BLOCK. No new `.unwrap()` or `.expect(` in production paths. Libraries use `thiserror` enums with context; `anyhow` only in binaries; no `Box<dyn Error>` in library signatures.
- R6 BLOCK. Dependencies come from crates.io (or pinned sibling-repo revisions) with a permissive licence; `cargo deny` licences, bans, and sources pass. Advisories (RUSTSEC) never block a PR.
- R7 BLOCK. Manifests parse after the merge: no duplicate keys or merge artifacts in changed `Cargo.toml`.
- R8 BLOCK. Crate drift: if main changed anything under a crate this PR touches (`crates/<crate>/`, including src, tests, Cargo files) since CI ran, require main merged and a fresh CI run. Check: `git diff head...main -- crates/<crate>`.
- R9 SHOULD. Minimal public API: small trait bounds, ownership visible in signatures, newtypes, no trait with a single implementation, no speculative abstraction.
- R12 BLOCK. Files stay under 1000 lines.
- T17 SHOULD. Rust coverage: do not rely on tests the PR profile skips; arms of one function covered only by different test binaries or generic instantiations count as missed.

## 3. This repository

- B13 BLOCK. kbf: plain cargo only; no dependency on buildrules, mopm, komira, or other code of ours; no BUCK/Bazel for its own build; no enforced TLS/mTLS (a start-line warning at most); no Mojo additions.
- B15 BLOCK. kbf scheduling is measured, not reserved: no per-category tables, no category env passing, no head-of-line node reservation. Drain and update paths wait by default; a deadline is an operator option with no cap; never cut in-flight work.
- B16 BLOCK. kbf/tierstore data paths: action commands run network-less in the job container (explicit logged opt-in only). Outputs are read once (hash and store in one pass), no blob size cap, no whole-file buffering. Jobs write only to their own output path with no delete privilege.
- B17 SHOULD. Host updates use one generic updater with per-service update-hook subcommands declared in a root-owned file; no per-roll scripts or per-service logic in the updater.
- R4 BLOCK. Pure crates (the kbf crates and modules `crates/kbf-integration-test/tests/layering.rs` lists as pure): no clock reads, no sleep, `BTreeMap`/`BTreeSet` not `HashMap`/`HashSet`, no sockets, no tokio/tonic/hyper/reqwest/rand/getrandom/mio/socket2, and `#![deny(clippy::disallowed_methods, clippy::disallowed_types)]` on one line in each lib/bin root, never relaxed.
- R5 BLOCK. Layering: a crate depends only on its own layer or lower, as `crates/kbf-integration-test/tests/layering.rs` holds; every `js-*` crate comes from one jobscheduler revision, so there is one `js-types` only, and jobscheduler never depends on kbf crates; external deps are `name.workspace = true`, declared once at the root.
- B4 BLOCK. kbf's tooling is Rust. No new shell, Python, awk, sed, Perl or Ruby file; a workflow `run:` step is one command, with no shell logic (more than one line, a pipe, `&&`, `;`, `$(`, a backtick, `if`, `for` or `while`): anything more is a `cargo xtask` verb or a flag of the program the step runs; no production code under `crates/*/src/` runs a shell with `-c`. `tools/interpreter_budget.tsv` lists what is left to port and only shrinks; a test that names an interpreter needs a `[test-interpreters]` row there, and a new test drives Rust instead.
- B8 BLOCK (what it covers here). The formats of B8 are every format kbf writes and another kbf reads: the metadata log, state-store rows, files a node keeps on disk, and the protobuf and JSON messages between processes. A reader skips what it does not know (an unknown field is ignored; an unknown record, command or enum value is passed over as a whole and counted); data that cannot be parsed at all is still refused. A PR that adds a record type says what an older release loses by skipping it, and that loss is safe.
- B10 BLOCK (this repository). Every workflow also has a top-level `permissions: {}`, and uses only `actions/*` and `tailscale/github-action` actions.
- P4 BLOCK (this repository enforces DCO). Every commit, merge commits included, has a `Signed-off-by:` trailer matching the author email.
- B9 BLOCK (macOS on a pull request, this repository). The core B9 keeps macOS out of the per-PR checks; here a pull request takes a macOS runner exactly in these jobs. `macos-pr.yml`'s `macos-test` (the macOS crates' tests) and `macos-device` (the steps of `beta.yml`'s `native-macos` that need root or a device) run on `macos-15` when the pull request changes a path `tools/ci/macos-pr-paths.txt` lists (the Mac daemon and executor platform, the launchd and macOS adapters, Mac packaging, the step runner and self-update, and the workflows and xtask code that build or check them); its `macos` job, the check to require, passes when both pass or neither was needed. Besides those, three workflows run on a pull request that changes their own paths (`host-install-mac.yml`, `mac-provision.yml`, `vmm-probe.yml`), and `artifacts.yml`'s darwin build runs with the label `needs-artifacts`. Any other pull request takes no Mac. A pull request's macOS job holds no secret, no environment, no OIDC token and no write scope, and a newer push to the pull request cancels its older macOS run. The after-merge macOS jobs still run on every push to `main` (`beta.yml` unchanged; ci.yml's `task-macos`, which no longer runs on a pull request, also in the merge queue), and every step of `native-macos` and `task-macos` is also a step of a pull request's macOS job.

Where the rules above are checked in this repository (all from `cargo test -p kbf-integration-test`, unless named otherwise): coverage
(T1, T3) by `kbf-coverage` against `coverage-floors` in CI's `test` job, its wiring by the `coverage_wiring` lint; test wording (T9) by
`test_wording`; test scratch directories made under the system temporary directory (part of T12) by `scratch_dirs`; workflow runners,
triggers, permissions and action pins (B10) by `workflows`; shell and interpreters (B4) by
`interpreters` and `test_spawns`; pure crates and layering (R4, R5) by `--test layering`; one jobscheduler revision (R5) by
`--test js_single_source`; this file and `CLAUDE.md` by `agents_md`; macOS on a pull request (B9 here: the jobs, their gates, no
secret, the shape of `macos-pr.yml`, its copy of the after-merge Mac steps and the paths file) by `macos_pr`. The lints other than
`layering` and `js_single_source` are modules of `--test repo_lints`.
