//! The Mac unpack itself: what it tells of the members it passes over, the request it takes,
//! the bounds on each member and on the stream, the names extension records give, where it
//! writes, and the modes it leaves under a restrictive umask.

use std::fs;
use std::path::Path;
use std::process::Command;

use tar::{EntryType, Header};

use super::fx::{Fx, gzip, mkdir, mode, raw_tarball, tarball, write};
use crate::mac::unpack::{
    DECLARATION_CAP, FRAMING, Wanted, declaration_member, ignored_line, unpack, unpack_release,
};

const DECLARATION: &str = "updater.d/kbf.toml";
const TOML: &[u8] = b"name = \"kbf\"\n";
const CAP: u64 = 1024;

/// Everything under `dir`, every level, sorted.
fn listing(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for item in fs::read_dir(dir).unwrap() {
        let path = item.unwrap().path();
        let name = path.strip_prefix(dir).unwrap().display().to_string();
        if path.is_dir() {
            found.extend(listing(&path).into_iter().map(|n| format!("{name}/{n}")));
        }
        found.push(name);
    }
    found.sort();
    found
}

/// The stage's ask, `kbf` (at most `cap`) and its declaration, of the tarball `bytes` written
/// to `dir/kbf.tar.gz` and unpacked into `dir`; the result and the lines told.
fn unpack_into(dir: &Path, bytes: &[u8], cap: u64) -> (Result<Vec<String>, String>, String) {
    let source = dir.join("kbf.tar.gz");
    write(&source, 0o644, bytes);
    let declaration = declaration_member("kbf");
    let wanted = [
        Wanted {
            name: "kbf",
            dest: &dir.join("kbf"),
            mode: 0o755,
            cap,
            required: true,
        },
        Wanted {
            name: &declaration,
            dest: &dir.join(&declaration),
            mode: 0o644,
            cap: DECLARATION_CAP,
            required: false,
        },
    ];
    let mut log = Vec::new();
    let result = unpack(&source, &wanted, &mut log);
    (result, String::from_utf8(log).unwrap())
}

/// A fresh directory `name` under the fixture's.
fn fresh(fx: &Fx, name: &str) -> std::path::PathBuf {
    let dir = fx.dir.join(name);
    mkdir(&dir, 0o755);
    dir
}

/// Catches a member passed over silently, told twice, or told only when the whole archive is
/// taken: each is named in the archive's order and told once, as it is passed over, with its
/// name escaped, even when a later member refuses the archive; a gzip bomb in a member no one
/// asked for is stopped at the bound.
#[test]
fn the_members_passed_over_are_told_once_and_bounded() {
    let fx = Fx::new("mac-unpack-told");
    let dir = fresh(&fx, "out");
    let (result, log) = unpack_into(
        &dir,
        &tarball(&[
            ("a", EntryType::Regular, b"1"),
            ("kbf", EntryType::Regular, b"bin"),
            ("b/", EntryType::Directory, b""),
            ("two\nlines", EntryType::Regular, b""),
        ]),
        CAP,
    );
    assert_eq!(result.unwrap(), ["a", "b/", "two\nlines"]);
    assert_eq!(
        log,
        format!(
            "{}\n{}\n{}\n",
            ignored_line("a"),
            ignored_line("b/"),
            ignored_line("two\nlines")
        )
    );
    assert_eq!(
        ignored_line("two\nlines"),
        "kbf-host-install: the release tarball's member \"two\\nlines\" is not taken: ignored"
    );
    assert_eq!(listing(&dir), ["kbf", "kbf.tar.gz"]);

    let dir = fresh(&fx, "refused");
    let (result, log) = unpack_into(
        &dir,
        &raw_tarball(&[
            (b"README", EntryType::Regular, b"r"),
            (b"../evil", EntryType::Regular, b""),
        ]),
        CAP,
    );
    assert!(result.is_err());
    assert_eq!(log, format!("{}\n", ignored_line("README")));

    let dir = fresh(&fx, "bomb");
    let bomb = tarball(&[
        ("kbf", EntryType::Regular, b"x"),
        ("big", EntryType::Regular, &vec![0u8; 8 << 20]),
    ]);
    assert_eq!(
        unpack_into(&dir, &bomb, CAP).0,
        Err(format!(
            "it decompresses to more than {} bytes",
            CAP + DECLARATION_CAP + FRAMING
        ))
    );

    // The release's own ask tells its lines on the log it is given.
    let dir = fresh(&fx, "release");
    write(
        &dir.join("kbf.tar.gz"),
        0o644,
        &tarball(&[
            ("README", EntryType::Regular, b"r"),
            ("kbf", EntryType::Regular, b"bin"),
        ]),
    );
    let mut log = Vec::new();
    unpack_release(&dir.join("kbf.tar.gz"), &dir, CAP, &mut log).unwrap();
    assert_eq!(
        String::from_utf8(log).unwrap(),
        format!("{}\n", ignored_line("README"))
    );
}

