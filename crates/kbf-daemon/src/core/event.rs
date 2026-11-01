//! What the daemon tells an observer.

use std::time::Duration;

use kbf_proto::worker;
use kbf_types::LeaseId;

use crate::phases::LeasePhases;

/// Something the daemon did that an observer (a test, later the meter) may want.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Event {
    /// A stream connected and its Hello went out; Welcome is awaited.
    HelloSent,
    /// A session began: the server answered Hello.
    Welcomed { heartbeat_interval: Duration },
    /// The server placed a lease here; nothing runs until its Start.
    Offered(LeaseId),
    /// A Start arrived after the window it names, or names a heartbeat this stream
    /// did not send: the lease was not run, and no Result is sent (issue #23).
    StartExpired(LeaseId),
    /// The server cancelled a running lease; its kill has begun.
    Cancelled(LeaseId),
    /// The server decided a Result: `accepted` says whether it became the operation's
    /// result, `reason` why (unspecified from a server that predates it). Either way the
    /// daemon forgets the Result: it is neither resent nor listed in heartbeats any more.
    ResultAcknowledged {
        lease: LeaseId,
        accepted: bool,
        reason: worker::ResultAckReason,
    },
    /// No heartbeat sent in the last two intervals has been acknowledged.
    HeartbeatGap { silent_for: Duration },
    /// An acknowledgement arrived after a gap.
    ContactRestored,
    /// Contact was lost for the fence time; these leases are being killed. Each
    /// reports, fenced (lost), once its run has stopped.
    Fenced(Vec<LeaseId>),
    /// A Welcome named another lease epoch: these leases, granted under an earlier
    /// one, are dropped. Their runs are being killed, and no Result of them is sent.
    Superseded(Vec<LeaseId>),
    /// A welcomed session ended, with the reason.
    Disconnected(String),
    /// A lease's run ended (its Result is being sent): where its time went, as the
    /// `lease phases:` line logs it.
    LeaseEnded(LeasePhases),
    /// An attempt to connect ended before the server's Welcome (a refused connection, a
    /// TLS failure, a server answering `UNAVAILABLE`, ...), or a server's name resolved
    /// to no address. `server` names the URL, and the address when there was one.
    ConnectFailed { server: String, reason: String },
    /// Every address of a round of attempts failed: the daemon waits `wait` and starts
    /// the next round. `failed_rounds` counts the rounds since the last session.
    Retrying { failed_rounds: u32, wait: Duration },
}
