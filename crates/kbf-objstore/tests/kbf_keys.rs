//! kbf's own rule for the keys it names ([`kbf_key`], [`kbf_prefix`]): narrower than the store's
//! S3 rules, which `listing.rs` checks.

use kbf_objstore::{KbfKeyError, KeyError, kbf_key, kbf_prefix};

/// Catches: a name kbf builds a key from (a node, a boot, a build, a pack key a node commits, a
/// prefix of a flag or a URI) taken with a character a store or a URL could treat apart, or with
/// a segment that could put its key outside its prefix; and a refusal that names neither the key
/// nor what is wrong. Each refusal is the whole line, by exact equality.
#[test]
fn a_key_kbf_names_is_the_alphabet_with_no_empty_or_dot_segment() {
    let ok = "cas/d/node-1/boot_2/0000000000000000.~";
    assert_eq!(
        kbf_key(ok).map(|k| k.as_str().to_owned()),
        Ok(ok.to_owned())
    );
    assert_eq!(
        kbf_key("k".repeat(1024)).map(|k| k.as_str().len()),
        Ok(1024)
    );
    let refused = |key: &str| kbf_key(key).map(|_| ()).expect_err(key).to_string();
    assert_eq!(refused(""), KeyError::Empty.to_string());
    assert_eq!(
        kbf_key("k".repeat(1025)).map(|_| ()),
        Err(KbfKeyError::Key(KeyError::TooLong(1025)))
    );
    for (key, ch) in [
        ("b c", ' '),
        ("caf\u{e9}", '\u{e9}'),
        ("a+b", '+'),
        ("a%2f", '%'),
    ] {
        assert_eq!(
            refused(key),
            format!("object key {key:?} has {ch:?}; only A-Z a-z 0-9 - _ . ~ / are allowed")
        );
    }
    for key in ["a//b", "/a", "a/", "a/./b", "a/../b", "..", "."] {
        assert_eq!(
            refused(key),
            format!("object key {key:?} has an empty, `.` or `..` segment")
        );
    }
}

/// Catches: a prefix that could make a key kbf does not name, refused or taken unlike the key
/// part of it: empty and `p/` and `p` are taken; a space, an empty or `..` segment, and a prefix
/// over 1024 bytes are refused.
#[test]
fn a_prefix_kbf_names_is_empty_or_a_key_it_names_with_or_without_a_slash() {
    for prefix in ["", "p/", "p", "a/b/"] {
        assert_eq!(
            kbf_prefix(prefix).map(|p| p.as_str().to_owned()),
            Ok(prefix.to_owned())
        );
    }
    for prefix in ["no spaces/", "a//", "//", "a/../", "/"] {
        assert!(kbf_prefix(prefix).is_err(), "{prefix:?}");
    }
    // A key part of 1024 bytes is a key; with its `/` the prefix is one byte too long.
    assert_eq!(
        kbf_prefix(format!("{}/", "p".repeat(1024))).map(|_| ()),
        Err(KbfKeyError::Key(KeyError::TooLong(1025)))
    );
}
