use std::time::Duration;

use kbf_proto::reapi::Digest;
use kbf_types::LeaseId;

use super::{PackKey, build_id, put_dir_argv};

fn digest() -> Digest {
    Digest {
        hash: "ab".repeat(32),
        size_bytes: 12,
    }
}

/// Catches a build id the neighbours refuse: the farm prints a lease as `7.3`, and the pack
/// key, the manifest and the ingest take a build of `0-9 a-z A-Z - _` only (a dot is refused,
/// and every action would be rerun and answered INTERNAL). The id is `7-3`, and passes the
/// pack crate's own check, for any lease.
#[test]
fn a_lease_is_a_build_id_the_pack_crate_accepts() {
    assert_eq!(build_id(LeaseId::new(7, 3)), "7-3");
    assert!(LeaseId::new(7, 3).to_string().contains('.'));
    for lease in [LeaseId::new(0, 0), LeaseId::new(u64::MAX, u64::MAX)] {
        assert_eq!(kbf_pack::check_build(&build_id(lease)), Ok(()), "{lease}");
    }
}

/// Catches a put directory that is not the string the mint issued: the executor passes it on to
/// the ingest and the job's mount as it is.
#[test]
fn a_pack_key_is_the_object_key_it_was_issued_as() {
    let key = PackKey::new("cas/b/7-3/ab-0");
    assert_eq!(key.to_string(), "cas/b/7-3/ab-0");
    assert_eq!(key.as_str(), "cas/b/7-3/ab-0");
}

/// Asserts the task tool's command line for a run that commits into a put directory: the tree
/// at `/`, the commit into `/out`, the action, the lease's build id, the deadline and the one-pass
/// object format, and nothing that names a url, a grant or a secret.
#[test]
fn a_put_dir_run_is_told_the_tree_and_out_and_no_secret() {
    let argv = put_dir_argv(&digest(), LeaseId::new(7, 3), Duration::from_secs(90));
    let want = [
        "--tree",
        "--stage-and-commit",
        "--out-dir",
        "/out",
        "--action",
        &format!("{}/12", "ab".repeat(32)),
        "--lease",
        "7-3",
        "--deadline-ms",
        "90000",
        "--object-format",
        "one-pass",
    ];
    assert_eq!(argv, want);
    assert!(
        !argv
            .iter()
            .any(|arg| arg.contains("url") || arg.contains("grant"))
    );
}
