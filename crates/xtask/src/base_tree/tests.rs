//! `base-tree`: the layers of a base image flattened into the kbf base tree, what is refused by
//! path and kind, how modes are normalised, and the bundle that is written.

mod bundle;
mod support;

use std::collections::BTreeMap;

use kbf_testkit::dir::TestDir;
use support::{E, Img, Seen, image, layer, listing, top_level};

use super::*;

const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

/// An empty directory of its own for one test.
fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("xtask-base-tree-{name}"))
}

/// The tree of `layers` as the layers of a base, with a `kbf-task` of the bytes `kbf-task`.
fn built(layers: &[Vec<u8>]) -> Tree {
    let img = image(layers, &[], false);
    match tree_of(&img.bytes, &img.base, b"kbf-task".to_vec()) {
        Ok(tree) => tree,
        Err(e) => panic!("refused: {e:?}"),
    }
}

/// The problems the tree of `layers` is refused with.
fn refused(layers: &[Vec<u8>]) -> Vec<String> {
    let img = image(layers, &[], false);
    match tree_of(&img.bytes, &img.base, b"kbf-task".to_vec()) {
        Err(Error::Refused(problems)) => problems,
        Err(e) => panic!("failed, not refused: {e:?}"),
        Ok(_) => panic!("accepted"),
    }
}

/// A file as the tree lists it.
fn file(bytes: &str, exec: bool) -> Seen {
    Seen::File {
        exec,
        bytes: bytes.as_bytes().to_vec(),
    }
}

/// `(path, what)` rows as the map `listing` gives.
fn rows(rows: &[(&str, Seen)]) -> BTreeMap<String, Seen> {
    rows.iter()
        .map(|(p, s)| ((*p).to_owned(), s.clone()))
        .collect()
}

/// A normalised path.
fn norm(path: &str, from: &str, to: &str) -> Normalised {
    Normalised {
        path: path.to_owned(),
        from: from.to_owned(),
        to: to.to_owned(),
    }
}

/// The message of a path a tree makes itself.
fn reserved(path: &str) -> String {
    format!(
        "{path}: a path the kbf tree makes itself (/work, /.kbf, /.kbf-run and /.kbf-tools are not in an image)"
    )
}

/// Asserts the layers are applied in order: a whiteout removes a lower layer's file or directory
/// and does nothing for a name that is not there (nor makes its directory), a later file replaces
/// an earlier one, a directory over a directory keeps what the directory holds, a file over a
/// directory replaces everything below it, a directory over a file or a symlink is empty, a path
/// below a file makes the file a directory, and a gzip-compressed layer is the same as a plain one.
/// Catches: a whiteout that leaves its target, one that makes a directory for a name that was
/// never there, a replacement that merges bytes or children it should drop, and a layer read only
/// when it is plain.
#[test]
fn layers_are_applied_in_order_with_whiteouts_and_replacements() {
    let one = layer(&[
        E::dir("etc/", 0o755),
        E::file("etc/passwd", 0o644, b"root"),
        E::file("etc/gone", 0o644, b"gone"),
        E::file("usr/bin/tool", 0o755, b"tool"),
        E::symlink("bin", "usr/bin"),
        E::dir("a/", 0o755),
        E::file("a/f", 0o644, b"f"),
        E::file("b", 0o644, b"b"),
        E::symlink("c", "x"),
        E::file("d", 0o644, b"d"),
        E::dir("e/", 0o755),
        E::file("e/inner", 0o644, b"inner"),
    ]);
    let two = layer(&[
        E::whiteout("etc/.wh.gone"),
        E::whiteout("nothing/.wh.x"),
        E::whiteout(".wh.missing"),
        E::file("etc/passwd", 0o644, b"root2"),
        E::dir("usr/", 0o755),
        E::dir("a/", 0o755),
        E::symlink("a/f", "z"),
        E::dir("b/", 0o755),
        E::file("c", 0o644, b"c"),
        E::file("d/inner", 0o644, b"in d"),
        E::file("e", 0o644, b"now a file"),
    ]);
    let expected = rows(&[
        ("/etc", Seen::Dir),
        ("/etc/passwd", file("root2", false)),
        ("/usr", Seen::Dir),
        ("/usr/bin", Seen::Dir),
        ("/usr/bin/tool", file("tool", true)),
        ("/bin", Seen::Symlink("usr/bin".to_owned())),
        ("/a", Seen::Dir),
        ("/a/f", Seen::Symlink("z".to_owned())),
        ("/b", Seen::Dir),
        ("/c", file("c", false)),
        ("/d", Seen::Dir),
        ("/d/inner", file("in d", false)),
        ("/e", file("now a file", false)),
        ("/.kbf-tools", Seen::Dir),
        ("/.kbf-tools/kbf-task", file("kbf-task", true)),
    ]);
    let layers = [one, two];
    assert_eq!(listing(&built(&layers)), expected);
    let gz = image(&layers, &[], true);
    let tree = tree_of(&gz.bytes, &gz.base, b"kbf-task".to_vec()).expect("gzip layers");
    assert_eq!(listing(&tree), expected);
}

