//! `read_to` fails over as `get` does: a copy that is not the blob, that breaks part way
//! or that is not there sends the read to the next owner, the read-repair hook is told
//! what `get` tells it, and the file holds the blob or what it held before.

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::Duration;

use kbf_proto::reapi::Digest;
use kbf_transfer::{Cas, CasError, digest_of, label};

use super::owners::{Owners, Told, parsed};
use super::support::scratch;

/// What a file holds before a read writes into it: a read writes from the file's offset,
/// and must leave these bytes, and the offset, as they were when it fails.
const HEAD: &[u8] = b"head";

/// What one owner does when it is read.
#[derive(Clone, Copy, Debug)]
enum Serves {
    /// Sends the blob.
    Good,
    /// Sends bytes of the blob's length that are not the blob.
    Lie,
    /// Sends one byte fewer than the blob.
    Short,
    /// Sends the blob and more.
    Long,
    /// Sends one chunk of the blob, then fails.
    Breaks,
    /// Does not hold it.
    Missing,
    /// Is down.
    Down,
}

/// A blob of `len` bytes that are not all alike.
fn blob(len: usize) -> Vec<u8> {
    (0..len).map(|i| (i * 7 + 3) as u8).collect()
}

/// `blob` with its first byte changed.
fn lie(blob: &[u8]) -> Vec<u8> {
    let mut lie = blob.to_vec();
    lie[0] = lie[0].wrapping_add(1);
    lie
}

/// Owners of `digest` (the digest of `blob`) that do as `plan` says, the first in read
/// order first.
fn farm(blob: &[u8], plan: &[Serves]) -> Owners {
    let digest = digest_of(blob);
    let lie = lie(blob);
    let long = [blob, b"tail"].concat();
    let copies: Vec<&[u8]> = plan
        .iter()
        .map(|copy| match copy {
            Serves::Lie => &lie[..],
            Serves::Short => &blob[..blob.len() - 1],
            Serves::Long => &long[..],
            _ => blob,
        })
        .collect();
    let owners = Owners::serving(&digest, &copies);
    for (fake, copy) in owners.in_order(&digest).iter().zip(plan) {
        match copy {
            Serves::Breaks => fake.breaks.store(true, Ordering::SeqCst),
            Serves::Missing => fake.holds.store(false, Ordering::SeqCst),
            Serves::Down => fake.down.store(true, Ordering::SeqCst),
            _ => {}
        }
    }
    owners
}

/// A directory of its own for one read, so that a file left behind in it shows.
fn directory(name: &str) -> PathBuf {
    let dir = scratch().join(name);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).expect("an old directory");
    }
    std::fs::create_dir_all(&dir).expect("a directory");
    dir
}

