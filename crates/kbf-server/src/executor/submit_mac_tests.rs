//! The job of a run of an action that asks for a Mac.

use std::sync::Arc;
use std::time::Duration;

use js_proto::v1::{
    Constraints, ExecMode, JobSpec, MountMode, MountPropagation, NetworkLevel, mount,
};
use kbf_front::DecodedAction;
use kbf_proto::reapi::{Digest, Platform, platform::Property};

use super::tests::{Harness, RUN, built_class};
use super::{Booking, FRONTEND_LABEL, INSTANCE_LABEL, Preparer, Settled, job_id, prepare};
use crate::executor::mac::mac_argv;
use crate::executor::metrics::Metrics;
use crate::executor::traits::InputsError;
use crate::executor::{
    ActionFacts, DEFAULT_MAC_OS_VERSION, Inputs, MAC_OUT_TARGET, MacPool, PutDirRoots, RunId,
    System, TREE_TARGET,
};

/// The key of the run's tree list, as [`Inputs::tree`] hands it out for a Mac run.
const TREE: &str = "cas/l/efefefefefefefefefefefefefefefefefefefefefefefefefefefefefefef";

fn action() -> Digest {
    Digest {
        hash: "cd".repeat(32),
        size_bytes: 3,
    }
}

/// The platform a komira darwin action sends: its OS family and ISA, and nothing else (no
/// `xcode`, no `os_version`).
const KOMIRA_DARWIN: &[(&str, &str)] = &[("OSFamily", "Darwin"), ("ISA", "arm-a64")];

/// Inputs of an action of `platform`, read as the frontend reads it ([`System::of`]), whose run
/// wrote the tree [`TREE`] (none when `tree` is false).
struct Mac {
    platform: Vec<(&'static str, &'static str)>,
    tree: bool,
}

#[tonic::async_trait]
impl Inputs for Mac {
    async fn action(
        &self,
        _: &Digest,
        _decoded: Option<&DecodedAction>,
    ) -> Result<ActionFacts, InputsError> {
        Ok(ActionFacts {
            image: format!("registry.test/kbf@sha256:{}", "ab".repeat(32)),
            timeout: Some(Duration::from_secs(20)),
            system: System::of(Some(&platform(&self.platform))).map_err(InputsError::Invalid)?,
            size_class: built_class(),
        })
    }

    async fn tree(&self, run: RunId) -> Result<String, InputsError> {
        match self.tree {
            true => Ok(TREE.to_owned()),
            false => Err(InputsError::Unavailable(format!(
                "no input tree was written for {run}"
            ))),
        }
    }
}

fn pool(roots: Option<&str>) -> MacPool {
    MacPool {
        bundle: BUNDLE.to_owned(),
        os_version: DEFAULT_MAC_OS_VERSION,
        roots: roots.map(|root| PutDirRoots::new(root).expect("roots")),
    }
}

/// The platform of `pairs`.
fn platform(pairs: &[(&str, &str)]) -> Platform {
    let property = |(name, value): &(&str, &str)| Property {
        name: (*name).to_owned(),
        value: (*value).to_owned(),
    };
    Platform {
        properties: pairs.iter().map(property).collect(),
    }
}

/// What a test sets: the pool, the outputs' mode, the action's platform, the booking.
struct Case {
    mac: Option<MacPool>,
    put_dir: bool,
    platform: Vec<(&'static str, &'static str)>,
    tree: bool,
    booking: Booking,
}

impl Default for Case {
    fn default() -> Self {
        Self {
            mac: Some(pool(None)),
            put_dir: true,
            platform: KOMIRA_DARWIN.to_vec(),
            tree: true,
            booking: Booking::Share,
        }
    }
}

/// [`KOMIRA_DARWIN`] with `more` after it.
fn darwin_and(more: &[(&'static str, &'static str)]) -> Vec<(&'static str, &'static str)> {
    KOMIRA_DARWIN.iter().chain(more).copied().collect()
}

/// Prepares the job of [`RUN`] for `case`, under the Linux put root `/mnt/ts/put`.
async fn prepared(case: Case) -> Result<super::Prepared, Settled> {
    let mut harness = Harness::new();
    harness.cfg.task_out = case
        .put_dir
        .then(|| PutDirRoots::new("/mnt/ts/put").expect("roots"));
    harness.cfg.mac = case.mac;
    let inputs: Arc<dyn Inputs> = Arc::new(Mac {
        platform: case.platform,
        tree: case.tree,
    });
    let ctx = Preparer {
        metrics: &Metrics::default(),
        inputs: &inputs,
        ..harness.preparer()
    };
    prepare(
        &ctx,
        RUN,
        (&action(), None, &kbf_front::ActionLabel::default()),
        case.booking,
    )
    .await
}

