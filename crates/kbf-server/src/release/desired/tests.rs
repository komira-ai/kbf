//! `POST /v1/update/desired` and R4 (the age check on the build seq), called as the router
//! calls it, over a real release store and the rows of a fake state store, with a fake
//! [`SeqLookup`] for what the store does not record yet.

use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::body::Bytes;
use axum::extract::State;
use axum::http::header::{AUTHORIZATION, CONTENT_TYPE};
use futures::FutureExt;
use futures::future::BoxFuture;
use kbf_state::fake::{Fake, Flaw};
use kbf_state::{ManualClock, Store};
use kbf_types::ReleasePlatform;
use serde_json::{Value, json};

use super::*;
use crate::release::key::{Component, Key};
use crate::release::store::Limits;
use crate::release::target_seq::{SeqLookup, TargetSeq};

const BEARER: &str = "operator-token-0123456789abcdef0123456789";
/// The running build of these tests: seq 500, commit `aaaaaaaaaaaa`.
const OWN_SEQ: u64 = 500;
const OWN_COMMIT: &str = "aaaaaaaaaaaa";
/// The 40 digits of the running build, and of two other commits.
const OWN_FULL: &str = "aaaaaaaaaaaa0000000000000000000000000000";
/// Another commit with the same first 12 digits as `OWN_FULL`.
const SIBLING: &str = "aaaaaaaaaaaa1111111111111111111111111111";
const OTHER: &str = "bbbbbbbbbbbb0000000000000000000000000000";
const NO_PLATFORM: &str = "cccccccccccc0000000000000000000000000000";

/// What the fake store records: an answer per commit, `NotRecorded` for the rest, and an
/// io error for `FAILS`.
struct Recorded {
    answers: BTreeMap<&'static str, TargetSeq>,
    /// How many times the store was asked.
    calls: Arc<AtomicUsize>,
}

const FAILS: &str = "dddddddddddd0000000000000000000000000000";

impl SeqLookup for Recorded {
    fn seq<'a>(&'a self, commit: &'a Commit) -> BoxFuture<'a, io::Result<TargetSeq>> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let answer = if commit.as_str() == FAILS {
            Err(io::Error::other("the disk went"))
        } else {
            Ok(self
                .answers
                .get(commit.as_str())
                .copied()
                .unwrap_or(TargetSeq::NotRecorded))
        };
        futures::future::ready(answer).boxed()
    }
}

struct Rig {
    update: Arc<Update>,
    rows: Rows,
    /// The asks made of the fake store.
    calls: Arc<AtomicUsize>,
    _dir: kbf_testkit::dir::TestDir,
}

async fn put(store: &ReleaseStore, commit: &str) {
    let key = Key {
        commit: Commit::parse(commit).expect("a commit"),
        platform: ReleasePlatform::LinuxX86_64,
        component: Component::Kbf,
    };
    let bytes = |b: &'static [u8]| futures::stream::iter([Ok::<_, io::Error>(Bytes::from(b))]);
    store
        .put_tarball(&key, &mut bytes(b"tarball"))
        .await
        .expect("tarball");
    store
        .put_attestation(&key, &mut bytes(b"bundle"))
        .await
        .expect("attestation");
}

