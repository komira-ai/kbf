use super::*;
use crate::fixtures::{COMMIT, OTHER, elf, macho, version_json};

/// Catches a header read as the wrong kind or CPU: ELF and Mach-O, for both CPUs each,
/// are told apart; and what only looks like one is `Other`: a 32-bit ELF, a big-endian
/// one, a ELF for another machine (RISC-V 243), a fat Mach-O, a Mach-O for another CPU,
/// a script, a truncated header and nothing.
///
/// Mutant seen red: e_machine read from the wrong offset (17).
#[test]
fn a_header_says_what_kind_of_binary_a_file_is() {
    assert_eq!(
        BinaryKind::of(&elf(0x3e, "")),
        BinaryKind::Elf(Arch::X86_64)
    );
    assert_eq!(
        BinaryKind::of(&elf(0xb7, "")),
        BinaryKind::Elf(Arch::Aarch64)
    );
    assert_eq!(
        BinaryKind::of(&macho(0x0100_000c, "")),
        BinaryKind::MachO(Arch::Aarch64)
    );
    assert_eq!(
        BinaryKind::of(&macho(0x0100_0007, "")),
        BinaryKind::MachO(Arch::X86_64)
    );

    let mut elf32 = elf(0x3e, "");
    elf32[4] = 1;
    let mut big_endian = elf(0x3e, "");
    big_endian[5] = 2;
    let others: [&[u8]; 8] = [
        &elf32,
        &big_endian,
        &elf(243, ""),
        &[0xca, 0xfe, 0xba, 0xbe, 0, 0, 0, 2],
        &macho(0x0000_000c, ""),
        b"#!/usr/bin/env kbf-no-such-program\necho hi\n",
        &elf(0x3e, "")[..19],
        b"",
    ];
    for header in others {
        assert_eq!(BinaryKind::of(header), BinaryKind::Other, "{header:?}");
    }
    // A Mach-O header cut before its CPU type.
    assert_eq!(
        BinaryKind::of(&[0xcf, 0xfa, 0xed, 0xfe, 0x0c]),
        BinaryKind::Other
    );
}

/// Catches the wanted kind mixed up: Linux runs ELF, macOS Mach-O, each for the host's CPU.
#[test]
fn a_platform_wants_its_own_kind_for_its_cpu() {
    assert_eq!(
        BinaryKind::wanted(Platform::Linux, Arch::Aarch64),
        BinaryKind::Elf(Arch::Aarch64)
    );
    assert_eq!(
        BinaryKind::wanted(Platform::MacOs, Arch::X86_64),
        BinaryKind::MachO(Arch::X86_64)
    );
    assert_eq!(Arch::named("aarch64"), Arch::Aarch64);
    assert_eq!(Arch::named("x86_64"), Arch::X86_64);
    assert_eq!(
        Arch::current() == Arch::Aarch64,
        cfg!(target_arch = "aarch64")
    );
}

/// Catches the kinds printed wrongly in a refusal, which an operator reads.
#[test]
fn the_kinds_print_for_an_operator() {
    assert_eq!(
        BinaryKind::Elf(Arch::X86_64).to_string(),
        "an ELF x86_64 binary"
    );
    assert_eq!(
        BinaryKind::MachO(Arch::Aarch64).to_string(),
        "a Mach-O arm64 binary"
    );
    assert_eq!(
        BinaryKind::Other.to_string(),
        "neither an ELF nor a thin Mach-O binary"
    );
}

/// Catches `--version --json` read loosely: the six fields are read (and `platform`, which
/// a build may add), and a missing field, a field of the wrong type, a field the contract
/// does not list, one named twice, text that is not JSON and anything after the object
/// are refused.
///
/// Mutant seen red: `deny_unknown_fields` dropped (the extra field is then accepted).
#[test]
fn the_version_object_is_read_by_its_fields() {
    let v = Version::parse(version_json(COMMIT, "release").as_bytes()).expect("parses");
    assert_eq!(
        v,
        Version {
            admin_protocol: Some(1),
            meta_log: Some(4),
            commit: COMMIT[..12].to_owned(),
            min_protocol: 3,
            protocol: 4,
            profile: "release".to_owned(),
            state_schema: 7,
            version: "0.1.0".to_owned(),
            platform: None,
        }
    );
    // A build before `admin_protocol` and `meta_log` omits them: unknown, not refused.
    let old = r#"{"commit":"0123456789ab","min_protocol":3,"profile":"release","protocol":4,"state_schema":7,"version":"0.1.0"}"#;
    let old = Version::parse(old.as_bytes()).expect("an older build parses");
    assert_eq!((old.admin_protocol, old.meta_log), (None, None));
    let said = version_json(COMMIT, "release").replace('}', ",\"platform\":\"linux-arm64\"}");
    assert_eq!(
        Version::parse(said.as_bytes()).expect("parses").platform,
        Some("linux-arm64".to_owned())
    );
    let more = version_json(COMMIT, "release").replace('}', ",\"future\":true}");
    let twice = version_json(COMMIT, "release").replace('}', ",\"protocol\":4}");
    let after = format!("{}{{}}", version_json(COMMIT, "release"));
    for bad in [
        more.as_str(),
        twice.as_str(),
        after.as_str(),
        r#"{"commit":"abc"}"#,
        "kbf 0.1.0",
        "",
        &version_json(COMMIT, "release").replace("\"protocol\":4", "\"protocol\":\"4\""),
    ] {
        let err = Version::parse(bad.as_bytes()).expect_err(bad);
        assert_eq!(err.code(), "version-unreadable", "{bad}");
    }
}

/// Catches an unbounded read of what a not yet trusted binary prints: more than 4 KiB is
/// refused whatever it holds, and exactly 4 KiB of valid JSON (padded with spaces) is read.
///
/// Mutant seen red: the length check dropped.
#[test]
fn a_version_object_is_read_up_to_four_kib() {
    let json = version_json(COMMIT, "release");
    let fits = format!("{json}{}", " ".repeat(VERSION_CAP - json.len()));
    assert_eq!(fits.len(), VERSION_CAP);
    Version::parse(fits.as_bytes()).expect("4 KiB fits");
    let big = format!("{fits} ");
    assert_eq!(
        Version::parse(big.as_bytes()).expect_err("over").code(),
        "version-oversize"
    );
}

/// Catches each check of a version being skipped: the commit (the first 12 digits of the
/// offered one) and the profile (a debug build is not a release). Each is its own refusal.
///
/// Mutants seen red: the commit comparison dropped; the profile comparison dropped.
#[test]
fn a_version_must_name_the_commit_and_be_a_release_build() {
    let commit = CommitId::new(COMMIT).expect("commit");
    let release = Version::parse(version_json(COMMIT, "release").as_bytes()).expect("parses");
    release.check(&commit).expect("the release of the commit");

    let other = Version::parse(version_json(OTHER, "release").as_bytes()).expect("parses");
    let err = other.check(&commit).expect_err("another commit");
    assert_eq!(err.code(), "version-commit");
    assert_eq!(
        err.to_string(),
        format!(
            "it says commit {}, and the release is {}",
            &OTHER[..12],
            &COMMIT[..12]
        )
    );

    let debug = Version::parse(version_json(COMMIT, "debug").as_bytes()).expect("parses");
    let err = debug.check(&commit).expect_err("debug");
    assert_eq!(err.code(), "version-profile");
}
