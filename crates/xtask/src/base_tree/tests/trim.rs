//! `base-tree`: the packages left out of the base by its dpkg database, what is refused when
//! leaving one out would leave the tree inconsistent, and the set-uid and set-gid files a tree
//! does not hold.

use std::collections::BTreeSet;

use super::super::removed::{EXCLUDED, Excluded, REMOVED, Removed};
use super::super::*;
use super::support::{E, Seen, image, layer, listing};
use super::{COMMIT, file, norm, rows};

/// The status file of the stanzas `stanzas`, as dpkg writes it: each stanza and a blank line.
fn status(stanzas: &[&str]) -> String {
    stanzas.iter().map(|s| format!("{s}\n\n")).collect()
}

const KEEP: &str = "Package: keep\nStatus: install ok installed\nPre-Depends: libc6 (>= 2.34), gone:any | keep\nProvides: toolkit, kit (= 1)\nDescription: kept\n a long\n .\n description";
const GONE: &str =
    "Package: gone\nStatus: install ok installed\nDepends: keep\nDescription: left out";

/// A base with a dpkg database of the packages `keep` and `gone`, and `extra` entries after
/// them: `gone` brings a set-uid `su`, a set-gid `chage`, a conffile, a documentation directory,
/// a directory that also holds a file no package lists, `more` (the one choice of the
/// alternative `pager`, reached through the `/bin` symbolic link), control files with and
/// without an architecture, and a diversion; `keep` brings `tool` and lists `/usr/bin` too.
fn base(stanzas: &[&str], extra: Vec<E>) -> Vec<u8> {
    let mut entries = vec![
        E::symlink("bin", "usr/bin"),
        E::dir("usr/bin/", 0o755),
        E::file("usr/bin/tool", 0o755, b"tool"),
        E::file("usr/bin/su", 0o4755, b"su"),
        E::file("usr/bin/chage", 0o2755, b"chage"),
        E::file("usr/bin/more", 0o755, b"more"),
        E::symlink("usr/bin/pager", "/etc/alternatives/pager"),
        E::symlink("etc/alternatives/pager", "/bin/more"),
        E::file("etc/gone.conf", 0o644, b"conf"),
        E::file("usr/share/doc/gone/copyright", 0o644, b"c"),
        E::file("usr/share/gone/listed", 0o644, b"l"),
        E::file("usr/share/gone/unlisted", 0o644, b"u"),
        E::dir("var/mail/", 0o2775),
        E::file(
            "var/lib/dpkg/status",
            0o644,
            status(stanzas).as_bytes(),
        ),
        E::file(
            "var/lib/dpkg/diversions",
            0o644,
            b"/usr/bin/a\n/usr/bin/a.real\ngone\n/usr/bin/b\n/usr/bin/b.real\nkeep\n",
        ),
        E::file(
            "var/lib/dpkg/alternatives/pager",
            0o644,
            b"auto\n/usr/bin/pager\npager.1.gz\n/usr/share/man/man1/pager.1.gz\n\n/bin/more\n50\n/usr/share/man/man1/more.1.gz\n\n",
        ),
        E::file(
            "var/lib/dpkg/alternatives/shell",
            0o644,
            b"auto\n/usr/bin/tooling\n\n/usr/bin/tool\n10\n\n",
        ),
        E::file("var/lib/dpkg/info/format", 0o644, b"1\n"),
        E::file("var/lib/dpkg/info/keep.list", 0o644, b"/.\n/usr\n/usr/bin\n/usr/bin/tool\n"),
        E::file("var/lib/dpkg/info/keep.md5sums", 0o644, b"k"),
        E::file(
            "var/lib/dpkg/info/gone.list",
            0o644,
            b"/.\n/etc\n/etc/gone.conf\n/usr\n/usr/bin\n/usr/bin/su\n/usr/bin/chage\n/usr/bin/more\n/usr/share\n/usr/share/doc\n/usr/share/doc/gone\n/usr/share/doc/gone/copyright\n/usr/share/gone\n/usr/share/gone/listed\n/usr/share/gone/never-there\n/usr/bin/tool/below-a-file\n",
        ),
        E::file("var/lib/dpkg/info/gone.md5sums", 0o644, b"g"),
        E::file("var/lib/dpkg/info/gone:amd64.postinst", 0o755, b"post\n"),
    ];
    entries.extend(extra);
    layer(&entries)
}

