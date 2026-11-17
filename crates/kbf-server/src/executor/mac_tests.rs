//! The Macs: which actions ask for one, the pool's rules, and the parts of a Mac job.

use std::time::Duration;

use js_proto::v1::{ExecMode, Isolation, MountMode, MountPropagation, mount};
use kbf_proto::reapi::{Digest, Platform, platform::Property};
use kbf_types::LeaseId;

use super::{
    DEFAULT_MAC_OS_VERSION, MAC_ARCH, MAC_OS, MAC_OUT_TARGET, MacPool, MacPoolError, System,
    TREE_TARGET, check_version, mac_argv, mac_constraints,
};
use crate::executor::PutDirRoots;

fn platform(properties: &[(&str, &str)]) -> Platform {
    Platform {
        properties: properties
            .iter()
            .map(|(name, value)| Property {
                name: (*name).to_owned(),
                value: (*value).to_owned(),
            })
            .collect(),
    }
}

fn of(properties: &[(&str, &str)]) -> Result<System, String> {
    System::of(Some(&platform(properties)))
}

fn pool() -> MacPool {
    MacPool {
        hosts: 2,
        bundle: format!("kbf-task@sha256:{}", "9a".repeat(32)),
        os_version: DEFAULT_MAC_OS_VERSION,
        roots: None,
    }
}

/// Asserts that an action asks for a Mac exactly when the farm would place it on one: every
/// spelling of `OSFamily` the farm reads as macOS and `os=macos`, in any case of the name; and
/// that no platform, an empty one, a Linux one and one that names only an architecture are
/// Linux. Catches a reader that knows fewer spellings than the farm, or takes any `os` for Linux.
#[test]
fn an_action_asks_for_a_mac_as_the_farm_reads_its_platform() {
    let mac = System::Macos { os_version: None };
    for spelling in [
        &[("OSFamily", "macos")][..],
        &[("OSFamily", "Darwin")],
        &[("osfamily", "MacOSX")],
        &[("OSFamily", "osx")],
        &[("os", "macos")],
        &[("OSFamily", "macos"), ("container-image", "docker://x")],
    ] {
        assert_eq!(of(spelling), Ok(mac), "{spelling:?}");
    }
    assert_eq!(System::of(None), Ok(System::Linux));
    assert_eq!(of(&[]), Ok(System::Linux));
    assert_eq!(of(&[("OSFamily", "linux")]), Ok(System::Linux));
    assert_eq!(of(&[("ISA", "arm64")]), Ok(System::Linux));
    assert_eq!(System::default(), System::Linux);
}

/// Asserts the `os_version` of a Mac action is its least major version, whatever the minor and
/// patch and the case of the name; that a Linux action's is not read, even one that is no
/// version; and that a Mac action's that is no version is refused, naming it.
#[test]
fn a_mac_actions_os_version_is_its_least_major_version() {
    for (value, major) in [("26", 26), ("26.1", 26), ("15.0.1", 15), ("0", 0)] {
        assert_eq!(
            of(&[("OSFamily", "macos"), ("os_version", value)]),
            Ok(System::Macos {
                os_version: Some(major)
            }),
            "{value}"
        );
    }
    assert_eq!(
        of(&[("OSFamily", "macos"), ("OS_Version", "27")]),
        Ok(System::Macos {
            os_version: Some(27)
        })
    );
    assert_eq!(
        of(&[("OSFamily", "linux"), ("os_version", "jammy")]),
        Ok(System::Linux)
    );
    for bad in [
        "",
        "x",
        "26.",
        ".1",
        "26.1.2.999",
        "26-beta",
        "+26",
        "99999999999",
    ] {
        assert_eq!(
            of(&[("OSFamily", "macos"), ("os_version", bad)]),
            Err(format!(
                "the action's os_version {bad:?} is not a version such as 26 or 26.1"
            )),
            "{bad:?}"
        );
    }
}

/// Asserts that a platform the farm refuses is refused here with the farm's reason: an `os` no
/// worker runs, and `OSFamily` beside `os`.
#[test]
fn a_platform_the_farm_refuses_is_refused() {
    for bad in [
        &[("os", "windows")][..],
        &[("OSFamily", "macos"), ("os", "macos")],
    ] {
        let farm = kbf_caps::Request::from_platform(bad.iter().copied())
            .expect_err("the farm refuses it")
            .to_string();
        assert_eq!(of(bad), Err(farm), "{bad:?}");
    }
}

