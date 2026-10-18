use serde_json::{Value, json};

type Edit = Box<dyn Fn(&mut Value)>;

use super::*;
use crate::golden::{ORDER_ACTIVATE, ORDER_CONFIRM, order_activate, order_confirm};

fn edit(fixture: &str, f: impl FnOnce(&mut Value)) -> Vec<u8> {
    let mut v: Value = serde_json::from_str(fixture).expect("json");
    f(&mut v);
    serde_json::to_vec(&v).expect("bytes")
}

fn shape_error(bytes: &[u8]) -> String {
    OrderRecord::parse(bytes).expect_err("refused").to_string()
}

/// Catches a change to the record's field names, order, types or defaults that does not
/// move the golden file: the fixture decodes to the expected value and the value encodes
/// to the fixture byte for byte (the writer in `kbf-update` and the reader here agree).
#[test]
fn the_golden_records_decode_and_encode_to_themselves() {
    for (text, want) in [
        (ORDER_ACTIVATE, order_activate()),
        (ORDER_CONFIRM, order_confirm()),
    ] {
        assert_eq!(OrderRecord::parse(text.as_bytes()).expect("parses"), want);
        let mut again = serde_json::to_string_pretty(&want).expect("encodes");
        again.push('\n');
        assert_eq!(again, text);
    }
}

/// Catches a parser that ignores a key it does not know: at the top, in a component, in the
/// expected facts. A record is closed; an unknown key is a writer this reader does not understand.
#[test]
fn an_unknown_key_is_refused_at_every_level() {
    let cases: [Edit; 3] = [
        Box::new(|v| v["path"] = json!("/tmp/x")),
        Box::new(|v| v["components"][0]["url"] = json!("https://x")),
        Box::new(|v| v["expected"]["extra"] = json!(1)),
    ];
    for (i, f) in cases.into_iter().enumerate() {
        let err = shape_error(&edit(ORDER_ACTIVATE, f));
        assert!(err.contains("unknown field"), "case {i}: {err}");
    }
}

/// Catches a duplicate key being resolved silently (the later one winning is how a second
/// "commit" smuggles past a check that read the first).
#[test]
fn a_duplicate_key_is_refused() {
    let text = ORDER_CONFIRM.replacen("\"epoch\": 3,", "\"epoch\": 3,\n  \"epoch\": 4,", 1);
    assert!(shape_error(text.as_bytes()).contains("duplicate field"));
}

/// Catches a size cap applied after the parse (the bytes are in memory by then) or not at all.
#[test]
fn a_record_over_the_cap_is_refused_before_it_is_parsed() {
    let big = vec![b' '; MAX_RECORD_BYTES + 1];
    assert_eq!(
        OrderRecord::parse(&big),
        Err(RecordError::TooLarge(MAX_RECORD_BYTES + 1))
    );
}

/// Catches values the types must keep out of the installer: a short or upper-case commit
/// (a second spelling of one release), a malformed digest, an unknown verb or platform, and
/// numbers that are not the unsigned integers the order of orders needs.
#[test]
fn field_syntax_is_checked() {
    let cases: [(&str, Edit); 9] = [
        ("commit", Box::new(|v| v["commit"] = json!("abc"))),
        ("commit", Box::new(|v| v["commit"] = json!("A".repeat(40)))),
        (
            "64 lowercase",
            Box::new(|v| v["components"][0]["sha256"] = json!("AB".repeat(32))),
        ),
        (
            "64 lowercase",
            Box::new(|v| v["components"][0]["sha256"] = json!("ab")),
        ),
        ("unknown variant", Box::new(|v| v["verb"] = json!("purge"))),
        (
            "unknown variant",
            Box::new(|v| v["expected"]["platform"] = json!("linux-riscv")),
        ),
        ("invalid type", Box::new(|v| v["order_id"] = json!("42"))),
        ("invalid value", Box::new(|v| v["generation"] = json!(-1))),
        ("invalid type", Box::new(|v| v["window_s"] = json!(1.5))),
    ];
    for (want, f) in cases {
        let err = shape_error(&edit(ORDER_ACTIVATE, f));
        assert!(err.contains(want), "{want}: {err}");
    }
}

/// Catches an `activate` that could reach the installer without the facts or files it is
/// checked against, and a `confirm` that carries a payload it has no use for.
#[test]
fn each_verb_carries_exactly_its_keys() {
    let cases: [(&str, &str, Edit); 9] = [
        (
            ORDER_ACTIVATE,
            "expected facts",
            Box::new(|v| {
                v.as_object_mut().expect("o").remove("expected");
            }),
        ),
        (
            ORDER_ACTIVATE,
            "1 to 4 components",
            Box::new(|v| v["components"] = json!([])),
        ),
        (
            ORDER_ACTIVATE,
            "1 to 4 components",
            Box::new(|v| {
                let c = v["components"][0].clone();
                v["components"] = json!([c, c, c, c, c]);
            }),
        ),
        (
            ORDER_ACTIVATE,
            "named twice",
            Box::new(|v| {
                let mut second = v["components"][0].clone();
                second["sha256"] = json!("cd".repeat(32));
                v["components"] = json!([v["components"][0].clone(), second]);
            }),
        ),
        (
            ORDER_ACTIVATE,
            "plain file name",
            Box::new(|v| v["components"][0]["name"] = json!("../kbf")),
        ),
        (
            ORDER_ACTIVATE,
            "plain file name",
            Box::new(|v| v["components"][0]["name"] = json!(".kbf")),
        ),
        (
            ORDER_ACTIVATE,
            "size 0",
            Box::new(|v| v["components"][0]["size"] = json!(0)),
        ),
        (
            ORDER_CONFIRM,
            "no components",
            Box::new(|v| v["components"] = json!([order_component()])),
        ),
        (
            ORDER_ACTIVATE,
            "profile",
            Box::new(|v| v["expected"]["profile"] = json!("rel.ease")),
        ),
    ];
    for (fixture, want, f) in cases {
        let err = shape_error(&edit(fixture, f));
        assert!(err.contains(want), "{want}: {err}");
    }
}

fn order_component() -> Value {
    serde_json::to_value(&order_activate().components[0]).expect("value")
}

/// Catches an attestation bound that is off by one, or missing: 256 KiB passes, one more
/// byte is refused, and an empty bundle is no attestation.
#[test]
fn the_attestation_is_bounded() {
    let with = |n: usize| {
        edit(ORDER_ACTIVATE, |v| {
            v["components"][0]["attestation"] = json!("x".repeat(n))
        })
    };
    assert!(OrderRecord::parse(&with(MAX_ATTESTATION_BYTES)).is_ok());
    assert!(shape_error(&with(MAX_ATTESTATION_BYTES + 1)).contains("attestation"));
    assert!(shape_error(&with(0)).contains("attestation"));
}

/// Catches an activation id parser that takes signs, spaces, a missing half or a third part.
#[test]
fn activation_ids_are_two_decimal_numbers() {
    assert_eq!(
        "3-42".parse(),
        Ok(ActivationId {
            epoch: 3,
            order_id: 42
        })
    );
    assert_eq!(order_activate().activation().to_string(), "3-42");
    for bad in [
        "",
        "3",
        "3-",
        "-4",
        "+3-4",
        "3-4-5",
        "3_4",
        " 3-4",
        "a-b",
        "99999999999999999999-1",
    ] {
        assert!(bad.parse::<ActivationId>().is_err(), "{bad:?}");
    }
}
