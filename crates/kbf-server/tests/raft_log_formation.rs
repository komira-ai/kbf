//! Forming a cluster, as the gateway directs it (docs/design/cluster.md, section 5.3):
//! `RaftMetaLog::create` for `CREATE` and `RaftMetaLog::join_cluster` for `JOIN`. Both
//! are keyed by the cluster id and idempotent, and a node whose raft directory holds a
//! cluster never creates or joins another.

use std::num::NonZeroU64;

use kbf_front::MetaLog;
use kbf_log::FarmLogId;
use kbf_meta::{Command, Retention};
use kbf_raft::{LogIndex, Membership, ServerId};
use kbf_server::raft_log::JoinAnswer;
use kbf_server::{RaftLogError, RaftMetaLog};
use kbf_store::FaultFs;

type Log = RaftMetaLog<FaultFs>;

const X: u64 = 0x2a;
const Y: u64 = 0x2b;

fn id(n: u64) -> FarmLogId {
    FarmLogId::new(NonZeroU64::new(n).expect("non-zero"))
}

async fn create(fs: &FaultFs, cluster: u64) -> Result<Log, RaftLogError> {
    RaftMetaLog::create(fs.clone(), Retention::default(), "kbf/", id(cluster)).await
}

async fn join(fs: &FaultFs, cluster: u64) -> Result<Log, RaftLogError> {
    RaftMetaLog::join_cluster(
        fs.clone(),
        Retention::default(),
        "kbf/",
        ServerId(2),
        &answer(cluster),
    )
    .await
}

/// What cluster `cluster`'s leader, server 1, answered node 2's join with.
fn answer(cluster: u64) -> JoinAnswer {
    JoinAnswer {
        cluster: id(cluster),
        leader: ServerId(1),
        membership: Membership::new([ServerId(1)], [ServerId(2)]).expect("valid"),
        config_index: LogIndex(3),
    }
}

fn refused<T>(result: Result<T, RaftLogError>) -> RaftLogError {
    match result {
        Ok(_) => panic!("not refused"),
        Err(e) => e,
    }
}

/// Catches: a create that is not idempotent (a repeat would draw a new id or commit
/// a second `Bootstrap`), and a node holding cluster X that creates cluster Y when told
/// to (the old open read the record and ignored the id it was given).
#[tokio::test]
async fn a_create_is_idempotent_and_a_second_create_with_another_id_is_refused() {
    let fs = FaultFs::new();
    let log = create(&fs, X).await.expect("created");
    assert_eq!(log.log_id(), id(X));
    assert_eq!(log.voters(), 1);
    let epoch = log.commit(Command::AllocEpoch).await;
    assert!(epoch.is_ok(), "{epoch:?}");
    let applied = log.applied_index();
    drop(log);

    let fs = fs.restart();
    let log = create(&fs, X).await.expect("created again");
    assert_eq!(log.log_id(), id(X));
    assert_eq!(log.bootstraps(), 1, "a repeated create commits nothing new");
    assert!(log.applied_index() > applied, "the same log, reopened");
    drop(log);

    let other = refused(create(&fs.restart(), Y).await);
    assert!(
        matches!(other, RaftLogError::OtherCluster { held, asked } if held == id(X) && asked == id(Y)),
        "{other:?}"
    );
}

/// Catches: a node that joined cluster X and then creates a cluster when told to (a
/// stale or confused gateway would make it a second, one-voter cluster), whether the
/// create names X or another id.
#[tokio::test]
async fn a_create_after_a_join_is_refused() {
    let fs = FaultFs::new();
    let log = join(&fs, X).await.expect("joined");
    assert_eq!(log.log_id(), id(X));
    assert_eq!(log.membership(), answer(X).membership);
    assert!(log.leadership().is_none(), "a learner does not lead");
    drop(log);

    let fs = fs.restart();
    let same = refused(create(&fs, X).await);
    assert!(
        matches!(same, RaftLogError::Joined(c) if c == id(X)),
        "{same:?}"
    );
    let other = refused(create(&fs, Y).await);
    assert!(
        matches!(other, RaftLogError::OtherCluster { .. }),
        "{other:?}"
    );
}

/// Catches: a rejoin of the same cluster that writes a new record or changes the
/// membership, a join of another cluster accepted by a node that holds one, and a
/// creator that refuses to "join" the cluster it created (it is a member already).
#[tokio::test]
async fn a_rejoin_of_the_same_cluster_is_a_no_op_and_another_cluster_is_refused() {
    let fs = FaultFs::new();
    drop(join(&fs, X).await.expect("joined"));
    let record = fs.contents(kbf_server::raft_log::RECORD).expect("a record");
    let fs = fs.restart();
    let log = join(&fs, X).await.expect("rejoined");
    assert_eq!(log.log_id(), id(X));
    assert_eq!(log.membership(), answer(X).membership);
    drop(log);
    assert_eq!(
        fs.contents(kbf_server::raft_log::RECORD),
        Some(record),
        "a rejoin writes nothing"
    );
    let other = refused(join(&fs.restart(), Y).await);
    assert!(
        matches!(other, RaftLogError::OtherCluster { .. }),
        "{other:?}"
    );

    let created = FaultFs::new();
    drop(create(&created, X).await.expect("created"));
    let log = join(&created.restart(), X)
        .await
        .expect("the creator is a member");
    assert_eq!(log.voters(), 1);
    assert!(log.leadership().is_some(), "the creator still leads");
}
