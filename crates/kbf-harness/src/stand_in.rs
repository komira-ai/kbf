//! The stand-in `buck2` and Envoy of the tests of scenario B0, which are binaries of this
//! package (`kbf-harness-fake-buck2`, `kbf-harness-fake-envoy`). A test of another package
//! (kbf-cli's, which has the `kbf` binary B0 needs) cannot name them with
//! `CARGO_BIN_EXE_*`, which only the package that owns a binary may; it finds them beside
//! its own test binary, where `cargo test --workspace` (and `cargo build -p kbf-harness
//! --bins`) puts them. Neither is a binary of `kbf-cli`, so neither is built into a release.

use std::path::{Path, PathBuf};

/// The stand-in `buck2`.
pub const FAKE_BUCK2: &str = "kbf-harness-fake-buck2";

/// The stand-in Envoy.
pub const FAKE_ENVOY: &str = "kbf-harness-fake-envoy";

/// The binary `name` built in the same profile as the running test binary.
///
/// # Panics
/// When it was not built, saying how to build it.
#[must_use]
pub fn sibling_bin(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("the running test binary");
    sibling_of(&exe, name)
}

/// The binary `name` in the profile directory (`target/debug`) that holds `exe`'s directory
/// (`target/debug/deps`).
///
/// # Panics
/// When there is no such file.
#[must_use]
pub fn sibling_of(exe: &Path, name: &str) -> PathBuf {
    let profile = exe.parent().and_then(Path::parent).unwrap_or(exe);
    let path = profile.join(name);
    assert!(
        path.is_file(),
        "{} is not built: `cargo test --workspace` builds it, or `cargo build -p kbf-harness --bins`",
        path.display()
    );
    path
}

#[cfg(test)]
#[path = "stand_in_tests.rs"]
mod tests;
