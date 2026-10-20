//! The decision to roll back and the call that does it.

mod common;

use std::time::Duration;

use common::api;
use kbf_bake::rollback::{Inputs, Plan, offer, plan};
use kbf_gate::{CommitId, Status};

fn old() -> CommitId {
    CommitId::parse(&"cd".repeat(20)).unwrap()
}

/// The inputs under which a rollback is offered; each test breaks one.
fn ready(prev: &CommitId) -> Inputs<'_> {
    Inputs {
        enabled: true,
        status: Status::Red,
        measured: true,
        reversible: Some(true),
        previous: Some(prev),
        generation: Some(7),
    }
}

/// The one case that offers: asked for, red, measured, reversible, with a previous commit and a
/// generation. The offer names the previous commit and the deploy's generation.
#[test]
fn a_measured_hard_red_of_a_reversible_release_offers_the_previous_commit() {
    let prev = old();
    assert_eq!(
        plan(&ready(&prev)),
        Plan::Offer {
            commit: prev.clone(),
            expect: 7
        }
    );
}

/// Catches a rollback nobody asked for: with the flag off, the same red offers nothing and does
/// not even page a rollback line (`Off`).
#[test]
fn the_flag_off_is_off() {
    let prev = old();
    let mut i = ready(&prev);
    i.enabled = false;
    assert_eq!(plan(&i), Plan::Off);
}

/// Catches a rollback on anything but a red end: a green bake, an extended bake (thin load, an
/// unreadable farm) and one still running never roll back. A soft gate (latency between 1.5x and
/// 3x) never ends a bake red, so it is covered here by its end, which is not red.
#[test]
fn only_a_red_end_rolls_back() {
    let prev = old();
    for status in [Status::Green, Status::Extended, Status::Running] {
        let mut i = ready(&prev);
        i.status = status;
        assert_eq!(plan(&i), Plan::Off, "{status:?}");
    }
}

/// Catches a rollback of an irreversible release: it pages, says why and offers nothing. Unknown
/// reversibility (no hand-off) is not "yes".
#[test]
fn an_irreversible_or_unknown_release_pages_and_offers_nothing() {
    let prev = old();
    let mut i = ready(&prev);
    i.reversible = Some(false);
    let Plan::Page(why) = plan(&i) else {
        panic!("a page")
    };
    assert!(why.contains("not reversible"), "{why}");
    i.reversible = None;
    let Plan::Page(why) = plan(&i) else {
        panic!("a page")
    };
    assert!(why.contains("unknown"), "{why}");
}

/// Catches a rollback on a gate that was red only for want of a measurement (an absent series, no
/// baseline): that is no evidence the release is bad.
#[test]
fn a_red_with_no_measurement_pages_and_offers_nothing() {
    let prev = old();
    let mut i = ready(&prev);
    i.measured = false;
    let Plan::Page(why) = plan(&i) else {
        panic!("a page")
    };
    assert!(why.contains("no measurement"), "{why}");
}

/// Catches an offer with nowhere to go or nothing to compare against: no previous commit, or no
/// generation (the compare-and-set would be blind), pages.
#[test]
fn no_previous_commit_or_no_generation_pages() {
    let prev = old();
    let mut i = ready(&prev);
    i.previous = None;
    let Plan::Page(why) = plan(&i) else {
        panic!("a page")
    };
    assert!(why.contains("previous commit"), "{why}");
    let mut i = ready(&prev);
    i.generation = None;
    let Plan::Page(why) = plan(&i) else {
        panic!("a page")
    };
    assert!(why.contains("generation"), "{why}");
}

/// Catches a call to the wrong route, without the token, without the JSON content type the write
/// gate demands, or without the compare-and-set generation: one POST, with each of them.
#[test]
fn the_offer_is_a_compare_and_set_post_with_the_token() {
    let (url, rx) = api("200 OK", "{}");
    offer(
        &format!("{url}/"),
        "s3cret",
        &old(),
        7,
        Duration::from_secs(5),
    )
    .unwrap();
    let req = rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(req.starts_with("POST /v1/update/desired HTTP/1.1"), "{req}");
    let lower = req.to_ascii_lowercase();
    assert!(lower.contains("authorization: bearer s3cret"), "{req}");
    assert!(lower.contains("content-type: application/json"), "{req}");
    let body: serde_json::Value =
        serde_json::from_str(req.split_once("\r\n\r\n").unwrap().1).unwrap();
    assert_eq!(
        body,
        serde_json::json!({"commit": "cd".repeat(20), "expect_generation": 7})
    );
}

/// Catches a refusal read as success: a 409 (someone moved the desired commit since) and a 401 are
/// errors that say so, and each was a single request.
#[test]
fn a_refused_offer_is_an_error() {
    for status in [
        "409 Conflict",
        "401 Unauthorized",
        "503 Service Unavailable",
    ] {
        let (url, rx) = api(status, "{}");
        let why = offer(&url, "t", &old(), 7, Duration::from_secs(5)).unwrap_err();
        assert!(why.contains(&status[..3]), "{status}: {why}");
        assert!(rx.recv_timeout(Duration::from_secs(5)).is_ok());
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "one request only"
        );
    }
}
