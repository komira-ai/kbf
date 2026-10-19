//! Finding the stand-ins beside a test binary.

use super::*;

#[test]
fn a_binary_is_found_in_the_profile_directory_above_the_test_binarys() {
    // Catches: a lookup in `deps/` (where only test binaries are), or in the working
    // directory, which would make the stand-ins a thing each caller finds its own way.
    let dir = crate::scratch("stand-in");
    std::fs::create_dir_all(dir.join("debug/deps")).expect("dirs");
    std::fs::write(dir.join("debug/fake"), b"").expect("file");
    let exe = dir.join("debug/deps/beta_b0-1234");
    assert_eq!(sibling_of(&exe, "fake"), dir.join("debug/fake"));
}

#[test]
fn a_binary_is_found_above_a_nested_test_binary_too() {
    // Catches: a lookup of exactly two directories up, which the coverage job's layout
    // (`debug/build/<package>/<hash>/deps/<test binary>`) defeats, and the nearest
    // directory not being preferred over a farther one.
    let dir = crate::scratch("stand-in-nested");
    std::fs::create_dir_all(dir.join("debug/build/kbf-harness/ab12/deps")).expect("dirs");
    std::fs::write(dir.join("debug/fake"), b"").expect("file");
    std::fs::write(dir.join("fake"), b"").expect("file");
    let exe = dir.join("debug/build/kbf-harness/ab12/deps/kbf_harness-1234");
    assert_eq!(sibling_of(&exe, "fake"), dir.join("debug/fake"));
    std::fs::write(dir.join("debug/build/fake"), b"").expect("file");
    assert_eq!(sibling_of(&exe, "fake"), dir.join("debug/build/fake"));
}

#[test]
#[should_panic(expected = "cargo build -p kbf-harness --bins")]
fn a_binary_that_was_not_built_says_how_to_build_it() {
    // Catches: a missing stand-in reported as a spawn failure far from the cause.
    let dir = crate::scratch("stand-in-missing");
    let _ = sibling_of(&dir.join("debug/deps/x"), "not-there");
}

#[test]
fn the_stand_ins_of_this_package_are_built_beside_its_tests() {
    // Catches: a `[[bin]]` renamed in Cargo.toml and not here (a caller then fails with
    // the message above, only when it runs).
    assert!(sibling_bin(FAKE_BUCK2).is_file(), "{FAKE_BUCK2}");
}
