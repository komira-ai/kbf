//! `tierstore-bench`: a measurement tool for tierstore. See the README.
//!
//! The pieces, and who fills what:
//! - [`config`], [`role`], [`result`], [`requestlog`], [`gate`], [`procload`]: the shared types and
//!   the reader of the machine's load.
//! - [`s3`]: the timed, logged S3 client the cells use: put, multipart put, ranged get, head,
//!   delete, list, and a sweep of unfinished uploads.
//! - [`proxy`]: the fault proxy; [`faultfile`]: a cell's switch for the proxy process.
//! - [`cells`] (`cold_put`, `cold_read`, `g`, then `a1`, `b`, `c`) and [`report`]: built; see the
//!   header of `cells`.
//! - [`preflight`], [`provision`], [`teardown`]: the operations verbs; each module's header says
//!   what it checks or creates.
//! - [`cli`]: the verbs.

#![forbid(unsafe_code)]

pub mod atomic;
pub mod cells;
pub mod cli;
pub mod coldserver;
pub mod coldstate;
pub mod config;
pub mod error;
pub mod faultfile;
pub mod gate;
pub mod livedir;
pub mod preflight;
pub mod procload;
pub mod provision;
pub mod proxy;
pub mod report;
pub mod requestlog;
pub mod result;
pub mod role;
pub mod s3;
pub mod teardown;
