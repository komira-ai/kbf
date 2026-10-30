//! The batch route of the read grant: many small blobs in one request.
//!
//! One GET per file costs a round trip each, and for a tree of thousands of small files those
//! dominate the fetch. `POST <grant>/batch` answers up to [`MAX_BATCH_DIGESTS`] digests whose sizes
//! sum to at most [`MAX_BATCH_BYTES`] in one response.
//!
//! # Wire format
//!
//! The request body is JSON, `{"digests":["<hex>/<size>", ...]}`, with the same bearer token as a
//! GET. The server may refuse the whole request before reading anything: 401 or 403 for the token,
//! 400 for a malformed request, 413 when the requested sizes sum to more than
//! [`MAX_BATCH_BYTES`], and 404 or 405 when it has no such route. Otherwise it answers 200 with
//! one frame per requested digest, in request order: one status byte, the length of the body as
//! eight bytes big-endian, then the body. The statuses are 0 ok (the length is the digest's
//! size), 1 not found, 2 out of scope, 3 too large for a batch, 4 store unavailable (the length
//! is 0 for each of the last four).
//!
//! # What the client does with it
//!
//! Only blobs smaller than [`BATCH_BLOB_LIMIT`] are asked for in a batch; the rest go by GET.
//! Each batch counts as one request against the concurrency bound. Nothing in a response is
//! trusted: an ok body is exactly the digest's size and passes the same size and SHA-256 check as
//! a GET body, and a frame that does not follow the format (short, a length the status does not
//! allow, an unknown status, bytes after the last frame) fails the fetch.
//!
//! A blob answered "too large" or "store unavailable" is fetched once by GET. A blob answered "not
//! found" or "out of scope" fails the fetch. A 404 or 405 for the whole request means the server
//! has no batch route: the source says so once on stderr and fetches every blob by GET from then
//! on, including those of the batches not yet sent. Any other refusal fails the fetch.

use super::*;

/// Blobs of at least this many bytes are fetched by GET even when batching is on.
pub const BATCH_BLOB_LIMIT: i64 = 64 << 10;

/// The most digests in one batch request.
pub const MAX_BATCH_DIGESTS: usize = 256;

/// The most the sizes in one batch request may sum to.
pub const MAX_BATCH_BYTES: i64 = 4 << 20;

/// One request's worth of work: the indexes of the digests it fetches.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Unit {
    /// One blob by GET.
    Get(usize),
    /// These blobs in one batch request.
    Batch(Vec<usize>),
}

/// Splits `digests` into requests: each blob under [`BATCH_BLOB_LIMIT`] joins a batch of at most
/// [`MAX_BATCH_DIGESTS`] digests and [`MAX_BATCH_BYTES`] bytes, in order; every other blob, and
/// any digest that is malformed (the GET reports it), is fetched alone by GET. Batches come first.
pub(super) fn plan(digests: &[Digest]) -> Vec<Unit> {
    let mut batches: Vec<Unit> = Vec::new();
    let mut gets: Vec<Unit> = Vec::new();
    let mut current: Vec<usize> = Vec::new();
    let mut bytes = 0;
    for (index, digest) in digests.iter().enumerate() {
        let small = check_digest(digest).is_ok() && digest.size_bytes < BATCH_BLOB_LIMIT;
        if !small {
            gets.push(Unit::Get(index));
            continue;
        }
        if current.len() == MAX_BATCH_DIGESTS || bytes + digest.size_bytes > MAX_BATCH_BYTES {
            batches.push(Unit::Batch(std::mem::take(&mut current)));
            bytes = 0;
        }
        current.push(index);
        bytes += digest.size_bytes;
    }
    if !current.is_empty() {
        batches.push(Unit::Batch(current));
    }
    batches.extend(gets);
    batches
}

impl HttpBlobSource {
    /// This source with the batch route on or off. Off, the default, every blob goes by GET.
    #[must_use]
    pub fn with_batch(mut self, on: bool) -> Self {
        self.batch = on;
        self
    }

    /// Whether small blobs are asked for in batches.
    #[must_use]
    pub fn batching(&self) -> bool {
        self.batch
    }

    /// [`BlobSource::fetch_many`] for this source: batches for the small blobs when they are on,
    /// GETs for the rest.
    pub(super) fn fetch_units(
        &self,
        digests: &[Digest],
        concurrency: usize,
        each: &Each<'_>,
    ) -> Result<(), FetchError> {
        let units = if self.batch {
            plan(digests)
        } else {
            (0..digests.len()).map(Unit::Get).collect()
        };
        run_bounded(concurrency, &units, &|_, unit| match unit {
            Unit::Get(index) => self.get_one(digests, *index, each),
            Unit::Batch(indexes) => self.fetch_batch(digests, indexes, each),
        })
    }

