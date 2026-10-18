//! What a container node needs on disk: the stand-in `crun` and the image it names, and
//! where a test that runs a container daemon may put its directories.

use std::os::unix::fs::PermissionsExt as _;

use super::*;

#[test]
fn the_stand_in_crun_is_set_up_with_its_id_tools_a_failing_podman_and_the_image_unpacked() {
    // Catches: a node whose start-up finds no `crun`, no setuid id tools or no unpacked image
    // (it would refuse to start, or pull an image no registry serves), and a `podman` that is
    // not the recorder (kbf starting it would go unseen). Mutant: `crun` makes the id tools
    // mode 0755, or unpacks the image under another digest.
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
    for link in ["crun", "podman"] {
        assert_eq!(
            std::fs::read_link(bin.join(link)).expect("a link"),
            PathBuf::from("/the/stand-in")
        );
    }
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
    assert_eq!(podman_ran(&bin), "", "nothing ran");
    std::fs::write(bin.join("podman-ran"), "podman pull\n").expect("record");
    assert_eq!(podman_ran(&bin), "podman pull\n");
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
