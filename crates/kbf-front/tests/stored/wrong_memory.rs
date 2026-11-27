//! What a run costs when the input walk's memory of a tree is wrong. A frontend's count of the
//! losses its packs found is its own, so a pack another frontend forgets does not move it: for
//! up to the memory's 45 s an Execute over a tree that names a blob of that pack is accepted
//! with no call to the pack index, and the first preparation of its run believes the Execute's
//! finding. The blob is then not found where the run's tree is written from it, which the
//! executor answers as the farm's failure and runs again; the preparation after it checks for
//! itself and finds the blob missing, the answer the Execute gives when it finds the blob gone
//! itself. Once the client uploads the blob again, the run is prepared.
//!
//! Every test says the behaviour it asserts.

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use kbf_auth::Authorizers;
use kbf_front::prepare_run::{ClosureError, ClosureLimits, PrepareError, PutDirPrep};
use kbf_front::{ExecutionService, PackKeyMint, PackSettings, Packs};
use kbf_pack::{MIN_AGE, PackKeys, WriterKind};
use kbf_pack_index::{Forgot, PackIndex as _};
use kbf_proto::reapi::ExecuteRequest;
use kbf_proto::reapi::execution_server::Execution as _;
use kbf_storage::StorageError;

use crate::empty_blob::{Queue, proto};
use crate::execute_walk::action_tree;
use crate::rig::{Rig, SMALL};

const LEASE: &str = "00000000000000010000000000000002";
const MINT_SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";
const WALL: Duration = Duration::from_secs(7_200);
const SLACK: Duration = Duration::from_secs(600);

/// Catches a wrong memory believed by more than one preparation of a run, or one that ends in
/// anything but the blob missing once a preparation checks: another frontend uploads an
/// Action's tree, the file `top` in a pack of its own; this frontend Executes the Action (the
/// tree is remembered, the finding vouched); the index forgets the pack of `top` as another
/// frontend's loss guard would, which this frontend does not learn of. A second Execute within
/// the memory's time is accepted and asks the index about nothing; the first preparation of its
/// run takes the finding and asks nothing either, and `top` is not found where the tree would
/// be written from; the next preparation asks, and answers `top` missing. Uploaded again, the
/// run is prepared.
// Tokio's clock is not paused: the memory's 45 s is the real one, and the test makes no wait.
// A paused clock moves on by itself while the index's blocking calls run, which could take it
// past the 45 s.
#[tokio::test]
async fn a_pack_another_frontend_forgot_costs_one_attempt_and_is_then_missing() {
    let rig = Rig::new().await;
    let settings = PackSettings {
        max_blob_bytes: SMALL,
        ..PackSettings::default()
    };
    let keys = PackKeys::new(WriterKind::Frontend, "fe2", "boot1").expect("keys");
    let other = Packs::new(Arc::clone(&rig.store), rig.index.clone(), keys, settings)
        .expect("the other frontend's packs");
    let (mut blobs, action) = action_tree("forgotten");
    let top = blobs.remove(4);
    other.put(vec![top.clone()]).await.expect("top uploaded");
    other.put(blobs).await.expect("the rest uploaded");
    let at = rig.index.where_is(&[top.0, action]);
    let (top_pack, action_pack) = (at[0].clone().expect("top"), at[1].clone().expect("action"));
    assert_ne!(
        top_pack.pack_key, action_pack.pack_key,
        "top alone in its pack"
    );

    let (_, closing) = kbf_front::closing();
    let service = ExecutionService::with_authorizers(
        rig.backend.clone(),
        Arc::new(Queue::default()),
        closing,
        Arc::new(Authorizers::allow_all()),
    );
    let execute = || {
        service.execute(tonic::Request::new(ExecuteRequest {
            instance_name: "main".to_owned(),
            action_digest: Some(proto(&action)),
            skip_cache_lookup: true,
            ..ExecuteRequest::default()
        }))
    };
    let prep = PutDirPrep::new(
        rig.backend.clone(),
        PackKeyMint::new(MINT_SECRET).expect("a mint"),
        SLACK,
        ClosureLimits::DEFAULT,
    );
    let now = SystemTime::UNIX_EPOCH + Duration::from_secs(1_790_000_040);
    let prepare = || prep.prepare_put_dir(LEASE, &action, None, now, WALL);
    let asked = || rig.stored.packs().metrics().asked.get();

    assert!(execute().await.is_ok(), "the first Execute was refused");
    rig.wait(MIN_AGE);
    let forgot = rig.index.inner.forget_pack(&top_pack.pack_key, MIN_AGE);
    assert!(
        matches!(forgot, Ok(Forgot::Forgotten { rows: 1 })),
        "{forgot:?}"
    );
    let before = asked();
    let accepted = execute().await.is_ok();
    let after_execute = asked();
    let believed = prepare().await;
    let after_believed = asked();
    let read = rig.backend.read_stream(&top.0).await.err();
    let checked = prepare().await;
    other
        .put(vec![top.clone()])
        .await
        .expect("top uploaded again");
    let again = prepare().await;

    assert!(accepted, "the memory is believed for its time");
    assert_eq!(after_execute, before, "the second Execute asks nothing");
    assert!(believed.is_ok(), "{:?}", believed.err());
    assert_eq!(after_believed, before, "the first preparation asks nothing");
    assert!(
        matches!(read, Some(StorageError::NotFound(digest)) if digest == top.0),
        "{read:?}"
    );
    assert!(
        matches!(
            &checked,
            Err(PrepareError::Closure(ClosureError::Missing { what: "a file", digest }))
                if *digest == top.0
        ),
        "{:?}",
        checked.err()
    );
    assert!(again.is_ok(), "{:?}", again.err());
}
