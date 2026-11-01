//! The scripted sessions. Each builds a farm of its own over a store of its own and
//! returns the text the farm's behaviour made. A script does not check what the farm did:
//! the golden file next to it is the check, and a reader of the golden file is the judge of
//! whether the farm did right.

use std::time::Duration;

use kbf_sched::NodePhase;
use kbf_server::EpochPolicy;
use kbf_server::farm::NodeAction;
use kbf_types::{LeaseKind, Qos};

use crate::session::{Ended, Job, Session};

fn secs(n: u64) -> Duration {
    Duration::from_secs(n)
}

/// One node; callers of one action share its run; a result is answered and cached unless
/// the action says not to or exited non-zero; a copy of a result is told it is a copy; a
/// finished operation is forgotten after its retention.
pub async fn answer_and_cache() -> String {
    let mut s = Session::start("a result answers every caller of its action").await;
    s.register("node-a", 4, 8, &["container"]).await;
    let build = Job::new("build", 1);
    s.submit(&build).await;
    let build_lease = s.lease_of("build");
    s.heartbeat("node-a", &[build_lease]).await;
    // A second caller of the action that runs joins its operation.
    s.submit(&build).await;
    let lint = Job::new("lint", 2).do_not_cache();
    let test = Job::new("test", 3).qos(Qos::Interactive);
    s.submit(&lint).await;
    s.submit(&test).await;
    s.report("node-a", "build", Ended::Ran(0)).await;
    // The daemon sends the result again: it did not see the acknowledgement.
    s.report("node-a", "build", Ended::Ran(0)).await;
    s.report("node-a", "lint", Ended::Ran(0)).await;
    s.report("node-a", "test", Ended::Ran(2)).await;
    // Past the finished retention the operation is forgotten, and the action runs afresh.
    s.advance(secs(30)).await;
    s.heartbeat("node-a", &[]).await;
    s.advance(secs(31)).await;
    s.heartbeat("node-a", &[]).await;
    s.tick().await;
    s.submit(&build).await;
    s.report("node-a", "build", Ended::Ran(0)).await;
    s.text()
}

/// Work that does not fit waits in the order of its level; the most urgent goes first when
/// room is made.
pub async fn qos_order() -> String {
    let mut s = Session::start("queued work is granted in the order of its level").await;
    s.register("node-a", 4, 8, &["container"]).await;
    s.submit(&Job::new("bulk", 1).qos(Qos::Batch).cores(3))
        .await;
    s.submit(&Job::new("more-bulk", 2).qos(Qos::Batch).cores(3))
        .await;
    s.submit(&Job::new("ci-job", 3).cores(3)).await;
    s.submit(&Job::new("ui-job", 4).qos(Qos::Interactive).cores(3))
        .await;
    s.tick().await;
    for label in ["bulk", "ui-job", "ci-job", "more-bulk"] {
        s.report("node-a", label, Ended::Ran(0)).await;
        // Room made by a result is filled at the next tick.
        s.tick().await;
    }
    s.text()
}

/// A whole-machine lease waits for the node to empty and holds it, so later actions queue
/// behind it; it books the whole node; the node switches its session around it.
pub async fn whole_machine() -> String {
    let mut s = Session::start("a whole-machine lease holds its node alone").await;
    let drivers = ["container", "native-whole-machine"];
    s.register("node-m", 8, 16, &drivers).await;
    s.submit(&Job::new("first", 1).cores(2)).await;
    s.submit(&Job::new("machine", 2).kind(LeaseKind::WholeMachine))
        .await;
    s.submit(&Job::new("behind", 3).cores(2)).await;
    s.tick().await;
    s.report("node-m", "first", Ended::Ran(0)).await;
    s.tick().await;
    let lease = s.lease_of("machine");
    s.phase("node-m", 8, 16, NodePhase::Preparing(lease)).await;
    s.heartbeat("node-m", &[lease]).await;
    s.phase("node-m", 8, 16, NodePhase::Restoring(lease)).await;
    s.report("node-m", "machine", Ended::Ran(0)).await;
    s.phase("node-m", 8, 16, NodePhase::Serving).await;
    s.tick().await;
    s.report("node-m", "behind", Ended::Ran(0)).await;
    s.text()
}

