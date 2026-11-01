//! Programs for kbf's tests to run in place of shell scripts: the `act` verbs, the fakes
//! of `xcrun` and friends, the test database, scratch directories, ports and the rest.
//!
//! The code now lives in the `js-testkit` crate of the `komira-ai/jobscheduler`
//! repository (`crates/js-testkit`), at the revision pinned in the workspace `Cargo.toml`;
//! this crate re-exports it under its old name so that every path (`kbf_testkit::dir`,
//! `kbf_testkit::act`, the `act_test_hook!` macro, ...) stays as it was. The tests of the
//! testkit run in that repository.
//!
//! The one program kept here is the `kbf-act` bin, a thin main over `act`.
#![forbid(unsafe_code)]

pub use js_testkit::*;
