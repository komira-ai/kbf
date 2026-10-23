//! A start over a state store that is already open, for several frontends in one process
//! over one store ([`crate::retire`]).

use std::error::Error;
use std::future::Future;
use std::io;

use super::{FrontendFlags, FrontendHandle, Prepared, Shutdown, boxed};

/// [`super::start_frontend`] over a state store that is already open (`state`), with no
/// generation taken: the start of one frontend among several over a state store they
/// share, each holding a handle of its own. `shutdown` is as for
/// [`super::start_frontend`].
///
/// # Errors
/// As [`super::start_frontend`], except that no state store is opened.
pub fn start_frontend_over<S, F>(
    flags: &FrontendFlags,
    state: crate::StateStore,
    shutdown: S,
) -> Result<FrontendHandle, Box<dyn Error>>
where
    S: FnOnce() -> io::Result<F> + 'static,
    F: Future<Output = ()> + Send + 'static,
{
    // No branch here: this is the one generic function, and `bind_over` is the code.
    bind_over(flags, state, boxed(shutdown))
}

/// [`start_frontend_over`], not generic.
fn bind_over(
    flags: &FrontendFlags,
    state: crate::StateStore,
    shutdown: Shutdown,
) -> Result<FrontendHandle, Box<dyn Error>> {
    Prepared::new(flags).and_then(|prepared| prepared.start(Some(state), shutdown, None))
}
