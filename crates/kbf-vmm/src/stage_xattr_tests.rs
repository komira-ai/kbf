//! Extended attributes through `stage` on a real macOS volume (the temporary directory is
//! on one). They run on a Mac only: the attribute calls are macOS's, and the pure parts
//! (the list, the skipped name, the bound) are tested everywhere in `xattr`.

use std::ffi::CString;
use std::path::Path;

use super::*;
use crate::xattr::QUARANTINE;

const KEPT: &str = "com.kbf.test.kept";
const BINARY: &[u8] = b"\x00bin\xff\x01value\x00";
const DOWNLOADED: &[u8] = b"0083;66ffffff;Safari;";

fn c(s: &str) -> CString {
    CString::new(s).expect("no NUL")
}

/// Sets an attribute on `path` itself (a link is not followed).
fn setx(path: &Path, name: &str, value: &[u8]) {
    let (p, n) = (c(&path.display().to_string()), c(name));
    // SAFETY: NUL-terminated strings and a readable value of the stated length.
    let rc = unsafe {
        libc::setxattr(
            p.as_ptr(),
            n.as_ptr(),
            value.as_ptr().cast(),
            value.len(),
            0,
            libc::XATTR_NOFOLLOW,
        )
    };
    assert_eq!(rc, 0, "set {name} on {}", path.display());
}

/// The names of the attributes of `path` itself.
fn names(path: &Path) -> Vec<String> {
    let p = c(&path.display().to_string());
    let mut buf = vec![0u8; 1 << 16];
    // SAFETY: `buf` is writable for its length; `p` is NUL-terminated.
    let n = unsafe {
        libc::listxattr(
            p.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            libc::XATTR_NOFOLLOW,
        )
    };
    let n = usize::try_from(n).expect("list");
    buf[..n]
        .split(|b| *b == 0)
        .filter(|s| !s.is_empty())
        .map(|s| String::from_utf8_lossy(s).into_owned())
        .collect()
}

/// The value of `name` on `path` itself, if it has one.
fn getx(path: &Path, name: &str) -> Option<Vec<u8>> {
    let (p, n) = (c(&path.display().to_string()), c(name));
    let mut buf = vec![0u8; 1 << 16];
    // SAFETY: as in `names`.
    let got = unsafe {
        libc::getxattr(
            p.as_ptr(),
            n.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
            0,
            libc::XATTR_NOFOLLOW,
        )
    };
    usize::try_from(got).ok().map(|n| buf[..n].to_vec())
}

/// Marks a node with an attribute to keep and the quarantine mark.
fn mark(path: &Path) {
    setx(path, KEPT, BINARY);
    setx(path, QUARANTINE, DOWNLOADED);
}

/// The node keeps the attribute and has no quarantine mark.
fn assert_kept_not_quarantined(path: &Path) {
    assert_eq!(
        getx(path, KEPT).as_deref(),
        Some(BINARY),
        "{}",
        path.display()
    );
    assert_eq!(getx(path, QUARANTINE), None, "{}", path.display());
    assert!(!names(path).iter().any(|n| n == QUARANTINE));
}

/// Catches a file copy that drops the attributes (a signature stored in them is lost),
/// changes a value (binary bytes, a NUL, a long value), or copies the quarantine mark.
#[test]
fn a_copied_file_keeps_its_attributes_but_not_the_quarantine_mark() {
    let dir = tmp("stage-xattr-file");
    let src = dir.join("src");
    write(&src, "bytes", 0o644);
    mark(&src);
    let long = vec![0xA5u8; 4000];
    setx(&src, "com.kbf.test.long", &long);
    let dst = dir.join("dst");
    copy_file(&src, &dst, 0o644, 5).expect("copy");
    assert_kept_not_quarantined(&dst);
    assert_eq!(getx(&dst, "com.kbf.test.long"), Some(long));
    assert_eq!(std::fs::read_to_string(&dst).expect("text"), "bytes");
}

/// Catches a file-by-file stage that leaves out the attributes of a file, a directory
/// (the app's own and a nested one) or a link, or copies the quarantine mark of any.
#[test]
fn the_file_by_file_stage_keeps_the_attributes_of_every_node_but_the_quarantine_mark() {
    let dir = tmp("stage-xattr-copy");
    let app = fake_xcode(&dir);
    let volume = volume_in(&dir);
    let nodes = [
        "",
        "Contents/Developer",
        "Contents/Info.plist",
        "Contents/Developer/Info.plist",
    ];
    nodes.iter().for_each(|n| mark(&app.join(n)));
    let plan = plan(Some(&app), &[], false).expect("plan");
    let h = Harness::new(100 * GIB);
    let out = h.run(&volume, &plan).expect("execute");
    assert_eq!(out["xcode"]["method"], "copy");
    let staged = volume.join("Applications/Xcode.app");
    nodes
        .iter()
        .for_each(|n| assert_kept_not_quarantined(&staged.join(n)));
}

/// Catches a clone that keeps the quarantine mark (on the app, a directory or a file) or
/// loses the other attributes.
#[test]
fn a_cloned_tree_has_the_quarantine_mark_removed_and_the_rest_kept() {
    let dir = tmp("stage-xattr-clone");
    let app = fake_xcode(&dir);
    let volume = volume_in(&dir);
    let plan = plan(Some(&app), &[], false).expect("plan");
    let mut h = Harness::new(100 * GIB);
    h.clone_ok = true;
    h.after_clone = Some(|clone| {
        ["", "Contents", "Contents/Info.plist"]
            .iter()
            .for_each(|n| mark(&clone.join(n)));
    });
    let out = h.run(&volume, &plan).expect("execute");
    assert_eq!(out["xcode"]["method"], "clone");
    let staged = volume.join("Applications/Xcode.app");
    ["", "Contents", "Contents/Info.plist"]
        .iter()
        .for_each(|n| assert_kept_not_quarantined(&staged.join(n)));
}

/// Catches an attribute above the bound being skipped or truncated: the copy must fail
/// naming the attribute, and a value within the bound still copies.
#[test]
fn an_attribute_above_the_bound_is_refused_by_name() {
    let dir = tmp("stage-xattr-bound");
    let (src, dst) = (dir.join("a"), dir.join("b"));
    std::fs::create_dir(&src).expect("a");
    std::fs::create_dir(&dst).expect("b");
    setx(&src, "com.kbf.test.big", &[7u8; 100]);
    let e = crate::xattr::copy_nofollow_with(&src, &dst, 99).expect_err("too big");
    assert!(
        e.contains("com.kbf.test.big") && e.contains("100 bytes"),
        "{e}"
    );
    assert_eq!(getx(&dst, "com.kbf.test.big"), None);
    crate::xattr::copy_nofollow_with(&src, &dst, 100).expect("at the bound");
    assert_eq!(getx(&dst, "com.kbf.test.big"), Some(vec![7u8; 100]));
}
