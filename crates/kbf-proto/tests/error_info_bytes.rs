//! `google.rpc.ErrorInfo` encodes to the same bytes every time.
//!
//! Catches: a metadata map held in a hash map, which writes its entries in the order of a
//! seed that differs for each map, so one message gives different bytes on two runs and a
//! stored row that holds it cannot be pinned by a file.

use kbf_proto::google::rpc::ErrorInfo;
use prost::Message;

fn info(order: &[(&str, &str)]) -> ErrorInfo {
    ErrorInfo {
        reason: "R".to_owned(),
        domain: "kbf".to_owned(),
        metadata: order
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect(),
    }
}

#[test]
fn the_metadata_entries_are_written_in_key_order() {
    let keys = ["a", "b", "c", "d", "e", "f", "g", "h"];
    let entries: Vec<(&str, &str)> = keys.iter().map(|k| (*k, "v")).collect();
    let reversed: Vec<(&str, &str)> = entries.iter().rev().copied().collect();
    let first = info(&entries).encode_to_vec();
    for _ in 0..64 {
        assert_eq!(info(&entries).encode_to_vec(), first);
        assert_eq!(info(&reversed).encode_to_vec(), first);
    }
    let mut at = 0;
    for key in keys {
        let found = first[at..]
            .windows(3)
            .position(|w| w == [0x0a, 0x01, key.as_bytes()[0]])
            .expect("the key is written");
        at += found + 3;
    }
}
