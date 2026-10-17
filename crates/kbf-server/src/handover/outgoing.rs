//! The frontend that is serving, when another asks to take over (see [`super`]).

use std::net::SocketAddr;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures::future::AbortHandle;
use kbf_front::Successor;
use kbf_state::{GENERATION, Store, StoreError};
use tokio::sync::{oneshot, watch};
use tonic::Status;
use tonic::transport::Channel;

use super::wire::{Link, Message, PROTOCOL};
use super::{HandoverError, Instance, Stop};
use crate::drain::Drain;
use crate::farm::Farm;

/// How often a holder looks again for the work that keeps it from standing down.
const QUIET_LOOK: Duration = Duration::from_millis(10);

/// What the holder tells a daemon whose stream it ends.
const STANDING_DOWN: &str = "this frontend handed its work to a new process; connect again";

/// What a holder serves, which a successor must be started with to take it over.
#[derive(Clone, Debug)]
pub(crate) struct Facts {
    /// Which instance this is; `None` if it was started without `--instance`.
    pub instance: Option<Instance>,
    pub state: PathBuf,
}

/// Everything a holder stops to let go, in the order it stops it.
pub(crate) struct Stand {
    pub drain: Arc<Drain>,
    pub farm: Arc<Farm>,
    /// Stops the worker listener.
    pub worker: Stop,
    /// Ends the task that expires the state store's rows.
    pub sweeper: AbortHandle,
    pub store: Arc<dyn Store>,
    /// The longest to wait for the farm to be [quiet](Farm::quiet).
    pub wait: Duration,
}

impl Stand {
    /// Stands down: see the module docs of [`super`], step 3. The listeners that answer
    /// Envoy (REAPI, operator API) are not stopped, and readiness is not touched: the
    /// holder stays ready until its successor has taken Envoy's streams.
    ///
    /// # Errors
    /// The state store cannot give up its writer role. The farm is fenced and the worker
    /// listener is stopped by then, so the drain is closed: the process only has its open
    /// streams to end.
    pub async fn run(&self) -> Result<(), StoreError> {
        tracing::info!("standing down for a successor");
        self.drain.hand_over();
        let _ = tokio::time::timeout(self.wait, self.until_quiet()).await;
        self.farm.fence();
        self.sweeper.abort();
        self.worker.fire();
        self.farm
            .end_worker_streams(&Status::unavailable(STANDING_DOWN));
        let demoted = self.store.demote();
        if demoted.is_err() {
            self.drain.close();
        }
        demoted
    }

    async fn until_quiet(&self) {
        while !self.farm.quiet() {
            tokio::time::sleep(QUIET_LOOK).await;
        }
    }
}

/// The channel to a successor whose REAPI listener is at an address, or why it cannot be
/// made.
pub(crate) type Dial = Box<
    dyn Fn(SocketAddr) -> Result<Channel, Box<dyn std::error::Error + Send + Sync>> + Send + Sync,
>;

/// How a holder reaches the successor it forwards calls to.
pub(crate) struct Peer {
    /// Where the relay of this frontend sends its calls, named when a successor is
    /// accepted.
    pub successor: Successor,
    /// The channel to a successor whose REAPI listener is at the address; the reason it
    /// cannot be made (a certificate that needs a name this frontend was not given).
    pub dial: Dial,
}

/// What a holder does once it has released, and how long it waits at each step.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Leave {
    /// How long after `released` it waits for the successor to say it serves.
    pub watchdog: Duration,
    /// Envoy's settle ([`super::EnvoyCheck::settle`]).
    pub settle: Duration,
    /// How often it looks in the state store for its callers' results.
    pub every: Duration,
}

/// The control socket of a serving frontend, and what it needs to hand over.
pub(crate) struct Holder {
    listener: tokio::net::UnixListener,
    /// The only user whose processes may ask (the one this runs as).
    uid: u32,
    facts: Facts,
    stand: Stand,
    peer: Peer,
    leave: Leave,
}