/// Asserts an opaque marker removes what the lower layers put in its directory and keeps what its
/// own layer puts there, wherever the marker is in the tar; that a whiteout of a directory
/// removes the whole subtree; and that a whiteout does not remove a file its own layer adds,
/// whichever of the two comes first in the tar.
/// Catches: an opaque directory that keeps lower files, one that drops its own layer's files when
/// the marker comes after them, and a whiteout applied in tar order so that it removes a file
/// added before it in the same layer.
#[test]
fn an_opaque_directory_and_a_whiteout_leave_what_their_own_layer_adds() {
    let one = layer(&[
        E::file("var/cache/old1", 0o644, b"1"),
        E::file("var/cache/old2", 0o644, b"2"),
        E::file("var/lib/pkg/info", 0o644, b"i"),
        E::file("opt/x", 0o644, b"x"),
        E::file("srv/a/b/c", 0o644, b"c"),
    ]);
    let two = layer(&[
        E::file("var/cache/new", 0o644, b"new"),
        E::whiteout("var/cache/.wh..wh..opq"),
        E::whiteout("var/lib/.wh.pkg"),
        E::file("opt/y", 0o644, b"y"),
        E::whiteout("opt/.wh.y"),
        E::whiteout("opt/.wh.z"),
        E::file("opt/z", 0o644, b"z"),
        E::whiteout("srv/.wh.a"),
        E::whiteout("opt/x/.wh.below-a-file"),
    ]);
    assert_eq!(
        listing(&built(&[one, two])),
        rows(&[
            ("/var", Seen::Dir),
            ("/var/cache", Seen::Dir),
            ("/var/cache/new", file("new", false)),
            ("/var/lib", Seen::Dir),
            ("/opt", Seen::Dir),
            ("/opt/x", file("x", false)),
            ("/opt/y", file("y", false)),
            ("/opt/z", file("z", false)),
            ("/srv", Seen::Dir),
            ("/.kbf-tools", Seen::Dir),
            ("/.kbf-tools/kbf-task", file("kbf-task", true)),
        ])
    );
}

