use std::sync::Arc;
use std::time::Duration;

use js_proto::v1::{JobResult, JobState, JobTimes, Needs, NetworkLevel, ResultReason};
use kbf_job_client::fake::Call;
use kbf_proto::reapi::Digest;
use kbf_types::LeaseId;
use ring::digest::{SHA256, digest};

use super::{
    Booking, FRONTEND_LABEL, INSTANCE_LABEL, Settled, job_id, pinned, prepare, require,
    wall_seconds,
};
use crate::executor::outcome::{Verdict, judge};
use crate::executor::test_support::{GIB, Harness, built_class};
use crate::executor::traits::InputsError;
use crate::executor::{ActionFacts, Ctx, Deps, Inputs, build_id};

const DIGEST: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// A frontend id of 200 bytes whose own words would show in an id that wrote it.
fn long_frontend() -> String {
    "visible-frontend-name-".repeat(10)[..200].to_owned()
}

/// Catches a job id the service would refuse, or one that is not the same for the same lease:
/// for the largest lease numbers, attempt and frontend id there are, the id is 1 to 63 bytes
/// of letters, digits, `.`, `_` and `-`; the same inputs give the same id and any one input
/// changed gives another; the frontend's id is hashed and not written; and the id is
/// `k<8 hex of the sha256 of the frontend id>-<term>-<seq>-<attempt>`.
#[test]
fn a_job_id_is_a_valid_idempotency_key() {
    let frontend = long_frontend();
    assert_eq!(frontend.len(), 200);
    let lease = LeaseId::new(u64::MAX, u64::MAX);
    let id = job_id(&frontend, lease, u32::MAX);
    assert!((1..=63).contains(&id.len()), "{id}");
    assert!(
        id.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-')),
        "{id}"
    );
    assert_eq!(id, job_id(&frontend, lease, u32::MAX));
    assert!(!id.contains("visible") && !id.contains("frontend"), "{id}");

    let hash = hex::encode(digest(&SHA256, frontend.as_bytes()));
    assert_eq!(
        id,
        format!("k{}-{}-{}-{}", &hash[..8], u64::MAX, u64::MAX, u32::MAX)
    );
    let small = job_id("front", LeaseId::new(4, 9), 0);
    let hash = hex::encode(digest(&SHA256, b"front"));
    assert_eq!(small, format!("k{}-4-9-0", &hash[..8]));

    let others = [
        job_id("other", lease, u32::MAX),
        job_id(&frontend, LeaseId::new(u64::MAX - 1, u64::MAX), u32::MAX),
        job_id(&frontend, LeaseId::new(u64::MAX, u64::MAX - 1), u32::MAX),
        job_id(&frontend, lease, u32::MAX - 1),
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

    async fn tree(&self, _: LeaseId) -> Result<String, InputsError> {
        Ok("cd".repeat(32))
    }
}

fn action() -> Digest {
    Digest {
        hash: "cd".repeat(32),
        size_bytes: 3,
    }
}

const BOOKING: Booking = Booking::Share {
    millicpus: 1_500,
    memory_bytes: GIB,
};

/// Prepares the job of lease 7.8 over `harness`'s deps, with `inputs` in place of its own.
async fn prepared(
    harness: &Harness,
    inputs: Option<Timed>,
    booking: Booking,
) -> Result<Arc<super::Prepared>, Settled> {
    let inputs: Arc<dyn Inputs> = match inputs {
        Some(inputs) => Arc::new(inputs),
        None => Arc::clone(&harness.deps.inputs),
    };
    let deps = Deps {
        inputs,
        ..harness.deps.clone()
    };
    let ctx = Ctx {
        cfg: &harness.cfg,
        deps: &deps,
        twin: &harness.twin,
        share: None,
    };
    prepare(
        &ctx.preparer(),
        crate::executor::mac::WorkerOs::Linux,
        LeaseId::new(7, 8),
        &action(),
        booking,
    )
    .await
}

/// Catches the job's wall limit being a lease's or the executor's longest run instead of the
/// action's: it is the action's own timeout, or the default where it names none, with the run
/// margin, in whole seconds rounded up. An action over the executor's longest run, named or
/// defaulted, is the caller's to fix, and nothing is submitted for it.
#[tokio::test(start_paused = true)]
async fn the_wall_limit_is_the_actions_not_a_leases() {
    let mut harness = Harness::new().await;
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

    let calls = harness.fake.calls();
    assert!(
        !calls.iter().any(|call| matches!(call, Call::SubmitJob(_))),
        "{calls:?}"
    );
}

/// Catches a job that ran out of wall time being taken for anything but the action's own
/// timeout, because of when it started or ended: nothing cuts a job's wall limit, so a failed
/// job with that reason is a timeout whatever its times say.
#[test]
fn a_job_stopped_at_its_wall_limit_is_a_timeout_whatever_its_times() {
    let job = js_proto::v1::Job {
        id: "k0123abcd-7-8-0".to_owned(),
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
/// disk, whatever the farm booked, so the service sizes it from its class's history; it runs
/// on a Linux container host of the configured architecture, with no network, and labels naming
/// the frontend, the process and the action's size class; and its id is the lease's, attempt 0.
#[tokio::test(start_paused = true)]
async fn prepare_builds_the_job_the_service_is_asked_for() {
    let harness = Harness::new().await;
    let cfg = &harness.cfg;
    let built = prepared(&harness, None, BOOKING).await.expect("prepared");
    let spec = &built.spec;
    assert_eq!(spec.job_id, job_id(&cfg.frontend, LeaseId::new(7, 8), 0));
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
    assert!(built.key.as_str().contains(&build_id(LeaseId::new(7, 8))));
}

/// Catches a `Start` read as the wrong booking: only the kind `whole_machine`, spelt as the
/// protocol spells it, is the whole host, whatever CPU and memory it carries; every other kind
/// (`action`, none, another spelling, a kind no driver here serves) is the share it booked.
#[test]
fn a_start_books_the_whole_host_only_for_a_whole_machine_lease() {
    let start = |kind: &str| kbf_proto::worker::Start {
        kind: kind.to_owned(),
        millicpus: 3_000,
        memory_bytes: 5 * GIB,
        ..Default::default()
    };
    let share = Booking::Share {
        millicpus: 3_000,
        memory_bytes: 5 * GIB,
    };
    for (kind, booking) in [
        ("whole_machine", Booking::WholeHost),
        ("action", share),
        ("", share),
        ("Whole_Machine", share),
        ("whole-machine", share),
        ("vm", share),
    ] {
        assert_eq!(Booking::of(&start(kind)), booking, "{kind:?}");
    }
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
