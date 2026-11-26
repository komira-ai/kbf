//! A job a host refused to start because it cannot mount its tree list through tierstore: the
//! frontend takes its miss path, and the runs it costs spend none of the operation's attempts, up
//! to the bound.

use js_proto::v1::JobState;
use kbf_front::Finished;
use kbf_proto::google::rpc;
use tonic::Code;

use crate::executor::InputTree;
use crate::executor::out::provider_layer;
use crate::executor::outcome::{LIST_REFUSALS, RESUBMITS};
use crate::executor::tests::{Rig, Setup};

const REFUSAL: &str = "tree list not supported: missing_sidecar";

fn status_of(finished: Finished) -> rpc::Status {
    match finished {
        Finished::Failed(status) => status,
        other => panic!("not a failure: {other:?}"),
    }
}

async fn provider_rig() -> Rig {
    Rig::with(|cfg| cfg.input_tree = InputTree::Provider).await
}

/// Asserts that refused starts spend no attempt of their own: an operation whose first run is
/// lost (an attempt spent), whose second and third are refused, and whose fourth runs, is
/// answered the action's result, though the bound of attempts is two and four jobs were made. The
/// frontend's miss path is asked once for each refusal, the form of the tree is asked for each
/// run (the configured provider form until the second refusal, manifests after), and the job's
/// layer follows the form: the host directory of the manifests for the last job, tierstore's mount
/// provider for the others.
#[tokio::test(start_paused = true)]
async fn refused_lists_spend_no_attempt_and_the_second_gives_the_job_its_manifests() {
    let rig = provider_rig().await;
    let caller = rig.submit(1);
    let first = rig.submitted(&caller, 0).await;
    rig.host_runs(&first);
    assert!(rig.fake.lose(&first));
    for attempt in 1..=2 {
        let job = rig.submitted(&caller, attempt).await;
        assert!(rig.fake.refuse_start(&job, REFUSAL));
    }
    let last = rig.submitted(&caller, 3).await;
    rig.host_finishes(&last, 0);

    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
    assert_eq!(rig.fake.jobs().len(), 4);
    assert_eq!(rig.ingest.list_refused.lock().unwrap().len(), 2);
    assert_eq!(
        *rig.ingest.minted_as.lock().unwrap(),
        [
            InputTree::Provider,
            InputTree::Provider,
            InputTree::Provider,
            InputTree::Manifests
        ]
    );
    let tree = "ef".repeat(32);
    let roots = rig.cfg.task_out.as_ref().expect("roots");
    let layers = |attempt: u32| {
        rig.fake
            .submitted(&rig.job_of(&caller, attempt))
            .expect("submitted")
            .layers
    };
    assert_eq!(layers(2), vec![provider_layer(&tree)]);
    assert_eq!(layers(3), vec![roots.layer(&tree)]);
    assert_eq!(rig.counted("kbf_executor_attempts_resubmitted_total "), 3);
    assert_eq!(
        rig.counted("kbf_executor_runs_total{outcome=\"list_refused\"} "),
        2
    );
    assert_eq!(rig.counted("kbf_executor_runs_total{outcome=\"lost\"} "), 1);
    assert_eq!(rig.counted("kbf_executor_runs_total{outcome=\"ran\"} "), 1);
}

/// Asserts the bound: after [`LIST_REFUSALS`] free runs a refusal is an ordinary infrastructure
/// ending, run again while the operation has attempts left ([`RESUBMITS`]) and then answered
/// UNAVAILABLE with the step the frontend took. Every refusal is told to the frontend, whose
/// miss path stays at manifests.
#[tokio::test(start_paused = true)]
async fn a_refusal_after_the_free_ones_is_an_ordinary_retry_then_unavailable() {
    let rig = provider_rig().await;
    let caller = rig.submit(1);
    let total = LIST_REFUSALS + RESUBMITS + 1;
    for attempt in 0..total {
        let job = rig.submitted(&caller, attempt).await;
        assert!(rig.fake.refuse_start(&job, REFUSAL));
    }
    let status = status_of(rig.until_done(&caller).await);

    assert_eq!(status.code, Code::Unavailable as i32);
    assert_eq!(
        status.message,
        "the host refused the job's tree list as not mountable (the tree is given as manifests)"
    );
    assert_eq!(
        rig.fake.jobs().len(),
        total as usize,
        "no job past the bounds"
    );
    assert_eq!(
        rig.ingest.list_refused.lock().unwrap().len(),
        total as usize
    );
    assert_eq!(
        rig.counted("kbf_executor_runs_total{outcome=\"list_refused\"} "),
        u64::from(total)
    );
    assert_eq!(
        rig.counted("kbf_executor_attempts_resubmitted_total "),
        u64::from(total - 1)
    );
    assert!(
        rig.fake
            .jobs()
            .iter()
            .all(|job| job.state() == JobState::FailedToStart)
    );
}

