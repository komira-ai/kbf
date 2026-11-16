//! The key of an object of a put directory, `cas/b/<build>/<nonce>/<name>`: its names, its
//! shape beside the slotted layout's, and the message its directory's MAC covers.

use kbf_task_pack::{
    KeyLayout, MAC_LEN, MAX_LARGE, MAX_PACKS, Slot, TaskKeyError, TaskPackKey, mac_input_put_dir,
    mac_input_v1,
};
use kbf_types::{Digest, DigestFunction};

const BUILD: &str = "0000000000000001000000000000002a";
const MAC_HEX: &str = "a0a1a2a3a4a5a6a7a8a9aaabacadaeaf";
const MINUTE: u32 = 29_833_335;
const MINUTE_HEX: &str = "01c73877";

fn mac() -> [u8; MAC_LEN] {
    std::array::from_fn(|i| 0xa0 + u8::try_from(i).expect("16 fits"))
}

fn dir() -> String {
    format!("cas/b/{BUILD}/{MAC_HEX}{MINUTE_HEX}")
}

fn action(seed: u8, size: u64) -> Digest {
    Digest::new(DigestFunction::Sha256, [seed; 32], size)
}

/// Asserts that the object of each slot of a run has the name `kbf-task` commits (`pack`,
/// `pack1` to `pack7`, `L0` to `L7`) and that the name parses back to its slot.
#[test]
fn every_slot_has_the_name_kbf_task_commits() {
    assert_eq!(Slot::Pack(0).object_name(), "pack");
    for k in 1..MAX_PACKS {
        assert_eq!(Slot::Pack(k).object_name(), format!("pack{k}"));
    }
    for i in 0..MAX_LARGE {
        assert_eq!(Slot::Large(i).object_name(), format!("L{i}"));
    }
    let slots = (0..MAX_PACKS)
        .map(Slot::Pack)
        .chain((0..MAX_LARGE).map(Slot::Large));
    for slot in slots {
        assert_eq!(Slot::from_object_name(&slot.object_name()), Ok(slot));
    }
}

/// Asserts that a name that is not one of the run's objects is refused: a second spelling of
/// `pack` (`pack0`), an index past the run or zero-padded, a slotted spelling (`0`, `L` alone),
/// another case, a sign, empty.
#[test]
fn a_name_that_is_no_objects_is_refused() {
    for name in [
        "",
        "pack0",
        "pack8",
        "pack01",
        "pack+1",
        "packL1",
        "pack-1",
        "PACK",
        "Pack",
        "pack ",
        "0",
        "1",
        "L",
        "L8",
        "L01",
        "l0",
        "LL0",
        "Lx",
        "out",
        "pack.part",
    ] {
        assert_eq!(
            Slot::from_object_name(name),
            Err(TaskKeyError::Name(name.to_owned())),
            "{name:?}"
        );
    }
}

/// Asserts that a key of a put directory is `<directory>/<name>`, that it parses back to the
/// same build, MAC, minute and slot in the put-directory layout, and that it names its
/// directory.
#[test]
fn a_put_dir_key_roundtrips_and_names_its_directory() {
    for (slot, name) in [
        (Slot::Pack(0), "pack"),
        (Slot::Pack(7), "pack7"),
        (Slot::Large(0), "L0"),
        (Slot::Large(7), "L7"),
    ] {
        let key = TaskPackKey::in_put_dir(BUILD, mac(), MINUTE, slot).expect("a key");
        let text = key.to_string();
        assert_eq!(text, format!("{}/{name}", dir()));
        let parsed = TaskPackKey::parse(&text).expect("parses");
        assert_eq!(parsed, key);
        assert_eq!(parsed.layout(), KeyLayout::PutDir);
        assert_eq!(parsed.build(), BUILD);
        assert_eq!(parsed.mac(), &mac());
        assert_eq!(parsed.minute(), MINUTE);
        assert_eq!(parsed.slot(), slot);
        assert_eq!(parsed.minted_unix_secs(), 1_790_000_100);
        assert_eq!(parsed.put_dir(), Some(dir()));
    }
}