/// A node that goes silent loses its lease after the grace; the work runs on another node;
/// the silent node's late result is refused and its late heartbeat is told to cancel.
pub async fn silent_node_requeue() -> String {
    let mut s = Session::start("a silent node's lease is requeued onto another node").await;
    s.register("node-a", 4, 8, &["container"]).await;
    s.register("node-b", 4, 8, &["container"]).await;
    s.submit(&Job::new("job", 1)).await;
    let first = s.lease_of("job");
    s.heartbeat("node-a", &[first]).await;
    s.heartbeat("node-b", &[]).await;
    s.advance(secs(30)).await;
    s.heartbeat("node-b", &[]).await;
    s.advance(secs(30)).await;
    s.heartbeat("node-b", &[]).await;
    s.advance(secs(2)).await;
    s.tick().await;
    let second = s.lease_of("job");
    s.heartbeat("node-a", &[first]).await;
    s.report_lease("node-a", "job", first, Ended::Ran(0)).await;
    s.heartbeat("node-b", &[second]).await;
    s.report("node-b", "job", Ended::Ran(0)).await;
    s.text()
}

/// A node whose stream ends keeps its lease until the grace has passed; the work then runs
/// on another node, and the node's new stream is told to cancel the lease it lists.
pub async fn stream_ends() -> String {
    let mut s = Session::start("a node whose stream ended loses its lease after the grace").await;
    s.register("node-a", 4, 8, &["container"]).await;
    s.register("node-b", 4, 8, &["container"]).await;
    s.submit(&Job::new("job", 1)).await;
    let first = s.lease_of("job");
    s.disconnect("node-a").await;
    s.tick().await;
    s.advance(secs(30)).await;
    s.heartbeat("node-b", &[]).await;
    s.advance(secs(31)).await;
    s.heartbeat("node-b", &[]).await;
    s.tick().await;
    s.register("node-a", 4, 8, &["container"]).await;
    s.heartbeat("node-a", &[first]).await;
    s.report("node-b", "job", Ended::Ran(0)).await;
    s.text()
}

/// A node whose stream ended and whose daemon restarted: the run is lost, not failed; it
/// runs again, and past its farm reruns the callers are told.
pub async fn lost_runs() -> String {
    let mut s = Session::start("a run the daemon lost runs again, then is given up").await;
    s.register("node-a", 4, 8, &["container"]).await;
    s.register("node-b", 4, 8, &["container"]).await;
    s.submit(&Job::new("flaky", 1)).await;
    for _ in 0..6 {
        let before = s.starts_of("flaky");
        let holder = s.holder_of("flaky");
        s.report(&holder, "flaky", Ended::LostToRestart).await;
        s.tick().await;
        if s.starts_of("flaky") == before {
            break;
        }
    }
    s.text()
}

/// A run killed for passing its own memory limit is booked higher each time and runs
/// again; at the top the callers are told it needs more than any node has. A run killed
/// by the node's memory pressure runs again as a farm fault, a limited number of times.
pub async fn memory_kills() -> String {
    let mut s =
        Session::start("memory kills climb the ladder, and pressure kills are farm faults").await;
    s.register("node-big", 16, 64, &["container"]).await;
    s.submit(&Job::new("link", 1)).await;
    for _ in 0..12 {
        let before = s.starts_of("link");
        s.report("node-big", "link", Ended::OwnLimit).await;
        s.tick().await;
        if s.starts_of("link") == before {
            break;
        }
    }
    s.submit(&Job::new("squeezed", 2)).await;
    for _ in 0..6 {
        let before = s.starts_of("squeezed");
        s.report("node-big", "squeezed", Ended::NodePressure).await;
        s.tick().await;
        if s.starts_of("squeezed") == before {
            break;
        }
    }
    s.submit(&Job::new("broken", 3)).await;
    for _ in 0..6 {
        let before = s.starts_of("broken");
        s.report("node-big", "broken", Ended::Infra).await;
        s.tick().await;
        if s.starts_of("broken") == before {
            break;
        }
    }
    s.submit(&Job::new("garbled", 4)).await;
    s.report("node-big", "garbled", Ended::Invalid).await;
    s.text()
}

/// Work no live node can run waits, is said to wait, and is refused after the wait.
pub async fn unservable_work() -> String {
    let mut s = Session::start("work no node can run waits and is then refused").await;
    s.register("node-a", 4, 8, &["container"]).await;
    s.submit(&Job::new("huge", 1).gib(64)).await;
    s.tick().await;
    // The node heartbeats throughout: it is live, and cannot run the work.
    for _ in 0..5 {
        s.advance(secs(50)).await;
        s.heartbeat("node-a", &[]).await;
    }
    s.advance(secs(49)).await;
    s.heartbeat("node-a", &[]).await;
    s.tick().await;
    s.advance(secs(2)).await;
    s.heartbeat("node-a", &[]).await;
    s.tick().await;
    s.text()
}

