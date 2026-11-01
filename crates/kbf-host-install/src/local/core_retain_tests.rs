//! What the update keeps of the releases: the prune after a confirm, the coordinator's last
//! good release, the versions remembered, and the listing a query carries.

use std::cell::RefCell;
use std::fs;
use std::path::PathBuf;
use std::rc::Rc;

use kbf_retain::CommitId;
use kbf_retain::{Kept, Retained};

use super::{A, B, C, Fx, VA, VB, VC, failed, refused};
use crate::local::answer::{Answer, Code};
use crate::local::backend::{Backend, Fault, Pending};
use crate::request::{Channel, Version};

type Notes = Rc<RefCell<Vec<String>>>;

/// A host over the scripted backend that has a `retain.json` and keeps `keep` releases, with the
/// notes the update tells.
fn host(name: &str, keep: usize) -> (Fx, Notes) {
    let mut fx = Fx::new(name);
    fx.world.borrow_mut().retain_file = Some(fx.dir.join("retain.json"));
    // The releases the host holds: what runs and the one it came from.
    fx.world.borrow_mut().kept = vec![held(B, None), held(A, None)];
    let notes: Notes = Rc::default();
    let told = Rc::clone(&notes);
    fx.ab = Fx::process(&fx.dir, &fx.world, &fx.clock).keeping(
        keep,
        Box::new(move |text| told.borrow_mut().push(text.to_owned())),
    );
    (fx, notes)
}

fn held(commit: &str, version: Option<&str>) -> Kept {
    Kept {
        commit: CommitId::new(commit).unwrap(),
        version: version.map(str::to_owned),
        why: Vec::new(),
        version_left_out: false,
    }
}

fn prunes(fx: &Fx) -> Vec<String> {
    fx.calls()
        .into_iter()
        .filter(|call| call.starts_with("prune"))
        .collect()
}

/// Lay out and switch to VB: the release is on trial.
fn on_trial(fx: &Fx) {
    fx.install(VB);
    fx.ask(&format!("switch kbf={VB}"));
}

fn retained(fx: &Fx) -> Retained {
    Retained::read(&fx.dir.join("retain.json")).unwrap()
}

fn commit(text: &str) -> CommitId {
    CommitId::new(text).unwrap()
}

/// The prune is asked once, after the confirm and with the configured count: not after the
/// stage or the switch, and the confirm is done before it.
///
/// Catches: a prune after a stage or a switch; a confirm that never prunes.
#[test]
fn a_prune_follows_a_confirm_once_and_nothing_before_it() {
    let (fx, _) = host("ab-prune-once", 6);
    fx.install(VB);
    assert_eq!(prunes(&fx), Vec::<String>::new(), "after the stage");
    fx.ask(&format!("switch kbf={VB}"));
    assert_eq!(prunes(&fx), Vec::<String>::new(), "after the switch");
    fx.ask(&format!("confirm kbf={VB}"));
    assert_eq!(prunes(&fx), ["prune 6"]);
    let calls = fx.calls();
    assert_eq!(calls.last().map(String::as_str), Some("prune 6"));
    assert_eq!(calls[calls.len() - 2], "confirm act");
}

/// A host that keeps no release directories (no retain file) is never asked to prune, and
/// records no last good.
///
/// Catches: a prune asked of a host with no retain file.
#[test]
fn a_backend_without_a_retain_file_is_not_pruned() {
    let (fx, notes) = host("ab-no-file", 4);
    fx.world.borrow_mut().retain_file = None;
    on_trial(&fx);
    let answer = fx.ask(&format!("confirm kbf={VB} last_good={VA}"));
    assert_eq!(answer, Answer::Confirmed { version: VB.into() });
    assert_eq!(prunes(&fx), Vec::<String>::new());
    assert!(!fx.dir.join("retain.json").exists());
    assert!(notes.borrow().is_empty());
}

