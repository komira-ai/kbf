//! What a staged binary is: the kind of executable it is by its header, and what it says
//! about itself with `--version --json`.
//!
//! The kind is read from the first bytes of the file, never by running it: an attested
//! file for another platform is refused before anything executes it. The version is what
//! `kbf --version --json` prints (crates/kbf-cli/src/version.rs): `commit` (the first 12
//! hex digits), `min_protocol`, `protocol`, `profile`, `state_schema` and `version`.
//! It is a consistency check on a file the attestation already vouches for, not trust.

use std::fmt;

use serde::Deserialize;

use crate::CommitId;
use crate::layout::Platform;
use crate::refusal::VersionFault;

/// The CPU a binary is built for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    /// The CPU this program was built for.
    pub fn current() -> Arch {
        Arch::named(std::env::consts::ARCH)
    }

    /// The CPU Rust calls `name` (`aarch64`, else x86-64).
    pub fn named(name: &str) -> Arch {
        if name == "aarch64" {
            Arch::Aarch64
        } else {
            Arch::X86_64
        }
    }
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Arch::X86_64 => "x86_64",
            Arch::Aarch64 => "arm64",
        })
    }
}

/// The kind of executable a file's header says it is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BinaryKind {
    /// A 64-bit little-endian ELF executable or shared object.
    Elf(Arch),
    /// A 64-bit little-endian thin Mach-O.
    MachO(Arch),
    /// Anything else: a script, a fat Mach-O, a 32-bit file, a truncated one.
    Other,
}

impl fmt::Display for BinaryKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BinaryKind::Elf(arch) => write!(f, "an ELF {arch} binary"),
            BinaryKind::MachO(arch) => write!(f, "a Mach-O {arch} binary"),
            BinaryKind::Other => f.write_str("neither an ELF nor a thin Mach-O binary"),
        }
    }
}

impl BinaryKind {
    /// The kind of the file that starts with `header` (at least its first 20 bytes).
    pub fn of(header: &[u8]) -> BinaryKind {
        let le16 = |at: usize| {
            header
                .get(at..at + 2)
                .map(|b| u16::from_le_bytes([b[0], b[1]]))
        };
        let le32 = |at: usize| {
            header
                .get(at..at + 4)
                .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
        };
        // 0x7f "ELF", 64-bit (class 2), little-endian (data 1); e_machine at offset 18.
        if header.starts_with(b"\x7fELF") && header.get(4) == Some(&2) && header.get(5) == Some(&1)
        {
            return match le16(18) {
                Some(0x3e) => BinaryKind::Elf(Arch::X86_64),
                Some(0xb7) => BinaryKind::Elf(Arch::Aarch64),
                _ => BinaryKind::Other,
            };
        }
        // MH_MAGIC_64 as a little-endian file starts cf fa ed fe; cputype at offset 4.
        if header.starts_with(&[0xcf, 0xfa, 0xed, 0xfe]) {
            return match le32(4) {
                Some(0x0100_0007) => BinaryKind::MachO(Arch::X86_64),
                Some(0x0100_000c) => BinaryKind::MachO(Arch::Aarch64),
                _ => BinaryKind::Other,
            };
        }
        BinaryKind::Other
    }

    /// The kind of binary a host of `platform` on `arch` runs.
    pub fn wanted(platform: Platform, arch: Arch) -> BinaryKind {
        match platform {
            Platform::Linux => BinaryKind::Elf(arch),
            Platform::MacOs => BinaryKind::MachO(arch),
        }
    }
}

/// What `--version --json` prints.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub struct Version {
    pub commit: String,
    pub min_protocol: u64,
    pub protocol: u64,
    pub profile: String,
    pub state_schema: u64,
    pub version: String,
}

impl Version {
    /// The object a binary printed. Fields beyond these are ignored: a later build may
    /// report more.
    pub fn parse(stdout: &[u8]) -> Result<Version, VersionFault> {
        serde_json::from_slice(stdout).map_err(|e| VersionFault::Unreadable(e.to_string()))
    }

    /// Whether this build is `commit`'s release build: it names the commit's first 12 hex
    /// digits and is a `release` profile, not a debug build.
    pub fn check(&self, commit: &CommitId) -> Result<(), VersionFault> {
        let want = &commit.as_str()[..12];
        if self.commit != want {
            return Err(VersionFault::Commit {
                want: want.to_owned(),
                got: self.commit.clone(),
            });
        }
        if self.profile != "release" {
            return Err(VersionFault::Profile(self.profile.clone()));
        }
        Ok(())
    }
}

#[cfg(test)]
#[path = "binary_tests.rs"]
mod tests;