/// A process that dies and starts again over its store: the claims it acknowledged are
/// kept, a lease its daemon never saw is granted again under a new id, a result of the
/// dead lease is refused, a finished operation is answered from the store, and no lease id
/// is reused.
pub async fn restart_over_one_store() -> String {
    let mut s = Session::start("a restart over the same store keeps what it acknowledged").await;
    s.register("node-a", 2, 8, &["container"]).await;
    s.submit(&Job::new("finished", 1)).await;
    s.submit(&Job::new("running", 2)).await;
    s.submit(&Job::new("undelivered", 3)).await;
    let running = s.lease_of("running");
    s.report("node-a", "finished", Ended::Ran(0)).await;
    s.tick().await;
    // The queued operation was granted when room was made; its daemon never saw the Start.
    let undelivered = s.lease_of("undelivered");
    s.restart(EpochPolicy::Keep).await;
    s.wait("finished-again", "finished#1").await;
    s.wait("running-again", "running#1").await;
    s.register("node-a", 2, 8, &["container"]).await;
    s.heartbeat("node-a", &[running]).await;
    s.report_lease("node-a", "undelivered", undelivered, Ended::Ran(0))
        .await;
    s.report("node-a", "undelivered", Ended::Ran(0)).await;
    s.report_lease("node-a", "running", running, Ended::Ran(0))
        .await;
    s.tick().await;
    s.text()
}

/// An operator raises the stored epoch, and the next start takes it: its `Welcome` and its
/// term name the new epoch, and the lease of the old one its daemon still runs is taken
/// up when the daemon says `Hello`; another start that bumps the epoch takes the next.
pub async fn epoch_change() -> String {
    let mut s = Session::start("an operator's epoch raise is taken by the next start").await;
    s.register("node-a", 4, 8, &["container"]).await;
    s.submit(&Job::new("old", 1)).await;
    let old = s.lease_of("old");
    s.bump_epoch().await;
    s.restart(EpochPolicy::Keep).await;
    s.register("node-a", 4, 8, &["container"]).await;
    s.heartbeat("node-a", &[old]).await;
    s.report_lease("node-a", "old", old, Ended::Ran(0)).await;
    s.restart(EpochPolicy::Bump).await;
    s.text()
}

/// A farm a handover took the work from queues nothing and accepts no result; resumed, it
/// does both; with its intake closed it keeps what it holds and places nothing new.
pub async fn handover() -> String {
    let mut s = Session::start("a fenced farm takes nothing; a closed intake places nothing").await;
    s.register("node-a", 4, 8, &["container"]).await;
    s.submit(&Job::new("held", 1)).await;
    s.fence().await;
    s.submit(&Job::new("refused", 2)).await;
    s.tick().await;
    s.report("node-a", "held", Ended::Ran(0)).await;
    s.resume().await;
    s.report("node-a", "held", Ended::Ran(0)).await;
    s.close_intake().await;
    s.submit(&Job::new("after-close", 3)).await;
    s.tick().await;
    s.text()
}

/// A cordoned node is offered nothing and the work goes to another; an uncordoned one is
/// offered work again; a drain cordons and, at its deadline, pauses.
pub async fn cordon_and_drain() -> String {
    let mut s = Session::start("cordon and drain steer placement").await;
    s.register("node-a", 4, 8, &["container"]).await;
    s.register("node-b", 4, 8, &["container"]).await;
    s.place("node-a", NodeAction::Cordon).await;
    s.submit(&Job::new("steered", 1)).await;
    s.place("node-a", NodeAction::Uncordon).await;
    s.submit(&Job::new("spread", 2)).await;
    s.place("node-b", NodeAction::Drain(secs(30))).await;
    s.submit(&Job::new("avoids-b", 3)).await;
    s.advance(secs(31)).await;
    s.tick().await;
    s.place("node-c", NodeAction::Cordon).await;
    for label in ["steered", "spread", "avoids-b"] {
        let holder = s.holder_of(label);
        s.report(&holder, label, Ended::Ran(0)).await;
    }
    s.text()
}
