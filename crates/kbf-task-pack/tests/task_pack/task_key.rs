//! The key of a task pack, `cas/b/<build>/<nonce>-<slot>`: its shape, its minute, and the
//! message its MAC covers.

use kbf_task_pack::{
    MAC_LEN, MAX_BUILD_BYTES, MAX_LARGE, MAX_PACKS, Slot, TaskKeyError, TaskPackKey, check_build,
    mac_input, mac_input_v1, minute_of,
};
use kbf_types::{Digest, DigestFunction};

const BUILD: &str = "0000000000000001000000000000002a";
const MAC_HEX: &str = "a0a1a2a3a4a5a6a7a8a9aaabacadaeaf";
const MINUTE: u32 = 29_833_335;
const MINUTE_HEX: &str = "01c73877";

fn mac() -> [u8; MAC_LEN] {
    std::array::from_fn(|i| 0xa0 + u8::try_from(i).expect("16 fits"))
}

fn key_text(build: &str, nonce: &str) -> String {
    format!("cas/b/{build}/{nonce}")
}

fn action(seed: u8, size: u64) -> Digest {
    Digest::new(DigestFunction::Sha256, [seed; 32], size)
}

/// Asserts that a key is `cas/b/<build>/<mac hex><minute hex>-<slot>`, that it parses back to
/// the same parts, and that the minute in it is the minute of the time it was minted at: the
/// janitor reads the age of a pack from nothing else.
#[test]
fn a_minted_key_roundtrips_and_names_its_minute() {
    // 1790000123 s after the epoch is in minute 29833335, which starts at 1790000100.
    let minute = minute_of(1_790_000_123).expect("in range");
    assert_eq!(minute, MINUTE);
    let key = TaskPackKey::new(BUILD, mac(), minute, Slot::Pack(0)).expect("a key");
    let text = key.to_string();
    assert_eq!(text, key_text(BUILD, &format!("{MAC_HEX}{MINUTE_HEX}-0")));

    let parsed = TaskPackKey::parse(&text).expect("parses");
    assert_eq!(parsed, key);
    assert_eq!(parsed.build(), BUILD);
    assert_eq!(parsed.mac(), &mac());
    assert_eq!(parsed.minute(), MINUTE);
    assert_eq!(parsed.slot(), Slot::Pack(0));
    assert_eq!(parsed.minted_unix_secs(), 1_790_000_100);
}

/// Asserts that a key prints exactly as it was written, whatever its slot, and that the
/// largest minute and slot are keys.
#[test]
fn a_parsed_key_prints_as_it_was_written() {
    for (minute_hex, slot) in [
        ("00000000", "0"),
        ("ffffffff", "7"),
        ("01c73877", "3"),
        ("00000000", "L0"),
        ("ffffffff", "L7"),
        ("01c73877", "L3"),
    ] {
        let text = key_text(BUILD, &format!("{MAC_HEX}{minute_hex}-{slot}"));
        assert_eq!(TaskPackKey::parse(&text).expect(&text).to_string(), text);
    }
}

/// Asserts the text form of every slot of a run, and that it parses back: `<k>` for pack `k`,
/// `L<i>` for large object `i`, and the values the key reads out of the text.
#[test]
fn a_slot_prints_as_k_or_l_i_and_parses_back() {
    for k in 0..MAX_PACKS {
        let slot = Slot::Pack(k);
        assert_eq!(slot.to_string(), k.to_string());
        assert_eq!(Slot::parse(&slot.to_string()), Ok(slot));
        assert!(slot.in_range());
    }
    for i in 0..MAX_LARGE {
        let slot = Slot::Large(i);
        assert_eq!(slot.to_string(), format!("L{i}"));
        assert_eq!(Slot::parse(&slot.to_string()), Ok(slot));
        assert!(slot.in_range());
    }
    let key =
        TaskPackKey::parse(&key_text(BUILD, &format!("{MAC_HEX}{MINUTE_HEX}-L5"))).expect("key");
    assert_eq!(key.slot(), Slot::Large(5));
}