/// A store holding `OWN_FULL`, `OTHER`, `FAILS` and `commits` complete, and a server whose
/// build is `own_seq` and which learns release seqs from `answers` (or from the store
/// itself when `answers` is `None`).
async fn rig(own_seq: u64, answers: Option<&[(&'static str, TargetSeq)]>) -> Rig {
    rig_of(own_seq, OWN_COMMIT, answers).await
}

/// [`rig`] for a running build of commit `own_commit`.
async fn rig_of(
    own_seq: u64,
    own_commit: &str,
    answers: Option<&[(&'static str, TargetSeq)]>,
) -> Rig {
    let dir = crate::test_dir::scratch_dir("kbf-desired-age");
    let store = Arc::new(ReleaseStore::open(&dir, Limits::default()).expect("a store"));
    for commit in [OWN_FULL, SIBLING, OTHER, FAILS] {
        put(&store, commit).await;
    }
    let calls = Arc::new(AtomicUsize::new(0));
    let lookup: Arc<dyn SeqLookup> = match answers {
        Some(answers) => Arc::new(Recorded {
            answers: answers.iter().copied().collect(),
            calls: Arc::clone(&calls),
        }),
        None => store.clone(),
    };
    let rows = Rows::new(Arc::new(Fake::with_flaw(
        Arc::new(ManualClock::default()),
        Flaw::None,
    )) as Arc<dyn Store>);
    let farm = Arc::new(Farm::new(
        Arc::new(kbf_front::Cache::<
            kbf_front::MemoryMetaLog,
            kbf_objstore::MemoryStore,
        >::memory()),
        kbf_sched::UNSERVABLE_WAIT,
        kbf_sched::FINISHED_RETENTION,
    ));
    let update = Arc::new(Update {
        held: Some(Held {
            age: AgeCheck::new(own_seq, own_commit, lookup),
            store,
        }),
        rows: Some(rows.clone()),
        token: Some(ApiToken::for_tests(BEARER)),
        farm,
        own_commit: own_commit.to_owned(),
    });
    Rig {
        update,
        rows,
        calls,
        _dir: dir,
    }
}

/// `rig` for a server started without `--releases-dir`: the same rows, build and token, and
/// no release store.
fn without_store(rig: Rig) -> Rig {
    let update = Arc::new(Update {
        held: None,
        rows: rig.update.rows.clone(),
        token: Some(ApiToken::for_tests(BEARER)),
        farm: Arc::clone(&rig.update.farm),
        own_commit: rig.update.own_commit.clone(),
    });
    Rig { update, ..rig }
}

async fn offer(rig: &Rig, body: &Value) -> (u16, String) {
    let mut headers = HeaderMap::new();
    headers.insert(
        AUTHORIZATION,
        HeaderValue::from_str(&format!("Bearer {BEARER}")).expect("header"),
    );
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    let peer = Caller::direct(SocketAddr::from(([127, 0, 0, 1], 4000)));
    let response = set_desired(
        State(Arc::clone(&rig.update)),
        peer,
        headers,
        Bytes::from(body.to_string()),
    )
    .await;
    let code = response.status().as_u16();
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("a body");
    (code, String::from_utf8_lossy(&bytes).into_owned())
}

fn desired_commit(rig: &Rig) -> Option<String> {
    rig.rows
        .desired()
        .expect("the row")
        .value
        .map(|d| d.commit.as_str().to_owned())
}

fn desired_source(rig: &Rig) -> Option<Source> {
    rig.rows.desired().expect("the row").value.map(|d| d.source)
}

/// What `GET /v1/update` shows as desired: "<generation> <source>", or "0 -" before any offer.
async fn shown(rig: &Rig) -> String {
    let response = view(State(Arc::clone(&rig.update))).await;
    let bytes = axum::body::to_bytes(response.into_body(), 1 << 20)
        .await
        .expect("a body");
    let doc: Value = serde_json::from_slice(&bytes).expect("json");
    let desired = &doc["desired"];
    let generation = desired["generation"].as_u64().unwrap_or(0);
    format!("{generation} {}", desired["source"].as_str().unwrap_or("-"))
}

/// The status and the parsed body of an offer.
async fn offered(rig: &Rig, body: &Value) -> (u16, Value) {
    let (code, text) = offer(rig, body).await;
    (code, serde_json::from_str(&text).unwrap_or(Value::Null))
}

const SEQ_500: TargetSeq = TargetSeq::Seq(500);

/// Catches: R4 refusing a repeat of the running build (`<=` for `<`), or any equal seq of
/// the same commit; the generation then moves as for any offer.
#[tokio::test]
async fn an_equal_seq_of_the_running_commit_is_offered() {
    let rig = rig(OWN_SEQ, Some(&[(OWN_FULL, SEQ_500)])).await;
    let (code, body) = offer(&rig, &json!({"commit": OWN_FULL})).await;
    assert_eq!(
        (code, desired_commit(&rig).as_deref()),
        (200, Some(OWN_FULL)),
        "{body}"
    );
}

/// Catches: a newer target refused, in particular one ordered by its commit text (the
/// target's text is smaller than the running build's) and an unrelated commit accepted only
/// as the running one.
#[tokio::test]
async fn a_newer_target_is_offered() {
    let rig = rig(OWN_SEQ, Some(&[(OTHER, TargetSeq::Seq(501))])).await;
    let (code, body) = offer(&rig, &json!({"commit": OTHER})).await;
    assert_eq!(
        (code, desired_commit(&rig).as_deref()),
        (200, Some(OTHER)),
        "{body}"
    );
}

/// Catches: an older target offered, a refusal that does not name the code, both seqs and
/// both commits, and a row written anyway.
#[tokio::test]
async fn an_older_target_is_refused_naming_both_builds_and_writes_nothing() {
    let rig = rig(OWN_SEQ, Some(&[(OTHER, TargetSeq::Seq(499))])).await;
    let (code, body) = offer(&rig, &json!({"commit": OTHER})).await;
    assert_eq!(code, 409, "{body}");
    for part in ["older", "499", "500", OTHER, OWN_COMMIT] {
        assert!(body.contains(part), "{part:?} in {body}");
    }
    assert_eq!(desired_commit(&rig), None, "the row is not written");
}

/// Catches: an equal seq of another commit offered (two branches cannot be ordered).
#[tokio::test]
async fn an_equal_seq_of_another_commit_is_refused_as_a_tie() {
    let rig = rig(OWN_SEQ, Some(&[(OTHER, SEQ_500)])).await;
    let (code, body) = offer(&rig, &json!({"commit": OTHER})).await;
    assert_eq!(code, 409, "{body}");
    assert!(body.contains("seq_tie"), "{body}");
    assert_eq!(desired_commit(&rig), None);
}

/// Catches: a release with no recorded seq (or seq 0) offered while the running build's seq
/// is known, in particular unknown taken as the newest.
#[tokio::test]
async fn a_release_with_no_seq_is_refused_when_the_running_seq_is_known() {
    for answer in [TargetSeq::Missing, TargetSeq::Seq(0)] {
        let rig = rig(OWN_SEQ, Some(&[(OTHER, answer)])).await;
        let (code, body) = offer(&rig, &json!({"commit": OTHER})).await;
        assert_eq!(code, 409, "{answer:?}: {body}");
        assert!(
            body.contains("seq_unknown") && body.contains("unknown"),
            "{body}"
        );
        assert_eq!(desired_commit(&rig), None, "{answer:?}");
    }
}

/// Catches: R4 applied to a build that could not count (it cannot apply), whatever the
/// store says of the target.
#[tokio::test]
async fn a_running_build_with_no_seq_offers_anything() {
    for answer in [TargetSeq::Missing, TargetSeq::Seq(1), TargetSeq::Seq(0)] {
        let rig = rig(0, Some(&[(OTHER, answer)])).await;
        let (code, body) = offer(&rig, &json!({"commit": OTHER})).await;
        assert_eq!(code, 200, "{answer:?}: {body}");
    }
}

/// Catches: a store that cannot record seq refusing every offer (an always-unknown seam): it
/// answers `NotRecorded`, and R4 does not apply even to a known running seq.
#[tokio::test]
async fn a_store_that_records_no_seq_leaves_the_offer_as_it_was() {
    let rig = rig(OWN_SEQ, Some(&[])).await;
    let (code, body) = offer(&rig, &json!({"commit": OTHER})).await;
    assert_eq!(code, 200, "{body}");
}

/// Catches: the real release store answering anything but `NotRecorded` before it records
/// seq, and an offer to a server on it refused by R4 whatever the running seq.
#[tokio::test]
async fn the_release_store_of_today_answers_not_recorded() {
    let rig = rig(OWN_SEQ, None).await;
    let held = rig.update.held.as_ref().expect("a store");
    let commit = Commit::parse(OTHER).expect("a commit");
    assert_eq!(
        held.store.seq(&commit).await.expect("an answer"),
        TargetSeq::NotRecorded
    );
    let (code, body) = offer(&rig, &json!({"commit": OTHER})).await;
    assert_eq!(code, 200, "{body}");
}

/// Catches: the production check built from other numbers than the build's own: a target
/// whose seq is one below [`crate::BUILD_SEQ`] is refused exactly when the build knows its seq.
#[tokio::test]
async fn the_check_of_a_server_is_the_build_it_is() {
    let other = Commit::parse(OTHER).expect("a commit");
    let lookup: Arc<dyn SeqLookup> = Arc::new(Recorded {
        answers: [(OTHER, TargetSeq::Seq(crate::BUILD_SEQ.saturating_sub(1)))]
            .into_iter()
            .collect(),
        calls: Arc::default(),
    });
    let refused = AgeCheck::running(lookup)
        .check(&other)
        .await
        .expect("a check")
        .is_err();
    assert_eq!(refused, std::hint::black_box(crate::BUILD_SEQ) >= 1);
}

/// Catches: a failed read of the seq answered as a refusal or an allowance; it is a 500 that
/// says nothing of the error.
#[tokio::test]
async fn a_lookup_that_fails_is_a_500() {
    let rig = rig(OWN_SEQ, Some(&[])).await;
    let (code, body) = offer(&rig, &json!({"commit": FAILS})).await;
    assert_eq!(code, 500, "{body}");
    assert!(
        body.contains("the release store failed") && !body.contains("disk"),
        "{body}"
    );
    assert_eq!(desired_commit(&rig), None);
}

/// Catches: R4 moved ahead of the other 409s: a release with no complete platform says so
/// (not "older"), and a stale `expect_generation` still names the generations.
#[tokio::test]
async fn the_other_conflicts_are_unchanged() {
    let rig = rig(
        OWN_SEQ,
        Some(&[
            (NO_PLATFORM, TargetSeq::Seq(1)),
            (OTHER, TargetSeq::Seq(501)),
        ]),
    )
    .await;
    let (code, body) = offer(&rig, &json!({"commit": NO_PLATFORM})).await;
    assert_eq!(code, 409, "{body}");
    assert!(
        body.contains("no platform") && !body.contains("older"),
        "{body}"
    );
    let (code, body) = offer(&rig, &json!({"commit": OTHER, "expect_generation": 7})).await;
    assert_eq!(code, 409, "{body}");
    assert!(body.contains('7'), "{body}");
    assert_eq!(desired_commit(&rig), None);
}

/// Catches: `rollback` ignored (an older, tied or unknown release refused with it), applied
/// to the other conflicts, or sticking to the offer after it (the next plain offer is checked).
#[tokio::test]
async fn a_rollback_skips_the_age_check_and_only_that() {
    for answer in [
        TargetSeq::Seq(499),
        SEQ_500,
        TargetSeq::Missing,
        TargetSeq::Seq(0),
    ] {
        let rig = rig(OWN_SEQ, Some(&[(OTHER, answer)])).await;
        let (code, body) = offer(&rig, &json!({"commit": OTHER, "rollback": true})).await;
        assert_eq!(code, 200, "{answer:?}: {body}");
        assert_eq!(desired_commit(&rig).as_deref(), Some(OTHER));
    }
    let rig = rig(OWN_SEQ, Some(&[(NO_PLATFORM, TargetSeq::Seq(1))])).await;
    let (code, body) = offer(&rig, &json!({"commit": NO_PLATFORM, "rollback": true})).await;
    assert_eq!(code, 409, "{body}");
    assert!(body.contains("no platform"), "{body}");
    let rig = rig_older().await;
    let (code, _) = offer(
        &rig,
        &json!({"commit": OTHER, "rollback": true, "expect_generation": 7}),
    )
    .await;
    assert_eq!(code, 409, "expect_generation still applies");
    let (code, body) = offer(&rig, &json!({"commit": OTHER, "rollback": false})).await;
    assert_eq!(code, 409, "{body}");
    assert!(body.contains("older"), "{body}");
    let (code, body) = offer(&rig, &json!({"commit": OTHER, "rollback": "yes"})).await;
    assert_eq!(code, 400, "{body}");
}

async fn rig_older() -> Rig {
    rig(OWN_SEQ, Some(&[(OTHER, TargetSeq::Seq(499))])).await
}

/// Catches: a refusal a program cannot tell from the others (no `code`), a code that is not
/// the refusal's, and a `code` on the conflicts that are not R4's.
#[tokio::test]
async fn an_r4_refusal_carries_its_stable_code_and_no_other_conflict_does() {
    let table = [
        (TargetSeq::Seq(499), "older"),
        (SEQ_500, "seq_tie"),
        (TargetSeq::Missing, "seq_unknown"),
    ];
    for (answer, code) in table {
        let rig = rig(OWN_SEQ, Some(&[(OTHER, answer)])).await;
        let (status, body) = offer(&rig, &json!({"commit": OTHER})).await;
        let body: Value = serde_json::from_str(&body).expect("json");
        assert_eq!(status, 409, "{body}");
        assert_eq!(body["code"], code, "{body}");
        assert!(
            body["error"].as_str().is_some_and(|e| e.starts_with(code)),
            "{body}"
        );
    }
    let rig = rig(
        OWN_SEQ,
        Some(&[
            (NO_PLATFORM, TargetSeq::Seq(1)),
            (OTHER, TargetSeq::Seq(501)),
        ]),
    )
    .await;
    for body in [
        json!({"commit": NO_PLATFORM}),
        json!({"commit": OTHER, "expect_generation": 7}),
    ] {
        let (status, text) = offer(&rig, &body).await;
        let text: Value = serde_json::from_str(&text).expect("json");
        assert_eq!(status, 409, "{text}");
        assert!(text.get("code").is_none(), "{text}");
    }
}

/// Catches: a server that knows its full commit offering a sibling of it (the same 12 digits,
/// another commit) as a repeat of itself: of the two, only its own commit is allowed.
#[tokio::test]
async fn a_server_that_knows_its_full_commit_tells_a_sibling_apart() {
    let answers = [(OWN_FULL, SEQ_500), (SIBLING, SEQ_500)];
    let rig = rig_of(OWN_SEQ, OWN_FULL, Some(&answers)).await;
    let (code, body) = offer(&rig, &json!({"commit": SIBLING})).await;
    assert_eq!(code, 409, "{body}");
    assert!(body.contains("seq_tie"), "{body}");
    assert_eq!(desired_commit(&rig), None);
    let (code, body) = offer(&rig, &json!({"commit": OWN_FULL})).await;
    assert_eq!(code, 200, "{body}");
}

/// Catches: the production check naming itself by the 12 digits when the build knew its full
/// commit: a commit with the build's 12 digits and another tail at the build's seq is a tie
/// exactly when the build knows its full commit and its seq.
#[tokio::test]
async fn the_check_of_a_server_uses_its_full_commit_when_it_has_one() {
    let sibling = format!("{}{:1<28}", crate::BUILD_COMMIT, "");
    let lookup: Arc<dyn SeqLookup> = Arc::new(Recorded {
        answers: [(
            Box::leak(sibling.clone().into_boxed_str()) as &'static str,
            TargetSeq::Seq(crate::BUILD_SEQ),
        )]
        .into_iter()
        .collect(),
        calls: Arc::default(),
    });
    let sibling = Commit::parse(&sibling).expect("a commit");
    let refused = AgeCheck::running(lookup)
        .check(&sibling)
        .await
        .expect("a check")
        .is_err();
    assert_eq!(
        refused,
        !crate::BUILD_COMMIT_FULL.is_empty() && std::hint::black_box(crate::BUILD_SEQ) >= 1
    );
}

/// Catches: R4 applied to the running build's own commit (the proxy's step-9 post names it, so
/// a release the store has no seq for would be refused), and the store asked about it. Under
/// every answer the own commit is allowed with no look; a different commit with the same
/// answer is looked up and refused where R4 says so.
#[tokio::test]
async fn the_running_commit_is_never_refused_and_never_looked_up() {
    for answer in [
        TargetSeq::Missing,
        TargetSeq::NotRecorded,
        TargetSeq::Seq(1),
        TargetSeq::Seq(0),
    ] {
        let rig = rig_of(
            OWN_SEQ,
            OWN_FULL,
            Some(&[(OWN_FULL, answer), (OTHER, answer)]),
        )
        .await;
        let (code, body) = offer(&rig, &json!({"commit": OWN_FULL})).await;
        assert_eq!(code, 200, "{answer:?}: {body}");
        assert_eq!(
            rig.calls.load(Ordering::SeqCst),
            0,
            "{answer:?}: no look at the own commit"
        );
    }
    // A build that knows only its 12 digits names its own commit by them.
    let rig = rig(OWN_SEQ, Some(&[(OWN_FULL, TargetSeq::Missing)])).await;
    let (code, body) = offer(&rig, &json!({"commit": OWN_FULL})).await;
    assert_eq!((code, rig.calls.load(Ordering::SeqCst)), (200, 0), "{body}");
    // Another commit with the same answer is looked up, and refused.
    let rig = rig_of(OWN_SEQ, OWN_FULL, Some(&[(OTHER, TargetSeq::Missing)])).await;
    let (code, body) = offer(&rig, &json!({"commit": OTHER})).await;
    assert_eq!(code, 409, "{body}");
    assert!(body.contains("seq_unknown"), "{body}");
    assert_eq!(rig.calls.load(Ordering::SeqCst), 1);
}

/// The two bodies of a fetch of `OTHER`: no `source`, and the explicit word.
fn fetch_bodies() -> [Value; 2] {
    [
        json!({"commit": OTHER}),
        json!({"commit": OTHER, "source": "fetch"}),
    ]
}

/// Catches: a staged offer of the build's own commit that needs a release store (the proxy
/// posts it to a frontend started without `--releases-dir`, where a fetch is `503`), a staged
/// offer written as a fetch, an answer that does not say its source, and the seq lookup asked.
///
/// Mutants it catches: the `503` made before the source is read; `Source::Fetch` passed to
/// `offer_expecting`; `source` left out of `desired_view`.
#[tokio::test]
async fn a_staged_offer_of_the_own_commit_needs_no_release_store() {
    let rig = without_store(rig_of(OWN_SEQ, OWN_FULL, Some(&[])).await);
    let (code, said) = offered(&rig, &json!({"commit": OWN_FULL, "source": "staged"})).await;
    assert_eq!(code, 200, "{said}");
    assert_eq!(said["commit"], OWN_FULL);
    assert_eq!(said["source"], "staged");
    assert_eq!(desired_commit(&rig).as_deref(), Some(OWN_FULL));
    assert_eq!(desired_source(&rig), Some(Source::Staged));
    assert_eq!(rig.calls.load(Ordering::SeqCst), 0);
}

/// Catches: a staged offer held to what a fetch needs of the store: here the store holds no
/// platform of the build's own commit, so the fetch of it is `409` and the staged offer is
/// taken. The checks are keyed by the source, not removed: the fetch is still refused.
///
/// Mutants it catches: the completeness check run for a staged offer; the check dropped for a
/// fetch.
#[tokio::test]
async fn a_staged_offer_needs_nothing_of_the_store_that_a_fetch_needs() {
    let rig = rig_of(OWN_SEQ, NO_PLATFORM, Some(&[])).await;
    let (code, said) = offered(&rig, &json!({"commit": NO_PLATFORM})).await;
    assert_eq!(code, 409, "{said}");
    let error = said["error"].as_str().unwrap_or_default();
    assert!(error.contains("no platform"), "{said}");
    assert_eq!(desired_commit(&rig), None);
    let staged = json!({"commit": NO_PLATFORM, "source": "staged"});
    let (code, said) = offered(&rig, &staged).await;
    assert_eq!(code, 200, "{said}");
    assert_eq!(desired_source(&rig), Some(Source::Staged));
}

/// Catches: a staged offer of a commit that is not the build's own written (the proxy would
/// move the hosts to a release this server has not got), a refusal that a program cannot tell
/// from the other `409`s (no `code`), and `rollback` lifting the own-commit rule (it skips R4
/// only). `OTHER` is complete in the store and newer than the build, so a fetch takes it.
///
/// Mutants it catches: the own-commit check dropped; `rollback` skipping it.
#[tokio::test]
async fn a_staged_offer_of_another_commit_is_not_own_and_writes_nothing() {
    let rig = rig_of(OWN_SEQ, OWN_FULL, Some(&[(OTHER, TargetSeq::Seq(501))])).await;
    for rollback in [false, true] {
        let body = json!({"commit": OTHER, "source": "staged", "rollback": rollback});
        let (code, said) = offered(&rig, &body).await;
        assert_eq!(code, 409, "{said}");
        assert_eq!(said["code"], "not_own");
        let error = said["error"].as_str().unwrap_or_default();
        for part in [OTHER, OWN_FULL] {
            assert!(error.contains(part), "{part}: {said}");
        }
        assert_eq!(desired_commit(&rig), None, "rollback {rollback}");
    }
    let (code, said) = offered(&rig, &json!({"commit": OTHER})).await;
    assert_eq!(code, 200, "the same commit as a fetch is offered: {said}");
}

/// Catches: the own-commit rule compared by the first 12 digits when the build knows all 40: a
/// sibling (the same 12 digits, another tail) is not this build and is `not_own`, while the
/// build's own commit is taken. A build that knows only its 12 digits names its own commit by
/// them, as R4 does, and refuses a commit with other digits.
///
/// Mutants it catches: a prefix-only compare of the 12 digits; the sibling taken as the build.
#[tokio::test]
async fn a_staged_sibling_that_shares_only_twelve_digits_is_not_own() {
    let rig = rig_of(OWN_SEQ, OWN_FULL, Some(&[])).await;
    let (code, said) = offered(&rig, &json!({"commit": SIBLING, "source": "staged"})).await;
    assert_eq!(code, 409, "{said}");
    assert_eq!(said["code"], "not_own");
    assert_eq!(desired_commit(&rig), None);
    let (code, said) = offered(&rig, &json!({"commit": OWN_FULL, "source": "staged"})).await;
    assert_eq!(code, 200, "{said}");
    let short = rig_of(OWN_SEQ, OWN_COMMIT, Some(&[])).await;
    let (code, said) = offered(&short, &json!({"commit": OTHER, "source": "staged"})).await;
    assert_eq!(code, 409, "{said}");
    assert_eq!(desired_commit(&short), None);
    let (code, said) = offered(&short, &json!({"commit": OWN_FULL, "source": "staged"})).await;
    assert_eq!(code, 200, "{said}");
}

/// Catches: a source that is not one of the two words read as a fetch or a staged offer (a
/// typo would then move the hosts from the wrong place), an answer that does not name the
/// field, and a row written anyway.
///
/// Mutants it catches: `source` read as a free text; an unknown word defaulting to a fetch.
#[tokio::test]
async fn a_source_that_is_not_fetch_or_staged_is_a_400_that_writes_nothing() {
    let rig = rig_of(OWN_SEQ, OWN_FULL, Some(&[])).await;
    for source in [json!("bogus"), json!("Staged"), json!(2), Value::Null] {
        let (code, text) = offer(&rig, &json!({"commit": OWN_FULL, "source": source})).await;
        assert_eq!(code, 400, "{source}: {text}");
        assert!(text.contains("source"), "{source}: {text}");
    }
    assert_eq!(desired_commit(&rig), None);
}

/// Catches: an explicit `"source": "fetch"` read as anything but the default: it passes the
/// checks of a fetch and is written as one, as an offer with no `source`.
///
/// Mutants it catches: `"fetch"` refused as an unknown word; `"fetch"` taken as a staged offer.
#[tokio::test]
async fn an_explicit_fetch_is_an_offer_with_no_source() {
    let mut seen = Vec::new();
    for body in &fetch_bodies() {
        let rig = rig(OWN_SEQ, Some(&[(OTHER, TargetSeq::Seq(501))])).await;
        let (code, said) = offered(&rig, body).await;
        seen.push((code, said["source"].clone(), desired_source(&rig)));
    }
    assert_eq!(seen[0], seen[1]);
    assert_eq!(seen[0], (200, json!("fetch"), Some(Source::Fetch)));
}

/// Catches: a fetch on a server started without `--releases-dir` taken, or refused with
/// another status or words, once the staged path needs no store: still `503` naming the flag,
/// for an absent source and the explicit word, and nothing is written.
///
/// Mutants it catches: the store required of no source; the `503` dropped for a fetch.
#[tokio::test]
async fn a_fetch_with_no_release_store_is_a_503_naming_the_flag() {
    for body in &fetch_bodies() {
        let rig = without_store(rig(OWN_SEQ, Some(&[])).await);
        let (code, text) = offer(&rig, body).await;
        assert_eq!(code, 503, "{text}");
        assert!(text.contains("--releases-dir"), "{text}");
        assert_eq!(desired_commit(&rig), None);
    }
}

/// Catches: the same commit offered from the other source answered as a repeat (the run would
/// not move to the package), a repeat from the same source starting a new run, `GET
/// /v1/update` not saying the source of the desired release, and `expect_generation` not
/// applying to a staged offer.
///
/// Mutants it catches: the source dropped between the body and the row; the generation kept
/// when only the source differs; `source` left out of `desired_view`.
#[tokio::test]
async fn the_same_commit_from_the_other_source_is_a_new_run_and_the_view_says_which() {
    let rig = rig_of(OWN_SEQ, OWN_FULL, Some(&[])).await;
    assert_eq!(shown(&rig).await, "0 -");
    let fetch = json!({"commit": OWN_FULL});
    let staged = json!({"commit": OWN_FULL, "source": "staged"});
    let steps = [
        (&fetch, "1 fetch"),
        (&staged, "2 staged"),
        (&staged, "2 staged"),
        (&fetch, "3 fetch"),
    ];
    for (body, want) in steps {
        let (code, said) = offered(&rig, body).await;
        assert_eq!(code, 200, "{said}");
        assert_eq!(shown(&rig).await, want);
    }
    let stale = json!({"commit": OWN_FULL, "source": "staged", "expect_generation": 1});
    let (code, said) = offered(&rig, &stale).await;
    assert_eq!(code, 409, "{said}");
    assert_eq!(shown(&rig).await, "3 fetch");
}
