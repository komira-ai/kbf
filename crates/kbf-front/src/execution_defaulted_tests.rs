//! The two pure parts of the default platform: which platforms get it, and how often
//! its log line is written.

use std::time::Duration;

use kbf_proto::reapi::{self, platform};
use tokio::time::Instant;

use super::{DEFAULTED_LOG_EVERY, DefaultedLog, properties, refuse_platform, wants_default};

fn sent(names: &[(&str, &str)]) -> reapi::Platform {
    reapi::Platform {
        properties: names
            .iter()
            .map(|(name, value)| platform::Property {
                name: (*name).to_owned(),
                value: (*value).to_owned(),
            })
            .collect(),
    }
}

/// Whether a request that sends `names` gets the default.
fn defaulted(names: &[(&str, &str)]) -> bool {
    wants_default(&properties(Some(&sent(names))).expect("a platform kbf reads"))
}

/// Catches: the default applied to a request that names an OS, an architecture, an ISA
/// level, an Xcode or an iOS device under any spelling (it would turn a Mac request into a
/// Linux one, or a half-named pair into a different whole), and withheld from one that
/// names nothing kbf places on (a booking, a lease kind, a `gpu`, a CPU model).
#[test]
fn only_a_platform_that_names_no_placement_gets_the_default() {
    for names in [
        &[][..],
        &[("kbf-lease", "action")],
        &[("kbf-book-cpus", "2"), ("kbf-book-mem-gib", "8")],
        &[("gpu", "1")],
        &[("cpu.model", "epyc")],
        &[("cpus", "4")],
        // Sent, but not read by `properties`: the container driver's.
        &[("container-image", "docker://img@sha256:00")],
    ] {
        assert!(defaulted(names), "{names:?} should be defaulted");
    }
    for names in [
        &[("OSFamily", "Darwin")][..],
        &[("osfamily", "linux")],
        &[("os", "macos")],
        &[("OS", "linux")],
        &[("ISA", "arm64")],
        &[("isa", "x86-64")],
        &[("Arch", "amd64")],
        &[("arch", "arm64")],
        &[("ARCH", "arm64")],
        &[("isa_level", "x86-64-v3")],
        &[("ISA_Level", "x86-64-v3")],
        &[("xcode", "16C5032a")],
        &[("XCODE", "16C5032a")],
        &[("ios.device", "1")],
        &[("ios.device", "1"), ("ios.device.class", "iPhone")],
        &[("ios.device", "1"), ("OSFamily", "darwin")],
        &[("OSFamily", "linux"), ("gpu", "1")],
    ] {
        assert!(!defaulted(names), "{names:?} should not be defaulted");
    }
}

/// Catches: `container-image` accepted beside macOS (the image is silently unused on a
/// Mac), refused beside Linux or alone, or the macOS check missing a spelling of macOS
/// (`OSFamily=Darwin`, `osx`, `os=macos`); the refusal must name the key and the os.
#[test]
fn container_image_is_refused_beside_macos_only() {
    let image = ("container-image", "docker://img@sha256:00");
    for os in [
        ("OSFamily", "Darwin"),
        ("osfamily", "macos"),
        ("OSFamily", "osx"),
        ("os", "macos"),
    ] {
        let status = refuse_platform(Some(&sent(&[image, os]))).expect_err("refused");
        assert_eq!(status.code(), tonic::Code::InvalidArgument, "{os:?}");
        let message = status.message();
        assert!(
            message.contains("\"container-image\"") && message.contains("\"macos\""),
            "{os:?}: {message}"
        );
    }
    for ok in [
        vec![image],
        vec![image, ("OSFamily", "Linux")],
        vec![image, ("os", "linux"), ("ISA", "arm64")],
        vec![("OSFamily", "Darwin")],
    ] {
        refuse_platform(Some(&sent(&ok))).unwrap_or_else(|s| panic!("{ok:?}: {s:?}"));
    }
}

/// Catches: the "defaulted" log line written for every application (a farm of
/// thousands of empty-platform actions would drown the log), never written again after
/// the first, written early, or that loses the count of the lines it skipped.
#[test]
fn the_defaulted_line_is_logged_once_per_interval_with_the_count_skipped() {
    let log = DefaultedLog::default();
    let start = Instant::now();
    assert_eq!(log.admit(start), Some(0), "the first is logged");
    let just_before = start + DEFAULTED_LOG_EVERY - Duration::from_millis(1);
    assert_eq!(log.admit(start), None);
    assert_eq!(log.admit(start + Duration::from_secs(1)), None);
    assert_eq!(log.admit(just_before), None, "not yet an interval");
    let due = start + DEFAULTED_LOG_EVERY;
    assert_eq!(log.admit(due), Some(3), "an interval, three skipped");
    assert_eq!(log.admit(due), None);
    assert_eq!(log.admit(due + DEFAULTED_LOG_EVERY), Some(1));
    assert_eq!(log.admit(due + 2 * DEFAULTED_LOG_EVERY), Some(0));
}
