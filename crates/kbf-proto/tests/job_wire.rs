//! The container-job fields of `kbf.worker.v1`: `Start.job` (9), `Start.fence` (10),
//! `Result.job_outcome` (7) and `PHASE_TAINTED` (6). Their numbers, wire types, encoded
//! bytes, presence rules, and how a peer of the earlier shape reads them.
//!
//! The earlier shape is held three ways: frozen `#[derive(Message)]` copies of `Start`,
//! `Result` and `Hello`/`NodePhase` below; hex of those messages as they encoded before
//! the fields existed; and `tests/fixtures/worker_before_job.proto`, the whole earlier
//! `worker.proto`, against which the descriptors of today's file are compared.
//!
//! Catches:
//! - a new field renumbered, retyped, moved into a `oneof` or given another presence rule;
//! - a field of the earlier shape removed, renumbered or retyped (descriptor comparison);
//! - an old peer that fails on, or re-encodes differently, a message that carries the new
//!   fields;
//! - a default written to the wire (a server that sets nothing must stay byte-identical to
//!   the earlier shape);
//! - a `Mount` whose absent flag means read-write;
//! - an unknown enum value that stops a decode, or that reads as a known one;
//! - `gpu_indices` read from only one of its two legal encodings;
//! - a job at its bounds that no longer fits a gRPC message.

use std::collections::BTreeSet;
use std::path::PathBuf;

use kbf_proto::google::rpc::Status;
use kbf_proto::reapi::{ActionResult, Digest};
use kbf_proto::worker as pb;
use prost::Message;
use prost_types::{FieldDescriptorProto, FileDescriptorProto, FileDescriptorSet};

// ---- the earlier shape, frozen -------------------------------------------------------

/// `Start` as it stood with fields 1 to 8.
#[derive(Clone, PartialEq, Message)]
struct OldStart {
    #[prost(message, optional, tag = "1")]
    lease_id: Option<pb::LeaseId>,
    #[prost(string, tag = "2")]
    kind: String,
    #[prost(message, optional, tag = "3")]
    action_digest: Option<Digest>,
    #[prost(uint64, tag = "4")]
    millicpus: u64,
    #[prost(uint64, tag = "5")]
    memory_bytes: u64,
    #[prost(uint64, tag = "6")]
    heartbeat_seq: u64,
    #[prost(uint64, tag = "7")]
    valid_for_ms: u64,
    #[prost(string, tag = "8")]
    device_id: String,
}

/// `Result` as it stood with fields 1 to 6.
#[derive(Clone, PartialEq, Message)]
struct OldResult {
    #[prost(message, optional, tag = "1")]
    lease_id: Option<pb::LeaseId>,
    #[prost(message, optional, tag = "2")]
    status: Option<Status>,
    #[prost(message, optional, tag = "3")]
    action_result: Option<ActionResult>,
    #[prost(message, optional, tag = "4")]
    action_digest: Option<Digest>,
    #[prost(int32, tag = "5")]
    memory_kill: i32,
    #[prost(int32, tag = "6")]
    lost: i32,
}

/// `Phase` as it stood, 0 to 5.
#[derive(Clone, Copy, Debug, PartialEq, Eq, prost::Enumeration)]
#[repr(i32)]
enum OldPhase {
    Serving = 0,
    Preparing = 1,
    Restoring = 2,
    Quarantined = 3,
    MemoryPressure = 4,
    Draining = 5,
}

#[derive(Clone, PartialEq, Message)]
struct OldNodePhase {
    #[prost(enumeration = "OldPhase", tag = "1")]
    phase: i32,
    #[prost(string, tag = "3")]
    reason: String,
}

#[derive(Clone, PartialEq, Message)]
struct OldHello {
    #[prost(string, tag = "2")]
    node_id: String,
    #[prost(message, optional, tag = "7")]
    phase: Option<OldNodePhase>,
}

// ---- messages and their pinned bytes --------------------------------------------------

fn lease() -> Option<pb::LeaseId> {
    Some(pb::LeaseId { term: 1, seq: 2 })
}

fn digest(hash: &str, size_bytes: i64) -> Option<Digest> {
    Some(Digest {
        hash: hash.to_owned(),
        size_bytes,
    })
}

/// A `Job` with every field set.
fn full_job() -> pb::Job {
    pb::Job {
        run_id: "r1".to_owned(),
        image: "img".to_owned(),
        args: vec!["a".to_owned(), "b".to_owned()],
        limits: Some(pb::JobLimits {
            cpu_millis: 2000,
            memory_bytes: 4096,
            pids: Some(0),
            wall_ms: 60_000,
        }),
        gpu_indices: vec![0, 1, 300],
        mounts: vec![pb::Mount {
            root_name: "w".to_owned(),
            rel: "x".to_owned(),
            writable: true,
        }],
        isolation: pb::Isolation::Contained as i32,
        labels: vec![pb::Label {
            name: "n".to_owned(),
            value: "v".to_owned(),
        }],
        cwd: "/w".to_owned(),
    }
}

