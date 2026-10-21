//! The picker's gate on where a run's release comes from (`update/pick.rs`, `Skip`): a staged
//! run orders only the hosts whose package carries the release, a fetch run orders every
//! host that fetches, and a host the source cannot reach stays in the picker's answer by
//! name. The rest of the picker is in `update_pick.rs` and `update_places.rs`; the node
//! step's own recheck of the package is in `update_node_source.rs`.
//!
//! Each test names the defect it would catch. Each 'Mutants it catches' line names a change
//! that turns the test red.

#[path = "update_support/mod.rs"]
mod support;

use kbf_server::update::{Facts, HostPhase, Pick, Picked, Release, Skip, Source, WaitReason, pick};
use kbf_server::update_status::ReleaseSource;
use support::{NEW, NOW, facts, hold, host_mut, id, linux, members, package, policy, storage, t};

/// A release no host in these tests is driven to.
const OTHER: &str = "v3";

fn pick_now(f: &Facts) -> Pick {
    pick(f, &policy(), t(NOW))
}

/// The answer that starts `name` alone.
fn go(name: &str) -> Pick {
    Pick::Hosts(vec![Picked {
        host: id(name),
        force: false,
    }])
}

fn wait(why: WaitReason) -> Pick {
    Pick::Wait { why }
}

/// The answer that starts none because each of these hosts is skipped, in this order.
fn none_eligible(skipped: &[(&str, Skip)]) -> Pick {
    let named = skipped.iter().map(|(n, why)| (id(n), why.clone()));
    wait(WaitReason::NoneEligible(named.collect()))
}

/// A host that reports packages and whose package carries `commit`.
fn has(commit: &str) -> Skip {
    Skip::NotAvailable {
        has: Some(Release::new(commit)),
    }
}

/// A host that reports packages and carries none.
fn lacks() -> Skip {
    Skip::NotAvailable { has: None }
}

/// Done in the run: the host is no candidate.
fn finish(f: &mut Facts, name: &str) {
    host_mut(f, name).record.phase = HostPhase::Done;
}

// A run whose every candidate is skipped.

/// Catches: a skipped host dropped from the candidates, so that a run which is only waiting
/// for packages (or only has hosts that fetch nothing) reads "nothing to update" and a
/// controller would call the farm done. Both sources: staged with one host that reports no
/// packages and one that carries another release, and fetch with only bundled hosts. Each
/// answer names its hosts, in the order given.
///
/// Mutants it catches: the skipped hosts filtered out of `candidates`.
#[test]
fn a_run_whose_every_candidate_is_skipped_is_none_eligible_not_nothing_to_update() {
    let mut f = facts(vec![linux("d1"), linux("d2")], members(3));
    f.source = Source::Staged;
    package(&mut f, "d2", OTHER, ReleaseSource::Bundled);
    let skipped = [("d1", Skip::InstallerTooOld), ("d2", has(OTHER))];
    assert_eq!(pick_now(&f), none_eligible(&skipped));

    let mut f = facts(vec![linux("d1"), linux("d2")], members(3));
    package(&mut f, "d1", NEW, ReleaseSource::Bundled);
    package(&mut f, "d2", NEW, ReleaseSource::Bundled);
    let skipped = [("d1", Skip::Bundled), ("d2", Skip::Bundled)];
    assert_eq!(pick_now(&f), none_eligible(&skipped));
}

// A staged run.

/// Catches: a staged run ordered on a host whose package is not the release: `d1` carries
/// another release and `d2` reports packages and carries none, so the host that would sort
/// first is skipped and `d3` is the one started. With `d3` finished the run waits, and
/// names what each of the others has.
///
/// Mutants it catches: the gate removed (`d1` is started); `has` dropped from the skip (the
/// second answer names no release).
#[test]
fn a_staged_run_starts_only_a_host_whose_package_carries_the_target() {
    let mut f = facts(vec![linux("d1"), linux("d2"), linux("d3")], members(3));
    f.source = Source::Staged;
    package(&mut f, "d1", OTHER, ReleaseSource::Bundled);
    package(&mut f, "d3", NEW, ReleaseSource::Bundled);
    host_mut(&mut f, "d2").release_source = ReleaseSource::Bundled;
    assert_eq!(pick_now(&f), go("d3"));

    finish(&mut f, "d3");
    let skipped = [("d1", has(OTHER)), ("d2", lacks())];
    assert_eq!(pick_now(&f), none_eligible(&skipped));
}

