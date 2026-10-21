//! What the run loop tells of the host's package and its source, each pass, through the real
//! cycle over a scratch host: `available` in `status.json` when the package's seed is whole
//! and above the floor of `seq`, `release_source`, and the floor itself (where it starts, when
//! it rises, what leaves it alone). What each test proves is named on it, with the mutant to
//! plant to turn it red.
//!
//! The host runs release A, which declares `EARLIER_SEQ`; the package carries the seed of B,
//! which declares `SEQ`. The seed fetcher is the real one over the package's files, with `gh`
//! and the binaries' reports played by a fake runner.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use super::*;
use crate::fixtures::version_json;
use crate::run::Spawn;
use crate::seed::SeedFacts;
use crate::seed_fixture::{EARLIER_SEQ, SEQ, Seeded};

/// The calls a seed fetcher's runner has been given.
type Calls = Rc<RefCell<Vec<Spawn>>>;

const BUNDLED: &str = "release_source = \"bundled\"\n";
const URL: &str = "release_source = \"url\"\nrelease_url = \"https://mirror.example\"\n";

/// A node running A whose package carries the seed of B as `plant` left it, looked at by the
/// seed fetcher over a runner whose calls are kept.
fn noticing(name: &str, plant: impl FnOnce(&mut Seeded)) -> (Rig, Seeded, Calls) {
    let mut rig = Rig::new(name);
    let mut seeded = Seeded::on(rig.layout.clone(), rig.root.clone(), B);
    plant(&mut seeded);
    seeded.write_tarball();
    let runner = seeded.runner();
    let calls = runner.log();
    let mut fetcher = seeded.fetcher();
    fetcher.runner = Box::new(runner);
    rig.helper.set_fetcher(Box::new(fetcher));
    (rig, seeded, calls)
}

/// The record of a staged `verb` on `commit`: a locator, with the source named.
fn staged(rig: &Rig, id: u64, verb: Verb, commit: &str) -> OrderRecord {
    let mut r = rig.rec(id, 1, 1, verb, commit);
    r.components.clear();
    r.expected = None;
    r.source = Source::Staged;
    r
}

/// The names in `dir`, sorted; none when it is not there.
fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    names.sort();
    names
}

/// The bytes of `state.json`.
fn state_bytes(rig: &Rig) -> Vec<u8> {
    fs::read(rig.layout.join(&rig.layout.state_file())).expect("state.json")
}

/// How many times `gh` was run.
fn gh_runs(calls: &Calls) -> usize {
    let is_gh = |spawn: &&Spawn| spawn.program.file_name().is_some_and(|n| n == "gh");
    calls.borrow().iter().filter(is_gh).count()
}

/// What the helper has said about the package to its log since the last time (the lines
/// `run` sends to the journal that begin `package: `; the log holds the steps of the verbs
/// too, which are not this test's).
fn said(rig: &mut Rig) -> String {
    let mut out = Vec::new();
    rig.helper.drain_log(&mut out);
    String::from_utf8_lossy(&out)
        .lines()
        .filter(|line| line.starts_with("package: "))
        .map(|line| format!("{line}\n"))
        .collect()
}

/// What `kbf --version --json` prints for a release of `commit` that declares `seq`.
fn declares(commit: &str, seq: u64) -> String {
    version_json(commit, "release").replace('}', &format!(",\"seq\":{seq}}}"))
}

/// The release's floor line, as the file holds it.
fn line(seq: u64) -> Option<String> {
    Some(format!("{seq}\n"))
}

