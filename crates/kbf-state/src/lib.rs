//! The farm's state store: the trait the scheduler writes every change through, an
//! in-memory fake, and the conformance suite every backend must pass.
//!
//! The code now lives in the `js-state` crate of the `komira-ai/jobscheduler`
//! repository (`crates/js-state`), at the revision pinned in the workspace `Cargo.toml`;
//! this crate re-exports it under its old name so that every path (`kbf_state::Store`,
//! `kbf_state::postgres::PostgresConfig`, `kbf_state::fake::Fake`, ...) stays as it was.
//! Its features (`postgres`, `test-support`, `test-database`) turn on the features of the
//! same name of `js-state`. The tests of the store run in that repository; the first
//! migration of the Postgres schema is its file, and a crate with tables of its own in
//! the schema (`kbf-pack-index`) adds a migration through `postgres::schema::extend`.
#![forbid(unsafe_code)]

pub use js_state::*;