    fn get_one(&self, digests: &[Digest], index: usize, each: &Each<'_>) -> Result<(), FetchError> {
        each(index, &mut |out| self.fetch_to(&digests[index], out))
    }

    /// Fetches the blobs at `indexes` with one batch request, or by GET when the server has no
    /// batch route.
    fn fetch_batch(
        &self,
        digests: &[Digest],
        indexes: &[usize],
        each: &Each<'_>,
    ) -> Result<(), FetchError> {
        if !self.batch_off.load(Ordering::SeqCst) {
            let mut response = self.post_batch(digests, indexes)?;
            let status = response.status().as_u16();
            if status == 200 {
                return self.read_frames(
                    digests,
                    indexes,
                    each,
                    &mut response.body_mut().as_reader(),
                );
            }
            if status != 404 && status != 405 {
                return Err(FetchError::Source {
                    digest: batch_label(digests, indexes),
                    why: format!("the grant answered {status} to a batch request"),
                });
            }
            self.batch_off.store(true, Ordering::SeqCst);
            self.batch_notice.call_once(|| {
                eprintln!(
                    "kbf-task: the grant answered {status} to a batch request; \
                     fetching every blob by GET"
                );
            });
        }
        indexes
            .iter()
            .try_for_each(|&index| self.get_one(digests, index, each))
    }

    fn post_batch(
        &self,
        digests: &[Digest],
        indexes: &[usize],
    ) -> Result<ureq::http::Response<ureq::Body>, FetchError> {
        let list: Vec<String> = indexes
            .iter()
            .map(|&index| format!("\"{}\"", label(&digests[index])))
            .collect();
        let body = format!("{{\"digests\":[{}]}}", list.join(","));
        let total = indexes.iter().map(|&index| digests[index].size_bytes).sum();
        self.agent
            .post(&format!("{}/batch", self.base))
            .config()
            .timeout_global(Some(request_timeout(total)))
            .build()
            .header("Authorization", &self.authorization)
            .header("Content-Type", "application/json")
            .send(body.as_str())
            .map_err(|e| FetchError::Source {
                digest: batch_label(digests, indexes),
                why: e.to_string(),
            })
    }

    /// Reads one frame per index from a 200 response and hands each blob to `each`.
    fn read_frames(
        &self,
        digests: &[Digest],
        indexes: &[usize],
        each: &Each<'_>,
        body: &mut impl Read,
    ) -> Result<(), FetchError> {
        let framing = |why: &str| FetchError::Source {
            digest: batch_label(digests, indexes),
            why: format!("the batch response is malformed: {why}"),
        };
        for &index in indexes {
            let digest = &digests[index];
            let size = digest.size_bytes.unsigned_abs();
            let mut head = [0_u8; 9];
            body.read_exact(&mut head)
                .map_err(|e| framing(&format!("no frame header for {}: {e}", label(digest))))?;
            let status = head[0];
            let mut length = [0_u8; 8];
            length.copy_from_slice(&head[1..]);
            let length = u64::from_be_bytes(length);
            if status > STORE_UNAVAILABLE {
                return Err(framing(&format!("unknown status {status}")));
            }
            let expected = if status == OK { size } else { 0 };
            if length != expected {
                return Err(framing(&format!(
                    "{} has status {status} and length {length}, not {expected}",
                    label(digest)
                )));
            }
            match status {
                OK => each(index, &mut |out| {
                    let copied = std::io::copy(&mut body.by_ref().take(size), out)
                        .map_err(|e| framing(&format!("{}: {e}", label(digest))))?;
                    if copied == size {
                        Ok(())
                    } else {
                        Err(framing(&format!("{} is cut short", label(digest))))
                    }
                })?,
                NOT_FOUND | OUT_OF_SCOPE => each(index, &mut |_| {
                    Err(FetchError::Source {
                        digest: label(digest),
                        why: if status == NOT_FOUND {
                            "the grant does not have it".to_owned()
                        } else {
                            "it is outside the grant's scope".to_owned()
                        },
                    })
                })?,
                _ => self.get_one(digests, index, each)?,
            }
        }
        let mut extra = [0_u8; 1];
        let more = body
            .read(&mut extra)
            .map_err(|e| framing(&format!("after the last frame: {e}")))?;
        if more == 0 {
            Ok(())
        } else {
            Err(framing("bytes follow the last frame"))
        }
    }
}

const OK: u8 = 0;
const NOT_FOUND: u8 = 1;
const OUT_OF_SCOPE: u8 = 2;
const STORE_UNAVAILABLE: u8 = 4;

/// Names a batch in an error: its first digest and its size.
fn batch_label(digests: &[Digest], indexes: &[usize]) -> String {
    format!(
        "batch of {} starting at {}",
        indexes.len(),
        label(&digests[indexes[0]])
    )
}

#[cfg(test)]
#[path = "batch_tests.rs"]
mod tests;
