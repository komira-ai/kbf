//! Why a lease was given up.

use std::fmt;

use kbf_types::LostWhy;

/// Why a lease was given up; the variants are the values 1..=11 of the proto enum
/// `kbf.invocation.v1.RequeueReason`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequeueReason {
    /// Its worker was not heard from for the lease grace.
    Silent,
    /// Another process registered as its worker and does not list the lease as running.
    Replaced,
    /// Its start went to an earlier session of the worker, which registered again and
    /// does not list the lease as running.
    Reconnected,
    /// Its start has been out for the start grace and the worker does not list the lease
    /// as running.
    NotStarted,
    /// Its worker switched the GUI session for it and was silent past the session switch
    /// grace.
    SessionSwitch,
    /// The action passed its own memory limit with `booked` bytes booked, and runs again
    /// with `raised` bytes booked.
    OutOfMemory {
        /// The memory booking of the run that was killed, in bytes.
        booked: u64,
        /// The memory booking of the next run, in bytes.
        raised: u64,
    },
    /// The node killed the lease for memory while the action was under its own limit.
    NodeMemoryPressure,
    /// Another process registered as its worker, naming the process its start went to as
    /// its predecessor.
    Restarted,
    /// The worker reported that the farm lost the run (`why`); it runs again.
    Lost(LostWhy),
    /// The worker reported that the farm could not run the action; it runs again.
    Infra,
}

impl fmt::Display for RequeueReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Silent => "the node was silent past the lease grace",
            Self::Replaced => {
                "another daemon process registered as the node and does not run it \
                 (handover grace over)"
            }
            Self::SessionSwitch => {
                "the node was silent past the session switch grace while it switched the \
                 GUI session for the lease"
            }
            Self::Reconnected => "the daemon reconnected and does not list it as running",
            Self::NotStarted => "the node has not listed it as running within the start grace",
            Self::OutOfMemory { booked, raised } => {
                return write!(
                    f,
                    "the action passed its memory limit with {} booked; it runs again with \
                     {} booked",
                    Gib(*booked),
                    Gib(*raised)
                );
            }
            Self::NodeMemoryPressure => {
                "the node killed it for memory below the action's own limit (node memory \
                 pressure); it runs again with the same booking"
            }
            Self::Restarted => {
                "the daemon restarted, and its successor killed the run before registering"
            }
            Self::Lost(why) => return write!(f, "the farm lost the run ({why}); it runs again"),
            Self::Infra => "the farm could not run the action (infra failure); it runs again",
        })
    }
}

/// A memory size in bytes, written in GiB: whole (`2 GiB`) when it is a whole number of
/// GiB, else to two decimals (`1.50 GiB`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Gib(pub u64);

impl fmt::Display for Gib {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        const GIB: u64 = 1 << 30;
        if self.0.is_multiple_of(GIB) {
            write!(f, "{} GiB", self.0 / GIB)
        } else {
            // Rounded to the nearest hundredth, in integers (no float in a pure crate's
            // output); u128 so no size overflows.
            let hundredths = (u128::from(self.0) * 100 + u128::from(GIB / 2)) / u128::from(GIB);
            write!(f, "{}.{:02} GiB", hundredths / 100, hundredths % 100)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Catches two reasons that read the same in the log (an operator could not tell
    /// a silent node from a restarted daemon), and an empty one.
    #[test]
    fn each_reason_reads_differently() {
        let all = [
            RequeueReason::Silent,
            RequeueReason::Replaced,
            RequeueReason::Reconnected,
            RequeueReason::NotStarted,
            RequeueReason::OutOfMemory {
                booked: 1 << 30,
                raised: 2 << 30,
            },
            RequeueReason::NodeMemoryPressure,
            RequeueReason::SessionSwitch,
            RequeueReason::Restarted,
            RequeueReason::Lost(LostWhy::Fenced),
            RequeueReason::Lost(LostWhy::DaemonRestarted),
            RequeueReason::Infra,
        ];
        let text: Vec<String> = all.iter().map(ToString::to_string).collect();
        for (i, a) in text.iter().enumerate() {
            assert!(!a.is_empty());
            for b in &text[i + 1..] {
                assert_ne!(a, b);
            }
        }
        assert!(text[0].contains("silent"), "{}", text[0]);
        assert!(text[1].contains("another daemon process"), "{}", text[1]);
        assert!(text[2].contains("reconnected"), "{}", text[2]);
        assert!(text[3].contains("start grace"), "{}", text[3]);
        assert!(text[4].contains("with 1 GiB booked"), "{}", text[4]);
        assert!(text[4].contains("with 2 GiB booked"), "{}", text[4]);
        assert!(text[5].contains("same booking"), "{}", text[5]);
        assert!(text[6].contains("session switch grace"), "{}", text[6]);
        assert!(
            text[7].contains("killed the run before registering"),
            "{}",
            text[7]
        );
        assert!(text[8].contains("fenced"), "{}", text[8]);
        assert!(text[9].contains("daemon restarted while"), "{}", text[9]);
    }
}
