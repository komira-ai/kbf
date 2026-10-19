//! The parts of the real fetcher that need no child and no pipeline: how failures are
//! classified and worded, the name check, and the replaced-release size. The whole path,
//! with a stand-in child and the real pipeline, is in `tests/fetcher_real.rs`.

use std::fs;
use std::io;
use std::path::PathBuf;
use std::time::Duration;

use super::*;
use crate::fetch::describe::DescribeFault;
use crate::fetch::identity::IdentityFault;
use crate::state::{Locked, State};
use crate::testkit::{A, B, c, host_root};

/// What `fetch_error` makes of `step`, against `want` given the step's own words.
fn classed(step: fetch::FetchError, want: fn(String) -> FetchError) {
    let shown = step.to_string();
    assert_eq!(fetch_error(step), want(shown));
}

/// The fetch step's failures: a digest or size that is not acceptable, bytes that do not
/// hash to the order's, and a source that sends more than it declared are verdicts
/// (`Rejected`, the contract's word first); a source that cannot be reached or ends early, a
/// disk without room, a refused source URL and an identity that cannot be lent say nothing
/// of the release (`Unavailable`). The word `fetch` is what both already begin with, so it is
/// not said twice.
///
/// Catches a verdict taken for an outage (the helper would retry a bad release) and an outage
/// taken for a verdict, and a word lost on the way to the log.
#[test]
fn fetch_failures_are_told_apart_and_carry_the_contract_word() {
    use fetch::FetchError as Step;
    classed(Step::BadDigest("x".to_owned()), |s| {
        FetchError::Rejected(format!("hash: {s}"))
    });
    classed(Step::Size { size: 0, cap: 100 }, |s| {
        FetchError::Rejected(format!("size: {s}"))
    });
    let digest = Step::Digest {
        actual: "aa".to_owned(),
        want: "bb".to_owned(),
    };
    classed(digest, |s| FetchError::Rejected(format!("hash: {s}")));
    classed(Step::Pipe(PipeFault::Oversize { size: 5 }), |s| {
        FetchError::Rejected(s)
    });
    // A source that will not describe a release, or a description that cannot be used, is a
    // release that could not be had under the word `fetch`, which `Unavailable` already begins
    // with (the frontend's cycle words an `Unavailable` `fetch` and a `Rejected` `stage`).
    classed(
        Step::Describe(DescribeFault::Refused("NotFound: x".to_owned())),
        FetchError::Unavailable,
    );
    classed(
        Step::Describe(DescribeFault::Unusable("no kbf".to_owned())),
        FetchError::Unavailable,
    );
    // A child that said nothing, whatever the reason, is an outage.
    classed(
        Step::Pipe(PipeFault::Silent { log: String::new() }),
        FetchError::Unavailable,
    );
    let disk = Step::Disk {
        need: 10,
        free: 1,
        path: PathBuf::from("/x"),
    };
    classed(disk, |s| FetchError::Unavailable(format!("disk: {s}")));
    let source = Step::Source("http://x".to_owned(), "not https");
    classed(source, FetchError::Unavailable);
    classed(
        Step::Identity(IdentityFault::Stray(5)),
        FetchError::Unavailable,
    );
    let short = PipeFault::Short {
        got: 1,
        size: 5,
        log: String::new(),
    };
    classed(Step::Pipe(short), FetchError::Unavailable);
    let slow = PipeFault::TimedOut {
        after: Duration::from_secs(3),
    };
    classed(Step::Pipe(slow), FetchError::Unavailable);
    // A host that would run the child as root is a host set up wrong, not a bad release.
    let root = PipeFault::Root { uid: 0, gid: 0 };
    classed(Step::Pipe(root), FetchError::Unavailable);
}

/// What `refused` makes of `r`, against `want` given the refusal's own words.
fn refusal(r: Refusal, want: fn(String) -> FetchError) {
    let shown = r.to_string();
    assert_eq!(refused(r), want(shown));
}

