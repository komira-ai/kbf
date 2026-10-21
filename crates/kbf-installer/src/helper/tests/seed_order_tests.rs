//! Orders whose source is `staged`, through the real cycle over a scratch host: the release is
//! the one the host's package carries, and the fetcher is the seed fetcher (the real verify
//! pipeline over the package's files, with `gh` and the binary's report played by a fake
//! runner) or the host fetcher reading an `installer.toml`. What each test proves is named on
//! it, with the mutant to plant to turn it red.

use super::*;
use crate::seed::SeedFacts;
use crate::seed_fixture::{EARLIER_SEQ, SEQ, Seeded};

const BUNDLED: &str = "release_source = \"bundled\"\n";

/// A commit that shares the first 12 digits with `B` and differs after them.
fn same_build_other_commit() -> String {
    format!("{}{}", &B[..12], "f".repeat(28))
}

/// The record of a staged `verb` on `commit`: a locator, with the source named.
fn staged(rig: &Rig, id: u64, verb: Verb, commit: &str) -> OrderRecord {
    let mut r = rig.rec(id, 1, 1, verb, commit);
    r.components.clear();
    r.expected = None;
    r.source = Source::Staged;
    r
}

/// A node host running A whose fetcher is the seed fetcher over the seed of B, which `plant`
/// has changed.
fn seeded_node(name: &str, plant: impl FnOnce(&mut Seeded)) -> (Rig, Seeded) {
    let mut rig = Rig::new(name);
    let mut seeded = Seeded::on(rig.layout.clone(), rig.root.clone(), B);
    plant(&mut seeded);
    rig.helper.set_fetcher(Box::new(seeded.fetcher()));
    (rig, seeded)
}

/// The `refusal` word of `status.json`: what precedes the first colon.
fn word(rig: &Rig) -> String {
    rig.refusal().split(':').next().unwrap_or("").to_owned()
}

/// A staged activation installs the seed through the pipeline, swaps to it on trial, restarts
/// the unit once and reports the facts the binary declared; the record is removed. The floor
/// is below the seed's `seq`.
///
/// Catches a staged order that is not carried out (it is refused for having no claims, as an
/// activation without them is), a seed activated without being installed, and a swap with no
/// restart owed.
///
/// Mutants to plant: the staged branch removed from the vetting (the order has no claims); the
/// facts of the seed not passed on (the status lacks them).
#[test]
fn a_staged_activation_installs_the_seed_and_swaps_to_it_on_trial() {
    let (mut rig, seeded) = seeded_node("seedcyc-activate", |_| {});
    seeded.set_floor(SEQ - 1);
    rig.put(&staged(&rig, 5, Verb::Activate, B));
    rig.cycle(T0);
    assert_eq!(rig.refusal(), "");
    assert_eq!(rig.current(), Some(c(B)));
    assert_eq!(link(&rig, "previous"), PathBuf::from(A));
    assert!(rig.state().pending_confirm.is_some());
    assert_eq!(rig.status()["state"], "activated_unconfirmed");
    assert_eq!(rig.status()["staged"]["protocol"], 4);
    assert_eq!(rig.status()["acked_order_id"], 5);
    assert_eq!(rig.restarts(), 1);
    assert!(seeded.installed());
    assert_eq!(rig.left(), 0);
}

/// An order that fetches, on a host whose `installer.toml` says `bundled`, is denied: the
/// refusal says `denied` and why, the floor moves (the order was acted on), nothing is
/// installed, and the fetch child never starts (it is a program that does not exist: had the
/// fetch reached it, the refusal would be about that). The record carries no source, as every
/// record did before the key.
///
/// Catches a bundled host that fetches (an air-gapped host reaching for a network, or a
/// release the package does not carry arriving), and a refusal made after the fetch began.
///
/// Mutants to plant: the gate in the host fetcher moved after the fetch child is asked for the
/// description (the refusal is about the child); the `bundled` arm made a `url` source.
#[test]
fn an_order_that_fetches_on_a_bundled_host_is_denied_before_any_child_and_moves_the_floor() {
    let mut rig = Rig::new("seedcyc-bundled-fetch");
    let seeded = Seeded::on(rig.layout.clone(), rig.root.clone(), B);
    rig.helper
        .set_fetcher(Box::new(seeded.host_fetcher(BUNDLED)));
    rig.order(5, Verb::Activate, B);
    rig.cycle(T0);
    assert_eq!(
        rig.refusal(),
        "denied: order 5: release_source is bundled; source=fetch is refused (use source=staged)"
    );
    assert_eq!(rig.status()["acked_order_id"], 5, "the floor moved");
    assert_eq!(rig.current(), Some(c(A)));
    assert_eq!(rig.restarts(), 0);
    assert!(!seeded.installed());
    assert_eq!(rig.left(), 0);
}

