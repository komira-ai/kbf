//! Membership changes (`Raft::change_membership`): single-server changes, one at a
//! time, kept in the log.
//!
//! The cluster here is in-process and deterministic: each server is a `Host` over a
//! `MemStorage`, messages go into one queue that the test delivers in order, and every
//! tick carries the same entropy. A restart is a crash that keeps only what was synced:
//! the host is rebuilt from its storage's durable state alone.

use std::cell::RefCell;
use std::collections::{BTreeMap, VecDeque};
use std::convert::Infallible;
use std::rc::Rc;

use kbf_raft::{
    Change, ChangeError, Config, Entry, Host, HostError, Machine, MemStorage, Membership, Message,
    Payload, Role, ServerId, Snapshot, Storage, Transport,
};

const S1: ServerId = ServerId(1);
const S2: ServerId = ServerId(2);
const S3: ServerId = ServerId(3);
const S4: ServerId = ServerId(4);
const S5: ServerId = ServerId(5);

type Wire = Rc<RefCell<VecDeque<(ServerId, ServerId, Message)>>>;

/// Puts each message on the shared wire, from this server.
#[derive(Debug)]
struct Outbox {
    from: ServerId,
    wire: Wire,
}

impl Transport for Outbox {
    fn send(&mut self, to: ServerId, msg: Message) {
        self.wire.borrow_mut().push_back((self.from, to, msg));
    }
}

/// The commands applied, in order; membership and blank entries are skipped.
#[derive(Debug, Default)]
struct Applied(Vec<Vec<u8>>);

impl Machine for Applied {
    type Error = Infallible;

    fn apply(&mut self, entry: &Entry) {
        if let Payload::Command(c) = &entry.payload {
            self.0.push(c.clone());
        }
    }

    fn snapshot(&self) -> Vec<u8> {
        self.0.join(&b","[..])
    }

    fn restore(&mut self, snapshot: &Snapshot) -> Result<(), Infallible> {
        self.0 = snapshot
            .state
            .split(|&b| b == b',')
            .filter(|c| !c.is_empty())
            .map(<[u8]>::to_vec)
            .collect();
        Ok(())
    }
}

type Node = Host<MemStorage, Outbox, Applied>;

fn config(id: ServerId, membership: Membership) -> Config {
    Config {
        id,
        membership,
        election_ticks: 10,
        heartbeat_ticks: 3,
        max_entries_per_append: 4,
    }
}

/// The servers, the membership each is opened with (its base: what a bootstrap or a
/// join answer gave it), and the wire.
struct Cluster {
    nodes: BTreeMap<ServerId, Node>,
    bases: BTreeMap<ServerId, Membership>,
    wire: Wire,
}

impl Cluster {
    /// One voter, `S1`: the bootstrap of a new cluster.
    fn bootstrap() -> Self {
        let mut cluster = Self {
            nodes: BTreeMap::new(),
            bases: BTreeMap::new(),
            wire: Rc::default(),
        };
        let base = Membership::new([S1], []).expect("one voter");
        cluster.start(S1, base, MemStorage::new());
        cluster.run_until(|c| c.leader() == Some(S1));
        cluster
    }

    fn start(&mut self, id: ServerId, base: Membership, mut storage: MemStorage) {
        let outbox = Outbox {
            from: id,
            wire: Rc::clone(&self.wire),
        };
        let stored = storage.load().expect("in memory");
        let host = Host::open_stored(
            config(id, base.clone()),
            storage,
            stored,
            outbox,
            Applied::default(),
            0,
        )
        .expect("open");
        self.nodes.insert(id, host);
        self.bases.insert(id, base);
    }

    /// A new server joins: the leader adds it as a learner, and it starts with an empty
    /// log and the membership the leader answered with.
    fn join(&mut self, id: ServerId) {
        self.run_until(|c| c.leader().is_some());
        let leader = self.leader().expect("a leader");
        let at = self
            .node(leader)
            .change_membership(Change::AddLearner(id))
            .expect("the leader adds the learner");
        let answer = self.node(leader).core().membership().clone();
        self.start(id, answer, MemStorage::new());
        self.run_until(|c| c.nodes[&leader].core().commit_index() >= at);
    }

