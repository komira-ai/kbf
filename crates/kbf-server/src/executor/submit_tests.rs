use std::sync::Arc;
use std::time::Duration;

use js_proto::v1::{JobResult, JobState, JobTimes, Needs, NetworkLevel, ResultReason};
use kbf_proto::reapi::Digest;
use kbf_types::OperationId;
use ring::digest::{SHA256, digest};

use super::{
    Booking, FRONTEND_LABEL, INSTANCE_LABEL, Preparer, Settled, job_id, pinned, prepare, require,
    wall_seconds,
};
use crate::executor::outcome::{Verdict, judge};
use crate::executor::size_class::SizeClass;
use crate::executor::traits::InputsError;
use crate::executor::{
    ActionFacts, Config, Ingest, Ingested, InputTree, Inputs, MintError, PackKey, PutDirRoots,
    RunId, UntrustedLimits, build_id,
};

/// The class of a build of `root//pkg:t`: `root.pkg.t.build`.
pub(super) fn built_class() -> SizeClass {
    let out = "buck-out/v2/art/root/7a3c1e9f0b2d4c68/pkg/__t__/o".to_owned();
    SizeClass::of_action(&crate::action_usage::Shape::of(&["cc".to_owned()], &[out]))
}

/// Fakes that answer at once and never fail.
struct Easy;

#[tonic::async_trait]
impl Ingest for Easy {
    async fn mint(
        &self,
        run: RunId,
        _: &Digest,
        _: Duration,
        _: InputTree,
    ) -> Result<PackKey, MintError> {
        Ok(PackKey::new(format!("cas/b/{}/aa-0", build_id(run))))
    }

    async fn ingest(&self, _: RunId, _: &PackKey, _: &Digest, _: &UntrustedLimits) -> Ingested {
        Ingested::Unavailable("not read here".to_owned())
    }
}

#[tonic::async_trait]
impl Inputs for Easy {
    async fn action(&self, _: &Digest) -> Result<ActionFacts, InputsError> {
        Ok(ActionFacts {
            image: format!("registry.test/app@sha256:{}", "ab".repeat(32)),
            timeout: Some(Duration::from_secs(20)),
            system: crate::executor::System::Linux,
            size_class: built_class(),
        })
    }

    async fn tree(&self, _: RunId) -> Result<String, InputsError> {
        Ok("ab".repeat(32))
    }
}

/// What the preparation of a run reads: a config and the two seams, which answer at once.
/// A test replaces the seam it is about.
pub(super) struct Harness {
    pub(super) cfg: Config,
    pub(super) inputs: Arc<dyn Inputs>,
    pub(super) ingest: Arc<dyn Ingest>,
}

impl Harness {
    pub(super) fn new() -> Self {
        let cfg = Config {
            task_out: Some(PutDirRoots::new("/mnt/ts/put").expect("roots")),
            ..Config::new("front")
        };
        Self {
            cfg,
            inputs: Arc::new(Easy),
            ingest: Arc::new(Easy),
        }
    }

    pub(super) fn preparer(&self) -> Preparer<'_> {
        Preparer {
            cfg: &self.cfg,
            inputs: &self.inputs,
            ingest: &self.ingest,
        }
    }
}

/// The run `attempt` of operation 7 used throughout: operation 7, attempt 8.
pub(super) const RUN: RunId = RunId::new(OperationId(7), 8);

const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A frontend id of 200 bytes whose own words would show in an id that wrote it.
fn long_frontend() -> String {
    "visible-frontend-name-".repeat(10)[..200].to_owned()
}