impl Holder {
    /// Binds the control socket at `path`, mode 0600, replacing a socket file left by a
    /// frontend that died (the caller holds the state store, so none is serving on it).
    ///
    /// # Errors
    /// The socket cannot be made.
    pub fn bind(
        path: &Path,
        uid: u32,
        facts: Facts,
        stand: Stand,
        peer: Peer,
        leave: Leave,
    ) -> std::io::Result<Self> {
        let _ = std::fs::remove_file(path);
        let listener = std::os::unix::net::UnixListener::bind(path)?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener: tokio::net::UnixListener::from_std(listener)?,
            uid,
            facts,
            stand,
            peer,
            leave,
        })
    }

    /// Serves the control socket until a successor has taken over, then leaves Envoy's
    /// rotation and answers the callers whose streams it kept open (see
    /// [`Leaving::run`]). `departed` is told how it ended: `Ok` when the process holds
    /// nothing and may exit, `Err` when it could not stand down. A failure is also
    /// logged.
    pub async fn run(self, departed: oneshot::Sender<Result<(), String>>) {
        let handed_over = self.hand_over().await;
        // The control socket goes: a successor that dies can be started again and finds
        // nobody serving on it.
        let Self {
            listener,
            stand,
            leave,
            ..
        } = self;
        drop(listener);
        let outcome = match handed_over {
            Ok(link) => {
                let leaving = Leaving {
                    drain: stand.drain,
                    farm: stand.farm,
                    leave,
                };
                leaving.run(link).await;
                Ok(())
            }
            Err(error) => {
                tracing::error!(%error, "the handover failed");
                Err(error.to_string())
            }
        };
        // The receiver is gone only when the process is already stopping.
        let _ = departed.send(outcome);
    }

    /// Takes connections one at a time until one of them takes over; the connection of
    /// that one, after `released`.
    ///
    /// # Errors
    /// The control socket fails, or the state store cannot be given up.
    async fn hand_over(&self) -> Result<Link, HandoverError> {
        loop {
            let (stream, _) = self.listener.accept().await?;
            let trusted = stream.peer_cred().is_ok_and(|peer| peer.uid() == self.uid);
            if !trusted {
                tracing::warn!("a handover was asked for by another user: refused");
                continue;
            }
            let mut link = Link::new(stream);
            match self.negotiate(&mut link).await {
                Ok(Some(channel)) => {
                    self.peer.successor.set(channel);
                    return self.release(link).await;
                }
                other => {
                    let outcome = other.as_ref().map(Option::is_some);
                    tracing::info!(?outcome, "a handover was asked for and did not happen");
                }
            }
        }
    }

    /// The conversation up to `ready`: the channel to the successor if it is ready to
    /// take over. Until it is, nothing about this frontend has changed.
    async fn negotiate(&self, link: &mut Link) -> Result<Option<Channel>, HandoverError> {
        let hello = link.hear().await?;
        let channel = match self.accept(&hello) {
            Ok(channel) => channel,
            Err(why) => {
                link.say(&Message::Refused(why)).await?;
                return Ok(None);
            }
        };
        let offer = Message::Offer {
            generation: self.generation()?,
        };
        link.say(&offer).await?;
        Ok((link.hear().await? == Message::Ready).then_some(channel))
    }

    /// The channel to the process that sent `hello`, or why this frontend will not hand
    /// over to it.
    fn accept(&self, hello: &Message) -> Result<Channel, String> {
        let Message::Hello {
            version,
            instance,
            reapi,
            state,
        } = hello
        else {
            return Err("the first message must be hello".to_owned());
        };
        let facts = &self.facts;
        if *version != PROTOCOL {
            return Err(format!(
                "this frontend speaks handover protocol {PROTOCOL}, not {version}"
            ));
        }
        match (facts.instance, instance) {
            (None, _) => return Err("this frontend was started without --instance".to_owned()),
            (_, None) => return Err("the successor was started without --instance".to_owned()),
            (Some(ours), Some(theirs)) if ours == *theirs => {
                return Err(format!(
                    "the successor is instance {} like this frontend: a handover goes to the \
                     other instance, which listens on its own ports",
                    ours.letter()
                ));
            }
            _ => {}
        }
        if *state != facts.state {
            return Err("the successor's --state-store is not this frontend's".to_owned());
        }
        if !reapi.ip().is_loopback() {
            return Err(format!(
                "the successor's REAPI address {reapi} is not loopback: calls that still \
                 reach this frontend, with their credentials, would be sent there"
            ));
        }
        (self.peer.dial)(*reapi).map_err(|error| error.to_string())
    }

    /// The generation the state store is at, which the successor must find.
    fn generation(&self) -> Result<u64, StoreError> {
        let row = self.stand.store.get(GENERATION)?;
        let bytes = row.and_then(|row| <[u8; 8]>::try_from(row.value.as_slice()).ok());
        Ok(bytes.map_or(0, u64::from_be_bytes))
    }

    /// The successor is ready: stands down, and says so. From here this frontend does not
    /// go back, whatever the successor does. The connection stays open for the
    /// successor's `serving`.
    async fn release(&self, mut link: Link) -> Result<Link, HandoverError> {
        let stood = self.stand.run().await;
        tracing::info!(stood = stood.is_ok(), "handed over to the successor");
        let said = match &stood {
            Ok(()) => Message::Released,
            Err(error) => Message::Failed(error.to_string()),
        };
        // The successor may be gone already; the rows are what it leaves behind.
        let _ = link.say(&said).await;
        stood?;
        Ok(link)
    }
}

