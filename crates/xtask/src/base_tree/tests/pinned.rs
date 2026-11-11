//! `base-tree` with the packages and paths the release leaves out ([`REMOVED`], [`EXCLUDED`]),
//! applied to the pinned `linux-x86_64` base as `noble-amd64.tar.gz` holds it.
//!
//! The fixture is the one layer of the pinned base (`base-linux-x86_64.manifest.json`) with every
//! entry as the image has it (path, type, mode, link target; owner 0, time 0) and every file's
//! bytes emptied, except the dpkg database the removal reads: `/var/lib/dpkg/status` (each stanza
//! cut to `Package`, `Status`, `Pre-Depends`, `Depends` and `Provides`, without version
//! constraints, which the removal does not read), `diversions`, `alternatives/*` and
//! `info/*.list`, byte for byte. Rolling the base means making it again from the new layer.

use std::collections::BTreeSet;
use std::io::Read;

use super::super::removed::{EXCLUDED, REMOVED};
use super::super::*;
use super::support::{Seen, bundle_paths, image, layer, listing};
use super::{COMMIT, norm};
use kbf_tree_bundle::Normalised;

/// The fixture, a gzip-compressed tar.
const FIXTURE: &[u8] = include_bytes!("noble-amd64.tar.gz");

/// The fixture's layer, as a plain tar.
fn pinned_layer() -> Vec<u8> {
    let mut tar = Vec::new();
    flate2::read::GzDecoder::new(FIXTURE)
        .read_to_end(&mut tar)
        .expect("the fixture is gzip");
    tar
}

/// The text of each `/var/lib/dpkg/info/*.list` of the fixture, by file name.
fn lists(tar: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for entry in tar::Archive::new(tar).entries().expect("entries") {
        let mut entry = entry.expect("entry");
        let path = entry.path().expect("path").to_string_lossy().into_owned();
        if let Some(name) = path.strip_prefix("var/lib/dpkg/info/")
            && name.ends_with(".list")
        {
            let name = name.to_owned();
            let mut text = String::new();
            entry.read_to_string(&mut text).expect("a list is text");
            out.push((name, text));
        }
    }
    out
}

/// The tree of the plain layer `tar` as a base with nothing left out, its set-uid and set-gid
/// bits taken off first so that it is not refused: the base as it was, to compare with.
fn built_from(tar: &[u8]) -> Tree {
    let mut plain = tar::Builder::new(Vec::new());
    for entry in tar::Archive::new(tar).entries().expect("entries") {
        let entry = entry.expect("entry");
        let mut header = entry.header().clone();
        let mode = header.mode().expect("mode");
        header.set_mode(mode & !0o6000);
        header.set_cksum();
        plain.append(&header, entry).expect("append");
    }
    let plain = plain.into_inner().expect("tar");
    let img = image(&[plain], &[], false);
    match tree_of(&img.bytes, &img.base, &[], &[], b"k".to_vec()) {
        Ok(tree) => tree,
        Err(e) => panic!("the pinned base is refused: {e:?}"),
    }
}

/// The production lists' names, as `tree_of` takes them.
fn production() -> (Vec<&'static str>, Vec<&'static str>) {
    (
        REMOVED.iter().map(|r| r.package).collect(),
        EXCLUDED.iter().map(|e| e.path).collect(),
    )
}