    /// Crashes `id`, keeping only what it synced: it is down until [`Cluster::up`].
    fn down(&mut self, id: ServerId) -> MemStorage {
        let node = self.nodes.remove(&id).expect("running");
        let mut storage = node.storage().clone();
        storage.crash(0);
        storage
    }

    /// Starts `id` again from what its crash left.
    fn up(&mut self, id: ServerId, storage: MemStorage) {
        let base = self.bases[&id].clone();
        self.start(id, base, storage);
    }

    /// Crashes `id` and starts it again at once.
    fn restart(&mut self, id: ServerId) {
        let storage = self.down(id);
        self.up(id, storage);
    }

    fn node(&mut self, id: ServerId) -> &mut Node {
        self.nodes.get_mut(&id).expect("a server")
    }

    fn leader(&self) -> Option<ServerId> {
        self.nodes
            .iter()
            .find(|(_, n)| n.core().role() == Role::Leader)
            .map(|(&id, _)| id)
    }

    /// Delivers every message in order, then ticks every server, until `done` or a
    /// bound on steps.
    fn run_until(&mut self, done: impl Fn(&Self) -> bool) {
        for _ in 0..2_000 {
            if done(self) {
                return;
            }
            self.step();
        }
        panic!("not reached within 2000 steps: {}", self.describe());
    }

    fn step(&mut self) {
        let pending: Vec<_> = self.wire.borrow_mut().drain(..).collect();
        // Each server draws its own election timeouts, so two never time out together
        // forever.
        for (from, to, msg) in pending {
            if let Some(node) = self.nodes.get_mut(&to) {
                node.receive(from, msg, to.0 * 3).expect("in memory");
            }
        }
        for (id, node) in &mut self.nodes {
            node.tick(id.0 * 3).expect("in memory");
        }
    }

    /// Proposes `command` at the leader and runs until every running member (in the
    /// leader's membership) applied it.
    fn commit(&mut self, command: &[u8]) {
        self.run_until(|c| c.leader().is_some());
        let leader = self.leader().expect("a leader");
        self.node(leader)
            .propose(command.to_vec())
            .expect("proposed");
        let members = self.nodes[&leader].core().membership().clone();
        let want = command.to_vec();
        self.run_until(|c| {
            c.nodes
                .iter()
                .filter(|(id, _)| members.contains(**id))
                .all(|(_, n)| n.machine().0.contains(&want))
        });
    }

    /// Runs until the change at the leader commits on every running member of the new
    /// membership. A removed server is not sent its removal.
    fn change(&mut self, change: Change) {
        self.run_until(|c| c.leader().is_some());
        let leader = self.leader().expect("a leader");
        let at = self
            .node(leader)
            .change_membership(change)
            .expect("accepted");
        let members = self.nodes[&leader].core().membership().clone();
        self.run_until(|c| {
            c.nodes
                .iter()
                .filter(|(id, _)| members.contains(**id))
                .all(|(_, n)| n.core().commit_index() >= at)
        });
    }

    fn voters(&self, id: ServerId) -> Vec<ServerId> {
        self.nodes[&id].core().membership().voters().collect()
    }

    fn learners(&self, id: ServerId) -> Vec<ServerId> {
        self.nodes[&id].core().membership().learners().collect()
    }

    fn describe(&self) -> String {
        let one = |(id, n): (&ServerId, &Node)| {
            let core = n.core();
            format!(
                "{id}: {:?} term {:?} commit {:?} voters {:?} learners {:?}",
                core.role(),
                core.hard_state().term,
                core.commit_index(),
                core.membership().voters().collect::<Vec<_>>(),
                core.membership().learners().collect::<Vec<_>>(),
            )
        };
        self.nodes.iter().map(one).collect::<Vec<_>>().join("; ")
    }
}

