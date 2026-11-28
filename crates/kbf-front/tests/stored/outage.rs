//! A build over a frontend with no storage node while the object store is in an outage, as a
//! client of the frontend sees it: the store resets every connection, stalls its bodies, or
//! answers 503, for a window of the runtime's clock, which is paused, so the window and every wait
//! of the retries pass when nothing else can run. The retries are not jittered, so the waits are
//! 0.5 s, 1 s, 2 s, 4 s, 8 s and then 8 s again, and what each test asserts exactly is the
//! virtual time a call took: the waits it made and the time of the attempts the store modelled,
//! never a bound on real time.

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use kbf_auth::Authorizers;
use kbf_front::store_metrics::Op;
use kbf_front::store_retry::{Cause, RetryPolicy};
use kbf_front::{ExecutionService, PackSettings, Packs};
use kbf_pack::{PackKeys, WriterKind};
use kbf_proto::reapi::execution_server::Execution as _;
use kbf_proto::reapi::{Action, Command, Directory, ExecuteRequest};
use kbf_types::Digest;
use prost::Message as _;
use tokio::time::Instant;
use tonic::{Code, Status};

use crate::bytestream::{pieces, read as read_stream, read_name, write};
use crate::cas::{item, missing, read, update};
use crate::empty_blob::{Queue, file, proto};
use crate::rig::{Rig, SMALL, blob_len, blob_of};
use crate::support::outage_faults::Fault;

const OK: i32 = Code::Ok as i32;

/// The retries of the tests: the defaults (a budget of 120 s, waits doubling from 0.5 s to 8 s),
/// with no jitter, so that a wait is exactly its nominal length.
fn policy() -> RetryPolicy {
    RetryPolicy {
        jitter_percent: 0,
        ..RetryPolicy::DEFAULT
    }
}

fn secs(s: f64) -> Duration {
    Duration::from_secs_f64(s)
}

/// A rig whose packs retry as [`policy`] says and wait up to `input_walk` for an input walk.
async fn rig_with(input_walk: Duration) -> Rig {
    Rig::with(PackSettings {
        max_blob_bytes: SMALL,
        store_retry: policy(),
        input_walk,
        ..PackSettings::default()
    })
    .await
}

/// A blob packed before an outage, and the Action over a tree that another frontend uploaded
/// into the same packs (so that this frontend has no memory of it and an Execute reads its root
/// Directory from the store): the Action's digest.
struct Build {
    held: (Digest, Bytes),
    action: Digest,
    service: ExecutionService<Queue>,
}

impl Build {
    async fn new(rig: &Rig) -> Self {
        let held = blob_of(b"held before the outage");
        let stored = update(rig, vec![item(&held.0, &held.1)]).await;
        assert_eq!(stored.expect("answered"), [OK]);

        let top = blob_of(b"top");
        let root = blob_of(
            &Directory {
                files: vec![file("top", &top.0)],
                ..Directory::default()
            }
            .encode_to_vec(),
        );
        let command = blob_of(
            &Command {
                arguments: vec!["true".to_owned()],
                ..Command::default()
            }
            .encode_to_vec(),
        );
        let action = blob_of(
            &Action {
                command_digest: Some(proto(&command.0)),
                input_root_digest: Some(proto(&root.0)),
                ..Action::default()
            }
            .encode_to_vec(),
        );
        let keys = PackKeys::new(WriterKind::Frontend, "fe2", "boot1").expect("keys");
        let other = Packs::new(
            Arc::clone(&rig.store),
            rig.index.clone(),
            keys,
            PackSettings {
                max_blob_bytes: SMALL,
                ..PackSettings::default()
            },
        )
        .expect("the other frontend's packs");
        let digest = action.0;
        // Two packs: the Action and the Command in the first, the tree in the second.
        other
            .put(vec![action, command])
            .await
            .expect("uploaded through the other frontend");
        other
            .put(vec![root, top])
            .await
            .expect("uploaded through the other frontend");

        let (_, closing) = kbf_front::closing();
        let service = ExecutionService::with_authorizers(
            rig.backend.clone(),
            Arc::new(Queue::default()),
            closing,
            Arc::new(Authorizers::allow_all()),
        );
        Self {
            held,
            action: digest,
            service,
        }
    }

    /// The answer of an Execute of the Action: its status if it was refused.
    async fn execute(&self) -> Result<(), Status> {
        self.service
            .execute(tonic::Request::new(ExecuteRequest {
                instance_name: "main".to_owned(),
                action_digest: Some(proto(&self.action)),
                ..ExecuteRequest::default()
            }))
            .await
            .map(|_| ())
    }
}

/// The retries counted under `cause`, for every operation.
fn retries(rig: &Rig, cause: Cause) -> u64 {
    let waits = rig.stored.packs().store_waits();
    Op::ALL.iter().map(|op| waits.retries(*op, cause)).sum()
}

