//! `collect_closure_keeping`: the closure `collect_closure` gives, with every Directory of the
//! input root kept once.

use kbf_front::prepare_run::{ClosureError, collect_closure};

use super::*;

/// Catches: a kept closure that differs from the closure of the one walk, keeps a Directory
/// twice or misses one (a nested directory two parents name, the empty Directory), or names
/// another input root.
#[tokio::test]
async fn the_kept_closure_is_the_closure_with_every_directory_once() {
    let rig = Rig::new().await;
    let blob = rig.farm.packed("shared", "cas/f/w/p0", 0).await;
    let empty = rig.store(&Directory::default()).await;
    let deep = rig
        .store(&directory(
            vec![file("f", &blob, false)],
            vec![subdir("e", &empty)],
            vec![],
        ))
        .await;
    let one = rig
        .store(&directory(vec![], vec![subdir("d", &deep)], vec![]))
        .await;
    let root = rig
        .store(&directory(
            vec![file("f", &blob, false)],
            vec![subdir("one", &one), subdir("deep", &deep)],
            vec![],
        ))
        .await;
    let action = rig.action(&root).await;
    let kept = collect_closure_keeping(&rig.farm.backend, &action, ClosureLimits::DEFAULT)
        .await
        .expect("a closure");
    let plain = collect_closure(&rig.farm.backend, &action, ClosureLimits::DEFAULT)
        .await
        .expect("a closure");
    assert_eq!(kept.closure(), &plain);
    assert_eq!(kept.root(), &root);
    assert_eq!(
        kept.directory_count(),
        4,
        "root, one, deep and the empty Directory"
    );
}

/// Catches: a Directory that names a malformed digest kept rather than refused: keeping the
/// Directories refuses what the closure refuses, naming the Directory.
#[tokio::test]
async fn a_malformed_digest_is_refused_when_kept_as_when_not() {
    let rig = Rig::new().await;
    let bad = FileNode {
        name: "f".to_owned(),
        digest: Some(reapi::Digest {
            hash: "not hex".to_owned(),
            size_bytes: 1,
        }),
        ..FileNode::default()
    };
    let bad_dir = DirectoryNode {
        name: "d".to_owned(),
        digest: None,
    };
    for message in [
        directory(vec![bad], vec![], vec![]),
        directory(vec![], vec![bad_dir], vec![]),
    ] {
        let root = rig.store(&message).await;
        let action = rig.action(&root).await;
        let kept =
            collect_closure_keeping(&rig.farm.backend, &action, ClosureLimits::DEFAULT).await;
        let plain = collect_closure(&rig.farm.backend, &action, ClosureLimits::DEFAULT).await;
        assert!(
            matches!(&kept, Err(ClosureError::Invalid { digest, .. }) if *digest == root),
            "{kept:?}"
        );
        assert_eq!(kept.map(|k| k.closure().clone()), plain);
    }
}

/// Catches: keeping the Directories changing which of two faults of one Directory is named:
/// its files pass the digest limit before its malformed subdirectory is reached, so both walks
/// say the tree is over the limit.
#[tokio::test]
async fn keeping_names_the_same_fault_first() {
    let rig = Rig::new().await;
    let a = rig.farm.packed("a", "cas/f/w/p0", 0).await;
    let b = rig.farm.packed("b", "cas/f/w/p0", 1).await;
    let bad_dir = DirectoryNode {
        name: "d".to_owned(),
        digest: None,
    };
    let root = rig
        .store(&directory(
            vec![file("a", &a, false), file("b", &b, false)],
            vec![bad_dir],
            vec![],
        ))
        .await;
    let action = rig.action(&root).await;
    let limits = ClosureLimits {
        max_digests: 2,
        max_bytes: ClosureLimits::DEFAULT.max_bytes,
    };
    let kept = collect_closure_keeping(&rig.farm.backend, &action, limits).await;
    let plain = collect_closure(&rig.farm.backend, &action, limits).await;
    assert_eq!(plain, Err(ClosureError::TooManyDigests { limit: 2 }));
    assert_eq!(kept.map(|k| k.closure().clone()), plain);
}