/// Catches a job id the service would refuse, or one that is not the same for the same run:
/// for the largest operation, attempt and frontend id there are, the id is 1 to 63 bytes of
/// letters, digits, `.`, `_` and `-`; the same inputs give the same id and any one input
/// changed gives another; the frontend's id is hashed and not written; and the id is
/// `k<8 hex of the sha256 of the frontend id>-<operation>-<attempt>`.
#[test]
fn a_job_id_is_a_valid_idempotency_key() {
    let frontend = long_frontend();
    assert_eq!(frontend.len(), 200);
    let run = RunId::new(OperationId(u64::MAX), u32::MAX);
    let id = job_id(&frontend, run);
    assert!((1..=63).contains(&id.len()), "{id}");
    assert!(
        id.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')),
        "{id}"
    );
    assert_eq!(id, job_id(&frontend, run));
    assert!(!id.contains("visible") && !id.contains("frontend"), "{id}");

    let hash = hex::encode(digest(&SHA256, frontend.as_bytes()));
    assert_eq!(id, format!("k{}-{}-{}", &hash[..8], u64::MAX, u32::MAX));
    let small = job_id("front", RunId::new(OperationId(4), 9));
    let hash = hex::encode(digest(&SHA256, b"front"));
    assert_eq!(small, format!("k{}-4-9", &hash[..8]));

    let others = [
        job_id("other", run),
        job_id(&frontend, RunId::new(OperationId(u64::MAX - 1), u32::MAX)),
        job_id(&frontend, RunId::new(OperationId(u64::MAX), u32::MAX - 1)),
    ];
    for other in &others {
        assert_ne!(other, &id);
    }
    let distinct: std::collections::BTreeSet<&String> = others.iter().collect();
    assert_eq!(distinct.len(), others.len());
}

/// Catches an image the job service would refuse (or, worse, run from a mutable tag) being
/// submitted: only `name@sha256:<64 hex>` is taken, and the platform property's `docker://` is
/// dropped.
#[test]
fn an_image_must_be_pinned_by_digest() {
    let image = format!("registry.test/app@sha256:{DIGEST}");
    assert_eq!(pinned(&image).as_deref(), Ok(image.as_str()));
    assert_eq!(
        pinned(&format!("docker://{image}")).as_deref(),
        Ok(image.as_str())
    );
    let tagged = format!("registry.test/app:1.0@sha256:{DIGEST}");
    assert_eq!(pinned(&tagged).as_deref(), Ok(tagged.as_str()));
    for bad in [
        "registry.test/app:latest".to_owned(),
        "registry.test/app@sha256:abc".to_owned(),
        format!("registry.test/app@sha256:{}z", &DIGEST[1..]),
        format!("registry.test/app@sha512:{DIGEST}"),
        format!("registry.test/app@sha256:{}", DIGEST.to_uppercase()),
        format!("@sha256:{DIGEST}"),
        String::new(),
    ] {
        let Err(Settled::Invalid(why)) = pinned(&bad) else {
            panic!("{bad:?} was taken");
        };
        assert!(why.contains("not pinned"), "{why}");
    }
}

/// Catches a user error (an invalid action) being classed as the farm's, and the farm's
/// (an unreachable store) as the user's: the first is never rerun.
#[test]
fn what_could_not_be_read_is_classed_by_whose_fault_it_is() {
    assert_eq!(
        Settled::from(InputsError::Invalid("no image".to_owned())),
        Settled::Invalid("no image".to_owned())
    );
    assert_eq!(
        Settled::from(InputsError::Unavailable("down".to_owned())),
        Settled::Infra("down".to_owned())
    );
    assert_eq!(require(true, || unreachable!()), Ok(()));
    assert_eq!(
        require(false, || "no".to_owned()),
        Err(Settled::Invalid("no".to_owned()))
    );
}

/// Catches a job given less wall time than it asks for: whole seconds, rounded up.
#[test]
fn the_wall_limit_rounds_up_to_whole_seconds() {
    assert_eq!(wall_seconds(Duration::from_millis(1_200)), 2);
    assert_eq!(wall_seconds(Duration::from_secs(3)), 3);
    assert_eq!(wall_seconds(Duration::from_nanos(1)), 1);
    assert_eq!(wall_seconds(Duration::ZERO), 0);
}

/// Inputs of an action with the given timeout (`None`: it names none), on a good image.
struct Timed(Option<Duration>);

#[tonic::async_trait]
impl Inputs for Timed {
    async fn action(&self, _: &Digest) -> Result<ActionFacts, InputsError> {
        Ok(ActionFacts {
            image: format!("registry.test/app@sha256:{DIGEST}"),
            timeout: self.0,
            system: crate::executor::System::Linux,
            size_class: built_class(),
        })
    }

    async fn tree(&self, _: RunId) -> Result<String, InputsError> {
        Ok("cd".repeat(32))
    }
}

fn action() -> Digest {
    Digest {
        hash: "cd".repeat(32),
        size_bytes: 3,
    }
}

const BOOKING: Booking = Booking::Share;

