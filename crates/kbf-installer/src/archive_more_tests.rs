//! More of the unpack's tests: names the tar reader resolves from extension records, the
//! request a caller may make, the size boundaries, the log line of an ignored member, and the
//! modes under a restrictive umask. They share `archive_tests`' helpers.

use std::os::unix::fs::MetadataExt;
use std::process::Command;

use tar::{EntryType, Header};

use super::*;

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
    // The length counts its own digits.
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

/// A ustar header of a regular file whose name is `prefix` + `/` + `name`, in the two fields.
fn ustar_prefixed(prefix: &[u8], name: &[u8], size: usize) -> Header {
    let mut h = Header::new_ustar();
    h.as_old_mut().name[..name.len()].copy_from_slice(name);
    h.as_ustar_mut().expect("ustar").prefix[..prefix.len()].copy_from_slice(prefix);
    h.set_entry_type(EntryType::Regular);
    h.set_size(size as u64);
    h.set_mode(0o644);
    h.set_cksum();
    h
}

/// The uncompressed tar of `entries`, each header written as given.
fn raw_tar(entries: &[(Header, Vec<u8>)]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    for (header, data) in entries {
        builder.append(header, data.as_slice()).expect("append");
    }
    builder.into_inner().expect("tar")
}

fn regular(name: &[u8], data: &[u8]) -> (Header, Vec<u8>) {
    (gnu(name, EntryType::Regular, data.len()), data.to_vec())
}

/// Catches a name hidden in an extension record escaping the checks: the name a GNU long
/// name, a PAX `path` or a ustar prefix gives is the one checked (an absolute one, or one
/// with `..`, is refused by that name, whatever the header's own field says), a GNU long
/// link on a symbolic link is refused as a link, and a PAX global header is refused as a
/// member of another kind. The resolved name is also the one matched: a declaration named by
/// a PAX `path` or a ustar prefix is taken, and a header named `kbf` whose PAX `path` is
/// another name is not the binary.
#[test]
fn the_name_an_extension_record_gives_is_the_one_checked_and_matched() {
    let refused: Vec<(&str, Entries, &str)> = vec![
        (
            "gnu-long-dotdot",
            vec![
                gnu_long(EntryType::GNULongName, "../evil"),
                regular(b"short", b"x"),
            ],
            "its member \"../evil\" has a name that leaves the directory it is unpacked into",
        ),
        (
            "gnu-long-absolute",
            vec![
                gnu_long(EntryType::GNULongName, "/etc/updater.d/kbf.toml"),
                regular(b"kbf", b"x"),
            ],
            "its member \"/etc/updater.d/kbf.toml\" has a name that leaves the directory it is unpacked into",
        ),
        (
            "pax-absolute",
            vec![
                pax(EntryType::XHeader, "path", "/etc/updater.d/kbf.toml"),
                regular(b"safe", b"x"),
            ],
            "its member \"/etc/updater.d/kbf.toml\" has a name that leaves the directory it is unpacked into",
        ),
        (
            "pax-dotdot-over-kbf",
            vec![
                pax(EntryType::XHeader, "path", "updater.d/../../kbf"),
                regular(b"kbf", b"x"),
            ],
            "its member \"updater.d/../../kbf\" has a name that leaves the directory it is unpacked into",
        ),
        (
            "ustar-prefix-dotdot",
            vec![(ustar_prefixed(b"..", b"kbf", 1), b"x".to_vec())],
            "its member \"../kbf\" has a name that leaves the directory it is unpacked into",
        ),
        (
            "gnu-long-link",
            vec![
                gnu_long(EntryType::GNULongLink, "/etc/passwd"),
                (gnu(b"evil", EntryType::Symlink, 0), Vec::new()),
            ],
            "its member \"evil\" is a symbolic link, not a regular file",
        ),
        (
            "pax-global",
            vec![
                pax(EntryType::XGlobalHeader, "comment", "x"),
                regular(b"kbf", b"x"),
            ],
            "its member \"PaxHeader\" is a member of another kind, not a regular file",
        ),
    ];
    for (why, entries, message) in refused {
        let (out, result) = unpack(
            &format!("archive-ext-{why}"),
            &gzip(&raw_tar(&entries)),
            CAP,
        );
        let err = result.expect_err(why);
        assert_eq!(err.to_string(), message, "{why}");
        assert!(listing(&out).is_empty(), "{why}: something was written");
    }

    // Matched by the resolved name.
    let tarball = raw_tar(&[
        regular(b"kbf", b"binary"),
        pax(EntryType::XHeader, "path", DECLARATION),
        regular(b"other", b"from pax"),
    ]);
    let (out, result) = unpack("archive-ext-pax-declaration", &gzip(&tarball), CAP);
    assert_eq!(result.expect("taken").taken, names(&["kbf", DECLARATION]));
    assert_eq!(fs::read(out.join(DECLARATION)).expect("there"), b"from pax");
    remove_binary(&out);

    let tarball = raw_tar(&[
        regular(b"kbf", b"binary"),
        (
            ustar_prefixed(b"updater.d", b"kbf.toml", 11),
            b"from prefix".to_vec(),
        ),
    ]);
    let (out, result) = unpack("archive-ext-prefix-declaration", &gzip(&tarball), CAP);
    assert_eq!(result.expect("taken").taken, names(&["kbf", DECLARATION]));
    assert_eq!(
        fs::read(out.join(DECLARATION)).expect("there"),
        b"from prefix"
    );
    remove_binary(&out);

    let tarball = raw_tar(&[
        gnu_long(EntryType::GNULongName, "kbf"),
        regular(b"ignored-header-name", b"from long name"),
    ]);
    let (out, result) = unpack("archive-ext-long-kbf", &gzip(&tarball), CAP);
    assert_eq!(result.expect("taken").taken, names(&["kbf"]));
    assert_eq!(fs::read(out.join("kbf")).expect("there"), b"from long name");
    remove_binary(&out);

    let tarball = raw_tar(&[
        pax(EntryType::XHeader, "path", "README"),
        regular(b"kbf", b"binary"),
    ]);
    let (out, result) = unpack("archive-ext-pax-renames-kbf", &gzip(&tarball), CAP);
    assert_eq!(
        result.expect_err("no binary").to_string(),
        "it holds no member named \"kbf\""
    );
    assert!(listing(&out).is_empty());
}

