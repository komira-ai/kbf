//! The frontend's blobs in packs (`kbf_front::Packs`), over real gRPC: the batch CAS calls and
//! ByteStream of a proxy given packs, in front of a storage node (a real one, or the fake node
//! that streams), an in-memory pack index and an in-memory object store that count what they
//! are asked and fail when told to; the action-cache hits such a proxy checks against the
//! index (`closure`); the commits of packs that daemons stored (`commit`); and the commits of packs
//! that builds wrote through presigned writes (`untrusted`); and the ingest of the pack a task wrote
//! under a key the frontend minted (`untrusted_task`), the ingest of a run of several packs and large
//! objects (`untrusted_multi`), and the minting of that key (`untrusted_mint`), and the keys an ingest holds against the janitor
//! (`untrusted_verifying`).
//!
//! Every test says the behaviour it asserts.

#[path = "../proxy/harness.rs"]
mod harness;

mod backend_many;
mod bytestream;
mod cas;
mod closure;
mod commit;
mod legacy_reads;
mod multi;
mod store;
mod support;
mod untrusted;
mod untrusted_mint;
mod untrusted_multi;
mod untrusted_task;
mod untrusted_verifying;
mod untrusted_writethrough;