/// The tree of `layers` as a base with the packages `removed` left out.
fn without(layers: &[Vec<u8>], removed: &[&str]) -> Result<Tree, Vec<String>> {
    excluding(layers, removed, &[])
}

/// The tree of `layers` as a base with the packages `removed` and the paths `excluded` left out.
fn excluding(layers: &[Vec<u8>], removed: &[&str], excluded: &[&str]) -> Result<Tree, Vec<String>> {
    let img = image(layers, &[], false);
    match tree_of(
        &img.bytes,
        &img.base,
        removed,
        excluded,
        b"kbf-task".to_vec(),
    ) {
        Ok(tree) => Ok(tree),
        Err(Error::Refused(problems)) => Err(problems),
        Err(e) => panic!("failed, not refused: {e:?}"),
    }
}

/// The problems leaving `removed` out of `layers` is refused with.
fn refused_without(layers: &[Vec<u8>], removed: &[&str]) -> Vec<String> {
    match without(layers, removed) {
        Err(problems) => problems,
        Ok(_) => panic!("accepted"),
    }
}

/// Asserts leaving out a package removes every file and symbolic link its list names (a conffile
/// included), each directory it alone lists once nothing is left in it, its control files with
/// and without an architecture, its stanza of the status file and its diversions, and the
/// alternative whose one choice it brought, with the links of that alternative; and that it keeps
/// a directory a kept package also lists, a directory still holding a file no package lists, a
/// listed path that is not there, and everything of the kept package, byte for byte. The tree is
/// then accepted: the set-uid and set-gid files went with the package, and the set-gid directory
/// `/var/mail` is normalised as before.
/// Catches: a removal that leaves a file of the package, takes a shared directory or one with
/// another file in it, leaves its control files or its stanza (dpkg would still call it
/// installed), drops a kept stanza or diversion, or leaves the links of an alternative pointing at
/// nothing.
#[test]
fn leaving_out_a_package_removes_what_dpkg_lists_for_it_and_nothing_else() {
    let tree = without(&[base(&[KEEP, GONE], vec![])], &["gone"]).expect("accepted");
    let status_after = status(&[KEEP]);
    assert_eq!(
        listing(&tree),
        rows(&[
            ("/.kbf-tools", Seen::Dir),
            ("/.kbf-tools/kbf-task", file("kbf-task", true)),
            ("/bin", Seen::Symlink("usr/bin".to_owned())),
            ("/etc", Seen::Dir),
            ("/etc/alternatives", Seen::Dir),
            ("/usr", Seen::Dir),
            ("/usr/bin", Seen::Dir),
            ("/usr/bin/tool", file("tool", true)),
            ("/usr/share", Seen::Dir),
            ("/usr/share/gone", Seen::Dir),
            ("/usr/share/gone/unlisted", file("u", false)),
            ("/var", Seen::Dir),
            ("/var/lib", Seen::Dir),
            ("/var/lib/dpkg", Seen::Dir),
            ("/var/lib/dpkg/alternatives", Seen::Dir),
            (
                "/var/lib/dpkg/alternatives/shell",
                file("auto\n/usr/bin/tooling\n\n/usr/bin/tool\n10\n\n", false)
            ),
            (
                "/var/lib/dpkg/diversions",
                file("/usr/bin/b\n/usr/bin/b.real\nkeep\n", false)
            ),
            ("/var/lib/dpkg/info", Seen::Dir),
            ("/var/lib/dpkg/info/format", file("1\n", false)),
            (
                "/var/lib/dpkg/info/keep.list",
                file("/.\n/usr\n/usr/bin\n/usr/bin/tool\n", false)
            ),
            ("/var/lib/dpkg/info/keep.md5sums", file("k", false)),
            ("/var/lib/dpkg/status", file(&status_after, false)),
            ("/var/mail", Seen::Dir),
        ])
    );
    assert_eq!(tree.normalised, [norm("/var/mail", "2775", "0755")]);
}

