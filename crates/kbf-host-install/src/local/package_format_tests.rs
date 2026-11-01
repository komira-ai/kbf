//! The package manager's commands and the readers of what they print, as plain data.

use std::path::Path;

use kbf_installer::binary::Arch;

use super::*;
use crate::mac::fetch::AssetKind;
use crate::runner::Exec;

fn printed(code: i32, stdout: &str) -> Exec {
    Exec {
        code: Some(code),
        signal: None,
        stdout: stdout.into(),
        stderr: b"complaint".to_vec(),
    }
}

const SHA: &str = "abcdef012345";

/// An rpm's Version and Release are put back into the deb's one version text, so both formats
/// are held to the version the update names.
#[test]
fn an_rpm_version_and_release_read_as_the_debs_version() {
    assert_eq!(
        deb_spelling("0.1.7", &format!("3.g{SHA}")).as_deref(),
        Some(format!("0.1.7+g{SHA}-3").as_str())
    );
    assert_eq!(
        deb_spelling("0.1.7~feature", &format!("12.g{SHA}")).as_deref(),
        Some(format!("0.1.7~feature+g{SHA}-12").as_str())
    );
    for release in [
        "3",
        "3.gabc",
        &format!("x.g{SHA}"),
        &format!(".g{SHA}"),
        "3.gABCDEF012345",
        &format!("3.g{SHA}0"),
    ] {
        assert_eq!(deb_spelling("0.1.7", release), None, "{release}");
    }
    assert_eq!(deb_spelling("", &format!("3.g{SHA}")), None);
}

#[test]
fn the_release_file_and_the_cpu_name_follow_the_format() {
    assert_eq!(Format::Deb.asset(), AssetKind::Deb);
    assert_eq!(Format::Rpm.asset(), AssetKind::Rpm);
    assert_eq!(Format::Deb.arch_name(Arch::X86_64), "amd64");
    assert_eq!(Format::Deb.arch_name(Arch::Aarch64), "arm64");
    assert_eq!(Format::Rpm.arch_name(Arch::X86_64), "x86_64");
    assert_eq!(Format::Rpm.arch_name(Arch::Aarch64), "aarch64");
}

/// The installed package is read from dpkg-query's status and version, or rpm's version and
/// release; a package the tool has no record of, or one removed with only its configuration
/// left, is not installed; any other failure is an error and not a guess.
#[test]
fn the_installed_version_is_read_from_the_package_database() {
    let deb = |code, out: &str| Format::Deb.read_installed(&printed(code, out));
    let rpm = |code, out: &str| Format::Rpm.read_installed(&printed(code, out));
    assert_eq!(
        deb(0, "ii  0.1.7+gabcdef012345-3\n").unwrap().as_deref(),
        Some("0.1.7+gabcdef012345-3")
    );
    assert_eq!(deb(0, "iF  0.1.7-1\n").unwrap().as_deref(), Some("0.1.7-1"));
    assert_eq!(deb(0, "rc  0.1.7-1\n").unwrap(), None);
    assert_eq!(deb(0, "pn  0.1.7-1\n").unwrap(), None);
    assert_eq!(deb(1, "").unwrap(), None);
    assert!(deb(2, "").unwrap_err().contains("complaint"));
    assert!(deb(0, "").is_err());
    assert!(deb(0, "ii\n").is_err());

    assert_eq!(
        rpm(0, "0.1.7 3.gabcdef012345\n").unwrap().as_deref(),
        Some("0.1.7+gabcdef012345-3")
    );
    assert_eq!(rpm(1, "package kbf is not installed\n").unwrap(), None);
    assert!(rpm(2, "").is_err());
    assert!(rpm(0, "0.1.7\n").is_err());
    assert!(rpm(0, "0.1.7 3\n").is_err());

    // Two installed versions (or architectures) are an error, never the first line.
    assert!(
        rpm(0, "0.1.7 3.gabcdef012345\n0.1.8 1.g123456789abc\n")
            .unwrap_err()
            .contains("more than one")
    );
    assert!(
        deb(0, "ii  0.1.7-1\nii  0.1.7-1\n")
            .unwrap_err()
            .contains("more than one")
    );
}

