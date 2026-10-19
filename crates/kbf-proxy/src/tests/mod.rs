//! End-to-end tests of the serving core, over loopback sockets, with fake children. They run
//! on a current-thread runtime under paused time, so every wait is virtual and no test asserts
//! on a wall clock.

mod bypass;
mod frames;
mod h1;
mod hold;
mod kit;
mod lifetime;
pub(crate) mod pki;
mod ready;
mod reload;
mod routing;
mod secure;
