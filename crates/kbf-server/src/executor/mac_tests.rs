//! The Macs: which actions ask for one, the pool's rules, and the parts of a Mac job.

use std::time::Duration;

use js_proto::v1::{
    Constraints, ExecMode, Isolation, JobSpec, MountMode, MountPropagation, ProviderSource, mount,
};
use kbf_proto::reapi::{Digest, Platform, platform::Property};
use kbf_types::OperationId;

use super::{
    DEFAULT_MAC_OS_VERSION, MAC_ARCH, MAC_OS, MAC_OUT_TARGET, MacPool, MacPoolError, RunId, System,
    TREE_TARGET, XcodeBuild, check_version, mac_argv, mac_constraints,
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
        bundle: format!("kbf-task@sha256:{}", "9a".repeat(32)),
        os_version: DEFAULT_MAC_OS_VERSION,
        roots: None,
    }
}

/// Asserts that an action asks for a Mac exactly when `kbf_caps` reads its platform as one:
/// every spelling of `OSFamily` it reads as macOS and `os=macos`, in any case of the name; and
/// that no platform, an empty one, a Linux one and one that names only an architecture are
/// Linux. Catches a reader that knows fewer spellings than `kbf_caps`, or takes any `os` for
/// Linux.
#[test]
fn an_action_asks_for_a_mac_as_kbf_caps_reads_its_platform() {
    let mac = System::Macos {
        os_version: None,
        xcode: None,
    };
    for spelling in [
        &[("OSFamily", "macos")][..],
        &[("OSFamily", "Darwin")],
        &[("osfamily", "MacOSX")],
        &[("OSFamily", "osx")],
        &[("os", "macos")],
        &[("OSFamily", "macos"), ("container-image", "docker://x")],
    ] {
        assert_eq!(of(spelling), Ok(mac.clone()), "{spelling:?}");
    }
    assert_eq!(System::of(None), Ok(System::Linux));
    assert_eq!(of(&[]), Ok(System::Linux));
    assert_eq!(of(&[("OSFamily", "linux")]), Ok(System::Linux));
    assert_eq!(of(&[("ISA", "arm64")]), Ok(System::Linux));
    assert_eq!(System::default(), System::Linux);
}

/// Asserts the `os_version` of a Mac action is its least major version, whatever the minor and
/// patch and the case of the name. A value that is no version is refused by the platform's
/// reader for any system (since `os_version` became a capability the reader parses, as `26` or
/// `26.1`), with the reader's reason; a Linux action's that the reader takes is not read. A
/// Mac action's that the reader takes but that is still no `<major>[.<minor>[.<patch>]]` of
/// digits is refused here, naming it.
#[test]
fn a_mac_actions_os_version_is_its_least_major_version() {
    for (value, major) in [("26", 26), ("26.1", 26), ("15.0.1", 15), ("0", 0)] {
        assert_eq!(
            of(&[("OSFamily", "macos"), ("os_version", value)]),
            Ok(System::Macos {
                os_version: Some(major),
                xcode: None,
            }),
            "{value}"
        );
    }
    assert_eq!(
        of(&[("OSFamily", "macos"), ("OS_Version", "27")]),
        Ok(System::Macos {
            os_version: Some(27),
            xcode: None,
        })
    );
    assert_eq!(
        of(&[("OSFamily", "linux"), ("os_version", "26.")]),
        Ok(System::Linux)
    );
    let reader = |pairs: &[(&str, &str)]| {
        kbf_caps::Request::from_platform(pairs.iter().copied())
            .expect_err("the reader refuses it")
            .to_string()
    };
    for bad in ["", "x", ".1", "26-beta", "99999999999"] {
        for os in ["macos", "linux"] {
            let pairs = [("OSFamily", os), ("os_version", bad)];
            assert_eq!(of(&pairs), Err(reader(&pairs)), "{os} {bad:?}");
        }
    }
    assert_eq!(
        reader(&[("os_version", "jammy")]),
        "invalid value \"jammy\" for capability key \"os_version\""
    );
    for bad in ["26.", "26.1.2.999", "+26"] {
        assert_eq!(
            of(&[("OSFamily", "macos"), ("os_version", bad)]),
            Err(format!(
                "the action's os_version {bad:?} is not a version such as 26 or 26.1"
            )),
            "{bad:?}"
        );
    }
}

