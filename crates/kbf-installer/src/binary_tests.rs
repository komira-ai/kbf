use super::*;
use crate::fixtures::{elf, macho};

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