const JOB_HEX: &str = concat!(
    "0a027231",     // run_id "r1"
    "1203696d67",   // image "img"
    "1a01611a0162", // args "a", "b"
    "220c",         // limits, 12 bytes:
    "08d00f",       //   cpu_millis 2000
    "108020",       //   memory_bytes 4096
    "1800",         //   pids 0, present
    "20e0d403",     //   wall_ms 60000
    "2a040001ac02", // gpu_indices 0, 1, 300, packed
    "3208",         // mount, 8 bytes:
    "0a0177",       //   root_name "w"
    "120178",       //   rel "x"
    "1801",         //   writable
    "3801",         // isolation CONTAINED
    "4206",         // label, 6 bytes:
    "0a016e",       //   name "n"
    "120176",       //   value "v"
    "4a022f77",     // cwd "/w"
);

/// Fields 1 to 8 of the full `Start`: lease 1.2, kind "job", 1000 millicpus, 2048 bytes,
/// heartbeat 7, valid for 14000 ms, device "d1".
const START_OLD_HEX: &str = concat!(
    "0a0408011002",
    "12036a6f62",
    "20e807",
    "288010",
    "3007",
    "38b06d",
    "42026431",
);

fn full_start(fence: pb::FencePolicy) -> pb::Start {
    pb::Start {
        lease_id: lease(),
        kind: "job".to_owned(),
        action_digest: None,
        millicpus: 1000,
        memory_bytes: 2048,
        heartbeat_seq: 7,
        valid_for_ms: 14_000,
        device_id: "d1".to_owned(),
        job: Some(full_job()),
        fence: fence as i32,
    }
}

fn full_outcome() -> pb::JobOutcome {
    pb::JobOutcome {
        run_id: "r1".to_owned(),
        exit: pb::JobExit::Failed as i32,
        exit_code: Some(3),
        term_signal: None,
        detail: "d".to_owned(),
        stdout_digest: digest("aa", 5),
        stderr_digest: digest("bb", 0),
        taint: Some(pb::JobTaint {
            reason: "t".to_owned(),
            check: Some("c".to_owned()),
        }),
        usage: Some(pb::ResourceUsage {
            wall_micros: 9,
            ..Default::default()
        }),
        image_digest: "sha256:ab".to_owned(),
    }
}

const OUTCOME_HEX: &str = concat!(
    "0a027231",         // run_id
    "1002",             // exit FAILED
    "1803",             // exit_code 3
    "2a0164",           // detail "d"
    "32060a0261611005", // stdout_digest aa/5
    "3a040a026262",     // stderr_digest bb/0
    "42060a0174120163", // taint reason "t", check "c"
    "4a022009",         // usage wall_micros 9
    "5209736861323536", // image_digest "sha256:ab"
    "3a6162",
);

/// The `Result` fields 1 and 2 that every shape below starts with: lease 1.2, status OK.
const RESULT_HEAD_HEX: &str = "0a04080110021200";

fn hex_of(m: &impl Message) -> String {
    hex::encode(m.encode_to_vec())
}

#[test]
fn a_start_with_a_job_and_a_fence_has_these_bytes_and_an_old_peer_reads_the_rest() {
    let start = full_start(pb::FencePolicy::SelfFence);
    let want = format!("{START_OLD_HEX}4a3b{JOB_HEX}5001");
    assert_eq!(hex_of(&start), want);
    assert_eq!(JOB_HEX.len() / 2, 0x3b, "the job's length byte");
    assert_eq!(
        pb::Start::decode(&start.encode_to_vec()[..]),
        Ok(start.clone())
    );

    let old = OldStart::decode(&start.encode_to_vec()[..]).expect("an older peer decodes");
    assert_eq!(
        (old.lease_id, old.kind.as_str(), old.action_digest.as_ref()),
        (lease(), "job", None)
    );
    assert_eq!(
        (
            old.millicpus,
            old.memory_bytes,
            old.heartbeat_seq,
            old.valid_for_ms
        ),
        (1000, 2048, 7, 14_000)
    );
    assert_eq!(old.device_id, "d1");
    // Fields 9 and 10 are skipped, not kept: the old peer's own bytes are the rest.
    assert_eq!(hex_of(&old), START_OLD_HEX);
}

