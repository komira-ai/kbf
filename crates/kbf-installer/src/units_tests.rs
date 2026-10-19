//! The paths the packaged units and the launchd job files run, read against the layout
//! (`layout.rs`) that the installer makes them true with.

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

/// Catches a unit whose `kbf` is not the release the installer links as `current`
/// (`/var/lib/kbf-installer/current/kbf` is a path the installer never makes, so the
/// unit's start fails on every host), and a unit program under the installer's tree
/// that is neither a release nor a frontend slot. Reads the layout's own `current()`,
/// so a change of the layout moves this with it. The count guards against the scan
/// reading nothing (a moved directory) and passing.
#[test]
fn every_linux_unit_runs_the_kbf_the_layout_links_as_current() {
    let layout = Layout::linux(Root::host());
    let current_kbf = format!("{}/kbf", layout.current());
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
        let name = path.file_name().expect("name").to_string_lossy().into_owned();
        let text = fs::read_to_string(&path).expect("unit");
        for (key, program) in programs(&text) {
            let in_tree = program.starts_with(&format!("{tree}/"));
            let is_kbf = program.ends_with("/kbf");
            if name == "kbf-frontend@.service" && is_kbf {
                // Slots are not in the layout: the frontend keeps its own link per
                // instance under the installer's tree until the proxy replaces it.
                assert_eq!(program, format!("{tree}/slots/%i/kbf"), "{name} {key}");
                checked += 1;
            } else if is_kbf || in_tree {
                assert_eq!(program, current_kbf, "{name} {key}");
                checked += 1;
            }
        }
    }
    // kbf-daemon, kbf-storage, kbf-storage-and-daemon, kbf-all-in-one: a check and a
    // start each; kbf-frontend@: a check and a start.
    assert!(checked >= 10, "only {checked} kbf commands found in {dir:?}");
}

/// Catches the installer's own unit running a program from the release tree (a
/// release can be pruned under it) and a unit program that is relative or missing.
#[test]
fn the_installer_unit_runs_the_installed_binary() {
    let text = fs::read_to_string(repo_file("packaging/linux/units/kbf-installer.service"))
        .expect("unit");
    let progs = programs(&text);
    assert!(progs.len() >= 2, "{progs:?}");
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