/// Asserts what an open operation's row names for the collector of tree lists: the key of the
/// tree list each attempt's job reads (tierstore's provider ref), from the row written before
/// that attempt's submit, and no list once an attempt's tree is given as manifests; and no list
/// in a frontend of the manifest form. Catches: a provider attempt's row that names no list, so
/// the collector would take the list of a queued or running job for one no job names.
#[tokio::test(start_paused = true)]
async fn an_open_operations_row_names_its_attempts_tree_list_and_none_for_manifests() {
    let stored = Setup {
        store: true,
        record: false,
    };
    let rig = Rig::build(stored, |cfg| cfg.input_tree = InputTree::Provider).await;
    // The key of a tree list, as the frontend names one (`cas/l/<id>`) and the collector reads it.
    let tree = format!("cas/l/{}", "ef".repeat(32));
    *rig.inputs.tree.lock().expect("tree") = Ok(tree.clone());
    let caller = rig.submit(1);
    let named = |rig: &Rig| {
        let row = rig.rows("reader").op(caller.operation()).expect("a read");
        row.expect("open").tree
    };
    let first = rig.submitted(&caller, 0).await;
    let layers = rig.fake.submitted(&first).expect("submitted").layers;
    assert_eq!(
        layers,
        vec![provider_layer(&tree)],
        "the row names the job's ref"
    );
    assert_eq!(named(&rig), Some(tree.clone()));
    assert_eq!(
        rig.rows("reader").named_lists(),
        Ok(std::collections::BTreeSet::from([tree.clone()]))
    );
    assert!(rig.fake.refuse_start(&first, REFUSAL));
    let second = rig.submitted(&caller, 1).await;
    assert_eq!(named(&rig), Some(tree.clone()), "the provider form again");
    assert!(rig.fake.refuse_start(&second, REFUSAL));
    let third = rig.submitted(&caller, 2).await;
    assert_eq!(named(&rig), None, "as manifests after the second refusal");
    rig.host_finishes(&third, 0);
    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
    assert_eq!(rig.rows("reader").named_lists(), Ok(Default::default()));

    let manifests = Rig::stored().await;
    let caller = manifests.submit(2);
    manifests.submitted(&caller, 0).await;
    let row = manifests
        .rows("reader")
        .op(caller.operation())
        .expect("a read");
    assert_eq!(row.expect("open").tree, None);
}

/// Catches a start whose tree list the collector of tree lists deleted (an old list taken up
/// after the collector read the rows) answered as a failure, or run again with the same missing
/// list from the memo: tierstore's `no tree list at <key>` takes the miss path, whose first step
/// writes the tree list again, spends no attempt, and the next job runs and answers the caller.
#[tokio::test(start_paused = true)]
async fn a_start_whose_list_was_deleted_writes_it_again_and_spends_no_attempt() {
    let rig = provider_rig().await;
    let caller = rig.submit(1);
    let tree = "ef".repeat(32);
    let first = rig.submitted(&caller, 0).await;
    assert!(
        rig.fake
            .refuse_start(&first, &format!("no tree list at {tree}"))
    );
    let second = rig.submitted(&caller, 1).await;
    rig.host_finishes(&second, 0);

    assert!(matches!(rig.until_done(&caller).await, Finished::Ran(_)));
    assert_eq!(
        rig.ingest.list_refused.lock().unwrap().len(),
        1,
        "the miss path's first step"
    );
    assert_eq!(
        *rig.ingest.minted_as.lock().unwrap(),
        [InputTree::Provider, InputTree::Provider],
        "written again as a list"
    );
    assert_eq!(
        rig.counted("kbf_executor_runs_total{outcome=\"list_refused\"} "),
        1
    );
    assert_eq!(rig.counted("kbf_executor_attempts_resubmitted_total "), 1);
}