/// A pass over a host whose package carries a whole seed above the floor tells `available` and
/// does nothing about it: `state.json` is byte for byte what it was, no release is installed
/// or staged, no unit is restarted, no scratch is left, no order is spent; the package was
/// checked once, not at each pass; and the floor, which this host had none of, starts at the
/// `seq` of the release that runs.
///
/// Catches a look that is more than a look (a pass that stages: the seed installed, or the
/// state written), a package checked every five seconds, and a host the announcement leaves
/// changed.
///
/// Mutants to plant: the look made by installing the seed through `pull` (red on the release
/// installed and on the state); the cache by digest removed (red on `gh` runs).
#[test]
fn an_upgrade_is_noticed_and_the_pass_moves_nothing() {
    let (mut rig, seeded, calls) = noticing("notice-quiet", |_| {});
    let state = state_bytes(&rig);
    let releases = listing(&rig.layout.join(&rig.layout.releases()));
    rig.cycle(T0);
    rig.cycle(T0 + 5);
    let status = rig.status();
    assert_eq!(
        status["available"],
        serde_json::json!({"commit": B, "seq": SEQ})
    );
    assert_eq!(status["state"], "idle");
    assert_eq!(
        state_bytes(&rig),
        state,
        "state.json is what it was, byte for byte"
    );
    let after = listing(&rig.layout.join(&rig.layout.releases()));
    assert_eq!(after, releases);
    assert_eq!(rig.current(), Some(c(A)));
    assert_eq!(rig.state().staged, None);
    assert_eq!(rig.restarts(), 0);
    assert!(!seeded.installed());
    let work = rig.layout.join(&rig.layout.work_dir());
    assert_eq!(listing(&work), Vec::<String>::new());
    assert_eq!(rig.left(), 0);
    assert_eq!(seeded.floor_text(), line(EARLIER_SEQ));
    assert_eq!(gh_runs(&calls), 1, "checked once, not at each pass");
    assert_eq!(said(&mut rig), "");
}

/// A seed nobody has checked is not announced: a tarball that is not the one the manifest
/// names, and a bundle `gh` does not accept, each leave `available` out of the status and are
/// said in the log, once and not at every pass, and nothing is installed.
///
/// Catches `available` written from the manifest alone, which any package can make say
/// anything.
///
/// Mutants to plant: the announcement made before the checks (red on both cases); the
/// complaint said at every pass (red on the second log).
#[test]
fn a_seed_nobody_has_checked_is_not_announced_and_the_reason_is_said_once() {
    type Plant = fn(&mut Seeded);
    let cases: [(&str, Plant, &str); 2] = [
        (
            "a flipped byte",
            |s| {
                let middle = s.tarball.len() / 2;
                s.tarball[middle] ^= 1;
            },
            "package: fetch: package_in_flux: the tarball is not the one",
        ),
        (
            "a bundle gh refuses",
            |s| s.attested = b"the bundle of another release\n".to_vec(),
            "package: fetch: rejected: attestation: ",
        ),
    ];
    for (index, (what, plant, want)) in cases.into_iter().enumerate() {
        let (mut rig, seeded, _) = noticing(&format!("notice-bad-{index}"), plant);
        rig.cycle(T0);
        assert!(rig.status().get("available").is_none(), "{what}");
        let first = said(&mut rig);
        assert!(first.starts_with(want), "{what}: {first}");
        rig.cycle(T0 + 5);
        assert!(rig.status().get("available").is_none(), "{what}");
        assert_eq!(said(&mut rig), "", "{what}: said once");
        assert!(!seeded.installed(), "{what}");
    }
}

/// A package that was half unpacked when it was looked at is looked at again after the minute a
/// failed look waits, and announced once it is whole: a failure is not remembered by the
/// manifest's digest. The complaint is not repeated when the seed is announced.
///
/// Catches a package announced never because the first look found it mid-upgrade.
///
/// Mutant to plant: the failure stored as the answer for the manifest's digest.
#[test]
fn a_seed_that_becomes_whole_is_announced_at_the_next_pass() {
    let flip = |s: &mut Seeded| {
        let middle = s.tarball.len() / 2;
        s.tarball[middle] ^= 1;
    };
    let (mut rig, mut seeded, _) = noticing("notice-repaired", flip);
    rig.cycle(T0);
    assert!(rig.status().get("available").is_none());
    assert!(said(&mut rig).contains("package_in_flux"));
    flip(&mut seeded);
    seeded.write_tarball();
    rig.cycle(T0 + 65);
    assert_eq!(
        rig.status()["available"],
        serde_json::json!({"commit": B, "seq": SEQ})
    );
    assert_eq!(said(&mut rig), "");
}

