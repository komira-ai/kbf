//! The job of a run that commits into a put directory.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use js_proto::v1::{JobSpec, NetworkLevel};
use kbf_proto::reapi::Digest;

use super::tests::{Harness, RUN, built_class};
use super::{Booking, FRONTEND_LABEL, INSTANCE_LABEL, Preparer, Settled, job_id, prepare};
use crate::executor::metrics::Metrics;
use crate::executor::out::provider_layer;
use crate::executor::traits::InputsError;
use crate::executor::{
    ActionFacts, DEFAULT_MAC_OS_VERSION, Ingest, Ingested, InputTree, Inputs, MacPool, MintError,
    OUT_TARGET, PackKey, PutDirRoots, RunId, System, TASK_IN_TREE, UntrustedLimits, build_id,
};

const TREE: &str = "efefefefefefefefefefefefefefefefefefefefefefefefefefefefefefefef";

/// The key of a tree list, as [`Inputs::tree`] hands it out with [`InputTree::Provider`].
const LIST_KEY: &str = "cas/l/abababababababababababababababababababababababababababababababab";

/// The macOS `kbf-task` bundle of a release, by its manifest digest.
const BUNDLE: &str =
    "kbf-task@sha256:9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a";

fn action() -> Digest {
    Digest {
        hash: "cd".repeat(32),
        size_bytes: 3,
    }
}

/// Inputs whose run wrote the action tree `tree`; a Mac action when `mac`.
struct Treed {
    tree: &'static str,
    mac: bool,
}

impl Treed {
    /// A Linux action whose tree is [`TREE`].
    fn linux() -> Self {
        Self {
            tree: TREE,
            mac: false,
        }
    }
}

/// An ingest that mints the directory of a run and records the form of tree each mint was
/// asked to write.
#[derive(Default)]
struct Recording {
    asked: Mutex<Vec<InputTree>>,
}

