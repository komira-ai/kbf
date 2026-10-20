use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::sha1::{cksum, sha1_hex};
use super::tree::Node;
use super::*;

mod read;

const BINARY_BYTES: &[u8] = b"kbf-installer's bytes";

/// An empty directory of its own for one test.
fn scratch(name: &str) -> PathBuf {
    let dir = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps")
        .join("xtask-pkg-unit")
        .join(format!("{name}-{}", std::process::id()));
    // Absent unless a run with this pid left it.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// The committed LaunchDaemon property list.
fn plist_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packaging/macos/com.komira.kbf-installer.plist")
}

fn plist_text() -> String {
    std::fs::read_to_string(plist_path()).expect("the committed property list")
}

/// A binary, the committed property list and two trust files in `dir`.
struct Files {
    binary: PathBuf,
    plist: PathBuf,
    trust: Vec<PathBuf>,
}

fn files(dir: &Path, binary: &[u8]) -> Files {
    let f = Files {
        binary: dir.join("kbf-installer"),
        plist: dir.join("job.plist"),
        trust: vec![dir.join("trusted_root.jsonl"), dir.join("release.pub")],
    };
    std::fs::write(&f.binary, binary).expect("binary");
    std::fs::write(&f.plist, plist_text()).expect("plist");
    std::fs::write(&f.trust[0], b"{\"root\":1}\n").expect("trust");
    std::fs::write(&f.trust[1], b"key\n").expect("trust");
    f
}

fn build_with(f: &Files, version: &str, out: &Path) -> Result<Vec<String>, Error> {
    build(&Build {
        binary: &f.binary,
        plist: &f.plist,
        trust: &f.trust,
        version,
        out,
    })
}

/// The package `pkg build` writes from `BINARY_BYTES` and the committed files.
fn built(name: &str) -> Vec<u8> {
    let dir = scratch(name);
    let f = files(&dir, BINARY_BYTES);
    build_with(&f, "1.2.3", &dir.join("out")).expect("build");
    std::fs::read(dir.join("out").join(pkg_name("1.2.3"))).expect("the package")
}

fn err_text(e: Error) -> String {
    e.to_string()
}

#[test]
fn sha1_matches_the_standard_vectors() {
    assert_eq!(sha1_hex(b""), "da39a3ee5e6b4b0d3255bfef95601890afd80709");
    assert_eq!(sha1_hex(b"abc"), "a9993e364706816aba3e25717850c26c9cd0d89d");
    // 56 bytes: the length does not fit the block that holds the padding byte.
    assert_eq!(
        sha1_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
        "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
    );
    // 55 bytes: the padding byte and the length fill the block exactly.
    assert_eq!(
        sha1_hex(&[b'a'; 55]),
        "c1c8bbdc22796e28c0e15163d20899b65621d65a"
    );
    assert_eq!(
        sha1_hex(&vec![b'a'; 1_000_000]),
        "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
    );
}

#[test]
fn cksum_matches_the_cksum_program() {
    // `printf ... | cksum`, which prints this number and the length.
    assert_eq!(cksum(b""), 4_294_967_295);
    assert_eq!(cksum(b"abc"), 1_219_131_554);
    assert_eq!(cksum(b"123456789"), 930_766_865);
    assert_eq!(cksum(b"hello"), 3_287_646_509);
    // A length of two bytes, and of three, is fed in as many bytes.
    assert_eq!(cksum(&[0; 300]), 351_385_237);
    assert_eq!(cksum(&vec![0; 65_536]), 4_215_202_376);
}

#[test]
fn a_tree_lists_directories_before_what_they_hold_and_adds_the_ones_above_a_file() {
    let mut t = Tree::rooted();
    t.file("usr/local/bin/tool", 0o755, b"x".to_vec()).unwrap();
    t.dir("usr/local/etc").unwrap();
    t.dir("usr/local/etc").unwrap();
    t.dir("Library/Logs/kbf").unwrap();
    let paths: Vec<&str> = t.nodes().map(|(p, _)| p).collect();
    assert_eq!(
        paths,
        [
            ".",
            "Library",
            "Library/Logs",
            "Library/Logs/kbf",
            "usr",
            "usr/local",
            "usr/local/bin",
            "usr/local/bin/tool",
            "usr/local/etc",
        ]
    );
    assert_eq!(t.count(), 9);
    assert_eq!(t.file_bytes(), 1);
}