/// Asserts each kind a tree cannot hold is refused by its path and kind, with no other problem
/// for it, and that the problems of every entry of every layer are listed together, in order:
/// a character device, a block device, a FIFO,
/// a tar entry of another type, an extended attribute of either PAX family, a name that is not
/// UTF-8 and a path with `..`.
/// Catches: a kind let into the tree (the release would then change what runs), a refusal that
/// names no path, and a build that stops at the first problem so that a release is fixed one
/// problem at a time.
#[test]
fn each_kind_a_tree_cannot_hold_is_refused_by_path_and_kind() {
    let cases: Vec<(E, Vec<String>)> = vec![
        (
            E::special("dev/null", tar::EntryType::Char),
            vec!["/dev/null: a character device".to_owned()],
        ),
        (
            E::special("dev/sda", tar::EntryType::Block),
            vec!["/dev/sda: a block device".to_owned()],
        ),
        (
            E::special("run/pipe", tar::EntryType::Fifo),
            vec!["/run/pipe: a FIFO".to_owned()],
        ),
        (
            E::special("run/odd", tar::EntryType::Continuous),
            vec!["/run/odd: an entry of type Continuous, which a tree cannot hold (a tar cannot hold a socket, and a type this reader does not know is refused, not skipped)".to_owned()],
        ),
        (
            E::file("usr/bin/ping", 0o755, b"x")
                .pax("SCHILY.xattr.security.capability", "cap"),
            vec!["/usr/bin/ping: an extended attribute (SCHILY.xattr.security.capability)".to_owned()],
        ),
        (
            E::dir("var/", 0o755).pax("LIBARCHIVE.xattr.user.k", "v"),
            vec!["/var: an extended attribute (LIBARCHIVE.xattr.user.k)".to_owned()],
        ),
        (
            E::file("x", 0o644, b"x").at(b"etc/caf\xe9"),
            vec!["etc/caf\u{fffd}: a name that is not UTF-8".to_owned()],
        ),
        (
            E::file("x", 0o644, b"x").at(b"etc/../x"),
            vec!["etc/../x: a path with a `..` component".to_owned()],
        ),
    ];
    for (entry, wanted) in cases {
        assert_eq!(refused(&[layer(&[entry])]), wanted);
    }

    let first = layer(&[
        E::special("a/sda", tar::EntryType::Block),
        E::file("a/fine", 0o644, b"x"),
        E::special("a/pipe", tar::EntryType::Fifo),
    ]);
    let second = layer(&[E::special("b/null", tar::EntryType::Char)]);
    assert_eq!(
        refused(&[first, second]),
        [
            "/a/sda: a block device".to_owned(),
            "/a/pipe: a FIFO".to_owned(),
            "/b/null: a character device".to_owned(),
        ]
    );
}

/// Asserts a PAX record that is not an extended attribute, a directory with the set-gid or
/// sticky bit, and a name that only looks like a reserved one are accepted, the directory listed
/// as normalised.
/// Catches: a refusal of every PAX record, of a directory only because it has the bit a tree
/// drops, or of `/workspace` for starting like `/work`.
#[test]
fn what_looks_like_a_refused_kind_but_is_not_is_accepted() {
    let tree = built(&[layer(&[
        E::file("etc/f", 0o644, b"f").pax("comment", "fine"),
        E::dir("var/mail/", 0o2775),
        E::dir("workspace/", 0o755),
        E::dir(".kbf-other/", 0o755),
    ])]);
    assert!(listing(&tree).contains_key("/workspace"));
    assert!(listing(&tree).contains_key("/.kbf-other"));
    assert_eq!(tree.normalised, [norm("/var/mail", "2775", "0755")]);
}

/// Asserts the four paths the kbf tree makes itself are refused whether the image names them as
/// a file or a directory, at or below the path, under any spelling of the root (`./`, `/`, none).
/// Catches: an image whose `/work`, `/.kbf`, `/.kbf-run` or `/.kbf-tools` collides with what the
/// action tree or `kbf-task` adds, and a spelling of the path that slips past the check.
#[test]
fn a_path_the_kbf_tree_makes_itself_is_refused_at_and_below_it() {
    for path in [
        "work",
        "work/x",
        ".kbf",
        ".kbf/action",
        ".kbf-run",
        ".kbf-run/s/t",
        ".kbf-tools",
        ".kbf-tools/kbf-task",
    ] {
        for spelling in ["", "./", "/"] {
            let at = format!("{spelling}{path}");
            for entry in [E::file(&at, 0o644, b"x"), E::dir(&at, 0o755)] {
                assert_eq!(
                    refused(&[layer(&[entry])]),
                    [reserved(&format!("/{path}"))],
                    "{at}"
                );
            }
        }
    }
    // Under a whiteout too: it names a path the tree makes itself.
    assert_eq!(
        refused(&[layer(&[E::whiteout(".kbf/.wh.x")])]),
        [reserved("/.kbf/.wh.x")]
    );
}