/// Nothing is pruned when no confirm happened: a confirm for another version, a confirm that
/// the platform could not do, one repeated after it was answered, a confirm with nothing on
/// trial, a revert and a timeout. Only the one confirm that kept the release prunes.
#[test]
fn nothing_is_pruned_but_after_a_confirm_that_kept_the_release() {
    let (fx, _) = host("ab-prune-refused", 4);
    assert!(refused(Code::NothingPending)(
        &fx.ask(&format!("confirm kbf={VB}"))
    ));
    on_trial(&fx);
    assert!(refused(Code::Pending)(
        &fx.ask(&format!("confirm kbf={VC}"))
    ));
    fx.world.borrow_mut().confirm_fault = Some(Fault::failed(Code::StateUnavailable, "disk"));
    assert!(failed(Code::StateUnavailable)(
        &fx.ask(&format!("confirm kbf={VB}"))
    ));
    assert_eq!(prunes(&fx), Vec::<String>::new(), "refused or failed");

    fx.world.borrow_mut().confirm_fault = None;
    fx.ask(&format!("confirm kbf={VB}"));
    assert_eq!(prunes(&fx).len(), 1);
    // The answer was lost and the confirm is asked again: it is answered, and nothing more
    // is removed.
    assert_eq!(
        fx.ask(&format!("confirm kbf={VB}")),
        Answer::Confirmed { version: VB.into() }
    );
    assert_eq!(prunes(&fx).len(), 1, "the repeated confirm");

    let (back, _) = host("ab-prune-revert", 4);
    on_trial(&back);
    back.ask("revert");
    assert_eq!(prunes(&back), Vec::<String>::new(), "after a revert");

    let (late, _) = host("ab-prune-timeout", 4);
    on_trial(&late);
    late.clock.set(1000 + 600);
    assert!(matches!(late.ab.tick(), Some(Answer::Reverted { .. })));
    assert_eq!(prunes(&late), Vec::<String>::new(), "after a timeout");
}

/// A count below two is raised to two: a host never keeps fewer than the release it came
/// from and one more.
///
/// Catches: a count below the floor passed on to the prune.
#[test]
fn a_keep_below_two_is_raised_to_two() {
    for keep in [1, 0] {
        let (fx, _) = host(&format!("ab-keep-{keep}"), keep);
        on_trial(&fx);
        fx.ask(&format!("confirm kbf={VB}"));
        assert_eq!(prunes(&fx), ["prune 2"], "keep {keep}");
    }
}

/// The last good release of a confirm is recorded as the commit the host holds (the 12 digits
/// of the version match the 40 of the directory), whichever other releases the host holds.
///
/// Catches: the version matched against the whole commit; the last good not recorded.
#[test]
fn the_last_good_release_is_recorded_as_the_held_commit() {
    let (fx, _) = host("ab-last-good", 4);
    fx.world.borrow_mut().kept = vec![held(B, None), held(C, None), held(A, None)];
    on_trial(&fx);
    fx.ask(&format!("confirm kbf={VB} last_good={VA}"));
    assert_eq!(retained(&fx).last_good, Some(commit(A)));
}

/// A last good the host does not hold clears the one recorded before (it is no longer the
/// coordinator's), and so does a version with no commit in it.
///
/// Catches: a stale last good kept when the order names one the host does not hold.
#[test]
fn a_last_good_that_is_not_held_clears_the_one_before() {
    for (name, last_good) in [("ab-lg-absent", VC), ("ab-lg-nocommit", "1.2.3-1")] {
        let (fx, _) = host(name, 4);
        let file = Retained {
            last_good: Some(commit(A)),
            pinned: [commit(C)].into(),
            ..Retained::default()
        };
        file.write(&fx.dir.join("retain.json")).unwrap();
        fx.world.borrow_mut().kept = vec![held(B, None), held(A, None)];
        on_trial(&fx);
        fx.ask(&format!("confirm kbf={VB} last_good={last_good}"));
        let now = retained(&fx);
        assert_eq!(now.last_good, None, "{name}");
        assert!(
            now.pinned.contains(&commit(C)),
            "{name}: a pin is not touched"
        );
    }
}

/// A confirm that names no last good leaves the recorded one alone.
///
/// Catches: the recorded last good cleared by a confirm that carries none.
#[test]
fn a_confirm_without_a_last_good_leaves_the_recorded_one() {
    let (fx, _) = host("ab-lg-left", 4);
    let file = Retained {
        last_good: Some(commit(A)),
        ..Retained::default()
    };
    file.write(&fx.dir.join("retain.json")).unwrap();
    fx.world.borrow_mut().kept = vec![held(B, None), held(A, None)];
    on_trial(&fx);
    fx.ask(&format!("confirm kbf={VB}"));
    assert_eq!(retained(&fx).last_good, Some(commit(A)));
}

