//! The subordinate id file reader and the block chooser.

use super::*;

fn r(owner: &str, start: u64, count: u64) -> Range {
    Range {
        owner: owner.to_owned(),
        start,
        count,
    }
}

/// Catches a reader that skips a line it cannot read (a range it missed is one a new
/// block would overlap), takes a zero count or a range past the id space, or reads a
/// comment or a blank line as a range.
#[test]
fn the_reader_takes_ranges_and_refuses_every_other_line() {
    let text = "# comment\n\nalice:100000:65536\n  1001:165536:65536  \nbob:4294901759:65536\n";
    assert_eq!(
        parse("/etc/subuid", text).expect("valid"),
        [
            r("alice", 100_000, 65536),
            r("1001", 165_536, 65536),
            r("bob", 4_294_901_759, 65536)
        ]
    );
    for (bad, line) in [
        ("alice:1:2:3\n", 1),
        ("alice:1\n", 1),
        ("# ok\nalice:x:2\n", 2),
        ("alice:1:y\n", 1),
        (":1:2\n", 1),
        ("alice:1:0\n", 1),
        ("alice:4294901760:65536\n", 1),
        ("alice:-1:2\n", 1),
    ] {
        match parse("/etc/subuid", bad) {
            Err(SubIdError::Line { file, line: l, .. }) => {
                assert_eq!((file.as_str(), l), ("/etc/subuid", line), "{bad:?}");
            }
            other => panic!("{bad:?}: {other:?}"),
        }
    }
    assert_eq!(parse("f", "").expect("empty"), []);
}

/// Catches the chooser taking the floor without reading the file (the mutant in the
/// design's test plan), and each way a neighbour can sit on the first block: below it, on
/// its first id, on its last id, spanning it, exactly equal, and followed by another.
#[test]
fn the_block_is_the_first_free_one_at_or_above_the_floor() {
    let free = |ranges: &[Range]| decide("/etc/subuid", ranges, "kbf", None);
    assert_eq!(free(&[]), Ok(Need::Add(200_000)));
    // Below the floor: no clash.
    assert_eq!(free(&[r("a", 100_000, 65536)]), Ok(Need::Add(200_000)));
    // Ends exactly at the floor: no clash.
    assert_eq!(free(&[r("a", 134_464, 65536)]), Ok(Need::Add(200_000)));
    // One id inside the block's start.
    assert_eq!(free(&[r("a", 134_465, 65536)]), Ok(Need::Add(200_001)));
    // The block's last id.
    assert_eq!(free(&[r("a", 265_535, 10)]), Ok(Need::Add(265_545)));
    // The very next id after the block: the floor still fits.
    assert_eq!(free(&[r("a", 265_536, 10)]), Ok(Need::Add(200_000)));
    // Exactly the block: the next one.
    assert_eq!(free(&[r("a", 200_000, 65536)]), Ok(Need::Add(265_536)));
    // Spanning the block and more.
    assert_eq!(free(&[r("a", 150_000, 500_000)]), Ok(Need::Add(650_000)));
    // Two neighbours in a row, given out of order, and a gap too small between them.
    let ranges = [
        r("c", 300_000, 65536),
        r("a", 200_000, 65536),
        r("b", 265_536 + 100, 30_000),
    ];
    assert_eq!(free(&ranges), Ok(Need::Add(365_536)));
    // The gap that is big enough is used.
    let ranges = [r("a", 200_000, 65536), r("b", 331_072, 10)];
    assert_eq!(free(&ranges), Ok(Need::Add(265_536)));
}

/// Catches a chooser that is wrong on shapes the table does not name: against a brute
/// force scan of small id spaces (blocks of 3 from a floor of 5, ranges inside 0..40), the
/// choice is the lowest start that overlaps nothing.
#[test]
fn the_choice_matches_a_brute_force_scan() {
    let mut seed = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = |n: u64| {
        seed = seed
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        (seed >> 33) % n
    };
    for _ in 0..2000 {
        let ranges: Vec<Range> = (0..next(6))
            .map(|i| r(&format!("u{i}"), next(40), 1 + next(12)))
            .collect();
        let want = (5..).find(|s| ranges.iter().all(|x| !x.overlaps(*s, 3)));
        assert_eq!(first_free(&ranges, 5, 3), want, "{ranges:?}");
    }
}

/// Catches an owner match on the name alone (a range recorded under the uid is the same
/// user's), a range of the user's own that is large enough being added to, a smaller one
/// being widened instead of refused, and the pick when only the name matches.
#[test]
fn a_range_of_the_users_own_stands_or_is_refused() {
    let big = r("kbf", 100_000, 65536);
    assert_eq!(
        decide("f", std::slice::from_ref(&big), "kbf", Some(998)),
        Ok(Need::Present)
    );
    assert_eq!(
        decide("f", &[r("998", 1000, 70_000)], "kbf", Some(998)),
        Ok(Need::Present),
        "a range under the uid is the user's"
    );
    // Someone else's uid is someone else's.
    assert_eq!(
        decide("f", &[r("998", 200_000, 65536)], "kbf", Some(999)),
        Ok(Need::Add(265_536))
    );
    // Without a uid (the user does not exist yet) a numeric owner is nobody known.
    assert_eq!(
        decide("f", &[r("998", 200_000, 65536)], "kbf", None),
        Ok(Need::Add(265_536))
    );
    let small = r("kbf", 100_000, 1000);
    let e = decide("/etc/subgid", &[small.clone(), r("a", 0, 5)], "kbf", None)
        .expect_err("a smaller range is refused");
    assert_eq!(
        e,
        SubIdError::Smaller {
            file: "/etc/subgid".to_owned(),
            range: small
        }
    );
    assert!(
        e.to_string().contains("kbf:100000:1000 only 1000 ids"),
        "{e}"
    );
    // The larger of two small ones is named; a large one beside a small one stands.
    let e = decide("f", &[r("kbf", 0, 10), r("kbf", 50, 20)], "kbf", None).expect_err("small");
    assert!(e.to_string().contains("kbf:50:20"), "{e}");
    assert_eq!(
        decide("f", &[r("kbf", 0, 10), big], "kbf", None),
        Ok(Need::Present)
    );
}

/// Catches an id space that runs out being reported as a block past `2^32`.
#[test]
fn a_full_id_space_is_refused() {
    let top = r("a", 200_000, 4_294_750_000);
    assert_eq!(
        decide("/etc/subuid", &[top], "kbf", None),
        Err(SubIdError::Exhausted {
            file: "/etc/subuid".to_owned()
        })
    );
    assert_eq!(Range::span(200_000, 65536), "200000-265535");
    assert_eq!(r("kbf", 1, 2).to_string(), "kbf:1:2");
}