/// The same for the frontend's own update: a `frontend_update` that fetches, on a bundled
/// host, is denied with the word `denied`, stages nothing and moves the floor.
///
/// Catches the gate placed only in the node's activation.
///
/// Mutant to plant: the denial of a fetch handled in `prepare` alone.
#[test]
fn a_frontend_update_that_fetches_on_a_bundled_host_is_denied_and_stages_nothing() {
    let mut rig = Rig::frontend("seedcyc-bundled-update");
    let seeded = Seeded::on(rig.layout.clone(), rig.root.clone(), B);
    rig.helper
        .set_fetcher(Box::new(seeded.host_fetcher(BUNDLED)));
    rig.order(9, Verb::FrontendUpdate, B);
    rig.cycle(T0);
    assert_eq!(word(&rig), "denied", "{}", rig.refusal());
    assert_eq!(
        rig.refusal(),
        "denied: order 9: release_source is bundled; source=fetch is refused (use source=staged)"
    );
    assert_eq!(rig.state().staged, None);
    assert_eq!(rig.status()["acked_order_id"], 9);
    assert!(!seeded.installed());
}

/// A source that is neither word is not an order: the record (otherwise a good activation, so
/// that reading the source as `fetch` would carry it out) is refused naming the word, removed,
/// and moves no floor: nothing is fetched and a real order of a lower id still runs.
///
/// Catches an unknown source read as the default, which would turn a typo or a newer sender's
/// word into a fetch.
///
/// Mutant to plant: an unknown variant read as `Fetch` (`#[serde(other)]`).
#[test]
fn a_source_that_is_neither_word_is_not_an_order_and_moves_no_floor() {
    let mut rig = Rig::new("seedcyc-bogus");
    let text = serde_json::to_string(&rig.rec(9, 1, 1, Verb::Activate, B)).expect("json");
    let open = text.strip_suffix('}').expect("an object");
    rig.put_raw(1, 9, format!("{open},\"source\":\"bogus\"}}").as_bytes());
    rig.cycle(T0);
    assert_eq!(rig.status()["state"], "refused");
    assert!(
        rig.refusal().contains("not an order") && rig.refusal().contains("bogus"),
        "{}",
        rig.refusal()
    );
    assert_eq!(rig.status()["acked_order_id"], 0, "the floor did not move");
    assert_eq!(rig.fetcher.count(), 0);
    assert_eq!(rig.left(), 0);
    rig.order(8, Verb::Activate, B);
    rig.cycle(T0 + 3);
    assert_eq!(rig.current(), Some(c(B)), "{}", rig.refusal());
}

/// A staged record that takes more than a locator, or a `confirm` that names a source, is not
/// an order, and an activation that names no source and no components is still one that names
/// no component: each is refused with its reason, removed, and moves no floor.
///
/// Catches a staged activation that also carries the release it would have fetched, a source
/// on a verb that moves no release, and the empty activation passing as a staged one.
///
/// Mutants to plant: components allowed beside a staged source; the source allowed on a
/// `confirm`; the staged arm taking an activation that has the key absent.
#[test]
fn a_staged_record_with_more_than_a_locator_is_not_an_order() {
    let mut rig = Rig::new("seedcyc-shapes");
    let mut with_components = staged(&rig, 9, Verb::Activate, B);
    with_components.components = rig.rec(9, 1, 1, Verb::Activate, B).components;
    let confirm = staged(&rig, 10, Verb::Confirm, B);
    let mut empty = rig.rec(11, 1, 1, Verb::Activate, B);
    empty.components.clear();
    empty.expected = None;
    let cases = [
        (with_components, "does not take"),
        (confirm, "does not take"),
        (empty, "names no release component"),
    ];
    for (record, want) in cases {
        rig.put(&record);
        rig.cycle(T0);
        assert!(
            rig.refusal().contains("not an order") && rig.refusal().contains(want),
            "{}: {}",
            record.order_id,
            rig.refusal()
        );
        assert_eq!(rig.status()["acked_order_id"], 0, "{}", record.order_id);
        assert_eq!(rig.left(), 0);
    }
    assert_eq!(rig.fetcher.count(), 0);
    rig.order(8, Verb::Activate, B);
    rig.cycle(T0 + 3);
    assert_eq!(rig.current(), Some(c(B)), "{}", rig.refusal());
}

