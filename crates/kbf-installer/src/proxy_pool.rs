//! Pool mode of `kbf-installer proxy`: the proxy as a load balancer over frontends that run on
//! their own (`[[frontend]]` tables of the `[proxy]` table), with nothing to supervise.
//!
//! The verb binds the runtime to the activated socket and serves until SIGTERM or SIGINT, and
//! SIGHUP re-reads the certificate files (`proxy_verb::handle_signals`).
//! There is no child to start or adopt, no migration, no handover and no update API behind
//! `/v1/update/frontend` (it answers 503): a frontend is updated by draining it through the
//! admin API (`admin_listen`), restarting it and putting it back, by whoever updates it. A stop
//! drains the proxy's own open streams, ends the process within the stop bound of the first
//! signal (at once on a second), and leaves the frontends running.

use std::io::{self, Write as _};
use std::os::fd::OwnedFd;
use std::sync::Arc;

use kbf_proxy::Proxy;
use kbf_proxy::config::ProxyConfig;
use kbf_proxy::control::NoUpdates;

use crate::proxy_verb::{Stop, handle_signals};

/// Binds the proxy of `config` to `listener` and serves until a stop signal. The signal
/// handlers are registered before it serves, so none is lost to the default action;
/// `handlers_ready` is called then (the verb lets a SIGHUP it held pending through there).
///
/// # Errors
/// The proxy could not be bound (its message starts with "cannot start the proxy"), or a
/// signal handler could not be registered.
pub(crate) fn run(
    config: ProxyConfig,
    listener: OwnedFd,
    handlers_ready: impl FnOnce(),
) -> io::Result<()> {
    let members = config.frontend.len();
    let (handle, runner) = Proxy::bind(config, listener)
        .map_err(|e| io::Error::other(format!("cannot start the proxy: {e}")))?;
    // One stop drains and ends the process within the bound, a second ends it at once, and
    // SIGHUP reloads the TLS files.
    let stop = Stop::production();
    let drain: Arc<dyn Fn() + Send + Sync> = {
        let handle = handle.clone();
        Arc::new(move || handle.shutdown())
    };
    handle_signals(&handle, &stop, &drain)?;
    handlers_ready();
    // A closed standard error (a journal that went away) must not stop the proxy from serving.
    let _ = writeln!(
        io::stderr(),
        "kbf-installer: proxy: pool of {members} frontends, no supervisor"
    );
    let _ = writeln!(io::stderr(), "kbf-installer: proxy serving");
    runner.run(NoUpdates)
}

#[cfg(test)]
#[path = "proxy_pool_tests.rs"]
mod tests;
