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

/// What a case does to the installed release before the same release is offered again.
type Plant = fn(&Rig);

/// Catches a release directory whose declarations are not plain taken as the same release:
/// an installed `updater.d` that is a file, a link to a directory holding the same bytes, a
/// declaration that is a link to itself, one that cannot be read, and one in an `updater.d/`
/// that cannot be searched are each
/// `release-differs` against the same release offered again. An installed `updater.d` with
/// no declaration in it is the same as none.
#[test]
fn a_declaration_that_is_not_plain_is_another_release() {
    let cases: [(&str, Plant, Vec<u8>); 5] = [
        (
            "dir-is-a-file",
            |rig| fs::write(rig.release().join("updater.d"), b"a file").expect("plant"),
            tarball(),
        ),
        (
            "dir-is-a-link",
            |rig| {
                let elsewhere = rig.dir.join("elsewhere");
                let installed = rig.release().join("updater.d");
                fs::rename(&installed, &elsewhere).expect("move");
                symlink(&elsewhere, &installed).expect("link");
            },
            with_declaration(DECLARATION),
        ),
        (
            "file-loops",
            |rig| {
                let installed = rig.release().join("updater.d/kbf.toml");
                fs::remove_file(&installed).expect("remove");
                symlink("kbf.toml", &installed).expect("loop");
            },
            with_declaration(DECLARATION),
        ),
        (
            "file-unreadable",
            |rig| {
                let installed = rig.release().join("updater.d/kbf.toml");
                fs::set_permissions(installed, fs::Permissions::from_mode(0o000)).expect("chmod");
            },
            with_declaration(DECLARATION),
        ),
        (
            "dir-unsearchable",
            |rig| {
                let dir = rig.release().join("updater.d");
                fs::set_permissions(dir, fs::Permissions::from_mode(0o000)).expect("chmod");
            },
            with_declaration(DECLARATION),
        ),
    ];
    for (why, plant, offered) in cases {
        let rig = Rig::new(&format!("declaration-plain-{why}"));
        let first = if why == "dir-is-a-file" {
            tarball()
        } else {
            with_declaration(DECLARATION)
        };
        stage(&rig, &first).expect("first");
        plant(&rig);
        if why.ends_with("-unreadable") | why.ends_with("-unsearchable")
            && fs::symlink_metadata(rig.release().join("updater.d/kbf.toml"))
                .and_then(|_| File::open(rig.release().join("updater.d/kbf.toml")))
                .is_ok()
        {
            // The superuser reads a file of mode 0; the case says nothing then.
            eprintln!("{why}: skipped, as this process reads a file of mode 0 (it is root)");
            let _ = fs::set_permissions(
                rig.release().join("updater.d"),
                fs::Permissions::from_mode(0o755),
            );
            continue;
        }
        let err = stage(&rig, &offered).expect_err(why);
        // The rig removes its root when dropped, which needs `updater.d/` searchable again.
        let _ = fs::set_permissions(
            rig.release().join("updater.d"),
            fs::Permissions::from_mode(0o755),
        );
        assert_eq!(err.code(), "release-differs", "{why}");
        assert_eq!(
            err.to_string(),
            Refusal::Differs {
                path: rig
                    .release()
                    .join("updater.d/kbf.toml")
                    .display()
                    .to_string()
            }
            .to_string(),
            "{why}"
        );
    }

    let rig = Rig::new("declaration-plain-empty-dir");
    stage(&rig, &tarball()).expect("first");
    fs::create_dir(rig.release().join("updater.d")).expect("plant");
    assert!(stage(&rig, &tarball()).expect("same").already_installed);
}

/// Stage one tarball per `(component, bytes)` as one order, the partial and the release
/// owned by `owner`, with the honest programs.
fn stage_parts(rig: &Rig, owner: Owner, parts: &[(&str, Vec<u8>)]) -> Result<Staged, Refusal> {
    let partial = Partial::create(&rig.host, &commit(), owner).expect("partial");
    let (mut locators, mut files) = (Vec::new(), Vec::new());
    for (name, bytes) in parts {
        let path = partial.dl().join(format!("{name}.tar.gz"));
        fs::write(&path, bytes).expect("seed");
        locators.push(Locator {
            name: (*name).to_owned(),
            sha256: sha(bytes),
            size: bytes.len() as u64,
            attestation: "{\"mediaType\":\"bundle\"}\n".to_owned(),
        });
        files.push(Received {
            name: (*name).to_owned(),
            tarball: path,
            bytes: bytes.len() as u64,
            sha256: sha(bytes),
        });
    }
    let commit = commit();
    let pipeline = Pipeline {
        host: &rig.host,
        settings: &rig.settings,
        policy: rig.policy,
        trust: &rig.trust,
        runner: &honest(),
        leases: &rig.leases,
        owner,
    };
    let order = Order {
        kind: OrderKind::Activate,
        commit: &commit,
        components: &locators,
        expected: Some(&facts()),
    };
    pipeline.verify(&order, Fetched { partial, files })
}

