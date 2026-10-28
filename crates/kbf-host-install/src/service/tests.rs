use super::{Descriptor, Entry, KBF, KBF_TREE, ServiceName};
use crate::request::{BadCommand, Unit};

fn entry(artifact: &str, binary: &str, tree: &str, units: &[&str]) -> Entry {
    let quoted: Vec<String> = units.iter().map(|u| format!("{u:?}")).collect();
    toml::from_str(&format!(
        "artifact = {artifact:?}\nbinary = {binary:?}\ntree = {tree:?}\nunits = [{}]\n",
        quoted.join(", ")
    ))
    .unwrap()
}

fn agent() -> Entry {
    entry("js-agent", "js-agent", "js-agent", &["js-agent.service"])
}

/// A service name is a lower-case letter, then lower-case letters, digits and `-`, at most 32
/// bytes, not ending in `-`: it is spelled into requests, file names and unit names.
#[test]
fn a_service_name_is_a_plain_lower_case_word() {
    for good in ["kbf", "js-agent", "a", "a1-b2", &"a".repeat(32)] {
        assert_eq!(ServiceName::new(good).unwrap().as_str(), good);
    }
    for bad in [
        "",
        "1a",
        "-a",
        "a-",
        "A",
        "a_b",
        "a b",
        "a/b",
        "a=b",
        "..",
        &"a".repeat(33),
    ] {
        assert_eq!(ServiceName::new(bad), Err(BadCommand), "{bad:?}");
    }
    assert!(ServiceName::kbf().is_kbf());
    assert!(!ServiceName::new("js-agent").unwrap().is_kbf());
    assert_eq!(ServiceName::kbf().as_str(), KBF);
}

/// kbf's own descriptor is built in and is what the helper has always done: the release files
/// are `kbf-...`, the program is `kbf`, and the releases are in the installer's own tree.
#[test]
fn kbfs_descriptor_is_the_built_in_one() {
    let units = vec![Unit::service("kbf-daemon.service").unwrap()];
    let kbf = Descriptor::kbf(units.clone());
    assert_eq!(kbf.name, ServiceName::kbf());
    assert_eq!((kbf.artifact.as_str(), kbf.binary.as_str()), ("kbf", "kbf"));
    assert_eq!(kbf.tree, KBF_TREE);
    assert_eq!(kbf.units, units);
}

/// A table with its four keys makes the descriptor it says.
#[test]
fn a_service_table_makes_its_descriptor() {
    let entry = entry(
        "js-agent",
        "js-agent",
        "js-agent-host",
        &["js-agent.service", "js-agent-helper@a.service"],
    );
    let d = Descriptor::from_entry("js-agent", &entry).unwrap();
    assert_eq!(d.name.as_str(), "js-agent");
    assert_eq!(
        (d.artifact.as_str(), d.binary.as_str()),
        ("js-agent", "js-agent")
    );
    assert_eq!(d.tree, "js-agent-host");
    let units: Vec<&str> = d.units.iter().map(Unit::as_str).collect();
    assert_eq!(units, ["js-agent.service", "js-agent-helper@a.service"]);
}

/// A key left out is a table that does not make a descriptor, and one this build does not know
/// is a table that does not parse: nothing has a default.
#[test]
fn every_key_is_required_and_none_is_unknown() {
    for text in [
        "binary = \"a\"\ntree = \"a\"\nunits = [\"a.service\"]\n",
        "artifact = \"a\"\nbinary = \"a\"\nunits = [\"a.service\"]\n",
    ] {
        assert!(toml::from_str::<Entry>(text).is_err(), "{text}");
    }
    for text in [
        "artifact = \"a\"\ntree = \"a\"\nunits = [\"a.service\"]\n",
        "artifact = \"a\"\nbinary = \"a\"\ntree = \"a\"\n",
    ] {
        let entry: Entry = toml::from_str(text).unwrap();
        let err = Descriptor::from_entry("a", &entry).unwrap_err();
        assert!(err.contains("required"), "{text}: {err}");
    }
    for text in [
        "artifact = \"a\"\nbinary = \"a\"\ntree = \"a\"\nunits = []\nextra = 1\n",
        "kind = \"vm\"\nartifact = \"a\"\nbinary = \"a\"\ntree = \"a\"\nunits = []\n",
    ] {
        assert!(toml::from_str::<Entry>(text).is_err(), "{text}");
    }
}

/// Kbf's names are kbf's, and a name that is not a name is not one.
#[test]
fn a_service_cannot_take_kbfs_name_or_the_family_of_its_units() {
    for name in ["kbf", "kbf-daemon", "kbf-x", "Js", "js-", ""] {
        let err = Descriptor::from_entry(name, &agent()).unwrap_err();
        assert!(!err.is_empty(), "{name:?}");
    }
    // A name that only starts with the letters is another service's.
    let kbfish = entry("kbfish", "kbfish", "kbfish", &["kbfish.service"]);
    assert!(Descriptor::from_entry("kbfish", &kbfish).is_ok());
}

/// What a descriptor names is checked: the artifact, the program, the tree and the units.
#[test]
fn a_descriptor_that_names_something_not_its_own_is_refused() {
    let refused = |name: &str, entry: Entry, word: &str| {
        let err = Descriptor::from_entry(name, &entry).unwrap_err();
        assert!(err.contains(word), "{err}");
    };
    let unit = ["js-agent.service"];
    // The artifact is a file-name prefix.
    refused(
        "js-agent",
        entry("-a", "js-agent", "js-agent", &unit),
        "artifact",
    );
    refused(
        "js-agent",
        entry("JS", "js-agent", "js-agent", &unit),
        "artifact",
    );
    // The program is a plain file name, and not the marker of a verified release.
    for binary in [
        "",
        ".hidden",
        "a/b",
        "..",
        "a b",
        "VERIFIED",
        &"a".repeat(65),
    ] {
        refused(
            "js-agent",
            entry("js-agent", binary, "js-agent", &unit),
            "file name",
        );
    }
    // The tree is the service's own: its name, or its name and a dash and more.
    for tree in [
        "other",
        "kbf-installer",
        "js",
        "js-agent-",
        "js-agent--",
        "js-agentx",
        "JS-agent",
        "js-agent/x",
        "js-agent..",
        &format!("js-agent-{}", "a".repeat(60)),
    ] {
        refused(
            "js-agent",
            entry("js-agent", "js-agent", tree, &unit),
            "tree",
        );
    }
    // There is a unit to restart, and each is the service's own.
    refused(
        "js-agent",
        entry("js-agent", "js-agent", "js-agent", &[]),
        "no unit",
    );
    for unit in [
        "sshd.service",
        "kbf-daemon.service",
        "js-agentx.service",
        "js-agent-.service",
        "js-agent.timer",
        "js-agent",
        "js-agent@-.service",
        "js-agent@A.service",
        "Js-agent.service",
    ] {
        refused(
            "js-agent",
            entry(
                "js-agent",
                "js-agent",
                "js-agent",
                &["js-agent.service", unit],
            ),
            "its units",
        );
    }
}