#[test]
fn a_directory_has_the_mode_it_is_given_once_and_0755_otherwise() {
    let mut t = Tree::rooted();
    t.dir_mode("var/db/state", 0o700).unwrap();
    // Again with the same mode is fine; the directories above are the default.
    t.dir_mode("var/db/state", 0o700).unwrap();
    t.dir("var/db").unwrap();
    t.file("var/db/state/f", 0o600, vec![]).unwrap();
    let modes: Vec<_> = t
        .nodes()
        .filter_map(|(p, n)| match n {
            Node::Dir { mode } => Some((p, *mode)),
            Node::File { .. } => None,
        })
        .collect();
    assert_eq!(
        modes,
        [
            (".", 0o755),
            ("var", 0o755),
            ("var/db", 0o755),
            ("var/db/state", 0o700)
        ]
    );
    // Another mode for a directory already there, in either order, is refused, as is a
    // directory added plain and then closed.
    assert_eq!(
        t.dir_mode("var/db/state", 0o755).unwrap_err(),
        "var/db/state is in the package with mode 700 and 755"
    );
    assert_eq!(
        t.dir_mode("var/db", 0o700).unwrap_err(),
        "var/db is in the package with mode 755 and 700"
    );
    assert_eq!(
        t.dir_mode("var/db/state/f", 0o700).unwrap_err(),
        "var/db/state/f is a file and a directory"
    );
    assert_eq!(
        t.dir_mode("x", 0o10_0700).unwrap_err(),
        "x: mode 100700 is more than permission bits"
    );
    assert!(t.dir_mode("a/../b", 0o700).is_err());
    // The state is a directory and a plain `dir` leaves its mode.
    t.dir("var/db/state").unwrap();
    assert_eq!(t.count(), 5);
}

#[test]
fn a_tree_refuses_what_a_package_cannot_hold() {
    let mut t = Tree::rooted();
    t.file("a/b", 0o644, vec![]).unwrap();
    for bad in [
        "", "/a", "a/", "a//b", "a/./b", "a/../b", "..", "a\nb", "a/b\0",
    ] {
        let e = t.file(bad, 0o644, vec![]).unwrap_err();
        assert!(e.contains("not a clean relative path"), "{bad:?}: {e}");
        assert!(t.dir(bad).is_err(), "{bad:?}");
    }
    assert_eq!(
        t.file("c", 0o10_0644, vec![]).unwrap_err(),
        "c: mode 100644 is more than permission bits"
    );
    assert_eq!(
        t.file("a/b", 0o644, vec![]).unwrap_err(),
        "a/b is in the package twice"
    );
    assert_eq!(t.dir("a/b").unwrap_err(), "a/b is a file and a directory");
    assert_eq!(
        t.file("a/b/c", 0o644, vec![]).unwrap_err(),
        "a/b is a file and a directory"
    );
    // Nothing was added by the refusals.
    assert_eq!(t.count(), 3);
    // The set-id and sticky bits are allowed.
    t.file("s", 0o4755, vec![]).unwrap();
}

#[test]
fn the_payload_is_the_tree_as_a_gzip_odc_archive_of_root_entries() {
    let mut t = Tree::rooted();
    t.file("usr/local/bin/tool", 0o755, b"tool".to_vec())
        .unwrap();
    t.file("usr/share/x", 0o644, b"".to_vec()).unwrap();
    let gz = cpio::payload(&t);
    // No file name, time 0, Unix.
    assert_eq!(
        (&gz[..4], &gz[4..8], gz[9]),
        (&[0x1f, 0x8b, 8, 0][..], &[0u8; 4][..], 3)
    );
    let got: Vec<_> = read::cpio(&gz)
        .unwrap()
        .into_iter()
        .map(|m| (m.name, m.mode, m.links, m.uid, m.gid, m.mtime, m.bytes))
        .collect();
    let dir = |n: &str| (n.to_owned(), 0o40755, 2, 0, 0, 0, vec![]);
    assert_eq!(
        got,
        [
            dir("."),
            dir("./usr"),
            dir("./usr/local"),
            dir("./usr/local/bin"),
            (
                "./usr/local/bin/tool".to_owned(),
                0o100755,
                1,
                0,
                0,
                0,
                b"tool".to_vec()
            ),
            dir("./usr/share"),
            ("./usr/share/x".to_owned(), 0o100644, 1, 0, 0, 0, vec![]),
        ]
    );
    // No two entries are hard links of each other.
    let inodes: BTreeSet<_> = read::cpio(&gz).unwrap().iter().map(|m| m.inode).collect();
    assert_eq!(inodes.len(), 7);
}

