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
to spend. In either case copy `coverage-baseline.measured` from the job's
`coverage` artifact over `coverage-baseline` in the same pull request.

Changes under `.github/workflows/` will need a review from the code owners
listed in [.github/CODEOWNERS](.github/CODEOWNERS). That rule is a placeholder
today: the owners line is commented out until a maintainers team exists, and
nothing enforces the review yet.

## Reporting security issues

Do not open a public issue or pull request for a vulnerability. Follow
[SECURITY.md](SECURITY.md) instead.
