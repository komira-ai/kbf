//! The daemon's side of a `ResultAck`: the reason it names, how that is logged, and the
//! bound on the Results kept until one arrives. The daemon forgets a Result on any ack
//! for its lease, accepted or not, and makes no resend decision from `accepted`: a
//! refused Result (a stale attempt, an unknown lease) is not worth sending again.

use kbf_proto::worker::{ResultAck, ResultAckReason};
use kbf_types::LeaseId;

/// The reason `ack` names; an ack of a server that predates the field, or one that names
/// a reason this daemon does not know, is [`ResultAckReason::Unspecified`]: acknowledged,
/// forget the Result.
pub(crate) fn reason(ack: &ResultAck) -> ResultAckReason {
    ResultAckReason::try_from(ack.reason).unwrap_or(ResultAckReason::Unspecified)
}

/// Logs the acknowledgement of `lease`: a stale attempt or an unknown lease at WARN (the
/// Result was of a lease the server no longer holds), the rest at INFO.
pub(crate) fn log(lease: LeaseId, accepted: bool, reason: ResultAckReason) {
    match reason {
        ResultAckReason::Stale | ResultAckReason::UnknownLease => {
            tracing::warn!(%lease, accepted, ?reason, "result refused by the server; forgotten");
        }
        _ => tracing::info!(%lease, accepted, ?reason, "result acknowledged"),
    }
}