/// A tree shaped so that numbering by depth, children by name, differs from numbering
/// by path.
fn numbering_tree() -> Tree {
    let mut t = Tree::rooted();
    t.file("usr/local/bin/tool", 0o755, b"tool bytes".to_vec())
        .unwrap();
    t.file("usr/local/share/x", 0o644, b"x".to_vec()).unwrap();
    t.file("Library/LaunchDaemons/p", 0o644, b"plist".to_vec())
        .unwrap();
    t.dir("Library/Logs/kbf").unwrap();
    t
}

#[test]
fn the_bill_numbers_paths_by_depth_with_children_by_name() {
    let (paths, total) = read::bom(&bom::bom(&numbering_tree()).unwrap()).unwrap();
    let got: Vec<_> = paths
        .iter()
        .map(|p| (p.id, p.parent, p.name.as_str()))
        .collect();
    assert_eq!(
        got,
        [
            (1, 0, "."),
            (2, 1, "Library"),
            (3, 1, "usr"),
            (4, 2, "LaunchDaemons"),
            (5, 2, "Logs"),
            (6, 3, "local"),
            (7, 4, "p"),
            (8, 5, "kbf"),
            (9, 6, "bin"),
            (10, 6, "share"),
            (11, 9, "tool"),
            (12, 10, "x"),
        ]
    );
    assert_eq!(total, 13);
}

#[test]
fn the_bill_gives_each_path_its_type_mode_owner_time_size_and_checksum() {
    let (paths, _) = read::bom(&bom::bom(&numbering_tree()).unwrap()).unwrap();
    for p in &paths {
        assert_eq!((p.uid, p.gid, p.mtime), (0, 0, 0), "{}", p.name);
    }
    let by_name = |n: &str| paths.iter().find(|p| p.name == n).unwrap();
    let dir = by_name("local");
    assert_eq!(
        (dir.kind, dir.mode, dir.size, dir.cksum),
        (2, 0o40755, 0, 0)
    );
    let root = by_name(".");
    assert_eq!((root.kind, root.mode), (2, 0o40755));
    let tool = by_name("tool");
    assert_eq!(
        (tool.kind, tool.mode, tool.size, tool.cksum),
        (1, 0o100755, 10, cksum(b"tool bytes"))
    );
    let plist = by_name("p");
    assert_eq!((plist.kind, plist.mode), (1, 0o100644));
}

#[test]
fn the_bill_holds_one_leaf_of_paths_and_refuses_more() {
    let tree_of = |files: usize| {
        let mut t = Tree::rooted();
        for i in 0..files {
            t.file(&format!("f{i}"), 0o644, vec![]).unwrap();
        }
        t
    };
    let full = bom::bom(&tree_of(bom::LEAF_PATHS - 1)).unwrap();
    assert_eq!(read::bom(&full).unwrap().0.len(), bom::LEAF_PATHS);
    assert_eq!(
        bom::bom(&tree_of(bom::LEAF_PATHS)).unwrap_err(),
        "257 paths; a bill of materials of this writer holds 256"
    );
}

#[test]
fn the_archive_holds_its_members_and_the_reader_refuses_a_changed_byte() {
    let x = xar::xar(&[("Bom", b"bom"), ("Payload", b""), ("PackageInfo", b"info")]);
    let got = read::xar(&x).unwrap();
    assert_eq!(
        got.into_iter().collect::<Vec<_>>(),
        [
            ("Bom".to_owned(), b"bom".to_vec()),
            ("PackageInfo".to_owned(), b"info".to_vec()),
            ("Payload".to_owned(), vec![]),
        ]
    );
    // The controls: a byte of the header, the table and the heap each changed.
    let last = x.len() - 1;
    for (at, why) in [
        (0, "not a xar"),
        (30, "does not match its checksum"),
        (last, "does not match"),
    ] {
        let mut bad = x.clone();
        bad[at] ^= 1;
        let e = read::xar(&bad).unwrap_err();
        assert!(e.contains(why), "byte {at}: {e}");
    }
}

