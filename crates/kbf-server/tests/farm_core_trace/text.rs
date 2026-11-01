//! The words of a scripted session: the actions it submits, how a node reports the end of a
//! lease, and the text each message, result and stage is written as. Shared by every
//! driver of the scripts in `scenarios.rs` (the farm's, in `session.rs`, and the scheduler
//! core's, in `tests/farm_core_adapter`), so that the same step is written in the same
//! words.

use std::collections::BTreeMap;

use kbf_front::{Finished, Stage};
use kbf_proto::google::rpc;
use kbf_proto::reapi::{ActionResult, Digest as ProtoDigest};
use kbf_proto::worker::{self, ServerMessage, server_message};
use kbf_sched::NodePhase;
use kbf_types::{Digest, DigestFunction, LeaseId, LeaseKind, Qos, Resources};
use prost_types::Timestamp;
use tonic::Status;

/// One GiB.
pub const GIB: u64 = 1 << 30;

/// An action the script submits: what it is called in the text, the digest it is keyed
/// by, and how it books.
#[derive(Clone, Debug)]
pub struct Job {
    pub label: &'static str,
    pub tag: u8,
    pub qos: Qos,
    pub kind: LeaseKind,
    pub resources: Resources,
    pub do_not_cache: bool,
}

impl Job {
    /// An action of the `ci` level that books one core and one GiB.
    pub fn new(label: &'static str, tag: u8) -> Self {
        Self {
            label,
            tag,
            qos: Qos::Ci,
            kind: LeaseKind::Action,
            resources: Resources::new(1_000, GIB),
            do_not_cache: false,
        }
    }

    pub fn qos(self, qos: Qos) -> Self {
        Self { qos, ..self }
    }

    pub fn kind(self, kind: LeaseKind) -> Self {
        Self { kind, ..self }
    }

    pub fn cores(self, cores: u64) -> Self {
        let resources = Resources::new(cores * 1_000, self.resources.memory_bytes);
        Self { resources, ..self }
    }

    pub fn gib(self, gib: u64) -> Self {
        let resources = Resources::new(self.resources.cpu_millis, gib * GIB);
        Self { resources, ..self }
    }

    pub fn do_not_cache(self) -> Self {
        Self {
            do_not_cache: true,
            ..self
        }
    }

    pub fn digest(&self) -> Digest {
        digest_of(self.tag)
    }
}

pub fn digest_of(tag: u8) -> Digest {
    Digest::new(DigestFunction::Sha256, [tag; 32], u64::from(tag))
}

/// How a node reports the end of a lease.
#[derive(Clone, Copy, Debug)]
pub enum Ended {
    /// The action ran and exited with this code.
    Ran(i32),
    /// The daemon refused the action as invalid.
    Invalid,
    /// The run passed its own memory limit and was killed.
    OwnLimit,
    /// The node killed the run for memory pressure it was not the cause of.
    NodePressure,
    /// The daemon restarted while the action ran.
    LostToRestart,
    /// The node could not run the action (a full disk).
    Infra,
}

pub fn lease_text(lease: LeaseId) -> String {
    format!("{}/{}", lease.term, lease.seq)
}

pub fn wire_lease_text(lease: Option<&worker::LeaseId>) -> String {
    lease.map_or_else(
        || "-".to_owned(),
        |l| lease_text(LeaseId::new(l.term, l.seq)),
    )
}

pub fn lease_list(leases: &[LeaseId]) -> String {
    let texts: Vec<String> = leases.iter().map(|l| lease_text(*l)).collect();
    texts.join(",")
}

pub fn phase_text(phase: &NodePhase) -> String {
    match phase {
        NodePhase::Serving => "serving".to_owned(),
        NodePhase::Preparing(lease) => format!("preparing({})", lease_text(*lease)),
        NodePhase::Restoring(lease) => format!("restoring({})", lease_text(*lease)),
        NodePhase::Quarantined(why) => format!("quarantined({why:?})"),
        NodePhase::MemoryPressure => "memory_pressure".to_owned(),
        NodePhase::Draining => "draining".to_owned(),
    }
}

pub fn ended_text(ended: Ended) -> String {
    match ended {
        Ended::Ran(code) => format!("ran(exit={code})"),
        Ended::Invalid => "invalid".to_owned(),
        Ended::OwnLimit => "own_limit".to_owned(),
        Ended::NodePressure => "node_pressure".to_owned(),
        Ended::LostToRestart => "lost_to_restart".to_owned(),
        Ended::Infra => "infra".to_owned(),
    }
}

fn status(code: tonic::Code, message: &str) -> Option<rpc::Status> {
    Some(rpc::Status {
        code: code as i32,
        message: message.to_owned(),
        details: Vec::new(),
    })
}

