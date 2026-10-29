//! The frontend's blobs in packs (`kbf_front::Packs`), over real gRPC: the batch CAS calls and
//! ByteStream of a proxy given packs, in front of a storage node (a real one, or the fake node
//! that streams), an in-memory pack index and an in-memory object store that count what they
//! are asked and fail when told to; the action-cache hits such a proxy checks against the
//! index (`closure`); the commits of packs that daemons stored (`commit`); and the commits of packs
//! that builds wrote through presigned writes (`untrusted`); and the ingest of the pack a task wrote
//! under a key the frontend minted (`untrusted_task`).
//!
//! Every test says the behaviour it asserts.

#[path = "../proxy/harness.rs"]
mod harness;

mod bytestream;
mod cas;
mod closure;
mod commit;
mod store;
mod support;
mod untrusted;
mod untrusted_task;
