//! `tierstore-bench`: a measurement tool for tierstore. See the README.
//!
//! The pieces, and who fills what:
//! - [`config`], [`role`], [`result`], [`requestlog`], [`gate`]: the shared types, built.
//! - [`s3`]: the timed, logged S3 client the cells use, built for put, ranged get, head, delete
//!   and list; streaming bodies, multipart and presigned requests are the cell owners' to add.
//! - [`proxy`]: the fault proxy, built.
//! - [`cells`] (`cold_put`, `cold_read`, `g`, then `a1`, `b`, `c`), [`preflight`], [`provision`], [`teardown`],
//!   [`report`]: stubs whose signatures are final; each module's header says what goes in it.
//! - [`cli`]: the verbs.

#![forbid(unsafe_code)]

pub mod cells;
pub mod cli;
pub mod config;
pub mod error;
pub mod gate;
pub mod preflight;
pub mod provision;
pub mod proxy;
pub mod report;
pub mod requestlog;
pub mod result;
pub mod role;
pub mod s3;
pub mod teardown;
