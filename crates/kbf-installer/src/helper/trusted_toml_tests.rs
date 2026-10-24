//! `read_trusted`: the trust rule `installer.toml` is read under, and what each way of failing
//! it is refused with.

use kbf_host::step::{Owner, Perms};
use kbf_host::testkit::FakeExec;

use super::*;

const FILE: &str = "/etc/kbf/installer.toml";
const ROOT: Owner = Owner { uid: 0, gid: 0 };

fn dir(mode: u32, owner: Owner) -> Entry {
    Entry::Dir {
        perms: Perms::new(mode),
        owner,
    }
}

fn file(bytes: &[u8], mode: u32, owner: Owner) -> Entry {
    Entry::File {
        perms: Perms::new(mode),
        owner,
        bytes: bytes.to_vec(),
    }
}

/// A host as `setup` leaves it: root's 0644 file in root's 0755 `/etc/kbf` in `/etc`.
fn host(text: &str) -> FakeExec {
    let mut exec = FakeExec::new(0);
    for at in ["/etc", "/etc/kbf"] {
        exec.entries
            .insert(HostPath::new(at).expect("path"), dir(0o755, ROOT));
    }
    exec.entries.insert(
        HostPath::new(FILE).expect("path"),
        file(text.as_bytes(), 0o644, ROOT),
    );
    exec
}

fn read(exec: &FakeExec) -> Result<InstallerToml, ReadError> {
    read_trusted(exec, &HostPath::new(FILE).expect("path"))
}

/// The word of a refusal: the code `Refusal` gives it, or `invalid` for a file that is not an
/// `installer.toml`.
fn why(result: Result<InstallerToml, ReadError>) -> String {
    match result.expect_err("refused") {
        ReadError::Untrusted(refusal) => refusal.code().to_owned(),
        ReadError::Invalid(e) => format!("invalid: {e}"),
    }
}

/// Asserts that a file root owns and nobody else can write is read and parsed as
/// `InstallerToml::parse` parses its text: the settings are the file's, not the defaults.
#[test]
fn a_trusted_file_is_read_and_parsed() {
    let text =
        "release_source = \"url\"\nrelease_url = \"https://mirror.example\"\nmin_protocol = 3\n";
    let got = read(&host(text)).expect("trusted");
    assert_eq!(got, InstallerToml::parse(text).expect("parses"));
    assert_eq!(got.min_protocol, 3);
}

/// Asserts that a file owned by anyone but root, or writable by group or others, or in a
/// directory that is, is refused with the trust rule's code, although its content is a valid
/// `installer.toml`: whoever can write the file decides where releases come from, so nothing of
/// its content is used.
#[test]
fn a_file_root_does_not_alone_control_is_refused() {
    let text = "release_source = \"url\"\nrelease_url = \"https://mirror.example\"\n";
    let user = Owner {
        uid: 1000,
        gid: 1000,
    };
    let mut owned = host(text);
    owned.entries.insert(
        HostPath::new(FILE).expect("path"),
        file(text.as_bytes(), 0o644, user),
    );
    assert_eq!(why(read(&owned)), "untrusted-owner");

    for mode in [0o664, 0o646, 0o666] {
        let mut open = host(text);
        open.entries.insert(
            HostPath::new(FILE).expect("path"),
            file(text.as_bytes(), mode, ROOT),
        );
        assert_eq!(why(read(&open)), "untrusted-writable", "{mode:o}");
    }

    let mut above = host(text);
    above
        .entries
        .insert(HostPath::new("/etc/kbf").expect("path"), dir(0o775, ROOT));
    assert_eq!(why(read(&above)), "untrusted-writable", "the directory");
    above
        .entries
        .insert(HostPath::new("/etc/kbf").expect("path"), dir(0o755, user));
    assert_eq!(
        why(read(&above)),
        "untrusted-owner",
        "the directory's owner"
    );
}

/// Asserts that a file which is missing, one that cannot be read, and a directory or a link
/// where the file should be are each refused as untrusted, with the code that names which: a
/// host whose file is unreadable is never read as the defaults, so it cannot fetch from the
/// default source.
#[test]
fn a_missing_unreadable_or_misplaced_file_is_refused() {
    let mut exec = host("");
    exec.entries.remove(&HostPath::new(FILE).expect("path"));
    assert_eq!(why(read(&exec)), "untrusted-missing");
    let err = read(&exec).expect_err("missing").to_string();
    assert_eq!(err, format!("{FILE} is not trusted: it is missing"));

    let unreadable = HostPath::new("/unreadable").expect("path");
    let got = read_trusted(&host(""), &unreadable);
    assert_eq!(why(got), "untrusted-unreadable");

    exec.entries
        .insert(HostPath::new(FILE).expect("path"), dir(0o755, ROOT));
    assert_eq!(why(read(&exec)), "untrusted-link");
}

/// Asserts that a trusted file which is not an `installer.toml` is refused with the parser's
/// own reason (every problem it lists), and that one which is not UTF-8 is refused instead of
/// being read with replacement characters.
#[test]
fn a_trusted_file_with_bad_content_is_refused_with_the_parsers_reason() {
    let got = why(read(&host("release_source = \"carrier-pigeon\"\n")));
    assert!(got.starts_with("invalid: installer.toml:"), "{got}");
    assert!(got.contains("release_source"), "{got}");
    let mut exec = host("");
    exec.entries.insert(
        HostPath::new(FILE).expect("path"),
        file(b"database = \"\xff\"\n", 0o644, ROOT),
    );
    assert_eq!(why(read(&exec)), "invalid: installer.toml: it is not UTF-8");
}

/// Asserts that a trusted file written by an earlier build, with the `[proxy]` table (the slot
/// table of the removed two-slot supervision included) and the release-source keys it carries,
/// is read without a refusal: the keys are still understood, so the fetcher goes on reading the
/// file of a host whose proxy table has not been edited yet.
#[test]
fn a_trusted_file_with_the_proxy_table_and_the_release_source_keys_is_read() {
    let text = include_str!("../../../kbf-setup/tests/golden/all_in_one/installer.toml");
    assert!(text.contains("\n[proxy.upstream]\n"));
    let got = read(&host(text)).expect("trusted");
    assert_eq!(got, InstallerToml::parse(text).expect("parses"));
    assert!(got.proxy.is_some());
    assert_eq!(got.source.word(), "url");
}
