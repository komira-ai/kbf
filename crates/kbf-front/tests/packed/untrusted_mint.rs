//! The minting of the key a task writes its pack under (`PackKeyMint`): a key is the frontend's
//! HMAC over the lease, the action and the minute, so a key it did not mint is refused before the
//! store is asked. Every test says the behaviour it asserts.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hmac::{Hmac, KeyInit, Mac};
use kbf_front::{IssuedPackKey, IssuedPutDir, MIN_SECRET_BYTES, MintError, PackKeyMint};
use kbf_pack::{MAX_LARGE, MAX_PACKS, Slot, mac_input_put_dir, mac_input_v1, minute_of};
use kbf_types::Digest;
use sha2::Sha256;

use crate::support::blob;
use crate::untrusted_task::{LEASE, SECRET, now};

fn mint() -> PackKeyMint {
    PackKeyMint::new(SECRET).expect("a mint")
}

fn action() -> Digest {
    blob("an action").0
}

/// Asserts that minting is deterministic and that each input changes the key: the lease, the
/// action, the minute and the secret; and that two times in one minute are one key.
#[test]
fn minting_is_deterministic_and_each_input_changes_the_key() {
    let key = |mint: &PackKeyMint, lease: &str, action: &Digest, at: SystemTime| {
        mint.mint(lease, action, at, Slot::Pack(0))
            .expect("a key")
            .key()
            .to_owned()
    };
    let base = key(&mint(), LEASE, &action(), now());

    assert_eq!(base, key(&mint(), LEASE, &action(), now()));
    let same_minute = now() + Duration::from_secs(59);
    assert_eq!(base, key(&mint(), LEASE, &action(), same_minute));
    let next_minute = now() + Duration::from_secs(60);
    let other_secret = PackKeyMint::new(&[9; 32]).expect("a mint");
    let changed = [
        key(&mint(), "another", &action(), now()),
        key(&mint(), LEASE, &blob("other").0, now()),
        key(&mint(), LEASE, &action(), next_minute),
        key(&other_secret, LEASE, &action(), now()),
    ];
    for other in &changed {
        assert_ne!(&base, other);
    }
    assert!(base.starts_with(&format!("cas/b/{LEASE}/")), "{base}");
    assert!(base.ends_with("-0"), "{base}");
}

/// Asserts that a minted key verifies and says the minute it was minted in (the start of it), and
/// that a key with another action, another secret or a changed MAC does not verify, nor does a key
/// of another shape. Catches a verification that compares less than the whole MAC.
#[test]
fn a_minted_key_verifies_and_names_its_minute() {
    let key = mint()
        .mint(
            LEASE,
            &action(),
            now() + Duration::from_secs(7),
            Slot::Pack(0),
        )
        .expect("a key");

    assert_eq!(mint().verify(&key, &action()).expect("minted here"), now());

    let other_secret = PackKeyMint::new(&[9; 32]).expect("a mint");
    let nonce = key.key().rsplit('/').next().expect("a nonce");
    let digit = if nonce.starts_with('0') { "1" } else { "0" };
    let flipped = format!("{digit}{}", &nonce[1..]);
    let keys = [
        (key.clone(), blob("other").0, &mint()),
        (key.clone(), action(), &other_secret),
        (
            IssuedPackKey::new(LEASE, &flipped).expect("a key"),
            action(),
            &mint(),
        ),
        (
            IssuedPackKey::new(LEASE, "n0nce1").expect("a key"),
            action(),
            &mint(),
        ),
    ];
    for (key, action, mint) in keys {
        let refused = mint.verify(&key, &action).expect_err("not minted here");
        assert!(matches!(refused, MintError::NotMinted(_)), "{refused:?}");
    }
}

/// Asserts what minting refuses: a secret under the least, a clock before the epoch, a time past
/// the last minute a key can name, and a lease that cannot be a build; and that the secret is not
/// in the mint's Debug text.
#[test]
fn a_mint_refuses_what_cannot_be_a_key_and_hides_its_secret() {
    let short = PackKeyMint::new(&[1; MIN_SECRET_BYTES - 1]).err();
    assert_eq!(short, Some(MintError::ShortSecret(MIN_SECRET_BYTES - 1)));
    let before = UNIX_EPOCH - Duration::from_secs(1);
    assert_eq!(
        mint().mint(LEASE, &action(), before, Slot::Pack(0)).err(),
        Some(MintError::Clock)
    );
    let far = UNIX_EPOCH + Duration::from_secs(60 * (1 << 32));
    let past = mint()
        .mint(LEASE, &action(), far, Slot::Pack(0))
        .expect_err("past the last minute");
    assert!(matches!(past, MintError::Key(_)), "{past:?}");
    let bad = mint()
        .mint("not/a/build", &action(), now(), Slot::Pack(0))
        .expect_err("a bad lease");
    assert!(matches!(bad, MintError::Key(_)), "{bad:?}");
    let slot = mint()
        .mint(LEASE, &action(), now(), Slot::Pack(MAX_PACKS))
        .expect_err("a slot a run does not have");
    assert!(matches!(slot, MintError::Key(_)), "{slot:?}");
    let debug = format!("{:?}", mint());
    assert!(!debug.contains("0123456789abcdef"), "{debug}");
}