/// What a holder does between `released` and its exit.
struct Leaving {
    drain: Arc<Drain>,
    farm: Arc<Farm>,
    leave: Leave,
}

impl Leaving {
    /// Waits for the successor to say it serves (up to the watchdog) and then for the
    /// settle, and goes not ready: Envoy sends it no new stream from then on. A successor
    /// that never says it serves, or whose connection closes, makes the holder go not
    /// ready at once or when the watchdog runs out, so Envoy has a clean answer to give.
    /// Meanwhile, and until the holder holds nothing, it answers its callers from the
    /// state store. Returns when it has been out of Envoy's rotation for the settle and
    /// holds no stream, no relay and no caller.
    async fn run(self, mut link: Link) {
        let (left, left_for_good) = watch::channel(false);
        let leaving = async {
            let heard = tokio::time::timeout(self.leave.watchdog, link.hear()).await;
            match heard {
                Ok(Ok(Message::Serving)) => tokio::time::sleep(self.leave.settle).await,
                Ok(Ok(other)) => {
                    tracing::warn!(said = %other.line(), "the successor said something else than serving");
                }
                Ok(Err(error)) => {
                    tracing::warn!(%error, "the successor went away before it served");
                }
                Err(_) => {
                    let watchdog = self.leave.watchdog;
                    tracing::warn!(?watchdog, "the successor does not serve: unready");
                }
            }
            self.drain.unready();
            tracing::info!("not ready: Envoy sends this frontend no new stream");
            // Envoy stops choosing a host within the settle of its first failed check.
            tokio::time::sleep(self.leave.settle).await;
            left.send_replace(true);
        };
        let following = async {
            let mut look = tokio::time::interval(self.leave.every);
            loop {
                look.tick().await;
                let waiting = match self.farm.follow() {
                    Ok(waiting) => waiting,
                    Err(error) => {
                        tracing::warn!(%error, "the successor's results could not be read");
                        continue;
                    }
                };
                let waiting = u64::try_from(waiting).unwrap_or(u64::MAX);
                let holding = waiting
                    .saturating_add(self.farm.open_executions())
                    .saturating_add(self.farm.relays_open());
                if holding == 0 && *left_for_good.borrow() {
                    return;
                }
            }
        };
        tokio::join!(leaving, following);
    }
}

#[cfg(test)]
mod tests;