/// A staged order that does not name the seed's commit, or whose seed's `seq` is not above the
/// floor, is refused before any work is done: `commit_or_ref`, saying `seed_mismatch` or
/// `seq_floor` and the numbers, the floor moves (the order was acted on), and nothing is
/// installed, swapped or restarted. The cases are another commit, a commit that shares the 12
/// digits that name the seed directory, a host with no floor, a floor equal to the seed's
/// `seq` and one above it.
///
/// Catches a commit judged by its first 12 digits, a floor that admits an equal `seq` (the
/// release that is running could be staged again), a seed older than the floor admitted, and
/// a host with no floor taken for a floor of 0.
///
/// Mutants to plant: the comparison on 12 digits; `>=` in place of `>` in the floor rule;
/// `None` read as `Some(0)`; the rule applied after the fetch (the seed is installed).
#[test]
fn a_staged_order_is_refused_for_another_commit_or_a_seq_not_above_the_floor() {
    let other = same_build_other_commit();
    let head = "commit_or_ref: order 5:";
    let mismatch = format!("{head} seed_mismatch: this host carries {B}, not");
    let below = format!("{head} seq_floor: seed seq {SEQ} is not above");
    // Whether the release that runs declares a `seq`: one that does keeps the floor at least at
    // it, so a host with no floor at an order is one whose release declares none.
    let cases: [(&str, &str, Option<u64>, bool, String); 5] = [
        (
            "another commit",
            C,
            Some(0),
            true,
            format!("{mismatch} {C}"),
        ),
        (
            "the same 12 digits",
            &other,
            Some(0),
            true,
            format!("{mismatch} {other}"),
        ),
        (
            "no floor",
            B,
            None,
            false,
            format!("{head} seq_floor: no floor is recorded yet, so no seed is above it"),
        ),
        (
            "a floor equal to the seq",
            B,
            Some(SEQ),
            true,
            format!("{below} {SEQ}"),
        ),
        (
            "a floor above the seq",
            B,
            Some(SEQ + 7),
            true,
            format!("{below} {}", SEQ + 7),
        ),
    ];
    for (index, (what, commit, floor, declares, want)) in cases.into_iter().enumerate() {
        let name = format!("seedcyc-refused-{index}");
        let (mut rig, seeded) = seeded_node(&name, |s| {
            if !declares {
                s.earlier = s.earlier.replace(&format!("\"seq\":{EARLIER_SEQ},"), "");
            }
        });
        if let Some(floor) = floor {
            seeded.set_floor(floor);
        }
        rig.put(&staged(&rig, 5, Verb::Activate, commit));
        rig.cycle(T0);
        assert_eq!(rig.refusal(), want, "{what}");
        assert_eq!(word(&rig), "commit_or_ref", "{what}");
        assert_eq!(rig.status()["acked_order_id"], 5, "{what}: the floor moved");
        assert_eq!(rig.current(), Some(c(A)), "{what}");
        assert_eq!(rig.restarts(), 0, "{what}");
        assert!(!seeded.installed(), "{what}: nothing was installed");
        assert_eq!(rig.left(), 0, "{what}");
    }
}

