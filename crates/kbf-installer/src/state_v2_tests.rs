use serde_json::{Value, json};

type Edit = Box<dyn Fn(&mut Value)>;

use super::*;
use crate::golden::{STATE, state};

fn edit(f: impl FnOnce(&mut Value)) -> Vec<u8> {
    let mut v: Value = serde_json::from_str(STATE).expect("json");
    f(&mut v);
    serde_json::to_vec(&v).expect("bytes")
}

/// Catches a change to the state's fields that does not move the golden file, and a
/// writer whose output the reader of the next start would not take back.
#[test]
fn the_golden_state_decodes_and_encodes_to_itself() {
    assert_eq!(StateV2::parse(STATE.as_bytes()).expect("parses"), state());
    assert_eq!(
        String::from_utf8(state().render().expect("encodes")).expect("text"),
        STATE
    );
}

/// Catches an unknown key being dropped on the way through (a newer writer's field lost on
/// the next write), at the top and in each nested record.
#[test]
fn an_unknown_key_is_refused_at_every_level() {
    let cases: [Edit; 4] = [
        Box::new(|v| v["order_floor"] = json!(7)),
        Box::new(|v| v["floor"]["x"] = json!(1)),
        Box::new(|v| v["pending_confirm"]["x"] = json!(1)),
        Box::new(|v| v["unacked"]["x"] = json!(1)),
    ];
    for (i, f) in cases.into_iter().enumerate() {
        let err = StateV2::parse(&edit(f)).expect_err("refused").to_string();
        assert!(err.contains("unknown field"), "case {i}: {err}");
    }
}

/// Catches a schema 1 file read as schema 2 (the migration is P3's; nothing may guess).
#[test]
fn another_schema_is_refused() {
    for n in [0, 1, 3] {
        let bytes = edit(|v| v["schema"] = json!(n));
        assert_eq!(StateV2::parse(&bytes), Err(StateError::Schema(n)));
    }
}

/// Catches ids that name something other than what they should: an activation that is not
/// `<epoch>-<id>`, an instance that is not a plain unit-name part.
#[test]
fn named_values_are_checked() {
    let cases: [Edit; 3] = [
        Box::new(|v| v["pending_confirm"]["activation"] = json!("42")),
        Box::new(|v| v["serving_instance"] = json!("a/../b")),
        Box::new(|v| v["serving_instance"] = json!("")),
    ];
    for (i, f) in cases.into_iter().enumerate() {
        assert!(
            matches!(StateV2::parse(&edit(f)), Err(StateError::Syntax(_))),
            "case {i}"
        );
    }
}

/// A host that installed nothing writes a state it can read back.
#[test]
fn a_new_state_round_trips() {
    let bytes = StateV2::new().render().expect("encodes");
    assert_eq!(StateV2::parse(&bytes), Ok(StateV2::new()));
}

fn instance(name: &str) -> Result<Instance, String> {
    Instance::try_from(name.to_owned())
}

/// Catches each clause of the instance name check: empty, over 16, upper case, a symbol;
/// digits and letters are accepted.
#[test]
fn an_instance_is_lower_case_letters_and_digits() {
    for bad in ["", "abcdefghijklmnopq", "A", "a-b", "a b"] {
        assert!(instance(bad).is_err(), "{bad:?}");
    }
    for ok in ["a", "7", "b2", "abcdefghijklmnop"] {
        assert_eq!(instance(ok).map(String::from), Ok(ok.to_owned()));
    }
}

#[test]
fn the_default_state_is_the_new_state() {
    assert_eq!(StateV2::default(), StateV2::new());
}