/// The uncompressed tar of a header named `kbf` with one byte and an ignored member of
/// `padding` bytes.
fn tar_with_padding(padding: usize) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (name, size) in [("kbf", 1), ("padding", padding)] {
        let mut header = Header::new_gnu();
        header.set_entry_type(EntryType::Regular);
        header.set_size(size as u64);
        header.set_mode(0o644);
        header.set_cksum();
        builder
            .append_data(&mut header, name, &vec![0u8; size][..])
            .unwrap();
    }
    builder.into_inner().unwrap()
}

/// Catches an off-by-one at a bound: an empty declaration and each member at exactly its cap
/// are taken whole with their modes, a byte more is refused; a member whose cap is 0 is taken
/// empty and refused at one byte; and the whole stream is taken at exactly the caps plus
/// [`FRAMING`] and refused one zero byte past it.
#[test]
fn each_bound_is_exact() {
    let fx = Fx::new("mac-unpack-bounds");
    let at_cap = vec![b'#'; usize::try_from(DECLARATION_CAP).unwrap()];
    let binary = vec![0x7f_u8; usize::try_from(CAP).unwrap()];
    for (why, decl) in [("empty", Vec::new()), ("exact", at_cap.clone())] {
        let dir = fresh(&fx, why);
        let bytes = tarball(&[
            ("kbf", EntryType::Regular, &binary[..]),
            (DECLARATION, EntryType::Regular, &decl[..]),
        ]);
        unpack_into(&dir, &bytes, CAP).0.unwrap();
        assert_eq!(fs::read(dir.join("kbf")).unwrap(), binary, "{why}");
        assert_eq!(fs::read(dir.join(DECLARATION)).unwrap(), decl, "{why}");
        assert_eq!(mode(&dir.join(DECLARATION)), 0o644, "{why}");
        assert_eq!(mode(&dir.join("kbf")), 0o755, "{why}");
    }
    let mut over = binary.clone();
    over.push(0);
    let dir = fresh(&fx, "over");
    assert_eq!(
        unpack_into(
            &dir,
            &tarball(&[("kbf", EntryType::Regular, &over[..])]),
            CAP
        )
        .0,
        Err(format!("the member \"kbf\" is larger than {CAP} bytes"))
    );
    assert!(!dir.join("kbf").exists());

    let dir = fresh(&fx, "zero");
    let source = dir.join("zero.tar.gz");
    let zero = [Wanted {
        name: "empty",
        dest: &dir.join("empty"),
        mode: 0o644,
        cap: 0,
        required: true,
    }];
    write(
        &source,
        0o644,
        &tarball(&[("empty", EntryType::Regular, b"")]),
    );
    unpack(&source, &zero, &mut Vec::new()).unwrap();
    assert_eq!(fs::read(dir.join("empty")).unwrap(), b"");
    fs::remove_file(dir.join("empty")).unwrap();
    write(
        &source,
        0o644,
        &tarball(&[("empty", EntryType::Regular, b"x")]),
    );
    assert_eq!(
        unpack(&source, &zero, &mut Vec::new()),
        Err("the member \"empty\" is larger than 0 bytes".to_owned())
    );

    let limit = CAP + DECLARATION_CAP + FRAMING;
    // 512 per header, the data padded to 512, and 1024 bytes of end blocks.
    let mut tar = tar_with_padding(usize::try_from(limit).unwrap() - 2560);
    assert_eq!(tar.len() as u64, limit);
    let dir = fresh(&fx, "at-bound");
    assert_eq!(unpack_into(&dir, &gzip(&tar), CAP).0.unwrap(), ["padding"]);
    tar.push(0);
    let dir = fresh(&fx, "past-bound");
    assert_eq!(
        unpack_into(&dir, &gzip(&tar), CAP).0,
        Err(format!("it decompresses to more than {limit} bytes"))
    );
}

