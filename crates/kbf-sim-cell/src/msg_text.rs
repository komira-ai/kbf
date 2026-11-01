//! The cell's messages as trace lines spell them: every field written by hand, in the
//! shapes of [`kbf_sim::text`], never through `Debug`.

use kbf_sim::text;
use kbf_types::Digest;

use crate::{Body, Msg};

/// A content digest: `sha256:<64 hex>:<size>`. The function's name comes from
/// [`kbf_types::DigestFunction::name`], whose match has no wildcard, so a second hash
/// function gets a name where it is defined.
fn spell_digest(digest: &Digest) -> String {
    format!(
        "{}:{}:{}",
        digest.function.name(),
        digest.hash_hex(),
        digest.size_bytes
    )
}

fn body(body: &Body) -> String {
    let blobs = text::list(&body.blobs, |(digest, offset)| {
        format!("{}@{offset}", spell_digest(digest))
    });
    format!("body(blobs={blobs} bytes={})", body.bytes.len())
}

impl Msg {
    /// The message as one trace line.
    pub(crate) fn trace_text(&self) -> String {
        match self {
            Self::Upload { digest } => format!("upload({})", spell_digest(digest)),
            Self::Uploaded { digest } => format!("uploaded({})", spell_digest(digest)),
            Self::UploadFailed { digest } => format!("upload_failed({})", spell_digest(digest)),
            Self::FindMissing { digests } => {
                format!("find_missing({})", text::list(digests, spell_digest))
            }
            Self::Missing { asked, missing } => format!(
                "missing(asked={} missing={})",
                text::list(asked, spell_digest),
                text::list(missing, spell_digest)
            ),
            Self::Execute { action } => format!("execute({})", spell_digest(action)),
            Self::Answered { action, cached } => {
                format!("answered({} cached={cached})", spell_digest(action))
            }
            Self::Put { object, digest } => format!("put({object} {})", spell_digest(digest)),
            Self::PutDone { object, digest } => {
                format!("put_done({object} {})", spell_digest(digest))
            }
            Self::PutFailed { object, digest } => {
                format!("put_failed({object} {})", spell_digest(digest))
            }
            Self::Flush { object, body: b } => format!("flush({object} {})", body(b)),
            Self::FlushDone { object } => format!("flush_done({object})"),
            Self::FlushFailed { object } => format!("flush_failed({object})"),
            Self::Get {
                object,
                digest,
                tag,
            } => format!("get({object} {} tag={tag})", spell_digest(digest)),
            Self::Got { tag, held } => format!("got(tag={tag} held={held})"),
            Self::Hello {
                stream,
                instance,
                capacity,
            } => format!(
                "hello(stream={stream} instance={} {})",
                text::quoted(instance),
                text::resources(capacity)
            ),
            Self::Welcome {
                stream,
                epoch,
                interval_ms,
            } => format!("welcome(stream={stream} epoch={epoch} interval_ms={interval_ms})"),
            Self::Heartbeat {
                stream,
                seq,
                running,
            } => format!(
                "heartbeat(stream={stream} seq={seq} running={})",
                text::list(running, |l| text::lease(*l))
            ),
            Self::HeartbeatAck { stream, seq } => {
                format!("heartbeat_ack(stream={stream} seq={seq})")
            }
            Self::Start {
                stream,
                lease,
                operation,
                action,
            } => format!(
                "start(stream={stream} lease={} op={} action={})",
                text::lease(*lease),
                text::operation(*operation),
                spell_digest(action)
            ),
            Self::Report {
                stream,
                lease,
                action,
                outcome,
            } => format!(
                "report(stream={stream} lease={} action={} outcome={})",
                text::lease(*lease),
                spell_digest(action),
                text::outcome(*outcome)
            ),
            Self::ResultAck { lease, accepted } => {
                format!(
                    "result_ack(lease={} accepted={accepted})",
                    text::lease(*lease)
                )
            }
            Self::Cancel { stream, lease } => {
                format!("cancel(stream={stream} lease={})", text::lease(*lease))
            }
            Self::Reset { stream } => format!("reset(stream={stream})"),
        }
    }
}

#[cfg(test)]
mod tests {
    use kbf_meta::{Epoch, ObjectId};
    use kbf_types::{Failure, LeaseId, OperationId, Outcome, Resources};

    use super::*;
    use crate::action;

