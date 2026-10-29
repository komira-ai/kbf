//! Tokens: the format of the id, the MAC, the claims a token may carry, the ceiling of a minting
//! key, and the minting key files.

use std::fs::Permissions;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use kbf_testkit::dir::TestDir;
use sha2::Sha256;

use super::{
    Ceiling, Claims, MARKER, MAX_KEY_ID, MAX_PREFIX, MintingKey, Ops, TokenError, TokenRole,
    check_name, forge, mint, validate_prefix, verify,
};

/// The time of the tests, in milliseconds.
const NOW: u64 = 1_800_000_000_000;

/// The secret bytes of the minting key `jobs-mint`.
const SECRET: &[u8] = b"0123456789abcdef0123456789abcdef";

pub(crate) const CREATE: Ops = Ops {
    create: true,
    read: false,
    delete: false,
};
pub(crate) const READ: Ops = Ops {
    create: false,
    read: true,
    delete: false,
};
pub(crate) const DELETE: Ops = Ops {
    create: false,
    read: false,
    delete: true,
};

/// A ceiling of `jobs/` for create and read, an hour at most, with no limit on a body.
pub(crate) fn ceiling() -> Ceiling {
    Ceiling {
        prefix: "jobs/".to_owned(),
        ops: Ops {
            create: true,
            read: true,
            delete: false,
        },
        max_ttl_secs: 3600,
        max_object_bytes: None,
    }
}

pub(crate) fn key(id: &str) -> MintingKey {
    MintingKey::new(id, SECRET, ceiling()).unwrap()
}

/// Claims for `prefix` and `ops` that expire a minute after `NOW`.
pub(crate) fn claims(prefix: &str, ops: Ops) -> Claims {
    Claims {
        role: TokenRole::Client,
        ops,
        prefix: prefix.to_owned(),
        not_after: NOW / 1000 + 60,
    }
}

/// The id of a credential, decoded to its bytes.
fn bytes_of(id: &str) -> Vec<u8> {
    URL_SAFE_NO_PAD
        .decode(id.strip_prefix(MARKER).unwrap())
        .unwrap()
}

fn id_of(bytes: &[u8]) -> String {
    format!("{MARKER}{}", URL_SAFE_NO_PAD.encode(bytes))
}

fn hmac(label: &[u8], covered: &[u8]) -> Vec<u8> {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(SECRET).unwrap();
    mac.update(label);
    mac.update(MARKER.as_bytes());
    mac.update(covered);
    mac.finalize().into_bytes().to_vec()
}

/// Asserts that the id has exactly the layout the module docs give, byte for byte, and that its
/// MAC and its secret are the HMACs they say, worked out here apart from the code: a client of
/// another language that follows the docs makes the same token.
#[test]
fn the_id_and_the_secret_are_the_documented_bytes() {
    let made = mint(&key("jobs-mint"), &claims("jobs/a/", CREATE), NOW).unwrap();
    assert!(made.access_key_id.starts_with("tt1"));
    let bytes = bytes_of(&made.access_key_id);
    let mut expected = vec![0_u8, 1, 9];
    expected.extend_from_slice(b"jobs-mint");
    expected.extend_from_slice(&(NOW / 1000 + 60).to_be_bytes());
    expected.extend_from_slice(&[0, 7]);
    expected.extend_from_slice(b"jobs/a/");
    let mac = hmac(b"tierstore/token/mac\0", &expected);
    expected.extend_from_slice(&mac[..16]);
    assert_eq!(bytes, expected);
    let secret = hmac(
        b"tierstore/token/secret\0",
        &expected[..expected.len() - 16],
    );
    assert_eq!(made.secret_access_key, hex::encode(secret));
    assert_eq!(made.secret_access_key.len(), 64);
}