/// Whether no operation ran out of its retries.
fn nothing_exhausted(rig: &Rig) -> bool {
    let waits = rig.stored.packs().store_waits();
    Op::ALL.iter().all(|op| waits.exhausted(*op) == 0)
}

/// Catches a build that fails, or hangs, in a store that resets every connection for 30 s: an
/// Execute whose input walk reads a Directory, a batch upload and a batch read, all begun as the
/// outage begins, each complete, and each complete at the same instant, 31.5 s later (the
/// attempts at 0, 0.5, 1.5, 3.5, 7.5, 15.5 and 23.5 s were reset, the one at 31.5 s, after the
/// waits of 0.5, 1, 2, 4, 8, 8 and 8 s, is answered); the retries are counted as made after a
/// connection, none as another cause, and no call ran out of its retries.
#[tokio::test(start_paused = true)]
async fn a_build_completes_through_a_store_that_resets_every_connection_for_30_seconds() {
    let rig = Arc::new(rig_with(Duration::from_secs(120)).await);
    let build = Arc::new(Build::new(&rig).await);
    let fresh = blob_of(b"uploaded during the outage");
    let began = Instant::now();
    rig.store.fault_for(Duration::from_secs(30), Fault::Reset);

    let execute = {
        let build = Arc::clone(&build);
        tokio::spawn(async move { (build.execute().await, Instant::now()) })
    };
    let upload = {
        let (rig, fresh) = (Arc::clone(&rig), fresh.clone());
        tokio::spawn(async move {
            (
                update(&rig, vec![item(&fresh.0, &fresh.1)]).await,
                Instant::now(),
            )
        })
    };
    let reading = {
        let (rig, digest) = (Arc::clone(&rig), build.held.0);
        tokio::spawn(async move { (read(&rig, &[digest]).await, Instant::now()) })
    };

    let (executed, executed_at) = execute.await.expect("ran");
    let (uploaded, uploaded_at) = upload.await.expect("ran");
    let (batch, read_at) = reading.await.expect("ran");

    assert!(executed.is_ok(), "the Execute was refused: {executed:?}");
    assert_eq!(uploaded.expect("answered"), [OK]);
    assert_eq!(batch.expect("answered"), [(OK, build.held.1.to_vec())]);
    for ended in [executed_at, uploaded_at, read_at] {
        assert_eq!(ended - began, secs(31.5));
    }
    assert!(retries(&rig, Cause::Connection) >= 21);
    for cause in [
        Cause::SlowDown,
        Cause::Unavailable,
        Cause::ServerError,
        Cause::RequestTimeout,
    ] {
        assert_eq!(retries(&rig, cause), 0, "{cause:?}");
    }
    assert!(nothing_exhausted(&rig));
    let waits = rig.stored.packs().store_waits();
    assert_eq!(waits.walks_cut(), 0);
    assert!(!waits.down());
}

/// Catches a read that a stalled body costs the whole outage, or that is not retried: with the
/// store stalling every body for 90 s and its client cutting a request at 60 s, a read begun as
/// the stall begins is cut at 60 s ("the store stopped sending"), waits 0.5 s, and its second
/// attempt, begun at 60.5 s, is answered when the stall ends at 90 s: the read completes with its
/// bytes at exactly 90 s, having been retried once, after a connection.
#[tokio::test(start_paused = true)]
async fn a_read_cut_by_a_stalled_body_is_retried_and_completes_when_the_stall_ends() {
    let rig = rig_with(Duration::from_secs(120)).await;
    let build = Build::new(&rig).await;
    let began = Instant::now();
    rig.store.fault_for(
        Duration::from_secs(90),
        Fault::Stall {
            client_timeout: Duration::from_secs(60),
        },
    );

    let batch = read(&rig, &[build.held.0]).await;

    assert_eq!(batch.expect("answered"), [(OK, build.held.1.to_vec())]);
    assert_eq!(Instant::now() - began, Duration::from_secs(90));
    let waits = rig.stored.packs().store_waits();
    assert_eq!(waits.retries(Op::Get, Cause::Connection), 1);
    assert_eq!(retries(&rig, Cause::Connection), 1);
    assert!(nothing_exhausted(&rig));
}