/// The tarball of the component `name` (an x86-64 ELF), with `declaration` as its
/// `updater.d/<name>.toml` when there is one.
fn component(name: &str, declaration: Option<&[u8]>) -> Vec<u8> {
    let binary = elf(0x3e, name);
    let member = archive::declaration_member(name);
    match declaration {
        Some(d) => tar_gz(&[file(name, &binary), file(&member, d)]),
        None => tar_gz(&[file(name, &binary)]),
    }
}

/// Catches the declarations of an order of several components colliding or getting lost in
/// the one `updater.d/`: each component's own declaration is installed under its name; a
/// component without one adds none; the release staged again is the same, and one whose
/// second declaration differs is not; and a later component refused after an earlier one's
/// declaration was unpacked leaves nothing installed and no scratch.
#[test]
fn each_component_of_an_order_brings_its_own_declaration() {
    let rig = Rig::new("declaration-pair");
    let both = [
        ("kbf", component("kbf", Some(b"name = \"kbf\"\n"))),
        (
            "kbf-daemon",
            component("kbf-daemon", Some(b"name = \"kbf-daemon\"\n")),
        ),
    ];
    let staged = stage_parts(&rig, owner(), &both).expect("staged");
    assert_eq!(staged.components, ["kbf", "kbf-daemon"]);
    let release = rig.release();
    assert_eq!(
        listing(&release),
        [
            "VERIFIED",
            "kbf",
            "kbf-daemon",
            "updater.d",
            "updater.d/kbf-daemon.toml",
            "updater.d/kbf.toml"
        ]
    );
    assert_eq!(
        fs::read(release.join("updater.d/kbf-daemon.toml")).expect("daemon's"),
        b"name = \"kbf-daemon\"\n"
    );
    assert!(
        stage_parts(&rig, owner(), &both)
            .expect("again")
            .already_installed
    );
    let other = [
        ("kbf", component("kbf", Some(b"name = \"kbf\"\n"))),
        (
            "kbf-daemon",
            component("kbf-daemon", Some(b"name = \"other\"\n")),
        ),
    ];
    let err = stage_parts(&rig, owner(), &other).expect_err("the second differs");
    assert_eq!(
        err.to_string(),
        Refusal::Differs {
            path: release
                .join("updater.d/kbf-daemon.toml")
                .display()
                .to_string()
        }
        .to_string()
    );

    let rig = Rig::new("declaration-pair-mixed");
    let mixed = [
        ("kbf", component("kbf", None)),
        ("kbf-daemon", component("kbf-daemon", Some(b"d"))),
    ];
    stage_parts(&rig, owner(), &mixed).expect("staged");
    assert_eq!(
        listing(&rig.release()),
        [
            "VERIFIED",
            "kbf",
            "kbf-daemon",
            "updater.d",
            "updater.d/kbf-daemon.toml"
        ]
    );

    let rig = Rig::new("declaration-pair-later-refused");
    let later = [
        ("kbf", component("kbf", Some(b"name = \"kbf\"\n"))),
        (
            "kbf-daemon",
            tar_gz(&[file("kbf-daemon", &elf(0x3e, "d")), file("../evil", b"x")]),
        ),
    ];
    let err = stage_parts(&rig, owner(), &later).expect_err("the second is unsafe");
    assert_eq!(err.code(), "archive-unsafe-path");
    rig.assert_nothing_installed();
}

/// Catches the declaration and its directory left to the owner of the scratch rather than
/// the release's: with the release owned by another group of this process's user (root
/// on a host, which a test cannot chown to), `updater.d/` and the declaration are that
/// group's, as the binary and the marker are.
#[test]
fn the_declaration_and_its_directory_are_the_releases_owner() {
    let me = owner();
    let groups = rustix::process::getgroups().expect("groups");
    let Some(other) = groups.iter().map(|g| g.as_raw()).find(|g| *g != me.gid) else {
        eprintln!(
            "skipped: this process's user is in no group but its own, so no other owner can be given"
        );
        return;
    };
    let rig = Rig::new("declaration-owner");
    let owner = Owner {
        uid: me.uid,
        gid: other,
    };
    stage_parts(&rig, owner, &[("kbf", with_declaration(DECLARATION))]).expect("staged");
    let release = rig.release();
    for entry in ["kbf", "VERIFIED", "updater.d", "updater.d/kbf.toml"] {
        let meta = fs::symlink_metadata(release.join(entry)).expect(entry);
        assert_eq!((meta.uid(), meta.gid()), (me.uid, other), "{entry}");
    }
}
