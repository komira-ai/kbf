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

/// Catches a layout path that is not a valid host path (the constant would panic on
/// first use), a release, file or pin path built in the wrong directory, and a link
/// target that is absolute (a copied tree would point out of itself).
#[test]
fn the_layout_names_every_path_under_its_directory() {
    use layout::*;
    for p in [
        KBF_DIR,
        RELEASES,
        CURRENT,
        PREVIOUS,
        STATE_DIR,
        INCOMING,
        STAGED,
        DEPLOYED,
        STATE_FILE,
        CONFIG_DIR,
        POLICY,
        PINS,
        INVENTORY_ALLOW,
        LOCK,
    ] {
        assert_eq!(path(p).as_str(), p);
    }
    let a = CommitId::new(A).expect("a commit");
    assert_eq!(
        release_dir(&a).as_str(),
        format!("/usr/local/kbf/releases/{A}")
    );
    let files = [
        (RELEASE_BINARY, "kbf"),
        (RELEASE_UNITS, "units"),
        (RELEASE_MANIFEST, "manifest.json"),
    ];
    for (name, want) in files {
        assert_eq!(
            release_file(&a, name).as_str(),
            format!("/usr/local/kbf/releases/{A}/{want}")
        );
    }
    assert_eq!(
        pin_file(&a).as_str(),
        format!("/etc/kbf-deploy/pins/{A}.sha256")
    );
    assert_eq!(
        release_link(&a),
        std::path::PathBuf::from(format!("releases/{A}"))
    );
}
