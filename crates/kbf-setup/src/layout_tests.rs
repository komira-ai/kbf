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

/// The default tree is `kbf-installer`, as it always was, on both platforms.
#[test]
fn the_default_tree_is_the_installers_own() {
    let linux = Host::with("tree-default-linux", &[]).layout(Platform::Linux);
    assert_eq!(linux.install_root().as_str(), "/var/lib/kbf-installer");
    assert_eq!(linux.release_programs(), ["kbf", "kbf-daemon"]);
    assert_eq!(linux.releases().as_str(), "/var/lib/kbf-installer/releases");
    assert_eq!(
        linux.state_file().as_str(),
        "/var/lib/kbf-installer/state.json"
    );
    assert_eq!(linux.lock().as_str(), "/run/kbf-installer/lock");
    let mac = Host::with("tree-default-mac", &[]).layout(Platform::MacOs);
    assert_eq!(mac.install_root().as_str(), "/usr/local/kbf-installer");
    assert_eq!(
        mac.state_file().as_str(),
        "/var/db/kbf-installer/state.json"
    );
    assert_eq!(mac.lock().as_str(), "/var/run/kbf-installer/lock");
}

/// Another tree moves the releases, the links, the state, the scratch and the lock, and
/// nothing else: the trust directory stays the host's.
#[test]
fn another_tree_moves_the_releases_the_state_and_the_lock_but_not_the_trust_files() {
    let commit = crate::CommitId::new(&"a".repeat(40)).unwrap();
    let linux = Host::with("tree-linux", &[])
        .layout(Platform::Linux)
        .with_tree("js-agent", "js-agent-bin")
        .unwrap();
    assert_eq!(
        linux.current().as_str(),
        "/var/lib/js-agent/releases/current"
    );
    assert_eq!(
        linux.previous().as_str(),
        "/var/lib/js-agent/releases/previous"
    );
    assert_eq!(
        linux.release_dir(&commit).as_str(),
        format!("/var/lib/js-agent/releases/{commit}")
    );
    assert_eq!(
        linux.stage_dir(&commit, "x1").as_str(),
        format!("/var/lib/js-agent/releases/.stage-{commit}.x1")
    );
    assert_eq!(linux.state_file().as_str(), "/var/lib/js-agent/state.json");
    assert_eq!(
        linux.state_backup().as_str(),
        "/var/lib/js-agent/state.json.bak"
    );
    assert_eq!(linux.orders_dir().as_str(), "/var/lib/js-agent/orders");
    assert_eq!(linux.work_dir().as_str(), "/var/lib/js-agent/work");
    assert_eq!(linux.seq_floor().as_str(), "/var/lib/js-agent/seq_floor");
    assert_eq!(linux.lock().as_str(), "/run/js-agent/lock");
    assert_eq!(linux.gh().as_str(), "/usr/share/kbf/trust/gh");
    assert_eq!(
        linux.release_file(&commit, "js-agent-bin").as_str(),
        format!("/var/lib/js-agent/releases/{commit}/js-agent-bin")
    );
    assert_eq!(linux.release_programs(), ["js-agent-bin"]);
    let mac = Host::with("tree-mac", &[])
        .layout(Platform::MacOs)
        .with_tree("js-agent", "js-agent-bin")
        .unwrap();
    assert_eq!(mac.install_root().as_str(), "/usr/local/js-agent");
    assert_eq!(mac.state_file().as_str(), "/var/db/js-agent/state.json");
    assert_eq!(mac.lock().as_str(), "/var/run/js-agent/lock");
}

/// A tree name that could leave its directory, or is empty, long or upper-case, is refused.
#[test]
fn a_tree_name_that_is_not_a_plain_lower_case_name_is_refused() {
    let layout = || Host::with("tree-bad", &[]).layout(Platform::Linux);
    for bad in [
        "",
        "-a",
        "1a",
        "A",
        "a/b",
        "..",
        "a b",
        "a_b",
        &"a".repeat(65),
    ] {
        let err = layout().with_tree(bad, "prog").unwrap_err();
        assert_eq!(err, super::NotATree(bad.to_owned()), "{bad:?}");
        assert!(
            err.to_string().contains("not usable in a layout"),
            "{bad:?}"
        );
    }
    assert!(layout().with_tree(&"a".repeat(64), "prog").is_ok());
    // The program is a plain file name too.
    for bad in ["", ".x", "-x", "a/b", "..", "a b", "a\u{0}"] {
        let err = layout().with_tree("ok", bad).unwrap_err();
        assert_eq!(err, super::NotATree(bad.to_owned()), "{bad:?}");
    }
}
