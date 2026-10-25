//! The most a `kbf-installer` package that carries a kbf release may weigh, by platform.
//!
//! A package with a release in it is about 33 to 43 MB: the release's tarball is stored byte
//! for byte beside the installer and the pinned `gh`. The packages stay gzip (zstd is not read
//! by every dpkg a farm runs), so the size grows only with what goes in. A budget makes that
//! growth a decision: a change that grows the package past it raises the number in the same
//! change, where a reviewer sees both.
//!
//! The numbers below are the top of that range with a little room, not a measurement: no
//! package carries a release yet. The change that first builds one replaces them with its
//! measured size and a margin, and `pkg-kbf-asset` calls [`problem`] on the package then. Until
//! then no package is held to it.

use crate::package::Platform;

/// A megabyte as the sizes of the packages are quoted: a million bytes.
const MB: u64 = 1_000_000;

/// The budget of each platform that has a Linux package, in bytes.
pub const BUDGETS: [(Platform, u64); 2] = [
    (Platform::LinuxX86_64, 45 * MB),
    (Platform::LinuxArm64, 45 * MB),
];

/// The budget of `platform`: `None` for a platform whose package carries no release yet.
pub fn budget(platform: Platform) -> Option<u64> {
    BUDGETS
        .iter()
        .find(|(p, _)| *p == platform)
        .map(|(_, bytes)| *bytes)
}

/// What is wrong with a package of `bytes` for `platform`: `None` when it is within its budget
/// or has none.
pub fn problem(platform: Platform, bytes: u64) -> Option<String> {
    let limit = budget(platform)?;
    (bytes > limit).then(|| {
        format!(
            "the {} package is {bytes} bytes, over its budget of {limit} bytes: a package \
             that grows raises the budget (crates/xtask/src/pkg_size.rs) in the same change",
            platform.name()
        )
    })
}
