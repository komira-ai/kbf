//! `kbf-server --scheduler-core=js` and `--usage-profiles=record` as the binary runs them:
//! refused with exit status 2 before the log file, the hot store, the metadata directory or
//! the state store is touched.

use std::path::PathBuf;
use std::process::{Command, Output};

/// Runs the binary with `flag` and every path it creates under a scratch directory, and
/// returns its output and the paths it must not have created.
fn run(flag: &str) -> (Output, Vec<PathBuf>) {
    let scratch = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "kbf-core-flag");
    let (log, hot, meta, state) = (
        scratch.join("log"),
        scratch.join("hot"),
        scratch.join("meta"),
        scratch.join("state").join("state.db"),
    );
    let out = Command::new(env!("CARGO_BIN_EXE_kbf-server"))
        .arg(flag)
        .arg(format!("--log-dir={}", log.display()))
        .arg(format!("--hot-store=file://{}", hot.display()))
        .arg(format!("--meta-dir={}", meta.display()))
        .arg(format!("--state-store=sqlite://{}", state.display()))
        .arg("--cold-store=file:///nonexistent-cold")
        .output()
        .expect("spawn");
    let parent = state.parent().expect("a parent").to_path_buf();
    (out, vec![log, hot, meta, state, parent])
}

/// Catches `js` accepted as a scheduler core, a status other than 2, and a message that
/// does not name the flag and say the core was removed.
#[test]
fn the_removed_js_core_is_refused_before_the_binary_creates_anything() {
    let (out, created) = run("--scheduler-core=js");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("--scheduler-core"), "{said}");
    assert!(said.contains("the js scheduler core was removed"), "{said}");
    for path in &created {
        assert!(!path.exists(), "{} was created", path.display());
    }
}

/// Catches `--usage-profiles=record` accepted now that no core records profiles, a
/// refusal that comes after the binary created something, and a message other than the
/// exact one.
#[test]
fn usage_profiles_record_is_refused_before_the_binary_creates_anything() {
    let (out, created) = run("--usage-profiles=record");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        said.contains(
            "--usage-profiles record needs the js scheduler core, which was removed: \
             scheduling runs in the job service; leave the flag out or use --usage-profiles off"
        ),
        "{said}"
    );
    for path in &created {
        assert!(!path.exists(), "{} was created", path.display());
    }
}
