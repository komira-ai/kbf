//! The host and place rows of the `update/*` family that an older build left in a state store
//! (`kbf_server::stored_update`): a store that holds them opens and its rows read, on the
//! in-memory fake and on a SQLite file that is closed and opened again.
//!
//! The bytes below are the frozen bytes the builds that wrote these rows produced, worked out
//! by hand from the protobuf wire format (a key is `tag << 3 | wire type`, a varint is seven
//! bits at a time with the high bit saying more follow, a zero is not written). Nothing here
//! writes a row through the crate: nothing in it can.
//!
//! Each test names the defect it would catch.

#[path = "update_support/mod.rs"]
mod support;

use kbf_server::stored_update::{Holder, Host, ReadError, Row, Slot, decode, read_all};
use kbf_state::{Cond, Store, Txn};
use support::harness::{Backing, Cell};

const BACKINGS: [Backing; 2] = [Backing::Fake, Backing::Sqlite];

/// A host held for a repair wait (phase 13, reason 7), order number 9 of epoch 2, generation
/// 3, entered at 5000 ms, boot counter 10 at the mark, driving to the commit "v2".
const HELD: &[u8] = &[
    0x08, 0x0d, 0x10, 0x09, 0x18, 0x03, 0x20, 0x88, 0x27, 0x28, 0x0a, 0x30, 0x07, 0x38, 0x02, 0x42,
    0x02, 0x76, 0x32,
];

/// The same row with its source, staged (`48 01`).
const HELD_STAGED: &[u8] = &[
    0x08, 0x0d, 0x10, 0x09, 0x18, 0x03, 0x20, 0x88, 0x27, 0x28, 0x0a, 0x30, 0x07, 0x38, 0x02, 0x42,
    0x02, 0x76, 0x32, 0x48, 0x01,
];

/// A host nothing was done to: only its phase, `Pending` (1).
const PENDING: &[u8] = &[0x08, 0x01];

/// A storage holder `s1` (order 5 of epoch 1) and two others, `d1` (6) and `m1` (7).
const SLOT: &[u8] = &[
    0x0a, 0x08, 0x0a, 0x02, 0x73, 0x31, 0x10, 0x05, 0x18, 0x01, 0x12, 0x08, 0x0a, 0x02, 0x64, 0x31,
    0x10, 0x06, 0x18, 0x01, 0x12, 0x08, 0x0a, 0x02, 0x6d, 0x31, 0x10, 0x07, 0x18, 0x01,
];

fn put(store: &dyn Store, key: &str, value: &[u8]) {
    store
        .txn(Txn::new().put(key, value.to_vec(), Cond::Any))
        .expect("a write")
        .expect("no condition");
}

fn host(bytes: &[u8]) -> Host {
    match decode("update/host/h", bytes) {
        Ok(Some(Row::Host { id, row })) => {
            assert_eq!(id, "h");
            row
        }
        other => panic!("not a host row: {other:?}"),
    }
}

fn refused(key: &str, bytes: &[u8]) -> bool {
    matches!(decode(key, bytes), Err(ReadError::Corrupt { key: k, .. }) if k == key)
}

/// Catches: a host row an older build wrote read as anything but what it holds: the phase,
/// the order and its epoch, the generation, the times, the commit and the source, with the
/// fields the row leaves out at zero, absent, or a fetch.
#[test]
fn an_old_host_row_reads_back_as_written() {
    let held = host(HELD);
    assert_eq!(
        (held.phase, held.reason, held.order_id, held.order_epoch),
        (13, 7, 9, 2)
    );
    assert_eq!((held.generation, held.since_ms), (3, 5000));
    assert_eq!((held.boot_before, held.commit.as_str()), (Some(10), "v2"));
    assert_eq!(held.source, 0);
    assert_eq!(host(HELD_STAGED).source, 1);
    let pending = host(PENDING);
    assert_eq!((pending.phase, pending.reason, pending.source), (1, 0, 0));
    assert_eq!(pending.boot_before, None);
    assert_eq!(pending.commit, "");
    // A source said outright as 0 is a fetch too.
    assert_eq!(host(&[0x08, 0x02, 0x48, 0x00]).source, 0);
}

/// Catches: the slot an older build wrote read with the storage place and the other places
/// mixed up, or a holder's order and epoch swapped.
#[test]
fn an_old_slot_row_reads_back_as_written() {
    let Ok(Some(Row::Slot(slot))) = decode("update/slot", SLOT) else {
        panic!("not a slot row");
    };
    let holder = |host: &str, order_id| Holder {
        host: host.to_owned(),
        order_id,
        epoch: 1,
    };
    assert_eq!(slot.storage, Some(holder("s1", 5)));
    assert_eq!(slot.hosts, [holder("d1", 6), holder("m1", 7)]);
    let held: Vec<&str> = slot.holders().map(|h| h.host.as_str()).collect();
    assert_eq!(held, ["s1", "d1", "m1"]);
    let empty = Slot::default();
    assert_eq!(empty.holders().count(), 0);
}

/// Catches: a phase or a hold reason numbered differently from the build that wrote the row:
/// phases 1 to 12 and 14 are read with no reason, 13 (`Held`) with each of the reasons 1 to
/// 12, and each reads back under its own number.
#[test]
fn every_phase_and_hold_reason_an_older_build_wrote_is_read() {
    for phase in (1..=12u8).chain([14]) {
        let row = host(&[0x08, phase]);
        assert_eq!((row.phase, row.reason), (u32::from(phase), 0));
    }
    for reason in 1..=12u8 {
        let row = host(&[0x08, 0x0d, 0x30, reason]);
        assert_eq!((row.phase, row.reason), (13, u32::from(reason)));
    }
}

