//! The seams U7 and U8 implement: the admin calls on a child, the update-API handler, and
//! the per-slot counts the update status reports.

use std::time::Duration;

use crate::slot::Slot;

/// What a child says about itself on `GET role`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChildRole {
    /// Holds the generation and schedules.
    Active,
    /// Started with `--await-takeover`, not yet taken over.
    Candidate,
    /// Quiesced or stood down: finishing open streams only.
    StandingDown,
}

/// Why an admin call failed.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ControlError {
    /// The child did not answer in time.
    #[error("the child did not answer")]
    Timeout,
    /// The child answered with a refusal.
    #[error("the child refused: {0}")]
    Refused(String),
}

/// The frozen admin contract with a child (`admin_protocol`, design section 10). The
/// proxy calls it on the child's admin port ([`crate::config::UpstreamTable::admin_port`]);
/// U7a implements it with fakes and U7b with the real loopback client. Every call is
/// idempotent.
pub trait ChildControl: Send + Sync {
    /// The candidate takes the generation and starts scheduling.
    fn takeover(&self, slot: Slot) -> impl Future<Output = Result<(), ControlError>> + Send;
    /// The old child stops scheduling, assigning and requeueing, and flushes.
    fn quiesce(&self, slot: Slot) -> impl Future<Output = Result<(), ControlError>> + Send;
    /// The old child ends its worker streams with `UNAVAILABLE "connect again"`.
    fn stand_down(&self, slot: Slot) -> impl Future<Output = Result<(), ControlError>> + Send;
    /// The inverse of quiesce and stand-down: full reload, then re-take the generation.
    fn resume(&self, slot: Slot) -> impl Future<Output = Result<(), ControlError>> + Send;
    /// The child's own view of its role.
    fn role(&self, slot: Slot) -> impl Future<Output = Result<ChildRole, ControlError>> + Send;
}

/// The proxy's count for one slot, for `GET /v1/update/frontend`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SlotStats {
    /// Streams bound to the slot, held-then-released and queued ones included.
    pub open_streams: u64,
    /// Age of the oldest of them, worker sessions included.
    pub oldest_stream_age: Duration,
}

/// A request to `/v1/update/frontend*`, after the proxy authenticated it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateRequest {
    /// `GET` or `POST`.
    pub method: String,
    /// The path, `/v1/update/frontend` and what follows it.
    pub path: String,
    /// The JSON body, at most 4 KiB.
    pub body: Vec<u8>,
}

/// The answer to an [`UpdateRequest`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UpdateReply {
    /// HTTP status.
    pub status: u16,
    /// JSON body.
    pub body: Vec<u8>,
}

/// The pluggable handler of the update API: U8a's actor. The proxy authenticates first
/// ([`crate::auth::decide`] with the update class, the one place that later swaps to OIDC)
/// and rate limits, then calls this.
pub trait UpdateHandler: Send + Sync {
    /// Handles one request.
    fn handle(&self, request: UpdateRequest) -> impl Future<Output = UpdateReply> + Send;
}

/// The reply of the built-in handler until U8 lands: 503 and a JSON error.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoUpdates;

impl UpdateHandler for NoUpdates {
    async fn handle(&self, _request: UpdateRequest) -> UpdateReply {
        UpdateReply {
            status: 503,
            body: br#"{"error":"updates are not wired"}"#.to_vec(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::pin::pin;
    use std::task::{Context, Poll, Waker};

    fn ready<F: Future>(f: F) -> F::Output {
        let mut f = pin!(f);
        match f.as_mut().poll(&mut Context::from_waker(Waker::noop())) {
            Poll::Ready(v) => v,
            Poll::Pending => panic!("the future is not ready"),
        }
    }

    struct Fake;

    impl ChildControl for Fake {
        async fn takeover(&self, _: Slot) -> Result<(), ControlError> {
            Err(ControlError::Timeout)
        }
        async fn quiesce(&self, _: Slot) -> Result<(), ControlError> {
            Ok(())
        }
        async fn stand_down(&self, _: Slot) -> Result<(), ControlError> {
            Ok(())
        }
        async fn resume(&self, _: Slot) -> Result<(), ControlError> {
            Err(ControlError::Refused("holder is alive".into()))
        }
        async fn role(&self, slot: Slot) -> Result<ChildRole, ControlError> {
            Ok(match slot {
                Slot::A => ChildRole::Active,
                Slot::B => ChildRole::Candidate,
            })
        }
    }

    #[test]
    fn the_admin_contract_is_implementable_and_errors_print() {
        let c = Fake;
        assert_eq!(ready(c.takeover(Slot::B)), Err(ControlError::Timeout));
        assert_eq!(ready(c.quiesce(Slot::A)), Ok(()));
        assert_eq!(ready(c.stand_down(Slot::A)), Ok(()));
        let refused = ready(c.resume(Slot::A)).expect_err("refused");
        assert_eq!(refused.to_string(), "the child refused: holder is alive");
        assert_eq!(
            ControlError::Timeout.to_string(),
            "the child did not answer"
        );
        assert_eq!(ready(c.role(Slot::A)), Ok(ChildRole::Active));
        assert_eq!(ready(c.role(Slot::B)), Ok(ChildRole::Candidate));
        assert_ne!(ChildRole::StandingDown, ChildRole::Active);
        assert_eq!(SlotStats::default().open_streams, 0);
    }

    #[test]
    fn until_u8_the_update_api_answers_503() {
        let reply = ready(NoUpdates.handle(UpdateRequest {
            method: "GET".into(),
            path: "/v1/update/frontend".into(),
            body: Vec::new(),
        }));
        assert_eq!(reply.status, 503);
        assert!(reply.body.starts_with(b"{"));
    }
}
