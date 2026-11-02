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
