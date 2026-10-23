use super::*;

/// Asserts that a `kbf` package directory that cannot be made, and a `kbf` file that cannot
/// be written, are reported, after the `kbf-installer` files are written.
#[test]
fn write_config_reports_the_kbf_directory_and_files_it_cannot_make() {
    let t = Tree::new("write-kbf-fails");
    let out = t.dir.join("out");
    std::fs::create_dir_all(&out).expect("out");
    std::fs::write(out.join(KBF_DIR), "x").expect("a file where the directory goes");
    let err = write_config(&t.inputs(), &out).expect_err("a file is no directory");
    assert!(err.to_string().contains("create"), "{err}");
    assert!(
        out.join("nfpm.yaml").is_file(),
        "the installer's came first"
    );
    std::fs::remove_file(out.join(KBF_DIR)).expect("rm");
    std::fs::create_dir_all(out.join(KBF_DIR).join("listing.txt")).expect("dir");
    let err = write_config(&t.inputs(), &out).expect_err("a directory is no file");
    assert!(err.to_string().contains("write"), "{err}");
}

/// Asserts that `write_config` refuses a unit directory that lacks the unit of a mode, naming
/// it, and writes nothing, not even the `kbf-installer` files.
#[test]
fn write_config_refuses_a_missing_mode_unit_and_writes_nothing() {
    let t = Tree::new("write-no-mode-unit");
    std::fs::remove_file(t.dir.join("units/kbf-daemon.service")).expect("rm");
    let out = t.dir.join("out");
    let err = write_config(&t.inputs(), &out).expect_err("a unit is missing");
    let text = err.to_string();
    assert!(
        text.contains("unit file kbf-daemon.service is missing"),
        "{text}"
    );
    assert!(!out.exists());
}

/// Asserts that the `kbf-installer` package has neither `/usr/bin/kbf` nor a unit of a
/// mode, though the unit directory holds all of them, and that the `kbf` package has exactly
/// the binary and those units, at their modes.
#[test]
fn the_installer_plan_lacks_the_kbf_files_and_the_kbf_plan_is_exactly_them() {
    let t = Tree::new("split");
    let installer: Vec<String> = plan(&t.inputs())
        .expect("installer plan")
        .into_iter()
        .map(|e| e.dst)
        .collect();
    assert!(!installer.iter().any(|d| d == KBF_PATH), "{installer:?}");
    for unit in KBF_UNITS {
        let dst = format!("{UNIT_DIR}/{unit}");
        assert!(!installer.contains(&dst), "{unit} in {installer:?}");
    }
    let kbf: Vec<(String, u32, bool)> = kbf_plan(&t.inputs())
        .expect("kbf plan")
        .into_iter()
        .map(|e| (e.dst, e.mode, e.sha256.is_some()))
        .collect();
    let mut want = vec![(KBF_PATH.to_owned(), 0o755, true)];
    for unit in KBF_UNITS {
        want.push((format!("{UNIT_DIR}/{unit}"), 0o644, true));
    }
    assert_eq!(kbf, want);
}

/// Asserts that the `kbf` plan refuses what the package does not ship, each with its reason:
/// a missing binary, a missing mode unit, a unit directory that cannot be read, a version
/// and a platform no package takes; and that it names every problem at once.
#[test]
fn the_kbf_plan_refuses_a_missing_binary_or_unit_and_collects_every_problem() {
    type Change = fn(&Tree) -> Inputs<'_>;
    let cases: [(&str, Change, &str); 5] = [
        (
            "a missing kbf",
            |t| {
                std::fs::remove_file(t.dir.join("kbf")).expect("rm");
                t.inputs()
            },
            "/kbf: No such file or directory",
        ),
        (
            "a missing mode unit",
            |t| {
                std::fs::remove_file(t.dir.join("units/kbf-daemon.service")).expect("rm");
                t.inputs()
            },
            "unit file kbf-daemon.service is missing",
        ),
        (
            "a unit directory that is not there",
            |t| {
                let mut i = t.inputs();
                i.units = Path::new("/nonexistent/units");
                i
            },
            "/nonexistent/units cannot be read as a directory",
        ),
        (
            "a bad version",
            |t| {
                let mut i = t.inputs();
                i.version = "v1";
                i
            },
            "version \"v1\" is not digits first",
        ),
        (
            "a macOS platform",
            |t| {
                let mut i = t.inputs();
                i.platform = Platform::DarwinArm64;
                i
            },
            "darwin-arm64 is not a Linux platform",
        ),
    ];
    for (n, (what, change, says)) in cases.iter().enumerate() {
        let t = Tree::new(&format!("kbf-refuse-{n}"));
        let text = refused(kbf_plan(&change(&t)));
        assert!(text.contains(says), "{what}: {text}");
    }
    let t = Tree::new("kbf-collects");
    std::fs::remove_file(t.dir.join("units/kbf-daemon.service")).expect("rm");
    std::fs::remove_file(t.dir.join("units/kbf-storage.service")).expect("rm");
    let text = refused(kbf_plan(&t.inputs()));
    assert_eq!(text.lines().count(), 2, "{text}");
}

