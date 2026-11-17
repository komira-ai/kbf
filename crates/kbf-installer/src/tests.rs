use super::*;
use crate::testkit::{A, B};

/// Catches a commit id check that takes a short or long id, an upper-case one (Git
/// would name the same commit, and a second release directory would hold it), a
/// non-hex character, or a multi-byte character of the right byte length; and serde,
/// parsing or printing that goes around the check.
#[test]
fn a_commit_id_is_exactly_40_lowercase_hex_characters() {
    let id = CommitId::new(B).expect("a commit");
    assert_eq!((id.as_str(), id.to_string()), (B, B.to_owned()));
    assert_eq!(String::from(id.clone()), B);
    assert_eq!(B.parse::<CommitId>(), Ok(id.clone()));
    assert_eq!(
        serde_json::to_string(&id).expect("serializes"),
        format!("\"{B}\"")
    );
    let refused = [
        "",
        &B[..39],
        &format!("{B}0"),
        &B.to_uppercase(),
        &format!("{}g", &B[..39]),
        &format!("{}é", &B[..38]),
        &format!("{} ", &B[..39]),
        // What would make a release path leave `releases/` or name it twice.
        "/",
        ".",
        "..",
        &format!("{}/", &B[..39]),
        &format!("/{}", &B[..39]),
        &format!("{}.", &B[..39]),
        &format!("..{}", &B[..38]),
        &format!("{}\0", &B[..39]),
        &format!("{}A", &B[..39]),
    ];
    for bad in refused {
        assert_eq!(
            CommitId::new(bad),
            Err(NotACommit(bad.to_owned())),
            "{bad:?}"
        );
        let json = serde_json::to_string(bad).expect("a JSON string");
        let err = serde_json::from_str::<CommitId>(&json).expect_err(bad);
        assert!(err.to_string().contains("is not a commit id"), "{err}");
    }
    assert_eq!(
        NotACommit("x".to_owned()).to_string(),
        "\"x\" is not a commit id (40 lowercase hex characters)"
    );
}

/// Catches a layout path that is not a valid host path (it would panic on first use), a
/// release, file or stage path built in the wrong directory, and a link target that is
/// absolute or has a directory in it (a copied tree would point out of itself).
#[test]
fn the_layout_names_every_path_under_its_directory() {
    use kbf_host::path::Root;
    use layout::*;
    let linux = Layout::linux(Root::host());
    let a = CommitId::new(A).expect("a commit");
    assert_eq!(
        linux.release_dir(&a).as_str(),
        format!("/var/lib/kbf-installer/releases/{A}")
    );
    let files = [
        (RELEASE_BINARY, "kbf"),
        (RELEASE_UNITS, "units"),
        (RELEASE_MANIFEST, "manifest.json"),
    ];
    for (name, want) in files {
        assert_eq!(
            linux.release_file(&a, name).as_str(),
            format!("/var/lib/kbf-installer/releases/{A}/{want}")
        );
    }
    assert_eq!(
        linux.stage_dir(&a, "x1").as_str(),
        format!("/var/lib/kbf-installer/releases/.stage-{A}.x1")
    );
    assert_eq!(release_link(&a), std::path::PathBuf::from(A));
}

/// Catches a path that has drifted from the contract (`docs/design/kbf-installer.md`, section
/// 3): the installer tree, the links inside `releases/`, the state and its backup, the lock
/// under its own directory, and the trust directory. The state is kept in the install root.
#[test]
fn the_layout_names_the_tree_the_state_the_lock_and_the_trust_directory() {
    use kbf_host::path::Root;
    use layout::{Layout, Platform};
    let linux = Layout::linux(Root::host());
    assert_eq!(linux.platform(), Platform::Linux);
    assert_eq!(linux.root(), &Root::host());
    let paths = |l: &Layout| {
        [
            l.install_root().to_string(),
            l.releases().to_string(),
            l.current().to_string(),
            l.previous().to_string(),
            l.state_dir().to_string(),
            l.state_file().to_string(),
            l.state_backup().to_string(),
            l.lock().to_string(),
            l.work_dir().to_string(),
            l.trust_dir().to_string(),
            l.gh().to_string(),
            l.trusted_root().to_string(),
            l.settings().to_string(),
        ]
    };
    assert_eq!(
        paths(&linux),
        [
            "/var/lib/kbf-installer",
            "/var/lib/kbf-installer/releases",
            "/var/lib/kbf-installer/releases/current",
            "/var/lib/kbf-installer/releases/previous",
            "/var/lib/kbf-installer",
            "/var/lib/kbf-installer/state.json",
            "/var/lib/kbf-installer/state.json.bak",
            "/run/kbf-installer/lock",
            "/var/lib/kbf-installer/work",
            "/usr/share/kbf/trust",
            "/usr/share/kbf/trust/gh",
            "/usr/share/kbf/trust/trusted_root.jsonl",
            "/usr/share/kbf/trust/installer.conf",
        ]
    );
    assert_eq!(
        linux.join(&linux.current()),
        std::path::PathBuf::from("/var/lib/kbf-installer/releases/current")
    );
}

/// Catches the platform spelled differently on the command line than it prints, and an
/// unknown name accepted.
#[test]
fn a_platform_is_named_linux() {
    use layout::Platform;
    assert_eq!(
        Platform::Linux.name().parse::<Platform>(),
        Ok(Platform::Linux)
    );
    assert_eq!(
        "macos".parse::<Platform>(),
        Err("\"macos\" is not a platform (linux)".to_owned())
    );
    assert_eq!(Platform::current(), Platform::Linux);
}