/// Asserts that a key cannot be made for a slot a run does not have, and that the slot is
/// refused in the MAC message too.
#[test]
fn a_slot_past_the_run_is_refused_to_make() {
    for slot in [
        Slot::Pack(MAX_PACKS),
        Slot::Large(MAX_LARGE),
        Slot::Pack(255),
    ] {
        assert!(!slot.in_range());
        assert_eq!(
            TaskPackKey::new(BUILD, mac(), 0, slot),
            Err(TaskKeyError::SlotRange(slot))
        );
        assert_eq!(
            mac_input(BUILD, &action(1, 5), 0, slot),
            Err(TaskKeyError::SlotRange(slot))
        );
    }
}

/// Asserts that no build and no nonce can hold a `/`: a build with one is refused to make, and a
/// key with an extra one does not parse, so a key cannot leave `cas/b/<build>/`.
#[test]
fn the_key_has_no_slash_in_build_or_nonce() {
    assert_eq!(
        TaskPackKey::new("a/b", mac(), 0, Slot::Pack(0)),
        Err(TaskKeyError::BuildCharacter('/'))
    );
    let nonce = format!("{MAC_HEX}{MINUTE_HEX}-0");
    let in_build = format!("cas/b/a/b/{nonce}");
    assert!(matches!(
        TaskPackKey::parse(&in_build),
        Err(TaskKeyError::NotAPackKey(_))
    ));
    let in_nonce = format!("cas/b/{BUILD}/x/{nonce}");
    assert!(matches!(
        TaskPackKey::parse(&in_nonce),
        Err(TaskKeyError::NotAPackKey(_))
    ));
}

/// Asserts the rule for a build id: 1 to 128 characters of letters, digits, `-` and `_`.
#[test]
fn a_build_is_one_to_128_letters_digits_dashes_and_underscores() {
    assert_eq!(check_build(""), Err(TaskKeyError::EmptyBuild));
    assert_eq!(
        check_build(&"a".repeat(MAX_BUILD_BYTES + 1)),
        Err(TaskKeyError::LongBuild(MAX_BUILD_BYTES + 1))
    );
    assert_eq!(check_build(&"a".repeat(MAX_BUILD_BYTES)), Ok(()));
    assert_eq!(check_build("Az09-_"), Ok(()));
    for bad in [".", "a.b", "a b", "a\0b", "é", ".."] {
        assert!(
            matches!(check_build(bad), Err(TaskKeyError::BuildCharacter(_))),
            "{bad:?}"
        );
    }
    assert_eq!(check_build("a.b"), Err(TaskKeyError::BuildCharacter('.')));
}

/// Asserts that a string of another shape is refused for its own reason: another prefix, no
/// nonce, no build.
#[test]
fn a_string_of_another_shape_is_not_a_task_pack_key() {
    let nonce = format!("{MAC_HEX}{MINUTE_HEX}-0");
    for text in [
        format!("cas/d/node/boot/{nonce}"),
        format!("cas/b/{BUILD}"),
        format!("/cas/b/{BUILD}/{nonce}"),
        String::new(),
    ] {
        assert!(
            matches!(TaskPackKey::parse(&text), Err(TaskKeyError::NotAPackKey(_))),
            "{text:?}"
        );
    }
    assert_eq!(
        TaskPackKey::parse(&format!("cas/b//{nonce}")),
        Err(TaskKeyError::EmptyBuild)
    );
}

/// Asserts that a nonce is refused unless it is 32 hex digits of MAC, 8 of minute, `-`: too
/// short or long, uppercase, not hex, no `-`.
#[test]
fn a_malformed_nonce_is_refused() {
    let minute = MINUTE_HEX;
    for nonce in [
        format!("{MAC_HEX}{minute}"),
        format!("{}{minute}-0", &MAC_HEX[..30]),
        format!("{MAC_HEX}{minute}00-0"),
        format!("{}{minute}-0", MAC_HEX.to_uppercase()),
        format!("{MAC_HEX}01c7387F-0"),
        format!("{}g{minute}-0", &MAC_HEX[..31]),
        format!("{MAC_HEX}{minute}_0"),
    ] {
        assert!(
            matches!(
                TaskPackKey::parse(&key_text(BUILD, &nonce)),
                Err(TaskKeyError::Nonce(_))
            ),
            "{nonce}"
        );
    }
}

