//! One script of reads, failures and writes, said line by line: run on the frontend's plane as
//! the daemon has it without the flag, and on the tierstore plane over a tierstore that serves
//! nothing (down, missing the packs, or holding damaged copies), the transcripts are the golden's,
//! byte for byte. What the frontend answers, what reaches a file, what is written and every call
//! made on the frontend are in it.

use std::fmt::Write as _;

use super::*;

/// The inputs the script reads, which the frontend holds.
const INPUTS: [&[u8]; 2] = [b"input one", b"input two"];

/// The transcript's line for `result`: the bytes as text, or the error.
fn said<T: AsRef<[u8]>>(result: &Result<T, CasError>) -> String {
    match result {
        Ok(bytes) => format!("Ok({:?})", String::from_utf8_lossy(bytes.as_ref())),
        Err(error) => format!("Err({error:?})"),
    }
}

/// `plane` read and written by the script, over `front` (the frontend's plane, which is `plane`
/// itself or the plane under it), with scratch files in `dir`; what it said.
async fn script<C: Cas>(plane: &Arc<C>, front: &Front, dir: &Path) -> String {
    let mut out = String::new();
    let inputs: Vec<Digest> = INPUTS.iter().map(|bytes| digest_of(bytes)).collect();
    let absent = digest_of(b"absent");
    let put = plane.put(b"an output".to_vec()).await;
    let _ = writeln!(
        out,
        "put an output -> {}",
        put.map(|d| label(&d)).unwrap_or_else(|e| format!("{e:?}"))
    );
    let asked = [inputs[0].clone(), absent.clone(), digest_of(b"an output")];
    let missing: Vec<String> = plane.missing(&asked).await.iter().map(label).collect();
    let _ = writeln!(out, "missing -> {missing:?}");
    for digest in inputs.iter().chain([&absent]) {
        let _ = writeln!(
            out,
            "get {} -> {}",
            label(digest),
            said(&plane.get(digest).await)
        );
        let path = dir.join("read");
        let mut file = tokio::fs::File::create(&path).await.expect("create");
        file.write_all(b"head:").await.expect("head");
        file.flush().await.expect("flush");
        let read = plane.read_to(digest, &mut file).await;
        let held = std::fs::read(&path).expect("read back");
        let _ = writeln!(
            out,
            "read_to {} -> {}, file {:?}",
            label(digest),
            said(&read.map(|()| b"")),
            String::from_utf8_lossy(&held)
        );
    }
    let failures = [
        CasError::Unavailable("front".to_owned(), "the frontend is down".to_owned()),
        CasError::Corrupt("front".to_owned(), "another digest".to_owned()),
    ];
    for failure in failures {
        front.fail(Some(failure));
        let _ = writeln!(
            out,
            "failing: get {} -> {}",
            label(&inputs[1]),
            said(&plane.get(&inputs[1]).await)
        );
    }
    front.fail(None);
    let lease = plane.lease(&kbf_types::LeaseId::new(3, 4));
    let _ = writeln!(
        out,
        "lease get {} -> {}",
        label(&inputs[0]),
        said(&lease.get(&inputs[0]).await)
    );
    let _ = writeln!(out, "frontend calls {:?}", front.calls());
    out
}

/// The golden: the transcript of the frontend's plane, as the daemon reads it without the flag.
const GOLDEN: &str = include_str!("../../../../tests/golden/plane-without-tierstore.txt");

/// A frontend's plane holding the inputs.
fn front() -> Front {
    let front = Front::default();
    for input in INPUTS {
        front.insert(input);
    }
    front
}

/// Asserts that the frontend's plane, which is the daemon's without the flag, says the golden
/// transcript byte for byte.
#[tokio::test]
async fn without_the_flag_the_plane_says_the_golden_transcript() {
    let dir = scratch("tierstore-transcript-plain");
    let front = front();
    let plane = Arc::new(front.clone());
    assert_eq!(script(&plane, &front, &dir).await, GOLDEN);
    kbf_outputs::remove_tree(&dir).expect("clean");
}

/// How the tierstore of a run of the script fails to serve the inputs.
#[derive(Clone, Copy, Debug)]
enum Unserved {
    /// It is down.
    Down,
    /// It does not have their packs.
    Missing,
    /// It holds other bytes where their packs are.
    Damaged,
}

/// Asserts that over a tierstore that serves none of the inputs (down, missing their packs, or
/// holding damaged copies), the plane says the golden transcript byte for byte: every answer,
/// every byte that reaches a file, every write and every call on the frontend is the same as
/// without the flag. Each run did ask the tierstore.
/// Catches a tierstore plane whose degraded behaviour differs from the daemon's without it.
#[tokio::test]
async fn over_a_tierstore_that_serves_nothing_the_plane_says_the_same_transcript() {
    for unserved in [Unserved::Down, Unserved::Missing, Unserved::Damaged] {
        let dir = scratch(&format!("tierstore-transcript-{unserved:?}"));
        let rig = Rig::new();
        for (i, input) in INPUTS.iter().enumerate() {
            let key = format!("cas/d/node1/boot1/{:016x}", i + 1);
            let stored = match unserved {
                Unserved::Down => Some(*input),
                Unserved::Missing => None,
                Unserved::Damaged => Some(if i == 0 {
                    b"input 0ne".as_slice()
                } else {
                    b"input tw0"
                }),
            };
            rig.record(&key, input, stored).await;
            rig.front.insert(input);
        }
        if matches!(unserved, Unserved::Down) {
            rig.tier.set(Mode::Down);
        }
        assert_eq!(
            script(&rig.cas, &rig.front, &dir).await,
            GOLDEN,
            "{unserved:?}"
        );
        assert!(rig.reads() > 0, "{unserved:?}: the tierstore was not asked");
        kbf_outputs::remove_tree(&dir).expect("clean");
    }
}
