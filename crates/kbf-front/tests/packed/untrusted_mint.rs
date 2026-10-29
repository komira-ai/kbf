//! The minting of the key a task writes its pack under (`PackKeyMint`): a key is the frontend's
//! HMAC over the lease, the action and the minute, so a key it did not mint is refused before the
//! store is asked. Every test says the behaviour it asserts.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use kbf_front::{IssuedPackKey, MIN_SECRET_BYTES, MintError, PackKeyMint};
use kbf_types::Digest;

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
        mint.mint(lease, action, at)
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
        .mint(LEASE, &action(), now() + Duration::from_secs(7))
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
        mint().mint(LEASE, &action(), before).err(),
        Some(MintError::Clock)
    );
    let far = UNIX_EPOCH + Duration::from_secs(60 * (1 << 32));
    let past = mint()
        .mint(LEASE, &action(), far)
        .expect_err("past the last minute");
    assert!(matches!(past, MintError::Key(_)), "{past:?}");
    let bad = mint()
        .mint("not/a/build", &action(), now())
        .expect_err("a bad lease");
    assert!(matches!(bad, MintError::Key(_)), "{bad:?}");
    let debug = format!("{:?}", mint());
    assert!(!debug.contains("0123456789abcdef"), "{debug}");
}
