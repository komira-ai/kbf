//! The record of approved declarations, and approving one.

mod support;

use std::fs;
use std::os::unix::fs::{MetadataExt, symlink};

use support::{Scratch, my_uid};
use update_rights::{ApprovalError, Approvals, Approved, Digest, approve};

fn digest(seed: &str) -> Digest {
    Digest::of(seed.as_bytes())
}

/// Catches a digest that is not what `sha256sum` prints: 64 lowercase hex digits.
#[test]
fn digests_are_sha256_text() {
    let d = digest("a");
    assert_eq!(d.as_str().len(), 64);
    assert_eq!(d.to_string(), d.as_str());
    assert_eq!(Digest::parse(d.as_str()).unwrap(), d);
    // The SHA-256 of "abc".
    assert_eq!(
        Digest::of(b"abc").as_str(),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    for bad in [
        "",
        "abc",
        &"A".repeat(64),
        &"g".repeat(64),
        &"a".repeat(63),
        &"a".repeat(65),
        " aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
    ] {
        let error = Digest::parse(bad).unwrap_err();
        assert!(error.to_string().contains("SHA-256"), "{bad:?}");
    }
}

/// Catches a record that misreads its own file: a digest per line, a name after it, comments,
/// blank lines.
#[test]
fn the_record_reads_digests_and_ignores_the_rest() {
    let (a, b, c) = (digest("a"), digest("b"), digest("c"));
    let text = format!(
        "# approved by the fleet\n\n{a}\n{b} tierstore\n  {c}   js-agent # note\n   \n#{a}\n"
    );
    let approvals = Approvals::parse(&text).unwrap();
    assert_eq!(approvals.len(), 3);
    assert!(!approvals.is_empty());
    for d in [&a, &b, &c] {
        assert!(approvals.contains(d));
    }
    assert!(!approvals.contains(&digest("d")));
    assert!(Approvals::parse("").unwrap().is_empty());
    assert!(Approvals::parse("# only a comment\n").unwrap().is_empty());
}

/// Catches a line that is not a digest being skipped: a typo in the file must not silently
/// approve nothing (or, worse, be read as something else).
#[test]
fn a_line_that_is_not_a_digest_is_an_error_with_its_number() {
    let a = digest("a");
    for (text, line) in [
        ("nonsense\n", 1),
        (&format!("{a}\n\nshort tierstore\n") as &str, 3),
        ("ABC\n", 1),
    ] {
        match Approvals::parse(text).unwrap_err() {
            ApprovalError::Line { line: found, .. } => assert_eq!(found, line, "{text:?}"),
            other => panic!("{text:?}: {other:?}"),
        }
    }
    assert!(
        Approvals::parse("x\n")
            .unwrap_err()
            .to_string()
            .contains("line 1")
    );
}

/// Catches a missing file being an error (a host with no approval is the normal host) and a file
/// anyone could have written being believed.
#[test]
fn the_record_is_read_like_a_declaration() {
    let scratch = Scratch::new("approvals-read");
    let path = scratch.path().join("approved");
    assert!(Approvals::read(&path, my_uid()).unwrap().is_empty());
    let d = digest("a");
    let file = scratch.file("approved", &format!("{d}\n"), 0o644);
    assert!(Approvals::read(&file, my_uid()).unwrap().contains(&d));
    // Not the trusted account's.
    assert!(matches!(
        Approvals::read(&file, my_uid() + 1).unwrap_err(),
        ApprovalError::Untrusted(_)
    ));
    // Writable by others.
    for mode in [0o664, 0o646, 0o666] {
        let file = scratch.file("loose", &format!("{d}\n"), mode);
        let error = Approvals::read(&file, my_uid()).unwrap_err();
        assert!(matches!(error, ApprovalError::Untrusted(_)), "{mode:o}");
        assert!(error.to_string().contains("cannot be trusted"));
    }
    // A link, and a directory.
    let link = scratch.path().join("link");
    symlink(&file, &link).unwrap();
    assert!(matches!(
        Approvals::read(&link, my_uid()).unwrap_err(),
        ApprovalError::Untrusted(_)
    ));
    assert!(matches!(
        Approvals::read(scratch.path(), my_uid()).unwrap_err(),
        ApprovalError::Untrusted(_)
    ));
}

/// Catches an approval that is not recorded, or recorded in a file that cannot be read back by
/// the rule that reads it.
#[test]
fn approving_records_the_digest_in_a_file_that_reads_back() {
    let scratch = Scratch::new("approve-new");
    let path = scratch.path().join("approved");
    let d = digest("tierstore declaration");
    assert_eq!(
        approve(&path, my_uid(), my_uid(), "tierstore", &d).unwrap(),
        Approved::Recorded
    );
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        format!("{d} tierstore\n")
    );
    let meta = fs::metadata(&path).unwrap();
    assert_eq!(meta.uid(), my_uid());
    assert_eq!(
        meta.mode() & 0o022,
        0,
        "the record must not be writable by others"
    );
    assert!(Approvals::read(&path, my_uid()).unwrap().contains(&d));
    assert!(
        !scratch.path().join("approved.new").exists(),
        "the new file is renamed over"
    );
}