#[test]
fn a_start_without_a_job_or_a_fence_is_byte_identical_to_the_earlier_shape() {
    let mut start = full_start(pb::FencePolicy::Unspecified);
    start.job = None;
    assert_eq!(hex_of(&start), START_OLD_HEX);
    assert_eq!(pb::Start::default().encode_to_vec(), Vec::<u8>::new());
    assert_eq!(pb::Result::default().encode_to_vec(), Vec::<u8>::new());
    // An unset job message is absent, an empty one is present.
    let bare = pb::Start {
        job: Some(pb::Job::default()),
        ..Default::default()
    };
    assert_eq!(bare.encode_to_vec(), [0x4a, 0x00]);
}

#[test]
fn every_new_field_is_on_its_number_with_its_wire_type() {
    fn check(label: &str, m: &impl Message, want: &[u8]) {
        assert_eq!(hex::encode(m.encode_to_vec()), hex::encode(want), "{label}");
    }
    let x = || "x".to_owned();
    let job = |f: fn(&mut pb::Job)| {
        let mut j = pb::Job::default();
        f(&mut j);
        j
    };
    check(
        "Job.run_id",
        &job(|j| j.run_id = "x".into()),
        &[0x0a, 1, b'x'],
    );
    check(
        "Job.image",
        &job(|j| j.image = "x".into()),
        &[0x12, 1, b'x'],
    );
    check(
        "Job.args",
        &job(|j| j.args = vec!["x".into()]),
        &[0x1a, 1, b'x'],
    );
    check(
        "Job.limits",
        &job(|j| j.limits = Some(pb::JobLimits::default())),
        &[0x22, 0],
    );
    check(
        "Job.gpu_indices",
        &job(|j| j.gpu_indices = vec![1]),
        &[0x2a, 1, 1],
    );
    check(
        "Job.mounts",
        &job(|j| j.mounts = vec![pb::Mount::default()]),
        &[0x32, 0],
    );
    check(
        "Job.isolation",
        &job(|j| j.isolation = pb::Isolation::Host as i32),
        &[0x38, 2],
    );
    check(
        "Job.labels",
        &job(|j| j.labels = vec![pb::Label::default()]),
        &[0x42, 0],
    );
    check("Job.cwd", &job(|j| j.cwd = "x".into()), &[0x4a, 1, b'x']);

    let limits = |f: fn(&mut pb::JobLimits)| {
        let mut l = pb::JobLimits::default();
        f(&mut l);
        l
    };
    check(
        "JobLimits.cpu_millis",
        &limits(|l| l.cpu_millis = 1),
        &[0x08, 1],
    );
    check(
        "JobLimits.memory_bytes",
        &limits(|l| l.memory_bytes = 1),
        &[0x10, 1],
    );
    check("JobLimits.pids", &limits(|l| l.pids = Some(1)), &[0x18, 1]);
    check("JobLimits.wall_ms", &limits(|l| l.wall_ms = 1), &[0x20, 1]);

    check(
        "Mount.root_name",
        &pb::Mount {
            root_name: x(),
            ..Default::default()
        },
        &[0x0a, 1, b'x'],
    );
    check(
        "Mount.rel",
        &pb::Mount {
            rel: x(),
            ..Default::default()
        },
        &[0x12, 1, b'x'],
    );
    check(
        "Mount.writable",
        &pb::Mount {
            writable: true,
            ..Default::default()
        },
        &[0x18, 1],
    );
    check(
        "Label.name",
        &pb::Label {
            name: x(),
            ..Default::default()
        },
        &[0x0a, 1, b'x'],
    );
    check(
        "Label.value",
        &pb::Label {
            value: x(),
            ..Default::default()
        },
        &[0x12, 1, b'x'],
    );

    let outcome = |f: fn(&mut pb::JobOutcome)| {
        let mut o = pb::JobOutcome::default();
        f(&mut o);
        o
    };
    check(
        "JobOutcome.run_id",
        &outcome(|o| o.run_id = "x".into()),
        &[0x0a, 1, b'x'],
    );
    check(
        "JobOutcome.exit",
        &outcome(|o| o.exit = pb::JobExit::Failed as i32),
        &[0x10, 2],
    );
    check(
        "JobOutcome.exit_code",
        &outcome(|o| o.exit_code = Some(1)),
        &[0x18, 1],
    );
    check(
        "JobOutcome.term_signal",
        &outcome(|o| o.term_signal = Some(1)),
        &[0x20, 1],
    );
    check(
        "JobOutcome.detail",
        &outcome(|o| o.detail = "x".into()),
        &[0x2a, 1, b'x'],
    );
    check(
        "JobOutcome.stdout_digest",
        &outcome(|o| o.stdout_digest = Some(Digest::default())),
        &[0x32, 0],
    );
    check(
        "JobOutcome.stderr_digest",
        &outcome(|o| o.stderr_digest = Some(Digest::default())),
        &[0x3a, 0],
    );
    check(
        "JobOutcome.taint",
        &outcome(|o| o.taint = Some(pb::JobTaint::default())),
        &[0x42, 0],
    );
    check(
        "JobOutcome.usage",
        &outcome(|o| o.usage = Some(pb::ResourceUsage::default())),
        &[0x4a, 0],
    );
    check(
        "JobOutcome.image_digest",
        &outcome(|o| o.image_digest = "x".into()),
        &[0x52, 1, b'x'],
    );
    check(
        "JobTaint.reason",
        &pb::JobTaint {
            reason: x(),
            check: None,
        },
        &[0x0a, 1, b'x'],
    );
    check(
        "JobTaint.check",
        &pb::JobTaint {
            reason: String::new(),
            check: Some(x()),
        },
        &[0x12, 1, b'x'],
    );

    check(
        "Start.job",
        &pb::Start {
            job: Some(pb::Job::default()),
            ..Default::default()
        },
        &[0x4a, 0],
    );
    check(
        "Start.fence",
        &pb::Start {
            fence: pb::FencePolicy::RunOn as i32,
            ..Default::default()
        },
        &[0x50, 0x02],
    );
    check(
        "Result.job_outcome",
        &pb::Result {
            job_outcome: Some(pb::JobOutcome::default()),
            ..Default::default()
        },
        &[0x3a, 0],
    );
}