/// The pipeline's refusals: one with a contract word is a verdict on the release, said with
/// that word first; one without (a scratch directory that cannot be made, a tool that would
/// not start) is the host failing, and is no verdict.
///
/// Catches a refusal of the release reported as an outage, or the host's own trouble blamed
/// on the release, and the word dropped.
#[test]
fn refusals_are_verdicts_only_when_the_contract_has_a_word_for_them() {
    let work = Refusal::Work {
        path: PathBuf::from("/w"),
        source: io::Error::other("no space"),
    };
    refusal(work, FetchError::Unavailable);
    let tool = Refusal::Tool {
        what: "gh",
        source: io::Error::other("missing"),
    };
    refusal(tool, FetchError::Unavailable);
    let hash = Refusal::HashMismatch {
        name: "kbf".to_owned(),
    };
    refusal(hash, |s| FetchError::Rejected(format!("hash: {s}")));
    let lease = Refusal::Lease("2 leases are live".to_owned());
    refusal(lease, |s| FetchError::Rejected(format!("lease: {s}")));
    let list = Refusal::Components("kbf was not fetched".to_owned());
    refusal(list, FetchError::Rejected);
}

/// A component's name becomes a file name in root's directory: only one plain name is taken.
///
/// Catches a name with a separator or a dot part (`../x`) being joined onto `dl/`.
#[test]
fn only_one_plain_file_name_is_a_component_name() {
    for good in ["kbf", "kbf-daemon", "a.b"] {
        assert!(is_plain_name(good), "{good}");
    }
    for bad in ["", ".", "..", "a/b", "/a", "../a", "a/", "a/.."] {
        assert!(!is_plain_name(bad), "{bad:?}");
    }
}

/// The release platform of a host is its operating system and CPU as the server names them,
/// and a Mac on x86-64 has none (no release is built for it).
///
/// Catches the CPU spelt as the OS spells it (`aarch64` for `arm64`), and a host given a
/// platform no release exists for.
#[test]
fn a_hosts_release_platform_is_its_os_and_cpu() {
    use ReleasePlatform::{DarwinArm64, LinuxArm64, LinuxX86_64};
    assert_eq!(host_platform(Platform::Linux, Arch::X86_64), Some(LinuxX86_64));
    assert_eq!(host_platform(Platform::Linux, Arch::Aarch64), Some(LinuxArm64));
    assert_eq!(host_platform(Platform::MacOs, Arch::Aarch64), Some(DarwinArm64));
    assert_eq!(host_platform(Platform::MacOs, Arch::X86_64), None);
}

/// The disk check counts the release that is current: the sizes of its files, and nothing
/// when there is no state, no current release or no directory for it.
///
/// Catches the replaced release left out of the room the contract asks for, and a
/// subdirectory or the wrong release counted.
#[test]
fn the_replaced_release_is_the_current_ones_files() {
    let (host, _) = host_root("fetcher-real-replaced");
    assert_eq!(replaced_bytes(&host), 0, "no state");
    let lock = Locked::acquire(&host).expect("lock");
    let current = State {
        current: Some(c(A)),
        ..State::default()
    };
    lock.write(&current).expect("state");
    assert_eq!(replaced_bytes(&host), 0, "no directory for it");
    let other = host.join(&host.release_dir(&c(B)));
    fs::create_dir_all(&other).expect("another release");
    fs::write(other.join("kbf"), vec![0; 1000]).expect("its binary");
    let dir = host.join(&host.release_dir(&c(A)));
    fs::create_dir_all(dir.join("sub")).expect("current release");
    fs::write(dir.join("kbf"), vec![0; 10]).expect("binary");
    fs::write(dir.join("VERIFIED"), b"abc").expect("marker");
    fs::write(dir.join("sub").join("deeper"), vec![0; 500]).expect("nested");
    assert_eq!(replaced_bytes(&host), 13);
    lock.write(&State::default()).expect("no current");
    assert_eq!(replaced_bytes(&host), 0, "no current release");
}