/// Asserts a link with no target, with a target that is not UTF-8 or with `..`, and a hard link to
/// a path that is missing, a directory or a symlink are each refused by path and what is wrong.
/// Catches: a link let into the tree with an empty or unreadable target, and a hard link that
/// gets no bytes.
#[test]
fn a_link_that_cannot_be_flattened_is_refused() {
    let base = layer(&[
        E::file("a/real", 0o644, b"real"),
        E::dir("a/dir/", 0o755),
        E::symlink("a/sym", "real"),
    ]);
    let cases = [
        (
            E::symlink("a/l", ""),
            "/a/l: a symlink: a link with no target",
        ),
        (
            E::symlink("a/l", "x").linking(b"\xff"),
            "/a/l: a symlink: a link target that is not UTF-8",
        ),
        (
            E::hardlink("a/h", 0o644, ""),
            "/a/h: a hard link: a link with no target",
        ),
        (
            E::hardlink("a/h", 0o644, "a/../real"),
            "/a/h: a hard link: a path with a `..` component",
        ),
        (
            E::hardlink("a/h", 0o644, "a/nothing"),
            "/a/h: a hard link to /a/nothing, which is not a regular file the layers hold at that point",
        ),
        (
            E::hardlink("a/h", 0o644, "a/dir"),
            "/a/h: a hard link to /a/dir, which is not a regular file the layers hold at that point",
        ),
        (
            E::hardlink("a/h", 0o644, "a/sym"),
            "/a/h: a hard link to /a/sym, which is not a regular file the layers hold at that point",
        ),
        (
            E::hardlink("a/h", 0o644, "."),
            "/a/h: a hard link: a path with no name",
        ),
        (
            E::hardlink("a/h", 0o644, "a/nothing/deeper"),
            "/a/h: a hard link to /a/nothing/deeper, which is not a regular file the layers hold at that point",
        ),
        (
            E::hardlink("a/h", 0o644, "a/real/deeper"),
            "/a/h: a hard link to /a/real/deeper, which is not a regular file the layers hold at that point",
        ),
    ];
    for (entry, wanted) in cases {
        assert_eq!(refused(&[base.clone(), layer(&[entry])]), [wanted]);
    }
}

/// Asserts the root directory's own entry says nothing a tree keeps (its mode is not listed) and
/// the root as anything else is refused.
/// Catches: `./` read as an entry named "" (a tree name tierstore refuses), and a root that is a
/// file passing.
#[test]
fn the_root_entry_of_a_layer_is_a_directory_and_adds_nothing() {
    let tree = built(&[layer(&[E::dir("./", 0o700), E::file("f", 0o644, b"f")])]);
    assert!(tree.normalised.is_empty());
    assert_eq!(listing(&tree).len(), 3);
    assert_eq!(
        refused(&[layer(&[E::file("./", 0o644, b"f")])]),
        ["/: the root is Regular, not a directory"]
    );
}

/// Asserts a name with tierstore's `.wh.` prefix that is not a whiteout (a directory, a symlink, a
/// file with bytes, a bare `.wh.`) is let into the tree and refused by the tree build by its
/// directory and name, and that a whiteout that removed what it names leaves nothing to refuse.
/// Catches: a `.wh.` name left in the tree that tierstore would refuse on every node, and every
/// `.wh.` name treated as a whiteout, which would drop a directory silently.
#[test]
fn a_whiteout_name_left_after_the_whiteouts_is_refused_by_the_tree_build() {
    for (entry, dir, name) in [
        (E::dir("etc/.wh.x/", 0o755), "/etc", ".wh.x"),
        (E::file("etc/.wh.x", 0o644, b"not empty"), "/etc", ".wh.x"),
        (E::symlink("etc/.wh.x", "y"), "/etc", ".wh.x"),
        (E::file("etc/.wh.", 0o644, b""), "/etc", ".wh."),
        (E::file(".wh.top", 0o644, b"x"), "/", ".wh.top"),
        (E::dir(".wh..wh..opq/", 0o755), "/", ".wh..wh..opq"),
    ] {
        assert_eq!(
            refused(&[layer(&[entry])]),
            [format!(
                "the tree is refused: {dir}: the name {name:?} is one a tierstore tree cannot hold"
            )]
        );
    }
    let tree = built(&[
        layer(&[E::file("etc/x", 0o644, b"x")]),
        layer(&[E::whiteout("etc/.wh.x")]),
    ]);
    assert_eq!(listing(&tree).get("/etc"), Some(&Seen::Dir));
    assert!(!listing(&tree).contains_key("/etc/x"));
}

