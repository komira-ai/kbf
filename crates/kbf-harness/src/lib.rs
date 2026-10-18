//! The beta harness as a library (docs/design/kbf-cicd.md, sections 4.2 and 4.3), grown
//! from the rehearsal in a box: one set of parts that the rehearsal tests of `kbf-cli`
//! and the beta scenarios share.
//!
//! - [`farm`]: a frontend, storage nodes (each with a daemon in its process, or alone) and
//!   plain daemons, each a real `kbf` process started from the `ExecStart=` line
//!   `kbf-deploy` renders for its unit
//!   ([`units`]), on a loopback address of its own, under certificates of the right
//!   rows ([`pki`]); a supervisor with `term`, `kill`, `restart`, a process table and a
//!   report of every process's output for a hang ([`procs`]). Optionally an Envoy in
//!   front of the frontend ([`envoy`]).
//! - [`scrape`] and [`gates`]: the frontend's metrics read every second, and the gates a
//!   scenario applies to what it measured.
//! - [`load`] and [`eventlog`]: the Buck2 builds that load the farm, and what their event
//!   logs say of each action.
//! - [`result`]: the result document, `beta-result.json`, and the job summary's table.
//! - [`b0`]: scenario B0, `farm_under_build_load`, and [`cli`], the command line of the
//!   `kbf-beta` binary that runs it.
//!
//! To run B0 by hand, with the binaries built (`cargo build -p kbf-cli -p kbf-harness -p kbf-it
//! --bins`), a pinned `buck2` and, for the Envoy in front of the frontend, `KBF_TEST_ENVOY` naming an
//! Envoy binary (the beta workflow fetches both by checksum):
//!
//! ```text
//! kbf-beta b0 --bin-dir target/debug --buck2 <buck2> --project crates/kbf-it/beta/buck2 \
//!     --work <a directory that does not exist yet> --duration-secs 60 --concurrency 2
//! ```
//!
//! writes `<work>/beta-result.json` and the logs in `<work>/logs`, and exits 0 only if every
//! gate passed. `--mutant` makes the farm worse in one way each gate exists to catch.
//!
//! The library spawns the binaries it is given by path and links neither `kbf-server`
//! nor `kbf-node` today (tests/layering.rs fails a crate outside its joiners that links
//! both): `kbf` is a process here, not a dependency.

#![cfg(target_os = "linux")]

pub mod api;
pub mod b0;
pub mod cli;
pub mod envoy;
pub mod eventlog;
pub mod farm;
pub mod gates;
pub mod load;
pub mod node_support;
pub mod pki;
pub mod procs;
pub mod result;
pub mod scrape;
pub mod units;

/// A fresh directory for one unit test, under the temporary directory, named for the test
/// (`CARGO_TARGET_TMPDIR` is for integration tests only).
#[cfg(test)]
pub(crate) fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir()
        .join("kbf-harness-tests")
        .join(format!("{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}
