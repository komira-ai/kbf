//! The generated types and services exist with the shapes the rest of kbf relies on.
//!
//! These are compile tests first: if `build.rs` stops generating a package, a module
//! path moves, or a field is renamed upstream, this file stops compiling. The asserts
//! catch what compiling alone cannot:
//! - a message whose fields do not survive an encode/decode round trip (a wrong prost
//!   mapping, or the well-known types mapped to something other than `prost-types`);
//! - a gRPC service path that differs from the one clients dial, which would make
//!   every REAPI or worker call fail with UNIMPLEMENTED.

use kbf_proto::google::bytestream::byte_stream_server::ByteStreamServer;
use kbf_proto::google::longrunning::operations_server::OperationsServer;
use kbf_proto::google::rpc::Status;
use kbf_proto::grpc::health::v1::health_server::HealthServer;
use kbf_proto::reapi::{
    self, ActionResult, Digest, FindMissingBlobsRequest, action_cache_server::ActionCacheServer,
    capabilities_server::CapabilitiesServer,
    content_addressable_storage_server::ContentAddressableStorageServer,
    execution_server::ExecutionServer,
};
use kbf_proto::worker::{
    self, Capability, DaemonMessage, Hello, LeaseId, NodePhase, Phase, Result as WorkerResult,
    ServerMessage, Start, daemon_message, pack_index_server::PackIndexServer, server_message,
    worker_server::WorkerServer,
};
use prost::Message;
use tonic::server::NamedService;

fn digest(hash: &str, size_bytes: i64) -> Digest {
    Digest {
        hash: hash.to_owned(),
        size_bytes,
    }
}

#[test]
fn find_missing_blobs_request_round_trips() {
    let req = FindMissingBlobsRequest {
        instance_name: "main".to_owned(),
        blob_digests: vec![digest(&"ab".repeat(32), 42), digest(&"cd".repeat(32), 0)],
        digest_function: reapi::digest_function::Value::Sha256 as i32,
    };
    let back = FindMissingBlobsRequest::decode(req.encode_to_vec().as_slice()).expect("decode");
    assert_eq!(back, req);
    assert_eq!(back.blob_digests[0].size_bytes, 42);
}

#[test]
fn action_result_uses_prost_types_for_well_known_types() {
    let result = ActionResult {
        exit_code: 3,
        execution_metadata: Some(reapi::ExecutedActionMetadata {
            worker: "node-1".to_owned(),
            worker_start_timestamp: Some(prost_types::Timestamp {
                seconds: 1_800_000_000,
                nanos: 5,
            }),
            ..Default::default()
        }),
        ..Default::default()
    };
    let back = ActionResult::decode(result.encode_to_vec().as_slice()).expect("decode");
    assert_eq!(back, result);
}

#[test]
fn services_have_their_upstream_names() {
    assert_eq!(
        <ExecutionServer<()> as NamedService>::NAME,
        "build.bazel.remote.execution.v2.Execution"
    );
    assert_eq!(
        <ActionCacheServer<()> as NamedService>::NAME,
        "build.bazel.remote.execution.v2.ActionCache"
    );
    assert_eq!(
        <ContentAddressableStorageServer<()> as NamedService>::NAME,
        "build.bazel.remote.execution.v2.ContentAddressableStorage"
    );
    assert_eq!(
        <CapabilitiesServer<()> as NamedService>::NAME,
        "build.bazel.remote.execution.v2.Capabilities"
    );
    assert_eq!(
        <ByteStreamServer<()> as NamedService>::NAME,
        "google.bytestream.ByteStream"
    );
    assert_eq!(
        <OperationsServer<()> as NamedService>::NAME,
        "google.longrunning.Operations"
    );
    assert_eq!(
        <WorkerServer<()> as NamedService>::NAME,
        "kbf.worker.v1.Worker"
    );
    assert_eq!(
        <HealthServer<()> as NamedService>::NAME,
        "grpc.health.v1.Health"
    );
    assert_eq!(
        <PackIndexServer<()> as NamedService>::NAME,
        "kbf.worker.v1.PackIndex"
    );
}

/// Asserts that a pack location survives an encode/decode round trip, and that the empty one
/// (the wire default) is a blob that is not located: a frontend that answers `Default` for a
/// digest it has no row for says so.
#[test]
fn pack_locations_round_trip_and_the_default_is_not_located() {
    let answer = worker::PackLocateResponse {
        locations: vec![
            worker::PackLocation {
                located: true,
                pack: "cas/d/node-1/boot/0000000000000001".to_owned(),
                offset: 4096,
                length: 12,
            },
            worker::PackLocation::default(),
        ],
    };
    let bytes = answer.encode_to_vec();
    let back = worker::PackLocateResponse::decode(bytes.as_slice()).expect("decode");
    assert_eq!(back, answer);
    assert!(!back.locations[1].located);
    let request = worker::PackLocateRequest {
        digests: vec![digest(&"ab".repeat(32), 12)],
    };
    let bytes = request.encode_to_vec();
    let back = worker::PackLocateRequest::decode(bytes.as_slice()).expect("decode");
    assert_eq!(back, request);
}

