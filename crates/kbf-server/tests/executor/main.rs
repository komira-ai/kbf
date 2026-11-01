//! The host executor against the real farm and an in-process lease server.
//!
//! The farm is [`kbf_server::Farm`] on the scheduler core the build names (the legacy one,
//! and the js core in the `farm-core-js` build, which CI runs these on too); the lease server
//! is `kbf_lease_client::fake::FakeLeaseServer`, which keeps the real server's admission rules
//! and leaves what the hosts do to the test. Time is tokio's paused clock, moved by hand: the
//! tests assert on what happened, never on how long it took.
//!
//! One test binary, as `Farm` is generic and coverage counts each generic function by its
//! best-covered instantiation.

mod classes;
mod driver;
mod keys;
mod phases;
mod pool;
mod quota;
mod real_prep;
mod recovery;
mod rig;
mod spec;
mod starts;
mod wiring;
