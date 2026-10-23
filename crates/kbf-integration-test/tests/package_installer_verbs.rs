//! What the shipped unit files and the macOS property list ask of `kbf-installer`, against
//! the binary's own command line and layout, so a verb that is renamed, not built, or never
//! existed (`serve`), and a release path the installer does not create, fail here and not on
//! a host at start.
//!
//! Catches:
//! - an `ExecStart=`/`ExecStartPre=` of any packaged unit, or a `ProgramArguments` of the
//!   property list, whose `kbf-installer` command line the real parser (`kbf_installer::cli`)
//!   refuses, or whose verb is one the binary answers "not implemented";
//! - a runtime unit that execs `kbf` anywhere but `/usr/bin/kbf`, the file the kbf package
//!   installs (a Linux host has no release tree for it);
//! - a property list that is not gated on the macOS configuration directory of the layout, or
//!   that restarts a launchd label `setup` does not bootstrap.
//!
//! `DEFECTS` plants one defect per rule in a copy of the real file; each must be reported.

use std::path::{Path, PathBuf};

use clap::Parser;
use kbf_host::path::Root;
use kbf_installer::cli::Cli;
use kbf_installer::layout::{Layout, Platform};
use kbf_installer::setup::plan::MAC_DAEMON_LABEL;
use kbf_unitfile::UnitFile;

const INSTALLER: &str = "/usr/bin/kbf-installer";
const PLIST: &str = "com.komira.kbf-installer.plist";

fn packaging() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packaging")
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn services() -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(packaging().join("linux/units"))
        .expect("units dir")
        .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".service"))
        .collect();
    names.sort();
    names
}

fn layout(platform: Platform) -> Layout {
    Layout::new(Root::new("/").expect("absolute"), platform)
}

/// Every command a unit runs, without systemd's `+` prefix.
fn commands(text: &str) -> Vec<String> {
    let unit = UnitFile::parse(text).expect("a unit");
    unit.directives()
        .iter()
        .filter(|d| d.section == "Service" && d.key.starts_with("Exec"))
        .map(|d| d.value.trim().trim_start_matches('+').to_owned())
        .collect()
}

/// What is wrong with one `kbf-installer` command line (the words after the program).
fn verb_problems(words: &[&str]) -> Vec<String> {
    let mut argv = vec!["kbf-installer"];
    argv.extend_from_slice(words);
    match Cli::try_parse_from(&argv) {
        Err(e) => vec![format!(
            "verb: {words:?} is not a kbf-installer command: {e}"
        )],
        Ok(cli) => cli.verb.built_by().map_or_else(Vec::new, |phase| {
            vec![format!(
                "verb: {words:?}: `{}` is not built ({phase})",
                cli.verb.name()
            )]
        }),
    }
}

/// The problems of the installer commands in a unit file, with how many there were.
fn unit_problems(name: &str, text: &str) -> (usize, Vec<String>) {
    let (mut n, mut found) = (0, Vec::new());
    for cmd in commands(text) {
        let words: Vec<&str> = cmd.split_whitespace().collect();
        if words.first() == Some(&INSTALLER) {
            n += 1;
            found.extend(
                verb_problems(&words[1..])
                    .into_iter()
                    .map(|p| format!("{name}: {p}")),
            );
        }
    }
    (n, found)
}

/// The strings of the plist's `ProgramArguments`, program first.
fn program_arguments(plist: &str) -> Vec<String> {
    let after = plist
        .split("<key>ProgramArguments</key>")
        .nth(1)
        .expect("ProgramArguments");
    let array = after
        .split("</array>")
        .next()
        .expect("an array")
        .split("<array>")
        .nth(1)
        .expect("an array");
    array
        .split("<string>")
        .skip(1)
        .map(|s| s.split("</string>").next().expect("closed").to_owned())
        .collect()
}

fn plist_problems(plist: &str) -> Vec<String> {
    let args = program_arguments(plist);
    let mut found = Vec::new();
    let words: Vec<&str> = args[1..].iter().map(String::as_str).collect();
    found.extend(verb_problems(&words));
    // Catches a label `setup` does not bootstrap: a swap would kickstart nothing.
    if words != ["run", "--launchd-label", MAC_DAEMON_LABEL] {
        found.push(format!(
            "label: ProgramArguments {words:?} is not run for {MAC_DAEMON_LABEL}"
        ));
    }
    // Catches a gate the layout does not read: the job would wait for a file nothing writes.
    let gate = format!("<key>{}</key>", layout(Platform::MacOs).installer_toml());
    if !plist.contains(&gate) {
        found.push(format!("gate: KeepAlive is not gated on {gate}"));
    }
    found
}

/// The `kbf` a runtime unit may exec: the kbf package's own file.
const PACKAGED_KBF: &str = "/usr/bin/kbf";

