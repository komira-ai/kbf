//! The generation's check on the real coordinator: a stale conditional write changes no file
//! and no mark, concurrent conditional writers on one generation get one write accepted, the
//! services' records are independent, and the coordinator's own rollback is a write that a
//! write conditional on the generation read before it cannot pass.

use std::collections::BTreeMap;
use std::fs;
use std::sync::Barrier;

use serde_json::json;

use crate::desired::KBF;
use crate::rollback_tests::{
    AUTO, ONE, THREE, TWO, marks, persisted, restart, rig, round, seed, status, two_failed, wanted,
};
use crate::service::SetError;
use crate::services::{Services, TIERSTORE_SERVICES};
use crate::testutil::{Rig, ci_token, version};
use crate::writes::Record;

/// The record of `service` as `GET /v1/convergence` shows it.
async fn record(rig: &Rig, service: &str) -> Record {
    rig.coordinator
        .convergence(Some(&ci_token()), service)
        .await
        .expect("a convergence")
        .record
}

/// `body` (a JSON object) with `if_generation` and `write_id` added.
fn conditional(body: &str, generation: u64, write_id: &str) -> String {
    let mut value: serde_json::Value = serde_json::from_str(body).expect("JSON");
    value["if_generation"] = json!(generation);
    value["write_id"] = json!(write_id);
    value.to_string()
}

/// Every file of the rig's directory, by name.
fn files(rig: &Rig) -> BTreeMap<String, Vec<u8>> {
    fs::read_dir(rig.dir.path())
        .expect("listed")
        .map(|e| {
            let e = e.expect("an entry");
            (
                e.file_name().to_string_lossy().into_owned(),
                fs::read(e.path()).expect("read"),
            )
        })
        .collect()
}

/// Catches: a stale conditional write that changes something before it is refused. The
/// version it names is marked bad, and an ordinary write by hand would clear the mark; made
/// on a generation another write has passed, it is a conflict, and the marks, the audit, the
/// record and every file of the coordinator are byte for byte what they were.
#[tokio::test]
async fn a_stale_write_of_a_marked_version_changes_no_mark_audit_record_or_file() {
    let mut rig = rig("gen-marked", AUTO, ONE).await;
    seed(&rig, |m| {
        m.last_good = Some(version("1.0"));
        m.bad = vec![version("2.0")];
    });
    restart(&mut rig, AUTO);
    let seen = record(&rig, KBF).await.generation;
    rig.set(THREE).await;
    let before = (
        marks(&rig),
        status(&rig).ops.audit,
        record(&rig, KBF).await,
        files(&rig),
    );
    let stale = conditional(TWO, seen, "late");
    let refused = rig
        .coordinator
        .set_desired(Some(&ci_token()), stale.as_bytes())
        .await;
    assert!(
        matches!(refused, Err(SetError::Conflict { asked, now }) if asked == seen && now == seen + 1),
        "{refused:?}"
    );
    let after = (
        marks(&rig),
        status(&rig).ops.audit,
        record(&rig, KBF).await,
        files(&rig),
    );
    assert_eq!(after, before);
    assert_eq!(persisted(&rig).bad, vec![version("2.0")]);
    assert_eq!(wanted(&rig), version("3.0"));
}

