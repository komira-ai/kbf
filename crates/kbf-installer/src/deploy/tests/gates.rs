//! The deploy gates (kbf#949 part 1) against a canned farm: `/metrics` and `/v1/invocations`
//! say what the test scripts. Because the farm decides what each gate sees, these tests prove
//! the gate logic (what is measured, when, what is red and what the red says) and not that a
//! dropped connection moves `kbf_frontend_relay_untraced` on a real farm. That is the
//! leased-node acceptance run. The fake answers a call it was not taught with 404 and notes
//! it, and every test here asserts the note is empty.
//!
//! Farm time: the preflight is at 0; the nodes confirm at 100 (`a-store`) and 200 (`b-mac`);
//! the poll is 5 s.

use std::fs;
use std::path::PathBuf;

use kbf_gate::{ResultDoc, Status, Verdict};

use super::*;
use crate::deploy::gates::name;

fn scratch(name: &str) -> PathBuf {
    let (_, dir) = crate::testkit::host_root(&format!("deploy-gates-{name}"));
    dir.join("result.json")
}

/// The farm's `/metrics` is `healthy()` with `change` applied at farm time `t`.
fn script(f: &mut Farm, change: impl Fn(u64, &mut Gauges) + 'static) {
    f.metrics = Box::new(move |t| {
        let mut g = healthy();
        change(t, &mut g);
        Some(g.text())
    });
}

/// Run an applied deploy that writes its document; the summary and the document.
fn run_gated(f: &Farm, test: &str) -> (Summary, ResultDoc) {
    run_gated_with(f, test, |_| {})
}

fn run_gated_with(f: &Farm, test: &str, tweak: impl FnOnce(&mut Options)) -> (Summary, ResultDoc) {
    let path = scratch(test);
    let mut o = opts(true);
    o.result = Some(path.clone());
    tweak(&mut o);
    let s = run_with(f, &o, SmokeResult::Pass, None);
    assert!(
        f.unknown.borrow().is_empty(),
        "calls the fake does not know: {:?}",
        f.unknown.borrow()
    );
    let doc = ResultDoc::from_bytes(&fs::read(&path).expect("the document is there"))
        .expect("the document reads");
    (s, doc)
}

fn gate<'a>(doc: &'a ResultDoc, name: &str) -> &'a kbf_gate::GateResult {
    doc.gates.iter().find(|g| g.name == name).expect(name)
}

/// The deploy ended red at a gate: the exit, the reason, and that the message names the
/// gate, the value and the host step.
#[track_caller]
fn red_at(s: &Summary, gate: &str, value: &str, step: &str) {
    assert_eq!((s.exit, s.reason.as_str()), (1, "gate_red"), "{}", s.line());
    for part in [gate, value, step] {
        assert!(
            s.detail.contains(part),
            "`{part}` missing from: {}",
            s.detail
        );
    }
}

// ---- the clean run -----------------------------------------------------------------

/// Proves a deploy on a healthy farm ends green: all seven gate rows are in the document,
/// green, and the document is `Green`, finished, for the commit. Catches a deploy that
/// leaves a gate out of the document, one that reads a healthy scrape as red (a mutant that
/// compares `kbf_cache_blob_losses_total` absolutely goes red here: the farm has lost 3
/// blobs before the deploy), and a call the fake was not taught.
///
/// Mutants planted: the loss counter compared as an absolute value.
#[test]
fn a_healthy_farm_gives_a_green_document_with_every_gate() {
    let f = farm(two_nodes());
    let (s, doc) = run_gated(&f, "clean");
    assert_eq!((s.exit, s.reason.as_str()), (0, ""), "{}", s.line());
    assert_eq!(doc.status, Status::Green);
    assert!(doc.passed() && doc.finished.is_some());
    assert_eq!(doc.commit.as_str(), TARGET);
    let names: Vec<_> = doc.gates.iter().map(|g| g.name.as_str()).collect();
    assert_eq!(names, name::ALL);
    assert!(
        doc.gates.iter().all(|g| g.verdict == Verdict::Green),
        "{doc:?}"
    );
    assert_eq!(doc.desired_generation, Some(8));
    assert!(doc.deploy.is_some_and(|w| w.end.is_some()));
}

