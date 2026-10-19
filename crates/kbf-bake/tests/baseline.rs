//! The 7-day baseline: the error ratio from the invocation records, the latency p99 from the
//! evaluator's own earlier scrapes, and the file that carries both to the bake.

mod common;

use kbf_bake::baseline::{BASELINE_SCHEMA, BaselineDoc, MAX_PAGES, error_ratio, rfc3339, take};
use kbf_bake::ledger::Ledger;
use kbf_bake::table::BASELINE_SECS;

const AT: u64 = 1_800_000_000; // 2027-01-15T08:00:00Z
const H: u64 = 3600;

fn item(ran: u64, failed: u64) -> String {
    format!(r#"{{"id":"i","state":"closed","counts":{{"ran":{ran},"hits":3,"failed":{failed}}}}}"#)
}

fn page(items: &[String], next: Option<&str>) -> String {
    let next = next.map_or(String::new(), |n| format!(r#","next_page":"{n}""#));
    format!(r#"{{"invocations":[{}]{next}}}"#, items.join(","))
}

/// A farm whose list is `pages`, each fetched in turn; records the URLs asked.
fn ask(pages: Vec<String>, origin: &str, at: u64) -> (Result<f64, String>, Vec<String>) {
    let mut urls = Vec::new();
    let mut rest = pages.into_iter();
    let r = error_ratio(
        &mut |u| {
            urls.push(u.to_owned());
            rest.next().ok_or_else(|| "no more pages".to_owned())
        },
        origin,
        at,
    );
    (r, urls)
}

/// Catches the wrong window and the wrong filter: the first request asks for closed invocations,
/// from exactly seven days before the deploy until the deploy, as RFC 3339 times.
#[test]
fn the_first_request_asks_for_the_seven_days_before_the_deploy() {
    let (_, urls) = ask(vec![page(&[item(10, 1)], None)], "http://farm:1/", AT);
    assert_eq!(
        urls[0],
        "http://farm:1/v1/invocations?since=2027-01-08T08:00:00Z&until=2027-01-15T08:00:00Z&state=closed&page_size=1000"
    );
    assert_eq!(BASELINE_SECS, 7 * 86_400);
}

/// Catches an average of ratios (or only the first page): failed and executed are summed across
/// every invocation of every page, then divided. 1/10 and 3/30 and 0/60 give 4/100.
#[test]
fn the_ratio_is_the_sum_of_failed_over_the_sum_of_executed_across_pages() {
    let (r, urls) = ask(
        vec![
            page(
                &[item(10, 1), item(30, 3)],
                Some("/v1/invocations?page_token=t1"),
            ),
            page(&[item(60, 0)], None),
        ],
        "http://farm:1",
        AT,
    );
    assert!((r.unwrap() - 0.04).abs() < 1e-12);
    assert_eq!(urls[1], "http://farm:1/v1/invocations?page_token=t1");
}

/// Catches a partial list read as complete: an instance that could not be read hides failures.
#[test]
fn a_partial_list_is_no_baseline() {
    let body = r#"{"invocations":[{"counts":{"ran":10,"hits":0,"failed":0}}],"partial":true}"#;
    let (r, _) = ask(vec![body.to_owned()], "http://farm", AT);
    assert!(r.unwrap_err().contains("partial"));
}

/// Catches a farm with no executed action reading as a zero error ratio (0/0 passing as clean).
#[test]
fn no_executed_action_is_no_baseline() {
    let (r, _) = ask(vec![page(&[], None)], "http://farm", AT);
    assert!(r.unwrap_err().contains("no action was executed"));
    let (r, _) = ask(vec![page(&[item(0, 0)], None)], "http://farm", AT);
    assert!(r.is_err());
}

/// Catches a next-page link that leaves the API (another host is not asked) and a list that never
/// ends (the page count is bounded).
#[test]
fn only_a_path_on_the_same_api_is_followed_and_the_walk_is_bounded() {
    let (r, urls) = ask(
        vec![page(&[item(1, 0)], Some("http://elsewhere/x"))],
        "http://farm",
        AT,
    );
    assert!(r.unwrap_err().contains("not a path"));
    assert_eq!(urls.len(), 1);

    let mut asked = 0usize;
    let r = error_ratio(
        &mut |_| {
            asked += 1;
            Ok(page(
                &[item(1, 0)],
                Some("/v1/invocations?page_token=again"),
            ))
        },
        "http://farm",
        AT,
    );
    assert!(r.unwrap_err().contains("pages"));
    assert_eq!(asked, MAX_PAGES);
}

/// Catches an error of the transport or a body that is not a list passing as a baseline.
#[test]
fn a_failed_read_or_a_foreign_body_is_no_baseline() {
    let r = error_ratio(
        &mut |_| Err("connection refused".to_owned()),
        "http://farm",
        AT,
    );
    assert_eq!(r.unwrap_err(), "connection refused");
    let (r, _) = ask(vec!["<html>".to_owned()], "http://farm", AT);
    assert!(r.unwrap_err().contains("not an invocation list"));
}

#[test]
fn times_are_rfc3339_in_utc() {
    assert_eq!(rfc3339(0).unwrap(), "1970-01-01T00:00:00Z");
    assert_eq!(rfc3339(AT).unwrap(), "2027-01-15T08:00:00Z");
    // Times the format cannot hold are errors that name them, not panics.
    for huge in [u64::MAX, i64::MAX as u64, 253_402_300_800] {
        assert!(
            rfc3339(huge).unwrap_err().contains("is not a time"),
            "{huge}"
        );
    }
}

/// A ledger of hourly readings from `from` to `to`, `n_per_hour` fast observations added each hour.
fn ledger(from: u64, to: u64, per_hour: f64) -> Ledger {
    let mut l = Ledger::default();
    let mut t = from;
    let mut total = 0.0;
    while t <= to {
        l.push(t, &[(0.5, total), (2.0, total), (f64::INFINITY, total)])
            .unwrap();
        total += per_hour;
        t += H;
    }
    l
}

/// Catches either half of the baseline lost on the way into the file, and the reason for an absent
/// half being dropped: complete, then each half missing on its own with its reason recorded.
#[test]
fn take_gives_both_values_or_names_what_is_missing() {
    let full = ledger(AT - BASELINE_SECS, AT, 100.0);
    let page_ok = page(&[item(100, 5)], None);
    let doc = take(&mut |_| Ok(page_ok.clone()), "http://farm", &full, AT);
    assert!(doc.complete());
    assert_eq!(doc.schema, BASELINE_SCHEMA);
    assert_eq!(
        (doc.error_ratio, doc.queue_p99, doc.taken_at),
        (Some(0.05), Some(0.5), AT)
    );
    assert!(doc.missing.is_empty());

    let doc = take(&mut |_| Err("down".to_owned()), "http://farm", &full, AT);
    assert_eq!((doc.error_ratio, doc.queue_p99), (None, Some(0.5)));
    assert_eq!(doc.missing.len(), 1);
    assert!(
        doc.missing[0].starts_with("error ratio: down"),
        "{:?}",
        doc.missing
    );

    let short = ledger(AT - 2 * H, AT, 100.0);
    let doc = take(&mut |_| Ok(page_ok.clone()), "http://farm", &short, AT);
    assert_eq!((doc.error_ratio, doc.queue_p99), (Some(0.05), None));
    assert!(
        doc.missing[0].starts_with("queue p99: "),
        "{:?}",
        doc.missing
    );
    assert!(!doc.complete());
}

/// The file round-trips, and a file whose values the gates cannot use is refused at load: a
/// negative or non-finite ratio, a zero p99, an unknown field, another schema.
#[test]
fn a_baseline_file_round_trips_and_unusable_values_are_refused() {
    let dir = common::scratch("baseline-file");
    let path = dir.join("baseline.json");
    let doc = BaselineDoc {
        schema: BASELINE_SCHEMA,
        taken_at: AT,
        error_ratio: Some(0.02),
        queue_p99: None,
        missing: vec!["queue p99: x".to_owned()],
    };
    doc.save(&path).unwrap();
    assert_eq!(BaselineDoc::load(&path).unwrap(), doc);

    let write = |json: &str| {
        std::fs::write(&path, json).unwrap();
        BaselineDoc::load(&path).unwrap_err()
    };
    let base = |extra: &str| {
        format!(
            r#"{{"schema":1,"taken_at":1,"error_ratio":null,"queue_p99":null,"missing":[]{extra}}}"#
        )
    };
    assert!(write(&base(r#","unknown":1"#)).contains("unknown"));
    assert!(write(&base("").replace("\"schema\":1", "\"schema\":2")).contains("schema 2"));
    assert!(
        write(&base("").replace("\"error_ratio\":null", "\"error_ratio\":-0.1"))
            .contains("not usable")
    );
    assert!(
        write(&base("").replace("\"queue_p99\":null", "\"queue_p99\":0")).contains("not usable")
    );
    assert!(BaselineDoc::load(&dir.join("absent.json")).is_err());
    // A directory that is not there cannot take the file.
    let why = doc
        .save(&dir.join("no-such-dir").join("b.json"))
        .unwrap_err()
        .to_string();
    assert!(why.starts_with("write "), "{why}");
}
