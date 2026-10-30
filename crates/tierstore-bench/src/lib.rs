//! `tierstore-bench`: a measurement tool for tierstore. See the README.
//!
//! The pieces, and who fills what:
//! - [`config`], [`role`], [`result`], [`requestlog`], [`gate`]: the shared types, built.
//! - [`s3`]: the timed, logged S3 client the cells use, built for put, ranged get, head, delete
//!   and list; streaming bodies, multipart and presigned requests are the cell owners' to add.
//! - [`cells::cold_put`], [`cleanup`], [`loadmeter`] and [`output`]: built (the create-only put
//!   cell, the deletion of what it wrote, the machine's load for the gate, and the result and
//!   sample files).
//! - [`cells`] (`cold_read`, `g`, then `a1`, `b`, `c`), [`proxy`], [`preflight`], [`provision`], [`teardown`],
//!   [`report`]: stubs whose signatures are final; each module's header says what goes in it.
//! - [`cli`]: the verbs.

#![forbid(unsafe_code)]

pub mod cells;
pub mod cleanup;
pub mod cli;
pub mod config;
pub mod error;
pub mod gate;
pub mod loadmeter;
pub mod output;
pub mod preflight;
pub mod provision;
pub mod proxy;
pub mod report;
pub mod requestlog;
pub mod result;
pub mod role;
pub mod s3;
pub mod teardown;