/// Proves a plan (no `--apply`) reads no metrics and writes no document. Catches a plan that
/// scrapes the farm or leaves a file.
#[test]
fn a_plan_neither_scrapes_nor_writes_a_document() {
    let f = farm(two_nodes());
    let path = scratch("plan");
    let mut o = opts(false);
    o.result = Some(path.clone());
    let s = run_with(&f, &o, SmokeResult::Pass, None);
    assert_eq!(s.exit, 0, "{}", s.line());
    assert!(
        f.observed_log.borrow().is_empty(),
        "a plan reads no gate: {:?}",
        f.observed_log.borrow()
    );
    assert!(!path.exists());
}

/// Proves the document's set hash is the caller's when it gave one and otherwise the hash of
/// what the store lists (never the zero placeholder it starts with). Catches a document that
/// keeps the placeholder, or that lets the derived hash win over the caller's.
#[test]
fn the_set_hash_is_the_callers_else_derived_from_the_store() {
    let f = farm(two_nodes());
    let (_, derived) = run_gated(&f, "derived");
    assert_ne!(derived.set_hash.as_str(), "0".repeat(64));
    let given = kbf_gate::SetHash::of_sums(b"SHA256SUMS bytes");
    let f = farm(two_nodes());
    let (_, doc) = run_gated_with(&f, "given", |o| o.set_hash = Some(given.clone()));
    assert_eq!(doc.set_hash, given);
    assert_ne!(doc.set_hash, derived.set_hash);
}

// ---- gate B ------------------------------------------------------------------------

/// Proves an untraced relay at any scrape ends the deploy red, even when the gauge is back
/// to 0 by the next scrape and by the end: the deploy names gate B, the value 1 and the host
/// step (the node being waited for), and the document row is red with measured 1. Catches a
/// check of the last scrape only (the gauge is 0 again by then), and a message that does not
/// say where.
///
/// Mutants planted: B checked only at the `post` step (the last scrape).
#[test]
fn an_untraced_relay_mid_deploy_is_red_naming_gate_value_and_step() {
    let mut f = farm(two_nodes());
    script(&mut f, |t, g| {
        if (40..60).contains(&t) {
            g.relay_untraced = Some(1.0);
        }
    });
    let (s, doc) = run_gated(&f, "relay");
    red_at(
        &s,
        name::RELAY_UNTRACED,
        ": 1,",
        "nodes: waiting for a-store",
    );
    let row = gate(&doc, name::RELAY_UNTRACED);
    assert_eq!((row.verdict, row.measured), (Verdict::Red, Some(1.0)));
    assert_eq!(doc.status, Status::Red);
    assert!(doc.finished.is_some());
}

/// Proves a scrape that cannot be read, and a scrape without the series, are red and not 0.
/// Catches an absent gauge mapped to zero, for B, for D and for an unreadable scrape.
///
/// Mutants planted: an unreadable scrape read as zeros; a missing series read as 0.
#[test]
fn a_scrape_that_fails_or_lacks_the_series_is_red() {
    let mut f = farm(two_nodes());
    f.metrics = Box::new(|t| (t < 50).then(|| healthy().text()));
    let (s, doc) = run_gated(&f, "unreadable");
    red_at(
        &s,
        name::RELAY_UNTRACED,
        "absent",
        "nodes: waiting for a-store",
    );
    assert!(s.detail.contains("status 503"), "{}", s.detail);
    assert_eq!(gate(&doc, name::RELAY_UNTRACED).measured, None);

    for (series, gate_name) in [
        ("relay", name::RELAY_UNTRACED),
        ("lost", name::LOST_BLOBS),
        ("losses", name::BLOB_LOSSES),
    ] {
        let mut f = farm(two_nodes());
        script(&mut f, move |t, g| {
            if t >= 50 {
                match series {
                    "relay" => g.relay_untraced = None,
                    "lost" => g.lost_blobs = None,
                    _ => g.losses_total = None,
                }
            }
        });
        let (s, _) = run_gated(&f, &format!("missing-{series}"));
        red_at(&s, gate_name, "absent", "nodes: waiting for a-store");
    }
}

// ---- gate D ------------------------------------------------------------------------

