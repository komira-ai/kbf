//! A frontend with no storage node (`kbf_front::Stored`), over real gRPC: the CAS, ByteStream and
//! ActionCache services and the `Backend` plane, over an in-memory object store and an in-memory
//! pack index and action cache that count what they are asked and fail when told to. No storage
//! node exists in any of these tests: nothing is dialled.
//!
//! Every test says the behaviour it asserts.

#[path = "../proxy/harness.rs"]
mod harness;
// The object store fixture of the packs' tests; it reads a clock of the untrusted tests.
#[path = "../packed/support.rs"]
mod support;

mod untrusted_task {
    /// The time the packs of the fixture take as now.
    pub fn now() -> std::time::SystemTime {
        std::time::SystemTime::UNIX_EPOCH
    }
}

mod action_cache;
mod backend;
mod bytestream;
mod cas;
mod denied;
mod empty_blob;
mod large;
mod rig;
