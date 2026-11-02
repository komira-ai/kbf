//! `activate` on a Linux host, continued: the locks inside the install lock, what the pin
//! protects, a pin that fails halfway, names that fit two releases, a listing past its bounds, a
//! record of takes cut short, the service manager's failures, and a run that was interrupted.

use std::fs;

use kbf_installer::state::{self, Locked, State};

use super::super::{A, Answer, Rig};
use super::{
    B, FORCE, How, LOCK_TAKES, PIN, Retained, VB, activated, agent_dir, core, current_and_previous,
    hold_as, id, keep, retain, target,
};
use crate::local::backend::Backend;

const C: &str = "cccccccccccccccccccccccccccccccccccccccc";
const VC: &str = "0.1.8+gcccccccccccc-1";

/// Another run of the installer that holds its state lock refuses the activate at once (busy,
/// naming the lock), before anything changes; a writer that holds `retain.json`'s lock refuses
/// an activate with `--pin` the same way, and one without `--pin` goes ahead.
///
/// Catches: an emergency command left hanging behind a lock it took no part in.
#[test]
fn the_locks_inside_the_install_lock_are_not_waited_for() {
    let rig = Rig::new("actm-inner");
    keep(&rig, B, Some(VB), 2);
    let (ab, _) = core(&rig);
    let held = Locked::acquire(&rig.layout).unwrap();
    let answer = ab.activate(&target(B), How::default());
    assert!(
        matches!(&answer, Answer::Refused(crate::local::answer::Code::Busy, d)
            if d.starts_with("kbf-installer's ") && d.ends_with("is held by another run")),
        "{answer:?}"
    );
    drop(held);
    assert_eq!(current_and_previous(&rig), (Some(A.to_owned()), None));
    assert!(rig.systemctl().is_empty());

    let retain_lock = kbf_retain::RetainLock::acquire(&retain::path(&rig.layout)).unwrap();
    let answer = ab.activate(&target(B), PIN);
    assert!(
        matches!(&answer, Answer::Refused(crate::local::answer::Code::Busy, d)
            if d.ends_with("is held by another writer")),
        "{answer:?}"
    );
    assert_eq!(current_and_previous(&rig), (Some(A.to_owned()), None));
    assert!(!rig.dir.join("agent/pin.json").exists());
    assert_eq!(
        ab.activate(&target(B), How::default()),
        activated(B, Some(VB), A)
    );
    drop(retain_lock);
}

/// A pinned release that is neither current nor previous, and older than the releases a prune
/// keeps, stays only because of the pin; an unpinned one of the same age is removed.
///
/// Catches: a pin that a prune ignores once the release has left the links.
#[test]
fn a_pinned_release_off_the_links_survives_a_prune_by_its_pin_alone() {
    let rig = Rig::new("actm-pinonly");
    keep(&rig, B, Some(VB), 2);
    keep(&rig, C, Some(VC), 3);
    let (ab, _) = core(&rig);
    assert!(matches!(
        ab.activate(&target(B), PIN),
        Answer::Activated { .. }
    ));
    assert!(matches!(
        ab.activate(&target(C), How::default()),
        Answer::Activated { .. }
    ));
    assert!(matches!(
        ab.activate(&target(A), How::default()),
        Answer::Activated { .. }
    ));
    // A runs, C is previous: B is on neither link. Four newer releases, then the prune.
    assert_eq!(
        current_and_previous(&rig),
        (Some(A.to_owned()), Some(C.to_owned()))
    );
    let old = format!("0{}", "e".repeat(39));
    keep(&rig, &old, None, 1);
    super::age(&rig, B, 1);
    for n in 1..5u64 {
        keep(&rig, &format!("{n}{}", "f".repeat(39)), None, 10 + n);
    }
    rig.backend.prune(2).unwrap();
    assert!(
        rig.releases().join(B).join("kbf").exists(),
        "the pin holds B"
    );
    assert!(
        !rig.releases().join(&old).exists(),
        "an unpinned one as old is removed"
    );
}