/// Proves a lost blob at any scrape is red naming gate D, the value and the step. Catches a
/// lost-blob gauge that is not read, or read only at the end.
///
/// Mutants planted: D's gauge read only at the `post` step.
#[test]
fn a_lost_blob_mid_deploy_is_red() {
    let mut f = farm(two_nodes());
    script(&mut f, |t, g| {
        if (110..130).contains(&t) {
            g.lost_blobs = Some(1.0);
        }
    });
    let (s, doc) = run_gated(&f, "lost");
    red_at(&s, name::LOST_BLOBS, ": 1,", "nodes: waiting for b-mac");
    assert_eq!(gate(&doc, name::LOST_BLOBS).measured, Some(1.0));
}

/// Proves a loss counter that grew by 1 since the first scrape is red naming gate D's delta
/// and the value 1, though the counter's absolute value (4) is as good as any other. Catches
/// a check that ignores the delta.
///
/// Mutants planted: the delta replaced by 0 (the counter ignored).
#[test]
fn a_blob_loss_since_the_start_is_red_by_delta() {
    let mut f = farm(two_nodes());
    script(&mut f, |t, g| {
        if t >= 50 {
            g.losses_total = Some(4.0);
        }
    });
    let (s, doc) = run_gated(&f, "delta");
    red_at(&s, name::BLOB_LOSSES, ": 1,", "nodes: waiting for a-store");
    assert_eq!(gate(&doc, name::BLOB_LOSSES).measured, Some(1.0));
}

/// Proves a loss counter that went backwards (a restart reset it) is red and not a delta of
/// 0 or a pass: nothing can be said about the losses across the deploy. Catches a delta
/// computed with a saturating subtraction.
///
/// Mutants planted: the `now >= base` guard removed (the delta saturates to 0).
#[test]
fn a_loss_counter_that_went_backwards_is_red() {
    let mut f = farm(two_nodes());
    script(&mut f, |t, g| {
        if t >= 50 {
            g.losses_total = Some(0.0);
        }
    });
    let (s, _) = run_gated(&f, "reset");
    red_at(
        &s,
        name::BLOB_LOSSES,
        "absent",
        "nodes: waiting for a-store",
    );
}

// ---- gate E ------------------------------------------------------------------------

/// Proves the preflight refuses a farm whose storage is not fully replicated, and one that
/// does not say (the series is absent): exit 3 before anything is set, naming gate E, the
/// value and `preflight`. Catches an absent series passed as healthy and a preflight that
/// does not look.
///
/// Mutants planted: an absent under-replicated reading passed as 0.
#[test]
fn the_preflight_refuses_storage_that_is_not_healthy_or_not_known() {
    for (under, value, reason) in [
        (Some(2.0), ": 2,", "gate_red"),
        (None, "no under-replicated series", "gate_unjudgeable"),
    ] {
        let mut f = farm(two_nodes());
        script(&mut f, move |_, g| g.under_replicated = under);
        let (s, doc) = run_gated(&f, &format!("preflight-{}", under.is_some()));
        assert_eq!((s.exit, s.reason.as_str()), (3, reason), "{}", s.line());
        for part in [name::UNDER_REPLICATED, value, "preflight"] {
            assert!(
                s.detail.contains(part),
                "`{part}` missing from {}",
                s.detail
            );
        }
        assert_eq!(f.requests("POST /v1/update/desired"), 0, "nothing was set");
        assert_eq!(gate(&doc, name::UNDER_REPLICATED).verdict, Verdict::Red);
        assert_eq!(doc.status, Status::Red);
    }
}

/// Proves an under-replicated count that returns to 0 inside the settle window after a node
/// step is green, and that the deploy does not go on to the frontend before it has returned.
/// Catches a gate that looks only before the deploy, and a wait that is not made.
///
/// Mutants planted: the wait for repair after the last node skipped.
#[test]
fn storage_that_is_repaired_inside_the_settle_window_is_green() {
    let mut f = farm(two_nodes());
    script(&mut f, |t, g| {
        if (100..130).contains(&t) || (200..240).contains(&t) {
            g.under_replicated = Some(3.0);
        }
    });
    let (s, doc) = run_gated(&f, "repaired");
    assert_eq!((s.exit, s.reason.as_str()), (0, ""), "{}", s.line());
    assert_eq!(gate(&doc, name::UNDER_REPLICATED).verdict, Verdict::Green);
    assert!(s.nodes.iter().all(|n| n.state == "confirmed"));
    // The deploy waited out the second repair (which ends at 240) before it went on to the
    // frontend; a deploy that does not wait is done by about 215.
    assert!(f.now.get() >= 240, "ended at {}", f.now.get());
}