/// Asserts that the claims of an id are in the clear and the secret is not in it: the id holds
/// neither the secret nor its bytes, and the MAC is not a prefix of the secret.
#[test]
fn the_id_does_not_hold_the_secret() {
    let made = mint(&key("jobs-mint"), &claims("jobs/a/", CREATE), NOW).unwrap();
    let bytes = bytes_of(&made.access_key_id);
    let secret = hex::decode(&made.secret_access_key).unwrap();
    let window = |part: &[u8]| bytes.windows(part.len()).any(|found| found == part);
    assert!(!window(&secret[..8]));
    assert!(!made.access_key_id.contains(&made.secret_access_key[..16]));
    let mac = &bytes[bytes.len() - 16..];
    assert_ne!(mac, &secret[..16]);
}

/// Asserts that an id verifies to the claims and the secret it was made with.
#[test]
fn a_minted_token_verifies_to_its_claims_and_secret() {
    let key = key("jobs-mint");
    let wanted = claims(
        "jobs/a/",
        Ops {
            create: true,
            read: true,
            delete: false,
        },
    );
    let made = mint(&key, &wanted, NOW).unwrap();
    let verified = verify(&[key], &made.access_key_id, NOW).unwrap();
    assert_eq!(verified.claims, wanted);
    assert_eq!(verified.secret, made.secret_access_key);
    assert_eq!(verified.max_body, None);
}

/// Asserts that no change to any byte of the claims in an id gives an id that verifies: a
/// tampered role, ops, key id, expiry, prefix length or prefix is refused, and nothing is
/// accepted with a changed claim. It catches a verifier that trusts the claims before the MAC.
#[test]
fn a_tampered_claim_never_verifies() {
    let key = key("jobs-mint");
    let made = mint(&key, &claims("jobs/a/", CREATE), NOW).unwrap();
    let bytes = bytes_of(&made.access_key_id);
    for at in 0..bytes.len() - 16 {
        for flip in [0x01_u8, 0x80] {
            let mut changed = bytes.clone();
            changed[at] ^= flip;
            let id = id_of(&changed);
            let refused = verify(std::slice::from_ref(&key), &id, NOW);
            assert!(refused.is_err(), "byte {at} ^ {flip}");
        }
    }
}

/// Asserts the error of a prefix changed to a longer one under the MAC of the shorter: the MAC
/// does not match.
#[test]
fn a_widened_prefix_is_a_wrong_mac() {
    let key = key("jobs-mint");
    let narrow = mint(&key, &claims("jobs/a/", CREATE), NOW).unwrap();
    let wide = forge(&key, &claims("jobs/", CREATE));
    let (narrow, wide) = (
        bytes_of(&narrow.access_key_id),
        bytes_of(&wide.access_key_id),
    );
    let mut spliced = wide[..wide.len() - 16].to_vec();
    spliced.extend_from_slice(&narrow[narrow.len() - 16..]);
    let refused = verify(&[key], &id_of(&spliced), NOW);
    assert_eq!(refused.unwrap_err(), TokenError::BadMac);
}

/// Asserts that a changed MAC is `BadMac`, and not any other error.
#[test]
fn a_wrong_mac_is_refused() {
    let key = key("jobs-mint");
    let made = mint(&key, &claims("jobs/a/", CREATE), NOW).unwrap();
    let mut bytes = bytes_of(&made.access_key_id);
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    let refused = verify(&[key], &id_of(&bytes), NOW);
    assert_eq!(refused.unwrap_err(), TokenError::BadMac);
}

/// Asserts that an id of a minting key the server does not hold is refused, and that a key that
/// was retired (dropped from the server's keys) refuses the tokens it signed while the new key
/// still accepts its own and, during the rotation, the old key's: the current and the previous
/// key are both accepted.
#[test]
fn an_unknown_or_retired_key_id_is_refused_and_the_previous_key_is_accepted() {
    let (old, new) = (key("jobs-old"), key("jobs-new"));
    let by_old = mint(&old, &claims("jobs/a/", CREATE), NOW).unwrap();
    let by_new = mint(&new, &claims("jobs/a/", CREATE), NOW).unwrap();
    let both = [new.clone(), old.clone()];
    assert!(verify(&both, &by_old.access_key_id, NOW).is_ok());
    assert!(verify(&both, &by_new.access_key_id, NOW).is_ok());
    let retired = [new];
    let refused = verify(&retired, &by_old.access_key_id, NOW);
    assert_eq!(refused.unwrap_err(), TokenError::UnknownKeyId);
    assert!(verify(&retired, &by_new.access_key_id, NOW).is_ok());
    let nobody = verify(&[], &by_new.access_key_id, NOW);
    assert_eq!(nobody.unwrap_err(), TokenError::UnknownKeyId);
}