#[test]
fn worker_messages_round_trip() {
    let hello = DaemonMessage {
        message: Some(daemon_message::Message::Hello(Hello {
            protocol_version: 1,
            node_id: "node-1".to_owned(),
            daemon_version: "0.1.0".to_owned(),
            capabilities: vec![
                Capability {
                    key: "arch".to_owned(),
                    value: "x86_64".to_owned(),
                },
                Capability {
                    key: "drivers".to_owned(),
                    value: "container".to_owned(),
                },
            ],
            report_hash: vec![7; 32],
            instance_id: "3f9c0a7d5e1b4c2a8d6f0e9b7a5c3d1e".to_owned(),
            phase: Some(NodePhase {
                phase: Phase::Preparing.into(),
                lease_id: Some(LeaseId { term: 2, seq: 8 }),
                reason: String::new(),
            }),
            predecessor_instance_id: "0b1c2d3e4f5a6b7c8d9e0f1a2b3c4d5e".to_owned(),
        })),
    };
    let back = DaemonMessage::decode(hello.encode_to_vec().as_slice()).expect("decode");
    assert_eq!(back, hello);

    let start = ServerMessage {
        message: Some(server_message::Message::Start(Start {
            lease_id: Some(LeaseId { term: 2, seq: 9 }),
            kind: "action".to_owned(),
            action_digest: Some(digest(&"ef".repeat(32), 140)),
            millicpus: 1500,
            memory_bytes: 1 << 30,
            heartbeat_seq: 12,
            valid_for_ms: 14_000,
            device_id: "00008110-000A".to_owned(),
            job: Some(worker::Job {
                run_id: "run-1".to_owned(),
                ..Default::default()
            }),
            fence: worker::FencePolicy::RunOn as i32,
        })),
    };
    let back = ServerMessage::decode(start.encode_to_vec().as_slice()).expect("decode");
    assert_eq!(back, start);

    let cancel = ServerMessage {
        message: Some(server_message::Message::Cancel(worker::Cancel {
            lease_id: Some(LeaseId { term: 2, seq: 8 }),
        })),
    };
    let back = ServerMessage::decode(cancel.encode_to_vec().as_slice()).expect("decode");
    assert_eq!(back, cancel);

    let result = DaemonMessage {
        message: Some(daemon_message::Message::Result(WorkerResult {
            lease_id: Some(LeaseId { term: 2, seq: 9 }),
            status: Some(Status::default()),
            action_result: Some(ActionResult::default()),
            action_digest: Some(Digest {
                hash: "cd".repeat(32),
                size_bytes: 9,
            }),
            memory_kill: worker::MemoryKill::NodePressure as i32,
            lost: worker::Lost::Fenced as i32,
            job_outcome: Some(worker::JobOutcome {
                run_id: "run-1".to_owned(),
                ..Default::default()
            }),
        })),
    };
    let back = DaemonMessage::decode(result.encode_to_vec().as_slice()).expect("decode");
    assert_eq!(back, result);

    let heartbeat = DaemonMessage {
        message: Some(daemon_message::Message::Heartbeat(worker::Heartbeat {
            seq: 11,
            report_hash: vec![7; 32],
            running: vec![LeaseId { term: 2, seq: 9 }],
            pack_reads: Some(worker::PackReadCounts {
                served: 5,
                served_bytes: 4096,
                elsewhere: 2,
                unasked: 1,
                lost: 3,
                damaged: 4,
            }),
            pack_writes: Some(worker::PackWriteCounts {
                packs: 1,
                packed: 2,
                packed_bytes: 3,
                fallback_off: 4,
                fallback_unfit: 5,
                fallback_unsupported: 6,
                fallback_refused: 7,
                fallback_store: 8,
                fallback_record: 9,
                fallback_index: 10,
                missing_indexed: 11,
                missing_unasked: 12,
            }),
            node_load: None,
            input_cache: None,
        })),
    };
    let back = DaemonMessage::decode(heartbeat.encode_to_vec().as_slice()).expect("decode");
    assert_eq!(back, heartbeat);

    let usage = worker::ResourceUsage {
        cpu_user_micros: 1,
        cpu_system_micros: 2,
        peak_memory_bytes: 3,
        wall_micros: 4,
        cgroup: Some(worker::CgroupUsage {
            cpu_usage_micros: Some(5),
            cpu_throttled_periods: Some(0),
            numa: vec![worker::NumaNodeBytes { node: 1, bytes: 6 }],
            ..worker::CgroupUsage::default()
        }),
        ..worker::ResourceUsage::default()
    };
    let back = worker::ResourceUsage::decode(usage.encode_to_vec().as_slice()).expect("decode");
    assert_eq!(back, usage);
}

/// Catches a renumbered `Result.memory_kill` field or `MemoryKill` value: a daemon and
/// a server built from different revisions would read an own-limit kill as a busy
/// node's (or the reverse), and the server would raise the booking for the wrong one.
#[test]
fn memory_kill_keeps_its_wire_numbers() {
    let only = |kill: worker::MemoryKill| WorkerResult {
        memory_kill: kill as i32,
        ..WorkerResult::default()
    };
    // Field 5, varint: tag 5 << 3 = 0x28.
    assert_eq!(
        only(worker::MemoryKill::OwnLimit).encode_to_vec(),
        [0x28, 1]
    );
    assert_eq!(
        only(worker::MemoryKill::NodePressure).encode_to_vec(),
        [0x28, 2]
    );
    assert!(
        only(worker::MemoryKill::Unspecified)
            .encode_to_vec()
            .is_empty()
    );
    assert_eq!(
        worker::MemoryKill::try_from(2),
        Ok(worker::MemoryKill::NodePressure)
    );
}

/// Catches a renumbered `Start.device_id`: the design names field 8
/// (`docs/design/ios-devices.md#54-booking`), and a daemon and a server built from
/// different revisions would read the device from another field, or not at all, and
/// run a device test against no device. Also catches a device id sent when none was
/// booked (an empty id is not on the wire, so an older daemon sees the Start it knew).
#[test]
fn start_device_id_is_field_8() {
    let only = Start {
        device_id: "d1".to_owned(),
        ..Start::default()
    };
    // Field 8, length-delimited: tag 8 << 3 | 2 = 0x42.
    assert_eq!(only.encode_to_vec(), [0x42, 2, b'd', b'1']);
    assert!(Start::default().encode_to_vec().is_empty());
}
