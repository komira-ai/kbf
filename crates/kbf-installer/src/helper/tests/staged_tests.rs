//! What the state's `staged` names after a fetch, with the real verify pipeline doing the
//! fetch's work ([`PipelineFetcher`]) instead of a fake that leaves a bare directory: a fake
//! cannot show what the pipeline writes into the state. `staged` is the one release
//! installed and waiting for a decision (a frontend update, until its confirm or revert). A
//! fetch alone decides nothing, so it leaves `staged` as it was; whoever keeps the release
//! records it.

use std::cell::RefCell;

use kbf_host::HostExec;
use kbf_host::testkit::ScratchHost;

use super::pipeline_fetcher_tests::PipelineFetcher;
use super::*;
use crate::helper::fetcher::fake::Tamper;

/// A second handle on the rig's root, for the first release: it is another program than the
/// helper, with an executor of its own. Both remove the root when they go; the second removal
/// finds nothing and does not mind.
fn second_handle(rig: &Rig) -> ScratchHost {
    ScratchHost {
        host: HostExec::new(Root::new(&rig.root).expect("absolute")),
        root: rig.root.clone(),
        euid: 0,
        log: Vec::new(),
        queries: RefCell::new(Vec::new()),
        answer: Box::new(|_| Ok(reply(0, "", ""))),
    }
}

/// The `refusal` word of `status.json`: what precedes the first colon.
fn word(rig: &Rig) -> String {
    rig.refusal().split(':').next().unwrap_or("").to_owned()
}

fn installed(rig: &Rig, commit: &str) -> bool {
    rig.root
        .join("var/lib/kbf-installer/releases")
        .join(commit)
        .join("VERIFIED")
        .is_file()
}

/// A frontend host running A, whose fetches are the real pipeline's.
fn real_frontend(name: &str) -> (Rig, PipelineFetcher) {
    let mut rig = Rig::frontend(name);
    let fetcher = PipelineFetcher::new(&rig.layout);
    rig.helper.set_fetcher(Box::new(fetcher.clone()));
    (rig, fetcher)
}

/// Asserts that a first release is not left staged as well as current. The first release goes
/// through the pipeline and `activate::first` (as `record-release` makes one), and the state
/// says it is current and stages nothing: status.json says `idle` (not `staged`, for good), and
/// the frontend's first update is taken, where a release both staged and current answers it
/// `frontend_busy` for ever.
///
/// Red on the code before the fix, where the pipeline wrote `staged` and `first` kept it.
/// Each of the two alone is masked here by the other being right (a pipeline that does not
/// write leaves `first` nothing to keep; a `first` that clears removes what the pipeline
/// wrote), so each has its own test: `the_pipeline_installs_and_never_writes_the_state`
/// and `a_first_release_that_was_staged_is_current_and_no_longer_staged`.
#[test]
fn a_first_release_is_not_left_staged_and_a_frontend_update_follows() {
    let mut rig = Rig::bare_frontend("first-then-update");
    let fetcher = PipelineFetcher::new(&rig.layout);
    let locator = ReleaseLocator {
        commit: c(A),
        components: Vec::new(),
        source: Source::Fetch,
    };
    fetcher.fetch(&locator).expect("fetched");
    let mut first = second_handle(&rig);
    activate::first(&mut first, &rig.layout, &c(A), &mut std::io::sink()).expect("the first");
    assert!(installed(&rig, A), "it came through the pipeline");
    let state = rig.state();
    assert_eq!(state.current, Some(c(A)));
    assert_eq!(
        state.staged, None,
        "a release that is current is not staged"
    );

    rig.helper.set_fetcher(Box::new(fetcher.clone()));
    rig.cycle(T0);
    assert_eq!(rig.status()["state"], "idle");
    assert!(rig.status()["staged_commit"].is_null());

    rig.order(1, Verb::FrontendUpdate, B);
    rig.cycle(T0 + 3);
    assert_eq!(rig.refusal(), "", "the update is taken");
    assert_eq!(rig.state().staged, Some(c(B)));
    assert_eq!(rig.status()["state"], "staged");
    assert_eq!(rig.status()["staged_commit"], B);
    assert_eq!(rig.current(), Some(c(A)));
}

/// Catches a frontend update that the helper refuses after the fetch leaving its release
/// staged: the fetcher's report is another commit, or another platform, than the order's and
/// the host's; the release is installed (the pipeline ran) and the order is refused with its
/// word, `staged` stays empty, status.json does not say `staged`, and the next update, of
/// another commit, is taken, where a leftover would answer it `frontend_busy`.
///
/// Catches: the pipeline writing `staged` after its install; a refused update
/// clearing `staged` whatever it names (see the next test).
#[test]
fn a_frontend_update_refused_after_its_fetch_leaves_nothing_staged() {
    let cases: [(&str, Tamper); 2] = [
        ("facts", |r| r.commit = c(C)),
        ("platform", |r| r.platform = "darwin-arm64".to_owned()),
    ];
    for (index, (want, tamper)) in cases.into_iter().enumerate() {
        let (mut rig, fetcher) = real_frontend(&format!("refused-after-fetch-{index}"));
        *fetcher.tamper.lock().expect("lock") = Some(tamper);
        rig.order(4, Verb::FrontendUpdate, B);
        rig.cycle(T0);
        assert_eq!(word(&rig), want, "case {index}: {}", rig.refusal());
        assert!(installed(&rig, B), "case {index}: the fetch ran to the end");
        assert_eq!(rig.state().staged, None, "case {index}");
        assert!(rig.status()["staged_commit"].is_null(), "case {index}");
        assert_eq!(rig.status()["state"], "refused", "case {index}");

        *fetcher.tamper.lock().expect("lock") = None;
        rig.order(5, Verb::FrontendUpdate, C);
        rig.cycle(T0 + 3);
        assert_eq!(rig.refusal(), "", "case {index}: nothing is held for B");
        assert_eq!(rig.state().staged, Some(c(C)), "case {index}");
        assert_eq!(rig.status()["state"], "staged", "case {index}");
    }
}

