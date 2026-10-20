//! The two isolation suites of the black-box crate (crates/kbf-daemon-test/README.md):
//! `no_residue` (nothing is left behind on a node after real builds) and `no_cross_access`
//! (one action's data is not reachable by another on the same node).
//!
//! One test binary so the shared helpers (`common`) are compiled, and checked for dead code,
//! once. Linux only, like the farm; not under coverage (the pull-request gate does not run
//! this crate, `.config/nextest.toml`).
//!
//! READ THIS BEFORE TRUSTING A GREEN RUN. The farm's `crun` is a stand-in
//! (crates/kbf-driver-container/tests/bin/fake_crun.rs): it makes no namespace, no overlay and
//! no cgroup, and runs every action as a plain process of the test's user. So these suites
//! prove the PRODUCT's bookkeeping and layout (what `kbf-daemon` creates, cleans, hands over
//! and collects), never the kernel's isolation. The checks that need real isolation are
//! `#[ignore]`d tier-R tests that fail loudly when run on the stand-in.

#![cfg(all(target_os = "linux", not(coverage)))]

kbf_testkit::act_test_hook!();

mod common;
mod no_cross_access;
mod no_residue;