/// Asserts the pinned base with nothing left out is refused for exactly its twelve set-uid and
/// set-gid programs, and that with the release's packages and paths left out it is accepted: the
/// tree has the entry count of the release's own build of the pinned base (2791), its facts list
/// no set-uid or set-gid file (two set-gid directories alone, with the base's other narrowed
/// modes), every file and link of a removed package's list and every control file of one is gone,
/// every file and link of a kept package's list is there as the base had it (so all the farm's
/// actions use: the C library, the shells, coreutils and the rest), `/usr/bin/su` is gone while
/// `flock` and the rest of util-linux stay, and the status file names exactly the kept packages.
/// Catches: a release list that names a package the pinned base does not hold or whose removal
/// breaks a kept package's dependency (the tree is then refused), one that leaves a set-uid
/// program in, one whose removal takes a file the actions need, and a removal that does less
/// than its list says.
#[test]
fn the_release_lists_leave_the_pinned_base_with_no_set_id_file_and_what_actions_use() {
    let tar = pinned_layer();
    let img = image(std::slice::from_ref(&tar), &[], false);

    let set_id = |path: &str, kind: &str, mode: &str| {
        format!(
            "{path}: a {kind} file (mode {mode}); the kbf base tree holds none: leave out the package that brings it"
        )
    };
    let Err(Error::Refused(before)) = tree_of(&img.bytes, &img.base, &[], &[], b"k".to_vec())
    else {
        panic!("the pinned base with nothing left out is not refused");
    };
    let mut expected: Vec<String> = ["chage", "expiry"]
        .iter()
        .map(|p| set_id(&format!("/usr/bin/{p}"), "set-gid", "2755"))
        .chain(
            [
                "chfn", "chsh", "gpasswd", "mount", "newgrp", "passwd", "su", "umount",
            ]
            .iter()
            .map(|p| set_id(&format!("/usr/bin/{p}"), "set-uid", "4755")),
        )
        .chain(
            ["pam_extrausers_chkpwd", "unix_chkpwd"]
                .iter()
                .map(|p| set_id(&format!("/usr/sbin/{p}"), "set-gid", "2755")),
        )
        .collect();
    expected.sort();
    assert_eq!(before, expected);

    let (removed, excluded) = production();
    let tree = match tree_of(&img.bytes, &img.base, &removed, &excluded, b"k".to_vec()) {
        Ok(tree) => tree,
        Err(e) => panic!("the release lists are refused on the pinned base: {e:?}"),
    };
    assert_eq!(tree.built.entries, 2791);
    // The facts list the base's narrowed modes and no set-uid or set-gid file: the only set-id
    // bits left are two directories' set-gid.
    assert_eq!(tree.normalised.len(), 23);
    let set_id: Vec<&Normalised> = tree
        .normalised
        .iter()
        .filter(|n| n.from.starts_with(['2', '4', '6']))
        .collect();
    assert_eq!(
        set_id,
        [
            &norm("/var/local", "2775", "0755"),
            &norm("/var/mail", "2775", "0755"),
        ]
    );
    assert!(
        tree.normalised
            .contains(&norm("/etc/shadow", "0640", "0644"))
    );
    assert!(tree.normalised.contains(&norm("/tmp", "1777", "0755")));

    let seen = listing(&tree);
    let original = listing(&built_from(&tar));
    let gone: BTreeSet<&str> = removed.iter().copied().collect();
    let mut checked = (0, 0);
    for (name, text) in lists(&tar) {
        let package = name
            .trim_end_matches(".list")
            .split(':')
            .next()
            .expect("a name");
        let listed = text.lines().filter(|p| *p != "/.");
        if gone.contains(package) {
            // Every file and link a removed package lists, and its control files, are gone.
            assert!(
                !seen.contains_key(&format!("/var/lib/dpkg/info/{name}")),
                "{name} is left"
            );
            for path in listed {
                if let Some(Seen::File { .. } | Seen::Symlink(_)) = seen.get(path) {
                    panic!("{path} of {package} is left");
                }
                checked.0 += 1;
            }
        } else {
            // Every file and link a kept package lists that the base held is still there, as it
            // was, but the paths left out of kept packages.
            for path in listed.filter(|p| !excluded.contains(p)) {
                if let Some(was @ (Seen::File { .. } | Seen::Symlink(_))) = original.get(path) {
                    assert_eq!(seen.get(path), Some(was), "{path} of {package}");
                    checked.1 += 1;
                }
            }
        }
    }
    assert!(
        checked.0 > 1000 && checked.1 > 1000,
        "only {checked:?} paths looked at"
    );
    assert!(!seen.contains_key("/usr/bin/su"));
    assert!(seen.contains_key("/usr/bin/flock"));

    // The status file names every kept package and no removed one.
    let Some(Seen::File { bytes, .. }) = seen.get("/var/lib/dpkg/status") else {
        panic!("no status file");
    };
    let status = String::from_utf8(bytes.clone()).expect("UTF-8");
    let named: BTreeSet<&str> = status
        .lines()
        .filter_map(|l| l.strip_prefix("Package: "))
        .collect();
    assert!(
        named.is_disjoint(&gone),
        "a removed package is still installed"
    );
    for kept in ["util-linux", "coreutils", "libc6", "dpkg", "debconf"] {
        assert!(named.contains(kept), "{kept} is not in the status file");
    }
    assert_eq!(named.len() + gone.len(), 92);

    // The merged-usr links and `/etc/passwd`, which a kept package does not list, are there.
    for link in ["/bin", "/lib", "/lib64", "/sbin"] {
        assert!(
            matches!(seen.get(link), Some(Seen::Symlink(_))),
            "{link} is not a link"
        );
    }
    for file in ["/etc/passwd", "/etc/group"] {
        assert!(seen.contains_key(file), "{file} is not in the tree");
    }
}

