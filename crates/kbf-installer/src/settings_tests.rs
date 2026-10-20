use kbf_host::exec::Entry;
use kbf_host::path::{HostPath, Root};
use kbf_host::step::{Owner, Perms};
use kbf_host::testkit::FakeExec;

use super::*;
use crate::fixtures::trusted_host;
use crate::layout::Platform;

fn host(platform: Platform) -> Layout {
    Layout::new(Root::new("/").expect("absolute"), platform)
}

fn with_settings(text: &str) -> FakeExec {
    let mut exec = trusted_host();
    exec.entries.insert(
        crate::layout::path(crate::fixtures::SETTINGS_PATH),
        Entry::File {
            perms: Perms::new(0o644),
            owner: Owner { uid: 0, gid: 0 },
            bytes: text.as_bytes().to_vec(),
        },
    );
    exec
}

fn load(text: &str) -> Result<Settings, Refusal> {
    Settings::load(&with_settings(text), &host(Platform::Linux))
}

fn code(result: Result<Settings, Refusal>) -> &'static str {
    result.expect_err("refused").code()
}

/// Catches defaults that are not the ones a host with no file runs on: the one source
/// ref `refs/heads/main`, the platform's `nobody`, the build's CPU.
#[test]
fn a_host_with_no_settings_file_gets_the_defaults() {
    let linux = Settings::load(&trusted_host(), &host(Platform::Linux)).expect("defaults");
    assert_eq!(
        linux.source_refs,
        vec![SourceRef::new("refs/heads/main").expect("ref")]
    );
    assert_eq!(
        linux.run_as,
        RunAs {
            uid: 65534,
            gid: 65534
        }
    );
    assert_eq!((linux.arch, linux.timeout), (Arch::current(), TOOL_TIMEOUT));
    let mac = Settings::load(&trusted_host(), &host(Platform::MacOs)).expect("defaults");
    assert_eq!(mac.run_as.uid, 4_294_967_294);
}

/// Catches each key of the file being ignored or misread, and the ones left out keeping
/// their defaults: refs (several, in order), the user and the group.
#[test]
fn each_key_of_the_file_replaces_its_default() {
    let s = load("# a comment\n\nSOURCE_REFS=refs/heads/main  refs/heads/release/1\nRUN_AS_UID=1234\nRUN_AS_GID=5678\n")
        .expect("valid");
    let names: Vec<&str> = s.source_refs.iter().map(SourceRef::as_str).collect();
    assert_eq!(names, ["refs/heads/main", "refs/heads/release/1"]);
    assert_eq!(
        s.run_as,
        RunAs {
            uid: 1234,
            gid: 5678
        }
    );
    let s = load("RUN_AS_UID=1234\n").expect("valid");
    assert_eq!(
        s.run_as,
        RunAs {
            uid: 1234,
            gid: 65534
        }
    );
    assert_eq!(s.source_refs.len(), 1);
    let s = load("").expect("an empty file is the defaults");
    assert_eq!(s, Settings::defaults(&host(Platform::Linux)));
}

/// Catches a settings file read loosely: a line that is no `KEY=VALUE`, a key the build
/// does not know, a key twice, a value that is no number, a ref that is no branch ref (or
/// would be an option to `gh`), a list with no ref, and the unprivileged user being root
/// by uid or by gid.
///
/// Mutants seen red: the repeated-key check dropped; root accepted as the user.
#[test]
fn a_settings_file_is_closed_and_strict() {
    let cases = [
        ("garbage\n", "settings-syntax"),
        ("FOO=1\n", "settings-unknown-key"),
        ("RUN_AS_UID=7\nRUN_AS_UID=8\n", "settings-repeated"),
        ("RUN_AS_UID=nobody\n", "settings-number"),
        ("RUN_AS_GID=-2\n", "settings-number"),
        ("SOURCE_REFS=main\n", "settings-ref"),
        ("SOURCE_REFS=refs/heads/main --deny\n", "settings-ref"),
        ("SOURCE_REFS=\n", "settings-no-refs"),
        ("RUN_AS_UID=0\n", "settings-run-as-root"),
        ("RUN_AS_GID=0\n", "settings-run-as-root"),
    ];
    for (text, want) in cases {
        assert_eq!(code(load(text)), want, "{text:?}");
    }
    let message = load("FOO=1\n").expect_err("refused").to_string();
    assert_eq!(
        message,
        "settings /usr/share/kbf-installer/trust/installer.conf: line 1: \"FOO\" is not a setting"
    );
    let mut exec = trusted_host();
    exec.entries.insert(
        crate::layout::path(crate::fixtures::SETTINGS_PATH),
        Entry::File {
            perms: Perms::new(0o644),
            owner: Owner { uid: 0, gid: 0 },
            bytes: vec![0xff, 0xfe],
        },
    );
    assert_eq!(
        code(Settings::load(&exec, &host(Platform::Linux))),
        "settings-utf8"
    );
}

/// Catches a settings file anyone could have written being believed: it is held to the
/// same chain as `gh` (owner, mode), a symlink in its place is refused, and so is a
/// directory.
///
/// Mutant seen red: the trust check on the file skipped.
#[test]
fn a_settings_file_others_could_write_is_refused() {
    for (mode, uid, want) in [
        (0o666, 0, "untrusted-writable"),
        (0o644, 501, "untrusted-owner"),
    ] {
        let mut exec = with_settings("RUN_AS_UID=9\n");
        exec.entries.insert(
            crate::layout::path(crate::fixtures::SETTINGS_PATH),
            Entry::File {
                perms: Perms::new(mode),
                owner: Owner { uid, gid: 0 },
                bytes: b"RUN_AS_UID=9\n".to_vec(),
            },
        );
        let err = Settings::load(&exec, &host(Platform::Linux)).expect_err("refused");
        assert_eq!(err.code(), want);
        assert!(
            err.to_string().starts_with(crate::fixtures::SETTINGS_PATH),
            "{err}"
        );
    }
    let mut exec = with_settings("");
    exec.entries.insert(
        crate::layout::path(crate::fixtures::SETTINGS_PATH),
        Entry::Link {
            target: "elsewhere".into(),
        },
    );
    assert_eq!(
        code(Settings::load(&exec, &host(Platform::Linux))),
        "untrusted-link"
    );
}

/// Catches an unreadable settings file read as an absent one (the defaults would then
/// apply on a host whose file is there and cannot be read), and a link, a directory or a
/// device where the file should be: both are refused.
#[test]
fn a_settings_file_that_cannot_be_read_is_refused_not_ignored() {
    let host = host(Platform::Linux);
    let exec = with_settings("");
    let unreadable = HostPath::new("/unreadable").expect("path");
    let err = Settings::load_from(&exec, &host, &unreadable).expect_err("read fails");
    assert_eq!(err.code(), "untrusted-unreadable");
    assert!(
        err.to_string().starts_with("/unreadable is not trusted"),
        "{err}"
    );

    let mut exec = with_settings("");
    exec.entries.insert(
        crate::layout::path(crate::fixtures::SETTINGS_PATH),
        Entry::Dir {
            perms: Perms::new(0o755),
            owner: Owner { uid: 0, gid: 0 },
        },
    );
    let err = Settings::load(&exec, &host).expect_err("a directory");
    assert_eq!(err.code(), "untrusted-link");
}
