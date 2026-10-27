//! The cell's messages as trace lines spell them: every field written by hand, in the
//! shapes of `kbf_sim::text`, never through `Debug`.

use kbf_sim::text;

use super::Msg;

/// The message as one trace line.
pub fn msg(msg: &Msg) -> String {
    match msg {
        Msg::Append {
            incarnation,
            record,
        } => format!("append(incarnation={incarnation} {})", text::record(record)),
        Msg::Committed {
            incarnation,
            index,
            record,
        } => format!(
            "committed(incarnation={incarnation} index={index} {})",
            text::record(record)
        ),
        Msg::Hello {
            node,
            instance,
            stream,
            capacity,
        } => format!(
            "hello(node={} instance={} stream={stream} {})",
            text::worker(node),
            text::quoted(instance),
            text::resources(capacity)
        ),
        Msg::Welcome { stream, epoch } => format!("welcome(stream={stream} epoch={epoch})"),
        Msg::Heartbeat {
            stream,
            seq,
            running,
        } => format!(
            "heartbeat(stream={stream} seq={seq} running={})",
            text::list(running, |l| text::lease(*l))
        ),
        Msg::HeartbeatAck { stream, seq } => format!("heartbeat_ack(stream={stream} seq={seq})"),
        Msg::Start {
            stream,
            start,
            heartbeat_seq,
            incarnation,
        } => format!(
            "start(stream={stream} heartbeat_seq={heartbeat_seq} incarnation={incarnation} {})",
            text::start(start)
        ),
        Msg::Report {
            stream,
            lease,
            outcome,
            action,
        } => format!(
            "report(stream={stream} lease={} outcome={} action={})",
            text::lease(*lease),
            text::outcome(*outcome),
            text::optional(action.as_ref(), text::digest)
        ),
        Msg::ResultAck { lease, accepted } => {
            format!(
                "result_ack(lease={} accepted={accepted})",
                text::lease(*lease)
            )
        }
        Msg::Cancel { stream, lease } => {
            format!("cancel(stream={stream} lease={})", text::lease(*lease))
        }
        Msg::Goodbye { stream } => format!("goodbye(stream={stream})"),
    }
}