/// A seed whose `seq` is not above the floor is not announced, and one above it is: the floor
/// the host holds (above the release that runs) decides, an equal `seq` is not above, and the
/// reason is said in the order's own words. The release that runs (it declares less than any
/// floor here) does not lower the floor.
///
/// Catches the announcement of a seed the order would refuse, an equal `seq` taken as above, and
/// a floor lowered to the release that runs.
///
/// Mutants to plant: the floor not consulted; `>` made `>=`; the floor written as the
/// running release's `seq`.
#[test]
fn a_seed_at_or_below_the_floor_is_not_announced_and_one_above_it_is() {
    for (floor, announced) in [(SEQ - 1, true), (SEQ, false), (SEQ + 7, false)] {
        let (mut rig, seeded, _) = noticing(&format!("notice-floor-{floor}"), |_| {});
        seeded.set_floor(floor);
        rig.cycle(T0);
        let told = rig.status().get("available").is_some();
        assert_eq!(told, announced, "floor {floor}");
        let wanted = if announced {
            String::new()
        } else {
            format!("package: seq_floor: seed seq {SEQ} is not above {floor}\n")
        };
        assert_eq!(said(&mut rig), wanted, "floor {floor}");
        assert_eq!(seeded.floor_text(), line(floor), "floor {floor}");
    }
}

/// A farm that is ahead of the package it received starts its floor at the `seq` of the release
/// that runs: the host runs a release that declares 500 and the package carries 450, so nothing
/// is announced, the floor file says 500, and an order for the seed is refused for it.
///
/// Catches a floor that starts at 0 (an older package than the farm, ordered by whoever can
/// write the spool, would be installed over it) and an announcement of that package.
///
/// Mutant to plant: the floor started at 0 (the seed is announced and the order installs it).
#[test]
fn a_farm_ahead_of_the_package_starts_its_floor_at_the_release_that_runs() {
    let (mut rig, seeded, _) = noticing("notice-ahead", |s| {
        s.manifest["seq"] = serde_json::json!(450);
        s.probe = s.probe.replace(&format!("\"seq\":{SEQ},"), "\"seq\":450,");
        s.set_earlier_seq(500);
    });
    rig.cycle(T0);
    assert_eq!(seeded.floor_text(), line(500));
    assert!(rig.status().get("available").is_none());
    let want = "package: seq_floor: seed seq 450 is not above 500\n";
    assert_eq!(said(&mut rig), want);

    rig.put(&staged(&rig, 5, Verb::Activate, B));
    rig.cycle(T0 + 5);
    let want = "commit_or_ref: order 5: seq_floor: seed seq 450 is not above 500";
    assert_eq!(rig.refusal(), want);
    assert_eq!(rig.current(), Some(c(A)));
    assert!(!seeded.installed());
}

/// An order is judged against the floor as it stands once caught up with the release that runs,
/// not against what the file held at the last pass: the host runs a release that declares 500,
/// the file still says 100 (a confirm made by the operator since, a crash between a confirm and
/// the raise, a raise that failed), and the package carries 450. The order for it is refused
/// for the floor of 500, which the file says afterwards, and nothing is installed.
///
/// Catches a downgrade by a staged order in the window before the pass's own catch-up.
///
/// Mutant to plant: the catch-up removed from the admission of a staged order (the order is
/// admitted for 450 over 100 and the seed is installed).
#[test]
fn an_order_is_judged_against_a_floor_caught_up_with_the_release_that_runs() {
    let (mut rig, seeded, _) = noticing("notice-stale-floor", |s| {
        s.manifest["seq"] = serde_json::json!(450);
        s.probe = s.probe.replace(&format!("\"seq\":{SEQ},"), "\"seq\":450,");
        s.set_earlier_seq(500);
    });
    seeded.set_floor(100);
    rig.put(&staged(&rig, 5, Verb::Activate, B));
    rig.cycle(T0);
    let want = "commit_or_ref: order 5: seq_floor: seed seq 450 is not above 500";
    assert_eq!(rig.refusal(), want);
    assert_eq!(seeded.floor_text(), line(500));
    assert_eq!(rig.current(), Some(c(A)));
    assert!(!seeded.installed());
}

/// The look makes its own scratch directory and touches no other: a `work/<commit>.check` of
/// the seed's commit, which a stage in another process (`setup`, an operator's verb) may be
/// using, is there with what it holds after a pass, and the pass's own scratch is gone.
///
/// Catches a look that clears a stage's checks from under it, which fails closed but fails.
///
/// Mutant to plant: the look made in `work/<commit>.check` (the other directory is removed).
#[test]
fn a_look_leaves_the_checks_of_a_stage_alone() {
    let (mut rig, _, _) = noticing("notice-scratch", |_| {});
    let work = rig.layout.join(&rig.layout.work_dir());
    let theirs = work.join(format!("{B}.check"));
    fs::create_dir_all(theirs.join("gh")).expect("a stage's checks");
    fs::write(theirs.join("gh/kept"), b"in use").expect("a file of theirs");
    rig.cycle(T0);
    let told = rig.status().get("available").is_some();
    assert!(told, "{}", said(&mut rig));
    assert_eq!(fs::read(theirs.join("gh/kept")).expect("kept"), b"in use");
    assert_eq!(listing(&work), vec![format!("{B}.check")]);
}

