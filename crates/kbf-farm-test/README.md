# kbf-farm-test

The black-box suite of the farm (a frontend, storage-and-daemon nodes and a plain daemon), a beta stage test (design: T009). One crate per
component under test, named for it (`kbf-daemon-test` is the daemon's). Tests only: the crate has no
library and no binary, and every dependency is a dev-dependency. A test starts the real
`kbf` processes through `kbf-harness` and talks to them as a deployer's client would
(REAPI, the operator API, the command line); it links no `kbf-server` or `kbf-node` code.
Its REAPI client (`kbf_harness::reapi`), the farm (`kbf_harness::farm`) and the action and
input-tree builders (`kbf_testkit::tree`) are shared with the other black-box crate,
`kbf-daemon-test`, and live in `kbf-harness` and `kbf-testkit`, not here.

## The farm, end to end

`tests/farm.rs` (test binary `kbf-farm-test::farm`) holds the farm suite: a farm of
rendered units (a frontend, storage-and-daemon nodes, a plain daemon) driven end to end.
Today it holds one test, `a_storage_node_takes_a_lease_and_runs_an_action`: an `Execute`
reaches a node's daemon half, the action runs under the stand-in `crun`, and its output is
read back through the frontend. It is the proof that the wiring works; the planned-restart
test of `kbf-cli/tests/rehearsal_in_a_box.rs` moves here next.

## How to add a test

1. Put it in `tests/farm.rs` (a new suite is a new file in `tests/`, named for the
   suite). Start the farm with `start(name, topology)`, wait with `ready`, drive it with
   the helpers of `kbf-harness`, and `farm.stop()` at the end; a failed `Farm::start` panics
   with the processes' last lines.
2. Say in the test's doc comment which defect it catches, and plant that defect once to see
   it red.
3. Keep the file under 1000 lines; a second suite goes in its own file.

## Where it runs: beta, not the pull-request gate

- `.config/nextest.toml`: the `pr` profile's `default-filter` excludes `package(kbf-farm-test)`;
  the default profile (the whole run of `.github/workflows/beta.yml`, job `test-x86`) runs it.
- Coverage: the pull-request job measures every crate at 99% under `cargo llvm-cov`. The
  test files here are `#![cfg(not(coverage))]`, so that job compiles them empty and the
  crate has no records (an unlisted crate with none passes); a normal build and the beta run
  compile them whole. This crate needs no line in `coverage-floors`.
- The processes are found beside the test binary, so build them first when running one
  package: `cargo build -p kbf-cli --bins` (`kbf` and the stand-in `crun`). `cargo test
  --workspace` and the beta run build them already.
- Linux only: `kbf-harness` is.
