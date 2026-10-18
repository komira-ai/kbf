//! The `kbf.toml` this tool renders, read by the loader the daemon runs (`kbf-cli`):
//! `kbf --mode=daemon --config <the file>` must stand for exactly the flags the job
//! passed before the file was the source of truth (`kbf --mode=daemon --driver native
//! --server ... --label ...`), which is the only proof that every key is in the table the
//! loader reads it from and spelled as the flag is.

mod support;

use std::ffi::OsString;
use std::fs;
use std::os::unix::fs::PermissionsExt as _;

use kbf_cli::Invocation;
use kbf_mac_provision::launchd::{NODE_CONFIG, render_config};
use kbf_mac_provision::profile::Profile;
use support::FakeMac;

/// The flags the job passed, from the profile, as `kbf --mode=daemon` takes them.
fn flags_of_the_profile(p: &Profile) -> Vec<OsString> {
    let mut args: Vec<String> = [
        "kbf",
        "--mode=daemon",
        "--driver",
        "native",
        "--server",
        p.get("kbf_server"),
        "--cas",
        p.get("kbf_cas"),
        "--ca-cert",
        p.get("kbf_ca_cert"),
        "--cert",
        p.get("kbf_cert"),
        "--key",
        p.get("kbf_key"),
        "--node-id",
        p.get("hostname"),
        "--data-dir",
        p.get("kbf_data_dir"),
        "--label",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    args.push(p.node_label());
    for label in &p.labels {
        args.push("--label".into());
        args.push(label.clone());
    }
    args.into_iter().map(OsString::from).collect()
}

fn daemon(invocation: &Invocation) -> String {
    match invocation {
        Invocation::Daemon(cli) => format!("{cli:?}"),
        other => panic!("not the daemon with flags: {other:?}"),
    }
}

/// Catches: a rendered file the daemon's own loader reads as other flags than the job
/// used to pass (a key in the wrong table, a flag name misspelled, the driver or a label
/// missing, a label's order changed), or as no daemon with flags at all (no `mode`: the
/// start would be refused; an enrollment table: it would enroll instead).
#[test]
fn the_rendered_file_stands_for_the_flags_the_job_used_to_pass() {
    let mac = FakeMac::fresh("node-config-parse");
    let p = Profile::parse(mac.profile_text().as_bytes()).unwrap();
    let dir = support::scratch("node-config-file");
    let file = dir.join("kbf.toml");
    fs::write(&file, render_config(&p)).unwrap();
    // As the Mac has it: root's, mode 644 (the loader refuses a file others can write).
    fs::set_permissions(&file, fs::Permissions::from_mode(0o644)).unwrap();

    let args = vec![
        OsString::from("kbf"),
        "--mode=daemon".into(),
        "--config".into(),
        file.clone().into(),
    ];
    let from_file = kbf_cli::parse_with(args, None).expect("the daemon's loader reads the file");
    assert!(!from_file.check);
    let from_flags = kbf_cli::parse_from(flags_of_the_profile(&p)).expect("the old flags parse");
    assert_eq!(daemon(&from_file.invocation), daemon(&from_flags));
    // Both are the native daemon of this node, so the comparison is not of two defaults.
    let text = daemon(&from_file.invocation);
    for want in [
        "Native",
        "kbf-mac-07",
        "https://farm.example.net:8981",
        "/Volumes/kbf/data",
        "(\"pool\", \"mac\")",
        "(\"rack\", \"r2\")",
        "(\"profile\", \"mac-arm64-test@",
    ] {
        assert!(text.contains(want), "{want} not in {text}");
    }
}

/// Catches: the job's path and the path the file is written to drifting apart (the daemon
/// would read a file that is never written).
#[test]
fn the_job_reads_the_file_the_tool_writes() {
    let mac = FakeMac::fresh("node-config-path");
    let p = Profile::parse(mac.profile_text().as_bytes()).unwrap();
    let args = kbf_mac_provision::launchd::daemon_args(&p);
    assert_eq!(args.last().map(String::as_str), Some(NODE_CONFIG));
    assert_eq!(args[args.len() - 2], "--config");
}