#[test]
fn the_enums_keep_their_numbers() {
    assert_eq!(pb::FencePolicy::Unspecified as i32, 0);
    assert_eq!(pb::FencePolicy::SelfFence as i32, 1);
    assert_eq!(pb::FencePolicy::RunOn as i32, 2);
    assert_eq!(pb::Isolation::Unspecified as i32, 0);
    assert_eq!(pb::Isolation::Contained as i32, 1);
    assert_eq!(pb::Isolation::Host as i32, 2);
    for (i, exit) in [
        pb::JobExit::Unspecified,
        pb::JobExit::Succeeded,
        pb::JobExit::Failed,
        pb::JobExit::KilledByServer,
        pb::JobExit::TimedOut,
        pb::JobExit::OutOfMemory,
        pb::JobExit::FailedToStart,
        pb::JobExit::KilledExternal,
    ]
    .into_iter()
    .enumerate()
    {
        assert_eq!(exit as usize, i);
    }
    assert_eq!(pb::Phase::Draining as i32, 5);
    assert_eq!(pb::Phase::Tainted as i32, 6);
}

#[test]
fn a_result_with_a_job_outcome_has_these_bytes_and_an_old_peer_reads_the_rest() {
    let result = pb::Result {
        lease_id: lease(),
        status: Some(Status::default()),
        job_outcome: Some(full_outcome()),
        ..Default::default()
    };
    assert_eq!(hex_of(&full_outcome()), OUTCOME_HEX);
    assert_eq!(OUTCOME_HEX.len() / 2, 0x30, "the outcome's length byte");
    assert_eq!(
        hex_of(&result),
        format!("{RESULT_HEAD_HEX}3a30{OUTCOME_HEX}")
    );
    assert_eq!(
        pb::Result::decode(&result.encode_to_vec()[..]),
        Ok(result.clone())
    );

    let old = OldResult::decode(&result.encode_to_vec()[..]).expect("an older peer decodes");
    assert_eq!(old.lease_id, lease());
    assert_eq!(old.status, Some(Status::default()));
    assert_eq!((&old.action_result, &old.action_digest), (&None, &None));
    assert_eq!((old.memory_kill, old.lost), (0, 0));
    assert_eq!(hex_of(&old), RESULT_HEAD_HEX);
}

#[test]
fn a_failed_to_start_and_a_fenced_result_are_read_by_an_old_peer() {
    let refused_run = pb::Result {
        lease_id: lease(),
        status: Some(Status::default()),
        job_outcome: Some(pb::JobOutcome {
            run_id: "r1".to_owned(),
            exit: pb::JobExit::FailedToStart as i32,
            detail: "pull".to_owned(),
            ..Default::default()
        }),
        ..Default::default()
    };
    assert_eq!(
        hex_of(&refused_run),
        format!("{RESULT_HEAD_HEX}3a0c0a02723110062a0470756c6c")
    );
    let old = OldResult::decode(&refused_run.encode_to_vec()[..]).expect("decodes");
    assert_eq!(old.action_result, None);

    // A fenced job: UNAVAILABLE and `lost`, no job_outcome, as an action's is.
    let fenced = pb::Result {
        lease_id: lease(),
        status: Some(Status {
            code: 14,
            message: "fenced".to_owned(),
            details: vec![],
        }),
        lost: pb::Lost::Fenced as i32,
        ..Default::default()
    };
    let old = OldResult::decode(&fenced.encode_to_vec()[..]).expect("decodes");
    assert_eq!(old.lost, pb::Lost::Fenced as i32);
    assert_eq!(
        hex_of(&old),
        hex_of(&fenced),
        "nothing of the new shape on it"
    );
}

