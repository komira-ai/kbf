//! Unit tests of the actions cgroup's CPU limit: which mode lifts it, what is written to
//! `cpu.max`, and that the node's load reading reports what was written.

use std::path::Path;

use super::*;
use crate::node_load::LoadTracker;

fn actions(name: &str) -> kbf_testkit::dir::TestDir {
    let root = crate::unit_dir(name);
    let dir = root.join("actions");
    std::fs::create_dir(&dir).expect("actions");
    for (file, text) in [
        ("cpu.stat", "usage_usec 5\n"),
        ("memory.current", "10\n"),
        ("memory.stat", "inactive_file 1\nactive_file 1\n"),
        ("cpu.max", "max 100000\n"),
    ] {
        std::fs::write(dir.join(file), text).expect("file");
    }
    root
}

fn cpu_max(root: &Path) -> String {
    std::fs::read_to_string(root.join("actions/cpu.max")).expect("cpu.max")
}

/// Catches a mode that does not follow its table: `auto` is the Welcome, `on` and `off`
/// ignore it, and a default other than `auto`.
#[test]
fn each_mode_says_whether_the_limit_is_lifted() {
    assert_eq!(ActionsCpuMax::default(), ActionsCpuMax::Auto);
    for (mode, welcomed_on, welcomed_off) in [
        (ActionsCpuMax::Auto, true, false),
        (ActionsCpuMax::On, true, true),
        (ActionsCpuMax::Off, false, false),
    ] {
        assert_eq!(mode.unlimited(true), welcomed_on, "{mode:?} with load");
        assert_eq!(mode.unlimited(false), welcomed_off, "{mode:?} without");
    }
}

/// Catches the limit written to the wrong file or in the wrong form: the lifted limit is
/// `max` over the kernel's period, the reserve's is the leases' CPUs as a quota, a later
/// Welcome with the other mode rewrites it, and the heartbeat's reading
/// (`actions_cpu_limited`) is the opposite of what was lifted each time.
#[tokio::test]
async fn the_limit_written_is_the_one_the_mode_and_the_welcome_call_for() {
    let root = actions("actions-cpu-apply");
    let auto = ActionsCpu {
        mode: ActionsCpuMax::Auto,
        lease_cpus: 6,
    };
    let tracker = LoadTracker::new(std::sync::Arc::new(|| 0));
    for (welcome, text) in [
        (true, "max 100000"),
        (false, "600000 100000"),
        (true, "max 100000"),
    ] {
        let lifted = auto
            .apply(&root, "/actions", welcome)
            .await
            .expect("applied");
        assert_eq!(lifted, welcome);
        assert_eq!(cpu_max(&root), text);
        let load = tracker
            .reading(root.join("actions"))
            .await
            .expect("reading");
        assert_eq!(load.actions_cpu_limited, !welcome);
    }
}

/// Catches `on` and `off` yielding to the Welcome, in both directions.
#[tokio::test]
async fn on_and_off_override_the_welcome() {
    let root = actions("actions-cpu-override");
    let on = ActionsCpu {
        mode: ActionsCpuMax::On,
        lease_cpus: 6,
    };
    let off = ActionsCpu {
        mode: ActionsCpuMax::Off,
        lease_cpus: 6,
    };
    assert!(on.apply(&root, "/actions", false).await.expect("on"));
    assert_eq!(cpu_max(&root), "max 100000");
    assert!(!off.apply(&root, "/actions", true).await.expect("off"));
    assert_eq!(cpu_max(&root), "600000 100000");
}

/// Catches a failed write being swallowed: a cgroup without the file is an error naming
/// `cpu.max`, for the lifted limit and the reserve's alike.
#[tokio::test]
async fn a_write_that_fails_is_an_error_naming_the_file() {
    let root = crate::unit_dir("actions-cpu-missing");
    let auto = ActionsCpu {
        mode: ActionsCpuMax::Auto,
        lease_cpus: 2,
    };
    for welcome in [true, false] {
        let err = auto
            .apply(&root, "/nowhere", welcome)
            .await
            .expect_err("no cgroup")
            .to_string();
        assert!(err.contains("cpu.max"), "{err}");
    }
}
