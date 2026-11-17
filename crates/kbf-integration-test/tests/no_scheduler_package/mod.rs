//! No package of `Cargo.lock` is a scheduler, in the layering of `tests/layering.rs`.
//!
//! kbf schedules nothing: every Execute is one job of the job service, whose scheduler runs in
//! the service. The crates that held the farm's scheduling (`kbf-sched`, over the jobscheduler
//! repository's `js-sched` and `js-whole`) are deleted, and this reads the lock file, which
//! lists every package the workspace resolves, normal, dev and build, whatever the target, so a
//! dependency that brings one back fails here.

use std::path::Path;

/// The packages that are a scheduler: no workspace crate resolves one.
const SCHEDULER_PACKAGES: [&str; 3] = ["kbf-sched", "js-sched", "js-whole"];

/// The packages of the lock file text `lock` that are a scheduler, in the file's order.
///
/// # Panics
///
/// `lock` is not a lock file.
fn scheduler_packages(lock: &str) -> Vec<String> {
    let lock: toml::Table = toml::from_str(lock).expect("a lock file is TOML");
    lock.get("package")
        .and_then(toml::Value::as_array)
        .expect("a lock file lists its packages")
        .iter()
        .filter_map(|package| package.get("name")?.as_str())
        .filter(|name| SCHEDULER_PACKAGES.contains(name))
        .map(str::to_owned)
        .collect()
}

/// Asserts that `Cargo.lock` holds no scheduler package.
#[test]
fn no_crate_reaches_a_scheduler() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../Cargo.lock");
    let lock =
        std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    assert_eq!(
        scheduler_packages(&lock),
        Vec::<String>::new(),
        "Cargo.lock holds scheduler packages: kbf schedules nothing, the job service does"
    );
}

/// Asserts that the check fails on a lock holding each scheduler package, naming it, and
/// passes one without them, so a check that never finds a package would not pass on the real
/// lock file: catches a name dropped from the list, and a parse that reads no package.
#[test]
fn a_lock_holding_a_scheduler_package_is_refused() {
    let lock = |names: &[&str]| {
        names
            .iter()
            .map(|name| format!("[[package]]\nname = \"{name}\"\nversion = \"0.1.0\"\n"))
            .collect::<Vec<_>>()
            .join("\n")
    };
    assert_eq!(
        scheduler_packages(&lock(&["kbf-types", "tokio"])),
        Vec::<String>::new()
    );
    for name in SCHEDULER_PACKAGES {
        let text = lock(&["kbf-types", name, "tokio"]);
        assert_eq!(scheduler_packages(&text), [name], "{name}");
    }
    let all = lock(&SCHEDULER_PACKAGES);
    assert_eq!(scheduler_packages(&all), SCHEDULER_PACKAGES);
    // A package that only has a scheduler in its name or its dependencies is not one.
    let near = "[[package]]\nname = \"js-sched-cli\"\nversion = \"0.1.0\"\ndependencies = [\"js-sched\"]\n";
    assert_eq!(scheduler_packages(near), Vec::<String>::new());
}
