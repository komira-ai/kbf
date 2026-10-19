//! The proxy secret is compared whole, as a digest (`kbf_server::proxy_auth::ProxySecret::admits`),
//! not by a prefix of it (issue #1095).
//!
//! A random wrong secret shares a digest's first byte with the right one one time in 256,
//! so a few hundred of them can all miss it (about a 30% chance at 300), and a compare of
//! the first byte would pass them. These tests do not draw: they search, in a fixed order,
//! for wrong values whose digest begins as the right one's does, so a compare that stops
//! early is refused by construction.

#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use kbf_server::proxy_auth::ProxySecret;
use sha2::{Digest as _, Sha256};

const SECRET: [u8; 32] = [0x5a; 32];

/// The value the proxy presents: the 32 bytes as 64 lower-case hex digits.
fn presented_secret() -> String {
    hex::encode(SECRET)
}

fn secret() -> ProxySecret {
    static N: AtomicU32 = AtomicU32::new(0);
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("kbf-server-proxy-auth-prefix");
    std::fs::create_dir_all(&dir).expect("a directory");
    let path = dir.join(format!(
        "secret-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, SECRET).expect("write the file");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).expect("chmod");
    ProxySecret::from_file(&path).expect("a usable secret")
}

/// The digest `ProxySecret` keeps and compares: SHA-256 of the 64 hex digits presented.
fn digest(value: &str) -> [u8; 32] {
    Sha256::digest(value.as_bytes()).into()
}

/// Well-formed wrong values (64 lower-case hex digits, so only the compare can refuse
/// them) whose digest begins with the first `prefix` bytes of the right one's, in the fixed
/// order of the counter that spells them, at most `count` of them.
fn sharing(prefix: usize, count: usize) -> Vec<String> {
    let right = presented_secret();
    let target = digest(&right);
    (0u64..)
        .map(|i| format!("{i:064x}"))
        .filter(|value| *value != right && digest(value)[..prefix] == target[..prefix])
        .take(count)
        .collect()
}

/// Catches: a compare of the first byte of the digest (`presented[..1] == stored[..1]`).
///
/// Mutants seen red: the compare cut to the first 1 byte, and to the first 2.
#[test]
fn a_value_sharing_the_first_digest_byte_is_refused() {
    let secret = secret();
    let found = sharing(1, 8);
    assert_eq!(
        found.len(),
        8,
        "the search is over a fixed, unbounded order"
    );
    for wrong in &found {
        assert_eq!(digest(wrong)[..1], digest(&presented_secret())[..1]);
        assert!(!secret.admits(wrong.as_bytes()), "{wrong}");
    }
}

/// Catches: a compare of the first two bytes of the digest. About 65 thousand hashes find
/// the first value, which is a fraction of a second.
///
/// Mutant seen red: the compare cut to the first 2 bytes.
#[test]
fn a_value_sharing_the_first_two_digest_bytes_is_refused() {
    let secret = secret();
    let found = sharing(2, 1);
    assert_eq!(found.len(), 1);
    assert_eq!(digest(&found[0])[..2], digest(&presented_secret())[..2]);
    assert!(!secret.admits(found[0].as_bytes()), "{}", found[0]);
}

/// A sweep beside the two searches: 300 distinct wrong values, and the right one.
#[test]
fn three_hundred_distinct_wrong_values_are_refused_and_the_right_one_is_not() {
    let secret = secret();
    let right = presented_secret();
    let wrong: Vec<String> = (0u64..300)
        .map(|i| format!("{i:064x}"))
        .filter(|value| *value != right)
        .collect();
    assert_eq!(wrong.len(), 300);
    for value in &wrong {
        assert!(!secret.admits(value.as_bytes()), "{value}");
    }
    assert!(secret.admits(right.as_bytes()));
}