#[test]
fn the_gate_accepts_the_committed_property_list_in_any_layout() {
    assert_eq!(gate_problems(&plist_text()), Vec::<String>::new());
    let compact: String = plist_text().lines().map(str::trim).collect();
    assert_eq!(gate_problems(&compact), Vec::<String>::new());
}

#[test]
fn the_gate_refuses_a_job_that_starts_without_the_configuration() {
    let good = plist_text();
    let mutant = |from: &str, to: &str| {
        assert!(good.contains(from), "{from}");
        gate_problems(&good.replace(from, to))
    };
    // RunAtLoad true, or gone.
    let p = mutant(
        "<key>RunAtLoad</key>\n\t<false/>",
        "<key>RunAtLoad</key>\n\t<true/>",
    );
    assert_eq!(p.len(), 1);
    assert!(p[0].contains("is not started at load"), "{p:?}");
    assert_eq!(mutant("<key>RunAtLoad</key>", "<key>Other</key>").len(), 1);
    // Kept alive without a condition, on another path, or on its absence.
    for to in [
        "<key>KeepAlive</key>\n\t<true/>\n\t<key>Unused</key>\n\t<dict>",
        "<key>KeepAlive</key>\n\t<dict>\n\t\t<key>SuccessfulExit</key>\n\t\t<false/>\n\t\t<key>X</key>\n\t\t<dict>",
    ] {
        let p = mutant(
            "<key>KeepAlive</key>\n\t<dict>\n\t\t<key>PathState</key>\n\t\t<dict>",
            to,
        );
        assert!(
            p.iter()
                .any(|l| l.contains("kept alive only while the gate file exists")),
            "{p:?}"
        );
    }
    let p = mutant(
        "<key>/usr/local/etc/kbf/installer.toml</key>",
        "<key>/etc/kbf/installer.toml</key>",
    );
    assert_eq!(p.len(), 1, "{p:?}");
    let p = mutant(
        "<key>/usr/local/etc/kbf/installer.toml</key>\n\t\t\t<true/>",
        "<key>/usr/local/etc/kbf/installer.toml</key>\n\t\t\t<false/>",
    );
    assert_eq!(p.len(), 1, "{p:?}");
    // Another job's label.
    let p = mutant(
        "<string>com.komira.kbf-installer</string>",
        "<string>com.komira.other</string>",
    );
    assert_eq!(p.len(), 1);
    assert!(p[0].contains("is labelled"), "{p:?}");
    // Each way to start a job that the gate does not hold back.
    for key in TRIGGERS {
        let p = mutant(
            "</dict>\n</plist>",
            &format!("<key>{key}</key>\n<integer>5</integer>\n</dict>\n</plist>"),
        );
        assert_eq!(p.len(), 1, "{key}: {p:?}");
        assert!(p[0].contains(key), "{p:?}");
    }
    // Another program, or the installed one in another mode: the job must run the
    // installed tool's service mode.
    for (from, to) in [
        (
            "<string>/usr/local/kbf-installer/bin/kbf-installer</string>",
            "<string>/usr/local/bin/kbf-installer</string>",
        ),
        ("<string>run</string>", "<string>setup</string>"),
        ("<string>run</string>", "<string>serve</string>"),
        (
            "<string>com.komira.kbf-daemon</string>",
            "<string>com.komira.other</string>",
        ),
        (
            "<string>--launchd-label</string>",
            "<string>--unit</string>",
        ),
    ] {
        let p = mutant(from, to);
        assert_eq!(p.len(), 1, "{to}: {p:?}");
        assert!(
            p[0].contains("runs the installed tool in its service mode"),
            "{p:?}"
        );
    }
    // Nothing at all.
    assert_eq!(gate_problems("").len(), 4);
}

