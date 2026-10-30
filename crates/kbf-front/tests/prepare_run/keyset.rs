//! `prepare_run`'s key set: N pack grants and M large grants for one run, each key minted for its
//! own slot and each write presigned for exactly its key, packs with the pack credential and
//! large objects with the large one, all good until the same second.

use kbf_front::prepare_run::{LargeTarget, PutGrant};
use kbf_pack::{MAX_LARGE, MAX_PACKS, MAX_RECORD_BYTES, Slot, TaskPackKey};

use super::*;

const LARGE_KEY_ID: &str = "large-presign";
const LARGE_KEY_SECRET: &str = "the large presign secret";
const LARGE_CAP: u64 = 512 << 20;

/// A config for runs of `packs` pack keys and `large` large keys.
fn sized(packs: u8, large: u8) -> RunPrepConfig {
    let mut sized = config();
    sized.max_packs = packs;
    sized.large = Some(LargeTarget {
        key: AccessKey::new(LARGE_KEY_ID, LARGE_KEY_SECRET),
        max_objects: large,
        max_bytes: LARGE_CAP,
    });
    sized
}

/// The inputs of a run of the standard Action prepared under `config`.
async fn run_of(config: RunPrepConfig) -> (Parts, kbf_front::prepare_run::RunInputs) {
    let parts = Parts::new();
    let store = Watched::holding(parts.blobs.clone()).await;
    let inputs = prep_with(&store, config)
        .prepare_run(LEASE, &parts.action, now())
        .await
        .expect("prepared");
    (parts, inputs)
}

/// The write the tier store's own signer makes for `key` with `signer`, good for the standard
/// wall and slack.
fn signed(signer: &AccessKey, key: &str) -> String {
    presign_put(
        signer,
        &PresignRequest {
            endpoint: ENDPOINT,
            region: REGION,
            bucket: BUCKET,
            key,
            expires_secs: 7_800,
            now: NOW_SECS * 1000,
        },
    )
    .expect("signed")
}

fn slot_of(grant: &PutGrant) -> Slot {
    TaskPackKey::parse(grant.key.key())
        .expect("a task pack key")
        .slot()
}

/// Asserts that a run gets N pack grants and M large grants, in slot order, and that every key
/// verifies with the mint for the lease and the Action and names the slot it was minted for.
#[tokio::test]
async fn a_run_gets_n_pack_grants_and_m_large_grants_whose_keys_verify_for_their_slots() {
    let (parts, inputs) = run_of(sized(3, 2)).await;
    assert_eq!(inputs.packs.len(), 3);
    assert_eq!(inputs.large.len(), 2);
    let slots: Vec<Slot> = inputs.packs.iter().map(slot_of).collect();
    assert_eq!(slots, [Slot::Pack(0), Slot::Pack(1), Slot::Pack(2)]);
    let slots: Vec<Slot> = inputs.large.iter().map(slot_of).collect();
    assert_eq!(slots, [Slot::Large(0), Slot::Large(1)]);
    let mint = PackKeyMint::new(MINT_SECRET).expect("a mint");
    let all = inputs.keys();
    assert_eq!(all.len(), 5);
    for key in &all {
        assert_eq!(key.build(), LEASE);
        assert_eq!(mint.verify(key, &parts.action).expect("verified"), now());
        assert!(
            mint.verify(key, &blob("another action").0).is_err(),
            "{} is for this action only",
            key.key()
        );
    }
    let distinct: BTreeSet<&str> = all.iter().map(|key| key.key()).collect();
    assert_eq!(distinct.len(), 5, "every slot has a key of its own");
}

/// Asserts that the key set is the one the mint makes for the run (packs, then large objects),
/// in that order: what ingest is given is what was minted.
#[tokio::test]
async fn the_keys_are_the_mints_set_packs_then_large_in_slot_order() {
    let (parts, inputs) = run_of(sized(2, 3)).await;
    let mint = PackKeyMint::new(MINT_SECRET).expect("a mint");
    let set = mint
        .mint_set(LEASE, &parts.action, now(), 2, 3)
        .expect("a set");
    assert_eq!(inputs.keys(), set);
}

/// Asserts that each URL is a presigned `PUT` of exactly its own key: it equals the URL the tier
/// store's signer makes for that key, so its verifier recomputes the same signature, and no
/// URL is good for another key's slot.
#[tokio::test]
async fn each_url_is_presigned_for_exactly_its_own_key() {
    let (_, inputs) = run_of(sized(4, 3)).await;
    let pack = AccessKey::new(KEY_ID, KEY_SECRET);
    let large = AccessKey::new(LARGE_KEY_ID, LARGE_KEY_SECRET);
    for grant in &inputs.packs {
        assert_eq!(grant.put_url, signed(&pack, grant.key.key()));
        let (path, _) = grant.put_url.split_once('?').expect("a query");
        assert_eq!(path, format!("{ENDPOINT}/{BUCKET}/{}", grant.key.key()));
    }
    for grant in &inputs.large {
        assert_eq!(grant.put_url, signed(&large, grant.key.key()));
        let (path, _) = grant.put_url.split_once('?').expect("a query");
        assert_eq!(path, format!("{ENDPOINT}/{BUCKET}/{}", grant.key.key()));
    }
}