/// The `Result` a node sends for `lease`, which ran `label`.
pub fn result_of(
    lease: LeaseId,
    digest: Option<ProtoDigest>,
    label: &str,
    ended: Ended,
) -> worker::Result {
    let mut result = worker::Result {
        lease_id: Some(worker::LeaseId {
            term: lease.term,
            seq: lease.seq,
        }),
        status: None,
        action_result: None,
        action_digest: digest,
        memory_kill: 0,
        lost: 0,
        job_outcome: None,
    };
    match ended {
        Ended::Ran(code) => {
            result.action_result = Some(ActionResult {
                exit_code: code,
                stdout_raw: format!("output of {label}").into_bytes(),
                ..Default::default()
            });
        }
        Ended::Invalid => result.status = status(tonic::Code::InvalidArgument, "bad input"),
        Ended::OwnLimit => {
            result.status = status(tonic::Code::ResourceExhausted, "killed");
            result.memory_kill = worker::MemoryKill::OwnLimit as i32;
        }
        Ended::NodePressure => {
            result.status = status(tonic::Code::ResourceExhausted, "killed");
            result.memory_kill = worker::MemoryKill::NodePressure as i32;
        }
        Ended::LostToRestart => {
            result.status = status(tonic::Code::Unavailable, "daemon restarted");
            result.lost = worker::Lost::DaemonRestarted as i32;
        }
        Ended::Infra => result.status = status(tonic::Code::Internal, "disk full"),
    }
    result
}

fn action_text(digest: Option<&ProtoDigest>, labels: &BTreeMap<String, &'static str>) -> String {
    digest.map_or_else(
        || "-".to_owned(),
        |d| {
            labels.get(&d.hash).map_or_else(
                || format!("{}:{}", &d.hash[..8], d.size_bytes),
                |label| (*label).to_owned(),
            )
        },
    )
}

pub fn message_text(
    sent: &Result<ServerMessage, Status>,
    labels: &BTreeMap<String, &'static str>,
) -> String {
    let message = match sent {
        Ok(ServerMessage { message: Some(m) }) => m,
        Ok(ServerMessage { message: None }) => return "empty".to_owned(),
        Err(status) => {
            return format!(
                "error code={} message={:?}",
                status.code() as i32,
                status.message()
            );
        }
    };
    match message {
        server_message::Message::Welcome(w) => format!(
            "welcome protocol={} interval_ms={} epoch={} pack_commit={}",
            w.protocol_version, w.heartbeat_interval_ms, w.epoch, w.pack_commit
        ),
        server_message::Message::HeartbeatAck(a) => format!("heartbeat_ack seq={}", a.seq),
        server_message::Message::Start(s) => format!(
            "start lease={} kind={} action={} millicpus={} memory_bytes={} heartbeat_seq={} \
             valid_for_ms={} device={:?} job={} fence={}",
            wire_lease_text(s.lease_id.as_ref()),
            s.kind,
            action_text(s.action_digest.as_ref(), labels),
            s.millicpus,
            s.memory_bytes,
            s.heartbeat_seq,
            s.valid_for_ms,
            s.device_id,
            s.job.is_some(),
            s.fence
        ),
        server_message::Message::LeaseOffer(o) => format!(
            "lease_offer lease={} kind={} action={}",
            wire_lease_text(o.lease_id.as_ref()),
            o.kind,
            action_text(o.action_digest.as_ref(), labels)
        ),
        server_message::Message::ResultAck(a) => format!(
            "result_ack lease={} accepted={} reason={}",
            wire_lease_text(a.lease_id.as_ref()),
            a.accepted,
            worker::ResultAckReason::try_from(a.reason).map_or("?", |r| r.as_str_name())
        ),
        server_message::Message::Cancel(c) => {
            format!("cancel lease={}", wire_lease_text(c.lease_id.as_ref()))
        }
        server_message::Message::MapUpdate(_) => "map_update".to_owned(),
        server_message::Message::UpdateOffer(_) => "update_offer".to_owned(),
        server_message::Message::UpdateOrder(_) => "update_order".to_owned(),
    }
}

fn rpc_status_text(status: &rpc::Status) -> String {
    let details: Vec<&str> = status.details.iter().map(|d| d.type_url.as_str()).collect();
    format!(
        "code={} message={:?} details=[{}]",
        status.code,
        status.message,
        details.join(",")
    )
}

fn ms(stamp: Option<&Timestamp>) -> String {
    stamp.map_or_else(
        || "-".to_owned(),
        |t| (t.seconds * 1_000 + i64::from(t.nanos) / 1_000_000).to_string(),
    )
}

fn result_text(result: &ActionResult) -> String {
    let meta = result.execution_metadata.as_ref();
    format!(
        "exit={} stdout={:?} worker={:?} queued_ms={} started_ms={} completed_ms={}",
        result.exit_code,
        String::from_utf8_lossy(&result.stdout_raw),
        meta.map_or("", |m| m.worker.as_str()),
        ms(meta.and_then(|m| m.queued_timestamp.as_ref())),
        ms(meta.and_then(|m| m.worker_start_timestamp.as_ref())),
        ms(meta.and_then(|m| m.worker_completed_timestamp.as_ref())),
    )
}

pub fn stage_text(stage: &Stage) -> String {
    match stage {
        Stage::Queued => "queued".to_owned(),
        Stage::Waiting(why) => format!("waiting {why:?}"),
        Stage::WaitingKept(why) => format!("waiting_kept {why:?}"),
        Stage::Executing => "executing".to_owned(),
        Stage::Done(Finished::Ran(result)) => format!("done ran {}", result_text(result)),
        Stage::Done(Finished::Failed(status)) => {
            format!("done failed {}", rpc_status_text(status))
        }
        Stage::Done(Finished::TimedOut { status, partial }) => format!(
            "done timed_out {} partial: {}",
            rpc_status_text(status),
            result_text(partial)
        ),
    }
}
