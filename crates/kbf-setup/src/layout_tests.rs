use kbf_host::path::Root;
use kbf_testkit::dir::TestDir;

use super::{Layout, Platform};

const NEW_DIR: &str = "usr/share/kbf/trust";
const OLD_DIR: &str = "usr/share/kbf-installer/trust";

/// A host root of this test's own, holding the trust directories named in `dirs`.
struct Host(TestDir);

impl Host {
    fn with(name: &str, dirs: &[&str]) -> Host {
        let beside = std::env::current_exe().expect("test binary");
        let root = TestDir::under(
            beside.parent().expect("deps").join("kbf-setup-layout-unit"),
            name,
        );
        for dir in dirs {
            std::fs::create_dir_all(root.join(dir)).expect("trust directory");
        }
        Host(root)
    }

    fn layout(&self, platform: Platform) -> Layout {
        Layout::new(
            Root::new(self.0.path().to_path_buf()).expect("absolute root"),
            platform,
        )
    }
}

/// A layout names the trust files the `kbf` package carries, whether or not the host has
/// them: nothing about the file system is read unless the fallback is asked for.
#[test]
fn the_trust_files_are_the_kbf_packages() {
    let l = Host::with("plain", &[OLD_DIR]).layout(Platform::Linux);
    assert_eq!(l.trust_dir().as_str(), "/usr/share/kbf/trust");
    assert_eq!(l.gh().as_str(), "/usr/share/kbf/trust/gh");
    assert_eq!(
        l.trusted_root().as_str(),
        "/usr/share/kbf/trust/trusted_root.jsonl"
    );
    assert_eq!(l.settings().as_str(), "/usr/share/kbf/trust/installer.conf");
}

/// A host that only has the retired package's directory is read there, all three files.
#[test]
fn the_old_directory_is_read_when_the_new_one_is_absent() {
    let l = Host::with("old-only", &[OLD_DIR])
        .layout(Platform::Linux)
        .with_trust_fallback();
    assert_eq!(l.trust_dir().as_str(), "/usr/share/kbf-installer/trust");
    assert_eq!(l.gh().as_str(), "/usr/share/kbf-installer/trust/gh");
    assert_eq!(
        l.trusted_root().as_str(),
        "/usr/share/kbf-installer/trust/trusted_root.jsonl"
    );
    assert_eq!(
        l.settings().as_str(),
        "/usr/share/kbf-installer/trust/installer.conf"
    );
}

/// The old directory is not consulted once the new one exists, even if both are there.
#[test]
fn the_old_directory_is_not_read_when_the_new_one_is_there() {
    for (name, dirs) in [
        ("both", &[NEW_DIR, OLD_DIR][..]),
        ("new-only", &[NEW_DIR][..]),
    ] {
        let l = Host::with(name, dirs)
            .layout(Platform::Linux)
            .with_trust_fallback();
        assert_eq!(l.gh().as_str(), "/usr/share/kbf/trust/gh", "{name}");
    }
}

/// A host with neither (a fresh one, before the package is installed) names the new path, so
/// the refusal a missing file gets names the file a package would put there.
#[test]
fn a_host_with_neither_directory_names_the_new_one() {
    let l = Host::with("neither", &[])
        .layout(Platform::Linux)
        .with_trust_fallback();
    assert_eq!(l.gh().as_str(), "/usr/share/kbf/trust/gh");
}

/// A Mac has no fallback: its trust directory is the pkg's, whatever a Linux path holds.
#[test]
fn a_mac_never_falls_back() {
    let l = Host::with("mac", &[OLD_DIR])
        .layout(Platform::MacOs)
        .with_trust_fallback();
    assert_eq!(l.gh().as_str(), "/usr/local/kbf-installer/trust/gh");
}
