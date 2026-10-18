//! The one task that owns the update state. `POST`, `abort`, `rollback` and
//! `force-retire` (and the status read, which also moves a staging update along) are
//! messages on one channel and are handled one at a time, in arrival order, so "is an
//! update open?" and "start one" are never separated by another caller's change.

use tokio::sync::{mpsc, oneshot};

use super::core::{ApiError, Core, Fault, Reply};
use super::model::UpdateRequest;
use hyper::StatusCode;

type Answer = oneshot::Sender<Result<Reply, ApiError>>;

enum Command {
    Post(UpdateRequest, Answer),
    Status(Answer),
    Abort(Answer),
    Rollback(Answer),
    ForceRetire(String, Answer),
    /// Looks at `status.json` without answering anyone (U8b calls it on a timer).
    Tick,
}

/// The handle of the actor: cheap to clone, one per request.
#[derive(Clone)]
pub struct Api {
    tx: mpsc::Sender<Command>,
}

/// Commands waiting for the actor; a full queue answers 503 rather than growing.
const QUEUE: usize = 64;

/// Starts the actor on the current tokio runtime.
pub fn spawn(mut core: Core) -> Api {
    let (tx, mut rx) = mpsc::channel::<Command>(QUEUE);
    tokio::spawn(async move {
        while let Some(command) = rx.recv().await {
            let (result, answer) = match command {
                Command::Post(req, a) => (core.post(req), a),
                Command::Status(a) => (Ok(core.status()), a),
                Command::Abort(a) => (core.abort(), a),
                Command::Rollback(a) => (core.rollback(), a),
                Command::ForceRetire(reason, a) => (core.force_retire(&reason), a),
                Command::Tick => {
                    core.advance();
                    continue;
                }
            };
            match result {
                Ok(reply) => {
                    let _ = answer.send(Ok(reply));
                }
                Err(Fault::Api(e)) => {
                    let _ = answer.send(Err(e));
                }
                // The process died mid-call: no answer, and no more calls.
                Err(Fault::Crash) => return,
            }
        }
    });
    Api { tx }
}

fn gone() -> ApiError {
    ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        code: "update_service_unavailable",
        message: "the update service is not running or is overloaded".to_owned(),
        extra: None,
    }
}

impl Api {
    async fn ask(&self, make: impl FnOnce(Answer) -> Command) -> Result<Reply, ApiError> {
        let (tx, rx) = oneshot::channel();
        self.tx.try_send(make(tx)).map_err(|_| gone())?;
        rx.await.map_err(|_| gone())?
    }

    pub async fn post(&self, req: UpdateRequest) -> Result<Reply, ApiError> {
        self.ask(|a| Command::Post(req, a)).await
    }

    pub async fn status(&self) -> Result<Reply, ApiError> {
        self.ask(Command::Status).await
    }

    pub async fn abort(&self) -> Result<Reply, ApiError> {
        self.ask(Command::Abort).await
    }

    pub async fn rollback(&self) -> Result<Reply, ApiError> {
        self.ask(Command::Rollback).await
    }

    pub async fn force_retire(&self, reason: String) -> Result<Reply, ApiError> {
        self.ask(|a| Command::ForceRetire(reason, a)).await
    }

    pub fn tick(&self) {
        let _ = self.tx.try_send(Command::Tick);
    }
}
