//! The cold-store registration end to end (W8b): an operator registers a connection and a
//! binding through the real routes (`/v1/object-stores`, called by the real
//! `OperatorClient`), real storage nodes (`kbf_server::storage_half`) fetch it over mutual
//! TLS, probe it against an S3 service and report in their heartbeats, and the operator's
//! status answers what the nodes reported. Only three things are stood in for: the S3
//! service ([`kbf_objstore::s3::fake::FakeS3`], which refuses any key pair but the one a
//! test sets), the state store (the repo's fake, with a clock a test moves) and the two
//! adapters the frontend binary does not have yet (see [`farm`]).
//!
//! Each scenario is a failure the operator can meet; the tests say which defect each
//! catches and which mutant was planted to see it red (`docs/design/object-store-registration.md`).

#![cfg(target_os = "linux")]

mod farm;
mod node;
mod operator;
mod scenarios;
