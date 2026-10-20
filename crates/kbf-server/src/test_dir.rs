//! The unit tests' scratch directories: one place, owned and removed by the test.

use kbf_testkit::dir::TestDir;

/// A new directory `<name>-<pid>-<n>` under `kbf-server-unit` beside the test binary
/// (unit tests get no `CARGO_TARGET_TMPDIR`), removed when it drops. Bind it before
/// anything the test starts that writes into it, so that is gone first.
pub(crate) fn scratch_dir(name: &str) -> TestDir {
    let beside = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps directory")
        .join("kbf-server-unit");
    TestDir::under(beside, name)
}