#[test]
fn the_package_holds_bom_payload_and_package_info_and_no_scripts() {
    let pkg = built("contents");
    let members = read::xar(&pkg).unwrap();
    assert_eq!(
        members.keys().map(String::as_str).collect::<Vec<_>>(),
        ["Bom", "PackageInfo", "Payload"]
    );
    let info = String::from_utf8(members["PackageInfo"].clone()).unwrap();
    // The binary, the property list and the two trust files, to the next KiB.
    let kbytes = (BINARY_BYTES.len() + plist_text().len() + 11 + 4).div_ceil(1024);
    assert_eq!(
        info,
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?>\n\
         <pkg-info format-version=\"2\" identifier=\"com.komira.kbf-installer\" version=\"1.2.3\" \
         install-location=\"/\" auth=\"root\">\n\
         <payload numberOfFiles=\"18\" installKBytes=\"{kbytes}\"/>\n\
         </pkg-info>\n"
        )
    );
}

#[test]
fn the_payload_and_the_bill_list_the_same_paths_and_the_files_the_package_installs() {
    let members = read::xar(&built("paths")).unwrap();
    let payload = read::cpio(&members["Payload"]).unwrap();
    let (bill, _) = read::bom(&members["Bom"]).unwrap();
    // The bill's paths, joined up from their names and parents.
    let full = |p: &read::BomPath| {
        let mut parts = vec![p.name.clone()];
        let mut at = p.parent;
        while at != 0 {
            let up = bill.iter().find(|q| q.id == at).unwrap();
            parts.push(up.name.clone());
            at = up.parent;
        }
        parts.reverse();
        parts.join("/")
    };
    let from_bill: BTreeSet<String> = bill.iter().map(full).collect();
    let from_payload: BTreeSet<String> = payload.iter().map(|m| m.name.clone()).collect();
    assert_eq!(from_bill, from_payload);
    let want = [
        ".",
        "./Library",
        "./Library/LaunchDaemons",
        "./Library/LaunchDaemons/com.komira.kbf-installer.plist",
        "./Library/Logs",
        "./Library/Logs/kbf-installer",
        "./private",
        "./private/var",
        "./private/var/db",
        "./private/var/db/kbf-installer",
        "./usr",
        "./usr/local",
        "./usr/local/kbf-installer",
        "./usr/local/kbf-installer/bin",
        "./usr/local/kbf-installer/bin/kbf-installer",
        "./usr/local/kbf-installer/trust",
        "./usr/local/kbf-installer/trust/release.pub",
        "./usr/local/kbf-installer/trust/trusted_root.jsonl",
    ];
    assert_eq!(
        from_payload.iter().map(String::as_str).collect::<Vec<_>>(),
        want
    );
    // What the files are, in both.
    let file = |name: &str| payload.iter().find(|m| m.name == name).unwrap();
    let bin = file("./usr/local/kbf-installer/bin/kbf-installer");
    assert_eq!((bin.mode, bin.bytes.as_slice()), (0o100755, BINARY_BYTES));
    let job = file("./Library/LaunchDaemons/com.komira.kbf-installer.plist");
    assert_eq!(
        (job.mode, job.bytes.as_slice()),
        (0o100644, plist_text().as_bytes())
    );
    for (name, bytes) in [
        ("release.pub", &b"key\n"[..]),
        ("trusted_root.jsonl", b"{\"root\":1}\n"),
    ] {
        let trust = file(&format!("./usr/local/kbf-installer/trust/{name}"));
        assert_eq!((trust.mode, trust.bytes.as_slice()), (0o100644, bytes));
    }
    // Directories: the state directory is closed to everyone but root, the rest of what
    // the package adds is 0755, so no directory on the way to the tool is writable by
    // group or other.
    for m in payload.iter().filter(|m| m.mode & 0o170000 == 0o040000) {
        let want = if m.name == "./private/var/db/kbf-installer" {
            0o40700
        } else {
            0o40755
        };
        assert_eq!(m.mode, want, "{}", m.name);
        let p = bill.iter().find(|p| full(p) == m.name).unwrap();
        assert_eq!(u32::from(p.mode), want, "{} in the bill", m.name);
    }
    for m in payload.iter().filter(|m| m.mode & 0o170000 == 0o100000) {
        let p = bill.iter().find(|p| full(p) == m.name);
        let p = p.unwrap_or_else(|| panic!("{} is in the bill", m.name));
        assert_eq!(
            (p.size as usize, p.cksum, u32::from(p.mode)),
            (m.bytes.len(), cksum(&m.bytes), m.mode)
        );
    }
}

