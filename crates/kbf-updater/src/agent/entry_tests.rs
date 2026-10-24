//! The binary's entry for the agent mode, and the status the heartbeats say.

use kbf_types::ReleasePlatform;

use super::status::{Shared, Status, host_platform, platform_of, version_of_commit};
use super::testing::{VA, version};
use crate::wire::Health;
use crate::{run, run_agent_with};

fn args(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

/// `kbf-updater agent` with flags that are wrong is a usage error (exit 2, the usage on the error
/// stream) and starts nothing; with flags that are right and a certificate that cannot be used it
/// is exit 1 and the reason. The roll mode's own flags are not the agent's.
#[test]
fn the_agent_mode_is_a_usage_error_or_a_start_that_fails() {
    for words in [
        &["kbf-updater", "agent"][..],
        &["kbf-updater", "agent", "--bogus", "1"],
        &[
            "kbf-updater",
            "agent",
            "--state-dir",
            "/var/lib/kbf-updater",
        ],
    ] {
        let (code, text) = run(&args(words));
        assert_eq!(code, 2, "{words:?}");
        assert!(text.contains("usage: kbf-updater agent"), "{text}");
        let (code, _) = run_agent_with(&args(words), |_| unreachable!("not started"));
        assert_eq!(code, 2);
    }
    let (code, text) = run(&args(&[
        "kbf-updater",
        "agent",
        "--coordinator=c.example:443",
        "--client-cert=/no/such/cert.pem",
        "--client-key=/no/such/key.pem",
        "--ca=/no/such/ca.pem",
        "--channel=stable",
        "--operator-api=http://127.0.0.1:18002",
        "--token-file=/no/such/token",
    ]));
    assert_eq!(code, 1);
    assert!(text.contains("/no/such/cert.pem"), "{text}");
}

/// The platform of this host is one kbf has releases for, named as the wire names it; the
/// heartbeat a status makes says exactly the status.
#[test]
fn the_status_is_the_heartbeat() {
    let platform = host_platform().unwrap();
    assert!(ReleasePlatform::ALL.contains(&platform));
    let status = Status::unknown(platform, version("0"));
    assert_eq!(status.health, Health::Unhealthy);
    let beat = status.heartbeat();
    assert_eq!(
        (beat.platform, beat.version.as_str(), beat.health),
        (platform, "0", Health::Unhealthy)
    );
    assert_eq!((beat.last_update, beat.updating), (None, None));
}

/// The shared status is read and changed from both threads; a thread that died holding it leaves
/// a status that is still read, not a host that can never heartbeat again.
#[test]
fn the_shared_status_survives_a_thread_that_died_holding_it() {
    let shared = Shared::new(Status::unknown(ReleasePlatform::LinuxX86_64, version("0")));
    shared.update(|s| s.version = version(VA));
    let poisoner = shared.clone();
    let died = std::thread::spawn(move || {
        poisoner.update(|_| panic!("a thread dies holding the status"));
    })
    .join();
    assert!(died.is_err());
    assert_eq!(shared.get().version, version(VA));
    shared.update(|s| s.health = Health::Healthy);
    assert_eq!(shared.get().health, Health::Healthy);
}

/// A release the host was never told the version of is named by its commit: twelve digits in the
/// form a version carries, which the health check reads the commit back out of; a commit that is
/// too short names nothing.
#[test]
fn a_release_without_a_version_is_named_by_its_commit() {
    let named = version_of_commit(&"ab".repeat(20)).unwrap();
    assert_eq!(named.as_str(), "0.1.0+gabababababab");
    assert_eq!(named.commit(), Some("abababababab"));
    assert!(version_of_commit("abc").is_none());
    assert!(version_of_commit("").is_none());
    assert!(version_of_commit(&"/".repeat(40)).is_none());
}

/// The platform is named from Rust's operating system and CPU, as the releases name theirs: a Mac
/// on Apple silicon and Linux on either CPU have one; anything else has none.
#[test]
fn the_platform_is_named_as_the_releases_name_it() {
    assert_eq!(
        platform_of("linux", "x86_64"),
        Some(ReleasePlatform::LinuxX86_64)
    );
    assert_eq!(
        platform_of("linux", "aarch64"),
        Some(ReleasePlatform::LinuxArm64)
    );
    assert_eq!(
        platform_of("macos", "aarch64"),
        Some(ReleasePlatform::DarwinArm64)
    );
    for (os, arch) in [
        ("macos", "x86_64"),
        ("windows", "x86_64"),
        ("linux", "riscv64"),
    ] {
        assert_eq!(platform_of(os, arch), None, "{os} {arch}");
    }
}