/// Asserts modes are normalised: a file is 0755 if any execute bit is set and else 0644, a
/// directory 0755; 0600, 0640, 0700, 0750 and 1777 (and others) become 0644 or 0755 and are each
/// listed with the mode they had and the one they have, sorted by path (as text: `/a-b` before
/// `/a/b`); a mode that already is 0644, 0755 or a directory's 0755, and a parent no entry made,
/// are not listed; the owner is not in the tree at all.
/// Catches: a mode kept (a tree cannot hold it), a normalised path left out of the facts, one
/// listed that was not changed, and facts in the order the tree is walked, not by path.
#[test]
fn modes_are_normalised_and_every_change_is_listed_by_path() {
    let tree = built(&[layer(&[
        E::file("etc/shadow", 0o640, b"s"),
        E::file("etc/secret", 0o600, b"s"),
        E::file("etc/ok", 0o644, b"ok"),
        E::file("etc/ro", 0o444, b"ro"),
        E::file("bin/run", 0o755, b"run"),
        E::file("bin/me", 0o700, b"me"),
        E::file("bin/grp", 0o750, b"grp"),
        E::file("bin/x", 0o111, b"x"),
        E::file("bin/w", 0o654, b"w"),
        E::dir("tmp/", 0o1777),
        E::dir("home/", 0o700),
        E::dir("fine/", 0o755),
        E::file("deep/er/f", 0o644, b"f"),
        E::file("a-b", 0o600, b"ab"),
        E::file("a/b", 0o600, b"a/b"),
    ])]);
    assert_eq!(
        tree.normalised,
        [
            norm("/a-b", "0600", "0644"),
            norm("/a/b", "0600", "0644"),
            norm("/bin/grp", "0750", "0755"),
            norm("/bin/me", "0700", "0755"),
            norm("/bin/w", "0654", "0755"),
            norm("/bin/x", "0111", "0755"),
            norm("/etc/ro", "0444", "0644"),
            norm("/etc/secret", "0600", "0644"),
            norm("/etc/shadow", "0640", "0644"),
            norm("/home", "0700", "0755"),
            norm("/tmp", "1777", "0755"),
        ]
    );
    let seen = listing(&tree);
    for (path, exec) in [
        ("/etc/shadow", false),
        ("/etc/ro", false),
        ("/bin/me", true),
        ("/bin/x", true),
        ("/bin/w", true),
        ("/bin/run", true),
    ] {
        let Some(Seen::File { exec: got, .. }) = seen.get(path) else {
            panic!("{path} is not a file");
        };
        assert_eq!(*got, exec, "{path}");
    }
}

/// Asserts a set-uid file, a set-gid file, one that is both, a set-uid file with no execute bit
/// and a hard link to a set-uid file are each taken, with the bits dropped like the other mode
/// bits, and listed with the mode they had; the base's set-uid programs (`su`, `passwd`) are such
/// files. Catches: a set-uid or set-gid bit kept in the tree, a file whose bits were dropped left
/// out of the facts, and a release refused for a base that holds one.
#[test]
fn set_uid_and_set_gid_files_are_normalised_and_listed() {
    let tree = built(&[layer(&[
        E::file("usr/bin/su", 0o4755, b"su"),
        E::file("usr/bin/chage", 0o2755, b"chage"),
        E::file("usr/bin/both", 0o6755, b"both"),
        E::file("usr/bin/odd", 0o4644, b"odd"),
        E::hardlink("usr/bin/su2", 0o4755, "usr/bin/su"),
    ])]);
    assert_eq!(
        tree.normalised,
        [
            norm("/usr/bin/both", "6755", "0755"),
            norm("/usr/bin/chage", "2755", "0755"),
            norm("/usr/bin/odd", "4644", "0644"),
            norm("/usr/bin/su", "4755", "0755"),
            norm("/usr/bin/su2", "4755", "0755"),
        ]
    );
    let seen = listing(&tree);
    for (path, exec) in [
        ("/usr/bin/su", true),
        ("/usr/bin/chage", true),
        ("/usr/bin/both", true),
        ("/usr/bin/odd", false),
        ("/usr/bin/su2", true),
    ] {
        let Some(Seen::File { exec: got, .. }) = seen.get(path) else {
            panic!("{path} is not a file");
        };
        assert_eq!(*got, exec, "{path}");
    }
}

