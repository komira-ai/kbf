//! End-to-end tests of the serving core, over loopback sockets, with fake children. They run
//! on a current-thread runtime under paused time, so every wait is virtual and no test asserts
//! on a wall clock.

mod bypass;
mod fenced;
mod flood;
mod hold;
mod keepalive;
mod kit;
mod lifetime;
mod routing;
