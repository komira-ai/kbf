use super::*;

/// `codesign --display --verbose=4` of an ad-hoc signed binary with the hardened
/// runtime, as macOS 15 prints it (abridged to the lines read).
const INFO: &str = "Executable=/x/kbf-daemon
Identifier=kbf_daemon-55554944b7a1
Format=Mach-O thin (arm64)
CodeDirectory v=20500 size=96143 flags=0x10002(adhoc,runtime) hashes=3000+7 location=embedded
Hash type=sha256 size=32
CDHash=0123456789abcdef0123456789abcdef01234567
Signature=adhoc
";

#[test]
fn the_code_directory_flags_are_read_from_their_line() {
    // Catches: flags read from another line, with the hex digits, or cut at the wrong
    // parenthesis, so `adhoc,runtime` is never seen (every signing refused) or a
    // linker-signed binary's flags taken for it.
    assert_eq!(code_directory_flags(INFO), Some("adhoc,runtime"));
    let linker = "CodeDirectory v=20400 size=100 flags=0x20002(adhoc,linker-signed) hashes=3\n";
    assert_eq!(code_directory_flags(linker), Some("adhoc,linker-signed"));
    for broken in [
        "",
        "Signature=adhoc\nflags=0x10002(adhoc,runtime)\n",
        "CodeDirectory v=20500 size=96143 hashes=3000+7\n",
        "CodeDirectory v=20500 flags=0x10002 hashes=3000+7\n",
        "CodeDirectory v=20500 flags=0x10002(adhoc,runtime\n",
    ] {
        assert_eq!(code_directory_flags(broken), None, "{broken:?}");
    }
}

#[test]
fn the_cdhash_and_minos_are_read_from_their_lines() {
    // Catches: a summary line naming another hash, and a minos read from the sdk line
    // or the first word of any line (a binary built for the wrong macOS passing).
    assert_eq!(
        cdhash(INFO),
        Some("0123456789abcdef0123456789abcdef01234567")
    );
    assert_eq!(cdhash("Signature=adhoc\n"), None);
    let build = "/x/kbf-daemon:\nLoad command 10\n      cmd LC_BUILD_VERSION\n  \
                 platform MACOS\n    minos 14.0\n      sdk 15.5\n";
    assert_eq!(minos(build), Some("14.0"));
    assert_eq!(minos("      sdk 15.5\n"), None);
    assert_eq!(minos("    minos\n"), None);
}

#[test]
fn sip_counts_as_enabled_only_when_csrutil_says_so() {
    // Catches: a SIP check that reads "disabled" as enabled, which would turn the
    // hosted runners' warning into a failure.
    assert!(sip_enabled(
        "System Integrity Protection status: enabled.\n"
    ));
    assert!(!sip_enabled(
        "System Integrity Protection status: disabled.\n"
    ));
    assert!(!sip_enabled(""));
}

#[test]
fn the_summary_names_the_entitlements_or_none() {
    // Catches: a summary that leaves the entitlements blank for kbf-daemon, or names
    // only the first of several.
    assert_eq!(granted(&[]), "none");
    assert_eq!(granted(&["a.b".to_owned(), "c.d".to_owned()]), "a.b c.d");
}

#[test]
fn another_host_is_told_the_verbs_need_macos() {
    // Catches: a message that does not say why the verb failed off macOS.
    assert_eq!(
        needs_macos("darwin-asset").to_string(),
        "darwin-asset runs on macOS only: it needs codesign, vtool and csrutil"
    );
}