/// Asserts the same base with nothing left out is refused for each set-uid and set-gid file, by
/// path and kind with its mode, sorted by path, and that a set-uid file with no execute bit, one
/// that is both and a hard link to a set-uid file are refused alike; a set-gid or sticky
/// directory is not refused.
/// Catches: a set-uid or set-gid file normalised into the tree (its facts then list it), one
/// missed because it is a hard link or not executable, and a directory's set-gid bit taken for a
/// file's.
#[test]
fn a_set_uid_or_set_gid_file_left_in_the_base_is_refused_by_path() {
    let extra = vec![
        E::file("usr/bin/both", 0o6755, b"both"),
        E::file("usr/bin/odd", 0o4644, b"odd"),
        E::hardlink("usr/bin/su2", 0o4755, "usr/bin/su"),
        E::dir("tmp/", 0o1777),
    ];
    let refusal = |path: &str, kind: &str, mode: &str| {
        format!(
            "{path}: a {kind} file (mode {mode}); the kbf base tree holds none: leave out the package that brings it"
        )
    };
    assert_eq!(
        refused_without(&[base(&[KEEP, GONE], extra)], &[]),
        [
            refusal("/usr/bin/both", "set-uid and set-gid", "6755"),
            refusal("/usr/bin/chage", "set-gid", "2755"),
            refusal("/usr/bin/odd", "set-uid", "4644"),
            refusal("/usr/bin/su", "set-uid", "4755"),
            refusal("/usr/bin/su2", "set-uid", "4755"),
        ]
    );
}

/// Asserts a kept package's dependency on a package left out is refused unless a kept package
/// satisfies the same group, by name or by what it provides; a dependency on a package the base
/// does not hold at all is not refused (it was never satisfied by the base).
/// Catches: a removal that breaks a kept package's `Depends` or `Pre-Depends`, and one refused
/// although a kept alternative or provider satisfies it.
#[test]
fn a_kept_package_that_needs_a_package_left_out_is_refused() {
    let needy = "Package: needy\nDepends: gone (>= 1), libc6\nPre-Depends: gone | elsewhere";
    assert_eq!(
        refused_without(&[base(&[KEEP, GONE, needy], vec![])], &["gone"]),
        [
            "needy: needs gone | elsewhere, which only packages left out give",
            "needy: needs gone (>= 1), which only packages left out give",
        ]
    );
    let served =
        "Package: served\nDepends: gone | toolkit, gone | keep:any, kit (>= 1) | gone, absent";
    without(&[base(&[KEEP, GONE, served], vec![])], &["gone"]).expect("a kept provider serves");
}

/// Asserts each way leaving packages out would leave the tree inconsistent is refused by name,
/// with every problem at once, and that a refused removal changes nothing (the tree is not
/// written): a package the status file does not list, a package with no file list, a listed
/// path below a symbolic link, an alternative with a choice kept and one left out, a symbolic
/// link left pointing below a removed directory, and a file list that is a directory.
/// Catches: a removal that goes ahead with a misspelled package (the tree then keeps what the
/// list meant to drop), one that misses files reached through a symbolic link, one that leaves a
/// kept link dangling, and one that silently ignores a database it cannot read.
#[test]
fn an_inconsistent_removal_is_refused_with_every_problem() {
    let ghost = "Package: nolist\nStatus: install ok installed";
    let extra = vec![
        E::file("usr/lib/gone/libgone.so", 0o644, b"so"),
        E::symlink("usr/lib/libgone.so", "gone/libgone.so"),
        E::file("usr/bin/vi", 0o755, b"vi"),
        E::file(
            "var/lib/dpkg/alternatives/editor",
            0o644,
            b"manual\n/usr/bin/editor\n\n/usr/bin/vi\n10\n/bin/more\n20\n\n",
        ),
        E::file(
            "var/lib/dpkg/info/other.list",
            0o644,
            b"/bin/tool\n/usr/lib/gone\n/usr/lib/gone/libgone.so\n",
        ),
        E::file("var/lib/dpkg/info/bad.list", 0o644, b"/usr/\xff\n"),
        E::dir("var/lib/dpkg/info/broken.list/", 0o755),
        E::file(
            "var/lib/dpkg/info/keep2.list",
            0o644,
            b"/usr/lib/libgone.so\n",
        ),
    ];
    let other = "Package: other\nStatus: install ok installed";
    assert_eq!(
        refused_without(
            &[base(&[KEEP, GONE, ghost, other], extra)],
            &["gone", "missing", "nolist", "other"]
        ),
        [
            "missing: not a package of the base (/var/lib/dpkg/status does not list it)",
            "/var/lib/dpkg/info/bad.list: not UTF-8",
            "/var/lib/dpkg/info/broken.list: not a regular file",
            "nolist: no file list (/var/lib/dpkg/info/nolist.list or nolist:<arch>.list)",
            "/bin/tool: listed by other below the symbolic link /bin, which the removal does not follow",
            "/var/lib/dpkg/alternatives/editor: the alternative editor keeps a choice of a kept package and loses one of a package left out",
            "/usr/lib/libgone.so: a symbolic link a kept package lists, to gone/libgone.so, which the packages left out remove",
        ]
    );
}

