//! Joining a `RaftMetaLog`'s cell (`RaftMetaLog::join`): the leader side of `Join`
//! (docs/design/cluster.md, section 5.3). The raft directory is an in-memory `FaultFs`
//! that a test restarts as a process restart leaves it.

use std::num::NonZeroU64;

use kbf_front::MetaLog;
use kbf_log::{FarmLogId, MachineVersion};
use kbf_meta::{Applied, Command, Retention};
use kbf_raft::{LogIndex, ServerId};
use kbf_server::RaftMetaLog;
use kbf_server::raft_log::{JoinError, JoinRequest};
use kbf_store::{Fault, FaultFs};

type Log = RaftMetaLog<FaultFs>;

const LOG_ID: u64 = 0x2a;

fn id(n: u64) -> FarmLogId {
    FarmLogId::new(NonZeroU64::new(n).expect("non-zero"))
}

async fn open(fs: &FaultFs) -> Log {
    RaftMetaLog::open(fs.clone(), Retention::default(), "kbf/", || Ok(id(LOG_ID)))
        .await
        .expect("open the log")
}

fn voters(log: &Log) -> Vec<ServerId> {
    log.membership().voters().collect()
}

fn learners(log: &Log) -> Vec<ServerId> {
    log.membership().learners().collect()
}

/// Catches: a join that does not commit the node as a learner (a voter would stop a
/// single voter committing alone: the auto-raise of #736 and every commit counts on one
/// voter); an answer that misses the cluster id, the leader, the node or the index of
/// the change; a join retried after a lost answer that commits a second change; and a
/// membership a restart forgets (kept in memory, not in the log).
#[tokio::test]
async fn a_join_adds_the_node_as_a_learner_once_and_a_restart_keeps_it() {
    let fs = FaultFs::new();
    let log = open(&fs).await;
    let request = JoinRequest {
        cluster: id(LOG_ID),
        node: ServerId(2),
    };
    let answer = log.join(&request).expect("joined");
    assert_eq!(answer.cluster, id(LOG_ID));
    assert_eq!(answer.leader, ServerId(1));
    assert_eq!(
        answer.membership.voters().collect::<Vec<_>>(),
        [ServerId(1)]
    );
    assert_eq!(
        answer.membership.learners().collect::<Vec<_>>(),
        [ServerId(2)]
    );
    assert!(answer.config_index > LogIndex(0), "{answer:?}");
    assert_eq!(answer.membership, log.membership());

    let again = log.join(&request).expect("a retried join");
    assert_eq!(again, answer, "a retried join commits nothing new");
    assert_eq!(log.voters(), 1, "a learner does not vote");

    // A single voter still commits alone, and still raises its own machine version.
    let epoch = log.commit(Command::AllocEpoch).await;
    assert!(matches!(epoch, Ok(Applied::Epoch(_))), "{epoch:?}");
    log.raise_machine_version(MachineVersion::V3)
        .expect("a single voter with a learner raises itself");
    drop(log);

    let log = open(&fs.restart()).await;
    assert_eq!(voters(&log), [ServerId(1)]);
    assert_eq!(learners(&log), [ServerId(2)], "the join is in the log");
    assert_eq!(log.join(&request).expect("after a restart"), answer);
}

/// Catches: a join that ignores the cluster id the node expects (a node aimed at the
/// wrong cluster would be attached to it), and a refusal that commits anything.
#[tokio::test]
async fn a_join_for_another_cluster_is_refused_and_commits_nothing() {
    let fs = FaultFs::new();
    let log = open(&fs).await;
    let applied = log.applied_index();
    let request = JoinRequest {
        cluster: id(LOG_ID + 1),
        node: ServerId(2),
    };
    assert_eq!(
        log.join(&request),
        Err(JoinError::WrongCluster {
            expected: id(LOG_ID + 1),
            actual: id(LOG_ID),
        })
    );
    assert_eq!(log.applied_index(), applied);
    assert_eq!(learners(&log), Vec::<ServerId>::new());
}

/// Catches: a join answered though its change never reached the disk (the node would
/// count on a membership a restart forgets).
#[tokio::test]
async fn a_join_the_log_cannot_make_durable_is_refused() {
    let fs = FaultFs::new();
    let log = open(&fs).await;
    fs.fail_at(fs.ops(), Fault::Error);
    let request = JoinRequest {
        cluster: id(LOG_ID),
        node: ServerId(2),
    };
    let refused = log.join(&request);
    assert!(matches!(refused, Err(JoinError::Refused(_))), "{refused:?}");
}

/// Catches: a drain mark that is not committed in the log (a restart forgets the
/// operator's intent), an undrain that leaves the mark, a drain of the only voter (the
/// cell would have no candidate to hand the vote to), and drains of strangers.
#[tokio::test]
async fn a_drain_mark_is_committed_survives_a_restart_and_undrain_clears_it() {
    let fs = FaultFs::new();
    let log = open(&fs).await;
    let request = JoinRequest {
        cluster: id(LOG_ID),
        node: ServerId(2),
    };
    log.join(&request).expect("joined");
    let drained = log.drain(ServerId(2)).expect("drained");
    assert!(drained.is_drained(ServerId(2)));
    assert_eq!(drained, log.membership());
    assert!(log.drain(ServerId(2)).is_err(), "drained already");
    assert!(log.drain(ServerId(1)).is_err(), "the only voter");
    assert!(log.drain(ServerId(7)).is_err(), "not a member");
    assert!(log.undrain(ServerId(7)).is_err(), "not drained");
    drop(log);

    let log = open(&fs.restart()).await;
    assert!(
        log.membership().is_drained(ServerId(2)),
        "the mark is in the log"
    );
    let undrained = log.undrain(ServerId(2)).expect("undrained");
    assert!(!undrained.is_drained(ServerId(2)));
    drop(log);
    let log = open(&fs.restart()).await;
    assert_eq!(log.membership().drained().count(), 0);
}