/// Proves the deploy ends red when the next node's step has begun (it shows
/// `activated_unconfirmed`) while the node before it is still under-replicated, naming gate
/// E, the count and which node began. The test fails if the next node step is allowed to
/// start before repair. Catches a gate that checks repair only at the end of the settle
/// window.
///
/// Mutants planted: the "next node began" check removed.
#[test]
fn the_next_node_step_may_not_begin_before_the_repair() {
    let mut nodes = two_nodes();
    nodes[1].activated_at = Some(150);
    nodes[1].fate = Fate::Confirm(400);
    let mut f = farm(nodes);
    script(&mut f, |t, g| {
        if (100..300).contains(&t) {
            g.under_replicated = Some(2.0);
        }
    });
    let (s, _) = run_gated(&f, "next-began");
    red_at(
        &s,
        name::UNDER_REPLICATED,
        ": 2,",
        "b-mac began its step before a-store was repaired",
    );
}

/// Proves a node that is seen to confirm while the node before it is still not repaired is
/// red too (its step ran between two looks). Catches a gate that only watches for the
/// `activated_unconfirmed` state.
///
/// Mutants planted: the "confirmed while unrepaired" check removed.
#[test]
fn a_node_that_confirms_before_the_repair_is_red() {
    let mut f = farm(two_nodes());
    script(&mut f, |t, g| {
        if (100..260).contains(&t) {
            g.under_replicated = Some(2.0);
        }
    });
    let (s, _) = run_gated(&f, "confirm-before");
    red_at(
        &s,
        name::UNDER_REPLICATED,
        ": 2,",
        "b-mac confirmed before a-store was repaired",
    );
}

/// Proves storage still under-replicated when the settle window ends is red naming the
/// window, and that the window is `--settle-timeout-s` and not the node timeout. One node,
/// so no other check can fire. Catches a missing window and a window read from the wrong
/// option.
///
/// Mutants planted: the settle window replaced by the node timeout.
#[test]
fn storage_not_repaired_when_the_settle_window_ends_is_red() {
    let mut f = farm(vec![node("a-store", "linux-x86_64", Fate::Confirm(100))]);
    f.complete = vec!["linux-x86_64"];
    script(&mut f, |t, g| {
        if t >= 100 {
            g.under_replicated = Some(1.0);
        }
    });
    let (s, _) = run_gated_with(&f, "settle", |o| {
        o.settle = Duration::from_secs(60);
        o.node_timeout = Duration::from_secs(1800);
    });
    red_at(
        &s,
        name::UNDER_REPLICATED,
        ": 1,",
        "not repaired within the 60s settle window",
    );
    // The window is 60 s after the node at 100, not the 1800 s of the node timeout.
    assert!(f.now.get() <= 170, "ended at {}", f.now.get());
}

/// Proves a series that goes missing after a node's step (the members stopped reporting) is
/// not a repair: the gate waits out the window and is red naming the absence. Catches an absent
/// reading taken as 0 once the farm is running.
///
/// Mutants planted: an absent under-replicated reading passed as 0.
#[test]
fn storage_that_stops_reporting_after_a_node_step_is_red() {
    let mut f = farm(vec![node("a-store", "linux-x86_64", Fate::Confirm(100))]);
    f.complete = vec!["linux-x86_64"];
    script(&mut f, |t, g| {
        if t >= 100 {
            g.under_replicated = None;
        }
    });
    let (s, _) = run_gated_with(&f, "stops-reporting", |o| {
        o.settle = Duration::from_secs(60)
    });
    red_at(
        &s,
        name::UNDER_REPLICATED,
        "absent",
        "not repaired within the 60s settle window",
    );
}

// ---- gate A and C ------------------------------------------------------------------

fn build(id: &'static str, from: u64, to: Option<u64>, classes: &[&'static str]) -> Build {
    Build {
        id,
        from,
        to,
        classes: classes.to_vec(),
        capped: false,
    }
}

