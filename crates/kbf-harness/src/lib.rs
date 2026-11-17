//! The harness as a library (docs/design/kbf-cicd.md, section 4.2): the parts that the
//! rehearsal tests of `kbf-cli` and the mixed-version test of `kbf-integration-test` share.
//!
//! - [`farm`]: a frontend and storage nodes, each a real `kbf` process started as the
//!   packaged unit starts it (`kbf --mode=<mode> --config <kbf.toml>`, [`units`]), on a
//!   loopback address of its own, under certificates of the right rows ([`pki`]); a
//!   supervisor with `term`, `kill`, `restart`, a process table and a report of every
//!   process's output for a hang ([`procs`]). The farm runs no action: placed actions run
//!   as js-agent jobs, not in this farm.
//! - [`reapi`]: a client channel to a farm's frontend (the REAPI listener under the farm
//!   CA), for the black-box test crates that write and read blobs through it.
//!
//! The library spawns the binaries it is given by path and links no `kbf-server` code:
//! `kbf` is a process here, not a dependency.

#![cfg(target_os = "linux")]

pub mod api;
pub mod farm;
pub mod node_support;
pub mod pki;
pub mod ports;
pub mod procs;
pub mod reapi;
pub mod stand_in;
pub mod topology;
pub mod units;

/// A fresh directory for one unit test, under the temporary directory, named for the test
/// (`CARGO_TARGET_TMPDIR` is for integration tests only).
#[cfg(test)]
pub(crate) fn scratch(name: &str) -> std::path::PathBuf {
    static SWEEP: std::sync::Once = std::sync::Once::new();
    let root = std::env::temp_dir().join("kbf-harness-tests");
    // The directories of runs that are over: one per test per run would pile up for ever.
    SWEEP.call_once(|| sweep_dead(&root, &alive));
    let dir = root.join(format!("{name}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch");
    dir
}

/// Removes the directories of `root` named `<name>-<pid>` whose process is not `running`
/// (those of earlier runs of the tests); anything else in `root` is left.
#[cfg(test)]
pub(crate) fn sweep_dead(root: &std::path::Path, running: &dyn Fn(u32) -> bool) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let pid = name
            .to_str()
            .and_then(|n| n.rsplit_once('-'))
            .and_then(|(_, pid)| pid.parse::<u32>().ok());
        if pid.is_some_and(|pid| !running(pid)) {
            // A file is not removed by this: `remove_dir_all` is for directories.
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// Whether process `pid` is running (a zombie, ended and not yet reaped, is not).
#[cfg(test)]
pub(crate) fn alive(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(')')
            .is_some_and(|(_, state)| !state.trim_start().starts_with('Z'))
    })
}

#[cfg(test)]
#[path = "lib_tests.rs"]
mod tests;