/// Where a mount comes from: a host path, or `<provider>:<ref>` for a provider's tree.
fn source(mount: &js_proto::v1::Mount) -> String {
    match &mount.source {
        Some(mount::Source::HostPath(path)) => format!("host:{path}"),
        Some(mount::Source::Provider(p)) => format!("{}:{}", p.provider, p.r#ref),
        None => panic!("no source"),
    }
}

/// Asserts the job of a komira darwin action (`OSFamily=Darwin`, `ISA=arm-a64`, nothing else):
/// the release's task bundle as its image, its entrypoint run (no command), no layer, the Mac
/// flags, exactly two mounts (the tree read-only at `/work/.kbf-tree` from tierstore's provider
/// given the tree list's key, and the put target `HostPath("<put root>/cas/b/<build>/<nonce>")`
/// read-write at `/work/.kbf-out`, both `no_exec` and private), constraints that are exactly
/// `os=macos` and `arch=arm64` with every other field empty or unspecified (no `exec`, no
/// `xcode`), the share booked (no CPU, memory or disk named: the service sizes the job from its
/// class), no network, no file, the wall limit and the labels of any job, the size class
/// included; and that the key kept for the ingest is the directory. Catches a komira Mac action
/// refused for naming no Xcode, a job that still asks for `exec=sandbox-exec`, and a change of
/// the mounts the Mac's agent reads.
#[tokio::test(start_paused = true)]
async fn a_komira_darwin_action_is_a_job_of_os_and_arch_only_with_the_tree_and_the_put_target() {
    let prepared = prepared(Case::default()).await.expect("prepared");
    let spec: &JobSpec = &prepared.spec;
    let dir = "cas/b/7-8/aa-0";
    assert_eq!(prepared.key.as_str(), dir);
    assert_eq!(spec.job_id, job_id("front", RUN));
    assert_eq!(spec.image, BUNDLE);
    assert!(spec.layers.is_empty());
    assert_eq!(spec.command, "");
    assert_eq!(spec.args, mac_argv(&action(), RUN, Duration::from_secs(20)));
    let mounts: Vec<(String, &str, MountMode, bool, MountPropagation, &str)> = spec
        .mounts
        .iter()
        .map(|m| {
            let sub = m.sub_path.as_str();
            (
                source(m),
                m.target.as_str(),
                m.mode(),
                m.no_exec,
                m.propagation(),
                sub,
            )
        })
        .collect();
    assert_eq!(
        mounts,
        [
            (
                format!("tierstore:{TREE}"),
                TREE_TARGET,
                MountMode::ReadOnly,
                true,
                MountPropagation::Private,
                ""
            ),
            (
                format!("host:/mnt/ts/put/{dir}"),
                MAC_OUT_TARGET,
                MountMode::ReadWrite,
                true,
                MountPropagation::Private,
                ""
            ),
        ]
    );
    assert_eq!(
        (TREE_TARGET, MAC_OUT_TARGET),
        ("/work/.kbf-tree", "/work/.kbf-out")
    );
    assert_eq!(
        spec.constraints,
        Some(Constraints {
            os: "macos".to_owned(),
            arch: "arm64".to_owned(),
            ..Constraints::default()
        })
    );
    let constraints = spec.constraints.as_ref().expect("constraints");
    assert_eq!(constraints.exec(), ExecMode::Unspecified);
    assert_eq!(constraints.xcode, "");
    assert_eq!(spec.network(), NetworkLevel::None);
    assert!(spec.files.is_empty() && spec.ports.is_empty() && spec.host_ports.is_empty());
    assert!(spec.env.is_empty());
    assert_eq!(spec.wall_limit_seconds, Some(20 + 300));
    assert_eq!(spec.labels[FRONTEND_LABEL], "front");
    assert!(spec.labels.contains_key(INSTANCE_LABEL));
    assert_eq!(spec.labels["size-class"], "root.pkg.t.cc.build");
    assert_eq!(spec.labels["size-fallback"], "cc.build.kind");
    assert_eq!(spec.history_key, "root//pkg:t cc");
    assert_eq!(
        spec.needs,
        Some(js_proto::v1::Needs::default()),
        "no CPU, memory or disk: the service sizes the job from its class"
    );
}

/// Asserts that a Mac job whose Execute's client sent a target and an action kind is sent them
/// as its history key, `<target_id> <action_mnemonic>`, in place of the key its paths give.
/// Catches a Mac job that drops what the client sent, which the Linux job keeps.
#[tokio::test(start_paused = true)]
async fn a_mac_jobs_history_key_is_the_target_and_kind_its_client_sent() {
    let mut harness = Harness::new();
    harness.cfg.task_out = Some(PutDirRoots::new("/mnt/ts/put").expect("roots"));
    harness.cfg.mac = Some(pool(None));
    let inputs: Arc<dyn Inputs> = Arc::new(Mac {
        platform: KOMIRA_DARWIN.to_vec(),
        tree: true,
    });
    let ctx = Preparer {
        metrics: &Metrics::default(),
        inputs: &inputs,
        ..harness.preparer()
    };
    let sent = kbf_front::ActionLabel {
        mnemonic: Some("SwiftCompile".to_owned()),
        target: Some("//ios/App:app".to_owned()),
    };
    let job = prepare(&ctx, RUN, (&action(), None, &sent), Booking::Share)
        .await
        .expect("a Mac job");
    assert_eq!(job.spec.history_key, "//ios/App:app SwiftCompile");
}

/// Asserts the wall limit of a Mac job is the action's timeout plus the run margin, from the
/// configured values: a margin and a timeout other than the defaults give their sum, and an
/// action that names no timeout takes the configured default (3000 s and 300 s by default, so
/// 3300 s). Catches a Mac job whose wall limit leaves out the margin, or is a fixed number.
#[tokio::test(start_paused = true)]
async fn a_mac_jobs_wall_limit_is_its_timeout_plus_the_run_margin() {
    let mut harness = Harness::new();
    harness.cfg.task_out = Some(PutDirRoots::new("/mnt/ts/put").expect("roots"));
    harness.cfg.mac = Some(pool(None));
    harness.cfg.run_margin = Duration::from_secs(77);
    let inputs: Arc<dyn Inputs> = Arc::new(Mac {
        platform: KOMIRA_DARWIN.to_vec(),
        tree: true,
    });
    let ctx = Preparer {
        metrics: &Metrics::default(),
        inputs: &inputs,
        ..harness.preparer()
    };
    let job = prepare(
        &ctx,
        RUN,
        (&action(), None, &kbf_front::ActionLabel::default()),
        Booking::Share,
    )
    .await
    .expect("prepared");
    assert_eq!(job.spec.wall_limit_seconds, Some(20 + 77));
    let defaults = crate::executor::Config::new("front");
    assert_eq!(
        defaults.wall(defaults.default_timeout),
        Duration::from_secs(3300)
    );
}

/// Asserts that a pool that names its own roots mounts the put directory under them, not under
/// `--task-out-root`'s, and the tree from the provider all the same.
#[tokio::test(start_paused = true)]
async fn a_pool_with_its_own_roots_mounts_under_them() {
    let prepared = prepared(Case {
        mac: Some(pool(Some("/Volumes/ts/put"))),
        ..Case::default()
    })
    .await
    .expect("prepared");
    let paths: Vec<String> = prepared.spec.mounts.iter().map(source).collect();
    assert_eq!(
        paths,
        [
            format!("tierstore:{TREE}"),
            "host:/Volumes/ts/put/cas/b/7-8/aa-0".to_owned()
        ]
    );
}

/// Asserts each refusal of a Mac run as the caller's, before anything is submitted: an
/// executor with no Macs, an action that asks for a newer macOS than they run, a
/// `whole_machine` action, and an executor with no roots for the put directory. Catches a
/// refusal that the removal of the Xcode rule took with it.
#[tokio::test(start_paused = true)]
async fn a_mac_run_the_executor_cannot_make_is_the_callers() {
    let cases = [
        (
            Case {
                mac: None,
                ..Case::default()
            },
            "the action asks for a Mac, and this executor has none (--executor-mac-task-bundle)",
        ),
        (
            Case {
                platform: darwin_and(&[("os_version", "27")]),
                ..Case::default()
            },
            "the action asks for macOS 27 or newer (os_version), and this executor's Macs are \
             sure to run only macOS 26 or newer (--executor-mac-os-version)",
        ),
        (
            Case {
                booking: Booking::WholeHost,
                ..Case::default()
            },
            "a whole_machine action cannot run on a Mac of the job service: it commits into a \
             put directory, and a whole-host job has no mount",
        ),
        (
            Case {
                put_dir: false,
                ..Case::default()
            },
            "a Mac action commits into a put directory: give --task-out-root",
        ),
    ];
    for (case, why) in cases {
        let refused = prepared(case).await.err().expect("refused");
        assert_eq!(refused, Settled::Invalid(why.to_owned()));
    }
}

/// Asserts that a Mac action that names an `xcode` is a Mac job as one that names none is: for a
/// build id, a value that is no build id, and beside an `os_version` the pool runs, the job is
/// the same (constraints exactly `os=macos`, `arch=arm64`, no Xcode) as the job of the platform
/// with no `xcode`. Catches a leftover read of the property: a refusal for its value, or the
/// value sent to the job service.
#[tokio::test(start_paused = true)]
async fn a_mac_actions_xcode_is_neither_required_nor_sent() {
    // The instance label is each harness's own process id: not the action's.
    let unlabelled = |mut spec: JobSpec| {
        spec.labels.remove(INSTANCE_LABEL);
        spec
    };
    let plain = unlabelled(prepared(Case::default()).await.expect("prepared").spec);
    for more in [
        &[("xcode", "16C5032a")][..],
        &[("xcode", "not a build id")],
        &[("xcode", "-16C5032a/..")],
        &[("os_version", "26"), ("xcode", "17A324")],
    ] {
        let job = prepared(Case {
            platform: darwin_and(more),
            ..Case::default()
        })
        .await
        .unwrap_or_else(|refused| panic!("{more:?}: {refused:?}"));
        let constraints = job.spec.constraints.as_ref().expect("constraints");
        assert_eq!(constraints.xcode, "", "{more:?}");
        assert_eq!(unlabelled(job.spec), plain, "{more:?}");
    }
}

/// Asserts that an action whose platform asks for no Mac is a Linux container job, with the
/// action tree as its layer, no Mac bundle and no Xcode, even when the executor has Macs.
/// Catches a pool of Macs that takes over the Linux actions.
#[tokio::test(start_paused = true)]
async fn a_linux_action_stays_a_linux_job_when_the_executor_has_macs() {
    let job = prepared(Case {
        platform: vec![("OSFamily", "Linux")],
        ..Case::default()
    })
    .await
    .expect("prepared");
    let constraints = job.spec.constraints.as_ref().expect("constraints");
    assert_eq!(constraints.os, "linux");
    assert_eq!(constraints.exec(), ExecMode::Container);
    assert_eq!(constraints.xcode, "");
    assert_eq!(job.spec.image, "");
    assert_eq!(job.spec.layers.len(), 1);
}

/// Asserts that a Mac run whose tree was not written is infrastructure, as a Linux one is.
#[tokio::test(start_paused = true)]
async fn a_mac_run_with_no_tree_is_infrastructure() {
    let refused = prepared(Case {
        tree: false,
        ..Case::default()
    })
    .await
    .err()
    .expect("refused");
    assert_eq!(
        refused,
        Settled::Infra("no input tree was written for 7-8".to_owned())
    );
}

/// The macOS `kbf-task` bundle of a release, by its manifest digest.
const BUNDLE: &str =
    "kbf-task@sha256:9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a9a";

/// A mint that answers at once and keeps the decode each mint was given.
#[derive(Default)]
struct Minting(std::sync::Mutex<Vec<Option<DecodedAction>>>);

#[tonic::async_trait]
impl crate::executor::Ingest for Minting {
    async fn mint(
        &self,
        run: RunId,
        _: &Digest,
        _: Duration,
        _: crate::executor::InputTree,
        decoded: Option<&DecodedAction>,
    ) -> Result<crate::executor::PackKey, crate::executor::MintError> {
        let mut minted = self
            .0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        minted.push(decoded.cloned());
        let dir = format!("cas/b/{}/aa-0", crate::executor::build_id(run));
        Ok(crate::executor::PackKey::new(dir))
    }

    async fn ingest(
        &self,
        _: RunId,
        _: &crate::executor::PackKey,
        _: &Digest,
        _: &crate::executor::UntrustedLimits,
    ) -> crate::executor::Ingested {
        crate::executor::Ingested::Unavailable("not read here".to_owned())
    }
}

/// Catches a Mac run that drops the Execute's decode, so that its closure would read the Action
/// from the CAS again, or labels its steps with a form other than its tree list's: the mint of a
/// Mac run is given the decode the run was, and the job is prepared as a tree list.
#[tokio::test]
async fn a_mac_run_hands_its_decode_to_the_mint_and_is_a_tree_list() {
    let harness = Harness::new();
    let mut cfg = harness.cfg.clone();
    cfg.mac = Some(pool(None));
    let inputs: Arc<dyn Inputs> = Arc::new(Mac {
        platform: KOMIRA_DARWIN.to_vec(),
        tree: true,
    });
    let minting = Arc::new(Minting::default());
    let ingest: Arc<dyn crate::executor::Ingest> = minting.clone();
    let metrics = Metrics::default();
    let ctx = Preparer {
        cfg: &cfg,
        metrics: &metrics,
        inputs: &inputs,
        ingest: &ingest,
    };
    let decoded = DecodedAction {
        action: kbf_proto::reapi::Action::default(),
        command: kbf_proto::reapi::Command {
            arguments: vec!["xcrun".to_owned()],
            ..Default::default()
        },
    };

    let prepared = prepare(
        &ctx,
        RUN,
        (
            &action(),
            Some(&decoded),
            &kbf_front::ActionLabel::default(),
        ),
        Booking::Share,
    )
    .await;

    let prepared = prepared.expect("a Mac job");
    assert_eq!(prepared.form, crate::executor::InputTree::Provider);
    let minted = minting.0.lock().expect("minted").clone();
    assert_eq!(minted, [Some(decoded)]);
}
