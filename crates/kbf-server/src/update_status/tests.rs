use super::*;

fn status(state: State) -> UpdateStatus {
    UpdateStatus {
        running_commit: "0123456789ab".to_owned(),
        state: state as i32,
        ..UpdateStatus::default()
    }
}

/// Catches a state name that drifts from the wire enum (a step shown under another's
/// name), a state this server does not know shown as one it does (a new state read as
/// `idle`), and the unspecified zero read as a state.
#[test]
fn every_wire_state_has_its_own_name_and_an_unknown_one_says_so() {
    let names: Vec<_> = [
        State::Idle,
        State::Fetching,
        State::Staged,
        State::ActivatedUnconfirmed,
        State::Confirmed,
        State::Reverted,
        State::Refused,
        State::Manual,
        State::Unspecified,
    ]
    .into_iter()
    .map(|s| UpdateReport::new(status(s), 1).state)
    .collect();
    assert_eq!(
        names,
        [
            "idle",
            "fetching",
            "staged",
            "activated_unconfirmed",
            "confirmed",
            "reverted",
            "refused",
            "manual",
            "unknown"
        ]
    );
    let future = UpdateStatus {
        state: 99,
        ..UpdateStatus::default()
    };
    assert_eq!(UpdateReport::new(future, 1).state, "unknown");
}

/// Catches a refusal kept uncut (a host sets what the operator API stores), one cut in the
/// middle of a character, one cut short of the bound or past it, control characters
/// (a newline, an escape, NUL) kept, and the same care missing from the commit fields.
/// Mutants: no cut; a cut by `chars().take(512)` (a string of 3-byte characters then
/// holds 1536 bytes); a cut by bytes at 512 (splits a character); no replacement.
#[test]
fn text_from_a_host_is_cut_on_a_character_boundary_and_has_no_control_characters() {
    let long = "x".repeat(100 * 1024);
    let report = UpdateReport::new(
        UpdateStatus {
            refusal: long,
            ..status(State::Refused)
        },
        7,
    );
    assert_eq!(report.refusal.len(), REFUSAL_MAX_BYTES);

    // 3-byte characters: 170 fit in 512 bytes (510), the 171st would make 513.
    let wide = "\u{20ac}".repeat(1000);
    let report = UpdateReport::new(
        UpdateStatus {
            refusal: wide,
            ..status(State::Refused)
        },
        7,
    );
    assert_eq!(report.refusal.len(), 510);
    assert!(report.refusal.chars().all(|c| c == '\u{20ac}'));

    let report = UpdateReport::new(
        UpdateStatus {
            refusal: "a\nb\u{1b}[31mc\0d\u{7f}".to_owned(),
            ..status(State::Refused)
        },
        7,
    );
    assert_eq!(
        report.refusal, "a\u{fffd}b\u{fffd}[31mc\u{fffd}d\u{fffd}",
        "control characters are replaced, the rest kept"
    );

    let report = UpdateReport::new(
        UpdateStatus {
            running_commit: "r".repeat(1000),
            installed_commit: "i".repeat(1000),
            staged_commit: "s\n".repeat(1000),
            ..UpdateStatus::default()
        },
        7,
    );
    assert_eq!(report.running_commit.len(), COMMIT_MAX_BYTES);
    assert_eq!(report.installed_commit.len(), COMMIT_MAX_BYTES);
    assert_eq!(report.staged_commit.len(), COMMIT_MAX_BYTES);
    assert!(!report.staged_commit.contains('\n'));
    assert_eq!(report.received_at_unix_ms, 7);
}

/// A refusal of exactly the bound is kept whole, and one a byte over loses its last
/// character only.
#[test]
fn a_refusal_at_the_bound_is_kept_whole() {
    let exact = "e".repeat(REFUSAL_MAX_BYTES);
    let kept = |refusal: String| {
        UpdateReport::new(
            UpdateStatus {
                refusal,
                ..UpdateStatus::default()
            },
            0,
        )
        .refusal
    };
    assert_eq!(kept(exact.clone()), exact);
    assert_eq!(kept(format!("{exact}e")), exact);
}

/// Catches an availability table with a hole: no report is `unsupported` whatever the
/// stream does (never `ok`, never `unavailable`), a report on a closed stream or one the
/// newest stream has not sent is `unavailable`, and only an open stream that sent it is `ok`.
#[test]
fn availability_is_ok_only_for_an_open_stream_that_sent_its_report() {
    use Availability::{Ok, Unavailable, Unsupported};
    for (has, connected, live, want) in [
        (false, false, false, Unsupported),
        (false, true, true, Unsupported),
        (true, true, true, Ok),
        (true, false, true, Unavailable),
        (true, true, false, Unavailable),
        (true, false, false, Unavailable),
    ] {
        assert_eq!(
            Availability::of(has, connected, live),
            want,
            "report {has}, connected {connected}, live {live}"
        );
    }
}

