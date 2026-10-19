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

## No new shell or Python

kbf's tooling is Rust. The repository lints (`cargo test -p kbf-it --test
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
`cargo deny`, and the workspace tests on x86-64 and arm64. A pull request is
merged only when CI is green on its head commit.

A tool a job needs from outside the workspace (cargo-deny, cargo-llvm-cov,
cargo-cyclonedx, Bazel, buck2) is pinned in the workflow by URL and SHA-256 and
installed by `cargo xtask fetch`, which refuses a download whose SHA-256 differs
before it unpacks or writes anything.

The target is 100% line and branch coverage for every crate, so a pull
request fully covers the code it adds or changes. The `coverage` job measures
line and branch coverage per crate and checks it against
[coverage-baseline](coverage-baseline), which records how many lines and
branches of each crate no test runs. The job fails when either count rises.
That is what it enforces, and it is slightly weaker than the target: a pull
request that leaves some new code uncovered still passes if it also covers at
least as many lines or branches that were uncovered before. Reviewers hold the
rest of the target. The job also fails when a count falls below the file's
(`BASELINE LOOSER THAN MEASURED`) and when a crate is missing from the file, so
the file always equals the measurement and never keeps slack for a later change
to spend.

When the job fails because coverage improved, running `kbf-coverage tighten` is
required: the failing job prints the exact command (in its log and its summary),
which downloads the run's `coverage` artifact into a new temporary directory and
rewrites `coverage-baseline` from the measured file in it:

```sh
d=$(mktemp -d) && gh run download <run id> --repo komira-ai/kbf --name coverage --dir "$d" \
  && cargo run -p kbf-coverage -- tighten --measured "$d"/coverage-baseline.measured
```

Commit the rewritten `coverage-baseline` in the same pull request. `tighten` is
not a bypass of the ratchet: it only lowers counts. The rule it keeps for every
value it writes: `tighten` writes no count above the recorded one (a value
recorded as `-`, and each value of a crate the file does not list, counts as
0), never turns lines into `-`, and turns branches into `-` only from 0 and
only while the crate's lines are measured, so a new crate goes in only as
`0 0` or `0 -`. It removes a crate only when its `Cargo.toml` is gone. It
refuses everything else, including a `--root` that holds none of the file's
crates, naming each crate and metric with the recorded and measured values,
and leaves the file untouched: a raise is fixed by covering the code, never by
the file. A failing check prints the `tighten` command only when `tighten`
would accept the run.

Lines `-` is where code hides: a crate that no test binary instruments has no
coverage record at all, so a library with `[lib] test = false` measures `- -`
however much untested code it gains, and a new crate measured `- -` may hold
any amount of untested code. `tighten` refuses both (and a branch count that
becomes `-`, whose missed branches went without being covered). Give the crate
a test target. When a crate legitimately has nothing to measure (all of its
code removed, or an empty skeleton), put `-` in its line by hand in a reviewed
pull request; the refusal names this. That is the one hand edit of
`coverage-baseline`.

`tighten` keeps the file's comments and order, prints each change, and replaces the
file whole. Running the whole command again downloads into another new directory
and changes nothing.

CI measures the pull request merged with `main`. When the branch is behind
`main`, that measurement can name crates the branch does not have (or lack one
it has), and `tighten` refuses it as a measurement of another tree: bring the
branch up to date with `main`, let CI run again, and run the command it prints.
Any other refusal of a measured file prints the same advice as a possibility,
since `main`'s changes can cause it too; a refusal that stays once the branch
is up to date is the branch's to fix.

Without CI, `tighten --lcov <tracefile> --root .` measures from a local
`cargo llvm-cov --branch --lcov` run instead. That run must use the coverage
job's toolchain and flags (see `.github/workflows/ci.yml`): another toolchain
can count fewer items missed, and the file it writes then fails CI as more
missed than recorded, so the mistake fails safe but costs a round trip. A
relative `--root` is resolved against the working directory. Do not
edit `coverage-baseline` by hand, except for the unmeasured crate above.

The ratchet does not see code that the report leaves out, and `tighten` cannot
tell such code from code that is gone; reviewers hold these until repository
lints refuse them:

- A crate moved outside `crates/` (listed in the workspace's `exclude` and
  kept as a `path` dependency) is no longer a workspace crate: it stops being
  measured, and `tighten` removes its line as a deleted crate. The workspace
  has no `exclude` today.
- `cargo llvm-cov`'s default pattern ignores every file in a directory named
  `tests`, `examples` or `benches`, wherever it sits: a module directory
  `src/**/tests/`, or a file in such a directory that `src` includes (for
  example with `#[path]`), is not measured.
- A binary target with `test = false`, `#[cfg(not(coverage))]` and
  `#[coverage(off)]` keep code out of the report while the crate's line stays
  a number.

Changes under `.github/workflows/` will need a review from the code owners
listed in [.github/CODEOWNERS](.github/CODEOWNERS). That rule is a placeholder
today: the owners line is commented out until a maintainers team exists, and
nothing enforces the review yet.

## Reporting security issues

Do not open a public issue or pull request for a vulnerability. Follow
[SECURITY.md](SECURITY.md) instead.
