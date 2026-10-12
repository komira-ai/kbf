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

/// `csrutil status` on a host whose SIP was set up with `csrutil enable --without ...`.
const CUSTOM_SIP: &str = "System Integrity Protection status: unknown (Custom Configuration).

Configuration:
\tApple Internal: disabled
\tKext Signing: enabled
\tFilesystem Protections: disabled
\tDebugging Restrictions: enabled
\tDTrace Restrictions: enabled
\tNVRAM Protections: enabled
\tBaseSystem Verification: enabled

This is an unsupported configuration, likely to break in the future and leave your machine in an unknown state.
";

#[test]
fn sip_counts_as_enabled_only_when_csrutil_says_so() {
    // Catches: a SIP check that reads "disabled" as enabled, which would turn the
    // hosted runners' warning into a failure; and one that finds "enabled" anywhere,
    // which reads a custom configuration (SIP partly off, each protection on its own
    // line) as enabled.
    assert!(sip_enabled(
        "System Integrity Protection status: enabled.\n"
    ));
    assert!(!sip_enabled(
        "System Integrity Protection status: disabled.\n"
    ));
    assert!(!sip_enabled(CUSTOM_SIP));
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

/// The bytes the test tarballs hold as `kbf-daemon`.
const BINARY: &[u8] = b"kbf-daemon's bytes";

/// A ustar header as `package` writes it, for `name` of `kind` holding `size` bytes.
fn header(name: &str, kind: tar::EntryType, size: usize) -> tar::Header {
    let mut h = tar::Header::new_ustar();
    h.set_path(name).expect("path");
    h.set_entry_type(kind);
    h.set_size(size as u64);
    h.set_mode(0o755);
    h.set_uid(0);
    h.set_gid(0);
    h.set_mtime(0);
    h
}

/// The tar of `entries`, each a header and its data, uncompressed.
fn tar_of(entries: Vec<(tar::Header, &[u8])>) -> Vec<u8> {
    let mut b = tar::Builder::new(Vec::new());
    for (mut h, data) in entries {
        h.set_cksum();
        b.append(&h, data).expect("append");
    }
    b.into_inner().expect("tar")
}

fn gzip(tar: &[u8]) -> Vec<u8> {
    use std::io::Write;
    let mut e = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    e.write_all(tar).expect("gzip");
    e.finish().expect("gzip")
}

fn tgz_of(entries: Vec<(tar::Header, &[u8])>) -> Vec<u8> {
    gzip(&tar_of(entries))
}

/// What the row tests, the name asked for, the tarball, and what must come back.
type Row = (
    &'static str,
    &'static str,
    Vec<u8>,
    Result<Vec<u8>, crate::Error>,
);

#[test]
fn darwin_asset_takes_only_the_one_entry_package_writes() {
    // Catches: darwin-asset checking the signature of a binary taken out of a tarball
    // that is not the one `package` writes (the refusal dropped, so a tarball with
    // more in it, or a link in place of the binary, would ship once its binary
    // passed), and a binary looked up by another name or outside the tarball's top.
    let at = Path::new("dist/kbf-daemon-darwin-arm64.tar.gz");
    let n = "kbf-daemon";
    let regular = || header(n, tar::EntryType::Regular, BINARY.len());
    let refused = |problems: &[&str]| {
        Err(crate::Error::Refused(
            problems
                .iter()
                .map(|p| format!("{}: {p}", at.display()))
                .collect(),
        ))
    };
    let holds_none = || {
        Err(crate::Error::Failed(format!(
            "{} holds no {n}",
            at.display()
        )))
    };
    let mut symlink = header(n, tar::EntryType::Symlink, 0);
    symlink.set_link_name("kbf-server").expect("link");
    let mut hard_link = header(n, tar::EntryType::Link, 0);
    hard_link.set_link_name("kbf-server").expect("link");
    let whole = tgz_of(vec![(regular(), BINARY)]);
    // A tar that ends inside the entry's data, and a gzip stream that does: a big
    // binary that does not compress, so the cut falls in its data.
    let tar_cut = gzip(&tar_of(vec![(regular(), BINARY)])[..520]);
    let mut seed = 1_u32;
    let big: Vec<u8> = (0..65_536)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            seed.to_be_bytes()[0]
        })
        .collect();
    let big_tgz = crate::package::tarball(n, &big).expect("tarball");
    let gzip_cut = big_tgz[..big_tgz.len() / 2].to_vec();
    let rows: Vec<Row> = vec![
        (
            "the package tarball",
            n,
            crate::package::tarball(n, BINARY).expect("tarball"),
            Ok(BINARY.to_vec()),
        ),
        ("one good entry", n, whole.clone(), Ok(BINARY.to_vec())),
        (
            "a path for a name",
            "bin/kbf-daemon",
            whole.clone(),
            Err(crate::Error::Failed(
                "\"bin/kbf-daemon\" is not a file name".to_owned(),
            )),
        ),
        (
            "the parent for a name",
            "..",
            whole,
            Err(crate::Error::Failed("\"..\" is not a file name".to_owned())),
        ),
        (
            "wrong name",
            n,
            tgz_of(vec![(
                header("kbf-server", tar::EntryType::Regular, BINARY.len()),
                BINARY,
            )]),
            holds_none(),
        ),
        (
            "extra entry",
            n,
            tgz_of(vec![
                (regular(), BINARY),
                (
                    header("README", tar::EntryType::Regular, BINARY.len()),
                    BINARY,
                ),
            ]),
            refused(&["README: the name is not \"kbf-daemon\"", "2 entries, not 1"]),
        ),
        (
            "a directory entry",
            n,
            tgz_of(vec![
                (header("bin/", tar::EntryType::Directory, 0), b""),
                (regular(), BINARY),
            ]),
            refused(&[
                "bin/: the name is not \"kbf-daemon\"",
                "bin/: the type is Directory, not a regular file",
                "bin/: the content is not the binary's bytes",
                "2 entries, not 1",
            ]),
        ),
        (
            "a symlink",
            n,
            tgz_of(vec![(symlink, b"")]),
            refused(&["kbf-daemon: the type is Symlink, not a regular file"]),
        ),
        (
            "a hard link",
            n,
            tgz_of(vec![(hard_link, b"")]),
            refused(&["kbf-daemon: the type is Link, not a regular file"]),
        ),
        (
            "two entries",
            n,
            tgz_of(vec![(regular(), BINARY), (regular(), BINARY)]),
            refused(&["2 entries, not 1"]),
        ),
        (
            "a header that does not parse",
            n,
            gzip(&[b'x'; 1024]),
            holds_none(),
        ),
        (
            "a tar cut in the binary",
            n,
            tar_cut,
            refused(&["not a tar: unexpected EOF during skip"]),
        ),
        ("a gzip stream cut in the binary", n, gzip_cut, holds_none()),
    ];
    for (what, name, tgz, want) in rows {
        assert_eq!(packaged_binary(at, &tgz, name), want, "{what}");
    }
}
