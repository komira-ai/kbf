//! The stand-in `buck2` of the tests of scenario B0, which is a binary of this
//! package (`kbf-harness-fake-buck2`). A test of another package
//! (kbf-cli's, which has the `kbf` binary B0 needs) cannot name it with
//! `CARGO_BIN_EXE_*`, which only the package that owns a binary may; it finds it beside
//! its own test binary, where `cargo test --workspace` (and `cargo build -p kbf-harness
//! --bins`) puts it. It is not a binary of `kbf-cli`, so it is not built into a release.

use std::path::{Path, PathBuf};

/// The stand-in `buck2`.
pub const FAKE_BUCK2: &str = "kbf-harness-fake-buck2";

/// The binary `name` built in the same profile as the running test binary.
///
/// # Panics
/// When it was not built, saying how to build it.
#[must_use]
pub fn sibling_bin(name: &str) -> PathBuf {
    let exe = std::env::current_exe().expect("the running test binary");
    sibling_of(&exe, name)
}

/// The binary `name` in the nearest directory above `exe` that holds it: the profile
/// directory (`target/debug`), which is two above a test binary in `deps/` and further above
/// one in the nested layout (`target/debug/build/<package>/<hash>/deps/`) that a newer cargo,
/// and the coverage job's, uses.
///
/// # Panics
/// When there is no such file.
#[must_use]
pub fn sibling_of(exe: &Path, name: &str) -> PathBuf {
    let found = exe
        .ancestors()
        .skip(1)
        .map(|dir| dir.join(name))
        .find(|path| path.is_file());
    found.unwrap_or_else(|| {
        panic!(
            "{name} is not built above {}: `cargo test --workspace` builds it, or `cargo build -p kbf-harness --bins`",
            exe.display()
        )
    })
}

#[cfg(test)]
#[path = "stand_in_tests.rs"]
mod tests;
