use kbf_proto::reapi::{Action, Digest};
use kbf_segments::sha256;
use kbf_testkit::dir::TestDir;
use prost::Message as _;

use super::*;
use crate::digest;

fn scratch(name: &str) -> TestDir {
    TestDir::under(std::env::temp_dir(), &format!("kbf-task-message-{name}"))
}

/// The digest `bytes` has, as the Action messages carry it.
fn digest_of(bytes: &[u8]) -> Digest {
    digest::to_reapi(&sha256(bytes))
}

/// An Action and the file holding it under `dir`.
fn action_file(dir: &TestDir) -> (Action, Vec<u8>, std::path::PathBuf) {
    let action = Action {
        salt: b"salt".to_vec(),
        ..Action::default()
    };
    let bytes = action.encode_to_vec();
    let path = dir.join("action");
    std::fs::write(&path, &bytes).expect("action file");
    (action, bytes, path)
}

/// Asserts that a file holding the blob its digest names is decoded.
#[test]
fn a_file_that_is_the_blob_is_decoded() {
    let dir = scratch("decoded");
    let (action, bytes, path) = action_file(&dir);
    let read: Action = file_message(&path, &digest_of(&bytes)).expect("read");
    assert_eq!(read, action);
}

/// Asserts that `label` is `<hex>/<size>`.
#[test]
fn a_label_is_the_hash_and_the_size() {
    let digest = Digest {
        hash: "ab".to_owned(),
        size_bytes: 3,
    };
    assert_eq!(label(&digest), "ab/3");
}

/// Asserts that a file longer than its digest says, shorter, or of other bytes is a mismatch that
/// names the file, and that the file is read only one byte past the size.
#[test]
fn a_file_that_is_not_the_blob_is_a_mismatch_naming_the_file() {
    let dir = scratch("mismatch");
    let (_, bytes, path) = action_file(&dir);
    let digest = digest_of(&bytes);
    let cases = [
        ([bytes.as_slice(), b"\0"].concat(), "it is longer than"),
        (bytes[..bytes.len() - 1].to_vec(), "it has"),
        (bytes.iter().map(|b| b ^ 1).collect(), "its SHA-256 is"),
    ];
    for (held, why) in cases {
        std::fs::write(&path, &held).expect("rewrite");
        let error = file_message::<Action>(&path, &digest).expect_err("mismatch");
        let text = error.to_string();
        assert!(matches!(error, FetchError::Mismatch { .. }), "{text}");
        assert!(text.contains(why), "{text}");
        assert!(
            text.ends_with(&format!(", as read from {}", path.display())),
            "{text}"
        );
    }
}

/// Asserts that a blob that verifies but is not an Action is refused as invalid.
#[test]
fn a_blob_that_does_not_decode_is_invalid() {
    let dir = scratch("undecodable");
    let path = dir.join("action");
    // Field 1 with wire type 7, which no message has.
    let garbage = b"\x0f".to_vec();
    std::fs::write(&path, &garbage).expect("file");
    let error = file_message::<Action>(&path, &digest_of(&garbage)).expect_err("garbage");
    assert!(
        matches!(&error, FetchError::Invalid(why) if why.contains("does not decode")),
        "{error}"
    );
}

/// Asserts that a digest over the bound is refused before the file is opened (there is none), a
/// malformed digest is refused, and a file that is missing or a directory is an I/O error naming it.
#[test]
fn a_digest_over_the_bound_or_malformed_is_refused_and_an_unreadable_file_is_an_io_error() {
    let dir = scratch("refused");
    let path = dir.join("absent");
    let hash = "0".repeat(64);
    let over = Digest {
        hash: hash.clone(),
        size_bytes: MAX_MESSAGE_BYTES + 1,
    };
    let error = file_message::<Action>(&path, &over).expect_err("over");
    assert!(
        matches!(&error, FetchError::Invalid(why) if why.contains("over the")),
        "{error}"
    );

    for bad in [
        Digest {
            hash: "xyz".to_owned(),
            size_bytes: 1,
        },
        Digest {
            hash: hash.clone(),
            size_bytes: -1,
        },
        Digest {
            hash: "A".repeat(64),
            size_bytes: 1,
        },
    ] {
        let error = file_message::<Action>(&path, &bad).expect_err("bad digest");
        assert!(matches!(error, FetchError::BadDigest(_)), "{error}");
    }

    let at = Digest {
        hash,
        size_bytes: MAX_MESSAGE_BYTES,
    };
    let error = file_message::<Action>(&path, &at).expect_err("missing");
    assert!(
        matches!(&error, FetchError::Io { path: named, .. } if *named == path),
        "{error}"
    );
    std::fs::create_dir(&path).expect("a directory in its place");
    let error = file_message::<Action>(&path, &at).expect_err("a directory");
    assert!(
        matches!(&error, FetchError::Io { path: named, .. } if *named == path),
        "{error}"
    );
}

/// Asserts the rule for a path a Command names: the empty path and plain relative ones pass, and
/// an absolute path, an empty, `.` or `..` part, a trailing slash or a NUL is refused, naming
/// what the path is.
#[test]
fn relative_paths_are_checked() {
    for good in ["", "a", "a/b/c", "a.b/.c/..d"] {
        check_relative("output path", good).expect(good);
    }
    for bad in ["/a", "a//b", "./a", "a/./b", "../a", "a/..", "a/", "a\0b"] {
        let error = check_relative("output path", bad).expect_err(bad);
        assert!(
            matches!(&error, FetchError::Invalid(why) if why.starts_with("output path ")),
            "{bad:?}: {error}"
        );
    }
}