/// Asserts that the `kbf` configuration is named `kbf`, depends (deb) and requires (rpm) on
/// `kbf-installer` of the version it is given or later, makes the deb, and only the deb,
/// replace `kbf-installer`, suggests nothing, runs no script, and lists its files as root's;
/// and that the `kbf-installer` configuration names nothing of the `kbf` package and replaces
/// nothing.
#[test]
fn the_kbf_config_depends_on_the_installer_of_its_own_version() {
    let t = Tree::new("kbf-config");
    let entries = kbf_plan(&t.inputs()).expect("kbf plan");
    let text = config(Package::Kbf, &entries, Platform::LinuxArm64, "1.2.3+ci");
    assert!(text.contains("name: \"kbf\"\n"), "{text}");
    assert!(text.contains("arch: \"arm64\"\n"), "{text}");
    assert!(text.contains("version: \"1.2.3+ci\"\n"), "{text}");
    assert!(
        text.contains("depends:\n  - \"kbf-installer (>= 1.2.3+ci)\"\noverrides:\n"),
        "{text}"
    );
    let rpm = "overrides:\n  rpm:\n    depends:\n      - \"kbf-installer >= 1.2.3+ci\"\n";
    let deb = "  deb:\n    replaces:\n      - \"kbf-installer\"\ndeb:\n";
    assert!(text.contains(&format!("{rpm}{deb}")), "{text}");
    assert_eq!(text.matches("replaces").count(), 1, "{text}");
    assert_eq!(KBF_DEB_REPLACES, ["kbf-installer"]);
    for banned in [
        "suggests",
        "recommends",
        "scripts",
        "postinstall",
        "preinstall",
    ] {
        assert!(!text.contains(banned), "{banned} in {text}");
    }
    assert_eq!(
        text.matches("owner: root, group: root").count(),
        entries.len()
    );
    assert!(
        text.contains("  - dst: \"/usr/bin/kbf\"\n    src: "),
        "{text}"
    );
    let installer = config(Package::Installer, &[], Platform::LinuxArm64, "1.2.3+ci");
    assert!(!installer.contains("kbf (>="), "{installer}");
    assert!(!installer.contains("replaces"), "{installer}");
    assert_eq!(kbf_depends_deb("9"), "kbf-installer (>= 9)");
    assert_eq!(kbf_depends_rpm("9"), "kbf-installer >= 9");
}

/// Asserts that the fixture unit directory holds the unit of every mode, so that the `kbf`
/// package of the packaging test is planned from the fixtures with the real files.
#[test]
fn the_fixtures_make_a_kbf_plan_with_the_real_files() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging/linux");
    let binary = root.join("testdata/units/kbf-node.service");
    let plan = kbf_plan(&Inputs {
        binary: &binary,
        kbf: &binary,
        units: &root.join("testdata/units"),
        trust: &root.join("testdata/trust"),
        sysusers: &root.join("sysusers.d/kbf-installer.conf"),
        tmpfiles: &root.join("tmpfiles.d/kbf-installer.conf"),
        needrestart: &root.join("needrestart/kbf.conf"),
        platform: Platform::LinuxX86_64,
        version: "0.0.0+ci",
    })
    .expect("the fixtures are a kbf plan");
    assert_eq!(plan.len(), 1 + KBF_UNITS.len());
}
