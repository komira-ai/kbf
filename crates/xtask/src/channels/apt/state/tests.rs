use super::*;

fn strings(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| (*w).to_owned()).collect()
}

#[test]
fn the_architectures_are_the_defaults_and_every_one_the_release_lists_once_sorted() {
    // Catches: an index rewritten without an architecture another product published.
    let release = "Origin: x\nSHA256:\n aa 10 main/binary-amd64/Packages\n bb 5 main/binary-amd64/Packages.gz\n cc 7 main/binary-s390x/Packages\n dd 7 main/binary-s390x/Packages\n";
    assert_eq!(archs_of_release(release), ["amd64", "arm64", "s390x"]);
    assert_eq!(archs_of_release(""), ["amd64", "arm64"]);
}

#[test]
fn a_word_that_is_not_a_clean_architecture_is_never_a_path() {
    // Catches: a Release line that walks out of dists/stable/ through an architecture name.
    let release = " aa 1 main/binary-../../x/Packages\n bb 1 main/binary-A_B/Packages\n cc 1 main/binary-/Packages\n";
    assert_eq!(archs_of_release(release), ["amd64", "arm64"]);
    assert!(valid_arch("arm64") && valid_arch("all") && !valid_arch("") && !valid_arch("A"));
}

#[test]
fn the_index_files_end_with_release_gpg_and_in_release_the_commit_point() {
    // Catches: InRelease written before the files it lists.
    let files = index_files(&strings(&["amd64", "arm64"]));
    assert_eq!(
        files,
        [
            "main/binary-amd64/Packages",
            "main/binary-amd64/Packages.gz",
            "main/binary-arm64/Packages",
            "main/binary-arm64/Packages.gz",
            "Release",
            "Release.gpg",
            "InRelease"
        ]
    );
    assert_eq!(
        index_key(Suite::Stable, "Release"),
        "apt/dists/stable/Release"
    );
    assert!(
        is_packages_file(&files[1]) && is_packages_file(&files[0]) && !is_packages_file("Release")
    );
}

#[test]
fn keys_stay_below_the_product_and_the_suite() {
    // Catches: a file name that writes outside kbf's pool directory.
    assert_eq!(
        pool_key("kbf_1.2.3_amd64.deb").expect("key"),
        "apt/pool/kbf/kbf_1.2.3_amd64.deb"
    );
    for bad in ["", ".hidden.deb", "a/b.deb", "../x.deb", "x.txt"] {
        assert!(pool_key(bad).is_err(), "{bad:?}");
    }
    assert_eq!(
        by_hash_key(Suite::Stable, "main/binary-amd64/Packages.gz", "ab"),
        "apt/dists/stable/main/binary-amd64/by-hash/SHA256/ab"
    );
    assert_eq!(
        by_hash_key(Suite::Stable, "Packages", "ab"),
        "apt/dists/stable//by-hash/SHA256/ab"
    );
}

#[test]
fn the_hash_a_release_lists_is_found_for_exactly_that_path() {
    // Catches: the hash of another file, a short hash, or a line with extra words taken.
    let h = "a".repeat(64);
    let release = format!(
        "SHA256:\n {h} 10 main/binary-amd64/Packages\n bad 5 main/binary-arm64/Packages\n {h} 5 main/binary-s390x/Packages extra\n x\n {h} 4\n"
    );
    assert_eq!(
        release_hash_of(&release, "main/binary-amd64/Packages"),
        Some(h.as_str())
    );
    assert_eq!(
        release_hash_of(&release, "main/binary-arm64/Packages"),
        None
    );
    assert_eq!(
        release_hash_of(&release, "main/binary-s390x/Packages"),
        None
    );
    assert_eq!(release_hash_of(&release, "Release"), None);
}

#[test]
fn etags_round_trip_and_a_bad_line_is_refused() {
    // Catches: an etags file that reads back differently, or a malformed line taken as an ETag.
    let entries = vec![
        ("Release".to_owned(), "\"abc\"".to_owned()),
        ("InRelease".to_owned(), ABSENT.to_owned()),
    ];
    let text = render_etags(&entries);
    assert_eq!(text, "Release\t\"abc\"\nInRelease\t-\n");
    assert_eq!(parse_etags(&text).expect("parse"), entries);
    assert_eq!(etag_of(&entries, "InRelease"), Some("-"));
    assert_eq!(etag_of(&entries, "Nope"), None);
    for bad in ["Release", "\tabc", "Release\t", "Release\ta\tb"] {
        assert!(parse_etags(bad).is_err(), "{bad:?}");
    }
    assert!(parse_etags("\n").expect("empty").is_empty());
}