/// Catches a request the unpack cannot honour being half done: a name asked for twice, or two
/// names into one destination, is refused before the tarball is opened. Any number of
/// members may be required (the first missing one is named, here the declaration though the
/// binary is there), an empty request lists every member as passed over, and an all-optional
/// request of an archive with no entries is refused as holding no file.
#[test]
fn the_request_names_each_member_and_destination_once() {
    let fx = Fx::new("mac-unpack-request");
    let dir = fresh(&fx, "req");
    let (a, b) = (dir.join("a"), dir.join("b"));
    let want = |name, dest, required| Wanted {
        name,
        dest,
        mode: 0o644,
        cap: CAP,
        required,
    };
    let none = dir.join("no-such.tar.gz");
    for request in [
        [want("kbf", &a, true), want("kbf", &b, false)],
        [want("kbf", &a, true), want(DECLARATION, &a, false)],
    ] {
        assert_eq!(
            unpack(&none, &request, &mut Vec::new()),
            Err(format!(
                "the unpack asks for {:?} by a name or a destination it already asks for",
                request[1].name
            ))
        );
    }
    let source = dir.join("kbf.tar.gz");
    write(
        &source,
        0o644,
        &tarball(&[("kbf", EntryType::Regular, b"bin")]),
    );
    let both = [want("kbf", &a, true), want(DECLARATION, &b, true)];
    assert_eq!(
        unpack(&source, &both, &mut Vec::new()),
        Err(format!("holds no member named {DECLARATION}"))
    );
    assert_eq!(unpack(&source, &[], &mut Vec::new()).unwrap(), ["kbf"]);
    let empty = dir.join("empty.tar.gz");
    write(&empty, 0o644, &gzip(&[0u8; 1024]));
    let c = dir.join("c");
    let optional = [want("kbf", &c, false)];
    assert_eq!(
        unpack(&empty, &optional, &mut Vec::new()),
        Err("holds no file".to_owned())
    );
}

/// The headers of a tar and the data after each.
type Entries = Vec<(Header, Vec<u8>)>;

/// A header of `kind` and `size` whose old (100-byte) name field is `name`, unchecked.
fn gnu(name: &[u8], kind: EntryType, size: usize) -> Header {
    let mut h = Header::new_gnu();
    h.as_old_mut().name[..name.len()].copy_from_slice(name);
    h.set_entry_type(kind);
    h.set_size(size as u64);
    h.set_mode(0o644);
    h.set_cksum();
    h
}

/// A GNU long-name (`L`) or long-link (`K`) record carrying `name`.
fn gnu_long(kind: EntryType, name: &str) -> (Header, Vec<u8>) {
    let mut data = name.as_bytes().to_vec();
    data.push(0);
    (gnu(b"././@LongLink", kind, data.len()), data)
}

/// A PAX extended header (`x`, or `g` for a global one) of the one record `key=value`.
fn pax(kind: EntryType, key: &str, value: &str) -> (Header, Vec<u8>) {
    let body = format!(" {key}={value}\n");
    let mut len = body.len() + 1;
    while format!("{len}{body}").len() != len {
        len += 1;
    }
    let data = format!("{len}{body}").into_bytes();
    let mut h = Header::new_ustar();
    h.as_old_mut().name[..9].copy_from_slice(b"PaxHeader");
    h.set_entry_type(kind);
    h.set_size(data.len() as u64);
    h.set_mode(0o644);
    h.set_cksum();
    (h, data)
}

/// A ustar header of a regular file named `prefix` + `/` + `name`, in the two fields.
fn ustar_prefixed(prefix: &[u8], name: &[u8], size: usize) -> Header {
    let mut h = Header::new_ustar();
    h.as_old_mut().name[..name.len()].copy_from_slice(name);
    h.as_ustar_mut().unwrap().prefix[..prefix.len()].copy_from_slice(prefix);
    h.set_entry_type(EntryType::Regular);
    h.set_size(size as u64);
    h.set_mode(0o644);
    h.set_cksum();
    h
}

fn regular(name: &[u8], data: &[u8]) -> (Header, Vec<u8>) {
    (gnu(name, EntryType::Regular, data.len()), data.to_vec())
}