/// Proves a build that started and ended between two snapshots, and failed `infra`, is found
/// by the `since`/`until` listing over the whole deploy and fails gate C naming the build and
/// the class. No snapshot can see it: it ran from 22 to 27, snapshots are at the start, each
/// node's confirmation, the frontend and the end. Catches a deploy that records ids from
/// snapshots only.
///
/// Mutants planted: the window listing skipped.
#[test]
fn a_build_between_two_snapshots_is_found_by_the_window_listing() {
    let mut f = farm(two_nodes());
    f.builds = vec![build("b-short", 22, Some(27), &["infra"])];
    let (s, doc) = run_gated(&f, "between");
    red_at(&s, name::FARM_FAILURES, ": 1,", "end");
    assert!(s.detail.contains("b-short: infra"), "{}", s.detail);
    assert_eq!(gate(&doc, name::FARM_FAILURES).measured, Some(1.0));
}

/// Proves a build that began before the deploy and is still running is found by the
/// snapshots (the window starts at the first scrape and does not hold it). Catches a deploy
/// that records only the window.
///
/// Mutants planted: the `state=open` snapshot skipped.
#[test]
fn a_build_running_at_the_start_is_found_by_the_snapshot() {
    let mut f = farm(two_nodes());
    f.now.set(1000);
    f.builds = vec![build("b-long", 500, None, &["infra"])];
    let (s, _) = run_gated(&f, "running");
    red_at(&s, name::FARM_FAILURES, ": 1,", "b-long: infra");
}

/// Proves each farm class fails gate C and the user's classes do not: `infra`, `lost`,
/// `lost_fenced` and `lost_daemon_restarted` are red; `failed`, `timed_out` and `memory_killed`
/// are not (they are the build's own). Catches a filter on `infra` alone and one that fails
/// every failure.
///
/// Mutants planted: the filter reduced to `infra`; the filter widened to every failure.
#[test]
fn only_the_farms_failure_classes_fail_gate_c() {
    for class in ["infra", "lost", "lost_fenced", "lost_daemon_restarted"] {
        let mut f = farm(two_nodes());
        f.builds = vec![build("b1", 22, Some(27), &[class])];
        let (s, _) = run_gated(&f, class);
        red_at(&s, name::FARM_FAILURES, ": 1,", &format!("b1: {class}"));
    }
    let mut f = farm(two_nodes());
    f.builds = vec![build(
        "b1",
        22,
        Some(27),
        &["failed", "timed_out", "memory_killed"],
    )];
    let (s, doc) = run_gated(&f, "own");
    assert_eq!((s.exit, s.reason.as_str()), (0, ""), "{}", s.line());
    assert_eq!(gate(&doc, name::FARM_FAILURES).measured, Some(0.0));
}

/// Proves listings and action pages are read to their last page: the failing build is the
/// second of the listing and its failure is on the second page of its actions. Catches a
/// reader that stops after one page.
///
/// Mutants planted: the listing cut after its first page; the actions cut after theirs.
#[test]
fn every_page_is_read() {
    let mut f = farm(two_nodes());
    f.page = 1;
    f.builds = vec![
        build("b-first", 22, Some(27), &[]),
        build("b-second", 23, Some(27), &["failed", "lost_fenced"]),
    ];
    let (s, _) = run_gated(&f, "pages");
    red_at(&s, name::FARM_FAILURES, ": 1,", "b-second: lost_fenced");
}

