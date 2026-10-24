//! What `fetch-child` decides from its flags alone, as a program that links the library calls it:
//! each case ends at a refusal before any descriptor is looked at and before any network is
//! reached. (The child against real servers is in `tests/fetch_e2e.rs`; the library's own tests
//! hold the same rules.)

use kbf_installer::fetch::MAX_COMPONENT_CEILING;
use kbf_installer::fetch::child::{ChildArgs, Kind, fetch};
use kbf_installer::fetch::describe::MAX_BYTES;
use kbf_installer::testkit::{A, B};

const SHA: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

/// Flags that are good so far, for a child of `kind`. A `url` child lent an identity stops at its
/// flags: this is how a case whose flags are good gets no further than the check it is about.
fn args(kind: Kind) -> ChildArgs {
    ChildArgs {
        kind,
        url: "https://h.example".to_owned(),
        sha256: None,
        describe: None,
        platform: None,
        limit: 100,
        ca_file: None,
        identity_fd: Some(5),
    }
}

/// The one line the child says when it is refused with `args`; the flags must be refused.
fn said(args: &ChildArgs) -> String {
    match fetch(args, &mut Vec::new()) {
        Err(why) => why.to_string(),
        Ok(n) => panic!("wrote {n} bytes"),
    }
}

/// Asserts that the child is asked for a blob or for a release, never both and never neither,
/// whatever the command line parser let through.
#[test]
fn a_blob_or_a_release_but_not_both_or_neither() {
    let mut neither = args(Kind::Url);
    assert!(said(&neither).contains("a blob or for a release"));
    neither.sha256 = Some(SHA.to_owned());
    neither.describe = Some(A.to_owned());
    assert!(said(&neither).contains("a blob or for a release"));
}

/// Asserts that a commit is exactly 40 lowercase hex digits and a digest is 64, and that a
/// platform given with a blob is refused (the command line parser lets it through).
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
    ask.sha256 = Some(SHA.to_owned());
    ask.platform = Some("linux-x86_64".to_owned());
    assert!(said(&ask).contains("a platform belongs to a description"));
}

/// Asserts that a description is limited to 1 to `MAX_BYTES` plus one bytes and a blob to 1 to the
/// ceiling plus one, each bound exactly there (the parent adds one to what it declared), and that a
/// limit in range goes on to the next check.
#[test]
fn the_limit_is_each_ones_own() {
    let mut release = args(Kind::Url);
    release.describe = Some(A.to_owned());
    for limit in [0, MAX_BYTES + 2, MAX_COMPONENT_CEILING + 1] {
        release.limit = limit;
        let way = format!("the limit {limit} is not 1 to {}", MAX_BYTES + 1);
        assert!(said(&release).contains(&way), "{limit}");
    }
    for limit in [1, MAX_BYTES + 1] {
        release.limit = limit;
        assert!(said(&release).contains("lent no identity"), "{limit}");
    }
    let mut blob = args(Kind::Url);
    blob.sha256 = Some(SHA.to_owned());
    for limit in [0, MAX_COMPONENT_CEILING + 2] {
        blob.limit = limit;
        let way = format!("is not 1 to {}", MAX_COMPONENT_CEILING + 1);
        assert!(said(&blob).contains(&way), "{limit}");
    }
    for limit in [1, MAX_COMPONENT_CEILING + 1] {
        blob.limit = limit;
        assert!(said(&blob).contains("lent no identity"), "{limit}");
    }
}

/// Asserts that the platform is the question of either kind of source and is one of the three, a
/// name that is not is refused, and that a `frontend` child lent its CA by the command line is
/// refused (it trusts the pinned CA only).
#[test]
fn the_platform_is_one_of_the_three_for_either_kind() {
    let mut url = args(Kind::Url);
    url.describe = Some(A.to_owned());
    let mut frontend = args(Kind::Frontend);
    frontend.describe = Some(B.to_owned());
    frontend.identity_fd = None;
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
        ask.platform = None;
        assert!(said(ask).contains(next));
    }
    let mut pinned = args(Kind::Frontend);
    pinned.describe = Some(B.to_owned());
    pinned.ca_file = Some("/x/ca.pem".into());
    assert!(said(&pinned).contains("trusts the pinned CA only"));
}
