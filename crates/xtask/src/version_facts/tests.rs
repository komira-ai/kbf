use super::*;

const COMMIT: &str = "c0ffee0123456789c0ffee0123456789c0ffee01";

#[test]
fn the_facts_file_is_named_like_the_tarball_of_kbf() {
    // Catches: a facts file whose name is not the stem of the `kbf` tarball `package` writes
    // for the commit and platform (nothing would pair the two), and a platform whose name
    // is spelled another way.
    for platform in Platform::ALL {
        let stem = format!("kbf-{COMMIT}-{}", platform.name());
        let facts = format!("{stem}.version.json");
        assert_eq!(name(COMMIT, platform), facts);
        assert_eq!(facts_of(&format!("{stem}.tar.gz")), Some(facts));
    }
}

#[test]
fn only_a_kbf_tarball_has_facts() {
    // Catches: facts demanded of every tarball (`kbf-daemon`'s starts with `kbf-` as well),
    // of a file that is no tarball, or of a name that is no commit and platform. One row
    // per way `facts_of` can say no.
    let others = [
        format!("kbf-daemon-{COMMIT}-linux-x86_64.tar.gz"),
        format!("kbf-installer-{COMMIT}-darwin-arm64.tar.gz"),
        format!("kbf-{COMMIT}-linux-x86_64.cdx.json"),
        format!("kbf-{COMMIT}-linux-x86_64.tar.gz.sigstore.json"),
        format!("kbf-{COMMIT}-linux-x86_64.version.json"),
        format!("daemon-{COMMIT}-linux-x86_64.tar.gz"),
        format!("kbfx-{COMMIT}-linux-x86_64.tar.gz"),
        format!("kbf-{COMMIT}-freebsd-x86_64.tar.gz"),
        "kbf.tar.gz".to_owned(),
        "kbf-abc.tar.gz".to_owned(),
        "kbf-abc-linux-x86_64.tar.gz".to_owned(),
    ];
    for asset in others {
        assert_eq!(facts_of(&asset), None, "{asset}");
    }
}

#[test]
fn a_facts_file_names_the_tarball_it_states_the_facts_of() {
    // Catches: the reverse pairing naming another file than the tarball `package` writes,
    // and taking as facts what is not (another binary's, a `.bak`, an unknown platform's, a
    // name with no commit). The round trip with `facts_of` is the pairing both ways.
    for platform in Platform::ALL {
        let facts = name(COMMIT, platform);
        let tarball = format!("kbf-{COMMIT}-{}.tar.gz", platform.name());
        assert_eq!(tarball_of(&facts), Some(tarball.clone()));
        assert_eq!(facts_of(&tarball), Some(facts));
    }
    let others = [
        format!("kbf-daemon-{COMMIT}-linux-x86_64.version.json"),
        format!("kbf-{COMMIT}-linux-x86_64.version.json.bak"),
        format!("kbf-{COMMIT}-freebsd-x86_64.version.json"),
        format!("kbf-{COMMIT}-linux-x86_64.tar.gz"),
        "kbf-abc-linux-x86_64.version.json".to_owned(),
        "kbf.version.json".to_owned(),
    ];
    for file in others {
        assert_eq!(tarball_of(&file), None, "{file}");
    }
}

#[test]
fn only_the_facts_of_kbf_for_the_commit_are_its_assets() {
    // Catches: any `.version.json` taken as an asset of the commit (another commit's,
    // another binary's, an unknown platform's, a renamed copy). Catches: the exact name
    // replaced by the suffix.
    let other = "badbad0123456789badbad0123456789badbad01";
    for platform in Platform::ALL {
        assert!(is_facts_of(&name(COMMIT, platform), COMMIT));
        assert!(!is_facts_of(&name(other, platform), COMMIT));
    }
    for file in [
        format!("kbf-daemon-{COMMIT}-linux-arm64.version.json"),
        format!("kbf-{COMMIT}-windows-x86_64.version.json"),
        format!("kbf-{COMMIT}-linux-arm64.version.json.bak"),
    ] {
        assert!(!is_facts_of(&file, COMMIT), "{file}");
    }
}
