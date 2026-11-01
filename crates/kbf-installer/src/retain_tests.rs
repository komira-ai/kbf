//! Where `retain.json` is on a host with the installer's layout.

use super::*;
use crate::layout::Platform;
use crate::testkit::host_root_on;

/// Catches the file living somewhere a release of the other platform would not look for it:
/// it is `retain.json` in the state directory, beside `state.json`, on each platform.
///
/// Catches: `path` joining the releases directory or the root.
#[test]
fn the_file_is_in_the_state_directory_on_both_platforms() {
    for platform in [Platform::Linux, Platform::MacOs] {
        let (layout, root) = host_root_on("retain-path", platform);
        let path = path(&layout);
        let state_dir = layout.join(&layout.state_dir());
        assert_eq!(path, state_dir.join("retain.json"), "{platform:?}");
        assert!(path.starts_with(&root));
    }
}
