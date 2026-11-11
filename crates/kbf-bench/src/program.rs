//! `./noop`: the program a `noop` action runs, a static Linux executable that exits 0
//! and does nothing else.
//!
//! It is written here byte for byte, not built: one ELF header, one program header that
//! loads the whole file read-and-execute at [`LOAD_ADDRESS`], and three instructions,
//! `exit(0)` by the raw system call. It links no library and opens no file, so it runs
//! in any root file system the action is given (the base image's or a bare one), and its
//! bytes, and so its digest, are the same on every run of `kbf-bench`.

/// The machine the program is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Arch {
    /// 64-bit x86 (`linux-x86_64`).
    #[value(name = "x86_64")]
    X86_64,
    /// 64-bit Arm (`linux-arm64`).
    #[value(name = "aarch64")]
    Aarch64,
}

/// Where the program is loaded: the whole file, the headers included.
pub const LOAD_ADDRESS: u64 = 0x40_0000;

/// The ELF header's size, and where the one program header starts.
const ELF_HEADER: u16 = 64;

/// The size of one 64-bit program header.
const PROGRAM_HEADER: u16 = 56;

impl Arch {
    /// The machine of `name`, as Rust names it (`std::env::consts::ARCH`); `None` for
    /// one the program is not written for.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "x86_64" => Some(Self::X86_64),
            "aarch64" => Some(Self::Aarch64),
            _ => None,
        }
    }

    /// The flag value.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::X86_64 => "x86_64",
            Self::Aarch64 => "aarch64",
        }
    }

    /// The ELF `e_machine` value.
    const fn machine(self) -> u16 {
        match self {
            Self::X86_64 => 62,
            Self::Aarch64 => 183,
        }
    }

    /// `exit(0)`, as machine code.
    const fn code(self) -> &'static [u8] {
        match self {
            // mov eax, 60 (exit); xor edi, edi; syscall
            Self::X86_64 => &[0xb8, 0x3c, 0, 0, 0, 0x31, 0xff, 0x0f, 0x05],
            // movz x0, #0; movz x8, #93 (exit); svc #0
            Self::Aarch64 => &[
                0x00, 0x00, 0x80, 0xd2, 0xa8, 0x0b, 0x80, 0xd2, 0x01, 0x00, 0x00, 0xd4,
            ],
        }
    }
}

/// The program for `arch`: an executable ELF file whose entry point is its code, right
/// after the two headers.
#[must_use]
pub fn noop(arch: Arch) -> Vec<u8> {
    let code = arch.code();
    let start = u64::from(ELF_HEADER + PROGRAM_HEADER);
    let size = start + code.len() as u64;
    let mut elf = Vec::with_capacity(size as usize);
    // e_ident: the magic, 64-bit, little-endian, version 1, the System V ABI, padding.
    elf.extend_from_slice(&[0x7f, b'E', b'L', b'F', 2, 1, 1, 0]);
    elf.extend_from_slice(&[0; 8]);
    elf.extend_from_slice(&2_u16.to_le_bytes()); // e_type: an executable
    elf.extend_from_slice(&arch.machine().to_le_bytes());
    elf.extend_from_slice(&1_u32.to_le_bytes()); // e_version
    elf.extend_from_slice(&(LOAD_ADDRESS + start).to_le_bytes()); // e_entry
    elf.extend_from_slice(&u64::from(ELF_HEADER).to_le_bytes()); // e_phoff
    elf.extend_from_slice(&0_u64.to_le_bytes()); // e_shoff: no sections
    elf.extend_from_slice(&0_u32.to_le_bytes()); // e_flags
    elf.extend_from_slice(&ELF_HEADER.to_le_bytes()); // e_ehsize
    elf.extend_from_slice(&PROGRAM_HEADER.to_le_bytes()); // e_phentsize
    elf.extend_from_slice(&1_u16.to_le_bytes()); // e_phnum
    elf.extend_from_slice(&[0; 6]); // e_shentsize, e_shnum, e_shstrndx
    elf.extend_from_slice(&1_u32.to_le_bytes()); // p_type: PT_LOAD
    elf.extend_from_slice(&5_u32.to_le_bytes()); // p_flags: read and execute
    elf.extend_from_slice(&0_u64.to_le_bytes()); // p_offset: the whole file
    elf.extend_from_slice(&LOAD_ADDRESS.to_le_bytes()); // p_vaddr
    elf.extend_from_slice(&LOAD_ADDRESS.to_le_bytes()); // p_paddr
    elf.extend_from_slice(&size.to_le_bytes()); // p_filesz
    elf.extend_from_slice(&size.to_le_bytes()); // p_memsz
    elf.extend_from_slice(&0x1000_u64.to_le_bytes()); // p_align
    elf.extend_from_slice(code);
    elf
}
