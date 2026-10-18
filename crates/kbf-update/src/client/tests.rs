//! The client's behaviour, through its handle: offers, status, starting. Orders are in
//! `order_tests`. Each test names the defect it catches.

use std::fs;
use std::path::Path;
use std::time::Duration;

use super::harness::{PROMPT, Rig, status_json};
use super::*;
use crate::fixtures::{PLATFORM, commit, offer_of, scratch};
use crate::types::Staged;

fn why_of(decision: &Decision) -> String {
    decision.refusal().unwrap_or_default().to_owned()
}

fn no_offer(commit: &str) -> String {
    format!("no offer for commit {commit} is held; not activating")
}

/// Catches an offer taken at a generation below one already seen (a stale offer would
/// replace the newer one the node may be ordered to activate), and the newest not
/// remembered. The same generation again is remembered (a restart loses the memory) but
/// changes nothing else.
///
/// Mutant planted: the stale check `<` made `<=` (the repeat is then not remembered after
/// a restart, and the activation is refused as having no offer).
#[tokio::test]
async fn an_older_generation_is_ignored_and_the_same_one_is_a_repeat() {
    let rig = Rig::new("generations");
    rig.offer(5);
    assert_eq!(rig.rx.borrow().offered_generation, 5);
    rig.offer(3);
    assert_eq!(
        rig.rx.borrow().offered_generation,
        5,
        "an older offer is ignored"
    );
    assert_eq!(
        why_of(&rig.ask(1, Verb::Activate, &commit(3))),
        no_offer(&commit(3))
    );
    rig.offer(5);
    assert_eq!(rig.ask(2, Verb::Activate, &commit(5)), Decision::Accepted);
}

/// Catches an offer that fails its checks being remembered, and a refusal that stays after
/// a good offer: the reason is in the status, and a good offer clears it.
#[tokio::test]
async fn a_refused_offer_is_reported_and_a_good_one_clears_it() {
    let rig = Rig::new("refused");
    let mut bad = offer_of(1);
    bad.platform = "darwin-arm64".into();
    rig.handle.offer(bad);
    let status = rig.rx.borrow().clone();
    assert_eq!(status.state, State::Refused);
    assert!(status.refusal.contains("platform"), "{}", status.refusal);
    assert_eq!(status.offered_generation, 1);
    assert_eq!(
        why_of(&rig.ask(1, Verb::Activate, &commit(1))),
        no_offer(&commit(1))
    );
    rig.offer(2);
    let status = rig.rx.borrow().clone();
    assert_eq!(status.state, State::Idle);
    assert_eq!(status.refusal, "");
}

/// Catches the offers kept without bound (a hostile frontend filling memory) or the wrong
/// ones dropped: the newest four generations are held and the oldest go.
#[tokio::test]
async fn only_the_newest_offers_are_held() {
    let rig = Rig::new("keep");
    for generation in 1..=6 {
        rig.offer(generation);
    }
    assert_eq!(
        why_of(&rig.ask(1, Verb::Activate, &commit(1))),
        no_offer(&commit(1))
    );
    assert_eq!(
        why_of(&rig.ask(2, Verb::Activate, &commit(2))),
        no_offer(&commit(2))
    );
    assert_eq!(rig.ask(3, Verb::Activate, &commit(3)), Decision::Accepted);
    assert_eq!(rig.ask(4, Verb::Activate, &commit(6)), Decision::Accepted);
}

/// Catches the generation floor lost on a restart: an offer of an older generation after a
/// restart is still ignored, and the newest is remembered when sent again.
#[tokio::test]
async fn the_generation_survives_a_restart() {
    let rig = Rig::new("restart");
    rig.offer(8);
    let rig = rig.restart();
    assert_eq!(rig.rx.borrow().offered_generation, 8);
    rig.offer(4);
    assert_eq!(rig.rx.borrow().offered_generation, 8);
    rig.offer(8);
    assert_eq!(rig.ask(1, Verb::Activate, &commit(8)), Decision::Accepted);
}

// --- the helper's status file -------------------------------------------------------