fn exec_problems(name: &str, text: &str) -> Vec<String> {
    commands(text)
        .iter()
        .filter_map(|c| c.split_whitespace().next())
        .filter(|p| p.ends_with("/kbf") && !p.contains("%i"))
        .filter(|p| *p != PACKAGED_KBF)
        .map(|p| format!("exec: {name}: execs {p}, the kbf package installs {PACKAGED_KBF}"))
        .collect()
}

#[test]
fn every_installer_command_of_a_unit_is_a_verb_that_exists() {
    let mut seen = std::collections::BTreeMap::<String, usize>::new();
    for name in services() {
        let text = read(&packaging().join("linux/units").join(&name));
        let (n, problems) = unit_problems(&name, &text);
        assert!(problems.is_empty(), "{problems:?}");
        seen.insert(name, n);
    }
    // Not vacuous: the installer's own unit runs one command. A mode unit runs none: its
    // `boot-check` is `kbf`'s own.
    assert_eq!(seen["kbf-installer.service"], 1, "{seen:?}");
    for unit in [
        "kbf-daemon.service",
        "kbf-storage.service",
        "kbf-all-in-one.service",
    ] {
        assert_eq!(seen[unit], 0, "{unit}");
    }
}

#[test]
fn the_property_list_runs_a_verb_that_exists_for_a_label_setup_bootstraps() {
    let plist = read(&packaging().join("macos").join(PLIST));
    assert_eq!(plist_problems(&plist), Vec::<String>::new());
}

/// Asserts that every `kbf` a runtime unit execs is `/usr/bin/kbf`, the kbf package's file,
/// and that the mode units run it three times each, a boot check, a check and a start.
#[test]
fn the_runtime_units_exec_the_packaged_kbf() {
    let mut checked = 0;
    for name in services() {
        let text = read(&packaging().join("linux/units").join(&name));
        let problems = exec_problems(&name, &text);
        assert!(problems.is_empty(), "{problems:?}");
        checked += commands(&text)
            .iter()
            .filter(|c| c.split_whitespace().next() == Some(PACKAGED_KBF))
            .count();
    }
    // Five mode units, a boot check, a check and a start each: the four node modes and the plain
    // frontend (kbf-frontend.service); the frontend template runs its slot.
    assert_eq!(checked, 15);
}

/// `(file, from, to, rule)`: one defect per rule, planted in a copy of the real file.
const DEFECTS: &[(&str, &str, &str, &str)] = &[
    (
        "kbf-installer.service",
        "ExecStart=/usr/bin/kbf-installer run",
        "ExecStart=/usr/bin/kbf-installer serve",
        "verb",
    ),
    (
        "kbf-installer.service",
        "ExecStart=/usr/bin/kbf-installer run",
        "ExecStartPre=/usr/bin/kbf-installer check\nExecStart=/usr/bin/kbf-installer run",
        "verb",
    ),
    (
        "kbf-installer.service",
        "ExecStart=/usr/bin/kbf-installer run",
        "ExecStart=/usr/bin/kbf-installer run --apply-now",
        "verb",
    ),
    (
        "kbf-daemon.service",
        "ExecStart=/usr/bin/kbf",
        "ExecStart=/var/lib/kbf-installer/releases/current/kbf",
        "exec",
    ),
    (
        "kbf-storage-and-daemon.service",
        "ExecStartPre=/usr/bin/kbf",
        "ExecStartPre=/var/lib/kbf-installer/releases/previous/kbf",
        "exec",
    ),
    (
        PLIST,
        "<string>run</string>",
        "<string>serve</string>",
        "verb",
    ),
    (
        PLIST,
        "<string>run</string>",
        "<string>check</string>",
        "verb",
    ),
    (
        PLIST,
        "com.komira.kbf-daemon</string>",
        "com.komira.other</string>",
        "label",
    ),
    (
        PLIST,
        "<key>/usr/local/etc/kbf/installer.toml</key>",
        "<key>/etc/kbf/installer.toml</key>",
        "gate",
    ),
];

#[test]
fn each_planted_defect_is_reported() {
    for (file, from, to, rule) in DEFECTS {
        let (is_plist, path) = if *file == PLIST {
            (true, packaging().join("macos").join(file))
        } else {
            (false, packaging().join("linux/units").join(file))
        };
        let text = read(&path);
        assert!(text.contains(from), "{file}: `{from}` is not in the file");
        let broken = text.replacen(from, to, 1);
        let found = match (is_plist, *rule) {
            (true, _) => plist_problems(&broken),
            (false, "exec") => exec_problems(file, &broken),
            (false, _) => unit_problems(file, &broken).1,
        };
        assert!(
            found.iter().any(|f| f.contains(&format!("{rule}: "))),
            "{file}: `{from}` -> `{to}` was not reported by {rule}: {found:?}"
        );
    }
    // Every rule has a defect.
    for rule in ["verb", "exec", "label", "gate"] {
        assert!(DEFECTS.iter().any(|m| m.3 == rule), "{rule}");
    }
}
