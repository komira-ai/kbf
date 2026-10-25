//! Behaviour tests of `kbf-pack-adopt`: its flags and what it refuses, the process and its exit
//! statuses, and runs of it over an S3 service on loopback and an in-memory pack index (and,
//! in one test, a Postgres database).

mod flags;
mod postgres;
mod process;
mod runs;
mod support;
