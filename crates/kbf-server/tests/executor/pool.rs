//! The pool of virtual workers, and the jobs an earlier process of the frontend left.

use std::sync::Arc;

use js_proto::v1::{Job, JobSpec, JobState, Needs};
use kbf_job_client::fake::{Call, Fault};
use kbf_server::executor::{Config, ConfigError, Deps, Executor};
use kbf_types::{LeaseKind, WorkerId};
use tonic::Code;

use crate::rig::{GIB, IMAGE, Rig, cancels, submits};

/// The spec of a job that the process `instance` of the frontend `frontend` made.
pub fn spec_of(frontend: &str, instance: &str) -> JobSpec {
    JobSpec {
        image: IMAGE.trim_start_matches("docker://").to_owned(),
        needs: Some(Needs {
            cpu_millis: 1_000,
            memory_bytes: GIB,
            ..Needs::default()
        }),
        wall_limit_seconds: Some(600),
        labels: [("kbf-frontend", frontend), ("kbf-instance", instance)]
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
            .collect(),
        ..JobSpec::default()
    }
}

/// Records `spec` on the job service without a call, and gives the id of the job.
pub fn record(rig: &Rig, spec: JobSpec) -> String {
    rig.fake.record(spec).expect("a job the service takes").id
}

/// Records a queued job of the process `instance` of the frontend `frontend`.
pub fn earlier(rig: &Rig, frontend: &str, instance: &str) -> String {
    record(rig, spec_of(frontend, instance))
}

/// The job `id` as the job service holds it now.
pub fn job(rig: &Rig, id: &str) -> Job {
    rig.fake
        .jobs()
        .into_iter()
        .find(|job| job.id == id)
        .expect("the job service holds the job")
}

/// The leases the farm holds on the worker `id`, if the farm knows it.
fn placed(rig: &Rig, id: &str) -> Option<u32> {
    let look = rig.farm.update_look();
    look.iter()
        .find(|n| n.id.to_string() == id)
        .map(|n| n.leases)
}

/// Catches an executor that reaches for the job service before it is run: building one asks
/// for nothing, and a config that breaks a rule is refused before anything is built. (What a
/// server without an executor never does is tested at the frontend, in src/frontend/executor_tests.rs.)
#[tokio::test(start_paused = true)]
async fn building_an_executor_asks_the_job_service_for_nothing() {
    let rig = Rig::start(&["node1"], 1, |_| {}).await;
    assert!(rig.fake.calls().is_empty());
    assert!(rig.exec.workers().is_empty());
    let deps = Deps {
        farm: rig.farm.clone(),
        client: rig.fake.client().expect("a client"),
        ingest: rig.ingest.clone(),
        presign: rig.presign.clone(),
        inputs: rig.inputs.clone(),
        metrics: Arc::default(),
    };
    let none = Config {
        hosts: 0,
        ..rig.cfg.clone()
    };
    let refused = Executor::new(none, deps);
    assert_eq!(refused.err(), Some(ConfigError::NoHosts));
    assert!(rig.fake.calls().is_empty());
}

/// Catches a pool that registers one worker twice or none at all: each of the frontend's
/// workers is a node of its own, `js:<frontend>-<n>`, of the declared cores and memory on
/// linux and the declared architecture, that takes ordinary and whole-machine actions: a
/// whole-machine action becomes one job of the whole host, and an ordinary action submitted
/// after it still becomes a job (that a whole-machine action books its worker whole is held by
/// `whole.rs`).
#[tokio::test(start_paused = true)]
async fn each_worker_is_a_node_of_the_declared_shape_that_takes_whole_machine_actions_too() {
    let mut rig = Rig::start(&["node1", "node2", "node3"], 2, |cfg| {
        cfg.cpu_millis = 8_000
    })
    .await;
    rig.pump().await;
    let workers: Vec<String> = rig
        .exec
        .workers()
        .iter()
        .map(|w| w.worker.to_string())
        .collect();
    assert_eq!(workers, ["js:front-1-0", "js:front-1-1"]);
    let look = rig.farm.update_look();
    assert_eq!(look.len(), 2, "the farm knows another node: {look:?}");
    for node in &look {
        assert_eq!((node.os.as_str(), node.arch), ("linux", "x86_64"));
    }
    for name in &workers {
        let detail = rig
            .farm
            .node_detail(&WorkerId::new(name.clone()))
            .expect("the node is read")
            .expect("the farm knows the node");
        assert_eq!(detail.capacity.cpu_millis, 8_000, "{name}");
        assert_eq!(
            detail.capacity.memory_bytes,
            rig.cfg.booked_memory_gib() * GIB,
            "{name}"
        );
    }
    let whole = rig.submit_kind(1, LeaseKind::WholeMachine);
    rig.pump().await;
    let jobs = rig.jobs_of(1);
    assert_eq!(jobs.len(), 1, "{jobs:?}");
    assert!(jobs[0].spec.needs.as_ref().is_some_and(|n| n.whole_host));
    let action = rig.submit(2);
    rig.pump().await;
    assert_eq!(rig.jobs_of(2).len(), 1);
    assert_eq!(rig.jobs().len(), 2);
    drop((whole, action));
}

