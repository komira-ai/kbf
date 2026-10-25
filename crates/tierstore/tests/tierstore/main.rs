//! Behaviour tests of the proxy and the reconciler, over in-memory backends behind fault
//! injectors, and, with the `s3` feature, of the S3 backend over the loopback S3 service.

mod backends;
mod cache;
mod capacity;
mod conformance;
mod faults;
mod journal;
mod lease;
mod reconciler;
mod reconciler_faults;
#[cfg(feature = "s3")]
mod s3;
mod support;
mod tiered;
