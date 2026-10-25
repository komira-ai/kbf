//! The object keys of packs.

use kbf_pack::{PackError, PackKeys, WriterKind};

/// Asserts that a daemon's pack keys are `cas/d/<node>/<boot>/<seq>` and a frontend's are
/// `cas/f/<frontend>/<boot>/<seq>`, with the sequence number in sixteen hex digits, that the
/// self-test key is in the writer's prefix and is no sequence number, and that the writer the
/// index records names the kind and the name.
#[test]
fn keys_follow_the_scheme_of_each_writer_kind() {
    let daemon = PackKeys::new(WriterKind::Daemon, "node1", "boot7").unwrap();
    let first = daemon.key(0);
    let last = daemon.key(255);
    let probe = daemon.self_test_key();
    assert_eq!(first.as_str(), "cas/d/node1/boot7/0000000000000000");
    assert_eq!(last.as_str(), "cas/d/node1/boot7/00000000000000ff");
    assert_eq!(probe.as_str(), "cas/d/node1/boot7/selftest");
    assert_eq!(daemon.writer(), "d/node1");

    let frontend = PackKeys::new(WriterKind::Frontend, "fe-a", "b1").unwrap();
    let key = frontend.key(1);
    assert_eq!(key.as_str(), "cas/f/fe-a/b1/0000000000000001");
    assert_eq!(frontend.writer(), "f/fe-a");
}

/// Asserts that keys sort in write order: the listing of a writer's prefix returns its packs
/// oldest first, even across a change in the number of hex digits a number needs.
#[test]
fn keys_sort_in_sequence_order() {
    let keys = PackKeys::new(WriterKind::Daemon, "n", "b").unwrap();
    let low = keys.key(9).to_string();
    let high = keys.key(10).to_string();
    let higher = keys.key(4096).to_string();
    assert!(low < high);
    assert!(high < higher);
}

/// Asserts that a name or a boot that holds a `/` is refused, because it would put the pack
/// outside its writer's prefix.
#[test]
fn a_name_or_boot_with_a_slash_is_refused() {
    let name = PackKeys::new(WriterKind::Daemon, "a/b", "boot").unwrap_err();
    assert!(matches!(name, PackError::Name(_)), "{name}");
    assert!(name.to_string().contains("\"a/b\""), "{name}");
    let boot = PackKeys::new(WriterKind::Daemon, "node", "x/y").unwrap_err();
    assert!(matches!(boot, PackError::Name(_)), "{boot}");
    assert!(boot.to_string().contains("\"x/y\""), "{boot}");
}

/// Asserts that a name that is no object key (a character a key cannot hold, an empty name) is
/// refused with the key's own error.
#[test]
fn a_name_that_makes_no_object_key_is_refused() {
    let space = PackKeys::new(WriterKind::Daemon, "bad name", "boot").unwrap_err();
    assert!(matches!(space, PackError::Key(_)), "{space}");
    assert!(space.to_string().contains("only A-Z"), "{space}");
    let empty = PackKeys::new(WriterKind::Frontend, "", "boot").unwrap_err();
    assert!(matches!(empty, PackError::Key(_)), "{empty}");
}