/// Asserts that a key with the same id and a different secret does not accept a token of the
/// other: the id alone does not make the key.
#[test]
fn a_key_with_another_secret_gives_a_wrong_mac() {
    let made = mint(&key("jobs-mint"), &claims("jobs/a/", CREATE), NOW).unwrap();
    let other = MintingKey::new("jobs-mint", &[7_u8; 32], ceiling()).unwrap();
    let refused = verify(&[other], &made.access_key_id, NOW);
    assert_eq!(refused.unwrap_err(), TokenError::BadMac);
}

/// Asserts that ids that are not the id of a token are `Malformed`.
#[test]
fn ids_that_are_not_tokens_are_malformed() {
    let key = key("jobs-mint");
    let made = mint(&key, &claims("jobs/a/", CREATE), NOW).unwrap();
    let good = bytes_of(&made.access_key_id);
    let mut trailing = good.clone();
    trailing.insert(trailing.len() - 16, 0);
    let mut role = good.clone();
    role[0] = 2;
    let mut ops = good.clone();
    ops[1] = 0x08;
    let mut utf8 = good.clone();
    utf8[3] = 0xFF;
    let mut short_prefix = good.clone();
    short_prefix[3 + 9 + 8 + 1] = 200;
    let cases = [
        "AKIDEXAMPLE".to_owned(),
        "tt1".to_owned(),
        "tt1!!!".to_owned(),
        format!("{}=", made.access_key_id),
        id_of(&good[..10]),
        id_of(&trailing),
        id_of(&role),
        id_of(&ops),
        id_of(&utf8),
        id_of(&short_prefix),
        id_of(&[]),
    ];
    for id in cases {
        let refused = verify(std::slice::from_ref(&key), &id, NOW);
        assert_eq!(refused.unwrap_err(), TokenError::Malformed, "{id}");
    }
}

/// Asserts that a token whose claims are not ones a token may carry is refused at mint time and,
/// when it carries a good MAC all the same, at verify time: an empty prefix, `..`, a NUL, a
/// control character, a `.` segment, a reserved prefix or a prefix of one, a prefix over 256
/// bytes, no operation, and the delete operation without the role gc.
#[test]
fn claims_that_are_not_valid_are_refused_at_mint_and_at_verify() {
    let long = "j".repeat(MAX_PREFIX + 1);
    let prefixes = [
        "",
        "jobs/../cas/",
        "jobs/a..b/",
        "jobs/\0/",
        "jobs/\n/",
        "jobs/./a/",
        "_journal/",
        "_journal/p1/",
        "_tiering/usage",
        "_",
        "_jou",
        long.as_str(),
    ];
    let wide = MintingKey::new(
        "wide",
        SECRET,
        Ceiling {
            prefix: "j".to_owned(),
            ..ceiling()
        },
    )
    .unwrap();
    let mut cases: Vec<Claims> = prefixes.iter().map(|p| claims(p, CREATE)).collect();
    cases.push(claims("jobs/a/", Ops::default()));
    cases.push(claims("jobs/a/", DELETE));
    for case in cases {
        let minted = mint(&wide, &case, NOW);
        assert!(minted.is_err(), "{case:?}");
        let forged = forge(&wide, &case);
        let verified = verify(std::slice::from_ref(&wide), &forged.access_key_id, NOW);
        assert!(verified.is_err(), "{case:?}");
    }
    let gc = Claims {
        role: TokenRole::Gc,
        ..claims("jobs/a/", DELETE)
    };
    let gc_key = MintingKey::new(
        "gc",
        SECRET,
        Ceiling {
            ops: DELETE,
            ..ceiling()
        },
    )
    .unwrap();
    assert!(mint(&gc_key, &gc, NOW).is_ok());
}

