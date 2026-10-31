//! The owner row's key and value, and how an owner is shown. The reads of a node's owner
//! over a state store are in `tests/state_store/cordon_owner.rs` and
//! `farm/durable/reads_tests.rs`.

use kbf_types::WorkerId;

use super::cordon_owner::{key, parse, shown};
use crate::fleet::Owner;

/// Asserts that the row of a node is in a family of its own, which the scans of the
/// `cordon/` rows (the intake poll, a node read) do not reach. Catches: a key under
/// `cordon/`, which a frontend that does not know of owners would refuse as a corrupt
/// cordon on every read.
#[test]
fn the_row_is_outside_the_cordon_family() {
    let key = key(&WorkerId::new("node-a"));

    assert_eq!(key, "cordon-owner/node-a");
    assert!(!key.starts_with("cordon/"));
}

/// Asserts that the two forms parse, with the unit as it is written. Catches: a parser that
/// trims or lowercases a unit, so the owner shown is not the owner written.
#[test]
fn the_operator_and_an_updater_unit_parse() {
    assert_eq!(parse(b"operator"), Some(Owner::Operator));
    assert_eq!(
        parse(b"updater:kbf-daemon"),
        Some(Owner::Updater("kbf-daemon".to_owned()))
    );
    assert_eq!(
        parse(b"updater:Roll 7:b"),
        Some(Owner::Updater("Roll 7:b".to_owned()))
    );
}

/// Asserts that every other value names no owner. Catches: an updater with no unit, a form
/// with other case or padding, a value that is not text, and an empty row, taken for an
/// owner.
#[test]
fn any_other_value_names_no_owner() {
    let others: [&[u8]; 8] = [
        b"",
        b"updater:",
        b"updater",
        b"Operator",
        b"operator ",
        b"operator\n",
        b"root",
        &[0xff, 0xfe],
    ];
    for value in others {
        assert_eq!(parse(value), None, "{value:?}");
    }
}

/// Asserts that an owner is shown as the word the row holds. Catches: a view that shows the
/// enum's debug form, or an object where clients of the API read a string.
#[test]
fn an_owner_is_shown_as_its_row_word() {
    let shown = |owner: &Owner| serde_json::to_string(owner).expect("json");

    assert_eq!(shown(&Owner::Operator), "\"operator\"");
    assert_eq!(shown(&Owner::Updater("u1".to_owned())), "\"updater:u1\"");
    assert_eq!(Owner::default(), Owner::Operator);
}

/// Asserts that a farm with no state store has no owner row to read and says the operator.
#[test]
fn a_farm_with_no_store_says_the_operator() {
    assert_eq!(shown(None, &WorkerId::new("node-a")), Owner::Operator);
}