/// Catches: the generation checked outside the lock the write is made under, so that two
/// writers that read the same generation both pass. Eight threads, released together by a
/// barrier, each write another version on the one generation they read: exactly one is
/// accepted, the seven others are conflicts, and the state and the record are the accepted
/// writer's.
#[test]
fn concurrent_writers_on_one_generation_get_one_write_accepted() {
    let rig = Rig::new("gen-race");
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("a runtime");
    runtime.block_on(rig.set(ONE));
    let seen = runtime.block_on(record(&rig, KBF)).generation;
    let writers = 8;
    let start = Barrier::new(writers);
    let results: Vec<(usize, Result<(), SetError>)> = std::thread::scope(|scope| {
        let handles: Vec<_> = (0..writers)
            .map(|i| {
                let (coordinator, start) = (&rig.coordinator, &start);
                scope.spawn(move || {
                    let body = conditional(
                        &format!(r#"{{"version":"2.{i}","min_up":{{"storage":0,"frontend":0}}}}"#),
                        seen,
                        &format!("writer-{i}"),
                    );
                    let token = ci_token();
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .build()
                        .expect("a runtime");
                    start.wait();
                    let result = runtime
                        .block_on(coordinator.set_desired(Some(&token), body.as_bytes()))
                        .map(drop);
                    (i, result)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().expect("a writer"))
            .collect()
    });
    let accepted: Vec<usize> = results
        .iter()
        .filter(|(_, r)| r.is_ok())
        .map(|(i, _)| *i)
        .collect();
    assert_eq!(accepted.len(), 1, "{results:?}");
    assert!(
        results
            .iter()
            .filter(|(_, r)| r.is_err())
            .all(|(_, r)| matches!(r, Err(SetError::Conflict { asked, now }) if *asked == seen && *now == seen + 1)),
        "{results:?}"
    );
    let winner = accepted[0];
    let now = runtime.block_on(record(&rig, KBF));
    assert_eq!(now.generation, seen + 1);
    assert_eq!(now.write_id, format!("writer-{winner}"));
    assert_eq!(wanted(&rig), version(&format!("2.{winner}")));
}

/// Catches: one service's writes moving another's generation or baseline, which would refuse
/// a restore of one because of a release of the other. `kbf` and `tierstore` are written in
/// turn; each keeps its own generation, `replaced` and `write_id`, a write conditional on
/// `kbf`'s generation passes after `tierstore` was written, and both records are the same
/// after a restart.
#[tokio::test]
async fn the_records_of_two_services_are_independent_and_survive_a_restart() {
    let mut rig = Rig::new("gen-services");
    let list = format!(
        "{}services = [\"kbf\", \"tierstore\"]\n",
        kbf_oidc::testkit::config_text(kbf_oidc::testkit::STATIC_ISSUER)
    );
    let start = |rig: &Rig| {
        Rig::start_with_allow_list(&rig.dir, &rig.clock, &list)
            .with_services(Services::parse(TIERSTORE_SERVICES).expect("services"))
    };
    rig.coordinator = start(&rig);
    let tier =
        |v: &str, extra: &str| format!(r#"{{"service":"tierstore","version":"{v}"{extra}}}"#);
    rig.set(ONE).await;
    rig.set(&tier("7.0", "")).await;
    rig.set(TWO).await;
    let seen = record(&rig, KBF).await.generation;
    rig.set(&tier("7.1", r#","write_id":"tier-1""#)).await;
    rig.set(&tier("7.1", r#","write_id":"tier-2""#)).await;
    rig.set(&conditional(THREE, seen, "kbf-3")).await;

    let kbf = record(&rig, KBF).await;
    let tierstore = record(&rig, "tierstore").await;
    assert_eq!((kbf.generation, kbf.write_id.as_str()), (3, "kbf-3"));
    assert_eq!(kbf.replaced.map(|r| r.version), Some(version("2.0")));
    assert_eq!(
        (tierstore.generation, tierstore.write_id.as_str()),
        (3, "tier-2")
    );
    assert_eq!(
        tierstore.replaced.as_ref().map(|r| r.version.clone()),
        Some(version("7.0"))
    );

    rig.coordinator = start(&rig);
    assert_eq!(record(&rig, KBF).await.generation, 3);
    assert_eq!(record(&rig, "tierstore").await, tierstore);
}

/// Catches: the coordinator's own rollback not counted as a write. A release is set, read
/// as the restore reads it, and then rolled back by the coordinator when two hosts fail it
/// after the commit: the generation is raised, `replaced` is the release, the write has no
/// id, and the restore's write on the generation it read is a conflict that leaves the
/// rollback standing.
#[tokio::test]
async fn an_automatic_rollback_raises_the_generation_and_a_restore_read_before_it_is_refused() {
    let rig = rig("gen-rollback", AUTO, ONE).await;
    round(&rig, 2_000, &[]);
    rig.set(TWO).await;
    let read = record(&rig, KBF).await;
    assert_eq!(
        read.replaced.as_ref().map(|r| r.version.clone()),
        Some(version("1.0"))
    );

    round(&rig, 2_030, &two_failed());
    assert_eq!(wanted(&rig), version("1.0"));
    let rolled = record(&rig, KBF).await;
    assert_eq!(rolled.generation, read.generation + 1);
    assert_eq!(rolled.replaced.map(|r| r.version), Some(version("2.0")));
    assert_eq!(rolled.write_id, "");

    let restore = conditional(ONE, read.generation, "restore-1-1");
    let refused = rig
        .coordinator
        .set_desired(Some(&ci_token()), restore.as_bytes())
        .await;
    assert!(
        matches!(refused, Err(SetError::Conflict { .. })),
        "{refused:?}"
    );
    assert_eq!(record(&rig, KBF).await.write_id, "");
    assert_eq!(wanted(&rig), version("1.0"));
}
