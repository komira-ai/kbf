//! The paths the packaged units and the launchd job files run, read against the layout
//! (`kbf-setup`'s `layout.rs`) that the installer makes them true with.

use std::fs;
use std::path::{Path, PathBuf};

use crate::layout::{Layout, Platform};
use kbf_host::path::Root;

fn repo_file(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(rel)
}

/// Every `Exec*=` command's program in `unit`, with the `-@+!:` prefixes systemd allows
/// before it taken off, as `(key, program)`.
fn programs(unit: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for line in unit.lines() {
        let line = line.trim();
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if !key.starts_with("Exec") || line.starts_with('#') {
            continue;
        }
        let program = value
            .trim_start_matches(['-', '@', '+', '!', ':'])
            .split_whitespace()
            .next()
            .unwrap_or("");
        out.push((key.to_owned(), program.to_owned()));
    }
    out
}

/// Asserts that every `kbf` a Linux unit runs is `/usr/bin/kbf`, the file the kbf package
/// installs (a path under the installer's tree, such as the `releases/current` link a host
/// of the earlier layout has, is a file the package manager does not replace), and that no
/// unit program lies under the installer's tree. The tree is the layout's own
/// `install_root()`, so a change of the layout moves this with it. The count guards against
/// the scan reading nothing (a moved directory) and passing.
#[test]
fn every_linux_unit_runs_the_kbf_the_package_installs() {
    let layout = Layout::linux(Root::host());
    let tree = layout.install_root().to_string();
    let dir = repo_file("packaging/linux/units");
    let mut checked = 0;
    let mut names: Vec<_> = fs::read_dir(&dir)
        .expect("the units directory")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().is_some_and(|e| e == "service"))
        .collect();
    names.sort();
    for path in names {
        let name = path
            .file_name()
            .expect("name")
            .to_string_lossy()
            .into_owned();
        let text = fs::read_to_string(&path).expect("unit");
        for (key, program) in programs(&text) {
            assert!(
                !program.starts_with(&format!("{tree}/")),
                "{name} {key} runs {program}, under the installer's tree"
            );
            if program.ends_with("/kbf") {
                assert_eq!(program, "/usr/bin/kbf", "{name} {key}");
                checked += 1;
            }
        }
    }
    // kbf-daemon, kbf-storage, kbf-storage-and-daemon, kbf-all-in-one: a boot check, a
    // check and a start each; kbf-frontend: a boot check, a check and a start.
    assert!(
        checked >= 15,
        "only {checked} kbf commands found in {dir:?}"
    );
}

/// Catches the installer's own unit running a program from the release tree (a
/// release can be pruned under it) and a unit program that is relative or missing.
#[test]
fn the_installer_unit_runs_the_installed_binary() {
    let text =
        fs::read_to_string(repo_file("packaging/linux/units/kbf-installer.service")).expect("unit");
    let progs = programs(&text);
    // The unit has no ExecStartPre (`check` is not built), so ExecStart is the one program.
    assert!(
        progs.iter().any(|(key, _)| *key == "ExecStart"),
        "{progs:?}"
    );
    for (key, program) in progs {
        assert_eq!(program, "/usr/bin/kbf-installer", "{key}");
    }
}

/// Catches a launchd job file for the installer whose program is outside the macOS
/// installer tree, and the daemon job's `kbf` (kbf-mac-provision's `KBF_BIN`) not being
/// the macOS layout's `releases/current/kbf`.
#[test]
fn the_macos_job_files_run_programs_under_the_macos_layout() {
    let layout = Layout::new(Root::host(), Platform::MacOs);
    let tree = layout.install_root().to_string();
    let plist = fs::read_to_string(repo_file("packaging/macos/com.komira.kbf-installer.plist"))
        .expect("plist");
    let program = plist
        .split("<string>")
        .map(|s| s.split("</string>").next().unwrap_or(""))
        .find(|s| s.starts_with('/'))
        .expect("a program");
    assert!(program.starts_with(&format!("{tree}/")), "{program}");

    let source = fs::read_to_string(repo_file("crates/kbf-mac-provision/src/launchd.rs"))
        .expect("launchd.rs");
    let line = source
        .lines()
        .find(|l| l.starts_with("pub const KBF_BIN"))
        .expect("KBF_BIN");
    let bin = line.split('"').nth(1).expect("a literal");
    assert_eq!(bin, format!("{}/kbf", layout.current()));
}

/// Catches the release store's components for a platform not naming the binary that platform
/// runs: the Mac's job runs `releases/current/kbf` (kbf-mac-provision's `KBF_BIN`), so a Mac
/// release must hold `kbf` and the store's `ReleasePlatform::components()` must name exactly
/// it; a Linux host's `kbf` is the layout's `RELEASE_BINARY` too. (The Mac's was `kbf-daemon`:
/// a roll fetched a file the job never runs, and the order check refused it.)
#[test]
fn the_components_of_each_platform_are_the_binary_its_host_runs() {
    let layout = Layout::new(Root::host(), Platform::MacOs);
    let source = fs::read_to_string(repo_file("crates/kbf-mac-provision/src/launchd.rs"))
        .expect("launchd.rs");
    let line = source
        .lines()
        .find(|l| l.starts_with("pub const KBF_BIN"))
        .expect("KBF_BIN");
    let bin = line.split('"').nth(1).expect("a literal");
    let run = bin
        .strip_prefix(&format!("{}/", layout.current()))
        .expect("under current");
    assert_eq!(run, crate::layout::RELEASE_BINARY);
    for platform in kbf_types::ReleasePlatform::ALL {
        assert_eq!(
            platform.components(),
            [run],
            "{platform}: a release holds the one binary the host's job runs"
        );
    }
}
