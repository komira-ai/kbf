//! Reading one member of a pinned archive.

mod common;

use common::{Tweak, tar_gz_of, zip_of, zip_raw};
use mdm_core::pins::Packing;
use mdm_host::archive::{ArchiveError, member, member_with_limit};

fn zip_err(bytes: &[u8]) -> String {
    match member("a.zip", Packing::Zip, bytes, "bin/tool") {
        Err(ArchiveError::Unreadable { why, .. }) => why,
        other => panic!("expected Unreadable, got {other:?}"),
    }
}

/// Catches a reader that returns the wrong member's bytes, or none, from a zip it should
/// read: a deflated member and a stored one, among others.
#[test]
fn a_zip_member_is_found_by_exact_name_stored_or_deflated() {
    let members: [(&str, &[u8]); 3] = [
        ("other", b"zzz"),
        ("bin/tool", b"tool bytes"),
        ("bin/tool2", b"x"),
    ];
    for deflate in [true, false] {
        let zip = zip_raw(&members, deflate, &Tweak::default());
        let got = member("a.zip", Packing::Zip, &zip, "bin/tool").unwrap();
        assert_eq!(got, b"tool bytes");
    }
    let big = vec![7u8; 100_000];
    let zip = zip_of(&[("big", &big)]);
    assert_eq!(member("a.zip", Packing::Zip, &zip, "big").unwrap(), big);
}

/// Catches a missing or empty member reported as success: both are `NoMember`, whose text
/// names the archive and the member.
#[test]
fn a_missing_or_empty_member_is_refused() {
    let zip = zip_of(&[("empty", b""), ("x", b"1")]);
    for name in ["empty", "absent"] {
        let err = member("a.zip", Packing::Zip, &zip, name).unwrap_err();
        assert!(matches!(err, ArchiveError::NoMember { .. }));
        assert_eq!(
            err.to_string(),
            format!("a.zip has no {name}, or it is empty")
        );
    }
}

/// Catches a reader that trusts a damaged zip: each way the structure can be wrong is
/// refused with a reason and never panics or reads outside the bytes.
#[test]
fn a_damaged_zip_is_unreadable() {
    let m: [(&str, &[u8]); 1] = [("bin/tool", b"tool bytes")];
    assert_eq!(zip_err(&[1, 2, 3]), "malformed zip");
    let t = |t: Tweak| zip_err(&zip_raw(&m, true, &t));
    assert_eq!(
        t(Tweak {
            central_sig: Some(1),
            ..Tweak::default()
        }),
        "malformed zip"
    );
    assert_eq!(
        t(Tweak {
            local_sig: Some(1),
            ..Tweak::default()
        }),
        "malformed zip"
    );
    assert_eq!(
        t(Tweak {
            name_len: Some(60_000),
            ..Tweak::default()
        }),
        "malformed zip"
    );
    assert_eq!(
        t(Tweak {
            packed: Some(1_000_000),
            ..Tweak::default()
        }),
        "malformed zip"
    );
    assert_eq!(
        t(Tweak {
            method: Some(99),
            ..Tweak::default()
        }),
        "compression method 99 is not supported"
    );
    let why = t(Tweak {
        plain: Some(3),
        ..Tweak::default()
    });
    assert!(why.starts_with("member is "), "{why}");
    // A deflate stream that is not one.
    let junk = zip_raw(
        &m,
        false,
        &Tweak {
            method: Some(8),
            ..Tweak::default()
        },
    );
    assert!(!zip_err(&junk).is_empty());
    let stored_lie = zip_raw(
        &m,
        false,
        &Tweak {
            plain: Some(99),
            ..Tweak::default()
        },
    );
    assert!(zip_err(&stored_lie).starts_with("member is 10 bytes"));
}

/// Catches a tar reader that matches on a suffix or returns an entry it should skip, and
/// one that treats a non-gzip file as success.
#[test]
fn a_tar_gz_member_is_found_by_exact_path() {
    let tgz = tar_gz_of(&[("lego", b"lego bytes"), ("sub/lego", b"nested")]);
    assert_eq!(
        member("l.tar.gz", Packing::TarGz, &tgz, "lego").unwrap(),
        b"lego bytes"
    );
    assert_eq!(
        member("l.tar.gz", Packing::TarGz, &tgz, "sub/lego").unwrap(),
        b"nested"
    );
    let err = member("l.tar.gz", Packing::TarGz, &tgz, "go").unwrap_err();
    assert!(matches!(err, ArchiveError::NoMember { .. }));
    let err = member("l.tar.gz", Packing::TarGz, b"not gzip at all", "lego").unwrap_err();
    assert!(matches!(err, ArchiveError::Unreadable { .. }), "{err}");
    assert!(
        err.to_string()
            .starts_with("l.tar.gz is not a readable TarGz archive: ")
    );
}

/// Catches a member cut off at the size limit and returned as if whole: a member longer than
/// the limit is an error in a zip (stored or deflated) and in a tar.gz, and one exactly the
/// limit is read in full.
#[test]
fn a_member_above_the_limit_is_an_error_not_a_short_read() {
    let body = [9u8; 10];
    let zips = [
        zip_raw(&[("m", &body[..])], true, &Tweak::default()),
        zip_raw(&[("m", &body[..])], false, &Tweak::default()),
    ];
    for zip in &zips {
        let err = member_with_limit("a.zip", Packing::Zip, zip, "m", 5).unwrap_err();
        assert!(matches!(err, ArchiveError::Unreadable { .. }), "{err}");
        assert_eq!(
            member_with_limit("a.zip", Packing::Zip, zip, "m", 10).unwrap(),
            body
        );
    }
    let tgz = tar_gz_of(&[("m", &body[..])]);
    let err = member_with_limit("a.tar.gz", Packing::TarGz, &tgz, "m", 5).unwrap_err();
    assert!(
        err.to_string().contains("member is larger than 5 bytes"),
        "{err}"
    );
    assert_eq!(
        member_with_limit("a.tar.gz", Packing::TarGz, &tgz, "m", 10).unwrap(),
        body
    );
}
