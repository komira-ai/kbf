//! `kbf-server --scheduler-core js` as the binary runs it: refused with exit status 2
//! before the log file, the hot store, the metadata directory or the state store is
//! touched.

use std::process::Command;

/// Catches a refusal that comes after the log directory, the hot store, the metadata
/// directory or the state store is created (the check moved below `log_file::init` or
/// into the start), a status other than 2, and a message that does not name the flag.
#[test]
#[cfg_attr(
    feature = "farm-core-js",
    ignore = "a build with farm-core-js has the js core"
)]
fn js_is_refused_before_the_binary_creates_anything() {
    let scratch = kbf_testkit::dir::TestDir::under(env!("CARGO_TARGET_TMPDIR"), "kbf-core-flag");
    let (log, hot, meta, state) = (
        scratch.join("log"),
        scratch.join("hot"),
        scratch.join("meta"),
        scratch.join("state").join("state.db"),
    );
    let out = Command::new(env!("CARGO_BIN_EXE_kbf-server"))
        .arg("--scheduler-core=js")
        .arg(format!("--log-dir={}", log.display()))
        .arg(format!("--hot-store=file://{}", hot.display()))
        .arg(format!("--meta-dir={}", meta.display()))
        .arg(format!("--state-store=sqlite://{}", state.display()))
        .arg("--cold-store=file:///nonexistent-cold")
        .output()
        .expect("spawn");
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("--scheduler-core js"), "{said}");
    for path in [&log, &hot, &meta, &state] {
        assert!(!path.exists(), "{} was created", path.display());
    }
    assert!(!state.parent().expect("a parent").exists());
}
