use serde_json::{Value, json};

use super::*;

fn host(node: &str, version: &str) -> Value {
    json!({"node_id": node, "role": "daemon", "version": version, "silent_secs": 5})
}

/// Catches a token file accepted that others can read or write, one that is not the
/// caller's, and one that is not a regular file (a link or a FIFO).
#[test]
fn a_token_file_must_be_the_callers_and_private() {
    assert_eq!(token_rule(true, 501, 0o100600, 501), Ok(()));
    assert_eq!(token_rule(true, 501, 0o100400, 501), Ok(()));
    for mode in [0o100640, 0o100604, 0o100660, 0o100644, 0o100700, 0o100000] {
        assert!(token_rule(true, 501, mode, 501).is_err(), "{mode:o}");
    }
    assert!(token_rule(true, 0, 0o100600, 501).is_err());
    assert!(token_rule(false, 501, 0o100600, 501).is_err());
}

/// Catches a row taken from the wrong place: another service's, or the wrong node's; and
/// a farm whose coordinator has no per-service section (kbf's hosts are then the
/// top-level ones).
#[test]
fn the_row_is_the_nodes_in_the_services_section() {
    let status = json!({
        "hosts": [host("a", "top")],
        "per_service": {
            "kbf": {"hosts": [host("a", "kbf"), host("b", "kbf")]},
            "other": {"hosts": [host("a", "other")]},
        },
    });
    assert_eq!(find_row(&status, "kbf", "b"), Ok(&host("b", "kbf")));
    assert_eq!(find_row(&status, "other", "a"), Ok(&host("a", "other")));
    assert_eq!(
        find_row(&status, "other", "b"),
        Err(RowError::NoNode("b".to_owned(), "other".to_owned()))
    );
    assert_eq!(
        find_row(&status, "nope", "a"),
        Err(RowError::NoService("nope".to_owned()))
    );
    let old = json!({"hosts": [host("a", "top")]});
    assert_eq!(find_row(&old, "kbf", "a"), Ok(&host("a", "top")));
    assert_eq!(
        find_row(&old, "other", "a"),
        Err(RowError::NoService("other".to_owned()))
    );
    assert!(find_row(&json!({}), "kbf", "a").is_err());
    assert!(
        find_row(
            &json!({"per_service": {"kbf": {"hosts": [{"role": "x"}]}}}),
            "kbf",
            "a"
        )
        .is_err()
    );
}

/// Catches a field invented for a row that lacks it, a null printed as text that looks like
/// a value, a last-seen without its unit, and a value with a control character (a terminal
/// escape in a reported string) printed raw.
#[test]
fn a_row_prints_what_it_has() {
    let row = json!({
        "node_id": "n\u{1b}[31m",
        "role": "daemon",
        "updating": null,
        "silent_secs": 5,
        "up": false,
        "hold": "no_slot",
    });
    let text = render_text(&row, &json!({}), "kbf");
    assert_eq!(
        text.lines().collect::<Vec<_>>(),
        [
            r#"node_id: "n\u001b[31m""#,
            "service: kbf",
            "role: daemon",
            "up: false",
            "updating: none",
            "last_seen: 5s ago",
            "hold: no_slot",
        ]
    );
}

/// Catches an exclusion or halt of another service or node reported for this one.
#[test]
fn only_the_exclusion_of_this_node_and_service_is_printed() {
    let status = json!({"ops": {
        "excluded": [
            {"service": "kbf", "node_id": "a", "reason": "disk"},
            {"service": "kbf", "node_id": "b", "reason": "other node"},
            {"service": "x", "node_id": "a", "reason": "other service"},
        ],
        "services": {"kbf": {"halt": {"node_id": "b", "check": "smoke", "target": "2"}},
                     "x": {"halt": {"node_id": "a", "check": "no", "target": "9"}}},
    }});
    let text = render_text(&host("a", "1"), &status, "kbf");
    assert!(text.lines().any(|l| l == "excluded: disk"), "{text}");
    assert!(!text.contains("other"), "{text}");
    assert!(
        text.contains(r#"halt: {"check":"smoke","node_id":"b","target":"2"}"#),
        "{text}"
    );
    assert!(!text.contains(r#""check":"no""#), "{text}");
    let bare = render_text(&host("a", "1"), &json!({}), "kbf");
    assert!(
        !bare.contains("excluded") && !bare.contains("halt"),
        "{bare}"
    );
}
