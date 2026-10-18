use std::collections::BTreeSet;

use kbf_host::path::HostPath;

use super::*;
use crate::binary::Arch;

fn io_error() -> io::Error {
    io::Error::from(io::ErrorKind::PermissionDenied)
}

/// One of every case, by its expected code.
fn every_case() -> Vec<(&'static str, Refusal)> {
    let untrusted = |fault| Refusal::Untrusted {
        path: "/usr/share/kbf-installer/trust/gh".into(),
        fault,
    };
    let settings = |fault| Refusal::Settings {
        path: "/etc/x".into(),
        fault,
    };
    let inbox = |fault| Refusal::Inbox {
        name: "kbf.tar.gz".into(),
        fault,
    };
    let archive = |fault| Refusal::Archive {
        component: "kbf".into(),
        fault,
    };
    let signature = |fault| Refusal::BadSignature {
        component: "kbf".into(),
        fault,
    };
    let version = |fault| Refusal::Version {
        component: "kbf".into(),
        fault,
    };
    vec![
        ("untrusted-missing", untrusted(TrustFault::Missing)),
        ("untrusted-link", untrusted(TrustFault::Link)),
        (
            "untrusted-kind",
            untrusted(TrustFault::Kind {
                found: "directory",
                want: "file",
            }),
        ),
        (
            "untrusted-owner",
            untrusted(TrustFault::NotRoot { uid: 501 }),
        ),
        (
            "untrusted-writable",
            untrusted(TrustFault::Writable { mode: 0o775 }),
        ),
        (
            "untrusted-unreadable",
            untrusted(TrustFault::Unreadable(io_error())),
        ),
        (
            "settings-syntax",
            settings(SettingsFault::Syntax { line: 3 }),
        ),
        (
            "settings-unknown-key",
            settings(SettingsFault::UnknownKey {
                line: 1,
                key: "X".into(),
            }),
        ),
        (
            "settings-repeated",
            settings(SettingsFault::Repeated {
                line: 2,
                key: "RUN_AS_UID",
            }),
        ),
        (
            "settings-number",
            settings(SettingsFault::NotANumber {
                line: 2,
                key: "RUN_AS_UID",
            }),
        ),
        (
            "settings-ref",
            settings(SettingsFault::BadRef {
                line: 1,
                value: "main".into(),
            }),
        ),
        ("settings-no-refs", settings(SettingsFault::NoRefs)),
        ("settings-run-as-root", settings(SettingsFault::RunAsRoot)),
        ("settings-utf8", settings(SettingsFault::NotUtf8)),
        (
            "offer-malformed",
            Refusal::Offer(OfferFault::NotAnOffer("eof".into())),
        ),
        (
            "offer-schema",
            Refusal::Offer(OfferFault::Schema { found: 2, want: 1 }),
        ),
        (
            "offer-no-components",
            Refusal::Offer(OfferFault::NoComponents),
        ),
        (
            "offer-component",
            Refusal::Offer(OfferFault::UnknownComponent("x".into())),
        ),
        (
            "offer-repeated-component",
            Refusal::Offer(OfferFault::RepeatedComponent("kbf".into())),
        ),
        (
            "offer-file-name",
            Refusal::Offer(OfferFault::BadFileName("../x".into())),
        ),
        (
            "offer-repeated-file",
            Refusal::Offer(OfferFault::RepeatedFile("a".into())),
        ),
        (
            "offer-sha256",
            Refusal::Offer(OfferFault::BadSha256("zz".into())),
        ),
        (
            "offer-commit",
            Refusal::Offer(OfferFault::BadCommit("HEAD".into())),
        ),
        ("inbox-missing", inbox(InboxFault::Missing)),
        ("inbox-symlink", inbox(InboxFault::Symlink)),
        ("inbox-not-regular", inbox(InboxFault::NotRegular)),
        ("inbox-not-directory", inbox(InboxFault::NotADirectory)),
        ("inbox-bad-name", inbox(InboxFault::BadName)),
        ("inbox-hard-linked", inbox(InboxFault::HardLinked)),
        ("inbox-oversize", inbox(InboxFault::Oversize { cap: 10 })),
        ("inbox-empty", inbox(InboxFault::Empty)),
        ("inbox-unexpected", inbox(InboxFault::Unexpected)),
        ("inbox-too-many", inbox(InboxFault::TooMany { max: 16 })),
        (
            "inbox-unreadable",
            inbox(InboxFault::Unreadable(io_error())),
        ),
        (
            "hash-mismatch",
            Refusal::HashMismatch {
                name: "kbf.tar.gz".into(),
            },
        ),
        (
            "not-attested",
            Refusal::NotAttested {
                subject: "kbf.tar.gz".into(),
                detail: "no matching attestation".into(),
            },
        ),
        (
            "archive-malformed",
            archive(ArchiveFault::NotTarGz("eof".into())),
        ),
        ("archive-entries", archive(ArchiveFault::Entries(2))),
        (
            "archive-name",
            archive(ArchiveFault::WrongName("kbf".into())),
        ),
        ("archive-not-regular", archive(ArchiveFault::NotRegular)),
        (
            "archive-oversize",
            archive(ArchiveFault::Oversize { cap: 10 }),
        ),
        ("archive-trailing", archive(ArchiveFault::TrailingData)),
        ("archive-timeout", archive(ArchiveFault::TimedOut)),
        (
            "size-mismatch",
            Refusal::Size {
                name: "kbf".into(),
                want: 10,
                got: 9,
                max: 100,
            },
        ),
        (
            "components",
            Refusal::Components("kbf was not fetched".into()),
        ),
        (
            "facts-missing",
            Refusal::Facts {
                component: "kbf".into(),
                fault: FactsFault::Missing,
            },
        ),
        (
            "facts-differ",
            Refusal::Facts {
                component: "kbf".into(),
                fault: FactsFault::Differs {
                    field: "protocol",
                    want: "4".into(),
                    got: "5".into(),
                },
            },
        ),
        (
            "installer-too-old",
            Refusal::InstallerTooOld {
                component: "kbf".into(),
                what: "protocol 9".into(),
            },
        ),
        (
            "below-floor",
            Refusal::Floor {
                component: "kbf".into(),
                what: "protocol 1".into(),
            },
        ),
        ("lease-live", Refusal::Lease("1 leases are live".into())),
        (
            "version-oversize",
            version(VersionFault::Oversize { cap: 4096 }),
        ),
        (
            "wrong-platform",
            Refusal::WrongPlatform {
                component: "kbf".into(),
                found: BinaryKind::MachO(Arch::Aarch64),
                want: BinaryKind::Elf(Arch::X86_64),
            },
        ),
        (
            "quarantined",
            Refusal::Quarantined {
                name: "kbf.tar.gz".into(),
            },
        ),
        ("signature-invalid", signature(SignatureFault::NotValid)),
        (
            "signature-flags",
            signature(SignatureFault::Flags {
                found: "adhoc".into(),
            }),
        ),
        ("version-failed", version(VersionFault::Failed(Some(1)))),
        (
            "version-unreadable",
            version(VersionFault::Unreadable("eof".into())),
        ),
        (
            "version-commit",
            version(VersionFault::Commit {
                want: "a".into(),
                got: "b".into(),
            }),
        ),
        (
            "version-profile",
            version(VersionFault::Profile("debug".into())),
        ),
        (
            "release-differs",
            Refusal::Differs {
                path: "/var/lib/kbf-installer/releases/x/kbf".into(),
            },
        ),
        ("install-failed", Refusal::Install(ReleaseError::NoPrevious)),
        (
            "tool-failed",
            Refusal::Tool {
                what: "gh",
                source: io_error(),
            },
        ),
        ("timed-out", Refusal::TimedOut { what: "gh" }),
        (
            "work-dir",
            Refusal::Work {
                path: "/work".into(),
                source: io_error(),
            },
        ),
    ]
}

