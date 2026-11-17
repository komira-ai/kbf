//! The executor over an in-process job service, as the frontend builds it and as the tests'
//! seams stand for it.
//!
//! The executor is [`kbf_server::executor::Executor`]; the job service is
//! `kbf_job_client::fake::FakeJobServer`, which keeps the service's rules and leaves what the
//! hosts do to the test. Time is real, and the executor reads its jobs every few milliseconds:
//! a test waits for an event, bounded, and never asserts how long anything took.

mod classes;
mod driver;
mod jobs;
mod keys;
mod mac;
mod n_minus_1;
mod phases;
mod promotion;
mod put_dir;
mod record;
mod recovery;
mod rig;
mod spec;
mod wiring;