/// Catches: a membership change that is not replicated or not used (the learner never
/// receives the log, or a promoted server never votes); a learner that votes or counts
/// towards a quorum before it is promoted; a promotion that the cluster forgets when the
/// leader restarts (membership held only in memory, not in the log); and a leader that
/// does not step down when it loses its quorum to a restart.
///
/// From the bootstrap's single voter, S2 and S3 join as learners, catch up (they apply
/// the commands committed before they joined) and are promoted, one change at a time.
/// Then the leader crashes and restarts from its disk: the other two elect a leader
/// between them and commit, and the restarted server rejoins with the three-voter
/// membership and catches up.
#[test]
fn learners_join_catch_up_are_promoted_and_the_cluster_survives_a_leader_restart() {
    let mut cluster = Cluster::bootstrap();
    cluster.commit(b"before-joins");

    cluster.join(S2);
    cluster.run_until(|c| c.nodes[&S2].machine().0.contains(&b"before-joins".to_vec()));
    assert_eq!(cluster.learners(S2), [S2], "S2 joined as a learner");
    assert_eq!(cluster.voters(S1), [S1], "a learner does not vote");
    cluster.change(Change::Promote(S2));

    cluster.join(S3);
    cluster.run_until(|c| c.nodes[&S3].machine().0.contains(&b"before-joins".to_vec()));
    cluster.change(Change::Promote(S3));
    for id in [S1, S2, S3] {
        assert_eq!(cluster.voters(id), [S1, S2, S3], "{}", cluster.describe());
        assert!(cluster.learners(id).is_empty());
    }
    cluster.commit(b"three-voters");

    let old = cluster.leader().expect("a leader");
    let disk = cluster.down(old);
    cluster.run_until(|c| c.leader().is_some());
    let new = cluster.leader().expect("a new leader");
    // Its blank entry is not acknowledged yet: until it commits an entry of its term,
    // a change of the old leader's may still be uncommitted elsewhere.
    assert_eq!(
        cluster.node(new).change_membership(Change::AddLearner(S4)),
        Err(HostError::Change(ChangeError::NotReady))
    );
    cluster.commit(b"after-the-leader-left");

    cluster.up(old, disk);
    cluster.commit(b"after-it-came-back");
    assert_eq!(cluster.voters(old), [S1, S2, S3], "{}", cluster.describe());
    assert!(
        cluster.nodes[&old]
            .machine()
            .0
            .contains(&b"after-the-leader-left".to_vec()),
        "the restarted server caught up with what was committed without it"
    );
    assert_ne!(new, old);
}

/// Catches: a leader that starts a second change before the first is committed (two
/// changes at once can leave the old and new majorities disjoint), changes that make no
/// sense (adding a member, promoting a voter, removing a stranger or the last voter),
/// and a last-voter rule that also keeps a learner beside a single voter.
#[test]
fn an_unsafe_or_meaningless_change_is_refused() {
    let mut cluster = Cluster::bootstrap();
    cluster.join(S2);
    cluster.change(Change::Promote(S2));
    cluster.join(S3);
    cluster.change(Change::Promote(S3));

    // With nothing delivered, the next change stays uncommitted: a second one is
    // refused, and refused without appending anything.
    let leader = cluster.leader().expect("a leader");
    let first = cluster
        .node(leader)
        .change_membership(Change::AddLearner(S4))
        .expect("the first change");
    let last = cluster.nodes[&leader].core().last_log_id().index;
    let second = cluster
        .node(leader)
        .change_membership(Change::AddLearner(S5));
    assert_eq!(
        second,
        Err(HostError::Change(ChangeError::InProgress {
            pending: first
        }))
    );
    assert_eq!(cluster.nodes[&leader].core().last_log_id().index, last);

    cluster.run_until(|c| c.nodes[&leader].core().commit_index() >= first);
    let refusals = [
        (Change::AddLearner(S2), ChangeError::AlreadyMember(S2)),
        (Change::AddLearner(S4), ChangeError::AlreadyMember(S4)),
        (Change::Promote(S2), ChangeError::NotALearner(S2)),
        (Change::Promote(S5), ChangeError::NotALearner(S5)),
        (Change::Remove(S5), ChangeError::NotAMember(S5)),
    ];
    for (change, refused) in refusals {
        let answer = cluster.node(leader).change_membership(change);
        assert_eq!(answer, Err(HostError::Change(refused)), "{change:?}");
    }
    let follower = [S1, S2, S3]
        .into_iter()
        .find(|&s| s != leader)
        .expect("a follower");
    let not_leader = cluster.node(follower).change_membership(Change::Remove(S4));
    assert!(
        matches!(
            not_leader,
            Err(HostError::Change(ChangeError::NotLeader(_)))
        ),
        "{not_leader:?}"
    );

    // The last voter stays; a learner beside it can still be removed.
    let mut alone = Cluster::bootstrap();
    assert_eq!(
        alone.node(S1).change_membership(Change::Remove(S1)),
        Err(HostError::Change(ChangeError::LastVoter(S1)))
    );
    alone.join(S5);
    alone.change(Change::Remove(S5));
    assert_eq!(alone.voters(S1), [S1]);
    assert!(alone.learners(S1).is_empty());
}

