#![cfg(target_os = "linux")]
//! `kbf storage-and-daemon` hardens its process for the registered cold store's keys
//! (`kbf_server::storage_node::harden`: no core dump, not dumpable) and its daemon half
//! shares that process. A non-dumpable process's `/proc/self` entries are root-owned, so
//! this pins that the daemon half's output removal (`kbf_outputs::remove_tree`, which
//! chmods a read-only directory through `/proc/self/fd`) still works in one. `harden`
//! changes the whole process, so this has a test binary of its own.

use std::os::unix::fs::PermissionsExt as _;

use kbf_server::storage_node::{command, harden, parse};

/// Catches a hardening that leaves the daemon half unable to remove a 0555 directory tree
/// (the module docs): with keys registered, core dumps are off, the process is not
/// dumpable, `/proc/self/fd` lists, and `remove_tree` removes read-only directories, as
/// a user who is not root (root passes whatever `/proc/self/fd` says).
#[test]
fn a_hardened_process_still_removes_read_only_directories() {
    let argv = [
        "storage",
        "--storage-listen=127.0.0.1:0",
        "--cold-store=s3://bkt/kbf/?endpoint=http://127.0.0.1:9",
        "--frontend=http://127.0.0.1:8981",
    ];
    let flags = parse(&command().try_get_matches_from(argv).expect("flags")).expect("taken");
    harden(&flags).expect("hardened");

    let limit = rustix::process::getrlimit(rustix::process::Resource::Core);
    assert_eq!((limit.current, limit.maximum), (Some(0), Some(0)));
    assert_eq!(
        rustix::process::dumpable_behavior().expect("dumpable"),
        rustix::process::DumpableBehavior::NotDumpable
    );
    assert!(std::fs::read_dir("/proc/self/fd").is_ok());

    let scratch = std::env::current_exe()
        .expect("test binary")
        .parent()
        .expect("deps directory")
        .join(format!("hardened-removal-{}", std::process::id()));
    let _ = kbf_outputs::remove_tree(&scratch);
    let tree = scratch.join("module-cache");
    let inner = tree.join("inner");
    std::fs::create_dir_all(&inner).expect("tree");
    std::fs::write(inner.join("f"), b"x").expect("file");
    std::fs::set_permissions(&inner, std::fs::Permissions::from_mode(0o555)).expect("mode");
    std::fs::set_permissions(&tree, std::fs::Permissions::from_mode(0o555)).expect("mode");
    kbf_outputs::remove_tree(&tree).expect("removed");
    assert!(!tree.exists());
    kbf_outputs::remove_tree(&scratch).expect("scratch removed");
}