/// The desired commit and what `up_to_date` says of each running commit: a prefix of it is
/// `true`, another commit `false`, and what is no commit at all `null`.
#[test]
fn up_to_date_is_true_false_or_unknown() {
    const DESIRED: &str = "0123456789abcdef0123456789abcdef01234567";
    assert_eq!(up_to_date("0123456789ab", DESIRED), Some(true));
    assert_eq!(up_to_date(DESIRED, DESIRED), Some(true), "a full commit");
    assert_eq!(up_to_date("fedcba987654", DESIRED), Some(false));
    assert_eq!(
        up_to_date("0123456789ac", DESIRED),
        Some(false),
        "the last digit differs"
    );
    for unknown in [
        "unknown",
        "",
        "0123",
        "0123456789AB",
        "0123456789a\n",
        "xyz",
    ] {
        assert_eq!(up_to_date(unknown, DESIRED), None, "{unknown:?}");
    }
}

fn report(running: &str) -> UpdateReport {
    UpdateReport::new(
        UpdateStatus {
            running_commit: running.to_owned(),
            ..status(State::Idle)
        },
        1,
    )
}

/// Catches a summary that judges a host it cannot trust (a stale or silent host called
/// up to date or out of date), shows its step state when it is not `ok`, judges with no
/// desired commit, or keeps a verdict after being judged against none.
#[test]
fn the_summary_judges_only_a_host_that_is_ok_and_only_with_a_desired_commit() {
    const DESIRED: &str = "0123456789abcdef0123456789abcdef01234567";
    let judged = |report: Option<&UpdateReport>, connected, live, desired| {
        let mut s = UpdateSummary::new(report, connected, live);
        s.judge(desired);
        s
    };
    let ok = judged(Some(&report("0123456789ab")), true, true, Some(DESIRED));
    assert_eq!(
        (ok.state, ok.up_to_date, ok.availability),
        (Some("idle"), Some(true), Availability::Ok)
    );
    let behind = judged(Some(&report("fedcba987654")), true, true, Some(DESIRED));
    assert_eq!(behind.up_to_date, Some(false));
    let none = judged(Some(&report("0123456789ab")), true, true, None);
    assert_eq!((none.state, none.up_to_date), (Some("idle"), None));
    for (connected, live) in [(false, true), (true, false)] {
        let stale = judged(
            Some(&report("0123456789ab")),
            connected,
            live,
            Some(DESIRED),
        );
        assert_eq!(
            (stale.state, stale.up_to_date, stale.availability),
            (None, None, Availability::Unavailable),
            "connected {connected}, live {live}"
        );
    }
    let silent = judged(None, true, true, Some(DESIRED));
    assert_eq!(
        (silent.state, silent.up_to_date, silent.availability),
        (None, None, Availability::Unsupported)
    );
    let mut again = ok;
    again.judge(None);
    assert_eq!(again.up_to_date, None, "judged again against none");
}

/// The summary serializes to exactly `state`, `up_to_date` and `availability`: the open
/// route carries no commit, no refusal and no helper flag (mutant: the running commit or
/// the report's refusal added to the summary).
#[test]
fn the_summary_is_three_keys() {
    let mut s = UpdateSummary::new(Some(&report("0123456789ab")), true, true);
    s.judge(Some("0123456789abcdef0123456789abcdef01234567"));
    assert_eq!(
        serde_json::to_value(&s).expect("json"),
        serde_json::json!({"state": "idle", "up_to_date": true, "availability": "ok"})
    );
    let silent = UpdateSummary::new(None, false, false);
    assert_eq!(
        serde_json::to_value(&silent).expect("json"),
        serde_json::json!({"state": null, "up_to_date": null, "availability": "unsupported"})
    );
}

/// The detail keeps what a host last said even when it is no longer `ok` (its commits
/// and refusal are the evidence an operator wants from a host that went quiet) while
/// its `state` is `null`; a host that never reported has `null` for every reported
/// field.
#[test]
fn the_detail_keeps_the_last_report_of_a_quiet_host_but_not_its_state() {
    let said = UpdateReport::new(
        UpdateStatus {
            running_commit: "0123456789ab".to_owned(),
            installed_commit: "i".to_owned(),
            staged_commit: "s".to_owned(),
            refusal: "no helper".to_owned(),
            offered_generation: 4,
            helper_present: true,
            ..status(State::Refused)
        },
        99,
    );
    let mut gone = HostUpdate::new("n1".to_owned(), false, true, Some(said));
    gone.judge(Some("0123456789abcdef0123456789abcdef01234567"));
    assert_eq!(
        serde_json::to_value(&gone).expect("json"),
        serde_json::json!({
            "node_id": "n1",
            "connected": false,
            "availability": "unavailable",
            "state": null,
            "up_to_date": null,
            "desired_commit": "0123456789abcdef0123456789abcdef01234567",
            "running_commit": "0123456789ab",
            "installed_commit": "i",
            "staged_commit": "s",
            "refusal": "no helper",
            "offered_generation": 4,
            "helper_present": true,
            "reported_at_unix_ms": 99,
        })
    );
    let never = HostUpdate::new("n2".to_owned(), true, false, None);
    let value = serde_json::to_value(&never).expect("json");
    assert_eq!(value["availability"], "unsupported");
    for key in [
        "state",
        "up_to_date",
        "desired_commit",
        "running_commit",
        "installed_commit",
        "staged_commit",
        "refusal",
        "offered_generation",
        "helper_present",
        "reported_at_unix_ms",
    ] {
        assert_eq!(value[key], serde_json::Value::Null, "{key}");
    }
}
