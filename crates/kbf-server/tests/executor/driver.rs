//! The task that steps an executor beside the listeners: it steps until it is stopped, stops by
//! itself when another frontend has live jobs, and cancels what it submitted when it ends.

use std::time::Duration;

use js_proto::v1::{Job, JobState};
use kbf_job_client::fake::Fault;
use kbf_server::executor::Running;
use tonic::Code;

use crate::pool::{earlier, job, record, spec_of};
use crate::rig::{Posted, Rig, cancels, settle, submits};

/// Lets `seconds` pass in steps of 5 s, the farm ticking after each. Each step moves the clock
/// a second at a time and lets the driver's task run in between: a call it has in flight to the
/// job service on loopback is answered before its bound passes, as on a real clock.
async fn pass(rig: &Rig, seconds: u64) {
    for _ in 0..seconds.div_ceil(5) {
        for _ in 0..5 {
            rig.advance(Duration::from_secs(1)).await;
            settle().await;
        }
        rig.farm.tick().await;
        settle().await;
    }
}

/// Seconds enough for two checks for a rival, and the watch of an earlier process's jobs.
fn long(rig: &Rig) -> u64 {
    rig.cfg.adopt_watch().as_secs() * 2 + 30
}

/// Lets time pass in steps of 5 s, at most `steps` of them, until `running` says why it
/// stopped. No wall-clock assertion: a driver that never stops is `None`, not a hang.
async fn stopped_within(rig: &Rig, running: &mut Running, steps: u64) -> Option<String> {
    let failed = running.failed();
    tokio::pin!(failed);
    for _ in 0..steps {
        tokio::select! {
            why = &mut failed => return Some(why),
            () = pass(rig, 5) => {}
        }
    }
    None
}

/// Whether `running` has not stopped by itself.
async fn goes_on(running: &mut Running) -> bool {
    tokio::time::timeout(Duration::ZERO, running.failed())
        .await
        .is_err()
}

/// The workers the farm knows, in node-id order.
fn nodes(rig: &Rig) -> Vec<String> {
    let view = rig.farm.nodes();
    view.nodes.into_iter().map(|node| node.node_id).collect()
}

/// The frontend a posted job was made for.
fn owner(posted: &Posted) -> String {
    posted
        .spec
        .labels
        .get("kbf-frontend")
        .cloned()
        .unwrap_or_default()
}

/// Whether `job` has been asked to stop, or has ended cancelled.
fn stopped(job: &Job) -> bool {
    job.cancel_requested || job.state() == JobState::Cancelled
}

/// Catches a driver that does not step the executor, or leaves its jobs behind: while it runs
/// the farm has its workers and an action becomes a job, and once stopped the job is cancelled
/// and the stop has waited for it.
#[tokio::test(start_paused = true)]
async fn the_driver_holds_workers_while_it_runs_and_cancels_its_jobs_when_stopped() {
    let rig = Rig::start(&["node1"], 2, |_| {}).await;
    let running = Running::spawn(rig.restarted_executor());
    pass(&rig, 20).await;
    assert_eq!(nodes(&rig), ["js:front-1-0", "js:front-1-1"]);
    let _caller = rig.submit(1);
    pass(&rig, 20).await;
    let posted = rig.jobs_of(1);
    assert_eq!(posted.len(), 1, "{posted:?}");
    let id = posted[0].id().to_owned();
    assert!(!stopped(&job(&rig, &id)));
    running.stop().await;
    assert_eq!(job(&rig, &id).state(), JobState::Cancelled);
    assert_eq!(cancels(&rig.fake.calls()), [id]);
}

/// Catches a second frontend that has jobs beside this one: the executor runs on exactly one
/// frontend, so the driver that finds another frontend's live job before its first pass says
/// so, registers no worker and submits nothing, and leaves that frontend's job alone.
#[tokio::test(start_paused = true)]
async fn a_rival_found_at_the_start_stops_the_driver_before_it_submits_anything() {
    let rig = Rig::start(&["node1", "node2"], 1, |_| {}).await;
    let rival = earlier(&rig, "front-2", "elsewhere");
    let _caller = rig.submit(1);
    let mut running = Running::spawn(rig.restarted_executor());
    let why = stopped_within(&rig, &mut running, 40)
        .await
        .expect("the driver did not stop");
    assert!(why.contains("front-2"), "{why}");
    running.stop().await;
    assert!(submits(&rig.fake.calls()).is_empty());
    assert!(nodes(&rig).is_empty());
    assert!(cancels(&rig.fake.calls()).is_empty());
    assert!(!stopped(&job(&rig, &rival)));
}

