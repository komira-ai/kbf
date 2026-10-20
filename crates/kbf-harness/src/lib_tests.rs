//! The helpers of the unit tests.

use super::*;

#[test]
fn only_the_directories_of_runs_that_are_over_are_swept() {
    // Catches: the scratch directories of every run of the tests piling up in the temporary
    // directory (thousands, after a day of runs), and a sweep that takes what is not its own:
    // the directory of a process that is running (a test binary alongside this one), a name
    // that is not `<name>-<pid>`, a file. Mutants: nothing removed; a running process's
    // directory removed; a name with no pid removed.
    let root = scratch("sweep").join("root");
    let _ = std::fs::remove_dir_all(&root);
    let make = |name: &str| {
        std::fs::create_dir_all(root.join(name).join("inner")).expect("dir");
        std::fs::write(root.join(name).join("inner/file"), "x").expect("file");
    };
    for name in [
        "old-test-111",
        "other-222",
        "mine-333",
        "no-pid-here",
        "pidless",
    ] {
        make(name);
    }
    std::fs::write(root.join("loose-111"), "a file").expect("file");
    sweep_dead(&root, &|pid| pid == 333);
    let left: std::collections::BTreeSet<String> = std::fs::read_dir(&root)
        .expect("root")
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    let want = ["loose-111", "mine-333", "no-pid-here", "pidless"];
    assert_eq!(left, want.iter().map(|s| (*s).to_owned()).collect());
    // A root that is not there is nothing to sweep.
    sweep_dead(&root.join("absent"), &|_| false);
}

#[test]
fn a_process_is_alive_until_it_is_gone_and_a_zombie_is_gone() {
    // Catches: the sweep treating the test's own process as gone (its scratch directory
    // would go with it), and a pid that does not exist as running.
    assert!(alive(std::process::id()));
    let mut child = std::process::Command::new("true").spawn().expect("true");
    let pid = child.id();
    child.wait().expect("reaped");
    assert!(!alive(pid));
}