/// Catches: a membership that a restart forgets, from the log (changes kept only in
/// memory) or from a snapshot (a compaction that folds the membership entries in and
/// keeps no membership with the snapshot); and a removed server that still counts.
#[test]
fn the_membership_survives_a_restart_from_the_log_and_from_a_snapshot() {
    let mut cluster = Cluster::bootstrap();
    cluster.join(S2);
    cluster.change(Change::Promote(S2));
    cluster.join(S3);
    cluster.change(Change::Promote(S3));
    cluster.join(S4);
    cluster.commit(b"x");

    for id in [S1, S2, S3, S4] {
        cluster.restart(id);
    }
    cluster.commit(b"after-restart");
    for id in [S1, S2, S3, S4] {
        assert_eq!(
            cluster.voters(id),
            [S1, S2, S3],
            "{id}: {}",
            cluster.describe()
        );
        assert_eq!(cluster.learners(id), [S4], "{id}");
    }

    for id in [S1, S2, S3, S4] {
        cluster.node(id).snapshot().expect("a snapshot");
        let held = cluster.nodes[&id].storage().durable().clone();
        let snapshot = held.snapshot.expect("written");
        assert!(
            !held
                .entries
                .iter()
                .any(|e| matches!(e.payload, Payload::Membership(_))),
            "the membership entries are folded into the snapshot"
        );
        assert_eq!(
            snapshot
                .membership
                .as_ref()
                .map(|m| m.voters().collect::<Vec<_>>()),
            Some(vec![S1, S2, S3]),
            "{id}"
        );
        cluster.restart(id);
    }
    cluster.commit(b"after-snapshots");
    for id in [S1, S2, S3, S4] {
        assert_eq!(
            cluster.voters(id),
            [S1, S2, S3],
            "{id}: {}",
            cluster.describe()
        );
        assert_eq!(cluster.learners(id), [S4], "{id}");
    }

    cluster.change(Change::Remove(S3));
    cluster.commit(b"without-s3");
    let applied_by_s3 = cluster.nodes[&S3].machine().0.len();
    cluster.restart(S1);
    cluster.restart(S2);
    cluster.commit(b"still-without-s3");
    assert_eq!(cluster.voters(S1), [S1, S2]);
    assert_eq!(
        cluster.nodes[&S3].machine().0.len(),
        applied_by_s3,
        "a removed server is no longer sent the log"
    );
}

/// Catches: a leader that keeps leading after its own removal commits (it no longer
/// counts in any quorum, and the remaining voters elect a leader of their own), and one
/// that steps down before the removal commits (the change would be left to a new
/// leader that may not hold it).
#[test]
fn a_removed_leader_leads_until_its_removal_commits_then_steps_down() {
    let mut cluster = Cluster::bootstrap();
    cluster.join(S2);
    cluster.change(Change::Promote(S2));
    cluster.join(S3);
    cluster.change(Change::Promote(S3));
    let old = cluster.leader().expect("a leader");

    let at = cluster
        .node(old)
        .change_membership(Change::Remove(old))
        .expect("a leader may remove itself");
    assert_eq!(
        cluster.nodes[&old].core().role(),
        Role::Leader,
        "it leads until the removal commits"
    );
    cluster.run_until(|c| c.nodes[&old].core().commit_index() >= at);
    assert_ne!(cluster.nodes[&old].core().role(), Role::Leader);
    cluster.run_until(|c| c.leader().is_some());
    cluster.commit(b"after-the-removal");
    let rest: Vec<ServerId> = [S1, S2, S3].into_iter().filter(|&s| s != old).collect();
    for id in &rest {
        assert_eq!(&cluster.voters(*id), &rest, "{}", cluster.describe());
    }
}