impl Recording {
    fn asked(&self) -> Vec<InputTree> {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

#[tonic::async_trait]
impl Ingest for Recording {
    async fn mint(
        &self,
        run: RunId,
        _: &Digest,
        _: Duration,
        tree: InputTree,
    ) -> Result<PackKey, MintError> {
        self.asked
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(tree);
        Ok(PackKey::new(format!("cas/b/{}/aa-0", build_id(run))))
    }

    async fn ingest(&self, _: RunId, _: &PackKey, _: &Digest, _: &UntrustedLimits) -> Ingested {
        Ingested::Unavailable("not read here".to_owned())
    }
}

#[tonic::async_trait]
impl Inputs for Treed {
    async fn action(&self, _: &Digest) -> Result<ActionFacts, InputsError> {
        Ok(ActionFacts {
            image: format!("registry.test/kbf@sha256:{}", "ab".repeat(32)),
            timeout: Some(Duration::from_secs(20)),
            system: match self.mac {
                true => System::Macos { os_version: None },
                false => System::Linux,
            },
            size_class: built_class(),
        })
    }

    async fn tree(&self, _: RunId) -> Result<String, InputsError> {
        Ok(self.tree.to_owned())
    }
}

/// What a test sets: the form of tree the config asks for, whether the outputs are a put
/// directory, whether the executor has Macs, and the inputs in place of the harness's.
struct Case {
    tree: InputTree,
    put_dir: bool,
    macs: bool,
    inputs: Option<Arc<dyn Inputs>>,
    booking: Booking,
}

impl Case {
    /// A Linux put-directory run with the manifests, as `inputs` has it.
    fn new(inputs: Option<Arc<dyn Inputs>>, booking: Booking) -> Self {
        Self {
            tree: InputTree::Manifests,
            put_dir: true,
            macs: false,
            inputs,
            booking,
        }
    }
}

/// Prepares the job of [`RUN`] for `case` under the put root `/mnt/ts/put`, with an ingest
/// that records what its mint was asked for, which is returned with the job.
async fn prepared_as(case: Case) -> (Result<super::Prepared, Settled>, Vec<InputTree>) {
    let mut harness = Harness::new();
    let roots = PutDirRoots::new("/mnt/ts/put").expect("roots");
    harness.cfg.task_out = case.put_dir.then_some(roots);
    harness.cfg.input_tree = case.tree;
    if case.macs {
        harness.cfg.mac = Some(MacPool {
            bundle: BUNDLE.to_owned(),
            os_version: DEFAULT_MAC_OS_VERSION,
            roots: None,
        });
    }
    let ingest = Arc::new(Recording::default());
    let inputs = case.inputs.unwrap_or_else(|| Arc::clone(&harness.inputs));
    let ingest_dyn: Arc<dyn Ingest> = Arc::clone(&ingest) as Arc<dyn Ingest>;
    let ctx = Preparer {
        metrics: &Metrics::default(),
        inputs: &inputs,
        ingest: &ingest_dyn,
        ..harness.preparer()
    };
    let job = prepare(&ctx, RUN, &action(), case.booking).await;
    (job, ingest.asked())
}

/// Prepares the job of [`RUN`] under the put root `/mnt/ts/put`, with `inputs` in place of
/// the harness's when given.
async fn prepared(
    inputs: Option<Arc<dyn Inputs>>,
    booking: Booking,
) -> Result<super::Prepared, Settled> {
    prepared_as(Case::new(inputs, booking)).await.0
}

const SHARE: Booking = Booking::Share;

/// Asserts the job of a put-directory run: no image, the action tree as its one layer, the
/// base tree's task tool as its program with the put-directory flags, the run's directory at
/// `/out` read-write, no network, no file, no host port, the wall limit and the labels of any
/// job, the size class included, and no CPU, memory or disk named (the service sizes it from
/// the class); and that the key it keeps for the ingest is the directory.
#[tokio::test(start_paused = true)]
async fn a_put_dir_job_is_the_tree_the_tool_and_the_dir_at_out() {
    let prepared = prepared(Some(Arc::new(Treed::linux())), SHARE)
        .await
        .expect("prepared");
    let spec: &JobSpec = &prepared.spec;
    let roots = PutDirRoots::new("/mnt/ts/put").expect("roots");
    let dir = "cas/b/7-8/aa-0";
    assert_eq!(prepared.key.as_str(), dir);
    assert_eq!(spec.job_id, job_id("front", RUN));
    assert_eq!(spec.image, "");
    assert_eq!(spec.command, TASK_IN_TREE);
    assert_eq!(spec.layers, vec![roots.layer(TREE)]);
    assert_eq!(spec.mounts, vec![roots.mount(dir)]);
    assert_eq!(spec.mounts[0].target, OUT_TARGET);
    assert_eq!(spec.network(), NetworkLevel::None);
    assert!(spec.files.is_empty() && spec.ports.is_empty() && spec.host_ports.is_empty());
    assert!(spec.env.is_empty());
    let deadline = Duration::from_secs(20).as_millis().to_string();
    let want = crate::executor::task::put_dir_argv(&action(), RUN, Duration::from_secs(20));
    assert_eq!(spec.args, want);
    assert_eq!(
        spec.args[spec.args.len() - 2..],
        ["--object-format", "one-pass"]
    );
    assert!(spec.args.contains(&deadline));
    assert_eq!(spec.wall_limit_seconds, Some(20 + 300));
    assert_eq!(spec.labels[FRONTEND_LABEL], "front");
    assert!(spec.labels.contains_key(INSTANCE_LABEL));
    assert_eq!(spec.labels["size-class"], "root.pkg.t.build");
    assert_eq!(spec.labels["size-fallback"], "cc.build.kind");
    assert_eq!(spec.needs, Some(js_proto::v1::Needs::default()));
}

/// Asserts that a whole-machine action is refused as the caller's before a key is minted: the
/// job service gives a whole-host job no mount.
#[tokio::test(start_paused = true)]
async fn a_whole_machine_action_cannot_commit_into_a_put_dir() {
    let refused = prepared(Some(Arc::new(Treed::linux())), Booking::WholeHost)
        .await
        .err()
        .expect("refused");
    assert!(
        matches!(&refused, Settled::Invalid(why) if why.contains("whole_machine")),
        "{refused:?}"
    );
}

/// Inputs of a Linux action that wrote no tree for the run: the default of the seam.
struct NoTree;

#[tonic::async_trait]
impl Inputs for NoTree {
    async fn action(&self, action: &Digest) -> Result<ActionFacts, InputsError> {
        Treed::linux().action(action).await
    }
}

/// Asserts that inputs that wrote no tree for the run (the default of the seam) settle the run
/// as infrastructure, and nothing is submitted.
#[tokio::test(start_paused = true)]
async fn a_run_with_no_tree_is_infrastructure() {
    let refused = prepared(Some(Arc::new(NoTree)), SHARE)
        .await
        .err()
        .expect("refused");
    assert_eq!(
        refused,
        Settled::Infra("no input tree was written for 7-8".to_owned())
    );
}

/// Asserts that with `--executor-input-tree provider` the job's one layer is tierstore's
/// provider layer for the key `Inputs::tree` returned, that the ingest's mint was asked for
/// `InputTree::Provider` once, and that everything else of the job (program, arguments, mount,
/// network, limits, labels) is the manifests job's. Catches: a provider job that still mounts
/// the manifests' host directory, a mint never told to write a tree list, and a provider job
/// that differs from the manifests job in more than its layer.
#[tokio::test(start_paused = true)]
async fn a_provider_job_has_the_tree_lists_provider_as_its_one_layer_and_nothing_else_differs() {
    let (listed, asked) = prepared_as(Case {
        tree: InputTree::Provider,
        inputs: Some(Arc::new(Treed {
            tree: LIST_KEY,
            mac: false,
        })),
        ..Case::new(None, SHARE)
    })
    .await;
    let listed = listed.expect("prepared");
    assert_eq!(asked, [InputTree::Provider]);
    assert_eq!(listed.spec.layers, vec![provider_layer(LIST_KEY)]);
    assert_eq!(listed.key.as_str(), "cas/b/7-8/aa-0");

    let (manifests, asked) = prepared_as(Case::new(Some(Arc::new(Treed::linux())), SHARE)).await;
    let manifests = manifests.expect("prepared");
    assert_eq!(asked, [InputTree::Manifests]);
    let roots = PutDirRoots::new("/mnt/ts/put").expect("roots");
    assert_eq!(manifests.spec.layers, vec![roots.layer(TREE)]);
    assert_eq!(listed.key.as_str(), manifests.key.as_str());
    // The layers are compared above; the instance label is of each harness's own config.
    let bare = |job: &super::Prepared| {
        let mut spec: JobSpec = job.spec.clone();
        spec.layers.clear();
        spec.labels.remove(INSTANCE_LABEL);
        spec
    };
    assert_eq!(bare(&listed), bare(&manifests));
}

/// Asserts that an executor given no roots refuses a Linux run as the caller's, naming
/// `--task-out-root`, before a key is minted: no put directory has anywhere to be mounted from.
/// Catches: a run minted and submitted with no host directory to commit into.
#[tokio::test(start_paused = true)]
async fn a_linux_run_with_no_task_out_root_is_refused_naming_the_flag() {
    let (job, asked) = prepared_as(Case {
        put_dir: false,
        ..Case::new(Some(Arc::new(Treed::linux())), SHARE)
    })
    .await;
    assert_eq!(
        job.err(),
        Some(Settled::Invalid(
            "a Linux action commits into a put directory: give --task-out-root".to_owned()
        ))
    );
    assert!(asked.is_empty(), "{asked:?}");
}

/// Asserts that a Mac job asks the mint for `InputTree::Manifests` when the config says
/// provider, and mounts the tree as the host directory of its manifests: the native runtime
/// takes no provider. Catches: a Mac job given a provider layer its runtime cannot read.
#[tokio::test(start_paused = true)]
async fn a_mac_job_asks_for_manifests_even_when_the_config_says_provider() {
    let (job, asked) = prepared_as(Case {
        tree: InputTree::Provider,
        macs: true,
        inputs: Some(Arc::new(Treed {
            tree: TREE,
            mac: true,
        })),
        ..Case::new(None, SHARE)
    })
    .await;
    let job = job.expect("prepared");
    assert_eq!(asked, [InputTree::Manifests]);
    assert!(job.spec.layers.is_empty());
    let trees: Vec<&str> = job
        .spec
        .mounts
        .iter()
        .filter_map(|m| match &m.source {
            Some(js_proto::v1::mount::Source::HostPath(path)) => Some(path.as_str()),
            _ => None,
        })
        .filter(|path| path.contains("/trees/"))
        .collect();
    assert_eq!(trees, [format!("/mnt/ts/trees/{TREE}")]);
}

/// Asserts that a Linux put-directory job on the manifests asks the mint for
/// `InputTree::Manifests`. Catches: a default that asks for the tree list.
#[tokio::test(start_paused = true)]
async fn a_linux_put_dir_job_asks_for_manifests_by_default() {
    let (job, asked) = prepared_as(Case::new(Some(Arc::new(Treed::linux())), SHARE)).await;
    job.expect("prepared");
    assert_eq!(asked, [InputTree::Manifests]);
}