/// A staged order is refused before any work when the host cannot say what its package carries
/// (no manifest to read) or cannot read its floor (the file is not a file): the refusal says why,
/// the floor moves, and nothing is fetched or installed. A host that cannot tell is not a host
/// with no floor.
///
/// Catches a floor that cannot be read taken for no floor or for a floor of zero, and a
/// package that cannot be read taken for one that carries a release.
///
/// Mutants to plant: the read error of the floor mapped to `None`; the manifest's error ignored.
#[test]
fn a_staged_order_is_refused_when_the_package_or_the_floor_cannot_be_read() {
    let mut rig = Rig::new("seedcyc-no-package");
    rig.put(&staged(&rig, 5, Verb::Activate, B));
    rig.cycle(T0);
    let want = "order 5: fetch: this installer has no release fetcher wired in";
    assert_eq!(rig.refusal(), want);
    assert_eq!(rig.status()["acked_order_id"], 5);
    assert_eq!(rig.fetcher.count(), 0);

    let mut rig = Rig::new("seedcyc-bad-floor");
    let seeded = Seeded::on(rig.layout.clone(), rig.root.clone(), B);
    *rig.fetcher.seed.lock().expect("lock") = Some(SeedFacts {
        commit: c(B),
        seq: SEQ,
        facts: seeded.facts(),
    });
    fs::create_dir(rig.layout.join(&rig.layout.seq_floor())).expect("a directory");
    rig.put(&staged(&rig, 5, Verb::Activate, B));
    rig.cycle(T0);
    let why = rig.refusal();
    assert!(
        why.starts_with("order 5: fetch: seq_floor: ") && why.contains("cannot be read"),
        "{why}"
    );
    assert_eq!(rig.status()["acked_order_id"], 5);
    assert_eq!(rig.fetcher.count(), 0);
    assert_eq!(rig.current(), Some(c(A)));
}

/// A repeated order for the release that is running is idempotent, not a refusal that holds
/// the run: with B current and confirmed, the floor equal to B's `seq` (as it stands once a
/// release is the running one) and the package carrying B, a staged activation of B fetches
/// nothing, restarts nothing and refuses nothing.
///
/// Catches the floor judged before the shortcut for the release that is already running: the
/// repeat would be refused as a seed that is not above the floor, and a refused activation
/// holds the run.
///
/// Mutant to plant: the floor rule moved above the shortcut in the vetting.
#[test]
fn a_repeated_staged_order_for_the_running_commit_is_idempotent_not_refused() {
    let mut rig = Rig::new("seedcyc-idempotent");
    rig.order(1, Verb::Activate, B);
    rig.cycle(T0);
    rig.first_start(B, T0 + 1);
    rig.order(2, Verb::Confirm, B);
    rig.cycle(T0 + 3);
    assert_eq!(rig.current(), Some(c(B)), "{}", rig.refusal());
    assert!(rig.state().pending_confirm.is_none());

    let seeded = Seeded::on(rig.layout.clone(), rig.root.clone(), B);
    seeded.set_floor(SEQ);
    *rig.fetcher.seed.lock().expect("lock") = Some(SeedFacts {
        commit: c(B),
        seq: SEQ,
        facts: seeded.facts(),
    });
    let (fetches, restarts) = (rig.fetcher.count(), rig.restarts());
    rig.put(&staged(&rig, 3, Verb::Activate, B));
    rig.cycle(T0 + 6);
    assert_eq!(rig.refusal(), "");
    assert_eq!(rig.fetcher.count(), fetches, "nothing is fetched");
    assert_eq!(rig.restarts(), restarts, "nothing is restarted");
    assert_eq!(rig.current(), Some(c(B)));
    assert_eq!(rig.status()["acked_order_id"], 3);
}