/// Catches the state table: no file is MANUAL with no helper; garbage, a schema this
/// build does not read, an unknown state and a commit that is not hex are REFUSED with the
/// reason; each state a helper reports maps to its own; and STAGED needs the staged commit
/// to be the one offered.
#[tokio::test]
async fn the_helpers_status_maps_to_the_state() {
    let rig = Rig::bare("status");
    let status = rig.rx.borrow().clone();
    assert_eq!(status.state, State::Manual);
    assert!(!status.helper_present);
    assert_eq!(status.running_commit, "f".repeat(12));

    let refused = |rig: &Rig, text: &str, contains: &str| {
        rig.write_status(text);
        let status = rig.rx.borrow().clone();
        assert_eq!(status.state, State::Refused, "{text}");
        assert!(status.refusal.contains(contains), "{}", status.refusal);
        assert!(status.helper_present, "{text}");
    };
    refused(&rig, "{not json", "status.json");
    refused(
        &rig,
        &status_json("idle", "", "", 0).replace("\"schema\":2", "\"schema\":3"),
        "schema 3",
    );
    refused(&rig, &status_json("rebooting", "", "", 0), "unknown state");
    refused(&rig, &status_json("idle", "XYZ", "", 0), "installed_commit");

    // The offered commit is the first generation's; the helper stages another.
    rig.write_status(&status_json("idle", "", "", 0));
    rig.offer(1);
    let offered = commit(1);
    let (other, installed_commit) = ("9".repeat(40), "1".repeat(40));
    let table = [
        ("idle", "", "", State::Idle),
        ("fetching", "", "", State::Fetching),
        ("staged", "", offered.as_str(), State::Staged),
        ("staged", "", other.as_str(), State::Idle),
        ("staged", "", "", State::Idle),
        (
            "activated_unconfirmed",
            installed_commit.as_str(),
            "",
            State::ActivatedUnconfirmed,
        ),
        ("confirmed", "", "", State::Confirmed),
        ("reverted", "", "", State::Reverted),
        ("refused", "", "", State::Refused),
    ];
    for (helper, installed, staged, expected) in table {
        rig.write_status(&status_json(helper, installed, staged, 4));
        let status = rig.rx.borrow().clone();
        assert_eq!(status.state, expected, "{helper} {staged}");
        assert!(status.helper_present);
        assert_eq!(
            status.staged,
            Some(Staged {
                protocol: 2,
                min_protocol: 1,
                state_schema: 3
            })
        );
        assert_eq!(
            status.installed_commit.as_deref(),
            (!installed.is_empty()).then_some(installed)
        );
        assert_eq!(
            status.staged_commit.as_deref(),
            (!staged.is_empty()).then_some(staged)
        );
        let why = if expected == State::Refused {
            "why not"
        } else {
            ""
        };
        assert_eq!(status.refusal, why, "{helper}");
    }

    // A status file that is not a file is unreadable, not missing.
    fs::remove_file(rig.dir.join("status.json")).expect("remove");
    fs::create_dir(rig.dir.join("status.json")).expect("dir");
    rig.handle.refresh();
    let status = rig.rx.borrow().clone();
    assert_eq!(status.state, State::Refused);
    assert!(status.refusal.contains("cannot read"), "{}", status.refusal);
}

/// Catches the poll loop not running: a status file written while the worker runs is
/// seen without anyone asking.
#[tokio::test]
async fn the_worker_watches_the_status_file() {
    let dir = scratch("poll");
    let mut config = Rig::config(&dir);
    config.poll_every = Duration::from_millis(5);
    let (handle, worker) = start(config).expect("start");
    let task = tokio::spawn(worker.run());
    let mut rx = handle.status();
    fs::write(dir.join("status.json"), status_json("confirmed", "", "", 0)).expect("write");
    tokio::time::timeout(PROMPT, rx.wait_for(|s| s.state == State::Confirmed))
        .await
        .expect("seen in time")
        .expect("runs");
    task.abort();
}

// --- starting ---

/// Catches a client that starts on a state file it cannot read (a guessed zero would
/// let an old offer or order in again) or in a directory it cannot make.
#[tokio::test]
async fn a_client_does_not_start_on_state_it_cannot_read() {
    let why = |dir: &Path| {
        let config = Rig::config(dir);
        start(config)
            .err()
            .map(|e| e.to_string())
            .unwrap_or_default()
    };

    let dir = scratch("start-garbage");
    fs::create_dir_all(dir.join("orders")).expect("spool");
    fs::write(dir.join("orders/client.json"), "not json").expect("write");
    assert!(why(&dir).contains("not the client's state file"));

    let dir = scratch("start-unreadable");
    fs::create_dir_all(dir.join("orders/client.json")).expect("dir");
    assert!(why(&dir).contains("cannot read"));

    let dir = scratch("start-blocked");
    fs::write(dir.join("file"), "").expect("write");
    assert!(why(&dir.join("file").join("under")).contains("orders"));

    // And a good one starts.
    assert_eq!(why(&scratch("start-fine")), "");
}

/// Catches the spool path: by default `orders` under the installer's directory, and made
/// private (0700) because only the service user may write records.
#[tokio::test]
async fn the_spool_is_orders_under_the_directory_and_private() {
    use std::os::unix::fs::PermissionsExt as _;
    let dir = scratch("defaults");
    let config = Config::new(dir.clone(), PLATFORM.to_owned(), "a".repeat(12));
    assert_eq!(config.orders_dir, dir.join("orders"));
    let _ = start(config).expect("start");
    let mode = fs::metadata(dir.join("orders"))
        .expect("spool")
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o700);
}
