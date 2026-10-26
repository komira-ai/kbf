//! Where `kbf-vmm image setup` stages the iOS simulator inputs inside the guest, as
//! absolute guest paths. The host side writes them offline into the Data volume (its
//! `stage` module) and the guest side reads them, so both name them from here.

/// The Xcode the setup clones into the guest.
pub const XCODE_APP: &str = "/Applications/Xcode.app";
/// The directory holding the simulator runtime disk images (`*.dmg`), exactly the ones
/// the latest setup staged.
pub const RUNTIME_DIR: &str = "/private/var/kbf-sim/runtimes";
/// The directory holding app bundles the setup staged for the simulator to install.
pub const APP_DIR: &str = "/private/var/kbf-sim/apps";
/// What the setup staged (names, sizes, SHA-256 of each runtime), as JSON.
pub const MANIFEST: &str = "/private/var/kbf-sim/staged.json";

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches a staged path that is relative, or one moved out of the one directory the
    /// host writes and the guest reads (the host derives its volume-relative paths from
    /// these by dropping the leading slash).
    #[test]
    fn the_staging_paths_are_absolute_and_the_simulator_ones_share_a_directory() {
        for p in [XCODE_APP, RUNTIME_DIR, APP_DIR, MANIFEST] {
            assert!(p.starts_with('/'), "{p}");
            assert!(!p.ends_with('/'), "{p}");
        }
        for p in [RUNTIME_DIR, APP_DIR, MANIFEST] {
            assert!(p.starts_with("/private/var/kbf-sim/"), "{p}");
        }
    }
}