    /// Catches: a message kind missing from the trace text, two kinds sharing a
    /// spelling, or a field dropped from a line (two runs that differ only in that
    /// field would share a hash). Each expected line is written out in full.
    #[test]
    fn every_message_has_its_fixed_spelling() {
        let d = action(1);
        let digest_text = format!(
            "sha256:ac{}0000000000000001{}:2",
            "00".repeat(7),
            "00".repeat(16)
        );
        assert_eq!(spell_digest(&d), digest_text);
        let object = ObjectId::new(Epoch::new(2), 3);
        let lease = LeaseId { term: 1, seq: 4 };
        let obj = object.to_string();
        let body_value = Body {
            blobs: vec![(d, 0)],
            bytes: vec![1, 2, 3],
        };
        let cases: Vec<(Msg, String)> = vec![
            (Msg::Upload { digest: d }, format!("upload({digest_text})")),
            (
                Msg::Uploaded { digest: d },
                format!("uploaded({digest_text})"),
            ),
            (
                Msg::UploadFailed { digest: d },
                format!("upload_failed({digest_text})"),
            ),
            (
                Msg::FindMissing {
                    digests: vec![d, d],
                },
                format!("find_missing([{digest_text}; {digest_text}])"),
            ),
            (
                Msg::Missing {
                    asked: vec![d],
                    missing: vec![],
                },
                format!("missing(asked=[{digest_text}] missing=[])"),
            ),
            (
                Msg::Execute { action: d },
                format!("execute({digest_text})"),
            ),
            (
                Msg::Answered {
                    action: d,
                    cached: true,
                },
                format!("answered({digest_text} cached=true)"),
            ),
            (
                Msg::Put { object, digest: d },
                format!("put({obj} {digest_text})"),
            ),
            (
                Msg::PutDone { object, digest: d },
                format!("put_done({obj} {digest_text})"),
            ),
            (
                Msg::PutFailed { object, digest: d },
                format!("put_failed({obj} {digest_text})"),
            ),
            (
                Msg::Flush {
                    object,
                    body: body_value,
                },
                format!("flush({obj} body(blobs=[{digest_text}@0] bytes=3))"),
            ),
            (Msg::FlushDone { object }, format!("flush_done({obj})")),
            (Msg::FlushFailed { object }, format!("flush_failed({obj})")),
            (
                Msg::Get {
                    object,
                    digest: d,
                    tag: 9,
                },
                format!("get({obj} {digest_text} tag=9)"),
            ),
            (
                Msg::Got {
                    tag: 9,
                    held: false,
                },
                "got(tag=9 held=false)".to_owned(),
            ),
            (
                Msg::Hello {
                    stream: 1,
                    instance: "d-1".to_owned(),
                    capacity: Resources::new(1_000, 2),
                },
                "hello(stream=1 instance=\"d-1\" res(cpu=1000 mem=2 gpus=0 vms=0 leases=-))"
                    .to_owned(),
            ),
            (
                Msg::Welcome {
                    stream: 1,
                    epoch: 2,
                    interval_ms: 3,
                },
                "welcome(stream=1 epoch=2 interval_ms=3)".to_owned(),
            ),
            (
                Msg::Heartbeat {
                    stream: 1,
                    seq: 2,
                    running: vec![lease],
                },
                "heartbeat(stream=1 seq=2 running=[1.4])".to_owned(),
            ),
            (
                Msg::HeartbeatAck { stream: 1, seq: 2 },
                "heartbeat_ack(stream=1 seq=2)".to_owned(),
            ),
            (
                Msg::Start {
                    stream: 1,
                    lease,
                    operation: OperationId(5),
                    action: d,
                },
                format!("start(stream=1 lease=1.4 op=op5 action={digest_text})"),
            ),
            (
                Msg::Report {
                    stream: 1,
                    lease,
                    action: d,
                    outcome: Outcome::Failed(Failure::Timeout),
                },
                format!("report(stream=1 lease=1.4 action={digest_text} outcome=failed(timeout))"),
            ),
            (
                Msg::ResultAck {
                    lease,
                    accepted: true,
                },
                "result_ack(lease=1.4 accepted=true)".to_owned(),
            ),
            (
                Msg::Cancel { stream: 1, lease },
                "cancel(stream=1 lease=1.4)".to_owned(),
            ),
            (Msg::Reset { stream: 7 }, "reset(stream=7)".to_owned()),
        ];
        assert_eq!(cases.len(), 24, "one case per message kind");
        for (msg, expected) in &cases {
            assert_eq!(&msg.trace_text(), expected);
        }
    }
}