/// Asserts a dpkg database that is not as dpkg writes it is refused by name: no status file, a
/// status file that is not UTF-8, a line that is not a field, a continuation line before any
/// field, a stanza with no `Package`, diversions not in threes or not UTF-8, no file list at all
/// (no `/var/lib/dpkg/info`), and an alternative with no master link, followers that do not end,
/// or a choice cut short; and that leaving nothing out reads no database at all.
/// Catches: a removal that guesses at a database it cannot read, and a base with no dpkg
/// database refused although nothing is to be left out.
#[test]
fn a_dpkg_database_that_cannot_be_read_is_refused_by_name() {
    let plain = layer(&[E::file("usr/bin/tool", 0o755, b"tool")]);
    without(std::slice::from_ref(&plain), &[]).expect("nothing to leave out reads nothing");
    assert_eq!(
        refused_without(&[plain], &["gone"]),
        ["/var/lib/dpkg/status: none, so the packages to leave out (gone) are not known"]
    );
    let bad_status = |bytes: &[u8]| {
        refused_without(
            &[layer(&[E::file("var/lib/dpkg/status", 0o644, bytes)])],
            &["gone"],
        )
    };
    assert_eq!(
        bad_status(b"Package: \xff\n"),
        ["/var/lib/dpkg/status: not UTF-8"]
    );
    assert_eq!(
        bad_status(b"Package: a\nno colon\n"),
        ["/var/lib/dpkg/status: a line that is not a field: \"no colon\""]
    );
    assert_eq!(
        bad_status(b" folded\nPackage: a\n"),
        ["/var/lib/dpkg/status: a continuation line before any field: \" folded\""]
    );
    assert_eq!(
        bad_status(b"Status: install ok installed\n"),
        ["/var/lib/dpkg/status: a stanza with no Package field: \"Status: install ok installed\""]
    );
    let with = |path: &str, bytes: &[u8]| {
        refused_without(
            &[base(&[KEEP, GONE], vec![E::file(path, 0o644, bytes)])],
            &["gone"],
        )
    };
    assert_eq!(
        with("var/lib/dpkg/diversions", b"/a\n/a.real\n"),
        ["/var/lib/dpkg/diversions: 2 lines, not three for each diversion"]
    );
    assert_eq!(
        with("var/lib/dpkg/diversions", b"\xff"),
        ["/var/lib/dpkg/diversions: not UTF-8"]
    );
    let status_only = layer(&[E::file(
        "var/lib/dpkg/status",
        0o644,
        status(&[KEEP, GONE]).as_bytes(),
    )]);
    assert_eq!(
        refused_without(&[status_only], &["gone"]),
        ["gone: no file list (/var/lib/dpkg/info/gone.list or gone:<arch>.list)"]
    );
    for (bytes, why) in [
        (&b""[..], "empty"),
        (b"auto\nrelative\n", "no absolute master link"),
        (b"auto\n/usr/bin/x\n", "the followers do not end"),
        (
            b"auto\n/usr/bin/x\nx.1\nrelative\n",
            "a follower with no absolute link",
        ),
        (
            b"auto\n/usr/bin/x\n\n/bin/more\n",
            "a choice that does not end",
        ),
    ] {
        assert_eq!(
            with("var/lib/dpkg/alternatives/x", bytes),
            [format!("/var/lib/dpkg/alternatives/x: {why}")]
        );
    }
}

