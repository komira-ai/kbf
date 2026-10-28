//! Where `kbf-vmm image setup` stages the iOS simulator inputs inside the guest, as
//! absolute guest paths. The host side writes them offline into the Data volume (its
//! `stage` module) and the guest side reads them, so both name them from here.

/// The Xcode the setup clones into the guest.
pub const XCODE_APP: &str = "/Applications/Xcode.app";
/// The Xcode's own version file (version and build): what tells one Xcode from another with
/// the same tree sizes.
pub const XCODE_VERSION_PLIST: &str = "/Applications/Xcode.app/Contents/version.plist";
/// The directory holding the simulator runtimes (disk images `*.dmg` and exported bundles
/// `*.exportedBundle`), exactly the ones
/// the latest setup staged.
pub const RUNTIME_DIR: &str = "/private/var/kbf-sim/runtimes";
/// The directory holding app bundles the setup staged for the simulator to install.
pub const APP_DIR: &str = "/private/var/kbf-sim/apps";
/// The directory holding the UI tests the setup staged: the `Build/Products` tree of an
/// `xcodebuild build-for-testing` (one `*.xctestrun` file and the built apps beside it).
pub const TESTS_DIR: &str = "/private/var/kbf-sim/tests";
/// What the setup staged (names, sizes, SHA-256 of each runtime), as JSON.
pub const MANIFEST: &str = "/private/var/kbf-sim/staged.json";
/// Written by `image setup --guest-user <name>`: the name of the user the guest is to have
/// (one line). While it exists and [`USER_READY`] does not, the simulator probe runs nothing
/// (there is no session for it yet): `image prime` creates the user first.
pub const USER_WANTED: &str = "/private/var/kbf-sim/user-wanted";
/// Written by `kbf-guest mkuser` once the user exists with auto-login set up: the name of
/// the user (one line). The simulator probe runs its user steps as this user.
pub const USER_READY: &str = "/private/var/kbf-sim/user-ready";
/// Written by `kbf-guest mkuser` in the base's priming boot once the root setup of the
/// simulator (Xcode selected, license accepted, first launch, every staged runtime imported or
/// added) is done: what was done, as JSON ([`crate::sim_primed`]). `kbf-guest sim` on a clone
/// skips the root steps this record covers.
pub const PRIMED: &str = "/private/var/kbf-sim/primed.json";

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a staged path that is relative, or one moved out of the one directory the
    /// host writes and the guest reads (the host derives its volume-relative paths from
    /// these by dropping the leading slash).
    #[test]
    fn the_staging_paths_are_absolute_and_the_simulator_ones_share_a_directory() {
        for p in [
            XCODE_APP,
            RUNTIME_DIR,
            APP_DIR,
            TESTS_DIR,
            MANIFEST,
            USER_WANTED,
            USER_READY,
            PRIMED,
        ] {
            assert!(p.starts_with('/'), "{p}");
            assert!(!p.ends_with('/'), "{p}");
        }
        for p in [
            RUNTIME_DIR,
            APP_DIR,
            TESTS_DIR,
            MANIFEST,
            USER_WANTED,
            USER_READY,
            PRIMED,
        ] {
            assert!(p.starts_with("/private/var/kbf-sim/"), "{p}");
        }
    }
}