/// Asserts a Mac action's `xcode` is the build it names, under any case of the name, beside an
/// `os_version` or not; that a build of 1 and of 32 bytes of every allowed kind is taken; and
/// that a Linux action's `xcode` is not checked as a build, even one that is no build id (an
/// empty or repeated one is the reader's refusal, for any system:
/// [`a_platform_the_farm_refuses_is_refused`]). Catches a Mac job whose
/// Xcode is dropped, a name read in one case only, and a Linux action refused for a property it
/// does not use.
#[test]
fn a_mac_actions_xcode_is_the_build_it_names_in_any_case_of_the_name() {
    let build = |id: &str| Some(XcodeBuild::parse(id).expect("a build"));
    for name in ["xcode", "XCODE", "Xcode"] {
        assert_eq!(
            of(&[("OSFamily", "macos"), (name, "16C5032a")]),
            Ok(System::Macos {
                os_version: None,
                xcode: build("16C5032a"),
            }),
            "{name}"
        );
    }
    assert_eq!(
        of(&[("os", "macos"), ("os_version", "26"), ("xcode", "17A324")]),
        Ok(System::Macos {
            os_version: Some(26),
            xcode: build("17A324"),
        })
    );
    let longest = format!("Z9{}yz", "a._-".repeat(7));
    assert_eq!(longest.len(), 32);
    for id in ["7", "17E5179g", "16.4_beta-2", longest.as_str()] {
        assert_eq!(build(id).expect("taken").as_str(), id);
    }
    for value in ["16C5032a", "not a build"] {
        assert_eq!(
            of(&[("OSFamily", "linux"), ("xcode", value)]),
            Ok(System::Linux),
            "{value}"
        );
        assert_eq!(of(&[("xcode", value)]), Ok(System::Linux), "{value}");
    }
}

/// Asserts a Mac action whose `xcode` is no build id is refused, naming the value: one that
/// starts with a punctuation mark, holds a space, a slash, a colon or a byte past ASCII, or is
/// 33 bytes long. Catches a value the job service would refuse at submit, or one that names a
/// path, passed on as a build.
#[test]
fn a_mac_actions_xcode_that_is_no_build_id_is_refused() {
    let long = "a".repeat(33);
    for bad in [
        "-16C5032a",
        ".16C5032a",
        "_x",
        "16C 5032a",
        "16C5032a/..",
        "Xcode:16",
        "16C5032\u{e9}",
        long.as_str(),
    ] {
        assert_eq!(
            of(&[("OSFamily", "macos"), ("xcode", bad)]),
            Err(format!(
                "the action's xcode {bad:?} is not an Xcode build id such as 16C5032a: 1 to 32 \
                 letters, digits, '.', '_' and '-', starting with a letter or a digit"
            )),
            "{bad:?}"
        );
    }
}

/// Asserts that a platform the reader refuses is refused here with the reader's reason: an `os` no
/// worker runs, `OSFamily` beside `os`, `xcode` under two spellings of its name, and an empty
/// `xcode`.
#[test]
fn a_platform_the_farm_refuses_is_refused() {
    for bad in [
        &[("os", "windows")][..],
        &[("OSFamily", "macos"), ("os", "macos")],
        &[
            ("OSFamily", "macos"),
            ("xcode", "16C5032a"),
            ("XCODE", "17A324"),
        ],
        &[("OSFamily", "macos"), ("xcode", "")],
    ] {
        let reader = kbf_caps::Request::from_platform(bad.iter().copied())
            .expect_err("the reader refuses it")
            .to_string();
        assert_eq!(of(bad), Err(reader), "{bad:?}");
    }
}