/// Catches an Execute that waits out a store that is down for good, or that is not answered
/// the way a client retries: with the store answering 503 for 5 minutes, an Execute is answered
/// UNAVAILABLE at 119.5 s, inside the budget of 120 s (the attempts at 0, 0.5, 1.5, 3.5 and 7.5 s,
/// then every 8 s up to 119.5 s, were refused, and the next wait of 8 s would end past the
/// budget), its message naming the 503 and the 19 attempts over the budget; the Execute after it
/// is answered UNAVAILABLE at once, the store being taken as down, and one begun after the
/// outage completes, the store being taken as up.
#[tokio::test(start_paused = true)]
async fn an_execute_in_a_store_down_for_5_minutes_is_unavailable_inside_the_budget() {
    let rig = rig_with(Duration::from_secs(120)).await;
    let build = Build::new(&rig).await;
    let began = Instant::now();
    rig.store
        .fault_for(Duration::from_secs(300), Fault::Unavailable);
    let waits = rig.stored.packs().store_waits();

    let refused = build.execute().await.expect_err("the store is down");

    assert_eq!(refused.code(), Code::Unavailable, "{}", refused.message());
    assert_eq!(Instant::now() - began, secs(119.5));
    assert!(
        refused.message().contains("503 ServiceUnavailable")
            && refused
                .message()
                .contains("(still failing after 19 attempts over 119.5 s, the budget being 120 s)"),
        "{}",
        refused.message()
    );
    assert_eq!(waits.exhausted(Op::Get), 1);
    assert_eq!(waits.retries(Op::Get, Cause::Unavailable), 18);
    assert!(waits.down());

    let again = build
        .execute()
        .await
        .expect_err("the store is taken as down");

    assert_eq!(again.code(), Code::Unavailable, "{}", again.message());
    assert_eq!(Instant::now() - began, secs(119.5), "no wait was made");
    assert!(
        again.message().contains("not retried"),
        "{}",
        again.message()
    );
    assert_eq!(waits.exhausted(Op::Get), 2);
    assert_eq!(waits.retries(Op::Get, Cause::Unavailable), 18);

    tokio::time::sleep_until(began + Duration::from_secs(300)).await;
    let completed = build.execute().await;

    assert!(completed.is_ok(), "the Execute was refused: {completed:?}");
    assert_eq!(Instant::now() - began, Duration::from_secs(300));
    assert!(!waits.down());
}

/// Catches an input walk that outlasts its deadline: with the walk limited to 30 s and the store
/// answering 503 to every read of the pack that holds the input tree (the reads of the Action and
/// the Command, which the deadline does not bound, are answered) for 5 minutes, an Execute is answered UNAVAILABLE at exactly 30 s, its message
/// saying the walk of the input tree did not end within 30 s (--pack-input-walk-secs), and one
/// walk is counted cut; the store's call that the walk cut was still within its own budget, so
/// it ran out of nothing.
#[tokio::test(start_paused = true)]
async fn an_input_walk_past_its_deadline_is_unavailable_at_the_deadline() {
    let rig = rig_with(Duration::from_secs(30)).await;
    let build = Build::new(&rig).await;
    let began = Instant::now();
    rig.store.fault_for(
        Duration::from_secs(300),
        Fault::UnavailableFor("0000000000000001"),
    );

    let refused = build.execute().await.expect_err("the walk is cut");

    assert_eq!(refused.code(), Code::Unavailable, "{}", refused.message());
    assert_eq!(Instant::now() - began, Duration::from_secs(30));
    assert!(
        refused
            .message()
            .contains("did not end within 30 s (--pack-input-walk-secs)"),
        "{}",
        refused.message()
    );
    let waits = rig.stored.packs().store_waits();
    assert_eq!(waits.walks_cut(), 1);
    assert!(nothing_exhausted(&rig));
}

/// Catches a streamed upload that a store dying mid-write leaves half stored, or answers in
/// a way a client does not retry: with the store losing the connection of a streamed write after
/// the first piece of its body, a client's ByteStream Write of a blob past [`SMALL`] (which
/// cannot be sent again from here) is answered UNAVAILABLE, nothing is stored under any key of
/// the store, and FindMissingBlobs still lists the blob; the client's second Write, the store
/// being well, is acknowledged whole and reads back byte for byte, and no virtual time passed
/// in all of it, the streamed write not being retried here.
#[tokio::test(start_paused = true)]
async fn a_streamed_upload_cut_by_the_store_is_unavailable_and_stores_nothing() {
    let rig = rig_with(Duration::from_secs(120)).await;
    let large = blob_len(3 * SMALL as usize, 5);
    let began = Instant::now();
    rig.store
        .fault_for(Duration::from_secs(3600), Fault::DieMidStream);

    let cut = write(&rig, pieces(&large.0, &large.1, 512)).await;

    let status = cut.expect_err("the upload was cut");
    assert_eq!(status.code(), Code::Unavailable, "{}", status.message());
    assert_eq!(rig.store.stream_puts(), 1);
    assert!(rig.store.keys().await.is_empty(), "something was stored");
    assert_eq!(missing(&rig, &[large.0]).await.expect("asked"), [large.0]);

    rig.store.fault_for(Duration::ZERO, Fault::DieMidStream);
    let again = write(&rig, pieces(&large.0, &large.1, 512)).await;

    assert_eq!(again.expect("acknowledged"), large.1.len() as i64);
    assert!(missing(&rig, &[large.0]).await.expect("asked").is_empty());
    let back = read_stream(&rig, &read_name(&large.0), 0, 0).await;
    assert_eq!(back.expect("read"), large.1.to_vec());
    assert_eq!(Instant::now() - began, Duration::ZERO);
}