/// A seed the pipeline refuses is not activated and not staged, whichever order asks: the
/// bundle `gh` does not accept ends a staged activation and a staged frontend update with the
/// pipeline's own word (`attestation`) in the refusal, nothing is installed, and `current` and
/// `staged` stay as they were. This is what a path that skipped `Pipeline::verify` would have
/// installed.
///
/// Catches the seed trusted because the package is root's: the package signature is not the
/// attestation, and only the pipeline checks the latter.
///
/// Mutants to plant: `Pipeline::verify` replaced by the install of the copied tarball in the
/// seed fetcher (both cases red).
#[test]
fn a_seed_the_pipeline_refuses_is_not_activated_or_staged() {
    let unattested = |s: &mut Seeded| s.attested = b"the bundle of another release\n".to_vec();

    let (mut rig, seeded) = seeded_node("seedcyc-unattested-node", unattested);
    seeded.set_floor(SEQ - 1);
    rig.put(&staged(&rig, 5, Verb::Activate, B));
    rig.cycle(T0);
    assert!(rig.refusal().contains("attestation"), "{}", rig.refusal());
    assert_eq!(rig.current(), Some(c(A)));
    assert_eq!(rig.restarts(), 0);
    assert!(!seeded.installed());

    let mut rig = Rig::frontend("seedcyc-unattested-frontend");
    let mut seeded = Seeded::on(rig.layout.clone(), rig.root.clone(), B);
    unattested(&mut seeded);
    seeded.set_floor(SEQ - 1);
    rig.helper.set_fetcher(Box::new(seeded.fetcher()));
    rig.put(&staged(&rig, 9, Verb::FrontendUpdate, B));
    rig.cycle(T0);
    assert_eq!(word(&rig), "stage", "{}", rig.refusal());
    assert!(rig.refusal().contains("attestation"), "{}", rig.refusal());
    assert_eq!(rig.state().staged, None);
    assert!(!seeded.installed());
}

/// A staged `frontend_update` stages the seed without flipping: the same checks as a fetched
/// one, `staged` names the commit, `current` and the links stay, no unit is restarted, and the
/// facts the binary declared appear under `staged{}` for the proxy.
///
/// Catches a staged frontend update that swaps, or that stages a release it did not verify.
///
/// Mutants to plant: the staged branch removed from the update (it fetches, which the host
/// refuses or cannot do); `current` written too.
#[test]
fn a_staged_frontend_update_stages_the_seed_without_flipping() {
    let mut rig = Rig::frontend("seedcyc-frontend");
    let seeded = Seeded::on(rig.layout.clone(), rig.root.clone(), B);
    seeded.set_floor(SEQ - 1);
    rig.helper.set_fetcher(Box::new(seeded.fetcher()));
    rig.put(&staged(&rig, 9, Verb::FrontendUpdate, B));
    rig.cycle(T0);
    assert_eq!(rig.refusal(), "");
    assert_eq!(rig.state().staged, Some(c(B)));
    assert_eq!(rig.current(), Some(c(A)));
    assert_eq!(rig.restarts(), 0);
    assert_eq!(rig.status()["state"], "staged");
    assert_eq!(rig.status()["staged"]["protocol"], 4);
    assert!(seeded.installed());
}

/// The ordering rules bind a staged frontend update as they bind an activation: another
/// commit, and a floor the seed's `seq` does not clear, are refused with the word
/// `commit_or_ref` before the pipeline runs, and stage nothing.
///
/// Catches the rules applied to the node's orders only.
///
/// Mutant to plant: the admission call removed from the frontend path.
#[test]
fn a_staged_frontend_update_meets_the_same_rules() {
    let below = format!("seq_floor: seed seq {SEQ} is not above {SEQ}");
    let cases = [(C, SEQ - 1, "seed_mismatch"), (B, SEQ, below.as_str())];
    for (index, (commit, floor, want)) in cases.into_iter().enumerate() {
        let mut rig = Rig::frontend(&format!("seedcyc-frontend-refused-{index}"));
        let seeded = Seeded::on(rig.layout.clone(), rig.root.clone(), B);
        seeded.set_floor(floor);
        rig.helper.set_fetcher(Box::new(seeded.fetcher()));
        rig.put(&staged(&rig, 9, Verb::FrontendUpdate, commit));
        rig.cycle(T0);
        assert_eq!(word(&rig), "commit_or_ref", "{}", rig.refusal());
        assert!(rig.refusal().contains(want), "{}", rig.refusal());
        assert_eq!(rig.state().staged, None);
        assert_eq!(rig.status()["acked_order_id"], 9);
        assert!(!seeded.installed());
    }
}