/// The hold file is written, then `retain.json`. With `retain.json` not writable the activate
/// swaps, writes the hold, and fails saying the release is not pinned; the hold names the
/// release, `retain.json` is as it was. Asked again once it is writable, both name it. An
/// activate without `--pin` leaves an earlier hold as it was. Units that do not come up and a
/// pin that cannot be written: the restart's failure is the answer, and the pin's is noted.
///
/// Catches: a partial pin that a retry does not complete; a hold rewritten by an activate that
/// did not ask for one.
#[test]
fn a_pin_that_fails_halfway_is_told_and_completed_when_asked_again() {
    let rig = Rig::new("actm-halfpin");
    keep(&rig, B, Some(VB), 2);
    let (ab, notes) = core(&rig);
    let hold = agent_dir(&rig).join("pin.json");
    fs::write(&hold, "{\"commit\":\"an earlier hold\"}").unwrap();
    assert_eq!(
        ab.activate(&target(B), How::default()),
        activated(B, Some(VB), A)
    );
    assert_eq!(
        fs::read_to_string(&hold).unwrap(),
        "{\"commit\":\"an earlier hold\"}",
        "no --pin, no new hold"
    );

    let file = retain::path(&rig.layout);
    // `retain.json` reads, and cannot be written: something in the way where it is written aside.
    let aside = file.with_extension("json.new");
    fs::create_dir_all(aside.join("full")).unwrap();
    let answer = ab.activate(&target(A), PIN);
    assert!(
        matches!(&answer, Answer::Failed(crate::local::answer::Code::StateUnavailable, d)
            if d.starts_with(&format!("{A} runs now, but it is not pinned: the pin: "))),
        "{answer:?}"
    );
    assert!(
        fs::read_to_string(&hold).unwrap().contains(A),
        "the hold names A"
    );
    assert!(
        Retained::read(&file).unwrap().pinned.is_empty(),
        "as it was"
    );
    fs::remove_dir_all(&aside).unwrap();
    let answer = ab.activate(&target(A), PIN);
    assert!(
        matches!(answer, Answer::Activated { pinned: true, .. }),
        "{answer:?}"
    );
    assert_eq!(
        Retained::read(&file)
            .unwrap()
            .pinned
            .into_iter()
            .collect::<Vec<_>>(),
        [id(A)]
    );

    rig.net
        .borrow_mut()
        .restart_fails
        .insert("kbf-daemon.service".to_owned());
    fs::remove_dir(agent_dir(&rig)).unwrap_or(());
    fs::remove_file(&hold).unwrap();
    // The directory stays, but the hold cannot be made: something in the way aside.
    fs::create_dir_all(agent_dir(&rig).join(".pin.json.new/full")).unwrap();
    let answer = ab.activate(&target(B), PIN);
    assert_eq!(
        answer.json_line(),
        "{\"result\":\"failed\",\"reason\":\"restart_failed\"}"
    );
    assert!(
        notes
            .borrow()
            .iter()
            .any(|n| n.starts_with("activate: the pin: "))
    );
}

/// A version text two releases were recorded as names both, and so does a text recorded for
/// one whose `+g` digits are another's: refused as ambiguous, listing both, and nothing moves.
///
/// Catches: the first of two matches taken, or the recorded text preferred silently over the
/// commit the version carries.
#[test]
fn a_version_that_fits_two_releases_is_refused() {
    let rig = Rig::new("actm-twins");
    keep(&rig, B, Some("2.0-1"), 2);
    keep(&rig, C, Some("2.0-1"), 3);
    let (ab, _) = core(&rig);
    let answer = ab.activate(&target("2.0-1"), How::default());
    assert!(
        answer
            .json_line()
            .starts_with("{\"result\":\"refused\",\"reason\":\"ambiguous_release\""),
        "{answer:?}"
    );
    assert_eq!(
        answer.log_detail().unwrap(),
        format!("\"2.0-1\" names more than one release this host keeps: {C} {B}")
    );
    // C recorded as a version whose digits are B's.
    Retained::update(&retain::path(&rig.layout), |r| {
        r.versions.insert(id(C), VB.to_owned());
    })
    .unwrap();
    let answer = ab.activate(&target(VB), How::default());
    assert!(
        answer.json_line().contains("\"ambiguous_release\""),
        "{answer:?}"
    );
    assert_eq!(current_and_previous(&rig), (Some(A.to_owned()), None));
    assert!(rig.systemctl().is_empty());
}

/// A host that keeps more releases than a listing carries: a release past the listing is still
/// activated by name; a name that fits none is refused with a listing of 64 and the count left
/// out, inside the listing's byte budget.
///
/// Catches: a release that can be activated only when it is in the first page of the listing; a
/// refusal line that outgrows what a client reads.
#[test]
fn a_listing_past_its_bounds_still_resolves_and_says_what_it_left_out() {
    let rig = Rig::new("actm-many");
    for n in 0..70u64 {
        keep(&rig, &format!("{n:02}{}", "d".repeat(38)), None, 10 + n);
    }
    let (ab, _) = core(&rig);
    let answer = ab.activate(&target("cccccccccccc"), How::default());
    let Answer::Unresolved {
        releases,
        releases_omitted,
        ..
    } = &answer
    else {
        panic!("{answer:?}");
    };
    assert_eq!((releases.len(), *releases_omitted), (64, 71 - 64));
    let line: serde_json::Value = serde_json::from_str(&answer.json_line()).unwrap();
    assert_eq!(line["releases_omitted"], 7);
    assert!(
        serde_json::to_string(&line["releases"]).unwrap().len() <= crate::local::core::LISTED_BYTES
    );
    // The oldest (left out of the listing) is activated by its commit.
    let oldest = format!("00{}", "d".repeat(38));
    assert!(!releases.iter().any(|k| k.commit.as_str() == oldest));
    assert!(matches!(
        ab.activate(&target(&oldest), How::default()),
        Answer::Activated { .. }
    ));
}