/// A confirm whose last good cannot be recorded (the retain file cannot be parsed) fails and
/// keeps the release on trial, so no collection removes anything; the log says why, and the
/// file is left as it was.
///
/// Catches: a retain file that cannot be parsed passed over in silence; a confirm that ends the
/// trial with its last good unrecorded, after which any collection may remove that release.
#[test]
fn a_last_good_that_cannot_be_recorded_fails_the_confirm_and_keeps_the_trial() {
    let (fx, notes) = host("ab-retain-bad", 4);
    fs::write(fx.dir.join("retain.json"), "{ not json").unwrap();
    on_trial(&fx);
    let answer = fx.ask(&format!("confirm kbf={VB} last_good={VA}"));
    assert!(
        matches!(&answer, Answer::Failed(Code::StateUnavailable, why) if why.contains("cannot be parsed")),
        "{answer:?}"
    );
    assert!(
        fx.world.borrow().pending.is_some(),
        "the release stays on trial"
    );
    let told = notes.borrow().clone();
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(told[0].starts_with("retain: "), "{told:?}");
    assert!(
        told[0].ends_with("; the release stays on trial"),
        "{told:?}"
    );
    assert!(prunes(&fx).is_empty());
    assert_eq!(
        fs::read_to_string(fx.dir.join("retain.json")).unwrap(),
        "{ not json",
        "the file is left as it was"
    );
}

/// What a prune says is told to the log; one that has nothing to say tells nothing; one that
/// fails is told with its detail, and the confirm still answers confirmed.
///
/// Catches: a failed prune turned into the answer of the confirm; a prune's note dropped.
#[test]
fn what_a_prune_did_or_failed_to_do_is_told_and_the_confirm_stands() {
    let (fx, notes) = host("ab-prune-say", 4);
    fx.world.borrow_mut().prune_say = "keeping: x; removing: y".to_owned();
    on_trial(&fx);
    assert_eq!(
        fx.ask(&format!("confirm kbf={VB}")),
        Answer::Confirmed { version: VB.into() }
    );
    assert_eq!(*notes.borrow(), ["prune: keeping: x; removing: y"]);

    let (quiet, notes) = host("ab-prune-quiet", 4);
    on_trial(&quiet);
    quiet.ask(&format!("confirm kbf={VB}"));
    assert!(notes.borrow().is_empty(), "{:?}", notes.borrow());

    let (broken, notes) = host("ab-prune-fault", 4);
    broken.world.borrow_mut().prune_fault = Some(Fault::failed(Code::StateUnavailable, "boom"));
    on_trial(&broken);
    assert_eq!(
        broken.ask(&format!("confirm kbf={VB}")),
        Answer::Confirmed { version: VB.into() }
    );
    assert_eq!(*notes.borrow(), ["prune: boom"]);
    assert_eq!(broken.query().version.as_deref(), Some(VB));
    assert_eq!(broken.query().last.unwrap().outcome, "succeeded");
}