/// Asserts that a slot is `<k>` or `L<i>` with a canonical decimal in range: not empty, signed,
/// zero-padded, lowercase `l`, doubled `L`, a bare `L`, 8 or more, past u8.
#[test]
fn a_slot_that_is_not_k_or_l_i_in_canonical_form_is_refused() {
    let bad = [
        "",
        "+1",
        "-1",
        "01",
        "00",
        "4294967296",
        "256",
        "8",
        "1x",
        "L",
        "L01",
        "L00",
        "L8",
        "L255",
        "L256",
        "L-1",
        "L+1",
        "l1",
        "LL1",
        "L1x",
        " 1",
        "1 ",
    ];
    for slot in bad {
        let nonce = format!("{MAC_HEX}{MINUTE_HEX}-{slot}");
        assert!(
            matches!(
                TaskPackKey::parse(&key_text(BUILD, &nonce)),
                Err(TaskKeyError::Slot(_))
            ),
            "{slot:?}"
        );
    }
}

/// Asserts the edges of the minute: the last second a 32-bit minute holds is a minute, the next
/// is not, and a minute is 60 seconds.
#[test]
fn a_time_past_the_last_minute_is_refused() {
    let last = u64::from(u32::MAX);
    assert_eq!(minute_of(0), Ok(0));
    assert_eq!(minute_of(59), Ok(0));
    assert_eq!(minute_of(60), Ok(1));
    assert_eq!(minute_of(last * 60 + 59), Ok(u32::MAX));
    assert_eq!(
        minute_of((last + 1) * 60),
        Err(TaskKeyError::MinuteRange((last + 1) * 60))
    );
}

/// Asserts that the message the MAC covers changes with each of the lease, the action's hash,
/// the action's size, the mint minute and the slot (a pack and a large object of one index
/// differ too), so a key minted for one of them is not valid for another once the frontend
/// recomputes the MAC over this message; and that its bytes are the documented ones (domain,
/// NUL, build, NUL, hash, size, minute, kind byte, index).
#[test]
fn a_changed_lease_action_minute_or_slot_changes_the_mac_input() {
    let base = mac_input("lease", &action(1, 5), 10, Slot::Pack(0)).expect("input");
    assert_eq!(
        base,
        mac_input("lease", &action(1, 5), 10, Slot::Pack(0)).expect("input")
    );

    let mut expected = b"kbf-task-pack/v2\0lease\0".to_vec();
    expected.extend_from_slice(&[1; 32]);
    expected.extend_from_slice(&5u64.to_be_bytes());
    expected.extend_from_slice(&10u32.to_be_bytes());
    expected.extend_from_slice(&[0, 0]);
    assert_eq!(base, expected);

    let large = mac_input("lease", &action(1, 5), 10, Slot::Large(3)).expect("input");
    assert_eq!(&large[large.len() - 2..], &[1, 3]);

    let others = [
        mac_input("lease2", &action(1, 5), 10, Slot::Pack(0)),
        mac_input("lease", &action(2, 5), 10, Slot::Pack(0)),
        mac_input("lease", &action(1, 6), 10, Slot::Pack(0)),
        mac_input("lease", &action(1, 5), 11, Slot::Pack(0)),
        mac_input("lease", &action(1, 5), 10, Slot::Pack(1)),
        mac_input("lease", &action(1, 5), 10, Slot::Large(0)),
    ];
    for other in others {
        assert_ne!(base, other.expect("input"));
    }
    assert_eq!(
        mac_input("a/b", &action(1, 5), 10, Slot::Pack(0)),
        Err(TaskKeyError::BuildCharacter('/'))
    );
}

/// Asserts that the first message is kept as it was minted by the code before the slot: the
/// domain tag `kbf-task-pack/v1` and no slot, and that it differs from the v2 message of slot 0.
#[test]
fn the_v1_mac_input_has_the_old_tag_and_no_slot() {
    let v1 = mac_input_v1("lease", &action(1, 5), 10).expect("input");
    let mut expected = b"kbf-task-pack/v1\0lease\0".to_vec();
    expected.extend_from_slice(&[1; 32]);
    expected.extend_from_slice(&5u64.to_be_bytes());
    expected.extend_from_slice(&10u32.to_be_bytes());
    assert_eq!(v1, expected);
    let v2 = mac_input("lease", &action(1, 5), 10, Slot::Pack(0)).expect("input");
    assert_ne!(v1, v2);
    assert_eq!(
        mac_input_v1("a/b", &action(1, 5), 10),
        Err(TaskKeyError::BuildCharacter('/'))
    );
}