/// Asserts a symbolic link a kept package lists is resolved as the kernel resolves it to find
/// what it points at: a relative target with `..` or `.` along it or ending in one, an absolute one,
/// one through a chain of relative or absolute links, and a loop (which points at nothing removed
/// and is kept); each that points at or below a removed path is refused.
/// Catches: a kept package's link to a removed file missed because its target is spelled through
/// `..` or another link, and a loop that hangs the build.
#[test]
fn a_link_a_kept_package_lists_into_a_removed_package_is_refused_however_it_is_spelled() {
    let extra = vec![
        E::symlink("usr/lib/up", "../share/doc/../doc/gone/copyright"),
        E::symlink("usr/lib/abs", "/bin/more"),
        E::symlink("usr/lib/chain", "/usr/lib/hop/copyright"),
        E::symlink("usr/lib/hop", "../share/doc/gone"),
        E::symlink("usr/lib/loop1", "loop2/x"),
        E::symlink("usr/lib/loop2", "loop1/x"),
        E::symlink("usr/lib/dot", "../share/./doc/gone/.").raw(),
        E::symlink("usr/lib/far", "/usr/lib/abshop/copyright"),
        E::symlink("usr/lib/abshop", "/usr/share/doc/gone"),
        E::symlink("usr/lib/updir", "../share/doc/gone/sub/.."),
        E::file(
            "var/lib/dpkg/info/linker.list",
            0o644,
            b"/usr/lib/up\n/usr/lib/abs\n/usr/lib/chain\n/usr/lib/hop\n/usr/lib/loop1\n/usr/lib/loop2\n/usr/lib/dot\n/usr/lib/far\n/usr/lib/abshop\n/usr/lib/updir\n",
        ),
    ];
    let refusal = |link: &str, target: &str| {
        format!(
            "{link}: a symbolic link a kept package lists, to {target}, which the packages left out remove"
        )
    };
    assert_eq!(
        refused_without(&[base(&[KEEP, GONE], extra)], &["gone"]),
        [
            refusal("/usr/lib/abs", "/bin/more"),
            refusal("/usr/lib/abshop", "/usr/share/doc/gone"),
            refusal("/usr/lib/chain", "/usr/lib/hop/copyright"),
            refusal("/usr/lib/dot", "../share/./doc/gone/."),
            refusal("/usr/lib/far", "/usr/lib/abshop/copyright"),
            refusal("/usr/lib/hop", "../share/doc/gone"),
            refusal("/usr/lib/up", "../share/doc/../doc/gone/copyright"),
            refusal("/usr/lib/updir", "../share/doc/gone/sub/.."),
        ]
    );
}

/// Asserts a symbolic link no package lists that points at a removed file (what a removed
/// package's maintainer script made, such as an enabled unit) goes with the package, and so does
/// a link that pointed at that link; a link no package lists that pointed at nothing before the
/// removal (a minimal image drops the files of some links) is kept, as is the directory the
/// removed link was in; an alternative's links are such leftovers when there is no alternatives
/// directory to read.
/// Catches: a tree left with links into what was removed, a chain of such links removed only in
/// part, and a link that was already dangling taken for one the removal broke.
#[test]
fn a_link_no_package_lists_into_a_removed_package_goes_with_it() {
    let extra = vec![
        E::file("usr/lib/systemd/system/gone.timer", 0o644, b"timer"),
        E::symlink(
            "etc/systemd/system/timers.target.wants/gone.timer",
            "/usr/lib/systemd/system/gone.timer",
        ),
        E::symlink("etc/second", "systemd/system/timers.target.wants/gone.timer"),
        E::dir("usr/share/locale/x/LC_MESSAGES/", 0o755),
        E::symlink("usr/share/locale/x/LC_TIME/m.mo", "../LC_MESSAGES/m.mo"),
        E::file(
            "var/lib/dpkg/info/gone.list",
            0o644,
            b"/usr/bin/su\n/usr/bin/chage\n/usr/bin/more\n/usr/lib/systemd/system/gone.timer\n/usr/share/locale/x/LC_MESSAGES\n",
        ),
    ];
    let tree = without(&[base(&[KEEP, GONE], extra)], &["gone"]).expect("accepted");
    let seen = listing(&tree);
    for gone in [
        "/usr/lib/systemd/system/gone.timer",
        "/etc/systemd/system/timers.target.wants/gone.timer",
        "/etc/second",
        "/usr/share/locale/x/LC_MESSAGES",
    ] {
        assert!(!seen.contains_key(gone), "{gone} is left");
    }
    assert_eq!(
        seen.get("/usr/share/locale/x/LC_TIME/m.mo"),
        Some(&Seen::Symlink("../LC_MESSAGES/m.mo".to_owned()))
    );
    assert_eq!(
        seen.get("/etc/systemd/system/timers.target.wants"),
        Some(&Seen::Dir)
    );

    // With no alternatives directory to read, the pager's links are such leftovers too.
    let flat = vec![E::file(
        "var/lib/dpkg/alternatives",
        0o644,
        b"not a directory",
    )];
    let tree = without(&[base(&[KEEP, GONE], flat)], &["gone"]).expect("accepted");
    let seen = listing(&tree);
    assert!(!seen.contains_key("/usr/bin/pager"));
    assert!(!seen.contains_key("/etc/alternatives/pager"));
}