/// A package that fails its look is not read in full at every pass: a bundle `gh` refuses is
/// checked once, not again at the next passes inside the minute, and again after it (the
/// package may have been repaired), the complaint said once all the same.
///
/// Catches a tarball and a `gh` run every five seconds for a package that stays broken, which
/// holds up the status of the orders behind it.
///
/// Mutants to plant: the wait removed (the second pass runs `gh` again); the wait made for ever
/// (the third pass does not).
#[test]
fn a_package_that_fails_its_look_is_looked_at_again_only_after_a_minute() {
    let (mut rig, _, calls) = noticing("notice-backoff", |s| {
        s.attested = b"the bundle of another release\n".to_vec();
    });
    rig.cycle(T0);
    let first = gh_runs(&calls);
    assert_ne!(first, 0, "gh was asked");
    assert!(said(&mut rig).starts_with("package: fetch: rejected: attestation: "));
    rig.cycle(T0 + 5);
    assert_eq!(gh_runs(&calls), first, "not again at the next pass");
    rig.cycle(T0 + 59);
    assert_eq!(gh_runs(&calls), first, "not before the minute is up");
    rig.cycle(T0 + 60);
    assert_eq!(gh_runs(&calls), 2 * first, "again after it");
    assert_eq!(said(&mut rig), "", "said once");
    assert!(rig.status().get("available").is_none());
}

/// The floor rises when a swap is confirmed and not before: a staged swap to the seed leaves
/// it where it was while the swap is on trial (a trial that is reverted must not burn the seed),
/// the confirm raises it to the seed's `seq`, after which the seed is not announced (it is the
/// release that runs), and a rollback to the release before leaves it at the seed's `seq`: the
/// release just rolled back from stays unorderable.
///
/// Catches a floor raised at the swap, one never raised once the swap is confirmed (a seed
/// older than a confirmed release stays orderable), and one lowered by a rollback.
///
/// Mutants to plant: the floor caught up while the swap is on trial (red on the floor of the
/// trial); the raise at the confirm and the catch-up at the end of the pass both removed (red
/// on the floor after the confirm; either alone leaves this test green, since the other makes
/// the same floor); the floor set to the `seq` of the release that runs after a rollback. The
/// raise at the confirm alone is the one
/// `a_floor_that_cannot_be_raised_after_a_confirm_is_said_and_the_confirm_stands` sees go red.
#[test]
fn the_floor_rises_at_a_confirm_not_at_a_swap_and_a_rollback_leaves_it() {
    let (mut rig, seeded, _) = noticing("notice-confirm", |_| {});
    rig.cycle(T0);
    assert_eq!(seeded.floor_text(), line(EARLIER_SEQ));

    rig.put(&staged(&rig, 5, Verb::Activate, B));
    rig.cycle(T0 + 5);
    assert_eq!(rig.current(), Some(c(B)), "{}", rig.refusal());
    assert!(rig.state().pending_confirm.is_some());
    let on_trial = seeded.floor_text();
    assert_eq!(on_trial, line(EARLIER_SEQ), "a trial raises nothing");

    rig.first_start(B, T0 + 6);
    rig.order(6, Verb::Confirm, B);
    rig.cycle(T0 + 8);
    assert!(rig.state().pending_confirm.is_none());
    assert_eq!(seeded.floor_text(), line(SEQ));
    let told = rig.status().get("available").is_some();
    assert!(!told, "the seed is the release that runs");

    let layout = rig.layout.clone();
    let lock = Locked::acquire(&layout).expect("lock");
    let state = lock.read().expect("reads").expect("there");
    let mut out = Vec::<u8>::new();
    let rolled = activate::rollback_in(rig.helper.exec_mut(), &layout, &lock, state, &mut out);
    rolled.expect("rolled back");
    drop(lock);
    rig.cycle(T0 + 20);
    assert_eq!(rig.current(), Some(c(A)));
    let after = seeded.floor_text();
    assert_eq!(after, line(SEQ), "a rollback leaves the floor");
    let told = rig.status().get("available").is_some();
    assert!(!told, "the seed rolled back from stays unorderable");
}