/// A request for `name` into `dest`, at most [`CAP`].
fn member<'a>(name: &'a str, dest: &'a Path, required: bool) -> Member<'a> {
    Member {
        name,
        dest,
        mode: 0o644,
        cap: CAP,
        required,
    }
}

/// Catches a request the unpack cannot honour being half done: a name asked for twice, or two
/// names into one destination, is refused before the tarball is opened (it need not exist).
/// Any number of members may be required: the first one missing is named (here the
/// declaration, though the binary is there), an empty request takes nothing and lists every
/// member as ignored, and an all-optional request of an archive with no entries is
/// `archive-entries`.
#[test]
fn the_request_names_each_member_and_destination_once() {
    let dir = scratch_dir("archive-request");
    let (a, b) = (dir.join("a"), dir.join("b"));
    let none = dir.join("no-such.tar.gz");
    for (why, request) in [
        (
            "same-name",
            vec![member("kbf", &a, true), member("kbf", &b, false)],
        ),
        (
            "same-dest",
            vec![member("kbf", &a, true), member(DECLARATION, &a, false)],
        ),
    ] {
        let err = unpack_members(&none, &request, WITHIN, &mut |_| {}).expect_err(why);
        assert_eq!(err.code(), "archive-request", "{why}");
    }
    let err = unpack_members(
        &none,
        &[member("kbf", &a, true), member("kbf", &b, true)],
        WITHIN,
        &mut |_| {},
    )
    .expect_err("twice");
    assert_eq!(
        err.to_string(),
        "the unpack asks for \"kbf\" by a name or a destination it already asks for"
    );

    let source = dir.join("kbf.tar.gz");
    fs::write(&source, tar_gz(&[file("kbf", b"binary")])).expect("seed");
    let both = [member("kbf", &a, true), member(DECLARATION, &b, true)];
    let err = unpack_members(&source, &both, WITHIN, &mut |_| {}).expect_err("no declaration");
    assert_eq!(err.code(), "archive-name");
    assert_eq!(
        err.to_string(),
        "it holds no member named \"updater.d/kbf.toml\""
    );

    let all = unpack_members(&source, &[], WITHIN, &mut |_| {}).expect("nothing asked");
    assert_eq!(
        all,
        Unpacked {
            taken: Vec::new(),
            ignored: names(&["kbf"]),
        }
    );

    let empty = dir.join("empty.tar.gz");
    fs::write(&empty, tar_gz(&[])).expect("seed");
    let c = dir.join("c");
    let optional = [member("kbf", &c, false)];
    let err = unpack_members(&empty, &optional, WITHIN, &mut |_| {}).expect_err("empty");
    assert_eq!(err.code(), "archive-entries");
    assert_eq!(err.to_string(), "it holds 0 entries");
    let _ = fs::remove_file(a);
}