/// Catches an earlier approval, a comment or an unfinished last line being lost when another is
/// recorded; and the same approval twice writing twice.
#[test]
fn approving_keeps_the_record_and_is_idempotent() {
    let scratch = Scratch::new("approve-more");
    let (a, b) = (digest("a"), digest("b"));
    let path = scratch.file("approved", &format!("# the fleet\n{a}"), 0o644);
    assert_eq!(
        approve(&path, my_uid(), my_uid(), "js-agent", &b).unwrap(),
        Approved::Recorded
    );
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        format!("# the fleet\n{a}\n{b} js-agent\n")
    );
    let before = fs::read(&path).unwrap();
    assert_eq!(
        approve(&path, my_uid(), my_uid(), "js-agent", &b).unwrap(),
        Approved::Already
    );
    assert_eq!(
        approve(&path, my_uid(), my_uid(), "other", &a).unwrap(),
        Approved::Already
    );
    assert_eq!(fs::read(&path).unwrap(), before);
    // A file that already ends its line gets no blank line.
    let c = digest("c");
    approve(&path, my_uid(), my_uid(), "kbf", &c).unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        format!("# the fleet\n{a}\n{b} js-agent\n{c} kbf\n")
    );
}

/// Catches an approval by an account that is not the trusted one (the file it wrote would be
/// the wrong owner's), and a service name that is no service name. Nothing is written.
#[test]
fn only_the_trusted_account_approves_and_only_for_a_service() {
    let scratch = Scratch::new("approve-refused");
    let path = scratch.path().join("approved");
    let d = digest("a");
    let error = approve(&path, my_uid() + 1, my_uid(), "tierstore", &d).unwrap_err();
    assert!(matches!(error, ApprovalError::NotTrustedAccount { .. }));
    assert!(error.to_string().contains("owns the approval file"));
    for name in ["", "Tier", "a b", "../x", &"a".repeat(65)] {
        let error = approve(&path, my_uid(), my_uid(), name, &d).unwrap_err();
        assert!(matches!(error, ApprovalError::ServiceName(_)), "{name:?}");
    }
    assert!(!path.exists());
}

/// Catches an untrusted record being copied into a trusted one: approving into a file others can
/// write, or one that is not text of digests, is refused and leaves the file as it was.
#[test]
fn approving_never_launders_an_untrusted_or_broken_record() {
    let scratch = Scratch::new("approve-launder");
    let d = digest("a");
    let loose = scratch.file("loose", "something\n", 0o666);
    assert!(matches!(
        approve(&loose, my_uid(), my_uid(), "tierstore", &d).unwrap_err(),
        ApprovalError::Untrusted(_)
    ));
    assert_eq!(fs::read_to_string(&loose).unwrap(), "something\n");
    let broken = scratch.file("broken", "not a digest\n", 0o644);
    assert!(matches!(
        approve(&broken, my_uid(), my_uid(), "tierstore", &d).unwrap_err(),
        ApprovalError::Line { .. }
    ));
    assert_eq!(fs::read_to_string(&broken).unwrap(), "not a digest\n");
    let binary = scratch.file("binary", "", 0o644);
    fs::write(&binary, [0xff, 0xfe]).unwrap();
    assert!(matches!(
        approve(&binary, my_uid(), my_uid(), "tierstore", &d).unwrap_err(),
        ApprovalError::Line { .. }
    ));
}