/// Asserts the pool's rules, each by its own error: a task bundle that is
/// not pinned by a digest, and a least macOS of 0; and that a pool's roots are its
/// own when it names them, else the Linux ones.
#[test]
fn a_pool_is_checked_and_its_roots_default_to_the_linux_ones() {
    assert_eq!(pool().validate(), Ok(()));
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

/// Asserts what a Mac job asks of its host: `os=macos`, `arch=arm64`, the native runtime, the
/// action's Xcode build, and nothing else (no least version: the executor holds that one).
/// Catches a Mac job placed on any Mac whatever Xcodes it has.
#[test]
fn a_mac_job_asks_for_a_mac_in_the_native_runtime_with_the_actions_xcode() {
    let xcode = XcodeBuild::parse("16C5032a").expect("a build");
    let constraints = mac_constraints(&xcode);
    assert_eq!(constraints.os, MAC_OS);
    assert_eq!(constraints.arch, MAC_ARCH);
    assert_eq!(constraints.exec(), ExecMode::SandboxExec);
    assert_eq!(constraints.xcode, "16C5032a");
    assert_eq!(constraints.isolation(), Isolation::Unspecified);
    assert!(constraints.isa_level.is_empty() && constraints.accelerator.is_none());
    assert!(constraints.os_version.is_empty());
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
    let argv = mac_argv(
        &action,
        RunId::new(OperationId(7), 8),
        Duration::from_secs(20),
    );
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

/// Asserts the two mounts a Mac job is given: the tree read-only from tierstore's provider,
/// asked for the tree list's key untouched, and the put directory read-write, each at its
/// target, `no_exec` and private. Catches a Mac tree mounted from a host directory nothing on a
/// Mac fills.
#[test]
fn a_mac_jobs_mounts_are_the_tree_from_the_provider_and_the_put_directory() {
    let key = "cas/l/abababababababababababababababababababababababababababababababab";
    let read = super::mount_at(super::tree_source(key), TREE_TARGET, MountMode::ReadOnly);
    assert_eq!(
        read.source,
        Some(mount::Source::Provider(ProviderSource {
            provider: "tierstore".to_owned(),
            r#ref: key.to_owned(),
        }))
    );
    assert_eq!(read.target, TREE_TARGET);
    assert_eq!(read.mode(), MountMode::ReadOnly);
    assert!(read.no_exec && read.sub_path.is_empty());
    assert_eq!(read.propagation(), MountPropagation::Private);
    let put = mount::Source::HostPath("/p".to_owned());
    let write = super::mount_at(put.clone(), MAC_OUT_TARGET, MountMode::ReadWrite);
    assert_eq!(write.source, Some(put));
    assert_eq!(write.mode(), MountMode::ReadWrite);
    assert!(write.no_exec);
    assert_eq!(write.propagation(), MountPropagation::Private);
}

/// Asserts which jobs are a Mac's: one whose constraints ask for `os=macos`, and no other (a
/// Linux job, a job with no constraints, a job read back with no spec). Catches the wait for a
/// Mac's pack applied to a Linux job, or a Mac job read back from a gone process missed.
#[test]
fn a_job_is_a_macs_when_it_asks_for_macos() {
    let xcode = XcodeBuild::parse("16C5032a").expect("a build");
    let mac = JobSpec {
        constraints: Some(mac_constraints(&xcode)),
        ..JobSpec::default()
    };
    assert!(super::is_mac(Some(&mac)));
    let linux = JobSpec {
        constraints: Some(Constraints {
            os: "linux".to_owned(),
            ..Constraints::default()
        }),
        ..JobSpec::default()
    };
    assert!(!super::is_mac(Some(&linux)));
    assert!(!super::is_mac(Some(&JobSpec::default())));
    assert!(!super::is_mac(None));
}