/// The frontend's flip raises the floor and its staging does not: a staged seed leaves the
/// floor where it was, and the confirm that makes it current raises it to the seed's `seq`.
///
/// Catches a floor raised by the stage (a release staged and dropped would burn the seed) and
/// one the frontend's confirm never raises.
///
/// Mutants to plant: the floor raised to the `seq` of the release that is staged (red on the
/// floor after the stage); the raise at the flip and the catch-up at the end of the pass both
/// removed (red on the floor after the flip). Each of the two alone leaves this test green,
/// since the other makes the same floor: the raise at the flip alone is the one
/// `a_floor_that_cannot_be_raised_after_a_flip_is_said_and_the_flip_stands` sees go red (the
/// failure is said when the flip raises, and the pass's own catch-up says it only once).
#[test]
fn a_frontend_flip_raises_the_floor_and_staging_does_not() {
    let mut rig = Rig::frontend("notice-flip");
    let seeded = Seeded::on(rig.layout.clone(), rig.root.clone(), B);
    rig.helper.set_fetcher(Box::new(seeded.fetcher()));
    rig.cycle(T0);
    assert_eq!(seeded.floor_text(), line(EARLIER_SEQ));

    rig.put(&staged(&rig, 9, Verb::FrontendUpdate, B));
    rig.cycle(T0 + 5);
    assert_eq!(rig.state().staged, Some(c(B)), "{}", rig.refusal());
    let staged_floor = seeded.floor_text();
    assert_eq!(staged_floor, line(EARLIER_SEQ), "staging raises nothing");

    rig.order(10, Verb::Confirm, B);
    rig.cycle(T0 + 10);
    assert_eq!(rig.current(), Some(c(B)), "{}", rig.refusal());
    assert_eq!(seeded.floor_text(), line(SEQ));
}

/// A release that declares no `seq` (a build before the field) leaves the floor unset, and a
/// host with no floor announces nothing and says so, once; the order is refused the same way.
///
/// Catches an unset floor taken for 0 and a seed announced to a host that cannot judge it.
///
/// Mutants to plant: a release with no `seq` taken as `seq` 0 (the floor becomes 0); the
/// announcement made when there is no floor.
#[test]
fn a_release_that_declares_no_seq_leaves_the_floor_unset_and_nothing_is_announced() {
    let declared = format!("\"seq\":{EARLIER_SEQ},");
    let (mut rig, seeded, _) = noticing("notice-no-seq", |s| {
        s.earlier = s.earlier.replace(&declared, "");
    });
    rig.cycle(T0);
    rig.cycle(T0 + 5);
    assert_eq!(seeded.floor_text(), None);
    assert!(rig.status().get("available").is_none());
    let want = "package: seq_floor: no floor is recorded yet, so no seed is above it\n";
    assert_eq!(said(&mut rig), want);
}

/// The release that runs is asked for its `seq` once, not at every pass; the source is told in
/// the status as the fetcher reports it.
///
/// Catches a binary run every five seconds, and a source left out of the status.
///
/// Mutants to plant: the answer not remembered (two questions); the source left out.
#[test]
fn the_release_that_runs_is_asked_once_and_the_source_is_told() {
    let mut rig = Rig::new("notice-fake");
    let seeded = Seeded::on(rig.layout.clone(), rig.root.clone(), B);
    *rig.fetcher.seed.lock().expect("lock") = Some(SeedFacts {
        commit: c(B),
        seq: SEQ,
        facts: seeded.facts(),
    });
    *rig.fetcher.source.lock().expect("lock") = Some("bundled");
    let version = declares(A, EARLIER_SEQ);
    rig.fetcher
        .versions
        .lock()
        .expect("lock")
        .insert(c(A), version);
    rig.cycle(T0);
    rig.cycle(T0 + 5);
    let status = rig.status();
    assert_eq!(status["release_source"], "bundled");
    assert_eq!(
        status["available"],
        serde_json::json!({"commit": B, "seq": SEQ})
    );
    assert_eq!(seeded.floor_text(), line(EARLIER_SEQ));
    assert_eq!(*rig.fetcher.probes.lock().expect("lock"), vec![c(A)]);
}