/// Catches a write that follows a link planted where the new file goes, or fails silently when
/// the directory is not there.
#[test]
fn the_new_file_is_not_written_through_a_link_and_a_missing_directory_is_an_error() {
    let scratch = Scratch::new("approve-write");
    let d = digest("a");
    let path = scratch.path().join("approved");
    let victim = scratch.file("victim", "keep\n", 0o644);
    symlink(&victim, scratch.path().join("approved.new")).unwrap();
    let error = approve(&path, my_uid(), my_uid(), "tierstore", &d).unwrap_err();
    assert!(matches!(error, ApprovalError::Write(_)));
    assert!(error.to_string().contains("cannot be written"));
    assert_eq!(fs::read_to_string(&victim).unwrap(), "keep\n");
    let missing = scratch.path().join("no-such-dir").join("approved");
    assert!(matches!(
        approve(&missing, my_uid(), my_uid(), "tierstore", &d).unwrap_err(),
        ApprovalError::Untrusted(_)
    ));
}

/// Catches a stale half-written file from an earlier crash blocking the next approval.
#[test]
fn a_stale_new_file_is_overwritten() {
    let scratch = Scratch::new("approve-stale");
    let d = digest("a");
    let path = scratch.path().join("approved");
    scratch.file(
        "approved.new",
        "garbage that is longer than the line we write\n",
        0o644,
    );
    approve(&path, my_uid(), my_uid(), "tierstore", &d).unwrap();
    assert_eq!(
        fs::read_to_string(&path).unwrap(),
        format!("{d} tierstore\n")
    );
}

/// Catches a record in a directory others can write, or that is not the trusted account's: the
/// file could be replaced there whatever its own mode is. Nothing is written.
#[test]
fn the_directory_of_the_record_must_be_the_trusted_accounts_and_closed() {
    let scratch = Scratch::new("approve-parent");
    let d = digest("a");
    for mode in [0o775, 0o757, 0o777] {
        let dir = scratch.dir("open", mode);
        let path = dir.join("approved");
        let error = approve(&path, my_uid(), my_uid(), "tierstore", &d).unwrap_err();
        assert!(matches!(error, ApprovalError::Untrusted(_)), "{mode:o}");
        assert!(!path.exists(), "{mode:o}");
    }
    let dir = scratch.dir("closed", 0o755);
    let path = dir.join("approved");
    assert!(approve(&path, my_uid() + 1, my_uid() + 1, "tierstore", &d).is_err());
    assert!(!path.exists());
    assert_eq!(
        approve(&path, my_uid(), my_uid(), "tierstore", &d).unwrap(),
        Approved::Recorded
    );
}

/// Catches two approvals made at once losing one: each reads the record, adds its line and
/// replaces the file, so without a lock the later replace drops the earlier line, or they
/// collide on the new file. Every approval from many threads is there afterwards.
#[test]
fn concurrent_approvals_all_land() {
    let scratch = Scratch::new("approve-concurrent");
    let path = scratch.path().join("approved");
    let digests: Vec<Digest> = (0..16)
        .map(|n| digest(&format!("declaration {n}")))
        .collect();
    std::thread::scope(|scope| {
        for d in &digests {
            let path = &path;
            scope.spawn(move || {
                approve(path, my_uid(), my_uid(), "tierstore", d)
                    .expect("the approval is recorded");
            });
        }
    });
    let approvals = Approvals::read(&path, my_uid()).unwrap();
    assert_eq!(approvals.len(), 16);
    for d in &digests {
        assert!(approvals.contains(d));
    }
}
