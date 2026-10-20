#![cfg(target_os = "linux")]
//! A node that may hold the registered cold store's keys turns off core dumps before it
//! starts, and one that cannot hold them does not touch the process. `harden` changes the
//! whole process, so this has a test binary of its own.

use kbf_server::storage_node::{StorageFlags, command, harden, parse};

fn flags(more: &[&str]) -> StorageFlags {
    let mut argv = vec!["storage", "--storage-listen=127.0.0.1:0"];
    argv.extend_from_slice(more);
    parse(&command().try_get_matches_from(argv).expect("flags")).expect("taken")
}

/// Catches a node that registers for an S3 store and keeps core dumps on (mutant:
/// `may_hold_keys` false, or `harden` doing nothing), and a node that cannot hold keys
/// being hardened for nothing (mutant: `may_hold_keys` always true): the process is
/// dumpable until a node that may hold keys is hardened, and not after.
#[test]
fn only_a_node_that_may_hold_keys_turns_off_core_dumps() {
    let s3 = "--cold-store=s3://bkt/kbf/?endpoint=http://127.0.0.1:9";
    let file = "--cold-store=file:///var/lib/kbf-cold";
    let frontend = "--frontend=http://127.0.0.1:8981";
    assert!(flags(&[s3, frontend]).may_hold_keys());
    for cannot in [vec![s3], vec![file, frontend], vec![file]] {
        assert!(!flags(&cannot).may_hold_keys(), "{cannot:?}");
    }

    let dumpable = || rustix::process::dumpable_behavior().expect("dumpable");
    assert_eq!(dumpable(), rustix::process::DumpableBehavior::Dumpable);
    harden(&flags(&[s3])).expect("nothing to do");
    harden(&flags(&[file, frontend])).expect("nothing to do");
    assert_eq!(
        dumpable(),
        rustix::process::DumpableBehavior::Dumpable,
        "a node that cannot hold keys is left alone"
    );

    harden(&flags(&[s3, frontend])).expect("hardened");
    assert_eq!(dumpable(), rustix::process::DumpableBehavior::NotDumpable);
    let limit = rustix::process::getrlimit(rustix::process::Resource::Core);
    assert_eq!((limit.current, limit.maximum), (Some(0), Some(0)));
}
