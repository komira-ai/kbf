//! What a container node needs on disk: the stand-in `crun` and the image it names, and
//! where a test that runs a container daemon may put its directories.

use std::os::unix::fs::PermissionsExt as _;

use super::*;

#[test]
fn the_stand_in_crun_is_set_up_with_its_id_tools_and_the_image_unpacked() {
    // Catches: a node whose start-up finds no `crun`, no setuid id tools or no unpacked image
    // (it would refuse to start, or pull an image no registry serves). Mutant: `crun` makes
    // the id tools mode 0755, or unpacks the image under another digest.
    let root = crate::scratch("node-support-crun");
    let (bin, data) = (root.join("bin"), root.join("data"));
    std::fs::create_dir_all(&bin).expect("bin");
    let image = crun(&bin, &data, "/the/stand-in", &["write-lines", "out", "a"]);
    let hex = kbf_transfer::digest_of(MANIFEST).hash;
    assert_eq!(
        image,
        format!("docker://registry.test/tools/busybox@sha256:{hex}")
    );
    assert!(
        data.join("leases/oci-images")
            .join(&hex)
            .join("rootfs/kbf/root")
            .is_dir()
    );
    assert_eq!(
        std::fs::read_link(bin.join("crun")).expect("a link"),
        PathBuf::from("/the/stand-in")
    );
    for tool in ["newuidmap", "newgidmap"] {
        let mode = std::fs::metadata(bin.join(tool))
            .expect("tool")
            .permissions()
            .mode();
        assert_eq!(mode & 0o7777, 0o4755, "{tool}");
    }
    let steps = std::fs::read_to_string(bin.join("crun-state/action")).expect("steps");
    assert_eq!(steps, "write-lines\nout\na\n");
    assert_eq!(
        oci_runtime(&bin),
        format!("--oci-runtime={}/crun", bin.display())
    );
}

#[test]
fn a_directory_closed_to_others_is_not_where_a_container_daemon_works() {
    // Catches: a container node started under a home directory the driver refuses (its
    // containers' ids cannot search down to the data directory), and a scratch root that
    // ignores a directory that is open. Mutant: `open_to_others` always true, or always false.
    let root = crate::scratch("node-support-root");
    let open = root.join("open");
    std::fs::create_dir_all(&open).expect("dir");
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o755)).expect("mode");
    let closed = root.join("closed");
    let below = closed.join("target");
    std::fs::create_dir_all(&below).expect("dirs");
    std::fs::set_permissions(&closed, std::fs::Permissions::from_mode(0o700)).expect("mode");
    // Whether the directories above the scratch root are open is the box's; the closed one
    // below it is not.
    assert!(!open_to_others(&below));
    assert_eq!(scratch_root(below), std::env::temp_dir());
    assert!(!open_to_others(Path::new("/nonexistent/dir")));
    if open_to_others(&root) {
        assert_eq!(scratch_root(open.clone()), open);
    }
}

#[test]
fn files_holding_finds_each_inode_that_holds_the_bytes_whole() {
    // Catches: a hot copy missed (the test then proves a blob is not stored twice when it
    // is), one inode counted once per name (a hard link is no second copy), a file that holds
    // only the start of the bytes (or all of it but the end) taken for a copy, and a path that is not there ending the
    // search. Mutants: the missing path `unwrap`ped; `any` on the anchor alone.
    let d = crate::scratch("files-holding");
    let bytes: Vec<u8> = (0..200u8).collect();
    let (dir, other) = (d.join("a/b"), d.join("c"));
    std::fs::create_dir_all(&dir).expect("dirs");
    std::fs::create_dir_all(&other).expect("dirs");
    std::fs::write(dir.join("whole"), [b"x".as_slice(), &bytes, b"y"].concat()).expect("write");
    std::fs::hard_link(dir.join("whole"), other.join("link")).expect("link");
    std::fs::write(other.join("start"), &bytes[..100]).expect("write");
    std::fs::write(other.join("short"), b"short").expect("write");
    // Long enough, with the anchor (the first 64 bytes) but not the rest: not a copy.
    let torn = [&bytes[..64], &[0u8; 200][..]].concat();
    std::fs::write(other.join("torn"), torn).expect("write");
    std::fs::write(other.join("zeros"), [0u8; 300]).expect("write");
    let found = files_holding(&[d.clone(), d.join("not-there")], &bytes);
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0].1, 202);
    assert!(files_holding(&[d.join("not-there")], &bytes).is_empty());
}