/// Catches a running driver stopped by a frontend that came after it: the older frontend goes
/// on, with its worker and its job, through the checks that find the newer one's job, and
/// the newer one's job is not touched.
#[tokio::test(start_paused = true)]
async fn a_rival_that_appears_later_does_not_stop_the_older_driver() {
    let rig = Rig::start(&["node1", "node2"], 1, |_| {}).await;
    let mut running = Running::spawn(rig.restarted_executor());
    let _caller = rig.submit(1);
    pass(&rig, 20).await;
    let mine = rig.jobs_of(1);
    assert_eq!(mine.len(), 1, "{mine:?}");
    let rival = earlier(&rig, "front-2", "elsewhere");
    pass(&rig, long(&rig)).await;
    assert!(goes_on(&mut running).await, "the older frontend stopped");
    assert_eq!(nodes(&rig), ["js:front-1-0"]);
    assert!(!stopped(&job(&rig, mine[0].id())), "it lost its job");
    running.stop().await;
    assert_eq!(cancels(&rig.fake.calls()), [mine[0].id().to_owned()]);
    assert!(!stopped(&job(&rig, &rival)));
}

/// Catches two frontends that started together, each submitting before it saw the other's
/// job, both stopping at the next check and the farm left with no executor, or neither stopping
/// and the farm with two: exactly one stops and cancels its job, and it is the one whose oldest
/// job is the younger (a tie goes to the lower frontend id). Time passes between the two
/// submits, so the order the job service recorded decides when it can.
#[tokio::test(start_paused = true)]
async fn of_two_frontends_that_started_together_exactly_one_stops() {
    let rig = Rig::start(&["node1", "node2"], 1, |cfg| cfg.slots = 1).await;
    let mut a = Running::spawn(rig.other_executor("front-1"));
    let mut b = Running::spawn(rig.other_executor("front-2"));
    pass(&rig, 20).await;
    let _first = rig.submit(1);
    pass(&rig, 5).await;
    let _second = rig.submit(2);
    pass(&rig, long(&rig)).await;
    let flags = [!goes_on(&mut a).await, !goes_on(&mut b).await];
    let jobs = rig.jobs();
    let oldest = jobs
        .iter()
        .map(|p| (p.job.times.as_ref().map_or(0, |t| t.queued_at_ms), owner(p)))
        .min()
        .expect("a job")
        .1;
    assert_eq!(
        flags,
        [oldest != "front-1", oldest != "front-2"],
        "the oldest job is {oldest}'s: {flags:?}"
    );
    for posted in &jobs {
        assert_eq!(stopped(&posted.job), owner(posted) != oldest, "{posted:?}");
    }
    a.stop().await;
    b.stop().await;
}

/// Catches a second process under the same frontend id that cancels the first one's jobs (a
/// config copied to a second frontend): it finds the first making a new job while it watches,
/// stops with an error that says the id is shared, and the first process keeps its worker and
/// its jobs.
#[tokio::test(start_paused = true)]
async fn a_second_process_under_a_live_frontends_id_stops_and_leaves_its_jobs_alone() {
    let rig = Rig::start(&["node1", "node2"], 1, |_| {}).await;
    let mut first = Running::spawn(rig.restarted_executor());
    let _one = rig.submit(1);
    pass(&rig, 20).await;
    assert_eq!(rig.jobs_of(1).len(), 1);
    let mut second = Running::spawn(rig.restarted_executor());
    pass(&rig, 5).await;
    let _two = rig.submit(2);
    let steps = long(&rig) / 5;
    let why = stopped_within(&rig, &mut second, steps)
        .await
        .expect("the second process did not stop");
    assert!(why.contains("same id"), "{why}");
    second.stop().await;
    let jobs = rig.jobs();
    assert_eq!(jobs.len(), 2, "{jobs:?}");
    assert!(jobs.iter().all(|p| !stopped(&p.job)), "{jobs:?}");
    assert!(cancels(&rig.fake.calls()).is_empty());
    assert!(goes_on(&mut first).await, "the first process stopped");
    first.stop().await;
}

