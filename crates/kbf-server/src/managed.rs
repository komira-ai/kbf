//! A frontend that runs under the admin port (`docs/design/frontend-proxy.md`, sections
//! 5 and 10): `--admin-listen` serves the calls of [`http`], and `--await-takeover` starts
//! the process as a candidate.
//!
//! **A candidate** opens the state store without taking the generation and without
//! touching the epoch, binds the REAPI and operator API listeners, and serves only
//! `/readyz`, the health service and `GetCapabilities` ([`candidate`]). It binds no worker
//! listener and runs no scheduler or reconciler. It makes and changes no schema: the
//! backend's open refuses a store whose schema version this build does not support.
//!
//! **`takeover`** takes the generation, which the store must be at exactly as the call
//! says (`expect`: what the holder's `role` reports), keeps the epoch, and builds the
//! frontend over the store as any start does: all state is read from the store, and the
//! scheduler, the reconciler and the worker listener start, on the sockets the candidate
//! already bound. A start with no `--await-takeover` is a cold start and bumps the epoch.
//!
//! **A SQLite state file** is held by one process (an exclusive file lock), so a candidate
//! cannot be opened beside the process that holds it: running side by side needs the
//! Postgres store.
//!
//! **`quiesce`** fences the farm ([`crate::farm::Farm::fence`]): it feeds the scheduler
//! nothing, so nothing is granted, assigned or requeued, and a call that would queue is
//! refused `UNAVAILABLE`. A heartbeat or result that reaches a quiesced farm is not applied
//! either, so a quiesce is meant to last seconds (the takeover's limit is 10): one longer
//! than the leases' expiry lets the scheduler expire the leases of work that is still
//! running when the farm resumes, and requeue it. Nothing bounds it. **`resume`** undoes it while the store is still at the
//! generation this process took. **`stand-down`** is the old holder's last step: it ends
//! the worker streams, stops the worker listener, goes not ready and answers the calls it
//! kept from `done/` rows ([`crate::farm::Farm::follow`]), with no GOAWAY. It never
//! gives up the store: another writer holds it already.

mod candidate;
mod control;
pub(crate) mod flags;
mod http;
mod instance;

use std::error::Error;
use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use kbf_state::Store;
use tokio::sync::oneshot;

pub(crate) use candidate::{Candidate, Sockets};
pub use control::{ADMIN_PROTOCOL, Control, Refusal, Role, RoleView};
pub use flags::AdminFlags;

use crate::frontend::{FrontendFlags, FrontendHandle};
use crate::handover::Stop;
use crate::listen::Binder;
use crate::serve::{AbortOnDrop, ServeError, bind_api, serve_api};
use crate::start::Escalation;
use crate::state_store;

/// How long a frontend run under the admin port waits for the calls between their request
/// and their queueing before it quiesces anyway (the handover's default).
pub const QUIESCE_WAIT: Duration = Duration::from_secs(10);

/// Where a managed frontend gets its state store.
pub enum StoreSource {
    /// Opened from `--state-store`.
    Flags,
    /// An open store, shared with other processes of a test (the SQLite file cannot be
    /// opened twice; an in-memory one can).
    Shared(Arc<dyn Store>),
}

/// A frontend, as a candidate or as the writer, with its admin port serving.
pub struct Managed {
    control: Arc<Control>,
    stop: Stop,
    admin: SocketAddr,
    running: Option<FrontendHandle>,
    takeover: oneshot::Receiver<Result<Box<FrontendHandle>, String>>,
    escalation: Option<Escalation>,
    notify: Option<std::ffi::OsString>,
    _admin_task: AbortOnDrop,
}

