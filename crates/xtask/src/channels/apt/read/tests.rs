use super::*;
use crate::channels::apt::testing::*;

fn key(rel: &str) -> String {
    format!("apt/dists/stable/{rel}")
}

fn out(dir: &Path) -> std::path::PathBuf {
    dir.join("current")
}

#[test]
fn the_index_and_the_etag_of_each_object_are_copied_and_a_missing_object_is_dash() {
    // Catches: an index read through anything but S3, an ETag kept for the wrong file, and an
    // object that does not exist written as if it had an ETag.
    let dir = scratch("read-all");
    let aws = FakeAws::new();
    let release =
        "Origin: o\nSHA256:\n aa 1 main/binary-amd64/Packages\n bb 1 main/binary-s390x/Packages\n";
    aws.put(&key("Release"), release.as_bytes());
    aws.put(&key("main/binary-amd64/Packages"), b"p");
    aws.put(&key("main/binary-s390x/Packages.gz"), b"z");
    aws.put(&key("InRelease"), b"signed");
    let said = read(&aws, Suite::Stable, BUCKET, &out(&dir)).expect("read");
    assert_eq!(
        said[0],
        "read main/binary-amd64/Packages: \"".to_owned() + &etag(b"p")[1..]
    );
    let etags = std::fs::read_to_string(out(&dir).join("etags.txt")).expect("etags");
    let want = [
        ("main/binary-amd64/Packages", etag(b"p")),
        ("main/binary-amd64/Packages.gz", "-".to_owned()),
        ("main/binary-arm64/Packages", "-".to_owned()),
        ("main/binary-arm64/Packages.gz", "-".to_owned()),
        ("main/binary-s390x/Packages", "-".to_owned()),
        ("main/binary-s390x/Packages.gz", etag(b"z")),
        ("Release", etag(release.as_bytes())),
        ("Release.gpg", "-".to_owned()),
        ("InRelease", etag(b"signed")),
    ];
    let text: String = want.iter().map(|(p, e)| format!("{p}\t{e}\n")).collect();
    assert_eq!(etags, text);
    let root = out(&dir).join("dists/stable");
    assert_eq!(
        std::fs::read(root.join("main/binary-amd64/Packages")).expect("copy"),
        b"p"
    );
    assert!(!root.join("main/binary-arm64/Packages").exists());
    assert!(said.contains(&"read Release.gpg: does not exist yet".to_owned()));
    // Every read names the bucket and goes to S3.
    for call in aws.calls_of("get-object") {
        assert_eq!(flag(&call, "--bucket").as_deref(), Some(BUCKET));
    }
}

#[test]
fn an_empty_repository_reads_as_the_defaults_with_every_object_absent() {
    // Catches: a first publish failing because there is nothing to read.
    let dir = scratch("read-empty");
    let aws = FakeAws::new();
    let said = read(&aws, Suite::Stable, BUCKET, &out(&dir)).expect("read");
    assert_eq!(said.len(), 7);
    let etags = std::fs::read_to_string(out(&dir).join("etags.txt")).expect("etags");
    assert_eq!(etags.lines().filter(|l| l.ends_with("\t-")).count(), 7);
}

#[test]
fn a_denied_read_is_an_absent_object_the_publish_role_may_not_list() {
    // Catches: a missing key that S3 answers with AccessDenied (no list permission) failing the job.
    let dir = scratch("read-denied");
    let aws = FakeAws::new();
    *aws.missing.borrow_mut() = DENIED.to_owned();
    assert!(read(&aws, Suite::Stable, BUCKET, &out(&dir)).is_ok());
}

#[test]
fn another_failure_a_missing_etag_and_a_bad_bucket_stop_the_read() {
    // Catches: an index built from a half-read repository.
    let dir = scratch("read-fail");
    let aws = FakeAws::new();
    aws.refuse_when("get-object", "could not connect to the endpoint");
    let e = read(&aws, Suite::Stable, BUCKET, &out(&dir))
        .unwrap_err()
        .to_string();
    assert_eq!(
        e,
        "get-object apt/dists/stable/Release failed: could not connect to the endpoint"
    );
    let aws = FakeAws::new();
    aws.put(&key("Release"), b"x");
    for stdout in [
        "not json",
        "{}",
        "{\"ETag\": 5}",
        "{\"ETag\": \"\"}",
        "{\"ETag\": \"a b\"}",
    ] {
        *aws.stdout.borrow_mut() = Some(stdout.to_owned());
        let e = read(&aws, Suite::Stable, BUCKET, &out(&dir))
            .unwrap_err()
            .to_string();
        assert_eq!(
            e, "get-object apt/dists/stable/Release: no ETag in the answer",
            "{stdout}"
        );
    }
    assert!(matches!(
        read(&aws, Suite::Stable, "Bad Bucket", &out(&dir)),
        Err(Error::Usage(_))
    ));
}

#[test]
fn a_destination_that_cannot_be_made_or_written_is_an_error_naming_it() {
    // Catches: a read that reports success with nothing on disk.
    let dir = scratch("read-disk");
    let file = dir.join("file");
    std::fs::write(&file, "x").expect("file");
    let aws = FakeAws::new();
    let e = read(&aws, Suite::Stable, BUCKET, &file.join("below"))
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("create "), "{e}");
    // The index is read, but etags.txt cannot be written where a directory stands.
    let out = dir.join("out");
    std::fs::create_dir_all(out.join("etags.txt")).expect("dir");
    let e = read(&aws, Suite::Stable, BUCKET, &out)
        .unwrap_err()
        .to_string();
    assert!(e.starts_with("write "), "{e}");
}
