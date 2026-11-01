//! The profile text: what it denies, what it leaves out, and which paths it refuses.

use std::path::{Path, PathBuf};

use super::*;

fn profile(deny_network: bool, paths: &[&str]) -> Profile {
    Profile {
        deny_network,
        deny_read: paths.iter().map(PathBuf::from).collect(),
    }
}

/// Catches a denied Command whose profile lacks the network denial, or whose denial lost one of
/// the loopback and Unix-socket allows that follow it.
#[test]
fn a_denied_command_gets_the_network_deny_rules_in_order() {
    let text = profile(true, &[]).render().expect("renders");
    assert!(text.starts_with("(version 1)\n(allow default)\n"), "{text}");
    let at = text.find(DENY_NETWORK_RULES).expect("network rules");
    assert!(text[at..].starts_with("(deny network*)\n"));
    for allow in [
        "(allow network-inbound (local ip \"localhost:*\"))",
        "(allow network* (remote ip \"localhost:*\"))",
        "(allow network* (remote unix-socket))",
    ] {
        let allowed = text.find(allow).expect(allow);
        assert!(allowed > text.find("(deny network*)").unwrap(), "{allow}");
    }
}

/// Catches an allowed Command that is denied the network by a stray clause.
#[test]
fn an_allowed_command_has_no_network_clause() {
    let text = profile(false, &["/run/grant"]).render().expect("renders");
    assert!(!text.contains("(deny network*)"), "{text}");
    assert!(!text.contains("localhost"), "{text}");
    assert!(!text.contains("remote unix-socket"), "{text}");
}

/// Catches a file the profile forgets to deny, a deny written twice, and one that names the
/// wrong operation or a prefix instead of the file.
#[test]
fn each_unreadable_path_gets_exactly_one_read_deny() {
    let text = profile(true, &["/run/grant", "/run/put urls (1)"])
        .render()
        .expect("renders");
    let denies: Vec<&str> = text.lines().filter(|l| l.contains("file-read*")).collect();
    assert_eq!(
        denies,
        [
            "(deny file-read* (literal \"/run/grant\"))",
            "(deny file-read* (literal \"/run/put urls (1)\"))",
        ]
    );
    let none = profile(true, &[]).render().expect("renders");
    assert!(!none.contains("file-read*"), "{none}");
}

/// Catches a read deny that an allow after it would override: a later rule wins.
#[test]
fn the_file_denies_are_the_last_rules() {
    let text = profile(true, &["/run/grant"]).render().expect("renders");
    let last_allow = text.rfind("(allow ").expect("an allow");
    assert!(
        last_allow < text.find("(deny file-read*").unwrap(),
        "{text}"
    );
    assert!(text.ends_with("(deny file-write* (literal \"/run/grant\"))\n"));
}

/// Catches a credential file that can be renamed, unlinked or hard-linked (each makes a second
/// name the read deny does not cover): one write deny per file, with the same literal.
#[test]
fn each_unreadable_path_also_gets_exactly_one_write_deny() {
    let text = profile(false, &["/run/grant", "/run/put urls (1)"])
        .render()
        .expect("renders");
    let denies: Vec<&str> = text.lines().filter(|l| l.contains("file-write*")).collect();
    assert_eq!(
        denies,
        [
            "(deny file-write* (literal \"/run/grant\"))",
            "(deny file-write* (literal \"/run/put urls (1)\"))",
        ]
    );
}

/// Catches a path that could end the profile string and be read as rules: a quote, a backslash
/// (an escape), and a quote followed by a closing parenthesis and a new rule. A `)` alone stays
/// inside the quotes and is allowed.
#[test]
fn a_path_that_could_end_the_profile_string_is_refused() {
    for path in [
        "/run/a\"b",
        "/run/a\\b",
        "/run/x\"))\n(allow file-read* (subpath \"/",
        "/run/x\")(allow default)(\"",
    ] {
        let refused = profile(true, &[path]).render();
        assert!(refused.is_err(), "{path:?} gave {refused:?}");
    }
    let text = profile(true, &["/run/a)b(c"])
        .render()
        .expect("a paren is text");
    assert!(text.contains("(literal \"/run/a)b(c\")"), "{text}");
    assert_eq!(quoted(Path::new("/a b")).unwrap(), "\"/a b\"");
    assert!(quoted(Path::new("/a\"b")).is_err());
}

/// Catches a path with a control character (a newline starts a new rule line) or bytes that are
/// not UTF-8 being written into the profile.
#[test]
fn a_path_with_a_control_character_or_invalid_utf8_is_refused() {
    for path in ["/run/a\nb", "/run/a\tb", "/run/a\0b", "/run/a\rb"] {
        assert!(profile(true, &[path]).render().is_err(), "{path:?}");
    }
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt as _;
        let bytes = Path::new(std::ffi::OsStr::from_bytes(b"/run/\xff"));
        assert!(quoted(bytes).is_err());
    }
}

/// Catches a relative path in a read deny, which the sandbox would compare against nothing.
#[test]
fn a_relative_path_is_refused() {
    for path in ["grant", "./grant", "../run/grant", ""] {
        let refused = profile(true, &[path]).render();
        assert!(refused.is_err(), "{path:?} gave {refused:?}");
    }
    let error = profile(false, &["grant"]).render().unwrap_err();
    assert!(
        error.to_string().contains("not an absolute path"),
        "{error}"
    );
}

/// Catches a profile that loses the device rules or the denies that keep a process from handing
/// work to `launchd`.
#[test]
fn every_profile_keeps_the_launchd_and_device_denies() {
    for deny_network in [true, false] {
        let text = profile(deny_network, &[]).render().expect("renders");
        for rule in [
            "(deny job-creation)",
            "(deny lsopen)",
            "(deny user-preference-write)",
        ] {
            assert!(text.lines().any(|l| l == rule), "{rule}");
        }
        for rule in NO_DEVICE_RULES.lines() {
            assert!(text.lines().any(|l| l == rule), "{rule}");
        }
    }
}

/// Catches a credential file reached by renaming a directory above it: every ancestor but `/` has
/// one `file-write-unlink` deny, written once for paths that share it.
#[test]
fn every_directory_above_an_unreadable_path_cannot_be_renamed() {
    let text = profile(false, &["/run/a/grant", "/run/a/b/urls"])
        .render()
        .expect("renders");
    let denies: Vec<&str> = text
        .lines()
        .filter(|l| l.contains("file-write-unlink"))
        .collect();
    assert_eq!(
        denies,
        [
            "(deny file-write-unlink (literal \"/run/a\"))",
            "(deny file-write-unlink (literal \"/run\"))",
            "(deny file-write-unlink (literal \"/run/a/b\"))",
        ]
    );
    let last_allow = text.rfind("(allow ").expect("an allow");
    assert!(
        last_allow < text.find("file-write-unlink").unwrap(),
        "{text}"
    );
}
