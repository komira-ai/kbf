use std::sync::Arc;
use std::time::Duration;

use js_proto::v1::{JobFile, JobResult, JobState, JobTimes, Needs, NetworkLevel, ResultReason};
use kbf_job_client::fake::Call;
use kbf_proto::reapi::Digest;
use kbf_types::LeaseId;
use ring::digest::{SHA256, digest};

use super::{
    Booking, FRONTEND_LABEL, INSTANCE_LABEL, Settled, job_id, pinned, prepare, require,
    wall_seconds,
};
use crate::executor::outcome::{Verdict, judge};
use crate::executor::test_support::{GIB, Harness};
use crate::executor::traits::{InputsError, PresignError};
use crate::executor::{
    ActionFacts, Config, Ctx, Delivery, Deps, Inputs, ReadGrant, Secret, build_id,
};

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
    for error in [
        PresignError::Refused("ceiling".to_owned()),
        PresignError::Unavailable("ceiling".to_owned()),
    ] {
        let Settled::Infra(why) = Settled::from(error) else {
            panic!("not infra");
        };
        assert!(why.contains("ceiling"), "{why}");
    }
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
        })
    }

    async fn read_grant(
        &self,
        _: LeaseId,
        _: &Digest,
        _: Duration,
    ) -> Result<ReadGrant, InputsError> {
        Ok(ReadGrant {
            url: "https://grant.test:9400".to_owned(),
            token: Secret::new("the-token"),
        })
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
    };
    prepare(&ctx, LeaseId::new(7, 8), &action(), booking).await
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

/// Catches a job asking for the wrong thing of the service: its needs are what the farm
/// booked (at least one millicpu) and the hard memory cap, and no disk; it runs
/// on a Linux container host of the configured architecture, with the allow-listed network, the
/// image's own entrypoint, and labels naming the frontend and the process; and its id is the
/// lease's, attempt 0.
#[tokio::test(start_paused = true)]
async fn prepare_builds_the_job_the_service_is_asked_for() {
    let harness = Harness::new().await;
    let cfg = &harness.cfg;
    let built = prepared(&harness, None, BOOKING).await.expect("prepared");
    let spec = &built.spec;
    assert_eq!(spec.job_id, job_id(&cfg.frontend, LeaseId::new(7, 8), 0));
    assert_eq!(
        spec.image,
        format!("registry.test/app@sha256:{}", "ab".repeat(32))
    );
    assert_eq!(spec.command, "");
    assert!(spec.env.is_empty());
    let needs = spec.needs.as_ref().expect("needs");
    assert_eq!(needs.cpu_millis, 1_500);
    assert_eq!(needs.memory_bytes, Config::hard_memory(GIB));
    assert_eq!(needs.disk_bytes, None, "a job books no disk");
    assert_eq!((needs.gpus, needs.whole_host), (0, false));
    assert_eq!(spec.constraints.as_ref(), Some(&cfg.constraints()));
    assert_eq!(spec.network(), NetworkLevel::AllowList);
    assert!(spec.ports.is_empty());
    assert_eq!(spec.labels.len(), 2);
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

    let none = Booking::Share {
        millicpus: 0,
        memory_bytes: GIB,
    };
    let idle = prepared(&harness, None, none).await.expect("prepared");
    assert_eq!(idle.spec.needs.as_ref().map(|n| n.cpu_millis), Some(1));
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

/// Catches a whole-host job the service would refuse, or one that differs from a shared job
/// in more than its needs: it needs the whole host and nothing else (no CPU, memory, disk or
/// GPU, which the service refuses beside `whole_host`); its id, image, command, environment, constraints, wall limit, network, ports,
/// priority and labels are those of the same lease's shared job, and its arguments name the
/// same lease and action (the pack url in them is minted again for each prepare).
#[tokio::test(start_paused = true)]
async fn a_whole_host_job_needs_the_whole_host_alone() {
    let harness = Harness::new().await;
    let whole = prepared(&harness, None, Booking::WholeHost)
        .await
        .expect("prepared");
    let shared = prepared(&harness, None, BOOKING).await.expect("prepared");
    assert_eq!(
        whole.spec.needs,
        Some(Needs {
            whole_host: true,
            ..Needs::default()
        })
    );
    let shared_needs = shared.spec.needs.clone().expect("needs");
    assert!(shared_needs.cpu_millis > 0 && !shared_needs.whole_host);
    let without_needs = |spec: &js_proto::v1::JobSpec| js_proto::v1::JobSpec {
        needs: None,
        // The grant and the pack url are made again for each prepare.
        args: Vec::new(),
        files: Vec::new(),
        ..spec.clone()
    };
    assert_eq!(without_needs(&whole.spec), without_needs(&shared.spec));
    assert_eq!(arg(&whole.spec.args, "--lease"), Some("7-8"));
    assert_eq!(
        arg(&whole.spec.args, "--action"),
        arg(&shared.spec.args, "--action")
    );
}

/// Catches the secrets travelling the way the operator did not choose: under `Files` the
/// presigned url and the read token are the contents of the job's two files and no argument
/// holds either; under `InsecureArgv` the arguments hold them and the job has no files.
#[tokio::test(start_paused = true)]
async fn the_secrets_travel_as_the_delivery_says() {
    let mut harness = Harness::new().await;
    harness.cfg.delivery = Delivery::Files;
    harness.cfg.insecure_argv_secrets = false;
    let files = prepared(&harness, Some(Timed(None)), BOOKING)
        .await
        .expect("prepared");
    let url = format!("https://store.test/{}", files.key);
    assert_eq!(
        files.spec.files,
        vec![
            JobFile {
                name: "pack-url".to_owned(),
                contents: url.clone(),
            },
            JobFile {
                name: "grant-token".to_owned(),
                contents: "the-token".to_owned(),
            },
        ]
    );
    for arg in &files.spec.args {
        assert!(
            arg != "the-token" && !arg.contains("store.test"),
            "an argument holds a secret: {arg}"
        );
    }
    assert!(arg(&files.spec.args, "--pack-url-file").is_some());
    assert!(arg(&files.spec.args, "--grant-token-file").is_some());

    harness.cfg.delivery = Delivery::InsecureArgv;
    harness.cfg.insecure_argv_secrets = true;
    let argv = prepared(&harness, Some(Timed(None)), BOOKING)
        .await
        .expect("prepared");
    assert!(argv.spec.files.is_empty());
    assert_eq!(arg(&argv.spec.args, "--pack-url"), Some(url.as_str()));
    assert_eq!(arg(&argv.spec.args, "--grant-token"), Some("the-token"));
    assert!(
        argv.spec
            .args
            .iter()
            .any(|a| a == "--insecure-argv-secrets")
    );
}