/// Asserts the names a key or a prefix may be.
#[test]
fn names_are_checked() {
    for good in ["jobs/a/b", "a.b", "a/.b", "a/b./c", "é/ü", "jobs/%2e"] {
        assert_eq!(check_name(good), Ok(()), "{good}");
    }
    for bad in [
        "a/../b", "..", "a\0b", "a/./b", "./a", "a/.", "a\u{7f}", "a\tb",
    ] {
        assert!(check_name(bad).is_err(), "{bad:?}");
    }
    assert!(check_name(&"a".repeat(1025)).is_err());
    assert_eq!(check_name(&"a".repeat(1024)), Ok(()));
    assert_eq!(validate_prefix("jobs/"), Ok(()));
}

/// Asserts that a token that exceeds its key's ceiling is refused with a good MAC, in each
/// dimension: a prefix outside the ceiling's (also a shorter one, and one that is a sibling), an
/// operation the ceiling does not grant, and a lifetime longer than the ceiling's; and that the
/// limits themselves are allowed. Minting refuses the same claims first.
#[test]
fn a_token_beyond_the_ceiling_of_its_key_is_refused() {
    let key = key("jobs-mint");
    let keys = std::slice::from_ref(&key);
    let hour = NOW / 1000 + 3600;
    let ok = [
        claims("jobs/", CREATE),
        Claims {
            not_after: hour,
            ..claims("jobs/a/", CREATE)
        },
        claims(
            "jobs/a/",
            Ops {
                create: true,
                read: true,
                delete: false,
            },
        ),
    ];
    for case in &ok {
        let made = mint(&key, case, NOW).unwrap();
        assert!(verify(keys, &made.access_key_id, NOW).is_ok(), "{case:?}");
    }
    let gc = |claims: Claims| Claims {
        role: TokenRole::Gc,
        ..claims
    };
    let beyond = [
        ("prefix", claims("cas/a/", CREATE)),
        ("prefix", claims("job", CREATE)),
        ("prefix", claims("jobs", CREATE)),
        ("prefix", claims("jobz/a/", CREATE)),
        ("operations", gc(claims("jobs/a/", DELETE))),
        (
            "operations",
            gc(claims(
                "jobs/a/",
                Ops {
                    create: true,
                    read: false,
                    delete: true,
                },
            )),
        ),
        (
            "lifetime",
            Claims {
                not_after: hour + 1,
                ..claims("jobs/a/", CREATE)
            },
        ),
    ];
    for (which, case) in beyond {
        let want = Err(TokenError::Exceeds(which));
        assert_eq!(mint(&key, &case, NOW).map(|_| ()), want, "{case:?}");
        let forged = forge(&key, &case);
        let refused = verify(keys, &forged.access_key_id, NOW).map(|_| ());
        assert_eq!(refused, want, "{case:?}");
    }
}

/// Asserts that the lifetime is counted from the time of the check: the same token is within the
/// ceiling early and beyond it never earlier, and an expired token is not beyond a ceiling.
#[test]
fn the_ceiling_ttl_counts_from_the_time_of_the_request() {
    let key = key("jobs-mint");
    let case = Claims {
        not_after: NOW / 1000 + 3600,
        ..claims("jobs/a/", CREATE)
    };
    let forged = forge(&key, &case);
    let keys = std::slice::from_ref(&key);
    assert!(verify(keys, &forged.access_key_id, NOW).is_ok());
    assert!(verify(keys, &forged.access_key_id, NOW + 5000).is_ok());
    let early = verify(keys, &forged.access_key_id, NOW - 1000);
    assert_eq!(early.unwrap_err(), TokenError::Exceeds("lifetime"));
    let past = verify(keys, &forged.access_key_id, NOW + 7_200_000);
    assert!(past.is_ok());
}