/// Catches a worker that takes more actions than the slots it declared: with two slots and
/// cores for more, three actions make two jobs, and the third waits.
#[tokio::test(start_paused = true)]
async fn a_worker_takes_no_more_actions_than_its_slots() {
    let mut rig = Rig::start(&["node1"], 1, |cfg| cfg.cpu_millis = 8_000).await;
    rig.pump().await;
    let _callers = [rig.submit(1), rig.submit(2), rig.submit(3)];
    rig.pump().await;
    assert_eq!(rig.jobs().len(), 2, "{:?}", rig.jobs());
}

/// Catches an executor that submits what the farm placed before it knows the jobs of an
/// earlier process, or that takes up the wrong ones. A restarted frontend lists its live jobs
/// by its own label and submits nothing, with no worker registered, until the process that
/// made them is seen to make no new one; then it cancels exactly the queued and the running
/// jobs of that process (nothing of the farm maps to them any more, and each holds a host's
/// room), leaves an ended job and another frontend's job alone, counts the two it cancelled,
/// and only then registers its workers and submits the action the farm places.
#[tokio::test(start_paused = true)]
async fn the_frontend_adopts_its_own_jobs_after_a_restart() {
    let mut rig = Rig::start(&["node1"], 2, |_| {}).await;
    let queued = earlier(&rig, "front-1", "earlier");
    let running = earlier(&rig, "front-1", "earlier");
    rig.host_runs(&running, "node1");
    let ended = earlier(&rig, "front-1", "earlier");
    rig.host_finishes(&ended, "node1", 0);
    let other = earlier(&rig, "front-2", "elsewhere");
    let _caller = rig.submit(1);

    rig.exec.step().await;
    let calls = rig.fake.calls();
    assert_eq!(rig.methods(), ["ListJobs"]);
    let Some(Call::ListJobs(list)) = calls.first() else {
        panic!("not a listing: {calls:?}");
    };
    assert_eq!(list.labels.len(), 1, "{list:?}");
    assert_eq!(
        list.labels.get("kbf-frontend").map(String::as_str),
        Some("front-1")
    );
    let live = [
        JobState::Queued,
        JobState::Placed,
        JobState::Starting,
        JobState::Running,
    ];
    assert_eq!(list.states, live.map(i32::from));
    assert!(rig.exec.workers().is_empty());
    rig.pump().await;
    assert_eq!(rig.methods(), ["ListJobs"], "asked again within the watch");
    assert!(
        rig.exec.workers().is_empty(),
        "a worker while the watch ran"
    );
    assert!(submits(&rig.fake.calls()).is_empty());
    assert_eq!(rig.counter("kbf_executor_jobs_adopted_cancelled_total"), 0);

    rig.watch_over().await;
    rig.exec.step().await;
    assert_eq!(
        cancels(&rig.fake.calls()),
        [queued.clone(), running.clone()]
    );
    assert_eq!(rig.counter("kbf_executor_jobs_adopted_cancelled_total"), 2);
    assert_eq!(job(&rig, &queued).state(), JobState::Cancelled);
    let stopping = job(&rig, &running);
    assert_eq!(stopping.state(), JobState::Running);
    assert!(stopping.cancel_requested);
    let finished = job(&rig, &ended);
    assert_eq!(finished.state(), JobState::Succeeded);
    assert!(!finished.cancel_requested);
    let theirs = job(&rig, &other);
    assert_eq!(theirs.state(), JobState::Queued);
    assert!(!theirs.cancel_requested);
    let workers: Vec<String> = rig
        .exec
        .workers()
        .iter()
        .map(|w| w.worker.to_string())
        .collect();
    assert_eq!(workers, ["js:front-1-0", "js:front-1-1"]);

    rig.pump().await;
    assert_eq!(rig.jobs_of(1).len(), 1, "the action was not submitted");
    assert_eq!(cancels(&rig.fake.calls()).len(), 2);
}

/// Catches a restarted frontend that cancels the jobs of a process that is alive (a config
/// copied to a second frontend): a job of the earlier instance that is new at the second look
/// shows it makes jobs, so the executor stops with the message that says the id is shared,
/// cancels nothing, registers no worker and asks for nothing more.
#[tokio::test(start_paused = true)]
async fn an_executor_that_sees_a_new_job_of_the_earlier_process_stops_and_cancels_nothing() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    earlier(&rig, "front-1", "alive");
    rig.exec.step().await;
    assert_eq!(rig.exec.twin(), None);
    earlier(&rig, "front-1", "alive");
    rig.watch_over().await;
    rig.exec.step().await;
    assert_eq!(
        rig.exec.twin(),
        Some(
            "another frontend runs under the same id \"front-1\": --executor-frontend-id must be different for each frontend"
        )
    );
    assert!(cancels(&rig.fake.calls()).is_empty());
    assert!(rig.exec.workers().is_empty());
    let asked = rig.fake.calls().len();
    rig.pump().await;
    assert_eq!(rig.fake.calls().len(), asked, "a stopped executor asked");
    assert_eq!(rig.counter("kbf_executor_jobs_adopted_cancelled_total"), 0);
}

