//! `static-elf` (`xtask::static_elf`): a binary with a dynamic loader or a needed library
//! is refused, read from its ELF headers.

use std::process::Command;

use xtask::Error;
use xtask::static_elf::{Args, dynamic_links, run};

/// One program header: type, file offset, file size.
fn phdr(kind: u32, offset: u64, size: u64) -> Vec<u8> {
    let mut h = vec![0u8; 56];
    h[..4].copy_from_slice(&kind.to_le_bytes());
    h[8..16].copy_from_slice(&offset.to_le_bytes());
    h[32..40].copy_from_slice(&size.to_le_bytes());
    h
}

/// An ELF64 little-endian file: the header, the program headers right after it, then
/// `tail` (where a dynamic section goes).
fn elf(phdrs: &[Vec<u8>], tail: &[u8]) -> Vec<u8> {
    let mut b = vec![0u8; 64];
    b[..4].copy_from_slice(b"\x7fELF");
    b[4] = 2;
    b[5] = 1;
    b[32..40].copy_from_slice(&64u64.to_le_bytes());
    b[54..56].copy_from_slice(&56u16.to_le_bytes());
    b[56..58].copy_from_slice(&(phdrs.len() as u16).to_le_bytes());
    for p in phdrs {
        b.extend_from_slice(p);
    }
    b.extend_from_slice(tail);
    b
}

/// A dynamic section of `(tag, value)` entries.
fn dynamic(entries: &[(u64, u64)]) -> Vec<u8> {
    entries
        .iter()
        .flat_map(|(t, v)| t.to_le_bytes().into_iter().chain(v.to_le_bytes()))
        .collect()
}

/// An ELF with one PT_DYNAMIC over `entries`.
fn with_dynamic(entries: &[(u64, u64)]) -> Vec<u8> {
    let section = dynamic(entries);
    let at = 64 + 56;
    elf(&[phdr(2, at, section.len() as u64)], &section)
}

/// Catches a static binary refused, and a PT_LOAD-only file read as dynamic.
#[test]
fn a_static_binary_has_nothing_dynamic() {
    assert_eq!(dynamic_links(&elf(&[phdr(1, 0, 0)], &[])), Ok(vec![]));
    assert_eq!(dynamic_links(&elf(&[], &[])), Ok(vec![]));
    // A static-pie binary has a dynamic section with no DT_NEEDED.
    assert_eq!(dynamic_links(&with_dynamic(&[(6, 1), (0, 0)])), Ok(vec![]));
}

/// Catches a dynamic loader going unnoticed.
#[test]
fn a_loader_is_found() {
    let found = dynamic_links(&elf(&[phdr(3, 0, 0)], &[])).unwrap();
    assert_eq!(found.len(), 1);
    assert!(found[0].contains("PT_INTERP"), "{found:?}");
}

/// Catches a needed library going unnoticed, one counted after the end of the section, and
/// one past DT_NULL counted.
#[test]
fn needed_libraries_are_counted_up_to_the_end_of_the_section() {
    let found = dynamic_links(&with_dynamic(&[(1, 5), (1, 9), (0, 0), (1, 12)])).unwrap();
    assert_eq!(found.len(), 1);
    assert!(found[0].contains("2 DT_NEEDED"), "{found:?}");
    // No DT_NULL: the section's size ends it.
    let found = dynamic_links(&with_dynamic(&[(1, 5)])).unwrap();
    assert!(found[0].contains("1 DT_NEEDED"), "{found:?}");
    // A loader and a library are both said.
    let section = dynamic(&[(1, 5), (0, 0)]);
    let both = elf(&[phdr(3, 0, 0), phdr(2, 64 + 112, 32)], &section);
    assert_eq!(dynamic_links(&both).unwrap().len(), 2);
}

/// Catches a file this cannot read being passed: not ELF, 32-bit, big-endian, cut short in
/// the header, the program headers or the dynamic section, a program header size too small
/// and an offset past the end.
#[test]
fn a_file_that_cannot_be_read_is_an_error() {
    let err = |bytes: &[u8]| dynamic_links(bytes).unwrap_err();
    // A text file, not an ELF one: too short, then long enough but without the ELF magic.
    assert!(err(b"plain text\n").contains("truncated"));
    let mut text = vec![0u8; 64];
    text[..10].copy_from_slice(b"plain text");
    assert!(err(&text).contains("not an ELF file"));
    let mut b32 = elf(&[], &[]);
    b32[4] = 1;
    assert!(err(&b32).contains("ELF64"));
    let mut big = elf(&[], &[]);
    big[5] = 2;
    assert!(err(&big).contains("little-endian"));
    let mut small = elf(&[phdr(1, 0, 0)], &[]);
    small[54..56].copy_from_slice(&40u16.to_le_bytes());
    assert!(err(&small).contains("too small"));
    let whole = elf(&[phdr(1, 0, 0)], &[]);
    assert!(err(&whole[..100]).contains("truncated"));
    let cut = with_dynamic(&[(1, 5), (0, 0)]);
    assert!(err(&cut[..cut.len() - 8]).contains("truncated"));
    let mut far = elf(&[phdr(1, 0, 0)], &[]);
    far[32..40].copy_from_slice(&u64::MAX.to_le_bytes());
    assert!(err(&far).contains("truncated"));
}

/// Catches `run` passing a dynamic file, losing the file's name from the message, or
/// reading a file that is not there as a pass.
#[test]
fn run_names_the_file_and_the_reason() {
    let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "xtask-static-elf");
    let write = |name: &str, bytes: Vec<u8>| {
        let path = dir.join(name);
        std::fs::write(&path, bytes).expect("write");
        Args { binary: path }
    };
    let lines = run(&write("static", elf(&[], &[]))).unwrap();
    assert!(lines[0].ends_with("static: statically linked"), "{lines:?}");
    let Err(Error::Refused(said)) = run(&write("dyn", elf(&[phdr(3, 0, 0)], &[]))) else {
        panic!("a binary with a loader passed");
    };
    assert!(said[0].contains("dyn is not statically linked"), "{said:?}");
    let Err(Error::Failed(said)) = run(&write("junk", b"junk".to_vec())) else {
        panic!("a file that is not ELF passed");
    };
    assert!(said.contains("junk"), "{said}");
    let missing = Args {
        binary: dir.join("absent"),
    };
    assert!(matches!(run(&missing), Err(Error::Spawn { .. })));
}

/// Catches the verb not being wired to the binary or its exit status.
#[test]
fn the_binary_exits_1_for_a_dynamic_file() {
    let dir = kbf_testkit::dir::TestDir::under(std::env::temp_dir(), "xtask-static-elf-bin");
    let run = |bytes: Vec<u8>| {
        let path = dir.join("bin");
        std::fs::write(&path, bytes).expect("write");
        Command::new(env!("CARGO_BIN_EXE_xtask"))
            .args(["static-elf"])
            .arg(&path)
            .output()
            .expect("run xtask")
    };
    let out = run(elf(&[phdr(3, 0, 0)], &[]));
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not statically linked"));
    assert_eq!(run(elf(&[], &[])).status.code(), Some(0));
}
