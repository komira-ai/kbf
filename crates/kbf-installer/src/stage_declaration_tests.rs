//! The stage's tests of a release that carries its updater declaration beside its binary
//! (`updater.d/<component>.toml`), and of a member the release carries that no one asked for.
//! They share `stage_tests`' rig.

use super::*;

const DECLARATION: &[u8] = b"name = \"kbf\"\n";

/// The tarball of an honest x86-64 Linux `kbf` and its declaration.
fn with_declaration(declaration: &[u8]) -> Vec<u8> {
    tar_gz(&[
        file("kbf", &elf(0x3e, "payload")),
        file("updater.d/kbf.toml", declaration),
    ])
}

/// Stage `bytes` as the tarball of `kbf`, with the honest programs.
fn stage(rig: &Rig, bytes: &[u8]) -> Result<Staged, Refusal> {
    let (locator, fetched) = rig.fetch(bytes);
    rig.verify(
        &honest(),
        OrderKind::Activate,
        &[locator],
        Some(&facts()),
        fetched,
    )
}

/// Everything under `dir`, every level, sorted.
fn listing(dir: &Path) -> Vec<String> {
    let mut found = Vec::new();
    for item in fs::read_dir(dir).expect("list") {
        let path = item.expect("item").path();
        let name = path.strip_prefix(dir).expect("under").display().to_string();
        if path.is_dir() {
            found.extend(listing(&path).into_iter().map(|n| format!("{name}/{n}")));
        }
        found.push(name);
    }
    found.sort();
    found
}

fn mode_of(path: &Path) -> u32 {
    fs::symlink_metadata(path)
        .expect("stat")
        .permissions()
        .mode()
        & 0o7777
}

/// Catches a declaration that a release carries not reaching the release: the binary and
/// `updater.d/kbf.toml` are both installed in `releases/<commit>/`, the binary 0755 and the
/// declaration 0644 in a 0755 `updater.d/`, beside the marker and nothing else; the stage's
/// scratch is gone and the stage reports the one component.
#[test]
fn a_release_with_a_declaration_installs_the_binary_and_the_declaration() {
    let rig = Rig::new("declaration");
    let staged = stage(&rig, &with_declaration(DECLARATION)).expect("staged");
    assert_eq!(staged.components, ["kbf"]);
    assert!(!staged.already_installed);

    let release = rig.release();
    assert_eq!(
        listing(&release),
        ["VERIFIED", "kbf", "updater.d", "updater.d/kbf.toml"]
    );
    assert_eq!(
        fs::read(release.join("kbf")).expect("binary"),
        elf(0x3e, "payload")
    );
    assert_eq!(
        fs::read(release.join("updater.d/kbf.toml")).expect("declaration"),
        DECLARATION
    );
    assert_eq!(mode_of(&release.join("kbf")), 0o755);
    assert_eq!(mode_of(&release.join("updater.d")), 0o755);
    assert_eq!(mode_of(&release.join("updater.d/kbf.toml")), 0o644);
    let left: Vec<_> = fs::read_dir(rig.work()).expect("work").collect();
    assert!(left.is_empty(), "scratch left: {left:?}");
}

/// Catches a member no one asked for being refused or installed: a README and another
/// service's declaration beside the binary are passed over, and the release holds the
/// binary and the marker alone, as a release of one member does.
#[test]
fn a_member_no_one_asked_for_is_not_installed() {
    let rig = Rig::new("declaration-extra");
    let bytes = tar_gz(&[
        file("README", b"read me"),
        file("kbf", &elf(0x3e, "payload")),
        file("updater.d/tierstore.toml", b"name = \"tierstore\"\n"),
    ]);
    stage(&rig, &bytes).expect("staged");
    assert_eq!(listing(&rig.release()), ["VERIFIED", "kbf"]);
}

/// Catches a release directory taken as the same release though its declaration is not:
/// the same release (declaration and all) staged again is `already_installed`; a directory
/// whose declaration has other bytes, is missing, is extra, or is a link is
/// `release-differs` and left as it was.
#[test]
fn the_same_release_has_the_same_declaration_or_none_in_both() {
    let rig = Rig::new("declaration-again");
    stage(&rig, &with_declaration(DECLARATION)).expect("first");
    let again = stage(&rig, &with_declaration(DECLARATION)).expect("again");
    assert!(again.already_installed);

    let installed = rig.release().join("updater.d/kbf.toml");
    fs::write(&installed, b"name = \"other\"\n").expect("tamper");
    let err = stage(&rig, &with_declaration(DECLARATION)).expect_err("other bytes");
    assert_eq!(err.code(), "release-differs");
    assert_eq!(
        err.to_string(),
        Refusal::Differs {
            path: installed.display().to_string()
        }
        .to_string()
    );
    assert_eq!(fs::read(&installed).expect("kept"), b"name = \"other\"\n");

    // Installed without a declaration, offered with one.
    let rig = Rig::new("declaration-new");
    stage(&rig, &tarball()).expect("first");
    let err = stage(&rig, &with_declaration(DECLARATION)).expect_err("now declared");
    assert_eq!(err.code(), "release-differs");
    assert!(!rig.release().join("updater.d").exists());

    // Installed with a declaration, offered without one.
    let rig = Rig::new("declaration-gone");
    stage(&rig, &with_declaration(DECLARATION)).expect("first");
    let err = stage(&rig, &tarball()).expect_err("no longer declared");
    assert_eq!(err.code(), "release-differs");

    // Installed as a link to a file with the same bytes.
    let rig = Rig::new("declaration-link");
    stage(&rig, &with_declaration(DECLARATION)).expect("first");
    let installed = rig.release().join("updater.d/kbf.toml");
    let elsewhere = rig.dir.join("kbf.toml");
    fs::write(&elsewhere, DECLARATION).expect("seed");
    fs::remove_file(&installed).expect("remove");
    symlink(&elsewhere, &installed).expect("link");
    let err = stage(&rig, &with_declaration(DECLARATION)).expect_err("a link");
    assert_eq!(err.code(), "release-differs");
}

/// Catches a declaration that is unsafe or wrong being installed: a declaration that is a
/// link is a `shape` refusal and one over its cap a `size` refusal, and nothing is installed
/// or run.
#[test]
fn an_unsafe_declaration_is_a_shape_refusal() {
    let link = Item {
        name: b"updater.d/kbf.toml",
        kind: b'2',
        data: b"",
    };
    let big = vec![b'#'; usize::try_from(archive::DECLARATION_CAP).expect("small") + 1];
    for (why, bytes, code, word) in [
        (
            "link",
            tar_gz(&[file("kbf", &elf(0x3e, "payload")), link]),
            "archive-not-regular",
            "shape",
        ),
        (
            "oversize",
            with_declaration(&big),
            "archive-oversize",
            "size",
        ),
    ] {
        let rig = Rig::new(&format!("declaration-{why}"));
        let runner = honest();
        let (locator, fetched) = rig.fetch(&bytes);
        let err = rig
            .verify(
                &runner,
                OrderKind::Activate,
                &[locator],
                Some(&facts()),
                fetched,
            )
            .expect_err(why);
        assert_eq!(err.word(), Some(word), "{why}: {err}");
        assert_eq!(err.code(), code, "{why}");
        assert_eq!(programs(&runner), ["gh"], "{why}: nothing was run");
        rig.assert_nothing_installed();
    }
}