/// Asserts a hard link becomes a second file entry with the target's bytes and mode, whether the
/// target is in a lower layer or earlier in the same layer and however the path is spelled, that
/// both paths are listed when the mode is normalised, and that the bundle holds the bytes once.
/// Catches: a hard link dropped or left empty, one that takes the link header's mode instead of
/// the target's, and the bytes written twice.
#[test]
fn a_hard_link_is_a_second_entry_with_the_bytes_and_mode_of_its_target() {
    let tree = built(&[
        layer(&[E::file("usr/bin/real", 0o750, b"BIN")]),
        layer(&[
            E::hardlink("usr/bin/alias", 0o644, "usr/bin/real"),
            E::file("usr/lib/new", 0o644, b"NEW"),
            E::hardlink("usr/lib/new2", 0o600, "./usr/lib/new"),
            E::hardlink("usr/lib/new3", 0o600, "/usr/lib/new"),
        ]),
    ]);
    let seen = listing(&tree);
    for path in ["/usr/bin/real", "/usr/bin/alias"] {
        assert_eq!(seen[path], file("BIN", true), "{path}");
    }
    for path in ["/usr/lib/new", "/usr/lib/new2", "/usr/lib/new3"] {
        assert_eq!(seen[path], file("NEW", false), "{path}");
    }
    assert_eq!(
        tree.normalised,
        [
            norm("/usr/bin/alias", "0750", "0755"),
            norm("/usr/bin/real", "0750", "0755"),
        ]
    );
    let digest = Sha256::of(b"BIN").to_string();
    let objects = objects_of(&tree);
    assert_eq!(objects.iter().filter(|d| **d == digest).count(), 1);
    // BIN, NEW and the kbf-task are the three distinct files; the rest are manifests.
    assert_eq!(objects.len(), tree.built.manifests.len() + 3);
}

/// Asserts `/.kbf-tools` holds exactly `kbf-task`, executable, with the bytes of `--binary`, and
/// that it is not listed as normalised; and that what the layers above the base hold (the
/// image's own `kbf-task` layer, even with what a tree refuses) is not read.
/// Catches: another file in `/.kbf-tools`, a `kbf-task` that is not executable or not the
/// release's build, and the top layer's contents leaking into the base tree.
#[test]
fn kbf_tools_holds_exactly_the_given_kbf_task_and_the_top_layer_is_not_read() {
    let base = [layer(&[E::file("etc/os", 0o644, b"os")])];
    let top = [layer(&[
        E::file("usr/bin/kbf-task", 0o4755, b"the image's own"),
        E::special("dev/null", tar::EntryType::Char),
    ])];
    let img = image(&base, &top, false);
    let tree = tree_of(&img.bytes, &img.base, b"the release's kbf-task".to_vec()).expect("tree");
    let seen = listing(&tree);
    let tools: Vec<(String, Seen)> = seen
        .iter()
        .filter(|(p, _)| p.starts_with("/.kbf-tools"))
        .map(|(p, s)| (p.clone(), s.clone()))
        .collect();
    assert_eq!(
        tools,
        [
            ("/.kbf-tools".to_owned(), Seen::Dir),
            (
                "/.kbf-tools/kbf-task".to_owned(),
                file("the release's kbf-task", true)
            ),
        ]
    );
    assert!(tree.normalised.is_empty());
    assert!(!seen.contains_key("/usr"));
}

/// Asserts two builds of the same base with different `kbf-task` binaries have the same manifest
/// digest for every top-level directory but `/.kbf-tools`, and different `/.kbf-tools` and root
/// digests; and that the same binary gives the same root.
/// Catches: a base subtree whose digest depends on the release (it would be published again by
/// every release), a root that does not change with the binary, and a build that is not a
/// function of its inputs.
#[test]
fn the_same_base_gives_the_same_subtrees_and_another_root_for_another_kbf_task() {
    let layers = [
        layer(&[
            E::file("etc/passwd", 0o644, b"root"),
            E::file("usr/bin/tool", 0o755, b"tool"),
            E::dir("tmp/", 0o1777),
        ]),
        layer(&[E::file("usr/lib/libc", 0o644, b"libc")]),
    ];
    let img = image(&layers, &[], false);
    let make = |binary: &[u8]| tree_of(&img.bytes, &img.base, binary.to_vec()).expect("tree");
    let (one, two, again) = (make(b"release 1"), make(b"release 2"), make(b"release 1"));
    let (a, b) = (top_level(&one), top_level(&two));
    assert_eq!(a.keys().collect::<Vec<_>>(), b.keys().collect::<Vec<_>>());
    for (path, digest) in &a {
        assert_eq!(
            digest == &b[path],
            path != "/.kbf-tools",
            "{path} (the digest of a base subtree changes with the release, or /.kbf-tools does not)"
        );
    }
    assert!(a.contains_key("/etc") && a.contains_key("/usr") && a.contains_key("/tmp"));
    assert_ne!(one.built.root, two.built.root);
    assert_eq!(one.built.root, again.built.root);
    assert_eq!(one.built.manifests, again.built.manifests);
}

