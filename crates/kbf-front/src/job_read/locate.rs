//! `GET /locate`: where the blobs of a grant's Action are, so that a task fetches them from
//! the stores directly and asks the listener only for what it cannot find.
//!
//! The answer covers exactly the grant's scope (the Action, its Command and the input closure
//! the memoised scope walk computed) and nothing else, and carries no digest that is not in it.
//! The body is JSON lines, one object per digest of the scope, in digest order:
//!
//! - `{"digest":"<hex>/<size>","pack":{"key":"<pack key>","offset":<n>,"length":<n>}}`: the blob
//!   is the `length` bytes at `offset` of that pack, as the pack index has it;
//! - `{"digest":"<hex>/<size>","large":{"key":"cas/l/<hex>","size":<n>}}`: a blob over
//!   [`super::Limits::stream_over`] that tierstore holds whole under its own key, `size` being the
//!   length of that object (always the digest's size: an object of another length is `absent`);
//! - `{"digest":"<hex>/<size>","absent":true}`: neither; the task asks the listener's GET or
//!   batch for it. The empty blob is always `absent`: the listener answers it without a read.
//!
//! The table is built for each request from the memoised closure and a fresh pack index lookup,
//! so it is as current as the index; the presence of a large object is remembered for
//! [`super::Limits::locate_present_for`] and its absence for [`super::Limits::locate_absent_for`]
//! (seconds). A span or key may still go stale before it is used (a pack compacted, an object
//! gone): that is by design, and safe, as the task checks the SHA-256 of every blob it takes and
//! falls back to the listener for one that does not verify or is gone.
//!
//! A `large` key is readable by a task only once it holds a read credential for tierstore (a
//! per-job grant or a presigned GET); until then kbf-task treats it as `absent`.

use kbf_types::Digest;

/// The path of the locate read, which takes a GET.
pub const LOCATE_PATH: &str = "/locate";

/// Where a blob lies in a pack.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    /// The key of the pack.
    pub key: String,
    /// Where the blob's bytes start in the pack.
    pub offset: u64,
    /// The blob's length in bytes.
    pub length: u64,
}

/// The key a blob over the record cap has in tierstore: `cas/l/<lowercase hex of its hash>`.
#[must_use]
pub fn large_key(digest: &Digest) -> String {
    format!("cas/l/{}", digest.hash_hex())
}