/// Proves a listing that cannot be read is red for gate A, at the preflight (exit 3, before
/// anything is set) and later in the deploy, and a recorded build whose actions cannot be read
/// is red for gate C: nothing was proved about it. Catches an unreadable listing counted as
/// "no builds" and an unreadable build counted as "no failures".
///
/// Mutants planted: a failed listing read as an empty one; a failed build read as clean.
#[test]
fn an_unreadable_listing_or_build_is_red() {
    let mut f = farm(two_nodes());
    f.broken.push(Broken {
        call: "GET /v1/invocations?state=open",
        how: Breaks::Status(404),
        after_desired: false,
    });
    let (s, _) = run_gated(&f, "off");
    assert_eq!((s.exit, s.reason.as_str()), (3, "gate_red"), "{}", s.line());
    for part in [name::INFLIGHT, "absent", "preflight"] {
        assert!(
            s.detail.contains(part),
            "`{part}` missing from {}",
            s.detail
        );
    }

    let mut f = farm(two_nodes());
    f.builds = vec![build("b-gone", 22, Some(27), &[])];
    f.broken.push(Broken {
        call: "GET /v1/invocations/b-gone/actions",
        how: Breaks::Status(404),
        after_desired: false,
    });
    let (s, doc) = run_gated(&f, "gone");
    red_at(&s, name::FARM_FAILURES, "absent", "b-gone");
    assert_eq!(gate(&doc, name::FARM_FAILURES).measured, None);

    // A listed id that could not go in a path is not followed.
    let mut f = farm(two_nodes());
    f.builds = vec![build("../x", 22, Some(27), &[])];
    let (s, _) = run_gated(&f, "bad-id");
    red_at(&s, name::INFLIGHT, "absent", "end");
}

// ---- gate F ------------------------------------------------------------------------

/// Proves a node that reverts by its own timer is a red row for gate F naming the node, with
/// measured 1, besides the deploy's own `node_not_confirmed`; and that a deploy with no
/// revert has F green. Catches an F row that does not record the revert.
///
/// Mutants planted: the revert recorded as 0.
#[test]
fn a_node_that_reverts_is_gate_f_red() {
    let mut f = farm(vec![
        node("a-store", "linux-x86_64", Fate::Confirm(100)),
        node("b-mac", "darwin-arm64", Fate::Revert(150)),
    ]);
    f.complete = vec!["linux-x86_64", "darwin-arm64"];
    let (s, doc) = run_gated(&f, "node-revert");
    assert_eq!(
        (s.exit, s.reason.as_str()),
        (1, "node_not_confirmed"),
        "{}",
        s.line()
    );
    let row = gate(&doc, name::AUTO_REVERT);
    assert_eq!((row.verdict, row.measured), (Verdict::Red, Some(1.0)));
    assert_eq!(doc.status, Status::Red);
    assert!(
        doc.note
            .as_deref()
            .is_some_and(|n| n.contains("node_not_confirmed"))
    );
}

/// Proves a reverted frontend is a red row for gate F as well. Catches F covering nodes only.
///
/// Mutants planted: the frontend revert not recorded.
#[test]
fn a_frontend_that_reverts_is_gate_f_red() {
    let mut f = farm(two_nodes());
    f.frontend_fate = FrontendFate::Revert;
    let (s, doc) = run_gated(&f, "frontend-revert");
    assert_eq!(
        (s.exit, s.reason.as_str()),
        (1, "frontend_reverted"),
        "{}",
        s.line()
    );
    let row = gate(&doc, name::AUTO_REVERT);
    assert_eq!((row.verdict, row.measured), (Verdict::Red, Some(1.0)));
}

// ---- the document ------------------------------------------------------------------

/// Proves a failed deploy still writes `FINISHED`: the document is red, finished, and says
/// why, whatever step failed (an upload, the preflight, a node that never confirms). Catches
/// a failure path that returns without sealing the document.
///
/// Mutants planted: the seal skipped for a failure; `std::process::exit` called on a failure
/// before the seal (the test binary dies, which is red).
#[test]
fn a_failed_deploy_still_writes_finished() {
    let mut f = farm(vec![node("a-store", "linux-x86_64", Fate::Never)]);
    f.complete = vec!["linux-x86_64"];
    let (s, doc) = run_gated_with(&f, "never", |o| o.node_timeout = Duration::from_secs(60));
    assert_eq!(s.reason, "node_not_confirmed");
    assert_eq!(doc.status, Status::Red);
    assert!(doc.finished.is_some() && !doc.passed());
    assert!(
        doc.note
            .as_deref()
            .is_some_and(|n| n.contains("node_not_confirmed")),
        "{doc:?}"
    );

    let mut f = farm(two_nodes());
    f.broken.push(Broken {
        call: "GET /v1/releases",
        how: Breaks::Io,
        after_desired: false,
    });
    let (s, doc) = run_gated(&f, "early");
    assert_eq!(s.exit, 1);
    assert_eq!(doc.status, Status::Red);
    assert!(doc.finished.is_some());
}