/// Asserts an image whose layers do not begin with the base's is refused by name: fewer layers,
/// a layer that is another, the base's layers in another order; a layer blob the image lacks,
/// bytes that are not a tar, and a gzip stream that is not one; and that an image that is no OCI
/// layout is refused.
/// Catches: a tree made of an image that is not the pinned base (the digests the subtrees share
/// would then mean nothing), and a layer that cannot be read passing as empty.
#[test]
fn an_image_whose_layers_are_not_the_bases_is_refused() {
    let (a, b, c) = (
        layer(&[E::file("a", 0o644, b"a")]),
        layer(&[E::file("b", 0o644, b"b")]),
        layer(&[E::file("c", 0o644, b"c")]),
    );
    let digest = |l: &[u8]| format!("sha256:{}", Sha256::of(l));
    let tree_of_img =
        |image: &Img, base: &Img| match tree_of(&image.bytes, &base.base, b"k".to_vec()) {
            Err(Error::Refused(problems)) => problems,
            _ => panic!("not refused"),
        };
    let ab = image(&[a.clone(), b.clone()], &[], false);
    let abc = image(&[a.clone(), b.clone(), c.clone()], &[], false);
    assert_eq!(
        tree_of_img(&ab, &abc),
        ["the image has 2 layers, fewer than the 3 of its base"]
    );
    let ac = image(&[a.clone(), c.clone()], &[], false);
    assert_eq!(
        tree_of_img(&ab, &ac),
        [format!(
            "layer 2 is {}, the base's is {}",
            digest(&b),
            digest(&c)
        )]
    );
    let ba = image(&[b.clone(), a.clone()], &[], false);
    assert_eq!(
        tree_of_img(&ab, &ba),
        [
            format!("layer 1 is {}, the base's is {}", digest(&a), digest(&b)),
            format!("layer 2 is {}, the base's is {}", digest(&b), digest(&a)),
        ]
    );
    // More layers than the base's are fine: the layers above it are ignored.
    assert!(tree_of(&abc.bytes, &ab.base, b"k".to_vec()).is_ok());

    let nothing = image(&[], &[], false);
    let empty_tar = Img {
        bytes: layer(&[]),
        base: nothing.base,
    };
    assert_eq!(
        tree_of_img(&empty_tar, &empty_tar),
        ["the image holds no index.json: not an OCI image layout"]
    );

    let missing = Img {
        bytes: support::without_blob(&ab.bytes, &ab.base.layers[0]),
        base: ab.base.clone(),
    };
    assert_eq!(
        tree_of_img(&missing, &missing),
        [format!(
            "the image holds no blob for the layer {}",
            ab.base.layers[0]
        )]
    );

    let junk: Vec<u8> = (0..2048u32).map(|i| (i % 251) as u8 | 1).collect();
    let bad = image(std::slice::from_ref(&junk), &[], false);
    let got = tree_of_img(&bad, &bad);
    assert_eq!(got.len(), 1, "{got:?}");
    assert!(
        got[0].starts_with(&format!("layer {}: not a tar", digest(&junk))),
        "{got:?}"
    );
    let garbled = image(&[layer(&[E::file("a", 0o644, b"a").garbled()])], &[], false);
    let got = tree_of_img(&garbled, &garbled);
    assert!(got[0].contains(": not a tar: "), "{got:?}");
    let gzip_junk = vec![0x1f, 0x8b, 1, 2, 3];
    let bad = image(std::slice::from_ref(&gzip_junk), &[], false);
    let got = tree_of_img(&bad, &bad);
    assert!(
        got[0].starts_with(&format!("layer {}: not a gzip stream", digest(&gzip_junk))),
        "{got:?}"
    );
}
