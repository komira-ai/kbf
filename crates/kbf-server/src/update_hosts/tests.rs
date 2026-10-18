use serde_json::json;

use super::*;

/// Catches the pause rule read wrong: the policy's `paused` when it is a boolean, false
/// without a policy row (as `GET /v1/update`), and unknown, never "running", when the
/// policy object lacks the key or holds a value that is not a boolean. Mutants: no policy
/// row read as unknown; a policy without the key read as running; a non-boolean read as
/// false.
#[test]
fn paused_is_the_policys_key_and_unknown_never_reads_running() {
    let table = [
        (json!(null), Some(false)),
        (json!({"paused": true}), Some(true)),
        (json!({"paused": false}), Some(false)),
        (json!({"auto_nodes": true}), None),
        (json!({"paused": null}), None),
        (json!({"paused": "yes"}), None),
    ];
    for (policy, want) in table {
        assert_eq!(effective_paused(&policy), want, "policy {policy}");
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