/// Asserts the expiry rule: a token is good until the millisecond before its `not after` second
/// and refused from it on, with no grace.
#[test]
fn a_token_expires_at_its_not_after_second_with_no_grace() {
    let case = claims("jobs/a/", CREATE);
    let at = case.not_after * 1000;
    assert!(!case.expired(at - 1));
    assert!(case.expired(at));
    assert!(case.expired(at + 1));
    assert!(case.expired(at + 15 * 60 * 1000));
    let forever = Claims {
        not_after: u64::MAX,
        ..case
    };
    assert!(!forever.expired(u64::MAX - 1));
}

/// Asserts the ceiling is validated: the prefix, the operations, the lifetime and the size.
#[test]
fn a_ceiling_must_be_usable() {
    let bad = [
        Ceiling {
            prefix: String::new(),
            ..ceiling()
        },
        Ceiling {
            prefix: "_tiering/".to_owned(),
            ..ceiling()
        },
        Ceiling {
            ops: Ops::default(),
            ..ceiling()
        },
        Ceiling {
            max_ttl_secs: 0,
            ..ceiling()
        },
        Ceiling {
            max_object_bytes: Some(0),
            ..ceiling()
        },
    ];
    for ceiling in bad {
        assert!(MintingKey::new("k", SECRET, ceiling).is_err());
    }
    assert!(MintingKey::new("k", SECRET, ceiling()).is_ok());
}

/// Asserts the minting key id and secret rules.
#[test]
fn a_minting_key_needs_a_plain_id_and_a_long_secret() {
    let too_long = "k".repeat(MAX_KEY_ID + 1);
    for id in ["", "a b", "a/b", "é", too_long.as_str()] {
        assert!(MintingKey::new(id, SECRET, ceiling()).is_err(), "{id:?}");
    }
    assert!(MintingKey::new(&"k".repeat(MAX_KEY_ID), SECRET, ceiling()).is_ok());
    assert!(MintingKey::new("k", &SECRET[..31], ceiling()).is_err());
    let shown = format!("{:?}", key("jobs-mint"));
    assert!(shown.contains("jobs-mint"));
    assert!(!shown.contains("0123456789abcdef"));
    let made = mint(&key("jobs-mint"), &claims("jobs/", CREATE), NOW).unwrap();
    assert!(!format!("{made:?}").contains(&made.secret_access_key));
}

/// Asserts the length of an id: for the longest prefix and the longest minting key id, an id is
/// 426 characters, all of the characters of a URL-safe alphabet; one for a realistic prefix of a
/// job is under 200, so a credential scope stays a few hundred bytes.
#[test]
fn the_longest_id_is_426_characters_of_a_url_safe_alphabet() {
    let long_key = MintingKey::new(&"k".repeat(MAX_KEY_ID), SECRET, ceiling()).unwrap();
    let prefix = format!("jobs/{}", "p".repeat(MAX_PREFIX - 5));
    let made = mint(&long_key, &claims(&prefix, CREATE), NOW).unwrap();
    assert_eq!(made.access_key_id.len(), 426);
    let safe = |c: char| c.is_ascii_alphanumeric() || c == '-' || c == '_';
    assert!(made.access_key_id.chars().all(safe));
    let hex_id = "0123456789abcdef".repeat(4);
    let realistic = format!("jobs/{hex_id}/{}/{}/", 123_456, 7);
    let made = mint(&key("jobs-mint"), &claims(&realistic, CREATE), NOW).unwrap();
    assert!(
        made.access_key_id.len() < 200,
        "{}",
        made.access_key_id.len()
    );
}

/// A file in `dir` with this text and mode.
fn file(dir: &TestDir, name: &str, text: &str, mode: u32) -> PathBuf {
    let path = dir.path().join(name);
    std::fs::write(&path, text).unwrap();
    std::fs::set_permissions(&path, Permissions::from_mode(mode)).unwrap();
    path
}

