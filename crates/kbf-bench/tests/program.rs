//! `./noop`, the program a `noop` action runs: the program for this machine is run and
//! exits 0, and both programs' headers say what they are.

use kbf_bench::program::{self, Arch, LOAD_ADDRESS};

fn u16_at(elf: &[u8], at: usize) -> u16 {
    u16::from_le_bytes(elf[at..at + 2].try_into().expect("two bytes"))
}

fn u64_at(elf: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(elf[at..at + 8].try_into().expect("eight bytes"))
}

/// Catches a program a kernel would refuse or misload: not a 64-bit little-endian
/// executable, for the wrong machine, with its entry point anywhere but its code, or a
/// load segment that is not the whole file read-and-execute.
#[test]
fn each_program_is_one_executable_segment_entered_at_its_code() {
    for (arch, machine, code) in [(Arch::X86_64, 62, 9), (Arch::Aarch64, 183, 12)] {
        let elf = program::noop(arch);
        assert_eq!(elf.len(), 120 + code, "{arch:?}");
        assert_eq!(&elf[..8], b"\x7fELF\x02\x01\x01\x00", "{arch:?}");
        assert_eq!(u16_at(&elf, 16), 2, "an executable");
        assert_eq!(u16_at(&elf, 18), machine, "{arch:?}");
        assert_eq!(
            u64_at(&elf, 24),
            LOAD_ADDRESS + 120,
            "the entry is the code"
        );
        assert_eq!(
            u64_at(&elf, 32),
            64,
            "the program header follows the ELF header"
        );
        assert_eq!(u16_at(&elf, 56), 1, "one program header");
        assert_eq!(
            &elf[64..72],
            [1, 0, 0, 0, 5, 0, 0, 0],
            "PT_LOAD, read and execute"
        );
        assert_eq!(u64_at(&elf, 64 + 8), 0, "from the file's start");
        assert_eq!(u64_at(&elf, 64 + 16), LOAD_ADDRESS);
        assert_eq!(u64_at(&elf, 64 + 32), elf.len() as u64, "the whole file");
        assert_eq!(u64_at(&elf, 64 + 40), elf.len() as u64);
    }
    assert_ne!(program::noop(Arch::X86_64), program::noop(Arch::Aarch64));
}

/// Catches a name that does not round-trip, so `--arch` (or the default from the
/// machine kbf-bench runs on) would pick the other program or none.
#[test]
fn an_arch_is_named_as_rust_names_it() {
    for arch in [Arch::X86_64, Arch::Aarch64] {
        assert_eq!(Arch::from_name(arch.name()), Some(arch));
    }
    assert_eq!(Arch::from_name("arm64"), None);
    assert_eq!(Arch::from_name("riscv64"), None);
}

/// Catches a program that does not run, or exits other than 0: the one for this machine
/// is written to a scratch file and run. Linux only: the program is a Linux executable,
/// and kbf-bench's `noop` runs against the Linux farm.
#[cfg(target_os = "linux")]
#[test]
fn the_program_for_this_machine_runs_and_exits_0() {
    use std::os::unix::fs::PermissionsExt as _;

    let arch = Arch::from_name(std::env::consts::ARCH).expect("a machine kbf-bench writes for");
    let dir = std::path::Path::new(env!("CARGO_TARGET_TMPDIR")).join("kbf-bench-program");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create scratch");
    let path = dir.join("noop");
    std::fs::write(&path, program::noop(arch)).expect("write");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    let status = std::process::Command::new(&path)
        .status()
        .expect("run ./noop");
    assert_eq!(status.code(), Some(0), "{status}");
}