/// Catches a list of jobs that could not be read being taken for a rival, or for the want of
/// one: nothing is concluded, the executor goes on and registers its worker, and the next check
/// reads the list and finds the rival.
#[tokio::test(start_paused = true)]
async fn a_list_that_cannot_be_read_is_no_rival() {
    let rig = Rig::start(&["node1"], 1, |_| {}).await;
    let rival = earlier(&rig, "front-2", "elsewhere");
    rig.fake.inject_on(
        "ListJobs",
        Fault::Status {
            code: Code::Unavailable,
            message: "try later".to_owned(),
        },
    );
    let mut running = Running::spawn(rig.restarted_executor());
    pass(&rig, 20).await;
    assert!(goes_on(&mut running).await, "nothing was concluded");
    assert_eq!(nodes(&rig), ["js:front-1-0"], "and it goes on");
    let steps = long(&rig) / 5;
    let why = stopped_within(&rig, &mut running, steps)
        .await
        .expect("the next check did not find the rival");
    assert!(why.contains("front-2"), "{why}");
    running.stop().await;
    assert!(!stopped(&job(&rig, &rival)));
}

/// Catches jobs that are not another frontend's live jobs being taken for a rival: a job of
/// another frontend that has ended, a job with no frontend label at all, and a job of another
/// frontend whose cancel was asked for are no rival, and the driver goes on and registers its
/// worker.
#[tokio::test(start_paused = true)]
async fn jobs_that_are_not_a_live_job_of_another_frontend_are_no_rival() {
    let rig = Rig::start(&["node1"], 1, |_| {}).await;
    let ended = earlier(&rig, "front-2", "elsewhere");
    rig.host_finishes(&ended, "node1", 0);
    let mut bare = spec_of("front-2", "elsewhere");
    bare.labels.clear();
    record(&rig, bare);
    let leaving = earlier(&rig, "front-2", "elsewhere");
    rig.host_runs(&leaving, "node1");
    rig.api.cancel_job(&leaving).await.expect("asked");

    let mut running = Running::spawn(rig.restarted_executor());
    pass(&rig, 20).await;
    pass(&rig, long(&rig)).await;
    assert!(goes_on(&mut running).await, "no rival");
    assert_eq!(nodes(&rig), ["js:front-1-0"]);
    running.stop().await;
    assert_eq!(
        cancels(&rig.fake.calls()),
        [leaving],
        "the driver cancelled a job it did not make"
    );
    assert_eq!(job(&rig, &ended).state(), JobState::Succeeded);
}

/// Catches a process under this frontend's id that starts after this one has taken up the
/// earlier jobs going unseen, and a job on its way out being taken for one: once the adoption is
/// done, a live job of another instance under the same id is a twin, named as such, unless its
/// cancel was asked for.
#[tokio::test(start_paused = true)]
async fn a_twin_that_appears_after_the_adoption_is_found_by_the_rival_check() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.exec.step().await;
    assert_eq!(rig.exec.workers().len(), 1, "the adoption is done");
    assert_eq!(rig.exec.rival().await, None);

    let leaving = earlier(&rig, "front-1", "intruder");
    rig.host_runs(&leaving, "node1");
    rig.api
        .cancel_job(&leaving)
        .await
        .expect("the cancel is taken");
    assert_eq!(
        rig.exec.rival().await,
        None,
        "a job being cancelled is no one's"
    );

    earlier(&rig, "front-1", "intruder");
    assert_eq!(
        rig.exec.rival().await.as_deref(),
        Some(
            "another frontend runs under the same id \"front-1\": --executor-frontend-id must be \
             different for each frontend"
        )
    );
}
