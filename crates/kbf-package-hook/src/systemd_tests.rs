//! The real host's default programs, read through its private fields; no host utility is run.

use std::path::Path;

use crate::systemd::Systemd;

/// Asserts that `Systemd::new` and `Systemd::default` both name `/usr/bin/systemctl` and
/// `/usr/bin/kbf`, and that `with_programs` keeps the paths it is given.
#[test]
fn the_default_host_uses_the_system_programs() {
    for host in [Systemd::new(), Systemd::default()] {
        assert_eq!(host.systemctl, Path::new("/usr/bin/systemctl"));
        assert_eq!(host.kbf, Path::new("/usr/bin/kbf"));
    }
    let other = Systemd::with_programs("/a/systemctl".into(), "/a/kbf".into());
    assert_eq!(other.systemctl, Path::new("/a/systemctl"));
    assert_eq!(other.kbf, Path::new("/a/kbf"));
}