/// Catches a case that has no stable code, one that shares a code with another, a code
/// that is not lowercase kebab-case (a code is read by programs), and a message that is
/// empty or runs over a line: every case is built, its code must be the one listed
/// for it, and no two are alike.
///
/// Mutant seen red: two variants given the same code string.
#[test]
fn every_refusal_has_its_own_stable_code_and_a_one_line_message() {
    let cases = every_case();
    let mut seen = BTreeSet::new();
    for (code, refusal) in &cases {
        assert_eq!(refusal.code(), *code, "{refusal}");
        assert!(seen.insert(*code), "{code} is used twice");
        assert!(
            code.bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-'),
            "{code}"
        );
        let message = refusal.to_string();
        assert!(
            !message.is_empty() && !message.contains('\n'),
            "{message:?}"
        );
    }
    assert_eq!(seen.len(), cases.len());
}

/// Catches a release error that is a refusal of its own (a release directory that is there
/// with other bytes) being buried in the generic install failure, and the rest losing
/// their cause.
#[test]
fn a_release_that_differs_is_its_own_refusal_and_other_errors_keep_their_cause() {
    let differs = ReleaseError::Differs {
        path: HostPath::new("/var/lib/kbf-installer/releases/a/kbf").expect("path"),
    };
    let refusal = Refusal::from(differs);
    assert_eq!(refusal.code(), "release-differs");
    assert!(
        refusal
            .to_string()
            .starts_with("/var/lib/kbf-installer/releases/a/kbf differs")
    );
    let other = Refusal::from(ReleaseError::NoPrevious);
    assert_eq!(other.code(), "install-failed");
    assert_eq!(
        other.to_string(),
        "cannot install the release: no previous release to roll back to"
    );
}

