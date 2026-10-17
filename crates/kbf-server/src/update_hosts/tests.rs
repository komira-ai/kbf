use serde_json::json;

use super::*;

/// Catches the pause rule read wrong: `policy.paused` used only when that key is there, so a
/// policy row without it never reads as "running" (a wrong "running" is a safety error), and
/// a policy that has the key wins over the desired row whatever the two say. A key that is
/// not a boolean is unknown, never running. Mutants: read only `policy.paused`; read only
/// `desired.paused`; prefer `desired.paused` when both are there; treat a policy object
/// as the answer when it lacks the key.
#[test]
fn paused_is_the_policys_key_when_it_has_one_else_the_desired_rows() {
    let desired = |paused| json!({"generation": 1, "paused": paused});
    let table = [
        // (desired, policy, paused)
        (desired(true), json!(null), Some(true)),
        (desired(false), json!(null), Some(false)),
        (desired(true), json!({"auto_nodes": true}), Some(true)),
        (desired(false), json!({"auto_nodes": true}), Some(false)),
        (desired(false), json!({"paused": true}), Some(true)),
        (desired(true), json!({"paused": false}), Some(false)),
        (json!(null), json!({"paused": true}), Some(true)),
        (json!(null), json!({"auto_nodes": true}), None),
        (json!(null), json!(null), None),
        (desired(false), json!({"paused": null}), None),
        (desired(false), json!({"paused": "yes"}), None),
    ];
    for (desired, policy, want) in table {
        assert_eq!(
            effective_paused(&desired, &policy),
            want,
            "desired {desired}, policy {policy}"
        );
    }
}

fn page(query: &str) -> Result<(usize, Option<String>), String> {
    Page::parse(query).map(|p| (p.size, p.after))
}

/// Catches a page query read loosely: the default and the bounds of `page_size` (0, past
/// the maximum, not a number), a token taken from the wrong key, and a query key the route
/// does not take ignored (a typo that silently pages nothing).
#[test]
fn a_page_query_is_checked() {
    assert_eq!(page(""), Ok((DEFAULT_PAGE_SIZE, None)));
    assert_eq!(page("page_size=1"), Ok((1, None)));
    assert_eq!(
        page(&format!("page_size={MAX_PAGE_SIZE}&page_token=n%201")),
        Ok((MAX_PAGE_SIZE, Some("n 1".to_owned())))
    );
    for bad in [
        "page_size=0",
        &format!("page_size={}", MAX_PAGE_SIZE + 1),
        "page_size=many",
        "page_size=-1",
        "size=3",
    ] {
        assert!(page(bad).is_err(), "{bad}");
    }
}