/// Asserts what a minting key file holds: the key and its ceiling, with the optional size limit;
/// and that a file without a ceiling, in the two-line form, with an operation that is not one,
/// with a field more, with an unusable ceiling or with a group-readable mode is refused, with no
/// message that carries the secret (fail closed).
#[test]
fn a_minting_key_file_has_a_ceiling_and_is_private() {
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-token-keys");
    let secret = "0123456789abcdef0123456789abcdef";
    let json = |ceiling: &str| {
        format!(
            "{{\"access_key_id\": \"jobs-mint\", \"secret_access_key\": \"{secret}\"{ceiling}}}"
        )
    };
    let full = ", \"ceiling\": {\"prefix\": \"jobs/\", \"ops\": [\"create\", \"read\"], \"max_ttl_secs\": 3600, \"max_object_bytes\": 1048576}";
    let path = file(&dir, "good", &json(full), 0o600);
    let read = MintingKey::from_file("--token-key-file", &path).unwrap();
    assert_eq!(read.id(), "jobs-mint");
    let expected = Ceiling {
        max_object_bytes: Some(1_048_576),
        ..ceiling()
    };
    assert_eq!(read.ceiling(), &expected);
    let nullable = full.replace("1048576", "null");
    let path = file(&dir, "null", &json(&nullable), 0o400);
    let read = MintingKey::from_file("--token-key-file", &path).unwrap();
    assert_eq!(read.ceiling().max_object_bytes, None);
    let bad = [
        ("none", json("")),
        ("lines", format!("jobs-mint\n{secret}\n")),
        ("op", json(&full.replace("read", "write"))),
        ("extra", json(&full.replace("\"ops\"", "\"x\": 1, \"ops\""))),
        ("prefix", json(&full.replace("jobs/", "_journal/"))),
        ("ttl", json(&full.replace("3600", "0"))),
        ("noops", json(&full.replace("[\"create\", \"read\"]", "[]"))),
        ("short", json(full).replace(secret, "short")),
        ("type", json(&full.replace("3600", "\"3600\""))),
    ];
    for (name, text) in bad {
        let path = file(&dir, name, &text, 0o600);
        let refused = MintingKey::from_file("--token-key-file", &path).unwrap_err();
        assert!(
            refused.starts_with("--token-key-file "),
            "{name}: {refused}"
        );
        assert!(!refused.contains(secret), "{name}: {refused}");
    }
    let open = file(&dir, "open", &json(full), 0o644);
    let refused = MintingKey::from_file("--token-key-file", &open).unwrap_err();
    assert!(refused.contains("0600"), "{refused}");
    let missing = dir.path().join("missing");
    assert!(MintingKey::from_file("--token-key-file", &missing).is_err());
}

/// Asserts that keys with one id are refused together, and the files are read in order.
#[test]
fn minting_keys_with_one_id_are_refused() {
    use super::{ensure_distinct_ids, token_keys};
    let dir = TestDir::under(std::env::temp_dir(), "tierstore-token-ids");
    let text = |id: &str| {
        format!(
            "{{\"access_key_id\": \"{id}\", \"secret_access_key\": \"{}\", \"ceiling\": {{\"prefix\": \"jobs/\", \"ops\": [\"read\"], \"max_ttl_secs\": 60, \"max_object_bytes\": null}}}}",
            "s".repeat(32)
        )
    };
    let one = file(&dir, "one", &text("one"), 0o600);
    let two = file(&dir, "two", &text("two"), 0o600);
    let keys = token_keys(&[one.clone(), two.clone()]).unwrap();
    assert_eq!(
        keys.iter().map(MintingKey::id).collect::<Vec<_>>(),
        ["one", "two"]
    );
    assert_eq!(ensure_distinct_ids(&keys), Ok(()));
    let again = token_keys(&[one.clone(), two, one]).unwrap();
    let refused = ensure_distinct_ids(&again).unwrap_err();
    assert!(refused.contains("\"one\""), "{refused}");
    assert!(refused.contains("--token-key-file"), "{refused}");
}