/// A release that cannot be asked leaves the floor unset and is asked again at the next pass
/// (a failure is not remembered), the trouble is said once, and nothing is announced.
///
/// Catches a failed question remembered for ever, and one said at every pass.
///
/// Mutants to plant: the failure stored with the answer; the complaint said at every pass.
#[test]
fn a_release_that_cannot_be_asked_is_asked_again_and_the_trouble_is_said_once() {
    let mut rig = Rig::new("notice-probe-fails");
    let seeded = Seeded::on(rig.layout.clone(), rig.root.clone(), B);
    *rig.fetcher.seed.lock().expect("lock") = Some(SeedFacts {
        commit: c(B),
        seq: SEQ,
        facts: seeded.facts(),
    });
    let broken = "it prints no JSON".to_owned();
    let mut versions = rig.fetcher.versions.lock().expect("lock");
    versions.insert(c(A), broken);
    drop(versions);
    rig.cycle(T0);
    rig.cycle(T0 + 5);
    assert_eq!(seeded.floor_text(), None);
    assert!(rig.status().get("available").is_none());
    let log = said(&mut rig);
    let want = format!("package: seq_floor: cannot ask {A} for its seq: ");
    assert!(log.starts_with(&want), "{log}");
    assert_eq!(log.matches("package:").count(), 1, "{log}");
    assert_eq!(rig.fetcher.probes.lock().expect("lock").len(), 2);
}

/// A floor that cannot be read (the file is a directory) withholds the announcement and is
/// said, and the floor is left as it is.
///
/// Catches a floor that cannot be read taken for one that is not there.
///
/// Mutant to plant: the read error mapped to `None` (the floor is made over the directory's
/// place or the seed announced).
#[test]
fn a_floor_that_cannot_be_read_withholds_the_announcement_and_is_said() {
    let (mut rig, _, _) = noticing("notice-floor-dir", |_| {});
    fs::create_dir(rig.layout.join(&rig.layout.seq_floor())).expect("a directory");
    rig.cycle(T0);
    assert!(rig.status().get("available").is_none());
    let log = said(&mut rig);
    assert!(log.starts_with("package: "), "{log}");
    assert!(log.contains("cannot be read"), "{log}");
    assert!(rig.layout.join(&rig.layout.seq_floor()).is_dir());
}

/// A floor that cannot be raised after a confirm is said at once, and the confirm stands: the
/// swap is kept, and the next pass tries again.
///
/// Catches a confirm undone, or a failure to raise the floor passed over without a word.
///
/// Mutants to plant: the error of the raise dropped; the confirm refused when the raise fails;
/// the raise dropped from the confirm (nothing is said: the pass's own catch-up has no seed to
/// complain of, and it is not asked to say what the confirm's raise has).
#[test]
fn a_floor_that_cannot_be_raised_after_a_confirm_is_said_and_the_confirm_stands() {
    let mut rig = Rig::new("notice-confirm-floor-dir");
    rig.fetcher
        .versions
        .lock()
        .expect("lock")
        .insert(c(B), declares(B, SEQ));
    fs::create_dir(rig.layout.join(&rig.layout.seq_floor())).expect("a directory");
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0);
    rig.first_start(B, T0 + 1);
    let _ = said(&mut rig);
    rig.order(6, Verb::Confirm, B);
    rig.cycle(T0 + 3);
    let pending = rig.state().pending_confirm;
    assert!(pending.is_none(), "the confirm stands");
    assert_eq!(rig.current(), Some(c(B)));
    let log = said(&mut rig);
    assert!(log.contains("cannot be read"), "{log}");
}

/// The same for the frontend's flip: a floor that cannot be raised is said at once, and the
/// release stays current.
///
/// Catches a flip undone, or a failure to raise the floor passed over without a word.
///
/// Mutants to plant: the error of the raise dropped; the flip refused when the raise fails; the
/// raise dropped from the flip (nothing is said).
#[test]
fn a_floor_that_cannot_be_raised_after_a_flip_is_said_and_the_flip_stands() {
    let mut rig = Rig::frontend("notice-flip-floor-dir");
    rig.fetcher
        .versions
        .lock()
        .expect("lock")
        .insert(c(B), declares(B, SEQ));
    rig.order(9, Verb::FrontendUpdate, B);
    rig.cycle(T0);
    assert_eq!(rig.state().staged, Some(c(B)), "{}", rig.refusal());
    fs::create_dir(rig.layout.join(&rig.layout.seq_floor())).expect("a directory");
    let _ = said(&mut rig);
    rig.order(10, Verb::Confirm, B);
    rig.cycle(T0 + 5);
    assert_eq!(rig.current(), Some(c(B)), "{}", rig.refusal());
    let log = said(&mut rig);
    assert!(log.contains("cannot be read"), "{log}");
}