/// The versions of releases the host no longer keeps are forgotten once the prune is done,
/// and only those.
///
/// Catches: the versions of removed releases kept in the state file.
#[test]
fn the_versions_of_releases_no_longer_kept_are_forgotten() {
    let (fx, _) = host("ab-forget", 4);
    fx.install(VA);
    fx.world.borrow_mut().lays = Some(C.to_owned());
    fx.install(VC);
    fx.world.borrow_mut().lays = Some(B.to_owned());
    fx.install(VB);
    fx.ask(&format!("switch kbf={VB}"));
    let versions = |fx: &Fx| -> Vec<String> {
        let text = fs::read_to_string(fx.dir.join("state.json")).unwrap();
        let saved: serde_json::Value = serde_json::from_str(&text).unwrap();
        saved["versions"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect()
    };
    assert_eq!(versions(&fx), [A, B, C]);
    {
        let mut world = fx.world.borrow_mut();
        world.kept = vec![held(B, None), held(A, None), held(C, None)];
        world.prune_removes = vec![C.to_owned()];
    }
    fx.ask(&format!("confirm kbf={VB}"));
    assert_eq!(versions(&fx), [A, B]);
}

/// When the listing cannot be read, nothing is forgotten (and the confirm stands).
///
/// Catches: an unreadable listing taken for an empty one, which forgets every version.
#[test]
fn an_unreadable_listing_forgets_nothing() {
    let (fx, _) = host("ab-forget-fault", 4);
    on_trial(&fx);
    fx.world.borrow_mut().kept_fault = true;
    assert_eq!(
        fx.ask(&format!("confirm kbf={VB}")),
        Answer::Confirmed { version: VB.into() }
    );
    let text = fs::read_to_string(fx.dir.join("state.json")).unwrap();
    assert!(text.contains(B), "{text}");
    assert_eq!(prunes(&fx).len(), 1, "the prune is asked for all the same");
}

/// The state file remembers every release the host keeps, whatever the count: with a keep of
/// twenty it holds twenty-eight versions, not sixteen (with the default four it holds sixteen,
/// as before).
///
/// Catches: the versions bounded by the old fixed sixteen whatever the keep.
#[test]
fn the_versions_remembered_cover_every_release_kept() {
    let (fx, _) = host("ab-bound-keep", 20);
    for n in 0..40_u32 {
        let current = format!("{n:012x}{}", "0".repeat(28));
        fx.world.borrow_mut().current = Some(current);
        fx.install(&format!("0.1.{n}+g{n:012x}-1"));
    }
    let text = fs::read_to_string(fx.dir.join("state.json")).unwrap();
    let saved: serde_json::Value = serde_json::from_str(&text).unwrap();
    let names = saved["versions"].as_object().unwrap();
    assert_eq!(names.len(), 28);
    assert!(names.contains_key(&format!("{:012x}{}", 39, "0".repeat(28))));
}

/// A query lists the releases the host keeps with their commit and version. The version the
/// backend knows wins; one it does not know is the text the release was staged as here; one
/// that is neither is unknown.
#[test]
fn a_query_lists_the_releases_with_commit_and_version() {
    let (fx, _) = host("ab-listing", 4);
    fx.install(VA);
    fx.world.borrow_mut().kept = vec![held(B, None), held(A, Some("0.0.1-native")), held(C, None)];
    fx.install(VB);
    let view = fx.query();
    let shown: Vec<(String, Option<String>)> = view
        .releases
        .iter()
        .map(|k| (k.commit.to_string(), k.version.clone()))
        .collect();
    assert_eq!(
        shown,
        [
            (B.to_owned(), Some(VB.to_owned())),
            (A.to_owned(), Some("0.0.1-native".to_owned())),
            (C.to_owned(), None),
        ]
    );
    let line = Answer::Queried(view).json_line();
    let json: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(json["releases"][0]["commit"], B);
    assert_eq!(json["releases"][0]["version"], VB);
    assert_eq!(json["releases"][2]["version"], serde_json::Value::Null);
}

/// A query lists at most 64 releases, the newest first.
#[test]
fn a_query_lists_at_most_sixty_four_releases() {
    let (fx, _) = host("ab-listing-cap", 4);
    let many: Vec<Kept> = (0..70_u32)
        .map(|n| held(&format!("{n:040x}"), None))
        .collect();
    fx.world.borrow_mut().kept = many;
    let view = fx.query();
    assert_eq!(view.releases.len(), 64);
    assert_eq!(view.releases[0].commit.to_string(), format!("{:040x}", 0));
    assert_eq!(view.releases[63].commit.to_string(), format!("{:040x}", 63));
}

/// A listing that cannot be read leaves the releases out and says so; the rest of the answer
/// stands. A host whose listing is empty says nothing of the kind: the two are told apart.
///
/// Catches: a listing that cannot be read answered as an empty one.
#[test]
fn a_query_whose_listing_fails_still_answers_and_says_it_is_unavailable() {
    let (fx, _) = host("ab-listing-fault", 4);
    fx.world.borrow_mut().kept_fault = true;
    let view = fx.query();
    assert!(view.releases.is_empty());
    assert!(view.releases_unavailable);
    assert_eq!(view.commit.as_deref(), Some(A));
    let json: serde_json::Value = serde_json::from_str(&Answer::Queried(view).json_line()).unwrap();
    assert_eq!(json["releases_unavailable"], true);
    assert_eq!(json["commit"], A);

    fx.world.borrow_mut().kept_fault = false;
    fx.world.borrow_mut().kept = Vec::new();
    let view = fx.query();
    assert!(view.releases.is_empty() && !view.releases_unavailable);
    let line = Answer::Queried(view).json_line();
    assert!(!line.contains("releases"), "{line}");
}

/// A backend that keeps no release directories (a package, a container image) and so answers
/// nothing of the three.
struct Bare;

impl Backend for Bare {
    fn lock_file(&self) -> PathBuf {
        PathBuf::new()
    }
    fn node_id(&self) -> Option<String> {
        None
    }
    fn current(&self) -> Result<Option<String>, Fault> {
        Ok(None)
    }
    fn pending(&self) -> Result<Option<Pending>, Fault> {
        Ok(None)
    }
    fn has(&self, _commit: &str) -> bool {
        false
    }
    fn stage(&self, _version: &Version, _channel: &Channel) -> Result<String, Fault> {
        unreachable!("not asked")
    }
    fn switch(&self, _commit: &str, _window_secs: u64, _now: u64) -> Result<u64, Fault> {
        unreachable!("not asked")
    }
    fn confirm(&self, _pending: &Pending) -> Result<(), Fault> {
        unreachable!("not asked")
    }
    fn revert(&self, _pending: &Pending) -> Result<(), Fault> {
        unreachable!("not asked")
    }
}

/// By default a backend lists no release, names no retain file and prunes nothing.
///
/// Catches: a default that lists, names a retain file or prunes.
#[test]
fn a_backend_that_keeps_no_release_directories_keeps_nothing() {
    assert_eq!(Bare.kept().unwrap(), Vec::<Kept>::new());
    assert_eq!(Bare.retain_file(), None);
    assert_eq!(Bare.prune(4).unwrap(), "");
}

/// A confirm that names a last good while the listing cannot be read fails and keeps the
/// release on trial; the recorded last good is left as it is. Once the listing reads, the same
/// confirm records it and keeps the release.
///
/// Catches: a listing that cannot be read taken for one that does not hold the last good, which
/// clears the recorded one; a confirm asked again that does not record what the first could not.
#[test]
fn a_last_good_is_left_as_recorded_when_the_listing_cannot_be_read() {
    let (fx, notes) = host("ab-lg-blind", 4);
    let file = Retained {
        last_good: Some(commit(C)),
        ..Retained::default()
    };
    file.write(&fx.dir.join("retain.json")).unwrap();
    on_trial(&fx);
    fx.world.borrow_mut().kept_fault = true;
    let line = format!("confirm kbf={VB} last_good={VA}");
    let answer = fx.ask(&line);
    assert!(
        matches!(answer, Answer::Failed(Code::StateUnavailable, _)),
        "{answer:?}"
    );
    assert!(
        fx.world.borrow().pending.is_some(),
        "the release stays on trial"
    );
    assert_eq!(retained(&fx).last_good, Some(commit(C)));
    assert_eq!(
        *notes.borrow(),
        ["retain: listing; the release stays on trial"]
    );
    assert!(prunes(&fx).is_empty());

    fx.world.borrow_mut().kept_fault = false;
    assert_eq!(fx.ask(&line), Answer::Confirmed { version: VB.into() });
    assert_eq!(retained(&fx).last_good, Some(commit(A)));
    assert!(fx.world.borrow().pending.is_none());
    assert_eq!(prunes(&fx), ["prune 4"]);
}

/// Past the limit of the listing, a release that must stay is listed however old it is: the
/// oldest others are the ones left out.
///
/// Catches: a cut of the listing by age alone, which drops an old `current` (after a rollback)
/// or an old pinned release from the query.
#[test]
fn a_cut_listing_still_names_every_release_that_must_stay() {
    let (fx, _) = host("ab-listing-must", 4);
    let mut many: Vec<Kept> = (0..70_u32)
        .map(|n| held(&format!("{n:040x}"), None))
        .collect();
    many[68].why = vec!["current"];
    many[69].why = vec!["pinned"];
    fx.world.borrow_mut().kept = many;
    let listed: Vec<String> = fx
        .query()
        .releases
        .iter()
        .map(|k| k.commit.to_string())
        .collect();
    assert_eq!(listed.len(), 64);
    assert_eq!(
        listed[..62],
        (0..62_u32).map(|n| format!("{n:040x}")).collect::<Vec<_>>()[..]
    );
    assert_eq!(
        listed[62..],
        [format!("{:040x}", 68), format!("{:040x}", 69)]
    );
}

/// A host with more releases that must stay than a query lists names the release that runs, and
/// then the pinned ones, newest first, and says how many it left out; the line carries the
/// count, which is absent when nothing is left out.
///
/// Catches: the listing cut by age alone, which drops an old `current`; pinned releases ranked
/// with the others; releases left out without a count.
#[test]
fn a_listing_past_the_limit_names_what_runs_first_and_counts_what_it_leaves_out() {
    let (fx, _) = host("ab-listing-pins", 4);
    let mut many: Vec<Kept> = (0..70_u32)
        .map(|n| {
            let mut k = held(&format!("{n:040x}"), None);
            k.why = vec!["pinned"];
            k
        })
        .collect();
    let mut old = held(&format!("{:040x}", 99), None);
    old.why = vec!["current"];
    many.push(old);
    // An other newer than every pinned release.
    many.insert(0, held(&format!("{:040x}", 98), None));
    fx.world.borrow_mut().kept = many;
    let view = fx.query();
    let listed: Vec<String> = view.releases.iter().map(|k| k.commit.to_string()).collect();
    let mut want: Vec<String> = (0..63_u32).map(|n| format!("{n:040x}")).collect();
    want.push(format!("{:040x}", 99));
    assert_eq!(listed, want);
    assert_eq!(view.releases_omitted, 8);
    let line = Answer::Queried(view).json_line();
    let json: serde_json::Value = serde_json::from_str(&line).unwrap();
    assert_eq!(json["releases"].as_array().unwrap().len(), 64);
    assert_eq!(json["releases_omitted"], 8);
    assert!(line.len() < 16 * 1024, "{} bytes", line.len());

    fx.world.borrow_mut().kept = vec![held(B, None), held(A, None)];
    let line = Answer::Queried(fx.query()).json_line();
    assert!(!line.contains("releases_omitted"), "{line}");
}

/// A last good whose 12 digits name two releases the host holds records neither: the confirm
/// fails and keeps the release on trial, the one recorded before stays, nothing is pruned, and
/// the log names both.
///
/// Catches: the first of two matching releases taken for the last good, by the order of age.
#[test]
fn a_last_good_that_names_two_held_releases_records_neither_and_keeps_the_trial() {
    let (fx, notes) = host("ab-lg-ambiguous", 4);
    let twin = format!("{}{}", &A[..12], "b".repeat(28));
    fx.world.borrow_mut().kept = vec![held(B, None), held(A, None), held(&twin, None)];
    let file = Retained {
        last_good: Some(commit(C)),
        ..Retained::default()
    };
    file.write(&fx.dir.join("retain.json")).unwrap();
    on_trial(&fx);
    let answer = fx.ask(&format!("confirm kbf={VB} last_good={VA}"));
    assert!(
        matches!(answer, Answer::Failed(Code::StateUnavailable, _)),
        "{answer:?}"
    );
    assert!(fx.world.borrow().pending.is_some());
    assert_eq!(retained(&fx).last_good, Some(commit(C)));
    assert!(prunes(&fx).is_empty());
    let told = notes.borrow();
    assert_eq!(told.len(), 1, "{told:?}");
    assert!(
        told[0].contains(&format!("names 2 releases: {A} {twin}"))
            && told[0].ends_with("; the release stays on trial"),
        "{told:?}"
    );
}

/// The versions copied from this program's state never replace one a stage records after the
/// listing they were chosen from was taken: the stage's is the release's own.
///
/// Catches: the copy deciding what is missing before it holds the file's lock, which writes the
/// older text over the one the stage just recorded.
#[test]
fn a_version_recorded_after_the_listing_is_not_replaced_by_the_copy() {
    let (fx, _) = host("ab-seed-late", 4);
    fx.install(VB);
    fx.world.borrow_mut().kept = vec![held(B, None), held(A, None)];
    fx.world.borrow_mut().recorded_after_listing = Some((B.to_owned(), "the stage's".to_owned()));
    fx.ab.seed_versions();
    let file = retained(&fx);
    assert_eq!(
        file.versions.get(&commit(B)).map(String::as_str),
        Some("the stage's")
    );
    assert_eq!(file.versions.get(&commit(A)).map(String::as_str), None);
}

/// Write this program's own state with the version text `versions` gives each commit.
fn saved_versions(fx: &Fx, versions: &[(String, String)]) {
    let map: serde_json::Map<String, serde_json::Value> = versions
        .iter()
        .map(|(c, v)| (c.clone(), serde_json::Value::String(v.clone())))
        .collect();
    let path = fx.dir.join("state.json");
    fs::write(&path, serde_json::json!({ "versions": map }).to_string()).unwrap();
    fs::set_permissions(&path, std::os::unix::fs::PermissionsExt::from_mode(0o600)).unwrap();
}

/// A version text this program's state knows, filled into an entry the backend lists without
/// one, is held to the same limit as any other: too long, it is left out and the entry says so.
///
/// Catches: the versions filled in after the cut, past the limit of an entry.
#[test]
fn a_long_version_from_this_programs_state_is_left_out_of_its_entry() {
    let (fx, _) = host("ab-fill-long", 4);
    let long = format!(
        "0.1.0+g{}-{}",
        &B[..12],
        "9".repeat(kbf_retain::MAX_LISTED_VERSION)
    );
    saved_versions(&fx, &[(B.to_owned(), long)]);
    fx.world.borrow_mut().kept = vec![held(B, None), held(A, None)];
    let view = fx.query();
    assert_eq!(view.releases[0].version, None);
    assert!(view.releases[0].version_left_out);
}

/// The listing stays within its budget with the versions this program's state fills in: many
/// releases the backend lists without a version, each filled with a long one that is still
/// short enough to list, make an array no longer than [`crate::local::core::LISTED_BYTES`], and the
/// count of what is left out says the rest.
///
/// Catches: the budget applied before the versions are filled in, which lets the line grow past
/// it and past what the agent reads.
#[test]
fn the_listing_fits_its_budget_after_the_versions_are_filled_in() {
    let (fx, _) = host("ab-fill-budget", 4);
    let commits: Vec<String> = (0..64_u32).map(|n| format!("{n:040x}")).collect();
    let versions: Vec<(String, String)> = commits
        .iter()
        .map(|c| {
            (
                c.clone(),
                format!("0.1.0+g{}-{}", &c[..12], "7".repeat(100)),
            )
        })
        .collect();
    saved_versions(&fx, &versions);
    fx.world.borrow_mut().kept = commits.iter().map(|c| held(c, None)).collect();
    let view = fx.query();
    let array = serde_json::to_string(&view.releases).unwrap();
    assert!(
        array.len() <= crate::local::core::LISTED_BYTES,
        "{} bytes",
        array.len()
    );
    assert!(view.releases.iter().all(|k| k.version.is_some()));
    assert_eq!(view.releases.len() + view.releases_omitted, 64);
    assert!(view.releases_omitted > 0);
}

/// An entry the backend lists with its version left out as too long is not filled in again
/// from this program's state.
///
/// Catches: a version that was left out put back, past the limit it was left out for.
#[test]
fn an_entry_whose_version_was_left_out_is_not_filled_in_again() {
    let (fx, _) = host("ab-fill-left-out", 4);
    saved_versions(&fx, &[(B.to_owned(), VB.to_owned())]);
    let mut left_out = held(B, None);
    left_out.version_left_out = true;
    fx.world.borrow_mut().kept = vec![left_out, held(A, None)];
    let view = fx.query();
    assert_eq!(view.releases[0].version, None);
    assert!(view.releases[0].version_left_out);
}