/// Prepares the job of [`RUN`] over `harness`, with `inputs` in place of its own.
async fn prepared(
    harness: &Harness,
    inputs: Option<Timed>,
    booking: Booking,
) -> Result<super::Prepared, Settled> {
    let inputs: Arc<dyn Inputs> = match inputs {
        Some(inputs) => Arc::new(inputs),
        None => Arc::clone(&harness.inputs),
    };
    let ctx = Preparer {
        inputs: &inputs,
        ..harness.preparer()
    };
    prepare(&ctx, RUN, &action(), booking).await
}

/// Catches the job's wall limit being the executor's longest run instead of the
/// action's: it is the action's own timeout, or the default where it names none, with the run
/// margin, in whole seconds rounded up. An action over the executor's longest run, named or
/// defaulted, is the caller's to fix, and nothing is submitted for it.
#[tokio::test(start_paused = true)]
async fn the_wall_limit_is_the_actions_own() {
    let mut harness = Harness::new();
    harness.cfg.run_margin = Duration::from_millis(300);
    let named = prepared(&harness, None, BOOKING).await.expect("prepared");
    assert_eq!(
        named.spec.wall_limit_seconds,
        Some(wall_seconds(harness.cfg.wall(Duration::from_secs(20))))
    );
    assert_eq!(named.spec.wall_limit_seconds, Some(21));

    let unnamed = prepared(&harness, Some(Timed(None)), BOOKING)
        .await
        .expect("prepared");
    assert_eq!(
        unnamed.spec.wall_limit_seconds,
        Some(wall_seconds(harness.cfg.wall(harness.cfg.default_timeout)))
    );

    let too_long = Timed(Some(harness.cfg.max_run + Duration::from_secs(1)));
    let Err(settled) = prepared(&harness, Some(too_long), BOOKING).await else {
        panic!("an action over the longest run was prepared");
    };
    assert!(matches!(settled, Settled::Invalid(_)), "{settled:?}");

    harness.cfg.default_timeout = harness.cfg.max_run + Duration::from_secs(1);
    let Err(settled) = prepared(&harness, Some(Timed(None)), BOOKING).await else {
        panic!("a default over the longest run was prepared");
    };
    assert!(matches!(settled, Settled::Invalid(_)), "{settled:?}");
}

/// Catches a job that ran out of wall time being taken for anything but the action's own
/// timeout, because of when it started or ended: nothing cuts a job's wall limit, so a failed
/// job with that reason is a timeout whatever its times say.
#[test]
fn a_job_stopped_at_its_wall_limit_is_a_timeout_whatever_its_times() {
    let job = js_proto::v1::Job {
        id: "k0123abcd-7-8".to_owned(),
        state: JobState::Failed.into(),
        ..js_proto::v1::Job::default()
    };
    let times = |started: Option<u64>, ended: Option<u64>| JobTimes {
        queued_at_ms: 1,
        placed_at_ms: Some(1),
        started_at_ms: started,
        ended_at_ms: ended,
    };
    for times in [
        None,
        Some(times(None, None)),
        Some(times(Some(5), Some(6))),
        Some(times(Some(5), Some(5 + 86_400_000))),
    ] {
        let result = JobResult {
            state: JobState::Failed.into(),
            reason: ResultReason::WallLimit.into(),
            times,
            ..JobResult::default()
        };
        assert_eq!(
            judge(&job, &result),
            Verdict::Settled(Settled::Timeout),
            "{result:?}"
        );
    }
}

fn arg<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
    let at = args.iter().position(|a| a == flag)?;
    args.get(at + 1).map(String::as_str)
}

