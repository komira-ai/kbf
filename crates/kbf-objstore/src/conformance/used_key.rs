//! The rule that a key, once written, never holds different bytes: a second write of the
//! same bytes is a retry and may be answered `Ok` or `AlreadyExists`; a write of any
//! other bytes (shorter, longer, or racing for the key) is refused with `AlreadyExists`
//! and the first bytes stay, byte for byte.

use bytes::Bytes;
use futures::future::join_all;

use super::{CheckResult, Outcome, Written, describe, key, pattern, put, range, read, streamed};
use crate::{Capabilities, KeyPrefix, ObjectKey, ObjectStore, ObjectStoreError};

/// How many distinct bodies race for one key.
const RACERS: u8 = 4;

/// Fails unless reading `k` whole gives exactly `want`: the read asks for more than
/// `want` holds, so a longer stored object shows as extra bytes.
async fn holds<S: ObjectStore>(
    store: &S,
    k: &ObjectKey,
    want: &Bytes,
    after: &str,
) -> Result<(), String> {
    let got = read(store, k, range(0, want.len() as u64 + 1024)).await?;
    ensure!(
        got == *want,
        "{k} after {after}: reads {} bytes that differ from the {} first written",
        got.len(),
        want.len()
    );
    Ok(())
}

/// `put_new` and `put_stream` of `body` onto `k`, in that order.
async fn put_both<S: ObjectStore>(
    store: &S,
    w: &Written,
    k: &ObjectKey,
    body: &Bytes,
) -> [(&'static str, Result<(), ObjectStoreError>); 2] {
    let new = w.put_new(store, k, body.clone(), None).await;
    let stream = w
        .put_stream(store, k, body.len() as u64, streamed(body, None))
        .await;
    [("put_new", new), ("put_stream", stream)]
}

pub(super) async fn used_key_keeps_its_bytes<S: ObjectStore>(
    store: &S,
    w: &Written,
    prefix: &KeyPrefix,
    caps: Capabilities,
) -> CheckResult {
    if !caps.conditional_put {
        return Ok(Outcome::Skipped(
            "the store does not claim conditional writes",
        ));
    }
    let k = key(prefix, "used-key")?;
    let first = pattern(64, 21);
    put(store, w, &k, &first).await?;

    // The same bytes again: a retry, never an error other than AlreadyExists.
    for (how, answer) in put_both(store, w, &k, &first).await {
        ensure!(
            matches!(answer, Ok(()) | Err(ObjectStoreError::AlreadyExists(_))),
            "{how} of the same bytes onto {k}: expected success or AlreadyExists, got {}",
            describe(&answer)
        );
        holds(store, &k, &first, &format!("{how} of the same bytes")).await?;
    }

    // Other bytes of another length, shorter and longer: refused, and the first stay.
    for (what, other) in [("shorter", pattern(32, 22)), ("longer", pattern(100, 23))] {
        for (how, answer) in put_both(store, w, &k, &other).await {
            ensure!(
                matches!(answer, Err(ObjectStoreError::AlreadyExists(_))),
                "{how} of {what} bytes onto {k}: expected AlreadyExists, got {}",
                describe(&answer)
            );
            holds(store, &k, &first, &format!("{how} of {what} bytes")).await?;
        }
    }

    // Writers racing for one fresh key: exactly one wins and its bytes are the key's.
    let raced = key(prefix, "used-key-race")?;
    let bodies: Vec<Bytes> = (0..RACERS)
        .map(|i| pattern(16 + 7 * usize::from(i), 30 + i))
        .collect();
    let answers = join_all(
        bodies
            .iter()
            .map(|b| w.put_new(store, &raced, b.clone(), None)),
    )
    .await;
    let mut winners = Vec::new();
    for (i, answer) in answers.iter().enumerate() {
        match answer {
            Ok(()) => winners.push(i),
            Err(ObjectStoreError::AlreadyExists(_)) => {}
            other => {
                return Err(format!(
                    "a put_new racing for {raced}: expected Ok or AlreadyExists, got {}",
                    describe(other)
                ));
            }
        }
    }
    ensure!(
        winners.len() == 1,
        "{} of {RACERS} put_new calls racing for {raced} were answered Ok, expected exactly one",
        winners.len()
    );
    holds(store, &raced, &bodies[winners[0]], "a race for the key").await?;
    Ok(Outcome::Passed)
}