/// Catches an off-by-one at a cap: an empty declaration, and each member at exactly its cap,
/// are taken whole with their modes; one byte more is refused by its header; a member whose
/// cap is 0 is taken when empty and refused at one byte.
#[test]
fn each_member_is_taken_up_to_exactly_its_cap() {
    let at_cap = vec![b'#'; usize::try_from(DECLARATION_CAP).expect("small")];
    let binary = vec![0x7f_u8; usize::try_from(CAP).expect("small")];
    for (why, decl) in [("empty", Vec::new()), ("exact", at_cap.clone())] {
        let tarball = tar_gz(&[file("kbf", &binary), file(DECLARATION, &decl)]);
        let (out, result) = unpack(&format!("archive-cap-{why}"), &tarball, CAP);
        result.expect(why);
        assert_eq!(fs::read(out.join("kbf")).expect("binary"), binary, "{why}");
        assert_eq!(
            fs::read(out.join(DECLARATION)).expect("decl"),
            decl,
            "{why}"
        );
        assert_eq!(mode_of(&out.join(DECLARATION)), 0o644, "{why}");
        remove_binary(&out);
    }
    let mut over = at_cap;
    over.push(b'#');
    let (out, result) = unpack(
        "archive-cap-over",
        &tar_gz(&[file("kbf", b"a"), file(DECLARATION, &over)]),
        CAP,
    );
    assert_eq!(result.expect_err("over").code(), "archive-oversize");
    assert!(!out.join(DECLARATION).exists());
    remove_binary(&out);

    let dir = scratch_dir("archive-cap-zero");
    let dest = dir.join("empty");
    let zero = [Member {
        name: "empty",
        dest: &dest,
        mode: 0o644,
        cap: 0,
        required: true,
    }];
    let source = dir.join("zero.tar.gz");
    fs::write(&source, tar_gz(&[file("empty", b"")])).expect("seed");
    unpack_members(&source, &zero, WITHIN, &mut |_| {}).expect("an empty member at cap 0");
    assert_eq!(fs::read(&dest).expect("there"), b"");
    fs::remove_file(&dest).expect("remove");
    fs::write(&source, tar_gz(&[file("empty", b"x")])).expect("seed");
    let err = unpack_members(&source, &zero, WITHIN, &mut |_| {}).expect_err("one byte at cap 0");
    assert_eq!(
        err.to_string(),
        "it is larger than 0 bytes (its entry, or what it decompresses to)"
    );
}

/// Catches the bound on the whole decompressed stream being off by one: a tar of the binary
/// and an ignored member that decompresses to exactly the caps plus the framing is taken,
/// and the same tar with one zero byte more after its end is refused as oversize.
#[test]
fn the_whole_stream_is_taken_up_to_exactly_its_bound() {
    let limit = CAP + DECLARATION_CAP + FRAMING;
    // 512 per header, the data padded to 512, and 1024 bytes of end blocks.
    let ignored = usize::try_from(limit).expect("small") - 2560;
    let mut tar = raw_tar(&[
        regular(b"kbf", b"x"),
        regular(b"padding", &vec![0u8; ignored]),
    ]);
    assert_eq!(tar.len() as u64, limit, "the tar is exactly at the bound");
    let (out, result) = unpack("archive-bound-at", &gzip(&tar), CAP);
    assert_eq!(result.expect("at the bound").ignored, names(&["padding"]));
    remove_binary(&out);

    tar.push(0);
    let (out, result) = unpack("archive-bound-over", &gzip(&tar), CAP);
    assert_eq!(result.expect_err("one over").code(), "archive-oversize");
    remove_binary(&out);
}