#[test]
fn the_same_inputs_give_the_same_package_wherever_and_whenever_they_are_built() {
    let a = built("same-a");
    // Another directory, files written later, the trust files in the other order.
    let dir = scratch("same-b");
    std::thread::sleep(std::time::Duration::from_millis(1100));
    let mut f = files(&dir, BINARY_BYTES);
    f.trust.reverse();
    build_with(&f, "1.2.3", &dir.join("deeper").join("out")).unwrap();
    let b = std::fs::read(dir.join("deeper/out").join(pkg_name("1.2.3"))).unwrap();
    assert_eq!(sha1_hex(&a), sha1_hex(&b));
    assert_eq!(a, b);
}

#[test]
fn another_binary_version_or_trust_file_gives_another_package() {
    let a = built("diff-a");
    let dir = scratch("diff-b");
    let f = files(&dir, b"another binary");
    build_with(&f, "1.2.3", &dir.join("out")).unwrap();
    assert_ne!(
        a,
        std::fs::read(dir.join("out").join(pkg_name("1.2.3"))).unwrap()
    );
    let dir = scratch("diff-c");
    let f = files(&dir, BINARY_BYTES);
    build_with(&f, "1.2.4", &dir.join("out")).unwrap();
    assert_ne!(
        a,
        std::fs::read(dir.join("out").join(pkg_name("1.2.4"))).unwrap()
    );
    let dir = scratch("diff-d");
    let f = files(&dir, BINARY_BYTES);
    std::fs::write(&f.trust[1], b"another key\n").unwrap();
    build_with(&f, "1.2.3", &dir.join("out")).unwrap();
    assert_ne!(
        a,
        std::fs::read(dir.join("out").join(pkg_name("1.2.3"))).unwrap()
    );
}

#[test]
fn build_prints_the_package_and_its_sum_and_will_not_replace_one() {
    let dir = scratch("print");
    let f = files(&dir, BINARY_BYTES);
    let out = dir.join("out");
    let lines = build_with(&f, "1.2.3", &out).unwrap();
    let bytes = std::fs::read(out.join("kbf-installer-1.2.3-macos-arm64.pkg")).unwrap();
    assert_eq!(
        lines,
        [
            "packaged kbf-installer-1.2.3-macos-arm64.pkg:".to_owned(),
            format!(
                "{}  kbf-installer-1.2.3-macos-arm64.pkg",
                Sha256::of(&bytes)
            ),
        ]
    );
    let e = err_text(build_with(&f, "1.2.3", &out).unwrap_err());
    assert!(
        e.contains("write") && e.contains("kbf-installer-1.2.3-macos-arm64.pkg"),
        "{e}"
    );
}