/// Asserts that the same build, MAC, minute and slot give two different keys in the two
/// layouts, that each parses to its own, and that a slotted key names no put directory.
#[test]
fn the_two_layouts_never_share_a_key() {
    let slotted = TaskPackKey::new(BUILD, mac(), MINUTE, Slot::Large(2)).expect("a key");
    let in_dir = TaskPackKey::in_put_dir(BUILD, mac(), MINUTE, Slot::Large(2)).expect("a key");
    assert_ne!(slotted, in_dir);
    assert_ne!(slotted.to_string(), in_dir.to_string());
    assert_eq!(slotted.layout(), KeyLayout::Slotted);
    assert_eq!(slotted.put_dir(), None);
    assert_eq!(
        TaskPackKey::parse(&slotted.to_string()).expect("parses"),
        slotted
    );
    assert_eq!(
        TaskPackKey::parse(&in_dir.to_string()).expect("parses"),
        in_dir
    );
}

/// Asserts each refusal of a put-directory key for its own reason: a name that is no object's,
/// a nonce that is not 40 lowercase hex digits (a slotted nonce among them), a slot past the
/// run, a `/` more, and a build that is no build.
#[test]
fn a_malformed_put_dir_key_is_refused_for_its_reason() {
    assert_eq!(
        TaskPackKey::parse(&format!("{}/pack0", dir())),
        Err(TaskKeyError::Name("pack0".to_owned()))
    );
    assert_eq!(
        TaskPackKey::parse(&format!("{}/", dir())),
        Err(TaskKeyError::Name(String::new()))
    );
    for nonce in [
        format!("{MAC_HEX}{MINUTE_HEX}-0"),
        format!("{MAC_HEX}{}", &MINUTE_HEX[..7]),
        format!("{}{MINUTE_HEX}", MAC_HEX.to_uppercase()),
        String::new(),
    ] {
        assert_eq!(
            TaskPackKey::parse(&format!("cas/b/{BUILD}/{nonce}/pack")),
            Err(TaskKeyError::Nonce(nonce.clone())),
            "{nonce:?}"
        );
    }
    assert!(matches!(
        TaskPackKey::parse(&format!("{}/pack/L0", dir())),
        Err(TaskKeyError::NotAPackKey(_))
    ));
    assert_eq!(
        TaskPackKey::parse(&format!("cas/b/a.b/{MAC_HEX}{MINUTE_HEX}/pack")),
        Err(TaskKeyError::BuildCharacter('.'))
    );
    assert_eq!(
        TaskPackKey::in_put_dir(BUILD, mac(), MINUTE, Slot::Pack(MAX_PACKS)),
        Err(TaskKeyError::SlotRange(Slot::Pack(MAX_PACKS)))
    );
}

/// Asserts the message of a put directory's MAC: its own domain tag, the build, the action and
/// the minute, and no slot; that it is not the v1 message of the same fields (whose MAC a
/// slotted key of slot 0 may carry); and that a changed lease, action or minute changes it.
#[test]
fn the_put_dir_mac_input_has_its_own_tag_and_no_slot() {
    let input = mac_input_put_dir("lease", &action(1, 5), 10).expect("input");
    let mut expected = b"kbf-task-pack/put-dir/v1\0lease\0".to_vec();
    expected.extend_from_slice(&[1; 32]);
    expected.extend_from_slice(&5u64.to_be_bytes());
    expected.extend_from_slice(&10u32.to_be_bytes());
    assert_eq!(input, expected);
    assert_ne!(
        input,
        mac_input_v1("lease", &action(1, 5), 10).expect("input")
    );
    for other in [
        mac_input_put_dir("lease2", &action(1, 5), 10),
        mac_input_put_dir("lease", &action(2, 5), 10),
        mac_input_put_dir("lease", &action(1, 6), 10),
        mac_input_put_dir("lease", &action(1, 5), 11),
    ] {
        assert_ne!(other.expect("input"), input);
    }
    assert_eq!(
        mac_input_put_dir("a/b", &action(1, 5), 10),
        Err(TaskKeyError::BuildCharacter('/'))
    );
}