/// The gzipped tar of `entries`, each header written as given.
fn raw(entries: &[(Header, Vec<u8>)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (header, data) in entries {
        builder.append(header, data.as_slice()).unwrap();
    }
    gzip(&builder.into_inner().unwrap())
}

/// Catches a name hidden in an extension record escaping the checks: the name a GNU long
/// name, a PAX `path` or a ustar prefix gives is the one checked and refused by, a GNU long
/// link on a symbolic link is refused as a link, and a PAX global header as a member of
/// another kind; and it is the name matched: a declaration named by a PAX `path` or a ustar
/// prefix is taken, and a header named `kbf` whose PAX `path` is another name is not the
/// binary.
#[test]
fn the_name_an_extension_record_gives_is_the_one_checked_and_matched() {
    let fx = Fx::new("mac-unpack-ext");
    let leaves = |n: &str| format!("the member {n:?} leaves the directory it is unpacked into");
    let cases: Vec<(&str, Entries, String)> = vec![
        (
            "gnu-long-dotdot",
            vec![
                gnu_long(EntryType::GNULongName, "../evil"),
                regular(b"short", b"x"),
            ],
            leaves("../evil"),
        ),
        (
            "gnu-long-absolute",
            vec![
                gnu_long(EntryType::GNULongName, "/etc/updater.d/kbf.toml"),
                regular(b"kbf", b"x"),
            ],
            leaves("/etc/updater.d/kbf.toml"),
        ),
        (
            "pax-absolute",
            vec![
                pax(EntryType::XHeader, "path", "/etc/updater.d/kbf.toml"),
                regular(b"safe", b"x"),
            ],
            leaves("/etc/updater.d/kbf.toml"),
        ),
        (
            "pax-dotdot-over-kbf",
            vec![
                pax(EntryType::XHeader, "path", "updater.d/../../kbf"),
                regular(b"kbf", b"x"),
            ],
            leaves("updater.d/../../kbf"),
        ),
        (
            "ustar-prefix-dotdot",
            vec![(ustar_prefixed(b"..", b"kbf", 1), b"x".to_vec())],
            leaves("../kbf"),
        ),
        (
            "gnu-long-link",
            vec![
                gnu_long(EntryType::GNULongLink, "/etc/passwd"),
                (gnu(b"evil", EntryType::Symlink, 0), Vec::new()),
            ],
            "the member \"evil\" is a symbolic link, not a regular file".to_owned(),
        ),
        (
            "pax-global",
            vec![
                pax(EntryType::XGlobalHeader, "comment", "x"),
                regular(b"kbf", b"x"),
            ],
            "the member \"PaxHeader\" is a member of another kind, not a regular file".to_owned(),
        ),
    ];
    for (why, entries, want) in cases {
        let dir = fresh(&fx, why);
        assert_eq!(unpack_into(&dir, &raw(&entries), CAP).0, Err(want), "{why}");
        assert_eq!(
            listing(&dir),
            ["kbf.tar.gz"],
            "{why}: something was written"
        );
    }

    let dir = fresh(&fx, "pax-declaration");
    let bytes = raw(&[
        regular(b"kbf", b"bin"),
        pax(EntryType::XHeader, "path", DECLARATION),
        regular(b"other", b"from pax"),
    ]);
    unpack_into(&dir, &bytes, CAP).0.unwrap();
    assert_eq!(fs::read(dir.join(DECLARATION)).unwrap(), b"from pax");

    let dir = fresh(&fx, "prefix-declaration");
    let bytes = raw(&[
        regular(b"kbf", b"bin"),
        (
            ustar_prefixed(b"updater.d", b"kbf.toml", 11),
            b"from prefix".to_vec(),
        ),
    ]);
    unpack_into(&dir, &bytes, CAP).0.unwrap();
    assert_eq!(fs::read(dir.join(DECLARATION)).unwrap(), b"from prefix");

    let dir = fresh(&fx, "pax-renames-kbf");
    let bytes = raw(&[
        pax(EntryType::XHeader, "path", "README"),
        regular(b"kbf", b"bin"),
    ]);
    assert_eq!(
        unpack_into(&dir, &bytes, CAP).0,
        Err("holds no member named kbf".to_owned())
    );
}

/// Catches a member written where it must not be: a file or a link in the place of
/// `updater.d` (not followed: nothing is written where it points), a missing parent (not
/// made), a file or a link already at the binary's or the declaration's destination (left as
/// it was), and a tarball that is not there.
#[test]
fn a_destination_that_cannot_be_made_is_refused() {
    let fx = Fx::new("mac-unpack-dest");
    let both = tarball(&[
        (DECLARATION, EntryType::Regular, TOML),
        ("kbf", EntryType::Regular, b"bin"),
    ]);
    let dir = fresh(&fx, "blocked");
    write(&dir.join("updater.d"), 0o644, b"a file");
    assert!(unpack_into(&dir, &both, CAP).0.is_err());
    assert_eq!(fs::read(dir.join("updater.d")).unwrap(), b"a file");

    let dir = fresh(&fx, "linked");
    let elsewhere = fresh(&fx, "elsewhere");
    std::os::unix::fs::symlink(&elsewhere, dir.join("updater.d")).unwrap();
    assert!(unpack_into(&dir, &both, CAP).0.is_err());
    assert!(listing(&elsewhere).is_empty(), "written through the link");

    let dir = fresh(&fx, "exists");
    write(&dir.join("kbf"), 0o755, b"old");
    assert!(unpack_into(&dir, &both, CAP).0.is_err());
    assert_eq!(fs::read(dir.join("kbf")).unwrap(), b"old");

    let dir = fresh(&fx, "declaration-exists");
    mkdir(&dir.join("updater.d"), 0o755);
    write(&dir.join(DECLARATION), 0o644, b"old");
    assert!(unpack_into(&dir, &both, CAP).0.is_err());
    assert_eq!(fs::read(dir.join(DECLARATION)).unwrap(), b"old");

    let dir = fresh(&fx, "declaration-link");
    mkdir(&dir.join("updater.d"), 0o755);
    let target = fx.dir.join("target.toml");
    write(&target, 0o644, b"old");
    std::os::unix::fs::symlink(&target, dir.join(DECLARATION)).unwrap();
    assert!(unpack_into(&dir, &both, CAP).0.is_err());
    assert_eq!(fs::read(&target).unwrap(), b"old");

    let deep = [Wanted {
        name: "kbf",
        dest: &fx.dir.join("missing/deeper/kbf"),
        mode: 0o755,
        cap: CAP,
        required: true,
    }];
    assert!(unpack(&dir.join("kbf.tar.gz"), &deep, &mut Vec::new()).is_err());
    assert!(!fx.dir.join("missing").exists());
    assert!(unpack(&dir.join("none.tar.gz"), &deep, &mut Vec::new()).is_err());
}

/// The unpack of `kbf.tar.gz` in the current directory into `out/` there, under a umask of
/// 077. It changes the process's umask, so it runs only in its own process.
#[test]
#[ignore = "run by modes_are_exact_under_a_restrictive_umask, in a process of its own: it sets the umask"]
fn unpacks_under_a_umask_of_077() {
    rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o077));
    let here = std::env::current_dir().unwrap();
    let mut log = Vec::new();
    unpack_release(&here.join("kbf.tar.gz"), &here.join("out"), CAP, &mut log).unwrap();
}

/// Catches a mode left to the umask: unpacked by a process whose umask is 077, the binary is
/// 0755, the declaration 0644 and the `updater.d/` made for it 0755.
#[test]
fn modes_are_exact_under_a_restrictive_umask() {
    let fx = Fx::new("mac-unpack-umask");
    let dir = fresh(&fx, "run");
    write(
        &dir.join("kbf.tar.gz"),
        0o644,
        &tarball(&[
            ("kbf", EntryType::Regular, b"bin"),
            (DECLARATION, EntryType::Regular, TOML),
        ]),
    );
    mkdir(&dir.join("out"), 0o755);
    let ran = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "mac::tests::unpack_tests::unpacks_under_a_umask_of_077",
        ])
        .current_dir(&dir)
        .output()
        .unwrap();
    assert!(
        ran.status.success(),
        "{}",
        String::from_utf8_lossy(&ran.stdout)
    );
    let out = dir.join("out");
    assert_eq!(mode(&out.join("kbf")), 0o755);
    assert_eq!(mode(&out.join("updater.d")), 0o755);
    assert_eq!(mode(&out.join(DECLARATION)), 0o644);
}