/// Asserts the verb, run with the release's lists on the pinned base, writes a bundle that holds
/// none of the set-uid and set-gid programs nor what their packages listed, and holds the tools the
/// farm's actions use; and that it prints a line for each package and path left out.
/// Catches: a release whose bundle differs from the tree the lists describe (the lists printed but
/// not applied).
#[test]
fn the_verb_with_the_release_lists_writes_a_bundle_without_them() {
    let dir = super::scratch("pinned-verb");
    let img = image(&[pinned_layer()], &[layer(&[])], true);
    std::fs::write(dir.join("image.oci.tar"), &img.bytes).expect("image");
    std::fs::write(dir.join("kbf-task"), b"k").expect("binary");
    let args = Args {
        image: dir.join("image.oci.tar"),
        platform: Platform::LinuxX86_64,
        commit: COMMIT.to_owned(),
        binary: dir.join("kbf-task"),
        out: dir.join("dist"),
    };
    let printed = run_with(&args, &img.base, REMOVED, EXCLUDED).expect("run");
    assert_eq!(printed.len(), 3 + REMOVED.len() + EXCLUDED.len());
    assert_eq!(
        printed[1],
        format!(
            "{}: 2791 entries, depth 8, 23 paths with a mode normalised",
            kbf_tree_bundle::file_name(COMMIT, "linux-x86_64")
        )
    );
    let name = kbf_tree_bundle::file_name(COMMIT, "linux-x86_64");
    let paths = bundle_paths(&std::fs::read(dir.join("dist").join(name)).expect("bundle"));
    for gone in [
        "/usr/bin/su",
        "/usr/bin/passwd",
        "/usr/bin/chfn",
        "/usr/bin/chsh",
        "/usr/bin/gpasswd",
        "/usr/bin/newgrp",
        "/usr/bin/chage",
        "/usr/bin/expiry",
        "/usr/bin/mount",
        "/usr/bin/umount",
        "/usr/sbin/unix_chkpwd",
        "/usr/sbin/pam_extrausers_chkpwd",
        "/usr/bin/apt-get",
        "/usr/sbin/mkfs.ext4",
        "/usr/bin/pidof",
        "/etc/pam.d/login",
    ] {
        assert!(!paths.contains(gone), "{gone} is in the bundle");
    }
    for kept in [
        "/usr/bin/flock",
        "/usr/bin/ls",
        "/usr/bin/tar",
        "/usr/lib/x86_64-linux-gnu/libc.so.6",
        "/.kbf-tools/kbf-task",
    ] {
        assert!(paths.contains(kept), "{kept} is not in the bundle");
    }
}
