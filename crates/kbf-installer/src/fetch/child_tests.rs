//! What the child decides from its flags alone, and how it writes an answer. (The child
//! against real servers is in `tests/fetch_e2e.rs`; these never reach a network.)

use super::*;
use crate::testkit::{A, B};

const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

fn args(kind: Kind) -> ChildArgs {
    ChildArgs {
        kind,
        url: "https://h.example".to_owned(),
        sha256: None,
        describe: None,
        platform: None,
        limit: 100,
        ca_file: None,
        // A url child lent an identity stops at its flags: this is how a test whose flags are
        // good gets no further than the check it is about.
        identity_fd: Some(5),
    }
}

fn said(args: &ChildArgs) -> String {
    match fetch(args, &mut Vec::new()) {
        Err(why) => why.to_string(),
        Ok(n) => panic!("wrote {n} bytes"),
    }
}

/// The child is asked for a blob or for a release, never both and never neither, whatever the
/// command line parser let through.
///
/// Catches a child that fetches a blob when it was also asked to describe (or the other way
/// round), or one that has nothing to do and carries on.
#[test]
fn a_blob_or_a_release_but_not_both_or_neither() {
    let mut neither = args(Kind::Url);
    assert!(said(&neither).contains("a blob or for a release"));
    neither.sha256 = Some(SHA.to_owned());
    neither.describe = Some(A.to_owned());
    assert!(said(&neither).contains("a blob or for a release"));
}

/// A commit is exactly 40 lowercase hex digits (the name of a file at the source) and a digest
/// is 64.
///
/// Catches a commit or a digest of another shape being put into a URL or a request.
#[test]
fn what_is_asked_for_is_checked_again() {
    for bad in [
        "",
        "abc",
        &A.to_uppercase(),
        &A[1..],
        &format!("{A}0"),
        "../x",
    ] {
        let mut ask = args(Kind::Url);
        ask.describe = Some(bad.to_owned());
        assert!(said(&ask).contains("is not a commit"), "{bad:?}");
    }
    let mut ask = args(Kind::Url);
    ask.sha256 = Some(A.to_owned());
    assert!(said(&ask).contains("is not a sha256"));
    // A platform is for a description; the command line parser lets it through with a blob.
    ask.sha256 = Some(SHA.to_owned());
    ask.platform = Some("linux-x86_64".to_owned());
    assert!(said(&ask).contains("a platform belongs to a description"));
}

/// A description is at most 16 KiB and one byte more is the sign of an overlong one; a blob is
/// at most the ceiling and one byte more. The limit of each is its own.
///
/// Catches the blob's ceiling applied to a description (a child told to write a gigabyte of
/// one) and the description's bound applied to a blob, and the boundaries off by one.
#[test]
fn the_limit_is_each_ones_own() {
    let mut release = args(Kind::Url);
    release.describe = Some(A.to_owned());
    for limit in [0, describe::MAX_BYTES + 2, MAX_COMPONENT_CEILING + 1] {
        release.limit = limit;
        assert!(
            said(&release).contains(&format!("the limit {limit} is not 1 to 16385")),
            "{limit}"
        );
    }
    for limit in [1, describe::MAX_BYTES + 1] {
        release.limit = limit;
        assert!(said(&release).contains("lent no identity"), "{limit}");
    }
    let mut blob = args(Kind::Url);
    blob.sha256 = Some(SHA.to_owned());
    for limit in [0, MAX_COMPONENT_CEILING + 2] {
        blob.limit = limit;
        assert!(said(&blob).contains("is not 1 to 1073741825"), "{limit}");
    }
    for limit in [1, MAX_COMPONENT_CEILING + 1] {
        blob.limit = limit;
        assert!(said(&blob).contains("lent no identity"), "{limit}");
    }
}

/// The platform is the question of either kind of source (a frontend is asked for it, a `url`
/// source's file is read for it) and is one of the three; a name that is not is not asked about.
///
/// Catches an unchecked string put into a request to the frontend or matched against a file's
/// entries, and a url child that refuses the platform the parent always sends it.
#[test]
fn the_platform_is_one_of_the_three_for_either_kind() {
    let mut url = args(Kind::Url);
    url.describe = Some(A.to_owned());
    let mut frontend = args(Kind::Frontend);
    frontend.describe = Some(B.to_owned());
    frontend.identity_fd = None;
    // Each kind stops at the next check after the platform's, which names what it lacks.
    for (ask, next) in [
        (&mut url, "lent no identity"),
        (&mut frontend, "--identity-fd"),
    ] {
        for bad in [
            "",
            "linux",
            "Linux-x86_64",
            "linux-aarch64",
            "linux-x86_64 ",
        ] {
            ask.platform = Some(bad.to_owned());
            assert!(said(ask).contains("is not a platform"), "{bad:?}");
        }
        for good in ["linux-x86_64", "linux-arm64", "darwin-arm64"] {
            ask.platform = Some(good.to_owned());
            assert!(said(ask).contains(next), "{good}");
        }
        // No platform at all asks for every one the release is complete for.
        ask.platform = None;
        assert!(said(ask).contains(next));
    }
}

/// A writer the other end of which has gone.
struct Gone;

impl Write for Gone {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        Err(io::ErrorKind::BrokenPipe.into())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// A reader that has gone away while the answer is written is the child's own write fault (not a
/// source's), and nothing is counted as written.
///
/// Catches the write error dropped (an answer reported as written to nobody).
#[test]
fn an_answer_nobody_reads_is_a_write_fault() {
    let answer = Answer::Refused("NotFound: x".to_owned());
    let got = write_answer(&answer, 100, &mut Gone);
    assert!(matches!(got, Err(ChildFault::Out(_))), "{got:?}");
    assert!(
        got.unwrap_err()
            .to_string()
            .starts_with("cannot write the bytes")
    );
}

/// An answer is written whole when it fits and cut at the limit when it does not (the parent
/// allows one byte fewer than the limit, so a cut answer is seen and refused).
///
/// Catches an answer longer than the limit written in full, and one cut short of it.
#[test]
fn an_answer_is_written_within_the_limit() {
    let answer = Answer::Refused("NotFound: x".to_owned());
    let text = answer.render();
    for (limit, want) in [
        (text.len() as u64 + 5, text.len()),
        (text.len() as u64, text.len()),
        (text.len() as u64 - 1, text.len() - 1),
        (1, 1),
    ] {
        let mut out = Vec::new();
        assert_eq!(
            write_answer(&answer, limit, &mut out).expect("written"),
            want as u64
        );
        assert_eq!(out, text.as_bytes()[..want], "limit {limit}");
    }
}

/// The one line the child says of a status is its code and the server's words.
#[test]
fn a_status_is_its_code_and_message() {
    assert_eq!(
        status_line(&tonic::Status::failed_precondition(
            "linux-arm64 has no tarball of kbf"
        )),
        "FailedPrecondition: linux-arm64 has no tarball of kbf"
    );
}
