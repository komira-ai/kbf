//! The host executor against the real farm and an in-process job service.
//!
//! The farm is [`kbf_server::Farm`] on the legacy scheduler core; the job service
//! is `kbf_job_client::fake::FakeJobServer`, which keeps the service's rules
//! and leaves what the hosts do to the test. Time is tokio's paused clock, moved by hand: the
//! tests assert on what happened, never on how long it took.
//!
//! One test binary, as `Farm` is generic and coverage counts each generic function by its
//! best-covered instantiation.

mod classes;
mod drain;
mod driver;
mod files;
mod jobs;
mod keys;
mod phases;
mod pool;
mod promotion;
mod put_dir;
mod real_prep;
mod recovery;
mod rig;
mod spec;
mod starts;
mod whole;
mod wiring;