#[test]
fn build_refuses_what_it_cannot_package() {
    let version = |v: &str| {
        let dir = scratch("version");
        let f = files(&dir, BINARY_BYTES);
        build_with(&f, v, &dir.join("out")).unwrap_err()
    };
    let long = "1".repeat(65);
    for bad in ["", "-1", "1 2", "1/2", "1\"2", "a#{b}", long.as_str()] {
        assert!(matches!(version(bad), Error::Usage(_)), "{bad:?}");
    }
    let dir = scratch("refuse");
    let f = files(&dir, BINARY_BYTES);
    let out = dir.join("out");
    // An empty binary.
    std::fs::write(&f.binary, b"").unwrap();
    let e = err_text(build_with(&f, "1", &out).unwrap_err());
    assert!(e.ends_with("kbf-installer is empty"), "{e}");
    // A binary that is not there.
    std::fs::remove_file(&f.binary).unwrap();
    assert!(err_text(build_with(&f, "1", &out).unwrap_err()).starts_with("read "));
    std::fs::write(&f.binary, BINARY_BYTES).unwrap();
    // A property list that is not text.
    std::fs::write(&f.plist, [0xff, 0xfe]).unwrap();
    let e = err_text(build_with(&f, "1", &out).unwrap_err());
    assert!(e.ends_with("job.plist is not UTF-8 text"), "{e}");
    // One that starts at load, and says so once per problem.
    std::fs::write(
        &f.plist,
        plist_text().replace(
            "<key>RunAtLoad</key>\n\t<false/>",
            "<key>RunAtLoad</key>\n\t<true/>",
        ),
    )
    .unwrap();
    let Error::Refused(problems) = build_with(&f, "1", &out).unwrap_err() else {
        panic!("a refusal");
    };
    assert_eq!(problems.len(), 1);
    assert!(
        problems[0]
            .contains("job.plist: the property list does not say the job is not started at load"),
        "{problems:?}"
    );
    std::fs::write(&f.plist, plist_text()).unwrap();
    // A trust file that is not there, two with one name, one with no name.
    let mut g = Files {
        binary: f.binary.clone(),
        plist: f.plist.clone(),
        trust: vec![dir.join("missing")],
    };
    assert!(err_text(build_with(&g, "1", &out).unwrap_err()).starts_with("read "));
    std::fs::create_dir_all(dir.join("other")).unwrap();
    std::fs::write(dir.join("other/release.pub"), b"k").unwrap();
    g.trust = vec![f.trust[1].clone(), dir.join("other/release.pub")];
    let e = err_text(build_with(&g, "1", &out).unwrap_err());
    assert!(e.ends_with("release.pub is in the package twice"), "{e}");
    g.trust = vec![dir.join("..")];
    let e = err_text(build_with(&g, "1", &out).unwrap_err());
    assert!(e.contains("has no UTF-8 file name"), "{e}");
    // Nothing of these was written.
    assert!(!out.exists());
}

#[test]
fn a_package_larger_than_one_leaf_is_refused_by_build() {
    let dir = scratch("many");
    let mut f = files(&dir, BINARY_BYTES);
    f.trust = (0..bom::LEAF_PATHS)
        .map(|i| {
            let p = dir.join(format!("t{i}"));
            std::fs::write(&p, b"t").unwrap();
            p
        })
        .collect();
    let e = err_text(build_with(&f, "1", &dir.join("out")).unwrap_err());
    assert!(
        e.contains("paths; a bill of materials of this writer holds 256"),
        "{e}"
    );
}

#[test]
fn versions_and_names() {
    for good in ["1", "0.1.0", "1.2.3+4", "20261018_x-y", "a"] {
        assert!(valid_version(good), "{good}");
    }
    let long = "1".repeat(65);
    for bad in ["", ".1", "-1", "a b", "a/b", "a\"b", "é", long.as_str()] {
        assert!(!valid_version(bad), "{bad:?}");
    }
    assert!(valid_version(&"1".repeat(64)));
    assert_eq!(pkg_name("0.1.0"), "kbf-installer-0.1.0-macos-arm64.pkg");
    // The gate path is the gate directory's file.
    assert_eq!(GATE, "/usr/local/etc/kbf/installer.toml");
    for dir in [PREFIX, BINARY, PLIST, TRUST_DIR, STATE_DIR, LOG_DIR] {
        assert!(!dir.starts_with('/'), "{dir}");
    }
    // The tool and the trust files are in the prefix, which is what the plist runs.
    assert!(BINARY.starts_with(PREFIX) && TRUST_DIR.starts_with(PREFIX));
    assert_eq!(PROGRAM, format!("/{BINARY}"));
}

fn sum() -> Sha256 {
    Sha256::of(b"the package")
}

const URL: &str =
    "https://github.com/komira-ai/kbf/releases/download/v1.2.3/kbf-installer-1.2.3-macos-arm64.pkg";

#[test]
fn the_cask_installs_the_package_and_its_uninstall_unloads_the_job_and_forgets_the_files() {
    let cask = brew::cask("1.2.3", &sum(), URL).unwrap();
    assert!(!cask.contains('@'), "a mark is left: {cask}");
    for line in [
        "cask \"kbf-installer\" do",
        "  version \"1.2.3\"",
        &format!("  sha256 \"{}\"", sum()),
        &format!("  url \"{URL}\""),
        "  depends_on arch: :arm64",
        "  pkg \"kbf-installer-#{version}-macos-arm64.pkg\"",
        "  uninstall launchctl: \"com.komira.kbf-installer\",",
        "            pkgutil:   \"com.komira.kbf-installer\"",
        "    \"/usr/local/kbf-installer\",",
        "    \"/usr/local/etc/kbf/installer.toml\",",
        "    \"/var/db/kbf-installer\",",
        "    \"/Library/Logs/kbf-installer\",",
    ] {
        assert!(cask.lines().any(|l| l == line), "{line}\n{cask}");
    }
    assert!(cask.ends_with("end\n"));
}

