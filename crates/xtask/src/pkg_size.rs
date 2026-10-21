//! The most a `kbf-installer` package that carries a kbf release may weigh, by platform.
//!
//! A package with a release in it is about 33 to 43 MB: the release's tarball is stored byte
//! for byte beside the installer and the pinned `gh`. The packages stay gzip (zstd is not read
//! by every dpkg a farm runs), so the size grows only with what goes in. A budget makes that
//! growth a decision: a change that grows the package past it raises the number in the same
//! change, where a reviewer sees both.
//!
//! This module holds the budget and the check; `pkg-asset` calls the check on a package once the
//! package carries a release. Until then no package is held to it.

use crate::package::Platform;

/// The budget of each platform that has a Linux package, in bytes.
pub const BUDGETS: [(Platform, u64); 2] = [(Platform::LinuxX86_64, 0), (Platform::LinuxArm64, 0)];

/// The budget of `platform`: `None` for a platform whose package carries no release yet.
pub fn budget(_platform: Platform) -> Option<u64> {
    None
}

/// What is wrong with a package of `bytes` for `platform`: `None` when it is within its budget
/// or has none.
pub fn problem(_platform: Platform, _bytes: u64) -> Option<String> {
    None
}
