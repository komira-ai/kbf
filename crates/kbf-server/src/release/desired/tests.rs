//! `POST /v1/update/desired` and R4 (the age check on the build seq), called as the router
//! calls it, over a real release store and the rows of a fake state store, with a fake
//! [`SeqLookup`] for what the store does not record yet.

use std::collections::BTreeMap;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

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
const OTHER: &str = "bbbbbbbbbbbb0000000000000000000000000000";
const NO_PLATFORM: &str = "cccccccccccc0000000000000000000000000000";

/// What the fake store records: an answer per commit, `NotRecorded` for the rest, and an
/// io error for `FAILS`.
struct Recorded {
    answers: BTreeMap<&'static str, TargetSeq>,
}

const FAILS: &str = "dddddddddddd0000000000000000000000000000";

impl SeqLookup for Recorded {
    fn seq<'a>(&'a self, commit: &'a Commit) -> BoxFuture<'a, io::Result<TargetSeq>> {
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
    let dir = crate::test_dir::scratch_dir("kbf-desired-age");
    let store = Arc::new(ReleaseStore::open(&dir, Limits::default()).expect("a store"));
    for commit in [OWN_FULL, OTHER, FAILS] {
        put(&store, commit).await;
    }
    let lookup: Arc<dyn SeqLookup> = match answers {
        Some(answers) => Arc::new(Recorded {
            answers: answers.iter().copied().collect(),
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
            age: AgeCheck::new(own_seq, OWN_COMMIT, lookup),
            store,
        }),
        rows: Some(rows.clone()),
        token: Some(ApiToken::for_tests(BEARER)),
        farm,
    });
    Rig {
        update,
        rows,
        _dir: dir,
    }
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
    });
    let refused = AgeCheck::running(lookup)
        .check(&other)
        .await
        .expect("a check")
        .is_err();
    assert_eq!(refused, crate::BUILD_SEQ >= 1);
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