/// Catches a refusal reported to the operator under the wrong contract word (the words
/// are read by programs: `status.json`, the frontend's rollout), or a failure that is not
/// a verdict on the release (a tool that would not start) reported as one. Every case is
/// listed with its word; a case with `None` is a failure, exit 1, not a refusal, exit 3.
///
/// Mutant seen red: `attestation` and `hash` swapped.
#[test]
fn every_refusal_is_reported_under_the_word_of_the_contract() {
    let expected = [
        ("untrusted-missing", Some("trust_chain")),
        ("hash-mismatch", Some("hash")),
        ("not-attested", Some("attestation")),
        ("archive-malformed", Some("shape")),
        ("archive-entries", Some("shape")),
        ("archive-oversize", Some("size")),
        ("archive-trailing", Some("shape")),
        ("archive-timeout", Some("shape")),
        ("size-mismatch", Some("size")),
        ("components", Some("fetch")),
        ("facts-missing", Some("facts")),
        ("facts-differ", Some("facts")),
        ("installer-too-old", Some("installer_too_old")),
        ("below-floor", Some("floor")),
        ("lease-live", Some("lease")),
        ("wrong-platform", Some("platform")),
        ("quarantined", Some("shape")),
        ("signature-invalid", Some("shape")),
        ("version-failed", Some("facts")),
        ("version-unreadable", Some("facts")),
        ("version-commit", Some("commit_or_ref")),
        ("version-profile", Some("facts")),
        ("version-oversize", Some("facts")),
        ("settings-syntax", None),
        ("offer-malformed", None),
        ("inbox-missing", None),
        ("release-differs", None),
        ("install-failed", None),
        ("tool-failed", None),
        ("work-dir", None),
    ];
    let cases = every_case();
    for (code, want) in expected {
        let (_, refusal) = cases
            .iter()
            .find(|(c, _)| *c == code)
            .unwrap_or_else(|| panic!("{code} is a case"));
        assert_eq!(refusal.word(), want, "{code}");
    }
    // The two programs the stage bounds are told apart; any other program is a shape check.
    let timed = |what| Refusal::TimedOut { what }.word();
    assert_eq!(timed(WHAT_GH), Some("attestation"));
    assert_eq!(timed(WHAT_PROBE), Some("facts"));
    assert_eq!(timed("codesign"), Some("shape"));
}