#[test]
fn a_package_files_fields_are_read_whole_and_only_once() {
    let deb = "Package: kbf\nVersion: 0.1.7+gabcdef012345-3\nArchitecture: amd64\n";
    let meta = Format::Deb.read_meta(deb).unwrap();
    assert_eq!(
        (
            meta.name.as_str(),
            meta.version.as_str(),
            meta.arch.as_str()
        ),
        ("kbf", "0.1.7+gabcdef012345-3", "amd64")
    );
    let rpm = "Package: kbf\nVersion: 0.1.7\nRelease: 3.gabcdef012345\nArchitecture: x86_64\n";
    let meta = Format::Rpm.read_meta(rpm).unwrap();
    assert_eq!(meta.version, "0.1.7+gabcdef012345-3");
    assert_eq!(meta.arch, "x86_64");

    for (format, text) in [
        (Format::Deb, "Package: kbf\nVersion: 1\n"),
        (
            Format::Deb,
            "Package: kbf\nPackage: kbf\nVersion: 1\nArchitecture: amd64\n",
        ),
        (
            Format::Deb,
            "Package: kbf\nVersion: \nArchitecture: amd64\n",
        ),
        (
            Format::Rpm,
            "Package: kbf\nVersion: 0.1.7\nArchitecture: x86_64\n",
        ),
        (
            Format::Rpm,
            "Package: kbf\nVersion: 0.1.7\nRelease: 3\nArchitecture: x86_64\n",
        ),
    ] {
        assert!(format.read_meta(text).is_err(), "{text:?}");
    }
}

/// The package manager is given the one file and no repository: `dpkg --install`, and
/// `rpm --upgrade --oldpackage`, so an older file is installed like a newer one.
#[test]
fn a_package_is_installed_from_its_file_alone_and_an_older_one_is_not_refused() {
    let file = Path::new("/var/lib/kbf-host-install/packages/kbf-x.deb");
    let deb = Format::Deb.install(file);
    assert_eq!(deb.program, DPKG);
    assert_eq!(
        deb.args,
        [
            "--force-confold",
            "--install",
            "--",
            "/var/lib/kbf-host-install/packages/kbf-x.deb"
        ]
    );
    let rpm = Format::Rpm.install(file);
    assert_eq!(rpm.program, RPM);
    assert!(rpm.args.contains(&"--oldpackage".to_owned()));
    assert_eq!(
        rpm.args.last().unwrap(),
        "/var/lib/kbf-host-install/packages/kbf-x.deb"
    );
    for cmd in [&deb, &rpm] {
        assert!(cmd.timeout.is_some());
        assert!(
            !cmd.args
                .iter()
                .any(|a| a.contains("http") || a == "--nodeps")
        );
    }
}

#[test]
fn the_commands_that_look_name_the_file_or_the_package() {
    let file = Path::new("/w/kbf.deb");
    let deb = Format::Deb.inspect(file);
    assert_eq!(deb.program, DPKG_DEB);
    assert_eq!(
        deb.args,
        [
            "--field",
            "/w/kbf.deb",
            "Package",
            "Version",
            "Architecture"
        ]
    );
    let rpm = Format::Rpm.inspect(file);
    assert_eq!(rpm.program, RPM);
    assert_eq!(rpm.args.last().unwrap(), "/w/kbf.deb");
    assert!(rpm.args.contains(&"--package".to_owned()));
    assert_eq!(Format::Deb.installed().program, DPKG_QUERY);
    assert!(Format::Deb.installed().args.contains(&"kbf".to_owned()));
    assert_eq!(Format::Rpm.installed().program, RPM);
    assert!(Format::Rpm.installed().args.contains(&"kbf".to_owned()));
}

const H1: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const H2: &str = "2222222222222222222222222222222222222222222222222222222222222222";

#[test]
fn the_sums_list_a_file_once_or_the_package_is_not_taken() {
    let sums = format!("{H1}  kbf-a-linux-x86_64.deb\n{H2}  other\n");
    assert_eq!(sums_entry(&sums, "kbf-a-linux-x86_64.deb").unwrap(), H1);
    assert_eq!(sums_entry(&sums, "other").unwrap(), H2);
    assert_eq!(sums_entry(&sums, "kbf-b.deb"), Err(SumsError::NotListed));
    assert_eq!(sums_entry("", "x"), Err(SumsError::NotListed));
    for bad in [
        format!("{H1}  a\n{H2}  a\n"),
        format!("{H1}  a"),
        format!("{H1} a\n"),
        format!("{}  a\n", &H1[..63]),
        format!("{}  a\n", "A".repeat(64)),
        format!("{H1}  \n"),
        format!("{H1}  dir/a\n"),
    ] {
        assert_eq!(sums_entry(&bad, "a"), Err(SumsError::Malformed), "{bad:?}");
    }
}