/// A record of takes left cut short (a crash in the middle of a line) is ended before the next
/// take is appended: each record is a line of its own that parses.
///
/// Catches: a new record glued onto a cut one, which no reader can parse.
#[test]
fn a_take_after_a_record_cut_short_is_a_line_of_its_own() {
    let rig = Rig::new("actm-cut");
    keep(&rig, B, Some(VB), 2);
    let (ab, _) = core(&rig);
    fs::write(rig.dir.join(LOCK_TAKES), "{\"at_unix\":1,\"by\":\"activ").unwrap();
    let _dead = hold_as(&rig.backend.lock_file(), "2147483647\n");
    assert!(matches!(
        ab.activate(&target(B), FORCE),
        Answer::Activated {
            lock_taken: true,
            ..
        }
    ));
    let text = fs::read_to_string(rig.dir.join(LOCK_TAKES)).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 2, "{text:?}");
    assert_eq!(lines[0], "{\"at_unix\":1,\"by\":\"activ");
    let record: serde_json::Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(record["holder_pid"], 2_147_483_647);
    assert!(text.ends_with('\n'));
}

/// `systemctl` that cannot be started, and a unit that never becomes active: `restart_failed`,
/// the host on the release named, every unit asked, none started on the release left.
///
/// Catches: a service manager's failure read as the release coming up.
#[test]
fn the_service_managers_failures_are_restart_failed_on_the_release_named() {
    for broken in ["no-systemctl", "inactive"] {
        let rig = Rig::new(&format!("actm-{broken}"));
        keep(&rig, B, Some(VB), 2);
        {
            let mut net = rig.net.borrow_mut();
            if broken == "inactive" {
                net.inactive.insert("kbf-frontend@a.service".to_owned());
            } else {
                net.broken.insert("no-systemctl");
            }
        }
        let (ab, _) = core(&rig);
        let answer = ab.activate(&target(B), How::default());
        assert_eq!(
            answer.json_line(),
            "{\"result\":\"failed\",\"reason\":\"restart_failed\"}",
            "{broken}"
        );
        assert_eq!(
            current_and_previous(&rig),
            (Some(B.to_owned()), Some(A.to_owned())),
            "{broken}"
        );
        let calls = rig.calls.borrow();
        assert!(
            calls
                .iter()
                .any(|c| c.args.join(" ") == "restart -- kbf-frontend@a.service"),
            "{broken}: every unit is asked"
        );
    }
}

/// A run cut short after the installer's state named B and before the links moved (the links
/// on A), and one cut short after the links moved and before the units restarted: the same
/// command asked again makes the links name B and restarts the units, and `previous` is the
/// release the host ran before.
///
/// Catches: an interrupted activate that cannot be finished by asking again.
#[test]
fn an_interrupted_activate_is_finished_by_asking_again() {
    let rig = Rig::new("actm-again");
    keep(&rig, B, Some(VB), 2);
    let (ab, _) = core(&rig);
    // The state says B (A previous), the links still name A.
    Locked::acquire(&rig.layout)
        .unwrap()
        .write(&State {
            current: Some(id(B)),
            previous: Some(id(A)),
            ..state::read_unlocked(&rig.layout).unwrap().unwrap()
        })
        .unwrap();
    assert_eq!(rig.link("current").as_deref(), Some(A));
    let answer = ab.activate(&target(B), How::default());
    assert!(matches!(answer, Answer::Activated { .. }), "{answer:?}");
    assert_eq!(rig.link("current").as_deref(), Some(B));
    assert_eq!(rig.link("previous").as_deref(), Some(A));
    assert_eq!(
        current_and_previous(&rig),
        (Some(B.to_owned()), Some(A.to_owned()))
    );
    let restarts = rig
        .systemctl()
        .iter()
        .filter(|c| c.starts_with("restart"))
        .count();
    assert_eq!(restarts, 2, "both units restarted on B");

    // The links name B and nothing was restarted: asked again, the units restart on B and
    // `previous` stays A.
    rig.calls.borrow_mut().clear();
    assert!(matches!(
        ab.activate(&target(B), How::default()),
        Answer::Activated { .. }
    ));
    assert_eq!(rig.link("previous").as_deref(), Some(A));
    assert_eq!(rig.systemctl().len(), 4);
}

/// A kept release whose binary is a link to another release's is refused (`not_kept`, with the
/// listing) before anything moves, as one with no binary is.
///
/// Catches: a swap whose units would run another release's binary through a link.
#[test]
fn a_kept_release_whose_binary_is_a_link_is_refused() {
    let rig = Rig::new("actm-link");
    keep(&rig, B, Some(VB), 2);
    let binary = rig.releases().join(B).join("kbf");
    fs::remove_file(&binary).unwrap();
    std::os::unix::fs::symlink(rig.releases().join(A).join("kbf"), &binary).unwrap();
    let (ab, _) = core(&rig);
    let answer = ab.activate(&target(B), How::default());
    assert!(
        answer
            .json_line()
            .starts_with("{\"result\":\"refused\",\"reason\":\"not_kept\",\"releases\":["),
        "{answer:?}"
    );
    assert_eq!(current_and_previous(&rig), (Some(A.to_owned()), None));
    assert!(rig.systemctl().is_empty());
}
