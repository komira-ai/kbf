use std::io::Read;

use kbf_outputs::CHUNK_BYTES;
use kbf_testkit::dir::TestDir;

use super::*;

/// `len` bytes that differ from place to place.
fn pattern(len: usize) -> Vec<u8> {
    (0..len)
        .map(|i| u8::try_from((i * 13 + i / 97) % 256).expect("a byte"))
        .collect()
}

/// Records a small file (held in memory) and a large one (held as a descriptor).
async fn record(name: &str) -> (TestDir, Outputs, Vec<u8>, Vec<u8>) {
    let dir = TestDir::under(std::env::temp_dir(), &format!("kbf-task-reader-{name}"));
    let small = pattern(100);
    let big = pattern(CHUNK_BYTES + 1000);
    std::fs::write(dir.join("small"), &small).expect("small");
    std::fs::write(dir.join("big"), &big).expect("big");
    std::fs::write(dir.join("out"), b"o").expect("out");
    std::fs::write(dir.join("err"), b"e").expect("err");
    let paths = vec!["small".to_owned(), "big".to_owned()];
    let outputs = collect_outputs(
        &dir,
        "",
        &paths,
        &dir.join("out"),
        &dir.join("err"),
        OutputLimits::default(),
        u64::MAX,
    )
    .await
    .expect("collected");
    (dir, outputs, small, big)
}

fn digest_of_output(outputs: &Outputs, path: &str) -> Digest {
    outputs
        .result
        .output_files
        .iter()
        .find(|f| f.path == path)
        .and_then(|f| f.digest.clone())
        .expect("an output")
}

/// Catches a reader that is not the blob's bytes, from the first byte, for both ways a blob is
/// held: a small file in memory and a large one as a descriptor read in pieces (the buffer is
/// smaller than the blob, so it takes many reads), and a second reader starting over, which is
/// what a retried PUT needs.
#[tokio::test]
async fn a_reader_streams_the_blob_from_its_first_byte_each_time() {
    let (_dir, outputs, small, big) = record("whole").await;
    for (path, bytes) in [("small", small), ("big", big)] {
        let digest = digest_of_output(&outputs, path);
        for _ in 0..2 {
            let mut reader = outputs.blobs.reader(&digest).expect("reader");
            let mut got = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = reader.read(&mut chunk).expect("read");
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&chunk[..n]);
            }
            assert!(got == bytes, "{path} differs");
        }
    }
}

/// Catches a reader that reads past the blob when the buffer is larger than what is left, and
/// one that reads a big blob in one piece: a buffer bigger than the whole blob gets exactly the
/// blob, then end of file.
#[tokio::test]
async fn a_reader_stops_at_the_end_of_the_blob() {
    let (_dir, outputs, _, big) = record("end").await;
    let digest = digest_of_output(&outputs, "big");
    let mut reader = outputs.blobs.reader(&digest).expect("reader");
    let mut buf = vec![0u8; big.len() + 4096];
    let mut total = 0;
    loop {
        let n = reader.read(&mut buf[total..]).expect("read");
        if n == 0 {
            break;
        }
        total += n;
    }
    assert_eq!(total, big.len());
    assert_eq!(reader.read(&mut buf).expect("read"), 0);
}

/// Catches a reader for a blob that was never recorded being made, which would send nothing as
/// if it were the blob.
#[tokio::test]
async fn a_reader_for_an_unrecorded_blob_is_not_found() {
    let (_dir, outputs, _, _) = record("absent").await;
    let absent = Digest {
        hash: "0".repeat(64),
        size_bytes: 5,
    };
    let err = outputs.blobs.reader(&absent).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
}

/// Catches `matches` not telling a changed blob from the recorded one: after a full read of the
/// recorded bytes it is true, and after the file changed in place (same size) it is false.
#[tokio::test]
async fn a_reader_knows_whether_the_bytes_it_read_are_the_blob() {
    let (dir, outputs, _, _) = record("matches").await;
    for path in ["small", "big"] {
        let digest = digest_of_output(&outputs, path);
        let mut reader = outputs.blobs.reader(&digest).expect("reader");
        std::io::copy(&mut reader, &mut std::io::sink()).expect("read");
        assert!(reader.matches(&digest));
    }
    let digest = digest_of_output(&outputs, "big");
    crate::task::recording_sink::flip_first_byte(&dir.join("big"));
    let mut reader = outputs.blobs.reader(&digest).expect("reader");
    std::io::copy(&mut reader, &mut std::io::sink()).expect("read");
    assert!(!reader.matches(&digest));
}