/// The names of the files in `dir`.
fn listing(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("list")
        .map(|entry| {
            entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

/// `read_to` of `digest` into a new file `dir/blob` that holds [`HEAD`] first: what it
/// answered and what the file holds after the head, which must still be there.
async fn read_to(owners: &Owners, digest: &Digest, dir: &Path) -> (Result<(), CasError>, Vec<u8>) {
    let path = dir.join("blob");
    let mut file = tokio::fs::File::create(&path).await.expect("a file");
    tokio::io::AsyncWriteExt::write_all(&mut file, HEAD)
        .await
        .expect("head");
    let answer = owners.cas.read_to(digest, &mut file).await;
    drop(file);
    let held = std::fs::read(&path).expect("the file");
    assert_eq!(&held[..HEAD.len()], HEAD, "the bytes before the offset");
    (answer, held[HEAD.len()..].to_vec())
}

/// Catches a same-length lie written as the blob (the read checks nothing, or only the
/// length), a read that stops at the first owner, a bad copy left in the file under the
/// good one (no truncation, or truncation without going back to the offset) and a bad
/// copy the hook is not told about: the first owner lies, the second has the blob. The
/// file is the head and the blob, each owner was read once, and the hook was told once,
/// naming the first owner as the one without a good copy.
#[tokio::test]
async fn a_lying_first_owner_is_passed_over() {
    let blob = blob(40);
    let digest = digest_of(&blob);
    let owners = farm(&blob, &[Serves::Lie, Serves::Good]);
    let order = owners.order(&digest);

    let (answer, written) = read_to(&owners, &digest, &directory("failover-lie")).await;
    assert_eq!(answer, Ok(()));
    assert_eq!(written, blob);
    assert_eq!(owners.reads(&digest), [1, 1]);
    let told: Vec<Told> = vec![(parsed(&digest), order[1], vec![order[0]])];
    assert_eq!(owners.hook.told(), told);
}

/// Catches a lie answered, or left in the file, when no owner has the blob, and a temp
/// file left beside it: every one of three owners lies, the read is `Corrupt` naming the
/// first lie, each owner was read once, the file holds only the head, nothing else is in
/// its directory, and the hook is told nothing, as `get` tells it nothing: no owner had a
/// copy to repair from.
#[tokio::test]
async fn every_owner_lying_is_corrupt_and_leaves_the_file_as_it_was() {
    let blob = blob(40);
    let digest = digest_of(&blob);
    let owners = farm(&blob, &[Serves::Lie, Serves::Lie, Serves::Lie]);
    let dir = directory("failover-every-lie");

    let (answer, written) = read_to(&owners, &digest, &dir).await;
    assert_eq!(
        answer,
        Err(CasError::Corrupt(
            label(&digest),
            label(&digest_of(&lie(&blob)))
        ))
    );
    assert!(written.is_empty(), "{} bytes left", written.len());
    assert_eq!(listing(&dir), ["blob"]);
    assert_eq!(owners.reads(&digest), [1, 1, 1]);
    assert!(owners.hook.told().is_empty());
}

/// Catches a read that breaks part way taken as the end of the read, or its first chunk
/// left in the file: the first owner sends a chunk and fails, the second has the blob,
/// and the file is the blob. The hook is told nothing: an owner that broke is down, not
/// missing a copy.
#[tokio::test]
async fn an_owner_that_breaks_part_way_is_passed_over() {
    let blob = blob(40);
    let digest = digest_of(&blob);
    let owners = farm(&blob, &[Serves::Breaks, Serves::Good]);

    let (answer, written) = read_to(&owners, &digest, &directory("failover-breaks")).await;
    assert_eq!(answer, Ok(()));
    assert_eq!(written, blob);
    assert_eq!(owners.reads(&digest), [1, 1]);
    assert!(owners.hook.told().is_empty());
}

/// Catches a read into a file that gathers the blob in memory before it writes it, on the
/// way through a failover: the first owner lies, the second sends half the blob's 1024
/// chunks and waits. While it waits the file already holds the head and exactly that
/// half: the lie is gone and the good bytes are on disk before the blob is complete.
#[tokio::test]
async fn a_large_blob_streams_through_a_failover() {
    let blob = blob(4096);
    let digest = digest_of(&blob);
    let owners = farm(&blob, &[Serves::Lie, Serves::Good]);
    let dir = directory("failover-streams");
    let path = dir.join("blob");
    let half = owners.in_order(&digest)[1].pause_after(512);

    let want: Vec<u8> = [HEAD, &blob[..2048]].concat();
    let watch = async {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        let mut seen = false;
        while !seen && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(10)).await;
            seen = std::fs::read(&path).is_ok_and(|held| held == want);
        }
        half.add_permits(1);
        seen
    };
    let ((answer, written), seen) = tokio::join!(read_to(&owners, &digest, &dir), watch);
    assert!(
        seen,
        "the first half was not in the file while the rest was held back"
    );
    assert_eq!(answer, Ok(()));
    assert_eq!(written, blob);
}

/// Catches `read_to` and `get` disagreeing on the same owners: for each set of owners,
/// two farms set up alike, one read with `get` and one with `read_to`, give the same
/// answer (the bytes, or the same error), read each owner as often, and tell the hook the
/// same things. A failed `read_to` leaves the file as it was.
#[tokio::test]
async fn read_to_and_get_agree_on_every_set_of_owners() {
    use Serves::{Breaks, Down, Good, Lie, Long, Missing, Short};
    let blob = blob(40);
    let digest = digest_of(&blob);
    let plans: [&[Serves]; 10] = [
        &[Good, Good, Good],
        &[Lie, Good, Good],
        &[Lie, Lie, Lie],
        &[Short, Good, Good],
        &[Long, Lie, Good],
        &[Breaks, Good, Good],
        &[Missing, Good, Good],
        &[Down, Missing, Good],
        &[Missing, Missing, Missing],
        &[Down, Breaks, Short],
    ];
    for plan in plans {
        let by_get = farm(&blob, plan);
        let got = by_get.cas.get(&digest).await;
        let by_read_to = farm(&blob, plan);
        let dir = directory("failover-parity");
        let (answer, written) = read_to(&by_read_to, &digest, &dir).await;
        let read = answer.map(|()| written.clone());
        assert_eq!(read, got, "{plan:?}");
        if read.is_err() {
            assert!(written.is_empty(), "{plan:?}: {} bytes left", written.len());
        }
        assert_eq!(listing(&dir), ["blob"], "{plan:?}");
        assert_eq!(by_read_to.reads(&digest), by_get.reads(&digest), "{plan:?}");
        assert_eq!(by_read_to.hook.told(), by_get.hook.told(), "{plan:?}");
    }
}