#[test]
fn the_cask_refuses_a_version_or_url_that_breaks_out_of_a_ruby_string() {
    for bad in ["1\"; system \"x", "1 2", ""] {
        let p = brew::cask(bad, &sum(), URL).unwrap_err();
        assert_eq!(p.len(), 1, "{p:?}");
        assert!(p[0].starts_with("version "), "{p:?}");
    }
    for bad in [
        "http://example.test/x.pkg",
        "ftp://example.test/x.pkg",
        "https://example.test/x\".pkg",
        "https://example.test/#{x}.pkg",
        "https://example.test/a b.pkg",
        "https://example.test/x\\.pkg",
        "https://example.test/x\n.pkg",
        "",
    ] {
        let p = brew::cask("1", &sum(), bad).unwrap_err();
        assert_eq!(p.len(), 1, "{bad:?}: {p:?}");
        assert!(p[0].starts_with("url "), "{p:?}");
    }
    // Both wrong: both said.
    assert_eq!(brew::cask("", &sum(), "").unwrap_err().len(), 2);
}

#[test]
fn the_cask_verb_hashes_the_file_and_writes_a_new_file() {
    let dir = scratch("brew-verbs");
    let name = pkg_name("1.2.3");
    std::fs::write(dir.join(&name), b"the package").unwrap();
    let lines = cask(&dir.join(&name), "1.2.3", URL, &dir.join("cask.rb")).unwrap();
    assert_eq!(
        lines,
        [format!("wrote the cask {}", dir.join("cask.rb").display())]
    );
    let text = std::fs::read_to_string(dir.join("cask.rb")).unwrap();
    assert!(text.contains(&format!("sha256 \"{}\"", sum())));
    // Not over a file there; not for a package of another name or a bad version.
    assert!(cask(&dir.join(&name), "1.2.3", URL, &dir.join("cask.rb")).is_err());
    std::fs::write(dir.join("other.pkg"), b"the package").unwrap();
    let e = err_text(cask(&dir.join("other.pkg"), "1.2.3", URL, &dir.join("c2.rb")).unwrap_err());
    assert!(
        e.contains("is not named kbf-installer-1.2.3-macos-arm64.pkg"),
        "{e}"
    );
    let e = cask(&dir.join(pkg_name("1 2")), "1 2", URL, &dir.join("c3.rb")).unwrap_err();
    assert!(matches!(e, Error::Spawn { .. }), "{e}");
    std::fs::write(dir.join(pkg_name("1 2")), b"the package").unwrap();
    let e = cask(&dir.join(pkg_name("1 2")), "1 2", URL, &dir.join("c3.rb")).unwrap_err();
    assert!(matches!(e, Error::Usage(_)), "{e}");
    assert!(!dir.join("c3.rb").exists());
}

#[test]
fn the_nodes_of_a_tree_are_files_and_directories() {
    let mut t = Tree::rooted();
    t.file("a", 0o600, b"z".to_vec()).unwrap();
    let nodes: Vec<_> = t.nodes().collect();
    assert_eq!(nodes[0], (".", &Node::Dir { mode: 0o755 }));
    assert_eq!(
        nodes[1],
        (
            "a",
            &Node::File {
                mode: 0o600,
                bytes: b"z".to_vec()
            }
        )
    );
}

#[test]
fn package_info_counts_entries_and_rounds_kilobytes_up() {
    let info = |bytes: usize| {
        let mut t = Tree::rooted();
        t.file("a/f", 0o644, vec![0; bytes]).unwrap();
        package_info(&t, "9")
    };
    for (bytes, kbytes) in [(0, 0), (1, 1), (1010, 1), (1024, 1), (1025, 2), (2040, 2)] {
        let want = format!("<payload numberOfFiles=\"3\" installKBytes=\"{kbytes}\"/>");
        let got = info(bytes);
        assert!(got.contains(&want), "{bytes} bytes: {got}");
    }
}