#[test]
fn a_set_zero_exit_code_term_signal_or_pids_is_on_the_wire_and_an_unset_one_is_not() {
    let zero = pb::JobOutcome {
        exit_code: Some(0),
        ..Default::default()
    };
    assert_eq!(zero.encode_to_vec(), [0x18, 0]);
    let signal = pb::JobOutcome {
        term_signal: Some(0),
        ..Default::default()
    };
    assert_eq!(signal.encode_to_vec(), [0x20, 0]);
    let pids = pb::JobLimits {
        pids: Some(0),
        ..Default::default()
    };
    assert_eq!(pids.encode_to_vec(), [0x18, 0]);

    assert_eq!(pb::JobOutcome::decode(&[0x18, 0][..]), Ok(zero));
    assert_eq!(pb::JobOutcome::decode(&[0x20, 0][..]), Ok(signal));
    assert_eq!(pb::JobLimits::decode(&[0x18, 0][..]), Ok(pids));
    let neither = pb::JobOutcome::decode(&[0x10, 1][..]).expect("decodes");
    assert_eq!((neither.exit_code, neither.term_signal), (None, None));
    assert_eq!(pb::JobLimits::decode(&[][..]).expect("decodes").pids, None);
    // The check of a taint is optional too.
    assert_eq!(pb::JobTaint::default().encode_to_vec(), Vec::<u8>::new());
}

#[test]
fn an_unknown_enum_value_decodes_and_reads_as_its_unspecified_value() {
    let start = pb::Start::decode(&[0x50, 99][..]).expect("fence 99 decodes");
    assert_eq!(start.fence, 99);
    assert_eq!(
        start.fence(),
        pb::FencePolicy::Unspecified,
        "read as self-fence"
    );
    assert!(pb::FencePolicy::try_from(99).is_err());

    let job = pb::Job::decode(&[0x38, 99][..]).expect("isolation 99 decodes");
    assert_eq!(job.isolation, 99);
    assert!(
        pb::Isolation::try_from(job.isolation).is_err(),
        "the daemon refuses it"
    );

    let outcome = pb::JobOutcome::decode(&[0x10, 99][..]).expect("exit 99 decodes");
    assert_eq!(outcome.exit, 99);
    assert_eq!(
        outcome.exit(),
        pb::JobExit::Unspecified,
        "never read as success"
    );
    assert!(pb::JobExit::try_from(99).is_err());
}

#[test]
fn a_tainted_phase_is_unknown_to_an_old_peer_and_does_not_stop_its_decode() {
    let hello = pb::Hello {
        node_id: "n".to_owned(),
        phase: Some(pb::NodePhase {
            phase: pb::Phase::Tainted as i32,
            lease_id: None,
            reason: "dirty".to_owned(),
        }),
        ..Default::default()
    };
    let old = OldHello::decode(&hello.encode_to_vec()[..]).expect("an older peer decodes");
    let phase = old.phase.expect("the phase is there");
    assert_eq!(phase.reason, "dirty");
    assert_eq!(phase.phase, 6);
    assert!(
        OldPhase::try_from(phase.phase).is_err(),
        "it has no name there"
    );
    for known in 0..=5 {
        assert!(OldPhase::try_from(known).is_ok());
    }
}

#[test]
fn today_s_start_and_result_decode_with_nothing_of_the_new_shape() {
    // Encoded by the build before these fields, with every earlier field set.
    let start_hex = concat!(
        "0a0408011002",
        "1206616374696f6e",
        "1a060a0261621007",
        "20e807",
        "288010",
        "3007",
        "38b06d",
        "42026431",
    );
    let start = pb::Start::decode(&hex::decode(start_hex).expect("hex")[..]).expect("decodes");
    assert_eq!(
        start,
        pb::Start {
            lease_id: lease(),
            kind: "action".to_owned(),
            action_digest: digest("ab", 7),
            millicpus: 1000,
            memory_bytes: 2048,
            heartbeat_seq: 7,
            valid_for_ms: 14_000,
            device_id: "d1".to_owned(),
            job: None,
            fence: 0,
        }
    );
    assert_eq!(hex_of(&start), start_hex, "and it encodes as it did");
    assert_eq!(start.fence(), pb::FencePolicy::Unspecified);

    let result_hex = concat!(
        "0a0408011002",
        "1200",
        "1a022003",
        "22060a0261621007",
        "2801",
        "3002",
    );
    let result = pb::Result::decode(&hex::decode(result_hex).expect("hex")[..]).expect("decodes");
    assert_eq!(result.lease_id, lease());
    assert_eq!(result.action_result.map(|a| a.exit_code), Some(3));
    assert_eq!(result.action_digest, digest("ab", 7));
    assert_eq!(result.memory_kill, pb::MemoryKill::OwnLimit as i32);
    assert_eq!(result.lost, pb::Lost::Fenced as i32);
    assert_eq!(result.job_outcome, None);
}

