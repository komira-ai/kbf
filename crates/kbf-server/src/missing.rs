//! An input the worker could not fetch, as the server reads the daemon's `Result`
//! (failure classes, "Missing blob at the worker").
//!
//! The daemon reports a missing input as FAILED_PRECONDITION with a
//! `google.rpc.PreconditionFailure` holding a `MISSING` violation per blob, subject
//! `blobs/<hash>/<size>` (`kbf-daemon`, `lease::missing`). The daemon's only CAS is this
//! server, which answers NOT_FOUND only for a blob its index does not hold, so the
//! report means the farm lost the blob after the front checked the inputs. The callers
//! get the same answer the front gives before queueing ([`kbf_front::missing_status`]),
//! rebuilt from the digests rather than forwarded as sent, so that it carries the one
//! detail Bazel re-uploads on and nothing a daemon added.
//!
//! Any other FAILED_PRECONDITION (no detail, a violation of another type, a subject
//! that is not a digest) is not a missing input: it stays the farm's failure.

use kbf_proto::google::rpc::{self, PreconditionFailure};
use kbf_types::{Digest, DigestFunction};
use prost::Message;
use tonic::Code;

/// The `type_url` of a `google.rpc.PreconditionFailure` detail.
const PRECONDITION_FAILURE: &str = "type.googleapis.com/google.rpc.PreconditionFailure";

/// The blobs a daemon's `status` says are missing, each once, in the order it named
/// them; `None` unless the status is FAILED_PRECONDITION whose `PreconditionFailure`
/// details hold at least one violation and every violation is `MISSING` with a
/// `blobs/<hash>/<size>` subject. Details of other types are ignored.
pub(crate) fn inputs(status: &rpc::Status) -> Option<Vec<Digest>> {
    if status.code != Code::FailedPrecondition as i32 {
        return None;
    }
    let mut blobs = Vec::new();
    for detail in status
        .details
        .iter()
        .filter(|d| d.type_url == PRECONDITION_FAILURE)
    {
        let failure = PreconditionFailure::decode(detail.value.as_slice()).ok()?;
        for violation in failure.violations {
            if violation.r#type != "MISSING" {
                return None;
            }
            let blob = violation.subject.strip_prefix("blobs/")?;
            let digest = Digest::parse(DigestFunction::Sha256, blob).ok()?;
            if !blobs.contains(&digest) {
                blobs.push(digest);
            }
        }
    }
    (!blobs.is_empty()).then_some(blobs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use kbf_proto::google::rpc::precondition_failure::Violation;

    fn digest(n: u8) -> Digest {
        Digest::new(DigestFunction::Sha256, [n; 32], u64::from(n) + 1)
    }

    fn subject(d: &Digest) -> String {
        format!("blobs/{}/{}", d.hash_hex(), d.size_bytes)
    }

    fn violation(kind: &str, subject: String) -> Violation {
        Violation {
            r#type: kind.to_owned(),
            subject,
            description: String::new(),
        }
    }

    fn detail(type_url: &str, violations: Vec<Violation>) -> prost_types::Any {
        prost_types::Any {
            type_url: type_url.to_owned(),
            value: PreconditionFailure { violations }.encode_to_vec(),
        }
    }

    fn status(code: Code, details: Vec<prost_types::Any>) -> rpc::Status {
        rpc::Status {
            code: code as i32,
            message: "from the daemon".to_owned(),
            details,
        }
    }

    /// Catches a reader that drops a blob, repeats one the daemon named twice (in one
    /// detail or across two), or reorders them; and one that refuses a report for a
    /// detail of another type next to the `PreconditionFailure`, which the answer
    /// leaves out anyway.
    #[test]
    fn every_missing_blob_is_read_once_in_order() {
        let (a, b, c) = (digest(1), digest(2), digest(3));
        let first = detail(
            PRECONDITION_FAILURE,
            vec![
                violation("MISSING", subject(&b)),
                violation("MISSING", subject(&a)),
                violation("MISSING", subject(&b)),
            ],
        );
        let other = prost_types::Any {
            type_url: "type.googleapis.com/google.rpc.ErrorInfo".to_owned(),
            value: b"not decoded".to_vec(),
        };
        let second = detail(
            PRECONDITION_FAILURE,
            vec![violation("MISSING", subject(&c))],
        );
        let read = inputs(&status(
            Code::FailedPrecondition,
            vec![first, other, second],
        ));
        assert_eq!(read, Some(vec![b, a, c]));
    }

    /// Catches a FAILED_PRECONDITION that is not a missing input answered as one: the
    /// daemon's "no driver serves the lease kind" (no detail), a violation of another
    /// type next to a `MISSING` one (Bazel would not re-upload on it, and the farm
    /// would claim the client's blob is gone), a subject that is not
    /// `blobs/<hash>/<size>`, a detail that does not decode, and another code with a
    /// `MISSING` detail.
    #[test]
    fn only_a_pure_missing_report_is_read() {
        let a = digest(1);
        let missing = || violation("MISSING", subject(&a));
        let cases = [
            ("no detail", status(Code::FailedPrecondition, Vec::new())),
            (
                "no violation",
                status(
                    Code::FailedPrecondition,
                    vec![detail(PRECONDITION_FAILURE, Vec::new())],
                ),
            ),
            (
                "another violation type",
                status(
                    Code::FailedPrecondition,
                    vec![detail(
                        PRECONDITION_FAILURE,
                        vec![missing(), violation("STALE", subject(&a))],
                    )],
                ),
            ),
            (
                "no blobs/ prefix",
                status(
                    Code::FailedPrecondition,
                    vec![detail(
                        PRECONDITION_FAILURE,
                        vec![violation("MISSING", a.to_string())],
                    )],
                ),
            ),
            (
                "not a digest",
                status(
                    Code::FailedPrecondition,
                    vec![detail(
                        PRECONDITION_FAILURE,
                        vec![violation("MISSING", "blobs/ab/1".to_owned())],
                    )],
                ),
            ),
            (
                "does not decode",
                status(
                    Code::FailedPrecondition,
                    vec![prost_types::Any {
                        type_url: PRECONDITION_FAILURE.to_owned(),
                        value: vec![0xff],
                    }],
                ),
            ),
            (
                "another code",
                status(
                    Code::Internal,
                    vec![detail(PRECONDITION_FAILURE, vec![missing()])],
                ),
            ),
        ];
        for (name, status) in cases {
            assert_eq!(inputs(&status), None, "{name}");
        }
    }
}