/// Proves the same through the command line: `cli::run` returns after the document is sealed,
/// so `main`'s `process::exit` cannot leave it `RUNNING`. Catches a command line that exits,
/// or returns, before the document is finished.
///
/// Mutants planted: as above.
#[test]
fn the_command_line_leaves_a_finished_document_on_an_api_failure() {
    use crate::deploy::cli::{self, FrontendConfig, Host, current_uid};
    use std::os::unix::fs::PermissionsExt;
    let (_, dir) = crate::testkit::host_root("deploy-gates-cli");
    let token = dir.join("token");
    fs::write(&token, "0123456789abcdef0123456789abcdef0123456789abcdef\n").unwrap();
    fs::set_permissions(&token, fs::Permissions::from_mode(0o600)).unwrap();
    let (result, lock) = (dir.join("result.json"), dir.join("lock"));
    let args: Vec<String> = [
        "--commit",
        TARGET,
        "--api",
        "127.0.0.1:1",
        "--token-file",
        token.to_str().unwrap(),
        "--lock",
        lock.to_str().unwrap(),
        "--result",
        result.to_str().unwrap(),
        "--apply",
        "--observe",
        "127.0.0.1:2",
    ]
    .map(str::to_owned)
    .to_vec();
    let host = Host {
        root: true,
        config_owner: current_uid(),
        frontend: FrontendConfig {
            toml: dir.join("none.toml"),
            units: Vec::new(),
        },
    };
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let code = cli::run(
        &args,
        &mut std::io::empty(),
        &mut out,
        &mut err,
        false,
        &host,
    );
    assert_eq!(code, 1, "{}", String::from_utf8_lossy(&err));
    let doc = ResultDoc::from_bytes(&fs::read(&result).unwrap()).unwrap();
    assert_eq!(doc.status, Status::Red);
    assert!(doc.finished.is_some());
}

