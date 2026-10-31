//! `kbf_worker_admission_refused_total{message}` on `GET /metrics`: how many messages the
//! worker listener refused on an open stream, by the kind of message, because the node's
//! certificate no longer admits it (a revoked or denied node) or the deny list could not be
//! read ([`crate::identity::Peers::admit`]). The check runs before the farm sees the
//! message, so a refused message is counted here and nowhere else.
//!
//! Counted per farm, from the start of its process. All six families are always written: a
//! zero is a farm that refused nothing.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicU64, Ordering};

/// The kinds of message a stream is checked for after its first `Hello`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Message {
    /// A `Hello` resent on an open stream.
    Hello,
    Heartbeat,
    Result,
    NodeStatus,
    MapRequest,
    UpdateStatus,
}

impl Message {
    const ALL: [Self; 6] = [
        Self::Hello,
        Self::Heartbeat,
        Self::Result,
        Self::NodeStatus,
        Self::MapRequest,
        Self::UpdateStatus,
    ];

    /// The `message` label value.
    const fn label(self) -> &'static str {
        match self {
            Self::Hello => "hello",
            Self::Heartbeat => "heartbeat",
            Self::Result => "result",
            Self::NodeStatus => "node_status",
            Self::MapRequest => "map_request",
            Self::UpdateStatus => "update_status",
        }
    }
}

/// The refusals counted so far.
#[derive(Debug, Default)]
pub(crate) struct Refusals([AtomicU64; 6]);

impl Refusals {
    /// Counts one refused `message`.
    pub(crate) fn note(&self, message: Message) {
        self.0[message as usize].fetch_add(1, Ordering::Relaxed);
    }

    /// Appends the family to `out`.
    pub(crate) fn render(&self, out: &mut String) {
        out.push_str(
            "# HELP kbf_worker_admission_refused_total Messages the worker listener refused on \
             an open stream, by kind, because the node is no longer admitted\n\
             # TYPE kbf_worker_admission_refused_total counter\n",
        );
        for message in Message::ALL {
            let count = self.0[message as usize].load(Ordering::Relaxed);
            let _ = writeln!(
                out,
                "kbf_worker_admission_refused_total{{message=\"{}\"}} {count}",
                message.label()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each kind is counted under its own label and the others stay 0.
    #[test]
    fn a_refusal_is_counted_under_its_own_message_kind() {
        let refusals = Refusals::default();
        refusals.note(Message::Result);
        refusals.note(Message::Result);
        refusals.note(Message::MapRequest);
        let mut out = String::new();
        refusals.render(&mut out);
        for (label, count) in [
            ("hello", 0),
            ("heartbeat", 0),
            ("result", 2),
            ("node_status", 0),
            ("map_request", 1),
            ("update_status", 0),
        ] {
            let line =
                format!("kbf_worker_admission_refused_total{{message=\"{label}\"}} {count}\n");
            assert!(out.contains(&line), "{label}: {out}");
        }
    }
}
