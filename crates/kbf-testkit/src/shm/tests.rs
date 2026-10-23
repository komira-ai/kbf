use super::*;

fn unique(tag: &str) -> String {
    format!("kt{}{tag}", std::process::id() % 100_000)
}

/// Catches: a step spelled wrongly accepted, a count of zero (no names to look after),
/// and a prefix that would make a name over the 31 bytes macOS allows or contain a slash.
#[test]
fn the_steps_are_checked() {
    assert_eq!(parse(&["exit", "1"]), Ok(None));
    assert_eq!(
        parse(&["shm-owner", "p", "3"]),
        Ok(Some(Shm::Owner {
            prefix: "p",
            count: 3,
            hold: HOLD
        }))
    );
    assert_eq!(
        parse(&["shm-owner", "p", "3", "0.5"]),
        Ok(Some(Shm::Owner {
            prefix: "p",
            count: 3,
            hold: Duration::from_millis(500)
        }))
    );
    assert_eq!(
        parse(&["shm-tracker", "p", "3"]),
        Ok(Some(Shm::Tracker {
            prefix: "p",
            count: 3
        }))
    );
    for bad in [
        ["shm-owner", "p", "0"],
        ["shm-owner", "p", "x"],
        ["shm-owner", "", "1"],
        ["shm-owner", "a/b", "1"],
        ["shm-owner", "abcdefghijklmnopqrs", "1"],
    ] {
        assert!(
            matches!(parse(&bad), Err(Error::Usage(_))),
            "{bad:?} was accepted"
        );
    }
}

/// Catches: names that differ between the owner and the test's check, a probe that
/// creates what it looks for (it would find it the next time), one that cannot see a
/// name that is there, and an unlink that leaves one.
#[test]
fn present_sees_exactly_the_names_that_were_made_and_unlink_removes_them() {
    let prefix = unique("a");
    assert_eq!(present(&prefix, 2), Vec::<String>::new());
    assert_eq!(
        present(&prefix, 2),
        Vec::<String>::new(),
        "a look made a name"
    );
    for (kind, name) in names(&prefix, 2) {
        make(kind, &name).expect("made");
    }
    let found = present(&prefix, 2);
    assert_eq!(
        found,
        [
            format!("/{prefix}-m0"),
            format!("/{prefix}-m1"),
            format!("/{prefix}-s0"),
            format!("/{prefix}-s1")
        ]
    );
    // Made again over what is there.
    for (kind, name) in names(&prefix, 1) {
        make(kind, &name).expect("made again");
    }
    unlink(&prefix, 2);
    assert_eq!(present(&prefix, 2), Vec::<String>::new());
    unlink(&prefix, 2);
}

/// Catches: a name that cannot be made going unnoticed (the owner would say ready with
/// fewer names than asked): an over-long name is an error for both kinds, and `ready`
/// takes the tracker's line and nothing else.
#[test]
fn a_name_that_cannot_be_made_is_an_error_and_ready_is_only_ready() {
    let long = CString::new(format!("/{}", "n".repeat(400))).expect("name");
    assert!(make(Kind::Memory, &long).is_err());
    assert!(make(Kind::Semaphore, &long).is_err());
    assert!(!exists(Kind::Memory, &long));
    assert_eq!(ready("ready\n"), Ok(()));
    assert_eq!(
        ready("oops\n"),
        Err(Error::Failed("the tracker said \"oops\\n\"".to_owned()))
    );
}