/// Proves a deploy whose smoke did not run (exit 5) leaves a document that is not a pass
/// (`Extended`) though every gate is green, and that an ok deploy is `Green`. Catches a
/// document that calls `smoke_skipped` green.
///
/// Mutants planted: exit 5 sealed through `finish`.
#[test]
fn a_skipped_smoke_is_not_a_green_document() {
    let f = farm(two_nodes());
    let path = scratch("skipped");
    let mut o = opts(true);
    o.result = Some(path.clone());
    let s = run_with(&f, &o, SmokeResult::Skipped, None);
    assert_eq!((s.exit, s.reason.as_str()), (5, "smoke_skipped"));
    let doc = ResultDoc::from_bytes(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(doc.status, Status::Extended);
    assert!(!doc.passed());
    assert!(doc.gates.iter().all(|g| g.verdict == Verdict::Green));
}

/// Proves a document that cannot be written ends the deploy before the first request.
/// Catches a deploy that goes on without a verdict it can record.
///
/// Mutants planted: the failed start ignored.
#[test]
fn a_document_that_cannot_be_started_stops_the_deploy_before_it_begins() {
    let f = farm(two_nodes());
    let mut o = opts(true);
    o.result = Some(PathBuf::from("/nonexistent-dir-for-the-test/result.json"));
    let s = run_with(&f, &o, SmokeResult::Pass, None);
    assert_eq!(
        (s.exit, s.reason.as_str()),
        (1, "result_unwritable"),
        "{}",
        s.line()
    );
    assert!(f.log.borrow().is_empty(), "{:?}", f.log.borrow());
}

// ---- where the gates read ----------------------------------------------------------

/// Proves the gates read only the observed frontend and the update calls go only to the
/// update API: every `/metrics` and `/v1/invocations` call is in the observed log, none is in
/// the API log, and the observed frontend was asked nothing else. Both fakes answer an unknown
/// call 404 (noted), so a gate that reads the wrong listener is red, not quietly served.
/// Catches gate reads sent to `--api` (the public proxy answers its own `/metrics`, and the
/// update API serves neither route).
///
/// Mutants planted: the gate reads sent to the update API.
#[test]
fn gate_reads_go_to_the_observed_frontend_and_update_calls_to_the_api() {
    let mut f = farm(two_nodes());
    f.builds = vec![build("b1", 22, Some(27), &[])];
    let (s, _) = run_gated(&f, "addressing");
    assert_eq!(s.exit, 0, "{}", s.line());
    let on_api = |l: &String| l.contains("/metrics") || l.contains("/v1/invocations");
    assert!(
        !f.log.borrow().iter().any(on_api),
        "gate reads reached --api: {:?}",
        f.log.borrow()
    );
    let seen = f.observed_log.borrow();
    assert!(seen.iter().any(|l| l == "GET /metrics"));
    assert!(
        seen.iter()
            .any(|l| l.starts_with("GET /v1/invocations?state=open"))
    );
    assert!(
        seen.iter().all(on_api),
        "the observed frontend was asked: {seen:?}"
    );
    assert!(f.requests("POST /v1/update/desired") == 1);
}

/// Proves a recorded build whose record stopped at its row bound is red for gate C (not
/// every attempt is visible), and an uncapped one is read. Catches a gate that passes on the
/// rows it can see.
///
/// Mutants planted: `rows_capped` not looked at.
#[test]
fn a_build_whose_rows_are_capped_is_red() {
    let mut f = farm(two_nodes());
    let mut capped = build("b-capped", 22, Some(27), &[]);
    capped.capped = true;
    f.builds = vec![capped];
    let (s, doc) = run_gated(&f, "capped");
    red_at(&s, name::FARM_FAILURES, "absent", "b-capped");
    assert!(s.detail.contains("capped"), "{}", s.detail);
    assert_eq!(gate(&doc, name::FARM_FAILURES).measured, None);
}

// ---- the frontend moves ------------------------------------------------------------

/// Run an applied deploy over the two children of the frontend proxy; the summary and the
/// document.
fn run_slots(f: &Farm, test: &str) -> (Summary, ResultDoc) {
    let path = scratch(test);
    let mut o = opts(true);
    o.result = Some(path.clone());
    let s = run_observing(f, &o, SmokeResult::Pass, None, &SlotObs(f));
    assert!(f.unknown.borrow().is_empty(), "{:?}", f.unknown.borrow());
    let doc = ResultDoc::from_bytes(&fs::read(&path).unwrap()).unwrap();
    (s, doc)
}

/// Proves the gates follow the frontend across its own handover. Slot A serves until the
/// deploy hands the frontend over, then slot A refuses connections and slot B serves: every
/// read before the handover is slot A's, every read after it slot B's, none is refused, the
/// deploy stays green, and the document says which child each stretch of readings came from.
/// Catches gates that go on reading the retired child (a refused scrape is red).
///
/// Mutants planted: no new resolution after the frontend step.
#[test]
fn the_gates_follow_the_serving_child_across_the_frontend_handover() {
    let f = farm(two_nodes());
    let (s, doc) = run_slots(&f, "handover");
    assert_eq!((s.exit, s.reason.as_str()), (0, ""), "{}", s.line());
    assert_eq!(doc.status, Status::Green);
    assert_eq!(doc.observed, ["preflight: slot a", "frontend: slot b"]);
    assert!(f.refused.borrow().is_empty(), "{:?}", f.refused.borrow());
    let reads = f.child_reads.borrow();
    assert!(!reads.is_empty());
    for (slot, after_post) in reads.iter() {
        let want = if *after_post {
            observe::Slot::B
        } else {
            observe::Slot::A
        };
        assert_eq!(*slot, want, "{reads:?}");
    }
    assert!(reads.iter().any(|(s, _)| *s == observe::Slot::B));
}

/// Proves a farm on which both children answer and the update API names none refuses to
/// start the gates, saying so, and reads neither. Catches a resolver that guesses.
///
/// Mutants planted: two live children resolved to the first (unit test in `observe.rs`).
#[test]
fn two_live_children_and_no_serving_slot_is_refused_at_the_preflight() {
    let mut f = farm(two_nodes());
    f.both_children = true;
    let (s, _) = run_slots(&f, "ambiguous");
    assert_eq!(
        (s.exit, s.reason.as_str()),
        (3, "observe_unresolved"),
        "{}",
        s.line()
    );
    assert!(
        s.detail.contains("both frontend slots answer"),
        "{}",
        s.detail
    );
    assert!(f.child_reads.borrow().is_empty());
}
