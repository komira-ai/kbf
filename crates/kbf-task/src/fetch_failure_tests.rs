//! Tests of copies, partial writes, skipped blobs and request times.

use super::*;

/// Hands out three bytes and then fails.
struct Breaks {
    sent: bool,
}

impl Read for Breaks {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.sent {
            return Err(std::io::Error::other("broken"));
        }
        self.sent = true;
        buf[..3].copy_from_slice(b"abc");
        Ok(3)
    }
}

/// Catches a partial file left at a path a copy was being made at: the copy fails after three
/// bytes, the error names the path, and nothing is left there.
#[test]
fn a_copy_that_fails_partway_is_removed() {
    let scratch_dir = scratch("copy-breaks");
    let path = scratch_dir.join("copy");
    let error = copy_from(&mut Breaks { sent: false }, &path, false).expect_err("broken");
    assert!(matches!(&error, FetchError::Io { .. }), "{error:?}");
    assert!(
        error
            .to_string()
            .starts_with(&format!("{}: ", path.display())),
        "{error}"
    );
    assert!(!path.exists());
}

/// Catches a file this code did not create being removed by a failed copy: the copy cannot create
/// its path because a file is there, and that file stays.
#[test]
fn a_copy_onto_an_existing_file_leaves_it() {
    let scratch_dir = scratch("copy-onto");
    let path = scratch_dir.join("there");
    std::fs::write(&path, b"mine").expect("pre-existing");
    let error = copy_from(&mut Breaks { sent: false }, &path, false).expect_err("exists");
    assert!(matches!(&error, FetchError::Io { .. }), "{error:?}");
    assert_eq!(std::fs::read(&path).expect("kept"), b"mine");
}

/// A source that sends the first three bytes of a blob and then fails.
struct Partial;

impl BlobSource for Partial {
    fn get(&self, digest: &Digest) -> Result<Vec<u8>, FetchError> {
        Err(FetchError::Source {
            digest: label(digest),
            why: "not whole".to_owned(),
        })
    }

    fn fetch_to(&self, digest: &Digest, out: &mut dyn Write) -> Result<(), FetchError> {
        out.write_all(b"hel").expect("take");
        self.get(digest).map(|_| ())
    }
}

/// Memory, except that the blob named by the second field comes through [`Partial`].
struct Mixed(Memory, Digest);

impl BlobSource for Mixed {
    fn get(&self, digest: &Digest) -> Result<Vec<u8>, FetchError> {
        self.0.get(digest)
    }

    fn fetch_to(&self, digest: &Digest, out: &mut dyn Write) -> Result<(), FetchError> {
        if *digest == self.1 {
            Partial.fetch_to(digest, out)
        } else {
            self.0.fetch_to(digest, out)
        }
    }
}

/// Catches a file left behind when the source fails after part of the body: no file at the path.
#[test]
fn a_source_that_fails_mid_stream_leaves_no_file() {
    let mut mem = Memory::default();
    let blob = mem.put(b"hello".to_vec());
    let root = mem.put_directory(&Directory {
        files: vec![file("f", &blob, false)],
        ..Directory::default()
    });
    let source = Mixed(mem, blob);
    let scratch_dir = scratch("midstream");
    let dir = scratch_dir.join("in");
    let error = materialize(&source, &root, &dir, 2).expect_err("partial");
    assert!(matches!(&error, FetchError::Source { .. }), "{error:?}");
    assert!(!dir.join("f").exists());
    assert!(source.get(&root).is_ok());
}

/// A batch source that never calls back for the digests it is given.
struct Skips(Memory);

impl BlobSource for Skips {
    fn get(&self, digest: &Digest) -> Result<Vec<u8>, FetchError> {
        self.0.get(digest)
    }

    fn fetch_many(&self, _: &[Digest], _: usize, _: &Each<'_>) -> Result<(), FetchError> {
        Ok(())
    }
}

/// Catches a source that skips a blob making the fetch panic or succeed: it fails as an invalid
/// tree.
#[test]
fn a_source_that_skips_a_blob_fails_the_fetch() {
    let (mem, root, _, _) = wide(2);
    let source = Skips(mem);
    assert!(source.get(&root).is_ok());
    let scratch_dir = scratch("skips");
    let error = materialize(&source, &root, &scratch_dir.join("in"), 4).expect_err("skipped");
    assert!(matches!(&error, FetchError::Invalid(_)), "{error:?}");
    assert!(error.to_string().contains("skipped"), "{error}");
}

/// Catches a request time that ignores the size or has no end: 60 s for a small blob, one more
/// second for each 8 MiB, and an hour at most.
#[test]
fn the_request_time_grows_with_the_blob_and_is_capped() {
    let secs = |size| request_timeout(size).as_secs();
    assert_eq!(secs(0), 60);
    assert_eq!(secs((8 << 20) - 1), 60);
    assert_eq!(secs(8 << 20), 61);
    assert_eq!(secs(800 << 20), 160);
    assert_eq!(secs(i64::MAX), 3600);
}

/// A source that serves what it is asked for but never calls back for the blobs in `skip`.
struct SkipsSome {
    mem: Memory,
    skip: Vec<String>,
}

impl BlobSource for SkipsSome {
    fn get(&self, digest: &Digest) -> Result<Vec<u8>, FetchError> {
        self.mem.get(digest)
    }

    fn fetch_many(&self, digests: &[Digest], _: usize, each: &Each<'_>) -> Result<(), FetchError> {
        for (index, digest) in digests.iter().enumerate() {
            if !self.skip.contains(&digest.hash) {
                each(index, &mut |out| self.fetch_to(digest, out))?;
            }
        }
        Ok(())
    }
}

/// Catches a source that skips one file blob while serving the rest being taken for a complete
/// fetch: the tree is read, one of two files is never delivered, and the fetch fails as invalid.
#[test]
fn a_source_that_skips_a_file_blob_fails_the_fetch() {
    let (mem, root, _, files) = wide(2);
    let source = SkipsSome {
        mem,
        skip: vec![files[1].clone()],
    };
    let scratch_dir = scratch("skips-file");
    let error = materialize(&source, &root, &scratch_dir.join("in"), 4).expect_err("skipped");
    assert!(matches!(&error, FetchError::Invalid(_)), "{error:?}");
    assert!(error.to_string().contains("skipped"), "{error}");
}