/// Catches a first step that gives up for good when the job service cannot be asked which
/// jobs are the frontend's: nothing is submitted and no worker registered meanwhile, the
/// question is asked again at the next step, and the pool is filled and the action submitted
/// once it is answered.
#[tokio::test(start_paused = true)]
async fn adoption_that_could_not_ask_is_asked_again() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.fake.inject_on(
        "ListJobs",
        Fault::Status {
            code: Code::Unavailable,
            message: "try later".to_owned(),
        },
    );
    let _caller = rig.submit(1);
    rig.exec.step().await;
    assert_eq!(rig.methods(), ["ListJobs"]);
    assert!(rig.exec.workers().is_empty());
    assert!(submits(&rig.fake.calls()).is_empty());
    rig.exec.step().await;
    // The second listing is answered; the worker the step then registers is placed on at
    // once, and its job submitted in the same step.
    assert_eq!(rig.methods()[..2], ["ListJobs", "ListJobs"]);
    assert_eq!(rig.exec.workers().len(), 1);
    rig.pump().await;
    assert_eq!(rig.jobs_of(1).len(), 1);
}

/// Catches a shutdown that leaves jobs behind: the job of a running action is asked to stop,
/// once, and counted, and the executor holds no worker afterwards.
#[tokio::test(start_paused = true)]
async fn shutdown_stops_the_runs_and_cancels_the_jobs() {
    let mut rig = Rig::start(&["node1"], 2, |_| {}).await;
    rig.pump().await;
    let _caller = rig.submit(1);
    rig.pump().await;
    let posted = rig.jobs_of(1).remove(0);
    rig.host_runs(posted.id(), "node1");
    rig.exec.shutdown().await;
    assert!(rig.exec.workers().is_empty());
    assert_eq!(cancels(&rig.fake.calls()), [posted.id().to_owned()]);
    assert!(job(&rig, posted.id()).cancel_requested);
    assert_eq!(rig.counter("kbf_executor_jobs_cancelled_total"), 1);
}

/// Catches a shutdown that leaves the farm placing actions on a worker that is gone: the farm
/// only learns it from its silence a grace later, and requeues what it placed there. The farm
/// is told the worker takes no more work, so an action submitted afterwards is not placed on
/// it.
#[tokio::test(start_paused = true)]
async fn shutdown_tells_the_farm_the_worker_takes_no_more_work() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    rig.pump().await;
    assert_eq!(placed(&rig, "js:front-1-0"), Some(0), "the worker is known");
    rig.exec.shutdown().await;
    let _caller = rig.submit(1);
    rig.farm.tick().await;
    crate::rig::settle().await;
    assert_eq!(
        placed(&rig, "js:front-1-0").unwrap_or(0),
        0,
        "an action was placed on a gone worker"
    );
}

/// Catches an adoption that takes an earlier process's jobs as cancelled when a cancel got no
/// answer, or that cancels again the ones that were answered: the workers wait until every
/// cancel is answered, the one that was not is sent again on the next pass, and each job is
/// counted once.
#[tokio::test(start_paused = true)]
async fn an_adoption_cancel_without_an_answer_is_sent_again_before_any_worker() {
    let mut rig = Rig::start(&["node1"], 1, |_| {}).await;
    let first = earlier(&rig, "front-1", "earlier");
    let second = earlier(&rig, "front-1", "earlier");
    rig.exec.step().await;
    rig.watch_over().await;
    rig.fake.inject_on(
        "CancelJob",
        Fault::Status {
            code: Code::Unavailable,
            message: "lost".to_owned(),
        },
    );
    rig.exec.step().await;
    assert_eq!(cancels(&rig.fake.calls()), [first.clone(), second.clone()]);
    assert!(rig.exec.workers().is_empty(), "a worker before the cancels");
    assert_eq!(rig.counter("kbf_executor_jobs_adopted_cancelled_total"), 1);
    rig.exec.step().await;
    assert_eq!(
        cancels(&rig.fake.calls()),
        [first.clone(), second, first.clone()]
    );
    assert_eq!(rig.counter("kbf_executor_jobs_adopted_cancelled_total"), 2);
    assert_eq!(job(&rig, &first).state(), JobState::Cancelled);
    assert_eq!(rig.exec.workers().len(), 1);
}
