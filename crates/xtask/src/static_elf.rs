//! `static-elf <binary>`: the binary is a statically linked ELF, so it runs in an image with
//! no libc and no dynamic loader (the task image holds nothing but the binary).
//!
//! It is refused when it has a `PT_INTERP` program header (it names a dynamic loader) or any
//! `DT_NEEDED` entry in its dynamic section (it needs a shared library). Only ELF64
//! little-endian files are read, which is what the static Linux targets build; any other
//! file, or a truncated one, is refused as not understood rather than passed.

use std::path::PathBuf;

use clap::Args as ClapArgs;

use crate::Error;

/// Program header type of the dynamic loader's name.
const PT_INTERP: u32 = 3;
/// Program header type of the dynamic section.
const PT_DYNAMIC: u32 = 2;
/// Dynamic entry tag of the end of the section.
const DT_NULL: u64 = 0;
/// Dynamic entry tag of a needed shared library.
const DT_NEEDED: u64 = 1;
/// The bytes of an ELF64 header, and of a program header and a dynamic entry.
const EHDR_BYTES: usize = 64;
const PHDR_BYTES: usize = 56;
const DYN_BYTES: usize = 16;

/// Flags of `static-elf`.
#[derive(Debug, ClapArgs)]
pub struct Args {
    /// The binary to check.
    pub binary: PathBuf,
}

/// `len` bytes of `bytes` at `at`, or why the file is cut short.
fn slice(bytes: &[u8], at: u64, len: usize) -> Result<&[u8], String> {
    usize::try_from(at)
        .ok()
        .and_then(|start| Some(start..start.checked_add(len)?))
        .and_then(|range| bytes.get(range))
        .ok_or_else(|| format!("truncated: {len} bytes wanted at offset {at}"))
}

fn u16_at(bytes: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([bytes[at], bytes[at + 1]])
}

fn u32_at(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes(bytes[at..at + 4].try_into().expect("4 bytes"))
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().expect("8 bytes"))
}

/// What makes `bytes`, an ELF64 little-endian file, dynamically linked: one line for a
/// `PT_INTERP` header and one for the `DT_NEEDED` entries. Empty for a static binary; an
/// error for a file this cannot read.
pub fn dynamic_links(bytes: &[u8]) -> Result<Vec<String>, String> {
    let header = slice(bytes, 0, EHDR_BYTES)?;
    if header[..4] != *b"\x7fELF" {
        return Err("not an ELF file".to_owned());
    }
    if header[4] != 2 || header[5] != 1 {
        return Err("not an ELF64 little-endian file".to_owned());
    }
    let phoff = u64_at(header, 32);
    let phentsize = usize::from(u16_at(header, 54));
    let phnum = u64::from(u16_at(header, 56));
    if phentsize < PHDR_BYTES {
        return Err(format!("program header size {phentsize} is too small"));
    }
    let mut interp = false;
    let mut needed = 0usize;
    for i in 0..phnum {
        let at = phoff.saturating_add(i * phentsize as u64);
        let phdr = slice(bytes, at, PHDR_BYTES)?;
        match u32_at(phdr, 0) {
            PT_INTERP => interp = true,
            PT_DYNAMIC => {
                needed += dynamic_section(bytes, u64_at(phdr, 8), u64_at(phdr, 32))?;
            }
            _ => {}
        }
    }
    let mut found = Vec::new();
    if interp {
        found.push("it has a PT_INTERP program header (it names a dynamic loader)".to_owned());
    }
    if needed > 0 {
        found.push(format!(
            "it has {needed} DT_NEEDED entries (shared libraries)"
        ));
    }
    Ok(found)
}

/// The `DT_NEEDED` entries of the dynamic section at file offset `offset`, `size` bytes
/// long, up to its `DT_NULL`.
fn dynamic_section(bytes: &[u8], offset: u64, size: u64) -> Result<usize, String> {
    let mut needed = 0;
    for i in 0..size / DYN_BYTES as u64 {
        let entry = slice(
            bytes,
            offset.saturating_add(i * DYN_BYTES as u64),
            DYN_BYTES,
        )?;
        match u64_at(entry, 0) {
            DT_NULL => break,
            DT_NEEDED => needed += 1,
            _ => {}
        }
    }
    Ok(needed)
}

/// Check the binary: one line saying it is static, or why it is not.
pub fn run(args: &Args) -> Result<Vec<String>, Error> {
    let name = args.binary.display().to_string();
    let bytes = std::fs::read(&args.binary).map_err(Error::spawn(format!("read {name}")))?;
    let found = dynamic_links(&bytes).map_err(|why| Error::Failed(format!("{name}: {why}")))?;
    if found.is_empty() {
        return Ok(vec![format!("{name}: statically linked")]);
    }
    Err(Error::Refused(
        found
            .into_iter()
            .map(|why| format!("{name} is not statically linked: {why}"))
            .collect(),
    ))
}
