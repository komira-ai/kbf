//! A release install, a switch and a collection never touch the files that are not a
//! release: the helper (`kbf-host-install`) and the agent (`kbf-updater`) live at fixed
//! paths, and a host that keeps several releases must not move them with the releases.
//! Children of the release tests, which hold the scratch hosts.

use super::*;
use crate::activate::{self, Activated, Arm, Order};

/// The fixed files, on Linux and on a Mac: the helper (`SELF_EXE` of `kbf-host-install`
/// and of its macOS layout) and the agent (`kbf-updater`).
const FIXED: [&str; 4] = [
    "/usr/libexec/kbf/kbf-host-install",
    "/usr/local/libexec/kbf/kbf-host-install",
    "/usr/libexec/kbf/kbf-updater",
    "/usr/local/libexec/kbf/kbf-updater",
];

/// Any path under the directories that hold the fixed files.
const FIXED_DIRS: [&str; 2] = ["/usr/libexec/kbf", "/usr/local/libexec/kbf"];

fn bytes_of(path: &str) -> Vec<u8> {
    format!("the bytes of {path}, which no release may change").into_bytes()
}

/// Make the fixed files under the scratch root, each with bytes of its own.
fn lay_down_the_fixed_files(exec: &ScratchHost) {
    for path in FIXED {
        let at = exec.at(path);
        fs::create_dir_all(at.parent().expect("a directory")).expect("dirs");
        fs::write(&at, bytes_of(path)).expect("file");
        fs::set_permissions(&at, fs::Permissions::from_mode(0o755)).expect("mode");
    }
}

/// Every fixed file is as it was laid down: there, with its bytes and mode.
fn assert_the_fixed_files_are_untouched(exec: &ScratchHost, what: &str) {
    for path in FIXED {
        let at = exec.at(path);
        assert_eq!(
            fs::read(&at).unwrap_or_else(|e| panic!("{what}: {path}: {e}")),
            bytes_of(path),
            "{what}: {path}"
        );
        assert_eq!(mode_of(exec, path), 0o755, "{what}: {path}");
    }
    for dir in FIXED_DIRS {
        let names: Vec<_> = fs::read_dir(exec.at(dir))
            .expect("list")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names.len(),
            2,
            "{what}: {dir} gained or lost a file: {names:?}"
        );
    }
}

/// No step the host applied names a fixed path, or anything under their directories.
fn assert_no_step_names_a_fixed_path(exec: &ScratchHost, what: &str) {
    assert!(!exec.log.is_empty(), "{what}: the steps were not recorded");
    for step in &exec.log {
        let text = format!("{step:?}");
        for dir in FIXED_DIRS {
            assert!(!text.contains(dir), "{what}: {step:?} names {dir}");
        }
        for path in FIXED {
            assert!(!text.contains(path), "{what}: {step:?} names {path}");
        }
        assert!(!text.contains("kbf-updater"), "{what}: {step:?}");
        assert!(!text.contains("kbf-host-install"), "{what}: {step:?}");
    }
}

fn order(commit: &str, id: &str) -> Order {
    Order {
        commit: c(commit),
        order_id: id.to_owned(),
    }
}

/// Catches a release operation that writes, replaces, links or removes the helper or the
/// agent: a run of installs, a stage and an activation with its confirm, a reverted
/// activation, a rollback and a collection that removes releases, on a Linux host and on a
/// Mac, leaves no step that names a path under `libexec/kbf` and the four fixed files
/// byte for byte and mode for mode as they were.
///
/// Catches: an install that copies the release's binary to the fixed helper path; a switch
/// that links the agent; a `gc` that sweeps `libexec`.
#[test]
fn installing_switching_and_collecting_releases_never_touch_the_helper_or_the_agent() {
    for (mut exec, root) in [host("fixed"), mac("fixed")] {
        let on = format!("{:?}", root.platform());
        lay_down_the_fixed_files(&exec);
        let src = |name: &str| source(&format!("fixed-{on}-{name}"), &format!("kbf {name}"), None);
        let id = |n: u64| format!("{n:040x}");

        // Installs: each moves the links.
        for n in 1..=3 {
            let from = src(&id(n));
            install_on(&mut exec, &root, &id(n), &from, Mode::Apply).expect("install");
            fs::File::open(root.join(&root.release_dir(&c(&id(n)))))
                .expect("open")
                .set_modified(UNIX_EPOCH + Duration::from_secs(1_000 + n))
                .expect("mtime");
        }
        assert_the_fixed_files_are_untouched(&exec, "after the installs");

        // A staged release swapped in on trial, then confirmed.
        for (n, order_id) in [(4, "o4"), (5, "o5")] {
            let install = Install {
                commit: c(&id(n)),
                from: src(&id(n)),
            };
            stage(
                &mut exec,
                &root,
                me(),
                &install,
                Mode::Apply,
                &mut Vec::new(),
            )
            .expect("stage");
            let swapped = activate::activate(
                &mut exec,
                &root,
                &order(&id(n), order_id),
                Arm::FromFirstStart { starts: 3 },
                1_000,
                &mut Vec::new(),
            )
            .expect("activate");
            assert_eq!(swapped, Activated::Swapped);
            if n == 4 {
                // This one goes back on trial.
                activate::revert(&mut exec, &root, &mut Vec::new()).expect("revert");
                assert_the_fixed_files_are_untouched(&exec, "after the revert");
                let install = Install {
                    commit: c(&id(n)),
                    from: src(&id(n)),
                };
                stage(
                    &mut exec,
                    &root,
                    me(),
                    &install,
                    Mode::Apply,
                    &mut Vec::new(),
                )
                .expect("stage again");
                activate::activate(
                    &mut exec,
                    &root,
                    &order(&id(n), "o4-again"),
                    Arm::FromFirstStart { starts: 3 },
                    1_000,
                    &mut Vec::new(),
                )
                .expect("activate again");
                activate::confirm(&root, "o4-again", &c(&id(n))).expect("confirm");
            } else {
                activate::confirm(&root, order_id, &c(&id(n))).expect("confirm");
            }
        }
        assert_the_fixed_files_are_untouched(&exec, "after the swaps");

        // Back one release, then collect what the floor lets go.
        rollback_on(&mut exec, &root, Mode::Apply).expect("rollback");
        assert_eq!(state_of(&root).current, Some(c(&id(4))));
        let out = gc_on(&mut exec, &root, 2).expect("gc");
        assert!(out.contains("removing: "), "{out}");
        assert!(
            !releases_dir_has(&root, &id(1)),
            "{out}: something was collected"
        );
        assert_the_fixed_files_are_untouched(&exec, "after the collection");

        assert_no_step_names_a_fixed_path(&exec, &on);
    }
}

fn releases_dir_has(root: &Layout, commit: &str) -> bool {
    root.join(&root.releases()).join(commit).exists()
}