/// Asserts the pool's rules, each by its own error: a pool of no worker, a task bundle that is
/// not pinned by a digest, and a least macOS of 0; and that a pool's roots are its
/// own when it names them, else the Linux ones.
#[test]
fn a_pool_is_checked_and_its_roots_default_to_the_linux_ones() {
    assert_eq!(pool().validate(), Ok(()));
    let none = MacPool { hosts: 0, ..pool() };
    assert_eq!(none.validate(), Err(MacPoolError::NoHosts));
    let short = format!("kbf-task@sha256:{}", "9a".repeat(31));
    let upper = format!("kbf-task@sha256:{}", "9A".repeat(32));
    let nameless = format!("@sha256:{}", "9a".repeat(32));
    for bundle in ["kbf-task", "", "kbf-task:latest", &short, &upper, &nameless] {
        let bad = MacPool {
            bundle: bundle.to_owned(),
            ..pool()
        };
        assert_eq!(
            bad.validate(),
            Err(MacPoolError::Bundle(bundle.to_owned())),
            "{bundle:?}"
        );
    }
    let zero = MacPool {
        os_version: 0,
        ..pool()
    };
    assert_eq!(zero.validate(), Err(MacPoolError::OsVersion));
    assert_eq!(
        MacPoolError::NoHosts.to_string(),
        "a pool of Macs needs at least one virtual worker"
    );
    assert_eq!(
        MacPoolError::Bundle("kbf-task".to_owned()).to_string(),
        "the Macs' task bundle \"kbf-task\" is not pinned by an @sha256 digest (NAME@sha256:HEX)"
    );
    assert_eq!(
        MacPoolError::OsVersion.to_string(),
        "the least macOS of the Macs must be a major version above 0"
    );
    let linux = PutDirRoots::new("/mnt/ts/put").expect("roots");
    assert_eq!(pool().roots(&linux), &linux);
    let own = PutDirRoots::new("/Volumes/ts/put").expect("roots");
    let named = MacPool {
        roots: Some(own.clone()),
        ..pool()
    };
    assert_eq!(named.roots(&linux), &own);
}

/// Asserts the at-least rule of `os_version`: an action that names none, the pool's version or
/// an older one runs; one that asks for a newer one is refused, naming both.
#[test]
fn an_action_that_asks_for_a_newer_macos_than_the_pool_runs_is_refused() {
    let pool = pool();
    for asked in [None, Some(14), Some(26)] {
        assert_eq!(check_version(asked, &pool), Ok(()), "{asked:?}");
    }
    assert_eq!(
        check_version(Some(27), &pool),
        Err(
            "the action asks for macOS 27 or newer (os_version), and this executor's Macs are \
             sure to run only macOS 26 or newer (--executor-mac-os-version)"
                .to_owned()
        )
    );
}

/// Asserts what a Mac job asks of its host: `os=macos`, `arch=arm64`, the native runtime, and
/// nothing else.
#[test]
fn a_mac_job_asks_for_a_mac_in_the_native_runtime() {
    let constraints = mac_constraints();
    assert_eq!(constraints.os, MAC_OS);
    assert_eq!(constraints.arch, MAC_ARCH);
    assert_eq!(constraints.exec(), ExecMode::SandboxExec);
    assert_eq!(constraints.isolation(), Isolation::Unspecified);
    assert!(constraints.isa_level.is_empty() && constraints.accelerator.is_none());
}

/// Asserts the task tool's flags of a Mac job: the tree and the put directory by their paths in
/// `exec/`, relative to the task's working directory, and the lease, the action and the
/// deadline as a Linux job has them.
#[test]
fn a_mac_task_is_told_the_tree_and_the_put_directory_in_exec() {
    let action = Digest {
        hash: "cd".repeat(32),
        size_bytes: 3,
    };
    let argv = mac_argv(&action, LeaseId::new(7, 8), Duration::from_secs(20));
    assert_eq!(
        argv,
        [
            "--materialize-work=.kbf-tree",
            "--stage-and-commit",
            "--out-dir",
            ".kbf-out",
            "--action",
            &format!("{}/3", "cd".repeat(32)),
            "--lease",
            "7-8",
            "--deadline-ms",
            "20000",
            "--object-format",
            "one-pass",
        ]
    );
    assert_eq!(TREE_TARGET, "/work/.kbf-tree");
    assert_eq!(MAC_OUT_TARGET, "/work/.kbf-out");
}

/// Asserts the two mounts a Mac job is given: the tree read-only and the put directory
/// read-write, each at its target, `no_exec` and private.
#[test]
fn a_mac_jobs_mounts_are_the_tree_and_the_put_directory() {
    let read = super::mount_at("/t".to_owned(), TREE_TARGET, MountMode::ReadOnly);
    assert_eq!(read.source, Some(mount::Source::HostPath("/t".to_owned())));
    assert_eq!(read.target, TREE_TARGET);
    assert_eq!(read.mode(), MountMode::ReadOnly);
    assert!(read.no_exec && read.sub_path.is_empty());
    assert_eq!(read.propagation(), MountPropagation::Private);
    let write = super::mount_at("/p".to_owned(), MAC_OUT_TARGET, MountMode::ReadWrite);
    assert_eq!(write.mode(), MountMode::ReadWrite);
    assert!(write.no_exec);
}