/// Catches a refusal after the fetch taking a staged release with it that the refused order
/// does not own: B is staged, and the proxy's repeat of the update for B (which the helper
/// takes: a repeat is verified again) is refused after the fetch. B is still the release
/// the first order staged, so it stays staged and the proxy can still confirm it.
///
/// Catches: the refusal after the fetch clearing `staged` whatever it names.
#[test]
fn a_repeat_of_the_staged_update_refused_after_its_fetch_keeps_it_staged() {
    let (mut rig, fetcher) = real_frontend("repeat-refused");
    rig.order(1, Verb::FrontendUpdate, B);
    rig.cycle(T0);
    assert_eq!(rig.state().staged, Some(c(B)), "{}", rig.refusal());

    *fetcher.tamper.lock().expect("lock") = Some(|r| r.platform = "darwin-arm64".to_owned());
    rig.order(2, Verb::FrontendUpdate, B);
    rig.cycle(T0 + 3);
    assert_eq!(word(&rig), "platform", "{}", rig.refusal());
    assert_eq!(rig.state().staged, Some(c(B)));

    *fetcher.tamper.lock().expect("lock") = None;
    rig.order(3, Verb::Confirm, B);
    rig.cycle(T0 + 6);
    assert_eq!(rig.current(), Some(c(B)), "{}", rig.refusal());
    assert_eq!(rig.state().staged, None);
}

/// Catches an order refused after its fetch taking the staged release of another order with
/// it, and a fetch displacing it: B is staged by the proxy's update; a refused frontend
/// update of C is refused for being busy before any fetch, and a refused node activation of
/// C (the report is not what the order expected) is refused after the fetch; B stays
/// staged through both, and a successful node activation of C swaps and still leaves B.
///
/// Catches: the pipeline writing `staged` (C displaces B, and the swap then clears
/// it as C); a refusal after the fetch clearing `staged` whatever it names.
#[test]
fn another_orders_staged_release_survives_a_refused_stage_and_a_node_swap() {
    let (mut rig, fetcher) = real_frontend("others-staged");
    rig.order(1, Verb::FrontendUpdate, B);
    rig.cycle(T0);
    assert_eq!(rig.state().staged, Some(c(B)), "{}", rig.refusal());

    rig.order(2, Verb::FrontendUpdate, C);
    rig.cycle(T0 + 3);
    assert_eq!(word(&rig), "frontend_busy", "{}", rig.refusal());
    assert_eq!(rig.state().staged, Some(c(B)));

    *fetcher.tamper.lock().expect("lock") = Some(|r| r.protocol = 9);
    rig.order(3, Verb::Activate, C);
    rig.cycle(T0 + 6);
    assert!(
        rig.refusal().contains("not the one ordered"),
        "{}",
        rig.refusal()
    );
    assert!(installed(&rig, C), "the fetch ran to the end");
    assert_eq!(rig.state().staged, Some(c(B)));
    assert_eq!(rig.current(), Some(c(A)));

    *fetcher.tamper.lock().expect("lock") = None;
    rig.order(4, Verb::Activate, C);
    rig.cycle(T0 + 9);
    let state = rig.state();
    assert_eq!(state.current, Some(c(C)), "{}", rig.refusal());
    assert_eq!(state.previous, Some(c(A)));
    assert!(state.pending_confirm.is_some());
    assert_eq!(state.staged, Some(c(B)), "the swap is not B's decision");
}

/// Catches a node activation that the helper refuses after the fetch leaving its release
/// staged: the release's report differs from the order's claims, the order is refused, and
/// `staged` stays empty, so the frontend's next update is taken (a leftover would answer
/// `frontend_busy`).
///
/// Catches: the pipeline writing `staged` after its install.
#[test]
fn a_node_activation_refused_after_its_fetch_stages_nothing() {
    let (mut rig, fetcher) = real_frontend("activation-refused");
    *fetcher.tamper.lock().expect("lock") = Some(|r| r.state_schema = 4);
    rig.order(1, Verb::Activate, C);
    rig.cycle(T0);
    assert!(
        rig.refusal().contains("not the one ordered"),
        "{}",
        rig.refusal()
    );
    assert!(installed(&rig, C), "the fetch ran to the end");
    assert_eq!(rig.state().staged, None);
    assert_eq!(rig.current(), Some(c(A)));

    *fetcher.tamper.lock().expect("lock") = None;
    rig.order(2, Verb::FrontendUpdate, B);
    rig.cycle(T0 + 3);
    assert_eq!(rig.refusal(), "");
    assert_eq!(rig.state().staged, Some(c(B)));
}
