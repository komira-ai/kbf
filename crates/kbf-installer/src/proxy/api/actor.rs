//! The one thread that owns the update state. `POST`, `abort`, `rollback` and
//! `force-retire` (and the status read, which also moves a staging update along) are
//! messages on one bounded queue and are handled one at a time, in arrival order, so "is an
//! update open?" and "start one" are never separated by another caller's change.
//!
//! **The actor has a thread of its own.** A call into [`Core`] reads files, fsyncs, and
//! (with the compatibility gate's fleet read) waits on the network. A task that does that
//! on the runtime of its callers takes one of that runtime's workers for as long as it
//! blocks, and on a small host a few stalled calls are every worker. So the actor runs on
//! a thread that serves nothing else, and a caller never waits on it without a limit:
//!
//! - a full queue answers 503 at once;
//! - a call that is not answered within [`ActorTiming::deadline`] answers 503, and if it
//!   had not begun, it never will (the actor drops a call whose caller is gone);
//! - an actor that ended answers 503 to everything, and `on_death` of [`spawn`] has run.
//!
//! The same thread looks at `status.json` every [`ActorTiming::tick`] when no call came,
//! so a staging update moves along with nobody asking.

use std::io;
use std::time::Duration;

use hyper::StatusCode;
use tokio::sync::{mpsc, oneshot};

use super::core::{ApiError, Core, Fault, Reply};
use super::model::UpdateRequest;

type Answer = oneshot::Sender<Result<Reply, ApiError>>;

/// What a caller asks of the actor.
enum Call {
    Post(UpdateRequest),
    Status,
    Abort,
    Rollback,
    ForceRetire(String),
}

/// A call and the place its answer goes.
struct Command {
    call: Call,
    answer: Answer,
}

/// The handle of the actor: cheap to clone, one per request.
#[derive(Clone)]
pub struct Api {
    tx: mpsc::Sender<Command>,
    deadline: Duration,
}

/// Commands waiting for the actor; a full queue answers 503 rather than growing.
pub(super) const QUEUE: usize = 64;

/// How the actor and its callers keep time.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActorTiming {
    /// How long the actor waits, with no call, before it reads `status.json` on its own.
    /// The wait starts again after every call, which is no loss: `POST`, `abort`,
    /// `rollback` and the status read each move a staging update along before they act on
    /// it, so the tick is for the time nobody asks. Zero means as often as the
    /// runtime's timer fires (once a millisecond), not a busy loop.
    pub tick: Duration,
    /// The longest a caller waits for its answer, in the queue and being handled together.
    /// Keep it under the deadline of whoever calls the API, so the caller gets this 503
    /// and not a timeout of its own.
    pub deadline: Duration,
}

impl Default for ActorTiming {
    fn default() -> ActorTiming {
        ActorTiming {
            tick: Duration::from_secs(1),
            deadline: Duration::from_secs(10),
        }
    }
}

/// Runs `on_death` when dropped unless it was taken out: the actor's thread ends in any
/// way but one, and this runs on the way out, unwinding included.
struct DeathWatch<F: FnOnce()>(Option<F>);

impl<F: FnOnce()> Drop for DeathWatch<F> {
    fn drop(&mut self) {
        if let Some(on_death) = self.0.take() {
            tracing::error!("the update API actor stopped without being asked to");
            on_death();
        }
    }
}

/// Starts the actor on a thread of its own (with a runtime for its timer and nothing
/// else).
///
/// `on_death` runs once, on the actor's thread, if the actor ends any way but "every
/// [`Api`] was dropped": a call that found the process dead ([`Fault::Crash`]) or a panic
/// inside one. It is how the process learns that its update API is gone, so it should stop
/// the process; it must not panic (it may run while the thread unwinds). When the actor
/// cannot be started at all, `on_death` is dropped without running.
pub fn spawn(
    core: Core,
    timing: ActorTiming,
    on_death: impl FnOnce() + Send + 'static,
) -> io::Result<Api> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()?;
    let (tx, rx) = mpsc::channel::<Command>(QUEUE);
    let actor = move || {
        let mut watch = DeathWatch(Some(on_death));
        if runtime.block_on(serve(core, rx, timing.tick)) == Exit::Closed {
            watch.0 = None;
        }
    };
    std::thread::Builder::new()
        .name("update-api".to_owned())
        .spawn(actor)?;
    Ok(Api {
        tx,
        deadline: timing.deadline,
    })
}

#[derive(Debug, PartialEq, Eq)]
enum Exit {
    /// Every handle was dropped.
    Closed,
    /// The process died mid-call: no answer, and no more calls.
    Crashed,
}

async fn serve(mut core: Core, mut rx: mpsc::Receiver<Command>, tick: Duration) -> Exit {
    loop {
        let Command { call, answer } = match tokio::time::timeout(tick, rx.recv()).await {
            Ok(Some(command)) => command,
            Ok(None) => return Exit::Closed,
            Err(_) => {
                core.advance();
                continue;
            }
        };
        // The caller gave up while the call waited. It was told 503 and will not read an
        // answer, so a `POST` or an `abort` that nobody can see the end of is not run.
        if answer.is_closed() {
            continue;
        }
        let result = match call {
            Call::Post(req) => core.post(req),
            Call::Status => Ok(core.status()),
            Call::Abort => core.abort(),
            Call::Rollback => core.rollback(),
            Call::ForceRetire(reason) => core.force_retire(&reason),
        };
        match result {
            Ok(reply) => {
                let _ = answer.send(Ok(reply));
            }
            Err(Fault::Api(e)) => {
                let _ = answer.send(Err(e));
            }
            Err(Fault::Crash) => return Exit::Crashed,
        }
    }
}

fn unavailable(message: &str) -> ApiError {
    ApiError {
        status: StatusCode::SERVICE_UNAVAILABLE,
        code: "update_service_unavailable",
        message: message.to_owned(),
        extra: None,
    }
}

fn stopped() -> ApiError {
    unavailable("the update service is not running")
}

impl Api {
    async fn ask(&self, call: Call) -> Result<Reply, ApiError> {
        let (answer, reply) = oneshot::channel();
        self.tx
            .try_send(Command { call, answer })
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => {
                    unavailable("the update service is overloaded")
                }
                mpsc::error::TrySendError::Closed(_) => stopped(),
            })?;
        match tokio::time::timeout(self.deadline, reply).await {
            Ok(Ok(result)) => result,
            // The actor ended with this call in hand or in the queue.
            Ok(Err(_)) => Err(stopped()),
            Err(_) => Err(unavailable("the update service did not answer in time")),
        }
    }

    pub async fn post(&self, req: UpdateRequest) -> Result<Reply, ApiError> {
        self.ask(Call::Post(req)).await
    }

    pub async fn status(&self) -> Result<Reply, ApiError> {
        self.ask(Call::Status).await
    }

    pub async fn abort(&self) -> Result<Reply, ApiError> {
        self.ask(Call::Abort).await
    }

    pub async fn rollback(&self) -> Result<Reply, ApiError> {
        self.ask(Call::Rollback).await
    }

    pub async fn force_retire(&self, reason: String) -> Result<Reply, ApiError> {
        self.ask(Call::ForceRetire(reason)).await
    }
}