/// What the unpack hands over as ignored while it unpacks `tarball` as the stage does, in
/// order, with the result.
fn logged(name: &str, tarball: &[u8]) -> (Result<Unpacked, ArchiveFault>, Vec<String>) {
    let dir = scratch_dir(name);
    let source = dir.join("kbf.tar.gz");
    fs::write(&source, tarball).expect("seed");
    let binary = dir.join("kbf");
    let declaration = dir.join(DECLARATION);
    let members = [
        member("kbf", &binary, true),
        member(DECLARATION, &declaration, false),
    ];
    let mut told = Vec::new();
    let result = unpack_members(&source, &members, WITHIN, &mut |m| told.push(m.to_owned()));
    let _ = fs::remove_file(binary);
    (result, told)
}

/// Catches an ignored member passed over silently, told twice, or told only when the whole
/// archive is taken: each one is handed to the caller's log once, as it is passed over, even
/// when a later member then refuses the archive; a member that is taken is not told. (The
/// stage logs each with `tracing`, its `{:?}` escaping a newline in the name.)
#[test]
fn each_ignored_member_is_logged_once_as_it_is_passed_over() {
    let (result, lines) = logged(
        "archive-log-ok",
        &tar_gz(&[
            file("README", b"r"),
            file("kbf", b"binary"),
            file("two\nlines", b"t"),
        ]),
    );
    result.expect("taken");
    assert_eq!(lines, ["README", "two\nlines"], "once each, in order");

    let unsafe_after = tar_gz(&[
        file("README", b"r"),
        file("kbf", b"binary"),
        Item {
            name: b"../evil",
            kind: b'0',
            data: b"",
        },
    ]);
    let (result, lines) = logged("archive-log-refused", &unsafe_after);
    assert_eq!(result.expect_err("refused").code(), "archive-unsafe-path");
    assert_eq!(lines, ["README"], "told before the refusal");
}

/// The unpack of `kbf.tar.gz` in the current directory as the stage does it, into `out/` there,
/// under a umask of 077. It changes the process's umask, so it runs only in its own process.
#[test]
#[ignore = "run by modes_are_exact_under_a_restrictive_umask, in a process of its own: it sets the umask"]
fn unpacks_under_a_umask_of_077() {
    rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o077));
    let here = std::env::current_dir().expect("cwd");
    unpack_into(&here.join("out"), &here.join("kbf.tar.gz"), CAP, WITHIN).expect("unpacked");
}

/// Catches a mode left to the umask: unpacked by a process whose umask is 077, the binary is
/// 0755, the declaration 0644 and the `updater.d/` made for it 0755, as the release needs
/// for the unprivileged user to read them.
#[test]
fn modes_are_exact_under_a_restrictive_umask() {
    let dir = scratch_dir("archive-umask");
    fs::write(
        dir.join("kbf.tar.gz"),
        tar_gz(&[file("kbf", b"binary"), file(DECLARATION, b"toml")]),
    )
    .expect("seed");
    fs::create_dir(dir.join("out")).expect("out");
    let ran = Command::new(std::env::current_exe().expect("this binary"))
        .args([
            "--ignored",
            "--exact",
            "archive::tests::more::unpacks_under_a_umask_of_077",
        ])
        .current_dir(&dir)
        .output()
        .expect("the test binary runs");
    assert!(
        ran.status.success(),
        "{}",
        String::from_utf8_lossy(&ran.stdout)
    );
    let out = dir.join("out");
    assert_eq!(mode_of(&out.join("kbf")), 0o755);
    assert_eq!(mode_of(&out.join("updater.d")), 0o755);
    assert_eq!(mode_of(&out.join(DECLARATION)), 0o644);
    assert_eq!(
        fs::metadata(out.join(DECLARATION)).expect("stat").uid(),
        fs::metadata(&dir).expect("stat").uid()
    );
    remove_binary(&out);
}