#[test]
fn a_mount_is_read_only_unless_it_says_otherwise() {
    let absent = pb::Mount::decode(&[0x0a, 1, b'w', 0x12, 1, b'x'][..]).expect("decodes");
    assert!(!absent.writable, "a dropped flag never grants write");
    let writable =
        pb::Mount::decode(&[0x0a, 1, b'w', 0x12, 1, b'x', 0x18, 1][..]).expect("decodes");
    assert!(writable.writable);
    // Read-only puts nothing on the wire; read-write puts exactly the flag.
    assert_eq!(absent.encode_to_vec(), [0x0a, 1, b'w', 0x12, 1, b'x']);
    assert_eq!(
        writable.encode_to_vec(),
        [0x0a, 1, b'w', 0x12, 1, b'x', 0x18, 1]
    );
}

#[test]
fn gpu_indices_are_packed_and_both_encodings_decode() {
    let job = pb::Job {
        gpu_indices: vec![0, 1, 300],
        ..Default::default()
    };
    assert_eq!(hex_of(&job), "2a040001ac02");
    // One varint per element, each with the varint tag of field 5.
    let unpacked = [0x28, 0x00, 0x28, 0x01, 0x28, 0xac, 0x02];
    assert_eq!(pb::Job::decode(&unpacked[..]), Ok(job.clone()));
    assert_eq!(
        pb::Job::decode(&hex::decode("2a040001ac02").expect("hex")[..]),
        Ok(job)
    );
}

#[test]
fn a_job_at_its_bounds_is_far_below_a_grpc_message() {
    const GRPC_DEFAULT: usize = 4 * 1024 * 1024;
    let job = pb::Job {
        run_id: "r".repeat(128),
        image: "i".repeat(512),
        // 256 KiB of arguments: 16 of 16 KiB.
        args: (0..16).map(|_| "a".repeat(16 * 1024)).collect(),
        limits: Some(pb::JobLimits {
            cpu_millis: u64::MAX,
            memory_bytes: u64::MAX,
            pids: Some(u64::MAX),
            wall_ms: u64::MAX,
        }),
        gpu_indices: (0..255).collect(),
        mounts: (0..64)
            .map(|_| pb::Mount {
                root_name: "r".repeat(64),
                rel: "p".repeat(4096),
                writable: true,
            })
            .collect(),
        isolation: pb::Isolation::Contained as i32,
        labels: (0..64)
            .map(|_| pb::Label {
                name: "n".repeat(128),
                value: "v".repeat(1024),
            })
            .collect(),
        cwd: "c".repeat(4096),
    };
    let start = pb::Start {
        job: Some(job),
        fence: pb::FencePolicy::RunOn as i32,
        ..full_start(pb::FencePolicy::RunOn)
    };
    let bytes = start.encode_to_vec();
    assert!(
        bytes.len() > 256 * 1024,
        "the arguments are in it: {}",
        bytes.len()
    );
    assert!(bytes.len() < 1024 * 1024, "{} bytes", bytes.len());
    assert!(bytes.len() < GRPC_DEFAULT);
    assert_eq!(pb::Start::decode(&bytes[..]), Ok(start));

    let result = pb::Result {
        job_outcome: Some(pb::JobOutcome {
            detail: "d".repeat(1024),
            ..full_outcome()
        }),
        ..Default::default()
    };
    assert!(
        result.encoded_len() < 2048,
        "{} bytes",
        result.encoded_len()
    );
}

// ---- the earlier file against today's ------------------------------------------------

fn compile(file: &str, first: Option<PathBuf>) -> FileDescriptorSet {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("proto");
    let mut includes: Vec<PathBuf> = first.into_iter().collect();
    includes.extend([
        root.join("third_party/remote-apis"),
        root.join("third_party/googleapis"),
        root.join("third_party/grpc-proto"),
        root.clone(),
    ]);
    protox::compile([file], includes).expect("the proto compiles")
}

fn file_of(set: FileDescriptorSet, name: &str) -> FileDescriptorProto {
    set.file
        .into_iter()
        .find(|f| f.name() == name)
        .unwrap_or_else(|| panic!("no file {name}"))
}

fn before() -> FileDescriptorProto {
    let fixtures = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let name = "worker_before_job.proto";
    file_of(compile(name, Some(fixtures)), name)
}

