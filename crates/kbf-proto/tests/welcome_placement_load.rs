//! `Welcome.placement_load` (field 6, `optional bool`): the bytes of a Welcome without it, its
//! presence on the wire, and a peer on either side of the field.

use kbf_proto::worker::Welcome;
use prost::Message;

/// A Welcome of a build before the field.
#[derive(Clone, PartialEq, Message)]
struct OldWelcome {
    #[prost(uint32, tag = "1")]
    protocol_version: u32,
    #[prost(uint64, tag = "2")]
    heartbeat_interval_ms: u64,
    #[prost(uint64, tag = "3")]
    epoch: u64,
    #[prost(bool, tag = "5")]
    pack_commit: bool,
}

fn welcome(placement_load: Option<bool>) -> Welcome {
    Welcome {
        protocol_version: 1,
        heartbeat_interval_ms: 1000,
        epoch: 7,
        pack_commit: true,
        placement_load,
        ..Welcome::default()
    }
}

/// Fields 1, 2, 3 and 5 of the Welcome above, as they were encoded before the field.
const BEFORE: [u8; 9] = [0x08, 0x01, 0x10, 0xe8, 0x07, 0x18, 0x07, 0x28, 0x01];

/// Catches an absent field encoded anyway (a server that does not place by measured load would
/// then send other bytes than it always has), `Some(false)` dropped as if absent (the field
/// would not tell absent from false), and a tag other than 6 or a type other than a varint.
#[test]
fn the_field_is_on_the_wire_only_when_set() {
    assert_eq!(welcome(None).encode_to_vec(), BEFORE);
    let mut with = BEFORE.to_vec();
    with.extend([0x30, 0x01]);
    assert_eq!(welcome(Some(true)).encode_to_vec(), with);
    with[10] = 0x00;
    assert_eq!(welcome(Some(false)).encode_to_vec(), with);
    for placement_load in [None, Some(true), Some(false)] {
        let sent = welcome(placement_load);
        assert_eq!(Welcome::decode(&*sent.encode_to_vec()), Ok(sent));
    }
}

/// Catches a Welcome of the old shape no longer decoding (a daemon of this build against an old
/// server reads the field as absent), and the new Welcome breaking an old daemon (it skips the
/// field and reads the rest as it did).
#[test]
fn a_peer_on_either_side_of_the_field_reads_the_rest() {
    let old = OldWelcome {
        protocol_version: 1,
        heartbeat_interval_ms: 1000,
        epoch: 7,
        pack_commit: true,
    };
    assert_eq!(
        Welcome::decode(&*old.encode_to_vec()).expect("decodes"),
        welcome(None)
    );
    let skipped = OldWelcome::decode(&*welcome(Some(true)).encode_to_vec()).expect("decodes");
    assert_eq!(skipped, old);
}
