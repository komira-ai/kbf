//! The lines the local socket answers with.

use serde_json::{Value, json};

use super::answer::{Answer, Code, LastView, PendingView, QueryView};

fn line(answer: &Answer) -> Value {
    serde_json::from_str(&answer.json_line()).unwrap()
}

/// Every answer is one JSON object with a `result`, and the fields the agent reads, in the
/// names the agent reads them by.
#[test]
fn each_answer_is_a_line_with_its_fields() {
    let staged = Answer::Staged {
        version: "1.0-1".into(),
        commit: "a".repeat(40),
        current: false,
    };
    assert_eq!(
        line(&staged),
        json!({"result":"staged","version":"1.0-1","commit":"a".repeat(40),"current":false})
    );
    let switched = Answer::Switched {
        version: "1.0-1".into(),
        commit: "a".repeat(40),
        deadline_unix: 77,
    };
    assert_eq!(
        line(&switched),
        json!({"result":"switched","version":"1.0-1","commit":"a".repeat(40),"deadline_unix":77})
    );
    let confirmed = Answer::Confirmed {
        version: "1.0-1".into(),
    };
    assert_eq!(
        line(&confirmed),
        json!({"result":"confirmed","version":"1.0-1"})
    );
    assert_eq!(
        line(&Answer::Reverted { version: None }),
        json!({"result":"reverted","version":null})
    );
    assert_eq!(
        line(&Answer::Reverted {
            version: Some("0.9-1".into())
        }),
        json!({"result":"reverted","version":"0.9-1"})
    );
}

/// A query tells what runs, what is on trial with its deadline, and how the last update ended.
#[test]
fn a_query_line_carries_the_release_the_trial_and_the_last_end() {
    let view = QueryView {
        commit: Some("a".repeat(40)),
        version: Some("1.0-1".into()),
        node_id: Some("n1".into()),
        pending: Some(PendingView {
            version: None,
            commit: "b".repeat(40),
            deadline_unix: Some(9),
        }),
        last: Some(LastView {
            target: "0.9-1".into(),
            outcome: "failed",
            reason: Some("timeout"),
        }),
        busy: true,
        releases: Vec::new(),
        releases_omitted: 0,
        releases_unavailable: false,
    };
    assert_eq!(
        line(&Answer::Queried(view)),
        json!({
            "result":"query","commit":"a".repeat(40),"version":"1.0-1","node_id":"n1",
            "pending":{"version":null,"commit":"b".repeat(40),"deadline_unix":9},
            "last":{"target":"0.9-1","outcome":"failed","reason":"timeout"},
            "busy":true
        })
    );
    let empty = QueryView {
        commit: None,
        version: None,
        node_id: None,
        pending: None,
        last: None,
        busy: false,
        releases: Vec::new(),
        releases_omitted: 0,
        releases_unavailable: false,
    };
    assert_eq!(
        line(&Answer::Queried(empty)),
        json!({"result":"query","commit":null,"version":null,"node_id":null,
               "pending":null,"last":null,"busy":false})
    );
}

/// A refusal and a failure carry a closed code and nothing of the detail, which goes to the
/// log; the audit fields say the same.
#[test]
fn a_refusal_and_a_failure_carry_a_code_and_not_the_detail() {
    let refused = Answer::Refused(Code::NotStaged, "secret path /root/x".into());
    assert_eq!(
        line(&refused),
        json!({"result":"refused","reason":"not_staged"})
    );
    assert_eq!(refused.audit_fields(), ("refused", Some("not_staged")));
    let failed = Answer::Failed(Code::SwitchFailed, "unit x".into());
    assert_eq!(
        line(&failed),
        json!({"result":"failed","reason":"switch_failed"})
    );
    assert_eq!(failed.audit_fields(), ("failed", Some("switch_failed")));
}

/// The audit fields of the answers that went well name the outcome and no reason.
#[test]
fn the_audit_fields_of_good_answers_name_the_outcome() {
    let cases = [
        (
            Answer::Staged {
                version: "1".into(),
                commit: "a".into(),
                current: true,
            },
            "staged",
        ),
        (
            Answer::Switched {
                version: "1".into(),
                commit: "a".into(),
                deadline_unix: 1,
            },
            "switched",
        ),
        (
            Answer::Confirmed {
                version: "1".into(),
            },
            "confirmed",
        ),
        (Answer::Reverted { version: None }, "reverted"),
    ];
    for (answer, word) in cases {
        assert_eq!(answer.audit_fields(), (word, None));
        assert_eq!(answer.log_detail(), None);
    }
    let query = Answer::Queried(QueryView {
        commit: None,
        version: None,
        node_id: None,
        pending: None,
        last: None,
        busy: false,
        releases: Vec::new(),
        releases_omitted: 0,
        releases_unavailable: false,
    });
    assert_eq!(query.audit_fields(), ("query", None));
}

/// The detail for the log has its control characters replaced and is cut at the limit, so a
/// child's output cannot forge a log line or fill the journal.
#[test]
fn the_detail_for_the_log_is_cleaned_and_cut() {
    let dirty = Answer::Failed(Code::StageFailed, "a\nb\u{1b}[31mc".into());
    assert_eq!(dirty.log_detail().as_deref(), Some("a?b?[31mc"));
    let long = Answer::Refused(Code::FetchFailed, "x".repeat(5000));
    assert_eq!(
        long.log_detail().map(|d| d.len()),
        Some(crate::outcome::MAX_DETAIL_LEN)
    );
}

/// The codes are distinct words that read back as themselves; a word that is none is none.
#[test]
fn the_codes_are_a_closed_set_of_distinct_words() {
    let mut words: Vec<&str> = Code::ALL.iter().map(|c| c.code()).collect();
    for code in Code::ALL {
        assert_eq!(Code::from_code(code.code()), Some(code));
    }
    words.sort_unstable();
    words.dedup();
    assert_eq!(words.len(), Code::ALL.len());
    assert_eq!(Code::from_code("not_a_code"), None);
    assert_eq!(Code::from_code(""), None);
}
