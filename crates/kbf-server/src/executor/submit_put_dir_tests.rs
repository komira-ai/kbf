//! The job of a run that commits into a put directory.

use std::sync::Arc;
use std::time::Duration;

use js_proto::v1::{JobSpec, NetworkLevel};
use kbf_proto::reapi::Digest;
use kbf_types::LeaseId;

use super::{Booking, FRONTEND_LABEL, INSTANCE_LABEL, Settled, job_id, prepare};
use crate::executor::test_support::{GIB, Harness};
use crate::executor::traits::InputsError;
use crate::executor::{
    ActionFacts, Ctx, Deps, Inputs, OUT_TARGET, PutDirRoots, ReadGrant, TASK_IN_TREE, TaskOut,
};

const TREE: &str = "efefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefef";

fn action() -> Digest {
    Digest {
        hash: "cd".repeat(32),
        size_bytes: 3,
    }
}

/// Inputs whose run wrote the action tree [`TREE`], and that never issue a grant.
struct Treed;

#[tonic::async_trait]
impl Inputs for Treed {
    async fn action(&self, _: &Digest) -> Result<ActionFacts, InputsError> {
        Ok(ActionFacts {
            image: format!("registry.test/kbf@sha256:{}", "ab".repeat(32)),
            timeout: Some(Duration::from_secs(20)),
        })
    }

    async fn read_grant(
        &self,
        _: LeaseId,
        _: &Digest,
        _: Duration,
    ) -> Result<ReadGrant, InputsError> {
        Err(InputsError::Invalid(
            "a put-directory run asks for no grant".to_owned(),
        ))
    }

    async fn tree(&self, _: LeaseId) -> Result<String, InputsError> {
        Ok(TREE.to_owned())
    }
}

/// Prepares the job of lease 7.8 under the put root `/mnt/ts/put`, with `inputs` in place of
/// the harness's when given.
async fn prepared(
    inputs: Option<Arc<dyn Inputs>>,
    booking: Booking,
) -> Result<Arc<super::Prepared>, Settled> {
    let mut harness = Harness::new().await;
    let roots = PutDirRoots::new("/mnt/ts/put").expect("roots");
    harness.cfg.task_out = TaskOut::PutDir(roots);
    let deps = Deps {
        inputs: inputs.unwrap_or_else(|| Arc::clone(&harness.deps.inputs)),
        ..harness.deps.clone()
    };
    let ctx = Ctx {
        cfg: &harness.cfg,
        deps: &deps,
        twin: &harness.twin,
    };
    prepare(&ctx, LeaseId::new(7, 8), &action(), booking).await
}

const SHARE: Booking = Booking::Share {
    millicpus: 1_500,
    memory_bytes: GIB,
};

/// Asserts the job of a put-directory run: no image, the action tree as its one layer, the
/// base tree's task tool as its program with the put-directory flags, the run's directory at
/// `/out` read-write, no network, no file, no host port, the wall limit and the labels of any
/// job; and that the key it keeps for the ingest is the directory.
#[tokio::test(start_paused = true)]
async fn a_put_dir_job_is_the_tree_the_tool_and_the_dir_at_out() {
    let prepared = prepared(Some(Arc::new(Treed)), SHARE)
        .await
        .expect("prepared");
    let spec: &JobSpec = &prepared.spec;
    let roots = PutDirRoots::new("/mnt/ts/put").expect("roots");
    let dir = "cas/b/7-8/aa-0";
    assert_eq!(prepared.key.as_str(), dir);
    assert_eq!(spec.job_id, job_id("front", LeaseId::new(7, 8), 0));
    assert_eq!(spec.image, "");
    assert_eq!(spec.command, TASK_IN_TREE);
    assert_eq!(spec.layers, vec![roots.layer(TREE)]);
    assert_eq!(spec.mounts, vec![roots.mount(dir)]);
    assert_eq!(spec.mounts[0].target, OUT_TARGET);
    assert_eq!(spec.network(), NetworkLevel::None);
    assert!(spec.files.is_empty() && spec.ports.is_empty() && spec.host_ports.is_empty());
    assert!(spec.env.is_empty());
    let deadline = Duration::from_secs(20).as_millis().to_string();
    let want =
        crate::executor::task::put_dir_argv(&action(), LeaseId::new(7, 8), Duration::from_secs(20));
    assert_eq!(spec.args, want);
    assert!(spec.args.contains(&deadline));
    assert_eq!(spec.wall_limit_seconds, Some(20 + 300));
    assert_eq!(spec.labels[FRONTEND_LABEL], "front");
    assert!(spec.labels.contains_key(INSTANCE_LABEL));
    assert_eq!(spec.needs.as_ref().map(|n| n.whole_host), Some(false));
}

/// Asserts that a whole-machine lease is refused as the caller's before a key is minted: the
/// job service gives a whole-host job no mount.
#[tokio::test(start_paused = true)]
async fn a_whole_machine_lease_cannot_commit_into_a_put_dir() {
    let refused = prepared(Some(Arc::new(Treed)), Booking::WholeHost)
        .await
        .err()
        .expect("refused");
    assert!(
        matches!(&refused, Settled::Invalid(why) if why.contains("whole_machine")),
        "{refused:?}"
    );
}

/// Asserts that inputs that wrote no tree for the run (the default of the seam) settle the run
/// as infrastructure, and nothing is submitted.
#[tokio::test(start_paused = true)]
async fn a_run_with_no_tree_is_infrastructure() {
    let refused = prepared(None, SHARE).await.err().expect("refused");
    assert_eq!(
        refused,
        Settled::Infra("no input tree was written for 7.8".to_owned())
    );
}