/// The key a mint made before the slot was in the MAC would have written: the v1 message, slot
/// 0 in the text, `slot_text` as the slot suffix.
fn v1_key(slot_text: &str) -> IssuedPackKey {
    let minute =
        minute_of(now().duration_since(UNIX_EPOCH).expect("after").as_secs()).expect("in range");
    let mut keyed = <Hmac<Sha256> as KeyInit>::new_from_slice(SECRET).expect("a key");
    keyed.update(&mac_input_v1(LEASE, &action(), minute).expect("input"));
    let full = keyed.finalize().into_bytes();
    let mac: String = full[..16].iter().map(|b| format!("{b:02x}")).collect();
    let nonce = format!("{mac}{minute:08x}-{slot_text}");
    IssuedPackKey::new(LEASE, &nonce).expect("a key")
}

/// Asserts that a key minted under the v1 tag (`-0`, no slot in the MAC) still verifies, and says
/// its minute; and that the same MAC under any other slot does not: a v1 MAC is good for slot 0
/// only, for a pack and for no large object.
#[test]
fn a_v1_key_verifies_for_slot_zero_only() {
    let minted = mint().verify(&v1_key("0"), &action()).expect("a v1 key");
    assert_eq!(minted, now());
    for slot in ["1", "7", "L0", "L7"] {
        let refused = mint()
            .verify(&v1_key(slot), &action())
            .expect_err("not minted here");
        assert!(
            matches!(refused, MintError::NotMinted(_)),
            "{slot}: {refused:?}"
        );
    }
    let other = mint().verify(&v1_key("0"), &blob("other").0);
    assert!(matches!(other, Err(MintError::NotMinted(_))));
}

/// Asserts that every slot of a run mints a key that verifies, that the keys differ from one
/// another, and that a key whose slot was changed in the text does not verify: not to another
/// pack, not from a pack to a large object, not from a large object to a pack of the same index.
#[test]
fn a_key_verifies_under_its_own_slot_only() {
    let slots: Vec<Slot> = (0..MAX_PACKS)
        .map(Slot::Pack)
        .chain((0..MAX_LARGE).map(Slot::Large))
        .collect();
    let mut seen = std::collections::BTreeSet::new();
    for slot in &slots {
        let key = mint().mint(LEASE, &action(), now(), *slot).expect("a key");
        assert!(key.key().ends_with(&format!("-{slot}")), "{}", key.key());
        assert!(seen.insert(key.key().to_owned()), "{}", key.key());
        assert_eq!(mint().verify(&key, &action()).expect("verifies"), now());
        for other in slots.iter().filter(|o| *o != slot) {
            let nonce = key.key().rsplit('/').next().expect("a nonce");
            let digits = nonce.rsplit_once('-').expect("a slot").0;
            let moved = IssuedPackKey::new(LEASE, &format!("{digits}-{other}")).expect("a key");
            let refused = mint().verify(&moved, &action());
            assert!(
                matches!(refused, Err(MintError::NotMinted(_))),
                "{slot} -> {other}"
            );
        }
    }
}

/// Asserts that the set of a run is `packs` pack keys then `large` large keys, minted at one
/// time, all verifying; and that no pack, more than 8 packs or more than 8 large objects is
/// refused.
#[test]
fn a_key_set_is_the_packs_then_the_large_objects() {
    let set = mint()
        .mint_set(LEASE, &action(), now(), 3, 2)
        .expect("a set");
    let texts: Vec<_> = set
        .iter()
        .map(|k| k.key().rsplit_once('-').expect("slot").1.to_owned())
        .collect();
    assert_eq!(texts, ["0", "1", "2", "L0", "L1"]);
    for key in &set {
        assert_eq!(mint().verify(key, &action()).expect("verifies"), now());
    }
    let full = mint()
        .mint_set(LEASE, &action(), now(), MAX_PACKS, MAX_LARGE)
        .expect("a full set");
    assert_eq!(full.len(), usize::from(MAX_PACKS + MAX_LARGE));
    assert_eq!(
        mint()
            .mint_set(LEASE, &action(), now(), 1, 0)
            .expect("v1-sized")
            .len(),
        1
    );
    for (packs, large) in [(0, 0), (MAX_PACKS + 1, 0), (1, MAX_LARGE + 1)] {
        let refused = mint()
            .mint_set(LEASE, &action(), now(), packs, large)
            .expect_err("refused");
        assert_eq!(refused, MintError::SetSize { packs, large });
    }
}

/// The 40 hex digits of a nonce of `mac`'s first 16 bytes and `minute`.
fn nonce_hex(mac: &[u8], minute: u32) -> String {
    let mac: String = mac[..16].iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{mac}{minute:08x}")
}