fn today() -> FileDescriptorProto {
    let name = "kbf/worker/v1/worker.proto";
    file_of(compile(name, None), name)
}

fn shape(f: &FieldDescriptorProto) -> String {
    format!(
        "{} {:?} {:?} {} {} {:?}",
        f.name(),
        f.r#type(),
        f.label(),
        f.type_name(),
        f.proto3_optional(),
        f.oneof_index
    )
}

/// What `new` removed, renumbered or retyped of `old`: messages (and their oneofs), fields
/// by number, enums and their values by number, and the methods of services.
fn breaks(old: &FileDescriptorProto, new: &FileDescriptorProto) -> Vec<String> {
    let mut out = Vec::new();
    for om in &old.message_type {
        let Some(nm) = new.message_type.iter().find(|m| m.name() == om.name()) else {
            out.push(format!("message {} removed", om.name()));
            continue;
        };
        let oneofs = |m: &prost_types::DescriptorProto| {
            m.oneof_decl
                .iter()
                .map(|o| o.name().to_owned())
                .collect::<Vec<_>>()
        };
        if !oneofs(nm).starts_with(&oneofs(om)) {
            out.push(format!("message {} oneofs changed", om.name()));
        }
        for of in &om.field {
            match nm.field.iter().find(|f| f.number() == of.number()) {
                None => out.push(format!("{}.{} removed or renumbered", om.name(), of.name())),
                Some(nf) if shape(nf) != shape(of) => out.push(format!(
                    "{}.{} changed: {} -> {}",
                    om.name(),
                    of.name(),
                    shape(of),
                    shape(nf)
                )),
                Some(_) => {}
            }
        }
    }
    for oe in &old.enum_type {
        let Some(ne) = new.enum_type.iter().find(|e| e.name() == oe.name()) else {
            out.push(format!("enum {} removed", oe.name()));
            continue;
        };
        for ov in &oe.value {
            let kept = ne
                .value
                .iter()
                .any(|v| v.number() == ov.number() && v.name() == ov.name());
            if !kept {
                out.push(format!("{}.{} removed or renumbered", oe.name(), ov.name()));
            }
        }
    }
    for os in &old.service {
        let Some(ns) = new.service.iter().find(|s| s.name() == os.name()) else {
            out.push(format!("service {} removed", os.name()));
            continue;
        };
        for om in &os.method {
            let kept = ns.method.iter().any(|m| {
                (
                    m.name(),
                    m.input_type(),
                    m.output_type(),
                    m.client_streaming(),
                    m.server_streaming(),
                ) == (
                    om.name(),
                    om.input_type(),
                    om.output_type(),
                    om.client_streaming(),
                    om.server_streaming(),
                )
            });
            if !kept {
                out.push(format!("{}.{} removed or changed", os.name(), om.name()));
            }
        }
    }
    out
}

/// What `new` has that `old` has not, as sorted lines.
fn additions(old: &FileDescriptorProto, new: &FileDescriptorProto) -> Vec<String> {
    let mut out = BTreeSet::new();
    for nm in &new.message_type {
        match old.message_type.iter().find(|m| m.name() == nm.name()) {
            None => {
                out.insert(format!("message {}", nm.name()));
            }
            Some(om) => {
                for f in &nm.field {
                    if !om.field.iter().any(|o| o.number() == f.number()) {
                        out.insert(format!("field {}.{} = {}", nm.name(), f.name(), f.number()));
                    }
                }
            }
        }
    }
    for ne in &new.enum_type {
        match old.enum_type.iter().find(|e| e.name() == ne.name()) {
            None => {
                out.insert(format!("enum {}", ne.name()));
            }
            Some(oe) => {
                for v in &ne.value {
                    if !oe.value.iter().any(|o| o.number() == v.number()) {
                        out.insert(format!("value {}.{} = {}", ne.name(), v.name(), v.number()));
                    }
                }
            }
        }
    }
    for ns in &new.service {
        match old.service.iter().find(|s| s.name() == ns.name()) {
            None => {
                out.insert(format!("service {}", ns.name()));
            }
            Some(os) => {
                for m in &ns.method {
                    if !os.method.iter().any(|o| o.name() == m.name()) {
                        out.insert(format!("method {}.{}", ns.name(), m.name()));
                    }
                }
            }
        }
    }
    out.into_iter().collect()
}

#[test]
fn today_s_worker_proto_removes_and_changes_nothing_of_the_earlier_file() {
    let old = before();
    assert!(
        old.message_type.iter().all(|m| m.nested_type.is_empty()),
        "the comparison reads top-level messages only"
    );
    assert!(
        old.message_type.iter().any(|m| m.name() == "Start"),
        "the earlier file was read"
    );
    assert_eq!(breaks(&old, &today()), Vec::<String>::new());
}

