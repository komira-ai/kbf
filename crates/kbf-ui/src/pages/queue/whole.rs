//! The Queue page's whole-machine section: shown only when the document has a
//! `whole_node` section, that is when the server runs with `--whole-node-max`.

use super::super::definitions;
use super::super::sched::{WholeNode, WholeRequest, age, code, node_link};
use super::{cell, heading, hint, table};
use crate::html::Html;

/// The words for a `reason` of the API; a reason this page does not know shows as the API
/// spells it.
fn stop_words(reason: &str) -> &str {
    match reason {
        "cap_ceiling" => "stopped: workers whole are at the ceiling",
        "cap_keep_for_ordinary" => "stopped: the rest of the pool is kept for ordinary work",
        "cap_max_draining" => "stopped: the most workers are already draining",
        "cap_per_consumer" => "stopped: the consumer is at its limit",
        "last_capable" => "stopped: the last worker an older ordinary request could run on",
        other => other,
    }
}

/// What one request is doing, in words: its state's fields, or the scheduler's reason
/// text as the API gave it.
fn detail(r: &WholeRequest) -> String {
    let some = |ms: Option<u64>| ms.map_or_else(|| "-".to_owned(), age);
    match r.state.as_str() {
        "early" => format!("may hold a worker in {}", some(r.may_hold_in_ms)),
        "holding" => {
            let worker = r.worker.as_deref().unwrap_or("-");
            let drain = if r.draining == Some(true) {
                "waiting for its leases to end"
            } else {
                "no leases left"
            };
            let until = r
                .deadline_in_ms
                .map_or_else(|| "never".to_owned(), |ms| format!("in {}", age(ms)));
            format!(
                "holds {worker} for {}, {drain}; gives it up {until}; holds given up: {}",
                some(r.held_for_ms),
                r.give_ups.unwrap_or(0)
            )
        }
        "barred" => format!(
            "barred from every worker it could hold; holds given up: {}",
            r.give_ups.unwrap_or(0)
        ),
        "stopped" => stop_words(&r.reason).to_owned(),
        "queued" => "waiting for a worker".to_owned(),
        "unservable" => r.unservable.clone().unwrap_or_default(),
        other => other.to_owned(),
    }
}

/// The state as a person reads it: a request that only waits is "Waiting".
fn state_name(state: &str) -> &str {
    match state {
        "early" => "Early",
        "holding" => "Holding",
        "barred" => "Barred",
        "stopped" => "Stopped",
        "queued" => "Waiting",
        "unservable" => "Unservable",
        other => other,
    }
}

/// The section: the caps' numbers, the count for each reason, and the requests.
pub(super) fn whole_node(h: &mut Html, whole: &WholeNode) {
    heading(h, "Whole-machine requests");
    definitions(
        h,
        &[
            (
                "Workers held by a whole-machine lease",
                whole.held.to_string(),
            ),
            ("Workers held by a queued request", whole.marks.to_string()),
            (
                "Of those, still draining",
                format!("{} (at most {})", whole.draining, whole.max_draining),
            ),
            (
                "Workers that may be whole",
                format!(
                    "{} (at most {}; {} capable, {} kept for ordinary work)",
                    whole.ceiling,
                    whole.max_whole_node,
                    whole.capable_hosts,
                    whole.keep_for_ordinary
                ),
            ),
        ],
    );
    let waiting: Vec<String> = whole
        .waiting
        .iter()
        .filter(|(_, count)| **count > 0)
        .map(|(reason, count)| format!("{reason}: {count}"))
        .collect();
    if waiting.is_empty() {
        hint(h, "No whole-machine request is queued.");
        return;
    }
    hint(h, &format!("Queued by reason: {}.", waiting.join(", ")));
    table(
        h,
        &["Operation", "Instance", "State", "Detail", "Barred"],
        |h| {
            for r in &whole.requests {
                h.el("tr", &[], |h| {
                    h.el("td", &[], |h| code(h, &r.operation));
                    cell(h, &r.instance);
                    cell(h, state_name(&r.state));
                    cell(h, &detail(r));
                    h.el("td", &[], |h| {
                        for (i, barred) in r.barred.iter().enumerate() {
                            if i > 0 {
                                h.text(", ");
                            }
                            node_link(h, &barred.worker);
                            match barred.until_in_ms {
                                Some(ms) => h.text(&format!(" for {}", age(ms))),
                                None => h.text(" with no end"),
                            };
                        }
                    });
                });
            }
        },
    );
    if u64::try_from(whole.requests.len()).is_ok_and(|shown| whole.requests_total > shown) {
        hint(
            h,
            &format!(
                "Showing {} of {}: the API lists the first of them.",
                whole.requests.len(),
                whole.requests_total
            ),
        );
    }
}