/// Asserts that the large URLs carry the large credential's access key id and the pack URLs the
/// pack credential's: a pack URL is not good under the large prefix, nor the reverse.
#[tokio::test]
async fn large_urls_are_signed_with_the_large_key_and_pack_urls_with_the_pack_key() {
    let (_, inputs) = run_of(sized(2, 2)).await;
    let credential = |url: &str| {
        let (_, query) = url.split_once('?').expect("a query");
        let pair = query
            .split('&')
            .find(|pair| pair.starts_with("X-Amz-Credential="));
        let scope = pair
            .expect("a credential")
            .trim_start_matches("X-Amz-Credential=");
        scope.split("%2F").next().expect("an id").to_owned()
    };
    for grant in &inputs.packs {
        assert_eq!(credential(&grant.put_url), KEY_ID);
    }
    for grant in &inputs.large {
        assert_eq!(credential(&grant.put_url), LARGE_KEY_ID);
    }
    let pack = AccessKey::new(KEY_ID, KEY_SECRET);
    assert_ne!(
        inputs.large[0].put_url,
        signed(&pack, inputs.large[0].key.key()),
        "a large key signed with the pack credential is another URL"
    );
}

/// Asserts that every URL of a run is good for the same time: the wall plus the slack, from the
/// same second, and that the read grant ends with them.
#[tokio::test]
async fn every_url_has_the_same_expiry() {
    let (_, inputs) = run_of(sized(8, 8)).await;
    assert_eq!(inputs.packs.len() + inputs.large.len(), 16);
    for grant in inputs.packs.iter().chain(&inputs.large) {
        let (_, query) = grant.put_url.split_once('?').expect("a query");
        let pairs: Vec<&str> = query.split('&').collect();
        assert!(pairs.contains(&"X-Amz-Expires=7800"), "{}", grant.key.key());
        assert!(pairs.contains(&"X-Amz-Date=20260921T141400Z"));
    }
    assert_eq!(inputs.read_grant.not_after, NOW_SECS + 7_800);

    let parts = Parts::new();
    let store = Watched::holding(parts.blobs.clone()).await;
    let capped = prep_with(&store, sized(2, 2))
        .prepare_run_for(LEASE, &parts.action, now(), MAX_EXPIRY - SLACK)
        .await
        .expect("prepared");
    for grant in capped.packs.iter().chain(&capped.large) {
        assert!(grant.put_url.contains("X-Amz-Expires=14400"));
    }
}

/// Asserts that a preparer with no large credential gives no large keys, and that one whose
/// large target allows none gives none either.
#[tokio::test]
async fn m_is_zero_without_the_large_credential_or_when_none_are_allowed() {
    let mut without = config();
    without.max_packs = 5;
    let (_, inputs) = run_of(without).await;
    assert_eq!(inputs.packs.len(), 5);
    assert!(inputs.large.is_empty());
    assert_eq!(inputs.keys().len(), 5);

    let (_, inputs) = run_of(sized(2, 0)).await;
    assert_eq!(inputs.packs.len(), 2);
    assert!(inputs.large.is_empty());
}

/// Asserts that `key`, `put_url` and `packs[0]` are one and the same grant, which is what a
/// caller that writes one pack reads, and that the first key of the set is pack 0.
#[tokio::test]
async fn the_v1_fields_are_the_first_pack() {
    for config in [config(), sized(4, 4)] {
        let (_, inputs) = run_of(config).await;
        assert_eq!(inputs.key, inputs.packs[0].key);
        assert_eq!(inputs.put_url, inputs.packs[0].put_url);
        assert_eq!(inputs.keys()[0], inputs.key);
        assert_eq!(slot_of(&inputs.packs[0]), Slot::Pack(0));
        assert!(inputs.key.key().ends_with("-0"));
    }
}

/// Asserts that the debug text of a run and of a grant shows the keys and no signature.
#[tokio::test]
async fn the_debug_of_a_grant_shows_the_key_and_no_url() {
    let (_, inputs) = run_of(sized(2, 1)).await;
    let shown = format!("{inputs:?} {:?}", inputs.large[0]);
    assert!(!shown.contains("X-Amz-Signature"), "{shown}");
    assert!(!shown.contains("X-Amz-Credential"), "{shown}");
    assert!(shown.contains(inputs.large[0].key.key()));
    assert!(shown.contains(inputs.packs[1].key.key()));
}

/// Asserts what a preparer refuses at start about the key set: no pack, more than 8 packs, more
/// than 8 large objects, and a large cap that is not above a pack record's.
#[tokio::test]
async fn a_key_set_the_wire_cannot_carry_is_refused_at_start() {
    let parts = Parts::new();
    let store = Watched::holding(parts.blobs.clone()).await;
    let start = |config| {
        let storage: Arc<dyn Storage> = store.clone();
        RunPrep::new(Backend::remote(storage), config)
    };
    for packs in [0, MAX_PACKS + 1] {
        let error = start(sized(packs, 1)).expect_err("refused");
        assert_eq!(error, ConfigError::Packs(packs));
    }
    let error = start(sized(1, MAX_LARGE + 1)).expect_err("refused");
    assert_eq!(error, ConfigError::LargeObjects(MAX_LARGE + 1));
    for cap in [0, 1, MAX_RECORD_BYTES] {
        let mut small = sized(1, 1);
        small.large.as_mut().expect("large").max_bytes = cap;
        assert_eq!(
            start(small).expect_err("refused"),
            ConfigError::LargeCap(cap)
        );
    }
    let mut edge = sized(MAX_PACKS, MAX_LARGE);
    edge.large.as_mut().expect("large").max_bytes = MAX_RECORD_BYTES + 1;
    let prep = start(edge).expect("the edge is allowed");
    assert_eq!(prep.large_max_bytes(), Some(MAX_RECORD_BYTES + 1));
    assert_eq!(start(config()).expect("no large").large_max_bytes(), None);
}