/// The minute of [`now`].
fn minute() -> u32 {
    let secs = now()
        .duration_since(UNIX_EPOCH)
        .expect("after the epoch")
        .as_secs();
    minute_of(secs).expect("in range")
}

/// Asserts that a put directory is `cas/b/<lease>/<40 hex digits>`, minted deterministically,
/// each input changing it, and that it and every key in it verify for its action and minute and
/// for no other action or secret.
#[test]
fn a_minted_put_dir_and_its_keys_verify() {
    let dir = mint().mint_put_dir(LEASE, &action(), now()).expect("a dir");
    let nonce = dir
        .dir()
        .strip_prefix(&format!("cas/b/{LEASE}/"))
        .expect("under the lease");
    assert_eq!(nonce.len(), 40, "{nonce}");
    assert!(
        nonce
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    );
    let again = mint().mint_put_dir(LEASE, &action(), now()).expect("a dir");
    assert_eq!(dir, again);
    let other_secret = PackKeyMint::new(&[9; 32]).expect("a mint");
    let changed = [
        mint().mint_put_dir("another", &action(), now()),
        mint().mint_put_dir(LEASE, &blob("other").0, now()),
        mint().mint_put_dir(LEASE, &action(), now() + Duration::from_secs(60)),
        other_secret.mint_put_dir(LEASE, &action(), now()),
    ];
    for other in changed {
        assert_ne!(other.expect("a dir").dir(), dir.dir());
    }
    let minute_start = UNIX_EPOCH + Duration::from_secs(u64::from(minute()) * 60);
    assert_eq!(mint().verify_put_dir(&dir, &action()), Ok(minute_start));
    let set = dir.key_set();
    for key in set.packs().iter().chain(set.large()) {
        assert_eq!(
            mint().verify(key, &action()),
            Ok(minute_start),
            "{}",
            key.key()
        );
    }
    assert!(matches!(
        mint().verify_put_dir(&dir, &blob("other").0),
        Err(MintError::NotMinted(_))
    ));
    assert!(matches!(
        other_secret.verify_put_dir(&dir, &action()),
        Err(MintError::NotMinted(_))
    ));
}

/// Asserts that a put directory's MAC is not one a slotted key carries: a directory whose nonce
/// is the v1 MAC of slot 0 (the same lease, action and minute, no slot) does not verify, and a
/// put directory's nonce given a slot (`-0`) does not verify as a slotted key.
#[test]
fn a_put_dir_mac_is_not_a_slotted_keys() {
    let keyed = |message: &[u8]| {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(SECRET).expect("a key");
        mac.update(message);
        mac.finalize().into_bytes().to_vec()
    };
    let v1 = keyed(&mac_input_v1(LEASE, &action(), minute()).expect("input"));
    let dir = IssuedPutDir::new(LEASE, &nonce_hex(&v1, minute())).expect("a dir");
    assert!(matches!(
        mint().verify_put_dir(&dir, &action()),
        Err(MintError::NotMinted(_))
    ));
    let put_dir = keyed(&mac_input_put_dir(LEASE, &action(), minute()).expect("input"));
    let ours = IssuedPutDir::new(LEASE, &nonce_hex(&put_dir, minute())).expect("a dir");
    assert!(mint().verify_put_dir(&ours, &action()).is_ok());
    let slotted = format!("{}-0", nonce_hex(&put_dir, minute()));
    let slotted = IssuedPackKey::new(LEASE, &slotted).expect("a key");
    assert!(matches!(
        mint().verify(&slotted, &action()),
        Err(MintError::NotMinted(_))
    ));
}

/// Asserts that a put directory with an empty or slashed lease or nonce, or one too long for the
/// names in it to be object keys, is refused to make, and that one whose nonce is not 40 hex
/// digits does not verify.
#[test]
fn a_put_dir_of_a_bad_part_is_refused() {
    for (lease, nonce) in [("", "ab"), (LEASE, ""), ("a/b", "ab"), (LEASE, "a/b")] {
        assert!(
            IssuedPutDir::new(lease, nonce).is_err(),
            "{lease:?} {nonce:?}"
        );
    }
    // A nonce long enough that the directory is an object key and `<dir>/pack7` is not.
    let edge = (1..2048).map(|n| "a".repeat(n)).find(|nonce| {
        IssuedPackKey::new(LEASE, nonce).is_ok() && IssuedPutDir::new(LEASE, nonce).is_err()
    });
    assert!(
        edge.is_some(),
        "a key too long for its directory's names is refused"
    );
    let short = IssuedPutDir::new(LEASE, "abc").expect("a dir");
    assert!(matches!(
        mint().verify_put_dir(&short, &action()),
        Err(MintError::NotMinted(_))
    ));
    assert_eq!(
        short.key(Slot::Large(3)).expect("a slot").key(),
        format!("cas/b/{LEASE}/abc/L3")
    );
}
