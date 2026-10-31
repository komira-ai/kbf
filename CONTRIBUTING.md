# Contributing to kbf

Thank you for helping. This file says what a pull request needs before it can
be merged.

## Developer Certificate of Origin (DCO)

Every commit must carry a `Signed-off-by` line. By adding it you certify the
[Developer Certificate of Origin](https://developercertificate.org): that you
wrote the change, or otherwise have the right to submit it under the project's
license ([Apache-2.0](LICENSE)).

Git adds the line for you with `-s`:

```sh
git commit -s -m "server: reject digests with a negative size"
```

which produces:

```
server: reject digests with a negative size

Signed-off-by: Your Name <you@example.com>
```

The name and email must match the commit author. Maintainers do not merge a
pull request with an unsigned commit. An automated DCO check will enforce this
once it is set up; until then, reviewers check the trailer by hand.

## The mutant rule

A test that cannot fail is not a test. Every test in kbf must have been seen
failing on a planted defect (a mutant) before it is merged:

1. Write the test and see it pass.
2. Plant a mutant: a small, deliberate bug in the code the test is meant to
   guard (flip a comparison, drop a check, return early).
3. Run the test and see it fail at the assertion you expect. A failure caused
   by a compile error proves nothing.
4. Remove the mutant.

Each test says, in a comment or its name, which defect it catches. The pull
request description lists every mutant you planted and the test that went red
on it.

A bug fix ships with a test that failed before the fix.

## Small pull requests

Keep pull requests small and about one thing. A reviewer should be able to
read the whole diff in one sitting. Split refactors from behavior changes, and
land interfaces before the code that uses them. Keep source files under 1000
lines.

## Formats change additively

Every format kbf writes and another kbf reads changes additively only: the
metadata log, state-store rows, files a node keeps on disk, and the protobuf and
JSON messages between processes. A farm runs two releases side by side during an
update, and a node may go back to the release before; each release must start on
data the other wrote.

Allowed:

- a new optional field, whose absence (or zero value) means what the code did
  before the field existed;
- a new record type, log command, oneof case or table;
- a new column with a default.

Not allowed:

- renumbering, renaming on the wire, or removing a field, case or table that a
  release has shipped;
- giving an existing field or value a new meaning;
- a required field, or a check that refuses data because it carries something
  this release does not know;
- a version or floor raise that an older release cannot open.

A reader skips what it does not know: an unknown field is ignored, and an
unknown record, command or enum value is passed over as a whole and counted.
Data that cannot be parsed at all is still refused. When you add a record type,
the pull request says what an older release loses by skipping it, and that loss
must be safe. Every pull request that changes a format names the test that
starts the previous release on data the new one wrote, and the new one on data
the previous release wrote.

## Building and testing

kbf is a Cargo workspace; the crates live under `crates/`. All builds and
tests use `cargo`, with the toolchain pinned in `rust-toolchain.toml`:

```sh
cargo build --workspace
cargo test --workspace --locked
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo fmt --all --check
```

There is no other build system in this repository.

The tests that need Postgres fail, and are not skipped, when no server is named. Set
`KBF_TEST_POSTGRES_URL` to the URL of a server and a superuser role (the tests create and
drop databases and roles), for example `postgres://postgres@localhost:5432/postgres`; the
Postgres service container of `.github/workflows/ci.yml` is one. Each such test makes a
database or a schema of its own on it (the `database` module of `js-testkit`, which `kbf-testkit` re-exports, makes the database)
and drops it when the test ends, so tests run in parallel and never share rows.

## No new shell or Python

kbf's tooling is Rust. The repository lints (`cargo test -p kbf-integration-test --test
repo_lints`) refuse a workflow `run:` step with shell logic (more than one line, a
pipe, `&&`, `;`, `$(`, a backtick, `if`, `for` or `while`) outright: a step is one
command, and anything more is a `cargo xtask` verb or a flag of the program the
step runs. They refuse a shell, Python, awk, sed, Perl or Ruby file, and production
code under `crates/*/src/` that names a shell and a `-c` flag, unless
[tools/interpreter_budget.tsv](tools/interpreter_budget.tsv) lists it by path
(a `<key><TAB><reason>` row). The rows there are what is left to port. The list
records what exists, not how big it is: editing a listed file or keeper leaves
the list alone. The list only shrinks: on a pull request,
`cargo xtask budget-diff --base HEAD^1` refuses any row the base branch's list
does not have, so a listed file is not moved or renamed either (that would be a
new row); it is ported. A pull request that deletes one removes its row in the
same commit; the lint refuses a row whose file or keeper is gone.

Test code is held to the same list. A test file (under `crates/*/tests/`, or
`#[cfg(test)]` code under `crates/*/src/`) that names an interpreter, as a
program (`Command::new("perl")`, an argv starting `/bin/sh`) or as the `#!` line
of a script it writes, needs a `[test-interpreters]` row for its path saying why
the test needs it. That list only shrinks too: a new test drives Rust (a
`kbf-testkit` fake or the program under test), not a shell.

## Continuous integration

CI is defined in [.github/workflows/ci.yml](.github/workflows/ci.yml) and runs
only on GitHub-hosted runners: formatting, clippy, the repository lints,
`cargo deny` (licences, bans and sources), and the workspace tests on x86-64 and
arm64. A pull request is merged only when CI is green on its head commit.

Security advisories against our dependencies do not block a pull request: a new
advisory against a crate we already use would fail every open pull request for
something none of them did. [.github/workflows/advisories.yml](.github/workflows/advisories.yml)
runs `cargo deny check advisories` daily and on every push to `main`, and
`cargo xtask advisories` keeps the result in one issue, "Security advisories in
kbf's dependencies": it opens it when there are advisories, rewrites its table when
they change, and comments and closes it when none remain. The run itself is green
whatever it finds; it fails only when cargo-deny or the GitHub API did.

A tool a job needs from outside the workspace (cargo-deny, cargo-llvm-cov,
cargo-cyclonedx, Bazel, buck2) is pinned in the workflow by URL and SHA-256 and
installed by `cargo xtask fetch`, which refuses a download whose SHA-256 differs
before it unpacks or writes anything.

The pull-request gate is 99.00% of lines and 99.00% of branches in every crate,
measured on the fast tests (the `pr` profile of `.config/nextest.toml`) by the
`test` job. The target is 100%, and reviewers hold the rest: a pull request
covers the code it adds or changes.

The fast tests run in shards, in the matrix job `test-shard`: the scheduler's
legacy core on the workspace in three parts and the js core on the four packages
that link the server in one, each part a `nextest --partition count:<part>/<parts>`
of that core's tests. The `test` job builds both cores' instrumented tests itself
(without running them), waits for every shard's raw profiles, checks that each
shard's profiles are there, makes one report of them all and gates it, so a crate's
coverage is that of all the tests of both cores together, as when one job ran them
in turn. The matrix is the list of shards: a core's entries are its parts 1 to N,
each once (`crates/kbf-integration-test/tests/coverage_wiring` holds that).

The job runs `kbf-coverage` against [coverage-floors](coverage-floors) and fails
a crate whose line or branch coverage is under its threshold: 99.00%, or, for a
crate the file lists, the floor listed for that metric. The comparison is exact
(`hit * 10000 >= percent * found`, in integers), a metric with nothing to
measure (a crate with no branches) passes, and a crate at 98.99% fails. A floor
is the crate's coverage when the file was written, floored to hundredths of a
percent; each crate listed has an open issue to reach 99%.

A floor may only rise:

- When coverage improves, `kbf-coverage raise` lifts the floors to the
  measurement and drops a floor once its metric reaches 99%. The job prints
  the floors that can rise and the command, and does not fail for them:

  ```sh
  cargo run -p kbf-coverage -- raise --lcov <lcov.info> --root .
  ```

  Commit the rewritten file. `raise` never adds a floor and never lowers one.
  Without CI, `<lcov.info>` is a `cargo llvm-cov --branch --lcov` run with
  the job's toolchain and flags (see `.github/workflows/ci.yml`), and a relative
  `--root` is resolved against the working directory.
- On a pull request, the job's last step runs `kbf-coverage no-lowering` against
  the base's file and fails when any threshold is lower there: a lowered floor,
  and a floor added for a crate or metric that had none (adding a line lowers its
  threshold from 99.00%). A crate under 99% is fixed with tests. A crate that
  reaches 99% on both metrics loses its line (removing a floor raises the
  threshold).
- A floor with nothing to measure fails (`FLOOR BUT NOTHING MEASURED`): a crate
  whose tests stopped being instrumented would otherwise pass its floor. The
  gate cannot see that for a crate with no floor, which has no record in the
  tracefile when it has no instrumented test target (a library with
  `[lib] test = false`); give the crate a test target.

The gate does not see code that the report leaves out; reviewers hold these until
repository lints refuse them:

- A crate moved outside `crates/` (listed in the workspace's `exclude` and
  kept as a `path` dependency) is no longer a workspace crate: it stops being
  measured. The workspace has no `exclude` today.
- `cargo llvm-cov`'s default pattern ignores every file in a directory named
  `tests`, `examples` or `benches`, wherever it sits: a module directory
  `src/**/tests/`, or a file in such a directory that `src` includes (for
  example with `#[path]`), is not measured.
- A binary target with `test = false`, `#[cfg(not(coverage))]` and
  `#[coverage(off)]` keep code out of the report.

Changes under `.github/workflows/` will need a review from the code owners
listed in [.github/CODEOWNERS](.github/CODEOWNERS). That rule is a placeholder
today: the owners line is commented out until a maintainers team exists, and
nothing enforces the review yet.

## Reporting security issues

Do not open a public issue or pull request for a vulnerability. Follow
[SECURITY.md](SECURITY.md) instead.