#[test]
fn the_only_additions_to_the_worker_proto_are_the_job_fields_and_the_node_and_placement_load_and_cache_counts()
 {
    let mut want: Vec<String> = [
        "enum FencePolicy",
        "enum Isolation",
        "enum JobExit",
        // The node load a heartbeat carries (with the clocks and minutes of its leases) and the
        // peak window a lease's usage carries.
        "field CgroupUsage.peak_window_millicores = 17",
        "field Heartbeat.node_load = 6",
        "message NodeLoad",
        // The input cache counts a heartbeat carries.
        "field Heartbeat.input_cache = 7",
        "message InputCacheCounts",
        "message TreeCounts",
        // Whether the farm places by the load the nodes measure.
        "field Welcome.placement_load = 6",
        "field Result.job_outcome = 7",
        "field Start.job = 9",
        "field Start.fence = 10",
        "message Job",
        "message JobLimits",
        "message JobOutcome",
        "message JobTaint",
        "message Label",
        "message LeaseClock",
        "message LeaseMinute",
        "message Mount",
        "value Phase.PHASE_TAINTED = 6",
    ]
    .map(str::to_owned)
    .to_vec();
    want.sort();
    assert_eq!(additions(&before(), &today()), want);
}

/// Catches a comparison that cannot fail: each kind of break, planted in a copy of today's
/// file, is named.
#[test]
fn the_comparison_names_each_kind_of_break() {
    let old = before();
    let now = today();
    let broken = |edit: fn(&mut FileDescriptorProto)| {
        let mut f = now.clone();
        edit(&mut f);
        breaks(&old, &f)
    };
    fn msg(f: &FileDescriptorProto, name: &str) -> usize {
        f.message_type
            .iter()
            .position(|m| m.name() == name)
            .unwrap_or_else(|| panic!("no {name}"))
    }

    // A field renumbered: the earlier device_id (8) is gone from its number.
    let got = broken(|f| {
        let i = msg(f, "Start");
        let field = f.message_type[i]
            .field
            .iter_mut()
            .find(|x| x.name() == "device_id")
            .expect("device_id");
        field.number = Some(11);
    });
    assert_eq!(got, ["Start.device_id removed or renumbered"]);

    // A field retyped.
    let got = broken(|f| {
        let i = msg(f, "Result");
        let field = f.message_type[i]
            .field
            .iter_mut()
            .find(|x| x.name() == "memory_kill")
            .expect("memory_kill");
        field.r#type = Some(prost_types::field_descriptor_proto::Type::Int64 as i32);
    });
    assert_eq!(got.len(), 1);
    assert!(got[0].starts_with("Result.memory_kill changed"), "{got:?}");

    // A message removed, an enum value removed, an enum value renumbered, a method removed.
    let got = broken(|f| {
        let i = msg(f, "Cancel");
        f.message_type.remove(i);
        let cancel = f.message_type.iter().position(|m| m.name() == "Cancel");
        assert_eq!(cancel, None);
    });
    assert_eq!(got, ["message Cancel removed"]);
    let got = broken(|f| {
        let e = f
            .enum_type
            .iter_mut()
            .find(|e| e.name() == "Phase")
            .expect("Phase");
        e.value.retain(|v| v.name() != "PHASE_DRAINING");
    });
    assert_eq!(got, ["Phase.PHASE_DRAINING removed or renumbered"]);
    let got = broken(|f| {
        let e = f
            .enum_type
            .iter_mut()
            .find(|e| e.name() == "Lost")
            .expect("Lost");
        e.value.iter_mut().for_each(|v| {
            if v.name() == "LOST_FENCED" {
                v.number = Some(9);
            }
        });
    });
    assert_eq!(got, ["Lost.LOST_FENCED removed or renumbered"]);
    let got = broken(|f| f.service[0].method.clear());
    assert_eq!(got, ["Worker.Session removed or changed"]);
    let got = broken(|f| f.service.clear());
    assert_eq!(got, ["service Worker removed"]);
    let got = broken(|f| {
        let e = f
            .enum_type
            .iter()
            .position(|e| e.name() == "Lost")
            .expect("Lost");
        f.enum_type.remove(e);
    });
    assert_eq!(got, ["enum Lost removed"]);

    // A oneof dropped from a message that had one.
    let got = broken(|f| {
        let i = msg(f, "DaemonMessage");
        f.message_type[i].oneof_decl.clear();
    });
    assert_eq!(got, ["message DaemonMessage oneofs changed"]);

    // An unrelated addition is not a break.
    assert_eq!(breaks(&old, &now), Vec::<String>::new());
}