/// Catches: the package compared by its first twelve digits, as the daemon reports the
/// running commit. A package that shares twelve digits with the target and differs after
/// them is another release, and is skipped naming what it carries; the same host carrying
/// the whole commit is started.
///
/// Mutants it catches: a prefix compare of the two commits.
#[test]
fn a_package_that_shares_twelve_digits_with_the_target_is_not_the_target() {
    let target = "0123456789abcdef0123456789abcdef01234567";
    let sibling = format!("{}{}", &target[..12], "f".repeat(28));
    let mut f = facts(vec![linux("d1")], members(3));
    f.source = Source::Staged;
    f.target = Release::new(target);
    package(&mut f, "d1", &sibling, ReleaseSource::Bundled);
    let skipped = [("d1", has(&sibling))];
    assert_eq!(pick_now(&f), none_eligible(&skipped));

    package(&mut f, "d1", target, ReleaseSource::Bundled);
    assert_eq!(pick_now(&f), go("d1"));
}

/// Catches: an installer that reports no packages at all called "not available". It could
/// carry the target and not show it, which is a different thing to tell an operator (upgrade
/// the installer) from a package that is missing. The word is read before the package is: a
/// host that says nothing of its source is too old even when a package shows. Whatever else
/// a package-reporting host says it fetches from, a staged run does not care.
///
/// Mutants it catches: the package compared before the source word is read (`d1` reads
/// "not available", `d2` is started); the source word ignored (`d1` reads "not available").
#[test]
fn an_installer_that_reports_no_packages_is_too_old_and_not_unavailable() {
    let mut f = facts(vec![linux("d1"), linux("d2"), linux("d3")], members(3));
    f.source = Source::Staged;
    package(&mut f, "d2", NEW, ReleaseSource::Unreported);
    package(&mut f, "d3", NEW, ReleaseSource::Frontend);
    assert_eq!(pick_now(&f), go("d3"));

    finish(&mut f, "d3");
    let old = Skip::InstallerTooOld;
    let skipped = [("d1", old.clone()), ("d2", old)];
    assert_eq!(pick_now(&f), none_eligible(&skipped));
}

/// Catches: the gate run after the places are shared out, or on the hosts that are left
/// after them. `s1` is a storage host that carries nothing and the one storage place is held
/// by `s0`: it is skipped for its package, not for the place, and a daemon-only host that
/// carries the target goes on at once. With that host finished, the only thing left is
/// `s1`'s package, which the answer says; a run that said "waiting for the place of `s0`"
/// would hide the host that needs a package.
///
/// Mutants it catches: the candidates split by place before the gate runs (the second
/// answer is `SlotHeld` for `s0`).
#[test]
fn a_skipped_host_takes_no_place_and_is_not_hidden_behind_a_full_one() {
    let hosts = vec![storage("s0", 1), storage("s1", 2), linux("d1")];
    let mut f = facts(hosts, members(3));
    f.source = Source::Staged;
    hold(&mut f, "s0");
    host_mut(&mut f, "s0").record.phase = HostPhase::Selected;
    host_mut(&mut f, "s1").release_source = ReleaseSource::Bundled;
    package(&mut f, "d1", NEW, ReleaseSource::Bundled);
    assert_eq!(pick_now(&f), go("d1"));

    finish(&mut f, "d1");
    let skipped = [("s1", lacks())];
    assert_eq!(pick_now(&f), none_eligible(&skipped));
}

// A fetch run.

/// Catches: a fetch run that orders a host whose installer fetches nothing (the installer
/// would deny the order), or that stops at it: `d1` is bundled and sorts first, the run goes
/// on to `d2`, and once `d2` is done the answer names `d1` as skipped rather than holding the
/// run or calling it finished.
///
/// Mutants it catches: a bundled host ordered (`d1` is started); a bundled host holding the
/// run (the first answer is a wait).
#[test]
fn a_fetch_run_skips_a_bundled_host_and_goes_on_to_the_next() {
    let mut f = facts(vec![linux("d1"), linux("d2")], members(3));
    package(&mut f, "d1", NEW, ReleaseSource::Bundled);
    assert_eq!(pick_now(&f), go("d2"));

    finish(&mut f, "d2");
    let skipped = [("d1", Skip::Bundled)];
    assert_eq!(pick_now(&f), none_eligible(&skipped));
}

/// Catches: a fetch run that reads the package, or turns a host away for not saying where it
/// fetches from. A url host whose package carries another release, a frontend host with no
/// package, and a host that says nothing at all (an installer that predates the report) are
/// each ordered, in turn, and the run ends when they are done.
///
/// Mutants it catches: the staged test applied to a fetch run (`d1` and `d2` skipped); an
/// unreported host skipped as `Skip::InstallerTooOld` on a fetch run.
#[test]
fn a_fetch_run_orders_a_url_a_frontend_and_an_unreported_host_whatever_their_packages() {
    let mut f = facts(vec![linux("d1"), linux("d2"), linux("d3")], members(3));
    package(&mut f, "d1", OTHER, ReleaseSource::Url);
    host_mut(&mut f, "d2").release_source = ReleaseSource::Frontend;
    for name in ["d1", "d2", "d3"] {
        assert_eq!(pick_now(&f), go(name));
        finish(&mut f, name);
    }
    assert_eq!(pick_now(&f), wait(WaitReason::NothingToUpdate));
}