/// A package without a seed is a host in its normal state: nothing is announced, nothing is
/// said, nothing is run for it, and the floor is kept all the same (the release that runs sets
/// it).
///
/// Catches a host with no seed complaining at every pass, and a floor that is kept only where a
/// seed is carried (a seed arriving later would find none).
///
/// Mutants to plant: the absent seed directory reported as missing; the floor kept only when a
/// seed is found.
#[test]
fn a_package_without_a_seed_says_nothing_and_the_floor_is_kept_all_the_same() {
    let mut rig = Rig::new("notice-no-seed");
    let seeded = Seeded::on(rig.layout.clone(), rig.root.clone(), B);
    let runner = seeded.runner();
    let calls = runner.log();
    let bare = crate::fixtures::trusted_host_on(rig.layout.platform());
    let mut fetcher = seeded.fetcher_on(bare);
    fetcher.runner = Box::new(runner);
    rig.helper.set_fetcher(Box::new(fetcher));
    rig.cycle(T0);
    assert!(rig.status().get("available").is_none());
    assert_eq!(said(&mut rig), "");
    assert_eq!(gh_runs(&calls), 0);
    assert_eq!(seeded.floor_text(), line(EARLIER_SEQ));
}

/// A package upgrade that replaces the seed (a newer manifest, tarball and binary under the
/// same directory) is noticed at the next pass: the new manifest's digest is not the one the
/// old seed was found by, so the package is checked again and the new `seq` is told. Nothing
/// of the host's state is touched by the upgrade or by noticing it.
///
/// Catches a seed announced for ever as the first one found (the cache keyed by anything but
/// the manifest's digest), and an upgrade that writes the state.
///
/// Mutants to plant: the cache returned whatever the digest; the new seed announced without
/// being checked (red on the second runner's `gh` runs).
#[test]
fn a_package_upgrade_is_noticed_at_the_next_pass_and_touches_no_state() {
    let (mut rig, mut seeded, _) = noticing("notice-upgrade", |_| {});
    let state = state_bytes(&rig);
    rig.cycle(T0);
    assert_eq!(rig.status()["available"]["seq"], SEQ);

    let newer = SEQ + 30;
    seeded.manifest["seq"] = serde_json::json!(newer);
    let old = format!("\"seq\":{SEQ},");
    let new = format!("\"seq\":{newer},");
    seeded.probe = seeded.probe.replace(&old, &new);
    let runner = seeded.runner();
    let calls = runner.log();
    let mut fetcher = seeded.fetcher();
    fetcher.runner = Box::new(runner);
    rig.helper.set_fetcher(Box::new(fetcher));
    rig.cycle(T0 + 5);
    assert_eq!(rig.status()["available"]["seq"], newer);
    assert_eq!(gh_runs(&calls), 1, "the new manifest is checked");
    assert_eq!(state_bytes(&rig), state, "state.json is what it was");
    assert_eq!(rig.current(), Some(c(A)));
}

/// The host fetcher tells the source `installer.toml` names, and the seed is announced on a
/// host whatever its source: `bundled` and `url` alike.
///
/// Catches the announcement made only where nothing is fetched, and the source left to the
/// frontend to guess.
///
/// Mutants to plant: the announcement made only on a `bundled` host; the source not put in the
/// status.
#[test]
fn a_host_tells_its_source_and_announces_its_seed_whatever_the_source() {
    for (file, word) in [(BUNDLED, "bundled"), (URL, "url")] {
        let mut rig = Rig::new(&format!("notice-source-{word}"));
        let seeded = Seeded::on(rig.layout.clone(), rig.root.clone(), B);
        rig.helper.set_fetcher(Box::new(seeded.host_fetcher(file)));
        rig.cycle(T0);
        let status = rig.status();
        assert_eq!(status["release_source"], word);
        assert_eq!(
            status["available"],
            serde_json::json!({"commit": B, "seq": SEQ}),
            "{word}"
        );
    }
}