/// Catches a job asking for the wrong thing of the service: it names no CPU, no memory and no
/// disk, so the service sizes it from its class's history; it runs on a Linux container host
/// of the configured architecture, with no network, and labels naming the frontend, the process
/// and the action's size class; and its id is the run's.
#[tokio::test(start_paused = true)]
async fn prepare_builds_the_job_the_service_is_asked_for() {
    let harness = Harness::new();
    let cfg = &harness.cfg;
    let built = prepared(&harness, None, BOOKING).await.expect("prepared");
    let spec = &built.spec;
    assert_eq!(spec.job_id, job_id(&cfg.frontend, RUN));
    assert!(spec.env.is_empty());
    assert_eq!(
        spec.needs,
        Some(Needs::default()),
        "no CPU, memory, disk or GPU, and not the whole host"
    );
    assert_eq!(spec.constraints.as_ref(), Some(&cfg.constraints()));
    assert_eq!(spec.network(), NetworkLevel::None);
    assert!(spec.ports.is_empty());
    assert_eq!(spec.labels.len(), 3);
    assert_eq!(
        spec.labels.get("size-class").map(String::as_str),
        Some("root.pkg.t.build")
    );
    assert_eq!(
        spec.labels.get(FRONTEND_LABEL).map(String::as_str),
        Some(cfg.frontend.as_str())
    );
    assert_eq!(
        spec.labels.get(INSTANCE_LABEL).map(String::as_str),
        Some(cfg.instance.as_str())
    );
    assert_eq!(arg(&spec.args, "--lease"), Some("7-8"));
    assert!(built.key.as_str().contains(&build_id(RUN)));
}

/// Catches an action read as the wrong booking: only a whole-machine action is the whole host,
/// every other is a share.
#[test]
fn an_action_books_the_whole_host_only_for_a_whole_machine() {
    assert_eq!(Booking::of(true), Booking::WholeHost);
    assert_eq!(Booking::of(false), Booking::Share);
}

/// Catches a whole-host need the service would refuse, or one that is sized: it needs the whole
/// host and nothing else (no CPU, memory, disk or GPU, which the service refuses beside
/// `whole_host`) and carries no size class (it is never sized, and its runs would teach the
/// class a whole host's use), where a share needs nothing named and carries its class.
#[test]
fn a_whole_host_needs_the_whole_host_alone_and_is_never_sized() {
    let class = built_class();
    assert_eq!(
        Booking::WholeHost.needs(),
        Needs {
            whole_host: true,
            ..Needs::default()
        }
    );
    assert_eq!(Booking::WholeHost.class(&class), None);
    assert_eq!(BOOKING.needs(), Needs::default());
    assert_eq!(BOOKING.class(&class), Some(&class));
}

/// An ingest whose mint fails as told.
struct NoMint(MintError);

#[tonic::async_trait]
impl Ingest for NoMint {
    async fn mint(
        &self,
        _: RunId,
        _: &Digest,
        _: Duration,
        _: InputTree,
    ) -> Result<PackKey, MintError> {
        Err(self.0.clone())
    }

    async fn ingest(&self, _: RunId, _: &PackKey, _: &Digest, _: &UntrustedLimits) -> Ingested {
        Ingested::Unavailable("not read here".to_owned())
    }
}

/// Catches a refused mint classed as the farm's, or an unavailable one as the caller's: a key
/// the frontend will not issue is the caller's to fix, one it could not issue is
/// infrastructure, and both messages say what failed.
#[tokio::test(start_paused = true)]
async fn a_put_directory_that_could_not_be_minted_is_classed_by_whose_fault_it_is() {
    let mut harness = Harness::new();
    harness.ingest = Arc::new(NoMint(MintError::Refused("too deep".to_owned())));
    let Err(refused) = prepared(&harness, None, BOOKING).await else {
        panic!("prepared with no key");
    };
    assert_eq!(
        refused,
        Settled::Invalid("no put directory: too deep".to_owned())
    );
    harness.ingest = Arc::new(NoMint(MintError::Unavailable("down".to_owned())));
    let Err(refused) = prepared(&harness, None, BOOKING).await else {
        panic!("prepared with no key");
    };
    assert_eq!(refused, Settled::Infra("no put directory: down".to_owned()));
}

/// Inputs whose action cannot be read.
struct Broken;

#[tonic::async_trait]
impl Inputs for Broken {
    async fn action(&self, _: &Digest) -> Result<ActionFacts, InputsError> {
        Err(InputsError::Invalid("no such action".to_owned()))
    }
}

/// Catches a failure to read the action being lost or classed wrong: the action that cannot
/// be read is the caller's, and nothing is prepared.
#[tokio::test(start_paused = true)]
async fn an_action_that_cannot_be_read_settles_the_run_as_the_callers() {
    let mut harness = Harness::new();
    harness.inputs = Arc::new(Broken);
    let Err(refused) = prepare(&harness.preparer(), RUN, &action(), BOOKING).await else {
        panic!("prepared an action that cannot be read");
    };
    assert_eq!(refused, Settled::Invalid("no such action".to_owned()));
}