/// Catches: a reader that guesses. A number this build does not know, a zero (a field that
/// is not there), a hold reason on a phase that has none, a `Held` with no reason, a source
/// above staged (2) and bytes that are no message are each refused with the row's name,
/// never read as the nearest phase or as a fetch.
#[test]
fn a_host_row_this_build_cannot_read_is_refused_not_guessed() {
    let bad: [(&str, &[u8]); 8] = [
        ("no phase", &[0x10, 0x00]),
        ("a phase from the future", &[0x08, 0x63]),
        ("the phase after the last", &[0x08, 0x0f]),
        ("a reason on a step", &[0x08, 0x03, 0x30, 0x01]),
        ("held with no reason", &[0x08, 0x0d]),
        (
            "held for a reason from the future",
            &[0x08, 0x0d, 0x30, 0x63],
        ),
        ("an unknown source", &[0x08, 0x03, 0x48, 0x02]),
        ("not a message", &[0xff, 0xff, 0xff]),
    ];
    for (what, bytes) in bad {
        assert!(refused("update/host/h", bytes), "{what}: the row was read");
    }
}

/// Catches: a slot with one host in two places, a holder with no name, or bytes that are no
/// message being read, which would let two updates believe they each held a place.
#[test]
fn a_slot_row_this_build_cannot_read_is_refused_not_guessed() {
    let twice = [
        0x0a, 0x04, 0x0a, 0x02, 0x73, 0x31, 0x12, 0x04, 0x0a, 0x02, 0x73, 0x31,
    ];
    assert!(refused("update/slot", &twice), "one host in two places");
    assert!(refused("update/slot", &[0x0a, 0x02, 0x10, 0x01]), "no name");
    assert!(refused("update/slot", &[0xff]), "not a message");
}

/// Catches: a reader that cannot take a row a later release wrote with one more field
/// (state-store rows are frozen across one step): the field is skipped and the rest read as
/// before.
#[test]
fn a_field_added_later_is_skipped() {
    let mut bytes = HELD.to_vec();
    // Field 15, a varint, and field 16, a string.
    bytes.extend_from_slice(&[0x78, 0x2a, 0x82, 0x01, 0x02, 0x68, 0x69]);
    assert_eq!(host(&bytes), host(HELD));
    let mut slot = SLOT.to_vec();
    slot.extend_from_slice(&[0x78, 0x2a]);
    assert_eq!(decode("update/slot", &slot), decode("update/slot", SLOT));
}

/// Catches: a key that is not a host or slot row being decoded as one or refused: the rows
/// the live code reads (`update/desired`, `update/policy`, `update/proposed/..`), the
/// counters, and a key a later build adds under `update/` are not this reader's, whatever
/// their bytes.
#[test]
fn other_keys_are_not_decoded() {
    for key in [
        "update/desired",
        "update/policy",
        "update/proposed/0123",
        "update/generation",
        "update/order/0000000000000001",
        "update/something-later",
        "update/hosts",
        "elsewhere/slot",
    ] {
        assert_eq!(decode(key, &[0xff]), Ok(None), "{key}");
    }
}

/// Catches: a store written by an older build that does not open or does not read: every
/// kind of row it left (hosts, the slot, and the rows and counters the live code owns) is in
/// a store that is closed and opened again, and `read_all` gives the host and slot rows by
/// key, and nothing of the others. A store with no update rows gives none.
#[test]
fn a_store_holding_old_update_rows_opens_and_reads() {
    for backing in BACKINGS {
        let mut cell = Cell::new(backing, "old-rows");
        assert_eq!(read_all(cell.store().as_ref()), Ok(Vec::new()));
        {
            let store = cell.store();
            put(store.as_ref(), "update/host/s1", HELD);
            put(store.as_ref(), "update/host/d1", PENDING);
            put(store.as_ref(), "update/slot", SLOT);
            put(
                store.as_ref(),
                "update/desired",
                &[0x08, 0x01, 0x12, 0x01, 0x76],
            );
            put(store.as_ref(), "update/policy", &[0x08, 0x01]);
            put(store.as_ref(), "update/generation", &[0x01]);
            put(store.as_ref(), "member/1", b"not an update row");
        }
        if backing == Backing::Sqlite {
            cell.reopen();
        }
        let mut rows = read_all(cell.store().as_ref()).expect("the rows read");
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        let keys: Vec<&str> = rows.iter().map(|(key, _)| key.as_str()).collect();
        assert_eq!(
            keys,
            ["update/host/d1", "update/host/s1", "update/slot"],
            "{backing:?}"
        );
        assert!(matches!(&rows[0].1, Row::Host { id, row } if id == "d1" && row.phase == 1));
        assert!(matches!(&rows[1].1, Row::Host { id, row } if id == "s1" && row.reason == 7));
        assert!(matches!(&rows[2].1, Row::Slot(slot) if slot.holders().count() == 3));
    }
}

/// Catches: one row this build cannot read being passed over, or hiding the rest: `read_all`
/// names the row that is corrupt and fails, rather than returning what it could.
#[test]
fn a_corrupt_row_in_the_store_is_named() {
    let cell = Cell::new(Backing::Fake, "corrupt-row");
    let store = cell.store();
    put(store.as_ref(), "update/host/ok", PENDING);
    put(store.as_ref(), "update/host/bad", &[0x08, 0x63]);
    let Err(ReadError::Corrupt { key, why }) = read_all(store.as_ref()) else {
        panic!("a corrupt row was read past");
    };
    assert_eq!(key, "update/host/bad");
    assert!(why.contains("phase 99"), "{why}");
}