/// A node host running A whose fake fetcher carries a package of B above the floor, with the
/// facts the fake's releases declare.
fn packaged_node(name: &str) -> Rig {
    let rig = Rig::new(name);
    Seeded::on(rig.layout.clone(), rig.root.clone(), B).set_floor(SEQ - 1);
    *rig.fetcher.seed.lock().expect("lock") = Some(SeedFacts {
        commit: c(B),
        seq: SEQ,
        facts: kbf_update::Facts {
            platform: "linux-x86_64".to_owned(),
            protocol: 7,
            min_protocol: 6,
            state_schema: 3,
            profile: "release".to_owned(),
        },
    });
    rig
}

/// A staged activation whose release is the package's is carried out, and the fetcher was
/// held to the facts of the manifest and not to any the order could carry.
///
/// Catches a staged activation fetched without facts to hold the release to.
///
/// Mutant to plant: the order's (absent) claims passed in place of the seed's facts.
#[test]
fn a_staged_activation_holds_the_release_to_the_manifests_facts() {
    let mut rig = packaged_node("seedcyc-held");
    rig.put(&staged(&rig, 5, Verb::Activate, B));
    rig.cycle(T0);
    assert_eq!(rig.refusal(), "");
    assert_eq!(rig.current(), Some(c(B)));
    let want = rig
        .fetcher
        .seed
        .lock()
        .expect("lock")
        .clone()
        .expect("seed");
    let asked = rig.fetcher.expected.lock().expect("lock").clone();
    assert_eq!(asked, vec![want.facts]);
}

/// A release the fetcher reports as other than the manifest said (another commit, protocol,
/// schema or profile) is refused before any link moves, as one that fetches is when it differs
/// from its claims.
///
/// Catches the comparison after the fetch skipped for a staged activation, which trusted the
/// fetcher alone to have held the release to the manifest.
///
/// Mutants to plant: the comparison dropped for `Held::Seed`; only the protocol compared.
#[test]
fn a_staged_release_that_declares_other_facts_than_the_manifest_is_refused_before_the_flip() {
    let cases: [(&str, crate::helper::fetcher::fake::Tamper); 4] = [
        ("protocol", |r| r.protocol = 8),
        ("profile", |r| r.profile = "debug".to_owned()),
        ("state schema", |r| r.state_schema = 9),
        ("commit", |r| r.commit = c(C)),
    ];
    for (what, change) in cases {
        let mut rig = packaged_node(&format!("seedcyc-tamper-{}", what.replace(' ', "-")));
        *rig.fetcher.tamper.lock().expect("lock") = Some(change);
        rig.put(&staged(&rig, 5, Verb::Activate, B));
        rig.cycle(T0);
        assert_eq!(rig.current(), Some(c(A)), "{what}");
        assert_eq!(rig.restarts(), 0, "{what}");
        assert_eq!(rig.status()["state"], "refused", "{what}");
        assert!(
            rig.refusal().contains("not the one ordered"),
            "{what}: {}",
            rig.refusal()
        );
    }
}

/// A package whose manifest is not the one the order was admitted on when the release has been
/// fetched (it changed, or cannot be read) is refused before any link moves.
///
/// Catches the floor judged on one reading of the manifest and the release installed from
/// another.
///
/// Mutants to plant: the second reading not made; made and not compared.
#[test]
fn a_package_that_changed_while_its_release_was_fetched_is_refused_before_the_flip() {
    let other = |rig: &Rig| {
        let mut seed = rig
            .fetcher
            .seed
            .lock()
            .expect("lock")
            .clone()
            .expect("seed");
        seed.seq += 1;
        seed
    };
    for (what, said) in [("changed", "changed"), ("gone", "cannot be read again")] {
        let mut rig = packaged_node(&format!("seedcyc-swap-{what}"));
        let next = (what == "changed").then(|| other(&rig));
        *rig.fetcher.swap_seed.lock().expect("lock") = Some(next);
        rig.put(&staged(&rig, 5, Verb::Activate, B));
        rig.cycle(T0);
        assert_eq!(rig.current(), Some(c(A)), "{what}");
        assert_eq!(rig.restarts(), 0, "{what}");
        assert!(rig.refusal().contains(said), "{what}: {}", rig.refusal());
    }
}