impl Managed {
    /// Starts as the flags say: a candidate with `--await-takeover`, otherwise a frontend
    /// that takes the generation as a start does (a cold start, which bumps the epoch).
    /// `shutdown` completing stops it.
    ///
    /// # Errors
    /// No `--admin-listen`, a start that is refused (see [`crate::frontend::start_frontend`]),
    /// or a store that cannot be opened.
    pub async fn start<F>(
        flags: &FrontendFlags,
        source: StoreSource,
        shutdown: F,
    ) -> Result<Self, Box<dyn Error>>
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let admin = flags
            .admin
            .ok_or("--admin-listen is needed to run under the admin port")?;
        if !crate::config::loopback_only(admin.listen.ip()) {
            return Err(format!("{} is not a loopback address", admin.listen).into());
        }
        let stop = Stop::new();
        let forward = stop.clone();
        let forwarding = tokio::spawn(async move {
            shutdown.await;
            forward.fire();
        });
        let (listener, bound) = bind_api(admin.listen, &Binder::new())?;
        let (handoff, takeover) = oneshot::channel();
        let control = Arc::new(Control::new(
            instance::new_id()?,
            flags.clone(),
            stop.clone(),
            handoff,
        ));
        let mut running = None;
        if admin.await_takeover {
            let candidate = match source {
                StoreSource::Flags => state_uri(flags)?.open_candidate()?,
                StoreSource::Shared(store) => state_store::Candidate::over(store),
            };
            control.set_candidate(Candidate::start(flags, candidate, &stop)?);
        } else {
            let handle = match source {
                StoreSource::Flags => {
                    let stop = stop.clone();
                    crate::frontend::start_or_take_over(flags, move || Ok(stop.fired())).await?
                }
                StoreSource::Shared(store) => {
                    let state = crate::StateStore {
                        store: Arc::new(kbf_state::Fenced::take(store, None)?),
                        epoch: crate::farm::EpochPolicy::Bump,
                        members: crate::members::MembershipConfig::default(),
                        history: None,
                    };
                    crate::frontend::start_over(flags, state, &stop)?
                }
            };
            control.set_running(&handle);
            running = Some(handle);
        }
        let routes = http::router(Arc::clone(&control));
        let stopped = stop.fired();
        let served = tokio::spawn(async move {
            // The admin port ends with the process; a listener that fails shows as calls
            // that are refused.
            let _ = serve_api(Some(listener), routes, stopped).await;
        });
        // The forwarder ends with the signal; it holds nothing else.
        drop(forwarding);
        Ok(Self {
            control,
            stop,
            admin: bound,
            running,
            takeover,
            escalation: None,
            notify: None,
            _admin_task: AbortOnDrop(served.abort_handle()),
        })
    }

    /// Where the admin port listens.
    #[must_use]
    pub const fn admin(&self) -> SocketAddr {
        self.admin
    }

    /// The role, as `GET role` answers it.
    #[must_use]
    pub fn view(&self) -> RoleView {
        self.control.view()
    }

    /// The line printed once the admin port is bound.
    #[must_use]
    pub fn start_line(&self) -> String {
        let view = self.view();
        format!(
            "kbf-frontend-admin admin={} instance={} role={} admin_protocol={}",
            self.admin,
            view.instance_id,
            serde_json::to_string(&view.role)
                .unwrap_or_default()
                .trim_matches('"'),
            view.admin_protocol
        )
    }

    /// With the escalation (the second stop signal) the served frontend closes its
    /// streams on.
    #[must_use]
    pub fn escalating(mut self, second: impl Future<Output = ()> + Send + 'static) -> Self {
        self.escalation = Some(Box::pin(second));
        self
    }

    /// With `READY=1` sent to `target` once the served frontend's probes pass.
    #[must_use]
    pub fn notifying(mut self, target: Option<std::ffi::OsString>) -> Self {
        self.notify = target;
        self
    }

    /// Serves until the shutdown future completes: a candidate waits for its takeover
    /// first (and ends without one if told to stop).
    ///
    /// # Errors
    /// A listener fails, or a takeover took the generation and the frontend could not
    /// start.
    pub async fn serve(mut self) -> Result<(), Box<dyn Error + Send + Sync>> {
        let handle = match self.running.take() {
            Some(handle) => handle,
            None => {
                tokio::select! {
                    // The sender lives in the control this process holds: it is never
                    // dropped before the answer.
                    taken = &mut self.takeover => match taken? {
                        Ok(handle) => *handle,
                        Err(why) => return Err(why.into()),
                    },
                    () = self.stop.fired() => return Ok(()),
                }
            }
        };
        let mut handle = handle.notifying(self.notify.take());
        handle.escalation = self.escalation.take();
        let served = handle.serve().await;
        self.stop.fire();
        served.map_err(|error: ServeError| error.into())
    }
}

fn state_uri(flags: &FrontendFlags) -> Result<&crate::StateStoreUri, Box<dyn Error>> {
    flags
        .server
        .state_store
        .as_ref()
        .ok_or_else(|| "--admin-listen needs --state-store".into())
}