/// Asserts a path of a kept package is left out as dpkg's `path-exclude` leaves one out: the file
/// goes and the package's file list still names it, so a set-uid file of a package the tree keeps
/// does not refuse the tree; and that a path to exclude is refused by name when it is not
/// absolute, when no kept package lists it (a removed package's does not count), when it is not
/// in the base, and when it is a directory.
/// Catches: a set-uid program kept because its package must stay, a package's list rewritten as
/// if it were removed, and an exclusion that does nothing because the path is misspelled or a
/// later base no longer has it.
#[test]
fn a_path_of_a_kept_package_is_left_out_as_dpkg_excludes_it() {
    let keep_list = "/.\n/usr\n/usr/bin\n/usr/bin/tool\n/usr/bin/setid\n/usr/bin/absent\n";
    let layers = [base(
        &[KEEP, GONE],
        vec![
            E::file("usr/bin/setid", 0o4755, b"setid"),
            E::file("var/lib/dpkg/info/keep.list", 0o644, keep_list.as_bytes()),
        ],
    )];
    assert_eq!(
        refused_without(&layers, &["gone"]),
        [
            "/usr/bin/setid: a set-uid file (mode 4755); the kbf base tree holds none: leave out the package that brings it"
        ]
    );
    let tree = excluding(&layers, &["gone"], &["/usr/bin/setid"]).expect("accepted");
    let seen = listing(&tree);
    assert!(!seen.contains_key("/usr/bin/setid"));
    assert_eq!(seen["/usr/bin/tool"], file("tool", true));
    assert_eq!(seen["/var/lib/dpkg/info/keep.list"], file(keep_list, false));
    let Err(problems) = excluding(
        &layers,
        &["gone"],
        &[
            "usr/bin/setid",
            "/usr/share/gone/listed",
            "/usr/bin/absent",
            "/usr/bin",
            "/usr/bin/setid",
        ],
    ) else {
        panic!("accepted");
    };
    assert_eq!(
        problems,
        [
            "usr/bin/setid: not an absolute path",
            "/usr/share/gone/listed: no kept package lists it",
            "/usr/bin/absent: not in the base",
            "/usr/bin: a directory, not a file",
        ]
    );
}

/// Asserts the verb leaves out the packages and paths it is given and prints one line for each,
/// with its reason, after the lines naming the bundle.
/// Catches: a release whose log does not say what the base tree left out, or why.
#[test]
fn the_verb_prints_each_package_and_path_left_out_with_its_reason() {
    let dir = super::scratch("trim-verb");
    let img = image(&[base(&[KEEP, GONE], vec![])], &[layer(&[])], true);
    std::fs::write(dir.join("image.oci.tar"), &img.bytes).expect("image");
    std::fs::write(dir.join("kbf-task"), b"k").expect("binary");
    let args = Args {
        image: dir.join("image.oci.tar"),
        platform: Platform::LinuxX86_64,
        commit: COMMIT.to_owned(),
        binary: dir.join("kbf-task"),
        out: dir.join("dist"),
    };
    let removed = [Removed {
        package: "gone",
        why: "a test's reason",
    }];
    let excluded = [Excluded {
        path: "/usr/bin/tool",
        why: "another reason",
    }];
    let printed = run_with(&args, &img.base, &removed, &excluded).expect("run");
    assert_eq!(printed.len(), 5);
    assert_eq!(printed[3], "left out gone: a test's reason");
    assert_eq!(printed[4], "left out /usr/bin/tool: another reason");
}

/// Asserts the packages and paths the release leaves out are each named once, each with a
/// reason, and that `su`, the one set-uid program of a package the tree keeps, is one of them.
/// Catches: a package or path listed twice or with no reason given for leaving it out.
#[test]
fn each_package_and_path_the_release_leaves_out_is_named_once_with_a_reason() {
    let names: BTreeSet<&str> = REMOVED.iter().map(|r| r.package).collect();
    assert_eq!(names.len(), REMOVED.len());
    let paths: BTreeSet<&str> = EXCLUDED.iter().map(|e| e.path).collect();
    assert_eq!(paths.len(), EXCLUDED.len());
    assert!(paths.contains("/usr/bin/su"));
    for (what, why) in REMOVED
        .iter()
        .map(|r| (r.package, r.why))
        .chain(EXCLUDED.iter().map(|e| (e.path, e.why)))
    {
        assert!(!why.trim().is_empty(), "{what}");
    }
}
