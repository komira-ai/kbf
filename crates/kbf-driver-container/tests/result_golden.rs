//! The `ActionResult` and blobs `collect` makes of a mixed tree, pinned to what the walk
//! made before files were hashed on a thread pool and sent from where they are.

mod support;

use std::os::unix::fs::PermissionsExt as _;

use kbf_daemon::cas::digest_of;
use kbf_daemon::cpu_pool::CpuPool;
use kbf_driver_container::MemoryCas;
use kbf_driver_container::tree::{OutputLimits, collect};
use kbf_proto::reapi::{ActionResult, Digest, Tree};
use prost::Message as _;

fn patterned(len: usize) -> Vec<u8> {
    (0..len).map(|i| ((i * 31 + 7) % 251) as u8).collect()
}

/// The tree: a file of every kind the walk meets, two of them identical, a file of three
/// chunks, an empty file and an empty directory, a non-ASCII name, a symlink inside and
/// one that is an output of its own, and two single-file outputs.
fn plant(upper: &std::path::Path) {
    let out = upper.join("out");
    std::fs::create_dir_all(out.join("empty")).expect("mkdir");
    std::fs::create_dir_all(out.join("sub/deep")).expect("mkdir");
    std::fs::write(out.join("a.txt"), b"alpha\n").expect("write");
    std::fs::write(out.join("b.sh"), b"#!/bin/sh\necho b\n").expect("write");
    std::fs::set_permissions(out.join("b.sh"), std::fs::Permissions::from_mode(0o755))
        .expect("chmod");
    std::fs::write(out.join("sub/c.txt"), b"alpha\n").expect("write");
    std::fs::write(out.join("sub/deep/d.bin"), patterned(3 << 20)).expect("write");
    std::fs::write(out.join("sub/deep/e.bin"), patterned(1 << 20)).expect("write");
    std::fs::write(out.join("sub/deep/zero"), b"").expect("write");
    std::os::unix::fs::symlink("../a.txt", out.join("sub/link")).expect("symlink");
    std::fs::write(out.join("z \u{e9}.txt"), "unicode\n").expect("write");
    std::fs::write(upper.join("lone.txt"), b"lone\n").expect("write");
    std::fs::write(upper.join("lone_exec"), b"#!/bin/sh\n").expect("write");
    std::fs::set_permissions(
        upper.join("lone_exec"),
        std::fs::Permissions::from_mode(0o700),
    )
    .expect("chmod");
    std::os::unix::fs::symlink("out/a.txt", upper.join("top-link")).expect("symlink");
}

fn digest(hash: &str, size_bytes: i64) -> Option<Digest> {
    Some(Digest {
        hash: hash.to_owned(),
        size_bytes,
    })
}

/// Catches any change in what the walk records: the digests below were made by the walk
/// at the commit before this one (one blocking task per file, a spool copy), from this
/// same tree. The result, with 1 thread, 4 and 16, is those digests; every Directory the
/// Tree names, every file, and the Tree itself are in the CAS with the bytes on disk. A
/// Directory built with a file's digest or executable bit unset, nodes out of order, or
/// a child's digest taken before the child's own files were filled in would change the
/// root and Tree digests (mutants seen red: the executable bit not copied to the node;
/// the children's digests filled in pre-order instead of last first).
#[tokio::test]
async fn a_mixed_tree_collects_to_what_it_always_did() {
    let upper = support::scratch("result-golden");
    plant(&upper);
    let paths: Vec<String> = ["out", "lone.txt", "lone_exec", "top-link", "missing"]
        .map(str::to_owned)
        .to_vec();
    let mut first: Option<ActionResult> = None;
    for threads in [1, 4, 16] {
        let cas = MemoryCas::new();
        let mut result = ActionResult::default();
        collect(
            &cas,
            &upper,
            &CpuPool::new(threads),
            "",
            &paths,
            OutputLimits::DEFAULT,
            &mut result,
        )
        .await
        .expect("collect");
        let files: Vec<_> = result
            .output_files
            .iter()
            .map(|f| (f.path.as_str(), f.digest.clone(), f.is_executable))
            .collect();
        assert_eq!(
            files,
            [
                (
                    "lone.txt",
                    digest(
                        "f39288032037dc6a3da03c1dc58951b25c24c9436047a1ae51e672c8f7a51dc5",
                        5
                    ),
                    false
                ),
                (
                    "lone_exec",
                    digest(
                        "a8076d3d28d21e02012b20eaf7dbf75409a6277134439025f282e368e3305abf",
                        10
                    ),
                    true
                ),
            ],
            "{threads} threads"
        );
        assert_eq!(result.output_symlinks.len(), 1);
        assert_eq!(result.output_symlinks[0].path, "top-link");
        assert_eq!(result.output_symlinks[0].target, "out/a.txt");
        assert_eq!(result.output_directories.len(), 1);
        let dir = &result.output_directories[0];
        assert_eq!(
            dir.tree_digest,
            digest(
                "87d27c9e101813bc224585ea4e124cc1f4ca0a68ee9067c04f14c3a60e2be366",
                822
            ),
            "{threads} threads"
        );
        assert_eq!(
            dir.root_directory_digest,
            digest(
                "dbe3c45862bfccea2fb064c3b8fb6b32c49c6fb001ed2c420abc0b7bd4c3983a",
                396
            ),
            "{threads} threads"
        );
        // Everything the result names is stored, byte for byte.
        let tree = Tree::decode(
            cas.blob(dir.tree_digest.as_ref().expect("tree"))
                .expect("the Tree is stored")
                .as_slice(),
        )
        .expect("a Tree");
        let root = tree.root.expect("root");
        assert_eq!(
            cas.blob(dir.root_directory_digest.as_ref().expect("root digest")),
            Some(root.encode_to_vec())
        );
        for child in &tree.children {
            assert!(cas.blob(&digest_of(&child.encode_to_vec())).is_some());
        }
        let stored =
            |name: &str| cas.blob(&digest_of(&std::fs::read(upper.join(name)).expect("read")));
        assert!(stored("out/a.txt").is_some() && stored("out/sub/deep/d.bin").is_some());
        assert!(stored("out/sub/deep/zero").is_some() && stored("lone_exec").is_some());
        match &first {
            None => first = Some(result),
            Some(first) => assert_eq!(&result, first, "{threads} threads"),
        }
    }
}
